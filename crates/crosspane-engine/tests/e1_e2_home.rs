//! WP-2.43b: the engine's "home on the twin" state machine (docs/wp/WP-2.43.md §2, amendments
//! A1-A9 and B1), driven through `Engine` so the E1 controller, the E1 target and both E2 roles
//! compose. Every test is deterministic: time is an explicit `MonoTime`, nothing sleeps.
//!
//! The scenario: node A (the node under test) controls peer B with E1 and projects its windows to
//! B with E2. A's window `W1` is twin-parked on A's twin display `TWIN`; B's proxy for it sits at
//! `origin` on B's display, 400x300 pixels. B reports the placement; A's pointer model is moved
//! into the proxy and B reports motion over it: that is the trigger of an entry.
//!
//! Every test asserts the invariants of 04 §8 at its end through [`H::quiet`]: every key and
//! button down has exactly one up on the node that received it (this node's injectors, and the
//! peer through the E1 session), and both journals are empty.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crosspane_engine::io::{HUD, HomeFailure, HomeOp, PortalsFailure, Warp};
use crosspane_engine::{
    Command, Engine, EngineConfig, Failure, InjectCmd, InjectId, Input, Notice, Output,
    ProjectionKey, ProxyEvent,
};
use crosspane_input::Held;
use crosspane_input::journal::{Journal, JournalError};
use crosspane_input::layout::{Layout, Placed};
use crosspane_platform::{
    CaptureEvent, CaptureId, CapturePortal, CaptureStart, Edge, EndReason as CaptureEnd, LockState,
    MotionKind, OverlayEvent, Parked, ParkingKind as PlatformParking, PortalId, SessionEvent,
    SessionState, StreamId, WindowEvent, WindowInfo, WindowRole, WindowState,
};
use crosspane_protocol::link::{LinkError, LinkEvent};
use crosspane_protocol::msg::{
    Capability, ControlMessage, EndReason, InputMessage, Placement, PointerMessage, Refusal,
    TargetStatus,
};
use crosspane_protocol::projection::{ProjInput, ProjectionMessage as Message};
use crosspane_types::color::ColorSpace;
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::euclid::Point2D;
use crosspane_types::geom::{
    DisplayGeometry, PixelRect, PixelSize, PointDevice, PointLogical, PointMm, RectLogical,
    SizeLogical, SizeMm,
};
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::id::{DisplayId, GlobalDisplayId, NodeId, ProjectionId, SessionId, WindowId};
use crosspane_types::input::{LockKeys, ScrollDelta, ScrollPhase};
use crosspane_types::time::MonoTime;

const A: NodeId = NodeId([1; 32]);
const B: NodeId = NodeId([2; 32]);
const C: NodeId = NodeId([3; 32]);
/// A's physical display, and B's display the proxy is on (1000x1000 pixels, 10 pixels per mm).
const LOCAL: DisplayId = DisplayId(1);
const REMOTE: DisplayId = DisplayId(1);
/// A's twin displays (not in the layout: the agent filters them out of `LocalDisplays`).
const TWIN: DisplayId = DisplayId(7);
const TWIN2: DisplayId = DisplayId(8);
const W1: WindowId = WindowId(10);
const W2: WindowId = WindowId(11);
const W3: WindowId = WindowId(12);
const OTHER: WindowId = WindowId(99);
const P1: ProjectionId = ProjectionId(1);
const P2: ProjectionId = ProjectionId(2);
const KEY: HidUsage = HidUsage::keyboard(4);
const KEY2: HidUsage = HidUsage::keyboard(5);
const LCTRL: HidUsage = HidUsage::keyboard(0xE0);
const LSHIFT: HidUsage = HidUsage::keyboard(0xE1);
const LALT: HidUsage = HidUsage::keyboard(0xE2);
const ESC: HidUsage = HidUsage::keyboard(0x29);
const BUTTON: MouseButton = MouseButton(1);
const OPEN: SessionState = SessionState {
    lock: LockState::Unlocked,
    active: Some(true),
};

// The constants of the design (docs/wp/WP-2.43.md §4), in milliseconds.
const DRAIN_TIMEOUT: u64 = 500;
const BIND_TIMEOUT: u64 = 1_000;
const FOCUS_TIMEOUT: u64 = 500;
const END_TIMEOUT: u64 = 300;
const HUD_TIMEOUT: u64 = 500;
const START_TIMEOUT: u64 = 1_000;
const HOME_RETRY: u64 = 1_000;
/// WP-2.43j: the HUD's quarantine after an abandoned or unavailable show.
const HUD_STALE: u64 = 1_000;
const REENTRY_GUARD: u64 = 150;
const LOCAL_MOTION_AGE: u64 = 500;
const PORTALS_RETRY: u64 = 500;
const STRANDED_RETRY: u64 = 1_000;
const CAPTURE_END_DEADLINE: u64 = 1_000;
const TWIN_PORTAL_BASE: u32 = 1 << 30;

fn ms(n: u64) -> MonoTime {
    MonoTime::from_nanos(n * 1_000_000)
}

fn display(id: u32) -> DisplayInfo {
    DisplayInfo {
        id: DisplayId(id),
        name: format!("test-{id}"),
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

fn rect(x0: i32, y0: i32, x1: i32, y1: i32) -> PixelRect {
    PixelRect::new(Point2D::new(x0, y0), Point2D::new(x1, y1))
}

fn point(x: f64, y: f64) -> PointDevice {
    PointDevice::new(x, y)
}

fn window(id: WindowId) -> WindowInfo {
    WindowInfo {
        id,
        title: "fixture".into(),
        app_id: "test".into(),
        pid: None,
        display: Some(LOCAL),
        frame: RectLogical::new(PointLogical::zero(), SizeLogical::new(320.0, 240.0)),
        state: WindowState::Normal,
        role: WindowRole::Toplevel,
        parent: None,
    }
}

fn parked(
    window: WindowId,
    kind: PlatformParking,
    display: DisplayId,
    content: PixelRect,
) -> Parked {
    Parked {
        window,
        kind,
        display,
        content,
    }
}

fn control(peer: NodeId, msg: ControlMessage) -> Input {
    Input::Link(LinkEvent::Control { peer, msg })
}

fn projection_msg(peer: NodeId, msg: Message) -> Input {
    control(peer, ControlMessage::Projection(msg))
}

fn proj_input(peer: NodeId, msg: ProjInput) -> Input {
    Input::Link(LinkEvent::Input {
        peer,
        msg: InputMessage::Proj(msg),
    })
}

/// B's acknowledgement of message `seq` of `session`.
fn proj_ack(session: SessionId, seq: u32) -> Input {
    Input::Link(LinkEvent::Input {
        peer: B,
        msg: InputMessage::Ack { session, seq },
    })
}

fn closed(peer: NodeId) -> Input {
    Input::Link(LinkEvent::Closed {
        peer,
        error: LinkError::Closed,
    })
}

fn locked() -> Input {
    Input::Session(SessionEvent::State(SessionState {
        lock: LockState::Locked,
        ..OPEN
    }))
}

fn unlocked() -> Input {
    Input::Session(SessionEvent::State(OPEN))
}

fn scroll_delta() -> ScrollDelta {
    ScrollDelta {
        pixels: None,
        v120_x: 0,
        v120_y: 120,
        phase: ScrollPhase::Discrete,
        stop_x: false,
        stop_y: false,
    }
}

// ---------------------------------------------------------------------------------------------
// Journals the tests can read back.
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Default)]
struct SharedJournal(Arc<Mutex<BTreeSet<Held>>>);

impl SharedJournal {
    fn with(items: &[Held]) -> Self {
        Self(Arc::new(Mutex::new(items.iter().copied().collect())))
    }
    fn items(&self) -> Vec<Held> {
        self.0.lock().unwrap().iter().copied().collect()
    }
}

impl Journal for SharedJournal {
    fn record_down(&mut self, item: Held) -> Result<(), JournalError> {
        self.0.lock().unwrap().insert(item);
        Ok(())
    }
    fn record_up(&mut self, item: Held) -> Result<(), JournalError> {
        self.0.lock().unwrap().remove(&item);
        Ok(())
    }
    fn held(&self) -> Result<Vec<Held>, JournalError> {
        Ok(self.items())
    }
}

/// The ledger's own retry delay for a release that has had no answer (and after a failure).
const LEDGER_RETRY: Duration = Duration::from_millis(50);

/// One request to release an item, and what the injector answered.
#[derive(Clone, Copy, Debug)]
struct UpAttempt {
    id: InjectId,
    at: MonoTime,
    answer: Option<bool>,
}

impl UpAttempt {
    /// A release may be asked for again only when its newest request has failed, or has had no
    /// answer for the ledger's retry delay.
    fn retry_due(&self, now: MonoTime) -> bool {
        match self.answer {
            Some(ok) => !ok,
            None => now.saturating_duration_since(self.at) >= LEDGER_RETRY,
        }
    }
}

/// The ordered history of one key or button on this node's injectors.
#[derive(Clone, Debug, Default)]
struct ItemTrace {
    downs: u32,
    /// Releases the injector confirmed.
    confirmed: u32,
    /// Presses not yet matched by a confirmed release.
    pending: u32,
    /// One chain per logical release in flight (never more than `pending`): the requests made for
    /// that press since its last confirmed release, oldest first. A request that finds a press
    /// without a chain starts one; any other is a retry, which must be due against the *newest*
    /// request of some chain, not any earlier one.
    chains: Vec<Vec<UpAttempt>>,
}

// ---------------------------------------------------------------------------------------------
// Output queries.
// ---------------------------------------------------------------------------------------------

fn injects(out: &[Output]) -> Vec<(InjectId, InjectCmd)> {
    out.iter()
        .filter_map(|o| match o {
            Output::Inject { id, cmd } => Some((*id, cmd.clone())),
            _ => None,
        })
        .collect()
}

fn has_inject(out: &[Output]) -> bool {
    out.iter().any(|o| matches!(o, Output::Inject { .. }))
}

fn binds(out: &[Output]) -> Vec<(HomeOp, bool)> {
    out.iter()
        .filter_map(|o| match o {
            Output::HomeBind { op, install } => Some((*op, *install)),
            _ => None,
        })
        .collect()
}

fn bind(out: &[Output], install: bool) -> Option<HomeOp> {
    binds(out)
        .into_iter()
        .find(|(_, i)| *i == install)
        .map(|(op, _)| op)
}

fn warps(out: &[Output]) -> Vec<(HomeOp, (DisplayId, PointDevice))> {
    out.iter()
        .filter_map(|o| match o {
            Output::ReleaseAndWarp { op, warp_to } => Some((*op, *warp_to)),
            _ => None,
        })
        .collect()
}

fn warp(out: &[Output]) -> Option<(HomeOp, (DisplayId, PointDevice))> {
    warps(out).into_iter().next()
}

fn notices(out: &[Output]) -> Vec<Notice> {
    out.iter()
        .filter_map(|o| match o {
            Output::Notice(n) => Some(n.clone()),
            _ => None,
        })
        .collect()
}

fn has_notice(out: &[Output], notice: &Notice) -> bool {
    notices(out).contains(notice)
}

fn home_failed(out: &[Output], reason: HomeFailure) -> bool {
    notices(out)
        .iter()
        .any(|n| matches!(n, Notice::HomeFailed { reason: r, .. } if *r == reason))
}

fn left_home(out: &[Output]) -> bool {
    notices(out)
        .iter()
        .any(|n| matches!(n, Notice::Home { entered: false, .. }))
}

fn end_controls(out: &[Output]) -> Vec<(NodeId, SessionId, EndReason)> {
    out.iter()
        .filter_map(|o| match o {
            Output::SendControl {
                peer,
                msg: ControlMessage::EndControl { session, reason },
            } => Some((*peer, *session, *reason)),
            _ => None,
        })
        .collect()
}

fn begin_capture(out: &[Output]) -> Option<(CaptureId, PortalId, bool)> {
    out.iter().find_map(|o| match o {
        Output::BeginCapture {
            id,
            portal,
            drain_first,
        } => Some((*id, *portal, *drain_first)),
        _ => None,
    })
}

fn has_end_capture(out: &[Output]) -> bool {
    out.iter()
        .any(|o| matches!(o, Output::EndCapture { warp_to: None }))
}

fn has_hud_show(out: &[Output]) -> bool {
    out.iter()
        .any(|o| matches!(o, Output::ShowOverlay { id, .. } if *id == HUD))
}

fn has_hud_hide(out: &[Output]) -> bool {
    out.contains(&Output::HideOverlay(HUD))
}

fn set_portals(out: &[Output]) -> Vec<Vec<CapturePortal>> {
    out.iter()
        .filter_map(|o| match o {
            Output::SetPortals(p) => Some(p.clone()),
            _ => None,
        })
        .collect()
}

fn motions(out: &[Output]) -> Vec<PointerMessage> {
    out.iter()
        .filter_map(|o| match o {
            Output::SendMotion { msg, .. } => Some(*msg),
            _ => None,
        })
        .collect()
}

fn activations(out: &[Output]) -> Vec<WindowId> {
    out.iter()
        .filter_map(|o| match o {
            Output::ActivateWindow { window } => Some(*window),
            _ => None,
        })
        .collect()
}

/// The key and button transitions this node sent to a peer through the E1 session.
fn sent_transitions(out: &[Output]) -> Vec<(NodeId, Held, bool)> {
    out.iter()
        .filter_map(|o| match o {
            Output::SendInput {
                peer,
                msg: InputMessage::Key { usage, down, .. },
            } => Some((*peer, Held::Key(*usage), *down)),
            Output::SendInput {
                peer,
                msg: InputMessage::Button { button, down, .. },
            } => Some((*peer, Held::Button(*button), *down)),
            _ => None,
        })
        .collect()
}

fn lock_keys_sent(out: &[Output]) -> Vec<LockKeys> {
    out.iter()
        .filter_map(|o| match o {
            Output::SendInput {
                msg: InputMessage::LockKeys { keys, .. },
                ..
            } => Some(*keys),
            _ => None,
        })
        .collect()
}

fn heartbeats(out: &[Output]) -> Vec<(Vec<HidUsage>, Vec<MouseButton>)> {
    out.iter()
        .filter_map(|o| match o {
            Output::SendInput {
                msg:
                    InputMessage::State {
                        held_keys,
                        held_buttons,
                        ..
                    },
                ..
            } => Some((held_keys.clone(), held_buttons.clone())),
            _ => None,
        })
        .collect()
}

fn is_up_of(cmd: &InjectCmd, usage: HidUsage) -> bool {
    matches!(cmd, InjectCmd::Key { usage: u, down: false } if *u == usage)
}

fn is_down_of(cmd: &InjectCmd, usage: HidUsage) -> bool {
    matches!(cmd, InjectCmd::Key { usage: u, down: true } if *u == usage)
}

// ---------------------------------------------------------------------------------------------
// The harness.
// ---------------------------------------------------------------------------------------------

/// Where the proxy is, as B reports it.
#[derive(Clone, Copy, Debug)]
struct Proxy {
    display: DisplayId,
    origin: PointDevice,
    size: PixelSize,
}

impl Proxy {
    /// 400x300 at (200, 300) on B's display, with room for every exit.
    fn standard() -> Proxy {
        Proxy {
            display: REMOTE,
            origin: point(200.0, 300.0),
            size: PixelSize::new(400, 300),
        }
    }
}

/// The content of `W1` on `TWIN`: 400x300 at (50, 40).
fn content1() -> PixelRect {
    rect(50, 40, 450, 340)
}

struct H {
    engine: Engine,
    now: MonoTime,
    e1_journal: SharedJournal,
    e2_journal: SharedJournal,
    /// Answer every `SetPortals` with `PortalsSet { Ok }` naming exactly its ids.
    auto_portals: bool,
    /// Acknowledge every sequenced message sent to B (B's target acks each one).
    auto_ack: bool,
    log: Vec<Output>,
    /// What this node's injectors were asked to press and release, in order, per item.
    injected: BTreeMap<Held, ItemTrace>,
    /// The items each injection request touched, by its id (a `Recover` or `ReleaseAll` names
    /// several), so an answer can be attributed.
    inject_ids: BTreeMap<InjectId, Vec<(Held, bool)>>,
    /// What this node's E1 session has sent to a peer and not yet released (a press while it is
    /// down, or a release while it is up, is a violation at once).
    sent: BTreeMap<(NodeId, Held), bool>,
    /// Between a `HomeBind { install: true }` and the confirmed removal (A1 provenance): nothing
    /// may be injected, whatever input, callback or tick it comes from.
    fence_open: bool,
    /// The removal this fenced interval waits for: the newest removal requested since the newest
    /// installation. Only its own successful acknowledgement, delivered once, lifts the fence; an
    /// acknowledgement of an older bind's removal (a duplicate, a late one) never does.
    fence_removal: Option<HomeOp>,
    /// Sequence numbers of B's projection input, per projection.
    seqs: BTreeMap<(NodeId, ProjectionId), u32>,
    session: Option<SessionId>,
    capture: Option<CaptureId>,
    startup: Vec<Output>,
    layout_portal: PortalId,
}

impl H {
    fn config() -> EngineConfig {
        let mut config = EngineConfig::new(A);
        config.accel.base_mm_per_unit = 0.1;
        config.accel.max_gain = 1.0;
        config
    }

    /// A node that has not projected or crossed anything yet: displays, layout, grants, windows.
    fn bare() -> H {
        H::bare_with(&[], &[])
    }

    /// `e1` and `e2`: items the journals held when the previous process died.
    fn bare_with(e1: &[Held], e2: &[Held]) -> H {
        H::bare_config(H::config(), e1, e2)
    }

    fn bare_config(config: EngineConfig, e1: &[Held], e2: &[Held]) -> H {
        let e1_journal = SharedJournal::with(e1);
        let e2_journal = SharedJournal::with(e2);
        let (engine, startup) = Engine::new(
            config,
            Box::new(e1_journal.clone()),
            Box::new(e2_journal.clone()),
            ms(0),
        )
        .unwrap();
        let placed = [(A, PointMm::new(0.0, 0.0)), (B, PointMm::new(100.0, 0.0))]
            .into_iter()
            .map(|(node, origin)| Placed {
                id: GlobalDisplayId {
                    node,
                    display: LOCAL,
                },
                geometry: display(1).geometry,
                origin,
            })
            .collect();
        let layout = Layout::new(placed, H::config().layout).unwrap();
        let layout_portal = layout
            .portals()
            .iter()
            .find(|p| p.from.node == A && p.to.node == B)
            .unwrap()
            .id;
        let mut h = H {
            engine,
            now: ms(0),
            e1_journal,
            e2_journal,
            auto_portals: true,
            auto_ack: true,
            log: Vec::new(),
            injected: BTreeMap::new(),
            inject_ids: BTreeMap::new(),
            sent: BTreeMap::new(),
            fence_open: false,
            fence_removal: None,
            seqs: BTreeMap::new(),
            session: None,
            capture: None,
            startup: Vec::new(),
            layout_portal,
        };
        // What the previous process left held is a press that was never released.
        for item in e1.iter().chain(e2) {
            let trace = h.injected.entry(*item).or_default();
            trace.pending += 1;
            trace.downs += 1;
        }
        h.record(&startup);
        h.startup = startup;
        h.feed(Input::Session(SessionEvent::State(OPEN)));
        h.feed(Input::LocalDisplays(vec![display(1)]));
        h.feed(Input::PeerDisplays {
            peer: B,
            displays: vec![display(1)],
        });
        h.feed(Input::Layout(vec![
            Placement {
                node: A,
                display: LOCAL,
                origin: PointMm::new(0.0, 0.0),
                version: 1,
            },
            Placement {
                node: B,
                display: REMOTE,
                origin: PointMm::new(100.0, 0.0),
                version: 1,
            },
        ]));
        h.feed(Input::Grants(
            [
                (
                    B,
                    [Capability::WindowShare, Capability::WindowPresent].into(),
                ),
                (
                    C,
                    [Capability::WindowShare, Capability::WindowPresent].into(),
                ),
            ]
            .into(),
        ));
        h.feed(Input::PeerUp { peer: B });
        h.feed(Input::Windows(WindowEvent::Added(window(W1))));
        h.feed(Input::Windows(WindowEvent::Added(window(W2))));
        h.feed(Input::Windows(WindowEvent::Added(window(W3))));
        h
    }

    /// `W1` is projected to B, twin-parked, live and placed at `Proxy::standard()`.
    fn projected() -> H {
        let mut h = H::bare();
        h.project(W1, B, P1, TWIN, content1(), PlatformParking::Twin);
        h.place(B, P1, 1, Some(Proxy::standard()));
        h
    }

    /// `projected`, and this node controls B.
    fn controlling() -> H {
        let mut h = H::projected();
        h.cross();
        h
    }

    /// The scenario of every entry test: controlling B, the pointer steered into the proxy.
    fn aimed() -> H {
        let mut h = H::controlling();
        h.aim();
        h
    }

    // ---- driving the engine ----

    fn record(&mut self, outs: &[Output]) {
        for o in outs {
            match o {
                Output::HomeBind { install: true, .. } => {
                    self.fence_open = true;
                    self.fence_removal = None;
                }
                Output::HomeBind { op, install: false } if self.fence_open => {
                    self.fence_removal = Some(*op)
                }
                Output::Inject { id, cmd } => {
                    // A1 provenance: nothing this node injects may reach the home bind.
                    assert!(
                        !self.fence_open,
                        "injection {cmd:?} between the bind's installation and its confirmed removal"
                    );
                    self.trace_injection(*id, cmd);
                }
                _ => {}
            }
        }
        for (peer, item, down) in sent_transitions(outs) {
            let held = self.sent.entry((peer, item)).or_default();
            assert_ne!(
                *held,
                down,
                "sent to {peer:?}: {item:?} {} while it was already {}",
                if down { "pressed" } else { "released" },
                if down { "down" } else { "up" }
            );
            *held = down;
        }
        self.log.extend(outs.iter().cloned());
    }

    /// Follow one injection request: presses are owed a release; a release must have something
    /// to release, and a second one for the same press is only a retry if the first failed or is
    /// old enough for the ledger's own retry (50 ms).
    fn trace_injection(&mut self, id: InjectId, cmd: &InjectCmd) {
        let now = self.now;
        let mut touched = Vec::new();
        let mut releases: Vec<Held> = Vec::new();
        match cmd {
            InjectCmd::Key { usage, down } => {
                let item = Held::Key(*usage);
                touched.push((item, !down));
                if *down {
                    let trace = self.injected.entry(item).or_default();
                    trace.pending += 1;
                    trace.downs += 1;
                } else {
                    releases.push(item);
                }
            }
            InjectCmd::Button { button, down } => {
                let item = Held::Button(*button);
                touched.push((item, !down));
                if *down {
                    let trace = self.injected.entry(item).or_default();
                    trace.pending += 1;
                    trace.downs += 1;
                } else {
                    releases.push(item);
                }
            }
            InjectCmd::Recover { keys, buttons } => {
                for item in keys
                    .iter()
                    .map(|k| Held::Key(*k))
                    .chain(buttons.iter().map(|b| Held::Button(*b)))
                {
                    touched.push((item, true));
                    releases.push(item);
                }
            }
            InjectCmd::ReleaseAll => {
                for (item, trace) in &self.injected {
                    if trace.pending > 0 {
                        touched.push((*item, true));
                        releases.push(*item);
                    }
                }
            }
            _ => {}
        }
        for item in releases {
            let trace = self.injected.entry(item).or_default();
            assert!(
                trace.pending > 0,
                "a release of {item:?} with nothing pressed (up before its down, or a second \
                 release after the first was confirmed): {trace:?}"
            );
            let chain = if trace.chains.len() < trace.pending as usize {
                trace.chains.push(Vec::new());
                trace.chains.len() - 1
            } else {
                trace
                    .chains
                    .iter()
                    .position(|chain| chain.last().is_some_and(|a| a.retry_due(now)))
                    .unwrap_or_else(|| {
                        panic!(
                            "{item:?} released again with no failure and no retry due: {trace:?}"
                        )
                    })
            };
            trace.chains[chain].push(UpAttempt {
                id,
                at: now,
                answer: None,
            });
        }
        if !touched.is_empty() {
            self.inject_ids.insert(id, touched);
        }
    }

    /// The injector's answer to request `id`: a confirmed release settles one press and ends the
    /// retries; a failed one is remembered (the retry that follows is legitimate).
    fn trace_answer(&mut self, id: InjectId, ok: bool) {
        let Some(items) = self.inject_ids.get(&id).cloned() else {
            return;
        };
        for (item, up) in items {
            if !up {
                continue;
            }
            let Some(trace) = self.injected.get_mut(&item) else {
                continue;
            };
            let Some((chain, index)) = trace
                .chains
                .iter()
                .enumerate()
                .find_map(|(c, chain)| chain.iter().position(|a| a.id == id).map(|i| (c, i)))
            else {
                // Answered after a newer request of its release already settled it.
                continue;
            };
            if ok {
                // The release is settled: the whole chain goes, so any other request of it that
                // is still in flight is a retry whose answer is ignored.
                trace.pending = trace.pending.saturating_sub(1);
                trace.confirmed += 1;
                trace.chains.remove(chain);
            } else {
                trace.chains[chain][index].answer = Some(false);
            }
        }
    }

    /// Feed one input at the current time, answering `SetPortals` like the agent does and
    /// acknowledging what the session sends to B like B's target does.
    fn feed(&mut self, input: Input) -> Vec<Output> {
        if let Input::InjectDone { id, ok } = &input {
            self.trace_answer(*id, *ok);
        }
        // The confirmed removal this interval waits for ends it (A1), once: the acknowledgement
        // is consumed, so a duplicate of it, or one of an older bind's removal, lifts nothing.
        let clears_fence = self.fence_open
            && matches!(
                &input,
                Input::HomeBindSet { op, install: false, result: Ok(()) }
                    if self.fence_removal == Some(*op)
            );
        let mut out = self.engine.handle(input, self.now);
        self.record(&out);
        if clears_fence {
            self.fence_open = false;
            self.fence_removal = None;
        }
        let mut index = 0;
        while index < out.len() {
            let answer = match &out[index] {
                Output::SetPortals(portals) if self.auto_portals => Some(Input::PortalsSet {
                    ids: portals.iter().map(|p| p.id).collect(),
                    result: Ok(()),
                }),
                Output::SendInput {
                    peer,
                    msg:
                        InputMessage::Key { session, seq, .. }
                        | InputMessage::Button { session, seq, .. }
                        | InputMessage::Scroll { session, seq, .. }
                        | InputMessage::LockKeys { session, seq, .. }
                        | InputMessage::State { session, seq, .. },
                } if self.auto_ack && *peer == B => Some(proj_ack(*session, *seq)),
                _ => None,
            };
            if let Some(answer) = answer {
                let more = self.engine.handle(answer, self.now);
                self.record(&more);
                out.extend(more);
            }
            index += 1;
        }
        out
    }

    fn at(&mut self, time: u64, input: Input) -> Vec<Output> {
        self.now = ms(time);
        self.feed(input)
    }

    fn advance(&mut self, delta: u64) {
        self.now = MonoTime::from_nanos(self.now.as_nanos() + delta * 1_000_000);
    }

    fn now_ms(&self) -> u64 {
        self.now.as_nanos() / 1_000_000
    }

    /// Advance and deliver a tick.
    fn tick_after(&mut self, delta: u64) -> Vec<Output> {
        self.advance(delta);
        self.feed(Input::Tick)
    }

    /// Deliver a tick at `time` (milliseconds).
    fn tick(&mut self, time: u64) -> Vec<Output> {
        self.at(time, Input::Tick)
    }

    /// Answer every injection in `out` as the injector would.
    fn confirm(&mut self, out: &[Output], ok: bool) -> Vec<Output> {
        let mut more = Vec::new();
        for (id, _) in injects(out) {
            more.extend(self.feed(Input::InjectDone { id, ok }));
        }
        more
    }

    /// Tick until the engine has nothing more to do before `until` (milliseconds): returns every
    /// output, answering injections with `ok`.
    fn run_until(&mut self, until: u64, ok: bool) -> Vec<Output> {
        let mut all = Vec::new();
        while self.now_ms() < until {
            let next = self
                .engine
                .next_deadline()
                .map_or(until, |d| (d.as_nanos() / 1_000_000).max(self.now_ms() + 1))
                .min(until);
            let out = self.tick(next);
            let answers = self.confirm(&out, ok);
            all.extend(out);
            all.extend(answers);
        }
        all
    }

    // ---- scenario steps ----

    fn next_proj_seq(&mut self, peer: NodeId, projection: ProjectionId) -> u32 {
        let seq = self.seqs.entry((peer, projection)).or_default();
        *seq += 1;
        *seq
    }

    /// Project `window` to `peer`: offer, accept, park, capture.
    fn project(
        &mut self,
        window: WindowId,
        peer: NodeId,
        projection: ProjectionId,
        twin: DisplayId,
        content: PixelRect,
        kind: PlatformParking,
    ) {
        let out = self.feed(Input::Command(Command::Project { window, to: peer }));
        assert!(
            out.iter().any(|o| matches!(
                o,
                Output::SendControl { msg: ControlMessage::Projection(Message::Start { projection: p, .. }), .. } if *p == projection
            )),
            "project: no Start for {projection:?}: {out:?}"
        );
        let size = PixelSize::new(
            (content.max.x - content.min.x) as u32,
            (content.max.y - content.min.y) as u32,
        );
        let out = self.feed(projection_msg(
            peer,
            Message::Accepted {
                projection,
                size,
                scale: 1.0,
            },
        ));
        assert!(
            out.iter().any(|o| matches!(o, Output::Park { .. })),
            "{out:?}"
        );
        let out = self.feed(Input::Parked {
            window,
            result: Ok(parked(window, kind, twin, content)),
        });
        assert!(
            out.iter().any(|o| matches!(o, Output::StartCapture { .. })),
            "{out:?}"
        );
        self.feed(Input::CaptureStarted {
            projection,
            result: Ok(StreamId(u64::from(projection.0 as u32) + 100)),
        });
    }

    /// B reports where its proxy for `projection` is (`None`: on no display).
    fn place(
        &mut self,
        peer: NodeId,
        projection: ProjectionId,
        generation: u32,
        proxy: Option<Proxy>,
    ) -> Vec<Output> {
        let msg = match proxy {
            Some(p) => Message::ProxyPlaced {
                projection,
                generation,
                display: Some(p.display),
                origin: p.origin,
                size: p.size,
            },
            None => Message::ProxyPlaced {
                projection,
                generation,
                display: None,
                origin: point(0.0, 0.0),
                size: PixelSize::new(0, 0),
            },
        };
        self.feed(projection_msg(peer, msg))
    }

    /// Cross from A into B at the middle of the shared edge: this node now controls B with a
    /// live capture, and B's pointer is at (0, 500) on its display.
    fn cross(&mut self) {
        self.cross_with(vec![]);
    }

    /// `cross`, with `held_keys` down when the capture becomes effective.
    fn cross_with(&mut self, held_keys: Vec<HidUsage>) {
        self.cross_ordered(held_keys, vec![], false);
    }

    /// `cross`, the way the agent delivers an ordinary activation: `begin()` returns, the agent
    /// answers `CaptureBegun` (with `held_keys`, the keys held when `begin` started), and only
    /// then do the events that were queued during `begin()` arrive (`Started` first, then
    /// `queued`). Today's order, unchanged for ordinary captures (B4).
    fn cross_agent(&mut self, held_keys: Vec<HidUsage>, queued: Vec<CaptureEvent>) {
        self.cross_ordered(held_keys, queued, true);
    }

    fn cross_ordered(
        &mut self,
        held_keys: Vec<HidUsage>,
        queued: Vec<CaptureEvent>,
        begun_first: bool,
    ) {
        let out = self.feed(Input::Capture(CaptureEvent::EdgePressed {
            portal: self.layout_portal,
            position: 0.5,
            at: self.now,
        }));
        assert!(has_hud_show(&out), "cross: no HUD: {out:?}");
        let out = self.feed(Input::Overlay(OverlayEvent::Visible(HUD)));
        let session = out
            .iter()
            .find_map(|o| match o {
                Output::SendControl {
                    msg: ControlMessage::StartControl { session, .. },
                    ..
                } => Some(*session),
                _ => None,
            })
            .expect("StartControl");
        self.session = Some(session);
        let out = self.feed(control(B, ControlMessage::ControlStarted { session }));
        let (id, _, drain_first) = begin_capture(&out).expect("BeginCapture");
        assert!(
            !drain_first,
            "an ordinary capture keeps today's delivery order"
        );
        self.capture = Some(id);
        let begun = Input::CaptureBegun {
            id,
            result: Ok(CaptureStart {
                held_keys,
                lock_keys: LockKeys::default(),
            }),
        };
        let started = Input::Capture(CaptureEvent::Started { id });
        let mut all = Vec::new();
        if begun_first {
            all.extend(self.feed(begun));
            all.extend(self.feed(started));
            for event in queued {
                all.extend(self.feed(Input::Capture(event)));
            }
        } else {
            all.extend(self.feed(started));
            for event in queued {
                all.extend(self.feed(Input::Capture(event)));
            }
            all.extend(self.feed(begun));
        }
        assert!(end_controls(&all).is_empty(), "{all:?}");
        assert_eq!(self.engine.controlling(), Some(B));
    }

    /// A physical motion of this node's pointer, forwarded to B.
    fn motion(&mut self, dx: f64, dy: f64) -> Vec<Output> {
        self.feed(Input::Capture(CaptureEvent::Motion {
            dx,
            dy,
            kind: MotionKind::Unaccelerated,
            at: self.now,
        }))
    }

    /// Steer the pointer from (0, 500) to (250, 400) on B's display: inside the standard proxy at
    /// content position (50, 100).
    fn aim(&mut self) {
        self.motion(250.0, -100.0);
    }

    /// B reports motion over the proxy of `projection` at `position` (content pixels).
    fn report(&mut self, projection: ProjectionId, position: PointDevice) -> Vec<Output> {
        let seq = self.next_proj_seq(B, projection);
        self.feed(proj_input(
            B,
            ProjInput::Motion {
                projection,
                seq,
                position,
            },
        ))
    }

    /// The report that matches `aim`.
    fn trigger(&mut self) -> Vec<Output> {
        self.report(P1, point(50.0, 100.0))
    }

    fn focus(&mut self, window: Option<WindowId>) -> Vec<Output> {
        self.feed(Input::Windows(WindowEvent::Focused(window)))
    }

    /// B presses `usage` in the proxy of `projection` (the window must be focused to take it).
    fn proj_key(&mut self, projection: ProjectionId, usage: HidUsage, down: bool) -> Vec<Output> {
        let seq = self.next_proj_seq(B, projection);
        self.feed(proj_input(
            B,
            ProjInput::Key {
                projection,
                seq,
                usage,
                down,
            },
        ))
    }

    fn bind_set(&mut self, op: HomeOp, install: bool, ok: bool) -> Vec<Output> {
        self.feed(Input::HomeBindSet {
            op,
            install,
            result: if ok { Ok(()) } else { Err(Failure::Other) },
        })
    }

    fn released(&mut self, op: HomeOp, result: Result<Warp, Failure>) -> Vec<Output> {
        self.feed(Input::CaptureReleased { op, result })
    }

    fn portals_set(
        &mut self,
        ids: Vec<PortalId>,
        result: Result<(), PortalsFailure>,
    ) -> Vec<Output> {
        self.feed(Input::PortalsSet { ids, result })
    }

    /// The trigger, with nothing held: `HomeBind { install: true }` comes out in the same
    /// handle. Returns its operation.
    fn reach_binding(&mut self) -> HomeOp {
        let out = self.trigger();
        bind(&out, true).unwrap_or_else(|| panic!("no HomeBind after the trigger: {out:?}"))
    }

    /// … and the bind is confirmed: the capture is released. Returns the entry's operation (the
    /// same `op`, correlating the bind and the release).
    fn reach_releasing(&mut self) -> HomeOp {
        let op = self.reach_binding();
        self.advance(1);
        let out = self.bind_set(op, true, true);
        assert!(warp(&out).is_some(), "no ReleaseAndWarp: {out:?}");
        op
    }

    /// … and the release is confirmed while the window is not focused: `ActivateWindow` is out.
    fn reach_focusing(&mut self) -> HomeOp {
        let op = self.reach_releasing();
        self.advance(1);
        let out = self.released(op, Ok(Warp::Done));
        assert_eq!(activations(&out), vec![W1], "{out:?}");
        op
    }

    /// Home, whichever way the window takes focus: already focused, or after `ActivateWindow`.
    fn home_now(&mut self) -> HomeOp {
        let op = self.reach_releasing();
        self.advance(1);
        let out = self.released(op, Ok(Warp::Done));
        let entered = Notice::Home {
            key: key(P1),
            entered: true,
        };
        if !has_notice(&out, &entered) {
            self.advance(1);
            let out = self.focus(Some(W1));
            assert!(has_notice(&out, &entered), "{out:?}");
        }
        op
    }

    /// The strip of `edge` of the projection in `slot`.
    fn strip(&self, slot: u32, edge: Edge) -> PortalId {
        PortalId(
            TWIN_PORTAL_BASE
                + 4 * slot
                + match edge {
                    Edge::Left => 0,
                    Edge::Right => 1,
                    Edge::Top => 2,
                    Edge::Bottom => 3,
                },
        )
    }

    /// A press against a strip while home, then the HUD, the capture, and its activation. Stops
    /// before `CaptureBegun`; returns the capture id and the outputs of the press and the HUD.
    fn exit_to_activating(&mut self, edge: Edge, t: f64) -> (CaptureId, Vec<Output>) {
        let portal = self.strip(0, edge);
        let mut all = self.feed(Input::Capture(CaptureEvent::EdgePressed {
            portal,
            position: t,
            at: self.now,
        }));
        assert!(has_hud_show(&all), "the exit shows the HUD first: {all:?}");
        assert!(
            begin_capture(&all).is_none(),
            "no capture before the HUD is visible"
        );
        self.advance(1);
        let out = self.feed(Input::Overlay(OverlayEvent::Visible(HUD)));
        let (id, begun, drain_first) = begin_capture(&out).expect("BeginCapture after the HUD");
        assert_eq!(begun, portal);
        assert!(drain_first, "a home exit's capture drains its events first");
        all.extend(out);
        (id, all)
    }

    fn capture_begun(&mut self, id: CaptureId, held_keys: Vec<HidUsage>) -> Vec<Output> {
        self.feed(Input::CaptureBegun {
            id,
            result: Ok(CaptureStart {
                held_keys,
                lock_keys: LockKeys {
                    caps_lock: Some(true),
                    num_lock: None,
                    scroll_lock: None,
                },
            }),
        })
    }

    /// Home, then an exit through `edge` at `t`, completed. Returns the outputs of `CaptureBegun`.
    fn exit_through(&mut self, edge: Edge, t: f64) -> Vec<Output> {
        let (id, _) = self.exit_to_activating(edge, t);
        self.advance(1);
        self.feed(Input::Capture(CaptureEvent::Started { id }));
        self.advance(1);
        self.capture_begun(id, vec![])
    }

    /// The capture the newest `BeginCapture` named.
    fn last_begin(&self) -> CaptureId {
        self.log
            .iter()
            .rev()
            .find_map(|o| match o {
                Output::BeginCapture { id, .. } => Some(*id),
                _ => None,
            })
            .expect("a capture was begun")
    }

    /// The newest removal of the home bind that was requested, if any.
    fn latest_removal(&self) -> Option<HomeOp> {
        self.log.iter().rev().find_map(|o| match o {
            Output::HomeBind { op, install: false } => Some(*op),
            _ => None,
        })
    }

    /// The newest removal of the home bind that was requested.
    fn last_removal(&self) -> HomeOp {
        self.latest_removal().expect("a removal was requested")
    }

    /// Confirm the removal of the bind named in `out`.
    fn confirm_removal(&mut self, out: &[Output]) -> Vec<Output> {
        let op = bind(out, false).expect("a removal was requested");
        self.advance(1);
        self.bind_set(op, false, true)
    }

    // ---- invariants ----

    /// 04 §8: every down has exactly one up, on the node that received it; the journals are empty.
    ///
    /// This node's injectors: a release that fails is retried (the same release asked for again),
    /// so the requests may outnumber the downs, but every down has been released (the journal
    /// proves the confirmation), and nothing was released that was never pressed here. The E1
    /// session to a peer sends each up exactly once.
    fn quiet(&mut self) {
        // The ordered history: every press has had its release confirmed by the injector (the
        // ordering rules themselves, no release without a press and no second release of one
        // press, are enforced as the injections happen, in `trace_injection`).
        for (item, trace) in &self.injected {
            assert_eq!(
                trace.pending, 0,
                "{item:?} was pressed {} times and released {} times: {trace:?}",
                trace.downs, trace.confirmed
            );
            assert_eq!(trace.downs, trace.confirmed, "{item:?}: {trace:?}");
        }
        // The E1 session: whatever it pressed on a peer it released (a press while down and a
        // release while up were refused as they were sent).
        for ((peer, item), down) in &self.sent {
            assert!(
                !down,
                "sent to {peer:?}: {item:?} was pressed and never released"
            );
        }
        assert_eq!(self.e1_journal.items(), vec![], "E1 journal");
        assert_eq!(self.e2_journal.items(), vec![], "E2 journal");
        // Behavioural probes that nothing is still owed, in the ledgers (a pending release, a
        // retry or a held item would inject on a tick) and in the router (a held item would be
        // listed by the next heartbeat). Two seconds of ticks, acknowledged like B's target does.
        let start = self.now_ms();
        for step in 1..=8 {
            let out = self.tick(start + step * 250);
            assert!(
                !has_inject(&out),
                "an injector still owes something: {out:?}"
            );
            for (keys, buttons) in heartbeats(&out) {
                assert!(
                    keys.is_empty() && buttons.is_empty(),
                    "the router still holds {keys:?} {buttons:?}"
                );
            }
        }
    }
}

fn key(projection: ProjectionId) -> ProjectionKey {
    ProjectionKey {
        source: A,
        projection,
    }
}

// ---------------------------------------------------------------------------------------------
// Scenario sanity.
// ---------------------------------------------------------------------------------------------

#[test]
fn scenario_offers_four_twin_strips_when_controlling() {
    let h = H::controlling();
    let strips: Vec<_> = h
        .log
        .iter()
        .filter_map(|o| match o {
            Output::SetPortals(p) => Some(p.clone()),
            _ => None,
        })
        .next_back()
        .unwrap()
        .into_iter()
        .filter(|p| p.display == TWIN)
        .collect();
    assert_eq!(strips.len(), 4);
    let by_edge = |edge| strips.iter().find(|s| s.edge == edge).unwrap();
    // Output-boundary strips spanning the content's extent (§2.5): device pixels of the twin.
    assert_eq!(
        (by_edge(Edge::Left).from, by_edge(Edge::Left).to),
        (40.0, 340.0)
    );
    assert_eq!(
        (by_edge(Edge::Right).from, by_edge(Edge::Right).to),
        (40.0, 340.0)
    );
    assert_eq!(
        (by_edge(Edge::Top).from, by_edge(Edge::Top).to),
        (50.0, 450.0)
    );
    assert_eq!(
        (by_edge(Edge::Bottom).from, by_edge(Edge::Bottom).to),
        (50.0, 450.0)
    );
    assert_eq!(by_edge(Edge::Left).id, h.strip(0, Edge::Left));
    assert_eq!(by_edge(Edge::Bottom).id, h.strip(0, Edge::Bottom));
}

// ---------------------------------------------------------------------------------------------
// ENTRY
// ---------------------------------------------------------------------------------------------

/// What an entry that did not start looks like: nothing bound, nothing released, nothing failed,
/// and the session continues as before.
fn assert_no_entry(h: &H, out: &[Output]) {
    assert!(binds(out).is_empty(), "no bind: {out:?}");
    assert!(warps(out).is_empty(), "no release: {out:?}");
    assert!(
        !notices(out)
            .iter()
            .any(|n| matches!(n, Notice::Home { .. } | Notice::HomeFailed { .. })),
        "no notice: {out:?}"
    );
    assert!(
        !out.iter().any(|o| matches!(
            o,
            Output::Inject {
                cmd: InjectCmd::Key { .. } | InjectCmd::Button { .. },
                ..
            }
        )),
        "nothing pressed or released: {out:?}"
    );
    assert_eq!(h.engine.controlling(), Some(B));
}

#[test]
fn entry_enters_through_drain_bind_release_and_focus() {
    let mut h = H::aimed();
    let out = h.trigger();
    // The trigger is decided before E2 sees it: it is never injected.
    assert!(!has_inject(&out), "{out:?}");
    let op = bind(&out, true).expect("HomeBind");
    assert!(warps(&out).is_empty());
    h.advance(1);
    let out = h.bind_set(op, true, true);
    // The pointer is warped onto the twin at the tracker's current position (250, 400) mapped
    // through the placement (200, 300) into the content (50, 40).
    assert_eq!(warps(&out), vec![(op, (TWIN, point(100.0, 140.0)))]);
    h.advance(1);
    let out = h.released(op, Ok(Warp::Done));
    assert!(has_hud_hide(&out));
    assert_eq!(activations(&out), vec![W1]);
    h.advance(1);
    let out = h.focus(Some(W1));
    assert!(has_notice(
        &out,
        &Notice::Home {
            key: key(P1),
            entered: true
        }
    ));
    // The E1 session with B stays open.
    assert_eq!(h.engine.controlling(), Some(B));
    assert!(end_controls(&out).is_empty());
    h.quiet();
}

#[test]
fn entry_needs_tracker_inside_and_fresh_motion() {
    // The control: everything agrees.
    let mut h = H::aimed();
    assert!(bind(&h.trigger(), true).is_some());

    // The tracker is outside the placement while the report is inside it.
    let mut h = H::controlling();
    h.motion(10.0, 0.0);
    let out = h.trigger();
    assert_no_entry(&h, &out);
    h.quiet();

    // A stale or duplicate sequence number: E2's own check refuses it.
    let mut h = H::controlling();
    let seq = h.next_proj_seq(B, P1) + 4;
    h.feed(proj_input(
        B,
        ProjInput::Motion {
            projection: P1,
            seq,
            position: point(50.0, 100.0),
        },
    ));
    h.aim();
    let out = h.feed(proj_input(
        B,
        ProjInput::Motion {
            projection: P1,
            seq,
            position: point(50.0, 100.0),
        },
    ));
    assert_no_entry(&h, &out);
    h.seqs.insert((B, P1), seq);
    assert!(bind(&h.trigger(), true).is_some(), "a newer report enters");

    // Another peer is not the projection's destination.
    let mut h = H::aimed();
    let out = h.feed(proj_input(
        C,
        ProjInput::Motion {
            projection: P1,
            seq: 1,
            position: point(50.0, 100.0),
        },
    ));
    assert_no_entry(&h, &out);

    // A Mirror-parked source has no twin to go home on.
    let mut h = H::bare();
    h.project(W1, B, P1, TWIN, content1(), PlatformParking::Mirror);
    h.place(B, P1, 1, Some(Proxy::standard()));
    h.cross();
    h.aim();
    let out = h.trigger();
    assert_no_entry(&h, &out);
    assert!(
        set_portals(&h.log)
            .iter()
            .all(|p| p.iter().all(|c| c.display != TWIN))
    );

    // No display: the proxy is on none right now.
    let mut h = H::bare();
    h.project(W1, B, P1, TWIN, content1(), PlatformParking::Twin);
    h.place(B, P1, 1, None);
    h.cross();
    h.aim();
    let out = h.trigger();
    assert_no_entry(&h, &out);

    // A size that differs from the twin's content: a resize round trip is under way.
    let mut h = H::bare();
    h.project(W1, B, P1, TWIN, content1(), PlatformParking::Twin);
    h.place(
        B,
        P1,
        1,
        Some(Proxy {
            size: PixelSize::new(399, 300),
            ..Proxy::standard()
        }),
    );
    h.cross();
    h.aim();
    let out = h.trigger();
    assert_no_entry(&h, &out);

    // The tracker and the report may differ by ENTRY_SLACK (96 pixels) per axis, not more.
    let mut h = H::aimed();
    let out = h.report(P1, point(50.0 + 97.0, 100.0));
    assert_no_entry(&h, &out);
    let out = h.report(P1, point(50.0, 100.0 - 97.0));
    assert_no_entry(&h, &out);
    let out = h.report(P1, point(50.0 + 96.0, 100.0 - 96.0));
    assert!(
        bind(&out, true).is_some(),
        "96 pixels is within the slack: {out:?}"
    );
}

#[test]
fn entry_rejects_outside_or_nonfinite_report() {
    let mut h = H::aimed();
    for position in [
        point(400.0, 100.0),
        point(-1.0, 100.0),
        point(50.0, 300.0),
        point(50.0, -0.5),
        point(f64::NAN, 100.0),
        point(50.0, f64::INFINITY),
        point(f64::NEG_INFINITY, 100.0),
    ] {
        let out = h.report(P1, position);
        assert_no_entry(&h, &out);
    }
    // A report inside the content enters.
    assert!(bind(&h.trigger(), true).is_some());
    h.quiet();
}

#[test]
fn entry_needs_recent_local_motion() {
    let mut h = H::controlling();
    h.now = ms(10_000);
    h.aim();
    // The pointer has been still for LOCAL_MOTION_AGE + 1 ms: the report cannot describe it.
    h.advance(LOCAL_MOTION_AGE + 1);
    let out = h.trigger();
    assert_no_entry(&h, &out);
    // Fresh motion, then a fresh report, enters.
    h.advance(10);
    h.motion(1.0, 0.0);
    let out = h.trigger();
    assert!(bind(&out, true).is_some(), "{out:?}");
    // Exactly LOCAL_MOTION_AGE is still recent.
    let mut h = H::controlling();
    h.now = ms(10_000);
    h.aim();
    h.advance(LOCAL_MOTION_AGE);
    assert!(bind(&h.trigger(), true).is_some());
    h.quiet();
}

#[test]
fn entry_refused_without_offerable_edge() {
    // A fullscreen proxy with no layout continuation offers no exit. First enter B normally,
    // then separate its display from A so the flush left edge no longer has a return portal.
    let mut h = H::bare();
    h.project(
        W1,
        B,
        P1,
        TWIN,
        rect(0, 0, 1000, 1000),
        PlatformParking::Twin,
    );
    h.place(
        B,
        P1,
        1,
        Some(Proxy {
            display: REMOTE,
            origin: point(0.0, 0.0),
            size: PixelSize::new(1000, 1000),
        }),
    );
    h.cross();
    h.feed(Input::Layout(flush_placements(&[
        (A, 1, 0.0, 0.0),
        (B, 1, 200.0, 0.0),
    ])));
    assert!(
        set_portals(&h.log)
            .last()
            .unwrap()
            .iter()
            .all(|c| c.display != TWIN),
        "no strip is offered"
    );
    h.motion(5.0, 0.0);
    let out = h.report(P1, point(5.0, 500.0));
    assert_no_entry(&h, &out);
    h.quiet();
}

#[test]
fn entry_refused_until_portals_installed() {
    let mut h = H::projected();
    h.auto_portals = false;
    h.cross();
    h.aim();
    let strips: Vec<PortalId> = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom]
        .into_iter()
        .map(|e| h.strip(0, e))
        .collect();
    let offered = set_portals(&h.log).pop().unwrap();
    let ids: Vec<PortalId> = offered.iter().map(|p| p.id).collect();
    assert!(strips.iter().all(|s| ids.contains(s)));
    // Requested is not installed: the answer has not come yet.
    let out = h.trigger();
    assert_no_entry(&h, &out);
    // The backend refused the set.
    h.portals_set(ids.clone(), Err(PortalsFailure::Rejected));
    h.motion(1.0, 0.0);
    let out = h.trigger();
    assert_no_entry(&h, &out);
    // It is offered again; its answer (in order, one per emitted set) installs it.
    let t = h.now_ms();
    let out = h.tick(t + PORTALS_RETRY);
    assert_eq!(set_portals(&out).len(), 1, "{out:?}");
    h.portals_set(ids, Ok(()));
    h.motion(-1.0, 0.0);
    let out = h.trigger();
    assert!(bind(&out, true).is_some(), "{out:?}");
    h.quiet();

    // An answer that names another set than the oldest outstanding request is not one the
    // controller can rely on: it is treated as uncertain (fail safe), which ends the capture.
    let mut h = H::projected();
    h.auto_portals = false;
    h.cross();
    h.aim();
    let out = h.portals_set(vec![PortalId(1)], Ok(()));
    assert!(has_end_capture(&out), "{out:?}");
    assert_eq!(h.engine.controlling(), None);
    h.quiet();

    // An answer nobody asked for is the same.
    let mut h = H::projected();
    h.cross();
    let out = h.portals_set(vec![], Ok(()));
    assert!(has_end_capture(&out), "{out:?}");
    h.quiet();
}

#[test]
fn entry_refused_while_button_held_then_enters_after_release() {
    let mut h = H::aimed();
    let out = h.feed(Input::Capture(CaptureEvent::Button {
        button: BUTTON,
        down: true,
        at: h.now,
    }));
    assert_eq!(
        sent_transitions(&out),
        vec![(B, Held::Button(BUTTON), true)],
        "the drag goes to B as today"
    );
    let out = h.trigger();
    assert_no_entry(&h, &out);
    h.advance(10);
    h.feed(Input::Capture(CaptureEvent::Button {
        button: BUTTON,
        down: false,
        at: h.now,
    }));
    // A report that arrived during the drag is gone: a fresh motion and a fresh report enter.
    h.motion(1.0, 0.0);
    let out = h.trigger();
    assert!(bind(&out, true).is_some(), "{out:?}");
    h.quiet();
}

#[test]
fn home_entry_discards_waiting_targeting_and_its_fifo() {
    for press in [true, false] {
        for completion_during_home in [true, false] {
            let mut h = H::controlling();
            h.focus(Some(W1));
            let seq = h.next_proj_seq(B, P1);
            let pending = if press {
                ProjInput::Button {
                    projection: P1,
                    seq,
                    button: BUTTON,
                    down: true,
                    position: point(10.0, 20.0),
                }
            } else {
                ProjInput::Scroll {
                    projection: P1,
                    seq,
                    delta: scroll_delta(),
                    position: point(10.0, 20.0),
                }
            };
            let out = h.feed(proj_input(B, pending));
            let injected = injects(&out);
            assert_eq!(injected.len(), 1);
            assert!(matches!(injected[0].1, InjectCmd::MoveTo { .. }));
            let pending = injected[0].0;
            assert!(h.proj_key(P1, KEY, true).is_empty());
            let seq = h.next_proj_seq(B, P1);
            assert!(
                h.feed(proj_input(
                    B,
                    ProjInput::Button {
                        projection: P1,
                        seq,
                        button: BUTTON,
                        down: false,
                        position: point(10.0, 20.0),
                    }
                ))
                .is_empty()
            );
            h.aim();
            let op = h.reach_binding();
            if completion_during_home {
                assert!(!has_inject(&h.feed(Input::InjectDone {
                    id: pending,
                    ok: true
                })));
            }
            // Abort home and remove its bind: even with the filter lifted, the old move is stale.
            let out = h.bind_set(op, true, false);
            let removal = bind(&out, false).unwrap();
            h.bind_set(removal, false, true);
            assert!(!has_inject(&h.feed(Input::InjectDone {
                id: pending,
                ok: true
            })));
            assert!(h.e2_journal.items().is_empty());
            let seq = h.next_proj_seq(B, P1);
            let out = h.feed(proj_input(
                B,
                ProjInput::Button {
                    projection: P1,
                    seq,
                    button: BUTTON,
                    down: false,
                    position: point(10.0, 20.0),
                },
            ));
            assert!(!has_inject(&out));
            h.quiet();
        }
    }
}

#[test]
fn entry_order() {
    let mut h = H::controlling();
    // A key B pressed in the proxy is down in W.
    h.focus(Some(W1));
    let out = h.proj_key(P1, KEY, true);
    let (down_id, _) = injects(&out).into_iter().next().expect("injected");
    h.feed(Input::InjectDone {
        id: down_id,
        ok: true,
    });
    h.aim();
    let out = h.trigger();
    // In one handle: the E2 drain's release, and nothing else (no bind until it is confirmed, no
    // release of the capture, and the trigger motion is not injected).
    assert_eq!(injects(&out).len(), 1, "{out:?}");
    assert!(is_up_of(&injects(&out)[0].1, KEY));
    assert!(binds(&out).is_empty() && warps(&out).is_empty(), "{out:?}");
    assert!(
        !injects(&out)
            .iter()
            .any(|(_, cmd)| matches!(cmd, InjectCmd::MoveTo { .. })),
        "the trigger motion is not injected"
    );
    // Confirmed: the bind follows in that handle.
    let out = h.confirm(&out, true);
    assert!(bind(&out, true).is_some(), "{out:?}");
    h.quiet();
}

#[test]
fn entry_waits_for_drain_confirmation() {
    // Two keys down in W: the release of each must be confirmed.
    let mut h = H::controlling();
    h.focus(Some(W1));
    for usage in [KEY, KEY2] {
        let out = h.proj_key(P1, usage, true);
        h.confirm(&out, true);
    }
    h.aim();
    let out = h.trigger();
    let ups = injects(&out);
    assert_eq!(ups.len(), 2, "{out:?}");
    let first = h.feed(Input::InjectDone {
        id: ups[0].0,
        ok: true,
    });
    assert!(
        binds(&first).is_empty() && warps(&first).is_empty(),
        "{first:?}"
    );
    let second = h.feed(Input::InjectDone {
        id: ups[1].0,
        ok: true,
    });
    let op = bind(&second, true).expect("the last confirmation starts the bind");
    assert!(
        warps(&second).is_empty(),
        "no release before the bind is confirmed"
    );
    h.advance(1);
    let out = h.bind_set(op, true, true);
    assert!(warp(&out).is_some());
    h.quiet();

    // The E1 target's ledger counts too: a recovery release that is still unconfirmed holds
    // the entry back, however many E2 releases are done.
    let mut h = H::bare_with(&[Held::Key(KEY)], &[]);
    let recovery: Vec<_> = injects(&h.startup);
    assert_eq!(recovery.len(), 1);
    h.project(W1, B, P1, TWIN, content1(), PlatformParking::Twin);
    h.place(B, P1, 1, Some(Proxy::standard()));
    h.cross();
    h.aim();
    let out = h.trigger();
    assert!(binds(&out).is_empty(), "{out:?}");
    let out = h.feed(Input::InjectDone {
        id: recovery[0].0,
        ok: true,
    });
    assert!(bind(&out, true).is_some(), "{out:?}");
    h.quiet();
}

#[test]
fn entry_aborts_on_drain_timeout() {
    let mut h = H::controlling();
    h.focus(Some(W1));
    let out = h.proj_key(P1, KEY, true);
    h.confirm(&out, true);
    h.now = ms(5_000);
    h.aim();
    let trigger = h.trigger();
    assert_eq!(injects(&trigger).len(), 1);
    // The release is answered `InjectDone { ok: false }`: the ledger retries every 50 ms, and the
    // entry never gets past the drain.
    let mut all = h.confirm(&trigger, false);
    all.extend(h.run_until(5_000 + DRAIN_TIMEOUT - 1, false));
    assert!(binds(&all).is_empty() && warps(&all).is_empty(), "{all:?}");
    assert!(
        injects(&all).iter().any(|(_, c)| is_up_of(c, KEY)),
        "the retries keep coming"
    );
    assert!(notices(&all).is_empty(), "{all:?}");
    let out = h.tick(5_000 + DRAIN_TIMEOUT);
    // The deadline: abort before release. Captured as before, nothing bound, nothing released.
    assert!(home_failed(&out, HomeFailure::Drain), "{out:?}");
    assert!(binds(&out).is_empty() && warps(&out).is_empty(), "{out:?}");
    assert!(!has_end_capture(&out) && end_controls(&out).is_empty());
    assert_eq!(h.engine.controlling(), Some(B));
    // Physical input afterwards is forwarded to B.
    let out = h.feed(Input::Capture(CaptureEvent::Key {
        usage: KEY2,
        down: true,
        at: h.now,
    }));
    assert_eq!(sent_transitions(&out), vec![(B, Held::Key(KEY2), true)]);
    let out = h.feed(Input::Capture(CaptureEvent::Key {
        usage: KEY2,
        down: false,
        at: h.now,
    }));
    assert_eq!(sent_transitions(&out), vec![(B, Held::Key(KEY2), false)]);
    // The retry's eventual up is injected while still captured, never while home: it arrives on
    // the next tick, and confirming it settles the ledger.
    let out = h.tick_after(50);
    let retry = injects(&out);
    assert!(retry.iter().any(|(_, c)| is_up_of(c, KEY)), "{out:?}");
    h.confirm(&out, true);
    h.quiet();
}

#[test]
fn entry_binds_then_releases() {
    let mut h = H::aimed();
    // B holds a key (pressed physically, routed to B).
    let out = h.feed(Input::Capture(CaptureEvent::Key {
        usage: KEY,
        down: true,
        at: h.now,
    }));
    assert_eq!(sent_transitions(&out), vec![(B, Held::Key(KEY), true)]);
    let out = h.trigger();
    let op = bind(&out, true).expect("HomeBind after settling");
    assert!(
        sent_transitions(&out).is_empty() && warps(&out).is_empty(),
        "nothing is released before the bind is confirmed: {out:?}"
    );
    h.advance(1);
    let out = h.bind_set(op, true, true);
    let up = out
        .iter()
        .position(|o| {
            matches!(
                o,
                Output::SendInput {
                    msg: InputMessage::Key { down: false, .. },
                    ..
                }
            )
        })
        .expect("the E1 up for the held key");
    let release = out
        .iter()
        .position(|o| matches!(o, Output::ReleaseAndWarp { .. }))
        .expect("ReleaseAndWarp");
    assert!(up < release, "E1 ups are sent before the release: {out:?}");
    assert!(
        end_controls(&out).is_empty(),
        "no EndControl: the session stays open"
    );
    h.quiet();
}

#[test]
fn entry_aborts_on_bind_failure_and_timeout() {
    for by_timeout in [false, true] {
        let mut h = H::aimed();
        h.now = ms(2_000);
        h.motion(1.0, 0.0);
        let out = h.trigger();
        let op = bind(&out, true).unwrap();
        let out = if by_timeout {
            h.tick(2_000 + BIND_TIMEOUT)
        } else {
            h.advance(5);
            h.bind_set(op, true, false)
        };
        // Captured as before: nothing released, the session untouched.
        assert!(home_failed(&out, HomeFailure::Bind), "{out:?}");
        assert!(warps(&out).is_empty() && !has_end_capture(&out) && end_controls(&out).is_empty());
        assert_eq!(h.engine.controlling(), Some(B));
        // The bind may be partly installed: it goes through its removal phase (A1).
        let removal = bind(&out, false).expect("rollback");
        // The fence: the next report is refused whatever it says, until the fence expires.
        let started = h.now_ms();
        h.advance(10);
        h.motion(1.0, 0.0);
        let out = h.trigger();
        assert_no_entry(&h, &out);
        h.advance(1);
        h.bind_set(removal, false, true);
        h.advance(10);
        h.motion(-1.0, 0.0);
        let out = h.trigger();
        assert_no_entry(&h, &out);
        h.now = ms(started + HOME_RETRY);
        h.motion(1.0, 0.0);
        let out = h.trigger();
        assert!(bind(&out, true).is_some(), "the fence expired: {out:?}");
        h.quiet();
    }
}

#[test]
fn entry_rechecks_guards_before_release() {
    // A button pressed while the drain is still waiting.
    let mut h = H::controlling();
    h.focus(Some(W1));
    let out = h.proj_key(P1, KEY, true);
    h.confirm(&out, true);
    h.aim();
    let trigger = h.trigger();
    assert_eq!(injects(&trigger).len(), 1);
    let out = h.feed(Input::Capture(CaptureEvent::Button {
        button: BUTTON,
        down: true,
        at: h.now,
    }));
    assert!(home_failed(&out, HomeFailure::Guard), "{out:?}");
    assert!(binds(&out).is_empty());
    // The drag is forwarded to B as today, and ends normally.
    assert_eq!(
        sent_transitions(&out),
        vec![(B, Held::Button(BUTTON), true)]
    );
    h.feed(Input::Capture(CaptureEvent::Button {
        button: BUTTON,
        down: false,
        at: h.now,
    }));
    h.confirm(&trigger, true);
    h.quiet();

    // A button pressed while the bind is being installed.
    let mut h = H::aimed();
    let op = h.reach_binding();
    h.feed(Input::Capture(CaptureEvent::Button {
        button: BUTTON,
        down: true,
        at: h.now,
    }));
    h.advance(1);
    let out = h.bind_set(op, true, true);
    assert!(home_failed(&out, HomeFailure::Guard), "{out:?}");
    assert!(warps(&out).is_empty(), "the capture is not released");
    assert!(bind(&out, false).is_some(), "the bind is rolled back");
    assert_eq!(h.engine.controlling(), Some(B));
    h.feed(Input::Capture(CaptureEvent::Button {
        button: BUTTON,
        down: false,
        at: h.now,
    }));
    h.quiet();
}

#[test]
fn entry_commits_on_release_and_focus() {
    // `Ended` of the released capture before and after the release's answer: expected either way.
    for ended_first in [true, false] {
        let mut h = H::aimed();
        let capture = h.capture.unwrap();
        let op = h.reach_releasing();
        let ended = Input::Capture(CaptureEvent::Ended {
            id: capture,
            reason: CaptureEnd::Requested,
        });
        if ended_first {
            h.advance(1);
            let out = h.feed(ended.clone());
            assert!(out.is_empty(), "{out:?}");
        }
        h.advance(1);
        let out = h.released(op, Ok(Warp::Done));
        assert!(has_hud_hide(&out));
        assert_eq!(activations(&out), vec![W1], "ActivateWindow once");
        if !ended_first {
            h.advance(1);
            let out = h.feed(ended);
            assert!(out.is_empty(), "{out:?}");
        }
        // The same focus event twice: one commit.
        h.advance(1);
        let out = h.focus(Some(W1));
        assert_eq!(notices(&out).len(), 1);
        let out = h.focus(Some(W1));
        assert!(notices(&out).is_empty());
        assert_eq!(h.engine.controlling(), Some(B));
        h.quiet();
    }
    // Focus already held: commit at once, no activation.
    let mut h = H::aimed();
    h.focus(Some(W1));
    let op = h.reach_releasing();
    h.advance(1);
    let out = h.released(op, Ok(Warp::Done));
    assert!(activations(&out).is_empty(), "{out:?}");
    assert!(has_notice(
        &out,
        &Notice::Home {
            key: key(P1),
            entered: true
        }
    ));
    h.quiet();
}

/// The fallback point: the centre of A's display the session was entered from.
const FALLBACK: (DisplayId, PointDevice) = (LOCAL, PointDevice::new(500.0, 500.0));

impl H {
    /// Whether a key B presses in `projection`'s proxy is injected right now (that is, whether the
    /// E2 home filter is off). If it was injected it is released again.
    fn probe(&mut self, projection: ProjectionId) -> bool {
        self.probe_in(B, projection, W1)
    }

    /// `probe` for another source: `window` is the projection's window, focused first so a key
    /// that is let through is taken.
    fn probe_in(&mut self, peer: NodeId, projection: ProjectionId, window: WindowId) -> bool {
        self.focus(Some(window));
        let seq = self.next_proj_seq(peer, projection);
        let out = self.feed(proj_input(
            peer,
            ProjInput::Key {
                projection,
                seq,
                usage: KEY,
                down: true,
            },
        ));
        let injected = injects(&out).iter().any(|(_, c)| is_down_of(c, KEY));
        if injected {
            self.confirm(&out, true);
            let seq = self.next_proj_seq(peer, projection);
            let out = self.feed(proj_input(
                peer,
                ProjInput::Key {
                    projection,
                    seq,
                    usage: KEY,
                    down: false,
                },
            ));
            self.confirm(&out, true);
        }
        injected
    }

    /// C joins the layout to the right of B (so B's pointer can leave B through its right edge
    /// into C) and is up.
    fn add_c_right(&mut self) {
        self.add_c(200.0);
    }

    /// C joins the layout to the left of A.
    fn add_c_left(&mut self) {
        self.add_c(-100.0);
    }

    fn add_c(&mut self, x_mm: f64) {
        self.feed(Input::PeerDisplays {
            peer: C,
            displays: vec![display(1)],
        });
        self.feed(Input::PeerUp { peer: C });
        let placement = |node, x| Placement {
            node,
            display: LOCAL,
            origin: PointMm::new(x, 0.0),
            version: 2,
        };
        self.feed(Input::Layout(vec![
            placement(A, 0.0),
            placement(B, 100.0),
            placement(C, x_mm),
        ]));
    }
}

#[test]
fn entry_aborts_on_release_err() {
    let mut h = H::aimed();
    let capture = h.capture.unwrap();
    let session = h.session.unwrap();
    let op = h.reach_releasing();
    h.advance(1);
    let out = h.released(op, Err(Failure::Other));
    // Abort after release, a capture may exist: the session ends, the pointer goes to the
    // fallback point, the bind is removed.
    assert!(home_failed(&out, HomeFailure::Warp), "{out:?}");
    let removal = bind(&out, false).expect("the bind is removed");
    assert_eq!(warps(&out).len(), 1);
    assert_eq!(warps(&out)[0].1, FALLBACK);
    assert_eq!(end_controls(&out), vec![(B, session, EndReason::Released)]);
    assert_eq!(h.engine.controlling(), None);
    assert!(!has_hud_hide(&out), "the capture's end fences the return");
    // The seat stays arbitrated until the removal is confirmed.
    assert!(!h.probe(P1), "{out:?}");
    // The capture's end finishes the return.
    h.advance(1);
    let out = h.feed(Input::Capture(CaptureEvent::Ended {
        id: capture,
        reason: CaptureEnd::Requested,
    }));
    assert!(has_hud_hide(&out));
    h.advance(1);
    h.bind_set(removal, false, true);
    assert!(
        h.probe(P1),
        "the filter lifts once the removal is confirmed"
    );
    h.quiet();

    // No `Ended`: the fence times out and warps once more.
    let mut h = H::aimed();
    let op = h.reach_releasing();
    h.advance(1);
    let out = h.released(op, Err(Failure::Other));
    let started = h.now_ms();
    let removal = bind(&out, false).unwrap();
    let out = h.tick(started + END_TIMEOUT - 1);
    assert!(warps(&out).is_empty());
    let out = h.tick(started + END_TIMEOUT);
    assert_eq!(warps(&out).len(), 1);
    assert_eq!(warps(&out)[0].1, FALLBACK);
    assert!(has_hud_hide(&out));
    h.bind_set(removal, false, true);
    h.quiet();
}

#[test]
fn entry_aborts_on_warp_skipped_with_an_open_gate_and_immediately_retries_the_fallback() {
    let mut h = H::aimed();
    let op = h.reach_releasing();
    h.advance(1);
    let out = h.released(op, Ok(Warp::Skipped));
    // A skipped warp can have moved the pointer: recover immediately with the gate open.
    assert!(home_failed(&out, HomeFailure::Warp), "{out:?}");
    assert_eq!(end_controls(&out).len(), 1);
    let leave = warp(&out).expect("the fallback warp");
    assert_eq!(leave.1, FALLBACK);
    assert!(has_hud_hide(&out));
    let started = h.now_ms();
    let out = h.tick(started + END_TIMEOUT);
    assert!(
        !has_end_capture(&out) && warps(&out).is_empty(),
        "no Returning: {out:?}"
    );
    // Confirmation still fails: retry every second while the gate remains open.
    h.released(leave.0, Ok(Warp::Skipped));
    let t = h.now_ms();
    assert!(warps(&h.tick(t + STRANDED_RETRY - 1)).is_empty());
    let out = h.tick(t + STRANDED_RETRY);
    let retry = warp(&out).expect("retry");
    assert_eq!(retry.1, FALLBACK);
    h.released(retry.0, Ok(Warp::Skipped));
    let t = h.now_ms();
    let out = h.tick(t + STRANDED_RETRY);
    let retry = warp(&out).expect("a second retry");
    // Until one is done.
    h.released(retry.0, Ok(Warp::Done));
    let t = h.now_ms();
    assert!(warps(&h.tick(t + 10 * STRANDED_RETRY)).is_empty());
    h.bind_set(bind(&h.log, false).unwrap(), false, true);
    h.quiet();
}

#[test]
fn entry_aborts_on_focus_timeout() {
    let mut h = H::aimed();
    let session = h.session.unwrap();
    let op = h.reach_focusing();
    let started = h.now_ms();
    let removal = bind(&h.log, false);
    assert!(
        removal.is_none(),
        "the bind stays while the focus is awaited"
    );
    let out = h.tick(started + FOCUS_TIMEOUT - 1);
    assert!(notices(&out).is_empty());
    let out = h.tick(started + FOCUS_TIMEOUT);
    // The capture is gone and the pointer is on the twin: warp it back, end the session.
    assert!(home_failed(&out, HomeFailure::Focus), "{out:?}");
    assert_eq!(warps(&out).len(), 1);
    assert_eq!(warps(&out)[0].1, FALLBACK);
    assert_eq!(end_controls(&out), vec![(B, session, EndReason::Released)]);
    assert!(bind(&out, false).is_some());
    assert_eq!(h.engine.controlling(), None);
    let _ = op;
    assert!(!has_end_capture(&out));
    h.quiet();
}

#[test]
fn entry_aborts_on_gate_close() {
    for step in 0..3 {
        let mut h = H::aimed();
        let session = h.session.unwrap();
        match step {
            0 => {
                h.reach_binding();
            }
            1 => {
                h.reach_releasing();
            }
            _ => {
                h.reach_focusing();
            }
        }
        h.advance(1);
        let out = h.feed(locked());
        assert!(
            home_failed(&out, HomeFailure::Guard),
            "step {step}: {out:?}"
        );
        assert_eq!(
            end_controls(&out),
            vec![(B, session, EndReason::ControllerLocked)],
            "step {step}"
        );
        assert!(
            bind(&out, false).is_some(),
            "step {step}: the bind is removed"
        );
        if step == 0 {
            // The capture is live and the pointer physical: the ordinary end.
            assert!(has_end_capture(&out) && warps(&out).is_empty(), "{out:?}");
        } else {
            assert_eq!(warps(&out).len(), 1, "step {step}: {out:?}");
            assert_eq!(warps(&out)[0].1, FALLBACK);
            assert!(!has_end_capture(&out));
        }
        assert_eq!(h.engine.controlling(), None);
        h.quiet();
    }
}

#[test]
fn stale_op_acknowledgement_ignored() {
    let mut h = H::aimed();
    let op = h.reach_binding();
    let stale = HomeOp(op.0.wrapping_sub(1));
    // An answer to some other operation: nothing happens.
    let out = h.bind_set(stale, true, true);
    assert!(out.is_empty(), "{out:?}");
    let out = h.bind_set(HomeOp(op.0 + 50), true, true);
    assert!(out.is_empty(), "{out:?}");
    // The same operation answered with the wrong direction: the removal's answer is not the
    // install's.
    let out = h.bind_set(op, false, true);
    assert!(out.is_empty(), "{out:?}");
    h.advance(1);
    let out = h.bind_set(op, true, true);
    assert!(warp(&out).is_some());
    // A release answer with an unknown operation, then with the install's older one.
    let out = h.released(HomeOp(op.0 + 50), Ok(Warp::Done));
    assert!(out.is_empty(), "{out:?}");
    let out = h.released(stale, Err(Failure::Other));
    assert!(out.is_empty(), "{out:?}");
    h.advance(1);
    let out = h.released(op, Ok(Warp::Done));
    assert_eq!(activations(&out), vec![W1]);
    // The answer, repeated, changes nothing.
    let out = h.released(op, Ok(Warp::Done));
    assert!(out.is_empty(), "{out:?}");
    h.quiet();

    // An answer of an earlier attempt can't satisfy a newer one.
    let mut h = H::aimed();
    h.now = ms(3_000);
    h.motion(1.0, 0.0);
    let first = h.reach_binding();
    h.advance(1);
    let out = h.bind_set(first, true, false);
    let removal = bind(&out, false).unwrap();
    h.bind_set(removal, false, true);
    h.now = ms(3_000 + HOME_RETRY + 10);
    h.motion(1.0, 0.0);
    let second = h.reach_binding();
    assert_ne!(first, second);
    let out = h.bind_set(first, true, true);
    assert!(
        out.is_empty(),
        "the first attempt's answer is stale: {out:?}"
    );
    h.advance(1);
    let out = h.bind_set(second, true, true);
    assert!(warp(&out).is_some());
    h.quiet();
}

// ---------------------------------------------------------------------------------------------
// HOME
// ---------------------------------------------------------------------------------------------

const P3: ProjectionId = ProjectionId(3);
const TWIN3: DisplayId = DisplayId(9);

fn proxy2() -> Proxy {
    Proxy {
        display: REMOTE,
        origin: point(650.0, 100.0),
        size: PixelSize::new(300, 200),
    }
}

fn content2() -> PixelRect {
    rect(30, 30, 330, 230)
}

impl H {
    /// `projected`, plus a second window projected to B (P2) and a third to C (P3).
    fn with_extra_sources() -> H {
        let mut h = H::projected();
        h.project(W2, B, P2, TWIN2, content2(), PlatformParking::Twin);
        h.place(B, P2, 1, Some(proxy2()));
        h.feed(Input::PeerUp { peer: C });
        h.project(
            W3,
            C,
            P3,
            TWIN3,
            rect(10, 10, 310, 210),
            PlatformParking::Twin,
        );
        h
    }

    /// Every kind of projection input from every source and both peers, plus focus requests and
    /// an incoming `StartControl`. Nothing may be injected and no window activated.
    fn storm(&mut self) -> Vec<Output> {
        let mut all = Vec::new();
        for (peer, projection) in [(B, P1), (B, P2), (C, P3)] {
            let base = self.seqs.get(&(peer, projection)).copied().unwrap_or(0);
            self.seqs.insert((peer, projection), base + 7);
            let batch = vec![
                ProjInput::Key {
                    projection,
                    seq: base + 1,
                    usage: KEY,
                    down: true,
                },
                ProjInput::Key {
                    projection,
                    seq: base + 2,
                    usage: KEY,
                    down: false,
                },
                ProjInput::Button {
                    projection,
                    seq: base + 3,
                    button: BUTTON,
                    down: true,
                    position: point(10.0, 10.0),
                },
                ProjInput::Button {
                    projection,
                    seq: base + 4,
                    button: BUTTON,
                    down: false,
                    position: point(10.0, 10.0),
                },
                ProjInput::Scroll {
                    projection,
                    seq: base + 5,
                    delta: scroll_delta(),
                    position: point(10.0, 10.0),
                },
                ProjInput::Motion {
                    projection,
                    seq: base + 6,
                    position: point(20.0, 20.0),
                },
                ProjInput::Held {
                    projection,
                    seq: base + 7,
                    keys: vec![KEY],
                    buttons: vec![BUTTON],
                },
            ];
            for msg in batch {
                let out = self.feed(proj_input(peer, msg));
                assert!(!has_inject(&out), "nothing is injected: {out:?}");
                all.extend(out);
            }
            for focused in [true, false] {
                let out = self.feed(projection_msg(
                    peer,
                    Message::Focus {
                        projection,
                        focused,
                    },
                ));
                assert!(activations(&out).is_empty(), "no focus change: {out:?}");
                assert!(!has_inject(&out));
                all.extend(out);
            }
        }
        // A third node asks to control this one: refused, whatever the state.
        let out = self.feed(control(
            C,
            ControlMessage::StartControl {
                session: SessionId(9),
                entry_display: LOCAL,
                entry: point(1.0, 1.0),
                lock_keys: LockKeys::default(),
            },
        ));
        assert!(
            out.contains(&Output::SendControl {
                peer: C,
                msg: ControlMessage::ControlRefused {
                    session: SessionId(9),
                    reason: Refusal::Busy,
                },
            }),
            "{out:?}"
        );
        assert!(!has_inject(&out));
        all.extend(out);
        all
    }
}

#[test]
fn home_filters_all_sources() {
    let mut h = H::with_extra_sources();
    h.cross();
    h.focus(Some(W1));
    h.aim();
    h.home_now();
    let out = h.storm();
    assert!(!has_inject(&out));
    // The filter only advanced `last_seq`: once it is lifted, every sequence number the sources
    // consumed is rejected (a stale input must not become eligible), and the next one is taken.
    // The numbers come from the harness's own counters, which prove nothing about the engine, so
    // each is replayed.
    let consumed: Vec<_> = [(B, P1, W1), (B, P2, W2), (C, P3, W3)]
        .into_iter()
        .map(|(peer, projection, window)| (peer, projection, window, h.seqs[&(peer, projection)]))
        .collect();
    assert!(
        consumed.iter().all(|(_, _, _, used)| *used >= 7),
        "{consumed:?}"
    );
    let exit = h.exit_through(Edge::Right, 0.25);
    h.confirm_removal(&exit);
    for (peer, projection, window, used) in consumed {
        h.focus(Some(window));
        for seq in 1..=used {
            let out = h.feed(proj_input(
                peer,
                ProjInput::Key {
                    projection,
                    seq,
                    usage: KEY,
                    down: true,
                },
            ));
            assert!(
                !has_inject(&out),
                "{peer:?} {projection:?}: sequence {seq} was already consumed: {out:?}"
            );
        }
        assert!(
            h.probe_in(peer, projection, window),
            "{peer:?} {projection:?}: a fresh sequence is taken"
        );
    }
    h.quiet();
}

#[test]
fn every_filtered_input_kind_consumes_its_sequence() {
    // Whatever kind of input is the last one the filter sees before home ends, its sequence number
    // was consumed: once the filter is lifted, that number is refused (carried by the same
    // message, and by any other kind), and a fresh one is taken. (In `home_filters_all_sources`
    // every batch ends with a `Held`, which would hide an implementation that advanced the
    // sequence for that kind alone.)
    type Make = fn(u32) -> ProjInput;
    fn key(seq: u32, down: bool) -> ProjInput {
        ProjInput::Key {
            projection: P1,
            seq,
            usage: KEY,
            down,
        }
    }
    fn button(seq: u32, down: bool) -> ProjInput {
        ProjInput::Button {
            projection: P1,
            seq,
            button: BUTTON,
            down,
            position: point(10.0, 10.0),
        }
    }
    let kinds: [(&str, Make); 7] = [
        ("key down", |seq| key(seq, true)),
        ("key up", |seq| key(seq, false)),
        ("button down", |seq| button(seq, true)),
        ("button up", |seq| button(seq, false)),
        ("scroll", |seq| ProjInput::Scroll {
            projection: P1,
            seq,
            delta: scroll_delta(),
            position: point(10.0, 10.0),
        }),
        ("motion", |seq| ProjInput::Motion {
            projection: P1,
            seq,
            position: point(20.0, 20.0),
        }),
        ("held", |seq| ProjInput::Held {
            projection: P1,
            seq,
            keys: vec![KEY],
            buttons: vec![BUTTON],
        }),
    ];
    for (name, make) in kinds {
        let mut h = H::home();
        let seq = h.next_proj_seq(B, P1);
        let out = h.feed(proj_input(B, make(seq)));
        assert!(!has_inject(&out), "{name}: nothing is injected: {out:?}");
        // Home ends with that message the last one the filter saw.
        let exit = h.exit_through(Edge::Right, 0.25);
        h.confirm_removal(&exit);
        h.focus(Some(W1));
        assert!(
            !has_inject(&h.feed(proj_input(B, make(seq)))),
            "{name}: its own sequence {seq} was taken again"
        );
        assert!(
            !has_inject(&h.feed(proj_input(B, key(seq, true)))),
            "{name}: sequence {seq} was taken by a key press"
        );
        assert!(
            h.probe(P1),
            "{name}: a fresh sequence is taken once the filter is lifted"
        );
        h.quiet();
    }
}

#[test]
fn home_bind_provenance() {
    // From the `HomeBind { install: true }` output until the removal is confirmed, this node
    // injects nothing into its own seat, for any input stream: nothing a peer sends can press the
    // home bind (the compositor can't tell a virtual keyboard from a physical one).
    let mut h = H::with_extra_sources();
    h.cross();
    h.focus(Some(W1));
    h.aim();
    let op = h.reach_binding();
    h.storm();
    h.advance(1);
    let out = h.bind_set(op, true, true);
    assert!(warp(&out).is_some());
    h.storm();
    h.advance(1);
    h.released(op, Ok(Warp::Done));
    h.storm();
    assert!(
        h.log
            .iter()
            .any(|o| matches!(o, Output::Notice(Notice::Home { entered: true, .. })))
    );
    // Home, and exiting: the HUD, the activation.
    let (id, _) = h.exit_to_activating(Edge::Left, 0.5);
    h.storm();
    h.advance(1);
    h.feed(Input::Capture(CaptureEvent::Started { id }));
    h.storm();
    h.advance(1);
    let out = h.capture_begun(id, vec![]);
    // The removal is requested in the handle that ends home; nothing is injected until it is
    // confirmed, however much arrives.
    let removal = bind(&out, false).expect("removal");
    assert!(!has_inject(&out));
    h.storm();
    h.advance(1);
    h.bind_set(removal, false, false);
    h.storm();
    let retry = h.tick_after(100);
    let removal = bind(&retry, false).expect("a retry");
    h.storm();
    h.advance(1);
    h.bind_set(removal, false, true);
    // Confirmed: the filter is off.
    assert!(h.probe(P1));
    h.quiet();
}

#[test]
fn home_ignores_focus_messages() {
    let mut h = H::controlling();
    h.feed(Input::Windows(WindowEvent::Added(window(OTHER))));
    // Another window of this node has focus; B's proxy asks for focus: the window is activated
    // and the other one is remembered to be restored.
    h.focus(Some(OTHER));
    let out = h.feed(projection_msg(
        B,
        Message::Focus {
            projection: P1,
            focused: true,
        },
    ));
    assert_eq!(activations(&out), vec![W1]);
    h.focus(Some(W1));
    h.aim();
    h.home_now();
    // While home: no activation on gaining focus, none on losing it (no restore of `OTHER`).
    for focused in [true, false] {
        h.advance(300);
        let out = h.feed(projection_msg(
            B,
            Message::Focus {
                projection: P1,
                focused,
            },
        ));
        assert!(out.is_empty(), "{out:?}");
    }
    // Back from home and with the removal confirmed, losing focus restores the other window.
    let exit = h.exit_through(Edge::Right, 0.25);
    h.confirm_removal(&exit);
    h.advance(300);
    let out = h.feed(projection_msg(
        B,
        Message::Focus {
            projection: P1,
            focused: false,
        },
    ));
    assert_eq!(activations(&out), vec![OTHER]);
    h.quiet();
}

#[test]
fn home_heartbeats_continue() {
    let mut h = H::controlling();
    h.aim();
    h.home_now();
    let started = h.now_ms();
    let out = h.run_until(started + 1_000, true);
    let beats = heartbeats(&out);
    assert!(beats.len() >= 3, "heartbeats keep the lease alive: {out:?}");
    // Nothing is held on B while home: every heartbeat lists nothing.
    assert!(
        beats
            .iter()
            .all(|(keys, buttons)| keys.is_empty() && buttons.is_empty())
    );
    assert_eq!(h.engine.controlling(), Some(B), "the session stays open");
    h.quiet();
}

#[test]
fn local_pointer_follows_inside_the_proxy() {
    // Not home: this node's own pointer is not sent anywhere.
    let mut h = H::aimed();
    let out = h.feed(Input::LocalPointer {
        display: TWIN,
        position: point(150.0, 90.0),
    });
    assert!(out.is_empty(), "{out:?}");

    // Home: the pointer on the twin, at (100, 50) inside the content (50, 40), maps to the same
    // place in the proxy at (200, 300).
    let mut h = H::home();
    h.advance(1);
    let out = h.feed(Input::LocalPointer {
        display: TWIN,
        position: point(150.0, 90.0),
    });
    let sent = motions(&out);
    assert_eq!(sent.len(), 1, "{out:?}");
    assert_eq!(
        (sent[0].display, sent[0].position),
        (REMOTE, point(300.0, 350.0))
    );
    // Sequence numbers keep growing.
    let again = motions(&h.feed(Input::LocalPointer {
        display: TWIN,
        position: point(151.0, 90.0),
    }));
    assert_eq!(again[0].seq, sent[0].seq + 1);
    // The padding around the content (the bar's area) clamps to its edge.
    let out = h.feed(Input::LocalPointer {
        display: TWIN,
        position: point(0.0, 10_000.0),
    });
    assert_eq!(motions(&out)[0].position, point(200.0, 599.0));
    // Another display, or a position that isn't finite, is ignored.
    assert!(
        h.feed(Input::LocalPointer {
            display: LOCAL,
            position: point(150.0, 90.0)
        })
        .is_empty()
    );
    assert!(
        h.feed(Input::LocalPointer {
            display: TWIN,
            position: point(f64::NAN, 90.0)
        })
        .is_empty()
    );
    h.quiet();
}

#[test]
fn home_refuses_start_control() {
    let mut h = H::controlling();
    h.aim();
    h.home_now();
    let out = h.feed(control(
        C,
        ControlMessage::StartControl {
            session: SessionId(5),
            entry_display: LOCAL,
            entry: point(1.0, 1.0),
            lock_keys: LockKeys::default(),
        },
    ));
    assert!(out.contains(&Output::SendControl {
        peer: C,
        msg: ControlMessage::ControlRefused {
            session: SessionId(5),
            reason: Refusal::Busy
        }
    }));
    assert!(has_notice(
        &out,
        &Notice::Refused {
            peer: C,
            reason: Refusal::Busy
        }
    ));
    h.quiet();
}

// ---------------------------------------------------------------------------------------------
// EXIT
// ---------------------------------------------------------------------------------------------

impl H {
    /// Home, with the pointer on the standard proxy.
    fn home() -> H {
        let mut h = H::controlling();
        h.aim();
        h.home_now();
        h
    }

    fn press(&mut self, edge: Edge, t: f64) -> Vec<Output> {
        let portal = self.strip(0, edge);
        self.feed(Input::Capture(CaptureEvent::EdgePressed {
            portal,
            position: t,
            at: self.now,
        }))
    }

    fn visible(&mut self) -> Vec<Output> {
        self.feed(Input::Overlay(OverlayEvent::Visible(HUD)))
    }

    fn started(&mut self, id: CaptureId) -> Vec<Output> {
        self.feed(Input::Capture(CaptureEvent::Started { id }))
    }

    fn ended(&mut self, id: CaptureId, reason: CaptureEnd) -> Vec<Output> {
        self.feed(Input::Capture(CaptureEvent::Ended { id, reason }))
    }
}

#[test]
fn exit_shows_hud_before_capture() {
    let mut h = H::home();
    let out = h.press(Edge::Right, 0.25);
    // The HUD goes on this node's own display, never on the twin, and no capture begins before
    // it is visible (04 §8 invariant 5).
    let shown: Vec<_> = out
        .iter()
        .filter_map(|o| match o {
            Output::ShowOverlay { id, overlay } if *id == HUD => Some(overlay.display),
            _ => None,
        })
        .collect();
    assert_eq!(shown, vec![LOCAL]);
    assert!(begin_capture(&out).is_none());
    // The platform repeats the press while the pointer keeps pushing: nothing more happens.
    h.advance(10);
    let out = h.press(Edge::Right, 0.25);
    assert!(out.is_empty(), "{out:?}");
    h.advance(10);
    let out = h.visible();
    let (id, portal, drain_first) = begin_capture(&out).expect("BeginCapture");
    assert_eq!(portal, h.strip(0, Edge::Right));
    assert!(drain_first);
    // And not while the activation is under way.
    h.advance(10);
    assert!(h.press(Edge::Right, 0.25).is_empty());
    assert!(h.press(Edge::Left, 0.5).is_empty());
    h.started(id);
    h.advance(1);
    let out = h.capture_begun(id, vec![]);
    assert!(left_home(&out));
    h.confirm_removal(&out);
    h.quiet();
}

#[test]
fn exit_hud_unavailable_retry_fence() {
    // The HUD can't be shown: back to home, and the strip is quiet for HOME_RETRY.
    let mut h = H::home();
    h.advance(100);
    let out = h.press(Edge::Right, 0.25);
    assert!(has_hud_show(&out));
    h.advance(1);
    let out = h.feed(Input::Overlay(OverlayEvent::Unavailable(HUD)));
    assert!(has_hud_hide(&out));
    assert!(begin_capture(&out).is_none());
    let failed_at = h.now_ms();
    h.advance(HOME_RETRY - 1);
    assert!(
        !has_hud_show(&h.press(Edge::Right, 0.25)),
        "inside the fence"
    );
    // WP-2.43j: after an `Unavailable` the HUD itself is quarantined for `HUD_STALE` (a later
    // outcome of the same show may still come): other strips wait too.
    assert!(!has_hud_show(&h.press(Edge::Left, 0.5)));
    h.now = ms(failed_at + HOME_RETRY);
    assert!(
        has_hud_show(&h.press(Edge::Right, 0.25)),
        "the fence expired"
    );
    // A HUD that never becomes visible times out: no capture, home again, fenced.
    let shown = h.now_ms();
    let out = h.tick(shown + HUD_TIMEOUT - 1);
    assert!(!has_hud_hide(&out));
    let out = h.tick(shown + HUD_TIMEOUT);
    assert!(has_hud_hide(&out) && begin_capture(&out).is_none());
    h.advance(1);
    assert!(!has_hud_show(&h.press(Edge::Right, 0.25)));
    // A HUD that becomes visible after its deadline is not enough either.
    h.now = ms(shown + HUD_TIMEOUT + HOME_RETRY);
    h.press(Edge::Right, 0.25);
    h.advance(HUD_TIMEOUT + 5);
    let out = h.visible();
    assert!(begin_capture(&out).is_none(), "{out:?}");
    assert!(has_hud_hide(&out));
    h.quiet();
}

#[test]
fn exit_both_callback_orders() {
    for started_first in [true, false] {
        let mut h = H::home();
        let (id, _) = h.exit_to_activating(Edge::Right, 0.25);
        h.advance(1);
        let out = if started_first {
            let out = h.started(id);
            assert!(out.is_empty());
            h.advance(1);
            h.capture_begun(id, vec![])
        } else {
            let out = h.capture_begun(id, vec![]);
            h.advance(1);
            let started = h.started(id);
            assert!(started.is_empty(), "{started:?}");
            out
        };
        assert!(left_home(&out), "started_first={started_first}: {out:?}");
        assert_eq!(motions(&out).len(), 1);
        assert_eq!(h.engine.controlling(), Some(B));
        // The session is ordinary again: motion is routed.
        h.advance(1);
        let out = h.motion(1.0, 0.0);
        assert_eq!(motions(&out).len(), 1, "started_first={started_first}");
        h.confirm_removal(&h.log.clone());
        h.quiet();
    }
}

#[test]
fn exit_sends_initial_motion_and_lock_keys() {
    let mut h = H::home();
    let (id, _) = h.exit_to_activating(Edge::Right, 0.25);
    h.advance(1);
    h.started(id);
    // A key held at the activation is reported in the snapshot and never replayed remotely.
    let out = h.capture_begun(id, vec![LCTRL]);
    // A press at 0.25 along the right strip maps to just outside the proxy's right edge:
    // (200 + 400, 300 + 0.25 * 300) on B's display, and B's cursor leaves the proxy at once.
    let sent = motions(&out);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        (sent[0].display, sent[0].position),
        (REMOTE, point(600.0, 375.0))
    );
    assert_eq!(sent[0].session, h.session.unwrap());
    // Lock keys may have changed in the window.
    assert_eq!(
        lock_keys_sent(&out),
        vec![LockKeys {
            caps_lock: Some(true),
            num_lock: None,
            scroll_lock: None
        }]
    );
    assert!(
        sent_transitions(&out).is_empty(),
        "held keys are not forwarded: {out:?}"
    );
    h.confirm_removal(&out);
    h.quiet();
}

#[test]
fn exit_replaces_chord_keys_from_snapshot() {
    // A modifier held when the pointer went into the window is released natively while home, which
    // this controller never saw: the exit's snapshot replaces the chord state.
    let mut h = H::controlling();
    h.feed(Input::Capture(CaptureEvent::Key {
        usage: LCTRL,
        down: true,
        at: h.now,
    }));
    h.aim();
    h.home_now();
    let exit = h.exit_through(Edge::Right, 0.25);
    h.confirm_removal(&exit);
    // Shift+Alt+Esc without Control is not the chord (the stale Control was dropped): all three
    // are routed to B as ordinary keys.
    let mut all = Vec::new();
    for usage in [LSHIFT, LALT, ESC] {
        h.advance(1);
        all.extend(h.feed(Input::Capture(CaptureEvent::Key {
            usage,
            down: true,
            at: h.now,
        })));
    }
    assert!(end_controls(&all).is_empty(), "{all:?}");
    assert_eq!(h.engine.controlling(), Some(B));
    assert_eq!(sent_transitions(&all).len(), 3);
    for usage in [LSHIFT, LALT, ESC] {
        h.feed(Input::Capture(CaptureEvent::Key {
            usage,
            down: false,
            at: h.now,
        }));
    }
    h.quiet();

    // With the snapshot saying Control is held, the same keys are the chord.
    let mut h = H::home();
    let (id, _) = h.exit_to_activating(Edge::Right, 0.25);
    h.started(id);
    h.advance(1);
    let out = h.capture_begun(id, vec![LCTRL]);
    h.confirm_removal(&out);
    let mut all = Vec::new();
    for usage in [LSHIFT, LALT, ESC] {
        h.advance(1);
        all.extend(h.feed(Input::Capture(CaptureEvent::Key {
            usage,
            down: true,
            at: h.now,
        })));
    }
    assert_eq!(
        end_controls(&all),
        vec![(B, h.session.unwrap(), EndReason::Released)],
        "{all:?}"
    );
    h.quiet();
}

#[test]
fn exit_button_held_stays_home() {
    // `begin` refuses: a button is held (a drag in W never leaks).
    let mut h = H::home();
    let (id, _) = h.exit_to_activating(Edge::Right, 0.25);
    h.advance(1);
    let out = h.feed(Input::CaptureBegun {
        id,
        result: Err(Failure::PointerButtonHeld),
    });
    assert!(has_hud_hide(&out));
    assert!(end_controls(&out).is_empty() && motions(&out).is_empty());
    let failed_at = h.now_ms();
    h.advance(10);
    assert!(!has_hud_show(&h.press(Edge::Right, 0.25)), "fenced");
    h.now = ms(failed_at + HOME_RETRY);
    assert!(has_hud_show(&h.press(Edge::Right, 0.25)));
    h.advance(1);
    h.feed(Input::Overlay(OverlayEvent::Unavailable(HUD)));
    h.quiet();

    // A button goes down between `Started` and `CaptureBegun`: the exit is cancelled and the
    // pointer stays on the twin.
    let mut h = H::home();
    let (id, _) = h.exit_to_activating(Edge::Right, 0.25);
    h.advance(1);
    h.started(id);
    h.advance(1);
    h.feed(Input::Capture(CaptureEvent::Button {
        button: BUTTON,
        down: true,
        at: h.now,
    }));
    h.advance(1);
    let out = h.capture_begun(id, vec![]);
    assert!(
        motions(&out).is_empty() && end_controls(&out).is_empty(),
        "{out:?}"
    );
    // Released with a warp to where the strip was pressed on the twin (the right edge of the
    // content, 0.25 along it), one pixel inside.
    let (op, target) = warp(&out).expect("the capture is released");
    assert_eq!(target, (TWIN, point(449.0, 40.0 + 0.25 * 299.0)));
    assert!(has_hud_hide(&out));
    assert!(bind(&out, false).is_none(), "the bind stays: still home");
    h.released(op, Ok(Warp::Done));
    h.advance(1);
    h.ended(id, CaptureEnd::Requested);
    let cancelled_at = h.now_ms();
    // The button's native up reaches W unseen. The next exit is not affected by it.
    h.advance(10);
    assert!(!has_hud_show(&h.press(Edge::Right, 0.25)), "fenced");
    h.now = ms(cancelled_at + HOME_RETRY);
    let exit = h.exit_through(Edge::Right, 0.25);
    assert!(left_home(&exit), "{exit:?}");
    h.confirm_removal(&exit);
    h.quiet();
}

#[test]
fn exit_timeout_cancels_late_success() {
    let mut h = H::home();
    let (id, _) = h.exit_to_activating(Edge::Right, 0.25);
    let begun = h.now_ms();
    assert!(!has_end_capture(&h.tick(begun + START_TIMEOUT - 1)));
    let out = h.tick(begun + START_TIMEOUT);
    // Cancel with capture: end it without moving the pointer (it stays on the twin, which is
    // home), hide the HUD.
    assert!(has_end_capture(&out) && has_hud_hide(&out), "{out:?}");
    // The late success: the capture is ended again (idempotent), and nothing else happens.
    h.advance(1);
    let out = h.capture_begun(id, vec![]);
    assert!(has_end_capture(&out));
    assert!(motions(&out).is_empty() && end_controls(&out).is_empty());
    h.advance(1);
    let out = h.ended(id, CaptureEnd::Requested);
    assert!(out.is_empty(), "{out:?}");
    assert_eq!(h.engine.controlling(), Some(B));
    h.quiet();
}

#[test]
fn exit_cancelled_serialises_next_press() {
    let mut h = H::home();
    let (a, _) = h.exit_to_activating(Edge::Right, 0.25);
    let begun = h.now_ms();
    h.tick(begun + START_TIMEOUT);
    // A press while the cancellation is outstanding is ignored: no new capture is ever begun
    // while one is being ended.
    h.advance(10);
    assert!(h.press(Edge::Left, 0.5).is_empty());
    // The late success of A ends it again.
    let out = h.capture_begun(a, vec![]);
    assert!(has_end_capture(&out));
    // Its end resolves the cancellation.
    h.advance(1);
    h.ended(a, CaptureEnd::Requested);
    let resolved = h.now_ms();
    // Still fenced for HOME_RETRY after the cancellation; after it the next press begins B.
    h.now = ms(begun + START_TIMEOUT + HOME_RETRY);
    let _ = resolved;
    let (b, _) = h.exit_to_activating(Edge::Right, 0.25);
    assert!(b > a, "capture ids increase");
    h.advance(1);
    h.started(b);
    h.advance(1);
    let out = h.capture_begun(b, vec![]);
    assert!(left_home(&out));
    h.confirm_removal(&out);
    h.quiet();
}

#[test]
fn older_capture_success_ignored() {
    let mut h = H::home();
    let (a, _) = h.exit_to_activating(Edge::Right, 0.25);
    let begun = h.now_ms();
    h.tick(begun + START_TIMEOUT);
    h.advance(1);
    h.ended(a, CaptureEnd::Requested);
    h.now = ms(begun + START_TIMEOUT + HOME_RETRY);
    let (b, _) = h.exit_to_activating(Edge::Right, 0.25);
    assert!(b > a);
    // B is live (activating): a late success for the older A proves nothing and must never end B.
    h.advance(1);
    let out = h.capture_begun(a, vec![]);
    assert!(!has_end_capture(&out), "{out:?}");
    assert!(out.is_empty(), "{out:?}");
    h.advance(1);
    h.started(b);
    h.advance(1);
    let out = h.capture_begun(b, vec![]);
    assert!(left_home(&out), "{out:?}");
    // Nor a duplicate of it for an older id afterwards, while B is the live capture.
    h.advance(1);
    let out = h.capture_begun(a, vec![]);
    assert!(!has_end_capture(&out), "{out:?}");
    h.confirm_removal(&h.log.clone());
    h.quiet();
}

#[test]
fn exit_success_after_deadline_before_tick() {
    let mut h = H::home();
    let (id, _) = h.exit_to_activating(Edge::Right, 0.25);
    h.started(id);
    // The answer arrives after the deadline and before any tick: checked on arrival.
    h.advance(START_TIMEOUT + 5);
    let out = h.capture_begun(id, vec![]);
    assert!(has_end_capture(&out) && has_hud_hide(&out), "{out:?}");
    assert!(motions(&out).is_empty() && !left_home(&out), "{out:?}");
    h.ended(id, CaptureEnd::Requested);
    h.quiet();
}

#[test]
fn exit_removes_bind_before_lifting_filter() {
    let mut h = H::home();
    let out = h.exit_through(Edge::Right, 0.25);
    let removal = bind(&out, false).expect("the bind removal is requested with the exit");
    assert!(!has_inject(&out));
    // Until the removal is confirmed the seat stays arbitrated: nothing a peer sends is injected.
    assert!(!h.probe(P1));
    let storm = h.storm();
    assert!(!has_inject(&storm));
    // A failed removal changes nothing; the confirmed one lifts the filter.
    h.advance(1);
    h.bind_set(removal, false, false);
    assert!(!h.probe(P1));
    let retry = h.tick_after(100);
    let again = bind(&retry, false).expect("retried");
    assert_ne!(again, removal);
    // An answer of the earlier attempt is stale.
    h.bind_set(removal, false, true);
    assert!(!h.probe(P1), "the earlier attempt's answer does not count");
    h.bind_set(again, false, true);
    assert!(h.probe(P1));
    h.quiet();
}

#[test]
fn exit_duplicate_presses() {
    let mut h = H::home();
    let out = h.press(Edge::Right, 0.25);
    assert!(has_hud_show(&out));
    for _ in 0..3 {
        h.advance(5);
        assert!(h.press(Edge::Right, 0.26).is_empty());
    }
    h.advance(1);
    let out = h.visible();
    let (id, _, _) = begin_capture(&out).unwrap();
    // A second `Visible` does not begin a second capture.
    assert!(h.visible().is_empty());
    for _ in 0..3 {
        h.advance(5);
        assert!(h.press(Edge::Right, 0.27).is_empty());
    }
    h.started(id);
    h.advance(1);
    let out = h.capture_begun(id, vec![]);
    assert!(left_home(&out));
    // After the exit the session is ordinary: a stray press on a twin strip does nothing.
    h.advance(1);
    assert!(h.press(Edge::Right, 0.25).is_empty());
    h.confirm_removal(&out);
    h.quiet();
}

#[test]
fn exit_rebinds_to_strip_generation() {
    let moved = |x: f64| Proxy {
        origin: point(x, 300.0),
        ..Proxy::standard()
    };
    // A change of the strip set during the HUD cancels that exit: the proxy now touches the
    // right edge of B's display, so the right strip goes away.
    let mut h = H::home();
    h.press(Edge::Left, 0.5);
    h.advance(1);
    let out = h.place(B, P1, 2, Some(moved(600.0)));
    assert!(has_hud_hide(&out), "{out:?}");
    assert!(
        set_portals(&out)
            .last()
            .is_some_and(|p| p.iter().filter(|c| c.display == TWIN).count() == 3),
        "{out:?}"
    );
    h.advance(1);
    let out = h.visible();
    assert!(begin_capture(&out).is_none(), "{out:?}");
    // The next press starts a fresh exit (after the retry fence).
    h.advance(HOME_RETRY);
    let exit = h.exit_through(Edge::Left, 0.5);
    assert!(left_home(&exit));
    h.confirm_removal(&exit);
    h.quiet();

    // During the activation, a placement change that keeps the strips: the exit completes with
    // the current placement, not the one at the press.
    let mut h = H::home();
    let (id, _) = h.exit_to_activating(Edge::Left, 0.5);
    h.advance(1);
    h.place(B, P1, 2, Some(moved(220.0)));
    h.started(id);
    h.advance(1);
    let out = h.capture_begun(id, vec![]);
    assert!(left_home(&out), "{out:?}");
    assert_eq!(motions(&out)[0].position, point(219.0, 450.0));
    h.confirm_removal(&out);
    h.quiet();

    // During the activation, the placement becomes incoherent: leave home with the live capture.
    let mut h = H::home();
    let session = h.session.unwrap();
    let (id, _) = h.exit_to_activating(Edge::Left, 0.5);
    h.advance(1);
    h.place(
        B,
        P1,
        2,
        Some(Proxy {
            size: PixelSize::new(399, 300),
            ..Proxy::standard()
        }),
    );
    h.started(id);
    h.advance(1);
    let out = h.capture_begun(id, vec![]);
    assert!(home_failed(&out, HomeFailure::Gone), "{out:?}");
    assert_eq!(end_controls(&out), vec![(B, session, EndReason::Released)]);
    assert_eq!(warps(&out)[0].1, FALLBACK);
    assert!(motions(&out).is_empty());
    h.ended(id, CaptureEnd::Requested);
    h.confirm_removal(&out);
    h.quiet();

    // During the activation, the strip set changes: cancel with capture.
    let mut h = H::home();
    let (id, _) = h.exit_to_activating(Edge::Left, 0.5);
    h.advance(1);
    h.place(B, P1, 2, Some(moved(600.0)));
    h.started(id);
    h.advance(1);
    let out = h.capture_begun(id, vec![]);
    assert!(has_end_capture(&out) && motions(&out).is_empty(), "{out:?}");
    assert_eq!(h.engine.controlling(), Some(B));
    h.ended(id, CaptureEnd::Requested);
    h.quiet();
}

#[test]
fn reentry_fence() {
    let mut h = H::home();
    let exit = h.exit_through(Edge::Right, 0.25);
    let exited = h.now_ms();
    h.confirm_removal(&exit);
    // The tracker starts one pixel outside the proxy: a report can't enter from there.
    h.advance(10);
    h.motion(-1.0, 0.0);
    let out = h.report(P1, point(399.0, 75.0));
    assert_no_entry(&h, &out);
    // Move back inside within the fence: the report still can't enter.
    h.now = ms(exited + 50);
    h.motion(-100.0, 0.0);
    let out = h.report(P1, point(300.0, 75.0));
    assert_no_entry(&h, &out);
    // After the fence, with fresh motion, it can.
    h.now = ms(exited + REENTRY_GUARD + 1);
    h.motion(1.0, 0.0);
    let out = h.report(P1, point(301.0, 75.0));
    assert!(bind(&out, true).is_some(), "{out:?}");
    h.quiet();

    // Without fresh motion (the pointer has been still) it can't either.
    let mut h = H::home();
    let exit = h.exit_through(Edge::Right, 0.25);
    let exited = h.now_ms();
    h.confirm_removal(&exit);
    h.now = ms(exited + 20);
    h.motion(-100.0, 0.0);
    h.now = ms(exited + 20 + LOCAL_MOTION_AGE + 1);
    let out = h.report(P1, point(300.0, 75.0));
    assert_no_entry(&h, &out);
    h.quiet();
}

fn strip_edges(out: &[Output]) -> BTreeSet<&'static str> {
    set_portals(out)
        .last()
        .map(|portals| {
            portals
                .iter()
                .filter(|p| p.display == TWIN)
                .map(|p| match p.edge {
                    Edge::Left => "left",
                    Edge::Right => "right",
                    Edge::Top => "top",
                    Edge::Bottom => "bottom",
                })
                .collect()
        })
        .unwrap_or_default()
}

fn flush_placements(displays: &[(NodeId, u32, f64, f64)]) -> Vec<Placement> {
    displays
        .iter()
        .map(|&(node, id, x, y)| Placement {
            node,
            display: DisplayId(id),
            origin: PointMm::new(x, y),
            version: 2,
        })
        .collect()
}

fn flush_layout(displays: &[(NodeId, u32, f64, f64)]) -> Layout {
    Layout::new(
        displays
            .iter()
            .map(|&(node, id, x, y)| Placed {
                id: GlobalDisplayId {
                    node,
                    display: DisplayId(id),
                },
                geometry: display(id).geometry,
                origin: PointMm::new(x, y),
            })
            .collect(),
        H::config().layout,
    )
    .unwrap()
}

fn fullscreen_host_home(displays: &[(NodeId, u32, f64, f64)]) -> H {
    fullscreen_host_home_with(flush_layout(displays).displays(), &[B, C])
}

fn flush_display(
    node: NodeId,
    id: u32,
    origin: (f64, f64),
    pixels: (u32, u32),
    millimetres: (f64, f64),
) -> Placed {
    let mut geometry = display(id).geometry;
    geometry.pixel_size = PixelSize::new(pixels.0, pixels.1);
    geometry.physical_size = SizeMm::new(millimetres.0, millimetres.1);
    Placed {
        id: GlobalDisplayId {
            node,
            display: DisplayId(id),
        },
        geometry,
        origin: PointMm::new(origin.0, origin.1),
    }
}

fn fullscreen_host_home_with(displays: &[Placed], reachable: &[NodeId]) -> H {
    let (mut h, geometry) = fullscreen_host_controlling_with(displays, reachable);
    let entry = motions(&h.motion(0.0, 0.0))[0].position;
    let delta = geometry.device_to_mm(point(50.0 - entry.x, 100.0 - entry.y));
    h.motion(delta.x / 0.1, delta.y / 0.1);
    h.home_now();
    h
}

/// B's window fullscreen on B's display (its proxy flush with every edge), and this node
/// controlling B; returns B's display geometry.
fn fullscreen_host_controlling_with(
    displays: &[Placed],
    reachable: &[NodeId],
) -> (H, DisplayGeometry) {
    let mut h = H::bare();
    for node in [A, B, C] {
        let infos: Vec<_> = displays
            .iter()
            .filter(|d| d.id.node == node)
            .map(|d| DisplayInfo {
                geometry: d.geometry,
                ..display(d.id.display.0)
            })
            .collect();
        if infos.is_empty() {
            continue;
        }
        if node == A {
            h.feed(Input::LocalDisplays(infos));
        } else {
            h.feed(Input::PeerDisplays {
                peer: node,
                displays: infos,
            });
            if reachable.contains(&node) {
                h.feed(Input::PeerUp { peer: node });
            }
        }
    }
    let layout = Layout::new(displays.to_vec(), H::config().layout).unwrap();
    h.layout_portal = layout
        .portals()
        .iter()
        .find(|p| p.from.node == A && p.to.node == B)
        .unwrap()
        .id;
    h.feed(Input::Layout(
        displays
            .iter()
            .map(|d| Placement {
                node: d.id.node,
                display: d.id.display,
                origin: d.origin,
                version: 2,
            })
            .collect(),
    ));
    let geometry = layout
        .get(GlobalDisplayId {
            node: B,
            display: REMOTE,
        })
        .unwrap()
        .geometry;
    h.project(
        W1,
        B,
        P1,
        TWIN,
        rect(
            0,
            0,
            geometry.pixel_size.width as i32,
            geometry.pixel_size.height as i32,
        ),
        PlatformParking::Twin,
    );
    h.place(
        B,
        P1,
        1,
        Some(Proxy {
            display: REMOTE,
            origin: point(0.0, 0.0),
            size: geometry.pixel_size,
        }),
    );
    h.cross();
    (h, geometry)
}

#[test]
fn fullscreen_host_right_exit_returns_to_controller_at_portal_point_with_hysteresis() {
    let mut h = fullscreen_host_home(&[(B, 1, 0.0, 0.0), (A, 1, 100.0, 0.0)]);
    assert_eq!(strip_edges(&h.log), ["right"].into());
    let session = h.session.unwrap();
    let out = h.exit_through(Edge::Right, 0.25);
    let id = h.last_begin();
    let (op, target) = warp(&out).expect("normal return releases and warps");
    assert_eq!(target, (LOCAL, point(0.0, 250.0)));
    assert_eq!(end_controls(&out), vec![(B, session, EndReason::Released)]);
    assert!(left_home(&out));
    assert!(motions(&out).is_empty());
    let returned_at = h.now_ms();
    h.confirm_removal(&out);
    h.released(op, Ok(Warp::Done));
    h.ended(id, CaptureEnd::Requested);
    assert_eq!(h.engine.controlling(), None);
    assert!(!has_hud_show(&h.feed(Input::Capture(
        CaptureEvent::EdgePressed {
            portal: h.layout_portal,
            position: 0.25,
            at: h.now,
        }
    ))));
    h.tick(returned_at + REENTRY_GUARD);
    let out = h.feed(Input::Capture(CaptureEvent::EdgePressed {
        portal: h.layout_portal,
        position: 0.25,
        at: h.now,
    }));
    assert!(
        has_hud_show(&out),
        "reverse portal re-arms after hysteresis: {out:?}"
    );
    h.feed(Input::Overlay(OverlayEvent::Unavailable(HUD)));
    h.quiet();
}

#[test]
fn fullscreen_host_flush_edge_without_continuation_is_not_offered() {
    let mut h = fullscreen_host_home(&[(A, 1, 0.0, 0.0), (B, 1, 100.0, 0.0)]);
    assert_eq!(strip_edges(&h.log), ["left"].into());
    let out = h.press(Edge::Right, 0.5);
    assert!(out.is_empty(), "no continuation, no exit: {out:?}");
    assert_eq!(h.engine.controlling(), Some(B));
    h.quiet();
}

#[test]
fn fullscreen_host_flush_exit_to_another_host_display_lands_and_continues() {
    let mut h = fullscreen_host_home(&[(A, 1, 0.0, 0.0), (B, 1, 100.0, 0.0), (B, 2, 200.0, 0.0)]);
    assert!(strip_edges(&h.log).contains("right"));
    let out = h.exit_through(Edge::Right, 0.25);
    let sent = motions(&out);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        (sent[0].display, sent[0].position),
        (DisplayId(2), point(0.0, 250.0))
    );
    assert_eq!(sent[0].session, h.session.unwrap());
    assert!(end_controls(&out).is_empty() && warps(&out).is_empty());
    h.confirm_removal(&out);
    assert_eq!(h.engine.controlling(), Some(B));
    let out = h.motion(10.0, 0.0);
    assert_eq!(motions(&out)[0].position, point(10.0, 250.0));
    h.quiet();
}

#[test]
fn nonflush_home_exit_mapping_is_unchanged_on_all_four_edges() {
    for (edge, expected) in [
        (Edge::Left, point(199.0, 375.0)),
        (Edge::Right, point(600.0, 375.0)),
        (Edge::Top, point(300.0, 299.0)),
        (Edge::Bottom, point(300.0, 600.0)),
    ] {
        let mut h = H::home();
        let out = h.exit_through(edge, 0.25);
        assert_eq!(motions(&out)[0].position, expected, "{edge:?}");
        assert_eq!(motions(&out)[0].display, REMOTE);
        assert_eq!(h.engine.controlling(), Some(B));
        h.confirm_removal(&out);
        h.quiet();
    }
    // Non-flush mapping continues to use the current display geometry during activation,
    // even while a layout confirmation is pending. Only flush continuation uses its snapshot.
    let mut h = H::home();
    // Keep the bottom strip available through a same-host neighbour before and after growth,
    // so this geometry change does not cancel activation by changing the offered strip set.
    h.feed(Input::PeerDisplays {
        peer: B,
        displays: vec![display(1), display(2)],
    });
    h.feed(Input::Layout(flush_placements(&[
        (A, 1, 0.0, 0.0),
        (B, 1, 100.0, 0.0),
        (B, 2, 100.0, 100.0),
    ])));
    h.place(
        B,
        P1,
        2,
        Some(Proxy {
            origin: point(200.0, 800.0),
            ..Proxy::standard()
        }),
    );
    let (id, _) = h.exit_to_activating(Edge::Right, 1.0);
    h.auto_portals = false;
    let mut larger = display(1);
    larger.geometry.pixel_size.height = 1200;
    h.feed(Input::PeerDisplays {
        peer: B,
        displays: vec![larger, display(2)],
    });
    h.started(id);
    let out = h.capture_begun(id, vec![]);
    assert_eq!(motions(&out)[0].position, point(600.0, 1100.0));
    h.confirm_removal(&out);
    h.quiet();
}

#[test]
fn flush_local_destination_removed_during_activation_uses_current_fallback() {
    let mut h = fullscreen_host_home(&[
        (A, 1, 0.0, 0.0),
        (B, 1, 100.0, 0.0),
        (A, 2, 200.0, -50.0),
        (C, 1, 200.0, 50.0),
    ]);
    let (id, _) = h.exit_to_activating(Edge::Right, 0.25);
    h.auto_portals = false;
    let update = h.feed(Input::LocalDisplays(vec![display(1)]));
    assert!(!set_portals(&update).is_empty());
    assert!(strip_edges(&update).contains("right"));
    h.started(id);
    let out = h.capture_begun(id, vec![]);
    assert!(home_failed(&out, HomeFailure::Gone));
    let (op, target) = warp(&out).unwrap();
    assert_eq!(target, FALLBACK);
    h.confirm_removal(&out);
    h.released(op, Ok(Warp::Skipped));
    h.ended(id, CaptureEnd::Requested);
    let retry = h.tick_after(STRANDED_RETRY);
    let (op, target) = warp(&retry).unwrap();
    assert_eq!(
        target, FALLBACK,
        "stranded retry never targets the removed A2"
    );
    h.released(op, Ok(Warp::Done));
    assert!(
        warps(&h.log)
            .iter()
            .all(|(_, (display, _))| *display != DisplayId(2))
    );
    h.quiet();
}

#[test]
fn normal_dimension_growth_behind_identical_strip_preserves_nonflush_mapping() {
    let mut h = fullscreen_host_home(&[(B, 1, 0.0, 0.0), (A, 1, 100.0, 0.0)]);
    let previous = set_portals(&h.log).last().unwrap().clone();
    let (id, _) = h.exit_to_activating(Edge::Right, 0.25);
    h.auto_portals = false;
    let mut larger = display(1);
    larger.geometry.pixel_size.width = 1200;
    let update = h.feed(Input::PeerDisplays {
        peer: B,
        displays: vec![larger],
    });
    assert_eq!(
        set_portals(&update)
            .last()
            .unwrap()
            .iter()
            .filter(|p| p.id.0 >= TWIN_PORTAL_BASE)
            .copied()
            .collect::<Vec<_>>(),
        previous
            .iter()
            .filter(|p| p.id.0 >= TWIN_PORTAL_BASE)
            .copied()
            .collect::<Vec<_>>()
    );
    h.started(id);
    let out = h.capture_begun(id, vec![]);
    assert!(warps(&out).is_empty() && end_controls(&out).is_empty());
    assert_eq!(
        (motions(&out)[0].display, motions(&out)[0].position),
        (REMOTE, point(1000.0, 250.0))
    );
    assert_eq!(h.engine.controlling(), Some(B));
    h.confirm_removal(&out);
    h.quiet();
}

#[test]
fn partial_flush_proxy_endpoints_cross_independently_of_tangent_containment() {
    for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
        for position in [0.0, 1.0] {
            let mut h = H::home();
            h.feed(Input::LocalDisplays(vec![display(1), display(2)]));
            let (neighbor, origin, expected) = match edge {
                Edge::Left => (
                    (0.0, 0.0),
                    point(0.0, 300.0),
                    (LOCAL, point(999.0, 300.0 + 300.0 * position)),
                ),
                Edge::Right => (
                    (200.0, 0.0),
                    point(600.0, 300.0),
                    (DisplayId(2), point(0.0, 300.0 + 300.0 * position)),
                ),
                Edge::Top => (
                    (100.0, -100.0),
                    point(200.0, 0.0),
                    (DisplayId(2), point(200.0 + 400.0 * position, 999.0)),
                ),
                Edge::Bottom => (
                    (100.0, 100.0),
                    point(200.0, 700.0),
                    (DisplayId(2), point(200.0 + 400.0 * position, 0.0)),
                ),
            };
            let mut displays = vec![(A, 1, 0.0, 0.0), (B, 1, 100.0, 0.0)];
            if edge != Edge::Left {
                displays.push((A, 2, neighbor.0, neighbor.1));
            }
            h.feed(Input::Layout(flush_placements(&displays)));
            h.place(
                B,
                P1,
                2,
                Some(Proxy {
                    display: REMOTE,
                    origin,
                    size: PixelSize::new(400, 300),
                }),
            );
            let out = h.exit_through(edge, position);
            assert_eq!(
                warp(&out).unwrap().1,
                expected,
                "{edge:?}, position {position}"
            );
            assert!(
                motions(&out).is_empty(),
                "endpoint must cross rather than resume on B"
            );
            h.confirm_removal(&out);
            h.quiet();
        }
    }
}

#[test]
fn unrelated_layout_changes_do_not_reoffer_portals_or_cancel_shown_home_exit_hud() {
    for flush in [false, true] {
        let mut h = if flush {
            fullscreen_host_home(&[(B, 1, 0.0, 0.0), (A, 1, 100.0, 0.0), (C, 1, 500.0, 500.0)])
        } else {
            let mut h = H::home();
            h.feed(Input::PeerDisplays {
                peer: C,
                displays: vec![display(1)],
            });
            h.feed(Input::PeerUp { peer: C });
            h.feed(Input::Layout(flush_placements(&[
                (A, 1, 0.0, 0.0),
                (B, 1, 100.0, 0.0),
                (C, 1, 500.0, 500.0),
            ])));
            h
        };
        assert!(has_hud_show(&h.press(Edge::Right, 0.25)));
        h.auto_portals = false;
        let (a_x, b_x) = if flush { (100.0, 0.0) } else { (0.0, 100.0) };
        let mut update = h.feed(Input::Layout(flush_placements(&[
            (A, 1, a_x, 0.0),
            (B, 1, b_x, 0.0),
            (C, 1, 700.0, 500.0),
        ])));
        let mut info = display(1);
        info.geometry.pixel_size.width = 1400;
        update.extend(h.feed(Input::PeerDisplays {
            peer: C,
            displays: vec![info],
        }));
        assert!(
            set_portals(&update).is_empty(),
            "unrelated layout must not be re-confirmed: {update:?}"
        );
        assert!(!has_hud_hide(&update));
        let visible = h.visible();
        let (id, _, _) =
            begin_capture(&visible).expect("shown HUD remains valid without a new answer");
        h.started(id);
        let out = h.capture_begun(id, vec![]);
        if flush {
            assert_eq!(warp(&out).unwrap().1, (LOCAL, point(0.0, 250.0)));
        } else {
            assert_eq!(motions(&out)[0].position, point(600.0, 375.0));
        }
        h.confirm_removal(&out);
        h.quiet();
    }
}

#[test]
fn off_midpoint_flush_portal_offers_full_strip_and_uncovered_press_stays_home_quietly() {
    let mut h = fullscreen_host_home(&[(B, 1, 0.0, 0.0), (A, 1, 100.0, 70.0)]);
    assert_eq!(strip_edges(&h.log), ["right"].into());
    let strip = set_portals(&h.log)
        .last()
        .unwrap()
        .iter()
        .find(|p| p.id == h.strip(0, Edge::Right))
        .copied()
        .unwrap();
    assert_eq!((strip.from, strip.to), (0.0, 1000.0));
    let out = h.press(Edge::Right, 0.25);
    assert!(
        out.is_empty(),
        "uncovered press has no HUD/capture/notice: {out:?}"
    );
    assert_eq!(h.engine.controlling(), Some(B));
    let out = h.exit_through(Edge::Right, 0.8);
    assert_eq!(warp(&out).unwrap().1, (LOCAL, point(0.0, 100.0)));
    h.confirm_removal(&out);
    h.quiet();
}

fn third_node_fullscreen_home() -> H {
    fullscreen_host_home(&[(A, 1, 0.0, 0.0), (B, 1, 100.0, 0.0), (C, 1, 200.0, 0.0)])
}

#[test]
fn flush_third_node_pending_handoff_revalidates_pointer_button_and_confirmed_mapping() {
    for change in ["pointer", "button", "mapping"] {
        let mut h = third_node_fullscreen_home();
        let old = h.session.unwrap();
        let exit = h.exit_through(Edge::Right, 0.25);
        match change {
            "pointer" => {
                assert_eq!(
                    motions(&h.motion(-10.0, 0.0))[0].position,
                    point(989.0, 250.0)
                );
            }
            "button" => {
                let down = h.feed(Input::Capture(CaptureEvent::Button {
                    button: BUTTON,
                    down: true,
                    at: h.now,
                }));
                assert_eq!(sent_transitions(&down), [(B, Held::Button(BUTTON), true)]);
            }
            "mapping" => {
                let update = h.feed(Input::Layout(flush_placements(&[
                    (A, 1, 0.0, 0.0),
                    (B, 1, 100.0, 0.0),
                    (C, 1, 200.0, -10.0),
                ])));
                assert!(
                    !set_portals(&update).is_empty(),
                    "new continuation is confirmed before removal"
                );
            }
            _ => unreachable!(),
        }
        let out = h.confirm_removal(&exit);
        assert!(start_control_to(&out).is_none(), "{change}: {out:?}");
        assert_eq!(end_controls(&out), [(B, old, EndReason::Released)]);
        let (op, target) = warp(&out).unwrap();
        assert_eq!(target, FALLBACK);
        if change == "button" {
            assert_eq!(sent_transitions(&out), [(B, Held::Button(BUTTON), false)]);
            h.feed(Input::Capture(CaptureEvent::Button {
                button: BUTTON,
                down: false,
                at: h.now,
            }));
        }
        h.released(op, Ok(Warp::Done));
        h.ended(h.last_begin(), CaptureEnd::Requested);
        h.quiet();
    }
}

#[test]
fn flush_third_node_handshake_refusal_and_timeout_release_retained_capture_to_desktop() {
    for timeout in [false, true] {
        let mut h = third_node_fullscreen_home();
        let exit = h.exit_through(Edge::Right, 0.25);
        let crossing = h.confirm_removal(&exit);
        let session = crossing
            .iter()
            .find_map(|o| match o {
                Output::SendControl {
                    peer: C,
                    msg: ControlMessage::StartControl { session, .. },
                } => Some(*session),
                _ => None,
            })
            .unwrap();
        assert!(begin_capture(&crossing).is_none());
        let out = if timeout {
            h.tick_after(START_TIMEOUT)
        } else {
            h.feed(control(
                C,
                ControlMessage::ControlRefused {
                    session,
                    reason: Refusal::Busy,
                },
            ))
        };
        let (op, target) = warp(&out).expect("retained twin capture needs a physical fallback");
        assert_eq!(target, FALLBACK);
        assert!(begin_capture(&out).is_none() && start_control_to(&out).is_none());
        h.released(op, Ok(Warp::Done));
        h.ended(h.last_begin(), CaptureEnd::Requested);
        assert_eq!(h.engine.controlling(), None);
        let late = h.feed(control(C, ControlMessage::ControlStarted { session }));
        assert!(begin_capture(&late).is_none());
        assert_eq!(h.engine.controlling(), None);
        h.quiet();
    }
}

#[test]
fn flush_third_node_reachability_controls_offers_without_removing_layout_display() {
    let displays = flush_layout(&[(A, 1, 0.0, 0.0), (B, 1, 100.0, 0.0), (C, 1, 200.0, 0.0)]);
    let mut h = fullscreen_host_home_with(displays.displays(), &[B]);
    assert_eq!(strip_edges(&h.log), ["left"].into());
    assert!(h.press(Edge::Right, 0.25).is_empty());
    assert_eq!(h.engine.controlling(), Some(B));
    let connected = h.feed(Input::PeerUp { peer: C });
    assert_eq!(strip_edges(&connected), ["left", "right"].into());
    let disconnected = h.feed(closed(C));
    assert_eq!(strip_edges(&disconnected), ["left"].into());
    assert!(!left_home(&disconnected));
    assert!(h.press(Edge::Right, 0.25).is_empty());
    assert_eq!(h.engine.controlling(), Some(B));
    h.feed(Input::PeerUp { peer: C });
    let exit = h.exit_through(Edge::Right, 0.25);
    let disconnected = h.feed(closed(C));
    assert!(start_control_to(&disconnected).is_none());
    let out = h.confirm_removal(&exit);
    assert!(
        start_control_to(&out).is_none(),
        "disconnect also invalidates a pending continuation"
    );
    let (op, target) = warp(&out).unwrap();
    assert_eq!(target, FALLBACK);
    h.released(op, Ok(Warp::Done));
    h.ended(h.last_begin(), CaptureEnd::Requested);
    h.quiet();
}

#[test]
fn flush_left_top_bottom_map_offset_spans_and_unequal_densities_with_touch_tolerance() {
    for destination in [A, B] {
        for (edge, local_origin, destination_origin, pixels, millimetres, expected) in [
            (
                Edge::Left,
                (200.0, 100.0),
                (-2.0, 120.0),
                (2000, 200),
                (100.0, 40.0),
                point(1999.0, 100.0),
            ),
            (
                Edge::Top,
                (0.0, 100.0),
                (120.0, -2.0),
                (200, 2000),
                (40.0, 100.0),
                point(100.0, 1999.0),
            ),
            (
                Edge::Bottom,
                (0.0, 100.0),
                (120.0, 202.0),
                (200, 2000),
                (40.0, 100.0),
                point(100.0, 0.0),
            ),
        ] {
            let displays = [
                flush_display(A, 1, local_origin, (1000, 1000), (100.0, 100.0)),
                flush_display(B, 1, (100.0, 100.0), (1000, 1000), (100.0, 100.0)),
                flush_display(destination, 2, destination_origin, pixels, millimetres),
            ];
            let mut h = fullscreen_host_home_with(&displays, &[B]);
            let out = h.exit_through(edge, 0.4);
            if destination == A {
                assert_eq!(warp(&out).unwrap().1, (DisplayId(2), expected), "{edge:?}");
                assert!(motions(&out).is_empty());
            } else {
                assert_eq!(
                    (motions(&out)[0].display, motions(&out)[0].position),
                    (DisplayId(2), expected),
                    "{edge:?}"
                );
                assert!(warps(&out).is_empty());
                assert_eq!(h.engine.controlling(), Some(B));
            }
            h.confirm_removal(&out);
            h.quiet();
        }
    }
    // A gap beyond the ordinary touch tolerance offers no bottom continuation.
    let mut h = fullscreen_host_home_with(
        &[
            flush_display(A, 1, (0.0, 100.0), (1000, 1000), (100.0, 100.0)),
            flush_display(B, 1, (100.0, 100.0), (1000, 1000), (100.0, 100.0)),
            flush_display(B, 2, (120.0, 202.01), (200, 2000), (40.0, 100.0)),
        ],
        &[B],
    );
    assert!(!strip_edges(&h.log).contains("bottom"));
    assert!(h.press(Edge::Bottom, 0.4).is_empty());
    h.quiet();
}

#[test]
fn flush_shared_boundary_and_competing_continuations_preserve_same_host_precedence() {
    let displays = [
        flush_display(A, 1, (0.0, 100.0), (1000, 1000), (100.0, 100.0)),
        flush_display(B, 1, (100.0, 100.0), (1000, 1000), (100.0, 100.0)),
        flush_display(B, 2, (200.0, 100.0), (2000, 250), (100.0, 50.0)),
        // One millimetre of overlap is legal within the layout's touch tolerance. In that
        // overlap the same-host neighbour wins; at its exclusive end the cross-node portal wins.
        flush_display(C, 1, (200.0, 149.0), (500, 1020), (100.0, 51.0)),
    ];
    for (position, same_host, expected) in [
        (0.0, true, point(0.0, 0.0)),
        (0.495, true, point(0.0, 247.5)),
        (0.5, false, point(0.0, 20.0)),
    ] {
        let mut h = fullscreen_host_home_with(&displays, &[B, C]);
        let exit = h.exit_through(Edge::Right, position);
        if same_host {
            assert_eq!(
                (motions(&exit)[0].display, motions(&exit)[0].position),
                (DisplayId(2), expected)
            );
            assert!(start_control_to(&h.confirm_removal(&exit)).is_none());
        } else {
            let out = h.confirm_removal(&exit);
            assert!(out.iter().any(|o| matches!(o,
                Output::SendControl { peer: C, msg: ControlMessage::StartControl { entry_display: DisplayId(1), entry, .. } }
                    if *entry == expected
            )), "{out:?}");
            h.feed(Input::Command(Command::ReleaseControl));
        }
        h.quiet();
    }
}

#[test]
fn flush_cross_node_shared_span_endpoints_are_inclusive_and_uncovered_positions_are_quiet() {
    let displays = [
        flush_display(A, 1, (0.0, 0.0), (1000, 1000), (100.0, 100.0)),
        flush_display(B, 1, (100.0, 0.0), (1000, 1000), (100.0, 100.0)),
        flush_display(C, 1, (200.0, 30.0), (600, 600), (100.0, 30.0)),
    ];
    for (position, expected) in [(0.3, point(0.0, 0.0)), (0.6, point(0.0, 599.0))] {
        let mut h = fullscreen_host_home_with(&displays, &[B, C]);
        assert!(h.press(Edge::Right, 0.2999).is_empty());
        assert!(h.press(Edge::Right, 0.6001).is_empty());
        let exit = h.exit_through(Edge::Right, position);
        let out = h.confirm_removal(&exit);
        assert!(out.iter().any(|o| matches!(o,
            Output::SendControl { peer: C, msg: ControlMessage::StartControl { entry_display: DisplayId(1), entry, .. } }
                if *entry == expected
        )), "{position}: {out:?}");
        h.feed(Input::Command(Command::ReleaseControl));
        h.quiet();
    }
}

#[test]
fn flush_third_node_entry_edge_requires_normal_spatial_rearm_before_returning() {
    let mut h = third_node_fullscreen_home();
    let exit = h.exit_through(Edge::Right, 0.25);
    let out = h.confirm_removal(&exit);
    let session = out
        .iter()
        .find_map(|o| match o {
            Output::SendControl {
                peer: C,
                msg: ControlMessage::StartControl { session, .. },
            } => Some(*session),
            _ => None,
        })
        .unwrap();
    h.feed(control(C, ControlMessage::ControlStarted { session }));
    let blocked = h.motion(-10.0, 0.0);
    assert!(start_control_to(&blocked).is_none() && end_controls(&blocked).is_empty());
    assert_eq!(motions(&blocked)[0].position, point(0.0, 250.0));
    assert_eq!(h.engine.controlling(), Some(C));
    assert_eq!(
        motions(&h.motion(20.0, 0.0))[0].position,
        point(20.0, 250.0)
    );
    let return_crossing = h.motion(-21.0, 0.0);
    assert_eq!(
        start_control_to(&return_crossing),
        Some(B),
        "inward 2 mm exceeds REARM_MM=1.5"
    );
    assert!(begin_capture(&return_crossing).is_none());
    assert_eq!(
        end_controls(&return_crossing),
        [(C, session, EndReason::Released)]
    );
    h.feed(Input::Command(Command::ReleaseControl));
    h.quiet();
}

#[test]
fn flush_controller_return_retries_skipped_and_failed_crossing_warps() {
    for failure in [Ok(Warp::Skipped), Err(Failure::Other)] {
        let mut h = fullscreen_host_home(&[(B, 1, 0.0, 0.0), (A, 1, 100.0, 0.0)]);
        let out = h.exit_through(Edge::Right, 0.25);
        let (op, target) = warp(&out).unwrap();
        assert_eq!(target, (LOCAL, point(0.0, 250.0)));
        h.confirm_removal(&out);
        h.released(op, failure);
        h.ended(h.last_begin(), CaptureEnd::Requested);
        let out = h.tick_after(STRANDED_RETRY);
        let (retry, retry_target) = warp(&out).unwrap();
        assert_ne!(retry, op);
        assert_eq!(retry_target, target);
        h.released(retry, Ok(Warp::Done));
        assert!(warps(&h.tick_after(STRANDED_RETRY)).is_empty());
        assert_eq!(h.engine.controlling(), None);
        h.quiet();
    }
}

#[test]
fn flush_third_node_continuation_waits_for_confirmed_bind_removal_then_crosses() {
    let mut h = third_node_fullscreen_home();
    let old = h.session.unwrap();
    let out = h.exit_through(Edge::Right, 0.25);
    assert!(start_control_to(&out).is_none());
    assert!(end_controls(&out).is_empty());
    assert!(!h.probe(P1), "A1 keeps the input fence until removal");
    let out = h.confirm_removal(&out);
    assert_eq!(start_control_to(&out), Some(C));
    assert_eq!(end_controls(&out), vec![(B, old, EndReason::Released)]);
    let session = out
        .iter()
        .find_map(|o| match o {
            Output::SendControl {
                peer: C,
                msg: ControlMessage::StartControl { session, .. },
            } => Some(*session),
            _ => None,
        })
        .unwrap();
    let out = h.feed(control(C, ControlMessage::ControlStarted { session }));
    assert!(
        begin_capture(&out).is_none(),
        "ordinary handoff retains capture"
    );
    assert_eq!(h.engine.controlling(), Some(C));
    assert_eq!(motions(&h.motion(0.0, 0.0))[0].position, point(0.0, 250.0));
    h.quiet();
}

#[test]
fn flush_third_node_failed_or_timed_out_removal_returns_safely_without_crossing() {
    for timeout in [false, true] {
        let mut h = third_node_fullscreen_home();
        let old = h.session.unwrap();
        let exit = h.exit_through(Edge::Right, 0.25);
        let removal = bind(&exit, false).unwrap();
        let out = if timeout {
            h.tick_after(100)
        } else {
            h.bind_set(removal, false, false)
        };
        assert!(start_control_to(&out).is_none(), "{timeout}: {out:?}");
        assert_eq!(end_controls(&out), vec![(B, old, EndReason::Released)]);
        assert_eq!(warp(&out).unwrap().1, FALLBACK);
        assert!(!h.probe(P1));
        let current = h.last_removal();
        let out = h.bind_set(current, false, true);
        assert!(
            start_control_to(&out).is_none(),
            "cancelled handoff never revives"
        );
        h.quiet();
    }
    // A successful removal stamped at the retry deadline, before any Tick, still removes the
    // bind but cannot revive the expired continuation.
    let mut h = third_node_fullscreen_home();
    let old = h.session.unwrap();
    let exit = h.exit_through(Edge::Right, 0.25);
    h.advance(100);
    let out = h.bind_set(bind(&exit, false).unwrap(), false, true);
    assert!(start_control_to(&out).is_none());
    assert_eq!(end_controls(&out), vec![(B, old, EndReason::Released)]);
    assert_eq!(warp(&out).unwrap().1, FALLBACK);
    h.quiet();
}

#[test]
fn flush_third_node_continuation_is_cancelled_by_release_while_removal_is_pending() {
    let mut h = third_node_fullscreen_home();
    let exit = h.exit_through(Edge::Right, 0.25);
    let out = h.feed(Input::Command(Command::ReleaseControl));
    assert_eq!(warp(&out).unwrap().1, FALLBACK);
    assert!(start_control_to(&out).is_none());
    let out = h.confirm_removal(&exit);
    assert!(start_control_to(&out).is_none());
    h.quiet();
}

#[test]
fn flush_return_preserves_bind_teardown_and_home_fence_across_another_local_portal() {
    let displays = [(B, 1, 0.0, 0.0), (A, 1, 100.0, 0.0), (A, 2, 0.0, 100.0)];
    let mut h = fullscreen_host_home(&displays);
    let out = h.exit_through(Edge::Right, 0.25);
    let exited = h.now_ms();
    let (leave, target) = warp(&out).unwrap();
    assert_eq!(target, (LOCAL, point(0.0, 250.0)));
    let removal = bind(&out, false).unwrap();
    let remove_index = out
        .iter()
        .position(|o| matches!(o, Output::HomeBind { install: false, .. }))
        .unwrap();
    let warp_index = out
        .iter()
        .position(|o| matches!(o, Output::ReleaseAndWarp { .. }))
        .unwrap();
    assert!(
        remove_index < warp_index,
        "A1 removal begins before the warp"
    );
    assert!(!h.probe(P1));
    h.bind_set(removal, false, false);
    assert!(!h.probe(P1));
    let retry = h.tick_after(100);
    let current = bind(&retry, false).unwrap();
    assert_ne!(current, removal);
    h.bind_set(removal, false, true);
    assert!(!h.probe(P1), "A2 rejects the stale removal answer");
    h.bind_set(current, false, true);
    h.released(leave, Ok(Warp::Done));
    h.ended(h.last_begin(), CaptureEnd::Requested);
    // Return through a different local connection: reverse-portal hysteresis does not block it,
    // so this independently probes the projection's home fence.
    h.layout_portal = flush_layout(&displays)
        .portals()
        .iter()
        .find(|p| p.from.node == A && p.from.display == DisplayId(2) && p.to.node == B)
        .unwrap()
        .id;
    h.cross();
    let entry = motions(&h.motion(0.0, 0.0))[0].position;
    h.motion(50.0 - entry.x, 100.0 - entry.y);
    let out = h.trigger();
    assert_no_entry(&h, &out);
    h.now = ms(exited + REENTRY_GUARD + 1);
    h.motion(1.0, 0.0);
    let out = h.report(P1, point(51.0, 100.0));
    assert!(
        bind(&out, true).is_some(),
        "home is allowed only after its fence: {out:?}"
    );
    let out = h.feed(Input::Command(Command::ReleaseControl));
    h.confirm_removal(&out);
    h.quiet();
}

#[test]
fn flush_activation_uses_confirmed_host_mapping_behind_identical_strip_and_local_portals() {
    let original = [(A, 1, 0.0, 0.0), (B, 1, 100.0, 0.0), (C, 1, 200.0, 70.0)];
    let changed = [(A, 1, 0.0, 0.0), (B, 1, 100.0, 0.0), (C, 1, 200.0, -70.0)];
    let mut h = fullscreen_host_home(&original);
    let previous = set_portals(&h.log).last().unwrap().clone();
    let (id, _) = h.exit_to_activating(Edge::Right, 0.8);
    h.auto_portals = false;
    let update = h.feed(Input::Layout(flush_placements(&changed)));
    assert_eq!(
        set_portals(&update),
        vec![previous.clone()],
        "changed host mapping gets its own confirmation"
    );
    h.started(id);
    let exit = h.capture_begun(id, vec![]);
    assert!(left_home(&exit));
    let out = h.confirm_removal(&exit);
    assert!(
        out.iter().any(|o| matches!(o,
            Output::SendControl { peer: C, msg: ControlMessage::StartControl { entry, .. } }
                if *entry == point(0.0, 100.0)
        )),
        "pending layout never replaces the confirmed entry: {out:?}"
    );
    h.feed(Input::Command(Command::ReleaseControl));
    h.quiet();

    // Once the same strip's new meaning is confirmed, the old covered position is quiet and
    // its new covered position maps through the new portal.
    let mut h = fullscreen_host_home(&original);
    h.auto_portals = false;
    let update = h.feed(Input::Layout(flush_placements(&changed)));
    let ids = ids_of(&set_portals(&update)[0]);
    h.portals_set(ids, Ok(()));
    assert!(h.press(Edge::Right, 0.8).is_empty());
    let exit = h.exit_through(Edge::Right, 0.2);
    let out = h.confirm_removal(&exit);
    assert!(
        out.iter().any(|o| matches!(o,
            Output::SendControl { peer: C, msg: ControlMessage::StartControl { entry, .. } }
                if *entry == point(0.0, 900.0)
        )),
        "confirmed mapping supplies the new entry: {out:?}"
    );
    h.feed(Input::Command(Command::ReleaseControl));
    h.quiet();
}

#[test]
fn rejected_host_mapping_change_cannot_redirect_a_flush_activation() {
    let original = [(A, 1, 0.0, 0.0), (B, 1, 100.0, 0.0), (C, 1, 200.0, 70.0)];
    let mut h = fullscreen_host_home(&original);
    let (id, _) = h.exit_to_activating(Edge::Right, 0.8);
    h.auto_portals = false;
    let update = h.feed(Input::Layout(flush_placements(&[
        (A, 1, 0.0, 0.0),
        (B, 1, 100.0, 0.0),
        (C, 1, 200.0, -70.0),
    ])));
    let out = h.portals_set(
        ids_of(&set_portals(&update)[0]),
        Err(PortalsFailure::Rejected),
    );
    assert!(
        home_failed(&out, HomeFailure::Gone),
        "existing A9 rejection fails closed: {out:?}"
    );
    assert_eq!(warp(&out).unwrap().1, FALLBACK);
    h.confirm_removal(&out);
    let late = h.capture_begun(id, vec![]);
    assert!(start_control_to(&late).is_none());
    assert!(!h.log.iter().any(|o| matches!(
        o,
        Output::SendControl {
            peer: C,
            msg: ControlMessage::StartControl { .. }
        }
    )));
    h.quiet();
}

#[test]
fn exit_edges_clamped_to_host_are_offered_only_with_continuation() {
    // B has a continuation only on its left edge (back to A). Clamping still removes the other
    // flush edges, while the flush left strip remains available through that portal.
    let cases: [((f64, f64), &[&str]); 6] = [
        ((200.0, 300.0), &["left", "right", "top", "bottom"]),
        ((600.0, 300.0), &["left", "top", "bottom"]),
        ((0.0, 300.0), &["left", "right", "top", "bottom"]),
        ((200.0, 0.0), &["left", "right", "bottom"]),
        ((200.0, 700.0), &["left", "right", "top"]),
        ((-50.0, 800.0), &["left", "right", "top"]),
    ];
    for ((x, y), expected) in cases {
        let mut h = H::bare();
        h.project(W1, B, P1, TWIN, content1(), PlatformParking::Twin);
        h.place(
            B,
            P1,
            1,
            Some(Proxy {
                origin: point(x, y),
                ..Proxy::standard()
            }),
        );
        h.cross();
        let offered: BTreeSet<_> = expected.iter().copied().collect();
        assert_eq!(strip_edges(&h.log), offered, "proxy at ({x}, {y})");
    }
}

#[test]
fn twin_portal_ids_stable_across_removal() {
    let mut h = H::with_extra_sources();
    h.cross();
    let offered = |h: &H| -> Vec<PortalId> {
        set_portals(&h.log)
            .last()
            .unwrap()
            .iter()
            .filter(|p| p.display == TWIN2 || p.display == TWIN)
            .map(|p| p.id)
            .collect()
    };
    let ids = offered(&h);
    let p1: Vec<_> = (0..4).map(|i| PortalId(TWIN_PORTAL_BASE + i)).collect();
    let p2: Vec<_> = (4..8).map(|i| PortalId(TWIN_PORTAL_BASE + i)).collect();
    assert_eq!(ids, [p1.clone(), p2.clone()].concat());
    // Removing the first projection does not rename the second's strips.
    h.advance(1);
    h.feed(Input::Command(Command::Return(key(P1))));
    assert_eq!(offered(&h), p2);
    // A slot is never reused while the controller lives: a new projection gets a new one.
    h.advance(1);
    h.project(
        W1,
        B,
        ProjectionId(4),
        TWIN,
        content1(),
        PlatformParking::Twin,
    );
    h.place(B, ProjectionId(4), 1, Some(Proxy::standard()));
    let ids = offered(&h);
    // Every twin home takes a slot, whichever peer shows it: P3 (C's) has slot 2, so the new
    // projection's strips are slot 3's.
    let p4: Vec<_> = (12..16).map(|i| PortalId(TWIN_PORTAL_BASE + i)).collect();
    assert_eq!(ids, [p2, p4].concat(), "the freed slot 0 is not reused");
    h.quiet();
}

#[test]
fn fullscreen_resize_while_home_leaves_home() {
    let mut h = H::home();
    let session = h.session.unwrap();
    // The user makes the proxy fullscreen: the twin is re-parked at B's size, and B reports a
    // placement that touches every edge. No pointer exit remains: leave home.
    h.advance(1);
    let out = h.feed(projection_msg(
        B,
        Message::Resize {
            projection: P1,
            request: 1,
            size: PixelSize::new(1000, 1000),
            scale: 1.0,
        },
    ));
    assert!(out.iter().any(|o| matches!(o, Output::ResizeParked { .. })));
    assert!(!left_home(&out));
    h.advance(1);
    let out = h.feed(Input::Parked {
        window: W1,
        result: Ok(parked(
            W1,
            PlatformParking::Twin,
            TWIN,
            rect(0, 0, 1000, 1000),
        )),
    });
    // The twin and the placement disagree: the placement is incoherent: home is left.
    assert!(home_failed(&out, HomeFailure::Gone), "{out:?}");
    assert_eq!(end_controls(&out), vec![(B, session, EndReason::Released)]);
    assert_eq!(warps(&out)[0].1, FALLBACK);
    h.confirm_removal(&out);
    h.quiet();
}

#[test]
fn no_exit_left_leaves_home() {
    // B's display shrinks to the proxy's size around it, with no continuation on any edge.
    let mut h = H::home();
    let session = h.session.unwrap();
    h.feed(Input::Layout(flush_placements(&[
        (A, 1, 0.0, 0.0),
        (B, 1, 200.0, 0.0),
    ])));
    let small = DisplayInfo {
        geometry: DisplayGeometry {
            physical_size: SizeMm::new(40.0, 30.0),
            pixel_size: PixelSize::new(400, 300),
            ..display(1).geometry
        },
        ..display(1)
    };
    h.advance(1);
    let out = h.feed(Input::PeerDisplays {
        peer: B,
        displays: vec![small],
    });
    assert!(
        !left_home(&out),
        "still an exit left on the left and top: {out:?}"
    );
    h.advance(1);
    let out = h.place(
        B,
        P1,
        2,
        Some(Proxy {
            origin: point(0.0, 0.0),
            ..Proxy::standard()
        }),
    );
    assert!(home_failed(&out, HomeFailure::Gone), "{out:?}");
    assert_eq!(end_controls(&out), vec![(B, session, EndReason::Released)]);
    h.confirm_removal(&out);
    h.quiet();
}

#[test]
fn strips_rebuilt_on_resize() {
    // Controlling (not home): the twin is re-parked at a new size and B reports the matching
    // placement: the strips follow the new content.
    let mut h = H::controlling();
    h.advance(1);
    h.feed(projection_msg(
        B,
        Message::Resize {
            projection: P1,
            request: 1,
            size: PixelSize::new(500, 350),
            scale: 1.0,
        },
    ));
    h.advance(1);
    let out = h.feed(Input::Parked {
        window: W1,
        result: Ok(parked(
            W1,
            PlatformParking::Twin,
            TWIN,
            rect(50, 40, 550, 390),
        )),
    });
    // The placement is incoherent until B reports the new size: no strips for it.
    assert_eq!(strip_edges(&out), BTreeSet::new(), "{out:?}");
    h.advance(1);
    let out = h.place(
        B,
        P1,
        2,
        Some(Proxy {
            size: PixelSize::new(500, 350),
            ..Proxy::standard()
        }),
    );
    let strips: Vec<_> = set_portals(&out)
        .last()
        .unwrap()
        .iter()
        .filter(|p| p.display == TWIN)
        .copied()
        .collect();
    assert_eq!(strips.len(), 4, "{out:?}");
    let left = strips.iter().find(|s| s.edge == Edge::Left).unwrap();
    let top = strips.iter().find(|s| s.edge == Edge::Top).unwrap();
    assert_eq!((left.from, left.to), (40.0, 390.0));
    assert_eq!((top.from, top.to), (50.0, 550.0));
    h.quiet();
}

#[test]
fn portals_resent_on_failure() {
    let mut h = H::projected();
    h.auto_portals = false;
    h.cross();
    let offered = set_portals(&h.log).pop().unwrap();
    let ids: Vec<PortalId> = offered.iter().map(|p| p.id).collect();
    assert!(offered.iter().any(|p| p.display == TWIN));
    h.advance(1);
    h.portals_set(ids.clone(), Err(PortalsFailure::Rejected));
    let failed = h.now_ms();
    // Re-sent every PORTALS_RETRY until installed.
    for round in 1..=3 {
        let out = h.tick(failed + (round - 1) * PORTALS_RETRY + PORTALS_RETRY - 1);
        assert!(set_portals(&out).is_empty(), "round {round}: {out:?}");
        let out = h.tick(failed + round * PORTALS_RETRY);
        assert_eq!(set_portals(&out), vec![offered.clone()], "round {round}");
        h.portals_set(ids.clone(), Err(PortalsFailure::Rejected));
        // The answer arrives at the tick's time.
        assert_eq!(h.now_ms(), failed + round * PORTALS_RETRY);
        h.now = ms(failed + round * PORTALS_RETRY);
    }
    let last = failed + 3 * PORTALS_RETRY;
    let out = h.tick(last + PORTALS_RETRY);
    assert_eq!(set_portals(&out).len(), 1);
    h.portals_set(ids.clone(), Ok(()));
    assert!(
        set_portals(&h.tick(last + 10 * PORTALS_RETRY)).is_empty(),
        "installed: no more"
    );
    // A failure with no twin strips among the portals is not re-sent: the layout's set alone is
    // the agent's concern, as today.
    let mut h = H::bare();
    h.auto_portals = false;
    h.advance(1);
    h.feed(Input::PeerDisplays {
        peer: B,
        displays: vec![display(1)],
    });
    h.portals_set(vec![], Err(PortalsFailure::Rejected));
    assert!(set_portals(&h.tick(10_000)).is_empty());
    h.quiet();
}

// ---------------------------------------------------------------------------------------------
// LEAVING (§2.7, §2.8): every cause, in every state it can happen in.
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum At {
    /// Entering, the drain still waiting (a key down in W is being released).
    Draining,
    /// Entering, the home bind requested and not yet confirmed.
    Binding,
    Releasing,
    Home,
    /// Exiting, the exit capture being activated.
    Activating,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cause {
    ProjectionEnds,
    PlacementLost,
    PeerLost,
    LeaseLost,
    EndControlFromPeer,
    LocalOverride,
    Panic,
    Lock,
    ReleaseCommand,
    BindLost,
}

const ALL_AT: [At; 5] = [
    At::Draining,
    At::Binding,
    At::Releasing,
    At::Home,
    At::Activating,
];

/// Drive a fresh scenario into `at`. With `no_acks`, B never acknowledges: the first heartbeat
/// (sent at once) goes unanswered, so the lease is lost 150 ms later.
fn drive(at: At, no_acks: bool) -> (H, HomeOp, CaptureId) {
    let mut h = H::projected();
    h.auto_ack = !no_acks;
    h.cross();
    if no_acks {
        h.feed(Input::Tick);
    }
    let capture = h.capture.unwrap();
    let op = match at {
        At::Draining => {
            // A key is down in W: the drain waits for its release to be confirmed.
            h.focus(Some(W1));
            let out = h.proj_key(P1, KEY, true);
            h.confirm(&out, true);
            h.advance(1);
            h.aim();
            let out = h.trigger();
            assert_eq!(injects(&out).len(), 1, "{out:?}");
            HomeOp(0)
        }
        At::Binding => {
            h.advance(1);
            h.aim();
            h.reach_binding()
        }
        At::Releasing => {
            h.advance(1);
            h.aim();
            h.reach_releasing()
        }
        At::Home => {
            h.advance(1);
            h.aim();
            h.home_now()
        }
        At::Activating => {
            h.advance(1);
            h.aim();
            let op = h.home_now();
            let (id, _) = h.exit_to_activating(Edge::Right, 0.25);
            h.advance(1);
            h.started(id);
            let _ = id;
            op
        }
    };
    (h, op, capture)
}

/// Apply `cause`; returns every output it produced.
fn apply(h: &mut H, cause: Cause, op: HomeOp) -> Vec<Output> {
    h.advance(1);
    match cause {
        Cause::ProjectionEnds => h.feed(Input::Command(Command::Return(key(P1)))),
        Cause::PlacementLost => h.place(B, P1, 99, None),
        Cause::PeerLost => h.feed(closed(B)),
        Cause::LeaseLost => {
            let t = h.now_ms();
            assert!(
                h.tick(t + 140)
                    .iter()
                    .all(|o| !matches!(o, Output::Notice(_)))
            );
            h.tick(t + 151)
        }
        Cause::EndControlFromPeer => {
            let session = h.session.unwrap();
            h.feed(control(
                B,
                ControlMessage::EndControl {
                    session,
                    reason: EndReason::Released,
                },
            ))
        }
        Cause::LocalOverride => h.feed(Input::Link(LinkEvent::Input {
            peer: B,
            msg: InputMessage::Status {
                session: h.session.unwrap(),
                status: TargetStatus::LocalOverride,
            },
        })),
        Cause::Panic => h.feed(Input::Command(Command::Panic)),
        Cause::Lock => h.feed(locked()),
        Cause::ReleaseCommand => h.feed(Input::Command(Command::ReleaseControl)),
        Cause::BindLost => h.bind_set(op, true, false),
    }
}

/// What leaving home (or abandoning an entry) must look like, for `cause` in state `at`. Where a
/// capture may still exist the return is fenced by its `Ended` or a timeout: `ended` says which
/// one is exercised (the matching `Ended` must complete the return at once, and an unrelated
/// capture's must not; otherwise the timeout does).
fn check_leave(cause: Cause, at: At, ended: bool) {
    let tag = format!("{cause:?} during {at:?} (ended: {ended})");
    let pre_release = matches!(at, At::Draining | At::Binding);
    let (mut h, op, capture) = drive(at, cause == Cause::LeaseLost);
    let session = h.session.unwrap();
    let mut out = apply(&mut h, cause, op);
    if cause == Cause::PlacementLost && at == At::Activating {
        // A placement that goes incoherent while the exit capture is being activated is judged
        // when the activation completes (§2.8): the exit would otherwise be abandoned for what
        // may be a transient state.
        assert_eq!(h.engine.controlling(), Some(B), "{out:?}");
        let id = h.last_begin();
        h.advance(1);
        out = h.capture_begun(id, vec![]);
    }
    // A projection that ends, or a placement that is lost, before the release only abandons the
    // entry: the capture is live and the session continues as before the trigger. So does a bind
    // that could not be installed.
    let abort_only = (pre_release && matches!(cause, Cause::ProjectionEnds | Cause::PlacementLost))
        || (at == At::Binding && cause == Cause::BindLost);
    if cause == Cause::BindLost && at == At::Draining {
        // No bind was requested yet: an answer for an operation nobody asked for is stale.
        assert!(out.is_empty(), "{tag}: {out:?}");
        return;
    }
    let survives = matches!(
        cause,
        Cause::PlacementLost
            | Cause::EndControlFromPeer
            | Cause::LocalOverride
            | Cause::ReleaseCommand
            | Cause::BindLost
            | Cause::LeaseLost
    );
    // Whatever bind was requested goes through its removal phase.
    let removal = bind(&out, false);
    assert_eq!(removal.is_some(), at != At::Draining, "{tag}: {out:?}");
    if abort_only {
        let reason = if cause == Cause::BindLost {
            HomeFailure::Bind
        } else {
            HomeFailure::Gone
        };
        assert!(home_failed(&out, reason), "{tag}: {out:?}");
        assert_eq!(h.engine.controlling(), Some(B), "{tag}");
        assert!(
            end_controls(&out).is_empty() && warps(&out).is_empty(),
            "{tag}: {out:?}"
        );
        if removal.is_some() {
            if survives {
                assert!(
                    !h.probe(P1),
                    "{tag}: the seat stays arbitrated until the removal"
                );
            }
            h.advance(1);
            h.bind_set(h.last_removal(), false, true);
            if survives {
                assert!(h.probe(P1), "{tag}");
            }
        }
        for o in h.log.clone() {
            if let Output::Inject { id, .. } = o {
                h.feed(Input::InjectDone { id, ok: true });
            }
        }
        h.quiet();
        return;
    }

    // The session ended; nothing is held anywhere (quiet, below).
    assert_eq!(h.engine.controlling(), None, "{tag}: {out:?}");
    if cause == Cause::LocalOverride {
        assert!(
            has_notice(&out, &Notice::LocalOverride(B)),
            "{tag}: {out:?}"
        );
        assert!(
            !notices(&out)
                .iter()
                .any(|n| matches!(n, Notice::ControlReleased { .. })),
            "{tag}: {out:?}"
        );
    }
    let expected_end = match cause {
        Cause::PeerLost | Cause::EndControlFromPeer => None,
        Cause::LeaseLost => Some(EndReason::LinkLost),
        Cause::Panic => Some(EndReason::Panic),
        Cause::Lock => Some(EndReason::ControllerLocked),
        _ => Some(EndReason::Released),
    };
    assert_eq!(
        end_controls(&out),
        expected_end
            .map(|r| (B, session, r))
            .into_iter()
            .collect::<Vec<_>>(),
        "{tag}: {out:?}"
    );
    // The pointer: before the release the capture is live and the pointer physical (the ordinary
    // end); after it, a warp to the fallback point replaces the plain end.
    if pre_release {
        assert!(has_end_capture(&out), "{tag}: {out:?}");
        assert!(warps(&out).is_empty(), "{tag}: {out:?}");
    } else {
        assert_eq!(warps(&out).len(), 1, "{tag}: {out:?}");
        assert_eq!(warps(&out)[0].1, FALLBACK, "{tag}");
        assert!(!has_end_capture(&out), "{tag}: {out:?}");
    }
    // Notices.
    let failed = notices(&out)
        .iter()
        .any(|n| matches!(n, Notice::HomeFailed { .. }));
    let left = left_home(&out);
    match at {
        At::Draining | At::Binding | At::Releasing => assert!(failed, "{tag}: {out:?}"),
        At::Home | At::Activating => {
            let reason = match cause {
                Cause::ProjectionEnds | Cause::PlacementLost => Some(HomeFailure::Gone),
                Cause::BindLost => Some(HomeFailure::Bind),
                _ => None,
            };
            match reason {
                Some(reason) => assert!(home_failed(&out, reason), "{tag}: {out:?}"),
                None => assert!(left, "{tag}: {out:?}"),
            }
        }
    }
    // The twin strips are withdrawn in the same handle: with no session they must not be pressable
    // by the pointer this node injects on the twin (03 §4.5).
    assert!(
        set_portals(&h.log)
            .last()
            .is_some_and(|p| p.iter().all(|c| c.display != TWIN)),
        "{tag}: the strips are withdrawn: {out:?}"
    );
    // The capture's end: where a capture may exist the return is fenced by it (or its timeout);
    // otherwise the controller is idle at once.
    let fenced = at != At::Home;
    assert_eq!(!has_hud_hide(&out), fenced, "{tag}: {out:?}");
    if fenced && ended {
        // The capture the fence waits for: the exit's own while activating, else the one the
        // session began with.
        let waiting_for = if at == At::Activating {
            h.last_begin()
        } else {
            capture
        };
        // An unrelated capture's end is not the one the fence is for.
        h.advance(1);
        let other = h.ended(CaptureId(waiting_for.0 + 1000), CaptureEnd::Requested);
        assert!(!has_hud_hide(&other), "{tag}: {other:?}");
        // The matching one completes the return at once, without waiting for the timeout and
        // without ending anything again.
        h.advance(1);
        let done = h.ended(waiting_for, CaptureEnd::Requested);
        assert!(has_hud_hide(&done), "{tag}: {done:?}");
        assert!(
            warps(&done).is_empty() && !has_end_capture(&done),
            "{tag}: {done:?}"
        );
        // Nothing is left to time out.
        let t = h.now_ms();
        let late = h.tick(t + END_TIMEOUT);
        assert!(
            warps(&late).is_empty() && !has_end_capture(&late) && !has_hud_hide(&late),
            "{tag}: {late:?}"
        );
    } else if fenced {
        let t = h.now_ms();
        let early = h.tick(t + END_TIMEOUT - 1);
        assert!(
            warps(&early).is_empty() && !has_hud_hide(&early),
            "{tag}: {early:?}"
        );
        let late = h.tick(t + END_TIMEOUT);
        assert!(has_hud_hide(&late), "{tag}: {late:?}");
        if pre_release {
            assert!(has_end_capture(&late), "{tag}: {late:?}");
        } else {
            assert_eq!(warps(&late).len(), 1, "{tag}: {late:?}");
            assert_eq!(warps(&late)[0].1, FALLBACK, "{tag}");
        }
    } else {
        let t = h.now_ms();
        let late = h.tick(t + END_TIMEOUT);
        assert!(
            warps(&late).is_empty() && !has_end_capture(&late),
            "{tag}: {late:?}"
        );
    }

    // The seat stays arbitrated until the removal is confirmed, then it is free (for causes that
    // leave the projection alive).
    if removal.is_some() {
        if survives {
            assert!(
                !h.probe(P1),
                "{tag}: the filter stays on until the removal is confirmed"
            );
        }
        h.advance(1);
        // The removal may have been retried while the return was fenced: confirm the newest.
        h.bind_set(h.last_removal(), false, true);
        if survives {
            assert!(h.probe(P1), "{tag}: the filter is off once it is confirmed");
        }
    }
    // Let every release the injectors owe be confirmed, then check the invariants.
    for o in h.log.clone() {
        if let Output::Inject { id, .. } = o {
            h.feed(Input::InjectDone { id, ok: true });
        }
    }
    h.quiet();
}

fn check_all(cause: Cause) {
    for at in ALL_AT {
        for ended in [false, true] {
            check_leave(cause, at, ended);
        }
    }
}

#[test]
fn home_ends_with_projection() {
    check_all(Cause::ProjectionEnds);
}

#[test]
fn home_ends_when_placement_lost() {
    check_all(Cause::PlacementLost);
}

#[test]
fn home_peer_lost() {
    check_all(Cause::PeerLost);
}

#[test]
fn home_lease_lost() {
    check_all(Cause::LeaseLost);
}

#[test]
fn home_end_control_from_peer() {
    check_all(Cause::EndControlFromPeer);
}

#[test]
fn home_local_override_ends_through_bind_removal_fence() {
    check_all(Cause::LocalOverride);
}

#[test]
fn home_panic() {
    check_all(Cause::Panic);
}

#[test]
fn home_local_lock() {
    check_all(Cause::Lock);
    // The gate is closed: the warp is skipped, and retried after the unlock.
    let mut h = H::home();
    h.advance(1);
    let out = h.feed(locked());
    let (leave, target) = warp(&out).expect("the fallback warp is requested");
    assert_eq!(target, FALLBACK);
    let removal = bind(&out, false).unwrap();
    h.released(leave, Ok(Warp::Skipped));
    let stranded_at = h.now_ms();
    // While locked, nothing is retried, and no timer spins waiting for the unlock.
    assert!(warps(&h.tick(stranded_at + STRANDED_RETRY)).is_empty());
    assert!(
        h.engine.next_deadline().is_none_or(|d| d > h.now),
        "a blocked retry is not a due deadline"
    );
    h.advance(5);
    let out = h.feed(unlocked());
    let retry = warp(&out).expect("retried at the unlock");
    assert_eq!(retry.1, FALLBACK);
    h.released(retry.0, Ok(Warp::Done));
    h.bind_set(removal, false, true);
    h.quiet();
}

#[test]
fn home_release_command() {
    check_all(Cause::ReleaseCommand);
    // The same through the release chord, captured while entering.
    let mut h = H::aimed();
    let session = h.session.unwrap();
    let mut out = Vec::new();
    for usage in [LCTRL, LSHIFT, LALT, ESC] {
        h.advance(1);
        out = h.feed(Input::Capture(CaptureEvent::Key {
            usage,
            down: true,
            at: h.now,
        }));
    }
    assert_eq!(end_controls(&out), vec![(B, session, EndReason::Released)]);
    assert!(!h.engine.armed(), "an explicit release disarms crossing");
    h.quiet();
}

#[test]
fn home_bind_lost() {
    check_all(Cause::BindLost);
}

#[test]
fn home_fails_closed_on_sleep_and_unknown_state() {
    // The lock state is unknown (a wake without a fresh state, or a state that can't be read),
    // or the machine is going to sleep: the same as locked.
    let unknown = Input::Session(SessionEvent::State(SessionState {
        lock: LockState::Unknown,
        active: None,
    }));
    let inactive = Input::Session(SessionEvent::State(SessionState {
        lock: LockState::Unlocked,
        active: Some(false),
    }));
    let causes = [
        unknown,
        inactive,
        Input::Session(SessionEvent::WillSleep),
        Input::Session(SessionEvent::Woke),
    ];
    for (index, cause) in causes.into_iter().enumerate() {
        for at in [At::Releasing, At::Home, At::Activating] {
            let (mut h, _, _) = drive(at, false);
            let session = h.session.unwrap();
            h.advance(1);
            let out = h.feed(cause.clone());
            assert_eq!(
                end_controls(&out),
                vec![(B, session, EndReason::ControllerLocked)],
                "cause {index} during {at:?}: {out:?}"
            );
            assert_eq!(warps(&out).len(), 1, "cause {index} during {at:?}: {out:?}");
            assert!(bind(&out, false).is_some());
            assert_eq!(h.engine.controlling(), None);
        }
    }
}

#[test]
fn stale_capture_events_are_not_routed_while_home() {
    // Home: no capture is live. Whatever the platform still delivers is not routed to B.
    let mut h = H::home();
    for event in [
        CaptureEvent::Motion {
            dx: 5.0,
            dy: 5.0,
            kind: MotionKind::Unaccelerated,
            at: h.now,
        },
        CaptureEvent::Key {
            usage: KEY,
            down: true,
            at: h.now,
        },
        CaptureEvent::Key {
            usage: KEY,
            down: false,
            at: h.now,
        },
        CaptureEvent::Button {
            button: BUTTON,
            down: true,
            at: h.now,
        },
        CaptureEvent::Button {
            button: BUTTON,
            down: false,
            at: h.now,
        },
        CaptureEvent::Scroll {
            delta: scroll_delta(),
            at: h.now,
        },
    ] {
        h.advance(1);
        let out = h.feed(Input::Capture(event));
        assert!(out.is_empty(), "nothing is routed to B while home: {out:?}");
    }
    h.quiet();

    // While the release is in flight the chord still works (and nothing else is routed).
    let mut h = H::aimed();
    let session = h.session.unwrap();
    h.reach_releasing();
    let mut out = Vec::new();
    for usage in [LCTRL, LSHIFT, LALT] {
        h.advance(1);
        out = h.feed(Input::Capture(CaptureEvent::Key {
            usage,
            down: true,
            at: h.now,
        }));
        assert!(out.is_empty(), "{out:?}");
    }
    h.advance(1);
    let out2 = h.feed(Input::Capture(CaptureEvent::Key {
        usage: ESC,
        down: true,
        at: h.now,
    }));
    let _ = out;
    assert_eq!(
        end_controls(&out2),
        vec![(B, session, EndReason::Released)],
        "{out2:?}"
    );
    h.quiet();
}

#[test]
fn no_entry_outside_a_controlling_session() {
    // No session at all.
    let mut h = H::projected();
    let out = h.report(P1, point(50.0, 100.0));
    assert!(binds(&out).is_empty(), "{out:?}");
    assert!(notices(&out).is_empty());

    // The session is over and its capture is being ended.
    let mut h = H::aimed();
    h.feed(Input::Command(Command::ReleaseControl));
    assert_eq!(h.engine.controlling(), None);
    h.advance(1);
    let out = h.trigger();
    assert!(binds(&out).is_empty(), "{out:?}");
    assert!(
        !notices(&out)
            .iter()
            .any(|n| matches!(n, Notice::Home { .. } | Notice::HomeFailed { .. }))
    );
    h.quiet();
}

// ---------------------------------------------------------------------------------------------
// PLACEMENT (§4), RECONNECT, TWO PROXIES
// ---------------------------------------------------------------------------------------------

impl H {
    /// B's link drops and comes back; B accepts the resumed projection and its capture restarts.
    fn reconnect_p1(&mut self) {
        self.feed(closed(B));
        let out = self.feed(Input::PeerUp { peer: B });
        assert!(
            out.iter().any(|o| matches!(
                o,
                Output::SendControl {
                    msg: ControlMessage::Projection(Message::Start { projection, .. }),
                    ..
                } if *projection == P1
            )),
            "the source offers the projection again: {out:?}"
        );
        let out = self.feed(projection_msg(
            B,
            Message::Accepted {
                projection: P1,
                size: PixelSize::new(400, 300),
                scale: 1.0,
            },
        ));
        assert!(
            out.iter().any(|o| matches!(o, Output::StartCapture { .. })),
            "{out:?}"
        );
        self.feed(Input::CaptureStarted {
            projection: P1,
            result: Ok(StreamId(500)),
        });
    }
}

#[test]
fn reconnect_requires_fresh_placement() {
    let mut h = H::projected();
    h.reconnect_p1();
    h.cross();
    // The placement belonged to the old connection: no strips, no entry, until B repeats it.
    assert_eq!(strip_edges(&h.log), BTreeSet::new());
    h.aim();
    let out = h.trigger();
    assert_no_entry(&h, &out);

    // An older generation never revives it, and neither does a different report under the same
    // generation (only an identical resend is one).
    let out = h.place(B, P1, 0, Some(Proxy::standard()));
    assert!(set_portals(&out).is_empty(), "{out:?}");
    let out = h.place(
        B,
        P1,
        1,
        Some(Proxy {
            origin: point(210.0, 300.0),
            ..Proxy::standard()
        }),
    );
    assert!(set_portals(&out).is_empty(), "{out:?}");
    // The equal-generation resend re-validates it.
    let out = h.place(B, P1, 1, Some(Proxy::standard()));
    assert_eq!(strip_edges(&out).len(), 4, "{out:?}");
    h.motion(1.0, 0.0);
    let out = h.trigger();
    assert!(bind(&out, true).is_some(), "{out:?}");
    h.quiet();

    // A newer generation does too (B's proxy moved while the link was down).
    let mut h = H::projected();
    h.reconnect_p1();
    h.cross();
    let out = h.place(
        B,
        P1,
        2,
        Some(Proxy {
            origin: point(210.0, 300.0),
            ..Proxy::standard()
        }),
    );
    assert_eq!(strip_edges(&out).len(), 4, "{out:?}");
    h.quiet();
}

#[test]
fn placement_before_live_is_kept() {
    let mut h = H::bare();
    h.feed(Input::Command(Command::Project { window: W1, to: B }));
    h.feed(projection_msg(
        B,
        Message::Accepted {
            projection: P1,
            size: PixelSize::new(400, 300),
            scale: 1.0,
        },
    ));
    // The proxy is open and placed before the source has parked or started capturing.
    h.place(B, P1, 1, Some(Proxy::standard()));
    h.feed(Input::Parked {
        window: W1,
        result: Ok(parked(W1, PlatformParking::Twin, TWIN, content1())),
    });
    h.feed(Input::CaptureStarted {
        projection: P1,
        result: Ok(StreamId(1)),
    });
    h.cross();
    assert_eq!(strip_edges(&h.log).len(), 4, "the early report was kept");
    h.quiet();
}

#[test]
fn placement_hwm_rejects_stale_after_none() {
    let mut h = H::controlling();
    assert_eq!(strip_edges(&h.log).len(), 4);
    // The proxy went off every display.
    let out = h.place(B, P1, 2, None);
    assert_eq!(strip_edges(&out), BTreeSet::new(), "{out:?}");
    // A report of the generation before it (a delayed one) can't bring it back: the high-water
    // mark never decreases, whatever the newest report said.
    let out = h.place(B, P1, 1, Some(Proxy::standard()));
    assert!(set_portals(&out).is_empty(), "{out:?}");
    let out = h.place(B, P1, 2, Some(Proxy::standard()));
    assert!(
        set_portals(&out).is_empty(),
        "same generation, other contents: {out:?}"
    );
    // A newer one does.
    let out = h.place(B, P1, 3, Some(Proxy::standard()));
    assert_eq!(strip_edges(&out).len(), 4, "{out:?}");
    h.quiet();
}

#[test]
fn placement_overflow_stops_reports() {
    let mut h = H::controlling();
    // The generation just below the terminal one is an ordinary report.
    let out = h.place(B, P1, u32::MAX - 1, None);
    assert_eq!(strip_edges(&out), BTreeSet::new(), "{out:?}");
    // `u32::MAX` is the terminal invalidation: it never carries a display, even if a peer sends
    // one, and nothing can exceed it.
    let out = h.place(B, P1, u32::MAX, Some(Proxy::standard()));
    assert!(set_portals(&out).is_empty(), "{out:?}");
    for generation in [u32::MAX - 1, u32::MAX, 5] {
        let out = h.place(B, P1, generation, Some(Proxy::standard()));
        assert!(
            set_portals(&out).is_empty(),
            "generation {generation}: {out:?}"
        );
    }
    h.aim();
    let out = h.trigger();
    assert_no_entry(&h, &out);
    h.quiet();
}

#[test]
fn two_proxies_on_one_peer() {
    let mut h = H::with_extra_sources();
    h.cross();
    h.aim();
    h.home_now();
    let exit = h.exit_through(Edge::Right, 0.25);
    h.confirm_removal(&exit);
    let exited = h.now_ms();
    // B's pointer leaves the first proxy at (600, 375) and goes on into the second, which sits at
    // (650, 100) 300x200. B reports motion over it.
    h.now = ms(exited + REENTRY_GUARD + 50);
    h.motion(100.0, -175.0);
    let out = h.report(P2, point(50.0, 100.0));
    let op =
        bind(&out, true).expect("an entry into the second proxy after the fence and fresh motion");
    // Its own strips, slot 1; the warp lands on its own twin display.
    h.advance(1);
    let out = h.bind_set(op, true, true);
    assert_eq!(warps(&out), vec![(op, (TWIN2, point(80.0, 130.0)))]);
    h.advance(1);
    let out = h.released(op, Ok(Warp::Done));
    assert_eq!(activations(&out), vec![W2]);
    h.advance(1);
    let out = h.focus(Some(W2));
    assert!(has_notice(
        &out,
        &Notice::Home {
            key: key(P2),
            entered: true
        }
    ));
    // Out through the second proxy's strip: slot 1's left edge.
    h.advance(1);
    let portal = h.strip(1, Edge::Left);
    assert_eq!(portal, PortalId(TWIN_PORTAL_BASE + 4));
    h.feed(Input::Capture(CaptureEvent::EdgePressed {
        portal,
        position: 0.5,
        at: h.now,
    }));
    h.advance(1);
    let out = h.visible();
    let (id, begun, _) = begin_capture(&out).unwrap();
    assert_eq!(begun, portal);
    h.started(id);
    h.advance(1);
    let out = h.capture_begun(id, vec![]);
    // Just outside the left edge of the second proxy, half way down it.
    assert_eq!(motions(&out)[0].position, point(649.0, 200.0));
    h.confirm_removal(&out);
    h.quiet();
}

// ---------------------------------------------------------------------------------------------
// The destination's placement production (§4).
// ---------------------------------------------------------------------------------------------

struct Dest {
    engine: Engine,
    now: MonoTime,
}

impl Dest {
    fn new() -> Dest {
        let (engine, _) = Engine::new(
            EngineConfig::new(B),
            Box::new(SharedJournal::default()),
            Box::new(SharedJournal::default()),
            ms(0),
        )
        .unwrap();
        let mut d = Dest { engine, now: ms(0) };
        d.feed(Input::Session(SessionEvent::State(OPEN)));
        d.feed(Input::PeerUp { peer: A });
        d.feed(Input::Grants(
            [(A, [Capability::WindowPresent].into())].into(),
        ));
        d.feed(projection_msg(
            A,
            Message::Start {
                projection: P1,
                window: crosspane_protocol::projection::WindowSummary {
                    title: "fixture".into(),
                    app_id: "test".into(),
                },
                size: PixelSize::new(400, 300),
            },
        ));
        d.feed(Input::ProxyOpened {
            key: key(P1),
            result: Ok((PixelSize::new(400, 300), 1.0)),
        });
        d
    }

    fn feed(&mut self, input: Input) -> Vec<Output> {
        self.engine.handle(input, self.now)
    }

    fn placed(
        &mut self,
        display: Option<DisplayId>,
        x: f64,
        y: f64,
        w: u32,
        h: u32,
    ) -> Vec<Output> {
        self.feed(Input::Proxy {
            key: key(P1),
            event: ProxyEvent::Placed {
                display,
                origin: point(x, y),
                size: PixelSize::new(w, h),
            },
        })
    }
}

fn proxy_placed(out: &[Output]) -> Vec<(u32, Option<DisplayId>, PointDevice, PixelSize)> {
    out.iter()
        .filter_map(|o| match o {
            Output::SendControl {
                peer,
                msg:
                    ControlMessage::Projection(Message::ProxyPlaced {
                        projection,
                        generation,
                        display,
                        origin,
                        size,
                    }),
            } if *peer == A && *projection == P1 => Some((*generation, *display, *origin, *size)),
            _ => None,
        })
        .collect()
}

#[test]
fn destination_reports_placement_with_generations() {
    let mut d = Dest::new();
    let out = d.placed(Some(REMOTE), 200.0, 300.0, 400, 300);
    assert_eq!(
        proxy_placed(&out),
        vec![(
            1,
            Some(REMOTE),
            point(200.0, 300.0),
            PixelSize::new(400, 300)
        )]
    );
    // A repeat is not a change.
    assert!(d.placed(Some(REMOTE), 200.0, 300.0, 400, 300).is_empty());
    // Every change grows the generation by one.
    let out = d.placed(Some(REMOTE), 210.0, 300.0, 400, 300);
    assert_eq!(proxy_placed(&out)[0].0, 2);
    let out = d.placed(None, 0.0, 0.0, 0, 0);
    assert_eq!(
        proxy_placed(&out),
        vec![(3, None, point(0.0, 0.0), PixelSize::new(0, 0))]
    );
    // A host that can't give a finite origin has not said where the proxy is.
    let out = d.placed(Some(REMOTE), f64::NAN, 1.0, 400, 300);
    assert_eq!(
        proxy_placed(&out),
        vec![(4, None, point(0.0, 0.0), PixelSize::new(400, 300))]
    );
    let out = d.placed(Some(REMOTE), 5.0, 6.0, 400, 300);
    assert_eq!(proxy_placed(&out)[0].0, 5);

    // While the link is down the newest report is only recorded, and resent with its generation
    // when the projection resumes; nothing is sent in between.
    d.feed(closed(A));
    assert!(d.placed(Some(REMOTE), 7.0, 8.0, 400, 300).is_empty());
    d.feed(Input::PeerUp { peer: A });
    let out = d.feed(projection_msg(
        A,
        Message::Start {
            projection: P1,
            window: crosspane_protocol::projection::WindowSummary {
                title: "fixture".into(),
                app_id: "test".into(),
            },
            size: PixelSize::new(400, 300),
        },
    ));
    assert_eq!(
        proxy_placed(&out),
        vec![(6, Some(REMOTE), point(7.0, 8.0), PixelSize::new(400, 300))],
        "{out:?}"
    );
    // Equal to what was resent: still not a change.
    assert!(d.placed(Some(REMOTE), 7.0, 8.0, 400, 300).is_empty());
}

// ---------------------------------------------------------------------------------------------
// A capture that began on a twin strip always warps when it ends (§2.7).
// ---------------------------------------------------------------------------------------------

#[test]
fn twin_capture_end_always_warps() {
    // Link lost: the capture is ended with a warp to the fallback point, never a plain end.
    let mut h = H::home();
    let exit = h.exit_through(Edge::Right, 0.25);
    h.confirm_removal(&exit);
    h.advance(1);
    let out = h.feed(closed(B));
    assert_eq!(warps(&out).len(), 1, "{out:?}");
    assert_eq!(warps(&out)[0].1, FALLBACK);
    assert!(!has_end_capture(&out));
    h.quiet();

    // The fence times out without an `Ended`: the second end warps too.
    let mut h = H::home();
    let exit = h.exit_through(Edge::Right, 0.25);
    h.confirm_removal(&exit);
    h.advance(1);
    let out = h.feed(Input::Command(Command::ReleaseControl));
    assert_eq!(warps(&out).len(), 1);
    assert_eq!(warps(&out)[0].1, FALLBACK);
    let t = h.now_ms();
    let out = h.tick(t + END_TIMEOUT);
    assert_eq!(warps(&out).len(), 1, "{out:?}");
    assert_eq!(warps(&out)[0].1, FALLBACK);
    assert!(!has_end_capture(&out));
    h.quiet();

    // The capture is lost: an extra plain warp to the fallback point.
    let mut h = H::home();
    let session = h.session.unwrap();
    let exit = h.exit_through(Edge::Right, 0.25);
    h.confirm_removal(&exit);
    let capture = h.last_begin();
    h.advance(1);
    let out = h.ended(capture, CaptureEnd::Lost);
    assert_eq!(warps(&out).len(), 1, "{out:?}");
    assert_eq!(warps(&out)[0].1, FALLBACK);
    assert_eq!(end_controls(&out), vec![(B, session, EndReason::Released)]);
    h.quiet();

    // A crossing back to this node's own display warps to the crossing point instead.
    let mut h = H::home();
    let exit = h.exit_through(Edge::Right, 0.25);
    h.confirm_removal(&exit);
    h.advance(1);
    // B's pointer is at (600, 375): go left through B's left edge into A.
    let out = h.motion(-700.0, 0.0);
    let (_, target) = warp(&out).expect("a warp, not a plain end");
    assert_eq!(target.0, LOCAL);
    assert!(target.1.x > 900.0, "A's right edge: {target:?}");
    assert!(!has_end_capture(&out));
    h.quiet();
}

// ---------------------------------------------------------------------------------------------
// Adversarial ownership: every down has exactly one up on the node that received it.
// ---------------------------------------------------------------------------------------------

#[test]
fn delayed_inject_done_during_entry() {
    let mut h = H::controlling();
    h.focus(Some(W1));
    let down = h.proj_key(P1, KEY, true);
    let down_id = injects(&down)[0].0;
    // The press has not been answered when the entry starts.
    h.aim();
    let trigger = h.trigger();
    let release = injects(&trigger)[0].0;
    let out = h.feed(Input::InjectDone {
        id: release,
        ok: true,
    });
    let op = bind(&out, true).expect("the release settled the drain");
    // The press's answer arrives late, during the bind: it changes nothing, whichever it says,
    // and neither does a repeat of the release's.
    for (id, ok) in [
        (down_id, true),
        (down_id, false),
        (release, true),
        (release, false),
    ] {
        let out = h.feed(Input::InjectDone { id, ok });
        assert!(out.is_empty(), "{out:?}");
    }
    h.advance(1);
    let out = h.bind_set(op, true, true);
    assert!(warp(&out).is_some());
    assert!(!has_inject(&out));
    h.quiet();
}

#[test]
fn failed_release_retry_never_lands_while_home() {
    let mut h = H::controlling();
    h.focus(Some(W1));
    let out = h.proj_key(P1, KEY, true);
    h.confirm(&out, true);
    h.now = ms(5_000);
    h.aim();
    let trigger = h.trigger();
    h.confirm(&trigger, false);
    h.run_until(5_000 + DRAIN_TIMEOUT, false);
    assert!(
        !h.log.iter().any(|o| matches!(o, Output::HomeBind { .. })),
        "the entry never got past the drain"
    );
    // The retry that lands afterwards is injected while still captured, and confirmed.
    let out = h.run_until(5_000 + DRAIN_TIMEOUT + 100, true);
    assert!(
        injects(&out).iter().any(|(_, c)| is_up_of(c, KEY)),
        "{out:?}"
    );
    // The fence expires; the second attempt settles at once and the entry commits.
    h.now = ms(5_000 + DRAIN_TIMEOUT + HOME_RETRY + 100);
    h.motion(1.0, 0.0);
    h.home_now();
    let entered = h.log.len();
    // Nothing is injected for as long as it is home, however long that is.
    let out = h.run_until(h.now_ms() + 3_000, true);
    assert!(!has_inject(&out), "{out:?}");
    assert!(
        !h.log[entered..]
            .iter()
            .any(|o| matches!(o, Output::Inject { .. })),
        "no injection while home"
    );
    h.quiet();
}

#[test]
fn remapped_key_released_to_right_node() {
    let mut config = H::config();
    config
        .remap
        .insert(B, crosspane_input::remap::RemapProfile::SwapCtrlGui);
    let mut h = H::bare_config(config, &[], &[]);
    h.project(W1, B, P1, TWIN, content1(), PlatformParking::Twin);
    h.place(B, P1, 1, Some(Proxy::standard()));
    h.cross();
    h.aim();
    // Physical left Control is sent to B as left GUI.
    let gui = HidUsage::keyboard(0xE3);
    let out = h.feed(Input::Capture(CaptureEvent::Key {
        usage: LCTRL,
        down: true,
        at: h.now,
    }));
    assert_eq!(sent_transitions(&out), vec![(B, Held::Key(gui), true)]);
    let op = h.reach_binding();
    h.advance(1);
    let out = h.bind_set(op, true, true);
    // The up goes to B, as the usage the press went as, and nothing is injected here.
    assert_eq!(
        sent_transitions(&out),
        vec![(B, Held::Key(gui), false)],
        "{out:?}"
    );
    assert!(!has_inject(&out));
    h.quiet();
}

#[test]
fn competing_projection_up_during_entry() {
    let mut h = H::with_extra_sources();
    h.cross();
    // The same key is down in two projected windows (each lease holds it; the journal keeps the
    // record until the last of them confirms its release).
    h.focus(Some(W1));
    let a = h.proj_key(P1, KEY, true);
    h.confirm(&a, true);
    h.focus(Some(W2));
    let b = h.proj_key(P2, KEY, true);
    h.confirm(&b, true);
    assert_eq!(h.e2_journal.items(), vec![Held::Key(KEY)]);
    h.focus(Some(W1));
    h.aim();
    let out = h.trigger();
    let ups = injects(&out);
    assert_eq!(ups.len(), 2, "each lease releases its own: {out:?}");
    // A competing up from B for the second projection arrives meanwhile: dropped.
    let late = h.proj_key(P2, KEY, false);
    assert!(!has_inject(&late), "{late:?}");
    let first = h.feed(Input::InjectDone {
        id: ups[0].0,
        ok: true,
    });
    assert!(binds(&first).is_empty());
    assert_eq!(
        h.e2_journal.items(),
        vec![Held::Key(KEY)],
        "the other lease still owes its release"
    );
    let second = h.feed(Input::InjectDone {
        id: ups[1].0,
        ok: true,
    });
    assert!(bind(&second, true).is_some(), "{second:?}");
    assert_eq!(h.e2_journal.items(), vec![]);
    h.quiet();
}

// ---------------------------------------------------------------------------------------------
// A1: the bind's removal is a phase; A4: the activation boundary; A6; A8; A9 and B1; B4.
// ---------------------------------------------------------------------------------------------

#[test]
fn teardown_retries_with_backoff() {
    let mut h = H::home();
    h.advance(1);
    let out = h.feed(Input::Command(Command::ReleaseControl));
    let first = bind(&out, false).expect("removal");
    let t0 = h.now_ms();
    // The removal fails again and again: retried after 100, 200, 400 ms (each a new operation).
    h.bind_set(first, false, false);
    let mut ops = vec![first];
    let mut expect = t0;
    for gap in [100, 200, 400, 800, 1_600, 2_000, 2_000] {
        let out = h.tick(expect + gap - 1);
        assert!(binds(&out).is_empty(), "too early: {out:?}");
        expect += gap;
        let out = h.tick(expect);
        let op = bind(&out, false).unwrap_or_else(|| panic!("a retry at {expect}: {out:?}"));
        assert!(!ops.contains(&op), "each attempt is its own operation");
        ops.push(op);
        h.bind_set(op, false, false);
        // Still arbitrated, and no session is admitted or begun.
        assert!(!h.probe(P1));
    }
    // An answer of an earlier attempt is stale; the current one's counts.
    h.bind_set(ops[1], false, true);
    assert!(!h.probe(P1), "stale");
    h.bind_set(*ops.last().unwrap(), false, true);
    assert!(h.probe(P1), "confirmed");
    // No more retries.
    assert!(binds(&h.tick(expect + 10_000)).is_empty());
    h.quiet();
}

#[test]
fn teardown_fence_refuses_input_and_sessions() {
    // The peer ends the session while home (the home bind is still installed).
    let mut h = H::home();
    let session = h.session.unwrap();
    h.advance(1);
    let out = h.feed(control(
        B,
        ControlMessage::EndControl {
            session,
            reason: EndReason::Released,
        },
    ));
    let removal = bind(&out, false).expect("removal");
    // The pointer is warped back to this node's display (the agent answers that at once; WP-2.43j:
    // a crossing's dwell never completes while a warp is unanswered).
    let (leave, _) = warp(&out).expect("the pointer leaves the twin");
    h.released(leave, Ok(Warp::Done));
    // Peer E2 key-downs during the fence are not injected (nothing can press the bind).
    assert!(!h.probe(P1));
    // An incoming StartControl is refused, though no session is running.
    assert_eq!(h.engine.controlling(), None);
    let out = h.feed(control(
        C,
        ControlMessage::StartControl {
            session: SessionId(7),
            entry_display: LOCAL,
            entry: point(1.0, 1.0),
            lock_keys: LockKeys::default(),
        },
    ));
    assert!(
        out.contains(&Output::SendControl {
            peer: C,
            msg: ControlMessage::ControlRefused {
                session: SessionId(7),
                reason: Refusal::Busy
            }
        }),
        "{out:?}"
    );
    // This node starts no new session as controller either: the edge is pushed, nothing happens.
    h.advance(10);
    let out = h.feed(Input::Capture(CaptureEvent::EdgePressed {
        portal: h.layout_portal,
        position: 0.5,
        at: h.now,
    }));
    assert!(!has_hud_show(&out), "{out:?}");
    // Once the removal is confirmed, everything works again.
    h.advance(1);
    h.bind_set(removal, false, true);
    assert!(h.probe(P1));
    h.advance(10);
    let out = h.feed(Input::Capture(CaptureEvent::EdgePressed {
        portal: h.layout_portal,
        position: 0.5,
        at: h.now,
    }));
    assert!(has_hud_show(&out), "{out:?}");
    h.feed(Input::Overlay(OverlayEvent::Unavailable(HUD)));
    h.quiet();
}

#[test]
fn exit_activation_buffers_transitions() {
    // `Started`, a button goes down, `CaptureBegun`: cancelled (also in the exit tests).
    // `Started`, modifiers pressed during the activation, then `CaptureBegun`: they count with
    // the snapshot: Control held at the start, Shift and Alt pressed during, Esc after: the chord.
    let mut h = H::home();
    let session = h.session.unwrap();
    let (id, _) = h.exit_to_activating(Edge::Right, 0.25);
    h.advance(1);
    h.started(id);
    for usage in [LSHIFT, LALT] {
        h.advance(1);
        let out = h.feed(Input::Capture(CaptureEvent::Key {
            usage,
            down: true,
            at: h.now,
        }));
        assert!(out.is_empty(), "buffered, nothing is routed: {out:?}");
    }
    h.advance(1);
    let out = h.capture_begun(id, vec![LCTRL]);
    assert!(left_home(&out), "{out:?}");
    assert!(
        sent_transitions(&out).is_empty(),
        "buffered keys are not forwarded"
    );
    h.confirm_removal(&out);
    h.advance(1);
    let out = h.feed(Input::Capture(CaptureEvent::Key {
        usage: ESC,
        down: true,
        at: h.now,
    }));
    assert_eq!(
        end_controls(&out),
        vec![(B, session, EndReason::Released)],
        "{out:?}"
    );
    h.quiet();

    // A modifier released during the activation is applied on top of the snapshot: Control was
    // held at the start, released during: Shift+Alt+Esc afterwards is not the chord.
    let mut h = H::home();
    let (id, _) = h.exit_to_activating(Edge::Right, 0.25);
    h.advance(1);
    h.started(id);
    h.advance(1);
    h.feed(Input::Capture(CaptureEvent::Key {
        usage: LCTRL,
        down: false,
        at: h.now,
    }));
    h.advance(1);
    let out = h.capture_begun(id, vec![LCTRL]);
    h.confirm_removal(&out);
    let mut all = Vec::new();
    for usage in [LSHIFT, LALT, ESC] {
        h.advance(1);
        all.extend(h.feed(Input::Capture(CaptureEvent::Key {
            usage,
            down: true,
            at: h.now,
        })));
    }
    assert!(end_controls(&all).is_empty(), "{all:?}");
    for usage in [LSHIFT, LALT, ESC] {
        h.feed(Input::Capture(CaptureEvent::Key {
            usage,
            down: false,
            at: h.now,
        }));
    }
    h.quiet();

    // The chord completes during the activation itself: ends everything, with the capture live.
    let mut h = H::home();
    let session = h.session.unwrap();
    let (id, _) = h.exit_to_activating(Edge::Right, 0.25);
    h.advance(1);
    h.started(id);
    h.advance(1);
    h.feed(Input::Capture(CaptureEvent::Key {
        usage: ESC,
        down: true,
        at: h.now,
    }));
    h.advance(1);
    let out = h.capture_begun(id, vec![LCTRL, LSHIFT, LALT]);
    assert_eq!(
        end_controls(&out),
        vec![(B, session, EndReason::Released)],
        "{out:?}"
    );
    assert_eq!(warps(&out).len(), 1);
    assert!(motions(&out).is_empty());
    h.ended(id, CaptureEnd::Requested);
    h.quiet();

    // `CaptureBegun` before `Started` (an agent that breaks its ordering): safe. The exit commits;
    // the button that arrives afterwards is an ordinary captured one, routed to B.
    let mut h = H::home();
    let (id, _) = h.exit_to_activating(Edge::Right, 0.25);
    h.advance(1);
    let out = h.capture_begun(id, vec![]);
    assert!(left_home(&out));
    h.advance(1);
    h.started(id);
    h.advance(1);
    let out = h.feed(Input::Capture(CaptureEvent::Button {
        button: BUTTON,
        down: true,
        at: h.now,
    }));
    assert_eq!(
        sent_transitions(&out),
        vec![(B, Held::Button(BUTTON), true)]
    );
    h.advance(1);
    let out = h.feed(Input::Capture(CaptureEvent::Button {
        button: BUTTON,
        down: false,
        at: h.now,
    }));
    assert_eq!(
        sent_transitions(&out),
        vec![(B, Held::Button(BUTTON), false)]
    );
    // A modifier pressed and released after the commit is routed like any other.
    for down in [true, false] {
        h.advance(1);
        let out = h.feed(Input::Capture(CaptureEvent::Key {
            usage: LSHIFT,
            down,
            at: h.now,
        }));
        assert_eq!(sent_transitions(&out), vec![(B, Held::Key(LSHIFT), down)]);
    }
    h.confirm_removal(&h.log.clone());
    h.quiet();

    // A modifier down and up before `Started` (dropped by the existing guard) changes nothing.
    let mut h = H::home();
    let (id, _) = h.exit_to_activating(Edge::Right, 0.25);
    h.advance(1);
    for down in [true, false] {
        let out = h.feed(Input::Capture(CaptureEvent::Key {
            usage: LCTRL,
            down,
            at: h.now,
        }));
        assert!(out.is_empty());
    }
    h.started(id);
    h.advance(1);
    let out = h.capture_begun(id, vec![]);
    assert!(left_home(&out));
    h.confirm_removal(&out);
    h.quiet();
}

#[test]
fn stranded_retry_runs_after_unlock() {
    let mut h = H::home();
    h.advance(1);
    let out = h.feed(locked());
    let (leave, _) = warp(&out).unwrap();
    h.advance(1);
    h.bind_set(h.last_removal(), false, true);
    h.released(leave, Ok(Warp::Skipped));
    let stranded = h.now_ms();
    // No other timer is left: the retry is only due once unlocked.
    assert!(
        h.engine.next_deadline().is_none_or(|d| d > h.now),
        "{:?}",
        h.engine.next_deadline()
    );
    // An unrelated input long after the retry deadline, while locked: nothing happens …
    let out = h.at(
        stranded + 5 * STRANDED_RETRY,
        Input::PeerRtt {
            peer: B,
            rtt: Duration::from_millis(5),
        },
    );
    assert!(warps(&out).is_empty(), "{out:?}");
    // … and the unlock itself runs the retry.
    let out = h.at(stranded + 5 * STRANDED_RETRY + 10, unlocked());
    let (retry, target) = warp(&out).expect("the retry still runs");
    assert_eq!(target, FALLBACK);
    h.released(retry, Ok(Warp::Done));
    assert!(warps(&h.tick(stranded + 100 * STRANDED_RETRY)).is_empty());
    h.quiet();
}

#[test]
fn entry_revalidates_before_release() {
    // The pointer leaves the proxy during the drain, without leaving B's display.
    let mut h = H::controlling();
    h.focus(Some(W1));
    let out = h.proj_key(P1, KEY, true);
    h.confirm(&out, true);
    h.aim();
    let trigger = h.trigger();
    h.advance(1);
    h.motion(0.0, -150.0);
    let out = h.confirm(&trigger, true);
    let op = bind(&out, true).expect("the drain settled");
    h.advance(1);
    let out = h.bind_set(op, true, true);
    assert!(home_failed(&out, HomeFailure::Gone), "{out:?}");
    assert!(
        warps(&out).is_empty() && end_controls(&out).is_empty(),
        "{out:?}"
    );
    assert!(bind(&out, false).is_some(), "the bind is removed");
    assert_eq!(h.engine.controlling(), Some(B));
    h.quiet();

    // … or during the bind.
    let mut h = H::aimed();
    let op = h.reach_binding();
    h.advance(1);
    h.motion(0.0, 250.0);
    h.advance(1);
    let out = h.bind_set(op, true, true);
    assert!(home_failed(&out, HomeFailure::Gone), "{out:?}");
    assert!(warps(&out).is_empty());
    assert_eq!(h.engine.controlling(), Some(B));
    h.quiet();

    // The physical pointer has been still for too long by the time the bind is confirmed.
    let mut h = H::aimed();
    let op = h.reach_binding();
    h.advance(LOCAL_MOTION_AGE + 20);
    let out = h.bind_set(op, true, true);
    assert!(home_failed(&out, HomeFailure::Guard), "{out:?}");
    assert!(warps(&out).is_empty());
    h.quiet();

    // The placement changed (a new generation) or the strip set did, during the bind.
    for (origin_x, tag) in [(220.0, "moved"), (600.0, "touching the right edge")] {
        let mut h = H::aimed();
        let op = h.reach_binding();
        h.advance(1);
        h.place(
            B,
            P1,
            2,
            Some(Proxy {
                origin: point(origin_x, 300.0),
                ..Proxy::standard()
            }),
        );
        h.advance(1);
        let out = h.bind_set(op, true, true);
        assert!(home_failed(&out, HomeFailure::Gone), "{tag}: {out:?}");
        assert!(warps(&out).is_empty(), "{tag}");
        assert!(bind(&out, false).is_some(), "{tag}");
        h.quiet();
    }

    // The warp target comes from the tracker as it is when the capture is released, not from
    // the report: nudge the pointer 20 pixels right during the bind.
    let mut h = H::aimed();
    let op = h.reach_binding();
    h.advance(1);
    h.motion(20.0, 0.0);
    h.advance(1);
    let out = h.bind_set(op, true, true);
    assert_eq!(
        warps(&out),
        vec![(op, (TWIN, point(120.0, 140.0)))],
        "{out:?}"
    );
    h.quiet();
}

#[test]
fn next_deadline_advances_after_fence_expiry() {
    let mut h = H::aimed();
    h.now = ms(2_000);
    h.motion(1.0, 0.0);
    let op = h.reach_binding();
    h.advance(1);
    let failed_at = h.now_ms();
    let out = h.bind_set(op, true, false);
    let removal = bind(&out, false).unwrap();
    h.bind_set(removal, false, true);
    let fence = failed_at + HOME_RETRY;
    // End the session (the fence outlives it): nothing else is pending.
    let out = h.feed(Input::Command(Command::ReleaseControl));
    assert!(end_controls(&out).len() == 1);
    h.advance(1);
    h.feed(Input::Capture(CaptureEvent::Ended {
        id: h.capture.unwrap(),
        reason: CaptureEnd::Requested,
    }));
    // The passive fence is the only deadline left, and it is in the future.
    let deadline = h.engine.next_deadline();
    assert_eq!(deadline, Some(ms(fence)));
    assert!(deadline.is_some_and(|d| d > h.now), "{deadline:?}");
    // Once it has expired, it is gone: it never pins the agent to a zero timeout.
    let at = deadline.unwrap();
    h.at(at.as_nanos() / 1_000_000, Input::Tick);
    assert_eq!(h.engine.next_deadline(), None);
    // An unrelated input after the expiry does not bring it back.
    h.advance(10);
    h.feed(Input::PeerRtt {
        peer: B,
        rtt: Duration::from_millis(5),
    });
    assert_eq!(h.engine.next_deadline(), None);
    h.quiet();
}

// ---------------------------------------------------------------------------------------------
// Review round 1: the harness proves what it claims (findings 10, 11).
// ---------------------------------------------------------------------------------------------

fn key_inject(id: u64, down: bool) -> Output {
    Output::Inject {
        id: InjectId(id),
        cmd: InjectCmd::Key { usage: KEY, down },
    }
}

#[test]
#[should_panic(expected = "with nothing pressed")]
fn harness_rejects_a_release_before_its_press() {
    let mut h = H::bare();
    h.record(&[key_inject(1, false)]);
}

#[test]
#[should_panic(expected = "with nothing pressed")]
fn harness_rejects_a_second_release_of_one_press() {
    // One down, then two ups that both succeed: the second has nothing to release.
    let mut h = H::bare();
    h.record(&[key_inject(1, true), key_inject(2, false)]);
    h.feed(Input::InjectDone {
        id: InjectId(2),
        ok: true,
    });
    h.record(&[key_inject(3, false)]);
}

#[test]
#[should_panic(expected = "released again with no failure and no retry due")]
fn harness_rejects_a_repeated_release_that_is_not_a_retry() {
    let mut h = H::bare();
    h.record(&[
        key_inject(1, true),
        key_inject(2, false),
        key_inject(3, false),
    ]);
}

#[test]
fn harness_accepts_a_retry_after_a_failure_and_settles_it() {
    let mut h = H::bare();
    h.record(&[key_inject(1, true), key_inject(2, false)]);
    h.feed(Input::InjectDone {
        id: InjectId(2),
        ok: false,
    });
    h.record(&[key_inject(3, false)]);
    h.feed(Input::InjectDone {
        id: InjectId(3),
        ok: true,
    });
    let trace = &h.injected[&Held::Key(KEY)];
    assert_eq!((trace.downs, trace.confirmed, trace.pending), (1, 1, 0));
    // And a retry that is due by time (the ledger's own 50 ms) is one too.
    h.record(&[key_inject(4, true), key_inject(5, false)]);
    h.advance(50);
    h.record(&[key_inject(6, false)]);
}

#[test]
#[should_panic(expected = "between the bind's installation and its confirmed removal")]
fn harness_rejects_injection_while_the_bind_may_exist() {
    let mut h = H::bare();
    h.record(&[
        Output::HomeBind {
            op: HomeOp(1),
            install: true,
        },
        key_inject(1, true),
    ]);
}

#[test]
#[should_panic(expected = "released again with no failure and no retry due")]
fn harness_rejects_an_immediate_repeat_after_a_failed_release_was_retried() {
    // The first release failed, so the second is a retry. The third comes with the second
    // neither failed nor old: no retry is due, whatever the first one did.
    let mut h = H::bare();
    h.record(&[key_inject(1, true), key_inject(2, false)]);
    h.feed(Input::InjectDone {
        id: InjectId(2),
        ok: false,
    });
    h.record(&[key_inject(3, false)]);
    h.record(&[key_inject(4, false)]);
}

#[test]
#[should_panic(expected = "released again with no failure and no retry due")]
fn harness_rejects_an_immediate_repeat_after_a_timed_out_release_was_retried() {
    let mut h = H::bare();
    h.record(&[key_inject(1, true), key_inject(2, false)]);
    h.advance(50);
    h.record(&[key_inject(3, false)]);
    h.record(&[key_inject(4, false)]);
}

#[test]
#[should_panic(expected = "released again with no failure and no retry due")]
fn harness_rejects_a_retry_that_is_due_only_against_the_oldest_request() {
    let mut h = H::bare();
    h.record(&[key_inject(1, true), key_inject(2, false)]);
    h.advance(50);
    h.record(&[key_inject(3, false)]);
    // 99 ms after the first request, 49 after the newest.
    h.advance(49);
    h.record(&[key_inject(4, false)]);
}

#[test]
fn harness_measures_each_retry_against_the_newest_request() {
    let mut h = H::bare();
    h.record(&[key_inject(1, true), key_inject(2, false)]);
    h.advance(50);
    h.record(&[key_inject(3, false)]);
    // The next one is due 50 ms after the newest request, not the first.
    h.advance(50);
    h.record(&[key_inject(4, false)]);
    // Settling by the newest request ends the chain; the older ones' late answers change nothing.
    h.feed(Input::InjectDone {
        id: InjectId(4),
        ok: true,
    });
    for id in [2, 3] {
        h.feed(Input::InjectDone {
            id: InjectId(id),
            ok: true,
        });
    }
    let trace = &h.injected[&Held::Key(KEY)];
    assert_eq!((trace.downs, trace.confirmed, trace.pending), (1, 1, 0));
    assert!(trace.chains.is_empty(), "{trace:?}");
}

#[test]
fn harness_allows_one_release_chain_per_press() {
    // Two presses of one key (two injectors): two first requests, one chain each; a retry then
    // needs a due newest request in one of them.
    let mut h = H::bare();
    h.record(&[
        key_inject(1, true),
        key_inject(2, true),
        key_inject(3, false),
        key_inject(4, false),
    ]);
    h.feed(Input::InjectDone {
        id: InjectId(3),
        ok: false,
    });
    h.record(&[key_inject(5, false)]);
    h.feed(Input::InjectDone {
        id: InjectId(5),
        ok: true,
    });
    h.feed(Input::InjectDone {
        id: InjectId(4),
        ok: true,
    });
    let trace = &h.injected[&Held::Key(KEY)];
    assert_eq!((trace.downs, trace.confirmed, trace.pending), (2, 2, 0));
}

#[test]
#[should_panic(expected = "released again with no failure and no retry due")]
fn harness_rejects_a_third_request_for_two_presses() {
    let mut h = H::bare();
    h.record(&[
        key_inject(1, true),
        key_inject(2, true),
        key_inject(3, false),
        key_inject(4, false),
        key_inject(5, false),
    ]);
}

fn bind_out(op: u64, install: bool) -> Output {
    Output::HomeBind {
        op: HomeOp(op),
        install,
    }
}

fn removal_ack(op: u64, ok: bool) -> Input {
    Input::HomeBindSet {
        op: HomeOp(op),
        install: false,
        result: if ok { Ok(()) } else { Err(Failure::Other) },
    }
}

#[test]
fn harness_fence_ends_with_the_removal_it_waits_for_and_only_then() {
    let mut h = H::bare();
    h.record(&[bind_out(1, true), bind_out(2, false)]);
    // A failed removal ends nothing; a removal that is not the outstanding one ends nothing.
    h.feed(removal_ack(2, false));
    assert!(h.fence_open);
    h.feed(removal_ack(9, true));
    assert!(h.fence_open);
    // A retry is a fresh op: the newest request is the one waited for.
    h.record(&[bind_out(3, false)]);
    h.feed(removal_ack(2, true));
    assert!(h.fence_open, "an older attempt's acknowledgement");
    h.feed(removal_ack(3, true));
    assert!(!h.fence_open);
    h.record(&[key_inject(1, true)]);
}

#[test]
#[should_panic(expected = "between the bind's installation and its confirmed removal")]
fn harness_keeps_the_fence_across_a_stale_removal_acknowledgement() {
    // The first interval ends with its removal's acknowledgement. A second bind is installed; a
    // duplicate of the first removal's acknowledgement must not open the second interval's fence.
    let mut h = H::bare();
    h.record(&[bind_out(1, true), bind_out(2, false)]);
    h.feed(removal_ack(2, true));
    assert!(!h.fence_open);
    h.record(&[bind_out(3, true)]);
    h.feed(removal_ack(2, true));
    assert!(h.fence_open, "the stale acknowledgement lifted the fence");
    h.record(&[key_inject(1, true)]);
}

#[test]
#[should_panic(expected = "between the bind's installation and its confirmed removal")]
fn harness_consumes_a_removal_acknowledgement_once() {
    // Removal 2 of the first bind is acknowledged; the second bind's removal request reuses no
    // op. A repeat of the first acknowledgement after the second install is not the second
    // interval's removal, whatever order the outputs came in.
    let mut h = H::bare();
    h.record(&[bind_out(1, true), bind_out(2, false)]);
    h.feed(removal_ack(2, true));
    h.record(&[bind_out(3, true), bind_out(4, false)]);
    h.feed(removal_ack(2, true));
    h.record(&[key_inject(1, true)]);
}

#[test]
#[should_panic(expected = "while it was already up")]
fn harness_rejects_a_peer_release_before_its_press() {
    let mut h = H::bare();
    h.record(&[Output::SendInput {
        peer: B,
        msg: InputMessage::Key {
            session: SessionId(1),
            seq: 1,
            usage: KEY,
            down: false,
        },
    }]);
}

#[test]
fn provenance_holds_across_the_whole_fenced_interval() {
    // Every output between the bind's installation request and the confirmed removal: no
    // injection, whatever produced it (inputs, callbacks, ticks, retries).
    let mut h = H::with_extra_sources();
    h.cross();
    h.focus(Some(W1));
    h.aim();
    let op = h.reach_binding();
    let from = h
        .log
        .iter()
        .position(|o| matches!(o, Output::HomeBind { install: true, .. }))
        .expect("the install request");
    h.advance(1);
    h.bind_set(op, true, true);
    h.advance(1);
    h.released(op, Ok(Warp::Done));
    h.storm();
    h.run_until(h.now_ms() + 600, true);
    let (id, _) = h.exit_to_activating(Edge::Left, 0.5);
    h.advance(1);
    h.started(id);
    h.advance(1);
    let out = h.capture_begun(id, vec![]);
    let removal = bind(&out, false).unwrap();
    h.advance(1);
    h.bind_set(removal, false, false);
    h.run_until(h.now_ms() + 500, true);
    h.storm();
    h.advance(1);
    h.bind_set(h.last_removal(), false, true);
    let to = h.log.len();
    assert!(
        !h.log[from..to]
            .iter()
            .any(|o| matches!(o, Output::Inject { .. })),
        "something was injected while the home bind may have existed"
    );
    assert!(h.probe(P1), "and the filter is off afterwards");
    h.quiet();
}

// ---------------------------------------------------------------------------------------------
// Review round 1: engine fixes (findings 1-9).
// ---------------------------------------------------------------------------------------------

fn start_control_to(out: &[Output]) -> Option<NodeId> {
    out.iter().find_map(|o| match o {
        Output::SendControl {
            peer,
            msg: ControlMessage::StartControl { .. },
        } => Some(*peer),
        _ => None,
    })
}

#[test]
fn exit_capture_loss_during_activation_leaves_home() {
    // `Ended { Lost | Aborted }`: the backend (or the watchdog) took the capture away, and the
    // strips with it, with no portal error before it. Nothing installed is left to push against:
    // leave home, with the bind's removal and the warp back to the desktop.
    for reason in [CaptureEnd::Lost, CaptureEnd::Aborted] {
        for cancelled in [false, true] {
            let tag = format!("{reason:?}, cancelled: {cancelled}");
            let mut h = H::home();
            let session = h.session.unwrap();
            let (id, _) = h.exit_to_activating(Edge::Right, 0.25);
            h.advance(1);
            h.started(id);
            if cancelled {
                let t = h.now_ms();
                h.tick(t + START_TIMEOUT);
            }
            h.advance(1);
            let out = h.ended(id, reason);
            assert!(home_failed(&out, HomeFailure::Gone), "{tag}: {out:?}");
            assert_eq!(
                end_controls(&out),
                vec![(B, session, EndReason::Released)],
                "{tag}"
            );
            assert_eq!(warps(&out).len(), 1, "{tag}: {out:?}");
            assert_eq!(warps(&out)[0].1, FALLBACK, "{tag}");
            assert!(bind(&out, false).is_some(), "{tag}: the bind is removed");
            assert!(!has_end_capture(&out), "{tag}: it is already over");
            assert!(has_hud_hide(&out), "{tag}");
            assert_eq!(h.engine.controlling(), None, "{tag}");
            h.confirm_removal(&out);
            h.quiet();
        }
    }
    // An end that this node asked for (`Requested`) is not a loss: the exit is just over.
    let mut h = H::home();
    let (id, _) = h.exit_to_activating(Edge::Right, 0.25);
    h.advance(1);
    h.started(id);
    h.advance(1);
    let out = h.ended(id, CaptureEnd::Requested);
    assert!(
        end_controls(&out).is_empty() && warps(&out).is_empty(),
        "{out:?}"
    );
    assert_eq!(h.engine.controlling(), Some(B));
    h.quiet();
}

#[test]
fn handoff_to_a_third_node_waits_for_the_bind_removal() {
    // A crossing into another node starts a new controller session: not while the home bind may
    // exist. During Binding (bind requested) the handoff is blocked and the entry goes on.
    let mut h = H::projected();
    h.add_c_right();
    h.cross();
    h.aim();
    let op = h.reach_binding();
    h.advance(1);
    let out = h.motion(800.0, 0.0);
    assert_eq!(start_control_to(&out), None, "{out:?}");
    assert!(
        notices(&out).is_empty() && end_controls(&out).is_empty(),
        "the pointer just stays at B's edge: {out:?}"
    );
    assert_eq!(h.engine.controlling(), Some(B));
    // The bind fails; its removal keeps failing: still no handoff, however often it is tried.
    h.advance(1);
    let out = h.bind_set(op, true, false);
    assert!(home_failed(&out, HomeFailure::Bind), "{out:?}");
    h.bind_set(bind(&out, false).unwrap(), false, false);
    for _ in 0..3 {
        let t = h.now_ms();
        let out = h.tick(t + 2_000);
        let retry = bind(&out, false).expect("a retry");
        h.advance(1);
        let out = h.motion(800.0, 0.0);
        assert_eq!(start_control_to(&out), None, "{out:?}");
        h.bind_set(retry, false, false);
    }
    assert_eq!(h.engine.controlling(), Some(B));
    // Confirmed: the same push now hands the session over.
    h.advance(1);
    h.bind_set(h.last_removal(), false, true);
    h.advance(1);
    let out = h.motion(800.0, 0.0);
    assert_eq!(start_control_to(&out), Some(C), "{out:?}");
    h.quiet();

    // During Draining no bind exists yet: the handoff abandons the entry and proceeds.
    let mut h = H::projected();
    h.add_c_right();
    h.cross();
    h.focus(Some(W1));
    let out = h.proj_key(P1, KEY, true);
    h.confirm(&out, true);
    h.aim();
    let trigger = h.trigger();
    h.advance(1);
    let out = h.motion(800.0, 0.0);
    assert_eq!(start_control_to(&out), Some(C), "{out:?}");
    assert!(home_failed(&out, HomeFailure::Gone), "{out:?}");
    assert!(bind(&out, false).is_none());
    h.confirm(&trigger, true);
    h.quiet();

    // After a home exit whose removal is still pending, the same.
    let mut h = H::projected();
    h.add_c_right();
    h.cross();
    h.aim();
    h.home_now();
    let exit = h.exit_through(Edge::Right, 0.25);
    h.advance(10);
    let out = h.motion(500.0, 0.0);
    assert_eq!(start_control_to(&out), None, "{out:?}");
    assert_eq!(h.engine.controlling(), Some(B));
    h.confirm_removal(&exit);
    h.advance(10);
    let out = h.motion(500.0, 0.0);
    assert_eq!(start_control_to(&out), Some(C), "{out:?}");
    h.quiet();
}

#[test]
fn late_drain_acknowledgement_is_too_late() {
    let mut h = H::controlling();
    h.focus(Some(W1));
    let out = h.proj_key(P1, KEY, true);
    h.confirm(&out, true);
    h.now = ms(5_000);
    h.aim();
    let trigger = h.trigger();
    let release = injects(&trigger)[0].0;
    // The last confirmation arrives after the deadline, before any tick: the entry is over.
    h.now = ms(5_000 + DRAIN_TIMEOUT + 10);
    let out = h.feed(Input::InjectDone {
        id: release,
        ok: true,
    });
    assert!(home_failed(&out, HomeFailure::Drain), "{out:?}");
    assert!(binds(&out).is_empty() && warps(&out).is_empty(), "{out:?}");
    assert_eq!(h.engine.controlling(), Some(B));
    // Fresh motion right after does not bring it back: the fence holds.
    h.advance(1);
    h.motion(1.0, 0.0);
    let out = h.trigger();
    assert_no_entry(&h, &out);
    h.quiet();

    // Exactly at the deadline is already too late; just before it is not.
    for (late, enters) in [(DRAIN_TIMEOUT - 1, true), (DRAIN_TIMEOUT, false)] {
        let mut h = H::controlling();
        h.focus(Some(W1));
        let out = h.proj_key(P1, KEY, true);
        h.confirm(&out, true);
        h.now = ms(5_000);
        h.aim();
        let trigger = h.trigger();
        let release = injects(&trigger)[0].0;
        h.now = ms(5_000 + late);
        let out = h.feed(Input::InjectDone {
            id: release,
            ok: true,
        });
        assert_eq!(bind(&out, true).is_some(), enters, "{late}: {out:?}");
    }
}

#[test]
fn late_focus_is_too_late() {
    let mut h = H::aimed();
    let session = h.session.unwrap();
    h.reach_focusing();
    let started = h.now_ms();
    // The window takes focus after the deadline, before any tick: home is not entered.
    h.now = ms(started + FOCUS_TIMEOUT + 1);
    let out = h.focus(Some(W1));
    assert!(home_failed(&out, HomeFailure::Focus), "{out:?}");
    assert!(
        !has_notice(
            &out,
            &Notice::Home {
                key: key(P1),
                entered: true
            }
        ),
        "{out:?}"
    );
    assert_eq!(end_controls(&out), vec![(B, session, EndReason::Released)]);
    assert_eq!(warps(&out)[0].1, FALLBACK);
    assert!(bind(&out, false).is_some());
    h.confirm_removal(&out);
    h.quiet();
}

#[test]
fn failed_cancel_warp_leaves_home() {
    // The A4 cancel warp (a button held at activation) was meant to keep the pointer on the twin.
    // If it is skipped or fails, the pointer may be anywhere: recover it like any other leave.
    for result in [Ok(Warp::Skipped), Err(Failure::Other)] {
        for ended_first in [false, true] {
            let tag = format!("{result:?}, ended first: {ended_first}");
            let mut h = H::home();
            let session = h.session.unwrap();
            let (id, _) = h.exit_to_activating(Edge::Right, 0.25);
            h.advance(1);
            h.started(id);
            h.advance(1);
            h.feed(Input::Capture(CaptureEvent::Button {
                button: BUTTON,
                down: true,
                at: h.now,
            }));
            h.advance(1);
            let out = h.capture_begun(id, vec![]);
            let (op, _) = warp(&out).expect("the cancel warp");
            if ended_first {
                h.advance(1);
                h.ended(id, CaptureEnd::Requested);
            }
            h.advance(1);
            let out = h.released(op, result);
            assert!(home_failed(&out, HomeFailure::Warp), "{tag}: {out:?}");
            assert_eq!(
                end_controls(&out),
                vec![(B, session, EndReason::Released)],
                "{tag}"
            );
            assert_eq!(warps(&out).len(), 1, "{tag}: {out:?}");
            assert_eq!(warps(&out)[0].1, FALLBACK, "{tag}");
            assert!(bind(&out, false).is_some(), "{tag}");
            assert_eq!(h.engine.controlling(), None, "{tag}");
            // The capture's end finishes the return when it may still exist.
            if !ended_first {
                assert!(!has_hud_hide(&out), "{tag}");
                h.advance(1);
                let done = h.ended(id, CaptureEnd::Requested);
                assert!(has_hud_hide(&done), "{tag}");
            }
            let leave = warps(&out)[0].0;
            h.released(leave, Ok(Warp::Done));
            h.confirm_removal(&out);
            h.quiet();
        }
    }
}

#[test]
fn release_chord_works_while_an_exit_is_being_cancelled() {
    let mut h = H::home();
    let session = h.session.unwrap();
    let (id, _) = h.exit_to_activating(Edge::Right, 0.25);
    h.advance(1);
    h.started(id);
    let begun = h.now_ms();
    // The exit times out: its capture is being ended and still exists.
    let out = h.tick(begun + START_TIMEOUT);
    assert!(has_end_capture(&out));
    for usage in [LCTRL, LSHIFT, LALT] {
        h.advance(1);
        let out = h.feed(Input::Capture(CaptureEvent::Key {
            usage,
            down: true,
            at: h.now,
        }));
        assert!(out.is_empty(), "nothing is routed: {out:?}");
    }
    h.advance(1);
    let out = h.feed(Input::Capture(CaptureEvent::Key {
        usage: ESC,
        down: true,
        at: h.now,
    }));
    assert_eq!(
        end_controls(&out),
        vec![(B, session, EndReason::Released)],
        "{out:?}"
    );
    assert_eq!(warps(&out)[0].1, FALLBACK);
    assert!(bind(&out, false).is_some());
    assert!(
        !has_hud_hide(&out),
        "the capture may still exist: its end fences the return"
    );
    h.advance(1);
    let done = h.ended(id, CaptureEnd::Requested);
    assert!(has_hud_hide(&done));
    h.confirm_removal(&out);
    h.quiet();

    // A modifier that was down before home and released natively while home is not remembered:
    // without it the keys are not the chord.
    let mut h = H::controlling();
    h.feed(Input::Capture(CaptureEvent::Key {
        usage: LCTRL,
        down: true,
        at: h.now,
    }));
    h.aim();
    h.home_now();
    let (id, _) = h.exit_to_activating(Edge::Right, 0.25);
    h.advance(1);
    h.started(id);
    let begun = h.now_ms();
    h.tick(begun + START_TIMEOUT);
    let mut all = Vec::new();
    for usage in [LSHIFT, LALT, ESC] {
        h.advance(1);
        all.extend(h.feed(Input::Capture(CaptureEvent::Key {
            usage,
            down: true,
            at: h.now,
        })));
    }
    assert!(end_controls(&all).is_empty(), "{all:?}");
    assert_eq!(h.engine.controlling(), Some(B));
    h.advance(1);
    h.ended(id, CaptureEnd::Requested);
    h.quiet();
}

#[test]
fn duplicate_exit_success_does_not_end_the_resumed_session() {
    let mut h = H::home();
    let (id, _) = h.exit_to_activating(Edge::Right, 0.25);
    h.advance(1);
    h.started(id);
    h.advance(1);
    let exit = h.capture_begun(id, vec![]);
    assert!(left_home(&exit));
    // A repeat of the success of the capture that committed is not a new capture.
    h.advance(1);
    let dup = h.capture_begun(id, vec![]);
    assert!(dup.is_empty(), "{dup:?}");
    assert_eq!(h.engine.controlling(), Some(B));
    h.advance(1);
    assert_eq!(
        motions(&h.motion(1.0, 0.0)).len(),
        1,
        "the session still routes"
    );
    h.confirm_removal(&exit);
    // Once the session is ending, the capture may still be live: a repeat then ends it.
    h.advance(1);
    h.feed(Input::Command(Command::ReleaseControl));
    h.advance(1);
    let again = h.capture_begun(id, vec![]);
    assert!(has_end_capture(&again), "{again:?}");
    h.ended(id, CaptureEnd::Requested);
    h.quiet();
}

#[test]
fn duplicate_exit_success_is_ignored_for_the_whole_retained_lifetime() {
    // After the exit commits and its bind removal is confirmed, the capture is retained through
    // a handoff to a third node ...
    let mut h = H::projected();
    h.add_c_right();
    h.cross();
    h.aim();
    h.home_now();
    let exit = h.exit_through(Edge::Right, 0.25);
    let capture = h.last_begin();
    h.confirm_removal(&exit);
    h.advance(10);
    let out = h.motion(500.0, 0.0);
    let session = out
        .iter()
        .find_map(|o| match o {
            Output::SendControl {
                msg: ControlMessage::StartControl { session, .. },
                ..
            } => Some(*session),
            _ => None,
        })
        .expect("the handoff to C starts");
    h.advance(1);
    let dup = h.capture_begun(capture, vec![]);
    assert!(dup.is_empty(), "the handshake keeps its capture: {dup:?}");
    h.advance(1);
    h.session = Some(session);
    let out = h.feed(Input::Link(LinkEvent::Control {
        peer: C,
        msg: ControlMessage::ControlStarted { session },
    }));
    assert!(end_controls(&out).is_empty(), "{out:?}");
    assert_eq!(h.engine.controlling(), Some(C));
    h.quiet();

    // ... and through a later entry: while Binding (the same capture is still the live one),
    // and while Draining.
    for draining in [false, true] {
        let mut h = H::projected();
        h.cross();
        h.aim();
        h.home_now();
        let exit = h.exit_through(Edge::Right, 0.25);
        let capture = h.last_begin();
        let exited = h.now_ms();
        h.confirm_removal(&exit);
        if draining {
            // A key is down in W: the drain waits for it.
            h.focus(Some(W1));
            let out = h.proj_key(P1, KEY, true);
            h.confirm(&out, true);
        }
        h.now = ms(exited + REENTRY_GUARD + 50);
        h.motion(-100.0, 0.0);
        let out = h.report(P1, point(300.0, 75.0));
        let entering = if draining {
            assert!(
                binds(&out).is_empty() && !injects(&out).is_empty(),
                "{out:?}"
            );
            injects(&out)
        } else {
            assert!(bind(&out, true).is_some(), "{out:?}");
            vec![]
        };
        h.advance(1);
        let dup = h.capture_begun(capture, vec![]);
        assert!(!has_end_capture(&dup), "draining: {draining}: {dup:?}");
        assert!(dup.is_empty(), "draining: {draining}: {dup:?}");
        // The entry carries on: the capture is still there to be released.
        if draining {
            let out = h.confirm(
                &[Output::Inject {
                    id: entering[0].0,
                    cmd: entering[0].1.clone(),
                }],
                true,
            );
            let op = bind(&out, true).expect("the drain settles");
            h.advance(1);
            let out = h.bind_set(op, true, true);
            assert!(warp(&out).is_some(), "{out:?}");
        } else {
            let op = bind(&out, true).unwrap();
            h.advance(1);
            let out = h.bind_set(op, true, true);
            assert!(warp(&out).is_some(), "{out:?}");
        }
        h.quiet();
    }
}

#[test]
fn modifiers_observed_during_activation_survive_a_cancellation() {
    // Control, Shift and Alt go down while the exit capture is being activated; the exit then
    // times out and the capture is being ended (still live). Escape arrives: the chord.
    let mut h = H::home();
    let session = h.session.unwrap();
    let (id, _) = h.exit_to_activating(Edge::Right, 0.25);
    h.advance(1);
    h.started(id);
    for usage in [LCTRL, LSHIFT, LALT] {
        h.advance(1);
        let out = h.feed(Input::Capture(CaptureEvent::Key {
            usage,
            down: true,
            at: h.now,
        }));
        assert!(out.is_empty(), "buffered: {out:?}");
    }
    let begun = h.now_ms();
    let out = h.tick(begun + START_TIMEOUT);
    assert!(
        has_end_capture(&out) && end_controls(&out).is_empty(),
        "{out:?}"
    );
    h.advance(1);
    let out = h.feed(Input::Capture(CaptureEvent::Key {
        usage: ESC,
        down: true,
        at: h.now,
    }));
    assert_eq!(
        end_controls(&out),
        vec![(B, session, EndReason::Released)],
        "{out:?}"
    );
    h.advance(1);
    h.ended(id, CaptureEnd::Requested);
    h.confirm_removal(&out);
    h.quiet();

    // The same when the success arrives late (after the deadline) and cancels the exit: the
    // snapshot and what was buffered both count.
    let mut h = H::home();
    let session = h.session.unwrap();
    let (id, _) = h.exit_to_activating(Edge::Right, 0.25);
    h.advance(1);
    h.started(id);
    for usage in [LSHIFT, LALT] {
        h.advance(1);
        h.feed(Input::Capture(CaptureEvent::Key {
            usage,
            down: true,
            at: h.now,
        }));
    }
    h.advance(START_TIMEOUT + 5);
    let out = h.capture_begun(id, vec![LCTRL]);
    assert!(
        has_end_capture(&out) && end_controls(&out).is_empty(),
        "{out:?}"
    );
    h.advance(1);
    let out = h.feed(Input::Capture(CaptureEvent::Key {
        usage: ESC,
        down: true,
        at: h.now,
    }));
    assert_eq!(
        end_controls(&out),
        vec![(B, session, EndReason::Released)],
        "{out:?}"
    );
    h.advance(1);
    h.ended(id, CaptureEnd::Requested);
    h.confirm_removal(&out);
    h.quiet();

    // A chord completed entirely during the activation ends everything when the exit times out.
    let mut h = H::home();
    let session = h.session.unwrap();
    let (id, _) = h.exit_to_activating(Edge::Right, 0.25);
    h.advance(1);
    h.started(id);
    for usage in [LCTRL, LSHIFT, LALT, ESC] {
        h.advance(1);
        h.feed(Input::Capture(CaptureEvent::Key {
            usage,
            down: true,
            at: h.now,
        }));
    }
    let begun = h.now_ms();
    let out = h.tick(begun + START_TIMEOUT);
    assert_eq!(
        end_controls(&out),
        vec![(B, session, EndReason::Released)],
        "{out:?}"
    );
    h.advance(1);
    h.ended(id, CaptureEnd::Requested);
    h.confirm_removal(&out);
    h.quiet();
}

#[test]
fn stranded_pointer_waits_for_the_rearm_after_a_panic() {
    let mut h = H::home();
    h.advance(1);
    let out = h.feed(Input::Command(Command::Panic));
    assert!(out.contains(&Output::EngineGate(false)));
    let (leave, _) = warp(&out).expect("the fallback warp");
    h.confirm_removal(&out);
    // The gate is closed: the warp is skipped.
    h.released(leave, Ok(Warp::Skipped));
    let t = h.now_ms();
    // Far longer than the retry budget's worth of seconds: nothing is retried while closed (the
    // budget is not spent on warps that can only be skipped), and nothing spins.
    for second in 1..=15 {
        let out = h.tick(t + second * STRANDED_RETRY);
        assert!(warps(&out).is_empty(), "second {second}: {out:?}");
        assert!(
            h.engine.next_deadline().is_none_or(|d| d > h.now),
            "second {second}: {:?}",
            h.engine.next_deadline()
        );
    }
    // The re-arm reopens the gate, and the pointer is brought back at once.
    h.advance(1);
    let out = h.feed(Input::Command(Command::Rearm));
    let gate = out
        .iter()
        .position(|o| *o == Output::EngineGate(true))
        .expect("the gate reopens");
    let retry = out
        .iter()
        .position(|o| matches!(o, Output::ReleaseAndWarp { .. }))
        .expect("the stranded pointer is warped");
    assert!(gate < retry, "{out:?}");
    let (op, target) = warp(&out).unwrap();
    assert_eq!(target, FALLBACK);
    h.released(op, Ok(Warp::Done));
    let now = h.now_ms();
    assert!(warps(&h.tick(now + 10 * STRANDED_RETRY)).is_empty());
    h.quiet();
}

fn ids_of(portals: &[CapturePortal]) -> Vec<PortalId> {
    portals.iter().map(|p| p.id).collect()
}

/// A controls B while the backend still has A's original A -> B strip installed. A replacement
/// A -> C set has been offered but not answered; it reuses the original strip's id.
fn local_override_with_pending_replacement() -> (H, Vec<CapturePortal>) {
    let mut h = H::bare();
    h.feed(Input::PeerDisplays {
        peer: C,
        displays: vec![display(1)],
    });
    h.feed(Input::PeerUp { peer: C });
    h.cross();
    h.auto_portals = false;
    h.advance(1);
    let out = h.feed(Input::Layout(vec![
        Placement {
            node: A,
            display: LOCAL,
            origin: PointMm::zero(),
            version: 2,
        },
        Placement {
            node: B,
            display: REMOTE,
            origin: PointMm::new(200.0, 0.0),
            version: 2,
        },
        Placement {
            node: C,
            display: LOCAL,
            origin: PointMm::new(100.0, 0.0),
            version: 2,
        },
    ]));
    let replacement = set_portals(&out).pop().expect("A -> C replacement");
    assert_eq!(ids_of(&replacement), vec![h.layout_portal]);
    assert_eq!(h.engine.control_established(), Some(B));
    (h, replacement)
}

fn local_override_and_end_capture(h: &mut H) -> u64 {
    h.advance(1);
    let overridden_at = h.now_ms();
    let out = h.feed(Input::Link(LinkEvent::Input {
        peer: B,
        msg: InputMessage::Status {
            session: h.session.unwrap(),
            status: TargetStatus::LocalOverride,
        },
    }));
    assert!(has_notice(&out, &Notice::LocalOverride(B)), "{out:?}");
    assert!(has_end_capture(&out), "{out:?}");
    assert_eq!(h.engine.controlling(), None);
    h.advance(1);
    h.ended(h.capture.unwrap(), CaptureEnd::Requested);
    overridden_at
}

#[test]
fn local_override_guard_survives_rejected_replacement_and_display_refresh() {
    let (mut h, replacement) = local_override_with_pending_replacement();
    h.portals_set(ids_of(&replacement), Err(PortalsFailure::Rejected));
    let overridden_at = local_override_and_end_capture(&mut h);
    // Refreshing unchanged display data still calls update_portals. Its offered layout has
    // A -> C, while the backend's confirmed A -> B connection remains installed.
    h.advance(1);
    h.feed(Input::PeerDisplays {
        peer: B,
        displays: vec![display(1)],
    });
    for at in [overridden_at + 4, overridden_at + REENTRY_GUARD - 1] {
        let out = h.at(
            at,
            Input::Capture(CaptureEvent::EdgePressed {
                portal: h.layout_portal,
                position: 0.5,
                at: ms(at),
            }),
        );
        assert!(
            !has_hud_show(&out) && start_control_to(&out).is_none(),
            "{out:?}"
        );
        assert_eq!(h.engine.controlling(), None);
    }
    let at = overridden_at + REENTRY_GUARD;
    let out = h.at(
        at,
        Input::Capture(CaptureEvent::EdgePressed {
            portal: h.layout_portal,
            position: 0.5,
            at: ms(at),
        }),
    );
    assert!(has_hud_show(&out), "{out:?}");
    let out = h.visible();
    assert_eq!(
        start_control_to(&out),
        Some(B),
        "the rejected set did not replace B: {out:?}"
    );
    h.feed(Input::Command(Command::ReleaseControl));
    h.quiet();
}

#[test]
fn local_override_guard_covers_pending_replacement_success_without_extending_deadline() {
    let (mut h, replacement) = local_override_with_pending_replacement();
    let overridden_at = local_override_and_end_capture(&mut h);
    h.advance(1);
    h.portals_set(ids_of(&replacement), Ok(()));
    // The formerly installed id now leads to C: a connection that did not exist at override.
    for at in [overridden_at + 4, overridden_at + REENTRY_GUARD - 1] {
        let out = h.at(
            at,
            Input::Capture(CaptureEvent::EdgePressed {
                portal: h.layout_portal,
                position: 0.5,
                at: ms(at),
            }),
        );
        assert!(
            !has_hud_show(&out) && start_control_to(&out).is_none(),
            "{out:?}"
        );
        assert_eq!(h.engine.controlling(), None);
    }
    let at = overridden_at + REENTRY_GUARD;
    let out = h.at(
        at,
        Input::Capture(CaptureEvent::EdgePressed {
            portal: h.layout_portal,
            position: 0.5,
            at: ms(at),
        }),
    );
    assert!(has_hud_show(&out), "{out:?}");
    let out = h.visible();
    assert_eq!(
        start_control_to(&out),
        Some(C),
        "the confirmed replacement leads to C: {out:?}"
    );
    h.feed(Input::Command(Command::ReleaseControl));
    h.quiet();
}

#[test]
fn rejected_replacement_keeps_the_installed_ids() {
    // C comes up; the new layout puts C on A's left, which renumbers the portals: B's strip (the
    // one the backend has installed, id 1 on the right edge) would become id 2 and id 1 would
    // name C's. The backend does not take the replacement.
    for answer in [None, Some(Err(PortalsFailure::Rejected))] {
        let mut h = H::bare();
        h.feed(Input::PeerDisplays {
            peer: C,
            displays: vec![display(1)],
        });
        h.feed(Input::PeerUp { peer: C });
        h.auto_portals = false;
        let out = h.feed(Input::Layout(vec![
            Placement {
                node: A,
                display: LOCAL,
                origin: PointMm::new(0.0, 0.0),
                version: 2,
            },
            Placement {
                node: B,
                display: REMOTE,
                origin: PointMm::new(100.0, 0.0),
                version: 2,
            },
            Placement {
                node: C,
                display: LOCAL,
                origin: PointMm::new(-100.0, 0.0),
                version: 2,
            },
        ]));
        let replacement = set_portals(&out).pop().expect("a replacement set");
        assert!(
            replacement
                .iter()
                .any(|p| p.id == PortalId(1) && p.edge == Edge::Left),
            "id 1 now names the strip to C: {replacement:?}"
        );
        if let Some(result) = answer {
            h.portals_set(ids_of(&replacement), result);
        }
        // The old strip (id 1, still installed) is pressed: it leads to B.
        let out = h.feed(Input::Capture(CaptureEvent::EdgePressed {
            portal: PortalId(1),
            position: 0.5,
            at: h.now,
        }));
        assert!(has_hud_show(&out), "answered: {answer:?}: {out:?}");
        let out = h.feed(Input::Overlay(OverlayEvent::Visible(HUD)));
        assert_eq!(
            start_control_to(&out),
            Some(B),
            "answered: {answer:?}: {out:?}"
        );
    }

    // The same replacement, confirmed: id 1 is C's.
    let mut h = H::bare();
    h.feed(Input::PeerDisplays {
        peer: C,
        displays: vec![display(1)],
    });
    h.feed(Input::PeerUp { peer: C });
    h.add_c_left();
    h.feed(Input::Capture(CaptureEvent::EdgePressed {
        portal: PortalId(1),
        position: 0.5,
        at: h.now,
    }));
    let out = h.feed(Input::Overlay(OverlayEvent::Visible(HUD)));
    assert_eq!(start_control_to(&out), Some(C), "{out:?}");

    // A timeout leaves unknown what is installed: nothing is honoured.
    let mut h = H::bare();
    h.feed(Input::PeerDisplays {
        peer: C,
        displays: vec![display(1)],
    });
    h.feed(Input::PeerUp { peer: C });
    h.auto_portals = false;
    h.add_c_left();
    let replacement = set_portals(&h.log).pop().unwrap();
    h.portals_set(ids_of(&replacement), Err(PortalsFailure::Uncertain));
    let out = h.feed(Input::Capture(CaptureEvent::EdgePressed {
        portal: PortalId(1),
        position: 0.5,
        at: h.now,
    }));
    assert!(!has_hud_show(&out), "{out:?}");
}

fn placement_at(node: NodeId, x_mm: f64) -> Placement {
    Placement {
        node,
        display: LOCAL,
        origin: PointMm::new(x_mm, 0.0),
        version: 2,
    }
}

/// Press portal 1 (the strip on A's right edge) and see which peer the session is started with.
fn press_right_strip(h: &mut H) -> Option<NodeId> {
    h.feed(Input::Capture(CaptureEvent::EdgePressed {
        portal: PortalId(1),
        position: 0.5,
        at: h.now,
    }));
    let out = h.feed(Input::Overlay(OverlayEvent::Visible(HUD)));
    start_control_to(&out)
}

/// Three layouts, none answered yet: A alone (an empty set), then B on A's right (the set S),
/// then C on A's right with B behind it: the same strips as S (the same ids), leading to C.
fn three_outstanding_sets() -> H {
    let mut h = H::bare();
    h.feed(Input::PeerDisplays {
        peer: C,
        displays: vec![display(1)],
    });
    h.feed(Input::PeerUp { peer: C });
    h.auto_portals = false;
    let first = set_portals(&h.feed(Input::Layout(vec![placement_at(A, 0.0)])));
    assert_eq!(first, vec![vec![]]);
    let second = set_portals(&h.feed(Input::Layout(vec![
        placement_at(A, 0.0),
        placement_at(B, 100.0),
    ])));
    assert_eq!(second.len(), 1);
    // The strips are identical but they lead somewhere else: a request of its own, so no answer
    // to an earlier one can stand for it.
    let third = set_portals(&h.feed(Input::Layout(vec![
        placement_at(A, 0.0),
        placement_at(C, 100.0),
        placement_at(B, 200.0),
    ])));
    assert_eq!(third, second, "the identical set is offered again");
    h
}

#[test]
fn identical_set_with_a_new_meaning_is_offered_again_and_acknowledged_in_order() {
    // Every answer consumes the oldest outstanding request: the last one confirms what the
    // strips lead to now (C), however many answers were pending when the layout changed.
    let mut h = three_outstanding_sets();
    h.portals_set(vec![], Ok(()));
    h.portals_set(vec![PortalId(1)], Ok(()));
    // Between the answers the installed set still means B: that is what was confirmed.
    h.portals_set(vec![PortalId(1)], Ok(()));
    assert_eq!(press_right_strip(&mut h), Some(C));

    // A succeeds and B is rejected (identical ids, different mappings): the strip that is
    // installed is A's, and it leads to A's peer.
    let mut h = three_outstanding_sets();
    h.portals_set(vec![], Ok(()));
    h.portals_set(vec![PortalId(1)], Ok(()));
    h.portals_set(vec![PortalId(1)], Err(PortalsFailure::Rejected));
    assert_eq!(press_right_strip(&mut h), Some(B));
}

// ---------------------------------------------------------------------------------------------
// Review round 3: the request queue's overflow keeps the answers aligned (finding 1), and the
// geometry behind identical strips is part of what a re-offer is decided on (finding 2).
// ---------------------------------------------------------------------------------------------

/// Requests the controller remembers (`PORTAL_REQUESTS_MAX`).
const REMEMBERED_REQUESTS: usize = 64;

/// The `k`th layout (counting from 1) of a run that alternates what A's one strip on the right
/// edge leads to: the odd ones put C there (with B behind it), the even ones put B there. The
/// strips, and so their ids, are identical every time; only what they lead to differs, so each
/// is a request of its own.
fn alternating_layout(k: usize) -> Input {
    Input::Layout(if k % 2 == 1 {
        vec![
            placement_at(A, 0.0),
            placement_at(C, 100.0),
            placement_at(B, 200.0),
        ]
    } else {
        vec![placement_at(A, 0.0), placement_at(B, 100.0)]
    })
}

/// The peer the strip leads to in the `k`th layout of the run.
fn alternating_target(k: usize) -> NodeId {
    if k % 2 == 1 { C } else { B }
}

/// Offer the layouts `ks` of the run, each a `SetPortals` of the same one strip, unanswered.
fn offer_alternating(h: &mut H, ks: std::ops::RangeInclusive<usize>) {
    for k in ks {
        let sets = set_portals(&h.feed(alternating_layout(k)));
        assert_eq!(sets.len(), 1, "layout {k} is offered once: {sets:?}");
        assert_eq!(ids_of(&sets[0]), vec![PortalId(1)], "layout {k}");
    }
}

/// `n` answers, each naming the strip every request of the run offers.
fn answer_strip(h: &mut H, n: usize, result: Result<(), PortalsFailure>) {
    for _ in 0..n {
        h.portals_set(vec![PortalId(1)], result);
    }
}

/// A node whose agent has stopped answering after the baseline (B on A's right, installed):
/// the layouts `1..=n` of the run are offered and none is answered.
fn alternating_run(n: usize) -> H {
    let mut h = H::bare();
    h.feed(Input::PeerDisplays {
        peer: C,
        displays: vec![display(1)],
    });
    h.feed(Input::PeerUp { peer: C });
    h.auto_portals = false;
    offer_alternating(&mut h, 1..=n);
    h
}

/// What the strip leads to once `answered` of the `offered` requests of the run have been
/// answered (in order, each `Ok`): nothing until an answer is aligned with a request that was
/// kept, then the meaning of the request it answers.
fn expected_after(offered: usize, answered: usize) -> Option<NodeId> {
    let dropped = offered.saturating_sub(REMEMBERED_REQUESTS);
    (answered > dropped).then(|| alternating_target(answered))
}

#[test]
fn overflowing_the_request_queue_keeps_answers_aligned_and_fails_closed() {
    // 69 sets are outstanding, one more than the queue holds five times over: the first five
    // requests are forgotten, and their answers (which come first) must not be read against the
    // sets that were kept. The ids are identical in every set, so only the order tells which
    // answer is whose; the meanings alternate, so an answer read against a neighbour confirms
    // the other peer. Every prefix of the answers is checked on a node of its own.
    const OFFERED: usize = 69;
    for answered in 0..=OFFERED {
        let mut h = alternating_run(OFFERED);
        answer_strip(&mut h, answered, Ok(()));
        assert_eq!(
            press_right_strip(&mut h),
            expected_after(OFFERED, answered),
            "after {answered} of {OFFERED} answers"
        );
    }
}

#[test]
fn answers_of_dropped_requests_confirm_nothing_whichever_way_they_went() {
    // The five answers owed for the forgotten requests are a success, a rejection or an uncertain
    // result: none of them confirms a mapping, so no strip can be pressed until the first answer
    // that is aligned with a kept request. A rejection of that one still leaves nothing
    // confirmed; the next success does.
    const OFFERED: usize = 69;
    let dropped = OFFERED - REMEMBERED_REQUESTS;
    // A press starts a crossing, so each probe is a node of its own: the dropped requests'
    // answers (all `result`), then the `aligned` ones, then the press.
    let press_after = |result, aligned: &[Result<(), PortalsFailure>]| {
        let mut h = alternating_run(OFFERED);
        answer_strip(&mut h, dropped, result);
        for answer in aligned {
            answer_strip(&mut h, 1, *answer);
        }
        press_right_strip(&mut h)
    };
    for result in [
        Ok(()),
        Err(PortalsFailure::Rejected),
        Err(PortalsFailure::Uncertain),
    ] {
        let tag = format!("dropped requests answered {result:?}");
        assert_eq!(press_after(result, &[]), None, "{tag}");
        assert_eq!(
            press_after(result, &[Ok(())]),
            Some(alternating_target(dropped + 1)),
            "{tag}: the first aligned answer confirms its own request"
        );
        let rejected = Err(PortalsFailure::Rejected);
        assert_eq!(press_after(result, &[rejected]), None, "{tag}");
        assert_eq!(
            press_after(result, &[rejected, Ok(())]),
            Some(alternating_target(dropped + 2)),
            "{tag}"
        );
    }
}

#[test]
fn overflow_while_dropped_answers_are_still_owed_counts_all_of_them() {
    // Three of the five owed answers have come when four more sets are offered: each drops one
    // more request, so nine answers in all are owed for forgotten requests, however the offers
    // and the answers interleave.
    const FIRST: usize = 69;
    const EARLY: usize = 3;
    const LATER: usize = 4;
    for late in 0..=(FIRST + LATER - EARLY) {
        let mut h = alternating_run(FIRST);
        answer_strip(&mut h, EARLY, Ok(()));
        offer_alternating(&mut h, FIRST + 1..=FIRST + LATER);
        answer_strip(&mut h, late, Ok(()));
        assert_eq!(
            press_right_strip(&mut h),
            expected_after(FIRST + LATER, EARLY + late),
            "{EARLY} answers, then {LATER} more offers, then {late} answers"
        );
    }
}

#[test]
fn answers_resume_in_order_after_an_overflow() {
    // Once every answer owed has come, the queue is aligned again: the next set is answered by
    // its own answer like any other, and a failure to answer it leaves the previous one in force.
    const OFFERED: usize = 69;
    let press_after = |result| {
        let mut h = alternating_run(OFFERED);
        answer_strip(&mut h, OFFERED, Ok(()));
        offer_alternating(&mut h, OFFERED + 1..=OFFERED + 1);
        answer_strip(&mut h, 1, result);
        press_right_strip(&mut h)
    };
    assert_eq!(press_after(Ok(())), Some(alternating_target(OFFERED + 1)));
    assert_eq!(
        press_after(Err(PortalsFailure::Rejected)),
        Some(alternating_target(OFFERED)),
        "the rejected set leaves the one before it installed"
    );
}

/// B's display (the one `display(1)` makes) with `pixels` by `pixels` pixels on the same 100 mm.
fn display_of_pixels(pixels: u32) -> DisplayInfo {
    let mut info = display(1);
    info.geometry.pixel_size = PixelSize::new(pixels, pixels);
    info
}

fn b_displays(pixels: u32) -> Input {
    Input::PeerDisplays {
        peer: B,
        displays: vec![display_of_pixels(pixels)],
    }
}

/// Press the strip on A's right edge at its midpoint and see where the session starts: the peer
/// and the point it enters at.
fn press_right_strip_entry(h: &mut H) -> Option<(NodeId, PointDevice)> {
    h.feed(Input::Capture(CaptureEvent::EdgePressed {
        portal: PortalId(1),
        position: 0.5,
        at: h.now,
    }));
    h.feed(Input::Overlay(OverlayEvent::Visible(HUD)))
        .iter()
        .find_map(|o| match o {
            Output::SendControl {
                peer,
                msg: ControlMessage::StartControl { entry, .. },
            } => Some((*peer, *entry)),
            _ => None,
        })
}

fn assert_entry(entry: Option<(NodeId, PointDevice)>, peer: NodeId, y: f64, tag: &str) {
    let (to, at) = entry.unwrap_or_else(|| panic!("{tag}: no crossing"));
    assert_eq!(to, peer, "{tag}");
    assert!(
        (at.y - y).abs() <= 1.0,
        "{tag}: entered at y = {}, expected {y}",
        at.y
    );
}

#[test]
fn a_geometry_change_behind_identical_strips_is_offered_again() {
    // B's display gets more pixels on the same physical size. A's strip (in A's own pixels) and
    // the connection its id names are unchanged, but the midpoint now maps to another pixel of
    // B: the set is offered again, and once that is answered the crossing enters there.
    // With the replacement installed (nothing outstanding when the geometry changes).
    let mut h = H::bare();
    h.auto_portals = false;
    let baseline = set_portals(&h.log).pop().expect("the baseline set");
    let out = h.feed(b_displays(2_000));
    assert_eq!(set_portals(&out), vec![baseline.clone()], "{out:?}");
    h.portals_set(ids_of(&baseline), Ok(()));
    assert_entry(
        press_right_strip_entry(&mut h),
        B,
        1_000.0,
        "confirmed snapshot",
    );

    // With an earlier set still unanswered when the geometry changes (twice).
    let mut h = H::bare();
    h.auto_portals = false;
    let out = h.feed(b_displays(2_000));
    assert_eq!(set_portals(&out), vec![baseline.clone()], "{out:?}");
    let out = h.feed(b_displays(4_000));
    assert_eq!(set_portals(&out), vec![baseline.clone()], "{out:?}");
    h.portals_set(ids_of(&baseline), Ok(()));
    h.portals_set(ids_of(&baseline), Ok(()));
    assert_entry(
        press_right_strip_entry(&mut h),
        B,
        2_000.0,
        "pending snapshot",
    );

    // The same geometry again is not a change: nothing is offered.
    let mut h = H::bare();
    h.auto_portals = false;
    let out = h.feed(b_displays(1_000));
    assert!(set_portals(&out).is_empty(), "{out:?}");
    let out = h.feed(b_displays(2_000));
    assert_eq!(set_portals(&out).len(), 1, "{out:?}");
    let out = h.feed(b_displays(2_000));
    assert!(set_portals(&out).is_empty(), "{out:?}");
}

#[test]
fn restored_portals_are_confirmed_by_their_own_answer() {
    // An incoming session removes this node's portals and its end restores them: both sets are
    // really emitted and answered, in order. A set the controller wanted to emit meanwhile was
    // suppressed (this node was controlled): it has no request, so it can't throw the answers
    // out of step. Afterwards an ordinary crossing works again.
    let mut h = H::bare();
    h.feed(Input::Grants(
        [
            (
                B,
                [Capability::WindowShare, Capability::WindowPresent].into(),
            ),
            (
                C,
                [
                    Capability::WindowShare,
                    Capability::WindowPresent,
                    Capability::InputAccept,
                ]
                .into(),
            ),
        ]
        .into(),
    ));
    h.feed(Input::PeerDisplays {
        peer: C,
        displays: vec![display(1)],
    });
    h.feed(Input::PeerUp { peer: C });
    let started = h.feed(control(
        C,
        ControlMessage::StartControl {
            session: SessionId(40),
            entry_display: LOCAL,
            entry: point(1.0, 1.0),
            lock_keys: LockKeys::default(),
        },
    ));
    assert_eq!(set_portals(&started), vec![vec![]], "{started:?}");
    // While controlled, a layout change makes the controller want another set: suppressed.
    h.advance(10);
    let out = h.feed(Input::Layout(vec![
        placement_at(A, 0.0),
        placement_at(B, 100.0),
        placement_at(C, -100.0),
    ]));
    assert!(set_portals(&out).is_empty(), "suppressed: {out:?}");
    // The session ends: the portals are restored.
    h.advance(10);
    let out = h.feed(control(
        C,
        ControlMessage::EndControl {
            session: SessionId(40),
            reason: EndReason::Released,
        },
    ));
    let restored = set_portals(&out);
    assert_eq!(restored.len(), 1, "{out:?}");
    assert!(restored[0].len() >= 2);
    // The restored strips arm again after the fallback; then a press on B's strip (right edge)
    // starts control of B: the restoration's own answer confirmed its mapping.
    h.advance(1_500);
    let right = restored[0]
        .iter()
        .find(|p| p.edge == Edge::Right)
        .expect("the strip toward B")
        .id;
    let out = h.feed(Input::Capture(CaptureEvent::EdgePressed {
        portal: right,
        position: 0.5,
        at: h.now,
    }));
    assert!(has_hud_show(&out), "{out:?}");
    let out = h.feed(Input::Overlay(OverlayEvent::Visible(HUD)));
    assert_eq!(start_control_to(&out), Some(B), "{out:?}");
}

#[test]
fn retried_portals_are_confirmed_by_their_own_answer() {
    // A rejected set is offered again by the timer: the retry is a real emission with a request
    // of its own, so its answer confirms it (the exits are installed, and a press on one works).
    let (mut h, _, replacement) = H::projected().with_outstanding_replacement();
    h.advance(1);
    h.portals_set(replacement.clone(), Err(PortalsFailure::Rejected));
    let t = h.now_ms();
    let out = h.tick(t + PORTALS_RETRY);
    assert_eq!(set_portals(&out).len(), 1, "{out:?}");
    // A second retry is answered in order too: the first retry's answer fails, the next one wins.
    h.portals_set(replacement.clone(), Err(PortalsFailure::Rejected));
    let t = h.now_ms();
    let out = h.tick(t + PORTALS_RETRY);
    assert_eq!(set_portals(&out).len(), 1, "{out:?}");
    h.portals_set(replacement, Ok(()));
    // The exits are installed: enter, and press an exit while home.
    h.advance(1);
    h.motion(700.0, -100.0);
    let out = h.report(P1, point(100.0, 100.0));
    assert!(bind(&out, true).is_some(), "{out:?}");
    let op = bind(&out, true).unwrap();
    h.advance(1);
    h.bind_set(op, true, true);
    h.advance(1);
    h.released(op, Ok(Warp::Done));
    h.advance(1);
    h.focus(Some(W1));
    h.advance(1);
    let out = h.press(Edge::Left, 0.5);
    assert!(has_hud_show(&out), "{out:?}");
    h.feed(Input::Overlay(OverlayEvent::Unavailable(HUD)));
    h.quiet();
}

// ---------------------------------------------------------------------------------------------
// Review round 2: settlement is proved by what it allows, not by silence (finding 8).
// ---------------------------------------------------------------------------------------------

/// The id of the one release of `KEY` in `out`.
fn only_up_of_key(out: &[Output]) -> InjectId {
    let ups: Vec<_> = injects(out)
        .into_iter()
        .filter(|(_, cmd)| is_up_of(cmd, KEY))
        .collect();
    assert_eq!(ups.len(), 1, "one release of the key: {out:?}");
    ups[0].0
}

/// `KEY`'s release was asked for as `release`, fails, and is asked for twice more, 50 ms apart,
/// without an answer to the second: three requests of one release in flight together.
fn overlapping_releases(h: &mut H, release: &[Output]) -> [InjectId; 3] {
    let first = only_up_of_key(release);
    h.feed(Input::InjectDone {
        id: first,
        ok: false,
    });
    let second = only_up_of_key(&h.tick_after(50));
    let third = only_up_of_key(&h.tick_after(50));
    assert!(first != second && second != third && first != third);
    [first, second, third]
}

#[test]
fn overlapping_release_attempts_all_acknowledged_leave_nothing_owed() {
    // After every request of a release has been answered, in whichever order and with whichever
    // result (the failed first request, and the other two: ok, or failed with the other one ok),
    // nothing is owed: no pending release, no retry, no held item. Silence on the ticks cannot
    // show that (a stale pending request injects nothing); the entry that needs the drain can.
    // The last two cases are an injector that never answers one of them (a stale pending request
    // that nothing will ever clear but the ledger's own cleanup).
    let cases: [&[(usize, bool)]; 6] = [
        &[(2, true), (1, true)],
        &[(1, true), (2, true)],
        &[(1, false), (2, true)],
        &[(2, false), (1, true)],
        &[(2, true)],
        &[(1, true)],
    ];
    for answers in cases {
        let mut h = H::controlling();
        h.focus(Some(W1));
        let out = h.proj_key(P1, KEY, true);
        h.confirm(&out, true);
        h.aim();
        let release = h.proj_key(P1, KEY, false);
        let ids = overlapping_releases(&mut h, &release);
        for &(index, ok) in answers {
            h.feed(Input::InjectDone { id: ids[index], ok });
        }
        h.advance(300);
        // Fresh motion, a fresh report: the drain has nothing to wait for, so the bind is out in
        // the very handle of the trigger.
        h.motion(1.0, 0.0);
        let op = h.home_now();
        assert!(
            !has_inject(&h.tick_after(100)),
            "{answers:?}: nothing is retried after the acknowledgements"
        );
        let _ = op;
        h.quiet();
    }
}

#[test]
fn overlapping_release_attempts_during_the_drain_enter_when_one_is_confirmed() {
    // The entry's own drain is the release: it fails, is retried twice without an answer, and the
    // entry starts in the handle of the first confirmation, whichever request that answers. The
    // others answer late and change nothing.
    // (The first request has already been answered, by its failure; each id is answered once.)
    for (settling, late) in [(2usize, 1usize), (1, 2)] {
        let mut h = H::controlling();
        h.focus(Some(W1));
        let out = h.proj_key(P1, KEY, true);
        h.confirm(&out, true);
        h.aim();
        let trigger = h.trigger();
        assert!(binds(&trigger).is_empty(), "{trigger:?}");
        let first = only_up_of_key(&trigger);
        h.feed(Input::InjectDone {
            id: first,
            ok: false,
        });
        let second = only_up_of_key(&h.tick_after(50));
        let third = only_up_of_key(&h.tick_after(50));
        let ids = [first, second, third];
        // (The entry is still waiting: nothing but the retries has happened.)
        let out = h.feed(Input::InjectDone {
            id: ids[settling],
            ok: true,
        });
        let op = bind(&out, true).unwrap_or_else(|| panic!("{settling}: no bind: {out:?}"));
        let out = h.feed(Input::InjectDone {
            id: ids[late],
            ok: true,
        });
        assert!(!has_inject(&out), "late answer {late}: {out:?}");
        assert!(binds(&out).is_empty(), "late answer {late}: {out:?}");
        h.advance(1);
        let out = h.bind_set(op, true, true);
        assert!(warp(&out).is_some(), "{out:?}");
        h.advance(1);
        h.released(op, Ok(Warp::Done));
        h.advance(1);
        h.focus(Some(W1));
        h.quiet();
    }
}

#[test]
fn an_unanswered_request_of_a_release_holds_the_entry_back() {
    // The control for the tests above: with the third request unanswered, the entry is still
    // waiting, however long the earlier ones have been settled by failure.
    let mut h = H::controlling();
    h.focus(Some(W1));
    let out = h.proj_key(P1, KEY, true);
    h.confirm(&out, true);
    h.aim();
    let release = h.proj_key(P1, KEY, false);
    let ids = overlapping_releases(&mut h, &release);
    h.advance(10);
    h.motion(1.0, 0.0);
    let out = h.trigger();
    assert!(binds(&out).is_empty(), "{out:?}");
    h.feed(Input::InjectDone {
        id: ids[2],
        ok: true,
    });
    h.quiet();
}

// ---------------------------------------------------------------------------------------------
// Review round 1: B1 through the acknowledgement contract (finding 12).
// ---------------------------------------------------------------------------------------------

impl H {
    /// Controlling B with acknowledgements off: the strips set is answered with its exact ids
    /// (installed), then a placement change that takes one strip away puts a real replacement
    /// `SetPortals` outstanding. Returns the installed ids and the replacement's.
    fn with_outstanding_replacement(mut self) -> (H, Vec<PortalId>, Vec<PortalId>) {
        self.auto_portals = false;
        self.cross();
        let installed = ids_of(&set_portals(&self.log).pop().expect("the strips set"));
        self.portals_set(installed.clone(), Ok(()));
        self.advance(1);
        // The proxy now touches B's right edge: the right strip goes.
        let out = self.place(
            B,
            P1,
            2,
            Some(Proxy {
                origin: point(600.0, 300.0),
                ..Proxy::standard()
            }),
        );
        let replacement = ids_of(&set_portals(&out).pop().expect("the replacement set"));
        assert_ne!(installed, replacement);
        (self, installed, replacement)
    }
}

#[test]
fn rejected_replacement_keeps_the_capture_and_is_offered_again() {
    let (mut h, _, replacement) = H::projected().with_outstanding_replacement();
    h.advance(1);
    let out = h.portals_set(replacement.clone(), Err(PortalsFailure::Rejected));
    // The previous set and the capture are intact: nothing ends, input is still routed.
    assert!(out.is_empty(), "{out:?}");
    assert_eq!(h.engine.controlling(), Some(B));
    assert_eq!(motions(&h.motion(1.0, 0.0)).len(), 1);
    // The replacement is not installed: no entry on the strips it names.
    h.advance(1);
    h.motion(699.0, -100.0);
    let out = h.report(P1, point(100.0, 100.0));
    assert_no_entry(&h, &out);
    // It is offered again, and when the backend takes it the exits are installed.
    let t = h.now_ms();
    let out = h.tick(t + PORTALS_RETRY);
    let again = set_portals(&out);
    assert_eq!(again.len(), 1, "{out:?}");
    assert_eq!(ids_of(&again[0]), replacement);
    h.portals_set(replacement, Ok(()));
    h.advance(1);
    h.motion(1.0, 0.0);
    let out = h.report(P1, point(100.0, 100.0));
    assert!(bind(&out, true).is_some(), "{out:?}");
    h.quiet();
}

#[test]
fn uncertain_replacement_ends_the_capture_and_fences_its_end() {
    // The answer to a real replacement says only that it timed out.
    let (mut h, _, replacement) = H::projected().with_outstanding_replacement();
    let session = h.session.unwrap();
    let capture = h.capture.unwrap();
    h.advance(1);
    let out = h.portals_set(replacement, Err(PortalsFailure::Uncertain));
    assert!(has_end_capture(&out), "{out:?}");
    assert_eq!(end_controls(&out), vec![(B, session, EndReason::Released)]);
    assert_eq!(h.engine.controlling(), None);
    // The fence is CAPTURE_END_DEADLINE, not the short one.
    let t = h.now_ms();
    let out = h.tick(t + END_TIMEOUT + 10);
    assert!(!has_end_capture(&out) && !has_hud_hide(&out), "{out:?}");
    // The capture's end (whichever kind) completes it.
    h.advance(1);
    let out = h.ended(capture, CaptureEnd::Aborted);
    assert!(has_hud_hide(&out), "{out:?}");
    h.quiet();

    // `Lost` after the timeout: the existing end handling.
    let (mut h, _, replacement) = H::projected().with_outstanding_replacement();
    let capture = h.capture.unwrap();
    h.portals_set(replacement, Err(PortalsFailure::Uncertain));
    h.advance(1);
    let out = h.ended(capture, CaptureEnd::Lost);
    assert!(has_hud_hide(&out), "{out:?}");
    h.quiet();

    // No `Ended` before the deadline: ended once more, then treated as gone.
    let (mut h, _, replacement) = H::projected().with_outstanding_replacement();
    let out = h.portals_set(replacement, Err(PortalsFailure::Uncertain));
    assert!(has_end_capture(&out));
    let t = h.now_ms();
    let out = h.tick(t + CAPTURE_END_DEADLINE - 1);
    assert!(!has_hud_hide(&out), "{out:?}");
    let out = h.tick(t + CAPTURE_END_DEADLINE);
    assert!(has_end_capture(&out) && has_hud_hide(&out), "{out:?}");
    h.quiet();
}

#[test]
fn replacement_failure_while_home_leaves_home() {
    for failure in [PortalsFailure::Rejected, PortalsFailure::Uncertain] {
        let mut h = H::controlling();
        h.aim();
        h.home_now();
        let session = h.session.unwrap();
        // While home a placement change takes a strip away: a real replacement is outstanding.
        h.auto_portals = false;
        h.advance(1);
        let out = h.place(
            B,
            P1,
            2,
            Some(Proxy {
                origin: point(600.0, 300.0),
                ..Proxy::standard()
            }),
        );
        assert!(
            !left_home(&out)
                && !notices(&out)
                    .iter()
                    .any(|n| matches!(n, Notice::HomeFailed { .. }))
        );
        let replacement = ids_of(&set_portals(&out).pop().expect("the replacement"));
        h.advance(1);
        let out = h.portals_set(replacement, Err(failure));
        // No exit is left that the backend confirmed: leave home, whichever way it failed.
        assert!(home_failed(&out, HomeFailure::Gone), "{failure:?}: {out:?}");
        assert_eq!(end_controls(&out), vec![(B, session, EndReason::Released)]);
        assert_eq!(warps(&out)[0].1, FALLBACK);
        assert_eq!(h.engine.controlling(), None);
        h.confirm_removal(&out);
        h.quiet();
    }
}

// ---------------------------------------------------------------------------------------------
// Review round 1: the activation boundary in both orders (finding 13), and sequences (14).
// ---------------------------------------------------------------------------------------------

#[test]
fn ordinary_activation_keeps_held_keys_handling() {
    // B4: the agent answers an ordinary activation with `CaptureBegun` (the keys held when
    // `begin` started), and the events queued during `begin()` follow. A Control held then and
    // released during `begin()` is released: Shift+Alt+Esc afterwards is not the chord.
    let mut h = H::projected();
    h.cross_agent(
        vec![LCTRL],
        vec![CaptureEvent::Key {
            usage: LCTRL,
            down: false,
            at: h.now,
        }],
    );
    let mut all = Vec::new();
    for usage in [LSHIFT, LALT, ESC] {
        h.advance(1);
        all.extend(h.feed(Input::Capture(CaptureEvent::Key {
            usage,
            down: true,
            at: h.now,
        })));
    }
    assert!(
        end_controls(&all).is_empty(),
        "Control was released: {all:?}"
    );
    assert_eq!(sent_transitions(&all).len(), 3, "{all:?}");
    for usage in [LSHIFT, LALT, ESC] {
        h.feed(Input::Capture(CaptureEvent::Key {
            usage,
            down: false,
            at: h.now,
        }));
    }
    h.quiet();

    // Still held: the same keys complete the chord (the snapshot counts, as today).
    let mut h = H::projected();
    h.cross_agent(vec![LCTRL], vec![]);
    let session = h.session.unwrap();
    let mut all = Vec::new();
    for usage in [LSHIFT, LALT, ESC] {
        h.advance(1);
        all.extend(h.feed(Input::Capture(CaptureEvent::Key {
            usage,
            down: true,
            at: h.now,
        })));
    }
    assert_eq!(end_controls(&all), vec![(B, session, EndReason::Released)]);
    h.quiet();
}

#[test]
fn exit_activation_snapshot_modifier_in_both_callback_orders() {
    // A home exit (`drain_first`): the agent feeds the events queued during `begin()` first, then
    // `CaptureBegun` with the keys held when `begin` started. A modifier in that stale snapshot
    // that was released (or one that was pressed) during `begin()` must come out right in both
    // orders, including one that violates the agent's ordering.
    for agent_order in [true, false] {
        // Control held at the start, released during `begin()`: not the chord afterwards.
        let tag = format!("agent order: {agent_order}, release");
        let mut h = H::home();
        let (id, _) = h.exit_to_activating(Edge::Right, 0.25);
        let up = Input::Capture(CaptureEvent::Key {
            usage: LCTRL,
            down: false,
            at: h.now,
        });
        h.advance(1);
        let mut out = Vec::new();
        if agent_order {
            h.started(id);
            h.advance(1);
            h.feed(up);
            h.advance(1);
            out.extend(h.capture_begun(id, vec![LCTRL]));
        } else {
            out.extend(h.capture_begun(id, vec![LCTRL]));
            h.advance(1);
            h.started(id);
            h.advance(1);
            h.feed(up);
        }
        assert!(left_home(&out), "{tag}: {out:?}");
        h.confirm_removal(&out);
        let mut all = Vec::new();
        for usage in [LSHIFT, LALT, ESC] {
            h.advance(1);
            all.extend(h.feed(Input::Capture(CaptureEvent::Key {
                usage,
                down: true,
                at: h.now,
            })));
        }
        assert!(end_controls(&all).is_empty(), "{tag}: {all:?}");
        for usage in [LSHIFT, LALT, ESC] {
            h.feed(Input::Capture(CaptureEvent::Key {
                usage,
                down: false,
                at: h.now,
            }));
        }
        h.quiet();

        // Alt pressed during `begin()` with Control and Shift in the snapshot: Esc is the chord.
        let tag = format!("agent order: {agent_order}, press");
        let mut h = H::home();
        let session = h.session.unwrap();
        let (id, _) = h.exit_to_activating(Edge::Right, 0.25);
        let alt = Input::Capture(CaptureEvent::Key {
            usage: LALT,
            down: true,
            at: h.now,
        });
        h.advance(1);
        let mut out = Vec::new();
        if agent_order {
            h.started(id);
            h.advance(1);
            h.feed(alt);
            h.advance(1);
            out.extend(h.capture_begun(id, vec![LCTRL, LSHIFT]));
        } else {
            out.extend(h.capture_begun(id, vec![LCTRL, LSHIFT]));
            h.advance(1);
            h.started(id);
            h.advance(1);
            h.feed(alt);
        }
        assert!(left_home(&out), "{tag}: {out:?}");
        h.confirm_removal(&out);
        h.advance(1);
        let out = h.feed(Input::Capture(CaptureEvent::Key {
            usage: ESC,
            down: true,
            at: h.now,
        }));
        assert_eq!(
            end_controls(&out),
            vec![(B, session, EndReason::Released)],
            "{tag}"
        );
        h.quiet();
    }
}

// ---------------------------------------------------------------------------------------------
// WP-2.43j: the live findings of 2026-10-02. The entry warp landed on the content's outermost
// pixels, which on the twin are under a strip (a strip is one logical pixel on the twin output's
// edge, and the content fills the output there): the warp itself pressed the strip, the exit
// began 10 ms after home and was refused (the pointer had already moved off), or completed and
// put the pointer back outside the proxy, where the same motion entered again: a loop of
// "home"/"returned" every few hundred milliseconds. Presses queued before a home warp were
// delivered after its answer and started crossings from where the pointer no longer was.
// ---------------------------------------------------------------------------------------------

impl H {
    /// Controlling B, the pointer steered onto the standard proxy at `tracker` (B's device
    /// pixels, inside the proxy), the matching report, and the entry up to `ReleaseAndWarp`.
    /// Returns the entry's operation and the warp's target.
    fn enter_at(&mut self, tracker: PointDevice) -> (HomeOp, (DisplayId, PointDevice)) {
        let start = motions(&self.motion(0.0, 0.0))[0].position;
        self.motion(tracker.x - start.x, tracker.y - start.y);
        let proxy = Proxy::standard();
        let out = self.report(
            P1,
            point(tracker.x - proxy.origin.x, tracker.y - proxy.origin.y),
        );
        let op = bind(&out, true).unwrap_or_else(|| panic!("no entry at {tracker:?}: {out:?}"));
        self.advance(1);
        let out = self.bind_set(op, true, true);
        let (released, target) = warp(&out).expect("ReleaseAndWarp");
        assert_eq!(released, op);
        (op, target)
    }

    /// `enter_at`, and home.
    fn home_at(tracker: PointDevice) -> (H, (DisplayId, PointDevice)) {
        let mut h = H::controlling();
        let (op, target) = h.enter_at(tracker);
        h.advance(1);
        h.commit_home(op);
        (h, target)
    }

    /// The entry's release is confirmed (and the window focused if it wasn't).
    fn commit_home(&mut self, op: HomeOp) {
        let out = self.released(op, Ok(Warp::Done));
        let entered = Notice::Home {
            key: key(P1),
            entered: true,
        };
        if !has_notice(&out, &entered) {
            self.advance(1);
            let out = self.focus(Some(W1));
            assert!(has_notice(&out, &entered), "{out:?}");
        }
    }

    fn release_strip(&mut self, edge: Edge) -> Vec<Output> {
        let portal = self.strip(0, edge);
        self.feed(Input::Capture(CaptureEvent::EdgeReleased {
            portal,
            at: self.now,
        }))
    }
}

/// The standard proxy is 400x300 at (200, 300) on B; its content is at (50, 40) on the twin.
#[test]
fn an_entry_on_the_proxy_edge_warps_clear_of_the_strips() {
    for (tracker, want) in [
        // The left edge (content x = 0): 8 device pixels inside, not on the left strip.
        (point(200.0, 400.0), point(58.0, 140.0)),
        // The right edge (content x = 399).
        (point(599.0, 400.0), point(441.0, 140.0)),
        // A corner: clear of both strips.
        (point(200.0, 300.0), point(58.0, 48.0)),
        (point(599.0, 599.0), point(441.0, 331.0)),
        // Well inside: unchanged.
        (point(250.0, 400.0), point(100.0, 140.0)),
    ] {
        let (mut h, target) = H::home_at(tracker);
        assert_eq!(target, (TWIN, want), "entered at {tracker:?}");
        h.quiet();
    }
}

/// Live: the warp pressed the entry's own strip and the exit began at once (refused, or a
/// bounce straight back out).
#[test]
fn the_entering_motion_pressing_its_own_strip_does_not_exit_until_it_leaves_the_strip() {
    let (mut h, _) = H::home_at(point(200.0, 400.0));
    // The hand keeps pushing over the left strip: presses repeat while it does.
    assert!(!has_hud_show(&h.press(Edge::Left, 0.3)));
    h.advance(150);
    assert!(!has_hud_show(&h.press(Edge::Left, 0.3)));
    // Past the guard: a strip pressed during it still needs the pointer to leave it first.
    h.advance(250);
    assert!(!has_hud_show(&h.press(Edge::Left, 0.3)));
    assert_eq!(h.engine.controlling(), Some(B));
    // Leaving the strip re-arms it: the next push exits.
    h.release_strip(Edge::Left);
    h.advance(10);
    let out = h.press(Edge::Left, 0.3);
    assert!(
        has_hud_show(&out),
        "a push after leaving the strip exits: {out:?}"
    );
    h.feed(Input::Overlay(OverlayEvent::Unavailable(HUD)));
    h.quiet();
}

/// A push kept up on the entry's strip is meant after a while, released or not.
#[test]
fn a_push_held_on_the_entry_strip_exits_after_the_hold() {
    let (mut h, _) = H::home_at(point(599.0, 400.0));
    let entered = h.now_ms();
    for t in [0, 100, 299, 500, 999] {
        h.now = ms(entered + t);
        assert!(!has_hud_show(&h.press(Edge::Right, 0.5)), "{t} ms");
    }
    h.now = ms(entered + 1_000);
    assert!(has_hud_show(&h.press(Edge::Right, 0.5)));
    h.feed(Input::Overlay(OverlayEvent::Unavailable(HUD)));
    h.quiet();
}

/// The entry's strip, untouched during the guard, exits on the first press after it: leaving the
/// way one came in later is ordinary.
#[test]
fn the_entry_strip_counts_on_the_first_press_after_the_guard() {
    let (mut h, _) = H::home_at(point(599.0, 400.0));
    h.advance(299);
    assert!(!has_hud_show(&h.press(Edge::Right, 0.5)));
    h.release_strip(Edge::Right);
    let (mut h, _) = H::home_at(point(599.0, 400.0));
    h.advance(300);
    let out = h.press(Edge::Right, 0.5);
    assert!(has_hud_show(&out), "{out:?}");
    h.feed(Input::Overlay(OverlayEvent::Unavailable(HUD)));
    h.quiet();
}

/// Only the entry's own strips are guarded: a fast pass straight through the window exits on the
/// far side at once.
#[test]
fn far_strips_exit_at_once_after_an_edge_entry() {
    let (mut h, _) = H::home_at(point(200.0, 400.0));
    let out = h.press(Edge::Right, 0.5);
    assert!(has_hud_show(&out), "{out:?}");
    h.feed(Input::Overlay(OverlayEvent::Unavailable(HUD)));
    h.quiet();
    // An entry well inside guards no strip.
    let (mut h, _) = H::home_at(point(400.0, 450.0));
    let out = h.press(Edge::Left, 0.5);
    assert!(has_hud_show(&out), "{out:?}");
    h.feed(Input::Overlay(OverlayEvent::Unavailable(HUD)));
    h.quiet();
}

/// Live: the loop. Each exit put the pointer just outside the proxy and the entering motion went
/// straight back in; with the warp clear of the strip the second entry stays home.
#[test]
fn a_reentry_right_after_an_exit_waits_for_the_fence_and_then_stays_home() {
    let mut h = H::home();
    let exit = h.exit_through(Edge::Right, 0.25);
    let exited = h.now_ms();
    h.confirm_removal(&exit);
    // Back inside within the fence: no entry.
    h.now = ms(exited + 20);
    h.motion(-2.0, 0.0);
    let out = h.report(P1, point(398.0, 75.0));
    assert_no_entry(&h, &out);
    // After the fence it enters, clear of the right strip, and the hand still moving in doesn't
    // bounce it out.
    h.now = ms(exited + REENTRY_GUARD + 1);
    h.motion(-1.0, 0.0);
    let out = h.report(P1, point(397.0, 75.0));
    let op = bind(&out, true).expect("entry after the fence");
    h.advance(1);
    let out = h.bind_set(op, true, true);
    assert_eq!(
        warp(&out).map(|(_, t)| t),
        Some((TWIN, point(441.0, 115.0)))
    );
    h.advance(1);
    h.commit_home(op);
    assert!(!has_hud_show(&h.press(Edge::Right, 0.25)));
    assert_eq!(h.engine.controlling(), Some(B));
    h.quiet();
}

/// Live: the agent answers a home warp, then delivers the edge events queued while it ran. A
/// press made before the warp is where the pointer no longer is.
#[test]
fn presses_made_before_a_home_warp_start_nothing() {
    // Home: a press stamped before the entry's answer is the old position's.
    let mut h = H::controlling();
    let (op, _) = h.enter_at(point(400.0, 450.0));
    h.advance(5);
    let answered = h.now_ms();
    h.commit_home(op);
    let stale = |h: &mut H, portal: PortalId, answered: u64| {
        h.feed(Input::Capture(CaptureEvent::EdgePressed {
            portal,
            position: 0.5,
            at: ms(answered - 3),
        }))
    };
    let portal = h.strip(0, Edge::Right);
    assert!(!has_hud_show(&stale(&mut h, portal, answered)));
    let out = h.press(Edge::Right, 0.5);
    assert!(has_hud_show(&out), "a press after the warp exits: {out:?}");
    h.feed(Input::Overlay(OverlayEvent::Unavailable(HUD)));
    h.quiet();

    // A failed entry ends the session and warps the pointer to this node's fallback: a press on
    // the layout's portal queued before that answer must not cross straight back.
    let mut h = H::controlling();
    let (op, _) = h.enter_at(point(400.0, 450.0));
    h.advance(5);
    let answered = h.now_ms();
    let out = h.released(op, Ok(Warp::Skipped));
    assert!(home_failed(&out, HomeFailure::Warp), "{out:?}");
    assert_eq!(h.engine.controlling(), None);
    let portal = h.layout_portal;
    assert!(
        !has_hud_show(&stale(&mut h, portal, answered)),
        "a stale press crossed back"
    );
    h.quiet();
}

/// Live: crossing from the controller straight into a fullscreen proxy entered home at the
/// crossing point, on the strip back to the controller: the entry exited at once.
#[test]
fn a_fullscreen_entry_at_the_crossing_point_lands_clear_of_the_return_strip() {
    let (mut h, geometry) = fullscreen_host_controlling_with(
        flush_layout(&[(B, 1, 0.0, 0.0), (A, 1, 100.0, 0.0)]).displays(),
        &[B, C],
    );
    // The crossing lands on B's right edge, inside the proxy (flush with it).
    let entry = motions(&h.motion(0.0, 0.0))[0].position;
    let width = f64::from(geometry.pixel_size.width);
    assert!(entry.x >= width - 1.0, "{entry:?}");
    let out = h.report(P1, entry);
    let op = bind(&out, true).expect("entry at the crossing point");
    h.advance(1);
    let out = h.bind_set(op, true, true);
    let (_, (display, at)) = warp(&out).expect("ReleaseAndWarp");
    assert_eq!(display, TWIN);
    assert_eq!(at.x, width - 1.0 - 8.0, "clear of the right strip");
    h.advance(1);
    h.commit_home(op);
    // The hand is still moving toward the controller: no bounce back.
    assert!(!has_hud_show(&h.press(Edge::Right, 0.5)));
    assert_eq!(h.engine.controlling(), Some(B));
    // Once it has left the strip, and past the guard, pushing out returns to the controller
    // (WP-2.43i).
    h.release_strip(Edge::Right);
    h.advance(300);
    let session = h.session.unwrap();
    let out = h.exit_through(Edge::Right, 0.5);
    assert_eq!(end_controls(&out), vec![(B, session, EndReason::Released)]);
    assert!(left_home(&out));
    let id = h.last_begin();
    let (warp_op, _) = warp(&out).expect("the return warps");
    h.confirm_removal(&out);
    h.released(warp_op, Ok(Warp::Done));
    h.ended(id, CaptureEnd::Requested);
    assert_eq!(h.engine.controlling(), None);
    h.quiet();
}

/// Live: the pointer touched a strip and moved away before the HUD was visible; the exit capture
/// still began, was refused (the pointer was no longer on the strip) and fenced the strip for a
/// second. Leaving the strip now cancels the exit quietly.
#[test]
fn leaving_the_strip_before_the_hud_is_visible_cancels_the_exit_without_a_fence() {
    let mut h = H::home();
    let out = h.press(Edge::Right, 0.5);
    assert!(has_hud_show(&out), "{out:?}");
    h.advance(5);
    let out = h.release_strip(Edge::Right);
    assert!(has_hud_hide(&out), "the HUD goes with the push: {out:?}");
    let hidden = h.now_ms();
    h.advance(5);
    let out = h.visible();
    assert!(
        begin_capture(&out).is_none(),
        "no capture after the push ended: {out:?}"
    );
    assert_eq!(h.engine.controlling(), Some(B));
    // The abandoned show's quarantine is the only wait (no retry fence on top): the first push
    // after it exits.
    h.now = ms(hidden + HUD_STALE);
    let out = h.press(Edge::Right, 0.5);
    assert!(has_hud_show(&out), "{out:?}");
    // Another strip's release changes nothing.
    let out = h.release_strip(Edge::Left);
    assert!(!has_hud_hide(&out), "{out:?}");
    h.advance(1);
    let (id, _, _) = begin_capture(&h.visible()).expect("the exit begins");
    h.advance(1);
    h.feed(Input::Capture(CaptureEvent::Started { id }));
    h.advance(1);
    let out = h.capture_begun(id, vec![]);
    assert!(left_home(&out) || !motions(&out).is_empty(), "{out:?}");
    h.quiet();
}

// ---- WP-2.43j review: stale HUD outcomes, pushes across warps, delayed entering presses ----

impl H {
    /// A press on the right strip while home shows the HUD; the pointer leaves the strip before
    /// the HUD is visible, so the exit is abandoned with its outcome still owed. Returns when the
    /// HUD was hidden: the quarantine runs `HUD_STALE` from there.
    fn abandon_exit_hud(&mut self) -> u64 {
        let out = self.press(Edge::Right, 0.5);
        assert!(has_hud_show(&out), "{out:?}");
        self.advance(5);
        let out = self.release_strip(Edge::Right);
        assert!(has_hud_hide(&out), "{out:?}");
        let hidden = self.now_ms();
        self.advance(5);
        hidden
    }

    /// An old outcome of the HUD arrives: it changes nothing.
    fn stale_outcome(&mut self, visible: bool) {
        let out = if visible {
            self.visible()
        } else {
            self.feed(Input::Overlay(OverlayEvent::Unavailable(HUD)))
        };
        assert!(
            begin_capture(&out).is_none() && !has_hud_hide(&out) && !left_home(&out),
            "a stale outcome changes nothing: {out:?}"
        );
        assert_eq!(self.engine.controlling(), Some(B));
    }

    /// The exit HUD of a new press becomes visible and its capture begins.
    fn exit_begins_after_new_show(&mut self) {
        let out = self.press(Edge::Right, 0.5);
        assert!(has_hud_show(&out), "a new push shows the HUD: {out:?}");
        self.advance(1);
        let out = self.visible();
        let (id, _, _) = begin_capture(&out).expect("the new show's Visible begins the exit");
        self.advance(1);
        self.feed(Input::Capture(CaptureEvent::Started { id }));
        self.advance(1);
        let out = self.capture_begun(id, vec![]);
        assert!(!motions(&out).is_empty(), "{out:?}");
    }
}

/// Every ordering of an abandoned show's outcomes around the next push: whichever comes, and
/// however many (the Mac host can report `Unavailable` and later `Visible` for one show), they
/// are dropped, no HUD is shown during the quarantine (it would be confused with them), and the
/// first push after it exits on its own HUD's `Visible`.
#[test]
fn stale_hud_outcomes_never_stand_for_a_newer_show() {
    // (outcomes before the re-press, outcomes after it), true = Visible.
    let orderings: [(&[bool], &[bool]); 6] = [
        (&[true], &[]),
        (&[], &[true]),
        (&[], &[false]),
        (&[false], &[true]),
        (&[false, true], &[]),
        (&[], &[false, true]),
    ];
    for (before, after) in orderings {
        let mut h = H::home();
        let hidden = h.abandon_exit_hud();
        for visible in before {
            h.stale_outcome(*visible);
            h.advance(5);
        }
        let out = h.press(Edge::Right, 0.5);
        assert!(
            !has_hud_show(&out),
            "{before:?}/{after:?}: no HUD during the quarantine: {out:?}"
        );
        h.advance(5);
        for visible in after {
            h.stale_outcome(*visible);
            h.advance(5);
        }
        // Still quarantined just before the end, whatever came.
        h.now = ms(hidden + HUD_STALE - 1);
        assert!(!has_hud_show(&h.press(Edge::Right, 0.5)));
        h.now = ms(hidden + HUD_STALE);
        h.exit_begins_after_new_show();
        h.quiet();
    }
}

/// A show that was not abandoned but reported `Unavailable` can still report `Visible` later (the
/// Mac host's presence expiring, then observed): that `Visible` must not stand for the next show,
/// on another strip that isn't fenced.
#[test]
fn a_late_visible_after_unavailable_never_stands_for_a_newer_show() {
    let mut h = H::home();
    assert!(has_hud_show(&h.press(Edge::Right, 0.5)));
    h.advance(5);
    let out = h.feed(Input::Overlay(OverlayEvent::Unavailable(HUD)));
    assert!(has_hud_hide(&out));
    let failed = h.now_ms();
    h.advance(5);
    assert!(!has_hud_show(&h.press(Edge::Left, 0.5)), "quarantined");
    h.advance(5);
    h.stale_outcome(true);
    h.now = ms(failed + HUD_STALE);
    let out = h.press(Edge::Left, 0.5);
    assert!(has_hud_show(&out), "{out:?}");
    h.advance(1);
    let (id, _, _) = begin_capture(&h.visible()).expect("its own Visible begins the exit");
    h.advance(1);
    h.feed(Input::Capture(CaptureEvent::Started { id }));
    h.advance(1);
    let out = h.capture_begun(id, vec![]);
    assert!(!motions(&out).is_empty(), "{out:?}");
    h.quiet();
}

/// An abandoned show whose outcome never comes quarantines the HUD for `HUD_STALE` from the hide.
#[test]
fn a_stale_hud_outcome_that_never_comes_blocks_for_a_second_at_most() {
    let mut h = H::home();
    let hidden = h.abandon_exit_hud();
    h.now = ms(hidden + HUD_STALE - 1);
    assert!(!has_hud_show(&h.press(Edge::Right, 0.5)));
    h.now = ms(hidden + HUD_STALE);
    h.exit_begins_after_new_show();
    h.quiet();
}

/// No session, a crossing dwell of 200 ms, and a stranded pointer whose next retry warp is due
/// at the returned time.
fn stranded_with_dwell() -> (H, u64) {
    let mut config = H::config();
    config.push_to_cross = Duration::from_millis(200);
    let mut h = H::bare_config(config, &[], &[]);
    h.project(W1, B, P1, TWIN, content1(), PlatformParking::Twin);
    h.place(B, P1, 1, Some(Proxy::standard()));
    // Cross with the dwell, then the usual handshake.
    h.feed(Input::Capture(CaptureEvent::EdgePressed {
        portal: h.layout_portal,
        position: 0.5,
        at: h.now,
    }));
    let start = h.now_ms();
    let out = h.tick(start + 200);
    assert!(has_hud_show(&out), "{out:?}");
    let out = h.visible();
    let session = out
        .iter()
        .find_map(|o| match o {
            Output::SendControl {
                msg: ControlMessage::StartControl { session, .. },
                ..
            } => Some(*session),
            _ => None,
        })
        .expect("StartControl");
    h.session = Some(session);
    let out = h.feed(control(B, ControlMessage::ControlStarted { session }));
    let (id, _, _) = begin_capture(&out).expect("BeginCapture");
    h.capture = Some(id);
    h.feed(Input::Capture(CaptureEvent::Started { id }));
    h.feed(Input::CaptureBegun {
        id,
        result: Ok(CaptureStart {
            held_keys: vec![],
            lock_keys: LockKeys::default(),
        }),
    });
    assert_eq!(h.engine.controlling(), Some(B));
    // A failed entry: the session ends, the pointer is stranded, its retry is due in a second.
    let (op, _) = h.enter_at(point(400.0, 450.0));
    h.advance(1);
    let out = h.released(op, Ok(Warp::Skipped));
    let (retry, _) = warp(&out).expect("an immediate retry");
    h.advance(1);
    h.released(retry, Ok(Warp::Skipped));
    let stranded = h.now_ms();
    h.advance(1);
    h.bind_set(h.last_removal(), false, true);
    assert_eq!(h.engine.controlling(), None);
    (h, stranded + STRANDED_RETRY)
}

impl H {
    fn press_portal(&mut self) -> Vec<Output> {
        self.feed(Input::Capture(CaptureEvent::EdgePressed {
            portal: self.layout_portal,
            position: 0.5,
            at: self.now,
        }))
    }
}

/// The live symptom behind the warp fence, for a crossing with a dwell: a push against the
/// layout's portal is pending when a stranded pointer's retry warp moves the pointer. The push
/// was made where the pointer no longer is: its dwell never starts a crossing. A push made while
/// the warp is unanswered completes its dwell (by the tick or by a repeated press) only after the
/// answer, which drops it.
#[test]
fn a_crossing_push_does_not_survive_a_warp() {
    let (mut h, due) = stranded_with_dwell();
    // A push 50 ms before the retry is due; the retry warps the pointer away at its deadline.
    h.now = ms(due - 50);
    assert!(!has_hud_show(&h.press_portal()));
    let out = h.tick(due);
    let (retry, _) = warp(&out).expect("the stranded retry");
    // The old push's deadline passes while the warp is unanswered: no crossing.
    let out = h.tick(due + 150);
    assert!(
        !has_hud_show(&out),
        "a push from before the warp crossed: {out:?}"
    );
    // A push just after the warp was issued: its dwell ends at +210, inside the warp's 300 ms
    // bound, and neither the tick nor a repeated press completes it while unanswered...
    h.now = ms(due + 10);
    assert!(!has_hud_show(&h.press_portal()));
    let out = h.tick(due + 210);
    assert!(
        !has_hud_show(&out),
        "the tick completed a dwell during the warp: {out:?}"
    );
    assert!(
        h.engine.next_deadline().is_some_and(|d| d > h.now),
        "the dwell waits for the warp's bound, without spinning: {:?}",
        h.engine.next_deadline()
    );
    h.now = ms(due + 250);
    assert!(
        !has_hud_show(&h.press_portal()),
        "a repeated press completed a dwell during the warp"
    );
    // ...and the answer drops it.
    h.now = ms(due + 260);
    h.released(retry, Ok(Warp::Done));
    let out = h.tick(due + 400);
    assert!(!has_hud_show(&out), "{out:?}");
    // A push after the answer crosses after its dwell.
    h.now = ms(due + 600);
    assert!(!has_hud_show(&h.press_portal()));
    let out = h.tick(due + 800);
    assert!(has_hud_show(&out), "{out:?}");
    h.feed(Input::Overlay(OverlayEvent::Unavailable(HUD)));
    h.quiet();
}

/// A warp whose answer never comes holds a crossing's dwell for its bound (`END_TIMEOUT`, 300
/// ms from the warp) and no longer: a push made during it crosses at the bound.
#[test]
fn an_unanswered_warp_holds_a_crossing_dwell_until_its_bound() {
    let (mut h, due) = stranded_with_dwell();
    let out = h.tick(due);
    warp(&out).expect("the stranded retry");
    h.now = ms(due + 10);
    assert!(!has_hud_show(&h.press_portal()));
    let out = h.tick(due + 210);
    assert!(!has_hud_show(&out), "{out:?}");
    assert_eq!(h.engine.next_deadline(), Some(ms(due + END_TIMEOUT)));
    let out = h.tick(due + END_TIMEOUT - 1);
    assert!(!has_hud_show(&out), "{out:?}");
    let out = h.tick(due + END_TIMEOUT);
    assert!(
        has_hud_show(&out),
        "the dwell completes at the bound: {out:?}"
    );
    h.feed(Input::Overlay(OverlayEvent::Unavailable(HUD)));
    h.quiet();
}

/// A press on the entry's own strip that happened during the guard but is delivered after it
/// (queue or IPC delay) is still the entering motion: classified by when it happened. The
/// absolute limit runs on the current time.
#[test]
fn a_delayed_entering_press_is_classified_by_when_it_happened() {
    let (mut h, _) = H::home_at(point(200.0, 400.0));
    let entered = h.now_ms();
    let delayed = |h: &mut H, happened: u64, delivered: u64| {
        h.now = ms(entered + delivered);
        let portal = h.strip(0, Edge::Left);
        h.feed(Input::Capture(CaptureEvent::EdgePressed {
            portal,
            position: 0.3,
            at: ms(entered + happened),
        }))
    };
    // Happened at 50 ms, delivered at 400 ms: guarded, and the strip is now held.
    assert!(!has_hud_show(&delayed(&mut h, 50, 400)));
    // The push goes on: still held, until it leaves the strip or the absolute second.
    assert!(!has_hud_show(&delayed(&mut h, 450, 450)));
    assert!(!has_hud_show(&delayed(&mut h, 999, 999)));
    let out = delayed(&mut h, 1_000, 1_000);
    assert!(has_hud_show(&out), "{out:?}");
    h.feed(Input::Overlay(OverlayEvent::Unavailable(HUD)));
    h.quiet();

    // Happened during the guard but delivered after the absolute second: the limit wins.
    let (mut h, _) = H::home_at(point(200.0, 400.0));
    let entered = h.now_ms();
    h.now = ms(entered + 1_000);
    let portal = h.strip(0, Edge::Left);
    let out = h.feed(Input::Capture(CaptureEvent::EdgePressed {
        portal,
        position: 0.3,
        at: ms(entered + 50),
    }));
    assert!(has_hud_show(&out), "{out:?}");
    h.feed(Input::Overlay(OverlayEvent::Unavailable(HUD)));
    h.quiet();
}
