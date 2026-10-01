//! CLI for the running agent: status, release, panic, re-arm, layout, dial. It speaks the agent's
//! control socket (one JSON request and response per line).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};

#[derive(Debug, Parser)]
#[command(
    name = "crosspanectl",
    version,
    about = "Control the running Crosspane agent"
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
    /// Collect a diagnostics bundle (status, config, paired machines, recent logs, versions) into
    /// a .tar.gz for a bug report. It never contains private keys or typed text.
    Diag {
        /// Where to write the bundle (default: a timestamped file in the current directory).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Unpair a peer (name or node-id prefix): its key is forgotten and the connection ends now.
    Forget { peer: String },
    /// Show the OS permission dialogs for whatever this machine still lacks (macOS: Screen
    /// Recording, Accessibility, Input Monitoring).
    RequestPermissions,
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
    /// Let a peer use a capability here (input, share, browse, present), or stop with --off.
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
        projection: u64,
        /// The source peer (default: this machine).
        #[arg(long)]
        source: Option<String>,
    },
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
    let cli = Cli::parse();
    let request = match &cli.command {
        Command::Status => json!({"cmd": "status"}),
        Command::Release => json!({"cmd": "release"}),
        Command::Panic => json!({"cmd": "panic"}),
        Command::Rearm => json!({"cmd": "rearm"}),
        Command::Restart => json!({"cmd": "restart"}),
        Command::Diag { out } => return diag(out.clone()),
        Command::Forget { peer } => json!({"cmd": "forget", "peer": peer}),
        Command::RequestPermissions => json!({"cmd": "ask_permissions"}),
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

/// The result of a request, or its error.
fn call(request: &Value) -> Result<Value> {
    let response = exchange(request)?;
    if response["ok"] != json!(true) {
        bail!("{}", response["error"].as_str().unwrap_or("request failed"));
    }
    Ok(response["result"].clone())
}

/// `crosspanectl diag`: gather what a bug report needs into one archive.
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
    let mut child = Process::new("sh")
        .arg("-c")
        .arg(menu)
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
        _ => println!("{}", result.as_str().unwrap_or(&result.to_string())),
    }
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
            "  peer {} ({})  {}  rtt {}{link}",
            p["name"].as_str().unwrap_or("?"),
            short(&p["node"]),
            if p["connected"] == json!(true) {
                "connected"
            } else {
                "offline"
            },
            rtt
        );
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
