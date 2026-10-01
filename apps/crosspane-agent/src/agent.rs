//! The engine loop (03 §1): one thread owns the [`Engine`] and the platform backends, feeds the
//! engine every event, and carries out its outputs. Everything else (network, control socket,
//! backend threads) talks to it through one channel.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use crosspane_engine::{
    Command, Engine, Failure, InjectCmd, Input, Notice, Output, ProjectionKey, ProxyEvent,
};
use crosspane_input::arrange::{self, Side};
use crosspane_platform::{
    CaptureEvent, EventSink, FrameEvent, OverlayEvent, Permission, PermissionState, PlatformError,
    StreamId, WindowEvent,
};
use crosspane_protocol::link::{LinkEvent, PeerLink};
use crosspane_protocol::msg::{Capability, ControlMessage, Placement, Refusal};
use crosspane_render::proxy::{HostCommand, HostEvent, HostHandle};
use crosspane_types::display::DisplayInfo;
use crosspane_types::id::NodeId;
use crosspane_types::id::{ProjectionId, WindowId};
use serde_json::{Value, json};

use crate::ctl::{Request, Response};
use crate::media::{DestCmd, ProxyIds, SourceCmd};
use crate::net::Net;
use crate::platform::{self, Platform};
use crate::tray::{self, PairingView, PeerView, RemoteWindows, TrayAction, TrayView};
use crate::trust::SharedTrust;

/// Everything the engine loop reacts to.
pub enum Event {
    /// A platform event, already in engine terms.
    Input(Input),
    LocalDisplays(Vec<DisplayInfo>),
    Link(LinkEvent),
    Ctl(Request, Sender<Response>),
    /// From the proxy window host (E2 destination).
    Host(HostEvent),
    /// A pairing exchange succeeded: pin the peer.
    Paired(crate::pairing::Paired),
    /// SIGTERM or SIGINT: stop cleanly.
    Shutdown,
    /// The user chose a tray / menu-bar item.
    Tray(crosspane_platform::TrayEvent),
}

/// What the loop knows about a peer.
#[derive(Debug, Default)]
struct PeerInfo {
    name: String,
    displays: Vec<DisplayInfo>,
    connected: bool,
    rtt: Option<Duration>,
}

pub struct Agent {
    node: NodeId,
    name: String,
    engine: Engine,
    platform: Platform,
    net: Net,
    trust: SharedTrust,
    links: HashMap<NodeId, Box<dyn PeerLink>>,
    peers: BTreeMap<NodeId, PeerInfo>,
    local_displays: Vec<DisplayInfo>,
    placements: Vec<Placement>,
    /// Inputs produced while executing outputs (capture results, injection results), handled
    /// before the next external event so their order is preserved.
    pending: VecDeque<Input>,
    notices: VecDeque<String>,
    last_trust_check: Instant,
    last_rtt_poll: Instant,
    /// OS permissions granted when the backends were created (they are created once, at start).
    granted: Vec<Permission>,
    /// A different set seen once; it must be seen again before the agent restarts.
    granted_changing: Option<Vec<Permission>>,
    last_permission_check: Instant,
    started: Instant,
    restart_requested: bool,
    /// ctl requests waiting for a peer's answer (browse and pull), by engine request number.
    waiters: HashMap<u32, Waiter>,
    next_request: u32,
    tray: TrayState,
    quit_requested: bool,
    // E2 data plane and window host.
    source_media: Sender<SourceCmd>,
    dest_media: Sender<DestCmd>,
    host: Option<HostHandle>,
    proxy_ids: ProxyIds,
    streams: HashMap<StreamId, ProjectionId>,
    projections: BTreeMap<ProjectionKey, String>,
    events: Sender<Event>,
    crossing: bool,
    pairing: crate::pairing::Pairing,
    identity: Arc<crosspane_security::identity::DeviceIdentity>,
    port: u16,
}

/// The E2 pieces the agent wires in (`media.rs`, the proxy host).
pub struct E2Wiring {
    pub source_media: Sender<SourceCmd>,
    pub dest_media: Sender<DestCmd>,
    pub host: Option<HostHandle>,
    pub proxy_ids: ProxyIds,
    pub events: Sender<Event>,
    /// `config.crossing`.
    pub crossing: bool,
    pub identity: Arc<crosspane_security::identity::DeviceIdentity>,
    pub port: u16,
}

const NOTICE_HISTORY: usize = 20;
const HOUSEKEEPING: Duration = Duration::from_secs(1);
const PERMISSION_CHECK: Duration = Duration::from_secs(2);
/// How long a browse or pull waits for the peer's answer.
const BROWSE_WAIT: Duration = Duration::from_secs(5);

/// How often the tray refreshes what it shows.
const TRAY_UPDATE: Duration = Duration::from_secs(1);
const TRAY_LOCAL_WINDOWS: Duration = Duration::from_secs(3);
const TRAY_BROWSE: Duration = Duration::from_secs(5);

/// What the tray menu needs beyond the agent's own state.
struct TrayState {
    menu: Option<crosspane_platform::TrayMenu>,
    actions: BTreeMap<crosspane_platform::TrayItemId, TrayAction>,
    last_update: Instant,
    local_windows: Vec<(WindowId, String)>,
    last_local_windows: Option<Instant>,
    remote_windows: HashMap<NodeId, RemoteWindows>,
    /// Browse requests the tray sent, by request number.
    browses: HashMap<u32, NodeId>,
    last_browse: Option<Instant>,
    /// Where the user put each peer (for the check marks).
    sides: HashMap<NodeId, Side>,
}

impl TrayState {
    fn new() -> TrayState {
        TrayState {
            menu: None,
            actions: BTreeMap::new(),
            last_update: Instant::now(),
            local_windows: Vec::new(),
            last_local_windows: None,
            remote_windows: HashMap::new(),
            browses: HashMap::new(),
            last_browse: None,
            sides: HashMap::new(),
        }
    }
}

/// A window as one menu line: "title — app", shortened.
fn window_label(title: &str, app: &str) -> String {
    let text = if title.is_empty() {
        app.to_owned()
    } else if app.is_empty() {
        title.to_owned()
    } else {
        format!("{title} — {app}")
    };
    if text.chars().count() > 60 {
        let cut: String = text.chars().take(59).collect();
        format!("{cut}…")
    } else {
        text
    }
}

/// A ctl request waiting for a peer.
struct Waiter {
    reply: Sender<Response>,
    peer: NodeId,
    pull: bool,
    deadline: Instant,
}
const MIN_RUN_BEFORE_RESTART: Duration = Duration::from_secs(30);

impl Agent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        node: NodeId,
        name: String,
        engine: Engine,
        platform: Platform,
        net: Net,
        trust: SharedTrust,
        local_displays: Vec<DisplayInfo>,
        e2: E2Wiring,
    ) -> Agent {
        Agent {
            node,
            name,
            engine,
            platform,
            net,
            trust,
            links: HashMap::new(),
            peers: BTreeMap::new(),
            local_displays,
            placements: Vec::new(),
            pending: VecDeque::new(),
            notices: VecDeque::new(),
            last_trust_check: Instant::now(),
            last_rtt_poll: Instant::now(),
            granted: Vec::new(),
            granted_changing: None,
            last_permission_check: Instant::now(),
            started: Instant::now(),
            restart_requested: false,
            waiters: HashMap::new(),
            next_request: 1,
            tray: TrayState::new(),
            quit_requested: false,
            source_media: e2.source_media,
            dest_media: e2.dest_media,
            host: e2.host,
            proxy_ids: e2.proxy_ids,
            streams: HashMap::new(),
            projections: BTreeMap::new(),
            events: e2.events,
            crossing: e2.crossing,
            pairing: crate::pairing::Pairing::default(),
            identity: e2.identity,
            port: e2.port,
        }
    }

    /// Carry out `outputs` (crash recovery from `Engine::new` first), then run until the channel
    /// closes.
    pub fn run(mut self, startup: Vec<Output>, events: &Receiver<Event>) {
        self.granted = self.granted_permissions();
        self.execute(startup);
        self.feed(Input::LocalDisplays(self.local_displays.clone()));
        self.send_grants();
        self.update_layout(false);
        loop {
            while let Some(input) = self.pending.pop_front() {
                self.feed(input);
            }
            let now = platform::now();
            let timeout = self
                .engine
                .next_deadline()
                .map_or(HOUSEKEEPING, |deadline| {
                    Duration::from_nanos(deadline.as_nanos().saturating_sub(now.as_nanos()))
                })
                .min(HOUSEKEEPING);
            match events.recv_timeout(timeout) {
                Ok(Event::Shutdown) => {
                    self.shutdown();
                    return;
                }
                Ok(event) => self.on_event(event),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if self
                .engine
                .next_deadline()
                .is_some_and(|deadline| deadline <= platform::now())
            {
                self.feed(Input::Tick);
            }
            self.housekeeping();
            self.update_tray();
            if self.quit_requested {
                self.shutdown();
                return;
            }
            if self.restart_requested || self.permissions_changed() {
                self.shutdown();
                restart();
            }
        }
    }

    fn feed(&mut self, input: Input) {
        if tracing::enabled!(tracing::Level::DEBUG) {
            log_input(&input);
        }
        let outputs = self.engine.handle(input, platform::now());
        self.execute(outputs);
    }

    fn on_event(&mut self, event: Event) {
        match event {
            // `run` stops the loop for this one.
            Event::Shutdown => {}
            Event::Tray(crosspane_platform::TrayEvent::Chosen(id)) => {
                if let Some(action) = self.tray.actions.get(&id).cloned() {
                    self.tray_action(action);
                }
            }
            Event::Input(input) => self.feed(input),
            Event::LocalDisplays(displays) => {
                if displays == self.local_displays {
                    return;
                }
                self.local_displays = displays.clone();
                self.feed(Input::LocalDisplays(displays.clone()));
                self.broadcast(&ControlMessage::Displays(displays));
                self.update_layout(true);
            }
            Event::Link(event) => self.on_link(event),
            Event::Ctl(request @ (Request::WindowsFrom { .. } | Request::Pull { .. }), reply) => {
                self.start_waiting(request, reply);
            }
            Event::Ctl(request, reply) => {
                let response = self.on_ctl(request);
                let _ = reply.send(response);
            }
            Event::Host(event) => self.on_host(event),
            Event::Paired(paired) => self.on_paired(paired),
        }
    }

    fn on_link(&mut self, event: LinkEvent) {
        if let LinkEvent::Media { peer, data } = event {
            let _ = self.dest_media.send(DestCmd::Media { peer, data });
            return;
        }
        match &event {
            LinkEvent::Control { peer, msg } => match msg {
                ControlMessage::Hello(hello) => {
                    let peer = *peer;
                    let name = self
                        .trust
                        .with(|t| t.get(peer).map(|e| e.name.clone()))
                        .unwrap_or_else(|| hello.name.clone());
                    tracing::info!(peer = %peer.short(), %name, "peer connected");
                    let info = self.peers.entry(peer).or_default();
                    info.name = name;
                    info.displays = hello.displays.clone();
                    info.connected = true;
                    if let Some(link) = self.net.link(peer) {
                        self.links.insert(peer, link);
                    }
                    self.feed(Input::PeerUp { peer });
                    self.feed(Input::PeerDisplays {
                        peer,
                        displays: hello.displays.clone(),
                    });
                    // Tell the peer our view of the layout; it merges by version.
                    self.update_layout(false);
                    let explicit = self.explicit();
                    if !explicit.is_empty()
                        && let Some(link) = self.links.get_mut(&peer)
                    {
                        let _ = link.send_control(&ControlMessage::Layout(explicit));
                    }
                    return;
                }
                ControlMessage::Displays(displays) => {
                    let peer = *peer;
                    self.peers.entry(peer).or_default().displays = displays.clone();
                    self.feed(Input::PeerDisplays {
                        peer,
                        displays: displays.clone(),
                    });
                    self.update_layout(false);
                    return;
                }
                ControlMessage::Layout(placements) => {
                    // Version 0 is the derived default every node computes for itself; only
                    // explicit placements (version ≥ 1) travel.
                    let explicit: Vec<Placement> = placements
                        .iter()
                        .copied()
                        .filter(|p| p.version > 0)
                        .collect();
                    if arrange::merge(&mut self.placements, &explicit) {
                        self.update_layout(false);
                    }
                    return;
                }
                ControlMessage::Ping { .. } | ControlMessage::Pong { .. } => return,
                _ => {}
            },
            LinkEvent::Closed { peer, error } => {
                tracing::info!(peer = %peer.short(), ?error, "peer disconnected");
                self.links.remove(peer);
                if let Some(info) = self.peers.get_mut(peer) {
                    info.connected = false;
                    info.rtt = None;
                }
            }
            _ => {}
        }
        self.feed(Input::Link(event));
    }

    fn execute(&mut self, outputs: Vec<Output>) {
        for output in outputs {
            self.execute_one(output);
        }
    }

    fn execute_one(&mut self, output: Output) {
        if tracing::enabled!(tracing::Level::DEBUG) {
            log_output(&output);
        }
        match output {
            Output::SetPortals(portals) => {
                let portals = if self.crossing { portals } else { Vec::new() };
                if let Some(capture) = &mut self.platform.capture
                    && let Err(e) = capture.set_portals(&portals)
                {
                    tracing::warn!(error = %e, "set_portals failed");
                }
            }
            Output::MonitorLocalActivity(on) => {
                if let Some(capture) = &mut self.platform.capture {
                    match capture.set_monitor_local_activity(on) {
                        Ok(()) | Err(PlatformError::Unsupported(_)) => {}
                        Err(e) => tracing::warn!(error = %e, "local-activity monitoring failed"),
                    }
                }
            }
            Output::BeginCapture { id, portal } => {
                let result = match &mut self.platform.capture {
                    Some(capture) => capture.begin(id, portal).map_err(failure),
                    None => Err(Failure::Other),
                };
                if let Err(f) = &result {
                    tracing::info!(failure = ?f, "capture refused");
                }
                self.pending.push_back(Input::CaptureBegun { id, result });
            }
            Output::EndCapture { warp_to } => {
                if let Some(capture) = &mut self.platform.capture
                    && let Err(e) = capture.end(warp_to)
                {
                    tracing::warn!(error = %e, "end capture failed");
                }
            }
            Output::ShowOverlay { id, overlay } => {
                let shown = match &mut self.platform.overlay {
                    Some(host) => host.show(id, &overlay),
                    None => Err(PlatformError::Unsupported("no overlay backend")),
                };
                if let Err(e) = shown {
                    tracing::warn!(error = %e, "overlay unavailable");
                    self.pending
                        .push_back(Input::Overlay(OverlayEvent::Unavailable(id)));
                }
            }
            Output::HideOverlay(id) => {
                if let Some(host) = &mut self.platform.overlay
                    && let Err(e) = host.hide(id)
                {
                    tracing::warn!(error = %e, "hide overlay failed");
                }
            }
            Output::Inject { id, cmd } => {
                let ok = self.inject(cmd);
                self.pending.push_back(Input::InjectDone { id, ok });
            }
            Output::SendInput { peer, msg } => {
                if let Some(link) = self.links.get_mut(&peer) {
                    let _ = link.send_input(&msg);
                }
            }
            Output::SendMotion { peer, msg } => {
                if let Some(link) = self.links.get_mut(&peer) {
                    let _ = link.send_motion(&msg);
                }
            }
            Output::SendControl { peer, msg } => {
                if let Some(link) = self.links.get_mut(&peer) {
                    let _ = link.send_control(&msg);
                }
            }
            Output::EngineGate(open) => self.platform.gate.set_engine_permits(open),
            Output::Notice(notice) => self.notice(&notice),
            Output::Park {
                window,
                size,
                scale,
            } => {
                let result = match &mut self.platform.parking {
                    Some(p) => p.park(window, size, scale).map_err(failure),
                    None => Err(Failure::Other),
                };
                if let Err(f) = &result {
                    tracing::warn!(failure = ?f, "parking failed");
                }
                self.pending.push_back(Input::Parked { window, result });
            }
            Output::ResizeParked {
                window,
                size,
                scale,
            } => {
                let result = match &mut self.platform.parking {
                    Some(p) => p.resize(window, size, scale).map_err(failure),
                    None => Err(Failure::Other),
                };
                self.pending.push_back(Input::Parked { window, result });
            }
            Output::Restore { window } => {
                if let Some(p) = &mut self.platform.parking
                    && let Err(e) = p.restore(window)
                {
                    tracing::error!(error = %e, "could not restore a parked window");
                }
            }
            Output::ActivateWindow { window } => {
                if let Some(w) = &mut self.platform.windows
                    && let Err(e) = w.activate(window)
                {
                    tracing::debug!(error = %e, "activate failed");
                }
            }
            Output::StartCapture {
                projection,
                peer,
                target,
                crop,
                max_fps,
            } => {
                let result = match &mut self.platform.frames {
                    Some(frames) => {
                        let media = self.source_media.clone();
                        let events = self.events.clone();
                        let sink: Arc<dyn EventSink<FrameEvent>> =
                            Arc::new(move |ev: FrameEvent| match ev {
                                FrameEvent::Frame { stream, frame } => {
                                    let _ = media.send(SourceCmd::Frame { stream, frame });
                                }
                                FrameEvent::Ended { stream, reason } => {
                                    let _ = events
                                        .send(Event::Input(Input::CaptureEnded { stream, reason }));
                                }
                                _ => {}
                            });
                        frames.start(target, crop, max_fps, sink).map_err(failure)
                    }
                    None => Err(Failure::Other),
                };
                if let Ok(stream) = result {
                    self.streams.insert(stream, projection);
                    let _ = self.source_media.send(SourceCmd::Start {
                        stream,
                        projection,
                        peer,
                    });
                }
                self.pending
                    .push_back(Input::CaptureStarted { projection, result });
            }
            Output::SetCaptureCrop { stream, crop } => {
                if let Some(frames) = &mut self.platform.frames
                    && let Err(e) = frames.set_crop(stream, crop)
                {
                    tracing::warn!(error = %e, "set_crop failed");
                }
            }
            Output::StopCapture { stream } => {
                self.streams.remove(&stream);
                let _ = self.source_media.send(SourceCmd::Stop { stream });
                if let Some(frames) = &mut self.platform.frames {
                    let _ = frames.stop(stream);
                }
            }
            Output::RequestKeyFrame { projection } => {
                let _ = self.source_media.send(SourceCmd::RequestKey { projection });
            }
            Output::OpenProxy {
                key,
                title,
                app_id: _,
                size,
            } => {
                let id = self.proxy_ids.open(key);
                let sent = self
                    .host
                    .as_ref()
                    .is_some_and(|h| h.send(HostCommand::Open { id, title, size }).is_ok());
                if sent {
                    let from = self
                        .peers
                        .get(&key.source)
                        .map_or_else(|| key.source.short(), |info| info.name.clone());
                    let text = format!("showing window {} of {from}", key.projection.0);
                    // A pull from this peer is answered by the projection it started.
                    let pulls: Vec<u32> = self
                        .waiters
                        .iter()
                        .filter(|(_, w)| w.pull && w.peer == key.source)
                        .map(|(&r, _)| r)
                        .collect();
                    for request in pulls {
                        if let Some(w) = self.waiters.remove(&request) {
                            let _ = w.reply.send(Response::ok(json!(text.clone())));
                        }
                    }
                    self.projections.insert(key, text);
                } else {
                    self.proxy_ids.close(key);
                    self.pending.push_back(Input::ProxyOpened {
                        key,
                        result: Err(Failure::Other),
                    });
                }
            }
            Output::ProxyGeometry {
                key,
                size,
                parking: _,
            } => {
                if let (Some(h), Some(id)) = (&self.host, self.proxy_ids.id(key)) {
                    let _ = h.send(HostCommand::SetContentSize { id, size });
                }
            }
            Output::ProxyTitle { key, title } => {
                if let (Some(h), Some(id)) = (&self.host, self.proxy_ids.id(key)) {
                    let _ = h.send(HostCommand::SetTitle { id, title });
                }
            }
            Output::BrowseResult {
                peer,
                request,
                result,
            } => {
                if self.tray.browses.remove(&request).is_some() {
                    let windows = match result {
                        Ok(list) => RemoteWindows::List(
                            list.iter()
                                .map(|w| {
                                    (w.window, window_label(&w.summary.title, &w.summary.app_id))
                                })
                                .collect(),
                        ),
                        Err(Refusal::Permission) => RemoteWindows::NotAllowed,
                        Err(_) => RemoteWindows::Unknown,
                    };
                    self.tray.remote_windows.insert(peer, windows);
                    return;
                }
                let Some(waiter) = self.waiters.remove(&request) else {
                    return;
                };
                let name = self.peer_label(peer);
                let response = match result {
                    Ok(windows) => Response::ok(json!(
                        windows
                            .iter()
                            .map(|w| json!({
                                "id": w.window.0,
                                "app": w.summary.app_id,
                                "title": w.summary.title,
                                "size": [w.size.width, w.size.height],
                            }))
                            .collect::<Vec<_>>()
                    )),
                    Err(Refusal::Permission) => Response::err(format!(
                        "{name} doesn't let this machine browse its windows; on {name}, run: crosspanectl allow {} browse",
                        self.name
                    )),
                    Err(reason) => Response::err(format!("{name} refused: {reason:?}")),
                };
                let _ = waiter.reply.send(response);
            }
            Output::CloseProxy { key } => {
                self.projections.remove(&key);
                if let Some(id) = self.proxy_ids.close(key)
                    && let Some(h) = &self.host
                {
                    let _ = h.send(HostCommand::Close { id });
                }
                let _ = self.dest_media.send(DestCmd::Forget(key));
            }
            other => tracing::debug!(output = ?other, "unhandled engine output"),
        }
    }

    fn inject(&mut self, cmd: InjectCmd) -> bool {
        let keys = self.platform.keys.as_mut();
        let pointer = self.platform.pointer.as_mut();
        let result = match (cmd, keys, pointer) {
            (InjectCmd::Key { usage, down }, Some(k), _) => k.key(usage, down),
            (InjectCmd::LockKeys(lock), Some(k), _) => k.set_lock_keys(lock),
            (InjectCmd::Button { button, down }, _, Some(p)) => p.button(button, down),
            (InjectCmd::MoveTo { display, position }, _, Some(p)) => p.move_to(display, position),
            (InjectCmd::Scroll(delta), _, Some(p)) => p.scroll(delta),
            (InjectCmd::ReleaseAll, Some(k), Some(p)) => {
                let a = k.release_all();
                let b = p.release_all();
                a.and(b)
            }
            (InjectCmd::Recover { keys, buttons }, Some(k), Some(p)) => {
                let a = k.recover_keys(&keys);
                let b = p.recover_buttons(&buttons);
                a.and(b)
            }
            (cmd, _, _) => {
                tracing::debug!(?cmd, "no injector for command");
                return false;
            }
        };
        match result {
            Ok(()) => true,
            Err(e) => {
                tracing::debug!(error = %e, "injection refused");
                false
            }
        }
    }

    fn notice(&mut self, notice: &Notice) {
        let peer_name = |agent: &Agent, peer: &NodeId| {
            agent
                .peers
                .get(peer)
                .map_or_else(|| peer.short(), |info| info.name.clone())
        };
        let text = match notice {
            Notice::LostConnection(p) => format!("lost connection to {}", peer_name(self, p)),
            Notice::TargetLocked(p) => format!("{} is locked", peer_name(self, p)),
            // Both roles report a refusal with the other node as `peer`.
            Notice::Refused { peer, reason } => {
                format!("control with {} refused: {reason:?}", peer_name(self, peer))
            }
            Notice::ControlledBy(p) => format!("controlled by {}", peer_name(self, p)),
            Notice::ControlEnded(p) => format!("control by {} ended", peer_name(self, p)),
            Notice::LocalOverride(p) => format!("local input overrode {}", peer_name(self, p)),
            Notice::Panic => "panic: everything stopped; re-arm to continue".to_owned(),
            Notice::ProjectionStarted { key, peer, parking } => {
                let text = format!(
                    "projecting window {} to {} ({parking:?})",
                    key.projection.0,
                    peer_name(self, peer)
                );
                self.projections.insert(*key, text.clone());
                text
            }
            Notice::ProjectionEnded { key, reason } => {
                self.projections.remove(key);
                format!("projection {} ended: {reason:?}", key.projection.0)
            }
            Notice::ProjectionRefused { peer, reason } => {
                format!(
                    "{} refused the projection: {reason:?}",
                    peer_name(self, peer)
                )
            }
            other => format!("{other:?}"),
        };
        tracing::info!(notice = %text);
        if self.notices.len() == NOTICE_HISTORY {
            self.notices.pop_front();
        }
        self.notices.push_back(text);
    }

    fn granted_permissions(&self) -> Vec<Permission> {
        let p = &self.platform.permissions;
        p.required()
            .into_iter()
            .filter(|&perm| p.state(perm) == PermissionState::Granted)
            .collect()
    }

    /// True when an OS permission was granted or revoked since start: the backends that depend on
    /// it only change when the agent starts again (macOS often needs that for the grant to apply).
    /// The change must be seen on two checks in a row and the agent must have run for a while, so
    /// a flapping permission state can't make it restart in a loop.
    fn permissions_changed(&mut self) -> bool {
        if self.last_permission_check.elapsed() < PERMISSION_CHECK
            || self.started.elapsed() < MIN_RUN_BEFORE_RESTART
        {
            return false;
        }
        self.last_permission_check = Instant::now();
        let now = self.granted_permissions();
        if now == self.granted {
            self.granted_changing = None;
            return false;
        }
        if self.granted_changing.as_ref() != Some(&now) {
            self.granted_changing = Some(now);
            return false;
        }
        tracing::info!(before = ?self.granted, now = ?now, "OS permissions changed");
        true
    }

    /// A forgotten (or revoked) peer's connection ends at once (05 scenario 1), not at its next
    /// handshake: its link is closed and it drops out of the peer list.
    fn close_untrusted(&mut self) {
        let trusted: BTreeSet<NodeId> = self
            .trust
            .with(|t| t.peers().iter().map(|e| e.node).collect());
        let gone: Vec<NodeId> = self
            .links
            .keys()
            .chain(self.peers.keys())
            .filter(|node| !trusted.contains(node))
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        for node in gone {
            tracing::info!(peer = %node.short(), "peer no longer trusted: disconnecting");
            if let Some(link) = self.links.get_mut(&node) {
                link.close("forgotten");
            }
            self.peers.remove(&node);
        }
    }

    fn start_waiting(&mut self, request: Request, reply: Sender<Response>) {
        let (peer, window) = match &request {
            Request::WindowsFrom { peer } => (peer.clone(), None),
            Request::Pull { peer, window } => (peer.clone(), Some(WindowId(*window))),
            _ => return,
        };
        let Some(node) = self.find_peer(&peer) else {
            let _ = reply.send(Response::err(format!("no peer called {peer}")));
            return;
        };
        let request = self.next_request;
        self.next_request = self.next_request.wrapping_add(1).max(1);
        self.waiters.insert(
            request,
            Waiter {
                reply,
                peer: node,
                pull: window.is_some(),
                deadline: Instant::now() + BROWSE_WAIT,
            },
        );
        let command = match window {
            None => Command::Browse {
                peer: node,
                request,
            },
            Some(window) => Command::Pull {
                peer: node,
                window,
                request,
            },
        };
        self.feed(Input::Command(command));
    }

    /// Rebuild the tray menu (at most every second) and show it if it changed.
    fn update_tray(&mut self) {
        if self.platform.tray.is_none() || self.tray.last_update.elapsed() < TRAY_UPDATE {
            return;
        }
        self.tray.last_update = Instant::now();
        if self
            .tray
            .last_local_windows
            .is_none_or(|t| t.elapsed() >= TRAY_LOCAL_WINDOWS)
        {
            self.tray.last_local_windows = Some(Instant::now());
            if let Some(Ok(list)) = self.platform.windows.as_ref().map(|w| w.windows()) {
                self.tray.local_windows = list
                    .iter()
                    .filter(|w| {
                        // Untitled windows are mostly helpers (tray proxies, splash screens).
                        !w.title.trim().is_empty()
                            && matches!(
                                w.role,
                                crosspane_platform::WindowRole::Toplevel
                                    | crosspane_platform::WindowRole::Dialog
                            )
                    })
                    .map(|w| (w.id, window_label(&w.title, &w.app_id)))
                    .collect();
            }
        }
        if self
            .tray
            .last_browse
            .is_none_or(|t| t.elapsed() >= TRAY_BROWSE)
        {
            self.tray.last_browse = Some(Instant::now());
            self.tray.browses.clear();
            let connected: Vec<NodeId> = self
                .peers
                .iter()
                .filter(|(_, p)| p.connected)
                .map(|(n, _)| *n)
                .collect();
            for peer in connected {
                let request = self.next_request;
                self.next_request = self.next_request.wrapping_add(1).max(1);
                self.tray.browses.insert(request, peer);
                self.feed(Input::Command(Command::Browse { peer, request }));
            }
        }
        let view = self.tray_view();
        let (menu, actions) = tray::build(&view);
        if self.tray.menu.as_ref() == Some(&menu) {
            return;
        }
        if let Some(host) = self.platform.tray.as_mut() {
            match host.set(&menu) {
                Ok(()) => {
                    self.tray.menu = Some(menu);
                    self.tray.actions = actions;
                }
                Err(e) => tracing::debug!(error = %e, "tray not shown"),
            }
        }
    }

    fn tray_view(&self) -> TrayView {
        let trust: Vec<(NodeId, String, bool, bool)> = self.trust.with(|t| {
            t.peers()
                .iter()
                .map(|e| {
                    (
                        e.node,
                        e.name.clone(),
                        e.granted.contains(&Capability::InputAccept),
                        e.granted.contains(&Capability::WindowBrowse),
                    )
                })
                .collect()
        });
        let peers = trust
            .into_iter()
            .map(|(node, name, input, browse)| PeerView {
                node,
                name,
                connected: self.peers.get(&node).is_some_and(|p| p.connected),
                side: self.tray.sides.get(&node).copied(),
                windows: self
                    .tray
                    .remote_windows
                    .get(&node)
                    .cloned()
                    .unwrap_or_default(),
                allows_input: input,
                allows_browse: browse,
            })
            .collect();
        let status = self.pairing.status();
        let missing_permissions = {
            let p = &self.platform.permissions;
            p.required()
                .into_iter()
                .filter(|&perm| p.state(perm) != PermissionState::Granted)
                .collect()
        };
        TrayView {
            name: self.name.clone(),
            peers,
            local_windows: self.tray.local_windows.clone(),
            projections: self
                .projections
                .iter()
                .map(|(k, text)| (*k, text.clone()))
                .collect(),
            controlling: self.engine.controlling().map(|n| self.peer_label(n)),
            controlled_by: self.engine.controlled_by().map(|n| self.peer_label(n)),
            disarmed: !self.engine.armed(),
            missing_permissions,
            pairing: PairingView {
                phase: status.phase,
                sas: status.sas,
                candidates: status.candidates,
                peer: status.peer,
                error: status.error,
                offers: Vec::new(),
            },
        }
    }

    fn tray_action(&mut self, action: TrayAction) {
        let name = |agent: &Agent, node: NodeId| agent.peer_label(node);
        let response = match action {
            TrayAction::Release => self.on_ctl(Request::Release),
            TrayAction::Panic => self.on_ctl(Request::Panic),
            TrayAction::Rearm => self.on_ctl(Request::Rearm),
            TrayAction::Restart => self.on_ctl(Request::Restart),
            TrayAction::Quit => {
                self.quit_requested = true;
                return;
            }
            TrayAction::Project { window, to } => {
                self.feed(Input::Command(Command::Project { window, to }));
                return;
            }
            TrayAction::Pull { peer, window } => {
                let request = self.next_request;
                self.next_request = self.next_request.wrapping_add(1).max(1);
                self.feed(Input::Command(Command::Pull {
                    peer,
                    window,
                    request,
                }));
                return;
            }
            TrayAction::Return(key) => {
                self.feed(Input::Command(Command::Return(key)));
                return;
            }
            TrayAction::Layout { peer, side } => {
                let peer = name(self, peer);
                self.on_ctl(Request::Layout { peer, side })
            }
            TrayAction::Allow {
                peer,
                capability,
                allow,
            } => {
                let capability = match capability {
                    Capability::InputAccept => "input",
                    Capability::WindowShare => "share",
                    Capability::WindowBrowse => "browse",
                    _ => "present",
                };
                let peer = name(self, peer);
                self.on_ctl(Request::Allow {
                    peer,
                    capability: capability.to_owned(),
                    allow,
                })
            }
            TrayAction::PairListen => self.on_ctl(Request::PairListen { allow_input: true }),
            TrayAction::PairJoin(addr) => self.on_ctl(Request::PairJoin {
                addr,
                allow_input: true,
            }),
            TrayAction::PairConfirm(accept) => self.on_ctl(Request::PairConfirm { accept }),
            TrayAction::PairPick(index) => self.on_ctl(Request::PairPick { index }),
            TrayAction::OpenSettings(permission) => {
                crate::open_settings_pane(permission);
                return;
            }
        };
        if let Some(error) = response.error {
            self.notices.push_back(error);
        }
        // Show the effect at once rather than at the next refresh.
        self.tray.last_update = Instant::now() - TRAY_UPDATE;
    }

    fn peer_label(&self, node: NodeId) -> String {
        self.peers
            .get(&node)
            .map_or_else(|| node.short(), |info| info.name.clone())
    }

    fn housekeeping(&mut self) {
        let expired: Vec<u32> = self
            .waiters
            .iter()
            .filter(|(_, w)| w.deadline <= Instant::now())
            .map(|(&r, _)| r)
            .collect();
        for request in expired {
            if let Some(w) = self.waiters.remove(&request) {
                let name = self.peer_label(w.peer);
                let _ = w.reply.send(if w.pull {
                    Response::ok(json!(format!("asked {name}; no answer yet")))
                } else {
                    Response::err(format!("no answer from {name}"))
                });
            }
        }
        if self.last_trust_check.elapsed() >= HOUSEKEEPING {
            self.last_trust_check = Instant::now();
            match self.trust.refresh() {
                Ok(true) => {
                    tracing::info!("trust store changed");
                    self.close_untrusted();
                    self.send_grants();
                }
                Ok(false) => {}
                Err(e) => tracing::warn!(error = %e, "trust store reload failed"),
            }
        }
        if self.last_rtt_poll.elapsed() >= HOUSEKEEPING {
            self.last_rtt_poll = Instant::now();
            let rtts: Vec<_> = self
                .links
                .iter()
                .filter_map(|(peer, link)| link.rtt().map(|rtt| (*peer, rtt)))
                .collect();
            for (peer, rtt) in rtts {
                if let Some(info) = self.peers.get_mut(&peer) {
                    info.rtt = Some(rtt);
                }
                self.feed(Input::PeerRtt { peer, rtt });
            }
        }
    }

    fn send_grants(&mut self) {
        // A node that can't inject (e.g. macOS without the Accessibility grant) refuses control
        // rather than accept a session whose input would go nowhere.
        let can_inject = self.platform.keys.is_some() && self.platform.pointer.is_some();
        let grants: BTreeMap<NodeId, BTreeSet<Capability>> = self.trust.with(|t| {
            t.peers()
                .into_iter()
                .map(|e| {
                    let mut granted = e.granted.clone();
                    if !can_inject {
                        granted.remove(&Capability::InputAccept);
                    }
                    (e.node, granted)
                })
                .collect()
        });
        self.feed(Input::Grants(grants));
    }

    fn broadcast(&mut self, msg: &ControlMessage) {
        for link in self.links.values_mut() {
            let _ = link.send_control(msg);
        }
    }

    /// Fill in default placements for any node that has none (version 0, computed identically on
    /// every peer), re-arrange this node's own displays after a local change, and tell the engine.
    fn update_layout(&mut self, local_changed: bool) {
        let mut nodes = vec![(self.node, self.local_displays.clone())];
        nodes.extend(
            self.peers
                .iter()
                .filter(|(_, info)| !info.displays.is_empty())
                .map(|(node, info)| (*node, info.displays.clone())),
        );
        // Defaults (version 0) are derived from the current set of nodes every time, so a default
        // computed while alone never conflicts with the joint one. Explicit placements (version ≥
        // 1, from `crosspanectl layout`) win over defaults through `merge`.
        let before_all = self.placements.clone();
        self.placements.retain(|p| p.version > 0);
        arrange::merge(&mut self.placements, &arrange::default_layout(&nodes));
        if local_changed
            && self
                .placements
                .iter()
                .any(|p| p.node == self.node && p.version > 0)
        {
            self.rearrange_local();
        }
        let mut changed = self.placements != before_all;
        // Drop placements for displays that no longer exist.
        let known: BTreeSet<_> = nodes
            .iter()
            .flat_map(|(node, displays)| displays.iter().map(move |d| (*node, d.id)))
            .collect();
        let before = self.placements.len();
        self.placements
            .retain(|p| known.contains(&(p.node, p.display)));
        changed |= self.placements.len() != before;
        if changed {
            self.feed(Input::Layout(self.placements.clone()));
            if local_changed {
                self.broadcast(&ControlMessage::Layout(self.explicit()));
            }
        }
    }

    /// Re-place this node's displays from the OS arrangement, keeping the node's top-left corner
    /// where it was.
    fn rearrange_local(&mut self) -> bool {
        let own: Vec<_> = self
            .placements
            .iter()
            .filter(|p| p.node == self.node)
            .collect();
        let (min_x, min_y) = own
            .iter()
            .fold((f64::INFINITY, f64::INFINITY), |(x, y), p| {
                (x.min(p.origin.x), y.min(p.origin.y))
            });
        let (min_x, min_y) = if min_x.is_finite() {
            (min_x, min_y)
        } else {
            (0.0, 0.0)
        };
        let version = self.placements.iter().map(|p| p.version).max().unwrap_or(0) + 1;
        let fresh: Vec<Placement> = arrange::arrange_node(&self.local_displays)
            .into_iter()
            .map(|(display, origin)| Placement {
                node: self.node,
                display,
                origin: crosspane_types::geom::PointMm::new(origin.x + min_x, origin.y + min_y),
                version,
            })
            .collect();
        arrange::merge(&mut self.placements, &fresh)
    }

    /// Put `peer` on `side` of this node (user action): new versions win on every peer.
    fn place_peer(&mut self, peer: NodeId, side: Side) -> Result<(), String> {
        let info = self.peers.get(&peer).ok_or("unknown peer")?;
        if info.displays.is_empty() {
            return Err("the peer has reported no displays yet".into());
        }
        let nodes = vec![(self.node, self.local_displays.clone())];
        let ours = arrange::placed(&self.placements, &nodes);
        let theirs: Vec<(DisplayInfo, crosspane_types::geom::PointMm)> =
            arrange::arrange_node(&info.displays)
                .into_iter()
                .filter_map(|(id, origin)| {
                    info.displays
                        .iter()
                        .find(|d| d.id == id)
                        .map(|d| (d.clone(), origin))
                })
                .collect();
        let origins = arrange::place_beside(&ours, &theirs, side);
        let version = self.placements.iter().map(|p| p.version).max().unwrap_or(0) + 1;
        let fresh: Vec<Placement> = theirs
            .iter()
            .zip(origins)
            .map(|((display, _), origin)| Placement {
                node: peer,
                display: display.id,
                origin,
                version,
            })
            .collect();
        arrange::merge(&mut self.placements, &fresh);
        // Pin this node's own displays explicitly too, so the relation survives reconnects.
        let own: Vec<Placement> = self
            .placements
            .iter()
            .filter(|p| p.node == self.node)
            .map(|p| Placement {
                version: version.max(p.version),
                ..*p
            })
            .collect();
        arrange::merge(&mut self.placements, &own);
        self.feed(Input::Layout(self.placements.clone()));
        self.broadcast(&ControlMessage::Layout(self.explicit()));
        Ok(())
    }

    fn explicit(&self) -> Vec<Placement> {
        self.placements
            .iter()
            .copied()
            .filter(|p| p.version > 0)
            .collect()
    }

    fn find_peer(&self, query: &str) -> Option<NodeId> {
        let query = query.to_lowercase();
        self.peers
            .iter()
            .find(|(node, info)| {
                info.name.to_lowercase() == query || node.to_string().starts_with(&query)
            })
            .map(|(node, _)| *node)
    }

    fn on_paired(&mut self, paired: crate::pairing::Paired) {
        let peer = paired.peer;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        let entry = crosspane_security::trust::PeerEntry {
            node: peer.node,
            spki: peer.spki.clone(),
            name: peer.name.clone(),
            granted: paired.granted,
            paired_at_ms: now_ms,
        };
        match self
            .trust
            .update(|t| t.pin(entry).map_err(|e| anyhow::anyhow!("{e}")))
        {
            Ok(()) => {
                tracing::info!(peer = %peer.node.short(), name = %peer.name, "paired");
                self.notices.push_back(format!("paired with {}", peer.name));
                self.send_grants();
                if let Some(addr) = paired.dial {
                    self.net.dial(addr);
                }
            }
            Err(e) => tracing::error!(error = %e, "could not pin the paired peer"),
        }
    }

    fn on_host(&mut self, event: HostEvent) {
        let (id, input_of): (u64, Box<dyn FnOnce(ProjectionKey) -> Input>) = match event {
            HostEvent::Opened { id, size, scale } => (
                id,
                Box::new(move |key| Input::ProxyOpened {
                    key,
                    result: Ok((size, scale)),
                }),
            ),
            HostEvent::OpenFailed { id, error } => {
                tracing::warn!(%error, "proxy window failed to open");
                (
                    id,
                    Box::new(|key| Input::ProxyOpened {
                        key,
                        result: Err(Failure::Other),
                    }),
                )
            }
            HostEvent::Resized { id, size, scale } => (
                id,
                Box::new(move |key| proxy(key, ProxyEvent::Resized { size, scale })),
            ),
            HostEvent::Focus { id, focused } => (
                id,
                Box::new(move |key| proxy(key, ProxyEvent::Focus(focused))),
            ),
            HostEvent::CloseRequested { id } => {
                (id, Box::new(|key| proxy(key, ProxyEvent::CloseRequested)))
            }
            HostEvent::Lost { id } => (id, Box::new(|key| proxy(key, ProxyEvent::Lost))),
            HostEvent::Key { id, usage, down } => (
                id,
                Box::new(move |key| proxy(key, ProxyEvent::Key { usage, down })),
            ),
            HostEvent::Button {
                id,
                button,
                down,
                position,
            } => (
                id,
                Box::new(move |key| {
                    proxy(
                        key,
                        ProxyEvent::Button {
                            button,
                            down,
                            position,
                        },
                    )
                }),
            ),
            HostEvent::Scroll {
                id,
                delta,
                position,
            } => (
                id,
                Box::new(move |key| proxy(key, ProxyEvent::Scroll { delta, position })),
            ),
            HostEvent::Motion { id, position } => (
                id,
                Box::new(move |key| proxy(key, ProxyEvent::Motion { position })),
            ),
        };
        if let Some(key) = self.proxy_ids.key(id) {
            self.feed(input_of(key));
        }
    }

    fn on_ctl(&mut self, request: Request) -> Response {
        match request {
            Request::Status => Response::ok(self.status()),
            Request::Release => {
                self.feed(Input::Command(Command::ReleaseControl));
                Response::ok(json!("released"))
            }
            Request::Panic => {
                self.feed(Input::Command(Command::Panic));
                Response::ok(json!("panic"))
            }
            Request::Allow {
                peer,
                capability,
                allow,
            } => {
                let capability = match capability.as_str() {
                    "input" => Capability::InputAccept,
                    "share" => Capability::WindowShare,
                    "browse" => Capability::WindowBrowse,
                    "present" => Capability::WindowPresent,
                    other => {
                        return Response::err(format!(
                            "unknown capability {other}: use input, share, browse or present"
                        ));
                    }
                };
                match self.find_peer(&peer) {
                    Some(node) => match self.trust.update(|t| {
                        t.set_grant(node, capability, allow)
                            .map_err(|e| anyhow::anyhow!("{e}"))
                    }) {
                        Ok(()) => {
                            self.send_grants();
                            Response::ok(json!(format!(
                                "{} {capability:?} for {peer}",
                                if allow { "allowed" } else { "withdrew" }
                            )))
                        }
                        Err(e) => Response::err(format!("could not update the trust store: {e}")),
                    },
                    None => Response::err(format!("no peer called {peer}")),
                }
            }
            Request::WindowsFrom { .. } | Request::Pull { .. } => {
                Response::err("internal: answered asynchronously")
            }
            Request::Forget { peer } => match self.find_peer(&peer) {
                Some(node) => match self.trust.update(|t| Ok(t.forget(node))) {
                    Ok(Some(entry)) => {
                        self.close_untrusted();
                        self.send_grants();
                        Response::ok(json!(format!("forgot {}", entry.name)))
                    }
                    Ok(None) => Response::err(format!("{peer} is not paired")),
                    Err(e) => Response::err(format!("could not update the trust store: {e}")),
                },
                None => Response::err(format!("no peer called {peer}")),
            },
            Request::Restart => {
                self.restart_requested = true;
                Response::ok(json!("restarting"))
            }
            Request::Rearm => {
                self.feed(Input::Command(Command::Rearm));
                Response::ok(json!("re-armed"))
            }
            Request::Layout { peer, side } => match self.find_peer(&peer) {
                None => Response::err(format!("no connected peer matches {peer:?}")),
                Some(node) => match self.place_peer(node, side) {
                    Ok(()) => {
                        self.tray.sides.insert(node, side);
                        Response::ok(json!("layout updated"))
                    }
                    Err(e) => Response::err(e),
                },
            },
            Request::Dial { addr } => {
                self.net.dial(addr);
                Response::ok(json!(format!("dialing {addr}")))
            }
            Request::PairListen { allow_input } => {
                match self.pairing.listen(
                    &self.net.runtime(),
                    self.port,
                    self.identity.clone(),
                    self.name.clone(),
                    allow_input,
                    self.events.clone(),
                ) {
                    Ok(()) => Response::ok(json!("pairing window open for 120 s")),
                    Err(e) => Response::err(e),
                }
            }
            Request::PairJoin { addr, allow_input } => {
                match self.pairing.join(
                    &self.net.runtime(),
                    addr,
                    self.identity.clone(),
                    self.name.clone(),
                    allow_input,
                    self.events.clone(),
                ) {
                    Ok(()) => Response::ok(json!(format!("joining {addr}"))),
                    Err(e) => Response::err(e),
                }
            }
            Request::PairStatus => Response::ok(json!(self.pairing.status())),
            Request::PairConfirm { accept } => {
                match self
                    .pairing
                    .decide(crate::pairing::Decision::Confirm(accept))
                {
                    Ok(()) => Response::ok(json!(if accept { "confirmed" } else { "rejected" })),
                    Err(e) => Response::err(e),
                }
            }
            Request::PairPick { index } => {
                match self.pairing.decide(crate::pairing::Decision::Pick(index)) {
                    Ok(()) => Response::ok(json!("picked")),
                    Err(e) => Response::err(e),
                }
            }
            Request::Windows => match &self.platform.windows {
                None => Response::err("no window source on this node"),
                Some(w) => match w.windows() {
                    Ok(list) => Response::ok(json!(
                        list.iter()
                            .map(|w| json!({
                                "id": w.id.0,
                                "app": w.app_id,
                                "title": w.title,
                                "display": w.display.map(|d| d.0),
                                "size": [w.frame.size.width, w.frame.size.height],
                            }))
                            .collect::<Vec<_>>()
                    )),
                    Err(e) => Response::err(format!("{e}")),
                },
            },
            Request::Project { window, peer } => match self.find_peer(&peer) {
                None => Response::err(format!("no connected peer matches {peer:?}")),
                Some(to) => {
                    self.feed(Input::Command(Command::Project {
                        window: WindowId(window),
                        to,
                    }));
                    Response::ok(json!("projection offered"))
                }
            },
            Request::Return { projection, source } => {
                let source = match source {
                    None => Some(self.node),
                    Some(s) => self.find_peer(&s),
                };
                match source {
                    None => Response::err("unknown source node"),
                    Some(source) => {
                        let key = ProjectionKey {
                            source,
                            projection: ProjectionId(projection),
                        };
                        self.feed(Input::Command(Command::Return(key)));
                        Response::ok(json!("returning"))
                    }
                }
            }
        }
    }

    /// Stop cleanly, in the order that keeps the invariants (04 §8): injected input is released and
    /// control ends (as `crosspanectl panic`), parked windows go back where they were (the journal
    /// would otherwise bring them back only at the next start), then the links close so peers end
    /// their sessions at once instead of after the idle timeout.
    fn shutdown(&mut self) {
        tracing::info!("stopping");
        self.feed(Input::Command(Command::Panic));
        if let Some(parking) = self.platform.parking.as_mut() {
            match parking.recover() {
                Ok(windows) => tracing::info!(restored = windows.len(), "parked windows restored"),
                Err(error) => {
                    tracing::warn!(%error, "parked windows not restored; the next start restores them")
                }
            }
        }
        self.net.shutdown();
    }

    fn status(&self) -> Value {
        let now = platform::now();
        json!({
            "node": self.node.to_string(),
            "name": self.name,
            "listening": self.net.local_addr().to_string(),
            "gate_open": self.platform.gate.is_open(),
            "session": format!("{:?}", self.platform.session.state()),
            "backends": format!("{:?}", self.platform),
            "permissions": self.platform.permissions.required().into_iter().map(|p| {
                let name = format!("{p:?}");
                json!({ "permission": name, "state": format!("{:?}", self.platform.permissions.state(p)) })
            }).collect::<Vec<_>>(),
            "displays": self.local_displays.iter().map(display_json).collect::<Vec<_>>(),
            "peers": self.peers.iter().map(|(node, info)| json!({
                "node": node.to_string(),
                "name": info.name,
                "connected": info.connected,
                "rtt_ms": info.rtt.map(|r| r.as_secs_f64() * 1000.0),
                "displays": info.displays.iter().map(display_json).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "layout": self.placements.iter().map(|p| json!({
                "node": p.node.short(),
                "display": p.display.0,
                "origin_mm": [p.origin.x, p.origin.y],
                "version": p.version,
            })).collect::<Vec<_>>(),
            "notices": self.notices.iter().collect::<Vec<_>>(),
            "projections": self.projections.iter().map(|(k, text)| {
                let frames = self.proxy_ids.stats(*k).map(|s| json!({
                    "frames": s.frames,
                    "bytes": s.bytes,
                    "last_ms_ago": s.last.map(|t| t.elapsed().as_millis() as u64),
                }));
                json!({
                    "source": k.source.short(),
                    "projection": k.projection.0,
                    "text": text,
                    "received": frames,
                })
            }).collect::<Vec<_>>(),
            "uptime_s": now.as_nanos() / 1_000_000_000,
        })
    }
}

fn display_json(d: &DisplayInfo) -> Value {
    json!({
        "id": d.id.0,
        "name": d.name,
        "pixels": [d.geometry.pixel_size.width, d.geometry.pixel_size.height],
        "scale": d.geometry.scale,
        "mm": [d.geometry.physical_size.width, d.geometry.physical_size.height],
        "origin": [d.geometry.logical_origin.x, d.geometry.logical_origin.y],
    })
}

fn failure(e: PlatformError) -> Failure {
    match e {
        PlatformError::Locked => Failure::Locked,
        PlatformError::SecureInput => Failure::SecureInput,
        PlatformError::PointerButtonHeld => Failure::PointerButtonHeld,
        PlatformError::PermissionDenied(_) => Failure::PermissionDenied,
        _ => Failure::Other,
    }
}

/// Wrap platform event streams into the loop's channel.
pub fn subscribe_platform(platform: &mut Platform, tx: &Sender<Event>) {
    let sink = |tx: &Sender<Event>| tx.clone();
    let session_tx = sink(tx);
    if let Err(e) = platform.session.subscribe(std::sync::Arc::new(move |ev| {
        let _ = session_tx.send(Event::Input(Input::Session(ev)));
    })) {
        tracing::error!(error = %e, "session events unavailable: I/O stays blocked");
    }
    let displays_tx = sink(tx);
    if let Err(e) = platform
        .displays
        .subscribe(std::sync::Arc::new(move |displays| {
            let _ = displays_tx.send(Event::LocalDisplays(displays));
        }))
    {
        tracing::warn!(error = %e, "display changes unavailable");
    }
    if let Some(capture) = &mut platform.capture {
        let capture_tx = sink(tx);
        if let Err(e) = capture.subscribe(std::sync::Arc::new(move |ev: CaptureEvent| {
            let _ = capture_tx.send(Event::Input(Input::Capture(ev)));
        })) {
            tracing::warn!(error = %e, "capture events unavailable");
            platform.capture = None;
        }
    }
    if let Some(overlay) = &mut platform.overlay {
        let overlay_tx = sink(tx);
        if let Err(e) = overlay.subscribe(std::sync::Arc::new(move |ev| {
            let _ = overlay_tx.send(Event::Input(Input::Overlay(ev)));
        })) {
            tracing::warn!(error = %e, "overlay events unavailable");
            platform.overlay = None;
        }
    }
    if let Some(tray) = &mut platform.tray {
        let tray_tx = sink(tx);
        if let Err(e) = tray.subscribe(std::sync::Arc::new(move |ev| {
            let _ = tray_tx.send(Event::Tray(ev));
        })) {
            tracing::warn!(error = %e, "tray events unavailable");
            platform.tray = None;
        }
    }
    if let Some(windows) = &mut platform.windows {
        let windows_tx = sink(tx);
        if let Err(e) = windows.subscribe(std::sync::Arc::new(move |ev: WindowEvent| {
            let _ = windows_tx.send(Event::Input(Input::Windows(ev)));
        })) {
            tracing::warn!(error = %e, "window events unavailable");
        }
    }
    if let Some(hotkeys) = &mut platform.hotkeys {
        let hotkey_tx = sink(tx);
        if let Err(e) = hotkeys.subscribe(std::sync::Arc::new(move |ev| {
            let _ = hotkey_tx.send(Event::Input(Input::Hotkey(ev)));
        })) {
            tracing::warn!(error = %e, "global hotkeys unavailable");
        }
    }
}

/// Debug trace of engine inputs, without motion noise or key contents (04 §7: logs never
/// record keys).
fn log_input(input: &Input) {
    match input {
        Input::Tick => {}
        Input::Capture(CaptureEvent::Motion { dx, dy, .. }) => {
            tracing::trace!(dx, dy, "in: motion")
        }
        Input::Capture(CaptureEvent::Key { .. }) => tracing::debug!("in: capture key"),
        Input::Link(LinkEvent::Motion { msg, .. }) => {
            tracing::trace!(pos = ?msg.position, "in: link motion")
        }
        Input::Link(LinkEvent::Input { peer, msg }) => match msg {
            crosspane_protocol::msg::InputMessage::Key { .. } => {
                tracing::debug!(peer = %peer.short(), "in: link key")
            }
            other => tracing::debug!(peer = %peer.short(), msg = ?other, "in: link input"),
        },
        other => tracing::debug!(input = ?other, "in"),
    }
}

fn log_output(output: &Output) {
    match output {
        Output::SendMotion { msg, .. } => tracing::trace!(pos = ?msg.position, "out: motion"),
        Output::Inject {
            cmd: crosspane_engine::InjectCmd::MoveTo { position, .. },
            ..
        } => {
            tracing::trace!(?position, "out: move")
        }
        Output::Inject {
            id,
            cmd: crosspane_engine::InjectCmd::Key { down, .. },
        } => {
            tracing::debug!(?id, down, "out: inject key")
        }
        Output::SendInput {
            peer,
            msg: crosspane_protocol::msg::InputMessage::Key { down, .. },
        } => {
            tracing::debug!(peer = %peer.short(), down, "out: send key")
        }
        other => tracing::debug!(output = ?other, "out"),
    }
}

fn proxy(key: ProjectionKey, event: ProxyEvent) -> Input {
    Input::Proxy { key, event }
}

/// Start this agent again in place (same binary, same arguments, same PID), after a clean
/// shutdown. Exits if that fails, rather than run on with closed links.
fn restart() -> ! {
    use std::os::unix::process::CommandExt;
    tracing::info!("restarting");
    let error = match std::env::current_exe() {
        Ok(exe) => std::process::Command::new(exe)
            .args(std::env::args_os().skip(1))
            .exec(),
        Err(error) => error,
    };
    tracing::error!(%error, "could not restart; exiting");
    std::process::exit(1);
}
