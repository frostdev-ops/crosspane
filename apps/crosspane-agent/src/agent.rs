//! The engine loop (03 §1): one thread owns the [`Engine`] and the platform backends, feeds the
//! engine every event, and carries out its outputs. Everything else (network, control socket,
//! backend threads) talks to it through one channel.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use crosspane_engine::io::{AudioKey, HomeFailure, HomeOp, PortalsFailure, Warp};
use crosspane_engine::{
    Command, Engine, Failure, InjectCmd, Input, Notice, Output, ProjectionKey, ProxyEvent,
};
use crosspane_input::arrange::{self, Side};
use crosspane_platform::{
    CaptureEvent, CaptureTarget, EventSink, FrameEvent, LinkClass, Permission, PermissionState,
    PlatformError, StreamId, WindowEvent, WindowInfo, WindowRole, WindowState,
};
use crosspane_protocol::audio::AudioPacket;
use crosspane_protocol::link::{LinkEvent, PeerLink};
use crosspane_protocol::msg::{
    Capability, ControlMessage, Hello, Placement, Refusal, RevocationNotice,
};
use crosspane_protocol::projection::ProjectionMessage;
use crosspane_render::proxy::{HostCommand, HostEvent, HostHandle};
use crosspane_types::audio::AudioKind;
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{PixelSize, PointDevice};
use crosspane_types::id::NodeId;
use crosspane_types::id::{DisplayId, ProjectionId, WindowId};
use serde_json::{Value, json};

use crate::audio::{AudioWorker, WorkerEvent, WorkerStats};
use crate::ctl::{Request, Response};
use crate::media::{DestCmd, ProxyIds, Shape, SourceCmd};
use crate::net::Net;
use crate::platform::{self, Platform};
use crate::tray::{self, PairingView, PeerView, RemoteWindows, TrayAction, TrayView};
use crate::trust::SharedTrust;

/// Everything the engine loop reacts to.
pub enum Event {
    /// A platform event, already in engine terms.
    Input(Input),
    /// Parking completed off the loop; the actual result supplies the twin identity.
    Parking(crate::parking_worker::Completion),
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
    /// mDNS discovery saw a change (WP-1.6).
    Discovery(crosspane_transport::discovery::DiscoveryEvent),
    /// The network interfaces changed (WP-1.7/1.8).
    Links(Vec<crosspane_platform::Interface>),
    /// The audio worker reports (WP-3.6d): a device open finished, a stream failed, or the
    /// platform's audio devices changed.
    Audio(WorkerEvent),
    /// The compositor reloaded its config (which drops runtime keybinds) or its event connection
    /// was made again: check the home bind now (WP-2.43 §2.9).
    HomeBind,
}

/// What the loop knows about a peer.
#[derive(Debug, Default)]
struct PeerInfo {
    name: String,
    /// What its `Hello` advertised (e.g. `h264`).
    features: Vec<String>,
    displays: Vec<DisplayInfo>,
    connected: bool,
    rtt: Option<Duration>,
    /// What the engine was last told about audio with this peer (`Input::AudioPeer`).
    audio: bool,
}

impl PeerInfo {
    /// Remember what `hello` says about this peer: its name, the features and displays it
    /// advertises. Returns whether its displays differ from what was remembered.
    fn apply_hello(&mut self, name: String, hello: &Hello) -> bool {
        let displays_changed = self.displays != hello.displays;
        self.name = name;
        self.features.clone_from(&hello.features);
        self.displays.clone_from(&hello.displays);
        self.connected = true;
        displays_changed
    }
}

/// `CROSSPANE_DISCOVERY=0` (exactly) turns mDNS discovery off.
fn discovery_switched_off(value: Option<&str>) -> bool {
    value == Some("0")
}

/// Whether audio with a peer is available: both `Hello`s advertise `audio`. This node advertises
/// it exactly when its audio worker runs, and the transport refuses audio on a connection that
/// didn't negotiate it, so the engine is only told of a peer that can really carry it.
fn audio_negotiated(local_features: &[String], peer_features: &[String]) -> bool {
    let has = |features: &[String]| features.iter().any(|f| f == "audio");
    has(local_features) && has(peer_features)
}

/// The audio worker as the loop drives it. [`AudioWorker`] is the one implementation; the tests at
/// the bottom of this file record the calls instead, to pin their order.
pub(crate) trait AudioPlane: Send {
    /// Execute one audio output of the engine. Never blocks.
    fn submit(&self, output: Output);
    /// An audio datagram from `peer`. Never blocks.
    fn packet(&self, peer: NodeId, packet: AudioPacket);
    /// Stop every stream with `peer` at once.
    fn cancel_peer(&self, peer: NodeId);
    fn stats(&self) -> WorkerStats;
    /// Stop everything and wait (bounded) for the worker's threads.
    fn shutdown(self: Box<Self>);
}

impl AudioPlane for AudioWorker {
    fn submit(&self, output: Output) {
        AudioWorker::submit(self, output);
    }

    fn packet(&self, peer: NodeId, packet: AudioPacket) {
        AudioWorker::packet(self, peer, packet);
    }

    fn cancel_peer(&self, peer: NodeId) {
        AudioWorker::cancel_peer(self, peer);
    }

    fn stats(&self) -> WorkerStats {
        AudioWorker::stats(self)
    }

    fn shutdown(self: Box<Self>) {
        AudioWorker::shutdown(*self);
    }
}

/// Whether this build produces `ProxyEvent::Placed` from the compositor's window list (the
/// Hyprland placement source). Wayland tells a client neither its position nor its output; macOS
/// does (`HostEvent::Placed { monitor: Some(..) }`) and keeps the host's own report.
const HYPRLAND_PLACEMENT: bool = cfg!(target_os = "linux");

/// How often the home bind is verified while it is wanted (also at once on a config reload).
const BIND_CHECK: Duration = Duration::from_secs(1);
const HOME_WATCHDOG: Duration = Duration::from_millis(500);
/// How often a startup removal that failed is tried again (amendment A1).
const FENCE_RETRY: Duration = Duration::from_secs(2);
/// How far, in device pixels per axis, the pointer read back after a warp may be from the point it
/// was sent to and still count as there (amendment A3).
const WARP_TOLERANCE: f64 = 2.0;
/// How long an injection error stays worth naming in a home notice.
const INJECT_ERROR_AGE: Duration = Duration::from_secs(10);

/// What a proxy reports about where it is: the display (`None`: on none), the content's top-left
/// in that display's device pixels, and its size. All zero when on no display, so two reports of
/// "nowhere" are equal.
type Placed = (Option<DisplayId>, PointDevice, PixelSize);

/// Where each open proxy is, from the host's visibility joined with the compositor's geometry
/// (WP-2.43e, the Hyprland placement source). Wayland gives a client neither its position nor its
/// output, so the proxy host says only whether the proxy is visible
/// (`HostEvent::Placed { monitor: None }`); where it is comes from the compositor's own window
/// list, matched to the proxy by pid and title. One producer feeds every `ProxyEvent::Placed`, so
/// the reports can't contradict each other.
#[derive(Debug)]
struct PlacementSource {
    /// This process: the proxies are its windows.
    pid: u32,
    /// This node's displays, as the engine knows them.
    displays: Vec<DisplayInfo>,
    /// Every window the compositor listed, newest state.
    windows: BTreeMap<WindowId, WindowInfo>,
    /// The open proxies and the title each was given (the compositor lists the window under it).
    proxies: BTreeMap<ProjectionKey, String>,
    /// What the host last said about each proxy's visibility (minimised, fully occluded: `false`).
    visible: BTreeMap<ProjectionKey, bool>,
    /// The last report made for each proxy, so an equal one isn't repeated.
    last: BTreeMap<ProjectionKey, Placed>,
}

impl PlacementSource {
    fn new(pid: u32) -> PlacementSource {
        PlacementSource {
            pid,
            displays: Vec::new(),
            windows: BTreeMap::new(),
            proxies: BTreeMap::new(),
            visible: BTreeMap::new(),
            last: BTreeMap::new(),
        }
    }

    fn set_displays(&mut self, displays: &[DisplayInfo]) {
        self.displays = displays.to_vec();
    }

    fn window_event(&mut self, event: &WindowEvent) {
        match event {
            WindowEvent::Added(window) | WindowEvent::Changed(window) => {
                self.windows.insert(window.id, window.clone());
            }
            WindowEvent::Removed(window) => {
                self.windows.remove(window);
            }
            _ => {}
        }
    }

    /// The proxy `key` is open and its window is called `title`.
    fn opened(&mut self, key: ProjectionKey, title: &str) {
        self.proxies.insert(key, title.to_owned());
    }

    /// The title of an open proxy changed. Preserve the already uniquely identified own window
    /// across our title request: a window-title event is debounced and may arrive after a flush.
    /// Geometry still comes from the compositor, and its next event revalidates the identity.
    fn retitled(&mut self, key: ProjectionKey, title: &str) {
        if self.compute(&key).0.is_some()
            && let Some(old) = self.proxies.get(&key)
            && let Some(window) = self
                .windows
                .values_mut()
                .find(|w| w.pid == Some(self.pid) && &w.title == old)
        {
            title.clone_into(&mut window.title);
        }
        if let Some(own) = self.proxies.get_mut(&key) {
            title.clone_into(own);
        }
    }

    fn closed(&mut self, key: ProjectionKey) {
        self.proxies.remove(&key);
        self.visible.remove(&key);
        self.last.remove(&key);
    }

    fn set_visible(&mut self, key: ProjectionKey, visible: bool) {
        self.visible.insert(key, visible);
    }

    /// The window of this node's that sits on `display` (a projected window parked alone on its
    /// twin output), for naming it in notices.
    fn window_on(&self, display: DisplayId) -> Option<&WindowInfo> {
        self.windows
            .values()
            .find(|w| w.display == Some(display) && w.role == WindowRole::Toplevel)
    }

    /// Where `key`'s proxy is now.
    ///
    /// Only a proxy that is visible (the host's word), whose title names exactly one open proxy
    /// and exactly one window of this process, whose window is neither hidden nor minimised and
    /// sits on a display the engine knows, is placed; everything else is nowhere. The origin is the
    /// window's top-left in that display's device pixels, the size its extent scaled (a size is
    /// not a point, so the display's origin isn't subtracted from it).
    fn compute(&self, key: &ProjectionKey) -> Placed {
        let nowhere = (None, PointDevice::zero(), PixelSize::new(0, 0));
        let Some(title) = self.proxies.get(key) else {
            return nowhere;
        };
        if !self.visible.get(key).copied().unwrap_or(false)
            || self.proxies.values().filter(|t| *t == title).count() != 1
        {
            return nowhere;
        }
        let mut mine = self
            .windows
            .values()
            .filter(|w| w.pid == Some(self.pid) && &w.title == title);
        let (Some(window), None) = (mine.next(), mine.next()) else {
            return nowhere;
        };
        if matches!(window.state, WindowState::Hidden | WindowState::Minimized) {
            return nowhere;
        }
        let Some(info) = window
            .display
            .and_then(|d| self.displays.iter().find(|i| i.id == d))
        else {
            return nowhere;
        };
        let geometry = &info.geometry;
        let scaled = |extent: f64| (extent * geometry.scale).round().max(0.0) as u32;
        (
            Some(info.id),
            geometry.logical_to_device(window.frame.origin),
            PixelSize::new(
                scaled(window.frame.size.width),
                scaled(window.frame.size.height),
            ),
        )
    }

    /// Every proxy whose placement differs from what was last reported, with the new placement
    /// (remembered as reported) and whether the report is notable: the proxy's first, or the one
    /// that places it after it was nowhere. The first report of a proxy always goes out, placed or
    /// not.
    fn changes(&mut self) -> Vec<(ProjectionKey, Placed, bool)> {
        let keys: Vec<ProjectionKey> = self.proxies.keys().copied().collect();
        let mut changed = Vec::new();
        for key in keys {
            let now = self.compute(&key);
            let before = self.last.insert(key, now);
            if before != Some(now) {
                let notable = before.is_none_or(|b| b.0.is_none() && now.0.is_some());
                changed.push((key, now, notable));
            }
        }
        changed
    }
}

/// Where this node's input is home (WP-2.43): what the notices name.
#[derive(Clone, Debug)]
struct HomeNow {
    key: ProjectionKey,
    /// The peer whose E1 session stays open while home.
    peer: Option<NodeId>,
    title: String,
}

/// What the agent keeps for home on the twin (WP-2.43e): the release bind it owns on the
/// compositor, and what it last learned about it.
#[derive(Debug)]
struct HomeAgent {
    /// Set by `Notice::Home { entered: true }`, cleared when home is left or fails.
    now: Option<HomeNow>,
    /// The engine's install request in force: retained across failures until physical safety or
    /// a clean capture-protected removal is verified. While set the bind is verified
    /// on every config reload and every [`BIND_CHECK`].
    wanted: Option<HomeOp>,
    /// What the agent last verified: `Some(true)` present, `Some(false)` absent, `None` unknown
    /// (never checked, or an install or removal failed part-way). Never reset on a failure.
    present: Option<bool>,
    /// The cause of the latest install, reinstall or removal failure, for the notices.
    error: Option<String>,
    /// A failed removal was already put in a notice (the engine retries it with backoff; the
    /// user hears of the episode once). Cleared by the next success.
    removal_reported: bool,
    /// Startup (amendment A1): a leftover bind could not be confirmed absent. Until it can, this
    /// node injects no key press, button press, motion or scroll (E1 target and E2 source alike),
    /// refuses E1 control, and doesn't install a bind.
    fence: bool,
    last_fence_try: Instant,
    last_check: Instant,
    /// The compositor reloaded its config: verify at the next housekeeping pass.
    reload: bool,
    /// The latest failed injection and when, for the `Drain` notice.
    inject_error: Option<(Instant, String)>,
    /// The parking backend's candidate twin displays. A fresh physical snapshot must still
    /// exclude an ID before rescue: restore can remove the output before its journal save fails.
    twins: BTreeMap<WindowId, DisplayId>,
    /// Physical IDs verified through command IPC. Subscription snapshots can become empty
    /// when their event connection is lost, even while command IPC and rescue still work.
    physical: BTreeSet<DisplayId>,
    /// Entering or home, from the bind transaction through its removal request.
    active: bool,
    /// A twin warp without a resumed E1 capture needs physical fallback verification.
    pointer_unsafe: bool,
    /// Capture lifecycle events distinguish a clean E1 resumption from an abandoned entry.
    capture: Option<crosspane_platform::CaptureId>,
    /// A1 removal deferred until the cursor is verified safe; keep its original correlation.
    removal: Option<HomeOp>,
    fallback: Option<(DisplayId, PointDevice)>,
    /// One warning per continuous episode on a twin outside home.
    rescue_reported: bool,
    watchdog_next: Instant,
}

impl HomeAgent {
    fn new() -> HomeAgent {
        HomeAgent {
            now: None,
            wanted: None,
            present: None,
            error: None,
            removal_reported: false,
            fence: false,
            last_fence_try: Instant::now(),
            last_check: Instant::now(),
            reload: false,
            inject_error: None,
            twins: BTreeMap::new(),
            physical: BTreeSet::new(),
            active: false,
            pointer_unsafe: false,
            capture: None,
            removal: None,
            fallback: None,
            rescue_reported: false,
            watchdog_next: Instant::now(),
        }
    }

    /// Remember what the injectors last said, for a `Drain` notice. At most once a second: a
    /// refused injection can repeat at pointer-motion rate, and the text is only ever read when
    /// a drain fails, which is about the last few seconds.
    fn note_inject_error(&mut self, text: impl FnOnce() -> String) {
        if self
            .inject_error
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() >= Duration::from_secs(1))
        {
            self.inject_error = Some((Instant::now(), text()));
        }
    }
}

/// Whether `cmd` only ever releases: the one kind of injection a fenced node still carries out
/// (the gate allows releases while closed for the same reason, `IoGate`).
fn releases_only(cmd: &InjectCmd) -> bool {
    matches!(
        cmd,
        InjectCmd::Key { down: false, .. }
            | InjectCmd::Button { down: false, .. }
            | InjectCmd::ReleaseAll
            | InjectCmd::Recover { .. }
    )
}

pub struct Agent {
    node: NodeId,
    name: String,
    engine: Engine,
    platform: Platform,
    /// Only scheduling state stays on the loop after the backend is handed to its worker.
    parking: Option<crate::parking_worker::Worker>,
    /// Backend presence survives the ownership transfer, including a worker start failure.
    parking_available: bool,
    /// Strictly increasing IDs cover every submitted operation, including refused requests.
    parking_next: Option<u64>,
    /// Only this request may answer the engine's current Park/Resize for each window.
    parking_latest: BTreeMap<WindowId, u64>,
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
    discovery: Option<crosspane_transport::discovery::Discovery>,
    /// Other Crosspane nodes on the network, by instance id.
    candidates: BTreeMap<String, crosspane_transport::discovery::Candidate>,
    last_candidate_dial: Option<Instant>,
    /// The device name is in the advertisement (a pairing window is open).
    advertising_name: bool,
    // E2 data plane and window host.
    source_media: Sender<SourceCmd>,
    dest_media: Sender<DestCmd>,
    host: Option<HostHandle>,
    proxy_ids: ProxyIds,
    streams: HashMap<StreamId, ProjectionId>,
    projections: BTreeMap<ProjectionKey, String>,
    events: Sender<Event>,
    crossing: bool,
    /// `config.latency_overlay`, and per destination projection its title and the frame count
    /// at the last readout.
    latency_overlay: bool,
    titles: HashMap<ProjectionKey, (String, u64)>,
    /// This node's interfaces, and the link class of the path to each connected peer (03 §2).
    interfaces: Vec<crosspane_platform::Interface>,
    paths: HashMap<NodeId, LinkClass>,
    /// `config.video_mbps`: `None` picks the bitrate from the path's link class.
    video_mbps: Option<u32>,
    /// Clock samples per peer, (round trip, offset) in ns, from the ping exchange.
    clocks: HashMap<NodeId, VecDeque<(u64, i64)>>,
    last_ping: Instant,
    /// When each peer last answered a ping (only peers that answer pings are held to it).
    last_pong: HashMap<NodeId, Instant>,
    last_titles: Instant,
    /// The previous liveness check: after a stall of the loop itself, answers may still be queued.
    last_liveness: Instant,
    pairing: crate::pairing::Pairing,
    identity: Arc<crosspane_security::identity::DeviceIdentity>,
    port: u16,
    /// Revocation notices this node issued, sent to every peer that connects (04 §4).
    revocations: crate::revocations::Issued,
    /// The audio worker (WP-3.6d); `None` when audio sharing is off or has no backend.
    audio: Option<Box<dyn AudioPlane>>,
    /// The features this node advertised in its `Hello`.
    features: Vec<String>,
    /// Sessions playing on this machine's speakers now: the engine's `AudioIndicators`.
    speakers: Vec<AudioKey>,
    /// Home on the twin (WP-2.43e): the release bind and what the notices name.
    home: HomeAgent,
    /// Where this node's proxies are (WP-2.43e); `changes()` is flushed after every input that
    /// can move one.
    placement: PlacementSource,
    placement_dirty: bool,
    /// Diagnostic only: has this capture produced any local motion for entry corroboration?
    capture_motion_seen: bool,
    /// The display each of this node's projections is captured from (the twin output on
    /// Hyprland), to name the projected window in notices.
    capture_display: BTreeMap<ProjectionId, DisplayId>,
    /// What the agent learned about its own process at startup (WP-4.5).
    startup: installer::StartupFacts,
    /// The actual on-disk config and journals for this run; absent in isolated fixtures.
    lifecycle_paths: Option<crate::paths::Paths>,
    /// What the agent counts and remembers for `status.result.installer` (WP-4.5).
    tracker: installer::Tracker,
    /// Every input fed to the engine, in order (tests only).
    #[cfg(test)]
    fed: Vec<Input>,
    #[cfg(test)]
    test_now: Option<crosspane_types::time::MonoTime>,
    #[cfg(test)]
    emitted: Vec<Output>,
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
    pub latency_overlay: bool,
    pub video_mbps: Option<u32>,
    pub identity: Arc<crosspane_security::identity::DeviceIdentity>,
    pub port: u16,
    pub revocations: crate::revocations::Issued,
}

/// Returned after the consumed agent has shut down; the caller writes its final receipt.
pub struct Stopped {
    pub outcomes: crate::lifecycle::Shutdown,
    pub restart: bool,
}

const NOTICE_HISTORY: usize = 20;
/// How long after the "controlled from" indicator is hidden the overlay host may still deliver
/// transitions of it (WP-4.5 `e1_hud_shows` attribution).
const INDICATOR_SETTLE: Duration = Duration::from_secs(1);
const HOUSEKEEPING: Duration = Duration::from_secs(1);
/// Clock-offset pings to every peer (for frame latency).
const PING_INTERVAL: Duration = Duration::from_secs(5);
const CLOCK_SAMPLES: usize = 8;
/// Layout versions at or above this from a peer are refused (see the Layout handler).
const MAX_LAYOUT_VERSION: u64 = 1 << 48;
/// A peer that answers pings is considered gone after this long without an answer.
const UNRESPONSIVE: Duration = Duration::from_secs(15);
/// Above this smoothed RTT a "wired" path has a slower hop on the way (usually the peer's Wi-Fi).
const WIRED_RTT: Duration = Duration::from_millis(3);
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

/// A candidate's addresses, best first: IPv4, then global IPv6, then link-local IPv6 (at most 3).
fn family_rank(a: &SocketAddr) -> u8 {
    match a {
        SocketAddr::V4(_) => 0,
        SocketAddr::V6(v6) if v6.ip().is_unicast_link_local() => 2,
        SocketAddr::V6(_) => 1,
    }
}

/// `::ffff:a.b.c.d` as `a.b.c.d`.
fn canonical(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => v6.ip().to_ipv4_mapped().map_or(addr, |v4| {
            SocketAddr::new(std::net::IpAddr::V4(v4), v6.port())
        }),
        SocketAddr::V4(_) => addr,
    }
}

fn class_rank(class: LinkClass) -> u8 {
    match class {
        LinkClass::DirectUsb4Tb => 0,
        LinkClass::DirectEthernet => 1,
        LinkClass::Lan => 2,
        LinkClass::Wifi => 3,
        _ => 4,
    }
}

/// The class of the local interface the OS would send to `remote` from: a connected UDP socket
/// learns its source address without sending anything.
fn local_class(interfaces: &[crosspane_platform::Interface], remote: SocketAddr) -> LinkClass {
    // quinn reports IPv4 peers on its dual-stack socket as IPv4-mapped IPv6.
    let remote = canonical(remote);
    let unspecified: SocketAddr = match remote {
        SocketAddr::V4(_) => (std::net::Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (std::net::Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let source = std::net::UdpSocket::bind(unspecified)
        .and_then(|socket| socket.connect(remote).map(|()| socket))
        .and_then(|socket| socket.local_addr())
        .map(|local| canonical(local).ip());
    let Ok(source) = source else {
        return LinkClass::Unknown;
    };
    interfaces
        .iter()
        .find(|i| i.addrs.contains(&source))
        .map_or(LinkClass::Unknown, |i| i.class)
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
        features: Vec<String>,
        audio: Option<Box<dyn AudioPlane>>,
    ) -> Agent {
        let mut agent = Agent {
            node,
            name,
            engine,
            platform,
            parking: None,
            parking_available: false,
            parking_next: Some(1),
            parking_latest: BTreeMap::new(),
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
            discovery: None,
            candidates: BTreeMap::new(),
            last_candidate_dial: None,
            advertising_name: false,
            source_media: e2.source_media,
            dest_media: e2.dest_media,
            host: e2.host,
            proxy_ids: e2.proxy_ids,
            streams: HashMap::new(),
            projections: BTreeMap::new(),
            events: e2.events,
            crossing: e2.crossing,
            latency_overlay: e2.latency_overlay,
            interfaces: Vec::new(),
            paths: HashMap::new(),
            video_mbps: e2.video_mbps,
            titles: HashMap::new(),
            clocks: HashMap::new(),
            last_ping: Instant::now(),
            last_pong: HashMap::new(),
            last_titles: Instant::now(),
            last_liveness: Instant::now(),
            pairing: crate::pairing::Pairing::default(),
            identity: e2.identity,
            port: e2.port,
            revocations: e2.revocations,
            audio,
            features,
            speakers: Vec::new(),
            home: HomeAgent::new(),
            placement: PlacementSource::new(std::process::id()),
            placement_dirty: false,
            capture_motion_seen: false,
            capture_display: BTreeMap::new(),
            startup: installer::StartupFacts::unknown(node),
            lifecycle_paths: None,
            tracker: installer::Tracker::new(),
            #[cfg(test)]
            fed: Vec::new(),
            #[cfg(test)]
            test_now: None,
            #[cfg(test)]
            emitted: Vec::new(),
        };
        agent.parking_start();
        agent
    }

    /// What this process learned about itself before the loop started: `status` reports it.
    pub fn set_startup(&mut self, facts: StartupFacts) {
        self.startup = facts;
    }

    pub fn set_lifecycle_paths(&mut self, paths: crate::paths::Paths) {
        self.lifecycle_paths = Some(paths);
    }

    /// Carry out `outputs` (crash recovery from `Engine::new` first), then run until the channel
    /// closes.
    pub fn run(mut self, startup: Vec<Output>, events: &Receiver<Event>) -> Stopped {
        self.granted = self.granted_permissions();
        // A release bind an earlier run left behind goes first, before any input is admitted
        // (04 §6, amendment A1). Crash recovery below only ever releases, so it isn't held back.
        self.home_startup();
        self.execute(startup);
        self.feed(Input::LocalDisplays(self.local_displays.clone()));
        // Speaker v0 hosts no microphone: the worker refuses to open one, and the engine refuses
        // microphone sessions without admitting them or showing an indicator (AUDIO-v0 §1).
        self.feed(Input::AudioMicrophoneSupport { available: false });
        self.send_grants();
        self.update_layout(false);
        // The first look at the backends: later differences advance `epochs.backends`.
        self.backends_now();
        loop {
            self.settle();
            let now = platform::now();
            let timeout = self.receive_timeout(now, Instant::now());
            match events.recv_timeout(timeout) {
                Ok(Event::Shutdown) => {
                    return Stopped {
                        outcomes: self.shutdown(),
                        restart: false,
                    };
                }
                Ok(event) => self.on_event(event),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Stopped {
                        outcomes: self.shutdown(),
                        restart: false,
                    };
                }
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
                return Stopped {
                    outcomes: self.shutdown(),
                    restart: false,
                };
            }
            if self.restart_requested || self.permissions_changed() {
                return Stopped {
                    outcomes: self.shutdown(),
                    restart: true,
                };
            }
        }
    }

    /// The receive loop must wake for the watchdog even without engine deadlines or input.
    /// Suspend its deadline while exempt; an expired deadline must never spin that loop.
    fn receive_timeout(
        &self,
        now: crosspane_types::time::MonoTime,
        clock_now: Instant,
    ) -> Duration {
        let timeout = self
            .engine
            .next_deadline()
            .map_or(HOUSEKEEPING, |deadline| {
                Duration::from_nanos(deadline.as_nanos().saturating_sub(now.as_nanos()))
            })
            .min(HOUSEKEEPING);
        if self.home_watchdog_needed() {
            timeout.min(self.home.watchdog_next.saturating_duration_since(clock_now))
        } else {
            timeout
        }
    }

    fn feed(&mut self, input: Input) {
        if tracing::enabled!(tracing::Level::DEBUG) {
            log_input(&input);
        }
        #[cfg(test)]
        self.fed.push(input.clone());
        self.observe(&input);
        let now = platform::now();
        #[cfg(test)]
        let now = self.test_now.unwrap_or(now);
        let from_controller = installer::e1_input_peer(&input);
        // The sessions that are established, not a handshake still waiting for its answer.
        let before = (
            self.engine.control_established(),
            self.engine.controlled_by(),
        );
        let outputs = self.engine.handle(input, now);
        let after = (
            self.engine.control_established(),
            self.engine.controlled_by(),
        );
        self.tracker
            .handled(before, after, from_controller, &outputs);
        #[cfg(test)]
        self.emitted.extend(outputs.iter().cloned());
        self.execute(outputs);
        self.tracker.executed();
        self.flush_placements();
    }

    /// Carry out the acknowledgements that keep their ordinary delivery order.
    fn settle(&mut self) {
        while let Some(input) = self.pending.pop_front() {
            self.feed(input);
        }
    }

    /// What the agent itself needs to learn from an input before the engine sees it.
    fn observe(&mut self, input: &Input) {
        match input {
            Input::Capture(CaptureEvent::Started { id }) => self.home.capture = Some(*id),
            Input::Capture(CaptureEvent::Ended { id, .. })
            | Input::CaptureBegun { id, result: Err(_) }
                if self.home.capture == Some(*id) =>
            {
                self.home.capture = None;
            }
            Input::Capture(CaptureEvent::Motion { .. }) => self.capture_motion_seen = true,
            Input::Link(LinkEvent::Input {
                peer,
                msg:
                    crosspane_protocol::msg::InputMessage::Proj(
                        crosspane_protocol::projection::ProjInput::Motion { projection, .. },
                    ),
            }) if self.engine.controlling() == Some(*peer) && !self.capture_motion_seen => {
                // A truthful observation for the nested harness: the frozen engine interface
                // does not expose whether prevalidation or corroboration rejected this report.
                tracing::debug!(
                    projection = projection.0,
                    "proxy motion observed without local capture motion"
                );
            }
            Input::Windows(event) => {
                self.placement.window_event(event);
                self.placement_dirty = true;
            }
            Input::LocalDisplays(displays) => {
                self.placement.set_displays(displays);
                self.placement_dirty = true;
            }
            // The engine learns the proxy is open from this very input; the report that follows
            // is queued behind it.
            Input::ProxyOpened { key, result: Ok(_) } => {
                let title = self.titles.get(key).map(|(title, _)| title.clone());
                self.placement
                    .opened(*key, title.as_deref().unwrap_or_default());
                self.placement_dirty = true;
                if key.source != self.node {
                    self.tracker.proxy_opened(*key);
                }
            }
            // The overlay host's outcome for a "controlled from" show.
            Input::Overlay(crosspane_platform::OverlayEvent::Visible(id))
                if *id == crosspane_engine::io::TARGET_INDICATOR =>
            {
                self.tracker.indicator_answer(true);
            }
            Input::Overlay(crosspane_platform::OverlayEvent::Unavailable(id))
                if *id == crosspane_engine::io::TARGET_INDICATOR =>
            {
                self.tracker.indicator_answer(false);
            }
            // The projection's capture is running: it is live (and counts) from here.
            Input::CaptureStarted {
                projection,
                result: Ok(_),
            } => self.tracker.capture_started(self.node, *projection),
            _ => {}
        }
    }

    /// Report every proxy whose placement changed (the Hyprland placement source). Each report is
    /// an input of its own, queued behind whatever is being handled.
    fn flush_placements(&mut self) {
        if !std::mem::take(&mut self.placement_dirty) || !HYPRLAND_PLACEMENT {
            return;
        }
        for (key, (on, origin, size), notable) in self.placement.changes() {
            // A proxy's first report, and the one that places it, are worth a line at any log
            // level: they show where the placement source puts the window (and that it does).
            if notable {
                tracing::info!(
                    source = %key.source.short(),
                    projection = key.projection.0,
                    display = ?on,
                    ?origin,
                    ?size,
                    "placement source: proxy reported"
                );
            }
            self.pending.push_back(proxy(
                key,
                ProxyEvent::Placed {
                    display: on,
                    origin,
                    size,
                },
            ));
        }
    }

    /// `platform::create` completed startup recovery before handing us this backend.
    fn parking_start(&mut self) {
        if self.parking_available {
            return;
        }
        if let Some(backend) = self.platform.parking.take() {
            self.parking_available = true;
            match crate::parking_worker::Worker::start(backend, self.events.clone()) {
                Ok(worker) => self.parking = Some(worker),
                Err(error) => {
                    tracing::error!(%error, "parking worker could not start; recovery remains pending")
                }
            }
        }
    }

    fn parking_submit(&mut self, command: crate::parking_worker::Command) {
        self.parking_start();
        let id = self.parking_next.unwrap_or(0);
        self.parking_next = self.parking_next.and_then(|id| id.checked_add(1));
        match command {
            crate::parking_worker::Command::Park { window, .. }
            | crate::parking_worker::Command::Resize { window, .. } => {
                self.parking_latest.insert(window, id);
            }
            crate::parking_worker::Command::Restore { .. } => self.tracker.restore_started(id),
        }
        if let Some(worker) = &mut self.parking
            && id != 0
        {
            worker.enqueue(id, command);
        } else {
            if id == 0 {
                tracing::error!("parking operation IDs exhausted; command refused");
            }
            let _ = self.events.send(Event::Parking(
                crate::parking_worker::Completion::unavailable(id, command),
            ));
        }
    }

    fn parking_completed(&mut self, completion: crate::parking_worker::Completion, feed: bool) {
        if let Some(worker) = &mut self.parking
            && !worker.acknowledge(&completion)
        {
            return;
        }
        match &completion.outcome {
            crate::parking_worker::Outcome::Started {
                window,
                kind: crate::parking_worker::Kind::Park,
            } => {
                self.tracker.parking_started(*window, completion.id);
            }
            crate::parking_worker::Outcome::Started { .. } => {}
            crate::parking_worker::Outcome::Parked { window, result } => {
                self.home_parked(*window, result);
                if feed && self.parking_latest.get(window) == Some(&completion.id) {
                    self.parking_latest.remove(window);
                    self.feed(Input::Parked {
                        window: *window,
                        result: *result,
                    });
                } else {
                    tracing::debug!(
                        operation = completion.id,
                        window = window.0,
                        ?result,
                        "parking completion consumed internally"
                    );
                }
            }
            crate::parking_worker::Outcome::Restored { window, ok } => {
                self.tracker.restored(*window, completion.id, *ok);
                if *ok {
                    self.home.twins.remove(window);
                }
            }
            crate::parking_worker::Outcome::Panicked => {}
        }
    }

    fn parking_shutdown(&mut self, wait: Duration) -> crate::lifecycle::Parking {
        self.parking_start();
        let Some(worker) = &mut self.parking else {
            return if self.parking_available {
                crate::lifecycle::Parking::Failed
            } else {
                crate::lifecycle::Parking::None
            };
        };
        let (outcome, completions) =
            worker.shutdown(wait.min(crate::parking_worker::SHUTDOWN_WAIT));
        for completion in completions {
            self.parking_completed(completion, false);
        }
        outcome
    }

    fn on_event(&mut self, event: Event) {
        match event {
            // `run` stops the loop for this one.
            Event::Shutdown => {}
            Event::Discovery(event) => self.on_discovery(event),
            Event::Links(interfaces) => {
                self.interfaces = interfaces;
                self.update_paths();
            }
            Event::Tray(crosspane_platform::TrayEvent::Chosen(id)) => {
                if let Some(action) = self.tray.actions.get(&id).cloned() {
                    self.tray_action(action);
                }
            }
            Event::Input(input) => self.feed(input),
            Event::Parking(completion) => self.parking_completed(completion, true),
            Event::LocalDisplays(displays) => {
                if displays == self.local_displays {
                    return;
                }
                self.local_displays = displays.clone();
                self.feed(Input::LocalDisplays(displays.clone()));
                self.broadcast(&ControlMessage::Displays(displays));
                // One advance for a local display change, whether or not it moved a placement.
                if !self.update_layout(true) {
                    self.tracker.layout_changed();
                }
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
            Event::Audio(event) => self.on_audio(event),
            // Checked by the housekeeping pass that follows this event.
            Event::HomeBind => self.home.reload = true,
        }
    }

    /// What the audio worker reports goes to the engine, which owns the policy: a stream that
    /// failed ends, a device that didn't open refuses the session.
    fn on_audio(&mut self, event: WorkerEvent) {
        let input = match event {
            WorkerEvent::DeviceOpened { key, kind, result } => {
                if result.is_err() {
                    tracing::info!(peer = %key.peer.short(), ?kind, "an audio device did not open");
                }
                Input::AudioDeviceOpened { key, kind, result }
            }
            WorkerEvent::StreamFailed { key } => {
                tracing::warn!(peer = %key.peer.short(), "an audio stream failed");
                Input::AudioStreamFailed { key }
            }
            WorkerEvent::Platform(event) => Input::Audio(event),
        };
        self.feed(input);
    }

    fn on_link(&mut self, event: LinkEvent) {
        if let LinkEvent::Media { peer, data } = event {
            let _ = self.dest_media.send(DestCmd::Media { peer, data });
            return;
        }
        // Audio datagrams go to the worker, which drops everything that isn't a started playback
        // stream; the engine has no use for them.
        if let LinkEvent::Audio { peer, packet } = event {
            if let Some(audio) = &self.audio {
                audio.packet(peer, packet);
            }
            return;
        }
        // A replacement connection's first Hello (the link itself never went down).
        if let LinkEvent::HelloRefresh { peer, hello } = &event {
            self.on_hello_refresh(*peer, hello);
            return;
        }
        match &event {
            LinkEvent::Control { peer, msg } => match msg {
                ControlMessage::Hello(hello) => {
                    self.on_hello(*peer, hello);
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
                    // A version near the top would make every later local edit lose (its
                    // `max + 1` can't exceed it): such placements are refused.
                    let explicit: Vec<Placement> = placements
                        .iter()
                        .copied()
                        .filter(|p| p.version > 0 && p.version < MAX_LAYOUT_VERSION)
                        .collect();
                    if arrange::merge(&mut self.placements, &explicit) {
                        // `update_layout` only feeds the engine when its own defaults change, so
                        // the merged layout goes to the engine here. One advance for the
                        // received layout, however many placements it moved.
                        if !self.update_layout(false) {
                            self.tracker.layout_changed();
                        }
                        self.feed(Input::Layout(self.placements.clone()));
                    }
                    return;
                }
                ControlMessage::Revocation(notice) => {
                    let (peer, notice) = (*peer, notice.clone());
                    self.on_revocation(peer, &notice);
                    return;
                }
                ControlMessage::Ping { t0 } => {
                    let now = platform::now().as_nanos();
                    if let Some(link) = self.links.get_mut(peer) {
                        let _ = link.send_control(&ControlMessage::Pong {
                            t0: *t0,
                            t1: now,
                            t2: now,
                        });
                    }
                    return;
                }
                ControlMessage::Pong { t0, t1, t2 } => {
                    self.on_pong(*peer, *t0, *t1, *t2);
                    return;
                }
                _ => {}
            },
            LinkEvent::Closed { peer, error } => {
                tracing::info!(peer = %peer.short(), ?error, "peer disconnected");
                // Audio with this peer stops before the engine hears of the close, so nothing is
                // still sent to or played for a connection that is gone (its devices go when the
                // engine removes the peer).
                if let Some(audio) = &self.audio {
                    audio.cancel_peer(*peer);
                }
                self.links.remove(peer);
                self.last_pong.remove(peer);
                if let Some(info) = self.peers.get_mut(peer) {
                    info.connected = false;
                    info.rtt = None;
                    // The engine drops its audio availability with the link.
                    info.audio = false;
                }
                // Projections from this peer stay open through the grace period (WP-2.15); a
                // resumed one starts a new stream whose sequence numbers start again, so the
                // decoder state goes now. The proxy keeps showing its last frame.
                for key in self.projections.keys().filter(|k| k.source == *peer) {
                    let _ = self.dest_media.send(DestCmd::Forget(*key));
                }
            }
            _ => {}
        }
        self.feed(Input::Link(event));
    }

    /// Remember what `hello` says about `peer` (its name, features and displays, and the handle to
    /// its link). `None`, after closing the link, if the peer is no longer trusted: a Hello queued
    /// before a forget or a revoke. Otherwise whether the peer's displays changed.
    fn cache_hello(&mut self, peer: NodeId, hello: &Hello) -> Option<bool> {
        if !self.trust.with(|t| t.get(peer).is_some()) {
            if let Some(mut link) = self.net.link(peer) {
                link.close("forgotten");
            }
            return None;
        }
        let name = self
            .trust
            .with(|t| t.get(peer).map(|e| e.name.clone()))
            .unwrap_or_else(|| hello.name.clone());
        let displays_changed = self.peers.entry(peer).or_default().apply_hello(name, hello);
        if let Some(link) = self.net.link(peer) {
            self.links.insert(peer, link);
        }
        self.update_paths();
        Some(displays_changed)
    }

    /// A peer's first `Hello` on a new logical link.
    fn on_hello(&mut self, peer: NodeId, hello: &Hello) {
        if self.cache_hello(peer, hello).is_none() {
            return;
        }
        self.tracker.link_established(peer);
        tracing::info!(peer = %peer.short(), name = %self.peer_label(peer), "peer connected");
        self.feed(Input::PeerUp { peer });
        // After `PeerUp`: the engine ignores audio availability for a peer that isn't up.
        self.sync_audio_peer(peer);
        self.feed(Input::PeerDisplays {
            peer,
            displays: hello.displays.clone(),
        });
        // Streams that outlived the previous link (E2's grace period) pick up the new features.
        self.peer_features_changed(peer);
        // Grants go out with the audio capabilities only once the connection has negotiated them,
        // which is when its Hello has been seen (WP-3.6b).
        self.send_grants();
        // Revocations this node issued: the peer may have been offline then.
        // Skip devices paired here again since (e.g. with `trust add`).
        let due: Vec<RevocationNotice> = self
            .revocations
            .notices()
            .iter()
            .filter(|n| n.revoked != peer && !self.trust.with(|t| t.get(n.revoked).is_some()))
            .cloned()
            .collect();
        if let Some(link) = self.links.get_mut(&peer) {
            for notice in due {
                let _ = link.send_control(&ControlMessage::Revocation(notice));
            }
        }
        // Tell the peer our view of the layout; it merges by version.
        self.update_layout(false);
        let explicit = self.explicit();
        if !explicit.is_empty()
            && let Some(link) = self.links.get_mut(&peer)
        {
            let _ = link.send_control(&ControlMessage::Layout(explicit));
        }
    }

    /// An authenticated connection silently replaced the one an announced link was using, and this
    /// is its `Hello`, which may carry different features. The link never went down, so the
    /// engine hears neither `Closed` nor `PeerUp`. The order matters:
    ///
    /// 1. the cache takes the new features and displays;
    /// 2. the worker stops every stream with the peer (they were bound to the old connection),
    ///    and then the engine is told, so it ends the sessions and keeps its identity counters;
    /// 3. audio availability is re-evaluated from the new features;
    /// 4. running video streams pick up the new features (after the cache update).
    fn on_hello_refresh(&mut self, peer: NodeId, hello: &Hello) {
        if !self.peers.get(&peer).is_some_and(|info| info.connected) {
            // Not a link this node has announced to the engine: take it as the first Hello.
            tracing::debug!(peer = %peer.short(), "refreshed Hello for an unannounced link");
            self.on_hello(peer, hello);
            return;
        }
        let Some(displays_changed) = self.cache_hello(peer, hello) else {
            return;
        };
        // A new connection was established, though the logical link never went down.
        self.tracker.link_established(peer);
        tracing::info!(peer = %peer.short(), name = %self.peer_label(peer), "peer connection replaced");
        if let Some(audio) = &self.audio {
            audio.cancel_peer(peer);
        }
        self.feed(Input::AudioConnectionReplaced { peer });
        self.sync_audio_peer(peer);
        self.peer_features_changed(peer);
        if displays_changed {
            self.feed(Input::PeerDisplays {
                peer,
                displays: hello.displays.clone(),
            });
            self.update_layout(false);
        }
        // The new connection's audio capabilities for the grants only exist from its Hello on.
        self.send_grants();
    }

    /// Tell the engine whether audio with `peer` is available (both `Hello`s advertise it), when
    /// that differs from what it was last told. Only for a peer that is up.
    fn sync_audio_peer(&mut self, peer: NodeId) {
        let Some(info) = self.peers.get_mut(&peer) else {
            return;
        };
        let available = info.connected && audio_negotiated(&self.features, &info.features);
        if info.audio == available {
            return;
        }
        info.audio = available;
        let name = info.name.clone();
        self.feed(Input::AudioPeer {
            peer,
            name,
            available,
        });
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
                let ids = portals.iter().map(|p| p.id).collect();
                let result = match &mut self.platform.capture {
                    Some(capture) => capture.set_portals(&portals).map_err(|e| {
                        tracing::warn!(error = %e, "set_portals failed");
                        portals_failure(&e)
                    }),
                    // No backend: nothing was installed and no capture can be live.
                    None => Err(PortalsFailure::Rejected),
                };
                self.pending.push_back(Input::PortalsSet { ids, result });
            }
            Output::MonitorLocalActivity(on) => {
                if let Some(capture) = &mut self.platform.capture {
                    match capture.set_monitor_local_activity(on) {
                        Ok(()) | Err(PlatformError::Unsupported(_)) => {}
                        Err(e) => tracing::warn!(error = %e, "local-activity monitoring failed"),
                    }
                }
            }
            Output::BeginCapture {
                id,
                portal,
                drain_first,
            } => {
                self.capture_motion_seen = false;
                let result = match &mut self.platform.capture {
                    Some(capture) => capture.begin(id, portal).map_err(failure),
                    None => Err(Failure::Other),
                };
                if let Err(f) = &result {
                    tracing::info!(failure = ?f, "capture refused");
                }
                let begun = Input::CaptureBegun { id, result };
                if drain_first {
                    // Linux delivers activation callbacks before begin returns. Put the answer
                    // on that same channel: every queued event precedes it, with no drain cap
                    // and without moving a lock, shutdown or other event out of order (A4, B4).
                    let _ = self.events.send(Event::Input(begun));
                } else {
                    // Every other capture keeps today's order: the answer first.
                    self.pending.push_back(begun);
                }
            }
            Output::EndCapture { warp_to } => {
                self.home.capture = None;
                if let Some(capture) = &mut self.platform.capture
                    && let Err(e) = capture.end(warp_to)
                {
                    tracing::warn!(error = %e, "end capture failed");
                }
            }
            // A home-related warp (WP-2.43 §3.2): the capture, if any, is released and the pointer
            // sent to `warp_to`. The backend's `Ok` doesn't say the pointer moved (it skips the
            // warp while the gate is closed and still returns `Ok`), so the answer is built from a
            // read-back of the pointer and then the gate (amendments A3 and B2).
            Output::ReleaseAndWarp { op, warp_to } => {
                self.home.capture = None;
                let twin = self
                    .home
                    .twins
                    .values()
                    .any(|display| *display == warp_to.0);
                if twin {
                    self.home.pointer_unsafe = true;
                } else if self.physical_display(warp_to.0) {
                    self.home.fallback = Some(warp_to);
                }
                let ended = match &mut self.platform.capture {
                    Some(capture) => capture.end(Some(warp_to)).map_err(failure),
                    None => Err(Failure::Other),
                };
                let result = match ended {
                    Ok(()) => self.warp_result(warp_to),
                    Err(f) => {
                        tracing::warn!(failure = ?f, "release and warp failed");
                        Err(f)
                    }
                };
                if !twin && matches!(result, Ok(Warp::Done)) && self.physical_display(warp_to.0) {
                    self.home.pointer_unsafe = false;
                    self.home.rescue_reported = false;
                }
                if !twin {
                    self.home_confirm_physical();
                }
                self.pending
                    .push_back(Input::CaptureReleased { op, result });
            }
            Output::HomeBind { op, install } => self.home_bind(op, install),
            // The overlay host owes exactly one outcome per configuration it accepted (`Visible`
            // or `Unavailable`, WP-2.42), so a show it refused outright is only logged: the agent
            // never makes up a second outcome. The engine's own HUD deadline covers the wait.
            Output::ShowOverlay { id, overlay } => {
                let shown = match &mut self.platform.overlay {
                    Some(host) => host.show(id, &overlay),
                    None => Err(PlatformError::Unsupported("no overlay backend")),
                };
                match shown {
                    // Accepted: an outcome is owed, for the peer controlling this node now.
                    Ok(()) if id == crosspane_engine::io::TARGET_INDICATOR => {
                        self.tracker
                            .indicator_shown(self.engine.controlled_by(), Instant::now());
                    }
                    Ok(()) => {}
                    Err(e) => tracing::warn!(error = %e, "overlay unavailable"),
                }
            }
            Output::HideOverlay(id) => {
                if id == crosspane_engine::io::TARGET_INDICATOR {
                    self.tracker.indicator_hidden(Instant::now());
                }
                if let Some(host) = &mut self.platform.overlay
                    && let Err(e) = host.hide(id)
                {
                    tracing::warn!(error = %e, "hide overlay failed");
                }
            }
            Output::Inject { id, cmd } => {
                let key_or_button = matches!(cmd, InjectCmd::Key { .. } | InjectCmd::Button { .. });
                let ok = self.inject(cmd);
                if key_or_button {
                    self.tracker.injected(id, ok);
                }
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
            Output::EngineGate(open) => {
                self.tracker.engine_permits = open;
                self.platform.gate.set_engine_permits(open);
            }
            Output::Notice(notice) => self.notice(&notice),
            Output::Park {
                window,
                size,
                scale,
            } => {
                self.parking_submit(crate::parking_worker::Command::Park {
                    window,
                    size,
                    scale,
                });
            }
            Output::ResizeParked {
                window,
                size,
                scale,
            } => {
                self.parking_submit(crate::parking_worker::Command::Resize {
                    window,
                    size,
                    scale,
                });
            }
            Output::Restore { window } => {
                self.parking_submit(crate::parking_worker::Command::Restore { window });
            }
            Output::ActivateWindow { window } => {
                if let Some(w) = &mut self.platform.windows
                    && let Err(e) = w.activate(window)
                {
                    tracing::warn!(error = %e, "could not focus a projected window: its keys are held back");
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
                                FrameEvent::Cursor { stream, cursor } => {
                                    let cursor = cursor.map_or(Shape::Hidden, Shape::Image);
                                    let _ = media.send(SourceCmd::Cursor { stream, cursor });
                                }
                                FrameEvent::CursorDefault { stream } => {
                                    let cursor = Shape::Default;
                                    let _ = media.send(SourceCmd::Cursor { stream, cursor });
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
                    if let CaptureTarget::Display(display) = target {
                        self.capture_display.insert(projection, display);
                    }
                    let has = |feature: &str| {
                        self.peers
                            .get(&peer)
                            .is_some_and(|p| p.features.iter().any(|f| f == feature))
                    };
                    let _ = self.source_media.send(SourceCmd::Start {
                        stream,
                        projection,
                        peer,
                        video: has("h264"),
                        region: has("h264roi"),
                        cursor: has("cursor"),
                        bits_per_second: self.video_bits(peer),
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
                if let Some(projection) = self.streams.remove(&stream) {
                    self.capture_display.remove(&projection);
                }
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
                let title = self.badged(key.source, &title);
                self.titles.insert(key, (title.clone(), 0));
                let sent = self.host.as_ref().is_some_and(|h| {
                    h.send(HostCommand::Open {
                        id,
                        title,
                        size,
                        accent: node_accent(key.source),
                    })
                    .is_ok()
                });
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
                let title = self.badged(key.source, &title);
                if let Some(entry) = self.titles.get_mut(&key) {
                    entry.0.clone_from(&title);
                }
                self.set_proxy_title(key, title);
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
                self.titles.remove(&key);
                self.placement.closed(key);
                self.placement_dirty = true;
                let id = self.proxy_ids.close(key);
                // Whether the host was handed its `Close` (it doesn't say when the proxy is gone).
                let queued = match (id, &self.host) {
                    (Some(id), Some(h)) => h.send(HostCommand::Close { id }).is_ok(),
                    _ => false,
                };
                #[cfg(test)]
                let queued = self
                    .tracker
                    .close_seam
                    .map_or(queued, |seam| id.is_some_and(seam));
                self.tracker.proxy_closed(queued);
                let _ = self.dest_media.send(DestCmd::Forget(key));
            }
            // Audio (D8): the worker carries these out off this loop and reports back through
            // `Event::Audio`.
            Output::AddAudioPeer { .. }
            | Output::RemoveAudioPeer { .. }
            | Output::OpenAudioCapture { .. }
            | Output::CloseAudioCapture { .. }
            | Output::OpenAudioPlayback { .. }
            | Output::CloseAudioPlayback { .. }
            | Output::StartAudioStream { .. }
            | Output::StopAudioStream { .. } => self.audio_output(output),
            Output::AudioIndicators {
                microphones,
                speakers,
            } => {
                // Speaker v0 hosts no microphone, so this list is empty. If it ever were not, the
                // indicator is not acknowledged (`AudioIndicatorShown`): the engine's admission
                // then fails closed and the microphone never opens.
                if !microphones.is_empty() {
                    tracing::warn!(
                        count = microphones.len(),
                        "microphone sessions listed but microphones are not supported: no indicator is shown"
                    );
                }
                self.speakers = speakers;
                // Show who is playing now, not at the next refresh.
                self.tray.last_update = Instant::now()
                    .checked_sub(TRAY_UPDATE)
                    .unwrap_or_else(Instant::now);
            }
            other => tracing::debug!(output = %variant(&other), "unhandled engine output"),
        }
    }

    /// Hand one audio output of the engine to the worker.
    fn audio_output(&mut self, output: Output) {
        self.tracker.audio_output(&output);
        let Some(audio) = &self.audio else {
            // The engine only asks when audio is negotiated, which needs this node's worker. If
            // it asks anyway, fail what it is waiting for rather than leave it hanging.
            match output {
                Output::OpenAudioPlayback { key } => {
                    self.pending.push_back(Input::AudioDeviceOpened {
                        key,
                        kind: AudioKind::Speaker,
                        result: Err(Failure::Other),
                    })
                }
                Output::OpenAudioCapture { key } => {
                    self.pending.push_back(Input::AudioDeviceOpened {
                        key,
                        kind: AudioKind::Microphone,
                        result: Err(Failure::Other),
                    })
                }
                Output::StartAudioStream { key, .. } => {
                    self.pending.push_back(Input::AudioStreamFailed { key });
                }
                _ => {}
            }
            return;
        };
        audio.submit(output);
    }

    fn inject(&mut self, cmd: InjectCmd) -> bool {
        // Amendment A1: while the release bind of an earlier run can't be confirmed gone, a key
        // injected here could press it (the compositor can't tell a virtual keyboard from a
        // physical one), so only releases are carried out: E1 target and E2 source input alike.
        if self.home.fence && !releases_only(&cmd) {
            tracing::debug!(cmd = %variant(&cmd), "injection refused: a release shortcut may still be bound");
            return false;
        }
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
                tracing::debug!(cmd = %variant(&cmd), "no injector for command");
                self.home
                    .note_inject_error(|| "no injector for the command".into());
                return false;
            }
        };
        match result {
            Ok(()) => true,
            Err(e) => {
                tracing::debug!(error = %e, "injection refused");
                // For the `Drain` notice: what the injectors last said (never a key or its
                // contents: a platform error names the failure).
                self.home.note_inject_error(|| e.to_string());
                false
            }
        }
    }

    fn notice(&mut self, notice: &Notice) {
        // Counted, not shown: how a controller session was released says nothing the user needs
        // (the end notices above are unchanged).
        if let Notice::ControlReleased { peer, cause } = notice {
            self.tracker.released(*peer, *cause);
            return;
        }
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
            Notice::LocalOverride(p) => {
                format!("{} was used locally: control returned", peer_name(self, p))
            }
            Notice::Panic => "panic: everything stopped; re-arm to continue".to_owned(),
            Notice::ProjectionStarted { key, peer, parking } => {
                // Parked: counted once the capture starts.
                self.tracker.projection_parked(*key, *peer, *parking);
                let text = format!(
                    "projecting window {} to {} ({parking:?})",
                    key.projection.0,
                    peer_name(self, peer)
                );
                self.projections.insert(*key, text.clone());
                text
            }
            Notice::ProjectionEnded { key, reason } => {
                self.tracker.projection_ended(self.node, *key, *reason);
                self.projections.remove(key);
                format!("projection {} ended: {reason:?}", key.projection.0)
            }
            Notice::ProjectionRefused { peer, reason } => {
                format!(
                    "{} refused the projection: {reason:?}",
                    peer_name(self, peer)
                )
            }
            Notice::SpeakerInUseBy(p) => {
                format!("{} is playing sound on these speakers", peer_name(self, p))
            }
            Notice::MicInUseBy(p) => format!("{} is using this microphone", peer_name(self, p)),
            // Both roles report a refusal with the other node as `peer`.
            Notice::AudioRefused { peer, kind, reason } => format!(
                "audio ({kind:?}) with {} refused: {reason:?}",
                peer_name(self, peer)
            ),
            Notice::Home { key, entered } => self.home_notice(*key, *entered),
            Notice::HomeFailed { key, reason } => self.home_failed_notice(*key, *reason),
            other => format!("{other:?}"),
        };
        self.say(text);
    }

    /// Tell the user: a line in the log, the notice history and `crosspanectl status`.
    fn say(&mut self, text: String) {
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
            // While input is home in a projected window, its line says so (WP-2.43).
            projections: self
                .projections
                .iter()
                .map(|(k, text)| match &self.home.now {
                    Some(home) if home.key == *k => (
                        *k,
                        format!(
                            "{text} — input is home here ({} returns)",
                            self.release_keys()
                        ),
                    ),
                    _ => (*k, text.clone()),
                })
                .collect(),
            controlling: self.engine.controlling().map(|n| self.peer_label(n)),
            controlled_by: self.engine.controlled_by().map(|n| self.peer_label(n)),
            speakers: self.speaker_peers().map(|n| self.peer_label(n)).collect(),
            disarmed: !self.engine.armed(),
            missing_permissions,
            pairing: PairingView {
                phase: status.phase,
                sas: status.sas,
                candidates: status.candidates,
                peer: status.peer,
                error: status.error,
                offers: self.pairing_offers(),
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
                let capability = crate::ctl::capability_name(capability).unwrap_or("present");
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
            TrayAction::OpenApp => {
                match (self.tracker.spawn_settings)() {
                    // Only a spawn that worked counts (WP-4.5).
                    Ok(()) => self.tracker.settings_opened += 1,
                    Err(e) => {
                        tracing::warn!(error = %e, "could not open the settings app");
                        self.notices
                            .push_back(format!("could not open the settings app: {e}"));
                    }
                }
                return;
            }
        };
        if let Some(error) = response.error {
            self.notices.push_back(error);
        }
        // Show the effect at once rather than at the next refresh.
        self.tray.last_update = Instant::now() - TRAY_UPDATE;
    }

    /// Start mDNS discovery (03 §2): advertise this node and dial candidates when a paired peer
    /// is offline. Without it the agent runs on configured addresses. `CROSSPANE_DISCOVERY=0`
    /// turns it off altogether: nothing is advertised, browsed or dialled from a discovered
    /// address, so the agent only connects to configured or explicitly dialled addresses (test
    /// harnesses use this to stay away from other agents on the network).
    pub fn start_discovery(&mut self) {
        use crosspane_transport::discovery::Discovery;
        if discovery_switched_off(std::env::var("CROSSPANE_DISCOVERY").ok().as_deref()) {
            tracing::info!(
                "discovery off (CROSSPANE_DISCOVERY=0): connecting only to configured or explicit addresses"
            );
            return;
        }
        let events = self.events.clone();
        match Discovery::start(
            self.port,
            Box::new(move |ev| {
                let _ = events.send(Event::Discovery(ev));
            }),
        ) {
            Ok(d) => self.discovery = Some(d),
            Err(e) => {
                tracing::warn!(error = %e, "no discovery: using configured addresses only");
                // `Discovery::start` doesn't say which step failed: the daemon as a whole didn't
                // start.
                self.tracker.discovery_error = Some("daemon_failed");
            }
        }
    }

    fn on_discovery(&mut self, event: crosspane_transport::discovery::DiscoveryEvent) {
        use crosspane_transport::discovery::DiscoveryEvent;
        match event {
            DiscoveryEvent::Found(candidate) => {
                let fresh = !self.candidates.contains_key(&candidate.instance);
                if self.paired_peer_offline() && fresh && !self.connected_to(&candidate.addrs) {
                    for addr in self.dial_order(&candidate.addrs) {
                        self.net.dial_once(addr);
                    }
                }
                self.candidates
                    .insert(candidate.instance.clone(), candidate);
            }
            DiscoveryEvent::Lost { instance } => {
                self.candidates.remove(&instance);
            }
        }
    }

    fn paired_peer_offline(&self) -> bool {
        self.trust.with(|t| {
            t.peers()
                .iter()
                .any(|e| !self.peers.get(&e.node).is_some_and(|p| p.connected))
        })
    }

    /// Housekeeping for discovery: re-try candidates while a paired peer is offline, and put the
    /// device name in the advertisement only while a pairing window is open (04 §3).
    fn discovery_housekeeping(&mut self) {
        let Some(discovery) = &self.discovery else {
            return;
        };
        let listening = self.pairing.status().phase == "listening";
        if listening != self.advertising_name {
            let name = listening.then_some(self.name.as_str());
            match discovery.set_pairing_name(name) {
                Ok(()) => self.advertising_name = listening,
                Err(e) => tracing::debug!(error = %e, "could not update the advertisement"),
            }
        }
        if self
            .last_candidate_dial
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(15))
        {
            self.last_candidate_dial = Some(Instant::now());
            if self.paired_peer_offline() {
                let addrs: Vec<SocketAddr> = self
                    .candidates
                    .values()
                    .take(16)
                    .filter(|c| !self.connected_to(&c.addrs))
                    .flat_map(|c| self.dial_order(&c.addrs))
                    .collect();
                for addr in addrs {
                    self.net.dial_once(addr);
                }
            }
        }
    }

    /// Machines with a pairing window open: name and the address to join.
    fn pairing_offers(&self) -> Vec<(String, SocketAddr)> {
        self.candidates
            .values()
            .filter_map(|c| {
                let name = c.pairing_name.clone()?;
                let addr = self.dial_order(&c.addrs).into_iter().next()?;
                Some((name, addr))
            })
            .collect()
    }

    fn peer_label(&self, node: NodeId) -> String {
        self.peers
            .get(&node)
            .map_or_else(|| node.short(), |info| info.name.clone())
    }

    /// The peers playing sound on this machine's speakers now, each once, in node order.
    fn speaker_peers(&self) -> impl Iterator<Item = NodeId> {
        self.speakers
            .iter()
            .map(|key| key.peer)
            .collect::<BTreeSet<_>>()
            .into_iter()
    }

    /// QUIC declares a silent path dead only after its idle timeout, which grows with the probe
    /// backoff (RFC 9000 §10.1: at least 3 PTOs), so a vanished peer (asleep, out of range) can
    /// look connected for a minute or more. A peer that answers pings but hasn't for
    /// `UNRESPONSIVE` has its link closed, which starts the E2 grace period (WP-2.15) promptly.
    fn close_unresponsive(&mut self) {
        // If this loop itself stalled, pongs may be waiting in the queue: start the clocks again.
        let stalled = self.last_liveness.elapsed() > PING_INTERVAL * 2;
        self.last_liveness = Instant::now();
        if stalled {
            for at in self.last_pong.values_mut() {
                *at = Instant::now();
            }
            return;
        }
        let silent: Vec<NodeId> = self
            .links
            .keys()
            .filter(|peer| {
                self.last_pong
                    .get(peer)
                    .is_some_and(|at| at.elapsed() > UNRESPONSIVE)
            })
            .copied()
            .collect();
        for peer in silent {
            tracing::info!(peer = %peer.short(), "peer stopped answering: closing the link");
            self.last_pong.remove(&peer);
            if let Some(link) = self.links.get_mut(&peer) {
                link.close("unresponsive");
            }
        }
    }

    /// The link class of each connected peer's current path (03 §2): the interface the OS routes
    /// the peer's address through.
    fn update_paths(&mut self) {
        let mut paths = HashMap::new();
        for (peer, link) in &self.links {
            if let Some(remote) = link.remote_addr() {
                let mut class = local_class(&self.interfaces, remote);
                // Each end sees only its own interface: a wired one here may meet Wi-Fi on the
                // peer's side. A wired path answers in well under a millisecond, so measured RTT
                // refines the class (03 §2).
                if matches!(
                    class,
                    LinkClass::Lan | LinkClass::DirectEthernet | LinkClass::DirectUsb4Tb
                ) && link.rtt().is_some_and(|rtt| rtt > WIRED_RTT)
                {
                    class = LinkClass::Wifi;
                }
                paths.insert(*peer, class);
            }
        }
        for (peer, class) in &paths {
            if self.paths.get(peer) != Some(class) {
                tracing::info!(peer = %peer.short(), link = ?class, "path to peer");
            }
        }
        self.paths = paths;
    }

    /// A peer's Hello features changed (a replacement connection with a new feature set, or a
    /// new link while E2 streams outlive the old one): the encoder switches its streams to that
    /// peer over (video on or off, region video, cursor shapes). Called where a Hello is applied,
    /// after the cache update.
    pub fn peer_features_changed(&mut self, peer: NodeId) {
        let has = |feature: &str| {
            self.peers
                .get(&peer)
                .is_some_and(|p| p.features.iter().any(|f| f == feature))
        };
        let _ = self.source_media.send(SourceCmd::PeerFeatures {
            peer,
            video: has("h264"),
            region: has("h264roi"),
            cursor: has("cursor"),
        });
    }

    /// The video bitrate for streams to `peer`: the configured one, or one for its link class
    /// (03 §7.4).
    fn video_bits(&self, peer: NodeId) -> u32 {
        let mbps = self
            .video_mbps
            .unwrap_or_else(|| match self.paths.get(&peer) {
                Some(LinkClass::DirectUsb4Tb | LinkClass::DirectEthernet) => 150,
                Some(LinkClass::Lan) => 50,
                _ => 20,
            });
        mbps.saturating_mul(1_000_000)
    }

    /// Whether one of `addrs` is the address of a live link: dialling it again could make the
    /// transport replace a healthy connection.
    fn connected_to(&self, addrs: &[SocketAddr]) -> bool {
        let live: Vec<std::net::IpAddr> = self
            .links
            .values()
            .filter_map(|link| link.remote_addr())
            .map(|a| canonical(a).ip())
            .collect();
        addrs.iter().any(|a| live.contains(&canonical(*a).ip()))
    }

    /// Candidate addresses, best first: by the link class of the local interface they route
    /// through (faster links first), then IPv4 before global IPv6 before link-local. At most 3.
    fn dial_order(&self, addrs: &[SocketAddr]) -> Vec<SocketAddr> {
        let mut sorted = addrs.to_vec();
        sorted.sort_by_key(|a| {
            (
                class_rank(local_class(&self.interfaces, *a)),
                family_rank(a),
            )
        });
        sorted.truncate(3);
        sorted
    }

    /// An NTP-style sample: the peer's clock minus ours is ((t1 - t0) + (t2 - t3)) / 2. The
    /// sample with the shortest round trip of the last few is the most trustworthy.
    fn on_pong(&mut self, peer: NodeId, t0: u64, t1: u64, t2: u64) {
        self.last_pong.insert(peer, Instant::now());
        let t3 = platform::now().as_nanos();
        let (t0, t1, t2, t3) = (
            i128::from(t0),
            i128::from(t1),
            i128::from(t2),
            i128::from(t3),
        );
        let rtt = (t3 - t0) - (t2 - t1);
        if !(0..5_000_000_000).contains(&rtt) {
            return;
        }
        let offset = ((t1 - t0) + (t2 - t3)) / 2;
        let Ok(offset) = i64::try_from(offset) else {
            return;
        };
        let samples = self.clocks.entry(peer).or_default();
        samples.push_back((rtt as u64, offset));
        while samples.len() > CLOCK_SAMPLES {
            samples.pop_front();
        }
        if let Some(&(_, best)) = samples.iter().min_by_key(|(rtt, _)| *rtt) {
            self.proxy_ids.set_offset(peer, best);
        }
    }

    /// The opt-in latency overlay: frame rate and latency in each projected window's title.
    /// A proxy's title starts with its source machine's name (04 §5: a projected window always
    /// says where it comes from; the window title is drawn by this OS, not by the remote content).
    fn badged(&self, source: NodeId, title: &str) -> String {
        let from = self
            .peers
            .get(&source)
            .map_or_else(|| source.short(), |info| info.name.clone());
        proxy_title(format!("{from} › {title}"))
    }

    fn latency_titles(&mut self) {
        let keys: Vec<ProjectionKey> = self.titles.keys().copied().collect();
        for key in keys {
            let (Some(stats), Some(_id)) = (self.proxy_ids.stats(key), self.proxy_ids.id(key))
            else {
                continue;
            };
            let Some((title, frames)) = self.titles.get_mut(&key) else {
                continue;
            };
            let fps = stats.frames.saturating_sub(*frames) as f64 / HOUSEKEEPING.as_secs_f64();
            *frames = stats.frames;
            let latency = stats
                .latency_ms
                .map_or_else(|| "—".to_owned(), |ms| format!("{ms:.0} ms"));
            let text = format!("{title} — {fps:.0} fps, {latency}");
            self.set_proxy_title(key, text);
        }
    }

    /// Keep compositor matching in step with every title sent to the proxy host, including the
    /// optional latency decoration. A title update may change whether a proxy is unambiguous.
    fn set_proxy_title(&mut self, key: ProjectionKey, title: String) {
        let title = proxy_title(title);
        self.placement.retitled(key, &title);
        self.placement_dirty = true;
        if let (Some(h), Some(id)) = (&self.host, self.proxy_ids.id(key)) {
            let _ = h.send(HostCommand::SetTitle { id, title });
        }
    }

    fn housekeeping(&mut self) {
        self.home_housekeeping();
        self.discovery_housekeeping();
        // Known limit (WP-4.5): a backend that flips and flips back within one tick is missed.
        // Backends rarely flip back without a restart, and a restart is a new instance.
        self.backends_now();
        if self.last_ping.elapsed() >= PING_INTERVAL {
            self.last_ping = Instant::now();
            let t0 = platform::now().as_nanos();
            self.broadcast(&ControlMessage::Ping { t0 });
            self.update_paths();
            self.close_unresponsive();
        }
        if self.latency_overlay && self.last_titles.elapsed() >= HOUSEKEEPING {
            self.last_titles = Instant::now();
            self.latency_titles();
        }
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
                    self.tracker.grants_changed();
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
        // The same while a release bind of an earlier run can't be confirmed gone (amendment A1):
        // its keys would be injected into a seat that has a bind on them.
        let can_inject =
            self.platform.keys.is_some() && self.platform.pointer.is_some() && !self.home.fence;
        // Likewise a node without an audio worker grants neither the speakers nor the microphone:
        // it never advertised `audio`, so nothing could be admitted anyway.
        let can_play = self.audio.is_some();
        let grants: BTreeMap<NodeId, BTreeSet<Capability>> = self.trust.with(|t| {
            t.peers()
                .into_iter()
                .map(|e| {
                    let mut granted = e.granted.clone();
                    if !can_inject {
                        granted.remove(&Capability::InputAccept);
                    }
                    if !can_play {
                        granted.remove(&Capability::AudioSpeaker);
                        granted.remove(&Capability::AudioMic);
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
    ///
    /// Returns whether the placements changed. When they did, the layout epoch has advanced by one
    /// (WP-4.5): a caller whose own trigger counts anyway adds one only if this didn't.
    fn update_layout(&mut self, local_changed: bool) -> bool {
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
            self.tracker.layout_changed();
            self.feed(Input::Layout(self.placements.clone()));
            if local_changed {
                self.broadcast(&ControlMessage::Layout(self.explicit()));
            }
        }
        changed
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
        let version = self
            .placements
            .iter()
            .map(|p| p.version)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
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
        let version = self
            .placements
            .iter()
            .map(|p| p.version)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
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
        self.tracker.layout_changed();
        self.feed(Input::Layout(self.placements.clone()));
        self.broadcast(&ControlMessage::Layout(self.explicit()));
        Ok(())
    }

    /// Place displays at explicit positions (the settings app's layout editor).
    fn place(&mut self, entries: &[crate::ctl::PlaceEntry]) -> Result<(), String> {
        if entries.is_empty() {
            return Err("no placements".into());
        }
        let version = self
            .placements
            .iter()
            .map(|p| p.version)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let mut fresh = Vec::new();
        for entry in entries {
            let [x, y] = entry.origin_mm;
            if !(x.is_finite() && y.is_finite() && x.abs() < 1e6 && y.abs() < 1e6) {
                return Err(format!("bad position for display {}", entry.display));
            }
            let query = entry.node.trim().to_lowercase();
            let is_self = entry.node == self.name
                || (query.len() >= 4
                    && query.chars().all(|c| c.is_ascii_hexdigit())
                    && self.node.to_string().starts_with(&query));
            let node = if is_self {
                self.node
            } else {
                self.find_peer(&entry.node)
                    .ok_or_else(|| format!("no peer matches {:?}", entry.node))?
            };
            let displays = if node == self.node {
                &self.local_displays
            } else {
                &self.peers.get(&node).ok_or("unknown peer")?.displays
            };
            if !displays.iter().any(|d| d.id.0 == entry.display) {
                return Err(format!("{} has no display {}", entry.node, entry.display));
            }
            fresh.push(Placement {
                node,
                display: crosspane_types::id::DisplayId(entry.display),
                origin: crosspane_types::geom::PointMm::new(x, y),
                version,
            });
        }
        // The layout as it would be, checked for overlaps before anything changes.
        let mut next = self.placements.clone();
        arrange::merge(&mut next, &fresh);
        let size = |p: &Placement| {
            let displays = if p.node == self.node {
                Some(&self.local_displays)
            } else {
                self.peers.get(&p.node).map(|i| &i.displays)
            };
            displays
                .and_then(|ds| ds.iter().find(|d| d.id == p.display))
                .map(|d| d.geometry.physical_size)
        };
        let rects: Vec<_> = next
            .iter()
            .filter_map(|p| size(p).map(|s| (p, s)))
            .collect();
        for (i, (a, sa)) in rects.iter().enumerate() {
            for (b, sb) in &rects[i + 1..] {
                let dx =
                    (a.origin.x + sa.width).min(b.origin.x + sb.width) - a.origin.x.max(b.origin.x);
                let dy = (a.origin.y + sa.height).min(b.origin.y + sb.height)
                    - a.origin.y.max(b.origin.y);
                // A millimetre of slack: edges that touch are adjacent, not overlapping.
                if dx > 1.0 && dy > 1.0 {
                    return Err(format!(
                        "display {} of {} would overlap display {} of {}",
                        a.display.0,
                        self.peer_label(a.node),
                        b.display.0,
                        self.peer_label(b.node)
                    ));
                }
            }
        }
        self.placements = next;
        self.tracker.layout_changed();
        // Explicit side choices from the tray no longer describe the layout.
        self.tray.sides.clear();
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

    /// A connected-or-known peer by exact name (case-insensitive), else by a unique node-id
    /// prefix of at least 4 hex digits. Empty and ambiguous queries match nothing.
    fn find_peer(&self, query: &str) -> Option<NodeId> {
        resolve(
            query,
            self.peers
                .iter()
                .map(|(node, info)| (*node, info.name.as_str())),
        )
    }

    /// A paired peer from the trust store (it may be offline, e.g. a lost device), matched like
    /// [`Agent::find_peer`].
    fn find_trusted(&self, query: &str) -> Result<NodeId, String> {
        let pinned: Vec<(NodeId, String)> = self
            .trust
            .with(|t| t.peers().iter().map(|e| (e.node, e.name.clone())).collect());
        resolve(
            query,
            pinned.iter().map(|(node, name)| (*node, name.as_str())),
        )
        .ok_or_else(|| format!("no single paired machine matches {query:?}"))
    }

    /// Revoke a lost or stolen device (04 §4): forget it here, refuse it until it pairs again,
    /// and tell every other peer with a signed notice.
    fn revoke(&mut self, node: NodeId) -> Result<String, String> {
        let name = self.peer_label(node);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        // Receivers drop notices older than their pairing with the device: an unset clock would
        // make this one useless everywhere while looking successful here.
        if now_ms < 1_700_000_000_000 {
            return Err("this machine's clock isn't set; fix it before revoking".into());
        }
        let notice =
            crosspane_security::trust::TrustStore::issue_revocation(&self.identity, node, now_ms)
                .map_err(|e| format!("could not sign the revocation: {e}"))?;
        // Keep the notice first, so peers that are offline now still get it later.
        self.revocations
            .add(notice.clone())
            .map_err(|e| format!("could not save the revocation notice: {e}"))?;
        self.trust
            .update(|t| Ok(t.revoke(node)))
            .map_err(|e| format!("could not update the trust store: {e}"))?;
        self.tracker.grants_changed();
        self.close_untrusted();
        self.send_grants();
        // The revoked node's link is closing; it isn't told.
        let mut told = 0;
        for (peer, link) in &mut self.links {
            if *peer != node
                && link
                    .send_control(&ControlMessage::Revocation(notice.clone()))
                    .is_ok()
            {
                told += 1;
            }
        }
        tracing::info!(peer = %node.short(), told, "revoked");
        self.notices.push_back(format!("revoked {name}"));
        Ok(format!(
            "revoked {name}; told {told} connected peer(s), the others when they next connect"
        ))
    }

    /// A peer passed on a revocation notice: apply it if its issuer is trusted and it verifies.
    fn on_revocation(&mut self, from: NodeId, notice: &RevocationNotice) {
        use crosspane_security::trust::Revoked;
        // Notices travel only from their issuer (it resends them on every connection); a relayed
        // copy could replay a revocation the issuer has withdrawn by pairing again.
        if from != notice.issuer {
            tracing::warn!(peer = %from.short(), "revocation notice relayed by a non-issuer: ignored");
            return;
        }
        // Nothing to do for a node this machine never paired (and no disk write per notice).
        if !self.trust.with(|t| t.get(notice.revoked).is_some()) {
            tracing::debug!(revoked = %notice.revoked.short(), "revocation of an unknown node");
            return;
        }
        let own = self.node;
        let result = self.trust.update(|t| {
            t.apply_revocation(notice, own)
                .map_err(|e| anyhow::anyhow!("{e}"))
        });
        match result {
            Ok(Revoked::Applied { forgotten }) => {
                let revoked = forgotten.map_or_else(|| notice.revoked.short(), |entry| entry.name);
                let issuer = self.peer_label(notice.issuer);
                tracing::info!(revoked = %notice.revoked.short(), issuer = %notice.issuer.short(), "revocation applied");
                self.tracker.grants_changed();
                self.close_untrusted();
                self.send_grants();
                self.notices
                    .push_back(format!("{revoked} was revoked by {issuer}"));
            }
            Ok(Revoked::Stale) => {
                tracing::info!(revoked = %notice.revoked.short(), issuer = %notice.issuer.short(),
                    "revocation notice predates the current pairing: ignored");
            }
            Ok(Revoked::Duplicate | Revoked::IgnoredSelf) => {
                tracing::debug!(revoked = %notice.revoked.short(), "revocation notice already handled");
            }
            Err(e) => {
                tracing::warn!(peer = %from.short(), error = %e, "revocation notice rejected")
            }
        }
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
                // A fresh pairing supersedes this node's own revocation of that device.
                if let Err(e) = self.revocations.remove(peer.node) {
                    tracing::warn!(error = %e, "could not update the revocations file");
                }
                self.notices.push_back(format!("paired with {}", peer.name));
                self.tracker.grants_changed();
                self.send_grants();
                if let Some(addr) = paired.dial {
                    self.net.dial(addr);
                }
            }
            Err(e) => tracing::error!(error = %e, "could not pin the paired peer"),
        }
    }

    fn on_host(&mut self, event: HostEvent) {
        // Where the host can't say which display a proxy is on (Wayland), it says only whether the
        // proxy is visible; the placement source supplies the rest and is the only producer of
        // `ProxyEvent::Placed` there, so the host's own origin and size aren't used.
        if let HostEvent::Placed {
            id,
            visible,
            monitor: None,
            ..
        } = &event
            && HYPRLAND_PLACEMENT
        {
            if let Some(key) = self.proxy_ids.key(*id) {
                self.placement.set_visible(key, *visible);
                self.placement_dirty = true;
                self.flush_placements();
            }
            return;
        }
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
            HostEvent::Presented { id, frames } => {
                if let Some(key) = self.proxy_ids.key(id) {
                    self.proxy_ids.presented(key, frames);
                }
                return;
            }
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
            HostEvent::Placed {
                id,
                visible,
                monitor,
                origin,
                size,
            } => (
                id,
                Box::new(move |key| {
                    proxy(
                        key,
                        ProxyEvent::Placed {
                            display: monitor.filter(|_| visible).map(DisplayId),
                            origin,
                            size,
                        },
                    )
                }),
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
                let Some(capability) = crate::ctl::capability_named(&capability) else {
                    return Response::err(crate::ctl::unknown_capability(&capability));
                };
                match self.find_peer(&peer) {
                    Some(node) => match self.trust.update(|t| {
                        let before = t.clone();
                        t.set_grant(node, capability, allow)
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                        Ok(*t != before)
                    }) {
                        Ok(changed) => {
                            if changed {
                                self.tracker.grants_changed();
                            }
                            self.send_grants();
                            let mut text = format!(
                                "{} {capability:?} for {peer}",
                                if allow { "allowed" } else { "withdrew" }
                            );
                            if let Some(note) = crate::ctl::capability_note(capability) {
                                text = format!("{text} ({note})");
                            }
                            Response::ok(json!(text))
                        }
                        Err(e) => Response::err(format!("could not update the trust store: {e}")),
                    },
                    None => Response::err(format!("no peer called {peer}")),
                }
            }
            Request::WindowsFrom { .. } | Request::Pull { .. } => {
                Response::err("internal: answered asynchronously")
            }
            Request::Forget { peer } => match self.find_trusted(&peer).ok() {
                Some(node) => match self.trust.update(|t| Ok(t.forget(node))) {
                    Ok(Some(entry)) => {
                        self.tracker.grants_changed();
                        self.close_untrusted();
                        self.send_grants();
                        Response::ok(json!(format!("forgot {}", entry.name)))
                    }
                    Ok(None) => Response::err(format!("{peer} is not paired")),
                    Err(e) => Response::err(format!("could not update the trust store: {e}")),
                },
                None => Response::err(format!("no peer called {peer}")),
            },
            Request::Revoke { peer } => match self.find_trusted(&peer) {
                Ok(node) => match self.revoke(node) {
                    Ok(text) => Response::ok(json!(text)),
                    Err(e) => Response::err(e),
                },
                Err(e) => Response::err(e),
            },
            Request::AskPermissions => {
                let requested = crate::request_missing_permissions(&mut self.platform);
                Response::ok(json!(if requested.is_empty() {
                    "every permission is granted".to_owned()
                } else {
                    format!(
                        "asked for {}; answer the dialogs on this machine",
                        requested.join(", ")
                    )
                }))
            }
            Request::Restart => {
                self.restart_requested = true;
                Response::ok(json!("restarting"))
            }
            Request::SettingsUpdate {
                expected_revision,
                mac_virtual_display,
            } => match self.lifecycle_paths.as_ref() {
                Some(paths) => {
                    crate::config::settings_update(paths, &expected_revision, mac_virtual_display)
                }
                None => Response::err("config_update_failed"),
            },
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
            Request::Place { placements } => match self.place(&placements) {
                Ok(()) => Response::ok(json!("layout updated")),
                Err(e) => Response::err(e),
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
            Request::PairScan => Response::ok(json!(
                self.pairing_offers()
                    .into_iter()
                    .map(|(name, addr)| json!({ "name": name, "addr": addr.to_string() }))
                    .collect::<Vec<_>>()
            )),
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
            Request::Snapshot { projection, source } => match self.find_peer(&source) {
                None => Response::err(format!("no peer matches {source:?}")),
                Some(source) => {
                    let key = ProjectionKey {
                        source,
                        projection: ProjectionId(projection),
                    };
                    let (reply, answer) = std::sync::mpsc::channel::<Option<crate::media::Shown>>();
                    let _ = self.dest_media.send(DestCmd::Snapshot { key, reply });
                    match answer.recv_timeout(Duration::from_secs(2)) {
                        Ok(Some((size, pixels))) => match write_snapshot(key, size, &pixels) {
                            Ok(path) => Response::ok(json!(path)),
                            Err(e) => Response::err(format!("could not save the snapshot: {e}")),
                        },
                        _ => Response::err("no picture shown for that projection"),
                    }
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
                        if self.projections.contains_key(&key) {
                            self.feed(Input::Command(Command::Return(key)));
                            Response::ok(json!("returning"))
                        } else {
                            // A window ID here is a common slip: projections have their own
                            // numbers, shown by `crosspanectl status`.
                            Response::err(format!(
                                "no projection {projection} from that source; `crosspanectl \
                                 status` lists them (\"projecting window N\" or \"showing \
                                 window N\")"
                            ))
                        }
                    }
                }
            }
        }
    }

    /// Stop cleanly, in the order that keeps the invariants (04 §8): injected input is released and
    /// control ends (as `crosspanectl panic`), parked windows go back where they were (the journal
    /// would otherwise bring them back only at the next start), then the links close so peers end
    /// their sessions at once instead of after the idle timeout.
    fn shutdown(&mut self) -> crate::lifecycle::Shutdown {
        tracing::info!("stopping");
        // The panic ends every audio session too (the engine stops and closes each one, which the
        // worker carries out), so the worker has nothing running when it is shut down below.
        self.feed(Input::Command(Command::Panic));
        // Releases queue InjectDone answers. The journals forget held input only after those
        // answers reach the engine; failed releases remain held and make the receipt unclean.
        self.settle();
        // The engine asked for the home bind's removal as part of that (if it was installed); this
        // makes sure, whatever became of the answer (amendment A2: never a foreign bind's).
        self.home_shutdown();
        let parking = self.parking_shutdown(crate::parking_worker::SHUTDOWN_WAIT);
        // A panic's first restore may fail while the final journal recovery succeeds. Only a
        // fresh physical read-back after that recovery permits the remaining bind cleanup.
        self.home_shutdown();
        self.net.shutdown();
        // Last: peers already heard of the close, and the worker's stop is bounded (2.5 s) but
        // can be slower than the rest of this.
        let audio_stopped = if let Some(audio) = self.audio.take() {
            let started = Instant::now();
            audio.shutdown();
            // The worker detaches unfinished threads only at its 2.5 s deadline. Counting the
            // entire call and drop makes reaching that bound unknown, never a clean outcome.
            started.elapsed() < Duration::from_millis(2500)
        } else {
            true
        };
        crate::lifecycle::Shutdown {
            parking,
            input_journals_empty: self
                .lifecycle_paths
                .as_ref()
                .is_some_and(crate::lifecycle::journals_empty),
            audio_stopped,
        }
    }

    /// The audio part of `status`: whether this node shares audio at all (speakers only in v0),
    /// who plays on its speakers, and the worker's counters (never samples).
    fn audio_status(&self) -> Value {
        let Some(audio) = &self.audio else {
            return json!({ "enabled": false });
        };
        let s = audio.stats();
        json!({
            "enabled": true,
            "speakers_in_use": self.speaker_peers().map(|n| self.peer_label(n)).collect::<Vec<_>>(),
            "counters": {
                "sent": s.sent,
                "congested": s.congested,
                "received_unknown": s.rx_unknown,
                "received_overflow": s.rx_overflow,
                "received_rejected": s.rx_rejected,
                "played_frames": s.played,
                "playback_overflow": s.playback_overflow,
                "discarded_samples": s.discarded_samples,
                "sanitized_frames": s.sanitized_frames,
                "stale_replies": s.stale_replies,
                "commands_over_cap": s.commands_over_cap,
                "sessions_created": s.sessions_created,
                "encoders_created": s.encoders_created,
                "jitters_created": s.jitters_created,
            },
        })
    }

    fn status(&self) -> Value {
        let now = platform::now();
        json!({
            "node": self.node.to_string(),
            "name": self.name,
            "listening": self.net.local_addr().to_string(),
            "gate_open": self.platform.gate.is_open(),
            "armed": self.engine.armed(),
            "controlling": self.engine.controlling().map(|peer| peer.to_string()),
            "controlled_by": self.engine.controlled_by().map(|peer| peer.to_string()),
            "session": format!("{:?}", self.platform.session.state()),
            "backends": format!("{:?}", self.platform).replacen("parking: false", "parking: true", usize::from(self.parking_available)),
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
                "link": self.paths.get(node).map(|c| format!("{c:?}")),
                "displays": info.displays.iter().map(display_json).collect::<Vec<_>>(),
                "features": info.features,
                "grants": self.trust.with(|t| t.peers().iter().find(|e| e.node == *node).map(|e| {
                    e.granted.iter().filter_map(|c| crate::ctl::capability_name(*c)).collect::<Vec<_>>()
                }).unwrap_or_default()),
                // This peer is playing sound on this machine's speakers now.
                "speaker_in_use": self.speakers.iter().any(|key| key.peer == *node),
            })).collect::<Vec<_>>(),
            "audio": self.audio_status(),
            // Home on the twin (WP-2.43): the projection this node's input is home in (`null`
            // when it isn't) and whether the release bind is installed, as last verified.
            "home": {
                "projection": self.home.now.as_ref().map(|h| h.key.projection.0),
                "bind_installed": self.home.present == Some(true),
            },
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
                    "latency_ms": s.latency_ms.map(|ms| (ms * 10.0).round() / 10.0),
                }));
                json!({
                    "source": k.source.short(),
                    "projection": k.projection.0,
                    "text": text,
                    "received": frames,
                })
            }).collect::<Vec<_>>(),
            "uptime_s": now.as_nanos() / 1_000_000_000,
            // The typed local facts the installer reads (WP-4.5).
            "installer": self.installer_status(),
        })
    }
}

/// Home on the twin (WP-2.43e): the release bind, the warp read-back and the notices. The engine
/// decides when home is entered, kept and left; this is only what it asks of the compositor.
impl Agent {
    /// The release chord as Hyprland spells it, for notices.
    fn release_keys(&self) -> String {
        self.platform
            .home
            .as_ref()
            .map_or_else(|| "the release shortcut".to_owned(), |seat| seat.keys())
    }

    /// The answer to a `ReleaseAndWarp` whose `end(Some(warp_to))` returned `Ok`. That `Ok` says
    /// nothing about where the pointer is: the backend skips the warp while the I/O gate is
    /// closed and still returns `Ok`. So the pointer is read back (amendment A3), and the
    /// authoritative gate (session and engine sides, panic included) is asked **after** the
    /// read-back (amendment B2): a closed gate is `Skipped` whatever the coordinates say, even
    /// when the gate closed between the warp and the read-back and the engine hasn't heard of the
    /// lock yet. `Done` needs an open gate and a pointer on `warp_to`'s display within
    /// [`WARP_TOLERANCE`] of its rounded device-pixel point. Hyprland 0.56.2 floors cursorpos
    /// in logical pixels, so scale 2 can report two device pixels below the rounded warp target.
    /// A read-back that can't be made (or no Hyprland to read from) is an error, never `Done`.
    fn warp_result(&self, warp_to: (DisplayId, PointDevice)) -> Result<Warp, Failure> {
        let seen = self.platform.home.as_ref().map(|seat| seat.cursor());
        if !self.platform.gate.is_open() {
            return Ok(Warp::Skipped);
        }
        match seen {
            Some(Ok((on, at))) => {
                let (want_display, want) = warp_to;
                if on == want_display
                    && (at.x.round() - want.x.round()).abs() <= WARP_TOLERANCE
                    && (at.y.round() - want.y.round()).abs() <= WARP_TOLERANCE
                {
                    Ok(Warp::Done)
                } else {
                    tracing::warn!(
                        ?on,
                        ?at,
                        ?warp_to,
                        "the pointer isn't where it was warped to"
                    );
                    Ok(Warp::Skipped)
                }
            }
            Some(Err(e)) => {
                tracing::warn!(error = %e, "the pointer could not be read back after a warp");
                Err(Failure::Other)
            }
            None => Err(Failure::Other),
        }
    }

    /// `Output::HomeBind`: install (verified) or remove (verified absent) the release bind, and
    /// answer with `Input::HomeBindSet`.
    fn home_bind(&mut self, op: HomeOp, install: bool) {
        self.home.active = install;
        self.home.removal = (!install).then_some(op);
        let result = if install {
            self.home_install(op)
        } else {
            self.home_remove()
        };
        if !install && result.is_ok() {
            self.home.removal = None;
        }
        self.pending.push_back(Input::HomeBindSet {
            op,
            install,
            result,
        });
    }

    fn home_install(&mut self, op: HomeOp) -> Result<(), Failure> {
        // Home never engages while the startup cleanup is unconfirmed (amendment A1), nor where
        // there is no way to bind (the Mac, a Hyprland without usable IPC).
        if self.home.fence {
            self.home.error =
                Some("a release shortcut of an earlier run is not confirmed removed yet".into());
            return Err(Failure::Other);
        }
        let Some(seat) = &self.platform.home else {
            self.home.error = Some("this machine can't install a release shortcut".into());
            return Err(Failure::Other);
        };
        match seat.install() {
            Ok(()) => {
                tracing::info!(keys = %seat.keys(), "home bind installed and verified");
                self.home.wanted = Some(op);
                self.home.present = Some(true);
                self.home.error = None;
                self.home.last_check = Instant::now();
                Ok(())
            }
            Err(e) => {
                tracing::warn!(error = %e, "the home bind could not be installed");
                // Part of it may be there: the removal the engine asks for next finds out.
                self.home.wanted = None;
                self.home.present = None;
                self.home.error = Some(e.to_string());
                Err(Failure::Other)
            }
        }
    }

    /// Remove the bind, never a foreign one (amendment A2, B5: that is the module's rule). `Err`
    /// keeps the engine's teardown fence up; it retries with backoff.
    fn home_remove(&mut self) -> Result<(), Failure> {
        // The engine's A1 teardown keeps the seat arbitrated while this answer is an error.
        // Keep verifying/reinstalling our bind until a fallback has read back on a physical
        // display. A2 still belongs entirely to HomeSeat::remove; no foreign bind is touched.
        if self.home.pointer_unsafe && !self.home_capture_active() {
            return Err(Failure::Other);
        }
        // A clean twin-strip exit may remove the bind while E1 owns escape, but it is not
        // physical safety. Keep that obligation across a failed removal and capture ending.
        if !self.home.pointer_unsafe {
            self.home.wanted = None;
        }
        let Some(seat) = &self.platform.home else {
            // Nothing could have been installed here.
            return Ok(());
        };
        let keys = seat.keys();
        match seat.remove() {
            Ok(()) => {
                self.home.wanted = None;
                self.home.present = Some(false);
                // Rollback precedes HomeFailed in the engine's output list. Keep the install
                // error until the next install so that notice can still explain the failure.
                self.home.removal_reported = false;
                if std::mem::take(&mut self.home.fence) {
                    self.say(format!(
                        "The release shortcut {keys} is gone; remote input to this machine is accepted again"
                    ));
                    self.send_grants();
                }
                Ok(())
            }
            Err(e) => {
                tracing::warn!(error = %e, "the home bind could not be removed");
                self.home.present = None;
                self.home.error.get_or_insert_with(|| e.to_string());
                // The engine retries with backoff: the user hears of the episode once.
                if !std::mem::replace(&mut self.home.removal_reported, true) {
                    self.say(format!(
                        "Crosspane can't remove its release shortcut {keys} ({e}); if another binding uses it, reload your Hyprland config"
                    ));
                }
                Err(Failure::Other)
            }
        }
    }

    /// At start (amendment A1): remove a bind an earlier run left. If that can't be confirmed,
    /// this node injects nothing but releases and refuses E1 control until it can (retried every
    /// [`FENCE_RETRY`]). A foreign bind on the chord, ours absent, is confirmed absent: it is left
    /// alone and doesn't fence (B5).
    fn home_startup(&mut self) {
        let Some(seat) = &self.platform.home else {
            return;
        };
        let keys = seat.keys();
        match seat.remove() {
            Ok(()) => self.home.present = Some(false),
            Err(e) => {
                tracing::warn!(error = %e, "a leftover home bind could not be removed");
                self.home.fence = true;
                self.home.error = Some(e.to_string());
                self.home.last_fence_try = Instant::now();
                self.say(format!(
                    "Crosspane can't remove its release shortcut {keys} because another binding uses it; reload your Hyprland config. Remote input to this machine is refused until then ({e})"
                ));
            }
        }
    }

    /// The periodic part: the startup fence's retry, and the bind's verification while it is
    /// wanted (on a config reload at once, else every [`BIND_CHECK`]).
    fn home_housekeeping(&mut self) {
        self.home_watchdog();
        if self.home.fence && self.home.last_fence_try.elapsed() >= FENCE_RETRY {
            self.home.last_fence_try = Instant::now();
            self.home_fence_retry();
        }
        if self.home.wanted.is_none() {
            self.home.reload = false;
        } else if self.home.reload || self.home.last_check.elapsed() >= BIND_CHECK {
            self.home.reload = false;
            self.home.last_check = Instant::now();
            self.home_verify();
        }
    }

    fn home_parked(
        &mut self,
        window: WindowId,
        result: &Result<crosspane_platform::Parked, Failure>,
    ) {
        if let Ok(parked) = result {
            if parked.kind == crosspane_platform::ParkingKind::Twin {
                self.home.twins.insert(window, parked.display);
            } else {
                self.home.twins.remove(&window);
            }
        }
    }

    fn physical_display(&self, display: DisplayId) -> bool {
        (self.local_displays.iter().any(|d| d.id == display)
            || self.home.physical.contains(&display))
            && !self.home.twins.values().any(|twin| *twin == display)
    }

    fn home_capture_active(&self) -> bool {
        self.home.capture.is_some() && self.engine.controlling().is_some()
    }

    /// A fallback may have moved the cursor even if end/warp confirmation failed or a lock
    /// arrived afterwards. Bind removal needs verified physical safety, not a successful `end`.
    fn home_confirm_physical(&mut self) {
        self.home_confirm_physical_in(None);
    }

    /// A rescue already queried physical Displays: its single confirmation must use that exact
    /// snapshot rather than subscription data, which can be stale or empty independently.
    fn home_confirm_physical_in(&mut self, physical: Option<&[DisplayInfo]>) {
        if self.home.pointer_unsafe
            && self.platform.home.as_ref().is_some_and(|seat| {
                seat.cursor().is_ok_and(|(on, _)| {
                    physical.map_or_else(
                        || self.physical_display(on),
                        |snapshot| snapshot.iter().any(|display| display.id == on),
                    )
                })
            })
        {
            self.home.pointer_unsafe = false;
            self.home.rescue_reported = false;
        }
        self.home_finish_removal();
    }

    fn home_watchdog_needed(&self) -> bool {
        self.platform.gate.is_open()
            && !self.home.active
            && !self.home_capture_active()
            && (!self.home.twins.is_empty()
                || self.home.pointer_unsafe
                || self.home.removal.is_some())
            && self.platform.home.is_some()
    }

    /// Shared by the post-warp read-back and a periodic read that already established safety.
    fn home_finish_removal(&mut self) {
        if !self.home.pointer_unsafe
            && let Some(op) = self.home.removal
        {
            let result = self.home_remove();
            if result.is_ok() {
                self.home.removal = None;
            }
            self.pending.push_back(Input::HomeBindSet {
                op,
                install: false,
                result,
            });
        }
    }

    fn home_fallback(&self, physical: &[DisplayInfo]) -> Option<(DisplayId, PointDevice)> {
        self.home
            .fallback
            .filter(|(display, _)| physical.iter().any(|d| d.id == *display))
            .or_else(|| {
                // DisplayInfo has no primary flag: the first physical display is the stable,
                // conservative default until the engine gives us its explicit fallback.
                physical.first().map(|d| {
                    let size = d.geometry.pixel_size;
                    (
                        d.id,
                        PointDevice::new(f64::from(size.width) / 2.0, f64::from(size.height) / 2.0),
                    )
                })
            })
    }

    /// The invariant backstop, independent of the engine's stranded retry budget. Event-driven
    /// housekeeping shares this deadline: at most one classification read per 500 ms, plus one
    /// fresh physical confirmation only when a rescue is attempted. Exempt states do no IPC.
    fn home_watchdog(&mut self) {
        if !self.home_watchdog_needed() || Instant::now() < self.home.watchdog_next {
            return;
        }
        self.home.watchdog_next = Instant::now() + HOME_WATCHDOG;
        let Some(Ok((on, _))) = self.platform.home.as_ref().map(|seat| seat.cursor()) else {
            return;
        };
        // The read can race a lock notification: ask the shared gate again after IPC.
        if !self.platform.gate.is_open() {
            return;
        }
        if self.physical_display(on) {
            self.home.rescue_reported = false;
            self.home.pointer_unsafe = false;
            self.home_finish_removal();
            return;
        }
        let twin = self.home.twins.values().any(|twin| *twin == on);
        if !twin && !self.home.pointer_unsafe && self.home.removal.is_none() {
            return;
        }
        // Displays excludes CROSSPANE-* by name. Revalidate a rescue candidate against
        // the current physical snapshot, so a hotplugged physical output reusing a cached ID
        // can never be mistaken for the removed twin (even when journal cleanup failed). An
        // outstanding cleanup also needs this lookup when subscription data cannot classify
        // the cursor, including after restoring the last twin.
        let Ok(physical) = self.platform.displays.displays() else {
            return;
        };
        self.home.physical = physical.iter().map(|display| display.id).collect();
        self.home
            .twins
            .retain(|_, twin| !physical.iter().any(|d| d.id == *twin));
        if physical.iter().any(|d| d.id == on) {
            self.home.pointer_unsafe = false;
            self.home.rescue_reported = false;
            self.home_finish_removal();
            return;
        }
        if !twin {
            return;
        }
        if !self.platform.gate.is_open() {
            return;
        }
        self.home.pointer_unsafe = true;
        if !std::mem::replace(&mut self.home.rescue_reported, true) {
            tracing::warn!(
                twin_display = on.0,
                "the pointer is on a twin outside home; returning it to a physical display"
            );
        }
        let Some(fallback) = self.home_fallback(&physical) else {
            return;
        };
        if !self.platform.gate.is_open() {
            return;
        }
        let Some(capture) = &mut self.platform.capture else {
            return;
        };
        if let Err(error) = capture.end(Some(fallback)) {
            tracing::debug!(%error, "the watchdog fallback could not be completed");
        }
        self.home_confirm_physical_in(Some(&physical));
    }

    fn home_fence_retry(&mut self) {
        let Some(seat) = &self.platform.home else {
            self.home.fence = false;
            self.send_grants();
            return;
        };
        let keys = seat.keys();
        match seat.remove() {
            Ok(()) => {
                tracing::info!("the leftover home bind is gone");
                self.home.fence = false;
                self.home.present = Some(false);
                self.home.error = None;
                self.say(format!(
                    "The release shortcut {keys} is gone; remote input to this machine is accepted again"
                ));
                self.send_grants();
            }
            Err(e) => {
                tracing::debug!(error = %e, "the leftover home bind is still there");
                self.home.error = Some(e.to_string());
            }
        }
    }

    /// While the engine wants the bind: it must be listed. A reload drops runtime binds; if it is
    /// missing it is reinstalled, silently. Only if that fails is the engine told, and it leaves
    /// home (04 §6: the escape must exist whenever input is home).
    fn home_verify(&mut self) {
        let (Some(op), Some(seat)) = (self.home.wanted, &self.platform.home) else {
            return;
        };
        if matches!(seat.installed(), Ok(true)) {
            self.home.present = Some(true);
            return;
        }
        match seat.install() {
            Ok(()) => {
                tracing::info!("the home bind was missing and is installed again");
                self.home.present = Some(true);
            }
            Err(e) => {
                tracing::warn!(error = %e, "the home bind was lost and could not be installed again");
                self.home.present = None;
                self.home.error = Some(e.to_string());
                if !self.home.pointer_unsafe {
                    self.home.wanted = None;
                }
                self.pending.push_back(Input::HomeBindSet {
                    op,
                    install: true,
                    result: Err(Failure::Other),
                });
            }
        }
    }

    /// At stop: make sure the bind is gone, whatever became of the engine's own removal.
    fn home_shutdown(&mut self) {
        self.home_confirm_physical();
        if self.home.pointer_unsafe {
            return;
        }
        self.home.wanted = None;
        if let Some(seat) = &self.platform.home {
            match seat.remove() {
                Ok(()) => self.home.present = Some(false),
                Err(e) => tracing::warn!(error = %e, "the home bind could not be removed at stop"),
            }
        }
    }

    /// The projected window `key` as the notices name it: its title if the compositor lists it
    /// on the display it is captured from, else its number.
    fn home_title(&self, key: ProjectionKey) -> String {
        self.capture_display
            .get(&key.projection)
            .and_then(|display| self.placement.window_on(*display))
            .map(|window| window.title.as_str())
            .filter(|title| !title.is_empty())
            .map_or_else(
                || format!("projected window {}", key.projection.0),
                |title| format!("\"{title}\""),
            )
    }

    /// `Notice::Home`: this node's input went home into a projected window, or left it.
    fn home_notice(&mut self, key: ProjectionKey, entered: bool) -> String {
        let controlling = self.engine.controlling();
        // Refresh the tray line now rather than at the next second.
        self.tray.last_update = Instant::now()
            .checked_sub(TRAY_UPDATE)
            .unwrap_or_else(Instant::now);
        if entered {
            self.home.active = true;
            let title = self.home_title(key);
            let peer =
                controlling.map_or_else(|| "the other machine".to_owned(), |p| self.peer_label(p));
            let keys = self.release_keys();
            self.home.now = Some(HomeNow {
                key,
                peer: controlling,
                title: title.clone(),
            });
            return format!(
                "Input is home in {title}; {peer} stays connected; press {keys} or push past the window's edges to return"
            );
        }
        self.home.active = false;
        let was = self.home.now.take_if(|home| home.key == key);
        match controlling {
            Some(peer) => format!("Input returned to {}", self.peer_label(peer)),
            // The session ended with home (a release, a lost link, a lock): input is simply here.
            None => match was {
                Some(HomeNow {
                    peer: Some(peer),
                    title,
                    ..
                }) => format!(
                    "Input left {title}; control of {} ended",
                    self.peer_label(peer)
                ),
                _ => "Input left the projected window".to_owned(),
            },
        }
    }

    /// `Notice::HomeFailed`: one line per reason, with the agent's own detail where it has one.
    fn home_failed_notice(&mut self, key: ProjectionKey, reason: HomeFailure) -> String {
        self.home.active = false;
        self.home.now.take_if(|home| home.key == key);
        self.tray.last_update = Instant::now()
            .checked_sub(TRAY_UPDATE)
            .unwrap_or_else(Instant::now);
        let title = self.home_title(key);
        let why = match reason {
            HomeFailure::Drain => {
                let detail = self
                    .home
                    .inject_error
                    .as_ref()
                    .filter(|(at, _)| at.elapsed() < INJECT_ERROR_AGE)
                    .map_or_else(String::new, |(_, e)| {
                        format!(" (last injection error: {e})")
                    });
                format!("a key or button release was not confirmed in time{detail}")
            }
            HomeFailure::Bind => {
                let detail = self
                    .home
                    .error
                    .as_ref()
                    .map_or_else(String::new, |e| format!(": {e}"));
                format!(
                    "the release shortcut {} could not be installed or kept{detail}",
                    self.release_keys()
                )
            }
            HomeFailure::Release => {
                "the keyboard and pointer could not be released from the capture in time".to_owned()
            }
            HomeFailure::Warp => {
                if self.platform.gate.is_open() {
                    "could not confirm the pointer position".to_owned()
                } else {
                    "could not confirm the pointer position (the input gate is closed)".to_owned()
                }
            }
            HomeFailure::Focus => "the window did not take focus in time".to_owned(),
            HomeFailure::Guard => {
                "a button was pressed, the screen locked or crossing was turned off while entering"
                    .to_owned()
            }
            HomeFailure::Gone => {
                "the window, its position or its last pointer exit went away".to_owned()
            }
        };
        format!("Input is not home in {title}: {why}")
    }
}

/// Winit's Wayland title limit, applied before sending and matching titles so truncation cannot
/// turn distinct cached strings into permanently missing or ambiguously named proxies.
fn proxy_title(mut title: String) -> String {
    if HYPRLAND_PLACEMENT && title.len() > 1024 {
        let mut end = 1024;
        while !title.is_char_boundary(end) {
            end -= 1;
        }
        title.truncate(end);
    }
    title
}

/// Match a peer by exact name (case-insensitive), else by a unique node-id prefix of at least 4
/// hex digits; empty and ambiguous queries match nothing.
fn resolve<'a>(
    query: &str,
    peers: impl Iterator<Item = (NodeId, &'a str)> + Clone,
) -> Option<NodeId> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return None;
    }
    let mut named = peers
        .clone()
        .filter(|(_, name)| name.to_lowercase() == query);
    if let Some((node, _)) = named.next() {
        return if named.next().is_none() {
            Some(node)
        } else {
            None
        };
    }
    if query.len() < 4 || !query.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut prefixed = peers.filter(|(node, _)| node.to_string().starts_with(&query));
    match (prefixed.next(), prefixed.next()) {
        (Some((node, _)), None) => Some(node),
        _ => None,
    }
}

/// Start the settings app: `crosspane-ui` next to this executable (the same bin directory, or
/// Contents/MacOS in the app bundle), else from PATH. Its exit is reaped on a thread.
fn open_settings_app() -> std::io::Result<()> {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("crosspane-ui")))
        .filter(|path| path.exists());
    let program = beside.unwrap_or_else(|| std::path::PathBuf::from("crosspane-ui"));
    let mut child = std::process::Command::new(program)
        .stdin(std::process::Stdio::null())
        .spawn()?;
    std::thread::Builder::new()
        .name("settings-app".into())
        .spawn(move || {
            let _ = child.wait();
        })?;
    Ok(())
}

/// Write a decoded picture as a binary PPM (RGB) in the temp directory, readable only by this user.
fn write_snapshot(
    key: ProjectionKey,
    size: crosspane_types::geom::PixelSize,
    bgra: &[u8],
) -> anyhow::Result<String> {
    let (w, h) = (size.width as usize, size.height as usize);
    anyhow::ensure!(bgra.len() >= w * h * 4, "picture shorter than its size");
    let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
    ppm.reserve(w * h * 3);
    for pixel in bgra[..w * h * 4].as_chunks::<4>().0 {
        ppm.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
    }
    let path = std::env::temp_dir().join(format!(
        "crosspane-snapshot-{}-{}.ppm",
        key.source.short(),
        key.projection.0
    ));
    crate::paths::write_private(&path, &ppm)?;
    Ok(path.display().to_string())
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

/// How a failed `set_portals` reads to the engine (WP-2.43 B1). The Hyprland backend says which it
/// is, by value (`set_portals_failure`): only a failure its worker reported, before anything
/// changed, is `Rejected` (the previous set and any capture are intact). The caller's receive
/// timeout, a backend that is gone or aborted, and anything else leave unknown whether they
/// survive, so they are `Uncertain`: the engine then ends any capture and waits for its end.
#[cfg(target_os = "linux")]
fn portals_failure(e: &PlatformError) -> PortalsFailure {
    use crosspane_platform_linux::hyprland::capture::{SetPortalsFailure, set_portals_failure};
    match set_portals_failure(e) {
        SetPortalsFailure::Rejected => PortalsFailure::Rejected,
        SetPortalsFailure::Uncertain => PortalsFailure::Uncertain,
    }
}

/// The same for the macOS backend, which has no such marker: a timeout and `Backend(..)` (a
/// stopped backend, or a rejection it only describes in text) leave unknown whether the previous
/// set and the capture survive, so they are `Uncertain`. Every other error is the backend refusing
/// the set up front.
#[cfg(not(target_os = "linux"))]
fn portals_failure(e: &PlatformError) -> PortalsFailure {
    match e {
        PlatformError::Timeout | PlatformError::Backend(_) => PortalsFailure::Uncertain,
        _ => PortalsFailure::Rejected,
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
    if let Some(links) = &mut platform.links {
        let links_tx = sink(tx);
        if let Err(e) = links.subscribe(std::sync::Arc::new(move |interfaces| {
            let _ = links_tx.send(Event::Links(interfaces));
        })) {
            tracing::warn!(error = %e, "network interface updates unavailable");
            platform.links = None;
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
    // A config reload drops runtime keybinds: the home bind is checked at once, not only at the
    // next housekeeping pass (WP-2.43 §2.9).
    if let Some(home) = &mut platform.home {
        let home_tx = sink(tx);
        if let Err(e) = home.watch_reload(Box::new(move || {
            let _ = home_tx.send(Event::HomeBind);
        })) {
            tracing::warn!(error = %e, "compositor reloads aren't watched: the home bind is verified once a second");
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
            other => {
                tracing::debug!(peer = %peer.short(), msg = %variant(other), "in: link input")
            }
        },
        // Many inputs can carry key usages (held-key states, proxy keys, recoveries): log only
        // the variant, except for the few known to hold none.
        // A destination's report of where its proxy is (WP-2.43): geometry only.
        Input::Link(LinkEvent::Control {
            peer,
            msg:
                ControlMessage::Projection(ProjectionMessage::ProxyPlaced {
                    projection,
                    generation,
                    display: on,
                    origin,
                    size,
                }),
        }) => tracing::debug!(
            peer = %peer.short(),
            projection = projection.0,
            generation,
            display = ?on,
            ?origin,
            ?size,
            "in: proxy placed report"
        ),
        Input::Link(LinkEvent::Control { peer, .. }) => {
            tracing::debug!(peer = %peer.short(), "in: control")
        }
        Input::Windows(_) | Input::Session(_) | Input::PeerUp { .. } => {
            tracing::debug!(input = ?input, "in")
        }
        // The answers of the home acknowledgements (WP-2.43): ids, geometry and results only.
        Input::PortalsSet { ids, result } => {
            let ids: Vec<u32> = ids.iter().map(|id| id.0).collect();
            tracing::debug!(?ids, ?result, "in: portals set")
        }
        Input::CaptureReleased { op, result } => {
            tracing::debug!(op = op.0, ?result, "in: capture released")
        }
        Input::HomeBindSet {
            op,
            install,
            result,
        } => tracing::debug!(op = op.0, install, ?result, "in: home bind set"),
        Input::Proxy {
            key,
            event:
                ProxyEvent::Placed {
                    display: on,
                    origin,
                    size,
                },
        } => tracing::debug!(
            projection = key.projection.0,
            display = ?on,
            ?origin,
            ?size,
            "in: proxy placed"
        ),
        other => tracing::debug!(input = %variant(other), "in"),
    }
}

/// The variant path of a value's `Debug` form ("Proxy", "Capture(Key"), without its fields: log
/// lines must never carry key usages (04 §7).
fn variant(value: &impl std::fmt::Debug) -> String {
    let text = format!("{value:?}");
    let end = text
        .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':' || c == '('))
        .unwrap_or(text.len());
    text[..end].trim_end_matches('(').to_owned()
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
        Output::SendInput { peer, msg } => {
            tracing::debug!(peer = %peer.short(), msg = %variant(msg), "out: send input")
        }
        Output::Inject { id, cmd } => tracing::debug!(?id, cmd = %variant(cmd), "out: inject"),
        other => tracing::debug!(output = %variant(other), "out"),
    }
}

fn proxy(key: ProjectionKey, event: ProxyEvent) -> Input {
    Input::Proxy { key, event }
}

/// Start this agent again in place (same binary, same arguments, same PID), after a clean
/// shutdown. Exits if that fails, rather than run on with closed links.
pub(crate) fn restart() -> ! {
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

/// A node's colour on proxy edges: a hue from its id, at fixed saturation and lightness.
fn node_accent(node: NodeId) -> [u8; 3] {
    let hue = f64::from(u16::from_be_bytes([node.0[0], node.0[1]])) * 6.0 / 65536.0;
    let chroma = 0.65;
    let secondary = chroma * (1.0 - (hue % 2.0 - 1.0).abs());
    let rgb = match hue as u8 {
        0 => [chroma, secondary, 0.0],
        1 => [secondary, chroma, 0.0],
        2 => [0.0, chroma, secondary],
        3 => [0.0, secondary, chroma],
        4 => [secondary, 0.0, chroma],
        _ => [chroma, 0.0, secondary],
    };
    rgb.map(|value| ((value + 0.175) * 255.0).round() as u8)
}

/// `status.result.installer`, schema version 1 (WP-4.5): the typed local facts the installer reads
/// to decide whether Crosspane works on this machine, without parsing prose.
///
/// Everything here is observed by the agent from what the engine emits and what the backends
/// report, and counted for this instance only (a restart is a new instance with a new id). Nothing
/// in it names a key, a typed character, a sample, a window title or a peer address.
mod installer {
    use std::cell::RefCell;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crosspane_engine::io::{InjectId, ReleaseCause};
    use crosspane_platform::LockState;
    use crosspane_protocol::msg::InputMessage;
    use crosspane_protocol::projection::{ParkingKind, ProjectionEndReason};

    use super::*;
    use crate::config::Config;
    use crate::keys::KeySource;
    use crate::paths::Paths;

    /// `installer.schema_version`.
    const SCHEMA_VERSION: u32 = 1;

    /// What the agent learned about its own process at startup.
    #[derive(Clone, Debug)]
    pub struct StartupFacts {
        instance_id: u64,
        pid: u32,
        uid: u32,
        exe: String,
        runtime_dir: String,
        started_unix_ms: u64,
        config_revision: String,
        /// Where the identity came from, and whether the config forced the key file.
        pub(super) key_source: KeySource,
        pub(super) force_file_keystore: bool,
    }

    impl StartupFacts {
        /// The facts of this process: its id, the files it runs from and with, and where its
        /// identity came from. `config` is the one loaded from `paths`, and `config_revision` the
        /// digest of the exact bytes it was parsed from (`Config::load_revision`).
        pub fn collect(
            node: NodeId,
            paths: &Paths,
            key_source: KeySource,
            config: &Config,
            config_revision: String,
        ) -> StartupFacts {
            StartupFacts::with(
                node,
                &paths.runtime_dir,
                config_revision,
                key_source,
                config.force_file_keystore,
            )
        }

        pub fn with_instance(mut self, instance: crate::lifecycle::Instance) -> Self {
            self.instance_id = instance.id;
            self.pid = instance.pid;
            self.started_unix_ms = instance.started_unix_ms;
            self
        }

        /// Facts for an agent nobody has given any (tests, and the instant before `main` does).
        pub fn unknown(node: NodeId) -> StartupFacts {
            StartupFacts::with(
                node,
                Path::new(""),
                crate::config::revision_of(None),
                KeySource::File,
                false,
            )
        }

        fn with(
            _node: NodeId,
            runtime_dir: &Path,
            config_revision: String,
            key_source: KeySource,
            force_file_keystore: bool,
        ) -> StartupFacts {
            let started = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            let pid = std::process::id();
            StartupFacts {
                instance_id: crate::lifecycle::instance_id(
                    pid,
                    u64::try_from(started.as_nanos()).unwrap_or(u64::MAX),
                ),
                pid,
                uid: rustix::process::geteuid().as_raw(),
                exe: std::env::current_exe()
                    .map(|exe| exe.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                runtime_dir: runtime_dir.to_string_lossy().into_owned(),
                started_unix_ms: u64::try_from(started.as_millis()).unwrap_or(u64::MAX),
                config_revision,
                key_source,
                force_file_keystore,
            }
        }
    }

    /// The E1 sessions this node has established right now: as the controller (the peer has
    /// acknowledged and the capture is live, `Engine::control_established`) and as the target.
    pub type Sessions = (Option<NodeId>, Option<NodeId>);

    /// The peer, if any, whose key or button messages this input carries as E1 input: a controller
    /// driving this node as its target. (A projection's input is E2's, and is never counted here.)
    pub fn e1_input_peer(input: &Input) -> Option<NodeId> {
        match input {
            Input::Link(LinkEvent::Input { peer, msg })
                if !matches!(msg, InputMessage::Proj(_)) =>
            {
                Some(*peer)
            }
            _ => None,
        }
    }

    /// What one input did to one E1 role's session: the peer whose session ended and the peer
    /// whose session started. `announced` is the peer a new session was announced to (or admitted
    /// from) during this input: it tells an end followed by a start with the same peer, which
    /// looks like no change in `before` and `after`.
    fn session_edges(
        before: Option<NodeId>,
        after: Option<NodeId>,
        announced: Option<NodeId>,
    ) -> (Option<NodeId>, Option<NodeId>) {
        let ended = before.filter(|b| after != Some(*b) || announced == Some(*b));
        let started = after.filter(|a| before != Some(*a) || announced == Some(*a));
        (ended, started)
    }

    /// The monotonic per-peer counters (never reset within an instance). The values that don't
    /// live here: `e2_frames_presented` comes from the proxy stats (`media.rs`).
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct PeerCounters {
        pub e1_controller_started: u64,
        pub e1_controller_ended: u64,
        pub e1_target_started: u64,
        pub e1_target_ended: u64,
        pub e1_injections_ok: u64,
        pub e1_hud_shows: u64,
        pub e1_chord_releases: u64,
        pub e1_command_releases: u64,
        pub e2_source_started: u64,
        pub e2_source_returned: u64,
        pub e2_dest_started: u64,
        pub e2_dest_returned: u64,
        pub e2_returns_failed: u64,
    }

    /// One backend as `status` lists it.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Backend {
        pub name: &'static str,
        /// `ready`, `missing`, `blocked` or `failed`.
        pub state: &'static str,
        /// `None` when ready, else `not_supported`, `permission`, `construction_failed`,
        /// `worker_exited`, `disabled` or `unknown`.
        pub reason: Option<&'static str>,
    }

    fn up(name: &'static str) -> Backend {
        Backend {
            name,
            state: "ready",
            reason: None,
        }
    }

    fn down(name: &'static str, state: &'static str, reason: &'static str) -> Backend {
        Backend {
            name,
            state,
            reason: Some(reason),
        }
    }

    /// The backend list as last computed, and how many times it has differed from the one before.
    #[derive(Debug, Default)]
    struct BackendsSeen {
        last: Option<Vec<Backend>>,
        epoch: u64,
    }

    /// Everything the agent counts and remembers for `status.result.installer`.
    pub struct Tracker {
        pub counters: BTreeMap<NodeId, PeerCounters>,
        /// Per peer: how many connections have been established this instance (monotonic, and
        /// kept when the peer is forgotten, so that a re-pairing doesn't start over).
        link_generation: BTreeMap<NodeId, u64>,
        /// Tray "Settings…" actions whose process spawn succeeded.
        pub settings_opened: u64,
        /// How a Settings spawn is attempted (a seam: tests don't start a process).
        pub spawn_settings: fn() -> std::io::Result<()>,
        grants_epoch: u64,
        layout_epoch: u64,
        /// Set through `&self` (`status` is a read), hence the cell.
        backends: RefCell<BackendsSeen>,
        /// Live projections of this node's windows (capture started), with the peer each goes to.
        sources: BTreeMap<ProjectionKey, NodeId>,
        /// Projections of this node's windows that are parked but whose capture hasn't started:
        /// what `ProjectionStarted` said, kept until the matching `CaptureStarted` succeeds.
        pending_sources: BTreeMap<ProjectionKey, (NodeId, ParkingKind)>,
        /// How the latest projection of this node's window to each peer is parked (`twin` or
        /// `mirror`). Never reset, and absent until the first projection to that peer.
        last_parking: BTreeMap<NodeId, &'static str>,
        /// Open proxies of other nodes' windows.
        proxies: BTreeSet<ProjectionKey>,
        /// The last Restore registered by this input, to attribute its following source notice.
        last_restore: Option<u64>,
        /// Source return attribution survives unrelated inputs until the actual completion.
        restores: BTreeMap<u64, Option<NodeId>>,
        /// Whether the last `CloseProxy` of this input's outputs was queued to the proxy host.
        last_close: Option<bool>,
        /// Tests only: stands in for sending the host its `Close` (there is no window host).
        #[cfg(test)]
        pub close_seam: Option<fn(u64) -> bool>,
        /// The controller session this node had established before the input being handled.
        established_before: Option<NodeId>,
        /// Windows this instance parked and has not restored since: its journal entries still
        /// unresolved. Entries an earlier run left behind are `startup_recovery`'s.
        parked: BTreeMap<WindowId, u64>,
        /// Key and button injections that answer a controller's own message, with that peer.
        e1_injects: BTreeMap<InjectId, NodeId>,
        /// "Controlled from" shows the overlay host accepted and hasn't answered, the peer of
        /// the latest, and whether more than one has been outstanding (an answer then can't be
        /// told from another's: the overlay events carry only the shared indicator id).
        indicator_pending: u32,
        indicator_peer: Option<NodeId>,
        indicator_ambiguous: bool,
        /// When the indicator was last hidden. The host may still have transitions of that
        /// earlier indicator queued (a `Visible` again after a move), carrying the same id, so a
        /// show accepted within `INDICATOR_SETTLE` of it is answered ambiguously too.
        pub(super) indicator_hidden_at: Option<Instant>,
        /// Speaker sessions the engine has started and not stopped, either direction.
        audio_sessions: BTreeSet<AudioKey>,
        /// The engine's side of the I/O gate: closed from a panic until the re-arm.
        pub engine_permits: bool,
        pub audio_off: bool,
        pub gpu_off: bool,
        pub discovery_off: bool,
        /// Why discovery isn't running, if it failed to start.
        pub discovery_error: Option<&'static str>,
    }

    impl Tracker {
        pub fn new() -> Tracker {
            let off = |var: &str| std::env::var(var).as_deref() == Ok("0");
            Tracker {
                counters: BTreeMap::new(),
                link_generation: BTreeMap::new(),
                settings_opened: 0,
                spawn_settings: open_settings_app,
                grants_epoch: 0,
                layout_epoch: 0,
                backends: RefCell::new(BackendsSeen::default()),
                sources: BTreeMap::new(),
                pending_sources: BTreeMap::new(),
                last_parking: BTreeMap::new(),
                proxies: BTreeSet::new(),
                last_restore: None,
                restores: BTreeMap::new(),
                last_close: None,
                #[cfg(test)]
                close_seam: None,
                established_before: None,
                parked: BTreeMap::new(),
                e1_injects: BTreeMap::new(),
                indicator_pending: 0,
                indicator_peer: None,
                indicator_ambiguous: false,
                indicator_hidden_at: None,
                audio_sessions: BTreeSet::new(),
                engine_permits: true,
                // The same switches the backends read (`platform.rs`, `start_discovery`).
                audio_off: off("CROSSPANE_AUDIO"),
                gpu_off: off("CROSSPANE_GPU"),
                discovery_off: discovery_switched_off(
                    std::env::var("CROSSPANE_DISCOVERY").ok().as_deref(),
                ),
                discovery_error: None,
            }
        }

        fn counter(&mut self, peer: NodeId) -> &mut PeerCounters {
            self.counters.entry(peer).or_default()
        }

        /// Trust or grants changed (an allow, a pairing, a forget, a revocation, a reload).
        pub fn grants_changed(&mut self) {
            self.grants_epoch = self.grants_epoch.saturating_add(1);
        }

        /// The placements or a display set changed.
        pub fn layout_changed(&mut self) {
            self.layout_epoch = self.layout_epoch.saturating_add(1);
        }

        /// A connection with `peer` was established.
        pub fn link_established(&mut self, peer: NodeId) {
            let generation = self.link_generation.entry(peer).or_insert(0);
            *generation = generation.saturating_add(1);
        }

        /// The engine handled one input: count the E1 sessions it established and ended (a
        /// handshake that is refused, times out or is released was never established, and counts
        /// nothing), and note which of its injections answer a controller's own key and button
        /// messages.
        pub fn handled(
            &mut self,
            before: Sessions,
            after: Sessions,
            from_controller: Option<NodeId>,
            outputs: &[Output],
        ) {
            // Restores and closes of an earlier input have nothing to do with the ends in this
            // one's.
            self.last_restore = None;
            self.last_close = None;
            self.established_before = before.0;
            // An established controller session starts and ends in different inputs even with
            // the same peer again (the new one is acknowledged later), so no announcement helps.
            let (ended, started) = session_edges(before.0, after.0, None);
            if let Some(peer) = ended {
                self.counter(peer).e1_controller_ended += 1;
            }
            if let Some(peer) = started {
                self.counter(peer).e1_controller_started += 1;
            }
            let admitted = outputs.iter().find_map(|output| match output {
                Output::Notice(Notice::ControlledBy(peer)) => Some(*peer),
                _ => None,
            });
            let (ended, started) = session_edges(before.1, after.1, admitted);
            if let Some(peer) = ended {
                self.counter(peer).e1_target_ended += 1;
            }
            if let Some(peer) = started {
                self.counter(peer).e1_target_started += 1;
            }
            if let Some(peer) = from_controller {
                for output in outputs {
                    if let Output::Inject {
                        id,
                        cmd: InjectCmd::Key { .. } | InjectCmd::Button { .. },
                    } = output
                    {
                        self.e1_injects.insert(*id, peer);
                    }
                }
            }
        }

        /// The outputs of that input are carried out.
        pub fn executed(&mut self) {
            self.e1_injects.clear();
        }

        /// A key or button injection was carried out; counted for the controller whose message
        /// it answers, and only if the injector reported success.
        pub fn injected(&mut self, id: InjectId, ok: bool) {
            if let Some(peer) = self.e1_injects.remove(&id)
                && ok
            {
                self.counter(peer).e1_injections_ok += 1;
            }
        }

        /// The overlay host accepted a "controlled from" show, while `peer` controlled this node.
        /// A second one before the first is answered, or one soon after the indicator was
        /// hidden, makes the next answer ambiguous: the events carry only the shared indicator
        /// id, so an answer may be an earlier indicator's, and nobody is credited for it.
        pub fn indicator_shown(&mut self, peer: Option<NodeId>, now: Instant) {
            let settling = self
                .indicator_hidden_at
                .is_some_and(|hidden| now.saturating_duration_since(hidden) < INDICATOR_SETTLE);
            if self.indicator_pending > 0 || settling {
                self.indicator_ambiguous = true;
            }
            self.indicator_pending = self.indicator_pending.saturating_add(1);
            self.indicator_peer = peer;
        }

        /// The overlay host answered for the indicator: on screen (`visible`) or not. Credited
        /// to the peer of the show only when that show was the one outstanding; an answer with
        /// none outstanding (the host says `Visible` again after a move) counts nothing.
        pub fn indicator_answer(&mut self, visible: bool) {
            let sole = self.indicator_pending == 1 && !self.indicator_ambiguous;
            if visible
                && sole
                && let Some(peer) = self.indicator_peer
            {
                self.counter(peer).e1_hud_shows += 1;
            }
            self.indicator_pending = self.indicator_pending.saturating_sub(1);
            if self.indicator_pending == 0 {
                self.indicator_ambiguous = false;
            }
        }

        /// The indicator was hidden: transitions of it may still be on their way.
        pub fn indicator_hidden(&mut self, now: Instant) {
            self.indicator_hidden_at = Some(now);
        }

        /// The engine ended a controller session by a release. Only a session this node had
        /// established is counted: releasing a handshake that was never answered isn't.
        pub fn released(&mut self, peer: NodeId, cause: ReleaseCause) {
            if self.established_before != Some(peer) {
                return;
            }
            match cause {
                ReleaseCause::Chord => self.counter(peer).e1_chord_releases += 1,
                ReleaseCause::Command => self.counter(peer).e1_command_releases += 1,
            }
        }

        /// A window of this node was parked for a projection to `peer` (`ProjectionStarted`),
        /// as `parking`. Nothing counts yet: the projection is live when its capture starts.
        pub fn projection_parked(
            &mut self,
            key: ProjectionKey,
            peer: NodeId,
            parking: ParkingKind,
        ) {
            self.pending_sources.insert(key, (peer, parking));
        }

        /// The capture of this node's projection `projection` started: it is live now, so its
        /// start and its parking kind count. (A projection that ends first, or whose capture
        /// fails, never gets here: its pending facts are dropped when it ends.)
        pub fn capture_started(&mut self, local: NodeId, projection: ProjectionId) {
            let key = ProjectionKey {
                source: local,
                projection,
            };
            let Some((peer, parking)) = self.pending_sources.remove(&key) else {
                return;
            };
            if self.sources.insert(key, peer).is_none() {
                self.counter(peer).e2_source_started += 1;
            }
            // The Mac's virtual-display parking reports `Twin` too. A kind this agent doesn't
            // know leaves the last one standing.
            let kind = match parking {
                ParkingKind::Twin => Some("twin"),
                ParkingKind::Mirror => Some("mirror"),
                _ => None,
            };
            if let Some(kind) = kind {
                self.last_parking.insert(peer, kind);
            }
        }

        /// A `CloseProxy` was carried out; `queued` says whether the proxy host was sent its
        /// `Close` (the host doesn't acknowledge it).
        pub fn proxy_closed(&mut self, queued: bool) {
            self.last_close = Some(queued);
        }

        /// A proxy of `key`'s window opened on this node.
        pub fn proxy_opened(&mut self, key: ProjectionKey) {
            if self.proxies.insert(key) {
                self.counter(key.source).e2_dest_started += 1;
            }
        }

        /// A projection ended. A projection that never went live isn't counted at all.
        ///
        /// As the source: wait for the Restore registered just before this notice. When it
        /// completes, the actual outcome counts a return or a failed return. When the window went
        /// away (`WindowClosed`) nothing was returned and nothing failed: a restore of a window
        /// that no longer exists succeeds without putting anything back.
        ///
        /// As the destination: a `Returned` end counts as returned when the proxy host was
        /// queued its `Close` just before the notice, and as a failed return when it wasn't.
        /// "Queued" is all that is known: the host doesn't say when the proxy is gone.
        pub fn projection_ended(
            &mut self,
            local: NodeId,
            key: ProjectionKey,
            reason: ProjectionEndReason,
        ) {
            if key.source == local {
                self.pending_sources.remove(&key);
                let operation = self.last_restore.take();
                if let Some(peer) = self.sources.remove(&key) {
                    match reason {
                        ProjectionEndReason::WindowClosed => {}
                        _ => {
                            if let Some(pending) =
                                operation.and_then(|id| self.restores.get_mut(&id))
                            {
                                *pending = Some(peer);
                            } else {
                                self.counter(peer).e2_returns_failed += 1;
                            }
                        }
                    }
                }
            } else if self.proxies.remove(&key) && reason == ProjectionEndReason::Returned {
                if self.last_close.take() == Some(true) {
                    self.counter(key.source).e2_dest_returned += 1;
                } else {
                    self.counter(key.source).e2_returns_failed += 1;
                }
            }
        }

        /// This node is about to park `window`: its journal entry exists from now on.
        pub fn parking_started(&mut self, window: WindowId, id: u64) {
            self.parked
                .entry(window)
                .and_modify(|previous| *previous = (*previous).max(id))
                .or_insert(id);
        }

        pub fn restore_started(&mut self, id: u64) {
            self.last_restore = Some(id);
            self.restores.insert(id, None);
        }

        /// A `Restore` of `window` was carried out; `restored` says whether the window is back.
        pub fn restored(&mut self, window: WindowId, id: u64, restored: bool) {
            if let Some(peer) = self.restores.remove(&id).flatten() {
                if restored {
                    self.counter(peer).e2_source_returned += 1;
                } else {
                    self.counter(peer).e2_returns_failed += 1;
                }
            }
            if restored && self.parked.get(&window).is_some_and(|park| *park < id) {
                self.parked.remove(&window);
            }
        }

        /// Keep the speaker sessions the engine has started.
        pub fn audio_output(&mut self, output: &Output) {
            match output {
                Output::StartAudioStream {
                    key,
                    kind: AudioKind::Speaker,
                    ..
                } => {
                    self.audio_sessions.insert(*key);
                }
                Output::StopAudioStream { key } => {
                    self.audio_sessions.remove(key);
                }
                Output::RemoveAudioPeer { peer } => self.audio_sessions.retain(|k| k.peer != *peer),
                _ => {}
            }
        }

        fn audio_peers(&self) -> BTreeSet<NodeId> {
            self.audio_sessions.iter().map(|key| key.peer).collect()
        }

        /// `recovery_pending`: this instance's parked-but-not-restored windows, that is the
        /// parking-journal entries it created and has not resolved (a restore that failed leaves
        /// its entry). Entries a previous run left behind are not counted here: whether the
        /// startup recovery cleared them is `startup_recovery`'s.
        fn recovery_pending(&self) -> u32 {
            u32::try_from(self.parked.len()).unwrap_or(u32::MAX)
        }
    }

    /// The tokens `status` uses for the permissions the installer cares about; `None` for any other.
    fn permission_token(permission: Permission) -> Option<&'static str> {
        match permission {
            Permission::ScreenRecording => Some("screen_recording"),
            Permission::Accessibility => Some("accessibility"),
            Permission::InputMonitoring => Some("input_monitoring"),
            Permission::Microphone => Some("microphone"),
            _ => None,
        }
    }

    fn state_token(state: PermissionState) -> &'static str {
        match state {
            PermissionState::Granted => "granted",
            PermissionState::NotGranted => "not_granted",
            PermissionState::Unknown => "unknown",
        }
    }

    impl Agent {
        /// The backends as they are now, in the order `status` lists them, and the epoch: how many
        /// times the list has differed from the one before it. The list is computed here, on each
        /// status request and on the housekeeping tick.
        pub(super) fn backends_now(&self) -> (Vec<Backend>, u64) {
            let now = self.backends();
            let mut seen = self.tracker.backends.borrow_mut();
            if seen.last.as_ref().is_some_and(|last| *last != now) {
                seen.epoch = seen.epoch.saturating_add(1);
            }
            seen.last = Some(now.clone());
            (now, seen.epoch)
        }

        /// What each backend is. `None` can't say why a backend is absent, so the reason is what
        /// this OS makes likely: a backend it builds that isn't there failed to construct; one
        /// it never builds is not supported; a permission this OS requires and hasn't been given
        /// blocks every backend that needs it. (The I/O gate is reported in `gate`, not here.)
        fn backends(&self) -> Vec<Backend> {
            let p = &self.platform;
            let t = &self.tracker;
            let missing: Vec<Permission> = p
                .permissions
                .required()
                .into_iter()
                .filter(|&permission| p.permissions.state(permission) != PermissionState::Granted)
                .collect();
            let blocked = |needs: &[Permission]| needs.iter().any(|n| missing.contains(n));
            // A backend this OS builds.
            let built = |name, present: bool, needs: &[Permission]| {
                if blocked(needs) {
                    down(name, "blocked", "permission")
                } else if present {
                    up(name)
                } else {
                    down(name, "failed", "construction_failed")
                }
            };
            // A backend only some OSes or compositors have.
            let optional = |name, present: bool| {
                if present {
                    up(name)
                } else {
                    down(name, "missing", "not_supported")
                }
            };
            // Where the identity came from: the OS key store, or the file.
            let keystore = match (self.startup.key_source, self.startup.force_file_keystore) {
                (KeySource::OsStore, _) => up("keystore"),
                (KeySource::File, true) => down("keystore", "missing", "disabled"),
                (KeySource::File, false) if p.keystore.is_none() => {
                    down("keystore", "failed", "construction_failed")
                }
                (KeySource::File, false) => down("keystore", "failed", "unknown"),
            };
            let home = match (&p.home, self.home.fence) {
                (None, _) => down("home", "missing", "not_supported"),
                // A leftover release bind can't be confirmed gone: the home seat isn't usable.
                (Some(_), true) => down("home", "failed", "unknown"),
                (Some(_), false) => up("home"),
            };
            let gpu = match (p.gpu.is_some(), t.gpu_off) {
                (true, _) => up("gpu"),
                (false, true) => down("gpu", "missing", "disabled"),
                (false, false) => down("gpu", "missing", "not_supported"),
            };
            let audio = if t.audio_off {
                down("audio", "missing", "disabled")
            } else {
                built("audio", self.audio.is_some(), &[Permission::Microphone])
            };
            let discovery = match (self.discovery.is_some(), t.discovery_off, t.discovery_error) {
                (true, _, _) => up("discovery"),
                (false, true, _) => down("discovery", "missing", "disabled"),
                (false, false, Some(_)) => down("discovery", "failed", "construction_failed"),
                // Not started yet.
                (false, false, None) => down("discovery", "failed", "unknown"),
            };
            use Permission::{Accessibility, InputMonitoring, ScreenRecording};
            vec![
                built(
                    "capture",
                    p.capture.is_some(),
                    &[InputMonitoring, Accessibility],
                ),
                built("keys", p.keys.is_some(), &[Accessibility]),
                built("pointer", p.pointer.is_some(), &[Accessibility]),
                built("overlay", p.overlay.is_some(), &[]),
                optional("hotkeys", p.hotkeys.is_some()),
                keystore,
                built("windows", p.windows.is_some(), &[Accessibility]),
                built(
                    "parking",
                    p.parking.is_some() || self.parking_available,
                    &[Accessibility],
                ),
                built("frames", p.frames.is_some(), &[ScreenRecording]),
                built("tray", p.tray.is_some(), &[]),
                built("links", p.links.is_some(), &[]),
                gpu,
                home,
                audio,
                discovery,
            ]
        }

        /// `result.installer`, the whole of it (the frozen schema of WP-4.5).
        pub(super) fn installer_status(&self) -> Value {
            let t = &self.tracker;
            let (backends, backends_epoch) = self.backends_now();
            let session = self.platform.session.state();
            let mut trusted: Vec<(NodeId, String, BTreeSet<Capability>)> =
                self.trust.with(|trust| {
                    trust
                        .peers()
                        .iter()
                        .map(|e| (e.node, e.name.clone(), e.granted.clone()))
                        .collect()
                });
            trusted.sort_by_key(|(node, _, _)| *node);
            let peers: Vec<Value> = trusted
                .into_iter()
                .map(|(node, name, granted)| {
                    let info = self.peers.get(&node);
                    let grants: BTreeSet<&str> = granted
                        .iter()
                        .filter_map(|c| crate::ctl::capability_name(*c))
                        .collect();
                    let c = t.counters.get(&node).copied().unwrap_or_default();
                    json!({
                        "node": node.to_string(),
                        "name": name,
                        "connected": info.is_some_and(|i| i.connected),
                        "link_generation": t.link_generation.get(&node),
                        "features": info.map(|i| i.features.clone()).unwrap_or_default(),
                        "grants_given": grants,
                        "last_source_parking": t.last_parking.get(&node),
                        "counters": {
                            "e1_controller_started": c.e1_controller_started,
                            "e1_controller_ended": c.e1_controller_ended,
                            "e1_target_started": c.e1_target_started,
                            "e1_target_ended": c.e1_target_ended,
                            "e1_injections_ok": c.e1_injections_ok,
                            "e1_hud_shows": c.e1_hud_shows,
                            "e1_chord_releases": c.e1_chord_releases,
                            "e1_command_releases": c.e1_command_releases,
                            "e2_source_started": c.e2_source_started,
                            "e2_source_returned": c.e2_source_returned,
                            "e2_dest_started": c.e2_dest_started,
                            "e2_dest_returned": c.e2_dest_returned,
                            // Null until the renderer reports (WP-4.5a), never 0.
                            "e2_frames_presented": self.proxy_ids.presented_from(node),
                            "e2_returns_failed": c.e2_returns_failed,
                        },
                    })
                })
                .collect();
            let permissions: Vec<Value> = self
                .platform
                .permissions
                .required()
                .into_iter()
                .filter_map(|permission| {
                    let name = permission_token(permission)?;
                    let state = state_token(self.platform.permissions.state(permission));
                    Some(json!({ "name": name, "state": state }))
                })
                .collect();
            let stats = self
                .audio
                .as_ref()
                .map(|audio| audio.stats())
                .unwrap_or_default();
            let mut features: Vec<&str> = Vec::new();
            if cfg!(feature = "private-vdisplay") {
                features.push("private-vdisplay");
            }
            if cfg!(feature = "video") {
                features.push("video");
            }
            let lock = match session.lock {
                LockState::Unlocked => "unlocked",
                LockState::Locked => "locked",
                LockState::Unknown => "unknown",
            };
            json!({
                "schema_version": SCHEMA_VERSION,
                "build": { "version": env!("CARGO_PKG_VERSION"), "features": features },
                "instance": {
                    "id": self.startup.instance_id,
                    "pid": self.startup.pid,
                    "uid": self.startup.uid,
                    "exe": self.startup.exe,
                    "runtime_dir": self.startup.runtime_dir,
                    "started_unix_ms": self.startup.started_unix_ms,
                },
                "config_revision": self.startup.config_revision,
                "node": self.node.to_string(),
                "recovery_pending": t.recovery_pending(),
                "startup_recovery": self.platform.startup_recovery.as_str(),
                "gate": {
                    "open": self.platform.gate.is_open(),
                    "session": lock,
                    "active": session.active,
                    "armed": self.engine.armed(),
                    "panic": !t.engine_permits,
                },
                "epochs": {
                    "gate": self.platform.gate.epoch(),
                    "grants": t.grants_epoch,
                    "layout": t.layout_epoch,
                    "backends": backends_epoch,
                },
                "backends": backends
                    .iter()
                    .map(|b| json!({ "name": b.name, "state": b.state, "reason": b.reason }))
                    .collect::<Vec<_>>(),
                "keystore": self.startup.key_source.as_str(),
                "permissions": permissions,
                "discovery": {
                    "enabled": !t.discovery_off,
                    "running": self.discovery.is_some(),
                    "candidates": u32::try_from(self.candidates.len()).unwrap_or(u32::MAX),
                    "error": t.discovery_error,
                },
                "tray": { "created": self.platform.tray.is_some() },
                "audio": {
                    "enabled": self.audio.is_some(),
                    "active_peers": t.audio_peers().iter().map(NodeId::to_string).collect::<Vec<_>>(),
                    "frames_sent": stats.sent,
                    "frames_played": stats.played,
                },
                "settings_opened": t.settings_opened,
                "peers": peers,
            })
        }
    }

    #[cfg(test)]
    mod tests {
        use crosspane_types::hid::HidUsage;

        use super::*;

        const A: NodeId = NodeId([1; 32]);
        const B: NodeId = NodeId([2; 32]);

        #[test]
        fn the_startup_facts_keep_the_revision_they_are_given_and_never_read_the_file_again() {
            let nowhere = std::path::PathBuf::from("/nonexistent/crosspane-test");
            let paths = Paths {
                config_dir: nowhere.clone(),
                state_dir: nowhere.clone(),
                runtime_dir: nowhere,
            };
            let facts = StartupFacts::collect(
                A,
                &paths,
                KeySource::OsStore,
                &Config::default(),
                "0123456789abcdef".to_owned(),
            );
            assert_eq!(facts.config_revision, "0123456789abcdef");
            assert_eq!(facts.key_source, KeySource::OsStore);
            // An agent nobody told anything has the zero revision: "no file".
            assert_eq!(StartupFacts::unknown(A).config_revision, "0000000000000000");
        }

        #[test]
        fn the_instance_id_uses_the_frozen_pid_then_u64_start_bytes() {
            use crate::lifecycle::instance_id;
            let id = instance_id(4242, 0x0102_0304_0506_0708);
            assert_eq!(
                id,
                xxhash_rust::xxh3::xxh3_64(&[0x92, 0x10, 0, 0, 8, 7, 6, 5, 4, 3, 2, 1])
            );
            assert_eq!(id, instance_id(4242, 0x0102_0304_0506_0708));
            assert_ne!(id, instance_id(4243, 0x0102_0304_0506_0708));
            assert_ne!(id, instance_id(4242, 0x0102_0304_0506_0709));
        }

        #[test]
        fn startup_uses_the_bootstrap_instance_stamp_and_keeps_the_loaded_revision() {
            let facts = StartupFacts::unknown(A).with_instance(crate::lifecycle::Instance {
                id: 123,
                pid: 4242,
                started_unix_ms: 1_790_950_000_000,
            });
            assert_eq!(facts.instance_id, 123);
            assert_eq!(facts.pid, 4242);
            assert_eq!(facts.started_unix_ms, 1_790_950_000_000);
            assert_eq!(facts.config_revision, "0000000000000000");
        }

        #[test]
        fn a_session_that_ends_and_starts_with_the_same_peer_is_both() {
            // (before, after, announced) → (ended, started)
            let cases = [
                ((None, Some(A), None), (None, Some(A))),
                ((Some(A), None, None), (Some(A), None)),
                // Nothing changed: no end, no start.
                ((Some(A), Some(A), None), (None, None)),
                ((None, None, None), (None, None)),
                // The same peer again: a new session announced while one was running.
                ((Some(A), Some(A), Some(A)), (Some(A), Some(A))),
                // Another peer replaces it.
                ((Some(A), Some(B), Some(B)), (Some(A), Some(B))),
                // Announced and gone again within the input: nothing was established.
                ((None, None, Some(A)), (None, None)),
            ];
            for ((before, after, announced), expected) in cases {
                assert_eq!(
                    session_edges(before, after, announced),
                    expected,
                    "{before:?} -> {after:?}, announced {announced:?}"
                );
            }
        }

        #[test]
        fn only_a_controllers_own_messages_are_e1_input() {
            use crosspane_protocol::projection::ProjInput;
            let key = HidUsage::keyboard(4);
            let e1 = |msg| Input::Link(LinkEvent::Input { peer: A, msg });
            let session = crosspane_types::id::SessionId(1);
            assert_eq!(
                e1_input_peer(&e1(InputMessage::Key {
                    session,
                    seq: 1,
                    usage: key,
                    down: true
                })),
                Some(A)
            );
            assert_eq!(
                e1_input_peer(&e1(InputMessage::Proj(ProjInput::Key {
                    projection: ProjectionId(1),
                    seq: 1,
                    usage: key,
                    down: true
                }))),
                None
            );
            assert_eq!(e1_input_peer(&Input::Tick), None);
        }
    }
}

pub use installer::StartupFacts;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_node_colours() {
        assert_eq!(node_accent(NodeId([0; 32])), [210, 45, 45]);
        let mut node = [0; 32];
        node[0] = 128;
        assert_eq!(node_accent(NodeId(node)), [45, 210, 210]);
    }

    fn peers() -> Vec<(NodeId, &'static str)> {
        vec![
            (NodeId([0xab; 32]), "Desk"),
            (NodeId([0xac; 32]), "laptop"),
            (NodeId([0x11; 32]), "twin"),
            (NodeId([0x12; 32]), "twin"),
        ]
    }

    #[test]
    fn peers_resolve_by_exact_name_or_unique_long_prefix() {
        let peers = peers();
        let find = |q: &str| resolve(q, peers.iter().copied());
        assert_eq!(find("desk"), Some(NodeId([0xab; 32])));
        assert_eq!(find(" Laptop "), Some(NodeId([0xac; 32])));
        assert_eq!(find("abab"), Some(NodeId([0xab; 32])));
        // Empty, too short, non-hex, ambiguous names and ambiguous prefixes match nothing.
        assert_eq!(find(""), None);
        assert_eq!(find("  "), None);
        assert_eq!(find("aba"), None);
        assert_eq!(find("lapt"), None);
        assert_eq!(find("twin"), None);
        assert_eq!(find("1111"), Some(NodeId([0x11; 32])));
        let shared = [(NodeId([0x11; 32]), "a"), (NodeId([0x11; 32]), "b")];
        assert_eq!(resolve("1111", shared.iter().copied()), None);
    }
}

/// The agent's audio routing (WP-3.6d), driven in-process: a real engine and agent loop, with a
/// recorder in place of the audio worker, so the order of the calls is what is checked.
#[cfg(test)]
mod audio_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc;

    use crosspane_engine::EngineConfig;
    use crosspane_engine::io::AudioEndpoint;
    use crosspane_input::journal::MemoryJournal;
    use crosspane_platform::{
        AudioEvent, Displays, IoGate, LockState, Permissions, SessionEvent, SessionEvents,
        SessionState,
    };
    use crosspane_protocol::link::LinkError;
    use crosspane_security::identity::DeviceIdentity;
    use crosspane_security::trust::{PeerEntry, default_grants};
    use crosspane_types::audio::AudioStreamId;

    use super::*;

    fn features(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn only_a_zero_switches_discovery_off() {
        assert!(discovery_switched_off(Some("0")));
        for other in [None, Some(""), Some("1"), Some("off"), Some("00")] {
            assert!(!discovery_switched_off(other), "{other:?}");
        }
    }

    #[test]
    fn audio_needs_the_feature_on_both_sides() {
        let audio = features(&["e1", "audio"]);
        let plain = features(&["e1", "cursor", "h264"]);
        assert!(audio_negotiated(&audio, &audio));
        assert!(!audio_negotiated(&audio, &plain));
        assert!(!audio_negotiated(&plain, &audio));
        assert!(!audio_negotiated(&plain, &plain));
        assert!(!audio_negotiated(&[], &audio));
        assert!(!audio_negotiated(&audio, &[]));
        // The name is exact.
        let near = features(&["Audio", "audio2", "audio.speaker"]);
        assert!(!audio_negotiated(&audio, &near));
    }

    #[test]
    fn a_hello_replaces_what_the_cache_knew_and_reports_changed_displays() {
        let mut info = PeerInfo::default();
        let mut hello = Hello {
            minor: crosspane_protocol::PROTOCOL_MINOR,
            name: "x".into(),
            features: features(&["e1", "audio"]),
            displays: Vec::new(),
        };
        assert!(!info.apply_hello("peer".into(), &hello));
        assert_eq!(info.name, "peer");
        assert_eq!(info.features, features(&["e1", "audio"]));
        assert!(info.connected);
        hello.features = features(&["e1"]);
        assert!(!info.apply_hello("peer".into(), &hello));
        assert_eq!(info.features, features(&["e1"]));
    }

    /// What the agent asked of the audio worker.
    #[derive(Clone, Debug, PartialEq)]
    enum Call {
        Submit(Output),
        Packet(NodeId, AudioStreamId),
        Cancel(NodeId),
        Shutdown,
    }

    impl Call {
        fn label(&self) -> &'static str {
            match self {
                Call::Submit(Output::AddAudioPeer { .. }) => "add",
                Call::Submit(Output::RemoveAudioPeer { .. }) => "remove",
                Call::Submit(Output::OpenAudioPlayback { .. }) => "open-playback",
                Call::Submit(Output::CloseAudioPlayback { .. }) => "close-playback",
                Call::Submit(Output::OpenAudioCapture { .. }) => "open-capture",
                Call::Submit(Output::CloseAudioCapture { .. }) => "close-capture",
                Call::Submit(Output::StartAudioStream {
                    endpoint: AudioEndpoint::VirtualSpeaker,
                    ..
                }) => "start-virtual-speaker",
                Call::Submit(Output::StartAudioStream {
                    endpoint: AudioEndpoint::LocalPlayback,
                    ..
                }) => "start-playback",
                Call::Submit(Output::StartAudioStream { .. }) => "start-other",
                Call::Submit(Output::StopAudioStream { .. }) => "stop",
                Call::Submit(_) => "other",
                Call::Packet(..) => "packet",
                Call::Cancel(_) => "cancel",
                Call::Shutdown => "shutdown",
            }
        }
    }

    /// Stands in for the worker: records the calls, in order.
    struct Recorder(Arc<Mutex<Vec<Call>>>);

    impl Recorder {
        fn push(&self, call: Call) {
            self.0.lock().unwrap().push(call);
        }
    }

    impl AudioPlane for Recorder {
        fn submit(&self, output: Output) {
            self.push(Call::Submit(output));
        }

        fn packet(&self, peer: NodeId, packet: AudioPacket) {
            self.push(Call::Packet(peer, packet.stream));
        }

        fn cancel_peer(&self, peer: NodeId) {
            self.push(Call::Cancel(peer));
        }

        fn stats(&self) -> WorkerStats {
            WorkerStats::default()
        }

        fn shutdown(self: Box<Self>) {
            self.push(Call::Shutdown);
        }
    }

    struct FakeSession;

    impl SessionEvents for FakeSession {
        fn state(&self) -> SessionState {
            SessionState {
                lock: LockState::Unlocked,
                active: Some(true),
            }
        }

        fn subscribe(&mut self, _: Arc<dyn EventSink<SessionEvent>>) -> Result<(), PlatformError> {
            Ok(())
        }
    }

    struct FakeDisplays;

    impl Displays for FakeDisplays {
        fn displays(&self) -> Result<Vec<DisplayInfo>, PlatformError> {
            Ok(Vec::new())
        }

        fn subscribe(
            &mut self,
            _: Arc<dyn EventSink<Vec<DisplayInfo>>>,
        ) -> Result<(), PlatformError> {
            Ok(())
        }
    }

    struct FakePermissions;

    impl Permissions for FakePermissions {
        fn required(&self) -> Vec<Permission> {
            Vec::new()
        }

        fn state(&self, _: Permission) -> PermissionState {
            PermissionState::Granted
        }

        fn request(&mut self, _: Permission) -> Result<(), PlatformError> {
            Ok(())
        }

        fn subscribe(
            &mut self,
            _: Arc<dyn EventSink<(Permission, PermissionState)>>,
        ) -> Result<(), PlatformError> {
            Ok(())
        }
    }

    /// A scratch directory, removed afterwards.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "crosspane-agent-audio-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// One agent with one trusted peer ("peer-name"), and the recorder in place of the worker.
    /// (The home tests build on it too.)
    pub(super) struct Rig {
        pub(super) agent: Agent,
        calls: Arc<Mutex<Vec<Call>>>,
        source: mpsc::Receiver<SourceCmd>,
        pub(super) local: NodeId,
        pub(super) peer: NodeId,
        /// The agent's event channel: what the backends' sinks queue.
        pub(super) events: mpsc::Receiver<Event>,
        // Kept so the channel stays open, and the directory until the end.
        _dest: mpsc::Receiver<DestCmd>,
        _dir: TempDir,
    }

    const AUDIO: &[&str] = &["e1", "cursor", "audio"];

    pub(super) fn rig(local_audio: bool) -> Rig {
        let dir = TempDir::new();
        let identity = Arc::new(DeviceIdentity::generate().unwrap());
        let peer_identity = DeviceIdentity::generate().unwrap();
        let (local, peer) = (identity.node(), peer_identity.node());
        let trust = SharedTrust::load(dir.0.join("trust.json")).unwrap();
        trust
            .update(|t| {
                t.pin(PeerEntry {
                    node: peer,
                    spki: peer_identity.spki().to_vec(),
                    name: "peer-name".into(),
                    granted: default_grants(),
                    paired_at_ms: 1,
                })
                .map_err(|e| anyhow::anyhow!("{e}"))
            })
            .unwrap();
        let mut advertised = features(&["e1", "cursor"]);
        if local_audio {
            advertised.push("audio".to_owned());
        }
        let (tx, events) = mpsc::channel();
        let hello = Hello {
            minor: crosspane_protocol::PROTOCOL_MINOR,
            name: "local".into(),
            features: advertised.clone(),
            displays: Vec::new(),
        };
        let pins: Arc<dyn crosspane_transport::PinStore> = Arc::new(trust.clone());
        let net = Net::start(0, identity.clone(), pins, hello, tx.clone()).unwrap();
        let (engine, _) = Engine::new(
            EngineConfig::new(local),
            Box::new(MemoryJournal::default()),
            Box::new(MemoryJournal::default()),
            platform::now(),
        )
        .unwrap();
        let (source_tx, source) = mpsc::channel();
        let (dest_tx, dest) = mpsc::channel();
        let platform = Platform {
            gate: IoGate::new(),
            session: Box::new(FakeSession),
            displays: Box::new(FakeDisplays),
            capture: None,
            keys: None,
            pointer: None,
            overlay: None,
            hotkeys: None,
            keystore: None,
            permissions: Box::new(FakePermissions),
            windows: None,
            parking: None,
            frames: None,
            tray: None,
            links: None,
            gpu: None,
            home: None,
            startup_recovery: crate::platform::StartupRecovery::None,
        };
        let e2 = E2Wiring {
            source_media: source_tx,
            dest_media: dest_tx,
            host: None,
            proxy_ids: ProxyIds::default(),
            events: tx,
            crossing: true,
            latency_overlay: false,
            video_mbps: None,
            identity,
            port: 0,
            revocations: crate::revocations::Issued::load(dir.0.join("revocations.json")),
        };
        let calls = Arc::new(Mutex::new(Vec::new()));
        let audio = local_audio.then(|| Box::new(Recorder(calls.clone())) as Box<dyn AudioPlane>);
        let mut agent = Agent::new(
            local,
            "local".into(),
            engine,
            platform,
            net,
            trust,
            Vec::new(),
            e2,
            advertised,
            audio,
        );
        // An unlocked, active session: the audio engine's gate opens.
        agent.feed(Input::Session(SessionEvent::State(SessionState {
            lock: LockState::Unlocked,
            active: Some(true),
        })));
        Rig {
            agent,
            calls,
            source,
            local,
            peer,
            events,
            _dest: dest,
            _dir: dir,
        }
    }

    #[test]
    fn status_bootstrap_and_exit_receipt_share_the_same_instance_id() {
        let mut rig = rig(false);
        let paths = crate::paths::Paths {
            config_dir: rig._dir.0.clone(),
            state_dir: rig._dir.0.clone(),
            runtime_dir: rig._dir.0.clone(),
        };
        let mut lifecycle = crate::lifecycle::Lifecycle::start(&paths).unwrap();
        let facts = StartupFacts::collect(
            rig.local,
            &paths,
            crate::keys::KeySource::File,
            &crate::config::Config::default(),
            "0123456789abcdef".into(),
        )
        .with_instance(lifecycle.instance);
        rig.agent.set_startup(facts);
        let response = rig.agent.on_ctl(Request::Status);
        assert!(response.ok);
        let bootstrap: Value =
            serde_json::from_slice(&std::fs::read(paths.bootstrap_file()).unwrap()).unwrap();
        assert_eq!(
            response.result["installer"]["instance"]["id"],
            bootstrap["instance_id"]
        );
        assert_eq!(
            response.result["installer"]["instance"]["pid"],
            bootstrap["pid"]
        );
        assert_eq!(
            response.result["installer"]["instance"]["started_unix_ms"],
            bootstrap["started_unix_ms"]
        );
        lifecycle
            .phase(crate::lifecycle::Phase::Ready, None)
            .unwrap();
        lifecycle.stopped(rig.agent.shutdown()).unwrap();
        let receipt: Value =
            serde_json::from_slice(&std::fs::read(paths.exit_receipt()).unwrap()).unwrap();
        assert_eq!(receipt["instance_id"], bootstrap["instance_id"]);
        assert_eq!(
            response.result["installer"]["config_revision"],
            json!("0123456789abcdef")
        );
    }

    impl Rig {
        fn hello_msg(names: &[&str]) -> Hello {
            Hello {
                minor: crosspane_protocol::PROTOCOL_MINOR,
                name: "peer-announced".into(),
                features: features(names),
                displays: Vec::new(),
            }
        }

        /// The peer's first Hello on a new link.
        fn hello(&mut self, names: &[&str]) {
            self.control(ControlMessage::Hello(Rig::hello_msg(names)));
        }

        /// A replacement connection's Hello.
        fn refresh(&mut self, names: &[&str]) {
            self.agent.on_link(LinkEvent::HelloRefresh {
                peer: self.peer,
                hello: Rig::hello_msg(names),
            });
        }

        fn close(&mut self) {
            self.agent.on_link(LinkEvent::Closed {
                peer: self.peer,
                error: LinkError::Closed,
            });
        }

        fn control(&mut self, msg: ControlMessage) {
            self.agent.on_link(LinkEvent::Control {
                peer: self.peer,
                msg,
            });
        }

        fn worker_says(&mut self, event: WorkerEvent) {
            self.agent.on_event(Event::Audio(event));
        }

        /// What the agent asked of the worker so far, as short labels.
        fn labels(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().iter().map(Call::label).collect()
        }

        /// The `PeerFeatures` the encoder was sent since the last call: (video, region, cursor).
        fn features_sent(&self) -> Vec<(bool, bool, bool)> {
            self.source
                .try_iter()
                .filter_map(|cmd| match cmd {
                    SourceCmd::PeerFeatures {
                        video,
                        region,
                        cursor,
                        ..
                    } => Some((video, region, cursor)),
                    _ => None,
                })
                .collect()
        }

        /// The stream ID this node allocates first (odd for the smaller node ID).
        fn own_stream(&self, n: u16) -> AudioStreamId {
            AudioStreamId(if self.local < self.peer { 1 } else { 2 } + 2 * n)
        }

        /// The stream ID the peer allocates first.
        fn peer_stream(&self, n: u16) -> AudioStreamId {
            AudioStreamId(if self.peer < self.local { 1 } else { 2 } + 2 * n)
        }

        /// An app on this machine starts or stops using the peer's virtual speakers.
        fn speakers_active(&mut self, active: bool) {
            self.worker_says(WorkerEvent::Platform(AudioEvent::VirtualActive {
                peer: self.peer,
                kind: AudioKind::Speaker,
                active,
            }));
        }

        /// The peer opens its `n`th speaker stream to this machine's speakers.
        fn peer_opens_speakers(&mut self, n: u16) {
            let stream = self.peer_stream(n);
            self.control(ControlMessage::AudioOpen {
                stream,
                kind: AudioKind::Speaker,
                channels: 2,
            });
        }

        fn grant(&mut self, capability: &str, allow: bool) -> Response {
            self.agent.on_ctl(Request::Allow {
                peer: "peer-name".into(),
                capability: capability.into(),
                allow,
            })
        }

        /// The key of the last `OpenAudioPlayback` the worker was asked for.
        fn opened_key(&self) -> AudioKey {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find_map(|call| match call {
                    Call::Submit(Output::OpenAudioPlayback { key }) => Some(*key),
                    _ => None,
                })
                .expect("the worker was asked to open a playback")
        }
    }

    #[test]
    fn a_hello_with_audio_on_both_sides_adds_the_peer_and_updates_the_encoder() {
        let mut rig = rig(true);
        rig.hello(&["e1", "cursor", "h264", "audio"]);
        assert_eq!(rig.labels(), ["add"]);
        assert_eq!(
            rig.calls.lock().unwrap()[0],
            Call::Submit(Output::AddAudioPeer {
                peer: rig.peer,
                // The name on the devices is the trusted name, not the one the Hello announced.
                name: "peer-name".into(),
            })
        );
        assert_eq!(rig.features_sent(), [(true, false, true)]);
    }

    #[test]
    fn no_audio_on_either_side_means_no_virtual_devices() {
        let mut peer_without = rig(true);
        peer_without.hello(&["e1", "cursor"]);
        assert!(peer_without.labels().is_empty());

        // This node without a worker never advertised `audio`, whatever the peer says.
        let mut node_without = rig(false);
        node_without.hello(AUDIO);
        assert!(node_without.labels().is_empty());
        // Audio datagrams and refreshes are harmless then.
        node_without.agent.on_link(LinkEvent::Audio {
            peer: node_without.peer,
            packet: AudioPacket {
                stream: AudioStreamId(1),
                seq: 0,
                sample_time: 0,
                opus: vec![0; 8],
            },
        });
        node_without.refresh(AUDIO);
        assert!(node_without.labels().is_empty());
        // The status says audio is off, and never lists a speaker in use.
        let status = node_without.agent.status();
        assert_eq!(status["audio"], json!({ "enabled": false }));
        assert_eq!(status["peers"][0]["speaker_in_use"], json!(false));
    }

    #[test]
    fn a_refresh_cancels_the_worker_then_tells_the_engine_then_updates_the_encoder() {
        let mut rig = rig(true);
        rig.hello(&["e1", "audio"]);
        // An app plays into the peer's speakers: the engine opens a stream, and the peer agrees.
        rig.speakers_active(true);
        let first = rig.own_stream(0);
        rig.control(ControlMessage::AudioOpened { stream: first });
        assert_eq!(rig.labels(), ["add", "start-virtual-speaker"]);
        assert_eq!(rig.features_sent(), [(false, false, false)]);

        rig.refresh(&["e1", "h264", "audio"]);
        // The worker stops first; only then does the engine end the session it was told is over.
        // The peer stays available: no remove, no second add.
        assert_eq!(
            rig.labels(),
            ["add", "start-virtual-speaker", "cancel", "stop"]
        );
        // The encoder gets the refreshed features, from the refreshed cache.
        assert_eq!(rig.features_sent(), [(true, false, false)]);

        // The demand latch is retained: nothing restarts until the app goes inactive and active
        // again, and then the stream ID continues (never reused after a replacement).
        rig.speakers_active(true);
        assert_eq!(rig.labels().len(), 4);
        rig.speakers_active(false);
        rig.speakers_active(true);
        rig.control(ControlMessage::AudioOpened {
            stream: rig.own_stream(0),
        });
        assert_eq!(rig.labels().len(), 4, "the old stream ID is not reused");
        rig.control(ControlMessage::AudioOpened {
            stream: rig.own_stream(1),
        });
        assert_eq!(
            rig.labels(),
            [
                "add",
                "start-virtual-speaker",
                "cancel",
                "stop",
                "start-virtual-speaker"
            ]
        );
    }

    #[test]
    fn a_refresh_that_changes_audio_availability_follows_the_new_features() {
        let mut dropped = rig(true);
        dropped.hello(&["e1", "audio"]);
        dropped.refresh(&["e1"]);
        assert_eq!(dropped.labels(), ["add", "cancel", "remove"]);
        // Nothing more is said while the state doesn't change.
        dropped.refresh(&["e1"]);
        assert_eq!(dropped.labels(), ["add", "cancel", "remove", "cancel"]);

        let mut gained = rig(true);
        gained.hello(&["e1"]);
        assert!(gained.labels().is_empty());
        gained.refresh(&["e1", "audio"]);
        assert_eq!(gained.labels(), ["cancel", "add"]);
    }

    #[test]
    fn a_refresh_is_not_a_reconnect() {
        let mut rig = rig(true);
        rig.hello(AUDIO);
        rig.refresh(AUDIO);
        assert_eq!(rig.labels(), ["add", "cancel"]);
        // The link is still the one the engine knows: a later close removes the peer once.
        rig.close();
        assert_eq!(rig.labels(), ["add", "cancel", "cancel", "remove"]);
    }

    #[test]
    fn closing_the_link_cancels_the_worker_before_the_engine_removes_the_peer() {
        let mut rig = rig(true);
        rig.hello(AUDIO);
        rig.speakers_active(true);
        rig.control(ControlMessage::AudioOpened {
            stream: rig.own_stream(0),
        });
        rig.close();
        assert_eq!(
            rig.labels(),
            ["add", "start-virtual-speaker", "cancel", "stop", "remove"]
        );
        // A new link negotiates audio again, from scratch.
        rig.hello(AUDIO);
        assert_eq!(
            rig.labels(),
            [
                "add",
                "start-virtual-speaker",
                "cancel",
                "stop",
                "remove",
                "add"
            ]
        );
    }

    #[test]
    fn audio_datagrams_go_to_the_worker() {
        let mut rig = rig(true);
        rig.hello(AUDIO);
        rig.agent.on_link(LinkEvent::Audio {
            peer: rig.peer,
            packet: AudioPacket {
                stream: AudioStreamId(7),
                seq: 3,
                sample_time: 480 * 3,
                opus: vec![1; 8],
            },
        });
        assert_eq!(
            rig.calls.lock().unwrap().last(),
            Some(&Call::Packet(rig.peer, AudioStreamId(7)))
        );
    }

    #[test]
    fn worker_reports_reach_the_engine_and_the_indicators_the_tray_and_status() {
        let mut rig = rig(true);
        rig.hello(AUDIO);
        assert!(rig.grant("speaker", true).ok);
        rig.peer_opens_speakers(0);
        assert_eq!(rig.labels(), ["add", "open-playback"]);
        // The engine's indicator and notice name the peer.
        assert_eq!(rig.agent.speakers.len(), 1);
        let view = rig.agent.tray_view();
        assert_eq!(view.speakers, ["peer-name"]);
        let status = rig.agent.status();
        assert_eq!(status["peers"][0]["speaker_in_use"], json!(true));
        assert_eq!(status["audio"]["speakers_in_use"], json!(["peer-name"]));
        assert!(
            rig.agent
                .notices
                .iter()
                .any(|n| n == "peer-name is playing sound on these speakers")
        );

        // The device opened: the stream starts on the playback.
        let key = rig.opened_key();
        rig.worker_says(WorkerEvent::DeviceOpened {
            key,
            kind: AudioKind::Speaker,
            result: Ok(()),
        });
        assert_eq!(rig.labels(), ["add", "open-playback", "start-playback"]);

        // The stream fails: the engine ends it, and the indicator clears.
        rig.worker_says(WorkerEvent::StreamFailed { key });
        assert_eq!(
            rig.labels(),
            [
                "add",
                "open-playback",
                "start-playback",
                "stop",
                "close-playback"
            ]
        );
        assert!(rig.agent.speakers.is_empty());
        assert!(rig.agent.tray_view().speakers.is_empty());
        assert_eq!(
            rig.agent.status()["peers"][0]["speaker_in_use"],
            json!(false)
        );
    }

    #[test]
    fn a_device_that_does_not_open_ends_the_session() {
        let mut rig = rig(true);
        rig.hello(AUDIO);
        assert!(rig.grant("speaker", true).ok);
        rig.peer_opens_speakers(0);
        let key = rig.opened_key();
        rig.worker_says(WorkerEvent::DeviceOpened {
            key,
            kind: AudioKind::Speaker,
            result: Err(Failure::Other),
        });
        assert_eq!(
            rig.labels(),
            ["add", "open-playback", "stop", "close-playback"]
        );
        assert!(rig.agent.speakers.is_empty());
    }

    #[test]
    fn a_granted_incoming_microphone_request_opens_nothing() {
        let mut rig = rig(true);
        rig.hello(AUDIO);
        // Both grants are stored; only the speakers can be served.
        assert!(rig.grant("speaker", true).ok);
        assert!(rig.grant("mic", true).ok);
        let stream = rig.peer_stream(0);
        rig.control(ControlMessage::AudioOpen {
            stream,
            kind: AudioKind::Microphone,
            channels: 1,
        });
        // No capture is opened, started or closed, no stream starts, and no indicator is listed.
        assert_eq!(rig.labels(), ["add"]);
        assert!(rig.agent.speakers.is_empty());
        let notices: Vec<&String> = rig.agent.notices.iter().collect();
        assert!(
            notices
                .iter()
                .any(|n| n.contains("Microphone") && n.contains("refused")),
            "{notices:?}"
        );
        assert!(
            !notices.iter().any(|n| n.contains("using this microphone")),
            "no microphone is in use: {notices:?}"
        );
        // The refused request used its ID up, but speakers still work with the next one.
        rig.peer_opens_speakers(1);
        assert_eq!(rig.labels(), ["add", "open-playback"]);
    }

    #[test]
    fn an_app_recording_from_the_virtual_microphone_is_refused_locally() {
        let mut rig = rig(true);
        rig.hello(AUDIO);
        assert!(rig.grant("mic", true).ok);
        rig.worker_says(WorkerEvent::Platform(AudioEvent::VirtualActive {
            peer: rig.peer,
            kind: AudioKind::Microphone,
            active: true,
        }));
        // Nothing is asked of the worker (no virtual microphone is fed, no capture opened), and
        // the user is told.
        assert_eq!(rig.labels(), ["add"]);
        assert!(
            rig.agent
                .notices
                .iter()
                .any(|n| n.contains("Microphone") && n.contains("refused")),
            "{:?}",
            rig.agent.notices
        );
    }

    #[test]
    fn a_speaker_session_is_refused_without_the_grant() {
        let mut rig = rig(true);
        rig.hello(AUDIO);
        rig.peer_opens_speakers(0);
        assert_eq!(rig.labels(), ["add"]);
        assert!(rig.agent.speakers.is_empty());
    }

    #[test]
    fn grants_are_sent_to_the_engine_again_after_a_hello_and_after_a_refresh() {
        let mut rig = rig(true);
        rig.hello(AUDIO);
        // The grant changes behind the agent's back (the trust file was edited): the engine only
        // learns it from a fresh `Grants` input.
        let peer = rig.peer;
        rig.agent
            .trust
            .update(|t| {
                t.set_grant(peer, Capability::AudioSpeaker, true)
                    .map_err(|e| anyhow::anyhow!("{e}"))
            })
            .unwrap();
        rig.peer_opens_speakers(0);
        assert_eq!(
            rig.labels(),
            ["add"],
            "refused: the engine has no grant yet"
        );

        // A refresh re-sends the grants.
        rig.refresh(AUDIO);
        rig.peer_opens_speakers(1);
        assert_eq!(rig.labels(), ["add", "cancel", "open-playback"]);

        // So does an ordinary Hello on a new link (the grant is withdrawn behind its back).
        rig.agent
            .trust
            .update(|t| {
                t.set_grant(peer, Capability::AudioSpeaker, false)
                    .map_err(|e| anyhow::anyhow!("{e}"))
            })
            .unwrap();
        rig.close();
        rig.hello(AUDIO);
        let before = rig.labels().len();
        rig.peer_opens_speakers(2);
        assert_eq!(rig.labels().len(), before, "refused again: no grant");
    }

    #[test]
    fn allow_speaker_and_mic_set_the_audio_grants() {
        let mut rig = rig(true);
        rig.hello(AUDIO);
        let granted = |rig: &Rig| -> Vec<Capability> {
            rig.agent
                .trust
                .with(|t| t.get(rig.peer).unwrap().granted.iter().copied().collect())
        };
        let response = rig.grant("speaker", true);
        assert!(response.ok, "{response:?}");
        assert!(granted(&rig).contains(&Capability::AudioSpeaker));
        let status = rig.agent.status();
        assert!(
            status["peers"][0]["grants"]
                .as_array()
                .unwrap()
                .contains(&json!("speaker"))
        );

        // The microphone grant is stored, and the answer says it does nothing yet.
        let response = rig.grant("mic", true);
        assert!(response.ok);
        assert!(
            response
                .result
                .as_str()
                .unwrap()
                .contains("not supported yet"),
            "{response:?}"
        );
        assert!(granted(&rig).contains(&Capability::AudioMic));
        assert!(
            rig.agent.status()["peers"][0]["grants"]
                .as_array()
                .unwrap()
                .contains(&json!("mic"))
        );

        let response = rig.grant("speaker", false);
        assert!(response.ok);
        assert!(!granted(&rig).contains(&Capability::AudioSpeaker));

        let response = rig.grant("sound", true);
        assert!(!response.ok);
        assert!(response.error.unwrap().contains("speaker"));
    }

    #[test]
    fn stopping_ends_the_audio_sessions_first_and_then_the_worker() {
        let mut rig = rig(true);
        rig.hello(AUDIO);
        rig.speakers_active(true);
        rig.control(ControlMessage::AudioOpened {
            stream: rig.own_stream(0),
        });
        rig.agent.shutdown();
        // The panic stops the stream (the worker carries it out); then the worker is shut down.
        assert_eq!(
            rig.labels(),
            ["add", "start-virtual-speaker", "stop", "shutdown"]
        );
    }

    #[test]
    fn a_node_without_a_worker_fails_what_the_engine_waits_for() {
        // Not reachable through the Hello rule; this is the guard if it ever were.
        let mut rig = rig(false);
        let key = AudioKey {
            peer: rig.peer,
            stream: AudioStreamId(1),
            generation: 1,
        };
        rig.agent.execute(vec![
            Output::OpenAudioPlayback { key },
            Output::OpenAudioCapture { key },
            Output::StartAudioStream {
                key,
                kind: AudioKind::Speaker,
                endpoint: AudioEndpoint::VirtualSpeaker,
            },
        ]);
        let pending: Vec<Input> = rig.agent.pending.iter().cloned().collect();
        assert_eq!(
            pending,
            [
                Input::AudioDeviceOpened {
                    key,
                    kind: AudioKind::Speaker,
                    result: Err(Failure::Other),
                },
                Input::AudioDeviceOpened {
                    key,
                    kind: AudioKind::Microphone,
                    result: Err(Failure::Other),
                },
                Input::AudioStreamFailed { key },
            ]
        );
    }
}

/// Home on the twin, driven in-process (WP-2.43e): a real engine and agent loop, with fakes for
/// the compositor's bind and cursor, the capture backend, the injectors and the overlay host, so
/// what is checked is what the agent asks of each and the order in which it answers the engine.
#[cfg(test)]
mod home_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Mutex;

    use crosspane_platform::{
        CaptureAbort, CaptureId, CapturePortal, CaptureStart, InputCapture, IoGate, KeyInjector,
        Overlay, OverlayAnchor, OverlayHost, OverlayId, PointerInjector, PortalId, Rgb8,
    };
    use crosspane_types::color::ColorSpace;
    use crosspane_types::geom::{DisplayGeometry, PointLogical, RectLogical, SizeLogical, SizeMm};
    use crosspane_types::hid::{HidUsage, MouseButton};
    use crosspane_types::input::{LockKeys, ScrollDelta, ScrollPhase};
    use crosspane_types::time::MonoTime;

    use super::audio_tests::{Rig, rig};
    use super::*;
    use crate::platform::HomeSeat;

    const KEYS: &str = "CTRL + SHIFT + ALT + Escape";
    const TARGET: (DisplayId, PointDevice) = (DisplayId(7), PointDevice::new(100.0, 50.0));

    // ---- fakes ----

    /// The compositor as home sees it: our bind, a foreign binding on the same chord, the pointer.
    #[derive(Default)]
    struct Compositor {
        ours: bool,
        foreign: bool,
        install_error: Option<String>,
        partial_install: bool,
        verify_error: bool,
        remove_error: Option<String>,
        installs: u32,
        removes: u32,
        checks: u32,
        cursor_reads: u32,
        display_reads: u32,
        physical: Vec<DisplayInfo>,
        restores: u32,
        recoveries: u32,
        /// What the read-back sees; `None`: the read-back fails.
        cursor: Option<(DisplayId, PointDevice)>,
        /// Runs once, during the read-back (a lock arriving in the middle of a warp).
        during_cursor: Option<Box<dyn FnOnce() + Send>>,
        reload: Option<Box<dyn Fn() + Send>>,
    }

    type Shared = Arc<Mutex<Compositor>>;

    /// A `HomeSeat` that models the module's ownership table: it never removes a foreign binding.
    struct FakeHome(Shared);

    impl HomeSeat for FakeHome {
        fn keys(&self) -> String {
            KEYS.to_owned()
        }

        fn install(&self) -> Result<(), PlatformError> {
            let mut c = self.0.lock().unwrap();
            c.installs += 1;
            if c.verify_error {
                return Err(PlatformError::Backend("ownership read ambiguous".into()));
            }
            if let Some(e) = c.install_error.clone() {
                c.ours |= c.partial_install;
                return Err(PlatformError::Backend(e));
            }
            if c.foreign {
                return Err(PlatformError::Backend(format!(
                    "home bind: another binding already uses {KEYS}"
                )));
            }
            c.ours = true;
            Ok(())
        }

        fn remove(&self) -> Result<(), PlatformError> {
            let mut c = self.0.lock().unwrap();
            c.removes += 1;
            if c.verify_error {
                return Err(PlatformError::Backend("ownership read ambiguous".into()));
            }
            if let Some(e) = &c.remove_error {
                return Err(PlatformError::Backend(e.clone()));
            }
            if c.ours && c.foreign {
                return Err(PlatformError::Backend(format!(
                    "home bind: another binding shares {KEYS} with ours; not removing it"
                )));
            }
            // Foreign only: absent, and left alone.
            c.ours = false;
            Ok(())
        }

        fn installed(&self) -> Result<bool, PlatformError> {
            let mut c = self.0.lock().unwrap();
            c.checks += 1;
            if c.verify_error {
                return Err(PlatformError::Backend("ownership read ambiguous".into()));
            }
            Ok(c.ours && !c.foreign)
        }

        fn cursor(&self) -> Result<(DisplayId, PointDevice), PlatformError> {
            let during = {
                let mut c = self.0.lock().unwrap();
                c.cursor_reads += 1;
                c.during_cursor.take()
            };
            if let Some(f) = during {
                f();
            }
            self.0
                .lock()
                .unwrap()
                .cursor
                .ok_or_else(|| PlatformError::Backend("no cursor".into()))
        }

        fn watch_reload(&mut self, reload: Box<dyn Fn() + Send>) -> Result<(), PlatformError> {
            self.0.lock().unwrap().reload = Some(reload);
            Ok(())
        }
    }

    struct HomeDisplays(Shared);
    impl crosspane_platform::Displays for HomeDisplays {
        fn displays(&self) -> Result<Vec<DisplayInfo>, PlatformError> {
            let mut c = self.0.lock().unwrap();
            c.display_reads += 1;
            Ok(c.physical.clone())
        }
        fn subscribe(
            &mut self,
            _sink: Arc<dyn EventSink<Vec<DisplayInfo>>>,
        ) -> Result<(), PlatformError> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct CaptureLog {
        sink: Option<Arc<dyn EventSink<CaptureEvent>>>,
        /// Emitted through the sink from inside `begin`, in order, before it returns.
        during_begin: Vec<CaptureEvent>,
        lifecycle: bool,
        active: Option<CaptureId>,
        warp_cursor: Option<Shared>,
        end_on_set_error: bool,
        start: Option<CaptureStart>,
        ends: Vec<Option<(DisplayId, PointDevice)>>,
        during_end: Option<Box<dyn FnOnce() + Send>>,
        end_error: Option<fn() -> PlatformError>,
        sets: Vec<Vec<u32>>,
        set_error: Option<fn() -> PlatformError>,
    }

    struct FakeCapture(Arc<Mutex<CaptureLog>>);

    struct NoAbort;

    impl CaptureAbort for NoAbort {
        fn abort(&self) {}
    }

    impl InputCapture for FakeCapture {
        fn set_portals(&mut self, portals: &[CapturePortal]) -> Result<(), PlatformError> {
            let mut log = self.0.lock().unwrap();
            log.sets.push(portals.iter().map(|p| p.id.0).collect());
            match log.set_error {
                Some(error) => {
                    if log.end_on_set_error
                        && let (Some(id), Some(sink)) = (log.active.take(), &log.sink)
                    {
                        sink.send(CaptureEvent::Ended {
                            id,
                            reason: crosspane_platform::EndReason::Aborted,
                        });
                    }
                    Err(error())
                }
                None => Ok(()),
            }
        }

        fn subscribe(
            &mut self,
            sink: Arc<dyn EventSink<CaptureEvent>>,
        ) -> Result<(), PlatformError> {
            self.0.lock().unwrap().sink = Some(sink);
            Ok(())
        }

        fn begin(
            &mut self,
            id: CaptureId,
            _portal: PortalId,
        ) -> Result<CaptureStart, PlatformError> {
            let (sink, mut events, start, lifecycle) = {
                let mut log = self.0.lock().unwrap();
                if log.lifecycle {
                    log.active = Some(id);
                }
                (
                    log.sink.clone(),
                    log.during_begin.clone(),
                    log.start.clone(),
                    log.lifecycle,
                )
            };
            if lifecycle {
                events.retain(|e| !matches!(e, CaptureEvent::Started { .. }));
                events.insert(0, CaptureEvent::Started { id });
            }
            if let Some(sink) = sink {
                for event in events {
                    sink.send(event);
                }
            }
            Ok(start.unwrap_or(CaptureStart {
                held_keys: Vec::new(),
                lock_keys: LockKeys::default(),
            }))
        }

        fn end(&mut self, warp_to: Option<(DisplayId, PointDevice)>) -> Result<(), PlatformError> {
            let (hook, error) = {
                let mut log = self.0.lock().unwrap();
                log.ends.push(warp_to);
                if let (Some(cursor), Some(to)) = (&log.warp_cursor, warp_to) {
                    cursor.lock().unwrap().cursor = Some(to);
                }
                if log.lifecycle
                    && let (Some(id), Some(sink)) = (log.active.take(), &log.sink)
                {
                    sink.send(CaptureEvent::Ended {
                        id,
                        reason: crosspane_platform::EndReason::Requested,
                    });
                }
                (log.during_end.take(), log.end_error)
            };
            if let Some(f) = hook {
                f();
            }
            match error {
                Some(error) => Err(error()),
                None => Ok(()),
            }
        }

        fn abort_handle(&self) -> Arc<dyn CaptureAbort> {
            Arc::new(NoAbort)
        }

        fn set_monitor_local_activity(&mut self, _on: bool) -> Result<(), PlatformError> {
            Ok(())
        }
    }

    /// Records what the injectors were asked: "key down", "key up", "button down", ...
    type Injected = Arc<Mutex<Vec<&'static str>>>;

    struct FakeKeys(Injected, Arc<Mutex<BTreeSet<HidUsage>>>);
    struct FakePointer(Injected);

    impl KeyInjector for FakeKeys {
        fn key(&mut self, usage: HidUsage, down: bool) -> Result<(), PlatformError> {
            if down {
                self.1.lock().unwrap().insert(usage);
            } else {
                self.1.lock().unwrap().remove(&usage);
            }
            self.0
                .lock()
                .unwrap()
                .push(if down { "key down" } else { "key up" });
            Ok(())
        }

        fn lock_keys(&self) -> Result<LockKeys, PlatformError> {
            Ok(LockKeys::default())
        }

        fn set_lock_keys(&mut self, _wanted: LockKeys) -> Result<(), PlatformError> {
            self.0.lock().unwrap().push("lock keys");
            Ok(())
        }

        fn release_all(&mut self) -> Result<(), PlatformError> {
            self.1.lock().unwrap().clear();
            self.0.lock().unwrap().push("release all keys");
            Ok(())
        }

        fn recover_keys(&mut self, _keys: &[HidUsage]) -> Result<(), PlatformError> {
            self.1.lock().unwrap().clear();
            self.0.lock().unwrap().push("recover keys");
            Ok(())
        }
    }

    impl PointerInjector for FakePointer {
        fn move_to(&mut self, _display: DisplayId, _at: PointDevice) -> Result<(), PlatformError> {
            self.0.lock().unwrap().push("move");
            Ok(())
        }

        fn button(&mut self, _button: MouseButton, down: bool) -> Result<(), PlatformError> {
            self.0
                .lock()
                .unwrap()
                .push(if down { "button down" } else { "button up" });
            Ok(())
        }

        fn scroll(&mut self, _delta: ScrollDelta) -> Result<(), PlatformError> {
            self.0.lock().unwrap().push("scroll");
            Ok(())
        }

        fn release_all(&mut self) -> Result<(), PlatformError> {
            self.0.lock().unwrap().push("release all buttons");
            Ok(())
        }

        fn recover_buttons(&mut self, _buttons: &[MouseButton]) -> Result<(), PlatformError> {
            self.0.lock().unwrap().push("recover buttons");
            Ok(())
        }
    }

    /// An overlay host that refuses every `show` outright and counts them.
    struct RefusingOverlay(Arc<Mutex<u32>>);

    impl OverlayHost for RefusingOverlay {
        fn subscribe(
            &mut self,
            _sink: Arc<dyn EventSink<crosspane_platform::OverlayEvent>>,
        ) -> Result<(), PlatformError> {
            Ok(())
        }

        fn show(&mut self, _id: OverlayId, _overlay: &Overlay) -> Result<(), PlatformError> {
            *self.0.lock().unwrap() += 1;
            Err(PlatformError::NotFound)
        }

        fn hide(&mut self, _id: OverlayId) -> Result<(), PlatformError> {
            Ok(())
        }
    }

    struct FakeParking;
    impl crosspane_platform::WindowParking for FakeParking {
        fn park(
            &mut self,
            window: WindowId,
            size: PixelSize,
            _scale: f64,
        ) -> Result<crosspane_platform::Parked, PlatformError> {
            Ok(crosspane_platform::Parked {
                window,
                kind: crosspane_platform::ParkingKind::Twin,
                display: DisplayId(7),
                content: crosspane_types::geom::PixelRect::new(
                    crosspane_types::geom::euclid::Point2D::new(0, 0),
                    crosspane_types::geom::euclid::Point2D::new(
                        size.width as i32,
                        size.height as i32,
                    ),
                ),
            })
        }
        fn resize(
            &mut self,
            window: WindowId,
            size: PixelSize,
            scale: f64,
        ) -> Result<crosspane_platform::Parked, PlatformError> {
            self.park(window, size, scale)
        }
        fn geometry(&self, _window: WindowId) -> Result<crosspane_platform::Parked, PlatformError> {
            Err(PlatformError::NotFound)
        }
        fn restore(&mut self, _window: WindowId) -> Result<(), PlatformError> {
            Ok(())
        }
        fn recover(&mut self) -> Result<Vec<WindowId>, PlatformError> {
            Ok(Vec::new())
        }
    }

    /// The existing parking fixture, with an explicit shutdown recovery result.
    struct ShutdownParking(Result<Vec<WindowId>, PlatformError>);

    impl crosspane_platform::WindowParking for ShutdownParking {
        fn park(
            &mut self,
            window: WindowId,
            size: PixelSize,
            scale: f64,
        ) -> Result<crosspane_platform::Parked, PlatformError> {
            crosspane_platform::WindowParking::park(&mut FakeParking, window, size, scale)
        }
        fn resize(
            &mut self,
            window: WindowId,
            size: PixelSize,
            scale: f64,
        ) -> Result<crosspane_platform::Parked, PlatformError> {
            self.park(window, size, scale)
        }
        fn geometry(&self, window: WindowId) -> Result<crosspane_platform::Parked, PlatformError> {
            crosspane_platform::WindowParking::geometry(&FakeParking, window)
        }
        fn restore(&mut self, window: WindowId) -> Result<(), PlatformError> {
            crosspane_platform::WindowParking::restore(&mut FakeParking, window)
        }
        fn recover(&mut self) -> Result<Vec<WindowId>, PlatformError> {
            std::mem::replace(&mut self.0, Ok(Vec::new()))
        }
    }

    #[test]
    fn shutdown_returns_parking_outcomes_and_failed_recovery_is_unclean() {
        use crate::lifecycle::{Lifecycle, Parking, Phase};
        use crosspane_input::journal::FileJournal;
        for (i, recovered, expected) in [
            (0, Ok(Vec::new()), Parking::NothingParked),
            (1, Ok(vec![WindowId(1)]), Parking::Restored),
            (
                2,
                Err(PlatformError::Backend("fixture: recovery failed".into())),
                Parking::Failed,
            ),
        ] {
            let dir =
                std::env::temp_dir().join(format!("crosspane-shutdown-{}-{i}", std::process::id()));
            crate::paths::create_private_dir(&dir).unwrap();
            let paths = crate::paths::Paths {
                config_dir: dir.clone(),
                state_dir: dir.clone(),
                runtime_dir: dir.clone(),
            };
            FileJournal::open(&paths.journal_file()).unwrap();
            FileJournal::open(&paths.e2_journal_file()).unwrap();
            let mut lifecycle = Lifecycle::start(&paths).unwrap();
            lifecycle.phase(Phase::Ready, None).unwrap();
            let mut rig = rig(false);
            rig.agent.set_lifecycle_paths(paths.clone());
            rig.agent.platform.parking = Some(Box::new(ShutdownParking(recovered)));
            let outcomes = rig.agent.shutdown();
            assert_eq!(outcomes.parking, expected);
            assert!(outcomes.input_journals_empty && outcomes.audio_stopped);
            lifecycle.stopped(outcomes).unwrap();
            let receipt: Value =
                serde_json::from_slice(&std::fs::read(paths.exit_receipt()).unwrap()).unwrap();
            assert_eq!(receipt["clean"], json!(expected != Parking::Failed));
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    /// This refusal leaves both release-all (E1) and individual ups (E2) unconfirmed.
    struct RefuseShutdownReleases;

    impl KeyInjector for RefuseShutdownReleases {
        fn key(&mut self, _usage: HidUsage, _down: bool) -> Result<(), PlatformError> {
            Err(PlatformError::Backend("fixture: release failed".into()))
        }
        fn lock_keys(&self) -> Result<LockKeys, PlatformError> {
            Ok(LockKeys::default())
        }
        fn set_lock_keys(&mut self, _wanted: LockKeys) -> Result<(), PlatformError> {
            Ok(())
        }
        fn release_all(&mut self) -> Result<(), PlatformError> {
            Err(PlatformError::Backend("fixture: release failed".into()))
        }
        fn recover_keys(&mut self, _keys: &[HidUsage]) -> Result<(), PlatformError> {
            Err(PlatformError::Backend("fixture: release failed".into()))
        }
    }

    fn check_shutdown_journals(releases_ok: bool) {
        use crate::lifecycle::{Lifecycle, Phase};
        use crosspane_input::Held;
        use crosspane_input::journal::{FileJournal, Journal};
        for e2 in [false, true] {
            let dir = std::env::temp_dir().join(format!(
                "crosspane-shutdown-journals-{}-{releases_ok}-{e2}",
                std::process::id()
            ));
            crate::paths::create_private_dir(&dir).unwrap();
            let paths = crate::paths::Paths {
                config_dir: dir.clone(),
                state_dir: dir.clone(),
                runtime_dir: dir.clone(),
            };
            let mut h = bare_scenario();
            // Keep the scenario's fake platform and initial inputs, but give the real engine
            // the same persistent journals production startup owns.
            let setup: Vec<_> = h
                .rig
                .agent
                .fed
                .iter()
                .filter(|input| {
                    matches!(
                        input,
                        Input::Session(_)
                            | Input::LocalDisplays(_)
                            | Input::PeerDisplays { .. }
                            | Input::Layout(_)
                            | Input::Grants(_)
                            | Input::PeerUp { .. }
                            | Input::Windows(_)
                    )
                })
                .cloned()
                .collect();
            let (engine, startup) = Engine::new(
                crosspane_engine::EngineConfig::new(h.rig.local),
                Box::new(FileJournal::open(&paths.journal_file()).unwrap()),
                Box::new(FileJournal::open(&paths.e2_journal_file()).unwrap()),
                ms(0),
            )
            .unwrap();
            h.rig.agent.engine = engine;
            h.rig.agent.execute(startup);
            process_events(&mut h);
            for input in setup {
                step(&mut h, input);
            }
            h.rig.agent.set_lifecycle_paths(paths.clone());
            let mut lifecycle = Lifecycle::start(&paths).unwrap();
            lifecycle.phase(Phase::Ready, None).unwrap();
            if e2 {
                project(&mut h, 1);
                step(
                    &mut h,
                    Input::Windows(WindowEvent::Focused(Some(WindowId(10)))),
                );
                proj_key(&mut h, 1, true);
            } else {
                h.rig.agent.platform.overlay = Some(Box::new(AcceptingOverlay));
                let peer = h.rig.peer;
                let session = crosspane_types::id::SessionId(77);
                start_control(&mut h, peer, session);
                from_controller(&mut h, session, key_msg(4, true, 1));
            }
            let journal_path = if e2 {
                paths.e2_journal_file()
            } else {
                paths.journal_file()
            };
            let held = vec![Held::Key(HidUsage::keyboard(4))];
            assert_eq!(
                FileJournal::open(&journal_path).unwrap().held().unwrap(),
                held,
                "e2={e2}"
            );
            assert!(!crate::lifecycle::journals_empty(&paths));
            if !releases_ok {
                h.rig.agent.platform.keys = Some(Box::new(RefuseShutdownReleases));
            }
            let outcomes = h.rig.agent.shutdown();
            assert!(h.rig.agent.pending.is_empty());
            assert_eq!(outcomes.input_journals_empty, releases_ok);
            assert_eq!(
                FileJournal::open(&journal_path).unwrap().held().unwrap(),
                if releases_ok { Vec::new() } else { held }
            );
            lifecycle.stopped(outcomes).unwrap();
            let receipt: Value =
                serde_json::from_slice(&std::fs::read(paths.exit_receipt()).unwrap()).unwrap();
            assert_eq!(receipt["clean"], json!(releases_ok));
            assert_eq!(receipt["input_journals_empty"], json!(releases_ok));
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn shutdown_settles_e1_and_e2_releases_before_writing_a_clean_receipt() {
        check_shutdown_journals(true);
    }

    #[test]
    fn shutdown_keeps_failed_e1_and_e2_releases_in_an_unclean_receipt() {
        check_shutdown_journals(false);
    }

    /// Models an undo that removes the twin but cannot persist its journal, followed by a
    /// successful final journal recovery that returns the cursor to a physical output.
    struct RestoreFailureParking(Shared);
    impl crosspane_platform::WindowParking for RestoreFailureParking {
        fn park(
            &mut self,
            window: WindowId,
            size: PixelSize,
            scale: f64,
        ) -> Result<crosspane_platform::Parked, PlatformError> {
            crosspane_platform::WindowParking::park(&mut FakeParking, window, size, scale)
        }
        fn resize(
            &mut self,
            window: WindowId,
            size: PixelSize,
            scale: f64,
        ) -> Result<crosspane_platform::Parked, PlatformError> {
            self.park(window, size, scale)
        }
        fn geometry(&self, _window: WindowId) -> Result<crosspane_platform::Parked, PlatformError> {
            Err(PlatformError::NotFound)
        }
        fn restore(&mut self, _window: WindowId) -> Result<(), PlatformError> {
            self.0.lock().unwrap().restores += 1;
            Err(PlatformError::Backend(
                "journal save after undo failed".into(),
            ))
        }
        fn recover(&mut self) -> Result<Vec<WindowId>, PlatformError> {
            let mut c = self.0.lock().unwrap();
            c.recoveries += 1;
            c.cursor = Some((DisplayId(1), PointDevice::new(500.0, 500.0)));
            Ok(vec![WindowId(10)])
        }
    }

    struct FakeFrames;
    impl crosspane_platform::FrameCapture for FakeFrames {
        fn start(
            &mut self,
            _target: CaptureTarget,
            _crop: Option<crosspane_types::geom::PixelRect>,
            _max_fps: u32,
            _sink: Arc<dyn EventSink<FrameEvent>>,
        ) -> Result<StreamId, PlatformError> {
            Ok(StreamId(101))
        }
        fn set_crop(
            &mut self,
            _stream: StreamId,
            _crop: Option<crosspane_types::geom::PixelRect>,
        ) -> Result<(), PlatformError> {
            Ok(())
        }
        fn stop(&mut self, _stream: StreamId) -> Result<(), PlatformError> {
            Ok(())
        }
    }

    // ---- the rig ----

    struct Home {
        rig: Rig,
        compositor: Shared,
        capture: Arc<Mutex<CaptureLog>>,
        injected: Injected,
        held: Arc<Mutex<BTreeSet<HidUsage>>>,
        gate: Arc<IoGate>,
    }

    /// An agent with the fake compositor, capture backend and injectors, an open gate (the session
    /// side is opened here: the real session backend does that), and nothing fed yet.
    fn home() -> Home {
        let mut rig = rig(false);
        let compositor = Shared::default();
        let capture = Arc::new(Mutex::new(CaptureLog::default()));
        let injected = Injected::default();
        let held = Arc::new(Mutex::new(BTreeSet::new()));
        let platform = &mut rig.agent.platform;
        platform.home = Some(Box::new(FakeHome(compositor.clone())));
        platform.displays = Box::new(HomeDisplays(compositor.clone()));
        platform.capture = Some(Box::new(FakeCapture(capture.clone())));
        platform.keys = Some(Box::new(FakeKeys(injected.clone(), held.clone())));
        platform.pointer = Some(Box::new(FakePointer(injected.clone())));
        // The capture's sink queues on the agent's channel, as `subscribe_platform` wires it.
        let tx = rig.agent.events.clone();
        rig.agent
            .platform
            .capture
            .as_mut()
            .unwrap()
            .subscribe(Arc::new(move |event: CaptureEvent| {
                let _ = tx.send(Event::Input(Input::Capture(event)));
            }))
            .unwrap();
        let gate = rig.agent.platform.gate.clone();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        rig.agent.fed.clear();
        Home {
            rig,
            compositor,
            capture,
            injected,
            held,
            gate,
        }
    }

    fn at() -> MonoTime {
        MonoTime::from_nanos(1)
    }

    /// What a capture's activation emits from inside `begin()`: `Started`, a button down and a
    /// modifier's release.
    fn activation() -> Vec<CaptureEvent> {
        vec![
            CaptureEvent::Started { id: CaptureId(7) },
            CaptureEvent::Button {
                button: MouseButton::PRIMARY,
                down: true,
                at: at(),
            },
            CaptureEvent::Key {
                usage: HidUsage::keyboard(0xE0),
                down: false,
                at: at(),
            },
            CaptureEvent::Button {
                button: MouseButton::PRIMARY,
                down: false,
                at: at(),
            },
        ]
    }

    fn label(input: &Input) -> &'static str {
        match input {
            Input::Capture(CaptureEvent::Started { .. }) => "started",
            Input::Capture(CaptureEvent::Button { .. }) => "button",
            Input::Capture(CaptureEvent::Key { .. }) => "key",
            Input::Capture(CaptureEvent::Motion { .. }) => "motion",
            Input::CaptureBegun { .. } => "begun",
            Input::Session(_) => "session",
            Input::Grants(_) => "grants",
            _ => "other",
        }
    }

    fn fed(h: &Home) -> Vec<&'static str> {
        h.rig.agent.fed.iter().map(label).collect()
    }

    fn begin(h: &mut Home, drain_first: bool) {
        h.rig.agent.execute(vec![Output::BeginCapture {
            id: CaptureId(7),
            portal: PortalId(1),
            drain_first,
        }]);
    }

    /// `ReleaseAndWarp` to `to`, and the engine's answer.
    fn warp(h: &mut Home, to: (DisplayId, PointDevice)) -> Result<Warp, Failure> {
        h.rig.agent.execute(vec![Output::ReleaseAndWarp {
            op: HomeOp(4),
            warp_to: to,
        }]);
        match h.rig.agent.pending.pop_back() {
            Some(Input::CaptureReleased { op, result }) => {
                assert_eq!(op, HomeOp(4), "the answer carries the request's op");
                result
            }
            other => panic!("expected CaptureReleased, got {other:?}"),
        }
    }

    /// `HomeBind` and the engine's answer.
    fn bind(h: &mut Home, op: u64, install: bool) -> Result<(), Failure> {
        h.rig.agent.execute(vec![Output::HomeBind {
            op: HomeOp(op),
            install,
        }]);
        match h.rig.agent.pending.pop_back() {
            Some(Input::HomeBindSet {
                op: got,
                install: was,
                result,
            }) => {
                assert_eq!(
                    (got, was),
                    (HomeOp(op), install),
                    "the answer echoes the request"
                );
                result
            }
            other => panic!("expected HomeBindSet, got {other:?}"),
        }
    }

    fn key(home: &Home) -> ProjectionKey {
        ProjectionKey {
            source: home.rig.local,
            projection: ProjectionId(3),
        }
    }

    fn last_notice(h: &Home) -> String {
        h.rig.agent.notices.back().cloned().unwrap_or_default()
    }

    fn grants_of(h: &Home, peer: NodeId) -> BTreeSet<Capability> {
        h.rig
            .agent
            .fed
            .iter()
            .rev()
            .find_map(|input| match input {
                Input::Grants(map) => map.get(&peer).cloned(),
                _ => None,
            })
            .expect("the engine was sent grants")
    }

    // Real engine transitions, with only platform backends and time replaced. Acknowledgements
    // run through the agent's ordinary pending queue and event channel, never swallowed by tests.
    fn ms(n: u64) -> MonoTime {
        MonoTime::from_nanos(n * 1_000_000)
    }
    fn control_input(peer: NodeId, msg: ControlMessage) -> Input {
        Input::Link(LinkEvent::Control { peer, msg })
    }
    fn projection_input(peer: NodeId, msg: ProjectionMessage) -> Input {
        control_input(peer, ControlMessage::Projection(msg))
    }
    fn step(h: &mut Home, input: Input) -> Vec<Output> {
        let before = h.rig.agent.emitted.len();
        let now = h.rig.agent.test_now.unwrap_or(ms(0));
        h.rig.agent.test_now = Some(MonoTime::from_nanos(now.as_nanos() + 1_000_000));
        h.rig.agent.feed(input);
        process_events(h);
        let mut index = before;
        while index < h.rig.agent.emitted.len() {
            use crosspane_protocol::msg::InputMessage;
            let ack = match &h.rig.agent.emitted[index] {
                Output::SendInput {
                    peer,
                    msg:
                        InputMessage::Key { session, seq, .. }
                        | InputMessage::Button { session, seq, .. }
                        | InputMessage::Scroll { session, seq, .. }
                        | InputMessage::LockKeys { session, seq, .. }
                        | InputMessage::State { session, seq, .. },
                } => Some(Input::Link(LinkEvent::Input {
                    peer: *peer,
                    msg: InputMessage::Ack {
                        session: *session,
                        seq: *seq,
                    },
                })),
                _ => None,
            };
            if let Some(ack) = ack {
                h.rig.agent.feed(ack);
                process_events(h);
            }
            index += 1;
        }
        h.rig.agent.emitted[before..].to_vec()
    }
    fn tick(h: &mut Home, delta: u64) -> Vec<Output> {
        let now = h.rig.agent.test_now.unwrap();
        h.rig.agent.test_now = Some(MonoTime::from_nanos(now.as_nanos() + delta * 1_000_000));
        step(h, Input::Tick)
    }
    fn motion(h: &mut Home, dx: f64, dy: f64) -> Vec<Output> {
        step(
            h,
            Input::Capture(CaptureEvent::Motion {
                dx,
                dy,
                kind: crosspane_platform::MotionKind::Unaccelerated,
                at: h.rig.agent.test_now.unwrap(),
            }),
        )
    }
    fn proj_key(h: &mut Home, seq: u32, down: bool) -> Vec<Output> {
        use crosspane_protocol::{msg::InputMessage, projection::ProjInput};
        step(
            h,
            Input::Link(LinkEvent::Input {
                peer: h.rig.peer,
                msg: InputMessage::Proj(ProjInput::Key {
                    projection: ProjectionId(1),
                    seq,
                    usage: HidUsage::keyboard(4),
                    down,
                }),
            }),
        )
    }
    fn trigger(h: &mut Home) -> Vec<Output> {
        use crosspane_protocol::{msg::InputMessage, projection::ProjInput};
        step(
            h,
            Input::Link(LinkEvent::Input {
                peer: h.rig.peer,
                msg: InputMessage::Proj(ProjInput::Motion {
                    projection: ProjectionId(1),
                    seq: 1,
                    position: PointDevice::new(50.0, 100.0),
                }),
            }),
        )
    }
    fn bare_scenario() -> Home {
        use crosspane_engine::EngineConfig;
        use crosspane_input::journal::MemoryJournal;
        use crosspane_types::geom::PointMm;
        let mut h = home();
        let mut config = EngineConfig::new(h.rig.local);
        config.accel.base_mm_per_unit = 0.1;
        config.accel.max_gain = 1.0;
        let (engine, startup) = Engine::new(
            config,
            Box::new(MemoryJournal::default()),
            Box::new(MemoryJournal::default()),
            ms(0),
        )
        .unwrap();
        h.rig.agent.engine = engine;
        h.rig.agent.test_now = Some(ms(0));
        h.rig.agent.platform.parking = Some(Box::new(FakeParking));
        h.rig.agent.platform.frames = Some(Box::new(FakeFrames));
        {
            let mut log = h.capture.lock().unwrap();
            log.lifecycle = true;
            log.warp_cursor = Some(h.compositor.clone());
        }
        h.rig.agent.execute(startup);
        process_events(&mut h);
        let mut d = display(1, 1.0, (0.0, 0.0), (1000, 1000));
        d.geometry.physical_size = SizeMm::new(100.0, 100.0);
        h.rig.agent.local_displays = vec![d.clone()];
        h.compositor.lock().unwrap().physical = vec![d.clone()];
        let peer = h.rig.peer;
        h.rig
            .agent
            .trust
            .update(|t| {
                t.set_grant(peer, Capability::InputAccept, true)
                    .map_err(|e| anyhow::anyhow!("{e}"))
            })
            .unwrap();
        h.rig.agent.peers.insert(
            peer,
            PeerInfo {
                name: "peer-name".into(),
                connected: true,
                displays: vec![d.clone()],
                ..PeerInfo::default()
            },
        );
        step(
            &mut h,
            Input::Session(crosspane_platform::SessionEvent::State(
                crosspane_platform::SessionState {
                    lock: crosspane_platform::LockState::Unlocked,
                    active: Some(true),
                },
            )),
        );
        step(&mut h, Input::LocalDisplays(vec![d.clone()]));
        step(
            &mut h,
            Input::PeerDisplays {
                peer,
                displays: vec![d],
            },
        );
        let local = h.rig.local;
        step(
            &mut h,
            Input::Layout(vec![
                Placement {
                    node: local,
                    display: DisplayId(1),
                    origin: PointMm::zero(),
                    version: 1,
                },
                Placement {
                    node: peer,
                    display: DisplayId(1),
                    origin: PointMm::new(100.0, 0.0),
                    version: 1,
                },
            ]),
        );
        step(
            &mut h,
            Input::Grants(
                [(
                    peer,
                    [
                        Capability::WindowShare,
                        Capability::WindowPresent,
                        Capability::InputAccept,
                    ]
                    .into(),
                )]
                .into(),
            ),
        );
        step(&mut h, Input::PeerUp { peer });
        step(
            &mut h,
            Input::Windows(WindowEvent::Added(window(
                10,
                "fixture",
                42,
                Some(1),
                (0.0, 0.0, 320.0, 240.0),
                WindowState::Normal,
            ))),
        );
        h
    }
    fn projected_scenario() -> Home {
        let mut h = bare_scenario();
        let peer = h.rig.peer;
        let out = step(
            &mut h,
            Input::Command(Command::Project {
                window: WindowId(10),
                to: peer,
            }),
        );
        assert!(
            out.iter().any(|o| matches!(
                o,
                Output::SendControl {
                    msg: ControlMessage::Projection(ProjectionMessage::Start {
                        projection: ProjectionId(1),
                        ..
                    }),
                    ..
                }
            )),
            "{out:?}"
        );
        let out = step(
            &mut h,
            projection_input(
                peer,
                ProjectionMessage::Accepted {
                    projection: ProjectionId(1),
                    size: PixelSize::new(400, 300),
                    scale: 1.0,
                },
            ),
        );
        assert!(
            out.iter().any(|o| matches!(o, Output::StartCapture { .. })),
            "{out:?}"
        );
        step(
            &mut h,
            projection_input(
                peer,
                ProjectionMessage::ProxyPlaced {
                    projection: ProjectionId(1),
                    generation: 1,
                    display: Some(DisplayId(1)),
                    origin: PointDevice::new(200.0, 300.0),
                    size: PixelSize::new(400, 300),
                },
            ),
        );
        h
    }
    fn cross_scenario(h: &mut Home) {
        let portal = h
            .rig
            .agent
            .emitted
            .iter()
            .rev()
            .find_map(|o| match o {
                Output::SetPortals(ps) => {
                    ps.iter().find(|p| p.display == DisplayId(1)).map(|p| p.id)
                }
                _ => None,
            })
            .unwrap();
        let out = step(
            h,
            Input::Capture(CaptureEvent::EdgePressed {
                portal,
                position: 0.5,
                at: h.rig.agent.test_now.unwrap(),
            }),
        );
        assert!(
            out.iter().any(|o| matches!(o, Output::ShowOverlay { .. })),
            "{out:?}"
        );
        let out = step(
            h,
            Input::Overlay(crosspane_platform::OverlayEvent::Visible(
                crosspane_engine::io::HUD,
            )),
        );
        let session = out
            .iter()
            .find_map(|o| match o {
                Output::SendControl {
                    msg: ControlMessage::StartControl { session, .. },
                    ..
                } => Some(*session),
                _ => None,
            })
            .unwrap();
        let out = step(
            h,
            control_input(h.rig.peer, ControlMessage::ControlStarted { session }),
        );
        assert!(
            out.iter().any(|o| matches!(
                o,
                Output::BeginCapture {
                    drain_first: false,
                    ..
                }
            )),
            "{out:?}"
        );
        assert_eq!(h.rig.agent.engine.controlling(), Some(h.rig.peer));
        assert!(h.capture.lock().unwrap().active.is_some());
    }
    fn aimed_scenario() -> Home {
        let mut h = projected_scenario();
        cross_scenario(&mut h);
        motion(&mut h, 250.0, -100.0);
        h
    }
    fn home_scenario() -> Home {
        let mut h = aimed_scenario();
        let out = trigger(&mut h);
        assert!(
            out.iter()
                .any(|o| matches!(o, Output::ReleaseAndWarp { .. })),
            "{out:?}"
        );
        step(
            &mut h,
            Input::Windows(WindowEvent::Focused(Some(WindowId(10)))),
        );
        assert_eq!(h.rig.agent.status()["home"]["projection"], json!(1));
        assert!(h.compositor.lock().unwrap().ours);
        assert!(h.capture.lock().unwrap().active.is_none());
        h
    }
    fn exit_home(h: &mut Home) -> Vec<Output> {
        let mut out = step(
            h,
            Input::Capture(CaptureEvent::EdgePressed {
                portal: PortalId((1 << 30) + 1),
                position: 0.5,
                at: h.rig.agent.test_now.unwrap(),
            }),
        );
        assert!(
            out.iter().any(|o| matches!(o, Output::ShowOverlay { .. })),
            "{out:?}"
        );
        out.extend(step(
            h,
            Input::Overlay(crosspane_platform::OverlayEvent::Visible(
                crosspane_engine::io::HUD,
            )),
        ));
        assert!(
            out.iter().any(|o| matches!(
                o,
                Output::BeginCapture {
                    drain_first: true,
                    ..
                }
            )),
            "{out:?}"
        );
        out
    }
    fn start_request(h: &mut Home, reason: Refusal) {
        let peer = NodeId([3; 32]);
        step(
            h,
            Input::Grants(
                [
                    (peer, [Capability::InputAccept].into()),
                    (
                        h.rig.peer,
                        [
                            Capability::WindowShare,
                            Capability::WindowPresent,
                            Capability::InputAccept,
                        ]
                        .into(),
                    ),
                ]
                .into(),
            ),
        );
        let out = step(
            h,
            control_input(
                peer,
                ControlMessage::StartControl {
                    session: crosspane_types::id::SessionId(77),
                    entry_display: DisplayId(1),
                    entry: PointDevice::new(1.0, 1.0),
                    lock_keys: LockKeys::default(),
                },
            ),
        );
        assert!(out.iter().any(|o| matches!(o, Output::SendControl { msg: ControlMessage::ControlRefused { reason: got, .. }, .. } if *got == reason)), "{out:?}");
        assert!(h.rig.agent.engine.controlled_by().is_none());
    }

    #[test]
    fn real_home_exit_removes_the_bind_and_keeps_control() {
        let mut h = home_scenario();
        let out = exit_home(&mut h);
        assert!(
            out.iter()
                .any(|o| matches!(o, Output::HomeBind { install: false, .. })),
            "{out:?}"
        );
        assert!(h.rig.agent.home.now.is_none());
        assert!(!h.compositor.lock().unwrap().ours);
        assert_eq!(h.rig.agent.engine.controlling(), Some(h.rig.peer));
        step(&mut h, Input::Command(Command::ReleaseControl));
        assert!(h.held.lock().unwrap().is_empty());
    }
    #[test]
    fn real_failed_install_rolls_back_even_a_partial_install_and_keeps_the_cause() {
        for partial in [false, true] {
            let mut h = aimed_scenario();
            {
                let mut c = h.compositor.lock().unwrap();
                c.install_error = Some("fixture install failure".into());
                c.partial_install = partial;
            }
            let out = trigger(&mut h);
            assert!(
                out.iter()
                    .any(|o| matches!(o, Output::HomeBind { install: false, .. })),
                "{out:?}"
            );
            assert!(
                out.iter().any(|o| matches!(
                    o,
                    Output::Notice(Notice::HomeFailed {
                        reason: HomeFailure::Bind,
                        ..
                    })
                )),
                "{out:?}"
            );
            assert!(!h.compositor.lock().unwrap().ours);
            assert!(last_notice(&h).contains("fixture install failure"));
            assert!(h.rig.agent.home.now.is_none());
            step(&mut h, Input::Command(Command::ReleaseControl));
        }
    }
    #[test]
    fn real_entry_abort_after_install_removes_the_bind() {
        let mut h = aimed_scenario();
        h.capture.lock().unwrap().end_error = Some(|| PlatformError::Backend("end failed".into()));
        let out = trigger(&mut h);
        assert!(
            out.iter()
                .any(|o| matches!(o, Output::ReleaseAndWarp { .. })),
            "{out:?}"
        );
        assert!(
            out.iter().any(|o| matches!(
                o,
                Output::Notice(Notice::HomeFailed {
                    reason: HomeFailure::Warp,
                    ..
                })
            )),
            "{out:?}"
        );
        assert!(!h.compositor.lock().unwrap().ours);
        assert!(h.rig.agent.home.now.is_none());
    }
    #[test]
    fn real_session_end_panic_and_lock_remove_home() {
        for kind in 0..3 {
            let mut h = home_scenario();
            let input = match kind {
                0 => Input::Command(Command::ReleaseControl),
                1 => Input::Command(Command::Panic),
                _ => Input::Session(crosspane_platform::SessionEvent::State(
                    crosspane_platform::SessionState {
                        lock: crosspane_platform::LockState::Locked,
                        active: Some(true),
                    },
                )),
            };
            let out = step(&mut h, input);
            assert!(
                out.iter()
                    .any(|o| matches!(o, Output::HomeBind { install: false, .. })),
                "{out:?}"
            );
            assert!(!h.compositor.lock().unwrap().ours);
            assert!(h.rig.agent.home.now.is_none());
            assert!(h.rig.agent.engine.controlling().is_none());
            assert!(h.held.lock().unwrap().is_empty());
        }
    }
    #[test]
    fn real_teardown_refuses_peer_downs_and_start_control_until_the_fourth_removal() {
        let mut h = home_scenario();
        h.compositor.lock().unwrap().remove_error = Some("uncertain ownership".into());
        let before = h.compositor.lock().unwrap().removes;
        step(&mut h, Input::Command(Command::ReleaseControl));
        for delay in [0, 100, 200] {
            if delay > 0 {
                tick(&mut h, delay);
            }
            proj_key(&mut h, 2 + delay as u32, true);
            proj_key(&mut h, 3 + delay as u32, false);
            assert!(h.held.lock().unwrap().is_empty());
            assert!(!h.injected.lock().unwrap().contains(&"key down"));
            start_request(&mut h, Refusal::Busy);
        }
        assert_eq!(h.compositor.lock().unwrap().removes - before, 3);
        h.compositor.lock().unwrap().remove_error = None;
        tick(&mut h, 400);
        assert_eq!(h.compositor.lock().unwrap().removes - before, 4);
        assert!(!h.compositor.lock().unwrap().ours);
        // After verified removal, E2 downs are accepted again. Restore the projection grant
        // removed by start_request's deliberately isolated E1 grant fixture.
        let peer = h.rig.peer;
        step(
            &mut h,
            Input::Grants(
                [(
                    peer,
                    [Capability::WindowShare, Capability::WindowPresent].into(),
                )]
                .into(),
            ),
        );
        proj_key(&mut h, 500, true);
        assert!(h.held.lock().unwrap().contains(&HidUsage::keyboard(4)));
        proj_key(&mut h, 501, false);
        assert!(h.held.lock().unwrap().is_empty());
    }
    #[test]
    fn the_warp_reads_the_cursor_after_end_moves_it() {
        let mut h = home();
        h.compositor.lock().unwrap().cursor = Some((DisplayId(1), PointDevice::zero()));
        h.capture.lock().unwrap().warp_cursor = Some(h.compositor.clone());
        assert_eq!(warp(&mut h, TARGET), Ok(Warp::Done));
        assert_eq!(h.compositor.lock().unwrap().cursor, Some(TARGET));
    }

    #[test]
    fn startup_uncertainty_refuses_actual_control_until_absence_is_confirmed() {
        for ambiguous in [false, true] {
            let mut h = bare_scenario();
            let peer = h.rig.peer;
            {
                let mut c = h.compositor.lock().unwrap();
                c.ours = true;
                c.verify_error = ambiguous;
                if !ambiguous {
                    c.remove_error = Some("ownership read failed".into());
                }
            }
            h.rig.agent.home_startup();
            h.rig.agent.send_grants();
            process_events(&mut h);
            let request = || {
                control_input(
                    peer,
                    ControlMessage::StartControl {
                        session: crosspane_types::id::SessionId(88),
                        entry_display: DisplayId(1),
                        entry: PointDevice::new(1.0, 1.0),
                        lock_keys: LockKeys::default(),
                    },
                )
            };
            for _ in 0..2 {
                let out = step(&mut h, request());
                assert!(
                    out.iter().any(|o| matches!(
                        o,
                        Output::SendControl {
                            msg: ControlMessage::ControlRefused {
                                reason: Refusal::Permission,
                                ..
                            },
                            ..
                        }
                    )),
                    "{out:?}"
                );
                assert!(h.rig.agent.engine.controlled_by().is_none());
                assert!(!h.rig.agent.inject(InjectCmd::Key {
                    usage: HidUsage::keyboard(4),
                    down: true
                }));
                assert!(h.rig.agent.inject(InjectCmd::Key {
                    usage: HidUsage::keyboard(4),
                    down: false
                }));
                h.rig.agent.home.last_fence_try = Instant::now() - Duration::from_millis(2100);
                h.rig.agent.home_housekeeping();
                process_events(&mut h);
                assert!(h.rig.agent.home.fence);
            }
            {
                let mut c = h.compositor.lock().unwrap();
                c.ours = false;
                c.foreign = true;
                c.verify_error = false;
                c.remove_error = None;
            }
            h.rig.agent.home.last_fence_try = Instant::now() - Duration::from_millis(2100);
            h.rig.agent.home_housekeeping();
            process_events(&mut h);
            let out = step(&mut h, request());
            assert!(
                out.iter().any(|o| matches!(
                    o,
                    Output::SendControl {
                        msg: ControlMessage::ControlStarted { .. },
                        ..
                    }
                )),
                "{out:?}"
            );
            assert_eq!(h.rig.agent.engine.controlled_by(), Some(peer));
            assert!(h.compositor.lock().unwrap().foreign);
            step(&mut h, Input::Command(Command::ReleaseControl));
            assert!(h.held.lock().unwrap().is_empty());
        }
    }
    #[test]
    fn real_verification_uncertainty_and_mixed_ownership_keep_the_teardown_fence() {
        for ambiguous in [false, true] {
            let mut h = home_scenario();
            {
                let mut c = h.compositor.lock().unwrap();
                c.verify_error = ambiguous;
                c.foreign = !ambiguous;
            }
            h.rig.agent.on_event(Event::HomeBind);
            h.rig.agent.home_housekeeping();
            process_events(&mut h);
            assert!(h.rig.agent.home.now.is_none());
            assert!(h.compositor.lock().unwrap().ours);
            start_request(&mut h, Refusal::Busy);
            proj_key(&mut h, 2, true);
            proj_key(&mut h, 3, false);
            assert!(h.held.lock().unwrap().is_empty());
            assert!(!h.injected.lock().unwrap().contains(&"key down"));
            {
                let mut c = h.compositor.lock().unwrap();
                c.ours = false;
                c.verify_error = false;
                c.foreign = true;
            }
            tick(&mut h, 100);
            assert_eq!(h.rig.agent.home.present, Some(false));
            assert!(h.compositor.lock().unwrap().foreign);
            proj_key(&mut h, 4, true);
            assert!(h.held.lock().unwrap().contains(&HidUsage::keyboard(4)));
            proj_key(&mut h, 5, false);
            assert!(h.held.lock().unwrap().is_empty());
        }
    }
    fn captured_key(h: &mut Home, usage: u16, down: bool) -> Vec<Output> {
        step(
            h,
            Input::Capture(CaptureEvent::Key {
                usage: HidUsage::keyboard(usage),
                down,
                at: h.rig.agent.test_now.unwrap(),
            }),
        )
    }
    #[test]
    fn a_button_during_real_exit_activation_cancels_before_commit() {
        let mut h = home_scenario();
        h.capture.lock().unwrap().during_begin = vec![CaptureEvent::Button {
            button: MouseButton::PRIMARY,
            down: true,
            at: at(),
        }];
        let out = exit_home(&mut h);
        assert!(
            !out.iter()
                .any(|o| matches!(o, Output::HomeBind { install: false, .. })),
            "{out:?}"
        );
        assert_eq!(h.rig.agent.status()["home"]["projection"], json!(1));
        assert!(h.capture.lock().unwrap().active.is_none());
        assert!(h.compositor.lock().unwrap().ours);
        step(
            &mut h,
            Input::Capture(CaptureEvent::Button {
                button: MouseButton::PRIMARY,
                down: false,
                at: at(),
            }),
        );
        step(&mut h, Input::Command(Command::ReleaseControl));
    }
    #[test]
    fn exit_activation_modifier_down_and_up_reconcile_the_snapshot_in_both_orders() {
        for down_first in [false, true] {
            let mut h = home_scenario();
            let mut events = vec![
                CaptureEvent::Key {
                    usage: HidUsage::keyboard(0xE0),
                    down: down_first,
                    at: at(),
                },
                CaptureEvent::Key {
                    usage: HidUsage::keyboard(0xE0),
                    down: !down_first,
                    at: at(),
                },
            ];
            // Seed the snapshot oppositely to the callback's final state. The exit's event
            // callbacks must win, and must not forward activation-time keys to the peer.
            h.capture.lock().unwrap().start = Some(CaptureStart {
                held_keys: if down_first {
                    vec![HidUsage::keyboard(0xE0)]
                } else {
                    vec![]
                },
                lock_keys: LockKeys::default(),
            });
            h.capture.lock().unwrap().during_begin.append(&mut events);
            let out = exit_home(&mut h);
            assert!(
                !out.iter().any(|o| matches!(
                    o,
                    Output::SendInput {
                        msg: crosspane_protocol::msg::InputMessage::Key { .. },
                        ..
                    }
                )),
                "activation keys must not be forwarded: {out:?}"
            );
            assert!(
                out.iter()
                    .any(|o| matches!(o, Output::HomeBind { install: false, .. })),
                "{out:?}"
            );
            captured_key(&mut h, 0xE1, true);
            captured_key(&mut h, 0xE2, true);
            captured_key(&mut h, 0x29, true);
            assert_eq!(h.rig.agent.engine.controlling().is_none(), !down_first);
            for usage in [0x29, 0xE2, 0xE1, 0xE0] {
                captured_key(&mut h, usage, false);
            }
            step(&mut h, Input::Command(Command::ReleaseControl));
            assert!(h.held.lock().unwrap().is_empty());
        }
    }
    #[test]
    fn ordinary_activation_preserves_the_final_modifier_state_for_the_chord() {
        let mut h = projected_scenario();
        h.capture.lock().unwrap().start = Some(CaptureStart {
            held_keys: vec![HidUsage::keyboard(0xE0)],
            lock_keys: LockKeys::default(),
        });
        h.capture.lock().unwrap().during_begin = vec![CaptureEvent::Key {
            usage: HidUsage::keyboard(0xE0),
            down: false,
            at: at(),
        }];
        cross_scenario(&mut h);
        for usage in [0xE1, 0xE2, 0x29] {
            captured_key(&mut h, usage, true);
        }
        assert_eq!(
            h.rig.agent.engine.controlling(),
            Some(h.rig.peer),
            "the released Ctrl must not remain held"
        );
        for usage in [0x29, 0xE2, 0xE1, 0xE0] {
            captured_key(&mut h, usage, false);
        }
        step(&mut h, Input::Command(Command::ReleaseControl));
        assert!(h.held.lock().unwrap().is_empty());
    }
    fn refresh_placement(h: &mut Home) -> Vec<Output> {
        step(
            h,
            projection_input(
                h.rig.peer,
                ProjectionMessage::ProxyPlaced {
                    projection: ProjectionId(1),
                    generation: 2,
                    display: Some(DisplayId(1)),
                    origin: PointDevice::new(600.0, 300.0),
                    size: PixelSize::new(400, 300),
                },
            ),
        )
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn rejected_portal_refresh_preserves_a_real_active_capture() {
        let mut h = aimed_scenario();
        let id = h.capture.lock().unwrap().active.unwrap();
        h.capture.lock().unwrap().set_error = Some(|| {
            PlatformError::Backend(format!(
                "{}verified refusal",
                crosspane_platform_linux::hyprland::capture::PORTALS_REJECTED
            ))
        });
        let out = refresh_placement(&mut h);
        assert!(
            out.iter().any(|o| matches!(o, Output::SetPortals(_))),
            "{out:?}"
        );
        assert!(h.rig.agent.fed.iter().any(|i| matches!(
            i,
            Input::PortalsSet {
                result: Err(PortalsFailure::Rejected),
                ..
            }
        )));
        assert_eq!(h.capture.lock().unwrap().active, Some(id));
        assert_eq!(h.rig.agent.engine.controlling(), Some(h.rig.peer));
        assert!(
            motion(&mut h, 1.0, 0.0)
                .iter()
                .any(|o| matches!(o, Output::SendMotion { .. }))
        );
        h.capture.lock().unwrap().set_error = None;
        step(&mut h, Input::Command(Command::ReleaseControl));
    }
    #[test]
    fn uncertain_portal_refresh_ends_capture_and_missing_ended_uses_the_deadline() {
        for ended in [false, true] {
            let mut h = aimed_scenario();
            {
                let mut log = h.capture.lock().unwrap();
                log.set_error = Some(|| PlatformError::Timeout);
                log.end_on_set_error = ended;
                if !ended {
                    log.lifecycle = false;
                }
            }
            let out = refresh_placement(&mut h);
            assert!(
                out.iter().any(|o| matches!(o, Output::EndCapture { .. })),
                "{out:?}"
            );
            assert!(h.rig.agent.engine.controlling().is_none());
            if ended {
                assert!(h.rig.agent.fed.iter().any(|i| matches!(
                    i,
                    Input::Capture(CaptureEvent::Ended {
                        reason: crosspane_platform::EndReason::Aborted,
                        ..
                    })
                )));
                assert!(
                    out.iter().any(|o| matches!(o, Output::HideOverlay { .. })),
                    "{out:?}"
                );
            } else {
                assert!(
                    !out.iter().any(|o| matches!(o, Output::HideOverlay { .. })),
                    "before the end deadline: {out:?}"
                );
                let out = tick(&mut h, 400);
                assert!(
                    !out.iter().any(|o| matches!(o, Output::HideOverlay { .. })),
                    "{out:?}"
                );
                let out = tick(&mut h, 650);
                assert!(
                    out.iter().any(|o| matches!(o, Output::HideOverlay { .. })),
                    "after the deadline: {out:?}"
                );
            }
            assert!(h.held.lock().unwrap().is_empty());
        }
    }

    // ---- BeginCapture: the order of an activation's events and its answer (A4, B4) ----

    /// Handle the event channel exactly as the run loop does, settling ordinary answers between
    /// events. Exit activation answers share this channel with the backend's callbacks.
    fn process_events(h: &mut Home) {
        h.rig.agent.settle();
        loop {
            let event = match h.rig.events.try_recv() {
                Ok(event) => event,
                Err(std::sync::mpsc::TryRecvError::Empty)
                    if h.rig
                        .agent
                        .parking
                        .as_ref()
                        .is_some_and(crate::parking_worker::Worker::pending) =>
                {
                    h.rig
                        .events
                        .recv_timeout(Duration::from_secs(1))
                        .expect("fake parking completes")
                }
                Err(_) => break,
            };
            h.rig.agent.on_event(event);
            h.rig.agent.settle();
        }
    }

    /// Fixture replacement waits for the old worker's cleanup, as a real backend swap would.
    fn replace_parking(agent: &mut Agent, backend: Box<dyn crosspane_platform::WindowParking>) {
        assert_ne!(
            agent.parking_shutdown(crate::parking_worker::SHUTDOWN_WAIT),
            crate::lifecycle::Parking::Failed
        );
        agent.parking = None;
        agent.parking_available = false;
        agent.platform.parking = Some(backend);
    }

    #[test]
    fn parking_resize_storm_keeps_engine_input_ticks_links_and_housekeeping_responsive() {
        use crate::parking_worker::tests::{Kind, fake};
        use crosspane_protocol::{msg::InputMessage, projection::ProjInput};
        let mut h = projected_scenario();
        step(
            &mut h,
            Input::Windows(WindowEvent::Focused(Some(WindowId(10)))),
        );
        let (backend, controls) = fake(Some(Kind::Resize));
        replace_parking(&mut h.rig.agent, Box::new(backend));
        let peer = h.rig.peer;
        let resize = |request, width| {
            projection_input(
                peer,
                ProjectionMessage::Resize {
                    projection: ProjectionId(1),
                    request,
                    size: PixelSize::new(width, 300),
                    scale: 1.0,
                },
            )
        };
        h.rig.agent.feed(resize(1, 600));
        assert_eq!(
            controls
                .observed
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            (Kind::Resize, WindowId(10), 600)
        );
        let fed = h.rig.agent.fed.len();
        let keys = h.injected.lock().unwrap().len();
        for request in 2..100 {
            h.rig
                .agent
                .on_event(Event::Input(resize(request, 600 + request)));
            h.rig.agent.on_event(Event::Input(Input::Tick));
        }
        h.rig
            .agent
            .on_event(Event::Input(Input::Link(LinkEvent::Input {
                peer,
                msg: InputMessage::Proj(ProjInput::Key {
                    projection: ProjectionId(1),
                    seq: 1,
                    usage: HidUsage::keyboard(4),
                    down: true,
                }),
            })));
        h.rig.agent.settle();
        h.rig.agent.on_event(Event::Links(Vec::new()));
        h.rig.agent.housekeeping();
        assert_eq!(
            h.rig.agent.fed[fed..]
                .iter()
                .filter(|i| matches!(i, Input::Tick))
                .count(),
            98
        );
        assert!(h.injected.lock().unwrap()[keys..].contains(&"key down"));
        assert!(
            controls.observed.try_recv().is_err(),
            "only one backend call can run"
        );
        controls.release.send(()).unwrap();
        process_events(&mut h);
        assert_eq!(
            controls
                .observed
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            (Kind::Resize, WindowId(10), 699)
        );
        assert!(controls.observed.try_recv().is_err());
        assert_eq!(
            h.rig.agent.home.twins.get(&WindowId(10)),
            Some(&DisplayId(37))
        );
    }

    #[test]
    fn parking_twin_identity_uses_completion_display_and_failed_park_records_none() {
        use crate::parking_worker::tests::{Kind, fake};
        for failed in [false, true] {
            let mut h = home();
            let (mut backend, controls) = fake(Some(Kind::Park));
            backend.park_fails = failed;
            replace_parking(&mut h.rig.agent, Box::new(backend));
            h.rig.agent.execute(vec![Output::Park {
                window: WindowId(10),
                size: PixelSize::new(400, 300),
                scale: 1.0,
            }]);
            assert_eq!(
                controls
                    .observed
                    .recv_timeout(Duration::from_secs(1))
                    .unwrap()
                    .0,
                Kind::Park
            );
            assert!(
                h.rig.agent.home.twins.is_empty(),
                "submission supplies no guessed identity"
            );
            controls.release.send(()).unwrap();
            process_events(&mut h);
            assert_eq!(
                h.rig.agent.home.twins.get(&WindowId(10)).copied(),
                if failed { None } else { Some(DisplayId(37)) }
            );
        }
    }

    #[test]
    fn parking_backend_presence_and_legacy_debug_status_survive_worker_handoff() {
        let mut h = home();
        h.rig.agent.platform.parking = Some(Box::new(FakeParking));
        let before = h.rig.agent.status();
        h.rig.agent.parking_start();
        assert!(h.rig.agent.platform.parking.is_none());
        assert!(h.rig.agent.parking.is_some());
        let after = h.rig.agent.status();
        assert_eq!(after["backends"], before["backends"]);
        assert_eq!(
            after["installer"]["backends"],
            before["installer"]["backends"]
        );
        assert!(
            after["backends"]
                .as_str()
                .unwrap()
                .contains("parking: true")
        );
        let entry = after["installer"]["backends"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["name"] == "parking")
            .unwrap();
        assert_eq!(entry["state"], json!("ready"));
        assert_eq!(
            after["installer"]["epochs"]["backends"],
            before["installer"]["epochs"]["backends"]
        );
    }

    #[test]
    fn source_return_counter_waits_for_restore_completion_across_unrelated_inputs() {
        use crate::parking_worker::tests::{Kind, fake};
        for failed in [false, true] {
            let mut h = projected_scenario();
            let (mut backend, controls) = fake(Some(Kind::Restore));
            backend.restore_fails = failed;
            replace_parking(&mut h.rig.agent, Box::new(backend));
            h.rig
                .agent
                .feed(Input::Command(Command::Return(ProjectionKey {
                    source: h.rig.local,
                    projection: ProjectionId(1),
                })));
            assert_eq!(
                controls
                    .observed
                    .recv_timeout(Duration::from_secs(1))
                    .unwrap()
                    .0,
                Kind::Restore
            );
            assert_eq!(nonzero(&h), json!({ "e2_source_started": 1 }));
            for _ in 0..10 {
                h.rig.agent.feed(Input::Tick);
            }
            assert_eq!(nonzero(&h), json!({ "e2_source_started": 1 }));
            assert_eq!(
                h.rig.agent.home.twins.get(&WindowId(10)),
                Some(&DisplayId(7))
            );
            controls.release.send(()).unwrap();
            process_events(&mut h);
            assert_eq!(
                nonzero(&h),
                if failed {
                    json!({ "e2_source_started": 1, "e2_returns_failed": 1 })
                } else {
                    json!({ "e2_source_started": 1, "e2_source_returned": 1 })
                }
            );
            assert_eq!(h.rig.agent.home.twins.contains_key(&WindowId(10)), failed);
        }
    }

    #[test]
    fn a_new_park_start_keeps_recovery_pending_while_blocked_despite_older_restore_completion() {
        use crate::parking_worker::tests::{Kind, fake};
        let mut h = projected_scenario();
        let (backend, controls) = fake(Some(Kind::Park));
        replace_parking(&mut h.rig.agent, Box::new(backend));
        h.rig.agent.execute(vec![
            Output::Restore {
                window: WindowId(10),
            },
            Output::Park {
                window: WindowId(10),
                size: PixelSize::new(400, 300),
                scale: 1.0,
            },
        ]);
        assert_eq!(
            controls
                .observed
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .0,
            Kind::Restore
        );
        assert_eq!(
            controls
                .observed
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .0,
            Kind::Park
        );
        // Deliberately defer the old Restore result until the newer Park's start has been
        // registered. Its ID must still be unable to clear the newer journal obligation.
        let mut old_restore = None;
        for _ in 0..3 {
            let event = h.rig.events.recv_timeout(Duration::from_secs(1)).unwrap();
            if matches!(
                &event,
                Event::Parking(crate::parking_worker::Completion {
                    outcome: crate::parking_worker::Outcome::Restored { .. },
                    ..
                })
            ) {
                old_restore = Some(event);
            } else {
                h.rig.agent.on_event(event);
            }
        }
        assert!(controls.journal.load(std::sync::atomic::Ordering::SeqCst));
        assert!(status_installer(&h)["recovery_pending"].as_u64().unwrap() >= 1);
        h.rig.agent.on_event(old_restore.unwrap());
        assert_eq!(status_installer(&h)["recovery_pending"], json!(1));
        assert!(h.rig.agent.home.twins.is_empty());
        controls.release.send(()).unwrap();
        process_events(&mut h);
        assert_eq!(
            h.rig.agent.home.twins.get(&WindowId(10)),
            Some(&DisplayId(37))
        );
    }

    #[test]
    fn an_old_park_completion_cannot_answer_reprojection_after_the_engine_fence_expires() {
        use crate::parking_worker::tests::{Kind, fake};
        let mut h = bare_scenario();
        let (mut backend, controls) = fake(Some(Kind::Park));
        backend.block_count = 2;
        backend.increment_display = true;
        replace_parking(&mut h.rig.agent, Box::new(backend));
        let peer = h.rig.peer;
        let accept = |projection| {
            projection_input(
                peer,
                ProjectionMessage::Accepted {
                    projection: ProjectionId(projection),
                    size: PixelSize::new(400, 300),
                    scale: 1.0,
                },
            )
        };
        h.rig.agent.feed(Input::Command(Command::Project {
            window: WindowId(10),
            to: peer,
        }));
        h.rig.agent.feed(accept(1));
        assert_eq!(
            controls
                .observed
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .0,
            Kind::Park
        );
        h.rig
            .agent
            .on_event(h.rig.events.recv_timeout(Duration::from_secs(1)).unwrap());
        let old = h.rig.agent.parking_latest[&WindowId(10)];
        h.rig
            .agent
            .feed(Input::Command(Command::Return(ProjectionKey {
                source: h.rig.local,
                projection: ProjectionId(1),
            })));
        h.rig.agent.test_now = Some(ms(6000));
        h.rig.agent.feed(Input::Tick);
        h.rig.agent.feed(Input::Command(Command::Project {
            window: WindowId(10),
            to: peer,
        }));
        h.rig.agent.feed(accept(2));
        let new = h.rig.agent.parking_latest[&WindowId(10)];
        assert!(new > old);
        let before = h.rig.agent.emitted.len();
        let answers = h
            .rig
            .agent
            .fed
            .iter()
            .filter(|input| matches!(input, Input::Parked { .. }))
            .count();
        controls.release.send(()).unwrap();
        // Both cancellation restores precede the replacement Park, which is held separately.
        for expected in [Kind::Restore, Kind::Restore, Kind::Park] {
            assert_eq!(
                controls
                    .observed
                    .recv_timeout(Duration::from_secs(1))
                    .unwrap()
                    .0,
                expected
            );
        }
        while let Ok(event) = h.rig.events.try_recv() {
            h.rig.agent.on_event(event);
            h.rig.agent.settle();
        }
        assert_eq!(
            h.rig
                .agent
                .fed
                .iter()
                .filter(|input| matches!(input, Input::Parked { .. }))
                .count(),
            answers
        );
        assert!(
            !h.rig.agent.emitted[before..]
                .iter()
                .any(|output| matches!(output, Output::StartCapture { .. }))
        );
        assert_eq!(h.rig.agent.parking_latest[&WindowId(10)], new);
        assert_eq!(status_installer(&h)["recovery_pending"], json!(1));
        controls.release.send(()).unwrap();
        process_events(&mut h);
        assert_eq!(
            h.rig
                .agent
                .fed
                .iter()
                .filter(|input| matches!(input, Input::Parked { .. }))
                .count(),
            answers + 1
        );
        assert!(h.rig.agent.emitted[before..].iter().any(|output| matches!(
            output,
            Output::StartCapture {
                target: CaptureTarget::Display(DisplayId(38)),
                ..
            }
        )));
        assert_eq!(
            h.rig.agent.home.twins.get(&WindowId(10)),
            Some(&DisplayId(38))
        );
    }

    struct ParkingReceiptDir(std::path::PathBuf);

    impl Drop for ParkingReceiptDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn receipt_journals(h: &mut Home) -> ParkingReceiptDir {
        use crosspane_input::journal::FileJournal;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "crosspane-wp236-shutdown-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        crate::paths::create_private_dir(&dir).unwrap();
        let paths = crate::paths::Paths {
            config_dir: dir.clone(),
            state_dir: dir.clone(),
            runtime_dir: dir.clone(),
        };
        FileJournal::open(&paths.journal_file()).unwrap();
        FileJournal::open(&paths.e2_journal_file()).unwrap();
        h.rig.agent.set_lifecycle_paths(paths);
        ParkingReceiptDir(dir)
    }

    #[test]
    fn parking_inflight_shutdown_is_clean_and_source_return_counts_actual_completion() {
        use crate::parking_worker::tests::{Kind, fake};
        let mut h = projected_scenario();
        let _journals = receipt_journals(&mut h);
        let (backend, controls) = fake(Some(Kind::Resize));
        replace_parking(&mut h.rig.agent, Box::new(backend));
        h.rig.agent.execute(vec![Output::ResizeParked {
            window: WindowId(10),
            size: PixelSize::new(400, 300),
            scale: 1.0,
        }]);
        assert_eq!(
            controls
                .observed
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .0,
            Kind::Resize
        );
        let release = controls.release.clone();
        let releaser = std::thread::spawn(move || release.send(()).unwrap());
        let outcomes = h.rig.agent.shutdown();
        releaser.join().unwrap();
        assert_eq!(outcomes.parking, crate::lifecycle::Parking::NothingParked);
        assert!(outcomes.input_journals_empty && outcomes.audio_stopped);
        assert_eq!(
            nonzero(&h),
            json!({ "e2_source_started": 1, "e2_source_returned": 1 })
        );
        assert!(h.rig.agent.home.twins.is_empty());
        assert_eq!(
            controls
                .observed
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .0,
            Kind::Restore
        );
        assert_eq!(
            controls
                .observed
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .0,
            Kind::Recover
        );
    }

    #[test]
    fn parking_hung_shutdown_returns_unclean_within_five_seconds_and_no_premature_return() {
        use crate::parking_worker::tests::{Kind, fake};
        use std::sync::atomic::Ordering;
        let mut h = projected_scenario();
        let _journals = receipt_journals(&mut h);
        let (backend, controls) = fake(Some(Kind::Resize));
        replace_parking(&mut h.rig.agent, Box::new(backend));
        h.rig.agent.execute(vec![Output::ResizeParked {
            window: WindowId(10),
            size: PixelSize::new(400, 300),
            scale: 1.0,
        }]);
        assert_eq!(
            controls
                .observed
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .0,
            Kind::Resize
        );
        let started = Instant::now();
        let outcomes = h.rig.agent.shutdown();
        assert_eq!(outcomes.parking, crate::lifecycle::Parking::Failed);
        assert!(started.elapsed() >= crate::parking_worker::SHUTDOWN_WAIT);
        assert!(started.elapsed() < crate::parking_worker::SHUTDOWN_WAIT + Duration::from_secs(1));
        assert!(controls.journal.load(Ordering::SeqCst));
        assert_eq!(nonzero(&h), json!({ "e2_source_started": 1 }));
        assert!(h.rig.agent.home.twins.contains_key(&WindowId(10)));
        controls.release.send(()).unwrap();
    }

    #[test]
    fn parking_panicked_shutdown_reports_unclean_without_hanging() {
        use crate::parking_worker::tests::{Kind, fake};
        let mut h = home();
        let _journals = receipt_journals(&mut h);
        let (mut backend, controls) = fake(None);
        backend.panic = Some(Kind::Park);
        replace_parking(&mut h.rig.agent, Box::new(backend));
        h.rig.agent.execute(vec![Output::Park {
            window: WindowId(10),
            size: PixelSize::new(400, 300),
            scale: 1.0,
        }]);
        assert_eq!(
            controls
                .observed
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .0,
            Kind::Park
        );
        process_events(&mut h);
        let started = Instant::now();
        assert_eq!(
            h.rig.agent.shutdown().parking,
            crate::lifecycle::Parking::Failed
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(h.rig.agent.home.twins.is_empty());
    }

    #[test]
    fn a_home_exit_capture_is_answered_after_the_events_of_its_own_activation() {
        let mut h = home();
        h.capture.lock().unwrap().during_begin = activation();
        h.rig.agent.events.send(Event::HomeBind).unwrap();
        begin(&mut h, true);
        assert!(h.rig.agent.pending.is_empty());
        process_events(&mut h);
        assert_eq!(fed(&h), ["started", "button", "key", "button", "begun"]);
        assert!(
            h.rig.agent.home.reload,
            "an unrelated event keeps its order"
        );
        assert!(h.rig.events.try_recv().is_err());
    }

    #[test]
    fn activation_does_not_overtake_an_earlier_lock_event() {
        let mut h = home();
        h.capture.lock().unwrap().during_begin = activation();
        h.rig
            .agent
            .events
            .send(Event::Input(Input::Session(
                crosspane_platform::SessionEvent::State(crosspane_platform::SessionState {
                    lock: crosspane_platform::LockState::Locked,
                    active: Some(true),
                }),
            )))
            .unwrap();
        begin(&mut h, true);
        process_events(&mut h);
        assert_eq!(
            fed(&h),
            ["session", "started", "button", "key", "button", "begun"]
        );
    }

    #[test]
    fn every_activation_event_precedes_the_answer_even_past_1024_events() {
        let mut h = home();
        let mut during = Vec::new();
        for _ in 0..1100 {
            during.push(CaptureEvent::Motion {
                dx: 1.0,
                dy: 0.0,
                kind: crosspane_platform::MotionKind::Unaccelerated,
                at: at(),
            });
        }
        during.extend(activation());
        h.capture.lock().unwrap().during_begin = during;
        begin(&mut h, true);
        process_events(&mut h);
        let order = fed(&h);
        assert!(order[..1100].iter().all(|label| *label == "motion"));
        assert_eq!(
            &order[1100..],
            ["started", "button", "key", "button", "begun"]
        );
        assert!(h.rig.events.try_recv().is_err());
    }

    #[test]
    fn a_refused_home_exit_capture_is_also_answered_after_its_events() {
        let mut h = home();
        h.rig.agent.platform.capture = None;
        begin(&mut h, true);
        process_events(&mut h);
        assert_eq!(fed(&h), ["begun"]);
        assert!(matches!(
            h.rig.agent.fed[0],
            Input::CaptureBegun {
                result: Err(Failure::Other),
                ..
            }
        ));
    }

    #[test]
    fn an_ordinary_capture_keeps_todays_order_with_the_answer_first() {
        let mut h = home();
        // A modifier held at activation and released during `begin()`.
        let start = CaptureStart {
            held_keys: vec![HidUsage::keyboard(0xE0)],
            lock_keys: LockKeys::default(),
        };
        {
            let mut log = h.capture.lock().unwrap();
            log.during_begin = activation();
            log.start = Some(start.clone());
        }
        begin(&mut h, false);
        h.rig.agent.settle();
        assert_eq!(fed(&h), ["begun"]);
        // The snapshot goes through untouched...
        assert!(matches!(
            &h.rig.agent.fed[0],
            Input::CaptureBegun { result: Ok(s), .. } if *s == start
        ));
        // ...and the events of the activation stay queued behind it, in order, for the loop.
        let queued: Vec<_> = h.rig.events.try_iter().collect();
        assert!(matches!(
            queued.as_slice(),
            [
                Event::Input(Input::Capture(CaptureEvent::Started { .. })),
                Event::Input(Input::Capture(CaptureEvent::Button { down: true, .. })),
                Event::Input(Input::Capture(CaptureEvent::Key { down: false, .. })),
                Event::Input(Input::Capture(CaptureEvent::Button { down: false, .. })),
            ]
        ));
    }

    // ---- ReleaseAndWarp: the read-back, then the gate (A3, B2, B3) ----

    #[test]
    fn a_warp_is_done_only_when_the_pointer_reads_back_on_the_target_and_the_gate_is_open() {
        let mut h = home();
        h.compositor.lock().unwrap().cursor = Some(TARGET);
        assert_eq!(warp(&mut h, TARGET), Ok(Warp::Done));
        assert_eq!(h.capture.lock().unwrap().ends, [Some(TARGET)]);
    }

    #[test]
    fn a_warp_tolerates_two_device_pixels_and_no_more() {
        let mut h = home();
        let near =
            |dx: f64, dy: f64| Some((TARGET.0, PointDevice::new(TARGET.1.x + dx, TARGET.1.y + dy)));
        for (dx, dy, want) in [
            (2.0, 0.0, Ok(Warp::Done)),
            (0.0, -2.0, Ok(Warp::Done)),
            (1.5, 1.5, Ok(Warp::Done)),
            (2.5, 0.0, Ok(Warp::Skipped)),
            (0.0, 3.0, Ok(Warp::Skipped)),
        ] {
            h.compositor.lock().unwrap().cursor = near(dx, dy);
            assert_eq!(warp(&mut h, TARGET), want, "({dx}, {dy}) off");
        }
        // The right place on the wrong display is not there.
        h.compositor.lock().unwrap().cursor = Some((DisplayId(1), TARGET.1));
        assert_eq!(warp(&mut h, TARGET), Ok(Warp::Skipped));
    }

    #[test]
    fn the_logged_home_warp_is_within_two_integer_device_pixels() {
        let mut h = home();
        let to = (
            DisplayId(3),
            PointDevice::new(1202.5749006681976, 56.2041219764659),
        );
        h.compositor.lock().unwrap().cursor = Some((DisplayId(3), PointDevice::new(1202.0, 54.0)));
        assert_eq!(warp(&mut h, to), Ok(Warp::Done));
        h.compositor.lock().unwrap().cursor = Some((DisplayId(3), PointDevice::new(1202.0, 53.0)));
        assert_eq!(warp(&mut h, to), Ok(Warp::Skipped));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_logged_twin_readback_converts_floored_layout_pixels_to_device_pixels() {
        use crosspane_platform_linux::hyprland::{cursor_position, ipc::HyprIpc};
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;

        let dir =
            std::env::temp_dir().join(format!("crosspane-home-cursor-{}", std::process::id()));
        let socket_dir = dir.join("hypr/test");
        std::fs::create_dir_all(&socket_dir).unwrap();
        let server = UnixListener::bind(socket_dir.join(".socket.sock")).unwrap();
        let thread = std::thread::spawn(move || {
            for (request, reply) in [
                ("j/cursorpos", json!({"x": 1049177, "y": 27})),
                (
                    "j/monitors",
                    json!([{"id": 3, "name": "CROSSPANE-a", "x": 1048576,
                    "y": 0, "width": 1710, "height": 1200, "scale": 2, "transform": 0,
                    "reserved": [0, 30, 0, 0]}]),
                ),
            ] {
                let (mut stream, _) = server.accept().unwrap();
                let mut buf = [0; 128];
                let n = stream.read(&mut buf).unwrap();
                assert_eq!(std::str::from_utf8(&buf[..n]).unwrap(), request);
                stream.write_all(reply.to_string().as_bytes()).unwrap();
            }
        });
        let ipc = HyprIpc::new("test", &dir, Duration::from_secs(1));
        assert_eq!(
            cursor_position(&ipc).unwrap(),
            (DisplayId(3), PointDevice::new(1202.0, 54.0))
        );
        thread.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_watchdog_rescues_a_twin_pointer_without_capture_and_uses_the_last_fallback() {
        let mut h = projected_scenario();
        let fallback = (DisplayId(1), PointDevice::new(123.0, 456.0));
        h.rig.agent.home.fallback = Some(fallback);
        h.compositor.lock().unwrap().cursor = Some(TARGET);
        let before = h.capture.lock().unwrap().ends.len();
        h.rig.agent.home_housekeeping();
        assert_eq!(h.capture.lock().unwrap().ends[before..], [Some(fallback)]);
        assert_eq!(h.compositor.lock().unwrap().cursor, Some(fallback));
        assert!(h.capture.lock().unwrap().active.is_none());
        assert!(!h.rig.agent.home.rescue_reported);
        h.rig.agent.home_housekeeping();
        assert_eq!(h.capture.lock().unwrap().ends.len(), before + 1);
        assert!(!h.rig.agent.home.rescue_reported);
    }

    #[test]
    fn the_watchdog_uses_the_physical_display_center_without_an_engine_fallback() {
        let mut h = projected_scenario();
        h.compositor.lock().unwrap().cursor = Some(TARGET);
        h.rig.agent.home_housekeeping();
        assert_eq!(
            h.compositor.lock().unwrap().cursor,
            Some((DisplayId(1), PointDevice::new(500.0, 500.0)))
        );
    }

    #[test]
    fn the_watchdog_leaves_active_home_and_entering_alone() {
        let mut h = home_scenario();
        let before = h.capture.lock().unwrap().ends.len();
        h.rig.agent.home_housekeeping();
        assert_eq!(h.capture.lock().unwrap().ends.len(), before);
        assert_eq!(h.compositor.lock().unwrap().cursor.unwrap().0, DisplayId(7));
        // Before focus commits, the bind transaction still marks a legitimate entry.
        let mut h = aimed_scenario();
        trigger(&mut h);
        assert!(h.rig.agent.home.now.is_none());
        assert!(h.rig.agent.home.active);
        let before = h.capture.lock().unwrap().ends.len();
        h.rig.agent.home_housekeeping();
        assert_eq!(h.capture.lock().unwrap().ends.len(), before);
    }

    #[test]
    fn a_clean_home_exit_keeps_capture_until_session_end_then_the_watchdog_rescues_once() {
        use crosspane_protocol::msg::EndReason;
        use std::io::Write;

        struct LogWriter(Arc<Mutex<Vec<u8>>>);
        impl Write for LogWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        for cause in 0..3 {
            let logs = Arc::new(Mutex::new(Vec::new()));
            let writer = logs.clone();
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_max_level(tracing::Level::WARN)
                .with_writer(move || LogWriter(writer.clone()))
                .finish();
            tracing::subscriber::with_default(subscriber, || {
                let mut h = home_scenario();
                exit_home(&mut h);
                assert!(!h.compositor.lock().unwrap().ours);
                assert!(h.rig.agent.home_capture_active());
                assert_eq!(h.compositor.lock().unwrap().cursor.unwrap().0, DisplayId(7));
                let before = h.capture.lock().unwrap().ends.len();
                h.rig.agent.home_housekeeping();
                assert_eq!(h.capture.lock().unwrap().ends.len(), before);
                assert!(!h.rig.agent.home.rescue_reported);
                // Make the ordinary return warp unconfirmed, leaving the cursor on the twin.
                h.capture.lock().unwrap().warp_cursor = None;
                let session = h
                    .rig
                    .agent
                    .emitted
                    .iter()
                    .find_map(|o| match o {
                        Output::SendControl {
                            msg: ControlMessage::StartControl { session, .. },
                            ..
                        } => Some(*session),
                        _ => None,
                    })
                    .unwrap();
                let ended = match cause {
                    0 => Input::Command(Command::ReleaseControl),
                    1 => control_input(
                        h.rig.peer,
                        ControlMessage::EndControl {
                            session,
                            reason: EndReason::Released,
                        },
                    ),
                    _ => Input::Link(LinkEvent::Closed {
                        peer: h.rig.peer,
                        error: crosspane_protocol::link::LinkError::Closed,
                    }),
                };
                step(&mut h, ended);
                assert!(!h.rig.agent.home_capture_active());
                assert_eq!(h.compositor.lock().unwrap().cursor.unwrap().0, DisplayId(7));
                h.capture.lock().unwrap().warp_cursor = Some(h.compositor.clone());
                let before = h.capture.lock().unwrap().ends.len();
                h.rig.agent.home_housekeeping();
                assert_eq!(h.capture.lock().unwrap().ends.len(), before + 1);
                assert_eq!(h.compositor.lock().unwrap().cursor.unwrap().0, DisplayId(1));
                h.rig.agent.home_housekeeping();
                assert_eq!(h.capture.lock().unwrap().ends.len(), before + 1);
            });
            let text = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
            assert_eq!(
                text.matches("the pointer is on a twin outside home")
                    .count(),
                1,
                "cause {cause}: {text}"
            );
        }
    }

    #[test]
    fn a_failed_entry_keeps_its_bind_until_the_watchdog_confirms_a_physical_fallback() {
        let mut h = aimed_scenario();
        h.compositor.lock().unwrap().cursor = Some(TARGET);
        h.capture.lock().unwrap().warp_cursor = None;
        let out = trigger(&mut h);
        assert!(out.iter().any(|o| matches!(
            o,
            Output::Notice(Notice::HomeFailed {
                reason: HomeFailure::Warp,
                ..
            })
        )));
        assert!(h.compositor.lock().unwrap().ours);
        assert!(h.rig.agent.home.wanted.is_some());
        assert!(h.rig.agent.home.pointer_unsafe);
        assert!(!h.rig.agent.home_capture_active());
        assert_eq!(h.compositor.lock().unwrap().cursor, Some(TARGET));
        h.capture.lock().unwrap().warp_cursor = Some(h.compositor.clone());
        h.rig.agent.home_housekeeping();
        process_events(&mut h);
        h.rig.agent.settle();
        assert_eq!(h.compositor.lock().unwrap().cursor.unwrap().0, DisplayId(1));
        assert!(!h.compositor.lock().unwrap().ours);
        assert!(h.rig.agent.home.wanted.is_none());
    }

    #[test]
    fn a_failed_clean_exit_removal_requires_fresh_safety_after_capture_ends() {
        let mut h = home_scenario();
        h.compositor.lock().unwrap().remove_error = Some("temporary removal failure".into());
        exit_home(&mut h);
        assert!(h.rig.agent.home_capture_active());
        assert!(h.compositor.lock().unwrap().ours);
        assert!(h.rig.agent.home.removal.is_some());
        assert!(h.rig.agent.home.pointer_unsafe);
        h.capture.lock().unwrap().warp_cursor = None;
        h.compositor.lock().unwrap().remove_error = None;
        let reads = h.compositor.lock().unwrap().cursor_reads;
        step(&mut h, Input::Command(Command::ReleaseControl));
        assert!(!h.rig.agent.home_capture_active());
        assert_eq!(h.compositor.lock().unwrap().cursor.unwrap().0, DisplayId(7));
        assert!(h.compositor.lock().unwrap().cursor_reads > reads);
        assert!(h.compositor.lock().unwrap().ours);
        assert!(h.rig.agent.home.removal.is_some());
        // A failed fallback must not convert the old capture exemption into physical safety.
        h.rig.agent.home_confirm_physical();
        assert!(h.compositor.lock().unwrap().ours);
        h.compositor.lock().unwrap().cursor = Some((DisplayId(1), PointDevice::zero()));
        h.rig.agent.home_confirm_physical();
        assert!(!h.compositor.lock().unwrap().ours);
        assert!(h.rig.agent.home.removal.is_none());
    }

    #[test]
    fn a_transient_reinstall_failure_keeps_the_unsafe_bind_obligation_and_foreign_ownership() {
        for foreign in [false, true] {
            let mut h = home_scenario();
            h.capture.lock().unwrap().warp_cursor = None;
            step(&mut h, Input::Command(Command::ReleaseControl));
            let wanted = h.rig.agent.home.wanted;
            assert!(wanted.is_some());
            {
                let mut c = h.compositor.lock().unwrap();
                c.ours = false;
                c.foreign = foreign;
                if !foreign {
                    c.install_error = Some("transient reinstall failure".into());
                }
            }
            h.rig.agent.home.reload = true;
            h.rig.agent.home_housekeeping();
            h.rig.agent.settle();
            assert_eq!(h.rig.agent.home.wanted, wanted);
            assert!(!h.compositor.lock().unwrap().ours);
            assert_eq!(h.compositor.lock().unwrap().foreign, foreign);
            {
                let mut c = h.compositor.lock().unwrap();
                // The owner removes the collision; Crosspane has never removed it.
                c.foreign = false;
                c.install_error = None;
            }
            h.rig.agent.home.last_check = Instant::now() - BIND_CHECK;
            h.rig.agent.home_housekeeping();
            assert!(h.compositor.lock().unwrap().ours);
            assert_eq!(h.rig.agent.home.wanted, wanted);
            assert_eq!(h.compositor.lock().unwrap().cursor.unwrap().0, DisplayId(7));
        }
    }

    #[test]
    fn shutdown_rechecks_physical_safety_after_final_parking_recovery() {
        let mut h = home_scenario();
        h.capture.lock().unwrap().warp_cursor = None;
        replace_parking(
            &mut h.rig.agent,
            Box::new(RestoreFailureParking(h.compositor.clone())),
        );
        h.rig.agent.shutdown();
        let c = h.compositor.lock().unwrap();
        assert!(c.restores > 0);
        assert_eq!(c.recoveries, 1);
        assert_eq!(c.cursor.unwrap().0, DisplayId(1));
        assert!(!c.ours);
        assert!(!h.rig.agent.home.pointer_unsafe);
        assert!(h.rig.agent.home.wanted.is_none());
    }

    #[test]
    fn a_physical_output_reusing_a_partially_restored_twin_id_is_not_rescued_and_is_pruned() {
        let mut h = projected_scenario();
        replace_parking(
            &mut h.rig.agent,
            Box::new(RestoreFailureParking(h.compositor.clone())),
        );
        h.rig.agent.execute(vec![Output::Restore {
            window: WindowId(10),
        }]);
        process_events(&mut h);
        assert_eq!(h.compositor.lock().unwrap().restores, 1);
        assert_eq!(
            h.rig.agent.home.twins.get(&WindowId(10)),
            Some(&DisplayId(7))
        );
        {
            let mut c = h.compositor.lock().unwrap();
            c.physical.push(display(7, 1.0, (0.0, 0.0), (1000, 1000)));
            c.cursor = Some(TARGET);
        }
        let ends = h.capture.lock().unwrap().ends.len();
        h.rig.agent.home_housekeeping();
        assert_eq!(h.capture.lock().unwrap().ends.len(), ends);
        assert_eq!(h.compositor.lock().unwrap().cursor, Some(TARGET));
        assert!(h.rig.agent.home.twins.is_empty());
        assert!(!h.rig.agent.home.rescue_reported);
    }

    #[test]
    fn watchdog_event_bursts_read_once_per_interval_and_rescue_reads_exactly_twice() {
        let mut h = projected_scenario();
        h.compositor.lock().unwrap().cursor = Some((DisplayId(1), PointDevice::zero()));
        let reads = h.compositor.lock().unwrap().cursor_reads;
        let displays = h.compositor.lock().unwrap().display_reads;
        for _ in 0..1000 {
            h.rig.agent.home_housekeeping();
        }
        assert_eq!(h.compositor.lock().unwrap().cursor_reads - reads, 1);
        assert_eq!(h.compositor.lock().unwrap().display_reads, displays);
        h.rig.agent.home.watchdog_next = Instant::now();
        h.rig.agent.home_housekeeping();
        assert_eq!(h.compositor.lock().unwrap().cursor_reads - reads, 2);
        h.compositor.lock().unwrap().cursor = Some(TARGET);
        h.rig.agent.home.watchdog_next = Instant::now();
        h.rig.agent.home_housekeeping();
        assert_eq!(h.compositor.lock().unwrap().cursor_reads - reads, 4);
        assert_eq!(h.compositor.lock().unwrap().display_reads - displays, 1);
        assert!(!h.rig.agent.home.rescue_reported);
        assert!(!h.rig.agent.home.pointer_unsafe);
    }

    #[test]
    fn every_watchdog_exemption_precedes_all_cursor_and_display_ipc() {
        let mut home = home_scenario();
        let mut entering = aimed_scenario();
        trigger(&mut entering);
        let mut captured = home_scenario();
        exit_home(&mut captured);
        let mut locked = projected_scenario();
        locked.gate.set_session_permits(false);
        for h in [&mut home, &mut entering, &mut captured, &mut locked] {
            let reads = h.compositor.lock().unwrap().cursor_reads;
            let displays = h.compositor.lock().unwrap().display_reads;
            h.rig.agent.home.watchdog_next = Instant::now();
            for _ in 0..1000 {
                h.rig.agent.home_housekeeping();
            }
            assert_eq!(h.compositor.lock().unwrap().cursor_reads, reads);
            assert_eq!(h.compositor.lock().unwrap().display_reads, displays);
        }
    }

    #[test]
    fn the_receive_timeout_wakes_for_the_watchdog_and_suspends_it_while_exempt() {
        let mut h = projected_scenario();
        // Use the exact timeout function the receive loop calls. No earlier engine deadline
        // should shorten this watchdog deadline in the idle projection fixture.
        let engine_now = MonoTime::from_nanos(0);
        let clock_now = Instant::now();
        h.rig.agent.home.watchdog_next = clock_now + HOME_WATCHDOG;
        assert_eq!(
            h.rig.agent.receive_timeout(engine_now, clock_now),
            HOME_WATCHDOG
        );
        assert_eq!(
            h.rig
                .agent
                .receive_timeout(engine_now, clock_now + HOME_WATCHDOG),
            Duration::ZERO
        );
        // An expired watchdog is suspended in every exempt state, so it cannot busy-loop.
        h.rig.agent.home.watchdog_next = clock_now;
        h.rig.agent.home.active = true;
        assert_eq!(
            h.rig.agent.receive_timeout(engine_now, clock_now),
            HOUSEKEEPING
        );
        h.rig.agent.home.active = false;
        h.gate.set_session_permits(false);
        assert_eq!(
            h.rig.agent.receive_timeout(engine_now, clock_now),
            HOUSEKEEPING
        );
        h.gate.set_session_permits(true);
        h.rig.agent.home.twins.clear();
        assert_eq!(
            h.rig.agent.receive_timeout(engine_now, clock_now),
            HOUSEKEEPING
        );
        h.rig.agent.home.pointer_unsafe = true;
        assert_eq!(
            h.rig.agent.receive_timeout(engine_now, clock_now),
            Duration::ZERO
        );
        h.rig.agent.home.pointer_unsafe = false;
        h.rig.agent.home.removal = Some(HomeOp(900));
        assert_eq!(
            h.rig.agent.receive_timeout(engine_now, clock_now),
            Duration::ZERO
        );
        h.rig.agent.platform.home = None;
        assert_eq!(
            h.rig.agent.receive_timeout(engine_now, clock_now),
            HOUSEKEEPING
        );

        let mut h = home_scenario();
        exit_home(&mut h);
        assert!(h.rig.agent.home_capture_active());
        h.rig.agent.home.watchdog_next = clock_now + HOUSEKEEPING;
        let engine_timeout = h.rig.agent.receive_timeout(engine_now, clock_now);
        assert!(!engine_timeout.is_zero());
        h.rig.agent.home.watchdog_next = clock_now;
        assert_eq!(
            h.rig.agent.receive_timeout(engine_now, clock_now),
            engine_timeout,
            "the live capture suspends the expired watchdog while keeping earlier engine deadlines"
        );
    }

    #[test]
    fn rescue_uses_current_physical_displays_when_subscription_data_is_empty_or_stale() {
        for empty in [true, false] {
            let mut h = aimed_scenario();
            h.compositor.lock().unwrap().cursor = Some(TARGET);
            h.capture.lock().unwrap().warp_cursor = None;
            trigger(&mut h);
            assert!(h.rig.agent.home.pointer_unsafe);
            assert!(h.compositor.lock().unwrap().ours);
            start_request(&mut h, Refusal::Busy);
            h.rig.agent.local_displays = if empty {
                Vec::new()
            } else {
                vec![display(99, 1.0, (0.0, 0.0), (1000, 1000))]
            };
            h.compositor.lock().unwrap().physical = vec![display(2, 1.0, (0.0, 0.0), (800, 600))];
            h.capture.lock().unwrap().warp_cursor = Some(h.compositor.clone());
            let reads = h.compositor.lock().unwrap().cursor_reads;
            h.rig.agent.home.watchdog_next = Instant::now();
            h.rig.agent.home_housekeeping();
            assert_eq!(h.compositor.lock().unwrap().cursor_reads - reads, 2);
            assert_eq!(
                h.compositor.lock().unwrap().cursor,
                Some((DisplayId(2), PointDevice::new(400.0, 300.0)))
            );
            assert!(!h.rig.agent.home.pointer_unsafe);
            assert!(!h.rig.agent.home.rescue_reported);
            assert!(!h.compositor.lock().unwrap().ours);
            assert!(h.rig.agent.home.removal.is_none());
            assert!(h.rig.agent.home.wanted.is_none());
            assert!(h.rig.agent.home.physical.contains(&DisplayId(2)));
            h.rig.agent.settle();
            // The same verified physical ID remains classifiable on subsequent ticks, even
            // though the event subscription still has no usable snapshot.
            let displays = h.compositor.lock().unwrap().display_reads;
            let reads = h.compositor.lock().unwrap().cursor_reads;
            h.rig.agent.home.watchdog_next = Instant::now();
            h.rig.agent.home_housekeeping();
            assert_eq!(h.compositor.lock().unwrap().cursor_reads - reads, 1);
            assert_eq!(h.compositor.lock().unwrap().display_reads, displays);
            let out = step(
                &mut h,
                control_input(
                    NodeId([3; 32]),
                    ControlMessage::StartControl {
                        session: crosspane_types::id::SessionId(78),
                        entry_display: DisplayId(1),
                        entry: PointDevice::new(1.0, 1.0),
                        lock_keys: LockKeys::default(),
                    },
                ),
            );
            assert!(
                out.iter().any(|output| matches!(
                    output,
                    Output::SendControl {
                        msg: ControlMessage::ControlStarted { .. },
                        ..
                    }
                )),
                "{out:?}"
            );
        }
    }

    #[test]
    fn exhausted_retries_and_last_twin_restore_still_confirm_safety_and_lift_the_fence() {
        let mut h = aimed_scenario();
        h.compositor.lock().unwrap().cursor = Some(TARGET);
        h.capture.lock().unwrap().warp_cursor = None;
        trigger(&mut h);
        for _ in 0..12 {
            tick(&mut h, 1001);
        }
        assert!(
            tick(&mut h, 5000)
                .iter()
                .all(|output| !matches!(output, Output::ReleaseAndWarp { .. })),
            "the engine's stranded retry budget must be exhausted"
        );
        assert!(h.rig.agent.home.pointer_unsafe);
        assert!(h.compositor.lock().unwrap().ours);
        start_request(&mut h, Refusal::Busy);
        let local = h.rig.local;
        let out = step(
            &mut h,
            Input::Command(Command::Return(ProjectionKey {
                source: local,
                projection: ProjectionId(1),
            })),
        );
        assert!(out.iter().any(|output| matches!(
            output,
            Output::Restore {
                window: WindowId(10)
            }
        )));
        assert!(h.rig.agent.home.twins.is_empty());
        // The owner's recovery/restore moved the pointer. Keep the subscription empty too:
        // the outstanding obligation needs a current physical lookup, with just one cursor read.
        h.compositor.lock().unwrap().cursor = Some((DisplayId(1), PointDevice::zero()));
        h.rig.agent.local_displays.clear();
        let reads = h.compositor.lock().unwrap().cursor_reads;
        h.rig.agent.home.watchdog_next = Instant::now();
        h.rig.agent.home_housekeeping();
        assert_eq!(h.compositor.lock().unwrap().cursor_reads - reads, 1);
        assert!(!h.rig.agent.home.pointer_unsafe);
        assert!(!h.compositor.lock().unwrap().ours);
        assert!(h.rig.agent.home.removal.is_none());
        assert!(h.rig.agent.home.wanted.is_none());
        h.rig.agent.settle();
        let peer = NodeId([3; 32]);
        let out = step(
            &mut h,
            control_input(
                peer,
                ControlMessage::StartControl {
                    session: crosspane_types::id::SessionId(78),
                    entry_display: DisplayId(1),
                    entry: PointDevice::new(1.0, 1.0),
                    lock_keys: LockKeys::default(),
                },
            ),
        );
        assert!(
            out.iter().any(|output| matches!(
                output,
                Output::SendControl {
                    msg: ControlMessage::ControlStarted { .. },
                    ..
                }
            )),
            "{out:?}"
        );
        assert_eq!(h.rig.agent.engine.controlled_by(), Some(peer));
    }

    #[test]
    fn two_twin_incidents_each_warn_without_an_intervening_physical_poll() {
        use std::io::Write;
        struct Writer(Arc<Mutex<Vec<u8>>>);
        impl Write for Writer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let logs = Arc::new(Mutex::new(Vec::new()));
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || Writer(writer.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let mut h = projected_scenario();
            for _ in 0..2 {
                h.compositor.lock().unwrap().cursor = Some(TARGET);
                h.rig.agent.home.watchdog_next = Instant::now();
                h.rig.agent.home_housekeeping();
                assert_eq!(h.compositor.lock().unwrap().cursor.unwrap().0, DisplayId(1));
                assert!(!h.rig.agent.home.rescue_reported);
            }
        });
        let text = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        assert_eq!(
            text.matches("the pointer is on a twin outside home")
                .count(),
            2,
            "{text}"
        );
    }

    #[test]
    fn the_watchdog_never_warps_while_locked_or_when_the_gate_closes_during_readback() {
        let mut h = projected_scenario();
        h.compositor.lock().unwrap().cursor = Some(TARGET);
        h.gate.set_session_permits(false);
        let before = h.capture.lock().unwrap().ends.len();
        h.rig.agent.home_housekeeping();
        assert_eq!(h.capture.lock().unwrap().ends.len(), before);
        h.gate.set_session_permits(true);
        let gate = h.gate.clone();
        h.compositor.lock().unwrap().during_cursor =
            Some(Box::new(move || gate.set_session_permits(false)));
        h.rig.agent.home_housekeeping();
        assert_eq!(h.capture.lock().unwrap().ends.len(), before);
        assert_eq!(h.compositor.lock().unwrap().cursor, Some(TARGET));
    }

    #[test]
    fn the_bind_stays_verified_until_a_fallback_reads_back_on_a_physical_display() {
        let mut h = home_scenario();
        let fallback = (DisplayId(1), PointDevice::new(500.0, 500.0));
        h.capture.lock().unwrap().warp_cursor = None;
        step(&mut h, Input::Command(Command::ReleaseControl));
        assert!(h.compositor.lock().unwrap().ours);
        assert!(h.rig.agent.home.wanted.is_some());
        assert!(h.rig.agent.home.pointer_unsafe);
        // The retained shortcut is still checked and restored after a compositor reload.
        h.compositor.lock().unwrap().ours = false;
        h.rig.agent.home.reload = true;
        h.rig.agent.home_housekeeping();
        assert!(h.compositor.lock().unwrap().ours);
        assert_eq!(bind(&mut h, 99, false), Err(Failure::Other));
        h.capture.lock().unwrap().warp_cursor = Some(h.compositor.clone());
        h.rig.agent.home.watchdog_next = Instant::now();
        h.rig.agent.home_housekeeping();
        assert_eq!(h.compositor.lock().unwrap().cursor, Some(fallback));
        assert!(!h.rig.agent.home.pointer_unsafe);
        tick(&mut h, 1000);
        assert!(!h.compositor.lock().unwrap().ours);
        assert!(h.rig.agent.home.wanted.is_none());
    }

    #[test]
    fn a_failed_entry_notice_names_the_confirmation_failure_and_the_actual_gate() {
        let mut h = home();
        let key = key(&h);
        h.rig.agent.notice(&Notice::HomeFailed {
            key,
            reason: HomeFailure::Warp,
        });
        assert!(last_notice(&h).contains("could not confirm the pointer position"));
        assert!(!last_notice(&h).contains("locked"));
        assert!(!last_notice(&h).contains("gate is closed"));
        h.gate.set_session_permits(false);
        h.rig.agent.notice(&Notice::HomeFailed {
            key,
            reason: HomeFailure::Warp,
        });
        assert!(last_notice(&h).contains("the input gate is closed"));
    }

    #[test]
    fn a_closed_gate_wins_over_agreeing_coordinates() {
        let mut h = home();
        h.compositor.lock().unwrap().cursor = Some(TARGET);
        // The session side (a lock) or the engine side (a panic) is closed.
        h.gate.set_session_permits(false);
        assert_eq!(warp(&mut h, TARGET), Ok(Warp::Skipped));
        h.gate.set_session_permits(true);
        assert_eq!(warp(&mut h, TARGET), Ok(Warp::Done));
        h.gate.set_engine_permits(false);
        assert_eq!(warp(&mut h, TARGET), Ok(Warp::Skipped));
    }

    #[test]
    fn a_gate_that_closes_between_end_and_the_read_back_is_skipped_never_done() {
        let mut h = home();
        h.compositor.lock().unwrap().cursor = Some(TARGET);
        // The lock arrives after `end()` returned `Ok`, and the engine hasn't been told yet
        // (session-state delivery is delayed): the read-back still agrees with the target.
        let gate = h.gate.clone();
        h.compositor.lock().unwrap().during_cursor =
            Some(Box::new(move || gate.set_session_permits(false)));
        assert_eq!(warp(&mut h, TARGET), Ok(Warp::Skipped));
        assert!(
            !h.rig
                .agent
                .fed
                .iter()
                .any(|i| matches!(i, Input::Session(_))),
            "the engine was told of no lock: the answer came from the gate itself"
        );
    }

    #[test]
    fn a_gate_that_closes_inside_end_is_skipped_too() {
        let mut h = home();
        h.compositor.lock().unwrap().cursor = Some(TARGET);
        let gate = h.gate.clone();
        h.capture.lock().unwrap().during_end =
            Some(Box::new(move || gate.set_engine_permits(false)));
        assert_eq!(warp(&mut h, TARGET), Ok(Warp::Skipped));
    }

    #[test]
    fn a_warp_that_cannot_be_confirmed_is_an_error_never_done() {
        let mut h = home();
        // The read-back fails.
        assert_eq!(warp(&mut h, TARGET), Err(Failure::Other));
        // `end` fails: the error is the engine's own kind of failure.
        h.compositor.lock().unwrap().cursor = Some(TARGET);
        h.capture.lock().unwrap().end_error = Some(|| PlatformError::Locked);
        assert_eq!(warp(&mut h, TARGET), Err(Failure::Locked));
        h.capture.lock().unwrap().end_error = None;
        // Nothing to read from (no Hyprland: the Mac), or nothing to end with.
        h.rig.agent.platform.home = None;
        assert_eq!(warp(&mut h, TARGET), Err(Failure::Other));
        h.rig.agent.platform.capture = None;
        assert_eq!(warp(&mut h, TARGET), Err(Failure::Other));
    }

    // ---- SetPortals: the backend says why it failed (B1) ----

    fn portal(id: u32) -> CapturePortal {
        CapturePortal {
            id: PortalId(id),
            display: DisplayId(1),
            edge: crosspane_platform::Edge::Left,
            from: 0.0,
            to: 10.0,
        }
    }

    fn portals_set(h: &mut Home) -> (Vec<PortalId>, Result<(), PortalsFailure>) {
        h.rig.agent.execute(vec![Output::SetPortals(vec![
            portal(1 << 30),
            portal((1 << 30) + 1),
        ])]);
        match h.rig.agent.pending.pop_back() {
            Some(Input::PortalsSet { ids, result }) => (ids, result),
            other => panic!("expected PortalsSet, got {other:?}"),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_failed_set_portals_is_rejected_only_when_the_worker_said_so() {
        use crosspane_platform_linux::hyprland::capture::PORTALS_REJECTED;
        let mut h = home();
        let ids = vec![PortalId(1 << 30), PortalId((1 << 30) + 1)];
        assert_eq!(portals_set(&mut h), (ids.clone(), Ok(())));
        h.capture.lock().unwrap().set_error =
            Some(|| PlatformError::Backend(format!("{PORTALS_REJECTED}no such display")));
        assert_eq!(
            portals_set(&mut h),
            (ids.clone(), Err(PortalsFailure::Rejected))
        );
        // The caller's receive timeout, a worker that is gone, and anything unmarked: unknown.
        for error in [
            (|| PlatformError::Timeout) as fn() -> PlatformError,
            || PlatformError::Backend("Hyprland capture: worker gone".into()),
            || PlatformError::NotFound,
        ] {
            h.capture.lock().unwrap().set_error = Some(error);
            assert_eq!(
                portals_set(&mut h),
                (ids.clone(), Err(PortalsFailure::Uncertain))
            );
        }
        // No capture backend: nothing was installed.
        h.rig.agent.platform.capture = None;
        assert_eq!(portals_set(&mut h), (ids, Err(PortalsFailure::Rejected)));
    }

    /// The macOS backend has no marker: a timeout and `Backend(..)` leave the capture's fate
    /// unknown, everything else is a refusal up front.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn a_failed_set_portals_off_hyprland_is_uncertain_unless_refused_up_front() {
        let mut h = home();
        let ids = vec![PortalId(1 << 30), PortalId((1 << 30) + 1)];
        for (error, want) in [
            (
                (|| PlatformError::Timeout) as fn() -> PlatformError,
                PortalsFailure::Uncertain,
            ),
            (
                || PlatformError::Backend("tap gone".into()),
                PortalsFailure::Uncertain,
            ),
            (|| PlatformError::NotFound, PortalsFailure::Rejected),
            (
                || PlatformError::Unsupported("no"),
                PortalsFailure::Rejected,
            ),
        ] {
            h.capture.lock().unwrap().set_error = Some(error);
            assert_eq!(portals_set(&mut h), (ids.clone(), Err(want)));
        }
    }

    // ---- the home bind ----

    #[test]
    fn installing_binds_verifies_and_the_status_says_so() {
        let mut h = home();
        let status = h.rig.agent.status();
        assert_eq!(
            status["home"],
            json!({ "projection": null, "bind_installed": false })
        );
        assert_eq!(bind(&mut h, 1, true), Ok(()));
        assert!(h.compositor.lock().unwrap().ours);
        assert_eq!(h.rig.agent.home.wanted, Some(HomeOp(1)));
        assert_eq!(h.rig.agent.status()["home"]["bind_installed"], json!(true));
    }

    #[test]
    fn a_failed_install_is_an_error_that_names_its_cause_in_the_notice() {
        let mut h = home();
        h.compositor.lock().unwrap().install_error = Some("boom".into());
        assert_eq!(bind(&mut h, 1, true), Err(Failure::Other));
        assert_eq!(h.rig.agent.home.wanted, None, "nothing to keep verified");
        assert_eq!(h.rig.agent.home.present, None, "part of it may be there");
        // The engine emits rollback before the failure notice.
        assert_eq!(bind(&mut h, 1, false), Ok(()));
        let key = key(&h);
        h.rig.agent.notice(&Notice::HomeFailed {
            key,
            reason: HomeFailure::Bind,
        });
        let text = last_notice(&h);
        assert!(text.contains("boom") && text.contains(KEYS), "{text}");
    }

    #[test]
    fn a_foreign_binding_on_the_chord_refuses_the_install() {
        let mut h = home();
        h.compositor.lock().unwrap().foreign = true;
        assert_eq!(bind(&mut h, 1, true), Err(Failure::Other));
        let c = h.compositor.lock().unwrap();
        assert!(c.foreign && !c.ours, "the owner's binding is untouched");
        assert!(
            h.rig
                .agent
                .home
                .error
                .as_ref()
                .unwrap()
                .contains("another binding")
        );
    }

    #[test]
    fn without_a_home_seat_installing_fails_and_removing_succeeds() {
        let mut h = home();
        h.rig.agent.platform.home = None;
        assert_eq!(bind(&mut h, 1, true), Err(Failure::Other));
        assert_eq!(bind(&mut h, 1, false), Ok(()));
        assert_eq!(h.rig.agent.home.wanted, None);
    }

    #[test]
    fn removal_is_verified_idempotent_and_ends_the_verification() {
        let mut h = home();
        assert_eq!(bind(&mut h, 1, true), Ok(()));
        assert_eq!(bind(&mut h, 2, false), Ok(()));
        assert!(!h.compositor.lock().unwrap().ours);
        assert_eq!(h.rig.agent.home.wanted, None);
        assert_eq!(h.rig.agent.home.present, Some(false));
        assert_eq!(
            bind(&mut h, 3, false),
            Ok(()),
            "a second removal is a no-op"
        );
        assert_eq!(h.rig.agent.status()["home"]["bind_installed"], json!(false));
    }

    #[test]
    fn a_failing_removal_is_an_error_and_the_user_hears_of_it_once_per_episode() {
        let mut h = home();
        h.compositor.lock().unwrap().remove_error = Some("ipc down".into());
        let before = h.rig.agent.notices.len();
        for op in 1..=3 {
            assert_eq!(bind(&mut h, op, false), Err(Failure::Other));
        }
        assert_eq!(
            h.rig.agent.notices.len(),
            before + 1,
            "one notice for three attempts"
        );
        let text = last_notice(&h);
        assert!(
            text.contains("can't remove its release shortcut") && text.contains("ipc down"),
            "{text}"
        );
        // The bind may still be there: unknown, never "absent".
        assert_eq!(h.rig.agent.home.present, None);
        // A success ends the episode; the next failure is news again.
        h.compositor.lock().unwrap().remove_error = None;
        assert_eq!(bind(&mut h, 4, false), Ok(()));
        h.compositor.lock().unwrap().remove_error = Some("ipc down".into());
        assert_eq!(bind(&mut h, 5, false), Err(Failure::Other));
        assert_eq!(h.rig.agent.notices.len(), before + 2);
    }

    #[test]
    fn a_removal_never_touches_a_foreign_binding() {
        let mut h = home();
        // Ours and a foreign one share the chord: refused, both stay.
        {
            let mut c = h.compositor.lock().unwrap();
            c.ours = true;
            c.foreign = true;
        }
        assert_eq!(bind(&mut h, 1, false), Err(Failure::Other));
        {
            let c = h.compositor.lock().unwrap();
            assert!(c.ours && c.foreign);
        }
        // A config reload drops runtime binds; the next retry finds ours gone: confirmed.
        h.compositor.lock().unwrap().ours = false;
        assert_eq!(bind(&mut h, 2, false), Ok(()));
        assert!(h.compositor.lock().unwrap().foreign);
    }

    #[test]
    fn a_reload_that_drops_the_bind_installs_it_again_silently() {
        let mut h = home();
        assert_eq!(bind(&mut h, 1, true), Ok(()));
        let notices = h.rig.agent.notices.len();
        h.compositor.lock().unwrap().ours = false; // `hyprctl reload` clears runtime binds
        h.rig.agent.on_event(Event::HomeBind);
        h.rig.agent.home_housekeeping();
        assert!(h.compositor.lock().unwrap().ours, "reinstalled");
        assert!(h.rig.agent.pending.is_empty(), "the engine is told nothing");
        assert_eq!(h.rig.agent.notices.len(), notices);
        assert_eq!(h.rig.agent.home.wanted, Some(HomeOp(1)));
        assert_eq!(h.rig.agent.home.present, Some(true));
    }

    #[test]
    fn a_bind_that_cannot_be_installed_again_ends_home() {
        let mut h = home();
        assert_eq!(bind(&mut h, 9, true), Ok(()));
        {
            let mut c = h.compositor.lock().unwrap();
            c.ours = false;
            c.install_error = Some("still gone".into());
        }
        h.rig.agent.on_event(Event::HomeBind);
        h.rig.agent.home_housekeeping();
        // The engine hears it, for the operation that installed the bind.
        match h.rig.agent.pending.pop_back() {
            Some(Input::HomeBindSet {
                op,
                install: true,
                result: Err(Failure::Other),
            }) => assert_eq!(op, HomeOp(9)),
            other => panic!("expected an error answer, got {other:?}"),
        }
        // Said once: the next pass finds nothing wanted.
        assert_eq!(h.rig.agent.home.wanted, None);
        h.rig.agent.home.last_check = Instant::now() - Duration::from_secs(5);
        h.rig.agent.home_housekeeping();
        assert!(h.rig.agent.pending.is_empty());
    }

    #[test]
    fn a_foreign_binding_that_appears_while_home_ends_home() {
        let mut h = home();
        assert_eq!(bind(&mut h, 1, true), Ok(()));
        // The compositor now lists ours plus another binding on the chord: not exactly ours, and
        // the install refuses a chord somebody else uses.
        h.compositor.lock().unwrap().foreign = true;
        h.rig.agent.home.last_check = Instant::now() - Duration::from_secs(2);
        h.rig.agent.home_housekeeping();
        assert!(matches!(
            h.rig.agent.pending.pop_back(),
            Some(Input::HomeBindSet {
                install: true,
                result: Err(_),
                ..
            })
        ));
        assert!(h.compositor.lock().unwrap().foreign, "never removed");
    }

    #[test]
    fn the_bind_is_verified_every_second_while_wanted_and_never_otherwise() {
        let mut h = home();
        // Nothing wanted: no checks, however long it has been.
        h.rig.agent.home.last_check = Instant::now() - Duration::from_secs(60);
        h.rig.agent.home_housekeeping();
        assert_eq!(h.compositor.lock().unwrap().checks, 0);

        assert_eq!(bind(&mut h, 1, true), Ok(()));
        h.rig.agent.home.last_check = Instant::now() + Duration::from_secs(3600);
        h.rig.agent.home_housekeeping();
        assert_eq!(
            h.compositor.lock().unwrap().checks,
            0,
            "just installed: under a second"
        );
        h.rig.agent.home.last_check = Instant::now() - Duration::from_millis(1100);
        h.rig.agent.home_housekeeping();
        assert_eq!(h.compositor.lock().unwrap().checks, 1);
        h.rig.agent.home.last_check = Instant::now() + Duration::from_secs(3600);
        h.rig.agent.home_housekeeping();
        assert_eq!(
            h.compositor.lock().unwrap().checks,
            1,
            "and not again at once"
        );
        // A reload is checked at once.
        h.rig.agent.on_event(Event::HomeBind);
        h.rig.agent.home_housekeeping();
        assert_eq!(h.compositor.lock().unwrap().checks, 2);
        // Once removed, no more checks, and a late reload event is dropped.
        assert_eq!(bind(&mut h, 2, false), Ok(()));
        h.rig.agent.on_event(Event::HomeBind);
        h.rig.agent.home_housekeeping();
        assert_eq!(h.compositor.lock().unwrap().checks, 2);
        assert!(!h.rig.agent.home.reload);
    }

    #[test]
    fn the_seat_reports_compositor_reloads_as_events() {
        let mut h = home();
        let (tx, rx) = std::sync::mpsc::channel();
        subscribe_platform(&mut h.rig.agent.platform, &tx);
        let reload = h
            .compositor
            .lock()
            .unwrap()
            .reload
            .take()
            .expect("a watch was set up");
        reload();
        assert!(matches!(rx.try_recv(), Ok(Event::HomeBind)));
    }

    // ---- start and stop (A1, A2, B5) ----

    #[test]
    fn startup_removes_a_leftover_bind_of_ours_and_does_not_fence() {
        let mut h = home();
        h.compositor.lock().unwrap().ours = true;
        h.rig.agent.home_startup();
        assert!(!h.compositor.lock().unwrap().ours);
        assert!(!h.rig.agent.home.fence);
        assert_eq!(h.rig.agent.home.present, Some(false));
    }

    #[test]
    fn startup_with_only_a_foreign_binding_leaves_it_alone_and_does_not_fence() {
        let mut h = home();
        h.compositor.lock().unwrap().foreign = true;
        let notices = h.rig.agent.notices.len();
        h.rig.agent.home_startup();
        assert!(
            h.compositor.lock().unwrap().foreign,
            "the owner's binding is untouched"
        );
        assert!(!h.rig.agent.home.fence, "ours is confirmed absent");
        assert_eq!(h.rig.agent.notices.len(), notices);
        assert!(h.rig.agent.inject(InjectCmd::Key {
            usage: HidUsage::keyboard(4),
            down: true
        }));
        assert!(h.rig.agent.inject(InjectCmd::Key {
            usage: HidUsage::keyboard(4),
            down: false
        }));
        assert!(h.held.lock().unwrap().is_empty());
    }

    /// Ours and a foreign binding share the chord at start: removal is refused.
    fn fenced() -> Home {
        let mut h = home();
        {
            let mut c = h.compositor.lock().unwrap();
            c.ours = true;
            c.foreign = true;
        }
        let peer = h.rig.peer;
        h.rig
            .agent
            .trust
            .update(|t| {
                t.set_grant(peer, Capability::InputAccept, true)
                    .map_err(|e| anyhow::anyhow!("{e}"))
            })
            .unwrap();
        h.rig.agent.home_startup();
        h
    }

    #[test]
    fn a_startup_removal_that_fails_fences_injection_and_tells_the_user() {
        let h = fenced();
        assert!(h.rig.agent.home.fence);
        {
            let c = h.compositor.lock().unwrap();
            assert!(c.ours && c.foreign, "nothing was removed");
        }
        let text = last_notice(&h);
        assert!(
            text.contains("can't remove its release shortcut")
                && text.contains(KEYS)
                && text.contains("reload your Hyprland config"),
            "{text}"
        );
    }

    #[test]
    fn a_fenced_node_injects_nothing_but_releases() {
        let mut h = fenced();
        let key = HidUsage::keyboard(4);
        for cmd in [
            InjectCmd::Key {
                usage: key,
                down: true,
            },
            InjectCmd::Button {
                button: MouseButton::PRIMARY,
                down: true,
            },
            InjectCmd::MoveTo {
                display: DisplayId(1),
                position: PointDevice::new(1.0, 1.0),
            },
            InjectCmd::Scroll(ScrollDelta {
                v120_x: 0,
                v120_y: 120,
                pixels: None,
                phase: ScrollPhase::Discrete,
                stop_x: false,
                stop_y: false,
            }),
            InjectCmd::LockKeys(LockKeys::default()),
        ] {
            assert!(!h.rig.agent.inject(cmd.clone()), "{cmd:?} is refused");
        }
        assert!(
            h.injected.lock().unwrap().is_empty(),
            "no injector was called"
        );
        for cmd in [
            InjectCmd::Key {
                usage: key,
                down: false,
            },
            InjectCmd::Button {
                button: MouseButton::PRIMARY,
                down: false,
            },
            InjectCmd::ReleaseAll,
            InjectCmd::Recover {
                keys: vec![key],
                buttons: vec![MouseButton::PRIMARY],
            },
        ] {
            assert!(
                h.rig.agent.inject(cmd.clone()),
                "{cmd:?} still goes through"
            );
        }
        assert_eq!(
            *h.injected.lock().unwrap(),
            [
                "key up",
                "button up",
                "release all keys",
                "release all buttons",
                "recover keys",
                "recover buttons"
            ]
        );
    }

    #[test]
    fn a_fenced_node_refuses_e1_control_and_never_installs_a_bind() {
        let mut h = fenced();
        let peer = h.rig.peer;
        h.rig.agent.send_grants();
        assert!(
            !grants_of(&h, peer).contains(&Capability::InputAccept),
            "the engine refuses control with `Refusal::Permission`"
        );
        let installs = h.compositor.lock().unwrap().installs;
        assert_eq!(bind(&mut h, 1, true), Err(Failure::Other));
        assert_eq!(
            h.compositor.lock().unwrap().installs,
            installs,
            "home never engages"
        );
    }

    #[test]
    fn the_startup_removal_is_retried_every_two_seconds_and_the_fence_lifts_when_it_works() {
        let mut h = fenced();
        let peer = h.rig.peer;
        let removes = h.compositor.lock().unwrap().removes;
        h.rig.agent.home.last_fence_try = Instant::now() + Duration::from_secs(3600);
        h.rig.agent.home_housekeeping();
        assert_eq!(
            h.compositor.lock().unwrap().removes,
            removes,
            "not before two seconds"
        );
        // Two seconds on, still refused (the same two bindings): another try, still fenced.
        h.rig.agent.home.last_fence_try = Instant::now() - Duration::from_millis(2100);
        h.rig.agent.home_housekeeping();
        assert_eq!(h.compositor.lock().unwrap().removes, removes + 1);
        assert!(h.rig.agent.home.fence);
        let said = h.rig.agent.notices.len();
        // The owner reloads the config: runtime binds are gone.
        {
            let mut c = h.compositor.lock().unwrap();
            c.ours = false;
            c.foreign = false;
        }
        h.rig.agent.home.last_fence_try = Instant::now() - Duration::from_millis(2100);
        h.rig.agent.home_housekeeping();
        assert!(!h.rig.agent.home.fence);
        assert_eq!(h.rig.agent.notices.len(), said + 1);
        assert!(
            last_notice(&h).contains("accepted again"),
            "{}",
            last_notice(&h)
        );
        // Control is accepted again, and injection with it.
        assert!(grants_of(&h, peer).contains(&Capability::InputAccept));
        assert!(h.rig.agent.inject(InjectCmd::Key {
            usage: HidUsage::keyboard(4),
            down: true
        }));
        assert!(h.rig.agent.inject(InjectCmd::Key {
            usage: HidUsage::keyboard(4),
            down: false
        }));
        assert!(h.held.lock().unwrap().is_empty());
        assert_eq!(bind(&mut h, 1, true), Ok(()));
    }

    #[test]
    fn a_foreign_binding_alone_confirms_ours_absent_and_lifts_the_fence() {
        let mut h = fenced();
        let peer = h.rig.peer;
        // Ours is gone (a reload dropped it); the owner's binding on the chord is still there.
        h.compositor.lock().unwrap().ours = false;
        h.rig.agent.home.last_fence_try = Instant::now() - Duration::from_millis(2100);
        h.rig.agent.home_housekeeping();
        assert!(!h.rig.agent.home.fence, "absence of ours is confirmed");
        assert!(
            h.compositor.lock().unwrap().foreign,
            "the owner's binding is untouched"
        );
        assert!(grants_of(&h, peer).contains(&Capability::InputAccept));
        // Home still can't engage: the install refuses a chord somebody else uses.
        assert_eq!(bind(&mut h, 1, true), Err(Failure::Other));
        assert!(h.compositor.lock().unwrap().foreign && !h.compositor.lock().unwrap().ours);
    }

    #[test]
    fn stopping_removes_our_bind_and_only_ours() {
        // Ours present.
        let mut h = home();
        h.compositor.lock().unwrap().ours = true;
        h.rig.agent.home_shutdown();
        assert!(!h.compositor.lock().unwrap().ours);
        assert_eq!(h.rig.agent.home.present, Some(false));
        // Only a foreign binding on the chord: left alone.
        let mut h = home();
        h.compositor.lock().unwrap().foreign = true;
        h.rig.agent.home_shutdown();
        assert!(h.compositor.lock().unwrap().foreign);
        // A whole shutdown removes it too, after the engine's panic did.
        let mut h = home();
        assert_eq!(bind(&mut h, 1, true), Ok(()));
        h.rig.agent.shutdown();
        assert!(!h.compositor.lock().unwrap().ours);
        assert_eq!(h.rig.agent.home.wanted, None);
    }

    // ---- notices, status, tray ----

    #[test]
    fn the_home_notices_name_the_window_the_chord_and_the_peer() {
        let mut h = home();
        let key = key(&h);
        h.rig
            .agent
            .capture_display
            .insert(key.projection, DisplayId(9));
        h.rig
            .agent
            .placement
            .window_event(&WindowEvent::Added(window(
                1,
                "Notes",
                1,
                Some(9),
                (0.0, 0.0, 10.0, 10.0),
                WindowState::Normal,
            )));
        h.rig.agent.notice(&Notice::Home { key, entered: true });
        assert_eq!(
            last_notice(&h),
            format!(
                "Input is home in \"Notes\"; the other machine stays connected; press {KEYS} or push past the window's edges to return"
            )
        );
        assert_eq!(h.rig.agent.status()["home"]["projection"], json!(3));
        // Home left with its peer's session over: input is simply here.
        h.rig.agent.home.now.as_mut().unwrap().peer = Some(h.rig.peer);
        h.rig.agent.notice(&Notice::Home {
            key,
            entered: false,
        });
        let text = last_notice(&h);
        assert!(
            text.starts_with("Input left \"Notes\"; control of "),
            "{text}"
        );
        assert_eq!(h.rig.agent.status()["home"]["projection"], json!(null));
    }

    #[test]
    fn an_untitled_window_is_named_by_its_number() {
        let mut h = home();
        let key = key(&h);
        h.rig.agent.notice(&Notice::Home { key, entered: true });
        assert!(last_notice(&h).starts_with("Input is home in projected window 3;"));
    }

    #[test]
    fn each_failure_has_its_own_line_with_the_agents_detail_where_it_has_one() {
        let mut h = home();
        let key = key(&h);
        h.rig.agent.home.error = Some("no such bind".into());
        h.rig.agent.home.inject_error = Some((Instant::now(), "the pointer is gone".into()));
        let mut lines = Vec::new();
        for reason in [
            HomeFailure::Drain,
            HomeFailure::Bind,
            HomeFailure::Release,
            HomeFailure::Warp,
            HomeFailure::Focus,
            HomeFailure::Guard,
            HomeFailure::Gone,
        ] {
            h.rig.agent.notice(&Notice::HomeFailed { key, reason });
            lines.push(last_notice(&h));
        }
        for line in &lines {
            assert!(
                line.starts_with("Input is not home in projected window 3: "),
                "{line}"
            );
        }
        let distinct: BTreeSet<&String> = lines.iter().collect();
        assert_eq!(distinct.len(), lines.len(), "{lines:?}");
        assert!(
            lines[0].contains("last injection error: the pointer is gone"),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].contains(KEYS) && lines[1].contains("no such bind"),
            "{}",
            lines[1]
        );
        // An injection error long ago isn't the cause of this drain.
        h.rig.agent.home.inject_error =
            Some((Instant::now() - Duration::from_secs(60), "old".into()));
        h.rig.agent.notice(&Notice::HomeFailed {
            key,
            reason: HomeFailure::Drain,
        });
        assert!(!last_notice(&h).contains("old"));
    }

    #[test]
    fn a_failed_home_clears_the_status_and_the_tray_line() {
        let mut h = home();
        let key = key(&h);
        h.rig
            .agent
            .projections
            .insert(key, "projecting window 3 to peer".into());
        h.rig.agent.notice(&Notice::Home { key, entered: true });
        let view = h.rig.agent.tray_view();
        assert_eq!(
            view.projections,
            [(
                key,
                format!("projecting window 3 to peer — input is home here ({KEYS} returns)")
            )]
        );
        h.rig.agent.notice(&Notice::HomeFailed {
            key,
            reason: HomeFailure::Gone,
        });
        assert_eq!(h.rig.agent.status()["home"]["projection"], json!(null));
        assert_eq!(
            h.rig.agent.tray_view().projections,
            [(key, "projecting window 3 to peer".to_owned())]
        );
    }

    // ---- overlay: one outcome per configuration (WP-2.42) ----

    #[test]
    fn a_refused_overlay_show_makes_up_no_outcome() {
        let mut h = home();
        let shown = Arc::new(Mutex::new(0));
        h.rig.agent.platform.overlay = Some(Box::new(RefusingOverlay(shown.clone())));
        let show = || Output::ShowOverlay {
            id: crosspane_engine::io::HUD,
            overlay: Overlay {
                display: DisplayId(1),
                anchor: OverlayAnchor::TopCenter,
                text: "Input → peer".into(),
                accent: Rgb8 { r: 1, g: 2, b: 3 },
            },
        };
        h.rig.agent.execute(vec![show()]);
        assert_eq!(*shown.lock().unwrap(), 1);
        assert!(
            h.rig.agent.pending.is_empty(),
            "no `Unavailable` of the agent's own"
        );
        // The same with no overlay backend at all: the engine's HUD deadline covers it.
        h.rig.agent.platform.overlay = None;
        h.rig.agent.execute(vec![show()]);
        assert!(h.rig.agent.pending.is_empty());
        assert!(h.rig.agent.fed.is_empty());
    }

    // ---- the placement source ----

    fn display(id: u32, scale: f64, origin: (f64, f64), pixels: (u32, u32)) -> DisplayInfo {
        DisplayInfo {
            id: DisplayId(id),
            name: format!("test-{id}"),
            geometry: DisplayGeometry {
                physical_size: SizeMm::new(300.0, 200.0),
                pixel_size: PixelSize::new(pixels.0, pixels.1),
                scale,
                logical_origin: PointLogical::new(origin.0, origin.1),
            },
            refresh_millihz: 60_000,
            color_space: ColorSpace::Srgb,
            hdr: false,
        }
    }

    fn window(
        id: u64,
        title: &str,
        pid: u32,
        on: Option<u32>,
        frame: (f64, f64, f64, f64),
        state: WindowState,
    ) -> WindowInfo {
        WindowInfo {
            id: WindowId(id),
            title: title.to_owned(),
            app_id: "crosspane-proxy".into(),
            pid: Some(pid),
            display: on.map(DisplayId),
            frame: RectLogical::new(
                PointLogical::new(frame.0, frame.1),
                SizeLogical::new(frame.2, frame.3),
            ),
            state,
            role: WindowRole::Toplevel,
            parent: None,
        }
    }

    const PID: u32 = 4242;

    fn proxy_key(n: u64) -> ProjectionKey {
        ProjectionKey {
            source: NodeId([9; 32]),
            projection: ProjectionId(n),
        }
    }

    /// One visible proxy "peer › one" with a matching window on display 1 (scale 2, origin
    /// (10, 20)), nothing reported yet.
    fn placed_source() -> PlacementSource {
        let mut source = PlacementSource::new(PID);
        source.set_displays(&[
            display(1, 2.0, (10.0, 20.0), (3000, 2000)),
            display(2, 1.0, (1500.0, 0.0), (1000, 800)),
        ]);
        source.opened(proxy_key(1), "peer › one");
        source.set_visible(proxy_key(1), true);
        source.window_event(&WindowEvent::Added(window(
            1,
            "peer › one",
            PID,
            Some(1),
            (110.0, 70.0, 400.0, 300.0),
            WindowState::Normal,
        )));
        source
    }

    fn only(source: &mut PlacementSource) -> Placed {
        let changes = source.changes();
        assert_eq!(changes.len(), 1, "{changes:?}");
        changes[0].1
    }

    const NOWHERE: Placed = (None, PointDevice::new(0.0, 0.0), PixelSize::new(0, 0));

    #[test]
    fn a_proxy_is_placed_in_device_pixels_of_its_display() {
        let mut source = placed_source();
        // The origin subtracts the display's origin then scales; the size only scales.
        assert_eq!(
            only(&mut source),
            (
                Some(DisplayId(1)),
                PointDevice::new(200.0, 100.0),
                PixelSize::new(800, 600)
            )
        );
    }

    #[test]
    fn a_fractional_size_rounds_to_the_nearest_device_pixel() {
        let mut source = placed_source();
        source.set_displays(&[display(1, 1.25, (0.0, 0.0), (3000, 2000))]);
        source.window_event(&WindowEvent::Changed(window(
            1,
            "peer › one",
            PID,
            Some(1),
            (8.0, 8.0, 401.3, 300.5),
            WindowState::Normal,
        )));
        let (shown, origin, size) = only(&mut source);
        assert_eq!(shown, Some(DisplayId(1)));
        assert_eq!(origin, PointDevice::new(10.0, 10.0));
        assert_eq!(size, PixelSize::new(502, 376), "501.625 and 375.625");
    }

    #[test]
    fn a_proxy_the_host_hasnt_called_visible_is_nowhere() {
        let mut source = placed_source();
        source.set_visible(proxy_key(1), false);
        assert_eq!(only(&mut source), NOWHERE);
        // Nothing heard at all is the same.
        let mut source = placed_source();
        source.visible.clear();
        assert_eq!(only(&mut source), NOWHERE);
    }

    #[test]
    fn hidden_and_minimised_windows_are_nowhere_but_a_fullscreen_one_is_placed() {
        for (state, placed) in [
            (WindowState::Hidden, false),
            (WindowState::Minimized, false),
            (WindowState::Fullscreen, true),
            (WindowState::Normal, true),
        ] {
            let mut source = placed_source();
            source.window_event(&WindowEvent::Changed(window(
                1,
                "peer › one",
                PID,
                Some(1),
                (110.0, 70.0, 400.0, 300.0),
                state,
            )));
            assert_eq!(only(&mut source).0.is_some(), placed, "{state:?}");
        }
    }

    #[test]
    fn a_window_on_a_display_the_engine_doesnt_know_is_nowhere() {
        // A projected window parked on a twin output is not on any LocalDisplays display.
        let mut source = placed_source();
        source.window_event(&WindowEvent::Changed(window(
            1,
            "peer › one",
            PID,
            Some(9),
            (0.0, 0.0, 400.0, 300.0),
            WindowState::Normal,
        )));
        assert_eq!(only(&mut source), NOWHERE);
        source.window_event(&WindowEvent::Changed(window(
            1,
            "peer › one",
            PID,
            None,
            (0.0, 0.0, 400.0, 300.0),
            WindowState::Normal,
        )));
        assert!(source.changes().is_empty(), "still nowhere: nothing to say");
    }

    #[test]
    fn only_this_processs_window_with_exactly_the_proxys_title_counts() {
        let mut source = placed_source();
        // Somebody else's window with the same title is not the proxy.
        source.window_event(&WindowEvent::Changed(window(
            1,
            "peer › one",
            PID + 1,
            Some(1),
            (110.0, 70.0, 400.0, 300.0),
            WindowState::Normal,
        )));
        assert_eq!(only(&mut source), NOWHERE);
        // Ours, but under another title.
        source.window_event(&WindowEvent::Changed(window(
            1,
            "peer › one (2)",
            PID,
            Some(1),
            (110.0, 70.0, 400.0, 300.0),
            WindowState::Normal,
        )));
        assert!(source.changes().is_empty());
    }

    #[test]
    fn two_windows_with_the_proxys_title_make_it_ambiguous() {
        let mut source = placed_source();
        assert!(only(&mut source).0.is_some());
        source.window_event(&WindowEvent::Added(window(
            2,
            "peer › one",
            PID,
            Some(1),
            (0.0, 0.0, 100.0, 100.0),
            WindowState::Normal,
        )));
        assert_eq!(only(&mut source), NOWHERE);
        // One goes: placed again.
        source.window_event(&WindowEvent::Removed(WindowId(2)));
        assert!(only(&mut source).0.is_some());
    }

    #[test]
    fn two_proxies_with_one_title_are_both_nowhere() {
        let mut source = placed_source();
        source.opened(proxy_key(2), "peer › one");
        source.set_visible(proxy_key(2), true);
        let changes = source.changes();
        assert_eq!(changes.len(), 2);
        assert!(
            changes.iter().all(|(_, placed, _)| *placed == NOWHERE),
            "{changes:?}"
        );
        // One is retitled: the title is unique again.
        source.retitled(proxy_key(2), "peer › two");
        let changes = source.changes();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].0, proxy_key(1));
        assert!(changes[0].1.0.is_some());
    }

    #[test]
    fn a_removed_window_is_nowhere_and_a_display_change_recomputes_every_placement() {
        let mut source = placed_source();
        assert!(only(&mut source).0.is_some());
        // The display's scale changes: the same window, other device pixels.
        source.set_displays(&[display(1, 1.0, (10.0, 20.0), (1500, 1000))]);
        assert_eq!(
            only(&mut source),
            (
                Some(DisplayId(1)),
                PointDevice::new(100.0, 50.0),
                PixelSize::new(400, 300)
            )
        );
        // The display goes away.
        source.set_displays(&[]);
        assert_eq!(only(&mut source), NOWHERE);
        source.set_displays(&[display(1, 1.0, (10.0, 20.0), (1500, 1000))]);
        assert!(only(&mut source).0.is_some());
        source.window_event(&WindowEvent::Removed(WindowId(1)));
        assert_eq!(only(&mut source), NOWHERE);
    }

    #[test]
    fn equal_reports_are_not_repeated_and_the_first_always_goes_out() {
        let mut source = PlacementSource::new(PID);
        source.opened(proxy_key(1), "peer › one");
        // Not visible, no window: the first report is "nowhere" and goes out, once.
        let first = source.changes();
        assert_eq!(first, [(proxy_key(1), NOWHERE, true)]);
        assert!(source.changes().is_empty());
        // Placing it is a notable report; a move after that is not.
        source.set_displays(&[display(1, 1.0, (0.0, 0.0), (1000, 800))]);
        source.set_visible(proxy_key(1), true);
        source.window_event(&WindowEvent::Added(window(
            1,
            "peer › one",
            PID,
            Some(1),
            (5.0, 5.0, 100.0, 100.0),
            WindowState::Normal,
        )));
        let placed = source.changes();
        assert_eq!(placed.len(), 1);
        assert!(placed[0].2, "nowhere → placed is notable");
        source.window_event(&WindowEvent::Changed(window(
            1,
            "peer › one",
            PID,
            Some(1),
            (6.0, 5.0, 100.0, 100.0),
            WindowState::Normal,
        )));
        let moved = source.changes();
        assert_eq!(moved.len(), 1);
        assert!(!moved[0].2);
        // The same state again: silence.
        source.window_event(&WindowEvent::Changed(window(
            1,
            "peer › one",
            PID,
            Some(1),
            (6.0, 5.0, 100.0, 100.0),
            WindowState::Normal,
        )));
        assert!(source.changes().is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn long_badged_and_decorated_titles_match_wayland_and_share_ambiguity() {
        let mut h = home();
        let key = proxy_key(1);
        let badged = h.rig.agent.badged(key.source, &"é".repeat(512));
        assert!(badged.len() <= 1024);
        assert!(badged.starts_with(&format!("{} › ", key.source.short())));
        h.rig.agent.placement = PlacementSource::new(PID);
        h.rig
            .agent
            .placement
            .set_displays(&[display(1, 1.0, (0.0, 0.0), (1000, 800))]);
        h.rig.agent.placement.opened(key, &badged);
        h.rig.agent.placement.set_visible(key, true);
        h.rig
            .agent
            .placement
            .window_event(&WindowEvent::Added(window(
                1,
                &badged,
                PID,
                Some(1),
                (0.0, 0.0, 400.0, 300.0),
                WindowState::Normal,
            )));
        assert!(only(&mut h.rig.agent.placement).0.is_some());
        let text = format!("{} — 60 fps, 12 ms", "a".repeat(1020));
        h.rig.agent.set_proxy_title(key, text.clone());
        let normalized = proxy_title(text);
        assert_eq!(normalized.len(), 1024);
        assert_eq!(h.rig.agent.placement.proxies[&key], normalized);
        h.rig
            .agent
            .placement
            .window_event(&WindowEvent::Changed(window(
                1,
                &normalized,
                PID,
                Some(1),
                (0.0, 0.0, 400.0, 300.0),
                WindowState::Normal,
            )));
        assert!(h.rig.agent.placement.compute(&key).0.is_some());
        // Distinct suffixes beyond the wire limit cannot evade title ambiguity detection.
        let shared = "x".repeat(1024);
        h.rig.agent.set_proxy_title(key, format!("{shared}one"));
        let other = proxy_key(2);
        h.rig
            .agent
            .placement
            .opened(other, &proxy_title(format!("{shared}two")));
        h.rig.agent.placement.set_visible(other, true);
        assert_eq!(h.rig.agent.placement.compute(&key), NOWHERE);
        assert_eq!(h.rig.agent.placement.compute(&other), NOWHERE);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn closing_an_ambiguous_proxy_recomputes_the_survivor_without_a_window_event() {
        let mut h = home();
        h.rig.agent.placement = placed_source();
        let key = proxy_key(1);
        let other = proxy_key(2);
        h.rig.agent.placement.opened(other, "peer › one");
        h.rig.agent.placement.set_visible(other, true);
        assert_eq!(h.rig.agent.placement.changes().len(), 2);
        assert_eq!(h.rig.agent.placement.compute(&key), NOWHERE);
        h.rig.agent.execute(vec![Output::CloseProxy { key: other }]);
        assert!(h.rig.agent.placement_dirty);
        h.rig.agent.flush_placements();
        assert!(h.rig.agent.pending.iter().any(|i| matches!(i, Input::Proxy { key: got, event: ProxyEvent::Placed { display: Some(_), .. } } if *got == key)));
    }

    #[test]
    fn decorated_proxy_titles_still_match_the_compositors_window() {
        let mut h = home();
        h.rig.agent.placement = placed_source();
        let key = proxy_key(1);
        assert!(only(&mut h.rig.agent.placement).0.is_some());
        let title = "peer › one — 60 fps, 12 ms";
        h.rig.agent.set_proxy_title(key, title.into());
        assert!(
            h.rig.agent.placement.compute(&key).0.is_some(),
            "our title request must not invalidate the known window before the next poll"
        );
        assert!(h.rig.agent.placement_dirty);
        h.rig.agent.flush_placements();
        h.rig.agent.settle();
        assert!(
            !h.rig.agent.fed.iter().any(|i| matches!(
                i,
                Input::Proxy {
                    event: ProxyEvent::Placed { display: None, .. },
                    ..
                }
            )),
            "a delayed title event must not send an invalidation"
        );
        h.rig
            .agent
            .placement
            .window_event(&WindowEvent::Changed(window(
                1,
                title,
                PID,
                Some(1),
                (110.0, 70.0, 400.0, 300.0),
                WindowState::Normal,
            )));
        assert!(h.rig.agent.placement.compute(&key).0.is_some());
        assert!(!h.rig.agent.placement_dirty);
    }

    #[test]
    fn a_closed_proxy_reports_nothing_more() {
        let mut source = placed_source();
        assert_eq!(source.changes().len(), 1);
        source.closed(proxy_key(1));
        source.set_displays(&[]);
        assert!(source.changes().is_empty());
        assert!(source.last.is_empty() && source.visible.is_empty());
    }

    #[test]
    fn a_projected_window_is_found_by_the_display_it_is_captured_from() {
        let mut source = placed_source();
        source.window_event(&WindowEvent::Added(window(
            5,
            "Notes",
            1,
            Some(9),
            (0.0, 0.0, 10.0, 10.0),
            WindowState::Normal,
        )));
        assert_eq!(source.window_on(DisplayId(9)).unwrap().title, "Notes");
        assert!(source.window_on(DisplayId(8)).is_none());
        // A dialog of the same app on that output is not the window.
        let mut dialog = window(
            6,
            "Dialog",
            1,
            Some(8),
            (0.0, 0.0, 5.0, 5.0),
            WindowState::Normal,
        );
        dialog.role = WindowRole::Dialog;
        source.window_event(&WindowEvent::Added(dialog));
        assert!(source.window_on(DisplayId(8)).is_none());
    }

    /// The whole path on Linux: windows, displays, the open proxy and the host's visibility in,
    /// `ProxyEvent::Placed` out; the host's own origin and size are never used.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_agent_reports_the_compositors_placement_once_the_host_says_visible() {
        let mut h = home();
        let key = proxy_key(5);
        h.rig.agent.titles.insert(key, ("peer › win".into(), 0));
        let id = h.rig.agent.proxy_ids.open(key);
        h.rig.agent.placement = PlacementSource::new(std::process::id());
        h.rig.agent.feed(Input::LocalDisplays(vec![display(
            1,
            1.0,
            (0.0, 0.0),
            (1000, 800),
        )]));
        h.rig.agent.feed(Input::Windows(WindowEvent::Added(window(
            1,
            "peer › win",
            std::process::id(),
            Some(1),
            (10.0, 20.0, 300.0, 200.0),
            WindowState::Normal,
        ))));
        // The proxy opens. (Not fed to the engine: it has no destination for this key and would
        // close the proxy again; what the agent learns from the input is the point here.)
        h.rig.agent.observe(&Input::ProxyOpened {
            key,
            result: Ok((PixelSize::new(300, 200), 1.0)),
        });
        h.rig.agent.flush_placements();
        let placed = |h: &mut Home| {
            let reports: Vec<_> = h
                .rig
                .agent
                .pending
                .drain(..)
                .filter_map(|input| match input {
                    Input::Proxy {
                        event:
                            ProxyEvent::Placed {
                                display: on,
                                origin,
                                size,
                            },
                        ..
                    } => Some((on, origin, size)),
                    _ => None,
                })
                .collect();
            reports
        };
        // The proxy is open but the host hasn't said it is visible: the first report is "nowhere".
        assert_eq!(placed(&mut h), [NOWHERE]);
        // The host says visible (its own origin and size are Wayland's guess and are ignored).
        h.rig.agent.on_host(HostEvent::Placed {
            id,
            visible: true,
            monitor: None,
            origin: PointDevice::new(777.0, 777.0),
            size: PixelSize::new(1, 1),
        });
        assert_eq!(
            placed(&mut h),
            [(
                Some(DisplayId(1)),
                PointDevice::new(10.0, 20.0),
                PixelSize::new(300, 200)
            )]
        );
        // Minimised: nowhere again. The same state twice says nothing.
        h.rig.agent.on_host(HostEvent::Placed {
            id,
            visible: false,
            monitor: None,
            origin: PointDevice::new(0.0, 0.0),
            size: PixelSize::new(0, 0),
        });
        assert_eq!(placed(&mut h), [NOWHERE]);
        h.rig.agent.on_host(HostEvent::Placed {
            id,
            visible: false,
            monitor: None,
            origin: PointDevice::new(0.0, 0.0),
            size: PixelSize::new(0, 0),
        });
        assert!(placed(&mut h).is_empty());
        // A host that does name a monitor (macOS) is believed as before.
        h.rig.agent.on_host(HostEvent::Placed {
            id,
            visible: true,
            monitor: Some(3),
            origin: PointDevice::new(5.0, 6.0),
            size: PixelSize::new(7, 8),
        });
        assert!(h.rig.agent.fed.iter().any(|input| matches!(
            input,
            Input::Proxy {
                event: ProxyEvent::Placed {
                    display: Some(DisplayId(3)),
                    ..
                },
                ..
            }
        )));
    }

    // ---- status.result.installer (WP-4.5) ----

    use crosspane_engine::io::ReleaseCause;

    /// `result.installer`, as `crosspanectl status` shows it.
    fn status_installer(h: &Home) -> Value {
        h.rig.agent.status()["installer"].clone()
    }

    /// The paired peer's counters that aren't zero or unreported.
    fn nonzero(h: &Home) -> Value {
        let counters = status_installer(h)["peers"][0]["counters"].clone();
        Value::Object(
            counters
                .as_object()
                .unwrap()
                .iter()
                .filter(|(_, v)| v.as_u64().is_some_and(|n| n > 0))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        )
    }

    fn epoch(h: &Home, which: &str) -> u64 {
        status_installer(h)["epochs"][which].as_u64().unwrap()
    }

    /// The status with this run's own ids replaced, so two runs can be compared.
    fn anonymous(h: &Home) -> Value {
        let mut status = status_installer(h);
        status["instance"] = json!("<instance>");
        let text = status
            .to_string()
            .replace(&h.rig.local.to_string(), "<local>")
            .replace(&h.rig.peer.to_string(), "<peer>");
        serde_json::from_str(&text).unwrap()
    }

    /// An injector that refuses every key.
    struct FailingKeys;

    impl KeyInjector for FailingKeys {
        fn key(&mut self, _usage: HidUsage, _down: bool) -> Result<(), PlatformError> {
            Err(PlatformError::Backend(
                "fixture: the injector refuses".into(),
            ))
        }

        fn lock_keys(&self) -> Result<LockKeys, PlatformError> {
            Ok(LockKeys::default())
        }

        fn set_lock_keys(&mut self, _wanted: LockKeys) -> Result<(), PlatformError> {
            Ok(())
        }

        fn release_all(&mut self) -> Result<(), PlatformError> {
            Ok(())
        }

        fn recover_keys(&mut self, _keys: &[HidUsage]) -> Result<(), PlatformError> {
            Ok(())
        }
    }

    /// Parking that parks as `kind`, and whose `restore` fails when told to.
    struct Parking {
        kind: crosspane_platform::ParkingKind,
        restore_fails: bool,
    }

    impl crosspane_platform::WindowParking for Parking {
        fn park(
            &mut self,
            window: WindowId,
            size: PixelSize,
            scale: f64,
        ) -> Result<crosspane_platform::Parked, PlatformError> {
            let mut parked =
                crosspane_platform::WindowParking::park(&mut FakeParking, window, size, scale)?;
            parked.kind = self.kind;
            Ok(parked)
        }
        fn resize(
            &mut self,
            window: WindowId,
            size: PixelSize,
            scale: f64,
        ) -> Result<crosspane_platform::Parked, PlatformError> {
            self.park(window, size, scale)
        }
        fn geometry(&self, _window: WindowId) -> Result<crosspane_platform::Parked, PlatformError> {
            Err(PlatformError::NotFound)
        }
        fn restore(&mut self, _window: WindowId) -> Result<(), PlatformError> {
            if self.restore_fails {
                Err(PlatformError::Backend(
                    "fixture: the window won't come back".into(),
                ))
            } else {
                Ok(())
            }
        }
        fn recover(&mut self) -> Result<Vec<WindowId>, PlatformError> {
            Ok(Vec::new())
        }
    }

    /// The Mac's four permissions, each granted or not (what is in the list is granted).
    struct MacLike(Arc<Mutex<Vec<Permission>>>);

    impl crosspane_platform::Permissions for MacLike {
        fn required(&self) -> Vec<Permission> {
            vec![
                Permission::ScreenRecording,
                Permission::Accessibility,
                Permission::InputMonitoring,
                Permission::Microphone,
            ]
        }

        fn state(&self, permission: Permission) -> PermissionState {
            if self.0.lock().unwrap().contains(&permission) {
                PermissionState::Granted
            } else {
                PermissionState::NotGranted
            }
        }

        fn request(&mut self, _permission: Permission) -> Result<(), PlatformError> {
            Ok(())
        }

        fn subscribe(
            &mut self,
            _sink: Arc<dyn EventSink<(Permission, PermissionState)>>,
        ) -> Result<(), PlatformError> {
            Ok(())
        }
    }

    /// A session whose state the test sets.
    struct VarSession(Arc<Mutex<crosspane_platform::SessionState>>);

    impl crosspane_platform::SessionEvents for VarSession {
        fn state(&self) -> crosspane_platform::SessionState {
            *self.0.lock().unwrap()
        }

        fn subscribe(
            &mut self,
            _sink: Arc<dyn EventSink<crosspane_platform::SessionEvent>>,
        ) -> Result<(), PlatformError> {
            Ok(())
        }
    }

    struct FakeTray;

    impl crosspane_platform::TrayHost for FakeTray {
        fn subscribe(
            &mut self,
            _sink: Arc<dyn EventSink<crosspane_platform::TrayEvent>>,
        ) -> Result<(), PlatformError> {
            Ok(())
        }

        fn set(&mut self, _menu: &crosspane_platform::TrayMenu) -> Result<(), PlatformError> {
            Ok(())
        }
    }

    /// An audio worker that reports the counters it is given.
    struct StatsPlane(WorkerStats);

    impl AudioPlane for StatsPlane {
        fn submit(&self, _output: Output) {}
        fn packet(&self, _peer: NodeId, _packet: AudioPacket) {}
        fn cancel_peer(&self, _peer: NodeId) {}
        fn stats(&self) -> WorkerStats {
            self.0
        }
        fn shutdown(self: Box<Self>) {}
    }

    /// An overlay host that accepts every show (the outcome events are the test's to send).
    struct AcceptingOverlay;

    impl OverlayHost for AcceptingOverlay {
        fn subscribe(
            &mut self,
            _sink: Arc<dyn EventSink<crosspane_platform::OverlayEvent>>,
        ) -> Result<(), PlatformError> {
            Ok(())
        }

        fn show(&mut self, _id: OverlayId, _overlay: &Overlay) -> Result<(), PlatformError> {
            Ok(())
        }

        fn hide(&mut self, _id: OverlayId) -> Result<(), PlatformError> {
            Ok(())
        }
    }

    /// The start of an E1 session to this node from `peer` (any node the engine grants input).
    fn start_control(
        h: &mut Home,
        peer: NodeId,
        session: crosspane_types::id::SessionId,
    ) -> Vec<Output> {
        step(
            h,
            control_input(
                peer,
                ControlMessage::StartControl {
                    session,
                    entry_display: DisplayId(1),
                    entry: PointDevice::new(1.0, 1.0),
                    lock_keys: LockKeys::default(),
                },
            ),
        )
    }

    /// This node controlled by the paired peer (E1 target), as session 77.
    fn controlled() -> (Home, crosspane_types::id::SessionId) {
        let mut h = bare_scenario();
        h.rig.agent.platform.overlay = Some(Box::new(AcceptingOverlay));
        let (peer, session) = (h.rig.peer, crosspane_types::id::SessionId(77));
        let out = start_control(&mut h, peer, session);
        assert!(
            out.iter()
                .any(|o| matches!(o, Output::Notice(Notice::ControlledBy(p)) if *p == peer)),
            "{out:?}"
        );
        (h, session)
    }

    /// The controller's message for one key (or button) change.
    fn from_controller(h: &mut Home, session: crosspane_types::id::SessionId, msg: InputMessageOf) {
        let peer = h.rig.peer;
        step(
            h,
            Input::Link(LinkEvent::Input {
                peer,
                msg: msg(session),
            }),
        );
    }

    type InputMessageOf =
        Box<dyn FnOnce(crosspane_types::id::SessionId) -> crosspane_protocol::msg::InputMessage>;

    fn key_msg(usage: u16, down: bool, seq: u32) -> InputMessageOf {
        Box::new(move |session| crosspane_protocol::msg::InputMessage::Key {
            session,
            seq,
            usage: HidUsage::keyboard(usage),
            down,
        })
    }

    fn button_msg(down: bool, seq: u32) -> InputMessageOf {
        Box::new(
            move |session| crosspane_protocol::msg::InputMessage::Button {
                session,
                seq,
                button: MouseButton::PRIMARY,
                down,
            },
        )
    }

    /// This node projects its window 10 to the paired peer, who accepts: projection `id` goes live.
    fn project(h: &mut Home, id: u64) {
        let peer = h.rig.peer;
        step(
            h,
            Input::Command(Command::Project {
                window: WindowId(10),
                to: peer,
            }),
        );
        step(
            h,
            projection_input(
                peer,
                ProjectionMessage::Accepted {
                    projection: ProjectionId(id),
                    size: PixelSize::new(400, 300),
                    scale: 1.0,
                },
            ),
        );
    }

    /// Return this node's own projection `id`.
    fn give_back(h: &mut Home, id: u64) {
        let key = ProjectionKey {
            source: h.rig.local,
            projection: ProjectionId(id),
        };
        step(h, Input::Command(Command::Return(key)));
    }

    #[test]
    fn the_installer_status_has_the_frozen_shape() {
        let mut h = home();
        // What the environment would otherwise decide.
        h.rig.agent.tracker.audio_off = false;
        h.rig.agent.tracker.gpu_off = false;
        h.rig.agent.tracker.discovery_off = false;
        // The legacy status keeps every field it had; `installer` is the one addition.
        let status = h.rig.agent.status();
        for field in [
            "node",
            "name",
            "listening",
            "gate_open",
            "armed",
            "controlling",
            "controlled_by",
            "session",
            "backends",
            "permissions",
            "displays",
            "peers",
            "audio",
            "home",
            "layout",
            "notices",
            "projections",
            "uptime_s",
        ] {
            assert!(status.get(field).is_some(), "{field}");
        }
        assert!(
            status["backends"].is_string(),
            "the legacy `backends` is still one line of text"
        );
        let instance = status_installer(&h)["instance"].clone();
        let mut names: Vec<_> = instance.as_object().unwrap().keys().cloned().collect();
        names.sort();
        assert_eq!(
            names,
            ["exe", "id", "pid", "runtime_dir", "started_unix_ms", "uid"]
        );
        assert_eq!(instance["pid"], json!(std::process::id()));
        assert_eq!(instance["uid"], json!(rustix::process::geteuid().as_raw()));
        assert!(instance["id"].is_u64());
        assert!(instance["started_unix_ms"].as_u64().unwrap() > 1_700_000_000_000);
        assert!(instance["exe"].is_string() && instance["runtime_dir"].is_string());
        let features: Vec<&str> = [
            ("private-vdisplay", cfg!(feature = "private-vdisplay")),
            ("video", cfg!(feature = "video")),
        ]
        .into_iter()
        .filter(|(_, on)| *on)
        .map(|(name, _)| name)
        .collect();
        let zeros = json!({
            "e1_controller_started": 0, "e1_controller_ended": 0,
            "e1_target_started": 0, "e1_target_ended": 0,
            "e1_injections_ok": 0, "e1_hud_shows": 0,
            "e1_chord_releases": 0, "e1_command_releases": 0,
            "e2_source_started": 0, "e2_source_returned": 0,
            "e2_dest_started": 0, "e2_dest_returned": 0,
            "e2_frames_presented": null,
            "e2_returns_failed": 0,
        });
        let missing = |name: &str, reason: &str| json!({ "name": name, "state": "missing", "reason": reason });
        let failed =
            |name: &str, reason: &str| json!({ "name": name, "state": "failed", "reason": reason });
        let ready = |name: &str| json!({ "name": name, "state": "ready", "reason": null });
        assert_eq!(
            anonymous(&h),
            json!({
                "schema_version": 1,
                "build": { "version": env!("CARGO_PKG_VERSION"), "features": features },
                "instance": "<instance>",
                "config_revision": "0000000000000000",
                "node": "<local>",
                "recovery_pending": 0,
                "startup_recovery": "none",
                "gate": {
                    "open": true, "session": "unlocked", "active": true,
                    "armed": true, "panic": false
                },
                "epochs": { "gate": 2, "grants": 0, "layout": 0, "backends": 0 },
                "backends": [
                    ready("capture"),
                    ready("keys"),
                    ready("pointer"),
                    failed("overlay", "construction_failed"),
                    missing("hotkeys", "not_supported"),
                    failed("keystore", "construction_failed"),
                    failed("windows", "construction_failed"),
                    failed("parking", "construction_failed"),
                    failed("frames", "construction_failed"),
                    failed("tray", "construction_failed"),
                    failed("links", "construction_failed"),
                    missing("gpu", "not_supported"),
                    ready("home"),
                    failed("audio", "construction_failed"),
                    failed("discovery", "unknown"),
                ],
                "keystore": "file",
                "permissions": [],
                "discovery": { "enabled": true, "running": false, "candidates": 0, "error": null },
                "tray": { "created": false },
                "audio": { "enabled": false, "active_peers": [], "frames_sent": 0, "frames_played": 0 },
                "settings_opened": 0,
                "peers": [{
                    "node": "<peer>",
                    "name": "peer-name",
                    "connected": false,
                    "link_generation": null,
                    "features": [],
                    "grants_given": ["present", "share"],
                    "last_source_parking": null,
                    "counters": zeros,
                }],
            })
        );
    }

    #[test]
    fn backend_states_and_reasons_are_the_bounded_vocabulary_in_the_frozen_order() {
        let granted = Arc::new(Mutex::new(Vec::new()));
        let mut h = home();
        h.rig.agent.platform.permissions = Box::new(MacLike(granted.clone()));
        h.rig.agent.platform.tray = Some(Box::new(FakeTray));
        let listed = |h: &Home| -> Vec<(String, String, Option<String>)> {
            status_installer(h)["backends"]
                .as_array()
                .unwrap()
                .iter()
                .map(|b| {
                    (
                        b["name"].as_str().unwrap().to_owned(),
                        b["state"].as_str().unwrap().to_owned(),
                        b["reason"].as_str().map(str::to_owned),
                    )
                })
                .collect()
        };
        let by_name = |list: &[(String, String, Option<String>)], name: &str| {
            let (_, state, reason) = list.iter().find(|(n, _, _)| n == name).unwrap().clone();
            (state, reason)
        };
        let list = listed(&h);
        let names: Vec<&str> = list.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "capture",
                "keys",
                "pointer",
                "overlay",
                "hotkeys",
                "keystore",
                "windows",
                "parking",
                "frames",
                "tray",
                "links",
                "gpu",
                "home",
                "audio",
                "discovery"
            ]
        );
        for (name, state, reason) in &list {
            assert!(
                ["ready", "missing", "blocked", "failed"].contains(&state.as_str()),
                "{name}"
            );
            match reason {
                None => assert_eq!(state, "ready", "{name}: only a ready backend has no reason"),
                Some(reason) => {
                    assert_ne!(state, "ready", "{name}");
                    assert!(
                        [
                            "not_supported",
                            "permission",
                            "construction_failed",
                            "worker_exited",
                            "disabled",
                            "unknown"
                        ]
                        .contains(&reason.as_str()),
                        "{name}: {reason}"
                    );
                }
            }
        }
        // Nothing the Mac needs is granted: what needs a permission is blocked on it, whether or
        // not the backend exists; what needs none is as it was.
        let blocked = || (String::from("blocked"), Some(String::from("permission")));
        for name in [
            "capture", "keys", "pointer", "windows", "parking", "frames", "audio",
        ] {
            assert_eq!(by_name(&list, name), blocked(), "{name}");
        }
        assert_eq!(by_name(&list, "tray"), ("ready".into(), None));
        assert_eq!(by_name(&list, "home"), ("ready".into(), None));
        // Granting is a change of the list, so the epoch moves (once, however often it is read).
        let before = epoch(&h, "backends");
        granted.lock().unwrap().extend([
            Permission::InputMonitoring,
            Permission::Accessibility,
            Permission::ScreenRecording,
            Permission::Microphone,
        ]);
        h.rig.agent.housekeeping();
        let list = listed(&h);
        assert_eq!(epoch(&h, "backends"), before + 1);
        assert_eq!(epoch(&h, "backends"), before + 1);
        assert_eq!(by_name(&list, "capture"), ("ready".into(), None));
        // Built, but not there: failed. Not granted by anyone: not the reason.
        assert_eq!(
            by_name(&list, "frames"),
            ("failed".into(), Some("construction_failed".into()))
        );
        assert_eq!(
            by_name(&list, "audio"),
            ("failed".into(), Some("construction_failed".into()))
        );
        // The permissions the Mac reports, by their frozen tokens.
        assert_eq!(
            status_installer(&h)["permissions"],
            json!([
                { "name": "screen_recording", "state": "granted" },
                { "name": "accessibility", "state": "granted" },
                { "name": "input_monitoring", "state": "granted" },
                { "name": "microphone", "state": "granted" },
            ])
        );
        granted
            .lock()
            .unwrap()
            .retain(|p| *p != Permission::Microphone);
        assert_eq!(
            status_installer(&h)["permissions"][3],
            json!({ "name": "microphone", "state": "not_granted" })
        );
    }

    #[test]
    fn what_is_switched_off_or_unavailable_says_why() {
        let mut h = home();
        let state = |h: &Home, name: &str| -> (String, Value) {
            let list = status_installer(h)["backends"].clone();
            let b = list
                .as_array()
                .unwrap()
                .iter()
                .find(|b| b["name"] == name)
                .unwrap()
                .clone();
            (b["state"].as_str().unwrap().to_owned(), b["reason"].clone())
        };
        let t = &mut h.rig.agent.tracker;
        (t.audio_off, t.gpu_off, t.discovery_off) = (true, true, true);
        for name in ["audio", "gpu", "discovery"] {
            assert_eq!(
                state(&h, name),
                ("missing".into(), json!("disabled")),
                "{name}"
            );
        }
        assert_eq!(status_installer(&h)["discovery"]["enabled"], json!(false));
        let t = &mut h.rig.agent.tracker;
        (t.audio_off, t.gpu_off, t.discovery_off) = (false, false, false);
        // Discovery that tried and could not start.
        h.rig.agent.tracker.discovery_error = Some("daemon_failed");
        assert_eq!(
            state(&h, "discovery"),
            ("failed".into(), json!("construction_failed"))
        );
        let discovery = status_installer(&h)["discovery"].clone();
        assert_eq!(
            discovery,
            json!({ "enabled": true, "running": false, "candidates": 0, "error": "daemon_failed" })
        );
        // The key file standing in for the OS store: forced by config, or not.
        let source = |h: &mut Home, file: bool, forced: bool| {
            h.rig.agent.startup.key_source = if file {
                crate::keys::KeySource::File
            } else {
                crate::keys::KeySource::OsStore
            };
            h.rig.agent.startup.force_file_keystore = forced;
        };
        source(&mut h, false, false);
        assert_eq!(state(&h, "keystore"), ("ready".into(), Value::Null));
        assert_eq!(status_installer(&h)["keystore"], json!("os_store"));
        source(&mut h, true, true);
        assert_eq!(state(&h, "keystore"), ("missing".into(), json!("disabled")));
        assert_eq!(status_installer(&h)["keystore"], json!("file"));
        source(&mut h, true, false);
        assert_eq!(
            state(&h, "keystore"),
            ("failed".into(), json!("construction_failed"))
        );
        // A leftover release bind that can't be confirmed gone: the home seat is not usable.
        h.rig.agent.home.fence = true;
        assert_eq!(state(&h, "home"), ("failed".into(), json!("unknown")));
        // A tray that exists.
        h.rig.agent.platform.tray = Some(Box::new(FakeTray));
        assert_eq!(status_installer(&h)["tray"], json!({ "created": true }));
        assert_eq!(state(&h, "tray"), ("ready".into(), Value::Null));
    }

    #[test]
    fn the_gate_section_follows_the_session_a_panic_and_a_re_arm() {
        let mut h = bare_scenario();
        let gate = |h: &Home| status_installer(h)["gate"].clone();
        assert_eq!(
            gate(&h),
            json!({ "open": true, "session": "unlocked", "active": true, "armed": true, "panic": false })
        );
        let seen = epoch(&h, "gate");
        step(&mut h, Input::Command(Command::Panic));
        assert_eq!(
            gate(&h),
            json!({ "open": false, "session": "unlocked", "active": true, "armed": false, "panic": true })
        );
        step(&mut h, Input::Command(Command::Rearm));
        assert_eq!(
            gate(&h),
            json!({ "open": true, "session": "unlocked", "active": true, "armed": true, "panic": false })
        );
        // The gate is open before and after, but it was shut in between: the epoch says so.
        assert_eq!(epoch(&h, "gate"), seen + 2);
        // The session's own report: locked, unknown, or inactive.
        let session = Arc::new(Mutex::new(crosspane_platform::SessionState {
            lock: crosspane_platform::LockState::Locked,
            active: None,
        }));
        h.rig.agent.platform.session = Box::new(VarSession(session.clone()));
        assert_eq!(gate(&h)["session"], json!("locked"));
        assert_eq!(gate(&h)["active"], Value::Null);
        *session.lock().unwrap() = crosspane_platform::SessionState {
            lock: crosspane_platform::LockState::Unknown,
            active: Some(false),
        };
        assert_eq!(gate(&h)["session"], json!("unknown"));
        assert_eq!(gate(&h)["active"], json!(false));
    }

    #[test]
    fn controller_sessions_are_counted_and_the_way_each_was_released_is_named() {
        let mut h = bare_scenario();
        assert_eq!(nonzero(&h), json!({}));
        // 1. The release command.
        cross_scenario(&mut h);
        assert_eq!(nonzero(&h), json!({ "e1_controller_started": 1 }));
        step(&mut h, Input::Command(Command::ReleaseControl));
        assert_eq!(h.rig.agent.engine.controlling(), None);
        let after_command = json!({
            "e1_controller_started": 1, "e1_controller_ended": 1, "e1_command_releases": 1
        });
        assert_eq!(nonzero(&h), after_command);
        // A release with no session ends nothing, and says nothing.
        step(&mut h, Input::Command(Command::ReleaseControl));
        assert_eq!(nonzero(&h), after_command);
        // 2. The chord, as the capture reports it (a release disarms crossing: re-arm to cross).
        step(&mut h, Input::Command(Command::Rearm));
        cross_scenario(&mut h);
        for usage in [0xE0, 0xE1, 0xE2, 0x29] {
            captured_key(&mut h, usage, true);
        }
        assert_eq!(h.rig.agent.engine.controlling(), None);
        for usage in [0x29, 0xE2, 0xE1, 0xE0] {
            captured_key(&mut h, usage, false);
        }
        assert_eq!(
            nonzero(&h),
            json!({
                "e1_controller_started": 2, "e1_controller_ended": 2,
                "e1_command_releases": 1, "e1_chord_releases": 1
            })
        );
        // 3. The platform's own report of the chord.
        step(&mut h, Input::Command(Command::Rearm));
        cross_scenario(&mut h);
        let at = h.rig.agent.test_now.unwrap();
        step(
            &mut h,
            Input::Hotkey(crosspane_platform::HotkeyEvent::Pressed { at }),
        );
        step(
            &mut h,
            Input::Hotkey(crosspane_platform::HotkeyEvent::Released { at }),
        );
        assert_eq!(h.rig.agent.engine.controlling(), None);
        assert_eq!(
            nonzero(&h),
            json!({
                "e1_controller_started": 3, "e1_controller_ended": 3,
                "e1_command_releases": 1, "e1_chord_releases": 2
            })
        );
        // 4. An end that is no release: the end counter moves, neither release counter does.
        step(&mut h, Input::Command(Command::Rearm));
        cross_scenario(&mut h);
        let peer = h.rig.peer;
        h.rig.agent.on_link(LinkEvent::Closed {
            peer,
            error: crosspane_protocol::link::LinkError::Closed,
        });
        assert_eq!(h.rig.agent.engine.controlling(), None);
        assert_eq!(
            nonzero(&h),
            json!({
                "e1_controller_started": 4, "e1_controller_ended": 4,
                "e1_command_releases": 1, "e1_chord_releases": 2
            })
        );
        // The legacy fields agree: nothing is left controlling. And the release notice is
        // counted, not shown: the notice history has no line for it.
        assert_eq!(h.rig.agent.status()["controlling"], Value::Null);
        assert!(
            !h.rig
                .agent
                .notices
                .iter()
                .any(|line| line.contains("ControlReleased")),
            "{:?}",
            h.rig.agent.notices
        );
    }

    /// A crossing to the paired peer whose `StartControl` is out and unanswered: the session id.
    fn handshake(h: &mut Home) -> crosspane_types::id::SessionId {
        let portal = h
            .rig
            .agent
            .emitted
            .iter()
            .rev()
            .find_map(|o| match o {
                Output::SetPortals(ps) => {
                    ps.iter().find(|p| p.display == DisplayId(1)).map(|p| p.id)
                }
                _ => None,
            })
            .unwrap();
        let at = h.rig.agent.test_now.unwrap();
        step(
            h,
            Input::Capture(CaptureEvent::EdgePressed {
                portal,
                position: 0.5,
                at,
            }),
        );
        let out = step(
            h,
            Input::Overlay(crosspane_platform::OverlayEvent::Visible(
                crosspane_engine::io::HUD,
            )),
        );
        let session = out
            .iter()
            .find_map(|o| match o {
                Output::SendControl {
                    msg: ControlMessage::StartControl { session, .. },
                    ..
                } => Some(*session),
                _ => None,
            })
            .unwrap();
        // The legacy `controlling` already names the peer; the session isn't established yet.
        assert_eq!(h.rig.agent.engine.controlling(), Some(h.rig.peer));
        assert_eq!(h.rig.agent.engine.control_established(), None);
        session
    }

    #[test]
    fn a_controller_handshake_that_is_never_established_counts_nothing() {
        let peer = |h: &Home| h.rig.peer;
        // The target refuses it.
        let mut h = bare_scenario();
        let session = handshake(&mut h);
        let p = peer(&h);
        step(
            &mut h,
            control_input(
                p,
                ControlMessage::ControlRefused {
                    session,
                    reason: Refusal::Busy,
                },
            ),
        );
        assert_eq!(h.rig.agent.engine.controlling(), None);
        assert_eq!(nonzero(&h), json!({}));
        // It is never answered.
        let mut h = bare_scenario();
        handshake(&mut h);
        tick(&mut h, 1_500);
        assert_eq!(h.rig.agent.engine.controlling(), None);
        assert_eq!(nonzero(&h), json!({}));
        // It is released: by the command, and by the chord (as the platform reports it).
        let mut h = bare_scenario();
        handshake(&mut h);
        step(&mut h, Input::Command(Command::ReleaseControl));
        assert_eq!(h.rig.agent.engine.controlling(), None);
        assert_eq!(nonzero(&h), json!({}));
        let mut h = bare_scenario();
        handshake(&mut h);
        let at = h.rig.agent.test_now.unwrap();
        step(
            &mut h,
            Input::Hotkey(crosspane_platform::HotkeyEvent::Pressed { at }),
        );
        step(
            &mut h,
            Input::Hotkey(crosspane_platform::HotkeyEvent::Released { at }),
        );
        assert_eq!(h.rig.agent.engine.controlling(), None);
        assert_eq!(nonzero(&h), json!({}));
        // A session that is established afterwards counts, once.
        step(&mut h, Input::Command(Command::Rearm));
        cross_scenario(&mut h);
        assert_eq!(nonzero(&h), json!({ "e1_controller_started": 1 }));
        step(&mut h, Input::Command(Command::ReleaseControl));
        assert_eq!(
            nonzero(&h),
            json!({
                "e1_controller_started": 1, "e1_controller_ended": 1, "e1_command_releases": 1
            })
        );
    }

    /// Both release paths that buffer the chord during a home exit's capture activation: each ends
    /// the session once, as a chord release, and nothing later adds a second.
    fn chord_events() -> Vec<CaptureEvent> {
        [0xE0, 0xE1, 0xE2, 0x29]
            .into_iter()
            .map(|usage| CaptureEvent::Key {
                usage: HidUsage::keyboard(usage),
                down: true,
                at: at(),
            })
            .collect()
    }

    fn controller_releases(h: &Home) -> Vec<(NodeId, ReleaseCause)> {
        h.rig
            .agent
            .emitted
            .iter()
            .filter_map(|o| match o {
                Output::Notice(Notice::ControlReleased { peer, cause }) => Some((*peer, *cause)),
                _ => None,
            })
            .collect()
    }

    /// After the release: the rest of the chord's keys come in, and the platform's own pair.
    fn late_chord_and_hotkey(h: &mut Home) {
        for usage in [0xE0, 0xE1, 0xE2, 0x29] {
            captured_key(h, usage, true);
        }
        let at = h.rig.agent.test_now.unwrap();
        step(
            h,
            Input::Hotkey(crosspane_platform::HotkeyEvent::Pressed { at }),
        );
        step(
            h,
            Input::Hotkey(crosspane_platform::HotkeyEvent::Released { at }),
        );
        for usage in [0x29, 0xE2, 0xE1, 0xE0] {
            captured_key(h, usage, false);
        }
    }

    #[test]
    fn a_chord_buffered_while_a_home_exit_activates_is_one_chord_release() {
        // `exit_activated`: the exit capture's snapshot has the modifiers held, and the chord's
        // key arrives during the activation (buffered, then reconciled when it is effective).
        let mut h = home_scenario();
        h.capture.lock().unwrap().start = Some(CaptureStart {
            held_keys: [0xE0, 0xE1, 0xE2]
                .into_iter()
                .map(HidUsage::keyboard)
                .collect(),
            lock_keys: LockKeys::default(),
        });
        h.capture.lock().unwrap().during_begin = vec![CaptureEvent::Key {
            usage: HidUsage::keyboard(0x29),
            down: true,
            at: at(),
        }];
        exit_home(&mut h);
        assert_eq!(h.rig.agent.engine.controlling(), None);
        let peer = h.rig.peer;
        assert_eq!(controller_releases(&h), [(peer, ReleaseCause::Chord)]);
        late_chord_and_hotkey(&mut h);
        assert_eq!(controller_releases(&h), [(peer, ReleaseCause::Chord)]);
        assert_eq!(
            nonzero(&h),
            json!({
                "e1_controller_started": 1, "e1_controller_ended": 1, "e1_chord_releases": 1,
                "e2_source_started": 1
            })
        );
        assert!(h.held.lock().unwrap().is_empty());
    }

    #[test]
    fn a_chord_buffered_until_a_home_exit_is_cancelled_is_one_chord_release() {
        // `cancel_exit`: the chord arrived during the activation, and the activation never
        // finished before its deadline, so the buffered keys are what the cancel finds.
        let mut h = home_scenario();
        h.capture.lock().unwrap().during_begin = chord_events();
        let edge_at = h.rig.agent.test_now.unwrap();
        step(
            &mut h,
            Input::Capture(CaptureEvent::EdgePressed {
                portal: PortalId((1 << 30) + 1),
                position: 0.5,
                at: edge_at,
            }),
        );
        h.rig
            .agent
            .feed(Input::Overlay(crosspane_platform::OverlayEvent::Visible(
                crosspane_engine::io::HUD,
            )));
        // The exit capture's events are the engine's to see; its answer is held back.
        let mut answer = None;
        while let Ok(event) = h.rig.events.try_recv() {
            if matches!(event, Event::Input(Input::CaptureBegun { .. })) {
                answer = Some(event);
            } else {
                h.rig.agent.on_event(event);
            }
        }
        let answer = answer.expect("the capture was begun");
        let peer = h.rig.peer;
        assert_eq!(h.rig.agent.engine.controlling(), Some(peer));
        assert!(controller_releases(&h).is_empty());
        tick(&mut h, 1_500);
        assert_eq!(h.rig.agent.engine.controlling(), None);
        assert_eq!(controller_releases(&h), [(peer, ReleaseCause::Chord)]);
        // The late answer of that capture, the rest of the chord and the platform's pair.
        h.rig.agent.on_event(answer);
        process_events(&mut h);
        late_chord_and_hotkey(&mut h);
        assert_eq!(controller_releases(&h), [(peer, ReleaseCause::Chord)]);
        assert_eq!(
            nonzero(&h),
            json!({
                "e1_controller_started": 1, "e1_controller_ended": 1, "e1_chord_releases": 1,
                "e2_source_started": 1
            })
        );
        assert!(h.held.lock().unwrap().is_empty());
    }

    #[test]
    fn target_sessions_count_the_injections_that_worked_and_the_indicator_that_showed() {
        let (mut h, session) = controlled();
        assert_eq!(nonzero(&h), json!({ "e1_target_started": 1 }));
        let injections = |h: &Home| {
            h.injected
                .lock()
                .unwrap()
                .iter()
                .filter(|what| what.starts_with("key") || what.starts_with("button"))
                .count() as u64
        };
        // A key's press and release, and a button's: four key and button events.
        from_controller(&mut h, session, key_msg(4, true, 1));
        from_controller(&mut h, session, key_msg(4, false, 2));
        from_controller(&mut h, session, button_msg(true, 3));
        from_controller(&mut h, session, button_msg(false, 4));
        assert_eq!(injections(&h), 4);
        assert_eq!(nonzero(&h)["e1_injections_ok"], json!(4));
        // Pointer motion is not a key or button event.
        let peer = h.rig.peer;
        step(
            &mut h,
            Input::Link(LinkEvent::Motion {
                peer,
                msg: crosspane_protocol::msg::PointerMessage {
                    session,
                    seq: 1,
                    display: DisplayId(1),
                    position: PointDevice::new(5.0, 5.0),
                },
            }),
        );
        assert!(h.injected.lock().unwrap().contains(&"move"));
        assert_eq!(nonzero(&h)["e1_injections_ok"], json!(4));
        // Attempts that the injector refuses are not counted.
        h.rig.agent.platform.keys = Some(Box::new(FailingKeys));
        from_controller(&mut h, session, key_msg(5, true, 5));
        from_controller(&mut h, session, key_msg(5, false, 6));
        assert_eq!(nonzero(&h)["e1_injections_ok"], json!(4));
        h.rig.agent.platform.keys = Some(Box::new(FakeKeys(h.injected.clone(), h.held.clone())));
        // The indicator counts when the overlay host confirms it is on screen, not before.
        assert!(nonzero(&h).get("e1_hud_shows").is_none());
        step(
            &mut h,
            Input::Overlay(crosspane_platform::OverlayEvent::Visible(
                crosspane_engine::io::TARGET_INDICATOR,
            )),
        );
        assert_eq!(nonzero(&h)["e1_hud_shows"], json!(1));
        // The host says `Visible` again after moving the overlay: no show is outstanding, so it
        // counts nothing.
        step(
            &mut h,
            Input::Overlay(crosspane_platform::OverlayEvent::Visible(
                crosspane_engine::io::TARGET_INDICATOR,
            )),
        );
        assert_eq!(nonzero(&h)["e1_hud_shows"], json!(1));
        // The capture HUD of a controller is not this counter.
        step(
            &mut h,
            Input::Overlay(crosspane_platform::OverlayEvent::Visible(
                crosspane_engine::io::HUD,
            )),
        );
        assert_eq!(nonzero(&h)["e1_hud_shows"], json!(1));
        // The controller ends the session; everything it pressed is released.
        step(
            &mut h,
            control_input(
                peer,
                ControlMessage::EndControl {
                    session,
                    reason: crosspane_protocol::msg::EndReason::Released,
                },
            ),
        );
        assert_eq!(h.rig.agent.engine.controlled_by(), None);
        assert!(h.held.lock().unwrap().is_empty());
        assert_eq!(
            nonzero(&h),
            json!({
                "e1_target_started": 1, "e1_target_ended": 1,
                "e1_injections_ok": 4, "e1_hud_shows": 1
            })
        );
    }

    #[test]
    fn an_indicator_the_host_cannot_show_or_was_never_asked_for_counts_nothing() {
        // The host says it can't put it on screen; a `Visible` that follows belongs to no show.
        let (mut h, _) = controlled();
        step(
            &mut h,
            Input::Overlay(crosspane_platform::OverlayEvent::Unavailable(
                crosspane_engine::io::TARGET_INDICATOR,
            )),
        );
        step(
            &mut h,
            Input::Overlay(crosspane_platform::OverlayEvent::Visible(
                crosspane_engine::io::TARGET_INDICATOR,
            )),
        );
        assert_eq!(nonzero(&h), json!({ "e1_target_started": 1 }));
        // A show the host refuses outright owes no outcome, so a stray one is nobody's.
        let mut h = bare_scenario();
        h.rig.agent.platform.overlay = Some(Box::new(RefusingOverlay(Arc::default())));
        let (peer, session) = (h.rig.peer, crosspane_types::id::SessionId(77));
        start_control(&mut h, peer, session);
        step(
            &mut h,
            Input::Overlay(crosspane_platform::OverlayEvent::Visible(
                crosspane_engine::io::TARGET_INDICATOR,
            )),
        );
        assert_eq!(nonzero(&h), json!({ "e1_target_started": 1 }));
    }

    #[test]
    fn an_indicator_answer_that_could_belong_to_either_of_two_peers_credits_nobody() {
        // Peer P controls and its indicator is shown; P ends and peer Q starts, whose indicator
        // is shown too, all before the host answers for P's. The overlay events carry only the
        // shared indicator id, so neither answer can be told apart: nobody is credited.
        let (mut h, session) = controlled();
        let p = h.rig.peer;
        let q = NodeId([3; 32]);
        step(
            &mut h,
            Input::Grants(
                [
                    (q, [Capability::InputAccept].into()),
                    (
                        p,
                        [
                            Capability::WindowShare,
                            Capability::WindowPresent,
                            Capability::InputAccept,
                        ]
                        .into(),
                    ),
                ]
                .into(),
            ),
        );
        step(
            &mut h,
            control_input(
                p,
                ControlMessage::EndControl {
                    session,
                    reason: crosspane_protocol::msg::EndReason::Released,
                },
            ),
        );
        let out = start_control(&mut h, q, crosspane_types::id::SessionId(78));
        assert!(
            out.iter()
                .any(|o| matches!(o, Output::Notice(Notice::ControlledBy(who)) if *who == q)),
            "{out:?}"
        );
        for _ in 0..2 {
            step(
                &mut h,
                Input::Overlay(crosspane_platform::OverlayEvent::Visible(
                    crosspane_engine::io::TARGET_INDICATOR,
                )),
            );
        }
        let shows = |h: &Home, who: NodeId| {
            h.rig
                .agent
                .tracker
                .counters
                .get(&who)
                .map_or(0, |c| c.e1_hud_shows)
        };
        assert_eq!((shows(&h, p), shows(&h, q)), (0, 0));
        // Once the host has caught up, the next show and its answer are attributed again.
        step(
            &mut h,
            control_input(
                q,
                ControlMessage::EndControl {
                    session: crosspane_types::id::SessionId(78),
                    reason: crosspane_protocol::msg::EndReason::Released,
                },
            ),
        );
        settled(&mut h);
        start_control(&mut h, p, crosspane_types::id::SessionId(79));
        step(
            &mut h,
            Input::Overlay(crosspane_platform::OverlayEvent::Visible(
                crosspane_engine::io::TARGET_INDICATOR,
            )),
        );
        assert_eq!((shows(&h, p), shows(&h, q)), (1, 0));
    }

    /// The indicator was hidden longer ago than the host may still deliver its transitions.
    fn settled(h: &mut Home) {
        let hidden = h.rig.agent.tracker.indicator_hidden_at.as_mut();
        *hidden.expect("the indicator was hidden") -= INDICATOR_SETTLE;
    }

    #[test]
    fn a_late_transition_of_an_earlier_indicator_credits_nobody() {
        // P's indicator is confirmed and credited. P ends and Q starts, and Q's show is accepted
        // before a transition the host reported for P's indicator (on screen again after a
        // move) arrives. That `Visible` is P's, not an answer to Q's show; Q's own answer then
        // says it could not be shown. Neither may credit Q.
        let (mut h, session) = controlled();
        let p = h.rig.peer;
        let q = NodeId([3; 32]);
        step(
            &mut h,
            Input::Grants(
                [
                    (q, [Capability::InputAccept].into()),
                    (
                        p,
                        [
                            Capability::WindowShare,
                            Capability::WindowPresent,
                            Capability::InputAccept,
                        ]
                        .into(),
                    ),
                ]
                .into(),
            ),
        );
        let indicator = |visible: bool| {
            let id = crosspane_engine::io::TARGET_INDICATOR;
            Input::Overlay(if visible {
                crosspane_platform::OverlayEvent::Visible(id)
            } else {
                crosspane_platform::OverlayEvent::Unavailable(id)
            })
        };
        step(&mut h, indicator(true));
        step(
            &mut h,
            control_input(
                p,
                ControlMessage::EndControl {
                    session,
                    reason: crosspane_protocol::msg::EndReason::Released,
                },
            ),
        );
        start_control(&mut h, q, crosspane_types::id::SessionId(78));
        step(&mut h, indicator(true));
        step(&mut h, indicator(false));
        let shows = |h: &Home, who: NodeId| {
            h.rig
                .agent
                .tracker
                .counters
                .get(&who)
                .map_or(0, |c| c.e1_hud_shows)
        };
        assert_eq!((shows(&h, p), shows(&h, q)), (1, 0));
    }

    #[test]
    fn a_projections_input_is_not_an_e1_injection() {
        // The peer's keys into a window this node projects go through the same injectors, but
        // they are E2's: no E1 counter moves.
        let mut h = projected_scenario();
        step(
            &mut h,
            Input::Windows(WindowEvent::Focused(Some(WindowId(10)))),
        );
        for (seq, down) in [(1, true), (2, false)] {
            proj_key(&mut h, seq, down);
        }
        assert!(
            h.injected
                .lock()
                .unwrap()
                .iter()
                .any(|what| what.starts_with("key")),
            "the window got its keys: {:?}",
            h.injected.lock().unwrap()
        );
        assert!(
            nonzero(&h).get("e1_injections_ok").is_none(),
            "{}",
            nonzero(&h)
        );
    }

    #[test]
    fn the_status_does_not_depend_on_what_was_typed() {
        // Two runs that type different keys (and a different number of them, ending in the same
        // count) are indistinguishable in the status: nothing about a key is in it.
        let run = |usages: &[u16]| {
            let (mut h, session) = controlled();
            let mut seq = 0;
            for usage in usages {
                for down in [true, false] {
                    seq += 1;
                    from_controller(&mut h, session, key_msg(*usage, down, seq));
                }
            }
            h.injected.lock().unwrap().clear();
            let text = anonymous(&h);
            // And no field or value names one: the usages, as numbers or in words.
            let flat = text.to_string();
            for usage in usages {
                assert!(
                    !flat.contains(&format!("\"usage\":{usage}")) && !flat.contains("usage"),
                    "{flat}"
                );
            }
            for word in [
                "typed",
                "scancode",
                "keycode",
                "character",
                "title",
                "pcm",
                "samples",
            ] {
                assert!(!flat.contains(word), "{word} in {flat}");
            }
            text
        };
        assert_eq!(run(&[4, 5, 6]), run(&[0x1A, 0x1B, 0x2C]));
    }

    #[test]
    fn an_e2_source_projection_counts_started_and_returned_and_keeps_its_journal_count() {
        let mut h = projected_scenario();
        assert_eq!(nonzero(&h), json!({ "e2_source_started": 1 }));
        // The window is parked: its journal entry is unresolved until it is restored.
        assert_eq!(status_installer(&h)["recovery_pending"], json!(1));
        give_back(&mut h, 1);
        assert_eq!(
            nonzero(&h),
            json!({ "e2_source_started": 1, "e2_source_returned": 1 })
        );
        assert_eq!(status_installer(&h)["recovery_pending"], json!(0));
        // An offer the peer refuses never went live: nothing is counted for it.
        let peer = h.rig.peer;
        step(
            &mut h,
            Input::Command(Command::Project {
                window: WindowId(10),
                to: peer,
            }),
        );
        step(
            &mut h,
            projection_input(
                peer,
                ProjectionMessage::Refused {
                    projection: ProjectionId(2),
                    reason: Refusal::Permission,
                },
            ),
        );
        assert_eq!(
            nonzero(&h),
            json!({ "e2_source_started": 1, "e2_source_returned": 1 })
        );
    }

    #[test]
    fn startup_recovery_reports_what_the_platform_kept_and_is_apart_from_recovery_pending() {
        use crate::platform::StartupRecovery;
        let mut h = home();
        for (kept, shown) in [
            (StartupRecovery::Restored(2), "restored"),
            (StartupRecovery::NothingParked, "nothing_parked"),
            (StartupRecovery::Failed, "failed"),
            (StartupRecovery::None, "none"),
        ] {
            h.rig.agent.platform.startup_recovery = kept;
            let status = status_installer(&h);
            assert_eq!(status["startup_recovery"], json!(shown), "{kept:?}");
            // This instance parked nothing, whatever the startup found.
            assert_eq!(status["recovery_pending"], json!(0), "{kept:?}");
        }
        // `recovery_pending` is this instance's own parked-but-not-restored windows (a failed
        // startup recovery is `startup_recovery`'s to say): one while a projection holds its
        // window, none after the return, and `startup_recovery` is not touched by either.
        let mut h = projected_scenario();
        h.rig.agent.platform.startup_recovery = StartupRecovery::Failed;
        let shown = status_installer(&h);
        assert_eq!(shown["recovery_pending"], json!(1));
        assert_eq!(shown["startup_recovery"], json!("failed"));
        give_back(&mut h, 1);
        let shown = status_installer(&h);
        assert_eq!(shown["recovery_pending"], json!(0));
        assert_eq!(shown["startup_recovery"], json!("failed"));
    }

    /// Frame capture that refuses to start.
    struct FailingFrames;

    impl crosspane_platform::FrameCapture for FailingFrames {
        fn start(
            &mut self,
            _target: CaptureTarget,
            _crop: Option<crosspane_types::geom::PixelRect>,
            _max_fps: u32,
            _sink: Arc<dyn EventSink<FrameEvent>>,
        ) -> Result<StreamId, PlatformError> {
            Err(PlatformError::Backend("fixture: no capture".into()))
        }
        fn set_crop(
            &mut self,
            _stream: StreamId,
            _crop: Option<crosspane_types::geom::PixelRect>,
        ) -> Result<(), PlatformError> {
            Ok(())
        }
        fn stop(&mut self, _stream: StreamId) -> Result<(), PlatformError> {
            Ok(())
        }
    }

    #[test]
    fn a_projection_whose_capture_fails_or_is_cancelled_never_started() {
        // The capture refuses to start: the window was parked and is put back, and none of
        // start, return or parking is counted.
        let mut h = bare_scenario();
        h.rig.agent.platform.frames = Some(Box::new(FailingFrames));
        project(&mut h, 1);
        assert!(
            h.rig
                .agent
                .notices
                .iter()
                .any(|n| n.contains("projection 1 ended")),
            "{:?}",
            h.rig.agent.notices
        );
        assert_eq!(nonzero(&h), json!({}));
        let shown = status_installer(&h);
        assert_eq!(shown["peers"][0]["last_source_parking"], Value::Null);
        assert_eq!(shown["recovery_pending"], json!(0));
        // The capture would start, but the projection is returned first: the answer that comes
        // late finds nothing to start.
        let mut h = bare_scenario();
        let peer = h.rig.peer;
        step(
            &mut h,
            Input::Command(Command::Project {
                window: WindowId(10),
                to: peer,
            }),
        );
        h.rig.agent.feed(projection_input(
            peer,
            ProjectionMessage::Accepted {
                projection: ProjectionId(1),
                size: PixelSize::new(400, 300),
                scale: 1.0,
            },
        ));
        // The window's parking result is the engine's to see; that starts the capture, whose
        // answer is queued behind it.
        let parked = loop {
            let event = h
                .rig
                .events
                .recv_timeout(Duration::from_secs(1))
                .expect("the window was parked");
            if matches!(
                &event,
                Event::Parking(crate::parking_worker::Completion {
                    outcome: crate::parking_worker::Outcome::Parked { .. },
                    ..
                })
            ) {
                break event;
            }
            h.rig.agent.on_event(event);
        };
        assert!(matches!(
            &parked,
            Event::Parking(crate::parking_worker::Completion {
                outcome: crate::parking_worker::Outcome::Parked { .. },
                ..
            })
        ));
        h.rig.agent.on_event(parked);
        assert!(
            h.rig
                .agent
                .pending
                .iter()
                .any(|input| matches!(input, Input::CaptureStarted { result: Ok(_), .. })),
            "{:?}",
            h.rig.agent.pending
        );
        give_back(&mut h, 1);
        h.rig.agent.settle();
        assert_eq!(nonzero(&h), json!({}));
        let shown = status_installer(&h);
        assert_eq!(shown["peers"][0]["last_source_parking"], Value::Null);
        assert_eq!(shown["recovery_pending"], json!(0));
    }

    #[test]
    fn a_window_that_is_destroyed_while_projected_is_neither_returned_nor_a_failed_return() {
        let mut h = projected_scenario();
        assert_eq!(nonzero(&h), json!({ "e2_source_started": 1 }));
        step(&mut h, Input::Windows(WindowEvent::Removed(WindowId(10))));
        assert!(
            h.rig
                .agent
                .notices
                .iter()
                .any(|n| n.contains("projection 1 ended: WindowClosed")),
            "{:?}",
            h.rig.agent.notices
        );
        // Its restore "worked" (there was nothing to restore), and nothing came back.
        assert_eq!(nonzero(&h), json!({ "e2_source_started": 1 }));
        assert_eq!(status_installer(&h)["recovery_pending"], json!(0));
    }

    #[test]
    fn a_return_whose_window_does_not_come_back_is_a_failed_return() {
        let mut h = projected_scenario();
        replace_parking(
            &mut h.rig.agent,
            Box::new(Parking {
                kind: crosspane_platform::ParkingKind::Twin,
                restore_fails: true,
            }),
        );
        give_back(&mut h, 1);
        assert_eq!(
            nonzero(&h),
            json!({ "e2_source_started": 1, "e2_returns_failed": 1 })
        );
        // The journal entry is still there: the window is not restored.
        assert_eq!(status_installer(&h)["recovery_pending"], json!(1));
    }

    #[test]
    fn last_source_parking_follows_the_kind_of_the_latest_projection_to_the_peer() {
        let mut h = bare_scenario();
        let parking = |h: &Home| status_installer(h)["peers"][0]["last_source_parking"].clone();
        assert_eq!(parking(&h), Value::Null);
        // The default fixture parks on a twin (the Mac's virtual display reports the same).
        project(&mut h, 1);
        assert_eq!(parking(&h), json!("twin"));
        give_back(&mut h, 1);
        // Never reset: the return doesn't clear it.
        assert_eq!(parking(&h), json!("twin"));
        replace_parking(
            &mut h.rig.agent,
            Box::new(Parking {
                kind: crosspane_platform::ParkingKind::Mirror,
                restore_fails: false,
            }),
        );
        project(&mut h, 2);
        assert_eq!(parking(&h), json!("mirror"));
        assert_eq!(nonzero(&h)["e2_source_started"], json!(2));
        give_back(&mut h, 2);
        assert_eq!(parking(&h), json!("mirror"));
    }

    #[test]
    fn an_e2_destination_proxy_counts_opened_and_returned_but_not_other_ends() {
        let mut h = bare_scenario();
        let peer = h.rig.peer;
        let key = |n: u64| ProjectionKey {
            source: peer,
            projection: ProjectionId(n),
        };
        // The peer offers a window. There is no proxy host in this fixture, so the agent's own
        // answer to the engine is a failure: drop that and give the host's `Opened` ourselves.
        let offered = |h: &mut Home, n: u64| {
            h.rig.agent.feed(projection_input(
                peer,
                ProjectionMessage::Start {
                    projection: ProjectionId(n),
                    window: crosspane_protocol::projection::WindowSummary {
                        title: "t".into(),
                        app_id: "a".into(),
                    },
                    size: PixelSize::new(400, 300),
                },
            ));
            h.rig.agent.pending.clear();
        };
        let opened = |h: &mut Home, n: u64| {
            // The host's window for it (there is no host here: the id it would have).
            h.rig.agent.proxy_ids.open(key(n));
            h.rig.agent.feed(Input::ProxyOpened {
                key: key(n),
                result: Ok((PixelSize::new(400, 300), 1.0)),
            });
        };
        // The host takes the `Close` it is sent (WP-4.5: "returned" is "close queued").
        h.rig.agent.tracker.close_seam = Some(|_| true);
        offered(&mut h, 5);
        opened(&mut h, 5);
        assert_eq!(nonzero(&h), json!({ "e2_dest_started": 1 }));
        h.rig.agent.feed(Input::Command(Command::Return(key(5))));
        assert_eq!(
            nonzero(&h),
            json!({ "e2_dest_started": 1, "e2_dest_returned": 1 })
        );
        // The source ends one (its window closed): the proxy closes, but not because of a return.
        offered(&mut h, 6);
        opened(&mut h, 6);
        h.rig.agent.feed(projection_input(
            peer,
            ProjectionMessage::End {
                projection: ProjectionId(6),
                reason: crosspane_protocol::projection::ProjectionEndReason::WindowClosed,
            },
        ));
        assert_eq!(
            nonzero(&h),
            json!({ "e2_dest_started": 2, "e2_dest_returned": 1 })
        );
        // A proxy that never opened never counts, whatever ends it.
        offered(&mut h, 7);
        h.rig.agent.feed(Input::ProxyOpened {
            key: key(7),
            result: Err(Failure::Other),
        });
        assert_eq!(
            nonzero(&h),
            json!({ "e2_dest_started": 2, "e2_dest_returned": 1 })
        );
    }

    #[test]
    fn a_destination_return_whose_close_was_not_queued_is_a_failed_return() {
        let mut h = bare_scenario();
        let peer = h.rig.peer;
        let key = ProjectionKey {
            source: peer,
            projection: ProjectionId(5),
        };
        h.rig.agent.feed(projection_input(
            peer,
            ProjectionMessage::Start {
                projection: ProjectionId(5),
                window: crosspane_protocol::projection::WindowSummary {
                    title: "t".into(),
                    app_id: "a".into(),
                },
                size: PixelSize::new(400, 300),
            },
        ));
        h.rig.agent.pending.clear();
        h.rig.agent.proxy_ids.open(key);
        h.rig.agent.feed(Input::ProxyOpened {
            key,
            result: Ok((PixelSize::new(400, 300), 1.0)),
        });
        assert_eq!(nonzero(&h), json!({ "e2_dest_started": 1 }));
        // The host has exited: sending it the `Close` fails. The return happened (the engine
        // ended the projection) but isn't counted as one; it is the return error.
        h.rig.agent.tracker.close_seam = Some(|_| false);
        h.rig.agent.feed(Input::Command(Command::Return(key)));
        assert_eq!(
            nonzero(&h),
            json!({ "e2_dest_started": 1, "e2_returns_failed": 1 })
        );
        // Without a window host at all (the production state when none started) it is the same:
        // no `Close` can be queued.
        h.rig.agent.tracker.close_seam = None;
        let again = ProjectionKey {
            projection: ProjectionId(6),
            ..key
        };
        h.rig.agent.feed(projection_input(
            peer,
            ProjectionMessage::Start {
                projection: ProjectionId(6),
                window: crosspane_protocol::projection::WindowSummary {
                    title: "t".into(),
                    app_id: "a".into(),
                },
                size: PixelSize::new(400, 300),
            },
        ));
        h.rig.agent.pending.clear();
        h.rig.agent.proxy_ids.open(again);
        h.rig.agent.feed(Input::ProxyOpened {
            key: again,
            result: Ok((PixelSize::new(400, 300), 1.0)),
        });
        h.rig.agent.feed(Input::Command(Command::Return(again)));
        assert_eq!(
            nonzero(&h),
            json!({ "e2_dest_started": 2, "e2_returns_failed": 2 })
        );
    }

    fn report_presented_without_feeding_engine(h: &mut Home, id: u64, frames: u32) {
        let fed = h.rig.agent.fed.clone();
        let pending = h.rig.agent.pending.clone();
        h.rig.agent.on_host(HostEvent::Presented { id, frames });
        assert_eq!(h.rig.agent.fed, fed, "Presented fed an engine input");
        assert_eq!(
            h.rig.agent.pending, pending,
            "Presented queued an engine input"
        );
    }

    #[test]
    fn frames_presented_is_null_until_the_renderer_reports_and_then_a_number() {
        let mut h = bare_scenario();
        let presented =
            |h: &Home| status_installer(h)["peers"][0]["counters"]["e2_frames_presented"].clone();
        assert_eq!(presented(&h), Value::Null);
        let key = ProjectionKey {
            source: h.rig.peer,
            projection: ProjectionId(1),
        };
        let id = h.rig.agent.proxy_ids.open(key);
        // The decoder showing frames is not the renderer presenting them.
        assert_eq!(presented(&h), Value::Null);
        report_presented_without_feeding_engine(&mut h, id, 3);
        assert_eq!(presented(&h), json!(3));
        h.rig.agent.proxy_ids.close(key);
        assert_eq!(presented(&h), json!(3));
        report_presented_without_feeding_engine(&mut h, id, 7);
        assert_eq!(presented(&h), json!(3));
    }

    #[test]
    fn presented_for_an_unknown_proxy_is_ignored() {
        let mut h = bare_scenario();
        let presented =
            |h: &Home| status_installer(h)["peers"][0]["counters"]["e2_frames_presented"].clone();
        report_presented_without_feeding_engine(&mut h, u64::MAX, 5);
        assert_eq!(presented(&h), Value::Null);
        let key = ProjectionKey {
            source: h.rig.peer,
            projection: ProjectionId(1),
        };
        let id = h.rig.agent.proxy_ids.open(key);
        report_presented_without_feeding_engine(&mut h, id, 2);
        report_presented_without_feeding_engine(&mut h, u64::MAX, 5);
        assert_eq!(presented(&h), json!(2));
    }

    #[test]
    fn the_grants_epoch_advances_on_real_trust_changes_only() {
        let mut h = bare_scenario();
        let allow = |h: &mut Home, capability: &str, allow: bool| {
            h.rig.agent.on_ctl(Request::Allow {
                peer: "peer-name".into(),
                capability: capability.into(),
                allow,
            })
        };
        let start = epoch(&h, "grants");
        // The peer is allowed input already (the fixture's doing): allowing it again changes
        // nothing.
        assert!(allow(&mut h, "input", true).ok);
        assert_eq!(epoch(&h, "grants"), start);
        assert!(allow(&mut h, "input", false).ok);
        assert_eq!(epoch(&h, "grants"), start + 1);
        assert!(allow(&mut h, "input", false).ok);
        assert_eq!(epoch(&h, "grants"), start + 1);
        assert!(allow(&mut h, "input", true).ok);
        assert_eq!(epoch(&h, "grants"), start + 2);
        // A request that is refused changes nothing.
        assert!(!allow(&mut h, "sound", true).ok);
        assert_eq!(epoch(&h, "grants"), start + 2);
        assert_eq!(
            status_installer(&h)["peers"][0]["grants_given"],
            json!(["input", "present", "share"])
        );
        // A change of the trust file under the running agent (a reload).
        let peer = h.rig.peer;
        h.rig
            .agent
            .trust
            .update(|t| {
                t.set_grant(peer, Capability::WindowBrowse, true)
                    .map_err(|e| anyhow::anyhow!("{e}"))
            })
            .unwrap();
        h.rig.agent.last_trust_check = Instant::now() - Duration::from_secs(10);
        h.rig.agent.housekeeping();
        assert_eq!(
            epoch(&h, "grants"),
            start + 2,
            "its own change was counted when made"
        );
        // Forgetting the peer.
        let forgotten = h.rig.agent.on_ctl(Request::Forget {
            peer: "peer-name".into(),
        });
        assert!(forgotten.ok, "{forgotten:?}");
        assert_eq!(epoch(&h, "grants"), start + 3);
        assert_eq!(status_installer(&h)["peers"], json!([]));
    }

    #[test]
    fn the_layout_epoch_advances_on_accepted_places_and_display_changes() {
        let mut h = bare_scenario();
        let start = epoch(&h, "layout");
        let local = display(1, 1.0, (0.0, 0.0), (1000, 1000));
        h.rig
            .agent
            .on_event(Event::LocalDisplays(vec![local.clone()]));
        // A local display change is one advance, though it also moved placements (the first
        // display is placed by default).
        let after_display = epoch(&h, "layout");
        assert_eq!(after_display, start + 1);
        // The same display set again is no change.
        h.rig
            .agent
            .on_event(Event::LocalDisplays(vec![local.clone()]));
        assert_eq!(epoch(&h, "layout"), after_display);
        // A change that moves no placement (the display's name) is one advance too.
        let renamed = DisplayInfo {
            name: "renamed".into(),
            ..local.clone()
        };
        h.rig.agent.on_event(Event::LocalDisplays(vec![renamed]));
        let after_rename = epoch(&h, "layout");
        assert_eq!(after_rename, after_display + 1);
        h.rig.agent.on_event(Event::LocalDisplays(vec![local]));
        assert_eq!(epoch(&h, "layout"), after_rename + 1);
        let after_display = epoch(&h, "layout");
        let place = |h: &mut Home, x: f64| {
            h.rig.agent.on_ctl(Request::Place {
                placements: vec![crate::ctl::PlaceEntry {
                    node: "local".into(),
                    display: 1,
                    origin_mm: [x, 0.0],
                }],
            })
        };
        // Clear of the peer's display, which the fixture puts at 100 mm.
        let accepted = place(&mut h, -500.0);
        assert!(accepted.ok, "{accepted:?}");
        let after_place = epoch(&h, "layout");
        assert_eq!(after_place, after_display + 1);
        // A place that is refused changes nothing.
        assert!(!place(&mut h, f64::NAN).ok);
        assert_eq!(epoch(&h, "layout"), after_place);
        // A peer's layout that changes the placements.
        let peer = h.rig.peer;
        let theirs = Placement {
            node: h.rig.local,
            display: DisplayId(1),
            origin: crosspane_types::geom::PointMm::new(50.0, 0.0),
            version: 1_000,
        };
        h.rig.agent.on_link(LinkEvent::Control {
            peer,
            msg: ControlMessage::Layout(vec![theirs]),
        });
        let after_received = epoch(&h, "layout");
        assert_eq!(after_received, after_place + 1);
        // The same layout again changes nothing.
        h.rig.agent.on_link(LinkEvent::Control {
            peer,
            msg: ControlMessage::Layout(vec![theirs]),
        });
        assert_eq!(epoch(&h, "layout"), after_received);
        // A peer's display set that changes the placements is one advance; the same set again,
        // none.
        let displays = vec![
            display(1, 1.0, (0.0, 0.0), (1000, 1000)),
            display(2, 1.0, (0.0, 0.0), (500, 500)),
        ];
        let announce = |h: &mut Home| {
            h.rig.agent.on_link(LinkEvent::Control {
                peer,
                msg: ControlMessage::Displays(displays.clone()),
            });
        };
        announce(&mut h);
        let after_displays = epoch(&h, "layout");
        assert_eq!(after_displays, after_received + 1);
        announce(&mut h);
        assert_eq!(epoch(&h, "layout"), after_displays);
    }

    #[test]
    fn link_generation_is_null_until_the_first_connection_and_counts_each_one() {
        let mut h = home();
        let peer = h.rig.peer;
        let generation = |h: &Home| status_installer(h)["peers"][0]["link_generation"].clone();
        let hello = || Hello {
            minor: crosspane_protocol::PROTOCOL_MINOR,
            name: "peer-announced".into(),
            features: vec!["e1".into(), "audio".into()],
            displays: Vec::new(),
        };
        assert_eq!(generation(&h), Value::Null);
        h.rig.agent.on_link(LinkEvent::Control {
            peer,
            msg: ControlMessage::Hello(hello()),
        });
        assert_eq!(generation(&h), json!(1));
        let shown = status_installer(&h)["peers"][0].clone();
        assert_eq!(shown["connected"], json!(true));
        assert_eq!(shown["features"], json!(["e1", "audio"]));
        h.rig.agent.on_link(LinkEvent::Closed {
            peer,
            error: crosspane_protocol::link::LinkError::Closed,
        });
        assert_eq!(generation(&h), json!(1));
        assert_eq!(status_installer(&h)["peers"][0]["connected"], json!(false));
        // A reconnect bumps it, and so does a connection that replaces a running one.
        h.rig.agent.on_link(LinkEvent::Control {
            peer,
            msg: ControlMessage::Hello(hello()),
        });
        assert_eq!(generation(&h), json!(2));
        h.rig.agent.on_link(LinkEvent::HelloRefresh {
            peer,
            hello: hello(),
        });
        assert_eq!(generation(&h), json!(3));
    }

    #[test]
    fn settings_opened_counts_only_spawns_that_worked() {
        let mut h = home();
        let opened = |h: &Home| status_installer(h)["settings_opened"].clone();
        h.rig.agent.tracker.spawn_settings = || Ok(());
        h.rig.agent.tray_action(TrayAction::OpenApp);
        assert_eq!(opened(&h), json!(1));
        h.rig.agent.tracker.spawn_settings =
            || Err(std::io::Error::other("fixture: no such program"));
        h.rig.agent.tray_action(TrayAction::OpenApp);
        assert_eq!(opened(&h), json!(1));
        assert!(
            h.rig
                .agent
                .notices
                .back()
                .is_some_and(|n| n.contains("could not open the settings app"))
        );
        h.rig.agent.tracker.spawn_settings = || Ok(());
        h.rig.agent.tray_action(TrayAction::OpenApp);
        assert_eq!(opened(&h), json!(2));
    }

    #[test]
    fn audio_peers_are_those_with_a_live_speaker_session_either_way_and_the_counters_pass_through()
    {
        let mut h = home();
        h.rig.agent.audio = Some(Box::new(StatsPlane(WorkerStats {
            sent: 7,
            played: 9,
            congested: 100,
            ..WorkerStats::default()
        })));
        let audio = |h: &Home| status_installer(h)["audio"].clone();
        assert_eq!(
            audio(&h),
            json!({ "enabled": true, "active_peers": [], "frames_sent": 7, "frames_played": 9 })
        );
        let (a, b) = (NodeId([1; 32]), h.rig.peer);
        let session = |peer, stream, kind, endpoint| {
            (
                AudioKey {
                    peer,
                    stream: crosspane_types::audio::AudioStreamId(stream),
                    generation: u64::from(stream),
                },
                kind,
                endpoint,
            )
        };
        use crosspane_engine::io::AudioEndpoint::{
            LocalPlayback, VirtualMicrophone, VirtualSpeaker,
        };
        // One session plays here, one is this node's virtual speaker playing there; a microphone
        // session is no speaker session.
        let sessions = [
            session(b, 1, AudioKind::Speaker, LocalPlayback),
            session(a, 2, AudioKind::Speaker, VirtualSpeaker),
            session(a, 3, AudioKind::Microphone, VirtualMicrophone),
        ];
        for (key, kind, endpoint) in sessions {
            h.rig.agent.execute(vec![Output::StartAudioStream {
                key,
                kind,
                endpoint,
            }]);
        }
        let mut both = [a, b];
        both.sort();
        assert_eq!(
            audio(&h)["active_peers"],
            json!(both.iter().map(NodeId::to_string).collect::<Vec<_>>())
        );
        // The engine stops one: only the other peer is left.
        h.rig
            .agent
            .execute(vec![Output::StopAudioStream { key: sessions[1].0 }]);
        assert_eq!(audio(&h)["active_peers"], json!([b.to_string()]));
        // A peer going away takes its sessions with it.
        h.rig
            .agent
            .execute(vec![Output::RemoveAudioPeer { peer: b }]);
        assert_eq!(audio(&h)["active_peers"], json!([]));
    }
}
