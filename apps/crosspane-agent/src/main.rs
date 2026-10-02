//! The per-session daemon wiring engine, platform, and transport (WP-1.37).

mod agent;
mod audio;
mod config;
mod ctl;
mod keys;
mod lifecycle;
mod media;
mod net;
mod pairing;
mod paths;
mod platform;
mod revocations;
mod tray;
mod trust;
// Twin-or-mirror parking: Hyprland, and macOS builds with `private-vdisplay`.
#[cfg_attr(
    not(any(
        target_os = "linux",
        all(target_os = "macos", feature = "private-vdisplay")
    )),
    allow(dead_code)
)]
mod twin;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
    /// Delete this machine's identity and pairings after a verified clean stop.
    EraseIdentity {
        /// Keep the pairings and issued revocations (repair and tests).
        #[arg(long)]
        keep_trust: bool,
    },
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

/// Set when the agent stops because it can't carry on (its compositor or window host is gone): it
/// then exits with a failure status, so the service manager starts it again.
static RESTART: AtomicBool = AtomicBool::new(false);

/// The exit status after a clean stop: 75 (`EX_TEMPFAIL`) when [`RESTART`] is set.
fn stop_status() -> i32 {
    if RESTART.load(Ordering::Acquire) {
        75
    } else {
        0
    }
}

/// Stop cleanly and exit with a failure status, killing the process if that takes too long.
fn stop_for_restart(events: &std::sync::mpsc::Sender<agent::Event>) {
    RESTART.store(true, Ordering::Release);
    let _ = events.send(agent::Event::Shutdown);
    platform::exit_deadline(std::time::Duration::from_secs(5));
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
    dispatch(Cli::parse().command.unwrap_or(Command::Run), run, one_shot)
}

fn dispatch(
    command: Command,
    run: impl FnOnce() -> Result<()>,
    one_shot: impl FnOnce(Command) -> Result<()>,
) -> Result<()> {
    match command {
        Command::Run => run(),
        command => one_shot(command),
    }
}

fn one_shot(command: Command) -> Result<()> {
    match command {
        Command::Run => bail!("run requires the lifecycle startup path"),
        Command::Identity => identity(),
        Command::EraseIdentity { keep_trust } => {
            let paths = Paths::new()?;
            let receipt = lifecycle::erase_identity(&paths, keep_trust, || {
                platform::keystore().context("standalone key store unavailable")
            });
            println!("{}", serde_json::to_string(&receipt)?);
            Ok(())
        }
        Command::Trust { action } => trust(action),
        Command::Permissions {
            request,
            open_settings,
        } => permissions(request, open_settings),
    }
}

/// The key store `config` lets the agent use, and whether the key file may stand in for it.
fn keystore_policy<'a>(
    config: &Config,
    store: Option<&'a dyn crosspane_platform::KeyStore>,
) -> (Option<&'a dyn crosspane_platform::KeyStore>, bool) {
    (
        store.filter(|_| !config.force_file_keystore),
        config.allow_file_keystore || config.force_file_keystore,
    )
}

/// The device identity for the one-shot `identity` command: a locked key store is an error.
fn load_identity(
    paths: &Paths,
    config: &Config,
    store: Option<&dyn crosspane_platform::KeyStore>,
) -> Result<DeviceIdentity> {
    let (store, allow_file) = keystore_policy(config, store);
    keys::load_or_create(store, &paths.key_file(), allow_file)
}

/// The device identity for `run`: a locked key store is waited out (the login keyring is often
/// still locked when the agent starts at login), until it unlocks or SIGTERM or SIGINT asks the
/// agent to stop. `None` means "stop requested while waiting": nothing was started yet, and the
/// caller returns, so the process exits with status 0. Otherwise the identity and where it came
/// from (for `status`).
fn load_identity_waiting(
    paths: &Paths,
    config: &Config,
    store: Option<&dyn crosspane_platform::KeyStore>,
    lifecycle: &mut lifecycle::Lifecycle,
) -> Result<Option<(DeviceIdentity, keys::KeySource)>> {
    let (store, allow_file) = keystore_policy(config, store);
    let startup = keys::load_or_create_waiting(
        store,
        &paths.key_file(),
        allow_file,
        &mut keys::SignalPacer::new(),
        || lifecycle.phase(lifecycle::Phase::WaitingForKeystore, None),
    )?;
    Ok(match startup {
        keys::Startup::Identity(identity, source) => Some((identity, source)),
        keys::Startup::Stopped => None,
    })
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

/// Ask the OS for every missing permission (04 §2 onboarding). The requests come from the agent's
/// own process, so the system dialogs name Crosspane. Returns what was asked for.
pub fn request_missing_permissions(platform: &mut platform::Platform) -> Vec<&'static str> {
    let perms = platform.permissions.as_mut();
    let mut requested = Vec::new();
    for permission in perms.required() {
        if perms.state(permission) != PermissionState::Granted {
            match perms.request(permission) {
                Ok(()) => requested.push(permission_name(permission)),
                Err(e) => tracing::warn!(error = %e, ?permission, "permission request failed"),
            }
        }
    }
    requested
}

/// At most once a day, show the OS permission dialogs for anything missing, so a fresh install
/// asks on its own rather than waiting for the user to find the menu.
fn request_permissions_daily(state_dir: &std::path::Path, platform: &mut platform::Platform) {
    let marker = state_dir.join("permissions-requested");
    let recent = std::fs::metadata(&marker)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|at| at.elapsed().ok())
        .is_some_and(|age| age < std::time::Duration::from_secs(24 * 3600));
    if recent {
        return;
    }
    let requested = request_missing_permissions(platform);
    if !requested.is_empty() {
        tracing::info!(?requested, "asked the OS for missing permissions");
        if let Err(e) = std::fs::write(&marker, b"") {
            tracing::debug!(error = %e, "could not record the permission request");
        }
    }
}

fn run() -> Result<()> {
    let paths = Paths::new()?;
    // One agent per user, settled before anything touches the session: creating the platform
    // recovers parked windows, which would un-hide a running agent's projections. The lock is
    // held for the life of the process (close-on-exec, so a restart in place takes it again).
    let lock_path = paths.instance_lock();
    let _lock = lifecycle::instance_lock(&paths)
        .with_context(|| format!("open {}", lock_path.display()))?;
    _lock.try_lock().map_err(|_| {
        anyhow::anyhow!(
            "another crosspane-agent is already running for this user ({} is locked)",
            lock_path.display()
        )
    })?;
    let mut lifecycle = lifecycle::Lifecycle::start(&paths)?;
    let mut failure = lifecycle::Failure::Other;
    let started = start_agent(&paths, &mut lifecycle, &mut failure);
    let Some(started) = (match started {
        Ok(started) => started,
        Err(error) => {
            if !lifecycle.ready() {
                lifecycle.phase(lifecycle::Phase::Failed, Some(failure))?;
            }
            return Err(error);
        }
    }) else {
        return Ok(());
    };
    run_loop(started, lifecycle)
}

struct Started {
    agent: agent::Agent,
    startup: Vec<crosspane_engine::Output>,
    rx: std::sync::mpsc::Receiver<agent::Event>,
    tx: std::sync::mpsc::Sender<agent::Event>,
    host: Option<crosspane_render::proxy::ProxyHost>,
}

fn start_agent(
    paths: &Paths,
    lifecycle: &mut lifecycle::Lifecycle,
    failure: &mut lifecycle::Failure,
) -> Result<Option<Started>> {
    *failure = lifecycle::Failure::Config;
    // The revision is of the very bytes this run is configured from (WP-4.5).
    let (config, config_revision) = Config::load_revision(paths)?;
    *failure = lifecycle::Failure::Platform;
    let mut platform = platform::create(&paths.state_dir, &config)?;
    tracing::info!(backends = ?platform, "platform ready");
    request_permissions_daily(&paths.state_dir, &mut platform);
    // `_lock` stays held while the key store is waited for, so a second agent still refuses to
    // start.
    *failure = lifecycle::Failure::Keystore;
    let Some((identity, key_source)) =
        load_identity_waiting(paths, &config, platform.keystore.as_deref(), lifecycle)?
    else {
        return Ok(None);
    };
    lifecycle.key_source(key_source);
    let identity = Arc::new(identity);
    let node = identity.node();
    // This run's id and the facts `status` reports about it (WP-4.5), fixed from here on.
    let startup_facts =
        agent::StartupFacts::collect(node, paths, key_source, &config, config_revision)
            .with_instance(lifecycle.instance);
    tracing::info!(node = %node, name = %config.name, "identity");
    *failure = lifecycle::Failure::Config;
    let trust = trust::SharedTrust::load(paths.trust_file())?;

    let local_displays = platform.displays.displays().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "could not list displays");
        Vec::new()
    });

    // Crash recovery runs first (04 §8 invariant 2): Engine::new returns it as outputs.
    *failure = lifecycle::Failure::Other;
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

    // E2 video (WP-2.14): this node's encoder/decoder, if it has one and video isn't turned off.
    let video = media::VideoSetup {
        codecs: if config.video_mbps != Some(0) {
            platform::video_codecs(platform.gpu.as_ref())
        } else {
            None
        },
        gpu: platform.gpu.clone(),
    };
    // `cursor`: this node shows the source's cursor shapes on its proxies (WP-2.16).
    let mut features = vec!["e1".to_owned(), "cursor".to_owned()];
    if video.codecs.is_some() {
        features.push("h264".to_owned());
        // Region video (WP-2.32): this node shows a video rectangle over its lossless canvas.
        // `CROSSPANE_REGION_VIDEO=0` keeps peers on whole-window video (comparisons, escape hatch).
        if std::env::var("CROSSPANE_REGION_VIDEO").as_deref() != Ok("0") {
            features.push("h264roi".to_owned());
        }
    }
    // Audio (WP-3.6d, speaker v0): the worker starts before the transport, so `audio` is
    // advertised exactly when it runs. It reaches the transport through `transport_slot`, filled
    // once the transport is bound; nothing is sent before a peer's link exists.
    let transport_slot = Arc::new(std::sync::OnceLock::new());
    let audio = start_audio(&platform, &transport_slot, &tx);
    if audio.is_some() {
        features.push("audio".to_owned());
    }
    let hello = Hello {
        minor: crosspane_protocol::PROTOCOL_MINOR,
        name: config.name.clone(),
        features: features.clone(),
        displays: local_displays.clone(),
    };
    let pins: Arc<dyn crosspane_transport::PinStore> = Arc::new(trust.clone());
    let net = net::Net::start(config.port, identity.clone(), pins, hello, tx.clone())?;
    // A failed bind leaves `transport_slot` empty and the worker dropped with it.
    let _ = transport_slot.set(net.transport());
    stop_on_signal(&net.runtime(), tx.clone());
    #[cfg(target_os = "linux")]
    platform::watch_compositor({
        let tx = tx.clone();
        move || {
            tracing::error!(
                "the Hyprland instance this agent belongs to is gone; stopping, to start again on the new one"
            );
            stop_for_restart(&tx);
        }
    });
    for peer in &config.peers {
        net.dial(peer.addr);
    }

    // E2: the proxy window host owns the main thread (winit's rule on macOS); without a display
    // the node can still project its own windows, just not show others'.
    let host = match crosspane_render::proxy::ProxyHost::new() {
        #[allow(unused_mut)]
        Ok(mut host) => {
            // Decoded VideoToolbox pictures go to the GPU without a copy (WP-2.26).
            #[cfg(target_os = "macos")]
            host.0.set_importer(Arc::new(
                crosspane_platform_macos::gpu_import::import_picture,
            ));
            Some(host)
        }
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
        features,
        audio.map(|worker| Box::new(worker) as Box<dyn agent::AudioPlane>),
    );
    agent.set_startup(startup_facts);
    agent.set_lifecycle_paths(paths.clone());
    agent.start_discovery();
    *failure = lifecycle::Failure::Socket;
    ctl::serve(&paths.control_socket(), tx.clone())?;
    lifecycle.phase(lifecycle::Phase::Ready, None)?;
    Ok(Some(Started {
        agent,
        startup,
        rx,
        tx,
        host: host.map(|(host, _)| host),
    }))
}

/// Start the audio worker (speaker v0, WP-3.6d) on this OS's audio backend: `None`, with a log
/// line saying why, when audio is off or this machine has no backend for it.
///
/// `transport` is where the transport appears once it is bound: the worker's sends go through
/// `Transport::link`, and fail (`Closed`) until then.
fn start_audio(
    platform: &platform::Platform,
    transport: &Arc<std::sync::OnceLock<Arc<crosspane_transport::Transport>>>,
    events: &std::sync::mpsc::Sender<agent::Event>,
) -> Option<audio::AudioWorker> {
    let host = platform::audio_host(platform.gate.clone())?;
    let slot = transport.clone();
    let send: audio::AudioSend = Arc::new(move |peer, packet| {
        let mut link = slot
            .get()
            .and_then(|transport| transport.link(peer))
            .ok_or(crosspane_protocol::link::LinkError::Closed)?;
        link.send_audio(packet)
    });
    let events = events.clone();
    let started = audio::AudioWorker::start(
        host,
        send,
        Arc::new(platform::now),
        Box::new(move |event| {
            let _ = events.send(agent::Event::Audio(event));
        }),
    );
    match started {
        Ok(worker) => {
            tracing::info!("audio sharing on: speakers (microphones are not supported yet)");
            Some(worker)
        }
        Err(error) => {
            tracing::warn!(%error, "the audio worker did not start: audio sharing is off");
            None
        }
    }
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
fn run_loop(started: Started, lifecycle: lifecycle::Lifecycle) -> Result<()> {
    let Started {
        agent,
        startup,
        rx,
        tx,
        host,
    } = started;
    match host {
        Some(host) => {
            let host_tx = tx.clone();
            let stop_tx = tx.clone();
            std::thread::Builder::new()
                .name("engine".into())
                .spawn(move || {
                    let stopped = exit_on_panic("engine", || agent.run(startup, &rx));
                    drop(tx);
                    finish_run_or_exit(lifecycle, stopped);
                    std::process::exit(stop_status());
                })
                .context("spawn engine thread")?;
            if let Err(error) = host.run(Box::new(move |event| {
                let _ = host_tx.send(agent::Event::Host(event));
            })) {
                // Usually the display server went away. The engine thread stops cleanly and ends
                // the process.
                tracing::error!(%error, "the proxy window host failed; stopping");
                stop_for_restart(&stop_tx);
                loop {
                    std::thread::park();
                }
            }
            Ok(())
        }
        None => {
            #[cfg(target_os = "macos")]
            {
                std::thread::Builder::new()
                    .name("engine".into())
                    .spawn(move || {
                        let stopped = exit_on_panic("engine", || agent.run(startup, &rx));
                        drop(tx);
                        finish_run_or_exit(lifecycle, stopped);
                        std::process::exit(stop_status());
                    })
                    .context("spawn engine thread")?;
                crosspane_platform_macos::main_thread::run_app()?;
                Ok(())
            }
            #[cfg(not(target_os = "macos"))]
            {
                let stopped = agent.run(startup, &rx);
                drop(tx);
                finish_run(lifecycle, stopped)?;
                if RESTART.load(Ordering::Acquire) {
                    bail!("stopped to be started again");
                }
                Ok(())
            }
        }
    }
}

fn finish_run(lifecycle: lifecycle::Lifecycle, stopped: agent::Stopped) -> Result<()> {
    lifecycle.stopped(stopped.outcomes)?;
    if stopped.restart {
        agent::restart();
    }
    Ok(())
}

fn finish_run_or_exit(lifecycle: lifecycle::Lifecycle, stopped: agent::Stopped) {
    if let Err(error) = finish_run(lifecycle, stopped) {
        tracing::error!(%error, "could not write the exit receipt");
        std::process::exit(1);
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
    trust_at(&paths, action)
}

fn trust_at(paths: &Paths, action: TrustAction) -> Result<()> {
    // Re-read under the same lock as the save, so a command delayed by erase cannot write a
    // pre-erase snapshot back. The instance lock stays available to the running agent.
    let _mutation = match &action {
        TrustAction::List => None,
        TrustAction::Add { .. } | TrustAction::Remove { .. } => {
            Some(paths::identity_mutation_lock(&paths.state_dir)?)
        }
    };
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
        Permission::Microphone => "microphone",
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
    // Microphone is required unless audio is off (`CROSSPANE_AUDIO=0`), as in `platform::create`.
    Box::new(crosspane_platform_macos::permissions::MacPermissions::new(
        platform::audio_enabled(),
    ))
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

#[cfg(test)]
mod lifecycle_dispatch_tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crosspane_platform::PlatformError;
    use std::sync::mpsc;

    #[derive(Clone)]
    struct PausedEraseStore {
        key: Arc<std::sync::Mutex<Option<zeroize::Zeroizing<Vec<u8>>>>>,
        operations: Arc<std::sync::Mutex<Vec<&'static str>>>,
        deleting: mpsc::Sender<()>,
        proceed: Arc<std::sync::Mutex<mpsc::Receiver<()>>>,
    }

    impl crosspane_platform::KeyStore for PausedEraseStore {
        fn load(
            &self,
            _name: &str,
        ) -> std::result::Result<Option<zeroize::Zeroizing<Vec<u8>>>, PlatformError> {
            self.operations.lock().unwrap().push("load");
            Ok(self.key.lock().unwrap().clone())
        }

        fn store(&self, _name: &str, bytes: &[u8]) -> std::result::Result<(), PlatformError> {
            self.operations.lock().unwrap().push("create");
            *self.key.lock().unwrap() = Some(zeroize::Zeroizing::new(bytes.to_vec()));
            Ok(())
        }

        fn delete(&self, _name: &str) -> std::result::Result<(), PlatformError> {
            self.operations.lock().unwrap().push("erase started");
            self.deleting.send(()).unwrap();
            self.proceed
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            *self.key.lock().unwrap() = None;
            self.operations.lock().unwrap().push("erase finished");
            Ok(())
        }
    }

    struct NoKeyWait;

    impl keys::Pacer for NoKeyWait {
        fn pause(&mut self, _duration: std::time::Duration) -> bool {
            panic!("fixture key store never locks")
        }

        fn now(&self) -> std::time::Duration {
            std::time::Duration::ZERO
        }
    }

    #[test]
    fn erase_excludes_identity_creation_and_trust_save_until_it_finishes() {
        use std::sync::Mutex;
        use std::time::Duration;
        for startup in [false, true] {
            let dir = std::env::temp_dir().join(format!(
                "crosspane-erase-mutations-{}-{startup}",
                std::process::id()
            ));
            paths::create_private_dir(&dir).unwrap();
            let paths = Paths {
                config_dir: dir.clone(),
                state_dir: dir.clone(),
                runtime_dir: dir.clone(),
            };
            let mut lifecycle = lifecycle::Lifecycle::start(&paths).unwrap();
            lifecycle.phase(lifecycle::Phase::Ready, None).unwrap();
            lifecycle
                .stopped(lifecycle::Shutdown {
                    parking: lifecycle::Parking::None,
                    input_journals_empty: true,
                    audio_stopped: true,
                })
                .unwrap();
            let old_peer = DeviceIdentity::generate().unwrap();
            trust_at(
                &paths,
                TrustAction::Add {
                    name: "old fixture peer".into(),
                    spki: keys::hex(old_peer.spki()),
                    allow_input: false,
                },
            )
            .unwrap();
            let new_peer = DeviceIdentity::generate().unwrap();
            let (deleting, delete_started) = mpsc::channel();
            let (proceed, delete_proceed) = mpsc::channel();
            let store = PausedEraseStore {
                key: Arc::new(Mutex::new(Some(zeroize::Zeroizing::new(
                    DeviceIdentity::generate().unwrap().pkcs8().to_vec(),
                )))),
                operations: Arc::new(Mutex::new(Vec::new())),
                deleting,
                proceed: Arc::new(Mutex::new(delete_proceed)),
            };
            std::thread::scope(|scope| {
                let erased = scope.spawn(|| {
                    lifecycle::erase_identity(&paths, false, || Ok(Box::new(store.clone())))
                });
                delete_started.recv_timeout(Duration::from_secs(2)).unwrap();
                let (waiting, wait_started) = mpsc::channel();
                let identity_waiting = waiting.clone();
                let identity_paths = &paths;
                let identity_store = &store;
                let identity = scope.spawn(move || {
                    paths::observe_mutation_lock_wait(identity_waiting);
                    if startup {
                        assert!(matches!(
                            keys::load_or_create_waiting(
                                Some(identity_store),
                                &identity_paths.key_file(),
                                false,
                                &mut NoKeyWait,
                                || Ok(())
                            )
                            .unwrap(),
                            keys::Startup::Identity(_, keys::KeySource::OsStore)
                        ));
                    } else {
                        load_identity(identity_paths, &Config::default(), Some(identity_store))
                            .unwrap();
                    }
                });
                let trust_paths = &paths;
                let peer = &new_peer;
                let saved = scope.spawn(move || {
                    paths::observe_mutation_lock_wait(waiting);
                    trust_at(
                        trust_paths,
                        TrustAction::Add {
                            name: "new fixture peer".into(),
                            spki: keys::hex(peer.spki()),
                            allow_input: false,
                        },
                    )
                    .unwrap();
                });
                // Each observer fires only after its real flock reports WouldBlock. Both
                // operations reached their lock while the fake erase is still paused.
                wait_started.recv_timeout(Duration::from_secs(2)).unwrap();
                wait_started.recv_timeout(Duration::from_secs(2)).unwrap();
                assert_eq!(*store.operations.lock().unwrap(), ["load", "erase started"]);
                let before = trust::SharedTrust::load(paths.trust_file()).unwrap();
                assert!(before.with(|t| t.get(old_peer.node()).is_some()));
                assert!(before.with(|t| t.get(new_peer.node()).is_none()));
                proceed.send(()).unwrap();
                let erased = serde_json::to_value(erased.join().unwrap()).unwrap();
                assert_eq!(erased["result"], "removed");
                identity.join().unwrap();
                saved.join().unwrap();
            });
            assert_eq!(
                *store.operations.lock().unwrap(),
                ["load", "erase started", "erase finished", "load", "create"]
            );
            let after = trust::SharedTrust::load(paths.trust_file()).unwrap();
            assert!(after.with(|t| t.get(old_peer.node()).is_none()));
            assert!(after.with(|t| t.get(new_peer.node()).is_some()));
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn identity_and_trust_mutations_allow_a_running_agent_instance() {
        let dir = std::env::temp_dir().join(format!(
            "crosspane-one-shot-mutations-{}",
            std::process::id()
        ));
        paths::create_private_dir(&dir).unwrap();
        let paths = Paths {
            config_dir: dir.clone(),
            state_dir: dir.clone(),
            runtime_dir: dir.clone(),
        };
        let instance = lifecycle::instance_lock(&paths).unwrap();
        instance.try_lock().unwrap();
        let config = Config {
            force_file_keystore: true,
            ..Config::default()
        };
        load_identity(&paths, &config, None).unwrap();
        assert!(paths.key_file().exists());
        let peer = DeviceIdentity::generate().unwrap();
        trust_at(
            &paths,
            TrustAction::Add {
                name: "fixture peer".into(),
                spki: keys::hex(peer.spki()),
                allow_input: false,
            },
        )
        .unwrap();
        assert!(
            trust::SharedTrust::load(paths.trust_file())
                .unwrap()
                .with(|t| t.get(peer.node()).is_some())
        );
        trust_at(
            &paths,
            TrustAction::Remove {
                peer: "fixture peer".into(),
            },
        )
        .unwrap();
        assert!(
            trust::SharedTrust::load(paths.trust_file())
                .unwrap()
                .with(|t| t.get(peer.node()).is_none())
        );
        drop(instance);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn one_shot_commands_never_enter_the_bootstrap_or_receipt_writer() {
        let dir =
            std::env::temp_dir().join(format!("crosspane-one-shot-routing-{}", std::process::id()));
        crate::paths::create_private_dir(&dir).unwrap();
        let paths = Paths {
            config_dir: dir.clone(),
            state_dir: dir.clone(),
            runtime_dir: dir.clone(),
        };
        for args in [
            vec!["crosspane-agent", "identity"],
            vec!["crosspane-agent", "trust", "list"],
            vec!["crosspane-agent", "permissions"],
            vec!["crosspane-agent", "erase-identity"],
            vec!["crosspane-agent", "erase-identity", "--keep-trust"],
        ] {
            let command = Cli::try_parse_from(args).unwrap().command.unwrap();
            let mut called = false;
            dispatch(
                command,
                || {
                    let _lifecycle = lifecycle::Lifecycle::start(&paths)?;
                    panic!("one-shot command entered run")
                },
                |_| {
                    called = true;
                    Ok(())
                },
            )
            .unwrap();
            assert!(called);
            assert!(!paths.bootstrap_file().exists());
            assert!(!paths.exit_receipt().exists());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
