//! The deterministic controller role: portals, capture fences and remote input sessions.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Duration;

use crosspane_input::accel::Accelerator;
use crosspane_input::layout::{Layout, Placed, PointerTracker, Portal, Step};
use crosspane_input::lease::ControllerLease;
use crosspane_input::router::Router;
use crosspane_input::{Edge, Held};
use crosspane_platform::EndReason as CaptureEnd;
use crosspane_platform::{
    CaptureEvent, CaptureId, CapturePortal, CaptureStart, HotkeyEvent, LockState, MotionKind,
    Overlay, OverlayAnchor, OverlayEvent, PortalId, Rgb8, SessionEvent, SessionState,
};
use crosspane_protocol::link::LinkEvent;
use crosspane_protocol::msg::{
    ControlMessage, EndReason, InputMessage, MAX_HELD_KEYS, Placement, PointerMessage, TargetStatus,
};
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{DisplayGeometry, PixelRect, PointDevice};
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::id::{DisplayId, GlobalDisplayId, NodeId, ProjectionId, SessionId, WindowId};
use crosspane_types::input::LockKeys;
use crosspane_types::time::MonoTime;

use crate::config::EngineConfig;
use crate::e2::{Placement as Proxy, TwinHome};
use crate::io::{
    Command, Failure, HUD, HomeFailure, HomeOp, Input, Notice, Output, PortalsFailure,
    ProjectionKey, ReleaseCause, Warp,
};

const REENTRY_GUARD: Duration = Duration::from_millis(150);
/// How long after a target session ends the restored portals stay disarmed if the platform never
/// reports an `EdgeReleased` for them (a pointer that was never inside the strip produces none).
pub const REARM_FALLBACK: Duration = Duration::from_secs(1);
const HUD_TIMEOUT: Duration = Duration::from_millis(500);
const START_TIMEOUT: Duration = Duration::from_secs(1);
const END_TIMEOUT: Duration = Duration::from_millis(300);
const NOT_PERMITTED: SessionState = SessionState {
    lock: LockState::Unknown,
    active: None,
};

// ---- WP-2.43 "home on the twin" (docs/wp/WP-2.43.md §4, amendments A1-A9, B1) ----
/// How long every injector this node owns has to confirm its releases before the entry gives up.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(500);
/// How long the home bind has to be installed and verified.
const BIND_TIMEOUT: Duration = Duration::from_secs(1);
/// How long the window has to take focus after the capture is released.
const FOCUS_TIMEOUT: Duration = Duration::from_millis(500);
/// A failed entry or exit is not retried for this long (a passive fence).
const HOME_RETRY: Duration = Duration::from_secs(1);
/// A peer's motion report only describes the pointer if this node's physical pointer moved this
/// recently (§2.2.6).
const LOCAL_MOTION_AGE: Duration = Duration::from_millis(500);
/// A8: how recent the last physical motion must still be when the capture is about to be
/// released. The same bound as at the trigger: a pointer that has been still this long is not
/// being steered into the window.
const ENTRY_FRESH: Duration = LOCAL_MOTION_AGE;
/// The tracker and the peer's report may differ by this many device pixels per axis (§2.3).
const ENTRY_SLACK: f64 = 96.0;
/// WP-2.43j: the entry warp lands at least this many device pixels inside the content. A strip is
/// one logical pixel on the twin output's edge and the content fills the output there, so a point
/// on the content's outermost pixels is on a strip: the warp itself would press it and bounce the
/// pointer straight back out (live, 2026-10-02). The width is the Hyprland backend's: a 1x`length`
/// layer surface per strip (`crosspane-platform-linux` `hyprland/capture/wayland.rs`, the
/// `layer.set_size(width, height)` of each new strip), that is one logical pixel, so up to the
/// output scale in device pixels; 8 covers scales up to 8.
const ENTRY_CLEARANCE: i32 = 8;
/// WP-2.43j: a strip whose content edge is at most this many device pixels from the entry point is
/// the entry's own: the entering motion can carry the pointer back onto it.
const ENTRY_NEAR: i32 = 48;
/// WP-2.43j: presses on the entry's own strips this soon after the entry are the entering motion,
/// not an exit. A strip pressed in that time stays ignored until the pointer leaves it
/// (`EdgeReleased`): the spatial re-arm of WP-1.39, which the controller can't measure on the
/// twin directly (the pointer is local while home).
const ENTRY_GUARD: Duration = Duration::from_millis(300);
/// WP-2.43j: a strip held since the guard counts again after this long from the entry even
/// without a release: a push that long is meant.
const ENTRY_HOLD: Duration = Duration::from_secs(1);
/// WP-2.43j: how long the HUD stays quarantined after a show was abandoned or reported
/// `Unavailable`: outcomes of that show arriving meanwhile are dropped (the overlay hosts answer
/// far sooner), and no new show starts.
const HUD_STALE: Duration = Duration::from_secs(1);
/// A set of portals that was not installed is offered again this often (§2.8).
const PORTALS_RETRY: Duration = Duration::from_millis(500);
/// A pointer left on the twin is warped home again this often, while it can be (§2.7).
const STRANDED_RETRY: Duration = Duration::from_secs(1);
const STRANDED_ATTEMPTS: u32 = 10;
/// Portal ids of twin strips: `TWIN_PORTAL_BASE + 4 * slot + edge index` (§2.5).
const TWIN_PORTAL_BASE: u32 = 1 << 30;
/// A slot this large or larger offers no strips.
const MAX_TWIN_SLOT: u32 = 1 << 28;
/// B1: after an uncertain portal result the capture is ended explicitly and treated as gone
/// only once it reports its end or this long has passed.
const CAPTURE_END_DEADLINE: Duration = Duration::from_secs(1);
/// A1: the bind removal is retried with this backoff (doubling up to the maximum).
const TEARDOWN_BACKOFF_MIN: Duration = Duration::from_millis(100);
const TEARDOWN_BACKOFF_MAX: Duration = Duration::from_secs(2);
/// The activation of an exit buffers at most this many key and button transitions.
const ACTIVATION_LOG_MAX: usize = 1024;
/// Outstanding warp operations remembered (the agent answers each one).
const WARPS_MAX: usize = 32;
/// Unanswered portal-set requests remembered (the agent answers every one, in order, at once).
const PORTAL_REQUESTS_MAX: usize = 64;
const EDGES: [Edge; 4] = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];

#[derive(Clone, Copy, Debug)]
enum Entering {
    /// Waiting for every injector this node owns to confirm its releases.
    Draining { deadline: MonoTime },
    /// Waiting for the home bind to be installed and verified.
    Binding { deadline: MonoTime },
    /// The capture is being released and the pointer warped onto the twin.
    Releasing { deadline: MonoTime },
    /// The pointer is on the twin; waiting for the window to take focus.
    Focusing { deadline: MonoTime },
}

#[derive(Clone, Copy, Debug)]
enum Exiting {
    /// The HUD is being shown before any capture begins (04 §8 invariant 5).
    Hud {
        portal: PortalId,
        position: f64,
        strips_gen: u64,
        deadline: MonoTime,
    },
    /// The exit capture is being activated.
    Activating {
        id: CaptureId,
        portal: PortalId,
        position: f64,
        strips_gen: u64,
        deadline: MonoTime,
    },
    /// The exit was abandoned; no new capture begins until this one is over.
    Cancelled { id: CaptureId, deadline: MonoTime },
}

#[derive(Clone, Copy, Debug)]
enum HomeState {
    Entering(Entering),
    Home,
    Exiting(Exiting),
}

/// §2.1 "home": this node's physical input drives one of its own projected windows on the twin
/// output while the E1 session stays open.
#[derive(Clone, Copy, Debug)]
struct Home {
    /// Correlates this attempt's `HomeBind` and entry `ReleaseAndWarp` with their answers.
    op: HomeOp,
    /// The node this controller is driving, and its projection of the window.
    peer: NodeId,
    projection: ProjectionId,
    window: WindowId,
    /// The capture that is ended to go home: once the release is requested its `Ended` is
    /// expected, not a loss.
    ended: CaptureId,
    /// Its `Ended` arrived.
    ended_seen: bool,
    /// Where the pointer goes when home ends without a crossing (§2.1).
    fallback: (DisplayId, PointDevice),
    /// `HomeBind { install: true }` was requested: the matching removal is owed.
    bind: bool,
    /// A8: what the trigger saw, re-checked immediately before the release: the peer's display
    /// the proxy was on, the placement's generation and the strip generation.
    display: DisplayId,
    generation: u32,
    strips_gen: u64,
    state: HomeState,
    /// WP-2.43j: the content edges (by `edge_index`) within `ENTRY_NEAR` of the entry point...
    entry_near: [bool; 4],
    /// ...those of them pressed during the guard, ignored until released (or `ENTRY_HOLD`)...
    entry_held: [bool; 4],
    /// ...and when home was committed: the guard runs from there.
    entered_at: Option<MonoTime>,
}

/// A1: the home bind was requested and its removal is not yet confirmed. Until it is, the seat
/// stays arbitrated exactly as while home.
#[derive(Clone, Copy, Debug)]
struct Teardown {
    /// The current attempt's operation: only its answer counts.
    op: HomeOp,
    attempt: u32,
    /// When the next attempt is made if this one is not confirmed (an actionable deadline).
    next: MonoTime,
    peer: NodeId,
    projection: ProjectionId,
    /// A flush exit to a third node cannot start its session until this removal succeeds.
    continuation: Option<FlushHandoff>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ExitTarget {
    host: GlobalDisplayId,
    host_point: PointDevice,
    display: GlobalDisplayId,
    point: PointDevice,
    portal: Option<Portal>,
}

#[derive(Clone, Copy, Debug)]
struct FlushHandoff {
    capture: CaptureId,
    target: ExitTarget,
    fallback: (DisplayId, PointDevice),
}

/// §2.7: the pointer may have been left on the invisible twin.
#[derive(Clone, Copy, Debug)]
struct Stranded {
    target: (DisplayId, PointDevice),
    next: MonoTime,
    attempts: u32,
}

/// What a `ReleaseAndWarp` is for, by its `op`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WarpPurpose {
    /// The entry's release: its answer drives the entry transaction.
    Entry,
    /// The pointer is returned to the desktop (or a crossing point): a failure strands it.
    Leave,
    /// A stranded pointer's retry: a failure is already accounted for.
    Retry,
    /// An exit abandoned because a button is held: the pointer stays on the twin.
    Cancel,
}

#[derive(Clone, Copy, Debug)]
struct WarpEntry {
    op: HomeOp,
    purpose: WarpPurpose,
    target: (DisplayId, PointDevice),
}

/// How captured key, button, scroll and motion events are treated right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputMode {
    /// Not at all: no capture is live for the controller.
    Off,
    /// Routed to the peer (today's behaviour).
    Routing,
    /// The capture is being released for home: the chord and the held buttons are tracked, nothing
    /// is routed (§2.4).
    ChordOnly,
    /// The exit capture is being activated: transitions are buffered (A4).
    Buffer,
}

#[derive(Debug)]
struct Session {
    peer: NodeId,
    id: SessionId,
    input_seq: u32,
    motion_seq: u32,
    lease: ControllerLease,
}

impl Session {
    fn transition(&mut self, item: Held, down: bool, now: MonoTime, out: &mut Vec<Output>) {
        let (session, seq) = self.next_input(now);
        let msg = match item {
            Held::Key(usage) => InputMessage::Key {
                session,
                seq,
                usage,
                down,
            },
            Held::Button(button) => InputMessage::Button {
                session,
                seq,
                button,
                down,
            },
        };
        out.push(Output::SendInput {
            peer: self.peer,
            msg,
        });
    }

    fn next_input(&mut self, now: MonoTime) -> (SessionId, u32) {
        let seq = self.input_seq;
        self.input_seq = seq.saturating_add(1);
        self.lease.sent(seq, now);
        (self.id, seq)
    }

    // Reserve room for every possible held item and its final release before accepting more
    // input. There are at most MAX_HELD_KEYS keys and 256 distinct MouseButton values.
    fn has_sequence_room(&self) -> bool {
        self.input_seq < u32::MAX - 290 && self.motion_seq < u32::MAX
    }
}

#[derive(Clone, Copy, Debug)]
struct Capture {
    id: CaptureId,
    started: bool,
}

#[derive(Clone, Copy, Debug)]
enum Wait {
    Hud(MonoTime),
    Handshake(MonoTime),
    Capture(MonoTime),
}

#[derive(Debug)]
struct Crossing {
    portal: PortalId,
    hud_display: DisplayId,
    entry: (GlobalDisplayId, PointDevice),
    session: Option<Session>,
    // Some during activation or a third-node handshake that keeps the capture.
    capture: Option<Capture>,
    wait: Wait,
    // The retained capture began on a twin strip (WP-2.43 §2.7).
    from_twin: bool,
}

#[derive(Debug)]
struct Control {
    session: Session,
    capture: Capture,
    tracker: PointerTracker,
    hud_display: DisplayId,
    // WP-2.43: this node's input is home in one of its own projected windows.
    home: Option<Home>,
    // The live capture began on a twin strip: the physical pointer is on the invisible twin, so
    // every end of it warps (§2.7).
    from_twin: bool,
    // The `at` of the last physical motion forwarded to the peer (§2.2.6).
    last_motion: Option<MonoTime>,
}

#[derive(Debug)]
enum Phase {
    Idle,
    Crossing(Crossing),
    Controlling(Control),
    Returning {
        capture: CaptureId,
        deadline: MonoTime,
        // When the fence times out: warp here (the capture began on a twin strip, or home was
        // left) rather than a plain `EndCapture`.
        warp: Option<(DisplayId, PointDevice)>,
    },
}

#[derive(Clone, Copy, Debug)]
struct Push {
    portal: PortalId,
    position: f64,
    since: MonoTime,
}

/// A portal restored under the pointer after this node stopped being an E1 target. The injected
/// pointer still sits at the entry edge, so presses against it are not a deliberate crossing.
/// Portal IDs are regenerated on layout changes, so the physical connection is the identity.
#[derive(Clone, Copy, Debug)]
struct Disarmed {
    from: GlobalDisplayId,
    to: GlobalDisplayId,
    edge: Edge,
    // The latest of the restore and the last change of the portal-ID mapping (layout or offered
    // set) since. An `EdgeReleased` stamped earlier doesn't re-arm: before the restore it is stale
    // (for example the one the platform emits when the session start removed the strips), and
    // before a mapping change its ID may now name another portal.
    valid_from: MonoTime,
    // The fallback re-arm: presses are ignored while `now < until`.
    until: MonoTime,
}

#[derive(Debug)]
struct HotkeyHold {
    since: MonoTime,
    fired: bool,
    rearm: bool,
}

/// The controller side of E1: crossing, capture, routing, heartbeats, release and panic.
#[derive(Debug)]
pub struct ControllerE1 {
    config: EngineConfig,
    state: SessionState,
    asleep: bool,
    displays: BTreeMap<NodeId, Vec<DisplayInfo>>,
    placements: Vec<Placement>,
    peers: BTreeSet<NodeId>,
    rtts: BTreeMap<NodeId, Duration>,
    lock_keys: LockKeys,
    armed: bool,
    // The authoritative hotkey pair for a chord already handled in captured Key events.
    chord_press_outstanding: bool,
    layout: Option<Layout>,
    // The set this controller offers the capture backend: the layout's portals, then the twin
    // strips (WP-2.43 §2.5).
    portals: Vec<CapturePortal>,
    layout_portals: Vec<CapturePortal>,
    accelerator: Accelerator,
    router: Router,
    // The router tracks physical keys; wire usages are fixed at each routed press.
    key_mappings: BTreeMap<HidUsage, HidUsage>,
    chord_keys: BTreeSet<HidUsage>,
    // Includes downs after Started but before activation completes, which aren't routed.
    capture_buttons: BTreeSet<MouseButton>,
    phase: Phase,
    push: Option<Push>,
    // Portal IDs are regenerated on layout changes; guard the physical connection instead.
    reentry: Option<(GlobalDisplayId, GlobalDisplayId, Edge, MonoTime)>,
    // Local handover guards every outgoing crossing, across offered/confirmed mapping changes.
    local_override_until: Option<MonoTime>,
    // Restored portals that ignore `EdgePressed` until their first `EdgeReleased` (not the same
    // as `armed`, which is the user's crossing switch).
    disarmed: Vec<Disarmed>,
    cancelled: BTreeMap<NodeId, SessionId>,
    hotkey: Option<HotkeyHold>,
    next_session: Option<u64>,
    next_capture: Option<u64>,
    // ---- WP-2.43 ----
    // E2's twin-parked windows, as of the last `set_twin_homes`.
    twin_homes: Vec<TwinHome>,
    // Stable per-projection strip slots: removing one projection never renames another's strips.
    twin_slots: BTreeMap<ProjectionId, u32>,
    next_slot: u32,
    twin_strips: Vec<CapturePortal>,
    // Only mappings a flush home exit can use: their meaning may change behind identical
    // full-content strips and identical local capture portals.
    home_exit_mapping: Vec<FlushMapping>,
    // Grows whenever the offered twin strips change; every exit request records it.
    strips_gen: u64,
    // What the last answers said about the current set (`portal_requests` holds the ones still
    // unanswered).
    portals_installed: bool,
    portals_failed: bool,
    portals_retry: Option<MonoTime>,
    // A failed entry is not retried for this projection until the time (passive).
    home_fence: Option<(ProjectionId, MonoTime)>,
    // Presses on a strip are ignored until the time (passive).
    exit_retry: BTreeMap<PortalId, MonoTime>,
    // WP-2.43j: when the newest home warp was answered. An edge press stamped before it was made
    // where the pointer was before the warp moved it (the agent delivers the answer first, then
    // the events queued meanwhile): it is stale and starts nothing. The stamp is the time the
    // engine processes the answer, a little after the warp itself, so a genuine press made in
    // between is dropped too; that window is a few milliseconds, and Wayland pointer timestamps
    // are whole milliseconds anyway. The platform repeats `EdgePressed` while the push goes on,
    // so a dropped press costs one repeat, never the exit.
    warp_fence: Option<MonoTime>,
    // WP-2.43j: when the newest home warp was issued. Until its answer (or `END_TIMEOUT`), a
    // crossing push's dwell never completes: the pointer may already be elsewhere.
    warp_issued: Option<MonoTime>,
    // WP-2.43j: when the HUD was last shown, while its outcome (`Visible` or `Unavailable`) is
    // still owed.
    hud_shown: Option<MonoTime>,
    // WP-2.43j: the HUD is quarantined until this time. Its events carry no show attempt (the
    // frozen `OverlayEvent`, and the frozen `HUD` id), so after a show was abandoned (hidden
    // before its outcome) or reported `Unavailable`, further outcomes of that show may still come
    // (the Mac host can send `Unavailable` and later `Visible` for one show). Until the time,
    // every HUD outcome is dropped and no HUD is shown for a crossing or an exit: a stale
    // `Visible` must never authorize, nor a stale `Unavailable` cancel, a newer attempt. A HUD
    // shown while controlling (`switch_target`) isn't blocked; it can't happen during a
    // quarantine, which only starts from home or from no session, and no exit or crossing starts
    // during it.
    hud_stale: Option<MonoTime>,
    next_op: u64,
    warps: Vec<WarpEntry>,
    teardown: Option<Teardown>,
    stranded: Option<Stranded>,
    // Key and button transitions of the exit capture, buffered until its activation completes.
    activation: Vec<(Held, bool)>,
    activation_overflow: bool,
    // Why the entry or home being ended is ending, for its notice.
    failure: Option<HomeFailure>,
    // The exit capture that committed (it is the session's live capture): a repeat of its
    // `CaptureBegun { Ok }` must not end it.
    committed_exit: Option<CaptureId>,
    // The engine's side of the I/O gate is closed by a panic: a stranded pointer's retries wait
    // for the re-arm instead of spending their budget on skipped warps.
    gate_closed: bool,
    // Every `SetPortals` the engine actually emitted and the backend has not answered yet, oldest
    // first, each with the mapping it was emitted under. Answers are strictly in order, one per
    // emitted set: each consumes the oldest (B1). A set the engine suppressed (while this node is
    // controlled) was never emitted and has no entry.
    portal_requests: VecDeque<PortalRequest>,
    // Answers still to come for requests dropped on overflow (their snapshots are gone, so what
    // they installed can't be told). They are the oldest answers: each one consumes this counter
    // instead of the head of `portal_requests`, so the answers that follow stay aligned with the
    // snapshots that were kept, and none of the skipped ones can confirm a mapping.
    portal_skip: usize,
    // The mapping of the newest set the backend confirmed. Edge events are interpreted through
    // it: a replacement the backend rejected must not reassign the ids of the strips that are
    // still installed. `None` until the backend has answered anything (the current layout is then
    // the only one there is). An empty mapping (nothing can be pressed) after an answer that
    // leaves the installed set unknown, and after a request was dropped on overflow, until an
    // answer that is aligned again confirms one.
    confirmed_portals: Option<PortalMap>,
    // What each offered layout portal id names, as of the newest set offered: the connection and
    // everything `Layout::entry` maps a crossing through (both displays' geometry and origin). A
    // change here re-offers the set even when its strips are identical.
    portal_mapping: Vec<PortalMapping>,
}

/// One offered layout portal and the two placed displays its entry coordinate is computed from.
type PortalMapping = (Portal, Placed, Placed);

#[derive(Clone, Debug, PartialEq)]
struct FlushMapping {
    projection: ProjectionId,
    edge: Edge,
    host: Placed,
    start: PointDevice,
    end: PointDevice,
    portals: Vec<PortalMapping>,
    neighbors: Vec<Placed>,
}

/// What a portal id means in the layout it was offered under.
#[derive(Clone, Debug)]
struct PortalMap {
    /// The strips of the set this mapping belongs to.
    offered: Vec<CapturePortal>,
    /// `None`: nothing is installed (no strip can be pressed).
    layout: Option<Layout>,
}

impl PortalMap {
    /// Nothing is installed.
    fn empty() -> PortalMap {
        PortalMap {
            offered: Vec::new(),
            layout: None,
        }
    }
}

#[derive(Clone, Debug)]
struct PortalRequest {
    ids: Vec<PortalId>,
    map: PortalMap,
}

impl ControllerE1 {
    pub fn new(config: &EngineConfig, now: MonoTime) -> ControllerE1 {
        let _ = now;
        ControllerE1 {
            config: config.clone(),
            state: NOT_PERMITTED,
            asleep: false,
            displays: BTreeMap::new(),
            placements: Vec::new(),
            peers: BTreeSet::new(),
            rtts: BTreeMap::new(),
            lock_keys: LockKeys::default(),
            armed: true,
            chord_press_outstanding: false,
            layout: None,
            portals: Vec::new(),
            layout_portals: Vec::new(),
            accelerator: Accelerator::new(config.accel),
            router: Router::new(),
            key_mappings: BTreeMap::new(),
            chord_keys: BTreeSet::new(),
            capture_buttons: BTreeSet::new(),
            phase: Phase::Idle,
            push: None,
            reentry: None,
            local_override_until: None,
            disarmed: Vec::new(),
            cancelled: BTreeMap::new(),
            hotkey: None,
            next_session: Some(1),
            next_capture: Some(1),
            twin_homes: Vec::new(),
            twin_slots: BTreeMap::new(),
            next_slot: 0,
            twin_strips: Vec::new(),
            home_exit_mapping: Vec::new(),
            strips_gen: 0,
            portals_installed: false,
            portals_failed: false,
            portals_retry: None,
            home_fence: None,
            exit_retry: BTreeMap::new(),
            warp_fence: None,
            warp_issued: None,
            hud_shown: None,
            hud_stale: None,
            next_op: 1,
            warps: Vec::new(),
            teardown: None,
            stranded: None,
            activation: Vec::new(),
            activation_overflow: false,
            failure: None,
            committed_exit: None,
            gate_closed: false,
            portal_requests: VecDeque::new(),
            portal_skip: 0,
            confirmed_portals: None,
            portal_mapping: Vec::new(),
        }
    }

    /// Handle one input (every input is offered to both roles), appending outputs.
    pub fn handle(&mut self, input: &Input, now: MonoTime, out: &mut Vec<Output>) {
        self.prune(now);
        match input {
            Input::LocalDisplays(displays) => {
                self.displays.insert(self.config.node, displays.clone());
                self.rebuild_layout(now, out);
            }
            Input::PeerDisplays { peer, displays } if *peer != self.config.node => {
                self.displays.insert(*peer, displays.clone());
                self.rebuild_layout(now, out);
            }
            Input::Layout(placements) => {
                self.placements = placements.clone();
                self.rebuild_layout(now, out);
            }
            Input::PeerUp { peer } => {
                self.peers.insert(*peer);
                self.update_portals(now, out);
            }
            Input::PeerRtt { peer, rtt } => {
                self.rtts.insert(*peer, *rtt);
            }
            Input::Session(event) => {
                match event {
                    SessionEvent::State(state) => self.state = *state,
                    SessionEvent::WillSleep => self.asleep = true,
                    SessionEvent::Woke => {
                        self.asleep = false;
                        self.state = NOT_PERMITTED;
                    }
                    _ => {}
                }
                if !self.permits_io() {
                    self.return_home(EndReason::ControllerLocked, None, false, true, now, out);
                }
            }
            Input::Capture(event) => self.capture_event(event, now, out),
            Input::CaptureBegun { id, result } => self.capture_begun(*id, result, now, out),
            Input::Overlay(OverlayEvent::Visible(id)) if *id == HUD => {
                if !self.hud_outcome(false, now) {
                    // The outcome of a show that was abandoned (WP-2.43j).
                } else if self.exiting_hud() {
                    self.exit_hud_visible(now, out);
                } else if let Phase::Crossing(c) = &self.phase
                    && let Wait::Hud(deadline) = c.wait
                {
                    if now >= deadline {
                        self.return_home(EndReason::Released, None, false, true, now, out);
                    } else {
                        self.start_handshake(now, out);
                    }
                }
            }
            Input::Overlay(OverlayEvent::Unavailable(id)) if *id == HUD => {
                if self.hud_outcome(true, now) {
                    self.hud_unavailable(now, out);
                }
            }
            Input::Link(event) => self.link_event(event, now, out),
            Input::Hotkey(event) => self.hotkey_event(*event, now, out),
            Input::Command(Command::ReleaseControl) if !matches!(self.phase, Phase::Idle) => {
                self.release(ReleaseCause::Command, now, out);
            }
            Input::Command(Command::Panic) => self.panic(now, out),
            Input::Command(Command::Rearm) => self.arm(now, out),
            Input::PortalsSet { ids, result } => self.portals_set(ids, result, now, out),
            Input::LocalPointer { display, position } => {
                self.local_pointer(*display, *position, now, out);
            }
            Input::CaptureReleased { op, result } => {
                self.capture_released(*op, result, now, out);
            }
            Input::HomeBindSet {
                op,
                install,
                result,
            } => self.home_bind_set(*op, *install, result, now, out),
            Input::Tick => self.tick(now, out),
            _ => {}
        }
        // A pointer left on the twin is warped home as soon as it can be, on any input (A6).
        self.stranded_retry(now, out);
    }

    fn capture_begun(
        &mut self,
        id: CaptureId,
        result: &Result<CaptureStart, Failure>,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        if self.exit_begun(id, result, now, out) {
            return;
        }
        let matches = matches!(&self.phase, Phase::Crossing(c)
            if matches!(c.wait, Wait::Capture(_)) && c.capture.is_some_and(|v| v.id == id));
        if matches {
            match result {
                Ok(start) => {
                    if matches!(&self.phase, Phase::Crossing(c)
                        if matches!(c.wait, Wait::Capture(deadline) if now >= deadline))
                    {
                        self.return_home(EndReason::Released, None, false, true, now, out);
                    } else {
                        self.chord_keys.extend(start.held_keys.iter().copied());
                        self.lock_keys = start.lock_keys;
                        self.activate(now, out);
                    }
                }
                Err(_) => self.return_home(EndReason::Released, None, true, true, now, out),
            }
        } else if result.is_ok() {
            // A repeat of the success of the exit capture that committed is not a new capture:
            // it is the session's live one, and ending it would end the session (a stale
            // success for any other id still ends what it began).
            if self.committed_exit == Some(id) && self.retains_capture(id) {
                return;
            }
            // Capture ids increase and the backend holds one capture at a time, so a success for
            // an id older than the capture this controller knows was superseded: the newer
            // capture's `begin` proves it is gone (WP-2.43 §2.6 "older-id rule").
            if self.known_capture().is_some_and(|known| id < known) {
                return;
            }
            // A delayed or duplicate success must never leave an unseen capture alive.
            out.push(Output::EndCapture { warp_to: None });
        } else if result.is_err()
            && matches!(self.phase, Phase::Returning { capture, .. } if capture == id)
        {
            // A rolled-back activation cannot emit an Ended fence.
            self.finish_return(now, out);
        }
    }

    pub fn next_deadline(&self) -> Option<MonoTime> {
        let phase = match &self.phase {
            // A dwell that ends while a warp is unanswered waits for the warp's bound (WP-2.43j).
            Phase::Idle => self.push.map(|p| {
                let dwell = p.since.saturating_add(self.config.push_to_cross);
                match self.warp_pending_until() {
                    Some(until) => dwell.max(until),
                    None => dwell,
                }
            }),
            Phase::Crossing(c) => match c.wait {
                Wait::Hud(at) | Wait::Handshake(at) | Wait::Capture(at) => Some(at),
            },
            Phase::Controlling(c) => {
                let heartbeat = c
                    .session
                    .lease
                    .next_heartbeat(!self.router.held_on(c.session.peer).is_empty());
                let ack = c
                    .session
                    .lease
                    .ack_deadline(self.rtts.get(&c.session.peer).copied());
                Some(ack.map_or(heartbeat, |at| heartbeat.min(at)))
            }
            Phase::Returning { deadline, .. } => Some(*deadline),
        };
        let panic = self
            .hotkey
            .as_ref()
            .filter(|h| !h.fired)
            .map(|h| h.since.saturating_add(self.config.panic_hold));
        [phase, panic, self.home_deadline()]
            .into_iter()
            .flatten()
            .min()
    }

    fn permits_io(&self) -> bool {
        self.state.permits_io() && !self.asleep
    }

    fn session(&self) -> Option<&Session> {
        match &self.phase {
            Phase::Crossing(c) => c.session.as_ref(),
            Phase::Controlling(c) => Some(&c.session),
            _ => None,
        }
    }

    fn capture_mut(&mut self) -> Option<&mut Capture> {
        match &mut self.phase {
            Phase::Crossing(c) => c.capture.as_mut(),
            Phase::Controlling(c) => Some(&mut c.capture),
            _ => None,
        }
    }

    fn rebuild_layout(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        let placed = self
            .placements
            .iter()
            .filter_map(|p| {
                let display = self
                    .displays
                    .get(&p.node)?
                    .iter()
                    .find(|d| d.id == p.display)?;
                Some(Placed {
                    id: GlobalDisplayId {
                        node: p.node,
                        display: p.display,
                    },
                    geometry: display.geometry,
                    origin: p.origin,
                })
            })
            .collect();
        let layout = Layout::new(placed, self.config.layout).ok();
        // Portal IDs are regenerated by a rebuild; only a rebuild that moves a portal matters.
        let remapped =
            self.layout.as_ref().map(Layout::portals) != layout.as_ref().map(Layout::portals);
        self.layout = layout;
        self.update_portals(now, out);
        if remapped {
            self.portal_mapping_changed(now);
        }
        // Hot-plug must not leave a tracker or a pending capture on a vanished display.
        let valid = match (&self.phase, &self.layout) {
            (Phase::Crossing(c), Some(layout)) => {
                layout.get(c.entry.0).is_some()
                    && layout
                        .get(GlobalDisplayId {
                            node: self.config.node,
                            display: c.hud_display,
                        })
                        .is_some()
                    && ((c.capture.is_some() && matches!(c.wait, Wait::Handshake(_)))
                        || layout.portals().iter().any(|p| {
                            p.id == c.portal
                                && p.from.node == self.config.node
                                && p.from.display == c.hud_display
                                && p.to == c.entry.0
                        }))
            }
            (Phase::Controlling(c), Some(layout)) => {
                layout.get(c.tracker.position().0).is_some()
                    && layout
                        .get(GlobalDisplayId {
                            node: self.config.node,
                            display: c.hud_display,
                        })
                        .is_some()
            }
            (Phase::Crossing(_) | Phase::Controlling(_), None) => false,
            _ => true,
        };
        if !valid {
            self.return_home(EndReason::Released, None, false, true, now, out);
        }
        if self
            .push
            .is_some_and(|p| self.portal_entry(p.portal, p.position).is_none())
        {
            self.push = None;
        }
    }

    /// The capture portals this controller currently wants (empty while disarmed).
    pub fn portals(&self) -> &[CapturePortal] {
        &self.portals
    }

    /// The node this controller drives (or is crossing to), if any.
    pub fn target(&self) -> Option<NodeId> {
        self.session().map(|s| s.peer)
    }

    /// The node whose session this controller has established (WP-4.5): the peer acknowledged it
    /// and the capture is live (`Phase::Controlling`). Unlike [`ControllerE1::target`], not the
    /// peer of a handshake still waiting for its answer.
    pub fn established(&self) -> Option<NodeId> {
        match &self.phase {
            Phase::Controlling(c) => Some(c.session.peer),
            _ => None,
        }
    }

    /// An acknowledged outgoing session owns the controller role, including capture activation
    /// and third-node handoffs that retain an existing capture. Also true while the home bind's
    /// removal is unconfirmed (WP-2.43 A1): no session is admitted or started until the seat is
    /// safe again.
    pub(crate) fn started(&self) -> bool {
        matches!(self.phase, Phase::Controlling(_))
            || matches!(&self.phase, Phase::Crossing(c) if c.capture.is_some())
            || self.teardown.is_some()
    }

    /// Yield an unacknowledged crossing before incoming target admission.
    pub(crate) fn cancel_pending(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        self.push = None;
        if matches!(self.phase, Phase::Crossing(_)) && !self.started() {
            if let Some(session) = self.session() {
                // Session IDs increase monotonically: one watermark per peer covers repeated
                // cancellations without retaining an unbounded list of handshake tombstones.
                self.cancelled.insert(session.peer, session.id);
            }
            self.return_home(EndReason::Released, None, false, true, now, out);
        }
    }

    /// False after a panic or release until re-armed: edges don't cross.
    pub fn armed(&self) -> bool {
        self.armed
    }

    fn update_portals(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        let layout_portals = if self.armed {
            self.layout.as_ref().map_or_else(Vec::new, |layout| {
                layout
                    .capture_portals(self.config.node)
                    .into_iter()
                    .filter(|p| {
                        layout
                            .portals()
                            .iter()
                            .any(|v| v.id == p.id && self.peers.contains(&v.to.node))
                    })
                    .collect()
            })
        } else {
            Vec::new()
        };
        let changed = layout_portals != self.layout_portals;
        self.layout_portals = layout_portals;
        // The twin strips follow the same rebuild (WP-2.43 §2.5): the offered set is the layout's
        // portals, then the strips.
        let strips = self.twin_strip_set();
        if strips != self.twin_strips {
            self.twin_strips = strips;
            self.strips_gen = self.strips_gen.saturating_add(1);
        }
        let mut offered = self.layout_portals.clone();
        offered.extend_from_slice(&self.twin_strips);
        // What the offered layout portals connect to now, and everything a crossing through them
        // is mapped by (`Layout::entry`: the portal's span and both displays' geometry, pixel
        // size, scale and origin). A change re-offers the set even when the strips are
        // identical (the backend treats an identical set as a no-op): the new meaning gets its
        // own request, answered in order like any other, so no pending answer can install the
        // old one afterwards.
        let mapping: Vec<PortalMapping> = self.layout.as_ref().map_or_else(Vec::new, |layout| {
            self.layout_portals
                .iter()
                .filter_map(|c| layout.portals().iter().find(|p| p.id == c.id))
                .filter_map(|p| Some((*p, *layout.get(p.from)?, *layout.get(p.to)?)))
                .collect()
        });
        let home_exit_mapping = self.flush_mapping();
        if offered != self.portals
            || mapping != self.portal_mapping
            || home_exit_mapping != self.home_exit_mapping
        {
            self.portals = offered;
            self.portal_mapping = mapping;
            self.home_exit_mapping = home_exit_mapping;
            out.push(Output::SetPortals(self.portals.clone()));
        }
        // A disarmed portal stays disarmed only while it is still offered.
        if !self.disarmed.is_empty() {
            let layout = self.layout.as_ref();
            let portals = &self.portals;
            self.disarmed.retain(|d| {
                layout.is_some_and(|layout| {
                    layout.portals().iter().any(|p| {
                        p.from == d.from
                            && p.to == d.to
                            && p.edge == d.edge
                            && portals.iter().any(|c| c.id == p.id)
                    })
                })
            });
        }
        if changed {
            self.portal_mapping_changed(now);
        }
    }

    /// This node stopped being an E1 target and its portals are being restored (the caller sends
    /// them): disarm every one of them. The pointer is still where the controller injected it,
    /// usually at the entry edge, so a press against a restored portal is not a deliberate
    /// crossing. A portal re-arms on its first `EdgeReleased` stamped at or after `now` and after
    /// any later change of the portal-ID mapping (an earlier one is stale or ambiguous), or
    /// [`REARM_FALLBACK`] after `now`.
    pub(crate) fn portals_restored(&mut self, now: MonoTime) {
        let until = now.saturating_add(REARM_FALLBACK);
        self.disarmed = match &self.layout {
            Some(layout) => self
                .portals
                .iter()
                .filter_map(|c| layout.portals().iter().find(|p| p.id == c.id))
                .map(|p| Disarmed {
                    from: p.from,
                    to: p.to,
                    edge: p.edge,
                    valid_from: now,
                    until,
                })
                .collect(),
            None => Vec::new(),
        };
    }

    // The layout a portal id from the backend is interpreted in: the one the newest set the
    // backend confirmed was offered under (B1), not the newest request. Until the backend has
    // answered anything the newest layout is the only one there is.
    fn portal_layout(&self) -> Option<&Layout> {
        match &self.confirmed_portals {
            Some(map) => map.layout.as_ref(),
            None => self.layout.as_ref(),
        }
    }

    // The physical connection a portal ID names.
    fn connection(&self, portal: PortalId) -> Option<(GlobalDisplayId, GlobalDisplayId, Edge)> {
        let layout = self.portal_layout()?;
        let p = layout.portals().iter().find(|p| p.id == portal)?;
        Some((p.from, p.to, p.edge))
    }

    fn is_disarmed(&mut self, portal: PortalId, now: MonoTime) -> bool {
        self.disarmed.retain(|d| now < d.until);
        self.connection(portal).is_some_and(|(from, to, edge)| {
            self.disarmed
                .iter()
                .any(|d| d.from == from && d.to == to && d.edge == edge)
        })
    }

    // `at` is the release's own timestamp (the platform's monotonic clock, which is also the
    // engine's `now`). One stamped before the restore predates the restored strip, and one stamped
    // before a mapping change is ambiguous: its ID may have been reassigned since. Neither
    // re-arms anything; the fallback or the next release does.
    fn rearm_portal(&mut self, portal: PortalId, at: MonoTime) {
        if let Some((from, to, edge)) = self.connection(portal) {
            self.disarmed.retain(|d| {
                !(d.from == from && d.to == to && d.edge == edge && at >= d.valid_from)
            });
        }
    }

    // Portal IDs were reassigned or the offered set changed: releases already stamped can't be
    // attributed to a portal any more. Each entry keeps its fallback deadline.
    fn portal_mapping_changed(&mut self, now: MonoTime) {
        for d in &mut self.disarmed {
            if now > d.valid_from {
                d.valid_from = now;
            }
        }
    }

    fn portal_entry(
        &self,
        portal: PortalId,
        position: f64,
    ) -> Option<(DisplayId, GlobalDisplayId, PointDevice)> {
        // No new session starts while a home bind's removal is unconfirmed (WP-2.43 A1).
        if !self.armed
            || !self.permits_io()
            || !self.router.no_buttons_held()
            || self.teardown.is_some()
        {
            return None;
        }
        let layout = self.portal_layout()?;
        let p = layout.portals().iter().find(|p| {
            p.id == portal && p.from.node == self.config.node && self.peers.contains(&p.to.node)
        })?;
        let (display, entry) = layout.entry(portal, position)?;
        Some((p.from.display, display, entry))
    }

    fn begin_crossing(&mut self, push: Push, now: MonoTime, out: &mut Vec<Output>) {
        self.push = None;
        let Some((hud_display, display, entry)) = self.portal_entry(push.portal, push.position)
        else {
            return;
        };
        self.show_hud(hud_display, display.node, now, out);
        self.phase = Phase::Crossing(Crossing {
            portal: push.portal,
            hud_display,
            entry: (display, entry),
            session: None,
            capture: None,
            wait: Wait::Hud(now.saturating_add(HUD_TIMEOUT)),
            from_twin: false,
        });
    }

    fn show_hud(&mut self, display: DisplayId, peer: NodeId, now: MonoTime, out: &mut Vec<Output>) {
        self.hud_shown = Some(now);
        out.push(Output::ShowOverlay {
            id: HUD,
            overlay: Overlay {
                display,
                anchor: OverlayAnchor::TopCenter,
                text: format!("Input → {}", peer.short()),
                accent: Rgb8 {
                    r: 0x3b,
                    g: 0x82,
                    b: 0xf6,
                },
            },
        });
    }

    /// Hide the HUD. If its show's outcome hasn't arrived, the HUD is quarantined (WP-2.43j,
    /// `hud_stale`).
    fn hide_hud(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        out.push(Output::HideOverlay(HUD));
        if self.hud_shown.take().is_some() {
            self.quarantine_hud(now);
        }
    }

    /// WP-2.43j: for `HUD_STALE` from now, every HUD outcome is dropped and no HUD is shown.
    fn quarantine_hud(&mut self, now: MonoTime) {
        let until = now.saturating_add(HUD_STALE);
        self.hud_stale = Some(self.hud_stale.map_or(until, |old| old.max(until)));
    }

    /// WP-2.43j: a HUD outcome. False while the HUD is quarantined: the outcome belongs to an
    /// abandoned show, or follows an `Unavailable` of the same show, and is dropped (the
    /// quarantine stays until it expires, however many such outcomes come). An `Unavailable`
    /// that counts quarantines the HUD in turn: the Mac host can report a show `Unavailable` and
    /// later `Visible` (its presence expiring and then observed), and that late `Visible` must
    /// not stand for a newer show.
    fn hud_outcome(&mut self, unavailable: bool, now: MonoTime) -> bool {
        if self.hud_blocked(now) {
            return false;
        }
        self.hud_stale = None;
        self.hud_shown = None;
        if unavailable {
            self.quarantine_hud(now);
        }
        true
    }

    /// WP-2.43j: while a home warp is unanswered (at most `END_TIMEOUT` from its issue), when
    /// that bound ends. A crossing's dwell doesn't complete before it: the pointer may already be
    /// elsewhere, and the answer drops the push.
    fn warp_pending_until(&self) -> Option<MonoTime> {
        if self.warps.is_empty() {
            return None;
        }
        self.warp_issued.map(|at| at.saturating_add(END_TIMEOUT))
    }

    /// WP-2.43j: an abandoned show's outcome is still owed: showing the HUD now would let it
    /// stand for the new show.
    fn hud_blocked(&self, now: MonoTime) -> bool {
        self.hud_stale.is_some_and(|until| now < until)
    }

    fn start_handshake(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        let Some(id) = self.next_session else {
            self.return_home(EndReason::Released, None, false, true, now, out);
            return;
        };
        let Phase::Crossing(c) = &mut self.phase else {
            return;
        };
        self.next_session = id.checked_add(1);
        let session = Session {
            peer: c.entry.0.node,
            id: SessionId(id),
            input_seq: 1,
            motion_seq: 1,
            lease: ControllerLease::new(now),
        };
        out.push(Output::SendControl {
            peer: session.peer,
            msg: ControlMessage::StartControl {
                session: session.id,
                entry_display: c.entry.0.display,
                entry: c.entry.1,
                lock_keys: self.lock_keys,
            },
        });
        c.session = Some(session);
        c.wait = Wait::Handshake(now.saturating_add(START_TIMEOUT));
    }

    fn activate(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        let phase = std::mem::replace(&mut self.phase, Phase::Idle);
        if let Phase::Crossing(c) = phase {
            let tracker = self
                .layout
                .as_ref()
                .and_then(|layout| PointerTracker::new(layout, c.entry.0, c.entry.1));
            match (tracker, c) {
                (
                    Some(tracker),
                    Crossing {
                        session: Some(mut session),
                        capture: Some(capture),
                        hud_display,
                        from_twin,
                        ..
                    },
                ) => {
                    session.lease = ControllerLease::new(now);
                    self.accelerator = Accelerator::new(self.config.accel);
                    self.phase = Phase::Controlling(Control {
                        session,
                        capture,
                        tracker,
                        hud_display,
                        home: None,
                        from_twin,
                        last_motion: None,
                    });
                }
                (_, crossing) => {
                    self.phase = Phase::Crossing(crossing);
                    self.return_home(EndReason::Released, None, false, true, now, out);
                }
            }
        } else {
            self.phase = phase;
        }
    }

    fn capture_event(&mut self, event: &CaptureEvent, now: MonoTime, out: &mut Vec<Output>) {
        match event {
            CaptureEvent::LockKeys(keys) => self.lock_keys = *keys,
            CaptureEvent::EdgePressed {
                portal,
                position,
                at,
            } if matches!(self.phase, Phase::Idle) => {
                if self.local_override_until.is_some_and(|until| now < until)
                    || self.warp_fence.is_some_and(|fence| *at < fence)
                    || self.hud_blocked(now)
                {
                    return;
                }
                if self.is_disarmed(*portal, now) {
                    // Restored under a stationary pointer after a target session: no push, no
                    // HUD, no handshake until the pointer leaves the strip (or the fallback).
                    return;
                }
                if self.reentry.is_some_and(|(from, to, edge, until)| {
                    now < until
                        && self.portal_layout().is_some_and(|layout| {
                            layout.portals().iter().any(|p| {
                                p.id == *portal && p.from == from && p.to == to && p.edge == edge
                            })
                        })
                }) {
                    return;
                }
                if self.portal_entry(*portal, *position).is_none() {
                    self.push = None;
                    return;
                }
                let push = match self.push {
                    Some(p) if p.portal == *portal => Push {
                        position: *position,
                        ..p
                    },
                    _ => Push {
                        portal: *portal,
                        position: *position,
                        since: *at,
                    },
                };
                let warp_pending = self.warp_pending_until().is_some_and(|until| now < until);
                if at.saturating_duration_since(push.since) >= self.config.push_to_cross
                    && !warp_pending
                {
                    self.begin_crossing(push, now, out);
                } else {
                    self.push = Some(push);
                }
            }
            // WP-2.43 §2.6: a push against a twin strip while home starts the exit.
            CaptureEvent::EdgePressed {
                portal,
                position,
                at,
            } if self.home_is_resting() => {
                if self.warp_fence.is_none_or(|fence| *at >= fence) {
                    self.exit_press(*portal, *position, *at, now, out);
                }
            }
            CaptureEvent::EdgeReleased { portal, at } => {
                self.rearm_portal(*portal, *at);
                self.entry_strip_released(*portal);
                self.exit_strip_released(*portal, now, out);
                if self.push.is_some_and(|p| p.portal == *portal) {
                    self.push = None;
                }
            }
            CaptureEvent::Started { id } => {
                if let Some(capture) = self.capture_mut().filter(|c| c.id == *id) {
                    capture.started = true;
                }
            }
            CaptureEvent::Ended { id, reason } => {
                if self.home_capture_ended(*id, *reason, now, out) {
                    // Expected, or the exit's own capture: handled there.
                } else if matches!(self.phase, Phase::Returning { capture, .. } if capture == *id) {
                    self.finish_return(now, out);
                } else if self.capture_mut().is_some_and(|c| c.id == *id) {
                    self.return_home(EndReason::Released, None, true, true, now, out);
                }
            }
            CaptureEvent::Key { usage, down, .. } if self.input_mode() != InputMode::Off => {
                self.capture_key(*usage, *down, now, out);
            }
            CaptureEvent::Button { button, down, .. } if self.input_mode() != InputMode::Off => {
                self.capture_button(*button, *down, now, out);
            }
            CaptureEvent::Scroll { delta, .. } if self.input_mode() == InputMode::Routing => {
                if self.ensure_sequence_room(now, out)
                    && let Phase::Controlling(c) = &mut self.phase
                {
                    let (session, seq) = c.session.next_input(now);
                    out.push(Output::SendInput {
                        peer: c.session.peer,
                        msg: InputMessage::Scroll {
                            session,
                            seq,
                            delta: *delta,
                        },
                    });
                }
            }
            CaptureEvent::Motion { dx, dy, kind, at }
                if self.input_mode() == InputMode::Routing =>
            {
                self.motion(*dx, *dy, *kind, *at, now, out);
            }
            // KeyboardBlinded is followed by Ended by the backend; it isn't itself a fence.
            _ => {}
        }
    }

    fn ensure_sequence_room(&mut self, now: MonoTime, out: &mut Vec<Output>) -> bool {
        if matches!(&self.phase, Phase::Controlling(c) if !c.session.has_sequence_room()) {
            self.return_home(EndReason::Released, None, false, true, now, out);
            false
        } else {
            matches!(self.phase, Phase::Controlling(_))
        }
    }

    fn route(&mut self, item: Held, down: bool, now: MonoTime, out: &mut Vec<Output>) {
        if !self.ensure_sequence_room(now, out) {
            return;
        }
        let Phase::Controlling(c) = &mut self.phase else {
            return;
        };
        if let Some(peer) = self.router.route(item, down, c.session.peer)
            && peer == c.session.peer
        {
            let wire_item = match item {
                Held::Key(physical) => {
                    let mapped = if down {
                        let mapped = self
                            .config
                            .remap
                            .get(&peer)
                            .copied()
                            .unwrap_or_default()
                            .map(physical);
                        self.key_mappings.insert(physical, mapped);
                        mapped
                    } else {
                        self.key_mappings.remove(&physical).unwrap_or(physical)
                    };
                    Held::Key(mapped)
                }
                Held::Button(_) => item,
            };
            c.session.transition(wire_item, down, now, out);
            if down
                && matches!(item, Held::Key(_))
                && self
                    .router
                    .held_on(peer)
                    .iter()
                    .filter(|h| matches!(h, Held::Key(_)))
                    .count()
                    > MAX_HELD_KEYS
            {
                // Release the excess key rather than silently omitting a held key from State.
                self.router.route(item, false, peer);
                if let Held::Key(physical) = item {
                    self.key_mappings.remove(&physical);
                }
                c.session.transition(wire_item, false, now, out);
            }
        }
    }

    fn motion(
        &mut self,
        dx: f64,
        dy: f64,
        kind: MotionKind,
        at: MonoTime,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        if !self.ensure_sequence_room(now, out) {
            return;
        }
        let mm = match kind {
            MotionKind::Unaccelerated => self.accelerator.unaccelerated(dx, dy, at),
            MotionKind::Accelerated { display } => {
                let Some(info) = self
                    .displays
                    .get(&self.config.node)
                    .and_then(|ds| ds.iter().find(|d| d.id == display))
                else {
                    return;
                };
                self.accelerator.accelerated(dx, dy, &info.geometry)
            }
        };
        let (Some(layout), Phase::Controlling(c)) = (&self.layout, &mut self.phase) else {
            return;
        };
        let previous = c.tracker.position();
        match c.tracker.step(layout, mm) {
            Step::On { display, position } => {
                c.last_motion = Some(at);
                let seq = c.session.motion_seq;
                c.session.motion_seq = seq.saturating_add(1);
                out.push(Output::SendMotion {
                    peer: c.session.peer,
                    msg: PointerMessage {
                        session: c.session.id,
                        seq,
                        display: display.display,
                        position,
                    },
                });
            }
            Step::Crossed {
                display,
                position,
                portal,
            } => {
                // A handoff to a third node starts a new controller session: not while this
                // node's home bind may still exist (an entry that has requested it, or a
                // removal that is unconfirmed, WP-2.43 A1). The pointer stays where it was.
                let fenced = display.node != self.config.node
                    && (self.teardown.is_some() || c.home.is_some_and(|h| h.bind));
                if !self.router.no_buttons_held()
                    || !self.capture_buttons.is_empty()
                    || (display.node != self.config.node && !self.peers.contains(&display.node))
                    || fenced
                {
                    if let Some(tracker) = PointerTracker::new(layout, previous.0, previous.1) {
                        c.tracker = tracker;
                    }
                } else if display.node == self.config.node {
                    // Layout portals are directional: guard the local reverse of the portal
                    // through which the remote pointer returned.
                    self.reentry = layout
                        .portals()
                        .iter()
                        .find(|p| p.id == portal)
                        .and_then(|returned| {
                            layout
                                .portals()
                                .iter()
                                .find(|p| p.from == returned.to && p.to == returned.from)
                        })
                        .map(|p| (p.from, p.to, p.edge, now.saturating_add(REENTRY_GUARD)));
                    // The pointer left the peer's display: an entry still before its release is
                    // over (WP-2.43 §2.4).
                    self.note_entry_failure(HomeFailure::Gone);
                    self.return_home(
                        EndReason::Released,
                        Some((display.display, position)),
                        false,
                        true,
                        now,
                        out,
                    );
                } else {
                    self.abort_entry_if_entering(HomeFailure::Gone, now, out);
                    self.switch_target(portal, (display, position), now, out);
                }
            }
        }
    }

    fn switch_target(
        &mut self,
        portal: PortalId,
        entry: (GlobalDisplayId, PointDevice),
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        let phase = std::mem::replace(&mut self.phase, Phase::Idle);
        if let Phase::Controlling(mut c) = phase {
            self.end_session(&mut c.session, EndReason::Released, true, now, out);
            self.phase = Phase::Crossing(Crossing {
                portal,
                hud_display: c.hud_display,
                entry,
                session: None,
                capture: Some(c.capture),
                wait: Wait::Handshake(now.saturating_add(START_TIMEOUT)),
                from_twin: c.from_twin,
            });
            self.start_handshake(now, out);
            if matches!(self.phase, Phase::Crossing(_)) {
                self.show_hud(c.hud_display, entry.0.node, now, out);
            }
        } else {
            self.phase = phase;
        }
    }

    fn link_event(&mut self, event: &LinkEvent, now: MonoTime, out: &mut Vec<Output>) {
        if let LinkEvent::Closed { peer, .. } = event {
            self.peers.remove(peer);
            self.rtts.remove(peer);
            self.cancelled.remove(peer);
            self.update_portals(now, out);
            if self.session().is_some_and(|s| s.peer == *peer)
                || matches!(&self.phase, Phase::Crossing(c) if c.entry.0.node == *peer)
            {
                out.push(Output::Notice(Notice::LostConnection(*peer)));
                self.return_home(EndReason::LinkLost, None, false, false, now, out);
            }
            if self
                .push
                .is_some_and(|p| self.portal_entry(p.portal, p.position).is_none())
            {
                self.push = None;
            }
            return;
        }
        match event {
            LinkEvent::Control {
                peer,
                msg: ControlMessage::ControlStarted { session },
            } if self
                .session()
                .is_some_and(|s| s.peer == *peer && s.id == *session) =>
            {
                let Phase::Crossing(c) = &mut self.phase else {
                    return;
                };
                let Wait::Handshake(deadline) = c.wait else {
                    return;
                };
                if now >= deadline {
                    out.push(Output::Notice(Notice::LostConnection(*peer)));
                    self.return_home(EndReason::LinkLost, None, false, true, now, out);
                    return;
                }
                if c.capture.is_some() {
                    self.activate(now, out);
                } else if let Some(id) = self.next_capture {
                    self.next_capture = id.checked_add(1);
                    let capture = Capture {
                        id: CaptureId(id),
                        started: false,
                    };
                    out.push(Output::BeginCapture {
                        id: capture.id,
                        portal: c.portal,
                        drain_first: false,
                    });
                    c.capture = Some(capture);
                    c.wait = Wait::Capture(now.saturating_add(START_TIMEOUT));
                } else {
                    self.return_home(EndReason::Released, None, false, true, now, out);
                }
            }
            // A cancelled handshake can still be acknowledged after its EndControl was sent.
            // There is no local session to capture; repeat the end for the cancelled identity.
            LinkEvent::Control {
                peer,
                msg: ControlMessage::ControlStarted { session },
            } if self.cancelled.get(peer).is_some_and(|last| session <= last) => {
                out.push(Output::SendControl {
                    peer: *peer,
                    msg: ControlMessage::EndControl {
                        session: *session,
                        reason: EndReason::Released,
                    },
                })
            }
            LinkEvent::Control {
                peer,
                msg: ControlMessage::ControlRefused { session, reason },
            } if matches!(&self.phase, Phase::Crossing(c) if matches!(c.wait, Wait::Handshake(_)))
                && self
                    .session()
                    .is_some_and(|s| s.peer == *peer && s.id == *session) =>
            {
                out.push(Output::Notice(Notice::Refused {
                    peer: *peer,
                    reason: *reason,
                }));
                self.return_home(EndReason::Released, None, false, false, now, out);
            }
            LinkEvent::Control {
                peer,
                msg: ControlMessage::EndControl { session, reason },
            } if self
                .session()
                .is_some_and(|s| s.peer == *peer && s.id == *session) =>
            {
                out.push(Output::Notice(if *reason == EndReason::TargetLocked {
                    Notice::TargetLocked(*peer)
                } else {
                    Notice::ControlEnded(*peer)
                }));
                self.return_home(*reason, None, false, false, now, out);
            }
            LinkEvent::Input {
                peer,
                msg: InputMessage::Ack { session, seq },
            } => {
                if let Phase::Controlling(c) = &mut self.phase
                    && c.session.peer == *peer
                    && c.session.id == *session
                {
                    c.session.lease.acked(*seq, now);
                }
            }
            LinkEvent::Input {
                peer,
                msg: InputMessage::Status { session, status },
            } if self
                .session()
                .is_some_and(|s| s.peer == *peer && s.id == *session) =>
            {
                match status {
                    TargetStatus::LocalOverride => {
                        // Ordinary capture leaves the pointer at its frozen departure point;
                        // home/twin captures retain return_home's fallback and teardown rules.
                        // Its exact departure portal is not retained. This controller-wide
                        // 150 ms guard is the lead-approved substitute for WP-1.39's tracker
                        // hysteresis here: portal refreshes, replacement answers and queued
                        // releases cannot expose an unguarded connection before the deadline.
                        self.local_override_until = Some(now.saturating_add(REENTRY_GUARD));
                        out.push(Output::Notice(Notice::LocalOverride(*peer)));
                        self.return_home(EndReason::Released, None, false, true, now, out);
                    }
                    TargetStatus::Refused(reason) => {
                        out.push(Output::Notice(Notice::Refused {
                            peer: *peer,
                            reason: *reason,
                        }));
                        self.return_home(EndReason::Released, None, false, true, now, out);
                    }
                    TargetStatus::Resumed => {}
                }
            }
            _ => {}
        }
    }

    fn end_session(
        &mut self,
        session: &mut Session,
        reason: EndReason,
        send_end: bool,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        self.release_session_held(session, now, out);
        let connected = self.peers.contains(&session.peer);
        if connected && send_end {
            out.push(Output::SendControl {
                peer: session.peer,
                msg: ControlMessage::EndControl {
                    session: session.id,
                    reason,
                },
            });
        }
    }

    /// Send an up for everything the router holds on the session's peer (the first half of
    /// ending a session: no `EndControl`). Each key goes out under the usage it was pressed as.
    fn release_session_held(
        &mut self,
        session: &mut Session,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        let held = self.router.release_all(session.peer);
        let connected = self.peers.contains(&session.peer);
        for item in held {
            let item = match item {
                Held::Key(physical) => {
                    Held::Key(self.key_mappings.remove(&physical).unwrap_or(physical))
                }
                Held::Button(_) => item,
            };
            if connected {
                session.transition(item, false, now, out);
            }
        }
    }

    fn return_home(
        &mut self,
        reason: EndReason,
        warp_to: Option<(DisplayId, PointDevice)>,
        ended: bool,
        send_end: bool,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        self.push = None;
        let phase = std::mem::replace(&mut self.phase, Phase::Idle);
        let (capture, session, home, from_twin, hud_display) = match phase {
            Phase::Controlling(c) => (
                Some(c.capture),
                Some(c.session),
                c.home,
                c.from_twin,
                Some(c.hud_display),
            ),
            Phase::Crossing(c) => (c.capture, c.session, None, c.from_twin, Some(c.hud_display)),
            other => {
                self.phase = other;
                return;
            }
        };
        self.activation.clear();
        self.activation_overflow = false;
        self.committed_exit = None;
        let failure = self.failure.take();
        // A failed entry owns its fallback through `stranded`, including the first attempt.
        // A skipped warp can already have moved the pointer: it never proves a closed gate.
        let entry_stranded = home.is_some_and(|h| {
            matches!(h.state, HomeState::Entering(Entering::Releasing { .. }))
                && failure == Some(HomeFailure::Warp)
                && self.stranded.is_some()
        });
        // The point a capture that began on a twin strip, or an ended home, returns the pointer
        // to when no crossing point is given (§2.7).
        let fallback = home
            .map(|h| h.fallback)
            .or_else(|| hud_display.and_then(|d| self.fallback_point(d)));
        // Whether a capture may still be live, and whether this end warps (§2.7).
        let mut live = capture.is_some() && !ended;
        let mut warps = from_twin;
        if let Some(h) = home {
            warps = true;
            // A1: the bind's removal is a phase of its own, started first.
            if h.bind {
                self.start_teardown(h.peer, h.projection, now, out);
            }
            let key = ProjectionKey {
                source: self.config.node,
                projection: h.projection,
            };
            let notice = match h.state {
                // Before the release: the capture is live and the pointer is on a physical
                // display, so the ordinary end applies; the entry is an abort.
                HomeState::Entering(Entering::Draining { .. } | Entering::Binding { .. }) => {
                    warps = from_twin;
                    self.home_fence = Some((h.projection, now.saturating_add(HOME_RETRY)));
                    Notice::HomeFailed {
                        key,
                        reason: failure.unwrap_or_else(|| failure_for(reason)),
                    }
                }
                // The release is in flight: its capture may still exist unless it already ended.
                HomeState::Entering(Entering::Releasing { .. }) => {
                    live &= !h.ended_seen;
                    Notice::HomeFailed {
                        key,
                        reason: failure.unwrap_or_else(|| failure_for(reason)),
                    }
                }
                // The capture is gone and the pointer is on the twin.
                HomeState::Entering(Entering::Focusing { .. }) => {
                    live = false;
                    Notice::HomeFailed {
                        key,
                        reason: failure.unwrap_or_else(|| failure_for(reason)),
                    }
                }
                HomeState::Home | HomeState::Exiting(Exiting::Hud { .. }) => {
                    live = false;
                    failure.map_or(
                        Notice::Home {
                            key,
                            entered: false,
                        },
                        |reason| Notice::HomeFailed { key, reason },
                    )
                }
                // An exit capture is being activated or cancelled: it may exist.
                HomeState::Exiting(Exiting::Activating { .. } | Exiting::Cancelled { .. }) => {
                    failure.map_or(
                        Notice::Home {
                            key,
                            entered: false,
                        },
                        |reason| Notice::HomeFailed { key, reason },
                    )
                }
            };
            out.push(Output::Notice(notice));
        }
        let point = warp_to.or(fallback).filter(|_| warps);
        if live {
            match point {
                Some(point) if !entry_stranded => {
                    self.release_and_warp(WarpPurpose::Leave, point, now, out);
                }
                Some(_) => out.push(Output::EndCapture { warp_to: None }),
                None => out.push(Output::EndCapture { warp_to }),
            }
        } else if let Some(point) = point
            && (home.is_some() || capture.is_some())
            && !entry_stranded
        {
            // Nothing to end, but the pointer may be on the invisible twin: a plain warp.
            self.release_and_warp(WarpPurpose::Leave, point, now, out);
        }
        if let Some(mut session) = session {
            self.end_session(&mut session, reason, send_end, now, out);
        }
        if let Some(capture) = capture.filter(|_| live) {
            self.phase = Phase::Returning {
                capture: capture.id,
                deadline: now.saturating_add(END_TIMEOUT),
                warp: point,
            };
        } else {
            self.finish_return(now, out);
        }
    }

    fn finish_return(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        self.phase = Phase::Idle;
        self.chord_keys.clear();
        self.capture_buttons.clear();
        self.push = None;
        self.hide_hud(now, out);
    }

    fn disarm(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        self.armed = false;
        self.push = None;
        if let Some(hold) = &mut self.hotkey {
            hold.rearm = false;
        }
        self.update_portals(now, out);
    }

    fn arm(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        self.armed = true;
        // Every re-arm reopens the engine's side of the gate (the callers emit `EngineGate`).
        self.gate_closed = false;
        self.update_portals(now, out);
    }

    fn release(&mut self, cause: ReleaseCause, now: MonoTime, out: &mut Vec<Output>) {
        // WP-4.5: the session this release ends, if there is one. `return_home` ends every session
        // it finds (and does nothing in `Idle` or `Returning`), so a session here is exactly one
        // ended controller session; a release with none says nothing.
        let ended = self.session().map(|s| s.peer);
        // Explicit release disarms crossing (04 §6); an ordinary pointer crossing home does not.
        self.return_home(EndReason::Released, None, false, true, now, out);
        if let Some(peer) = ended {
            out.push(Output::Notice(Notice::ControlReleased { peer, cause }));
        }
        self.disarm(now, out);
    }

    fn panic(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        if let Some(hold) = &mut self.hotkey {
            hold.fired = true;
        }
        // The engine gate is closed until the re-arm (the callers emit `EngineGate(false)`).
        self.gate_closed = true;
        self.return_home(EndReason::Panic, None, false, true, now, out);
        self.disarm(now, out);
        out.push(Output::Notice(Notice::Panic));
    }

    fn hotkey_event(&mut self, event: HotkeyEvent, now: MonoTime, out: &mut Vec<Output>) {
        match event {
            HotkeyEvent::Pressed { at } if self.hotkey.is_none() => {
                let rearm = !self.armed && !self.chord_press_outstanding;
                self.hotkey = Some(HotkeyHold {
                    since: at,
                    fired: false,
                    rearm,
                });
                if self.armed {
                    self.release(ReleaseCause::Chord, now, out);
                }
            }
            HotkeyEvent::Released { .. } => {
                let rearm =
                    self.hotkey.take().is_some_and(|h| h.rearm) && !self.chord_press_outstanding;
                self.chord_press_outstanding = false;
                if rearm && !self.armed && matches!(self.phase, Phase::Idle) {
                    out.push(Output::EngineGate(true));
                    self.arm(now, out);
                }
            }
            _ => {}
        }
    }

    fn tick(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        if self
            .hotkey
            .as_ref()
            .is_some_and(|h| !h.fired && now >= h.since.saturating_add(self.config.panic_hold))
        {
            // Engine handles Command(Panic)'s gate itself. A timed hotkey panic needs this output.
            out.push(Output::EngineGate(false));
            self.panic(now, out);
        }
        // WP-2.43: the home deadlines, the bind removal's retries and the portal re-sends.
        self.home_tick(now, out);
        match &mut self.phase {
            Phase::Idle => {
                // WP-2.43j: a push from before the newest warp's answer is stale; while a warp is
                // unanswered (bounded by `END_TIMEOUT`), the dwell waits for it.
                if self
                    .push
                    .zip(self.warp_fence)
                    .is_some_and(|(push, fence)| push.since < fence)
                {
                    self.push = None;
                }
                let warp_pending = self.warp_pending_until().is_some_and(|until| now < until);
                if let Some(push) = self
                    .push
                    .filter(|p| now >= p.since.saturating_add(self.config.push_to_cross))
                    .filter(|_| !warp_pending)
                {
                    self.begin_crossing(push, now, out);
                }
            }
            Phase::Crossing(c) => match c.wait {
                Wait::Hud(at) if now >= at => {
                    self.return_home(EndReason::Released, None, false, true, now, out)
                }
                Wait::Handshake(at) if now >= at => {
                    out.push(Output::Notice(Notice::LostConnection(c.entry.0.node)));
                    self.return_home(EndReason::LinkLost, None, false, true, now, out);
                }
                Wait::Capture(at) if now >= at => {
                    self.return_home(EndReason::Released, None, false, true, now, out);
                }
                _ => {}
            },
            Phase::Controlling(c) => {
                if c.session
                    .lease
                    .lost(now, self.rtts.get(&c.session.peer).copied())
                {
                    out.push(Output::Notice(Notice::LostConnection(c.session.peer)));
                    self.return_home(EndReason::LinkLost, None, false, true, now, out);
                    return;
                }
                if !self.ensure_sequence_room(now, out) {
                    return;
                }
                let Phase::Controlling(c) = &mut self.phase else {
                    return;
                };
                let held = self.router.held_on(c.session.peer);
                if now >= c.session.lease.next_heartbeat(!held.is_empty()) {
                    let mut held_keys = Vec::new();
                    let mut held_buttons = Vec::new();
                    for item in held {
                        match item {
                            Held::Key(k) => {
                                held_keys.push(self.key_mappings.get(&k).copied().unwrap_or(k));
                            }
                            Held::Button(b) => held_buttons.push(b),
                        }
                    }
                    let (session, seq) = c.session.next_input(now);
                    c.session.lease.heartbeat_sent(now);
                    out.push(Output::SendInput {
                        peer: c.session.peer,
                        msg: InputMessage::State {
                            session,
                            seq,
                            held_keys,
                            held_buttons,
                        },
                    });
                }
            }
            Phase::Returning { deadline, warp, .. } if now >= *deadline => {
                // A capture that began on a twin strip, or whose home ended, always warps (§2.7).
                match *warp {
                    Some(point) => {
                        self.release_and_warp(WarpPurpose::Leave, point, now, out);
                    }
                    None => out.push(Output::EndCapture { warp_to: None }),
                }
                self.finish_return(now, out);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------------------------
// WP-2.43: home on the twin (docs/wp/WP-2.43.md §2, amendments A1-A9 and B1).
//
// The controller can *go home* into one of this node's own twin-parked windows that the peer it
// controls shows: it settles every injector this node owns, installs the home bind, releases the
// capture, puts the physical pointer on the twin and confirms the window has focus, all while the
// E1 session stays open. Pushing against a twin strip resumes the session at the proxy's edge.
// ---------------------------------------------------------------------------------------------

/// Why an entry or a home that ends with its session is reported as failed.
fn failure_for(reason: EndReason) -> HomeFailure {
    match reason {
        EndReason::Released | EndReason::Panic | EndReason::ControllerLocked => HomeFailure::Guard,
        _ => HomeFailure::Gone,
    }
}

/// Whether `p` (device pixels of the proxy's display) is inside the placement.
fn inside(placement: &Proxy, p: PointDevice) -> bool {
    p.x >= placement.origin.x
        && p.y >= placement.origin.y
        && p.x < placement.origin.x + f64::from(placement.size.width)
        && p.y < placement.origin.y + f64::from(placement.size.height)
}

/// §2.5 "Mapping": a press at fraction `t` along a strip on the twin output's `edge` maps to the
/// host point just outside the proxy's corresponding edge, clamped into the host display.
fn exit_point(placement: &Proxy, geometry: &DisplayGeometry, edge: Edge, t: f64) -> PointDevice {
    let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
    let (w, h) = (
        f64::from(placement.size.width),
        f64::from(placement.size.height),
    );
    let origin = placement.origin;
    geometry.clamp_device(match edge {
        Edge::Left => PointDevice::new(origin.x - 1.0, origin.y + t * h),
        Edge::Right => PointDevice::new(origin.x + w, origin.y + t * h),
        Edge::Top => PointDevice::new(origin.x + t * w, origin.y - 1.0),
        Edge::Bottom => PointDevice::new(origin.x + t * w, origin.y + h),
    })
}

/// A side with room on the host needs no continuation. Check only the edge's normal coordinate:
/// the tangent endpoint is valid even when it is outside the half-open proxy rectangle.
fn offerable(placement: &Proxy, geometry: &DisplayGeometry, edge: Edge) -> bool {
    let point = exit_point(placement, geometry, edge, 0.5);
    match edge {
        Edge::Left | Edge::Right => {
            point.x < placement.origin.x
                || point.x >= placement.origin.x + f64::from(placement.size.width)
        }
        Edge::Top | Edge::Bottom => {
            point.y < placement.origin.y
                || point.y >= placement.origin.y + f64::from(placement.size.height)
        }
    }
}

/// Same-host displays have no portal records. Match the ordinary pointer tracker's adjacency,
/// including its touch tolerance and half-open shared span, in canvas millimetres.
fn host_span(from: &Placed, to: &Placed, edge: Edge) -> Option<(f64, f64)> {
    if from.id == to.id || from.id.node != to.id.node {
        return None;
    }
    let a = from.rect();
    let b = to.rect();
    let distance = match edge {
        Edge::Left => a.min().x - b.max().x,
        Edge::Right => a.max().x - b.min().x,
        Edge::Top => a.min().y - b.max().y,
        Edge::Bottom => a.max().y - b.min().y,
    };
    if distance.abs() > crosspane_input::layout::TOUCH_TOLERANCE_MM {
        return None;
    }
    let (start, end) = match edge {
        Edge::Left | Edge::Right => (a.min().y.max(b.min().y), a.max().y.min(b.max().y)),
        Edge::Top | Edge::Bottom => (a.min().x.max(b.min().x), a.max().x.min(b.max().x)),
    };
    (start < end).then_some((start, end))
}

fn along_edge(point: PointDevice, edge: Edge) -> f64 {
    match edge {
        Edge::Left | Edge::Right => point.y,
        Edge::Top | Edge::Bottom => point.x,
    }
}

/// §2.5: a strip sits on the twin output's `edge` and spans the content's extent along it, in
/// device pixels of the twin (as `CapturePortal` requires).
fn strip_span(content: PixelRect, edge: Edge) -> (f64, f64) {
    match edge {
        Edge::Left | Edge::Right => (f64::from(content.min.y), f64::from(content.max.y)),
        Edge::Top | Edge::Bottom => (f64::from(content.min.x), f64::from(content.max.x)),
    }
}

fn edge_index(edge: Edge) -> u32 {
    match edge {
        Edge::Left => 0,
        Edge::Right => 1,
        Edge::Top => 2,
        Edge::Bottom => 3,
    }
}

/// A1: 100 ms doubling to 2 s.
fn backoff(attempt: u32) -> Duration {
    TEARDOWN_BACKOFF_MIN
        .saturating_mul(1 << attempt.min(8))
        .min(TEARDOWN_BACKOFF_MAX)
}

impl ControllerE1 {
    /// Snapshot only the continuations that intersect an offered flush strip. Unrelated layout
    /// changes, especially for non-flush homes, must not invalidate an already shown exit HUD.
    fn flush_mapping(&self) -> Vec<FlushMapping> {
        let (Phase::Controlling(c), Some(layout)) = (&self.phase, &self.layout) else {
            return Vec::new();
        };
        let mut mappings = Vec::new();
        for home in self.twin_homes.iter().filter(|h| h.peer == c.session.peer) {
            let Some((placement, host, geometry)) = self.coherent(home) else {
                continue;
            };
            let Some(from) = layout.get(host) else {
                continue;
            };
            for strip in self.strips_of(home.projection) {
                let edge = strip.edge;
                if offerable(&placement, &geometry, edge) {
                    continue;
                }
                let start = exit_point(&placement, &geometry, edge, 0.0);
                let end = exit_point(&placement, &geometry, edge, 1.0);
                let portals = layout
                    .portals()
                    .iter()
                    .filter(|p| {
                        p.from == host
                            && p.edge == edge
                            && (p.to.node == self.config.node || self.peers.contains(&p.to.node))
                            && along_edge(start, edge) <= p.end
                            && along_edge(end, edge) >= p.start
                    })
                    .filter_map(|p| Some((*p, *from, *layout.get(p.to)?)))
                    .collect();
                let neighbors = layout
                    .to_canvas(host, start)
                    .zip(layout.to_canvas(host, end))
                    .map_or_else(Vec::new, |(start, end)| {
                        let (start, end) = match edge {
                            Edge::Left | Edge::Right => (start.y, end.y),
                            Edge::Top | Edge::Bottom => (start.x, end.x),
                        };
                        layout
                            .displays()
                            .iter()
                            .filter(|to| {
                                host_span(from, to, edge)
                                    .is_some_and(|(low, high)| start < high && end >= low)
                            })
                            .copied()
                            .collect()
                    });
                mappings.push(FlushMapping {
                    projection: home.projection,
                    edge,
                    host: *from,
                    start,
                    end,
                    portals,
                    neighbors,
                });
            }
        }
        mappings
    }

    /// A flush edge keeps the full content strip when any of its mapped span continues.
    /// Actual presses are checked separately, so uncovered positions stay home quietly.
    fn exit_offerable(
        &self,
        placement: &Proxy,
        host: GlobalDisplayId,
        geometry: &DisplayGeometry,
        edge: Edge,
    ) -> bool {
        if offerable(placement, geometry, edge) {
            return true;
        }
        let Some(layout) = &self.layout else {
            return false;
        };
        let Some(from) = layout.get(host) else {
            return false;
        };
        let start_point = exit_point(placement, geometry, edge, 0.0);
        let end_point = exit_point(placement, geometry, edge, 1.0);
        let (start, end) = (along_edge(start_point, edge), along_edge(end_point, edge));
        if layout.portals().iter().any(|p| {
            p.from == host
                && p.edge == edge
                && (p.to.node == self.config.node || self.peers.contains(&p.to.node))
                && start <= p.end
                && end >= p.start
        }) {
            return true;
        }
        let Some(start_canvas) = layout.to_canvas(host, start_point) else {
            return false;
        };
        let Some(end_canvas) = layout.to_canvas(host, end_point) else {
            return false;
        };
        let (start, end) = match edge {
            Edge::Left | Edge::Right => (start_canvas.y, end_canvas.y),
            Edge::Top | Edge::Bottom => (start_canvas.x, end_canvas.x),
        };
        layout.displays().iter().any(|to| {
            host_span(from, to, edge).is_some_and(|(low, high)| start < high && end >= low)
        })
    }

    /// Map through the layout the backend actually confirmed, with ordinary same-host
    /// adjacency taking precedence over cross-node portals as in PointerTracker::step.
    fn flush_target(
        &self,
        host: GlobalDisplayId,
        edge: Edge,
        point: PointDevice,
    ) -> Option<ExitTarget> {
        let layout = self.confirmed_portals.as_ref()?.layout.as_ref()?;
        let from = layout.get(host)?;
        let canvas = layout.to_canvas(host, point)?;
        let along = match edge {
            Edge::Left | Edge::Right => canvas.y,
            Edge::Top | Edge::Bottom => canvas.x,
        };
        if let Some(to) = layout.displays().iter().find(|to| {
            host_span(from, to, edge).is_some_and(|(start, end)| along >= start && along < end)
        }) {
            let rect = to.rect();
            let canvas = match edge {
                Edge::Left => crosspane_types::geom::PointMm::new(rect.max().x, along),
                Edge::Right => crosspane_types::geom::PointMm::new(rect.min().x, along),
                Edge::Top => crosspane_types::geom::PointMm::new(along, rect.max().y),
                Edge::Bottom => crosspane_types::geom::PointMm::new(along, rect.min().y),
            };
            let position = to
                .geometry
                .clamp_device(to.geometry.mm_to_device((canvas - to.origin).to_point()));
            return Some(ExitTarget {
                host,
                host_point: point,
                display: to.id,
                point: position,
                portal: None,
            });
        }
        let coordinate = along_edge(point, edge);
        let portal = layout.portals().iter().find(|p| {
            p.from == host
                && p.edge == edge
                && coordinate >= p.start
                && coordinate <= p.end
                && (p.to.node == self.config.node || self.peers.contains(&p.to.node))
        })?;
        let fraction = (coordinate - portal.start) / (portal.end - portal.start);
        let (display, position) = layout.entry(portal.id, fraction)?;
        Some(ExitTarget {
            host,
            host_point: point,
            display,
            point: position,
            portal: Some(*portal),
        })
    }

    // ---- state accessors ----

    fn home_copy(&self) -> Option<Home> {
        match &self.phase {
            Phase::Controlling(c) => c.home,
            _ => None,
        }
    }

    fn home_state(&self) -> Option<HomeState> {
        self.home_copy().map(|h| h.state)
    }

    fn set_home_state(&mut self, state: HomeState) {
        if let Phase::Controlling(c) = &mut self.phase
            && let Some(home) = &mut c.home
        {
            home.state = state;
        }
    }

    fn key_of(&self, projection: ProjectionId) -> ProjectionKey {
        ProjectionKey {
            source: self.config.node,
            projection,
        }
    }

    /// The projection this controller is entering, home in, exiting, or whose bind it is still
    /// removing, if any (WP-2.43 A1: the seat stays arbitrated until removal is confirmed).
    pub(crate) fn home(&self) -> Option<(NodeId, ProjectionId)> {
        if let Phase::Controlling(c) = &self.phase
            && let Some(home) = &c.home
        {
            return Some((home.peer, home.projection));
        }
        self.teardown.map(|t| (t.peer, t.projection))
    }

    /// The capture the controller knows about: the live one, or the one being ended.
    fn known_capture(&self) -> Option<CaptureId> {
        match &self.phase {
            Phase::Controlling(c) => Some(c.capture.id),
            Phase::Crossing(c) => c.capture.map(|c| c.id),
            Phase::Returning { capture, .. } => Some(*capture),
            Phase::Idle => None,
        }
    }

    /// This controller still holds capture `id` as its live capture: as the session's capture
    /// (also while an entry that has not released it yet is under way) or retained through a
    /// handoff to a third node. Not once it is being ended (`Returning`) or has been released.
    fn retains_capture(&self, id: CaptureId) -> bool {
        match &self.phase {
            Phase::Controlling(c) => {
                c.capture.id == id
                    && matches!(
                        c.home.map(|h| h.state),
                        None | Some(HomeState::Entering(
                            Entering::Draining { .. } | Entering::Binding { .. }
                        ))
                    )
            }
            Phase::Crossing(c) => c.capture.is_some_and(|capture| capture.id == id),
            _ => false,
        }
    }

    /// An entry is waiting for every injector this node owns to settle (§2.3 step 1).
    pub(crate) fn draining(&self) -> bool {
        matches!(
            self.home_state(),
            Some(HomeState::Entering(Entering::Draining { .. }))
        )
    }

    fn exiting_hud(&self) -> bool {
        matches!(
            self.home_state(),
            Some(HomeState::Exiting(Exiting::Hud { .. }))
        )
    }

    /// Home, with nothing in flight: a press against a strip may start an exit.
    fn home_is_resting(&self) -> bool {
        matches!(self.home_state(), Some(HomeState::Home))
    }

    fn entering_before_release(&self) -> bool {
        matches!(
            self.home_state(),
            Some(HomeState::Entering(
                Entering::Draining { .. } | Entering::Binding { .. }
            ))
        )
    }

    fn input_mode(&self) -> InputMode {
        match &self.phase {
            Phase::Crossing(c) if c.capture.is_some_and(|c| c.started) => InputMode::Routing,
            Phase::Controlling(c) if c.capture.started => match c.home.map(|h| h.state) {
                None
                | Some(HomeState::Entering(Entering::Draining { .. } | Entering::Binding { .. })) => {
                    InputMode::Routing
                }
                // The capture still exists (being released or cancelled): the chord must still
                // work through it, though nothing is routed.
                Some(
                    HomeState::Entering(Entering::Releasing { .. })
                    | HomeState::Exiting(Exiting::Cancelled { .. }),
                ) => InputMode::ChordOnly,
                Some(HomeState::Exiting(Exiting::Activating { .. })) => InputMode::Buffer,
                Some(_) => InputMode::Off,
            },
            _ => InputMode::Off,
        }
    }

    fn twin_home(&self, peer: NodeId, projection: ProjectionId) -> Option<TwinHome> {
        self.twin_homes
            .iter()
            .find(|h| h.projection == projection && h.peer == peer)
            .copied()
    }

    /// §2.2.1: the coherent placement of `home` (display, geometry of the peer's display in the
    /// layout), if it has one.
    fn coherent(&self, home: &TwinHome) -> Option<(Proxy, GlobalDisplayId, DisplayGeometry)> {
        let placement = home.placed?;
        let id = GlobalDisplayId {
            node: home.peer,
            display: placement.display,
        };
        let geometry = self.layout.as_ref()?.get(id)?.geometry;
        Some((placement, id, geometry))
    }

    fn strips_of(&self, projection: ProjectionId) -> Vec<CapturePortal> {
        let Some(slot) = self
            .twin_slots
            .get(&projection)
            .copied()
            .filter(|slot| *slot < MAX_TWIN_SLOT)
        else {
            return Vec::new();
        };
        let base = TWIN_PORTAL_BASE + 4 * slot;
        self.twin_strips
            .iter()
            .filter(|s| (base..base + 4).contains(&s.id.0))
            .copied()
            .collect()
    }

    fn portal_offered(&self, projection: ProjectionId, portal: PortalId) -> bool {
        self.strips_of(projection).iter().any(|s| s.id == portal)
    }

    /// §2.2.2: at least one exit exists and the current portal set that contains it is installed.
    fn exits_installed(&self, projection: ProjectionId) -> bool {
        self.portals_installed && !self.strips_of(projection).is_empty()
    }

    fn fallback_point(&self, display: DisplayId) -> Option<(DisplayId, PointDevice)> {
        let info = self
            .displays
            .get(&self.config.node)?
            .iter()
            .find(|d| d.id == display)?;
        let size = info.geometry.pixel_size;
        Some((
            display,
            PointDevice::new(f64::from(size.width) / 2.0, f64::from(size.height) / 2.0),
        ))
    }

    // ---- operations ----

    fn alloc_op(&mut self) -> HomeOp {
        let op = HomeOp(self.next_op);
        self.next_op = self.next_op.saturating_add(1);
        op
    }

    fn register_warp(
        &mut self,
        op: HomeOp,
        purpose: WarpPurpose,
        target: (DisplayId, PointDevice),
        now: MonoTime,
    ) {
        // WP-2.43j: a push against a portal was made where the pointer was before this warp.
        self.push = None;
        self.warp_issued = Some(now);
        self.warps.push(WarpEntry {
            op,
            purpose,
            target,
        });
        if self.warps.len() > WARPS_MAX {
            self.warps.remove(0);
        }
    }

    fn take_warp(&mut self, op: HomeOp) -> Option<WarpEntry> {
        let index = self.warps.iter().position(|w| w.op == op)?;
        Some(self.warps.remove(index))
    }

    /// End the capture (if one is live) and warp the pointer, correlated by a fresh operation.
    fn release_and_warp(
        &mut self,
        purpose: WarpPurpose,
        target: (DisplayId, PointDevice),
        now: MonoTime,
        out: &mut Vec<Output>,
    ) -> HomeOp {
        let op = self.alloc_op();
        self.register_warp(op, purpose, target, now);
        out.push(Output::ReleaseAndWarp {
            op,
            warp_to: target,
        });
        op
    }

    /// A1: the home bind's removal is a phase of its own.
    fn start_teardown(
        &mut self,
        peer: NodeId,
        projection: ProjectionId,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        if self.teardown.is_some() {
            return;
        }
        let op = self.alloc_op();
        self.teardown = Some(Teardown {
            op,
            attempt: 0,
            next: now.saturating_add(backoff(0)),
            peer,
            projection,
            continuation: None,
        });
        out.push(Output::HomeBind { op, install: false });
    }

    // ---- timers ----

    /// Passive guards are dropped once expired (A6).
    fn prune(&mut self, now: MonoTime) {
        self.exit_retry.retain(|_, until| now < *until);
        if self.home_fence.is_some_and(|(_, until)| now >= until) {
            self.home_fence = None;
        }
    }

    /// A retry can only succeed while the platform's gate is open on both sides: the session
    /// permits I/O and no panic has closed the engine's side. While it is closed the budget is
    /// kept, so the re-arm still recovers the pointer.
    fn stranded_can_run(&self) -> bool {
        self.permits_io() && !self.gate_closed && matches!(self.phase, Phase::Idle)
    }

    fn home_deadline(&self) -> Option<MonoTime> {
        let state = match self.home_state() {
            Some(HomeState::Entering(
                Entering::Draining { deadline }
                | Entering::Binding { deadline }
                | Entering::Releasing { deadline }
                | Entering::Focusing { deadline },
            ))
            | Some(HomeState::Exiting(
                Exiting::Hud { deadline, .. }
                | Exiting::Activating { deadline, .. }
                | Exiting::Cancelled { deadline, .. },
            )) => Some(deadline),
            _ => None,
        };
        // Actionable deadlines stay while due, until their action runs (A6); a stranded pointer's
        // retry can only run while this node is unlocked and idle, so it waits for an input then.
        let stranded = self
            .stranded
            .filter(|_| self.stranded_can_run())
            .map(|s| s.next);
        [
            state,
            self.teardown.map(|t| t.next),
            self.portals_retry,
            stranded,
            self.home_fence.map(|(_, until)| until),
            self.exit_retry.values().copied().min(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn home_tick(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        // A1: the bind's removal, retried with backoff until confirmed.
        if let Some(t) = self.teardown
            && now >= t.next
        {
            let op = self.alloc_op();
            let attempt = t.attempt.saturating_add(1);
            self.teardown = Some(Teardown {
                op,
                attempt,
                next: now.saturating_add(backoff(attempt)),
                continuation: None,
                ..t
            });
            out.push(Output::HomeBind { op, install: false });
            if let Some(continuation) = t.continuation {
                // An unanswered removal is retried under A1, but never authorizes a handoff.
                self.finish_flush_handoff(continuation, false, now, out);
            }
        }
        // §2.8: a portal set that was not installed is offered again.
        if self.portals_retry.is_some_and(|at| now >= at) {
            if self.portals_failed
                && self.portal_requests.is_empty()
                && !self.twin_strips.is_empty()
            {
                self.portals_retry = Some(now.saturating_add(PORTALS_RETRY));
                out.push(Output::SetPortals(self.portals.clone()));
            } else {
                self.portals_retry = None;
            }
        }
        let Some(home) = self.home_copy() else {
            return;
        };
        match home.state {
            // Settled or not, `after_e2` decides: it alone sees the injectors.
            HomeState::Entering(Entering::Draining { .. }) | HomeState::Home => {}
            HomeState::Entering(Entering::Binding { deadline }) if now >= deadline => {
                self.abort_entry(HomeFailure::Bind, now, out);
            }
            HomeState::Entering(Entering::Releasing { deadline }) if now >= deadline => {
                self.leave_home(Some(HomeFailure::Release), now, out);
            }
            HomeState::Entering(Entering::Focusing { deadline }) if now >= deadline => {
                self.leave_home(Some(HomeFailure::Focus), now, out);
            }
            HomeState::Exiting(Exiting::Hud {
                portal, deadline, ..
            }) if now >= deadline => {
                self.hide_hud(now, out);
                self.set_home_state(HomeState::Home);
                self.exit_retry
                    .insert(portal, now.saturating_add(HOME_RETRY));
            }
            HomeState::Exiting(Exiting::Activating {
                id,
                portal,
                deadline,
                ..
            }) if now >= deadline => self.cancel_exit(id, portal, now, out),
            HomeState::Exiting(Exiting::Cancelled { deadline, .. }) if now >= deadline => {
                // No `Ended` came: end it once more (idempotent) and carry on.
                out.push(Output::EndCapture { warp_to: None });
                self.exit_resolved();
            }
            _ => {}
        }
    }

    /// §2.7 "Stranded pointer": a home-related warp that was skipped or failed is retried while
    /// this node is unlocked and idle, at most `STRANDED_ATTEMPTS` times, until one is done.
    fn stranded_retry(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        let Some(stranded) = self.stranded else {
            return;
        };
        if now < stranded.next || !self.stranded_can_run() {
            return;
        }
        if stranded.attempts >= STRANDED_ATTEMPTS {
            self.stranded = None;
            return;
        }
        self.stranded = Some(Stranded {
            next: now.saturating_add(STRANDED_RETRY),
            attempts: stranded.attempts + 1,
            ..stranded
        });
        self.release_and_warp(WarpPurpose::Retry, stranded.target, now, out);
    }

    // ---- portals ----

    /// The twin strips to offer now (§2.4, §2.5): while this controller controls a peer, four
    /// edge strips (the offerable ones) for each of that peer's twin homes with a coherent
    /// placement. Never while idle, crossing or returning, so an injected pointer on the twin
    /// can never press one.
    fn twin_strip_set(&self) -> Vec<CapturePortal> {
        let Phase::Controlling(c) = &self.phase else {
            return Vec::new();
        };
        let mut strips = Vec::new();
        for home in &self.twin_homes {
            if home.peer != c.session.peer {
                continue;
            }
            let (Some((placement, host, geometry)), Some(slot)) = (
                self.coherent(home),
                self.twin_slots
                    .get(&home.projection)
                    .copied()
                    .filter(|slot| *slot < MAX_TWIN_SLOT),
            ) else {
                continue;
            };
            for edge in EDGES {
                let (from, to) = strip_span(home.content, edge);
                if !self.exit_offerable(&placement, host, &geometry, edge) || from >= to {
                    continue;
                }
                strips.push(CapturePortal {
                    id: PortalId(TWIN_PORTAL_BASE + 4 * slot + edge_index(edge)),
                    display: home.display,
                    edge,
                    from,
                    to,
                });
            }
        }
        strips
    }

    /// One `Output::SetPortals` the engine actually emitted (the engine reports each, in order,
    /// after it has dropped the ones it suppresses while this node is controlled): it is answered
    /// by one `Input::PortalsSet`, in order. The mapping it was emitted under is kept until then,
    /// so what its ids mean is known whichever way the answer goes (B1).
    pub(crate) fn portal_emitted(&mut self, set: &[CapturePortal]) {
        if self.portal_requests.len() >= PORTAL_REQUESTS_MAX {
            // The answers stopped coming: forget the oldest rather than grow. Its answer is still
            // owed and arrives first, so it is counted: the answers that follow must not be read
            // against the wrong snapshots (identical ids can mean different strips). What the
            // backend holds is unknown until an answer that is aligned again confirms a set, so
            // nothing is confirmed meanwhile: no strip can be pressed (fail closed).
            self.portal_requests.pop_front();
            self.portal_skip = self.portal_skip.saturating_add(1);
            self.confirmed_portals = Some(PortalMap::empty());
        }
        self.portal_requests.push_back(PortalRequest {
            ids: set.iter().map(|p| p.id).collect(),
            map: PortalMap {
                offered: set.to_vec(),
                // An empty set installs nothing: no strip can be pressed under it.
                layout: if set.is_empty() {
                    None
                } else {
                    self.layout.clone()
                },
            },
        });
        self.portals_installed = false;
        self.portals_failed = false;
        self.portals_retry = None;
    }

    fn mark_portals_failed(&mut self, now: MonoTime) {
        self.portals_installed = false;
        self.portals_failed = true;
        if !self.twin_strips.is_empty() {
            self.portals_retry = Some(now.saturating_add(PORTALS_RETRY));
        }
    }

    /// `Input::PortalsSet`: the backend's answer to one `SetPortals` (§2.2.2, A9, B1).
    fn portals_set(
        &mut self,
        ids: &[PortalId],
        result: &Result<(), PortalsFailure>,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        // Answers are strictly in order, one per emitted set: this one belongs to the oldest
        // outstanding request. Its ids only check that the two agree; if they don't (an answer
        // nobody asked for, or one for another set) nothing can be relied on, which is what an
        // uncertain answer says. (The engine is sans-IO: there is no log to note it in.)
        let (request, result) = if self.portal_skip > 0 {
            // The answer of a request dropped on overflow: there is no snapshot to read it
            // against, so it confirms nothing, whichever way it went (no request, below). It
            // still counts for the failure, retry and uncertainty handling.
            self.portal_skip -= 1;
            (None, *result)
        } else {
            let request = self.portal_requests.pop_front();
            let consistent = request.as_ref().is_some_and(|r| r.ids == ids);
            let result = if consistent {
                *result
            } else {
                Err(PortalsFailure::Uncertain)
            };
            (request, result)
        };
        match (&result, request) {
            // The set is installed: what its ids mean is what this request was emitted under.
            (Ok(()), Some(request)) => self.confirmed_portals = Some(request.map),
            // The previous set stays in force; if the backend never confirmed one, it holds none.
            (Err(PortalsFailure::Rejected), _) => {
                if self.confirmed_portals.is_none() {
                    self.confirmed_portals = Some(PortalMap::empty());
                }
            }
            // Whether the previous set survives is unknown: rely on nothing.
            _ => self.confirmed_portals = Some(PortalMap::empty()),
        }
        match result {
            Ok(()) => {
                // Only the answer to the newest set installs it: the set the backend now holds is
                // the one this controller offers, and nothing newer is outstanding.
                if self.portal_requests.is_empty() {
                    if self
                        .confirmed_portals
                        .as_ref()
                        .is_some_and(|map| map.offered == self.portals)
                    {
                        self.portals_installed = true;
                        self.portals_failed = false;
                        self.portals_retry = None;
                    } else {
                        self.mark_portals_failed(now);
                    }
                } else {
                    self.portals_installed = false;
                }
            }
            Err(failure) => {
                self.portals_installed = false;
                if self.portal_requests.is_empty() {
                    self.mark_portals_failed(now);
                }
                // B1: a timeout or a stopped backend leaves unknown whether the capture
                // survives: end it explicitly and wait for its end. A rejection leaves the
                // previous set and the capture intact.
                if failure == PortalsFailure::Uncertain {
                    self.capture_uncertain(now, out);
                }
                // A9: while home, no exit may be left that the backend didn't confirm.
                self.exits_lost(now, out);
            }
        }
    }

    /// B1: end the capture explicitly and treat it as gone only after its `Ended` or
    /// `CAPTURE_END_DEADLINE`.
    fn capture_uncertain(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        let has_capture = matches!(self.phase, Phase::Controlling(_))
            || matches!(&self.phase, Phase::Crossing(c) if c.capture.is_some());
        if !has_capture {
            return;
        }
        self.failure = Some(HomeFailure::Gone);
        self.return_home(EndReason::Released, None, false, true, now, out);
        self.failure = None;
        if let Phase::Returning { deadline, .. } = &mut self.phase {
            *deadline = now.saturating_add(CAPTURE_END_DEADLINE);
        }
    }

    /// The twin strips are no longer installed: an entry still before its release is abandoned,
    /// a home is left without a crossing.
    fn exits_lost(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        if self.entering_before_release() {
            self.abort_entry(HomeFailure::Gone, now, out);
        } else if self.home_copy().is_some() {
            self.leave_home(Some(HomeFailure::Gone), now, out);
        }
    }

    // ---- entry (§2.2, §2.3) ----

    /// §2.3: a prevalidated peer motion report; starts the entry transaction when every check of
    /// §2.2 and the corroboration of §2.3 pass.
    pub(crate) fn peer_motion(
        &mut self,
        peer: NodeId,
        projection: ProjectionId,
        position: PointDevice,
        now: MonoTime,
        _out: &mut Vec<Output>,
    ) {
        let Phase::Controlling(c) = &self.phase else {
            return;
        };
        if c.session.peer != peer || c.home.is_some() || !c.capture.started {
            return;
        }
        let (tracker_display, tracker) = c.tracker.position();
        let (hud_display, ended, last_motion) = (c.hud_display, c.capture.id, c.last_motion);
        // §2.2.4: no fence, and no bind removal outstanding (A1).
        if self.teardown.is_some()
            || self
                .home_fence
                .is_some_and(|(p, until)| p == projection && now < until)
        {
            return;
        }
        // §2.2.3: the guards of an ordinary crossing.
        if !self.armed
            || !self.permits_io()
            || !self.router.no_buttons_held()
            || !self.capture_buttons.is_empty()
        {
            return;
        }
        // §2.2.6: the physical pointer moved recently.
        if last_motion.is_none_or(|at| now.saturating_duration_since(at) > LOCAL_MOTION_AGE) {
            return;
        }
        // §2.2.1 and §2.2.2: a coherent placement, and an exit that is installed.
        let Some(twin) = self.twin_home(peer, projection) else {
            return;
        };
        let Some((placement, id, _)) = self.coherent(&twin) else {
            return;
        };
        if !self.exits_installed(projection) {
            return;
        }
        // §2.3: this node's own tracker is inside the placement, and agrees with the report.
        if tracker_display != id
            || !inside(&placement, tracker)
            || (tracker.x - (placement.origin.x + position.x)).abs() > ENTRY_SLACK
            || (tracker.y - (placement.origin.y + position.y)).abs() > ENTRY_SLACK
        {
            return;
        }
        let Some(fallback) = self.fallback_point(hud_display) else {
            return;
        };
        let op = self.alloc_op();
        let home = Home {
            op,
            peer,
            projection,
            window: twin.window,
            ended,
            ended_seen: false,
            fallback,
            bind: false,
            display: placement.display,
            generation: placement.generation,
            strips_gen: self.strips_gen,
            state: HomeState::Entering(Entering::Draining {
                deadline: now.saturating_add(DRAIN_TIMEOUT),
            }),
            entry_near: [false; 4],
            entry_held: [false; 4],
            entered_at: None,
        };
        if let Phase::Controlling(c) = &mut self.phase {
            c.home = Some(home);
        }
    }

    /// §2.2 items 1-5, re-checked at the drain-to-bind and (with `release`, A8) bind-to-release
    /// transitions.
    fn entry_guards(&self, home: &Home, release: bool, now: MonoTime) -> Result<(), HomeFailure> {
        let Phase::Controlling(c) = &self.phase else {
            return Err(HomeFailure::Gone);
        };
        let twin = self
            .twin_home(home.peer, home.projection)
            .ok_or(HomeFailure::Gone)?;
        let (placement, id, _) = self.coherent(&twin).ok_or(HomeFailure::Gone)?;
        if !self.exits_installed(home.projection) || !c.capture.started {
            return Err(HomeFailure::Gone);
        }
        if !self.armed
            || !self.permits_io()
            || !self.router.no_buttons_held()
            || !self.capture_buttons.is_empty()
        {
            return Err(HomeFailure::Guard);
        }
        if release {
            // A8: nothing the trigger saw has changed.
            let (display, position) = c.tracker.position();
            if display != id
                || placement.display != home.display
                || placement.generation != home.generation
                || self.strips_gen != home.strips_gen
                || !inside(&placement, position)
            {
                return Err(HomeFailure::Gone);
            }
            if c.last_motion
                .is_none_or(|at| now.saturating_duration_since(at) > ENTRY_FRESH)
            {
                return Err(HomeFailure::Guard);
            }
        }
        Ok(())
    }

    /// Called after E2 handled the same input: advances `Entering(Draining)` to `Binding` once
    /// every injector this node owns has settled (§2.3 step 1), and abandons it on a deadline or
    /// a failed guard.
    pub(crate) fn after_e2(&mut self, settled: bool, now: MonoTime, out: &mut Vec<Output>) {
        let Some(home) = self.home_copy() else {
            return;
        };
        let HomeState::Entering(Entering::Draining { deadline }) = home.state else {
            return;
        };
        if let Err(failure) = self.entry_guards(&home, false, now) {
            self.abort_entry(failure, now, out);
        } else if now >= deadline {
            // The deadline wins over a confirmation that arrives after it, whether or not a
            // tick has been delivered in between.
            self.abort_entry(HomeFailure::Drain, now, out);
        } else if settled {
            // §2.3 step 2: only now may the bind exist.
            out.push(Output::HomeBind {
                op: home.op,
                install: true,
            });
            if let Phase::Controlling(c) = &mut self.phase
                && let Some(home) = &mut c.home
            {
                home.bind = true;
                home.state = HomeState::Entering(Entering::Binding {
                    deadline: now.saturating_add(BIND_TIMEOUT),
                });
            }
        }
    }

    /// `Input::HomeBindSet` (§2.3 step 2, A1, A2).
    fn home_bind_set(
        &mut self,
        op: HomeOp,
        install: bool,
        result: &Result<(), Failure>,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        if !install {
            // Only the current attempt's answer counts; anything older is stale.
            if let Some(t) = self.teardown
                && t.op == op
            {
                if result.is_ok() {
                    self.teardown = None;
                } else if let Some(teardown) = &mut self.teardown {
                    teardown.continuation = None;
                }
                if let Some(continuation) = t.continuation {
                    // The timeout also wins when its callback arrives before the next Tick.
                    self.finish_flush_handoff(
                        continuation,
                        result.is_ok() && now < t.next,
                        now,
                        out,
                    );
                }
            }
            return;
        }
        let Some(home) = self.home_copy().filter(|h| h.op == op) else {
            return;
        };
        match (home.state, result) {
            (HomeState::Entering(Entering::Draining { .. }), _) => {}
            (HomeState::Entering(Entering::Binding { deadline }), Ok(())) => {
                if now >= deadline {
                    self.abort_entry(HomeFailure::Bind, now, out);
                } else {
                    self.begin_release(now, out);
                }
            }
            (HomeState::Entering(Entering::Binding { .. }), Err(_)) => {
                self.abort_entry(HomeFailure::Bind, now, out);
            }
            // The bind was lost while home and could not be reinstalled.
            (_, Err(_)) => self.leave_home(Some(HomeFailure::Bind), now, out),
            _ => {}
        }
    }

    /// §2.3 step 3, after the A8 re-check: release what the router holds on the peer, then end
    /// the capture and warp the pointer onto the twin at the tracker's current position.
    fn begin_release(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        let Some(home) = self.home_copy() else {
            return;
        };
        if let Err(failure) = self.entry_guards(&home, true, now) {
            self.abort_entry(failure, now, out);
            return;
        }
        let Some(twin) = self.twin_home(home.peer, home.projection) else {
            return;
        };
        let (Some((placement, _, _)), Phase::Controlling(c)) = (self.coherent(&twin), &self.phase)
        else {
            return;
        };
        let (_, position) = c.tracker.position();
        let content = twin.content;
        let (w, h) = (content.max.x - content.min.x, content.max.y - content.min.y);
        if w <= 0 || h <= 0 {
            self.abort_entry(HomeFailure::Gone, now, out);
            return;
        }
        // The current tracker position, mapped into the placement (A8): never the report's. It
        // lands `ENTRY_CLEARANCE` clear of the content's edges, never on a strip (WP-2.43j).
        let inset = |len: i32| f64::from(ENTRY_CLEARANCE.min((len - 1) / 2));
        let (cx, cy) = (inset(w), inset(h));
        let x = (position.x - placement.origin.x).clamp(cx, f64::from(w - 1) - cx);
        let y = (position.y - placement.origin.y).clamp(cy, f64::from(h - 1) - cy);
        let target = (
            twin.display,
            PointDevice::new(f64::from(content.min.x) + x, f64::from(content.min.y) + y),
        );
        let near = f64::from(ENTRY_NEAR);
        let entry_near = [
            x <= near,
            f64::from(w - 1) - x <= near,
            y <= near,
            f64::from(h - 1) - y <= near,
        ];
        // The first half of ending a session: an up for everything held, no `EndControl`.
        let phase = std::mem::replace(&mut self.phase, Phase::Idle);
        if let Phase::Controlling(mut c) = phase {
            self.release_session_held(&mut c.session, now, out);
            self.phase = Phase::Controlling(c);
        } else {
            self.phase = phase;
        }
        // The capture is released from here on: it is no longer one this controller retains.
        self.committed_exit = None;
        self.register_warp(home.op, WarpPurpose::Entry, target, now);
        out.push(Output::ReleaseAndWarp {
            op: home.op,
            warp_to: target,
        });
        self.set_home_state(HomeState::Entering(Entering::Releasing {
            deadline: now.saturating_add(END_TIMEOUT),
        }));
        if let Phase::Controlling(c) = &mut self.phase
            && let Some(home) = &mut c.home
        {
            home.entry_near = entry_near;
            home.entry_held = [false; 4];
            home.entered_at = None;
        }
    }

    /// `Input::CaptureReleased` (§3.2): the answer to one `ReleaseAndWarp`.
    fn capture_released(
        &mut self,
        op: HomeOp,
        result: &Result<Warp, Failure>,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        // Whatever it was for, the pointer may have moved: presses made before are stale, and so
        // is a push one of them started.
        self.warp_fence = Some(now);
        self.push = None;
        // An answer to an operation that isn't outstanding is stale and changes nothing.
        let Some(warp) = self.take_warp(op) else {
            return;
        };
        match warp.purpose {
            WarpPurpose::Entry => self.entry_released(op, result, now, out),
            WarpPurpose::Leave => match result {
                Ok(Warp::Done) => self.stranded = None,
                Ok(Warp::Skipped) | Err(_) => {
                    self.stranded = Some(Stranded {
                        target: warp.target,
                        next: now.saturating_add(STRANDED_RETRY),
                        attempts: 0,
                    });
                }
            },
            WarpPurpose::Retry => {
                if matches!(result, Ok(Warp::Done)) {
                    self.stranded = None;
                } else if let Some(stranded) = &mut self.stranded {
                    stranded.next = now.saturating_add(STRANDED_RETRY);
                }
            }
            // The pointer was meant to stay on the twin; a warp that failed or was skipped leaves
            // it wherever it drifted (F5): leave home and put it back on the desktop.
            WarpPurpose::Cancel => {
                if !matches!(result, Ok(Warp::Done)) {
                    self.leave_home(Some(HomeFailure::Warp), now, out);
                }
            }
        }
    }

    /// §2.3 step 4.
    fn entry_released(
        &mut self,
        op: HomeOp,
        result: &Result<Warp, Failure>,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        let Some(home) = self.home_copy().filter(|h| h.op == op) else {
            return;
        };
        let HomeState::Entering(Entering::Releasing { deadline }) = home.state else {
            return;
        };
        match result {
            Ok(Warp::Done) if now < deadline => {
                self.hide_hud(now, out);
                let focused = self
                    .twin_home(home.peer, home.projection)
                    .is_some_and(|t| t.focused);
                if focused {
                    self.commit_entry(now, out);
                } else {
                    out.push(Output::ActivateWindow {
                        window: home.window,
                    });
                    self.set_home_state(HomeState::Entering(Entering::Focusing {
                        deadline: now.saturating_add(FOCUS_TIMEOUT),
                    }));
                }
            }
            Ok(Warp::Skipped) | Err(_) => {
                // The pointer may already be on the twin, even with an open gate. Account for
                // recovery before leaving; keep a possibly-live capture fenced on an error.
                self.stranded = Some(Stranded {
                    target: home.fallback,
                    next: now.saturating_add(STRANDED_RETRY),
                    attempts: 0,
                });
                if matches!(result, Ok(Warp::Skipped))
                    && let Phase::Controlling(c) = &mut self.phase
                    && let Some(home) = &mut c.home
                {
                    home.ended_seen = true;
                }
                self.leave_home(Some(HomeFailure::Warp), now, out);
                if self.permits_io() && !self.gate_closed {
                    if let Some(stranded) = &mut self.stranded {
                        stranded.attempts = 1;
                    }
                    self.release_and_warp(WarpPurpose::Retry, home.fallback, now, out);
                }
            }
            // An error, or an answer after the deadline: a capture may still exist.
            _ => self.leave_home(Some(HomeFailure::Release), now, out),
        }
    }

    /// §2.3 step 6.
    fn commit_entry(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        let Some(home) = self.home_copy() else {
            return;
        };
        self.set_home_state(HomeState::Home);
        if let Phase::Controlling(c) = &mut self.phase
            && let Some(home) = &mut c.home
        {
            home.entered_at = Some(now);
        }
        out.push(Output::Notice(Notice::Home {
            key: self.key_of(home.projection),
            entered: true,
        }));
    }

    /// Abort before release (§2.3): the capture is still live and the session continues as
    /// before the trigger; the bind, if one was requested, goes through its removal phase (A1).
    fn abort_entry(&mut self, failure: HomeFailure, now: MonoTime, out: &mut Vec<Output>) {
        let home = match &mut self.phase {
            Phase::Controlling(c) => c.home.take(),
            _ => None,
        };
        let Some(home) = home else {
            return;
        };
        if home.bind {
            self.start_teardown(home.peer, home.projection, now, out);
        }
        self.home_fence = Some((home.projection, now.saturating_add(HOME_RETRY)));
        out.push(Output::Notice(Notice::HomeFailed {
            key: self.key_of(home.projection),
            reason: failure,
        }));
    }

    fn abort_entry_if_entering(
        &mut self,
        failure: HomeFailure,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        if self.entering_before_release() {
            self.abort_entry(failure, now, out);
        }
    }

    /// Record why the entry that the session's end is about to abort failed.
    fn note_entry_failure(&mut self, failure: HomeFailure) {
        if self.entering_before_release() {
            self.failure = Some(failure);
        }
    }

    /// §2.7: leave home without a crossing: end the session through the ordinary path, with the
    /// home rule (a correlated warp to the fallback point).
    fn leave_home(&mut self, failure: Option<HomeFailure>, now: MonoTime, out: &mut Vec<Output>) {
        if self.home_copy().is_none() {
            return;
        }
        self.failure = failure;
        self.return_home(EndReason::Released, None, false, true, now, out);
        self.failure = None;
    }

    // ---- E2's twin set (§2.5, §2.8) ----

    /// E2's current twin homes: allocate slots, rebuild the twin strips when they changed
    /// (bumping `strips_gen`), commit a pending entry when its window is focused, and abandon or
    /// leave home if its projection, placement or every exit is gone (§2.5, §2.7).
    pub(crate) fn set_twin_homes(
        &mut self,
        homes: Vec<TwinHome>,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        // Stable slots: assigned when a projection first appears, freed when it disappears, never
        // reused while the controller lives.
        self.twin_slots
            .retain(|projection, _| homes.iter().any(|h| h.projection == *projection));
        for home in &homes {
            if !self.twin_slots.contains_key(&home.projection) {
                self.twin_slots.insert(home.projection, self.next_slot);
                self.next_slot = self.next_slot.saturating_add(1);
            }
        }
        self.twin_homes = homes;
        if self.twin_strip_set() != self.twin_strips
            || self.flush_mapping() != self.home_exit_mapping
        {
            self.update_portals(now, out);
        }
        self.home_follow(now, out);
        // Leaving home ends the phase the strips depend on.
        if !self.twin_strips.is_empty() && self.twin_strip_set() != self.twin_strips {
            self.update_portals(now, out);
        }
    }

    /// Re-check the home against the twin set that was just updated.
    fn home_follow(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        let Some(home) = self.home_copy() else {
            return;
        };
        let twin = self.twin_home(home.peer, home.projection);
        let gone = twin.is_none_or(|t| self.coherent(&t).is_none())
            || self.strips_of(home.projection).is_empty();
        match home.state {
            HomeState::Entering(Entering::Draining { .. } | Entering::Binding { .. }) => {
                if gone {
                    self.abort_entry(HomeFailure::Gone, now, out);
                }
            }
            HomeState::Entering(Entering::Focusing { deadline }) => {
                if gone {
                    self.leave_home(Some(HomeFailure::Gone), now, out);
                } else if now >= deadline {
                    // A focus event that arrives after the deadline is too late, tick or not.
                    self.leave_home(Some(HomeFailure::Focus), now, out);
                } else if twin.is_some_and(|t| t.focused) {
                    self.commit_entry(now, out);
                }
            }
            HomeState::Entering(Entering::Releasing { .. }) | HomeState::Home => {
                if gone {
                    self.leave_home(Some(HomeFailure::Gone), now, out);
                }
            }
            HomeState::Exiting(Exiting::Hud {
                portal, strips_gen, ..
            }) => {
                if gone {
                    self.leave_home(Some(HomeFailure::Gone), now, out);
                } else if strips_gen != self.strips_gen {
                    // The strip set changed under the HUD: this exit is over, the next press
                    // starts a fresh one.
                    self.hide_hud(now, out);
                    self.set_home_state(HomeState::Home);
                    self.exit_retry
                        .insert(portal, now.saturating_add(HOME_RETRY));
                }
            }
            // The capture exists: a placement that is incoherent is judged when it is activated.
            HomeState::Exiting(Exiting::Activating { .. } | Exiting::Cancelled { .. }) => {
                if twin.is_none() {
                    self.leave_home(Some(HomeFailure::Gone), now, out);
                }
            }
        }
    }

    /// §2.4 (the phase 2 fallback cursor): this node's physical pointer on the twin while home is
    /// mapped through the placement and sent to the peer as ordinary pointer motion, so the
    /// peer's cursor follows it inside the proxy. Nothing is sent unless the pointer is on the
    /// twin of the home's window, the placement is coherent and the session may do I/O.
    fn local_pointer(
        &mut self,
        display: DisplayId,
        position: PointDevice,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        let Some(home) = self
            .home_copy()
            .filter(|h| matches!(h.state, HomeState::Home))
        else {
            return;
        };
        if !self.permits_io() || !position.x.is_finite() || !position.y.is_finite() {
            return;
        }
        let Some(twin) = self.twin_home(home.peer, home.projection) else {
            return;
        };
        let Some((placement, id, _)) = self.coherent(&twin) else {
            return;
        };
        let content = twin.content;
        let (w, h) = (content.max.x - content.min.x, content.max.y - content.min.y);
        if twin.display != display || w <= 0 || h <= 0 || !self.ensure_sequence_room(now, out) {
            return;
        }
        let x = (position.x - f64::from(content.min.x)).clamp(0.0, f64::from(w - 1));
        let y = (position.y - f64::from(content.min.y)).clamp(0.0, f64::from(h - 1));
        let Phase::Controlling(c) = &mut self.phase else {
            return;
        };
        let seq = c.session.motion_seq;
        c.session.motion_seq = seq.saturating_add(1);
        out.push(Output::SendMotion {
            peer: c.session.peer,
            msg: PointerMessage {
                session: c.session.id,
                seq,
                display: id.display,
                position: PointDevice::new(placement.origin.x + x, placement.origin.y + y),
            },
        });
    }

    // ---- captured input while an exit is activated (§2.6 step 5, A4) ----

    fn chord_completed(&self, usage: HidUsage) -> bool {
        usage == self.config.release_chord.key
            && self
                .config
                .release_chord
                .modifiers
                .iter()
                .all(|k| self.chord_keys.contains(k))
    }

    fn buffer_transition(&mut self, item: Held, down: bool) {
        if self.activation.len() >= ACTIVATION_LOG_MAX {
            // Fail closed: an exit whose activation can't be replayed is cancelled.
            self.activation_overflow = true;
        } else {
            self.activation.push((item, down));
        }
    }

    fn capture_key(&mut self, usage: HidUsage, down: bool, now: MonoTime, out: &mut Vec<Output>) {
        let mode = self.input_mode();
        if mode == InputMode::Buffer {
            self.buffer_transition(Held::Key(usage), down);
            return;
        }
        if down {
            self.chord_keys.insert(usage);
        } else {
            self.chord_keys.remove(&usage);
        }
        if down && self.chord_completed(usage) {
            // The authoritative hotkey pair may arrive after this captured chord.
            // Suppress that pair for re-arm purposes without comparing timestamps.
            self.chord_press_outstanding = true;
            self.release(ReleaseCause::Chord, now, out);
        } else if mode == InputMode::Routing {
            self.route(Held::Key(usage), down, now, out);
        }
    }

    fn capture_button(
        &mut self,
        button: MouseButton,
        down: bool,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        let mode = self.input_mode();
        if down {
            self.capture_buttons.insert(button);
        } else {
            self.capture_buttons.remove(&button);
        }
        match mode {
            InputMode::Routing => self.route(Held::Button(button), down, now, out),
            InputMode::Buffer => self.buffer_transition(Held::Button(button), down),
            InputMode::ChordOnly | InputMode::Off => {}
        }
    }

    /// `CaptureEvent::Ended` while home. True if it was expected or belongs to the exit capture.
    fn home_capture_ended(
        &mut self,
        id: CaptureId,
        reason: CaptureEnd,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) -> bool {
        let Some(home) = self.home_copy() else {
            return false;
        };
        match home.state {
            // The capture that was ended to go home: its `Ended` is expected, not a loss.
            HomeState::Entering(Entering::Releasing { .. } | Entering::Focusing { .. })
            | HomeState::Home
            | HomeState::Exiting(Exiting::Hud { .. })
                if id == home.ended =>
            {
                if let Phase::Controlling(c) = &mut self.phase
                    && let Some(home) = &mut c.home
                {
                    home.ended_seen = true;
                }
                true
            }
            // The exit capture's end during its activation, or while its cancellation is
            // outstanding. If this node asked for it, it is over; if the backend or the watchdog
            // took the capture away (`Lost`, `Aborted`), the strips and portals went with it:
            // nothing installed is left to push against, so leave home (A9, §2.7).
            HomeState::Exiting(
                Exiting::Activating { id: ended, .. } | Exiting::Cancelled { id: ended, .. },
            ) if ended == id && reason != CaptureEnd::Requested => {
                self.portals_installed = false;
                self.failure = Some(HomeFailure::Gone);
                self.return_home(EndReason::Released, None, true, true, now, out);
                self.failure = None;
                true
            }
            HomeState::Exiting(Exiting::Activating {
                id: active, portal, ..
            }) if active == id => {
                self.hide_hud(now, out);
                self.exit_resolved();
                self.exit_retry
                    .insert(portal, now.saturating_add(HOME_RETRY));
                true
            }
            HomeState::Exiting(Exiting::Cancelled { id: cancelled, .. }) if cancelled == id => {
                self.exit_resolved();
                true
            }
            _ => false,
        }
    }

    // ---- exit (§2.6) ----

    /// A press against a strip while home: show the HUD before any capture (04 §8 invariant 5).
    fn exit_press(
        &mut self,
        portal: PortalId,
        position: f64,
        at: MonoTime,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        let Phase::Controlling(c) = &self.phase else {
            return;
        };
        let Some(home) = c.home.filter(|h| matches!(h.state, HomeState::Home)) else {
            return;
        };
        let hud_display = c.hud_display;
        if self.entry_guarded(portal, at, now) || self.hud_blocked(now) {
            return;
        }
        if !self.permits_io()
            || !self.portals_installed
            || self
                .exit_retry
                .get(&portal)
                .is_some_and(|until| now < *until)
            || !self.portal_offered(home.projection, portal)
            || self.exit_target(&home, portal, position).is_none()
        {
            return;
        }
        self.show_hud(hud_display, home.peer, now, out);
        self.set_home_state(HomeState::Exiting(Exiting::Hud {
            portal,
            position,
            strips_gen: self.strips_gen,
            deadline: now.saturating_add(HUD_TIMEOUT),
        }));
    }

    /// WP-2.43j: a press on one of the entry's own strips made during the guard is the motion
    /// that carried the pointer in, not an exit; the strip then stays ignored until the pointer
    /// leaves it (or `ENTRY_HOLD` from the entry, by the current time). Every other strip, and
    /// every press made later on a strip that was left, exits.
    fn entry_guarded(&mut self, portal: PortalId, at: MonoTime, now: MonoTime) -> bool {
        let Some(i) = self.home_strip_edge(portal) else {
            return false;
        };
        let Phase::Controlling(c) = &mut self.phase else {
            return false;
        };
        let Some(home) = &mut c.home else {
            return false;
        };
        let Some(entered) = home.entered_at else {
            return false;
        };
        if !home.entry_near[i] {
            return false;
        }
        // The press is classified by when it happened, which a delayed delivery doesn't change;
        // the absolute limit runs on the current time.
        if now.saturating_duration_since(entered) >= ENTRY_HOLD {
            return false;
        }
        if at.saturating_duration_since(entered) < ENTRY_GUARD || home.entry_held[i] {
            home.entry_held[i] = true;
            return true;
        }
        false
    }

    /// WP-2.43j: the pointer left one of the entry's own strips: it counts again.
    fn entry_strip_released(&mut self, portal: PortalId) {
        let Some(i) = self.home_strip_edge(portal) else {
            return;
        };
        if let Phase::Controlling(c) = &mut self.phase
            && let Some(home) = &mut c.home
        {
            home.entry_held[i] = false;
        }
    }

    /// The `edge_index` of `portal` if it is one of the current home's own strips.
    fn home_strip_edge(&self, portal: PortalId) -> Option<usize> {
        let home = self.home_copy()?;
        self.strips_of(home.projection)
            .iter()
            .find(|s| s.id == portal)
            .map(|s| edge_index(s.edge) as usize)
    }

    /// WP-2.43j: the pointer left the strip whose exit HUD is waiting to become visible. The push
    /// is over: the HUD goes and home resumes, with no retry fence (nothing failed) and no
    /// capture (`begin` needs the pointer on the strip and would be refused).
    fn exit_strip_released(&mut self, portal: PortalId, now: MonoTime, out: &mut Vec<Output>) {
        if let Some(HomeState::Exiting(Exiting::Hud {
            portal: pressed, ..
        })) = self.home_state()
            && pressed == portal
        {
            self.hide_hud(now, out);
            self.set_home_state(HomeState::Home);
        }
    }

    /// The HUD is visible: only now may the exit capture begin.
    fn exit_hud_visible(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        let Some(home) = self.home_copy() else {
            return;
        };
        let HomeState::Exiting(Exiting::Hud {
            portal,
            position,
            strips_gen,
            deadline,
        }) = home.state
        else {
            return;
        };
        let id = self.next_capture.filter(|_| {
            now < deadline
                && strips_gen == self.strips_gen
                && self.portals_installed
                && self.portal_offered(home.projection, portal)
        });
        let Some(id) = id else {
            self.hide_hud(now, out);
            self.set_home_state(HomeState::Home);
            self.exit_retry
                .insert(portal, now.saturating_add(HOME_RETRY));
            return;
        };
        self.next_capture = id.checked_add(1);
        let id = CaptureId(id);
        out.push(Output::BeginCapture {
            id,
            portal,
            drain_first: true,
        });
        self.activation.clear();
        self.activation_overflow = false;
        self.capture_buttons.clear();
        // Nothing observed before this capture says what is held now: the snapshot replaces it
        // when the activation completes, and until then (a cancelled exit) only what the capture
        // itself reports counts.
        self.chord_keys.clear();
        if let Phase::Controlling(c) = &mut self.phase {
            c.capture = Capture { id, started: false };
        }
        self.set_home_state(HomeState::Exiting(Exiting::Activating {
            id,
            portal,
            position,
            strips_gen,
            deadline: now.saturating_add(START_TIMEOUT),
        }));
    }

    fn hud_unavailable(&mut self, now: MonoTime, out: &mut Vec<Output>) {
        match self.home_state() {
            Some(HomeState::Exiting(Exiting::Hud { portal, .. })) => {
                self.hide_hud(now, out);
                self.set_home_state(HomeState::Home);
                self.exit_retry
                    .insert(portal, now.saturating_add(HOME_RETRY));
            }
            // No HUD is shown while home.
            Some(HomeState::Home) => {}
            _ => self.return_home(EndReason::Released, None, false, true, now, out),
        }
    }

    /// `Input::CaptureBegun` for the exit capture or a cancelled one. True if it was consumed.
    fn exit_begun(
        &mut self,
        id: CaptureId,
        result: &Result<CaptureStart, Failure>,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) -> bool {
        let Some(home) = self.home_copy() else {
            return false;
        };
        match home.state {
            HomeState::Exiting(Exiting::Activating {
                id: active,
                portal,
                position,
                strips_gen,
                deadline,
            }) if active == id => {
                match result {
                    Ok(start) => {
                        self.exit_activated(
                            id, portal, position, strips_gen, deadline, start, now, out,
                        );
                    }
                    Err(failure) => {
                        // A rolled-back activation emits no `Ended`.
                        self.hide_hud(now, out);
                        self.exit_resolved();
                        self.activation.clear();
                        self.activation_overflow = false;
                        if *failure != Failure::Locked {
                            // A drag in W never leaks; the next press tries again later.
                            self.exit_retry
                                .insert(portal, now.saturating_add(HOME_RETRY));
                        }
                    }
                }
                true
            }
            HomeState::Exiting(Exiting::Cancelled { id: cancelled, .. }) if cancelled == id => {
                match result {
                    // Idempotent: no other capture can exist (the backend holds one at a time).
                    Ok(_) => out.push(Output::EndCapture { warp_to: None }),
                    // No `Ended` will come.
                    Err(_) => self.exit_resolved(),
                }
                true
            }
            _ => false,
        }
    }

    /// The exit capture is effective (§2.6 step 4, A4).
    #[allow(clippy::too_many_arguments)]
    fn exit_activated(
        &mut self,
        id: CaptureId,
        portal: PortalId,
        position: f64,
        strips_gen: u64,
        deadline: MonoTime,
        start: &CaptureStart,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        let Some(home) = self.home_copy() else {
            return;
        };
        // A4 steps 1 and 2, before anything can cancel the exit: the snapshot replaces the chord
        // state and the buffered transitions of the activation are applied on top of it in
        // order, so a cancellation (or a leave) below finds every modifier observed so far.
        let overflow = std::mem::take(&mut self.activation_overflow);
        if self.reconcile_chord(Some(&start.held_keys)) {
            // The release chord was pressed while the exit was activating: end everything.
            self.chord_press_outstanding = true;
            self.release(ReleaseCause::Chord, now, out);
            return;
        }
        // Checked on arrival, not only on `Tick`, as a crossing's is.
        if now >= deadline {
            self.cancel_exit(id, portal, now, out);
            return;
        }
        // The placement is the newest coherent one at this moment, never a snapshot. If there is
        // none (the proxy's placement is incoherent or its projection is gone), leave home with
        // the live capture (§2.7, `Returning`).
        let coherent = self
            .twin_home(home.peer, home.projection)
            .is_some_and(|twin| self.coherent(&twin).is_some());
        if !coherent {
            self.leave_home(Some(HomeFailure::Gone), now, out);
            return;
        }
        // A strip set that changed under the exit, or a strip that is gone, cancels it.
        let target = if strips_gen == self.strips_gen {
            self.exit_target(&home, portal, position)
        } else {
            None
        };
        let Some(target) = target else {
            self.cancel_exit(id, portal, now, out);
            return;
        };
        // A4 step 3: a button still down (or an activation that can't be replayed) cancels the
        // exit: the pointer stays home and the button's native up reaches the window unpaired,
        // which is harmless.
        if overflow || !self.capture_buttons.is_empty() {
            self.cancel_exit_with_warp(id, portal, position, now, out);
            return;
        }
        self.complete_exit(id, home, target, start, now, out);
    }

    /// Reconcile the chord state with what the exit capture has reported: with a snapshot (the
    /// keys held when `begin` started) it replaces the state first; the key transitions buffered
    /// during the activation are then applied in order. True if one of them completed the
    /// release chord. The buffer is consumed.
    fn reconcile_chord(&mut self, snapshot: Option<&[HidUsage]>) -> bool {
        if let Some(held) = snapshot {
            self.chord_keys = held.iter().copied().collect();
        }
        let mut chord = false;
        for (item, down) in std::mem::take(&mut self.activation) {
            if let Held::Key(usage) = item {
                if down {
                    self.chord_keys.insert(usage);
                    chord |= self.chord_completed(usage);
                } else {
                    self.chord_keys.remove(&usage);
                }
            }
        }
        chord
    }

    /// §2.6 step 4 `Ok`: the session resumes at the proxy's edge.
    #[allow(clippy::too_many_arguments)]
    fn complete_exit(
        &mut self,
        id: CaptureId,
        home: Home,
        target: ExitTarget,
        start: &CaptureStart,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        if !self.ensure_sequence_room(now, out) {
            return;
        }
        if target.display.node == self.config.node {
            let Some(info) = self
                .displays
                .get(&self.config.node)
                .and_then(|displays| displays.iter().find(|d| d.id == target.display.display))
                .filter(|_| {
                    self.layout
                        .as_ref()
                        .is_some_and(|layout| layout.get(target.display).is_some())
                })
            else {
                self.leave_home(Some(HomeFailure::Gone), now, out);
                return;
            };
            let point = info.geometry.clamp_device(target.point);
            // Use the same reverse-connection hysteresis as an ordinary remote edge return.
            self.reentry = self
                .confirmed_portals
                .as_ref()
                .and_then(|map| map.layout.as_ref())
                .and_then(|layout| {
                    let returned = target.portal?;
                    layout
                        .portals()
                        .iter()
                        .find(|p| p.from == returned.to && p.to == returned.from)
                })
                .map(|p| (p.from, p.to, p.edge, now.saturating_add(REENTRY_GUARD)));
            self.home_fence = Some((home.projection, now.saturating_add(REENTRY_GUARD)));
            self.lock_keys = start.lock_keys;
            self.return_home(
                EndReason::Released,
                Some((target.display.display, point)),
                false,
                true,
                now,
                out,
            );
            return;
        }
        let third_node = target.display.node != home.peer;
        let (display, point) = if third_node {
            // Until A1 confirms the removal, resume only on the old host's edge.
            (target.host, target.host_point)
        } else {
            (target.display, target.point)
        };
        let tracker = self
            .layout
            .as_ref()
            .and_then(|layout| PointerTracker::new(layout, display, point));
        let Some(tracker) = tracker else {
            self.leave_home(Some(HomeFailure::Gone), now, out);
            return;
        };
        self.lock_keys = start.lock_keys;
        self.accelerator = Accelerator::new(self.config.accel);
        let Phase::Controlling(c) = &mut self.phase else {
            return;
        };
        c.tracker = tracker;
        c.from_twin = true;
        c.home = None;
        c.last_motion = None;
        // The peer's cursor leaves the proxy at once.
        let seq = c.session.motion_seq;
        c.session.motion_seq = seq.saturating_add(1);
        out.push(Output::SendMotion {
            peer: c.session.peer,
            msg: PointerMessage {
                session: c.session.id,
                seq,
                display: display.display,
                position: point,
            },
        });
        // Caps may have changed in the window.
        let (session, seq) = c.session.next_input(now);
        out.push(Output::SendInput {
            peer: c.session.peer,
            msg: InputMessage::LockKeys {
                session,
                seq,
                keys: start.lock_keys,
            },
        });
        // The exit capture is now the session's live capture: a repeat of its success is not a
        // new capture to end.
        self.committed_exit = Some(id);
        // A1: the bind goes through its removal phase; E2's filter stays on until it is confirmed.
        if home.bind {
            self.start_teardown(home.peer, home.projection, now, out);
        }
        self.home_fence = Some((home.projection, now.saturating_add(REENTRY_GUARD)));
        out.push(Output::Notice(Notice::Home {
            key: self.key_of(home.projection),
            entered: false,
        }));
        if third_node {
            let continuation = FlushHandoff {
                capture: id,
                target,
                fallback: home.fallback,
            };
            if let Some(teardown) = &mut self.teardown {
                teardown.continuation = Some(continuation);
            } else {
                self.finish_flush_handoff(continuation, true, now, out);
            }
        }
    }

    fn finish_flush_handoff(
        &mut self,
        continuation: FlushHandoff,
        removed: bool,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        let Phase::Controlling(c) = &self.phase else {
            return;
        };
        if c.capture.id != continuation.capture || c.home.is_some() {
            return;
        }
        let target = continuation.target;
        if removed
            && self.armed
            && self.permits_io()
            && self.router.no_buttons_held()
            && self.capture_buttons.is_empty()
            && c.tracker.position() == (target.host, target.host_point)
            && target.portal.is_some_and(|p| {
                self.flush_target(target.host, p.edge, target.host_point) == Some(target)
            })
        {
            if let Some(portal) = target.portal {
                self.switch_target(portal.id, (target.display, target.point), now, out);
            }
        } else {
            // Removal failure/timeout, changed mapping or motion cancels this continuation.
            // A1 still owns any unconfirmed removal; returning never lifts its input fence.
            self.return_home(
                EndReason::Released,
                Some(continuation.fallback),
                false,
                true,
                now,
                out,
            );
        }
    }

    /// §2.5 "Mapping" with the current placement.
    fn exit_target(&self, home: &Home, portal: PortalId, position: f64) -> Option<ExitTarget> {
        let twin = self.twin_home(home.peer, home.projection)?;
        let (placement, id, current_geometry) = self.coherent(&twin)?;
        let strip = self.twin_strips.iter().find(|s| s.id == portal)?;
        // Classify against current geometry first. A host resize can make the same strip a
        // non-flush exit while its replacement portal confirmation is still pending.
        if offerable(&placement, &current_geometry, strip.edge) {
            let point = exit_point(&placement, &current_geometry, strip.edge, position);
            return Some(ExitTarget {
                host: id,
                host_point: point,
                display: id,
                point,
                portal: None,
            });
        }
        let geometry = self
            .confirmed_portals
            .as_ref()?
            .layout
            .as_ref()?
            .get(id)?
            .geometry;
        if offerable(&placement, &geometry, strip.edge) {
            return None;
        }
        let point = exit_point(&placement, &geometry, strip.edge, position);
        self.flush_target(id, strip.edge, point)
    }

    /// Where on the twin the strip was pressed, one pixel inside the content.
    fn strip_point(
        &self,
        home: &Home,
        portal: PortalId,
        position: f64,
    ) -> Option<(DisplayId, PointDevice)> {
        let twin = self.twin_home(home.peer, home.projection)?;
        let strip = self.twin_strips.iter().find(|s| s.id == portal)?;
        let content = twin.content;
        let (w, h) = (content.max.x - content.min.x, content.max.y - content.min.y);
        if w <= 0 || h <= 0 {
            return None;
        }
        let t = if position.is_nan() {
            0.0
        } else {
            position.clamp(0.0, 1.0)
        };
        let (dx, dy) = (f64::from(w - 1), f64::from(h - 1));
        let (x, y) = match strip.edge {
            Edge::Left => (0.0, t * dy),
            Edge::Right => (dx, t * dy),
            Edge::Top => (t * dx, 0.0),
            Edge::Bottom => (t * dx, dy),
        };
        Some((
            twin.display,
            PointDevice::new(f64::from(content.min.x) + x, f64::from(content.min.y) + y),
        ))
    }

    /// Cancel with capture (§2.6 step 4): end the capture without moving the pointer (it stays on
    /// the twin, which is home) and serialise any further exit behind its end.
    fn cancel_exit(
        &mut self,
        id: CaptureId,
        portal: PortalId,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        // What the capture reported during the activation is not lost with the exit: the
        // modifiers already pressed count toward the chord while the cancellation is outstanding
        // (and a chord already completed ends everything).
        if self.reconcile_chord(None) {
            self.chord_press_outstanding = true;
            self.release(ReleaseCause::Chord, now, out);
            return;
        }
        out.push(Output::EndCapture { warp_to: None });
        self.hide_hud(now, out);
        self.begin_cancelled(id, portal, now);
    }

    /// A4 step 3: the exit is cancelled because a button is held: the capture is released with a
    /// warp to the strip's position on the twin (the pointer stays home).
    fn cancel_exit_with_warp(
        &mut self,
        id: CaptureId,
        portal: PortalId,
        position: f64,
        now: MonoTime,
        out: &mut Vec<Output>,
    ) {
        let target = self.home_copy().and_then(|home| {
            self.strip_point(&home, portal, position)
                .or(Some(home.fallback))
        });
        if let Some(target) = target {
            self.release_and_warp(WarpPurpose::Cancel, target, now, out);
        } else {
            out.push(Output::EndCapture { warp_to: None });
        }
        self.hide_hud(now, out);
        self.begin_cancelled(id, portal, now);
    }

    fn begin_cancelled(&mut self, id: CaptureId, portal: PortalId, now: MonoTime) {
        self.activation.clear();
        self.activation_overflow = false;
        self.set_home_state(HomeState::Exiting(Exiting::Cancelled {
            id,
            deadline: now.saturating_add(END_TIMEOUT),
        }));
        self.exit_retry
            .insert(portal, now.saturating_add(HOME_RETRY));
    }

    /// An exit is over without a crossing: home again, with nothing held.
    fn exit_resolved(&mut self) {
        // The button's native up reaches the window, unseen: nothing about it is remembered.
        self.capture_buttons.clear();
        self.set_home_state(HomeState::Home);
    }
}

#[cfg(test)]
mod tests {
    //! The exit mapping of WP-2.43 §2.5 (pure functions): where a press on a twin strip lands on
    //! the peer's display, which edges are offered, and the strips' spans.

    use crosspane_types::geom::{PixelSize, PointLogical, SizeMm};
    use proptest::prelude::*;

    use super::*;

    const W: u32 = 1000;
    const H: u32 = 800;

    fn geometry() -> DisplayGeometry {
        DisplayGeometry {
            physical_size: SizeMm::new(f64::from(W) / 10.0, f64::from(H) / 10.0),
            pixel_size: PixelSize::new(W, H),
            scale: 1.0,
            logical_origin: PointLogical::zero(),
        }
    }

    fn placement(x: f64, y: f64, w: u32, h: u32) -> Proxy {
        Proxy {
            generation: 1,
            display: DisplayId(1),
            origin: PointDevice::new(x, y),
            size: PixelSize::new(w, h),
        }
    }

    fn within_display(p: PointDevice) -> bool {
        p.x >= 0.0 && p.y >= 0.0 && p.x <= f64::from(W - 1) && p.y <= f64::from(H - 1)
    }

    proptest! {
        /// All four edges, placements that touch or exceed each side of the host display, and
        /// strips with padding on each side of the twin's content.
        #[test]
        fn exit_mapping_all_edges(
            x in -400.0..1400.0f64,
            y in -400.0..1200.0f64,
            w in 1u32..1400,
            h in 1u32..1000,
            t in -0.5..1.5f64,
            pad in (0i32..120, 0i32..120, 0i32..120, 0i32..120),
        ) {
            let pl = placement(x.round(), y.round(), w, h);
            let (ox, oy) = (pl.origin.x, pl.origin.y);
            let (fw, fh) = (f64::from(w), f64::from(h));
            let tc = t.clamp(0.0, 1.0);
            for edge in EDGES {
                let p = exit_point(&pl, &geometry(), edge, t);
                // Always on the host display.
                prop_assert!(within_display(p), "{edge:?} {p:?}");
                // Where nothing is clamped the formula is exact: just outside the proxy's
                // corresponding edge, at the fraction along it.
                let raw = match edge {
                    Edge::Left => PointDevice::new(ox - 1.0, oy + tc * fh),
                    Edge::Right => PointDevice::new(ox + fw, oy + tc * fh),
                    Edge::Top => PointDevice::new(ox + tc * fw, oy - 1.0),
                    Edge::Bottom => PointDevice::new(ox + tc * fw, oy + fh),
                };
                if within_display(raw) {
                    prop_assert_eq!(p, raw, "{:?}", edge);
                    prop_assert!(!inside(&pl, p), "{edge:?} outside the proxy: {p:?}");
                }
                // Monotonic along the edge.
                let q = exit_point(&pl, &geometry(), edge, (t + 0.1).min(1.5));
                match edge {
                    Edge::Left | Edge::Right => prop_assert!(q.y >= p.y),
                    Edge::Top | Edge::Bottom => prop_assert!(q.x >= p.x),
                }
                // A proxy with room on a side offers an exit there; one that touches or exceeds
                // the display's side offers none, provided its middle is on the display.
                let mid = exit_point(&pl, &geometry(), edge, 0.5);
                let outside_normal = match edge {
                    Edge::Left | Edge::Right => mid.x < ox || mid.x >= ox + fw,
                    Edge::Top | Edge::Bottom => mid.y < oy || mid.y >= oy + fh,
                };
                prop_assert_eq!(offerable(&pl, &geometry(), edge), outside_normal);
                // (a proxy that is entirely beyond the display is on no side of it: it can't be
                // placed there, and the rule says nothing about it)
                let (room, touches, along_on_display) = match edge {
                    Edge::Left => (
                        ox >= 1.0,
                        ox <= 0.0 && ox + fw > 0.0,
                        (0.0..f64::from(H)).contains(&(oy + fh / 2.0)),
                    ),
                    Edge::Right => (
                        ox + fw <= f64::from(W) - 1.0,
                        ox + fw >= f64::from(W) && ox < f64::from(W),
                        (0.0..f64::from(H)).contains(&(oy + fh / 2.0)),
                    ),
                    Edge::Top => (
                        oy >= 1.0,
                        oy <= 0.0 && oy + fh > 0.0,
                        (0.0..f64::from(W)).contains(&(ox + fw / 2.0)),
                    ),
                    Edge::Bottom => (
                        oy + fh <= f64::from(H) - 1.0,
                        oy + fh >= f64::from(H) && oy < f64::from(H),
                        (0.0..f64::from(W)).contains(&(ox + fw / 2.0)),
                    ),
                };
                if room && along_on_display {
                    prop_assert!(offerable(&pl, &geometry(), edge), "{edge:?} room");
                }
                if touches && along_on_display {
                    prop_assert!(!offerable(&pl, &geometry(), edge), "{edge:?} touches");
                }
            }
            // Strips span the content's extent along the edge, whatever the padding around it
            // (the bar's reserved area on any side of the twin output).
            let (l, tp, r, b) = pad;
            let content = PixelRect::new(
                crosspane_types::geom::euclid::Point2D::new(l, tp),
                crosspane_types::geom::euclid::Point2D::new(l + w as i32, tp + h as i32),
            );
            let _ = (r, b);
            for edge in [Edge::Left, Edge::Right] {
                prop_assert_eq!(strip_span(content, edge), (f64::from(tp), f64::from(tp + h as i32)));
            }
            for edge in [Edge::Top, Edge::Bottom] {
                prop_assert_eq!(strip_span(content, edge), (f64::from(l), f64::from(l + w as i32)));
            }
        }

        #[test]
        fn exit_mapping_ignores_nan(x in 0.0..800.0f64, y in 0.0..600.0f64) {
            let pl = placement(x.round(), y.round(), 100, 100);
            for edge in EDGES {
                let nan = exit_point(&pl, &geometry(), edge, f64::NAN);
                prop_assert_eq!(nan, exit_point(&pl, &geometry(), edge, 0.0));
            }
        }
    }

    #[test]
    fn twin_portal_ids_are_disjoint_per_slot_and_edge() {
        let mut seen = BTreeSet::new();
        for slot in 0..64u32 {
            for edge in EDGES {
                let id = TWIN_PORTAL_BASE + 4 * slot + edge_index(edge);
                assert!(id >= TWIN_PORTAL_BASE);
                assert!(seen.insert(id), "slot {slot} {edge:?}");
            }
        }
        // The largest slot that offers strips still fits in a portal id.
        assert!(
            u64::from(TWIN_PORTAL_BASE) + 4 * u64::from(MAX_TWIN_SLOT - 1) + 3
                <= u64::from(u32::MAX)
        );
    }

    #[test]
    fn teardown_backoff_doubles_to_the_cap() {
        let steps: Vec<_> = (0..9).map(|attempt| backoff(attempt).as_millis()).collect();
        assert_eq!(steps, [100, 200, 400, 800, 1600, 2000, 2000, 2000, 2000]);
    }
}

#[cfg(test)]
mod entry_recovery_tests {
    use super::*;
    use crosspane_input::layout::Placed;
    use crosspane_types::geom::{PixelSize, PointLogical, PointMm, SizeMm};

    const FALLBACK: (DisplayId, PointDevice) = (DisplayId(1), PointDevice::new(500.0, 500.0));

    fn releasing() -> ControllerE1 {
        let node = NodeId([1; 32]);
        let peer = NodeId([2; 32]);
        let config = EngineConfig::new(node);
        let mut controller = ControllerE1::new(&config, MonoTime::ZERO);
        controller.state = SessionState {
            lock: LockState::Unlocked,
            active: Some(true),
        };
        let host = GlobalDisplayId {
            node: peer,
            display: DisplayId(1),
        };
        let layout = Layout::new(
            vec![Placed {
                id: host,
                geometry: DisplayGeometry {
                    physical_size: SizeMm::new(100.0, 100.0),
                    pixel_size: PixelSize::new(1000, 1000),
                    scale: 1.0,
                    logical_origin: PointLogical::zero(),
                },
                origin: PointMm::zero(),
            }],
            config.layout,
        )
        .unwrap();
        controller.phase = Phase::Controlling(Control {
            session: Session {
                peer,
                id: SessionId(1),
                input_seq: 1,
                motion_seq: 1,
                lease: ControllerLease::new(MonoTime::ZERO),
            },
            capture: Capture {
                id: CaptureId(1),
                started: true,
            },
            tracker: PointerTracker::new(&layout, host, PointDevice::zero()).unwrap(),
            hud_display: DisplayId(1),
            home: Some(Home {
                op: HomeOp(1),
                peer,
                projection: ProjectionId(1),
                window: WindowId(1),
                ended: CaptureId(1),
                ended_seen: false,
                fallback: FALLBACK,
                bind: true,
                display: DisplayId(1),
                generation: 1,
                strips_gen: 1,
                state: HomeState::Entering(Entering::Releasing {
                    deadline: MonoTime::from_nanos(1_000_000_000),
                }),
                entry_near: [false; 4],
                entry_held: [false; 4],
                entered_at: None,
            }),
            from_twin: false,
            last_motion: None,
        });
        controller
    }

    #[test]
    fn flush_handoff_ignores_superseded_capture_or_reentered_home() {
        for reentered in [false, true] {
            let mut controller = releasing();
            let Phase::Controlling(c) = &mut controller.phase else {
                unreachable!();
            };
            if !reentered {
                c.home = None;
            }
            let (host, host_point) = c.tracker.position();
            let continuation = FlushHandoff {
                capture: CaptureId(if reentered { 1 } else { 2 }),
                target: ExitTarget {
                    host,
                    host_point,
                    display: GlobalDisplayId {
                        node: NodeId([3; 32]),
                        display: DisplayId(1),
                    },
                    point: PointDevice::zero(),
                    portal: None,
                },
                fallback: FALLBACK,
            };
            let mut out = Vec::new();
            controller.finish_flush_handoff(continuation, true, MonoTime::ZERO, &mut out);
            assert!(out.is_empty());
            let Phase::Controlling(c) = &controller.phase else {
                panic!("a stale continuation cannot end the current capture/home");
            };
            assert_eq!(c.capture.id, CaptureId(1));
            assert_eq!(c.home.is_some(), reentered);
        }
    }

    #[test]
    fn skipped_entry_with_an_open_gate_emits_an_immediate_stranded_retry() {
        let mut controller = releasing();
        let mut out = Vec::new();
        controller.entry_released(HomeOp(1), &Ok(Warp::Skipped), MonoTime::ZERO, &mut out);
        assert!(
            out.iter().any(
                |o| matches!(o, Output::ReleaseAndWarp { warp_to, .. } if *warp_to == FALLBACK)
            )
        );
        assert!(
            controller
                .warps
                .iter()
                .any(|w| w.purpose == WarpPurpose::Retry && w.target == FALLBACK)
        );
        assert_eq!(controller.stranded.unwrap().attempts, 1);
    }

    #[test]
    fn skipped_entry_with_a_closed_gate_waits_for_unlock_without_spending_a_retry() {
        let mut controller = releasing();
        controller.state.lock = LockState::Locked;
        let mut out = Vec::new();
        controller.entry_released(HomeOp(1), &Ok(Warp::Skipped), MonoTime::ZERO, &mut out);
        assert!(
            !out.iter()
                .any(|o| matches!(o, Output::ReleaseAndWarp { .. }))
        );
        assert_eq!(controller.stranded.unwrap().attempts, 0);
        controller.state.lock = LockState::Unlocked;
        controller.stranded_retry(MonoTime::from_nanos(2_000_000_000), &mut out);
        assert!(
            out.iter().any(
                |o| matches!(o, Output::ReleaseAndWarp { warp_to, .. } if *warp_to == FALLBACK)
            )
        );
    }

    #[test]
    fn entry_error_with_an_open_gate_retries_immediately_even_if_capture_may_be_live() {
        let mut controller = releasing();
        let mut out = Vec::new();
        controller.entry_released(HomeOp(1), &Err(Failure::Other), MonoTime::ZERO, &mut out);
        assert!(matches!(controller.phase, Phase::Returning { .. }));
        assert!(
            out.iter().any(
                |o| matches!(o, Output::ReleaseAndWarp { warp_to, .. } if *warp_to == FALLBACK)
            )
        );
        assert!(
            controller
                .warps
                .iter()
                .any(|w| w.purpose == WarpPurpose::Retry)
        );
    }
}

#[cfg(test)]
mod release_cause_tests {
    //! WP-4.5: `Notice::ControlReleased` names the way a controller session was released (the
    //! chord, or the command), once per ended session, and says nothing for a release with no
    //! session or for any other way a session ends.

    #![allow(clippy::unwrap_used)]

    use crosspane_platform::{
        CaptureEvent, CaptureId, CaptureStart, HotkeyEvent, LockState, OverlayEvent, PortalId,
        SessionEvent, SessionState,
    };
    use crosspane_protocol::msg::{ControlMessage, EndReason as WireEnd, Placement};
    use crosspane_types::color::ColorSpace;
    use crosspane_types::geom::{PixelSize, PointLogical, PointMm, SizeMm};

    use super::*;

    const A: NodeId = NodeId([1; 32]);
    const B: NodeId = NodeId([2; 32]);

    fn ms(n: u64) -> MonoTime {
        MonoTime::from_nanos(n * 1_000_000)
    }

    fn display() -> DisplayInfo {
        DisplayInfo {
            id: DisplayId(1),
            name: "test".into(),
            geometry: DisplayGeometry {
                physical_size: SizeMm::new(100.0, 100.0),
                pixel_size: PixelSize::new(1000, 1000),
                scale: 1.0,
                logical_origin: PointLogical::zero(),
            },
            refresh_millihz: 60_000,
            color_space: ColorSpace::Srgb,
            hdr: false,
        }
    }

    /// A controller on node A with peer B to its right, up, and the session permitted.
    struct Rig {
        controller: ControllerE1,
        portal: PortalId,
        now: MonoTime,
        chord: Vec<HidUsage>,
    }

    impl Rig {
        fn new() -> Rig {
            let mut config = EngineConfig::new(A);
            config.accel.base_mm_per_unit = 0.1;
            config.accel.max_gain = 1.0;
            let mut chord = config.release_chord.modifiers.clone();
            chord.push(config.release_chord.key);
            let mut controller = ControllerE1::new(&config, ms(0));
            let mut setup = Vec::new();
            let mut placements = Vec::new();
            let mut placed = Vec::new();
            for (index, node) in [A, B].into_iter().enumerate() {
                let info = display();
                let origin = PointMm::new(index as f64 * 100.0, 0.0);
                placements.push(Placement {
                    node,
                    display: info.id,
                    origin,
                    version: 1,
                });
                placed.push(Placed {
                    id: GlobalDisplayId {
                        node,
                        display: info.id,
                    },
                    geometry: info.geometry,
                    origin,
                });
                let input = if node == A {
                    Input::LocalDisplays(vec![info])
                } else {
                    Input::PeerDisplays {
                        peer: node,
                        displays: vec![info],
                    }
                };
                controller.handle(&input, ms(0), &mut setup);
            }
            let layout = Layout::new(placed, config.layout).unwrap();
            let portal = layout
                .portals()
                .iter()
                .find(|p| p.from.node == A && p.to.node == B)
                .unwrap()
                .id;
            controller.handle(&Input::Layout(placements), ms(0), &mut setup);
            controller.handle(
                &Input::Session(SessionEvent::State(SessionState {
                    lock: LockState::Unlocked,
                    active: Some(true),
                })),
                ms(0),
                &mut setup,
            );
            controller.handle(&Input::PeerUp { peer: B }, ms(0), &mut setup);
            Rig {
                controller,
                portal,
                now: ms(0),
                chord,
            }
        }

        fn send(&mut self, input: Input) -> Vec<Output> {
            let mut out = Vec::new();
            self.controller.handle(&input, self.now, &mut out);
            out
        }

        /// The pointer pushes B's edge and the HUD is up: crossing, no session yet.
        fn crossing(&mut self) {
            let out = self.send(Input::Capture(CaptureEvent::EdgePressed {
                portal: self.portal,
                position: 0.5,
                at: self.now,
            }));
            assert!(
                out.iter()
                    .any(|o| matches!(o, Output::ShowOverlay { id, .. } if *id == HUD))
            );
            assert_eq!(self.controller.target(), None);
        }

        /// A full crossing: B acknowledges and the capture is live.
        fn controlling(&mut self) -> SessionId {
            self.crossing();
            let out = self.send(Input::Overlay(OverlayEvent::Visible(HUD)));
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
            let out = self.send(Input::Link(LinkEvent::Control {
                peer: B,
                msg: ControlMessage::ControlStarted { session },
            }));
            let capture: CaptureId = out
                .iter()
                .find_map(|o| match o {
                    Output::BeginCapture { id, .. } => Some(*id),
                    _ => None,
                })
                .unwrap();
            self.send(Input::Capture(CaptureEvent::Started { id: capture }));
            self.send(Input::CaptureBegun {
                id: capture,
                result: Ok(CaptureStart {
                    held_keys: Vec::new(),
                    lock_keys: LockKeys::default(),
                }),
            });
            assert_eq!(self.controller.target(), Some(B));
            session
        }

        fn press_chord(&mut self) -> Vec<Output> {
            let mut out = Vec::new();
            for usage in self.chord.clone() {
                out.extend(self.send(Input::Capture(CaptureEvent::Key {
                    usage,
                    down: true,
                    at: self.now,
                })));
            }
            out
        }
    }

    fn released(out: &[Output]) -> Vec<(NodeId, ReleaseCause)> {
        out.iter()
            .filter_map(|o| match o {
                Output::Notice(Notice::ControlReleased { peer, cause }) => Some((*peer, *cause)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_release_command_says_command_once() {
        let mut rig = Rig::new();
        rig.controlling();
        let out = rig.send(Input::Command(Command::ReleaseControl));
        assert_eq!(released(&out), [(B, ReleaseCause::Command)]);
        assert_eq!(rig.controller.target(), None);
        // The session is over: a second release has none to end.
        let again = rig.send(Input::Command(Command::ReleaseControl));
        assert!(released(&again).is_empty());
    }

    #[test]
    fn the_captured_chord_says_chord_once() {
        let mut rig = Rig::new();
        rig.controlling();
        let out = rig.press_chord();
        assert_eq!(released(&out), [(B, ReleaseCause::Chord)]);
        assert_eq!(rig.controller.target(), None);
        // The platform's own report of the same chord arrives after it: nothing left to release
        // (and crossing is disarmed), so there is no second notice.
        let pair = rig.send(Input::Hotkey(HotkeyEvent::Pressed { at: rig.now }));
        assert!(released(&pair).is_empty());
    }

    #[test]
    fn the_hotkey_says_chord() {
        let mut rig = Rig::new();
        rig.controlling();
        let out = rig.send(Input::Hotkey(HotkeyEvent::Pressed { at: rig.now }));
        assert_eq!(released(&out), [(B, ReleaseCause::Chord)]);
        assert_eq!(rig.controller.target(), None);
    }

    #[test]
    fn a_release_while_crossing_with_a_session_still_counts_once() {
        // The handshake was sent (a session exists) but B hasn't acknowledged yet.
        let mut rig = Rig::new();
        rig.crossing();
        let out = rig.send(Input::Overlay(OverlayEvent::Visible(HUD)));
        assert!(out.iter().any(|o| matches!(
            o,
            Output::SendControl {
                msg: ControlMessage::StartControl { .. },
                ..
            }
        )));
        assert_eq!(rig.controller.target(), Some(B));
        let out = rig.send(Input::Command(Command::ReleaseControl));
        assert_eq!(released(&out), [(B, ReleaseCause::Command)]);
    }

    #[test]
    fn a_release_with_no_session_says_nothing() {
        // Idle: the hotkey releases (and disarms) but there is nothing to end.
        let mut rig = Rig::new();
        let out = rig.send(Input::Hotkey(HotkeyEvent::Pressed { at: rig.now }));
        assert!(released(&out).is_empty());
        // Crossing, HUD not yet acknowledged: no session has started either.
        let mut rig = Rig::new();
        rig.crossing();
        let out = rig.send(Input::Command(Command::ReleaseControl));
        assert!(released(&out).is_empty());
        assert_eq!(rig.controller.target(), None);
    }

    #[test]
    fn the_other_ways_a_session_ends_are_not_releases() {
        // The target ends it.
        let mut rig = Rig::new();
        let session = rig.controlling();
        let out = rig.send(Input::Link(LinkEvent::Control {
            peer: B,
            msg: ControlMessage::EndControl {
                session,
                reason: WireEnd::Released,
            },
        }));
        assert!(released(&out).is_empty());
        assert_eq!(rig.controller.target(), None);
        // The link drops.
        let mut rig = Rig::new();
        rig.controlling();
        let out = rig.send(Input::Link(LinkEvent::Closed {
            peer: B,
            error: crosspane_protocol::link::LinkError::Closed,
        }));
        assert!(released(&out).is_empty());
        // Panic.
        let mut rig = Rig::new();
        rig.controlling();
        let out = rig.send(Input::Command(Command::Panic));
        assert!(released(&out).is_empty());
        assert_eq!(rig.controller.target(), None);
    }
}
