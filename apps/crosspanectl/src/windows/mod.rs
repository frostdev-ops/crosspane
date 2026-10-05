//! Windows-only CLI adapters. The JSON control envelopes remain platform-independent.

mod security;

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;

pub fn config_dir() -> Result<PathBuf> {
    absolute_env("APPDATA").map(|dir| dir.join("Crosspane"))
}

fn absolute_env(name: &str) -> Result<PathBuf> {
    let dir = PathBuf::from(std::env::var_os(name).with_context(|| format!("{name} is not set"))?);
    ensure!(dir.is_absolute(), "{name} must name an absolute directory");
    Ok(dir)
}

fn runtime_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CROSSPANE_RUNTIME_DIR") {
        let dir = PathBuf::from(dir);
        ensure!(dir.is_absolute(), "CROSSPANE_RUNTIME_DIR must be absolute");
        return Ok(dir);
    }
    Ok(absolute_env("LOCALAPPDATA")?.join("Crosspane/runtime"))
}

pub fn exchange(request: &Value) -> Result<Value> {
    let path = security::endpoint(&runtime_dir()?)?;
    let identity = security::current_identity()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create control client runtime")?;
    runtime.block_on(async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut pipe = loop {
            match security::open_client(&path, &identity) {
                Ok(pipe) => break pipe,
                Err(error)
                    if error
                        .downcast_ref::<io::Error>()
                        .is_some_and(|e| e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)) =>
                {
                    ensure!(
                        tokio::time::Instant::now() < deadline,
                        "control pipe stayed busy for ten seconds"
                    );
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => return Err(error).context("is crosspane-agent running?"),
            }
        };
        // open_client validated the server's exact user and logon BEFORE any JSON is sent.
        let mut bytes = serde_json::to_vec(request)?;
        bytes.push(b'\n');
        tokio::time::timeout(Duration::from_secs(10), pipe.write_all(&bytes))
            .await
            .context("control request write timed out")??;
        let mut reader = BufReader::new(pipe);
        let line = tokio::time::timeout(Duration::from_secs(10), async {
            let mut text = Vec::new();
            loop {
                let available = reader.fill_buf().await?;
                if available.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "agent closed before answering",
                    ));
                }
                let newline = available.iter().position(|byte| *byte == b'\n');
                let consumed = newline.map_or(available.len(), |index| index + 1);
                if consumed > (4 * 1024 * 1024usize).saturating_sub(text.len()) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "agent response is too large",
                    ));
                }
                text.extend_from_slice(&available[..consumed]);
                reader.consume(consumed);
                if newline.is_some() {
                    return Ok(text);
                }
            }
        })
        .await
        .context("agent response timed out")??;
        serde_json::from_slice(&line).context("bad response from agent")
    })
}

pub fn choose_terminal(lines: &[String]) -> Result<Option<String>> {
    for line in lines {
        println!("{line}");
    }
    print!("Choose a window number (blank to cancel): ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().lock().read_line(&mut answer)?;
    let answer = answer.trim();
    if answer.is_empty() || matches!(answer, "0" | "q" | "Q") {
        return Ok(None);
    }
    let number: usize = answer
        .parse()
        .context("enter one of the displayed window numbers")?;
    Ok(number
        .checked_sub(1)
        .and_then(|index| lines.get(index))
        .cloned())
}

/// Diagnostics are an explicit user command: they may collect the agent's window list, but
/// contain no clipboard contents, private key files, unrelated OS windows or system logs.
pub fn diag(out: Option<PathBuf>) -> Result<()> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("clock is before Unix epoch")?
        .as_nanos();
    let name = format!("crosspane-diag-{}-{stamp}", std::process::id());
    // Staging must be below an existing admitted private runtime. No AppData parent ACLs
    // are changed; offline diagnostics works once this runtime exists, even without the agent.
    let temp = runtime_dir()?;
    let dir = temp.join(&name);
    let staging = security::create_private_directory(&dir)?;
    // Always explicitly finish staging, including when collection or tar fails. The guards'
    // Drop implementations provide a second cleanup attempt if explicit cleanup fails.
    let collected = collect_diagnostics(&staging, &temp, &name, out);
    let cleaned = staging.finish();
    match (collected, cleaned) {
        (Ok((archive, out)), Ok(())) => {
            archive.commit()?;
            println!("{}", out.display());
            Ok(())
        }
        (Ok((archive, _)), Err(error)) => match archive.abort() {
            Ok(()) => Err(error),
            Err(cleanup) => Err(error.context(format!("archive cleanup also failed: {cleanup:#}"))),
        },
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("staging cleanup also failed: {cleanup:#}")))
        }
    }
}

fn collect_diagnostics(
    staging: &security::PrivateDirectory,
    temp: &Path,
    name: &str,
    out: Option<PathBuf>,
) -> Result<(security::PrivateFile, PathBuf)> {
    for (file, cmd) in [
        ("status.json", "status"),
        ("windows.json", "windows"),
        ("pairing.json", "pair_status"),
    ] {
        let text = exchange(&json!({"cmd":cmd}))
            .and_then(|value| serde_json::to_string_pretty(&value).context("serialize diagnostics"))
            .unwrap_or_else(|error| format!("(agent not reachable: {error})"));
        staging
            .write(file, text.as_bytes())
            .context("write agent diagnostics")?;
    }
    // Only these two known metadata files; never traverse the config tree or copy device keys.
    let config = config_dir()?;
    for file in ["config.toml", "trust.json"] {
        match security::read_metadata(&config.join(file)) {
            Ok(text) => staging.write(file, text.as_bytes())?,
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) => {}
            Err(error) => return Err(error).context("read diagnostics metadata"),
        }
    }
    staging.write(
        "agent.log",
        b"Unavailable: the Windows foreground agent logs to stderr.\n",
    )?;
    staging.write(
        "system.txt",
        format!("OS: Windows\nArchitecture: {}\n", std::env::consts::ARCH).as_bytes(),
    )?;
    staging.write(
        "crosspanectl.txt",
        format!("crosspanectl {}\n", env!("CARGO_PKG_VERSION")).as_bytes(),
    )?;

    let tar = absolute_env("SystemRoot")?.join("System32/tar.exe");
    ensure!(
        tar.is_file(),
        "Windows system tar.exe is unavailable; cannot create diagnostics archive"
    );
    let out = out.unwrap_or_else(|| PathBuf::from(format!("{name}.tar.gz")));
    let out = if out.is_absolute() {
        out
    } else {
        std::env::current_dir()?.join(out)
    };
    // An output inside staging would pin that tree against its required cleanup.
    let parent = out.parent().context("diagnostics output has no parent")?;
    let parent = std::fs::canonicalize(parent).context("canonicalize diagnostics output parent")?;
    let staged =
        std::fs::canonicalize(staging.path()).context("canonicalize diagnostics staging")?;
    ensure!(
        !parent.starts_with(staged),
        "diagnostics output must be outside the staging directory"
    );
    let mut archive = security::create_private_file(&out)
        .context("reserve a new diagnostics archive (output must not exist)")?;
    let members = staging.tar_members()?;
    let spawned = std::process::Command::new(tar)
        .arg("czf")
        .arg("-")
        .arg("-C")
        .arg(temp)
        .args(members)
        .stdout(std::process::Stdio::piped())
        // Tar error output can include diagnostic paths/text. Bound it to zero instead of
        // sending it into product or board logs; status/spawn/stream failures remain errors.
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("start Windows system tar.exe");
    let result = match spawned {
        Err(error) => Err(error),
        Ok(mut child) => {
            let copied = child
                .stdout
                .take()
                .context("tar stdout pipe is absent")
                .and_then(|mut stdout| archive.copy_from(&mut stdout).map(|_| ()));
            match copied {
                Ok(()) => match child.wait().context("wait for Windows system tar.exe") {
                    Ok(status) => Ok(status),
                    Err(error) => Err(stop_tar(&mut child, error)),
                },
                Err(error) => Err(stop_tar(&mut child, error)),
            }
        }
    };
    match result {
        Ok(status) if status.success() => Ok((archive, out)),
        result => {
            let error = match result {
                Err(error) => error,
                Ok(_) => anyhow::anyhow!("Windows system tar.exe failed"),
            };
            match archive.abort() {
                Ok(()) => Err(error),
                Err(cleanup) => {
                    Err(error.context(format!("archive cleanup also failed: {cleanup:#}")))
                }
            }
        }
    }
}

fn stop_tar(child: &mut std::process::Child, mut error: anyhow::Error) -> anyhow::Error {
    // Stop/wait before archive abort or staging cleanup, including failed wait operations.
    // The primary streaming/wait error remains the cause; no stderr or payload text is logged.
    if let Err(kill) = child.kill()
        && kill.kind() != io::ErrorKind::InvalidInput
    {
        error = error.context(format!("tar termination also failed: {kill}"));
    }
    if let Err(wait) = child.wait() {
        error = error.context(format!("tar wait also failed: {wait}"));
    }
    error
}
