//! The engine loop (03 §1): one thread owns the [`Engine`] and the platform backends, feeds the
//! engine every event, and carries out its outputs. Everything else (network, control socket,
//! backend threads) talks to it through one channel.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use crosspane_engine::{Engine, Failure, InjectCmd, Input, Notice, Output};
use crosspane_input::arrange::{self, Side};
use crosspane_platform::{CaptureEvent, OverlayEvent, PlatformError};
use crosspane_protocol::link::{LinkEvent, PeerLink};
use crosspane_protocol::msg::{Capability, ControlMessage, Placement};
use crosspane_types::display::DisplayInfo;
use crosspane_types::id::NodeId;
use serde_json::{Value, json};

use crate::ctl::{Request, Response};
use crate::net::Net;
use crate::platform::{self, Platform};
use crate::trust::SharedTrust;

/// Everything the engine loop reacts to.
pub enum Event {
    /// A platform event, already in engine terms.
    Input(Input),
    LocalDisplays(Vec<DisplayInfo>),
    Link(LinkEvent),
    Ctl(Request, Sender<Response>),
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
}

const NOTICE_HISTORY: usize = 20;
const HOUSEKEEPING: Duration = Duration::from_secs(1);

impl Agent {
    pub fn new(
        node: NodeId,
        name: String,
        engine: Engine,
        platform: Platform,
        net: Net,
        trust: SharedTrust,
        local_displays: Vec<DisplayInfo>,
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
        }
    }

    /// Carry out `outputs` (crash recovery from `Engine::new` first), then run until the channel
    /// closes.
    pub fn run(mut self, startup: Vec<Output>, events: &Receiver<Event>) {
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
        }
    }

    fn feed(&mut self, input: Input) {
        let outputs = self.engine.handle(input, platform::now());
        self.execute(outputs);
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::Input(input) => self.feed(input),
            Event::LocalDisplays(displays) => {
                self.local_displays = displays.clone();
                self.feed(Input::LocalDisplays(displays.clone()));
                self.broadcast(&ControlMessage::Displays(displays));
                self.update_layout(true);
            }
            Event::Link(event) => self.on_link(event),
            Event::Ctl(request, reply) => {
                let response = self.on_ctl(request);
                let _ = reply.send(response);
            }
        }
    }

    fn on_link(&mut self, event: LinkEvent) {
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
                    self.feed(Input::PeerDisplays { peer, displays: hello.displays.clone() });
                    // Tell the peer our view of the layout; it merges by version.
                    self.update_layout(false);
                    if let Some(link) = self.links.get_mut(&peer) {
                        let _ = link.send_control(&ControlMessage::Layout(self.placements.clone()));
                    }
                    return;
                }
                ControlMessage::Displays(displays) => {
                    let peer = *peer;
                    self.peers.entry(peer).or_default().displays = displays.clone();
                    self.feed(Input::PeerDisplays { peer, displays: displays.clone() });
                    self.update_layout(false);
                    return;
                }
                ControlMessage::Layout(placements) => {
                    if arrange::merge(&mut self.placements, placements) {
                        self.feed(Input::Layout(self.placements.clone()));
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
            LinkEvent::Input { .. } | LinkEvent::Motion { .. } => {}
        }
        self.feed(Input::Link(event));
    }

    fn execute(&mut self, outputs: Vec<Output>) {
        for output in outputs {
            self.execute_one(output);
        }
    }

    fn execute_one(&mut self, output: Output) {
        match output {
            Output::SetPortals(portals) => {
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
                    self.pending.push_back(Input::Overlay(OverlayEvent::Unavailable(id)));
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
            Notice::Refused { peer, reason } => {
                format!("{} refused control: {reason:?}", peer_name(self, peer))
            }
            Notice::ControlledBy(p) => format!("controlled by {}", peer_name(self, p)),
            Notice::ControlEnded(p) => format!("control by {} ended", peer_name(self, p)),
            Notice::LocalOverride(p) => format!("local input overrode {}", peer_name(self, p)),
            Notice::Panic => "panic: everything stopped; re-arm to continue".to_owned(),
            other => format!("{other:?}"),
        };
        tracing::info!(notice = %text);
        if self.notices.len() == NOTICE_HISTORY {
            self.notices.pop_front();
        }
        self.notices.push_back(text);
    }

    fn housekeeping(&mut self) {
        if self.last_trust_check.elapsed() >= HOUSEKEEPING {
            self.last_trust_check = Instant::now();
            match self.trust.refresh() {
                Ok(true) => {
                    tracing::info!("trust store changed");
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
        let grants: BTreeMap<NodeId, BTreeSet<Capability>> = self.trust.with(|t| {
            t.peers().into_iter().map(|e| (e.node, e.granted.clone())).collect()
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
        let mut changed = arrange::merge(&mut self.placements, &arrange::default_layout(&nodes));
        if local_changed {
            changed |= self.rearrange_local();
        }
        // Drop placements for displays that no longer exist.
        let known: BTreeSet<_> = nodes
            .iter()
            .flat_map(|(node, displays)| displays.iter().map(move |d| (*node, d.id)))
            .collect();
        let before = self.placements.len();
        self.placements.retain(|p| known.contains(&(p.node, p.display)));
        changed |= self.placements.len() != before;
        if changed {
            self.feed(Input::Layout(self.placements.clone()));
            if local_changed {
                self.broadcast(&ControlMessage::Layout(self.placements.clone()));
            }
        }
    }

    /// Re-place this node's displays from the OS arrangement, keeping the node's top-left corner
    /// where it was.
    fn rearrange_local(&mut self) -> bool {
        let own: Vec<_> = self.placements.iter().filter(|p| p.node == self.node).collect();
        let (min_x, min_y) = own.iter().fold((f64::INFINITY, f64::INFINITY), |(x, y), p| {
            (x.min(p.origin.x), y.min(p.origin.y))
        });
        let (min_x, min_y) = if min_x.is_finite() { (min_x, min_y) } else { (0.0, 0.0) };
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
                    info.displays.iter().find(|d| d.id == id).map(|d| (d.clone(), origin))
                })
                .collect();
        let origins = arrange::place_beside(&ours, &theirs, side);
        let version = self.placements.iter().map(|p| p.version).max().unwrap_or(0) + 1;
        let fresh: Vec<Placement> = theirs
            .iter()
            .zip(origins)
            .map(|((display, _), origin)| Placement { node: peer, display: display.id, origin, version })
            .collect();
        arrange::merge(&mut self.placements, &fresh);
        self.feed(Input::Layout(self.placements.clone()));
        self.broadcast(&ControlMessage::Layout(self.placements.clone()));
        Ok(())
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

    fn on_ctl(&mut self, request: Request) -> Response {
        use crosspane_engine::Command;
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
            Request::Rearm => {
                self.feed(Input::Command(Command::Rearm));
                Response::ok(json!("re-armed"))
            }
            Request::Layout { peer, side } => match self.find_peer(&peer) {
                None => Response::err(format!("no connected peer matches {peer:?}")),
                Some(node) => match self.place_peer(node, side) {
                    Ok(()) => Response::ok(json!("layout updated")),
                    Err(e) => Response::err(e),
                },
            },
            Request::Dial { addr } => {
                self.net.dial(addr);
                Response::ok(json!(format!("dialing {addr}")))
            }
        }
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
    if let Err(e) = platform
        .session
        .subscribe(std::sync::Arc::new(move |ev| {
            let _ = session_tx.send(Event::Input(Input::Session(ev)));
        }))
    {
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
    if let Some(hotkeys) = &mut platform.hotkeys {
        let hotkey_tx = sink(tx);
        if let Err(e) = hotkeys.subscribe(std::sync::Arc::new(move |ev| {
            let _ = hotkey_tx.send(Event::Input(Input::Hotkey(ev)));
        })) {
            tracing::warn!(error = %e, "global hotkeys unavailable");
        }
    }
}

