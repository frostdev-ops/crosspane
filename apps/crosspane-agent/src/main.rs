//! The per-session daemon wiring engine, platform, and transport (WP-1.37).

mod agent;
mod audio;
mod clipboard;
mod config;
mod ctl;
mod keys;
mod lifecycle;
#[cfg(any(target_os = "macos", test))]
mod macos_launch;
mod media;
mod net;
mod os_permissions;
mod pairing;
mod parking_worker;
mod paths;
mod platform;
#[cfg(any(windows, test))]
mod reachability;
mod revocations;
mod tray;
mod trust;
#[cfg(windows)]
mod windows;
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
        /// Ask the OS for the first permission not yet granted (one per run, as the menu does).
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

/// Set for a restart-required stop, including an engine restart pending teardown, so a timeout
/// also exits with failure and lets the service manager start it again.
static RESTART: AtomicBool = AtomicBool::new(false);
/// Accepted installer stop is sticky for this process, including later host restart failures.
/// Tests that accept a stop run alone in an ignored child process; the latch is never cleared.
static INSTALLER_STOP_ACCEPTED: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "macos")]
static APPKIT_STOPPED: AtomicBool = AtomicBool::new(false);

/// The exit status after a clean stop: 75 (`EX_TEMPFAIL`) for a restart or AppKit termination.
#[cfg(unix)]
fn stop_status() -> i32 {
    if INSTALLER_STOP_ACCEPTED.load(Ordering::Acquire) {
        return 0;
    }
    #[cfg(target_os = "macos")]
    if APPKIT_STOPPED.load(Ordering::Acquire) {
        // The engine can finish concurrently with the callback's first restart publication.
        return 75;
    }
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
    #[cfg(windows)]
    windows::process::wait_for_restart_parent()?;
    #[cfg(target_os = "linux")]
    platform::prepare_exit_diagnostics();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    #[cfg(unix)]
    let elevated = is_elevated();
    #[cfg(windows)]
    let elevated = windows::security::is_elevated().context("check agent elevation")?;
    if elevated {
        bail!("crosspane-agent refuses to run elevated (04 §7)");
    }
    let command = Cli::parse().command;
    #[cfg(target_os = "macos")]
    match macos_launch::handoff(
        command.is_none(),
        rustix::process::geteuid().as_raw(),
        macos_launch::system_command,
    ) {
        Ok(true) => return Ok(()),
        Ok(false) => {}
        Err(error) => {
            tracing::error!(%error, "managed Crosspane handoff failed");
            macos_launch::show_handoff_error();
            return Err(error);
        }
    }
    dispatch(command.unwrap_or(Command::Run), run, one_shot)
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
            #[cfg(target_os = "linux")]
            {
                platform::exit_diagnostic(
                    thread,
                    "thread panicked; exiting so the agent restarts and recovers",
                );
                platform::exit_without_handlers(101);
            }
            #[cfg(not(target_os = "linux"))]
            {
                tracing::error!(
                    thread,
                    "thread panicked; exiting so the agent restarts and recovers"
                );
                std::process::exit(101);
            }
        }
    }
}

/// Delete the once-a-day burst's marker an older agent left (WP-4.33 removed the burst: the
/// agent asks for a permission only on a click, one at a time).
fn remove_old_burst_marker(state_dir: &std::path::Path) {
    match std::fs::remove_file(state_dir.join(os_permissions::OLD_BURST_MARKER)) {
        Ok(()) => tracing::info!("removed the old permission-burst marker"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::debug!(error = %e, "could not remove the old permission-burst marker"),
    }
}

fn run() -> Result<()> {
    let paths = Paths::new()?;
    run_with_startup(&paths, start_agent)
}

fn run_with_startup(
    paths: &Paths,
    start: impl FnOnce(
        &Paths,
        &mut lifecycle::Lifecycle,
        &mut lifecycle::Failure,
    ) -> Result<Option<Started>>,
) -> Result<()> {
    // One agent per user, settled before anything touches the session: creating the platform
    // recovers parked windows, which would un-hide a running agent's projections. The lock is
    // held for the life of the process (close-on-exec, so a restart in place takes it again).
    let lock_path = paths.instance_lock();
    let _lock =
        lifecycle::instance_lock(paths).with_context(|| format!("open {}", lock_path.display()))?;
    _lock.try_lock().map_err(|_| {
        anyhow::anyhow!(
            "another crosspane-agent is already running for this user ({} is locked)",
            lock_path.display()
        )
    })?;
    let mut lifecycle = lifecycle::Lifecycle::start(paths)?;
    let mut failure = lifecycle::Failure::Other;
    let started = start(paths, &mut lifecycle, &mut failure);
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
    #[cfg(any(target_os = "linux", windows))]
    host_shutdown: Option<crosspane_render::proxy::HostHandle>,
    #[cfg(any(target_os = "linux", windows))]
    media_workers: [media::Worker; 2],
    host: Option<crosspane_render::proxy::ProxyHost>,
    #[cfg(windows)]
    shutdown_watch: windows::shutdown::Watch,
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
    // Winit registers both Raw Input classes at EventLoop construction. On Windows the
    // host relinquishes those registrations before ANY native platform observer exists,
    // preserving capture's mouse target as well as the required release/panic target.
    #[cfg(windows)]
    let mut host = match crosspane_render::proxy::ProxyHost::new() {
        Ok(host) => Some(host),
        Err(e) => {
            tracing::warn!(error = %e, "no proxy window host: this node can't show projected windows");
            None
        }
    };
    let mut platform = platform::create(&paths.state_dir, &config)?;
    #[cfg(windows)]
    let acceptance_scratch = platform.acceptance_scratch;
    #[cfg(not(windows))]
    let acceptance_scratch = false;
    let bind = std::net::SocketAddr::new(
        platform::acceptance_bind_ip(&config, acceptance_scratch)?,
        config.port,
    );
    #[cfg(not(windows))]
    tracing::info!(backends = ?platform, "platform ready");
    remove_old_burst_marker(&paths.state_dir);
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
    let journal = paths::open_journal(&paths.journal_file()).context("open input journal")?;
    let e2_journal =
        paths::open_journal(&paths.e2_journal_file()).context("open projection input journal")?;
    let mut engine_config = EngineConfig::new(node);
    engine_config.drag_across = config.drag.across;
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
    #[cfg(windows)]
    let shutdown_watch = windows::shutdown::Watch::for_agent(tx.clone())?;
    #[cfg(windows)]
    {
        *failure = lifecycle::Failure::Platform;
        let hotkey_tx = tx.clone();
        platform.hotkeys = Some(
            platform::windows_hotkeys_after_host(Arc::new(move |event| {
                let _ = hotkey_tx.send(agent::Event::Input(crosspane_engine::Input::Hotkey(event)));
            }))
            .context("Windows release/panic hotkeys (required for startup)")?,
        );
        tracing::info!(backends = ?platform, "platform ready");
        *failure = lifecycle::Failure::Other;
    }
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
    if config.drag.across {
        features.push(crosspane_protocol::projection::DRAG_FEATURE.to_owned());
    }
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
    // `start_agent` runs on process MAIN before AppKit's loop; construct the facade here,
    // then transfer its Rust handle to the serialized worker. Advertise only after readiness.
    let clipboard = platform::clipboard_host(platform.gate.clone()).and_then(|host| {
        match clipboard::Worker::start(host, tx.clone()) {
            Ok(worker) => Some(worker),
            Err(reason) => {
                tracing::info!(?reason, "clipboard worker unavailable");
                None
            }
        }
    });
    if clipboard.is_some() {
        features.push(crosspane_protocol::clip::CLIP_FEATURE.to_owned());
    }
    let hello = Hello {
        minor: crosspane_protocol::PROTOCOL_MINOR,
        name: config.name.clone(),
        features: features.clone(),
        displays: local_displays.clone(),
    };
    let pins: Arc<dyn crosspane_transport::PinStore> = Arc::new(trust.clone());
    let net = net::Net::start(bind, identity.clone(), pins, hello, tx.clone())?;
    // A failed bind leaves `transport_slot` empty and the worker dropped with it.
    let _ = transport_slot.set(net.transport());
    #[cfg(target_os = "linux")]
    stop_on_signal(&net.runtime(), tx.clone()).inspect_err(|error| {
        tracing::error!(%error, "could not install signal handlers");
    })?;
    #[cfg(target_os = "macos")]
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
    #[cfg(not(windows))]
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
    #[cfg(windows)]
    if let Some((host, _)) = &mut host
        && let Some(mapping) = platform.host_placement_mapping.clone()
    {
        host.set_placement_mapping(mapping);
    }
    let proxy_ids = media::ProxyIds::default();
    let (source_media, source_worker) = media::start_source(net.transport(), video.clone());
    let (dest_media, dest_worker) = media::start_destination(
        host.as_ref().map(|(_, handle)| handle.clone()),
        proxy_ids.clone(),
        tx.clone(),
        video,
    );
    #[cfg(target_os = "macos")]
    drop((source_worker, dest_worker)); // Preserve macOS's existing orchestration.
    let e2 = agent::E2Wiring {
        source_media,
        dest_media,
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
    agent.set_clipboard(clipboard);
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
        #[cfg(any(target_os = "linux", windows))]
        host_shutdown: host.as_ref().map(|(_, handle)| handle.clone()),
        #[cfg(any(target_os = "linux", windows))]
        media_workers: [source_worker, dest_worker],
        host: host.map(|(host, _)| host),
        #[cfg(windows)]
        shutdown_watch,
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
#[cfg(target_os = "linux")]
fn stop_on_signal(
    runtime: &tokio::runtime::Handle,
    events: std::sync::mpsc::Sender<agent::Event>,
) -> Result<()> {
    let _ = runtime;
    start_linux_signals(events, |task| {
        std::thread::Builder::new()
            .name("stop-signals".into())
            .spawn(task)
            .map(|_| ())
    })
}

#[cfg(target_os = "macos")]
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

/// Net is dropped by `Agent::run` before its receipt and the host's teardown. Keep the second
/// signal listener on its own runtime until process exit, including when that teardown stalls.
#[cfg(target_os = "linux")]
fn start_linux_signals(
    events: std::sync::mpsc::Sender<agent::Event>,
    spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<()>,
) -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let (setup, ready) = std::sync::mpsc::channel();
    // Spawn first: failure must not install Tokio's permanent process-wide signal hooks.
    spawn(Box::new(move || {
        let prepared = (|| -> Result<_> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("start signal runtime")?;
            let (term, int) = {
                let _entered = runtime.enter();
                (
                    signal(SignalKind::terminate()).context("listen for SIGTERM")?,
                    signal(SignalKind::interrupt()).context("listen for SIGINT")?,
                )
            };
            Ok((runtime, term, int))
        })();
        let (runtime, mut term, mut int) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = setup.send(Err(error));
                return;
            }
        };
        if setup.send(Ok(())).is_err() {
            return;
        }
        runtime.block_on(async move {
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
            let _ = events.send(agent::Event::Shutdown);
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
            platform::exit_without_handlers(1);
        });
    }))
    .context("spawn signal thread")?;
    // Startup cannot reach Ready unless both listeners were installed successfully.
    ready.recv().context("signal thread ended before setup")?
}

/// Run the engine loop on its own thread and the proxy host (if any) on this, the main thread.
/// Without a host on Linux, the engine loop runs here; on macOS the AppKit loop always owns the
/// main thread.
#[cfg(unix)]
fn run_loop(started: Started, lifecycle: lifecycle::Lifecycle) -> Result<()> {
    let Started {
        agent,
        startup,
        rx,
        tx,
        #[cfg(target_os = "linux")]
        host_shutdown,
        #[cfg(target_os = "linux")]
        media_workers,
        host,
    } = started;
    #[cfg(target_os = "linux")]
    let host_done = Arc::new(AtomicBool::new(host.is_none()));
    #[cfg(target_os = "linux")]
    let engine_done = Arc::new(AtomicBool::new(false));
    #[cfg(target_os = "linux")]
    let owners = media_workers
        .iter()
        .map(media::Worker::completion)
        .chain([
            ("proxy-host", host_done.clone()),
            ("engine", engine_done.clone()),
        ])
        .collect();
    match host {
        Some(host) => {
            let host_tx = tx.clone();
            let stop_tx = tx.clone();
            #[cfg(target_os = "linux")]
            {
                let host_shutdown = host_shutdown.context("missing proxy host shutdown handle")?;
                let restart = run_linux_host(
                    move || {
                        let stopped = exit_on_panic("engine", || agent.run(startup, &rx));
                        drop(tx);
                        let restart = stopped.restart;
                        let receipt = lifecycle.stopped(stopped.outcomes);
                        let deadline =
                            start_shutdown_watchdog(owners, receipt.is_ok(), true, restart);
                        let receipt = receipt.inspect_err(|error| {
                            tracing::error!(%error, "could not write the exit receipt");
                        });
                        stop_linux_media(media_workers, deadline);
                        receipt.context("could not write the exit receipt")?;
                        Ok(restart)
                    },
                    move || {
                        host.run(Box::new(move |event| {
                            let _ = host_tx.send(agent::Event::Host(event));
                        }))
                        .map_err(anyhow::Error::from)
                    },
                    move || {
                        let _ = host_shutdown.send(crosspane_render::proxy::HostCommand::Shutdown);
                    },
                    move |error| host_failed(error, &stop_tx),
                    [host_done, engine_done],
                )?;
                if restart_after_stop(restart) {
                    agent::restart();
                }
                // The host's graphics resources and the engine are gone before libc's exit
                // handlers run. Exiting from the worker raced those handlers with host teardown.
                std::process::exit(stop_status());
            }
            #[cfg(not(target_os = "linux"))]
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
        }
        None => {
            #[cfg(target_os = "macos")]
            {
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
                let terminate_tx = stop_tx.clone();
                crosspane_platform_macos::main_thread::run_app_with_termination(move || {
                    stop_appkit_for_restart(&terminate_tx);
                })?;
                // stop() can also return from run; ordinary terminate() uses the delegate above.
                stop_appkit_for_restart(&stop_tx);
                loop {
                    std::thread::park();
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                let stopped = agent.run(startup, &rx);
                drop(tx);
                let restart = stopped.restart;
                let receipt = lifecycle.stopped(stopped.outcomes);
                let deadline = start_shutdown_watchdog(owners, receipt.is_ok(), false, restart);
                let receipt = receipt.inspect_err(|error| {
                    tracing::error!(%error, "could not write the exit receipt");
                });
                stop_linux_media(media_workers, deadline);
                engine_done.store(true, Ordering::Release);
                receipt?;
                if restart_after_stop(restart) {
                    agent::restart();
                }
                if restart_after_stop(RESTART.load(Ordering::Acquire)) {
                    bail!("stopped to be started again");
                }
                Ok(())
            }
        }
    }
}

/// Windows retains every engine/media/host owner before writing a clean receipt or restarting.
#[cfg(windows)]
fn run_loop(started: Started, lifecycle: lifecycle::Lifecycle) -> Result<()> {
    let Started {
        agent,
        startup,
        rx,
        tx,
        host_shutdown,
        mut media_workers,
        host,
        shutdown_watch,
    } = started;
    let stopped = match host {
        Some(host) => {
            let host_shutdown = host_shutdown.context("missing proxy host shutdown handle")?;
            let host_tx = tx.clone();
            let engine = std::thread::Builder::new()
                .name("engine".into())
                .spawn(move || {
                    let stopped = exit_on_panic("engine", || agent.run(startup, &rx));
                    platform::exit_deadline(std::time::Duration::from_secs(5));
                    let _ = host_shutdown.send(crosspane_render::proxy::HostCommand::Shutdown);
                    stopped
                })
                .context("spawn engine thread")?;
            if let Err(error) = host.run(Box::new(move |event| {
                let _ = host_tx.send(agent::Event::Host(event));
            })) {
                stop_for_restart(&tx);
                tracing::error!(%error, "the proxy window host failed; stopping");
            }
            engine
                .join()
                .map_err(|_| anyhow::anyhow!("engine thread panicked"))?
        }
        None => {
            let stopped = exit_on_panic("engine", || agent.run(startup, &rx));
            platform::exit_deadline(std::time::Duration::from_secs(5));
            stopped
        }
    };
    drop(tx);
    let pending = media::stop_workers(
        &mut media_workers,
        std::time::Instant::now() + std::time::Duration::from_secs(3),
    );
    if !pending.is_empty() {
        // No receipt: a still-running owner cannot establish completion.
        windows::process::exit_without_handlers(1);
    }
    lifecycle
        .stopped(stopped.outcomes)
        .context("write exit receipt")?;
    drop(shutdown_watch);
    if restart_after_stop(stopped.restart || RESTART.load(Ordering::Acquire)) {
        agent::restart();
    }
    Ok(())
}

/// Final replacement/exit policy observes sticky intent even after a later host failure.
fn restart_after_stop(restart_requested: bool) -> bool {
    restart_requested && !INSTALLER_STOP_ACCEPTED.load(Ordering::Acquire)
}

#[cfg(target_os = "macos")]
fn stop_appkit_for_restart(events: &std::sync::mpsc::Sender<agent::Event>) {
    macos_launch::appkit_returned(&APPKIT_STOPPED, &RESTART, || {
        // Arm before sending Shutdown. A stalled engine or cleanup can never let AppKit exit 0.
        if let Err(error) = std::thread::Builder::new()
            .name("appkit-exit-deadline".into())
            .spawn(|| {
                std::thread::sleep(std::time::Duration::from_secs(5));
                tracing::error!("AppKit termination cleanup timed out; aborting for restart");
                std::process::abort();
            })
        {
            tracing::error!(%error, "cannot arm AppKit termination deadline");
            std::process::abort();
        }
        let _ = events.send(agent::Event::Shutdown);
    });
}

#[cfg(target_os = "linux")]
fn host_failed(error: anyhow::Error, events: &std::sync::mpsc::Sender<agent::Event>) {
    stop_for_restart(events);
    tracing::error!(%error, "the proxy window host failed; stopping");
}

/// The engine completes all shutdown work and attempts its receipt before the host is asked to
/// stop. Until that handback, SIGTERM relies on the service's stop timeout (or a second signal).
/// One three-second observer bounds all remaining owners inside the existing five-second fallback;
/// compositor-loss deadlines are independently armed as before.
#[cfg(target_os = "linux")]
fn run_linux_host(
    run_engine: impl FnOnce() -> Result<bool> + Send + 'static,
    run_host: impl FnOnce() -> Result<()>,
    stop_host: impl FnOnce() + Send + 'static,
    host_failed: impl FnOnce(anyhow::Error),
    completed: [Arc<AtomicBool>; 2],
) -> Result<bool> {
    let engine = std::thread::Builder::new()
        .name("engine".into())
        .spawn(move || {
            let result = run_engine();
            stop_host();
            result
        })
        .context("spawn engine thread")?;
    let host_result = run_host();
    completed[0].store(true, Ordering::Release);
    if let Err(error) = host_result {
        host_failed(error);
    }
    let result = engine.join();
    completed[1].store(true, Ordering::Release);
    result.map_err(|_| anyhow::anyhow!("engine thread panicked"))?
}

/// Only orderly stops reach here: all releases/recovery and the receipt attempt are complete.
/// The pre-receipt capture joins still rely on the service's outer stop timeout.
#[cfg(target_os = "linux")]
fn stop_linux_media(mut workers: [media::Worker; 2], deadline: std::time::Instant) {
    if !media::stop_workers(&mut workers, deadline).is_empty() {
        // The observer owns exit/status selection. Never reach normal libc exit with a pending owner.
        loop {
            std::thread::park();
        }
    }
}

/// Arm at the receipt attempt, before closure drops, host teardown or engine TLS/join can block.
#[cfg(target_os = "linux")]
fn start_shutdown_watchdog(
    owners: Vec<(&'static str, Arc<AtomicBool>)>,
    receipt_ok: bool,
    hosted: bool,
    restart: bool,
) -> std::time::Instant {
    RESTART.fetch_or(restart, Ordering::Release);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    platform::exit_deadline(std::time::Duration::from_secs(5));
    let spawned = std::thread::Builder::new()
        .name("shutdown-watchdog".into())
        .spawn(move || {
            std::thread::sleep(deadline.saturating_duration_since(std::time::Instant::now()));
            let mut pending = false;
            for (owner, completed) in owners {
                if !completed.load(Ordering::Acquire) {
                    pending = true;
                    platform::exit_diagnostic(
                        owner,
                        "shutdown owner did not stop in time; exiting without driver exit handlers",
                    );
                }
            }
            if !pending {
                platform::exit_diagnostic("main", "shutdown did not reach exit in time");
            }
            // Read RESTART now: a host failure during this wait must still trigger restart.
            platform::exit_without_handlers(media_exit_status(receipt_ok, hosted));
        });
    if spawned.is_err() {
        platform::exit_diagnostic("shutdown-watchdog", "could not start shutdown watchdog");
        platform::exit_without_handlers(media_exit_status(receipt_ok, hosted));
    }
    deadline
}

/// Hosted restarts exit 75; without a proxy host the existing Result/bail path exits 1.
#[cfg(target_os = "linux")]
fn media_exit_status(receipt_ok: bool, has_proxy_host: bool) -> i32 {
    if !receipt_ok {
        1
    } else if has_proxy_host {
        stop_status()
    } else {
        i32::from(restart_after_stop(RESTART.load(Ordering::Acquire)))
    }
}

#[cfg(target_os = "macos")]
fn finish_run(lifecycle: lifecycle::Lifecycle, stopped: agent::Stopped) -> Result<()> {
    lifecycle.stopped(stopped.outcomes)?;
    if restart_after_stop(stopped.restart) {
        #[cfg(target_os = "macos")]
        if !APPKIT_STOPPED.load(Ordering::Acquire) {
            agent::restart();
        }
        #[cfg(not(target_os = "macos"))]
        agent::restart();
    }
    Ok(())
}

#[cfg(target_os = "macos")]
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
    if request {
        let record = Paths::new()
            .ok()
            .map(|p| p.state_dir.join(os_permissions::ASK_RECORD));
        let asked = os_permissions::ask_first_missing(
            perms.as_mut(),
            &mut os_permissions::SystemTcc,
            &mut os_permissions::AskRecord::at(record),
        )
        .map_err(anyhow::Error::msg)?;
        eprintln!("{}", serde_json::to_string(&asked)?);
    }
    let mut report = serde_json::Map::new();
    for permission in perms.required() {
        let state = perms.state(permission);
        if state != PermissionState::Granted && open_settings {
            open_settings_pane(permission);
        }
        report.insert(
            os_permissions::permission_token(permission).into(),
            os_permissions::state_token(state).into(),
        );
    }
    println!("{}", serde_json::Value::Object(report));
    Ok(())
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

#[cfg(windows)]
fn platform_permissions() -> Box<dyn Permissions> {
    // The agent watches only required(): an empty set has no notices/retries/backend failures.
    Box::new(crosspane_platform_windows::stubs::UnsupportedWindows)
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
#[cfg(unix)]
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

#[cfg(all(test, target_os = "linux"))]
mod host_exit_tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::cell::RefCell;
    use std::os::unix::process::ExitStatusExt;
    use std::path::PathBuf;
    use std::sync::{Mutex, mpsc};
    use std::time::{Duration, Instant};

    type Trace = Arc<Mutex<Vec<&'static str>>>;

    struct RecordedDrop(Trace, &'static str);

    impl Drop for RecordedDrop {
        fn drop(&mut self) {
            self.0.lock().unwrap().push(self.1);
        }
    }

    thread_local! {
        static ENGINE_TLS: RefCell<Option<RecordedDrop>> = const { RefCell::new(None) };
    }

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let next = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("crosspane-host-exit-{}-{next}", std::process::id()));
            std::fs::create_dir(&dir).unwrap();
            Self(dir)
        }

        fn paths(&self) -> Paths {
            Paths {
                config_dir: self.0.clone(),
                state_dir: self.0.clone(),
                runtime_dir: self.0.clone(),
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn receipt(paths: &Paths) -> Result<()> {
        let mut lifecycle = lifecycle::Lifecycle::start(paths)?;
        lifecycle.phase(lifecycle::Phase::Ready, None)?;
        lifecycle.stopped(lifecycle::Shutdown {
            parking: lifecycle::Parking::NothingParked,
            input_journals_empty: true,
            audio_stopped: true,
        })
    }

    fn wait_for_child_marker(child: &mut std::process::Child, marker: &std::path::Path) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !marker.is_file() {
            if let Some(status) = child.try_wait().unwrap() {
                panic!("owned child exited before {}: {status}", marker.display());
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let _ = child.wait();
                panic!("owned child did not reach {}", marker.display());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn unfinished_host_and_engine() -> [Arc<AtomicBool>; 2] {
        [
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        ]
    }

    fn monotonic_ns() -> u128 {
        let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
        u128::try_from(now.tv_sec).unwrap() * 1_000_000_000 + u128::try_from(now.tv_nsec).unwrap()
    }

    fn register_exit_marker(dir: &std::path::Path) {
        platform::prepare_exit_diagnostics();
        static EXIT_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        EXIT_DIR.set(dir.to_owned()).unwrap();
        extern "C" fn handler() {
            if let Some(dir) = EXIT_DIR.get() {
                let _ = std::fs::write(dir.join("exit-handler-ran"), "unexpected");
            }
        }
        // SAFETY: atexit accepts a process-lifetime C callback with no arguments or result.
        unsafe extern "C" {
            fn atexit(callback: extern "C" fn()) -> std::ffi::c_int;
        }
        // SAFETY: the static callback has the C ABI and lives until this owned child exits.
        assert_eq!(unsafe { atexit(handler) }, 0);
    }

    fn capture_child_stderr(
        child: &mut std::process::Child,
        log: &std::path::Path,
    ) -> std::thread::JoinHandle<()> {
        let mut stderr = child.stderr.take().unwrap();
        let mut log = std::fs::File::create(log).unwrap();
        std::thread::spawn(move || {
            std::io::copy(&mut stderr, &mut log).unwrap();
        })
    }

    #[test]
    fn media_owners_finish_after_receipt_attempt_before_host_drop() {
        for fail in [false, true] {
            let fixture = Fixture::new();
            let paths = fixture.paths();
            let marker = paths.state_dir.join("receipt-attempted");
            let trace: Trace = Arc::new(Mutex::new(Vec::new()));
            let workers = ["media-encode", "media-decode"].map(|name| {
                let marker = marker.clone();
                let trace = trace.clone();
                media::fake_worker(name, move |stop| {
                    let _gpu = RecordedDrop(trace, "GPU dropped");
                    while !stop.load(Ordering::Acquire) {
                        std::thread::yield_now();
                    }
                    assert!(
                        marker.is_file(),
                        "media stop must follow the receipt attempt"
                    );
                })
            });
            let joined_trace = trace.clone();
            let host_trace = trace.clone();
            let (stop, stopping) = mpsc::channel();
            let result = run_linux_host(
                move || {
                    let mut lifecycle = lifecycle::Lifecycle::start(&paths)?;
                    lifecycle.phase(lifecycle::Phase::Ready, None)?;
                    if fail {
                        std::fs::create_dir(paths.state_dir.join("last_exit.json"))?;
                    }
                    let receipt = lifecycle.stopped(lifecycle::Shutdown {
                        parking: lifecycle::Parking::NothingParked,
                        input_journals_empty: true,
                        audio_stopped: true,
                    });
                    std::fs::write(marker, "attempted")?;
                    assert_eq!(receipt.is_ok(), !fail);
                    let mut workers = workers;
                    assert!(
                        media::stop_workers(&mut workers, Instant::now() + Duration::from_secs(2))
                            .is_empty()
                    );
                    assert_eq!(
                        *joined_trace.lock().unwrap(),
                        ["GPU dropped", "GPU dropped"]
                    );
                    receipt.map(|()| true)
                },
                move || {
                    stopping.recv_timeout(Duration::from_secs(2)).unwrap();
                    host_trace.lock().unwrap().push("host dropped");
                    Ok(())
                },
                move || stop.send(()).unwrap(),
                |_| panic!("fake host did not fail"),
                unfinished_host_and_engine(),
            );
            if fail {
                assert!(result.is_err());
            } else {
                assert!(result.unwrap(), "restart request survives media joins");
            }
            assert_eq!(
                *trace.lock().unwrap(),
                ["GPU dropped", "GPU dropped", "host dropped"]
            );
        }
    }

    #[test]
    fn media_timeout_preserves_status_and_receipt_without_exit_handlers() {
        const CHILD: &str = "CROSSPANE_MEDIA_EXIT_TEST_DIR";
        if let Some(dir) = std::env::var_os(CHILD) {
            let dir = PathBuf::from(dir);
            let fail = std::env::var("CROSSPANE_MEDIA_EXIT_TEST_FAIL").unwrap() == "1";
            let late_restart = std::env::var("CROSSPANE_MEDIA_EXIT_TEST_RESTART").unwrap() == "1";
            let hosted = std::env::var("CROSSPANE_MEDIA_EXIT_TEST_HOSTED").unwrap() == "1";
            let engine_restart =
                std::env::var("CROSSPANE_MEDIA_EXIT_TEST_ENGINE_RESTART").unwrap() == "1";
            let kind = std::env::var("CROSSPANE_MEDIA_EXIT_TEST_KIND").unwrap();
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .init();
            register_exit_marker(&dir);
            let paths = Paths {
                config_dir: dir.clone(),
                state_dir: dir.clone(),
                runtime_dir: dir.clone(),
            };
            let mut lifecycle = lifecycle::Lifecycle::start(&paths).unwrap();
            lifecycle.phase(lifecycle::Phase::Ready, None).unwrap();
            if fail {
                std::fs::create_dir(dir.join("last_exit.json")).unwrap();
            }
            struct HungOwner(PathBuf);
            impl Drop for HungOwner {
                fn drop(&mut self) {
                    std::fs::write(&self.0, monotonic_ns().to_string()).unwrap();
                    loop {
                        std::thread::park();
                    }
                }
            }
            thread_local! {
                static HUNG_EXIT_TLS: RefCell<Option<HungOwner>> = const { RefCell::new(None) };
            }
            let (ready, started) = mpsc::channel();
            let workers = ["media-encode", "media-decode"].map(|name| {
                let marker = dir.join(name);
                let hangs = kind == "media";
                let slow = kind.ends_with("-after-media");
                let ready = ready.clone();
                media::fake_worker(name, move |stop| {
                    let _gpu = hangs.then(|| HungOwner(marker));
                    ready.send(()).unwrap();
                    while !stop.load(Ordering::Acquire) {
                        std::thread::yield_now();
                    }
                    if slow {
                        std::thread::sleep(Duration::from_secs(1));
                    }
                })
            });
            for _ in 0..2 {
                started.recv_timeout(Duration::from_secs(2)).unwrap();
            }
            let completed = unfinished_host_and_engine();
            completed[0].store(!hosted, Ordering::Release);
            let owners = workers
                .iter()
                .map(media::Worker::completion)
                .chain([
                    ("proxy-host", completed[0].clone()),
                    ("engine", completed[1].clone()),
                ])
                .collect();
            if late_restart {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    while !dir.join("media-encode").is_file() {
                        std::thread::yield_now();
                    }
                    RESTART.store(true, Ordering::Release);
                    std::fs::write(dir.join("restart-during-wait"), "set").unwrap();
                });
            }
            let engine_dir = dir.clone();
            let engine_tls = kind.starts_with("engine");
            let run_engine = move || {
                if engine_tls {
                    HUNG_EXIT_TLS
                        .with(|tls| *tls.borrow_mut() = Some(HungOwner(engine_dir.join("engine"))));
                }
                let attempted = monotonic_ns();
                let result = lifecycle.stopped(lifecycle::Shutdown {
                    parking: lifecycle::Parking::NothingParked,
                    input_journals_empty: true,
                    audio_stopped: true,
                });
                assert_eq!(result.is_err(), fail);
                let deadline =
                    start_shutdown_watchdog(owners, result.is_ok(), hosted, engine_restart);
                std::fs::write(engine_dir.join("receipt-clock"), attempted.to_string())?;
                std::fs::write(
                    engine_dir.join("receipt-attempted"),
                    if result.is_ok() { "written" } else { "failed" },
                )?;
                stop_linux_media(workers, deadline);
                result.map(|()| false)
            };
            if hosted {
                let (stop, stopping) = mpsc::channel();
                let _ = run_linux_host(
                    run_engine,
                    move || {
                        let _host = kind
                            .starts_with("host")
                            .then(|| HungOwner(dir.join("proxy-host")));
                        stopping.recv_timeout(Duration::from_secs(4)).unwrap();
                        Ok(())
                    },
                    move || stop.send(()).unwrap(),
                    |_| panic!("fake host did not fail"),
                    completed,
                );
            } else {
                let _ = run_engine();
            }
            panic!("hung owner returned");
        }
        for (kind, expected, hosted, restart, engine_restart, fail) in [
            ("media", 0, true, false, false, false),
            ("media", 75, true, true, false, false),
            ("media", 1, true, true, false, true),
            ("media", 1, false, true, false, false),
            ("media", 75, true, false, true, false),
            ("media", 1, true, false, true, true),
            ("media", 1, false, false, true, false),
            ("host", 0, true, false, false, false),
            ("host", 1, true, false, false, true),
            ("engine", 0, true, false, false, false),
            ("host-after-media", 0, true, false, false, false),
            ("engine-after-media", 0, true, false, false, false),
        ] {
            let fixture = Fixture::new();
            let log = fixture.0.join("media-exit.log");
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "host_exit_tests::media_timeout_preserves_status_and_receipt_without_exit_handlers"])
                .env(CHILD, &fixture.0)
                .env("CROSSPANE_MEDIA_EXIT_TEST_FAIL", if fail { "1" } else { "0" })
                .env("CROSSPANE_MEDIA_EXIT_TEST_RESTART", if restart { "1" } else { "0" })
                .env("CROSSPANE_MEDIA_EXIT_TEST_HOSTED", if hosted { "1" } else { "0" })
                .env("CROSSPANE_MEDIA_EXIT_TEST_KIND", kind)
                .env("CROSSPANE_MEDIA_EXIT_TEST_ENGINE_RESTART", if engine_restart { "1" } else { "0" })
                .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped()).spawn().unwrap();
            let logging = capture_child_stderr(&mut child, &log);
            wait_for_child_marker(&mut child, &fixture.0.join("receipt-clock"));
            let attempted: u128 = std::fs::read_to_string(fixture.0.join("receipt-clock"))
                .unwrap()
                .parse()
                .unwrap();
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if monotonic_ns() - attempted >= 3_500_000_000 {
                    child.kill().unwrap();
                    let _ = child.wait();
                    panic!("owned child missed aggregate shutdown bound ({kind})");
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            logging.join().unwrap();
            let elapsed = Duration::from_nanos(u64::try_from(monotonic_ns() - attempted).unwrap());
            eprintln!(
                "{kind} hosted={hosted} late_restart={restart} engine_restart={engine_restart} receipt_failed={fail}: status={status}, receipt-to-exit={elapsed:?}"
            );
            assert_eq!(status.code(), Some(expected));
            assert!(
                elapsed >= Duration::from_millis(2950) && elapsed < Duration::from_millis(3350),
                "one aggregate 3 s budget: {elapsed:?}"
            );
            if kind == "media" {
                for owner in ["media-encode", "media-decode"] {
                    assert!(fixture.0.join(owner).is_file());
                }
            } else {
                assert!(
                    fixture
                        .0
                        .join(if kind.starts_with("host") {
                            "proxy-host"
                        } else {
                            "engine"
                        })
                        .is_file()
                );
            }
            if restart {
                assert!(fixture.0.join("restart-during-wait").is_file());
            }
            if kind.ends_with("-after-media") {
                let owner = if kind.starts_with("host") {
                    "proxy-host"
                } else {
                    "engine"
                };
                let blocked: u128 = std::fs::read_to_string(fixture.0.join(owner))
                    .unwrap()
                    .parse()
                    .unwrap();
                let media_elapsed =
                    Duration::from_nanos(u64::try_from(blocked - attempted).unwrap());
                eprintln!("{kind}: media completed before blocked {owner} at {media_elapsed:?}");
                assert!(
                    media_elapsed >= Duration::from_millis(950)
                        && media_elapsed < Duration::from_millis(1500)
                );
            }
            assert!(!fixture.0.join("exit-handler-ran").exists());
            assert_eq!(
                std::fs::read_to_string(fixture.0.join("receipt-attempted")).unwrap(),
                if fail { "failed" } else { "written" }
            );
            if fail {
                assert!(fixture.0.join("last_exit.json").is_dir());
            } else {
                let receipt: serde_json::Value = serde_json::from_slice(
                    &std::fs::read(fixture.0.join("last_exit.json")).unwrap(),
                )
                .unwrap();
                assert_eq!(receipt["clean"], true);
            }
            let log = std::fs::read_to_string(log).unwrap();
            assert!(log.contains("shutdown owner did not stop in time"));
            for owner in match kind {
                "media" => &["media-encode", "media-decode"][..],
                "host" | "host-after-media" => &["proxy-host"][..],
                _ => &["engine"][..],
            } {
                assert_eq!(
                    log.matches(owner).count(),
                    1,
                    "each unjoined owner is named once: {log}"
                );
            }
        }
    }

    #[test]
    fn signal_thread_spawn_failure_exits_one_before_ready() {
        const CHILD: &str = "CROSSPANE_SIGNAL_SPAWN_FAILURE_TEST_DIR";
        if let Some(dir) = std::env::var_os(CHILD) {
            let dir = PathBuf::from(dir);
            let paths = Paths {
                config_dir: dir.clone(),
                state_dir: dir.clone(),
                runtime_dir: dir,
            };
            // Exercise the real startup error/phase boundary without creating OS backends.
            let result = run_with_startup(&paths, |_, lifecycle, _| {
                let (events, _receiving) = mpsc::channel();
                start_linux_signals(events, |_task| {
                    Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
                })?;
                lifecycle.phase(lifecycle::Phase::Ready, None)?;
                Ok(None)
            });
            match result {
                Ok(()) => std::process::exit(0),
                Err(error) => {
                    // Match the binary's Result termination rather than the test harness's 101.
                    eprintln!("Error: {error:#}");
                    std::process::exit(1);
                }
            }
        }
        let fixture = Fixture::new();
        let error_path = fixture.0.join("startup-error.log");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "host_exit_tests::signal_thread_spawn_failure_exits_one_before_ready",
                "--nocapture",
            ])
            .env(CHILD, &fixture.0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(&error_path).unwrap())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let _ = child.wait();
                panic!("the owned child continued after signal setup failed");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.code(), Some(1));
        let bootstrap: serde_json::Value =
            serde_json::from_slice(&std::fs::read(fixture.0.join("bootstrap.json")).unwrap())
                .unwrap();
        assert_eq!(bootstrap["phase"], "failed");
        assert!(
            std::fs::read_to_string(error_path)
                .unwrap()
                .contains("spawn signal thread")
        );
    }

    #[test]
    fn second_term_or_int_after_receipt_attempt_exits_one_during_blocked_teardown() {
        const CHILD: &str = "CROSSPANE_SECOND_SIGNAL_TEST_DIR";
        if let Some(dir) = std::env::var_os(CHILD) {
            let dir = PathBuf::from(dir);
            register_exit_marker(&dir);
            let paths = Paths {
                config_dir: dir.clone(),
                state_dir: dir.clone(),
                runtime_dir: dir.clone(),
            };
            let fail = std::env::var("CROSSPANE_SECOND_SIGNAL_TEST_FAIL").unwrap() == "1";
            let net_runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap();
            let (events, stopping) = mpsc::channel();
            stop_on_signal(net_runtime.handle(), events).unwrap();
            std::fs::write(dir.join("signals-ready"), "ready").unwrap();
            let (stop_host, host_stopping) = mpsc::channel();
            let _ = run_linux_host(
                move || {
                    assert!(matches!(
                        stopping.recv_timeout(Duration::from_secs(3)).unwrap(),
                        agent::Event::Shutdown
                    ));
                    // Match Agent::run: Net's runtime is gone before the receipt attempt.
                    drop(net_runtime);
                    let mut lifecycle = lifecycle::Lifecycle::start(&paths)?;
                    lifecycle.phase(lifecycle::Phase::Ready, None)?;
                    if fail {
                        std::fs::create_dir(paths.state_dir.join("last_exit.json"))?;
                    }
                    let result = lifecycle.stopped(lifecycle::Shutdown {
                        parking: lifecycle::Parking::NothingParked,
                        input_journals_empty: true,
                        audio_stopped: true,
                    });
                    platform::exit_deadline(Duration::from_secs(5));
                    std::fs::write(
                        paths.state_dir.join("receipt-attempted"),
                        if result.is_ok() { "written" } else { "failed" },
                    )?;
                    result.map(|()| false)
                },
                move || {
                    host_stopping.recv_timeout(Duration::from_secs(3)).unwrap();
                    std::fs::write(dir.join("teardown-blocked"), "blocked").unwrap();
                    loop {
                        std::thread::park();
                    }
                },
                move || stop_host.send(()).unwrap(),
                |_| panic!("fake host did not fail"),
                unfinished_host_and_engine(),
            );
            panic!("blocked teardown unexpectedly returned");
        }
        for fail in [false, true] {
            for first in [rustix::process::Signal::TERM, rustix::process::Signal::INT] {
                for second in [rustix::process::Signal::TERM, rustix::process::Signal::INT] {
                    let fixture = Fixture::new();
                    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                        .args([
                            "--exact",
                            "host_exit_tests::second_term_or_int_after_receipt_attempt_exits_one_during_blocked_teardown",
                        ])
                        .env(CHILD, &fixture.0)
                        .env("CROSSPANE_SECOND_SIGNAL_TEST_FAIL", if fail { "1" } else { "0" })
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                        .unwrap();
                    let pid =
                        rustix::process::Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
                    wait_for_child_marker(&mut child, &fixture.0.join("signals-ready"));
                    rustix::process::kill_process(pid, first).unwrap();
                    wait_for_child_marker(&mut child, &fixture.0.join("teardown-blocked"));
                    assert_eq!(
                        std::fs::read_to_string(fixture.0.join("receipt-attempted")).unwrap(),
                        if fail { "failed" } else { "written" }
                    );
                    let started = Instant::now();
                    rustix::process::kill_process(pid, second).unwrap();
                    let status = loop {
                        if let Some(status) = child.try_wait().unwrap() {
                            break status;
                        }
                        if started.elapsed() >= Duration::from_secs(2) {
                            child.kill().unwrap();
                            let _ = child.wait();
                            panic!(
                                "second {second:?} did not stop the owned child after {first:?}"
                            );
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    };
                    assert_eq!(status.code(), Some(1), "{first:?} then {second:?}");
                    assert!(!fixture.0.join("exit-handler-ran").exists());
                    assert!(started.elapsed() < Duration::from_secs(2));
                    let receipt = fixture.0.join("last_exit.json");
                    if fail {
                        assert!(receipt.is_dir());
                    } else {
                        let value: serde_json::Value =
                            serde_json::from_slice(&std::fs::read(receipt).unwrap()).unwrap();
                        assert_eq!(value["clean"], true);
                    }
                }
            }
        }
    }

    #[test]
    fn panic_exits_101_without_driver_exit_handlers() {
        const CHILD: &str = "CROSSPANE_PANIC_EXIT_TEST_DIR";
        if let Some(dir) = std::env::var_os(CHILD) {
            let dir = PathBuf::from(dir);
            register_exit_marker(&dir);
            // Keep another owner's destructor blocked while this thread panics.
            let (entered, dropping) = mpsc::channel();
            let worker = media::fake_worker("media-decode", move |_| {
                struct Hung(mpsc::Sender<()>);
                impl Drop for Hung {
                    fn drop(&mut self) {
                        self.0.send(()).unwrap();
                        loop {
                            std::thread::park();
                        }
                    }
                }
                let _gpu = Hung(entered);
            });
            dropping.recv_timeout(Duration::from_secs(2)).unwrap();
            std::fs::write(dir.join("destructor-running"), "blocked").unwrap();
            let _retained = worker;
            exit_on_panic("owned panic fixture", || panic!("owned test panic"));
            unreachable!();
        }
        let fixture = Fixture::new();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "host_exit_tests::panic_exits_101_without_driver_exit_handlers",
            ])
            .env(CHILD, &fixture.0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let before = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if before.elapsed() > Duration::from_secs(3) {
                child.kill().unwrap();
                let _ = child.wait();
                panic!("owned panic child did not terminate");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.code(), Some(101));
        assert!(fixture.0.join("destructor-running").is_file());
        assert!(!fixture.0.join("exit-handler-ran").exists());
    }

    #[test]
    fn blocked_stderr_cannot_delay_shutdown_deadlines() {
        const CHILD: &str = "CROSSPANE_BLOCKED_STDERR_TEST_DIR";
        if let Some(dir) = std::env::var_os(CHILD) {
            let dir = PathBuf::from(dir);
            register_exit_marker(&dir);
            let full = std::env::var("CROSSPANE_BLOCKED_STDERR_FULL").unwrap() == "1";
            let fallback = std::env::var("CROSSPANE_BLOCKED_STDERR_FALLBACK").unwrap() == "1";
            if full {
                let stderr = std::io::stderr();
                let flags = rustix::fs::fcntl_getfl(&stderr).unwrap();
                rustix::fs::fcntl_setfl(&stderr, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
                while rustix::io::write(&stderr, &[b'x'; 4096]).is_ok() {}
                // Restore blocking mode: a tracing/write_all diagnostic would now hang.
                rustix::fs::fcntl_setfl(&stderr, flags).unwrap();
            } else {
                let (ready, locked) = mpsc::channel();
                std::thread::spawn(move || {
                    let _stdio_lock = std::io::stderr().lock();
                    ready.send(()).unwrap();
                    loop {
                        std::thread::park();
                    }
                });
                locked.recv_timeout(Duration::from_secs(2)).unwrap();
            }
            let paths = Paths {
                config_dir: dir.clone(),
                state_dir: dir.clone(),
                runtime_dir: dir.clone(),
            };
            let attempted = monotonic_ns();
            receipt(&paths).unwrap();
            if fallback {
                platform::exit_deadline(Duration::from_millis(100));
            } else {
                start_shutdown_watchdog(
                    vec![("blocked-owner", Arc::new(AtomicBool::new(false)))],
                    true,
                    true,
                    false,
                );
            }
            std::fs::write(dir.join("receipt-clock"), attempted.to_string()).unwrap();
            loop {
                std::thread::park();
            }
        }
        for full in [false, true] {
            for fallback in [false, true] {
                let fixture = Fixture::new();
                let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "host_exit_tests::blocked_stderr_cannot_delay_shutdown_deadlines",
                    ])
                    .env(CHILD, &fixture.0)
                    .env(
                        "CROSSPANE_BLOCKED_STDERR_FULL",
                        if full { "1" } else { "0" },
                    )
                    .env(
                        "CROSSPANE_BLOCKED_STDERR_FALLBACK",
                        if fallback { "1" } else { "0" },
                    )
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap();
                // Keep the owned pipe open but unread until exit; do not accidentally clear the block.
                let _unread = child.stderr.take().unwrap();
                wait_for_child_marker(&mut child, &fixture.0.join("receipt-clock"));
                let attempted: u128 = std::fs::read_to_string(fixture.0.join("receipt-clock"))
                    .unwrap()
                    .parse()
                    .unwrap();
                let max = if fallback { 500_000_000 } else { 3_500_000_000 };
                let status = loop {
                    if let Some(status) = child.try_wait().unwrap() {
                        break status;
                    }
                    if monotonic_ns() - attempted >= max {
                        child.kill().unwrap();
                        let _ = child.wait();
                        panic!(
                            "blocked stderr defeated owned child's deadline full={full} fallback={fallback}"
                        );
                    }
                    std::thread::sleep(Duration::from_millis(10));
                };
                let elapsed =
                    Duration::from_nanos(u64::try_from(monotonic_ns() - attempted).unwrap());
                eprintln!(
                    "blocked stderr full={full} fallback={fallback}: status={status}, receipt-to-exit={elapsed:?}"
                );
                if fallback {
                    assert_eq!(status.signal(), Some(9));
                } else {
                    assert_eq!(status.code(), Some(0));
                    assert!(elapsed < Duration::from_millis(3350));
                }
                assert!(!fixture.0.join("exit-handler-ran").exists());
                assert!(fixture.0.join("last_exit.json").is_file());
            }
        }
    }

    #[test]
    fn exit_diagnostic_uses_startup_classification_without_rechecking_metadata() {
        const CHILD: &str = "CROSSPANE_EXIT_CLASSIFICATION_TEST_DIR";
        if std::env::var_os(CHILD).is_some() {
            use std::io::Read;
            use std::os::fd::AsRawFd;
            // The parent gave only this test child a regular-file stderr: cache unsupported.
            platform::prepare_exit_diagnostics();
            let (mut reading, writing) = std::os::unix::net::UnixStream::pair().unwrap();
            // SAFETY: this declaration matches POSIX dup2's C ABI and integer descriptors.
            unsafe extern "C" {
                fn dup2(oldfd: std::ffi::c_int, newfd: std::ffi::c_int) -> std::ffi::c_int;
            }
            // SAFETY: writing owns a valid private socket. Only this owned child's fd2 changes;
            // dup2 retains its own reference, and no owner-process descriptor is accessed.
            assert_eq!(unsafe { dup2(writing.as_raw_fd(), 2) }, 2);
            // Calling startup again must not reclassify; termination reads only the cache.
            platform::prepare_exit_diagnostics();
            platform::exit_diagnostic("owned-cache-test", "must remain omitted");
            assert!(
                !rustix::fs::fcntl_getfl(std::io::stderr())
                    .unwrap()
                    .contains(rustix::fs::OFlags::NONBLOCK)
            );
            reading.set_nonblocking(true).unwrap();
            assert_eq!(
                reading.read(&mut [0; 64]).unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
            return;
        }
        let fixture = Fixture::new();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "host_exit_tests::exit_diagnostic_uses_startup_classification_without_rechecking_metadata"])
            .env(CHILD, &fixture.0)
            .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(fixture.0.join("regular-stderr")).unwrap())
            .spawn().unwrap();
        let before = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if before.elapsed() > Duration::from_secs(2) {
                child.kill().unwrap();
                let _ = child.wait();
                panic!("owned cache-classification child did not finish");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.code(), Some(0));
    }

    #[test]
    fn actual_host_failure_publishes_restart_before_blocked_logging() {
        const CHILD: &str = "CROSSPANE_HOST_FAILURE_LOG_TEST_DIR";
        if let Some(dir) = std::env::var_os(CHILD) {
            let dir = PathBuf::from(dir);
            register_exit_marker(&dir);
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .init();
            let (ready, locked) = mpsc::channel();
            std::thread::spawn(move || {
                let _stdio_lock = std::io::stderr().lock();
                ready.send(()).unwrap();
                loop {
                    std::thread::park();
                }
            });
            locked.recv_timeout(Duration::from_secs(2)).unwrap();
            let completed = unfinished_host_and_engine();
            let owners = vec![
                ("proxy-host", completed[0].clone()),
                ("engine", completed[1].clone()),
            ];
            let (events, receiving) = mpsc::channel();
            let (armed, observer_ready) = mpsc::channel();
            let engine_dir = dir.clone();
            let _ = run_linux_host(
                move || {
                    let paths = Paths {
                        config_dir: engine_dir.clone(),
                        state_dir: engine_dir.clone(),
                        runtime_dir: engine_dir.clone(),
                    };
                    let attempted = monotonic_ns();
                    receipt(&paths)?;
                    start_shutdown_watchdog(owners, true, true, false);
                    std::fs::write(engine_dir.join("receipt-clock"), attempted.to_string())?;
                    armed.send(()).unwrap();
                    assert!(matches!(
                        receiving.recv_timeout(Duration::from_secs(2)).unwrap(),
                        agent::Event::Shutdown
                    ));
                    std::fs::write(engine_dir.join("shutdown-requested"), "requested")?;
                    loop {
                        std::thread::park();
                    }
                },
                move || {
                    observer_ready.recv_timeout(Duration::from_secs(2)).unwrap();
                    std::fs::write(dir.join("host-failed"), "failed")?;
                    bail!("owned late host failure")
                },
                || {},
                move |error| host_failed(error, &events),
                completed,
            );
            unreachable!();
        }
        let fixture = Fixture::new();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "host_exit_tests::actual_host_failure_publishes_restart_before_blocked_logging",
            ])
            .env(CHILD, &fixture.0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let _unread = child.stderr.take().unwrap();
        wait_for_child_marker(&mut child, &fixture.0.join("receipt-clock"));
        let attempted: u128 = std::fs::read_to_string(fixture.0.join("receipt-clock"))
            .unwrap()
            .parse()
            .unwrap();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if monotonic_ns() - attempted >= 3_500_000_000 {
                child.kill().unwrap();
                let _ = child.wait();
                panic!("owned host-failure child missed its deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let elapsed = Duration::from_nanos(u64::try_from(monotonic_ns() - attempted).unwrap());
        eprintln!(
            "actual host failure with blocked logging: status={status}, receipt-to-exit={elapsed:?}"
        );
        assert_eq!(status.code(), Some(75));
        assert!(elapsed >= Duration::from_millis(2950) && elapsed < Duration::from_millis(3350));
        assert!(fixture.0.join("host-failed").is_file());
        assert!(fixture.0.join("shutdown-requested").is_file());
        assert!(!fixture.0.join("exit-handler-ran").exists());
        let receipt: serde_json::Value =
            serde_json::from_slice(&std::fs::read(fixture.0.join("last_exit.json")).unwrap())
                .unwrap();
        assert_eq!(receipt["clean"], true);
    }

    #[test]
    fn host_teardown_and_engine_tls_finish_before_main_exit() {
        let fixture = Fixture::new();
        let paths = fixture.paths();
        let trace: Trace = Arc::new(Mutex::new(Vec::new()));
        let engine_trace = trace.clone();
        let host_trace = trace.clone();
        let deadline_trace = trace.clone();
        let deadline_paths = paths.clone();
        let (stop, stopping) = mpsc::channel();
        let main_thread = std::thread::current().id();
        let restart = run_linux_host(
            move || {
                ENGINE_TLS.with(|slot| {
                    *slot.borrow_mut() = Some(RecordedDrop(engine_trace.clone(), "engine TLS"));
                });
                let agent = RecordedDrop(engine_trace.clone(), "agent dropped");
                drop(agent);
                receipt(&paths)?;
                engine_trace.lock().unwrap().push("receipt");
                assert!(deadline_paths.state_dir.join("last_exit.json").is_file());
                deadline_trace.lock().unwrap().push("deadline armed");
                Ok(true)
            },
            move || {
                let _host = RecordedDrop(host_trace, "host dropped");
                stopping.recv_timeout(Duration::from_secs(2)).unwrap();
                assert_eq!(std::thread::current().id(), main_thread);
                Ok(())
            },
            move || stop.send(()).unwrap(),
            |_| panic!("fake host did not fail"),
            unfinished_host_and_engine(),
        )
        .unwrap();
        assert!(
            restart,
            "the restart request must reach the main thread unchanged"
        );
        trace.lock().unwrap().push("main exit");
        let trace = trace.lock().unwrap();
        let position = |event| trace.iter().position(|entry| *entry == event).unwrap();
        assert!(position("agent dropped") < position("receipt"));
        assert!(position("receipt") < position("deadline armed"));
        assert!(position("deadline armed") < position("host dropped"));
        assert!(position("host dropped") < position("main exit"));
        assert!(position("engine TLS") < position("main exit"));
    }

    #[test]
    fn host_failure_still_waits_for_shutdown_and_the_exit_receipt() {
        let fixture = Fixture::new();
        let paths = fixture.paths();
        let completed = paths.state_dir.join("last_exit.json");
        let (stop_engine, stopping) = mpsc::channel();
        let result = run_linux_host(
            move || {
                stopping.recv_timeout(Duration::from_secs(2)).unwrap();
                receipt(&paths)?;
                Ok(false)
            },
            || bail!("fake compositor disappeared"),
            || {},
            move |error| {
                assert_eq!(error.to_string(), "fake compositor disappeared");
                stop_engine.send(()).unwrap();
            },
            unfinished_host_and_engine(),
        )
        .unwrap();
        assert!(!result);
        assert!(completed.is_file());
    }

    #[test]
    fn receipt_write_error_returns_after_host_teardown_and_join() {
        let trace: Trace = Arc::new(Mutex::new(Vec::new()));
        let host_trace = trace.clone();
        let deadline_trace = trace.clone();
        let (stop, stopping) = mpsc::channel();
        let error = run_linux_host(
            move || {
                deadline_trace.lock().unwrap().push("deadline armed");
                bail!("could not write the exit receipt")
            },
            move || {
                let _host = RecordedDrop(host_trace, "host dropped");
                stopping.recv_timeout(Duration::from_secs(2)).unwrap();
                Ok(())
            },
            move || stop.send(()).unwrap(),
            |_| panic!("fake host did not fail"),
            unfinished_host_and_engine(),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "could not write the exit receipt");
        assert_eq!(*trace.lock().unwrap(), ["deadline armed", "host dropped"]);
    }

    #[test]
    fn post_receipt_deadline_bounds_hung_teardown_even_if_the_write_failed() {
        const CHILD: &str = "CROSSPANE_HOST_EXIT_TEST_DIR";
        if let Some(dir) = std::env::var_os(CHILD) {
            let dir = PathBuf::from(dir);
            let paths = Paths {
                config_dir: dir.clone(),
                state_dir: dir.clone(),
                runtime_dir: dir,
            };
            let fail = std::env::var("CROSSPANE_HOST_EXIT_TEST_FAIL").unwrap() == "1";
            let (stop, stopping) = mpsc::channel();
            let _ = run_linux_host(
                move || {
                    let mut lifecycle = lifecycle::Lifecycle::start(&paths)?;
                    lifecycle.phase(lifecycle::Phase::Ready, None)?;
                    if fail {
                        // Refuse the atomic receipt rename after the complete fake shutdown.
                        std::fs::create_dir(paths.state_dir.join("last_exit.json"))?;
                    }
                    let result = lifecycle.stopped(lifecycle::Shutdown {
                        parking: lifecycle::Parking::NothingParked,
                        input_journals_empty: true,
                        audio_stopped: true,
                    });
                    platform::exit_deadline(Duration::from_millis(100));
                    std::fs::write(
                        paths.state_dir.join("receipt-attempted"),
                        if result.is_ok() { "written" } else { "failed" },
                    )?;
                    result.map(|()| false)
                },
                move || {
                    stopping.recv_timeout(Duration::from_secs(2)).unwrap();
                    loop {
                        std::thread::park();
                    }
                },
                move || stop.send(()).unwrap(),
                |_| panic!("fake host did not fail"),
                unfinished_host_and_engine(),
            );
            panic!("hung teardown unexpectedly returned");
        }
        for fail in [false, true] {
            let fixture = Fixture::new();
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "host_exit_tests::post_receipt_deadline_bounds_hung_teardown_even_if_the_write_failed",
                ])
                .env(CHILD, &fixture.0)
                .env("CROSSPANE_HOST_EXIT_TEST_FAIL", if fail { "1" } else { "0" })
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    let _ = child.wait();
                    panic!("the post-receipt deadline did not stop the owned child");
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            assert_eq!(status.signal(), Some(9));
            assert_eq!(
                std::fs::read_to_string(fixture.0.join("receipt-attempted")).unwrap(),
                if fail { "failed" } else { "written" }
            );
            let receipt = fixture.0.join("last_exit.json");
            if fail {
                assert!(
                    receipt.is_dir(),
                    "failed write must not produce a clean receipt"
                );
            } else {
                let value: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(receipt).unwrap()).unwrap();
                assert_eq!(value["clean"], true);
            }
        }
    }
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

#[cfg(all(test, target_os = "macos"))]
mod macos_launch_tests {
    use super::*;
    #[test]
    fn appkit_return_uses_failure_exit_after_shutdown_request() {
        const MARKER: &str = "CROSSPANE_APPKIT_EXIT_TEST";
        if std::env::var_os(MARKER).is_some() {
            let (send, recv) = std::sync::mpsc::channel();
            assert_eq!(
                stop_status(),
                0,
                "normal Quit must remain a successful stop"
            );
            APPKIT_STOPPED.store(true, Ordering::Release);
            assert_eq!(
                stop_status(),
                75,
                "AppKit termination cannot race into exit 0"
            );
            APPKIT_STOPPED.store(false, Ordering::Release);
            stop_appkit_for_restart(&send);
            assert!(matches!(recv.try_recv().unwrap(), agent::Event::Shutdown));
            assert!(APPKIT_STOPPED.load(Ordering::Acquire));
            assert_eq!(stop_status(), 75);
            // No platform startup, launchctl, AppKit or real lifecycle state in this child.
            std::process::exit(stop_status());
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "macos_launch_tests::appkit_return_uses_failure_exit_after_shutdown_request",
                "--nocapture",
            ])
            .env(MARKER, "1")
            .spawn()
            .unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert_eq!(status.code(), Some(75));
                break;
            }
            if std::time::Instant::now() >= until {
                let _ = child.kill();
                let _ = child.wait();
                panic!("owned AppKit exit fixture timed out");
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}
