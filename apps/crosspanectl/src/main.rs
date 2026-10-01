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
    /// Put a peer (name or node-id prefix) on a side of this machine.
    Layout { peer: String, side: Side },
    /// Connect to a peer at ADDR (host:port) now.
    Dial { addr: String },
    /// List this machine's windows (ids for `project`).
    Windows,
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
    /// Join the pairing window of the machine at ADDR (host:port, its normal port).
    Join {
        addr: String,
        #[arg(long)]
        allow_input: bool,
    },
    /// Show the pairing state (the code, or the candidates to pick from).
    Status,
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
        Command::Layout { peer, side } => {
            let side = format!("{side:?}").to_lowercase();
            json!({"cmd": "layout", "peer": peer, "side": side})
        }
        Command::Dial { addr } => {
            use std::net::ToSocketAddrs;
            let resolved = addr
                .to_socket_addrs()
                .with_context(|| format!("resolve {addr}"))?
                .next()
                .context("no address")?;
            json!({"cmd": "dial", "addr": resolved.to_string()})
        }
        Command::Windows => json!({"cmd": "windows"}),
        Command::Pair { action } => match action {
            PairAction::Listen { allow_input } => {
                json!({"cmd": "pair_listen", "allow_input": allow_input})
            }
            PairAction::Join { addr, allow_input } => {
                use std::net::ToSocketAddrs;
                let resolved = addr
                    .to_socket_addrs()
                    .with_context(|| format!("resolve {addr}"))?
                    .next()
                    .context("no address")?;
                json!({"cmd": "pair_join", "addr": resolved.to_string(), "allow_input": allow_input})
            }
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
    let path = socket_path()?;
    let stream = UnixStream::connect(&path)
        .with_context(|| format!("is crosspane-agent running? ({})", path.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut writer = stream.try_clone()?;
    writeln!(writer, "{request}")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let response: Value = serde_json::from_str(&line).context("bad response from agent")?;
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
        Command::Windows => {
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
        println!(
            "  peer {} ({})  {}  rtt {}",
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
    }
    for n in s["notices"].as_array().into_iter().flatten() {
        println!("  notice: {}", n.as_str().unwrap_or(""));
    }
}
