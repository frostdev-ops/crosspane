//! Default bundle launches hand off to the exact managed GUI job before any agent startup.
use anyhow::{Result, bail};
use std::sync::atomic::{AtomicBool, Ordering};
const LABEL: &str = "io.frostdev.crosspane.agent";
const LIMIT: usize = 1024 * 1024;

pub(crate) struct Output {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}
pub(crate) fn handoff(
    default_launch: bool,
    uid: u32,
    mut command: impl FnMut(&[&str]) -> Result<Output>,
) -> Result<bool> {
    if !default_launch {
        return Ok(false);
    }
    let job = format!("gui/{uid}/{LABEL}");
    let output = command(&["print", &job])?;
    if output.stdout.len().saturating_add(output.stderr.len()) > LIMIT {
        bail!("Crosspane's sign-in item could not be checked (output too large)");
    }
    let absent = format!("Could not find service \"{LABEL}\" in domain for user gui: {uid}\n");
    let stderr = output
        .stderr
        .strip_prefix(b"Bad request.\n")
        .unwrap_or(&output.stderr);
    if output.code == Some(113) && output.stdout.is_empty() && stderr == absent.as_bytes() {
        return Ok(false);
    }
    let header = format!("{job} = {{\n");
    if output.code != Some(0)
        || !output.stderr.is_empty()
        || !output.stdout.starts_with(header.as_bytes())
    {
        bail!("Crosspane's sign-in item could not be checked; run its installer to check again");
    }
    // Never -k: opening the bundle must not interrupt a running managed agent or session.
    let output = command(&["kickstart", &job])?;
    if output.code != Some(0) || output.stdout.len().saturating_add(output.stderr.len()) > LIMIT {
        bail!("Crosspane's sign-in item could not be started; run its installer to check again");
    }
    Ok(true)
}
pub(crate) fn appkit_returned(closed: &AtomicBool, restart: &AtomicBool, stop: impl FnOnce()) {
    if closed.swap(true, Ordering::AcqRel) {
        return;
    }
    restart.store(true, Ordering::Release);
    stop();
}

#[cfg(target_os = "macos")]
pub(crate) fn system_command(args: &[&str]) -> Result<Output> {
    use rustix::fs::{OFlags, fcntl_setfl};
    use std::{
        io::Read,
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    let mut child = Command::new("/bin/launchctl")
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let result = (|| {
        let mut out = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("missing launchctl stdout"))?;
        let mut err = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("missing launchctl stderr"))?;
        fcntl_setfl(&out, OFlags::NONBLOCK)?;
        fcntl_setfl(&err, OFlags::NONBLOCK)?;
        let until = Instant::now() + Duration::from_secs(5);
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        loop {
            if Instant::now() >= until {
                bail!("launchctl timed out");
            }
            let mut done = [false; 2];
            let (out_len, err_len) = (stdout.len(), stderr.len());
            for (i, reader, bytes, other) in [
                (0, &mut out as &mut dyn Read, &mut stdout, err_len),
                (1, &mut err as &mut dyn Read, &mut stderr, out_len),
            ] {
                let mut buffer = [0; 4096];
                loop {
                    if Instant::now() >= until {
                        bail!("launchctl timed out");
                    }
                    match reader.read(&mut buffer) {
                        Ok(0) => {
                            done[i] = true;
                            break;
                        }
                        Ok(n) => {
                            if bytes.len() + other + n > LIMIT {
                                bail!("launchctl output too large");
                            }
                            bytes.extend_from_slice(&buffer[..n]);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(e.into()),
                    }
                }
            }
            if stdout.len() + stderr.len() > LIMIT {
                bail!("launchctl output too large");
            }
            if let Some(status) = child.try_wait()?
                && done == [true, true]
            {
                return Ok(Output {
                    code: status.code(),
                    stdout,
                    stderr,
                });
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

#[cfg(target_os = "macos")]
pub(crate) fn show_handoff_error() {
    // Fixed JXA source opens NSAlert directly; no automation target, credentials or user data.
    // It runs separately so the dialog remains visible after this failed bundle launch ends.
    let script = "ObjC.import('AppKit'); const a=$.NSAlert.alloc.init; a.messageText='Crosspane could not start'; a.informativeText='The sign-in item could not be checked or started. Open Crosspane Installer and choose Check again.'; a.addButtonWithTitle('OK'); a.runModal;";
    if let Err(error) = std::process::Command::new("/usr/bin/osascript")
        .args(["-l", "JavaScript", "-e", script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        tracing::error!(%error, "could not show Crosspane startup error");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn loaded() -> Output {
        Output {
            code: Some(0),
            stdout: format!("gui/501/{LABEL} = {{\n\tstate = running\n}}\n").into_bytes(),
            stderr: vec![],
        }
    }
    #[test]
    fn bundle_launch_hands_off_without_interrupting_the_managed_job() {
        let mut calls = Vec::new();
        assert!(
            handoff(true, 501, |args| {
                calls.push(args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
                Ok(if calls.len() == 1 {
                    loaded()
                } else {
                    Output {
                        code: Some(0),
                        stdout: vec![],
                        stderr: vec![],
                    }
                })
            })
            .unwrap()
        );
        assert_eq!(
            calls,
            vec![
                vec!["print", "gui/501/io.frostdev.crosspane.agent"],
                vec!["kickstart", "gui/501/io.frostdev.crosspane.agent"]
            ]
        );
    }
    #[test]
    fn explicit_run_bypasses_handoff_and_exact_absence_keeps_manual_launch() {
        assert!(!handoff(false, 501, |_| panic!("explicit run must not query itself")).unwrap());
        for prefix in ["", "Bad request.\n"] {
            assert!(!handoff(true, 501, |args| {
                assert_eq!(args[0], "print");
                Ok(Output { code: Some(113), stdout: vec![], stderr: format!("{prefix}Could not find service \"{LABEL}\" in domain for user gui: 501\n").into_bytes() })
            }).unwrap());
        }
    }
    #[test]
    fn unknown_job_and_failed_kickstart_never_start_an_unmanaged_agent() {
        for kind in 0..5 {
            assert!(
                handoff(true, 501, |_| {
                    let mut output = loaded();
                    match kind {
                        0 => output.code = Some(1),
                        1 => output.stdout = b"gui/502/foreign = {\n".to_vec(),
                        2 => output.stderr = b"warning".to_vec(),
                        3 => output.stdout = vec![0; LIMIT + 1],
                        _ => output.code = None,
                    }
                    Ok(output)
                })
                .is_err()
            );
        }
        let mut calls = 0;
        assert!(
            handoff(true, 501, |_| {
                calls += 1;
                let mut output = loaded();
                if calls == 2 {
                    output.code = Some(1);
                }
                Ok(output)
            })
            .is_err()
        );
        assert_eq!(calls, 2);
    }
    #[test]
    fn appkit_return_marks_restart_before_requesting_clean_shutdown() {
        let (closed, restart) = (AtomicBool::new(false), AtomicBool::new(false));
        let (send, recv) = std::sync::mpsc::channel();
        appkit_returned(&closed, &restart, || {
            assert!(closed.load(Ordering::Acquire));
            assert!(restart.load(Ordering::Acquire));
            send.send(75).unwrap();
        });
        assert_eq!(recv.try_recv().unwrap(), 75);
        appkit_returned(&closed, &restart, || {
            panic!("shutdown must only be requested once")
        });
    }
}
