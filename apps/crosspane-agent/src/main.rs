//! The per-session daemon wiring engine, platform, and transport (WP-1.37).

mod agent;
mod config;
mod ctl;
mod keys;
mod net;
mod paths;
mod platform;
mod trust;

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use crosspane_engine::{Engine, EngineConfig};
use crosspane_input::journal::FileJournal;
use crosspane_platform::{Permission, PermissionState, Permissions};
use crosspane_protocol::msg::{Capability, Hello};
use crosspane_security::identity::{DeviceIdentity, node_id};
use crosspane_security::trust::{PeerEntry, default_grants};

use crate::config::Config;
use crate::paths::Paths;

#[derive(Debug, Parser)]
#[command(name = "crosspane-agent", version, about = "Crosspane per-session agent")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the agent (the default).
    Run,
    /// Print this device's node id and public key (SPKI, hex).
    Identity,
    /// Manage trusted peers.
    Trust {
        #[command(subcommand)]
        action: TrustAction,
    },
    /// Print the OS permission status as one JSON line.
    Permissions {
        /// Start the OS's grant flow for every permission not yet granted.
        #[arg(long)]
        request: bool,
        /// Also open the System Settings pane for every permission not yet granted (macOS).
        #[arg(long)]
        open_settings: bool,
    },
}

#[derive(Debug, Subcommand)]
enum TrustAction {
    /// Pin a peer's public key, verified out of band (e.g. read from `crosspane-agent identity`
    /// on the peer). Prefer SAS pairing once available.
    Add {
        name: String,
        /// The peer's SPKI in hex.
        spki: String,
        /// Let this peer control this node's keyboard and mouse.
        #[arg(long)]
        allow_input: bool,
    },
    /// List trusted peers.
    List,
    /// Forget a peer (by node-id prefix or name).
    Remove { peer: String },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    if is_elevated() {
        bail!("crosspane-agent refuses to run elevated (04 §7)");
    }
    match Cli::parse().command.unwrap_or(Command::Run) {
        Command::Run => run(),
        Command::Identity => identity(),
        Command::Trust { action } => trust(action),
        Command::Permissions { request, open_settings } => permissions(request, open_settings),
    }
}

fn load_identity(paths: &Paths, config: &Config, platform: Option<&platform::Platform>) -> Result<DeviceIdentity> {
    let store = platform.and_then(|p| p.keystore.as_deref());
    keys::load_or_create(store, &paths.key_file(), config.allow_file_keystore)
}

fn run() -> Result<()> {
    let paths = Paths::new()?;
    let config = Config::load(&paths)?;
    let mut platform = platform::create()?;
    tracing::info!(backends = ?platform, "platform ready");
    let identity = Arc::new(load_identity(&paths, &config, Some(&platform))?);
    let node = identity.node();
    tracing::info!(node = %node, name = %config.name, "identity");
    let trust = trust::SharedTrust::load(paths.trust_file())?;

    let local_displays = platform.displays.displays().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "could not list displays");
        Vec::new()
    });

    // Crash recovery runs first (04 §8 invariant 2): Engine::new returns it as outputs.
    let journal = FileJournal::open(&paths.journal_file()).context("open input journal")?;
    let mut engine_config = EngineConfig::new(node);
    engine_config.push_to_cross = std::time::Duration::from_millis(config.push_to_cross_ms.min(200));
    let (engine, startup) = Engine::new(engine_config, Box::new(journal), platform::now())
        .context("start engine")?;

    let (tx, rx) = std::sync::mpsc::channel();
    agent::subscribe_platform(&mut platform, &tx);
    ctl::serve(&paths.control_socket(), tx.clone())?;

    let hello = Hello {
        minor: crosspane_protocol::PROTOCOL_MINOR,
        name: config.name.clone(),
        features: vec!["e1".to_owned()],
        displays: local_displays.clone(),
    };
    let pins: Arc<dyn crosspane_transport::PinStore> = Arc::new(trust.clone());
    let net = net::Net::start(config.port, identity, pins, hello, tx.clone())?;
    for peer in &config.peers {
        net.dial(peer.addr);
    }
    let agent = agent::Agent::new(node, config.name, engine, platform, net, trust, local_displays);
    run_loop(agent, startup, rx, tx)
}

#[cfg(target_os = "macos")]
fn run_loop(
    agent: agent::Agent,
    startup: Vec<crosspane_engine::Output>,
    rx: std::sync::mpsc::Receiver<agent::Event>,
    tx: std::sync::mpsc::Sender<agent::Event>,
) -> Result<()> {
    // AppKit owns the main thread; the engine loop runs beside it.
    std::thread::Builder::new()
        .name("engine".into())
        .spawn(move || {
            agent.run(startup, &rx);
            drop(tx);
            std::process::exit(0);
        })
        .context("spawn engine thread")?;
    crosspane_platform_macos::main_thread::run_app()?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn run_loop(
    agent: agent::Agent,
    startup: Vec<crosspane_engine::Output>,
    rx: std::sync::mpsc::Receiver<agent::Event>,
    tx: std::sync::mpsc::Sender<agent::Event>,
) -> Result<()> {
    // Keep a sender alive so the loop only ends on shutdown.
    let _keep = tx;
    agent.run(startup, &rx);
    Ok(())
}

fn identity() -> Result<()> {
    let paths = Paths::new()?;
    let config = Config::load(&paths)?;
    let platform = platform::create().ok();
    let identity = load_identity(&paths, &config, platform.as_ref())?;
    println!("name: {}", config.name);
    println!("node: {}", identity.node());
    println!("spki: {}", keys::hex(identity.spki()));
    Ok(())
}

fn trust(action: TrustAction) -> Result<()> {
    let paths = Paths::new()?;
    let trust = trust::SharedTrust::load(paths.trust_file())?;
    match action {
        TrustAction::Add { name, spki, allow_input } => {
            let spki = keys::unhex(&spki)?;
            let node = node_id(&spki);
            let mut granted = default_grants();
            if allow_input {
                granted.insert(Capability::InputAccept);
            }
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
            trust.update(|t| {
                t.pin(PeerEntry { node, spki, name: name.clone(), granted, paired_at_ms: now_ms })
                    .map_err(|e| anyhow::anyhow!("{e}"))
            })?;
            println!("trusted {name} ({node})");
        }
        TrustAction::List => trust.with(|t| {
            for entry in t.peers() {
                println!("{}  {}  {:?}", entry.node, entry.name, entry.granted);
            }
        }),
        TrustAction::Remove { peer } => {
            let removed = trust.update(|t| {
                let node = t
                    .peers()
                    .into_iter()
                    .find(|e| e.name == peer || e.node.to_string().starts_with(&peer))
                    .map(|e| e.node);
                Ok(node.and_then(|n| t.forget(n)))
            })?;
            match removed {
                Some(entry) => println!("forgot {} ({})", entry.name, entry.node),
                None => bail!("no trusted peer matches {peer:?}"),
            }
        }
    }
    Ok(())
}

fn permissions(request: bool, open_settings: bool) -> Result<()> {
    let mut perms = platform_permissions();
    let mut report = serde_json::Map::new();
    for permission in perms.required() {
        let mut state = perms.state(permission);
        if state != PermissionState::Granted {
            if request {
                perms.request(permission)?;
            }
            if open_settings {
                open_settings_pane(permission);
            }
            state = perms.state(permission);
        }
        report.insert(permission_name(permission).into(), state_name(state).into());
    }
    println!("{}", serde_json::Value::Object(report));
    Ok(())
}

fn permission_name(permission: Permission) -> &'static str {
    match permission {
        Permission::ScreenRecording => "screen_recording",
        Permission::Accessibility => "accessibility",
        Permission::InputMonitoring => "input_monitoring",
        _ => "other",
    }
}

fn state_name(state: PermissionState) -> &'static str {
    match state {
        PermissionState::Granted => "granted",
        PermissionState::NotGranted => "not_granted",
        PermissionState::Unknown => "unknown",
    }
}

#[cfg(target_os = "macos")]
fn platform_permissions() -> Box<dyn Permissions> {
    Box::new(crosspane_platform_macos::permissions::MacPermissions::new())
}

#[cfg(target_os = "linux")]
fn platform_permissions() -> Box<dyn Permissions> {
    Box::new(crosspane_platform_linux::permissions::LinuxPermissions)
}

#[cfg(target_os = "macos")]
fn open_settings_pane(permission: Permission) {
    let url = crosspane_platform_macos::permissions::MacPermissions::settings_url(permission);
    if let Err(e) = std::process::Command::new("/usr/bin/open").arg(url).status() {
        tracing::warn!(error = %e, "could not open System Settings");
    }
}

#[cfg(not(target_os = "macos"))]
fn open_settings_pane(_permission: Permission) {}

/// True when running as root (uid 0), which the agent refuses (04 §7).
fn is_elevated() -> bool {
    std::fs::metadata("/proc/self")
        .map(|m| std::os::unix::fs::MetadataExt::uid(&m) == 0)
        .unwrap_or_else(|_| {
            std::process::Command::new("/usr/bin/id")
                .arg("-u")
                .output()
                .map(|o| o.stdout.starts_with(b"0\n"))
                .unwrap_or(false)
        })
}
