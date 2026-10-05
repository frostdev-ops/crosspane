//! CLI for the running agent: status, release, panic, re-arm, layout, dial. It speaks the agent's
//! control socket (one JSON request and response per line).

use std::io::Write;
#[cfg(unix)]
use std::io::{BufRead, BufReader};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
#[cfg(unix)]
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};

#[cfg(windows)]
mod windows;

#[derive(Debug, Parser)]
#[command(
    name = "crosspanectl",
    version,
    about = "Control the running Crosspane agent",
    after_help = "To delete this machine's identity and pairings after a clean stop, run crosspane-agent erase-identity [--keep-trust]."
)]
struct Cli {
    /// Print the raw JSON response.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show this node, its peers, the layout and recent notices.
    Status,
    /// Give input back to this machine now.
    Release,
    /// End every session and disarm crossing until re-armed.
    Panic,
    /// Re-arm edge crossing after a release or panic.
    Rearm,
    /// Restart the agent in place (e.g. after granting macOS permissions).
    Restart,
    /// Update the macOS virtual display setting for the next start. Rewrites config.toml without
    /// preserving comments. Send `restart` afterwards to apply it.
    SettingsUpdate {
        #[arg(long)]
        expected_revision: String,
        #[arg(long, action = clap::ArgAction::Set)]
        mac_virtual_display: bool,
    },
    /// Collect a diagnostics bundle (status, config, paired machines, recent logs, versions) into
    /// a .tar.gz for a bug report. It never contains private keys or typed text.
    Diag {
        /// Where to write the bundle (default: a timestamped file in the current directory).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Unpair a peer (name or node-id prefix): its key is forgotten and the connection ends now.
    Forget { peer: String },
    /// Save the picture a projected window last showed here (decoded, before drawing) as a PPM
    /// file, for checking exactness; prints its path. SOURCE is the machine it comes from.
    Snapshot { source: String, projection: u64 },
    /// Ask the OS for the first permission this machine still lacks (macOS: Accessibility, then
    /// Input Monitoring, Screen Recording and Microphone), one per run.
    RequestPermissions,
    /// Ask the OS for one permission: its prompt, or System Settings at its pane when the prompt
    /// was already answered. Prints what was shown.
    AskPermission { permission: PermissionArg },
    /// Reset Crosspane's own entry for one macOS permission (tccutil reset for
    /// io.frostdev.crosspane.agent only), so its prompt can show again. Then ask-permission.
    ResetPermission { permission: PermissionArg },
    /// A lost or stolen device: forget it here and tell every other paired machine to forget it
    /// too (a signed revocation notice). It can only come back through a fresh pairing.
    Revoke { peer: String },
    /// Put a peer (name or node-id prefix) on a side of this machine.
    Layout { peer: String, side: Side },
    /// Place one display on the shared canvas: NODE (this machine's or a peer's name, or a
    /// node-id prefix), its display id (from `status`), and its top-left corner in millimetres.
    Place {
        node: String,
        display: u32,
        #[arg(allow_hyphen_values = true)]
        x_mm: f64,
        #[arg(allow_hyphen_values = true)]
        y_mm: f64,
    },
    /// Connect to a peer at ADDR (host:port) now.
    Dial { addr: String },
    /// List this machine's windows (ids for `project`).
    Windows {
        /// List a peer's windows instead (it must allow this machine to browse).
        #[arg(long)]
        from: Option<String>,
    },
    /// Ask a peer to project one of its windows here (ids from `windows --from`).
    Pull { peer: String, window: u64 },
    /// Choose one of a peer's windows from a menu and show it here. On Linux the menu is a
    /// dmenu-style launcher (walker, fuzzel, wofi or rofi, or --menu); on macOS a list dialog.
    Pick {
        #[arg(long)]
        from: String,
        /// A dmenu-compatible command that reads lines on stdin and prints the chosen one.
        #[arg(long)]
        menu: Option<String>,
    },
    /// Let a peer use a capability here, or stop with --off: input, share, browse, present,
    /// speaker (play sound on this machine's speakers) or mic (stored, but microphones are not
    /// supported yet). clipboard.read lets the peer read my clipboard when pasting there;
    /// clipboard.write lets the peer offer its clipboard here. Both default off.
    Allow {
        peer: String,
        capability: String,
        #[arg(long)]
        off: bool,
    },
    /// Project one of this machine's windows to a peer.
    Project {
        /// Window id from `crosspanectl windows`.
        window: u64,
        /// Peer name or node-id prefix.
        peer: String,
    },
    /// Pair with another machine (SAS number matching).
    Pair {
        #[command(subcommand)]
        action: PairAction,
    },
    /// End a projection and give the window back to its source.
    Return {
        /// The projection number from `crosspanectl status` (not a window ID).
        projection: u64,
        /// The source peer (default: this machine).
        #[arg(long)]
        source: Option<String>,
    },
}

/// An OS permission, by its `status` token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "snake_case")]
enum PermissionArg {
    Accessibility,
    InputMonitoring,
    ScreenRecording,
    Microphone,
}

impl PermissionArg {
    fn token(self) -> &'static str {
        match self {
            PermissionArg::Accessibility => "accessibility",
            PermissionArg::InputMonitoring => "input_monitoring",
            PermissionArg::ScreenRecording => "screen_recording",
            PermissionArg::Microphone => "microphone",
        }
    }
}

#[cfg(test)]
mod settings_tests {
    #![allow(clippy::unwrap_used)]

    use clap::CommandFactory;

    use super::*;

    #[test]
    fn permission_commands_parse_and_word_their_answers() {
        let cli =
            Cli::try_parse_from(["crosspanectl", "ask-permission", "input_monitoring"]).unwrap();
        let Command::AskPermission { permission } = cli.command else {
            panic!("ask-permission");
        };
        assert_eq!(permission.token(), "input_monitoring");
        let cli =
            Cli::try_parse_from(["crosspanectl", "reset-permission", "screen_recording"]).unwrap();
        let Command::ResetPermission { permission } = cli.command else {
            panic!("reset-permission");
        };
        assert_eq!(permission.token(), "screen_recording");
        assert!(Cli::try_parse_from(["crosspanectl", "ask-permission", "camera"]).is_err());
        assert!(
            ask_wording(&json!({"permission": "accessibility", "shown": "pane"}))
                .contains("System Settings")
        );
        assert_eq!(
            ask_wording(
                &json!({"permission": null, "shown": "nothing", "note": "every permission is granted"})
            ),
            "every permission is granted"
        );
    }

    #[test]
    fn clipboard_allow_help_and_off_parse_without_changing_default_grants() {
        let mut command = Cli::command();
        let help = command
            .find_subcommand_mut("allow")
            .unwrap()
            .render_long_help()
            .to_string();
        for capability in ["clipboard.read", "clipboard.write"] {
            assert!(help.contains(capability));
            for off in [false, true] {
                let mut args = vec!["crosspanectl", "allow", "peer", capability];
                if off {
                    args.push("--off");
                }
                let Command::Allow {
                    peer,
                    capability: parsed,
                    off: parsed_off,
                } = Cli::try_parse_from(args).unwrap().command
                else {
                    panic!("allow command");
                };
                assert_eq!(peer, "peer");
                assert_eq!(parsed, capability);
                assert_eq!(parsed_off, off);
            }
        }
        assert!(help.contains("Both default off"));
    }

    #[test]
    fn settings_update_accepts_explicit_boolean_values_and_json() {
        for (text, expected) in [("true", true), ("false", false)] {
            let cli = Cli::try_parse_from([
                "crosspanectl",
                "settings-update",
                "--expected-revision",
                "0123456789abcdef",
                "--mac-virtual-display",
                text,
                "--json",
            ])
            .unwrap();
            assert!(cli.json);
            let Command::SettingsUpdate {
                expected_revision,
                mac_virtual_display,
            } = cli.command
            else {
                panic!("wrong command")
            };
            assert_eq!(expected_revision, "0123456789abcdef");
            assert_eq!(mac_virtual_display, expected);
        }
        assert!(
            Cli::try_parse_from([
                "crosspanectl",
                "settings-update",
                "--expected-revision",
                "0123456789abcdef",
                "--mac-virtual-display",
                "yes"
            ])
            .is_err()
        );
    }

    #[test]
    fn help_documents_comment_loss_and_direct_erase_identity_command() {
        let mut command = Cli::command();
        let help = command.render_long_help().to_string();
        assert!(help.contains("crosspane-agent erase-identity [--keep-trust]"));
        let settings = command
            .find_subcommand_mut("settings-update")
            .unwrap()
            .render_long_help()
            .to_string();
        assert!(settings.contains("without preserving comments"));
    }
}

#[derive(Debug, Subcommand)]
enum PairAction {
    /// Open a pairing window here (shows a code to compare).
    Listen {
        /// Let the new peer control this machine's keyboard and mouse.
        #[arg(long)]
        allow_input: bool,
    },
    /// Join the pairing window of a machine: its name as `pair scan` shows it, or host:port.
    Join {
        addr: String,
        #[arg(long)]
        allow_input: bool,
    },
    /// Show the pairing state (the code, or the candidates to pick from).
    Status,
    /// List machines on the network with a pairing window open.
    Scan,
    /// Confirm (yes) or reject (no) the code on the listening machine.
    Confirm { answer: String },
    /// Pick candidate N (1–3) on the joining machine.
    Pick { n: usize },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Side {
    Left,
    Right,
    Above,
    Below,
}

#[cfg(unix)]
fn socket_path() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CROSSPANE_RUNTIME_DIR") {
        return Ok(PathBuf::from(dir).join("agent.sock"));
    }
    if cfg!(target_os = "macos") {
        let tmp = std::env::var_os("TMPDIR").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
        Ok(tmp.join("crosspane/agent.sock"))
    } else {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?;
        Ok(PathBuf::from(runtime).join("crosspane/agent.sock"))
    }
}

fn main() -> Result<()> {
    // `crosspanectl status | head` closes stdout early: end quietly instead of panicking.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or("");
        if message.contains("Broken pipe") {
            std::process::exit(0);
        }
        default_hook(info);
    }));
    let cli = Cli::parse();
    let request = match &cli.command {
        Command::Status => json!({"cmd": "status"}),
        Command::Release => json!({"cmd": "release"}),
        Command::Panic => json!({"cmd": "panic"}),
        Command::Rearm => json!({"cmd": "rearm"}),
        Command::Restart => json!({"cmd": "restart"}),
        Command::SettingsUpdate {
            expected_revision,
            mac_virtual_display,
        } => json!({
            "cmd": "settings_update",
            "expected_revision": expected_revision,
            "mac_virtual_display": mac_virtual_display,
        }),
        Command::Diag { out } => return diag(out.clone()),
        Command::Forget { peer } => json!({"cmd": "forget", "peer": peer}),
        Command::RequestPermissions => json!({"cmd": "ask_permissions"}),
        Command::AskPermission { permission } => {
            json!({"cmd": "ask_permission", "permission": permission.token()})
        }
        Command::ResetPermission { permission } => {
            json!({"cmd": "reset_permission", "permission": permission.token()})
        }
        Command::Snapshot { source, projection } => {
            json!({"cmd": "snapshot", "projection": projection, "source": source})
        }
        Command::Revoke { peer } => json!({"cmd": "revoke", "peer": peer}),
        Command::Layout { peer, side } => {
            let side = format!("{side:?}").to_lowercase();
            json!({"cmd": "layout", "peer": peer, "side": side})
        }
        Command::Place {
            node,
            display,
            x_mm,
            y_mm,
        } => json!({
            "cmd": "place",
            "placements": [{"node": node, "display": display, "origin_mm": [x_mm, y_mm]}],
        }),
        Command::Dial { addr } => {
            use std::net::ToSocketAddrs;
            let resolved = addr
                .to_socket_addrs()
                .with_context(|| format!("resolve {addr}"))?
                .next()
                .context("no address")?;
            json!({"cmd": "dial", "addr": resolved.to_string()})
        }
        Command::Windows { from: None } => json!({"cmd": "windows"}),
        Command::Windows { from: Some(peer) } => json!({"cmd": "windows_from", "peer": peer}),
        Command::Pull { peer, window } => json!({"cmd": "pull", "peer": peer, "window": window}),
        Command::Pick { from, menu } => return pick(from, menu.as_deref()),
        Command::Allow {
            peer,
            capability,
            off,
        } => json!({"cmd": "allow", "peer": peer, "capability": capability, "allow": !off}),
        Command::Pair { action } => match action {
            PairAction::Listen { allow_input } => {
                json!({"cmd": "pair_listen", "allow_input": allow_input})
            }
            PairAction::Join { addr, allow_input } => {
                use std::net::ToSocketAddrs;
                // A name from `pair scan` first, then host:port.
                let offered = call(&json!({"cmd": "pair_scan"})).ok().and_then(|offers| {
                    offers.as_array()?.iter().find_map(|o| {
                        (o["name"].as_str()? == addr.as_str())
                            .then(|| o["addr"].as_str().map(str::to_owned))
                            .flatten()
                    })
                });
                let resolved = match offered {
                    Some(a) => a.parse().context("bad address from discovery")?,
                    None => addr
                        .to_socket_addrs()
                        .with_context(|| {
                            format!("no machine called {addr} is pairing, and it isn't host:port")
                        })?
                        .next()
                        .context("no address")?,
                };
                json!({"cmd": "pair_join", "addr": resolved.to_string(), "allow_input": allow_input})
            }
            PairAction::Scan => json!({"cmd": "pair_scan"}),
            PairAction::Status => json!({"cmd": "pair_status"}),
            PairAction::Confirm { answer } => {
                json!({"cmd": "pair_confirm", "accept": matches!(answer.as_str(), "yes" | "y")})
            }
            PairAction::Pick { n } => json!({"cmd": "pair_pick", "index": n.saturating_sub(1)}),
        },
        Command::Project { window, peer } => {
            json!({"cmd": "project", "window": window, "peer": peer})
        }
        Command::Return { projection, source } => {
            json!({"cmd": "return", "projection": projection, "source": source})
        }
    };
    let response = exchange(&request)?;
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    }
    if response["ok"] != json!(true) {
        bail!("{}", response["error"].as_str().unwrap_or("request failed"));
    }
    if !cli.json {
        print_result(&cli.command, &response["result"]);
    }
    Ok(())
}

/// One request and its response on the agent's control socket.
#[cfg(unix)]
fn exchange(request: &Value) -> Result<Value> {
    let path = socket_path()?;
    let stream = UnixStream::connect(&path)
        .with_context(|| format!("is crosspane-agent running? ({})", path.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut writer = stream.try_clone()?;
    writeln!(writer, "{request}")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    serde_json::from_str(&line).context("bad response from agent")
}

#[cfg(windows)]
fn exchange(request: &Value) -> Result<Value> {
    windows::exchange(request)
}

/// The result of a request, or its error.
fn call(request: &Value) -> Result<Value> {
    let response = exchange(request)?;
    if response["ok"] != json!(true) {
        bail!("{}", response["error"].as_str().unwrap_or("request failed"));
    }
    Ok(response["result"].clone())
}

/// `crosspanectl diag`: gather what a bug report needs into one archive.
#[cfg(unix)]
fn diag(out: Option<PathBuf>) -> Result<()> {
    use std::process::Command as Process;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let name = format!("crosspane-diag-{stamp}");
    let dir = std::env::temp_dir().join(&name);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let write = |file: &str, text: &str| {
        let _ = std::fs::write(dir.join(file), text);
    };
    let run = |program: &str, args: &[&str]| -> String {
        match Process::new(program).args(args).output() {
            Ok(o) => format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ),
            Err(e) => format!("({program} failed: {e})\n"),
        }
    };
    // The agent's view.
    for (file, cmd) in [
        ("status.json", "status"),
        ("windows.json", "windows"),
        ("pairing.json", "pair_status"),
    ] {
        let text = exchange(&json!({ "cmd": cmd }))
            .map(|v| serde_json::to_string_pretty(&v).unwrap_or_default())
            .unwrap_or_else(|e| format!("(agent not reachable: {e})"));
        write(file, &text);
    }
    // Configuration and paired machines: public keys and grants only (no private keys exist in
    // these files; the device key lives in the OS key store or its own 0600 file, not copied).
    let config_dir = if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library/Application Support/Crosspane"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|c| c.join("crosspane"))
    };
    if let Some(config_dir) = config_dir {
        for file in ["config.toml", "trust.json"] {
            if let Ok(text) = std::fs::read_to_string(config_dir.join(file)) {
                write(file, &text);
            }
        }
    }
    // Recent logs (they never record key contents, 04 §8) and versions.
    if cfg!(target_os = "macos") {
        let log = std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library/Logs/Crosspane/agent.log"));
        if let Some(text) = log.and_then(|p| std::fs::read_to_string(p).ok()) {
            let tail: Vec<&str> = text.lines().rev().take(5000).collect();
            write(
                "agent.log",
                &tail.into_iter().rev().collect::<Vec<_>>().join("\n"),
            );
        }
        write("system.txt", &run("/usr/bin/sw_vers", &[]));
    } else {
        write(
            "agent.log",
            &run(
                "journalctl",
                &[
                    "--user",
                    "-u",
                    "crosspane-agent",
                    "-n",
                    "5000",
                    "--no-pager",
                    "-o",
                    "short-iso",
                ],
            ),
        );
        write(
            "system.txt",
            &format!("{}{}", run("uname", &["-a"]), run("hyprctl", &["version"])),
        );
    }
    write(
        "crosspanectl.txt",
        &format!("crosspanectl {}\n", env!("CARGO_PKG_VERSION")),
    );

    let out = out.unwrap_or_else(|| PathBuf::from(format!("{name}.tar.gz")));
    let status = Process::new("tar")
        .arg("czf")
        .arg(&out)
        .arg("-C")
        .arg(std::env::temp_dir())
        .arg(&name)
        .status()
        .context("run tar")?;
    let _ = std::fs::remove_dir_all(&dir);
    if !status.success() {
        bail!("tar failed");
    }
    println!("{}", out.display());
    Ok(())
}

#[cfg(windows)]
fn diag(out: Option<PathBuf>) -> Result<()> {
    windows::diag(out)
}

/// `crosspanectl pick`: list `peer`'s windows, let the user choose one, pull it.
fn pick(peer: &str, menu: Option<&str>) -> Result<()> {
    let windows = call(&json!({"cmd": "windows_from", "peer": peer}))?;
    let windows: Vec<(u64, String)> = windows
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|w| {
            let id = w["id"].as_u64()?;
            let app = w["app"].as_str().unwrap_or("");
            let title = w["title"].as_str().unwrap_or("");
            let line = if title.is_empty() {
                app.to_owned()
            } else {
                format!("{title} — {app}")
            };
            // One line per window; the menu shows the text, the id rides at the end.
            Some((id, line.replace(['\n', '\t'], " ")))
        })
        .collect();
    if windows.is_empty() {
        bail!("{peer} has no windows to show");
    }
    let Some(index) = choose(&windows, menu)? else {
        return Ok(());
    };
    let (window, _) = &windows[index];
    let result = call(&json!({"cmd": "pull", "peer": peer, "window": window}))?;
    println!("{}", result.as_str().unwrap_or(&result.to_string()));
    Ok(())
}

/// Show `windows` in a menu; the index of the chosen one, or `None` if cancelled.
fn choose(windows: &[(u64, String)], menu: Option<&str>) -> Result<Option<usize>> {
    let lines: Vec<String> = windows
        .iter()
        .enumerate()
        .map(|(i, (_, text))| format!("{}. {text}", i + 1))
        .collect();
    #[cfg(windows)]
    let chosen = match menu {
        Some(menu) => run_menu(menu, &lines)?,
        None => windows::choose_terminal(&lines)?,
    };
    #[cfg(unix)]
    let chosen = if cfg!(target_os = "macos") && menu.is_none() {
        choose_macos(&lines)?
    } else {
        let menu = match menu {
            Some(m) => m.to_owned(),
            None => default_menu()
                .context("no menu program: install walker, fuzzel, wofi or rofi, or pass --menu")?,
        };
        run_menu(&menu, &lines)?
    };
    let Some(chosen) = chosen else {
        return Ok(None);
    };
    let number: usize = chosen
        .split('.')
        .next()
        .and_then(|n| n.trim().parse().ok())
        .context("the menu returned something that isn't one of the windows")?;
    Ok(number.checked_sub(1).filter(|i| *i < windows.len()))
}

#[cfg(unix)]
fn default_menu() -> Option<String> {
    let found = |name: &str| {
        std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
    };
    [
        ("walker", "walker --dmenu"),
        ("fuzzel", "fuzzel --dmenu"),
        ("wofi", "wofi --dmenu"),
        ("rofi", "rofi -dmenu"),
    ]
    .into_iter()
    .find(|(bin, _)| found(bin))
    .map(|(_, cmd)| cmd.to_owned())
}

fn run_menu(menu: &str, lines: &[String]) -> Result<Option<String>> {
    use std::process::{Command as Process, Stdio};
    #[cfg(unix)]
    let mut process = {
        let mut process = Process::new("sh");
        process.arg("-c").arg(menu);
        process
    };
    #[cfg(windows)]
    let mut process = {
        let root = std::env::var_os("SystemRoot").context("SystemRoot is not set")?;
        let mut process = Process::new(PathBuf::from(root).join("System32/cmd.exe"));
        process.arg("/d").arg("/s").arg("/c").arg(menu);
        process
    };
    let mut child = process
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("run {menu}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        for line in lines {
            writeln!(stdin, "{line}")?;
        }
    }
    let output = child.wait_with_output()?;
    let chosen = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok((!chosen.is_empty()).then_some(chosen))
}

#[cfg(unix)]
fn choose_macos(lines: &[String]) -> Result<Option<String>> {
    // AppleScript's `choose from list` needs no special permission.
    let items = lines
        .iter()
        .map(|l| format!("\"{}\"", l.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(", ");
    let script = format!(
        "set c to choose from list {{{items}}} with title \"Crosspane\" with prompt \"Show which window here?\"\nif c is false then return \"\"\nreturn item 1 of c"
    );
    let output = std::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .output()
        .context("run osascript")?;
    let chosen = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok((!chosen.is_empty()).then_some(chosen))
}

fn print_result(command: &Command, result: &Value) {
    match command {
        Command::Status => print_status(result),
        Command::Pair {
            action: PairAction::Status,
        } => {
            println!("phase: {}", result["phase"].as_str().unwrap_or("idle"));
            if let Some(sas) = result["sas"].as_str() {
                println!(
                    "code:  {} {}",
                    &sas[..3.min(sas.len())],
                    &sas[3.min(sas.len())..]
                );
            }
            for (i, c) in result["candidates"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
            {
                println!("  {}) {}", i + 1, c.as_str().unwrap_or(""));
            }
            if let Some(peer) = result["peer"].as_str() {
                println!("peer:  {peer}");
            }
            if let Some(e) = result["error"].as_str() {
                println!("error: {e}");
            }
        }
        Command::Pair {
            action: PairAction::Scan,
        } => {
            let offers = result.as_array().cloned().unwrap_or_default();
            if offers.is_empty() {
                println!("no machine is pairing (open a pairing window there first)");
            }
            for o in offers {
                println!(
                    "{}  {}",
                    o["name"].as_str().unwrap_or("?"),
                    o["addr"].as_str().unwrap_or("?")
                );
            }
        }
        Command::Windows { .. } => {
            for w in result.as_array().into_iter().flatten() {
                println!(
                    "{:>12}  {:<24} {}x{}  {}",
                    w["id"],
                    w["app"].as_str().unwrap_or(""),
                    w["size"][0],
                    w["size"][1],
                    w["title"].as_str().unwrap_or("")
                );
            }
        }
        Command::RequestPermissions | Command::AskPermission { .. } => {
            println!("{}", ask_wording(result));
        }
        Command::ResetPermission { permission } => println!(
            "reset Crosspane's {0} entry; now run: crosspanectl ask-permission {0}",
            permission.token()
        ),
        _ => println!("{}", result.as_str().unwrap_or(&result.to_string())),
    }
}

/// What an ask showed, in words (the agent answers `{permission, shown, note?}`).
fn ask_wording(result: &Value) -> String {
    let Some(shown) = result["shown"].as_str() else {
        return result.as_str().unwrap_or(&result.to_string()).to_owned();
    };
    let permission = result["permission"]
        .as_str()
        .unwrap_or("permission")
        .replace('_', " ");
    let mut text = match shown {
        "prompt" => format!("asked for {permission}: answer the request on this machine"),
        "pane" => format!(
            "opened System Settings at {permission}: turn Crosspane on there (its request was \
             already answered)"
        ),
        "prompt_then_pane" => format!(
            "asked for {permission}: answer the request, or turn Crosspane on in the System \
             Settings pane that opens"
        ),
        _ => result["note"]
            .as_str()
            .map_or_else(|| format!("{permission} is already granted"), str::to_owned),
    };
    if shown != "nothing"
        && let Some(note) = result["note"].as_str()
    {
        text.push_str(&format!(" ({note})"));
    }
    text
}

fn print_status(s: &Value) {
    let short = |v: &Value| {
        v.as_str()
            .map(|t| t.chars().take(12).collect::<String>())
            .unwrap_or_default()
    };
    println!(
        "{} ({})  listening on {}",
        s["name"].as_str().unwrap_or("?"),
        short(&s["node"]),
        s["listening"].as_str().unwrap_or("?")
    );
    println!(
        "  session: {}   input gate: {}",
        s["session"].as_str().unwrap_or("?"),
        if s["gate_open"] == json!(true) {
            "open"
        } else {
            "closed"
        }
    );
    println!("  backends: {}", s["backends"].as_str().unwrap_or("?"));
    if let Some(label) = format_drag_label(s) {
        println!("{label}");
    }
    if let Some(label) = format_clipboard_status(s) {
        println!("{label}");
    }
    let missing: Vec<&str> = s["permissions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["state"].as_str() != Some("Granted"))
        .filter_map(|p| p["permission"].as_str())
        .collect();
    if !missing.is_empty() {
        println!("  permissions missing: {}", missing.join(", "));
    }
    for d in s["displays"].as_array().into_iter().flatten() {
        println!(
            "  display {} {}  {}x{} @{} scale  {}x{} mm",
            d["id"],
            d["name"].as_str().unwrap_or(""),
            d["pixels"][0],
            d["pixels"][1],
            d["scale"],
            d["mm"][0],
            d["mm"][1]
        );
    }
    let peers = s["peers"].as_array().cloned().unwrap_or_default();
    if peers.is_empty() {
        println!("  no peers");
    }
    for p in peers {
        let rtt = p["rtt_ms"]
            .as_f64()
            .map(|r| format!("{r:.2} ms"))
            .unwrap_or_else(|| "-".into());
        let link = p["link"]
            .as_str()
            .map_or_else(String::new, |link| format!("  link {link}"));
        println!(
            "  peer {} ({})  {}  rtt {}{link}  {}",
            p["name"].as_str().unwrap_or("?"),
            short(&p["node"]),
            if p["connected"] == json!(true) {
                "connected"
            } else {
                "offline"
            },
            rtt,
            format_peer_drag(&p)
        );
        if let Some(grants) = p["grants"].as_array() {
            let grants: Vec<&str> = grants.iter().filter_map(Value::as_str).collect();
            println!(
                "    allowed here: {}",
                if grants.is_empty() {
                    "nothing".to_owned()
                } else {
                    grants.join(", ")
                }
            );
        }
        if p["speaker_in_use"] == json!(true) {
            println!("    playing sound on this machine's speakers now");
        }
    }
    match s["audio"]["enabled"].as_bool() {
        Some(true) => println!("  audio: speakers (microphones are not supported yet)"),
        Some(false) => println!("  audio: off"),
        None => {}
    }
    for l in s["layout"].as_array().into_iter().flatten() {
        println!(
            "  layout {}:{} at ({:.0}, {:.0}) mm v{}",
            l["node"].as_str().unwrap_or("?"),
            l["display"],
            l["origin_mm"][0].as_f64().unwrap_or(0.0),
            l["origin_mm"][1].as_f64().unwrap_or(0.0),
            l["version"]
        );
    }
    for p in s["projections"].as_array().into_iter().flatten() {
        println!(
            "  projection {}:{}  {}",
            p["source"].as_str().unwrap_or("?"),
            p["projection"],
            p["text"].as_str().unwrap_or("")
        );
        let r = &p["received"];
        if r.is_object() {
            let latency = r["latency_ms"]
                .as_f64()
                .map_or_else(String::new, |ms| format!(", latency {ms:.0} ms"));
            println!(
                "    received {} frames, {:.1} MB, last {} ms ago{latency}",
                r["frames"],
                r["bytes"].as_f64().unwrap_or(0.0) / 1e6,
                r["last_ms_ago"]
            );
        }
    }
    for n in s["notices"].as_array().into_iter().flatten() {
        println!("  notice: {}", n.as_str().unwrap_or(""));
    }
}

fn format_peer_drag(peer: &Value) -> &'static str {
    if peer["drag"] == "on" {
        "drag:on"
    } else {
        "drag:off"
    }
}

fn format_drag_label(status: &Value) -> Option<String> {
    status["drag"]
        .as_str()
        .map(|label| format!("  drag: {label}"))
}

fn format_clipboard_status(status: &Value) -> Option<String> {
    let clipboard = status.get("clipboard")?.as_object()?;
    let count = |name| clipboard.get(name).and_then(Value::as_u64).unwrap_or(0);
    Some(format!(
        "Clipboard offers: {} sent, {} received · Fetches: {} served, {} made · Failures: Expired {}, Locked {}, Not granted {}, Too large {}, Unavailable {}",
        count("offers_sent"),
        count("offers_received"),
        count("fetches_served"),
        count("fetches_made"),
        count("expired"),
        count("locked"),
        count("not_granted"),
        count("too_large"),
        count("unavailable"),
    ))
}

#[cfg(test)]
mod drag_status_tests {
    use super::*;

    #[test]
    fn clipboard_status_defaults_and_formats_only_fixed_numeric_fields() {
        for old in [json!({}), json!({"clipboard":null})] {
            let parsed: Value = serde_json::from_str(&old.to_string()).unwrap();
            assert_eq!(format_clipboard_status(&parsed), None);
        }
        let status: Value = serde_json::from_str(r#"{"clipboard":{"offers_sent":7,"offers_received":3,"fetches_served":2,"fetches_made":4,"expired":1,"locked":2,"not_granted":3,"too_large":4,"unavailable":5,"content":"never display this fixture","bytes":777777}}"#).unwrap();
        let text = format_clipboard_status(&status).expect("clipboard counts");
        assert!(text.contains("Clipboard offers: 7 sent, 3 received"));
        assert!(text.contains("Expired 1") && text.contains("Unavailable 5"));
        assert!(!text.contains("fixture") && !text.contains("777777"));
        let partial = format_clipboard_status(&json!({"clipboard":{}})).unwrap();
        assert!(partial.contains("0 sent, 0 received"));
    }

    #[test]
    fn formatted_status_names_each_peers_drag_and_the_active_engine_label() {
        assert_eq!(format_peer_drag(&json!({"drag":"on"})), "drag:on");
        assert_eq!(format_peer_drag(&json!({"drag":"off"})), "drag:off");
        assert_eq!(format_peer_drag(&json!({})), "drag:off");
        assert_eq!(
            format_drag_label(&json!({"drag":"Dragging to Moon"})),
            Some("  drag: Dragging to Moon".into())
        );
        assert_eq!(format_drag_label(&json!({"drag":null})), None);
    }
}
