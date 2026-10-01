//! The per-session daemon wiring engine, platform, and transport (WP-1.37).

mod agent;
mod config;
mod ctl;
mod keys;
mod media;
mod net;
mod pairing;
mod paths;
mod platform;
mod revocations;
mod tray;
mod trust;
// Twin-or-mirror parking is used only by macOS builds with `private-vdisplay`; its tests run
// everywhere.
#[cfg_attr(
    not(all(target_os = "macos", feature = "private-vdisplay")),
    allow(dead_code)
)]
mod twin;

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
#[command(
    name = "crosspane-agent",
    version,
    about = "Crosspane per-session agent"
)]
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
        Command::Permissions {
            request,
            open_settings,
        } => permissions(request, open_settings),
    }
}

fn load_identity(
    paths: &Paths,
    config: &Config,
    store: Option<&dyn crosspane_platform::KeyStore>,
) -> Result<DeviceIdentity> {
    let store = store.filter(|_| !config.force_file_keystore);
    keys::load_or_create(
        store,
        &paths.key_file(),
        config.allow_file_keystore || config.force_file_keystore,
    )
}

/// Run `f` (a thread's whole body); if it panics, exit the process. A dead engine or media thread
/// in a live process would leave input or windows stranded; exiting lets the service manager
/// restart the agent, whose startup recovery releases input and restores parked windows (04 §8).
pub fn exit_on_panic<R>(thread: &str, f: impl FnOnce() -> R) -> R {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(value) => value,
        Err(_) => {
            tracing::error!(
                thread,
                "thread panicked; exiting so the agent restarts and recovers"
            );
            std::process::exit(101);
        }
    }
}

fn run() -> Result<()> {
    let paths = Paths::new()?;
    let config = Config::load(&paths)?;
    // One agent per user, settled before anything touches the session: creating the platform
    // recovers parked windows, which would un-hide a running agent's projections. The lock is
    // held for the life of the process (close-on-exec, so a restart in place takes it again).
    let lock_path = paths.state_dir.join("agent.lock");
    let _lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("open {}", lock_path.display()))?;
    _lock.try_lock().map_err(|_| {
        anyhow::anyhow!(
            "another crosspane-agent is already running for this user ({} is locked)",
            lock_path.display()
        )
    })?;
    let mut platform = platform::create(&paths.state_dir, &config)?;
    tracing::info!(backends = ?platform, "platform ready");
    let identity = Arc::new(load_identity(
        &paths,
        &config,
        platform.keystore.as_deref(),
    )?);
    let node = identity.node();
    tracing::info!(node = %node, name = %config.name, "identity");
    let trust = trust::SharedTrust::load(paths.trust_file())?;

    let local_displays = platform.displays.displays().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "could not list displays");
        Vec::new()
    });

    // Crash recovery runs first (04 §8 invariant 2): Engine::new returns it as outputs.
    let journal = FileJournal::open(&paths.journal_file()).context("open input journal")?;
    let e2_journal =
        FileJournal::open(&paths.e2_journal_file()).context("open projection input journal")?;
    let mut engine_config = EngineConfig::new(node);
    engine_config.push_to_cross =
        std::time::Duration::from_millis(config.push_to_cross_ms.min(200));
    for (name, profile) in &config.remap {
        match trust.with(|t| t.peers().iter().find(|e| &e.name == name).map(|e| e.node)) {
            Some(node) => {
                engine_config.remap.insert(node, *profile);
            }
            None => tracing::warn!(peer = %name, "remap: no paired peer with that name"),
        }
    }
    let (engine, startup) = Engine::new(
        engine_config,
        Box::new(journal),
        Box::new(e2_journal),
        platform::now(),
    )
    .context("start engine")?;

    let (tx, rx) = std::sync::mpsc::channel();
    agent::subscribe_platform(&mut platform, &tx);
    ctl::serve(&paths.control_socket(), tx.clone())?;

    // E2 video (WP-2.14): this node's encoder/decoder, if it has one and video isn't turned off.
    let video = media::VideoSetup {
        codecs: if config.video_mbps != Some(0) {
            platform::video_codecs()
        } else {
            None
        },
    };
    // `cursor`: this node shows the source's cursor shapes on its proxies (WP-2.16).
    let mut features = vec!["e1".to_owned(), "cursor".to_owned()];
    if video.codecs.is_some() {
        features.push("h264".to_owned());
    }
    let hello = Hello {
        minor: crosspane_protocol::PROTOCOL_MINOR,
        name: config.name.clone(),
        features,
        displays: local_displays.clone(),
    };
    let pins: Arc<dyn crosspane_transport::PinStore> = Arc::new(trust.clone());
    let net = net::Net::start(config.port, identity.clone(), pins, hello, tx.clone())?;
    stop_on_signal(&net.runtime(), tx.clone());
    for peer in &config.peers {
        net.dial(peer.addr);
    }

    // E2: the proxy window host owns the main thread (winit's rule on macOS); without a display
    // the node can still project its own windows, just not show others'.
    let host = match crosspane_render::proxy::ProxyHost::new() {
        Ok(host) => Some(host),
        Err(e) => {
            tracing::warn!(error = %e, "no proxy window host: this node can't show projected windows");
            None
        }
    };
    let proxy_ids = media::ProxyIds::default();
    let e2 = agent::E2Wiring {
        source_media: media::start_source(net.transport(), video.clone()),
        dest_media: media::start_destination(
            host.as_ref().map(|(_, handle)| handle.clone()),
            proxy_ids.clone(),
            tx.clone(),
            video,
        ),
        host: host.as_ref().map(|(_, handle)| handle.clone()),
        proxy_ids,
        events: tx.clone(),
        crossing: config.crossing,
        latency_overlay: config.latency_overlay,
        video_mbps: config.video_mbps,
        identity,
        port: config.port,
        revocations: revocations::Issued::load(revocations::file_beside(&paths.trust_file())),
    };
    let mut agent = agent::Agent::new(
        node,
        config.name,
        engine,
        platform,
        net,
        trust,
        local_displays,
        e2,
    );
    agent.start_discovery();
    run_loop(agent, startup, rx, tx, host.map(|(host, _)| host))
}

/// SIGTERM or SIGINT asks the engine loop to stop cleanly (`Agent::shutdown`); a second one exits at
/// once, in case the clean stop hangs.
fn stop_on_signal(runtime: &tokio::runtime::Handle, events: std::sync::mpsc::Sender<agent::Event>) {
    use tokio::signal::unix::{SignalKind, signal};
    runtime.spawn(async move {
        let (Ok(mut term), Ok(mut int)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) else {
            tracing::warn!("no signal handlers: stopping the agent skips the clean shutdown");
            return;
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
        let _ = events.send(agent::Event::Shutdown);
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
        std::process::exit(1);
    });
}

/// Run the engine loop on its own thread and the proxy host (if any) on this, the main thread.
/// Without a host on Linux, the engine loop runs here; on macOS the AppKit loop always owns the
/// main thread.
fn run_loop(
    agent: agent::Agent,
    startup: Vec<crosspane_engine::Output>,
    rx: std::sync::mpsc::Receiver<agent::Event>,
    tx: std::sync::mpsc::Sender<agent::Event>,
    host: Option<crosspane_render::proxy::ProxyHost>,
) -> Result<()> {
    match host {
        Some(host) => {
            let host_tx = tx.clone();
            std::thread::Builder::new()
                .name("engine".into())
                .spawn(move || {
                    exit_on_panic("engine", || agent.run(startup, &rx));
                    drop(tx);
                    std::process::exit(0);
                })
                .context("spawn engine thread")?;
            host.run(Box::new(move |event| {
                let _ = host_tx.send(agent::Event::Host(event));
            }))
            .context("proxy host")?;
            Ok(())
        }
        None => {
            #[cfg(target_os = "macos")]
            {
                std::thread::Builder::new()
                    .name("engine".into())
                    .spawn(move || {
                        exit_on_panic("engine", || agent.run(startup, &rx));
                        drop(tx);
                        std::process::exit(0);
                    })
                    .context("spawn engine thread")?;
                crosspane_platform_macos::main_thread::run_app()?;
                Ok(())
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _keep = tx;
                agent.run(startup, &rx);
                Ok(())
            }
        }
    }
}

fn identity() -> Result<()> {
    let paths = Paths::new()?;
    let config = Config::load(&paths)?;
    let store = platform::keystore();
    let identity = load_identity(&paths, &config, store.as_deref())?;
    println!("name: {}", config.name);
    println!("node: {}", identity.node());
    println!("spki: {}", keys::hex(identity.spki()));
    Ok(())
}

fn trust(action: TrustAction) -> Result<()> {
    let paths = Paths::new()?;
    let trust = trust::SharedTrust::load(paths.trust_file())?;
    match action {
        TrustAction::Add {
            name,
            spki,
            allow_input,
        } => {
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
                t.pin(PeerEntry {
                    node,
                    spki,
                    name: name.clone(),
                    granted,
                    paired_at_ms: now_ms,
                })
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
pub(crate) fn open_settings_pane(permission: Permission) {
    let url = crosspane_platform_macos::permissions::MacPermissions::settings_url(permission);
    if let Err(e) = std::process::Command::new("/usr/bin/open")
        .arg(url)
        .status()
    {
        tracing::warn!(error = %e, "could not open System Settings");
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn open_settings_pane(_permission: Permission) {}

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
