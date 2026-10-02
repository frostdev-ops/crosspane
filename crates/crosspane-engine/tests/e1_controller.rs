#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Duration;

use crosspane_engine::e1::controller::{ControllerE1, REARM_FALLBACK};
use crosspane_engine::io::{HUD, TARGET_INDICATOR};
use crosspane_engine::{Command, EngineConfig, Failure, Input, Notice, Output};
use crosspane_input::Held;
use crosspane_input::layout::{Layout, Placed};
use crosspane_input::remap::RemapProfile;
use crosspane_platform::{
    CaptureEvent, CaptureId, CapturePortal, CaptureStart, EndReason as CaptureEnd, HotkeyEvent,
    LockState, MotionKind, OverlayAnchor, OverlayEvent, PortalId, Rgb8, SessionEvent, SessionState,
};
use crosspane_protocol::link::{LinkError, LinkEvent};
use crosspane_protocol::msg::{
    ControlMessage, EndReason, InputMessage, MAX_HELD_KEYS, Placement, PointerMessage, Refusal,
    TargetStatus,
};
use crosspane_types::color::ColorSpace;
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{
    DisplayGeometry, PixelSize, PointDevice, PointLogical, PointMm, SizeMm,
};
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::id::{DisplayId, GlobalDisplayId, NodeId, SessionId};
use crosspane_types::input::{LockKeys, ScrollDelta, ScrollPhase};
use crosspane_types::time::MonoTime;
use proptest::prelude::*;

const A: NodeId = NodeId([1; 32]);
const B: NodeId = NodeId([2; 32]);
const C: NodeId = NodeId([3; 32]);
const KEY: HidUsage = HidUsage::keyboard(4);
const OTHER_KEY: HidUsage = HidUsage::keyboard(5);
const ESC: HidUsage = HidUsage::keyboard(0x29);
const MODIFIERS: [HidUsage; 3] = [
    HidUsage::keyboard(0xe0),
    HidUsage::keyboard(0xe1),
    HidUsage::keyboard(0xe2),
];
const PERMITTED: SessionState = SessionState {
    lock: LockState::Unlocked,
    active: Some(true),
};

fn time(ms: u64) -> MonoTime {
    MonoTime::from_nanos(ms * 1_000_000)
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

fn config() -> EngineConfig {
    let mut config = EngineConfig::new(A);
    config.accel.base_mm_per_unit = 0.1;
    config.accel.max_gain = 1.0;
    config
}

fn key(usage: HidUsage, down: bool, at: MonoTime) -> Input {
    Input::Capture(CaptureEvent::Key { usage, down, at })
}

fn button(down: bool, at: MonoTime) -> Input {
    Input::Capture(CaptureEvent::Button {
        button: MouseButton::PRIMARY,
        down,
        at,
    })
}

fn motion(dx: f64, dy: f64, kind: MotionKind, at: MonoTime) -> Input {
    Input::Capture(CaptureEvent::Motion { dx, dy, kind, at })
}

fn control(peer: NodeId, msg: ControlMessage) -> Input {
    Input::Link(LinkEvent::Control { peer, msg })
}
fn discrete(peer: NodeId, msg: InputMessage) -> Input {
    Input::Link(LinkEvent::Input { peer, msg })
}

struct Fixture {
    engine: ControllerE1,
    layout: Layout,
    portal: PortalId,
    now: MonoTime,
}

impl Fixture {
    fn new(config: EngineConfig, nodes: usize) -> Self {
        let mut engine = ControllerE1::new(&config, time(0));
        let mut setup = Vec::new();
        let mut placements = Vec::new();
        let mut placed = Vec::new();
        for (index, peer) in [A, B, C].into_iter().take(nodes).enumerate() {
            let info = display(1);
            let origin = PointMm::new(index as f64 * 100.0, 0.0);
            placements.push(Placement {
                node: peer,
                display: info.id,
                origin,
                version: 1,
            });
            placed.push(Placed {
                id: GlobalDisplayId {
                    node: peer,
                    display: info.id,
                },
                geometry: info.geometry,
                origin,
            });
            let input = if peer == A {
                Input::LocalDisplays(vec![info])
            } else {
                Input::PeerDisplays {
                    peer,
                    displays: vec![info],
                }
            };
            engine.handle(&input, time(0), &mut setup);
        }
        let layout = Layout::new(placed, config.layout).unwrap();
        let portal = layout
            .portals()
            .iter()
            .find(|p| p.from.node == A && p.to.node == B)
            .unwrap()
            .id;
        engine.handle(&Input::Layout(placements), time(0), &mut setup);
        engine.handle(
            &Input::Session(SessionEvent::State(PERMITTED)),
            time(0),
            &mut setup,
        );
        Fixture {
            engine,
            layout,
            portal,
            now: time(0),
        }
    }

    fn at(&mut self, now: MonoTime, input: Input) -> Vec<Output> {
        self.now = now;
        let mut out = Vec::new();
        self.engine.handle(&input, now, &mut out);
        out
    }
    fn feed(&mut self, ms: u64, input: Input) -> Vec<Output> {
        self.at(time(ms), input)
    }
    fn send(&mut self, input: Input) -> Vec<Output> {
        self.at(self.now, input)
    }
    fn up(&mut self, peer: NodeId) -> Vec<Output> {
        self.send(Input::PeerUp { peer })
    }
    fn edge(&self, position: f64) -> Input {
        Input::Capture(CaptureEvent::EdgePressed {
            portal: self.portal,
            position,
            at: self.now,
        })
    }
    fn hud(&mut self) -> Vec<Output> {
        self.send(self.edge(0.5))
    }
    fn handshake(&mut self) -> SessionId {
        self.up(B);
        assert!(
            self.hud()
                .iter()
                .any(|o| matches!(o, Output::ShowOverlay { id, .. } if *id == HUD))
        );
        start(&self.send(Input::Overlay(OverlayEvent::Visible(HUD)))).1
    }
    fn begin(&mut self) -> (SessionId, CaptureId) {
        let session = self.handshake();
        let out = self.send(control(B, ControlMessage::ControlStarted { session }));
        let capture = out
            .iter()
            .find_map(|o| match o {
                Output::BeginCapture { id, .. } => Some(*id),
                _ => None,
            })
            .unwrap();
        (session, capture)
    }
    fn controlling(&mut self, held_keys: Vec<HidUsage>) -> (SessionId, CaptureId) {
        let (session, capture) = self.begin();
        assert!(
            self.send(Input::Capture(CaptureEvent::Started { id: capture }))
                .is_empty()
        );
        assert!(
            self.send(Input::CaptureBegun {
                id: capture,
                result: Ok(CaptureStart {
                    held_keys,
                    lock_keys: LockKeys::default()
                })
            })
            .is_empty()
        );
        (session, capture)
    }
    fn raw(&mut self, ms: u64, dx: f64) -> Vec<Output> {
        self.feed(ms, motion(dx, 0.0, MotionKind::Unaccelerated, time(ms)))
    }
    fn ended(&mut self, ms: u64, capture: CaptureId) -> Vec<Output> {
        self.feed(
            ms,
            Input::Capture(CaptureEvent::Ended {
                id: capture,
                reason: CaptureEnd::Requested,
            }),
        )
    }
}

fn start(out: &[Output]) -> (NodeId, SessionId, DisplayId, PointDevice, LockKeys) {
    out.iter()
        .find_map(|o| match o {
            Output::SendControl {
                peer,
                msg:
                    ControlMessage::StartControl {
                        session,
                        entry_display,
                        entry,
                        lock_keys,
                    },
            } => Some((*peer, *session, *entry_display, *entry, *lock_keys)),
            _ => None,
        })
        .expect("StartControl")
}

fn motions(out: &[Output]) -> Vec<(NodeId, PointerMessage)> {
    out.iter()
        .filter_map(|o| match o {
            Output::SendMotion { peer, msg } => Some((*peer, *msg)),
            _ => None,
        })
        .collect()
}

fn transitions(out: &[Output]) -> Vec<(NodeId, SessionId, u32, Held, bool)> {
    out.iter()
        .filter_map(|o| match o {
            Output::SendInput {
                peer,
                msg:
                    InputMessage::Key {
                        session,
                        seq,
                        usage,
                        down,
                    },
            } => Some((*peer, *session, *seq, Held::Key(*usage), *down)),
            Output::SendInput {
                peer,
                msg:
                    InputMessage::Button {
                        session,
                        seq,
                        button,
                        down,
                    },
            } => Some((*peer, *session, *seq, Held::Button(*button), *down)),
            _ => None,
        })
        .collect()
}

fn heartbeat(out: &[Output]) -> (SessionId, u32, Vec<HidUsage>, Vec<MouseButton>) {
    out.iter()
        .find_map(|o| match o {
            Output::SendInput {
                msg:
                    InputMessage::State {
                        session,
                        seq,
                        held_keys,
                        held_buttons,
                    },
                ..
            } => Some((*session, *seq, held_keys.clone(), held_buttons.clone())),
            _ => None,
        })
        .expect("heartbeat")
}

fn assert_end(out: &[Output], peer: NodeId, session: SessionId, reason: EndReason) {
    assert!(out.contains(&Output::SendControl {
        peer,
        msg: ControlMessage::EndControl { session, reason }
    }));
}

fn assert_returns(out: &[Output]) {
    assert!(out.contains(&Output::EndCapture { warp_to: None }));
    assert!(!out.contains(&Output::HideOverlay(HUD)));
}

#[test]
fn portals_require_up_peers_and_emit_only_changes() {
    let mut f = Fixture::new(config(), 3);
    assert!(f.hud().is_empty());
    assert_eq!(f.up(C), vec![]); // C has no edge shared with A.
    let expected = f.layout.capture_portals(A);
    assert_eq!(f.up(B), vec![Output::SetPortals(expected.clone())]);
    assert!(f.up(B).is_empty());
    let placements = f
        .layout
        .displays()
        .iter()
        .map(|d| Placement {
            node: d.id.node,
            display: d.id.display,
            origin: d.origin,
            version: 2,
        })
        .collect();
    assert!(f.send(Input::Layout(placements)).is_empty());
    assert_eq!(
        f.send(Input::Command(Command::Panic)),
        vec![Output::SetPortals(vec![]), Output::Notice(Notice::Panic)]
    );
    assert!(f.hud().is_empty());
    assert_eq!(
        f.send(Input::Command(Command::Rearm)),
        vec![Output::SetPortals(expected)]
    );
    assert!(f.send(Input::Command(Command::Rearm)).is_empty());
    let out = f.send(Input::Link(LinkEvent::Closed {
        peer: B,
        error: LinkError::Closed,
    }));
    assert_eq!(out, vec![Output::SetPortals(vec![])]);

    // A local display with two remote neighbours must advertise only the up neighbour.
    let mut f = Fixture::new(config(), 3);
    let mut placed = f.layout.displays().to_vec();
    placed.iter_mut().find(|d| d.id.node == C).unwrap().origin.x = -100.0;
    let layout = Layout::new(placed, config().layout).unwrap();
    let placements = layout
        .displays()
        .iter()
        .map(|d| Placement {
            node: d.id.node,
            display: d.id.display,
            origin: d.origin,
            version: 2,
        })
        .collect();
    assert!(f.send(Input::Layout(placements)).is_empty());
    let portals = layout.capture_portals(A);
    assert_eq!(portals.len(), 2);
    let b_portal = layout
        .portals()
        .iter()
        .find(|p| p.from.node == A && p.to.node == B)
        .unwrap()
        .id;
    let only_b = portals
        .iter()
        .copied()
        .filter(|p| p.id == b_portal)
        .collect();
    assert_eq!(f.up(B), vec![Output::SetPortals(only_b)]);
    assert_eq!(f.up(C), vec![Output::SetPortals(portals)]);
}

#[test]
fn display_snapshots_rebuild_portals_only_where_placements_exist() {
    let mut f = Fixture::new(config(), 2);
    f.up(B);
    assert_eq!(
        f.send(Input::PeerDisplays {
            peer: B,
            displays: vec![]
        }),
        vec![Output::SetPortals(vec![])]
    );
    assert!(f.hud().is_empty());
    let mut unplaced = display(2);
    unplaced.name = "no placement".into();
    assert!(
        f.send(Input::PeerDisplays {
            peer: B,
            displays: vec![unplaced.clone()]
        })
        .is_empty()
    );
    assert_eq!(
        f.send(Input::PeerDisplays {
            peer: B,
            displays: vec![display(1), unplaced]
        }),
        vec![Output::SetPortals(f.layout.capture_portals(A))]
    );
    assert_eq!(
        f.send(Input::LocalDisplays(vec![])),
        vec![Output::SetPortals(vec![])]
    );

    let mut f = Fixture::new(config(), 2);
    f.up(B);
    f.hud();
    let placements = f
        .layout
        .displays()
        .iter()
        .map(|d| Placement {
            node: d.id.node,
            display: d.id.display,
            origin: if d.id.node == B {
                PointMm::new(200.0, 0.0)
            } else {
                d.origin
            },
            version: 2,
        })
        .collect();
    assert_eq!(
        f.send(Input::Layout(placements)),
        vec![Output::SetPortals(vec![]), Output::HideOverlay(HUD)]
    );
    assert!(
        f.send(Input::Overlay(OverlayEvent::Visible(HUD)))
            .is_empty()
    );
}

#[test]
fn push_delay_repeats_cancel_and_tick_deadline() {
    let mut cfg = config();
    cfg.push_to_cross = Duration::from_millis(100);
    let mut f = Fixture::new(cfg, 2);
    f.up(B);
    assert!(f.hud().is_empty());
    assert_eq!(f.engine.next_deadline(), Some(time(100)));
    let event = Input::Capture(CaptureEvent::EdgePressed {
        portal: f.portal,
        position: 0.25,
        at: time(50),
    });
    assert!(f.feed(50, event).is_empty());
    assert_eq!(f.engine.next_deadline(), Some(time(100)));
    assert!(
        f.feed(
            60,
            Input::Capture(CaptureEvent::EdgeReleased {
                portal: PortalId(999),
                at: time(60)
            })
        )
        .is_empty()
    );
    f.feed(
        70,
        Input::Capture(CaptureEvent::EdgeReleased {
            portal: f.portal,
            at: time(70),
        }),
    );
    assert_eq!(f.engine.next_deadline(), None);
    assert!(f.feed(100, Input::Tick).is_empty());
    let press = Input::Capture(CaptureEvent::EdgePressed {
        portal: f.portal,
        position: 0.75,
        at: time(110),
    });
    f.feed(110, press);
    assert!(f.feed(209, Input::Tick).is_empty());
    assert!(matches!(
        f.feed(210, Input::Tick).as_slice(),
        [Output::ShowOverlay { .. }]
    ));
    let (_, _, _, entry, _) = start(&f.feed(211, Input::Overlay(OverlayEvent::Visible(HUD))));
    assert_eq!(entry.y, 750.0);
}

#[test]
fn push_delay_can_complete_on_repeated_press_and_cancels_on_disarm_or_lock() {
    for cancel in [
        Input::Command(Command::Panic),
        Input::Session(SessionEvent::WillSleep),
    ] {
        let mut cfg = config();
        cfg.push_to_cross = Duration::from_millis(50);
        let mut f = Fixture::new(cfg, 2);
        f.up(B);
        f.hud();
        f.feed(20, cancel);
        assert_eq!(f.engine.next_deadline(), None);
        assert!(f.feed(50, Input::Tick).is_empty());
    }
    let mut cfg = config();
    cfg.push_to_cross = Duration::from_millis(50);
    let mut f = Fixture::new(cfg, 2);
    f.up(B);
    f.hud();
    let input = Input::Capture(CaptureEvent::EdgePressed {
        portal: f.portal,
        position: 0.5,
        at: time(50),
    });
    assert!(matches!(
        f.feed(50, input).as_slice(),
        [Output::ShowOverlay { .. }]
    ));
}

#[test]
fn crossing_requires_unlocked_active_awake_armed_and_local_portal() {
    let states = [
        SessionState {
            lock: LockState::Locked,
            ..PERMITTED
        },
        SessionState {
            lock: LockState::Unknown,
            ..PERMITTED
        },
        SessionState {
            active: Some(false),
            ..PERMITTED
        },
        SessionState {
            active: None,
            ..PERMITTED
        },
    ];
    for state in states {
        let mut f = Fixture::new(config(), 2);
        f.up(B);
        f.send(Input::Session(SessionEvent::State(state)));
        assert!(f.hud().is_empty());
    }
    let mut f = Fixture::new(config(), 2);
    f.up(B);
    f.send(Input::Session(SessionEvent::WillSleep));
    f.send(Input::Session(SessionEvent::State(PERMITTED)));
    assert!(f.hud().is_empty());
    f.send(Input::Session(SessionEvent::Woke));
    assert!(f.hud().is_empty()); // Woke invalidates the old session proof.
    f.send(Input::Session(SessionEvent::State(PERMITTED)));
    assert!(
        f.send(Input::Capture(CaptureEvent::EdgePressed {
            portal: PortalId(999),
            position: 0.5,
            at: time(0)
        }))
        .is_empty()
    );
    let remote_portal = f
        .layout
        .portals()
        .iter()
        .find(|p| p.from.node == B)
        .unwrap()
        .id;
    assert!(
        f.send(Input::Capture(CaptureEvent::EdgePressed {
            portal: remote_portal,
            position: 0.5,
            at: time(0)
        }))
        .is_empty()
    );
    f.send(Input::Command(Command::Panic));
    assert!(f.hud().is_empty());
    f.send(Input::Command(Command::Rearm));
    assert!(matches!(f.hud().as_slice(), [Output::ShowOverlay { .. }]));
}

#[test]
fn controller_starts_without_permission_to_capture() {
    let mut f = Fixture::new(config(), 2);
    f.engine = ControllerE1::new(&config(), time(0));
    f.send(Input::LocalDisplays(vec![display(1)]));
    f.send(Input::PeerDisplays {
        peer: B,
        displays: vec![display(1)],
    });
    f.send(Input::Layout(vec![
        Placement {
            node: A,
            display: DisplayId(1),
            origin: PointMm::zero(),
            version: 1,
        },
        Placement {
            node: B,
            display: DisplayId(1),
            origin: PointMm::new(100.0, 0.0),
            version: 1,
        },
    ]));
    f.up(B);
    assert!(f.hud().is_empty());
    assert_eq!(f.engine.next_deadline(), None);
}

#[test]
fn hud_must_be_visible_before_handshake_and_has_exact_appearance() {
    let mut f = Fixture::new(config(), 2);
    f.up(B);
    let out = f.hud();
    assert!(matches!(&out[..], [Output::ShowOverlay { id, overlay }]
        if *id == HUD && overlay.display == DisplayId(1) && overlay.anchor == OverlayAnchor::TopCenter
        && overlay.text == format!("Input → {}", B.short()) && overlay.accent == Rgb8 { r: 0x3b, g: 0x82, b: 0xf6 }));
    assert_eq!(f.engine.next_deadline(), Some(time(500)));
    assert!(
        f.feed(
            100,
            control(
                B,
                ControlMessage::ControlStarted {
                    session: SessionId(1)
                }
            )
        )
        .is_empty()
    );
    assert!(
        f.feed(200, Input::Overlay(OverlayEvent::Visible(TARGET_INDICATOR)))
            .is_empty()
    );
    let locks = LockKeys {
        caps_lock: Some(true),
        num_lock: Some(false),
        scroll_lock: None,
    };
    f.feed(201, Input::Capture(CaptureEvent::LockKeys(locks)));
    let (peer, session, display, entry, lock_keys) =
        start(&f.feed(300, Input::Overlay(OverlayEvent::Visible(HUD))));
    assert_eq!(
        (peer, session, display, entry, lock_keys),
        (
            B,
            SessionId(1),
            DisplayId(1),
            PointDevice::new(0.0, 500.0),
            locks
        )
    );
    assert_eq!(f.engine.next_deadline(), Some(time(1300)));
    assert!(
        f.feed(301, Input::Overlay(OverlayEvent::Visible(HUD)))
            .is_empty()
    );
}

#[test]
fn hud_timeout_and_unavailability_abort_without_starting_capture() {
    for input in [
        Input::Tick,
        Input::Overlay(OverlayEvent::Unavailable(HUD)),
        Input::Overlay(OverlayEvent::Visible(HUD)),
    ] {
        let mut f = Fixture::new(config(), 2);
        f.up(B);
        f.hud();
        assert!(f.feed(499, Input::Tick).is_empty());
        assert_eq!(f.feed(500, input), vec![Output::HideOverlay(HUD)]);
        assert_eq!(f.engine.next_deadline(), None);
        assert!(
            f.send(Input::Overlay(OverlayEvent::Visible(HUD)))
                .is_empty()
        );
    }
}

#[test]
fn refusal_and_handshake_timeout_abort() {
    let mut f = Fixture::new(config(), 2);
    let session = f.handshake();
    assert_eq!(
        f.feed(
            1,
            control(
                B,
                ControlMessage::ControlRefused {
                    session,
                    reason: Refusal::Busy
                }
            )
        ),
        vec![
            Output::Notice(Notice::Refused {
                peer: B,
                reason: Refusal::Busy
            }),
            Output::HideOverlay(HUD)
        ]
    );
    assert_eq!(f.engine.next_deadline(), None);
    let next = f.handshake();
    assert_eq!(next, SessionId(2));
    assert!(f.feed(1000, Input::Tick).is_empty());
    let out = f.feed(1001, Input::Tick);
    assert!(out.contains(&Output::Notice(Notice::LostConnection(B))));
    assert!(out.contains(&Output::HideOverlay(HUD)));
    assert!(!out.iter().any(|o| matches!(o, Output::BeginCapture { .. })));
    assert!(
        f.send(control(B, ControlMessage::ControlStarted { session: next }))
            .is_empty()
    );

    let mut f = Fixture::new(config(), 2);
    let session = f.handshake();
    let late_reply = f.feed(1001, control(B, ControlMessage::ControlStarted { session }));
    assert!(late_reply.contains(&Output::Notice(Notice::LostConnection(B))));
    assert!(late_reply.contains(&Output::HideOverlay(HUD)));
    assert!(
        !late_reply
            .iter()
            .any(|o| matches!(o, Output::BeginCapture { .. }))
    );
}

#[test]
fn capture_failure_rolls_back_and_capture_ids_do_not_repeat() {
    for failure in [
        Failure::Locked,
        Failure::SecureInput,
        Failure::PointerButtonHeld,
        Failure::PermissionDenied,
        Failure::Other,
    ] {
        let mut f = Fixture::new(config(), 2);
        let (session, capture) = f.begin();
        let out = f.send(Input::CaptureBegun {
            id: capture,
            result: Err(failure),
        });
        assert_eq!(
            out,
            vec![
                Output::SendControl {
                    peer: B,
                    msg: ControlMessage::EndControl {
                        session,
                        reason: EndReason::Released
                    }
                },
                Output::HideOverlay(HUD)
            ]
        );
        let (session, capture) = f.begin();
        assert_eq!(session, SessionId(2));
        assert_eq!(capture, CaptureId(2));
    }
}

#[test]
fn fences_accept_both_started_result_orders_and_ignore_wrong_ids() {
    for fence_first in [true, false] {
        let mut f = Fixture::new(config(), 2);
        let (_, capture) = f.begin();
        assert!(f.send(key(KEY, true, f.now)).is_empty());
        assert!(f.raw(1, 1.0).is_empty());
        f.send(Input::Capture(CaptureEvent::Started { id: CaptureId(999) }));
        assert!(f.send(button(true, f.now)).is_empty());
        if fence_first {
            f.send(Input::Capture(CaptureEvent::Started { id: capture }));
        }
        f.send(Input::CaptureBegun {
            id: CaptureId(999),
            result: Err(Failure::Other),
        });
        f.send(Input::CaptureBegun {
            id: capture,
            result: Ok(CaptureStart {
                held_keys: vec![KEY],
                lock_keys: LockKeys::default(),
            }),
        });
        if !fence_first {
            assert!(f.send(key(OTHER_KEY, true, f.now)).is_empty());
            f.send(Input::Capture(CaptureEvent::Started { id: capture }));
        }
        assert!(f.send(key(KEY, false, f.now)).is_empty()); // Held at entry was never forwarded.
        assert_eq!(transitions(&f.send(key(OTHER_KEY, true, f.now))).len(), 1);
        assert!(
            f.send(Input::Capture(CaptureEvent::Ended {
                id: CaptureId(999),
                reason: CaptureEnd::Lost
            }))
            .is_empty()
        );
        let out = f.send(Input::Capture(CaptureEvent::Ended {
            id: capture,
            reason: CaptureEnd::Lost,
        }));
        assert_eq!(transitions(&out).len(), 1);
        assert!(!out.iter().any(|o| matches!(o, Output::EndCapture { .. })));
        assert!(out.contains(&Output::HideOverlay(HUD)));
        assert!(f.send(key(OTHER_KEY, false, f.now)).is_empty());
        assert!(f.raw(2, 2.0).is_empty());
    }
}

#[test]
fn motion_converts_units_once_and_numbers_datagrams_separately() {
    let mut f = Fixture::new(config(), 2);
    let (session, _) = f.controlling(vec![]);
    f.send(key(KEY, true, f.now));
    let first = motions(&f.feed(
        1,
        motion(
            50.0,
            20.0,
            MotionKind::Accelerated {
                display: DisplayId(1),
            },
            time(1),
        ),
    ));
    assert_eq!(
        first,
        vec![(
            B,
            PointerMessage {
                session,
                seq: 1,
                display: DisplayId(1),
                position: PointDevice::new(50.0, 520.0)
            }
        )]
    );
    assert_eq!(
        motions(&f.raw(2, 10.0))[0].1,
        PointerMessage {
            session,
            seq: 2,
            display: DisplayId(1),
            position: PointDevice::new(60.0, 520.0)
        }
    );
    assert!(
        f.feed(
            3,
            motion(
                100.0,
                0.0,
                MotionKind::Accelerated {
                    display: DisplayId(999)
                },
                time(3)
            )
        )
        .is_empty()
    );
    assert_eq!(motions(&f.raw(4, 0.0))[0].1.seq, 3);
    assert_eq!(transitions(&f.send(key(KEY, false, f.now)))[0].2, 2);
}

#[test]
fn raw_motion_uses_configured_acceleration_curve() {
    let mut cfg = config();
    cfg.accel.max_gain = 4.0;
    cfg.accel.threshold = 50.0;
    let mut f = Fixture::new(cfg, 2);
    f.controlling(vec![]);
    // 10 units at the first sample's 8 ms: 125 mm/s, gain 2.5, hence 25 pixels.
    assert_eq!(motions(&f.raw(1, 10.0))[0].1.position.x, 25.0);
}

#[test]
fn crossing_home_warps_to_layout_entry_and_keeps_hud_until_ended() {
    let mut f = Fixture::new(config(), 2);
    let (session, capture) = f.controlling(vec![]);
    f.send(key(KEY, true, f.now));
    // Right after entering, the edge back home is disarmed (WP-1.39): move in first.
    f.raw(1, 50.0);
    let out = f.raw(2, -2000.0);
    assert_eq!(
        out[0],
        Output::EndCapture {
            warp_to: Some((DisplayId(1), PointDevice::new(999.0, 500.0)))
        }
    );
    assert_eq!(
        transitions(&out),
        vec![(B, session, 2, Held::Key(KEY), false)]
    );
    assert_end(&out, B, session, EndReason::Released);
    assert!(!out.contains(&Output::HideOverlay(HUD)));
    assert_eq!(f.engine.next_deadline(), Some(time(302)));
    assert!(f.raw(3, 10.0).is_empty());
    assert!(
        f.send(Input::Command(Command::ReleaseControl))
            .iter()
            .all(|o| !matches!(o, Output::EndCapture { .. }))
    );
    assert_eq!(f.ended(4, capture), vec![Output::HideOverlay(HUD)]);
    assert!(f.ended(5, capture).is_empty());
}

#[test]
fn button_held_blocks_crossing_and_restores_previous_pointer_position() {
    let mut f = Fixture::new(config(), 3);
    f.up(C);
    f.controlling(vec![]);
    // Right after entering, the edge back home is disarmed (WP-1.39): move in first.
    let inside = motions(&f.raw(1, 50.0))[0].1.position.x;
    f.send(button(true, f.now));
    assert!(f.raw(2, -2000.0).is_empty());
    assert!(f.raw(3, 2000.0).is_empty());
    assert!((motions(&f.raw(4, 1.0))[0].1.position.x - (inside + 1.0)).abs() < 1e-8);
    f.send(button(false, f.now));
    assert!(
        f.raw(5, -2000.0)
            .iter()
            .any(|o| matches!(o, Output::EndCapture { .. }))
    );
}

#[test]
fn unavailable_third_node_cannot_be_crossed() {
    let mut f = Fixture::new(config(), 3);
    f.controlling(vec![]);
    assert!(f.raw(1, 2000.0).is_empty());
    assert!((motions(&f.raw(2, 1.0))[0].1.position.x - 1.0).abs() < 1e-8);
}

#[test]
fn switch_releases_old_target_and_drops_later_ups_without_replaying_modifiers() {
    let mut f = Fixture::new(config(), 3);
    f.up(C);
    let (session, _) = f.controlling(vec![]);
    f.send(key(KEY, true, f.now));
    let locks = LockKeys {
        caps_lock: Some(true),
        ..LockKeys::default()
    };
    f.send(Input::Capture(CaptureEvent::LockKeys(locks)));
    let out = f.raw(1, 2000.0);
    assert_eq!(
        transitions(&out),
        vec![(B, session, 2, Held::Key(KEY), false)]
    );
    assert_end(&out, B, session, EndReason::Released);
    let (peer, next, display, entry, lock_keys) = start(&out);
    assert_eq!(
        (peer, next, display, entry, lock_keys),
        (
            C,
            SessionId(2),
            DisplayId(1),
            PointDevice::new(0.0, 500.0),
            locks
        )
    );
    assert!(out.iter().any(|o| matches!(o, Output::ShowOverlay { id, overlay } if *id == HUD && overlay.text == format!("Input → {}", C.short()))));
    assert!(!out.iter().any(|o| matches!(
        o,
        Output::EndCapture { .. } | Output::BeginCapture { .. } | Output::HideOverlay(_)
    )));
    assert_eq!(f.engine.next_deadline(), Some(time(1001)));
    assert!(f.send(key(OTHER_KEY, true, f.now)).is_empty());
    assert!(f.send(key(KEY, false, f.now)).is_empty());
    assert!(f.raw(2, 10.0).is_empty());
    assert!(
        f.send(control(B, ControlMessage::ControlStarted { session: next }))
            .is_empty()
    );
    assert!(
        f.send(control(C, ControlMessage::ControlStarted { session: next }))
            .is_empty()
    );
    assert!(f.send(key(OTHER_KEY, false, f.now)).is_empty());
    assert!(f.send(key(KEY, false, f.now)).is_empty());
    assert_eq!(
        transitions(&f.send(key(KEY, true, f.now))),
        vec![(C, next, 1, Held::Key(KEY), true)]
    );
    assert_eq!(motions(&f.raw(3, 1.0))[0].1.seq, 1);
}

#[test]
fn switch_refusal_and_timeout_end_kept_capture() {
    for refuse in [true, false] {
        let mut f = Fixture::new(config(), 3);
        f.up(C);
        let (_, capture) = f.controlling(vec![]);
        let (_, session, _, _, _) = start(&f.raw(1, 2000.0));
        let out = if refuse {
            f.feed(
                2,
                control(
                    C,
                    ControlMessage::ControlRefused {
                        session,
                        reason: Refusal::Permission,
                    },
                ),
            )
        } else {
            f.feed(1001, Input::Tick)
        };
        assert_returns(&out);
        assert!(out.contains(&Output::Notice(if refuse {
            Notice::Refused {
                peer: C,
                reason: Refusal::Permission,
            }
        } else {
            Notice::LostConnection(C)
        })));
        assert_eq!(f.ended(1002, capture), vec![Output::HideOverlay(HUD)]);
    }
}

#[test]
fn release_chord_is_checked_before_forwarding_and_seeded_keys_are_not_replayed() {
    for seeded in [true, false] {
        let mut f = Fixture::new(config(), 2);
        let (session, capture) = f.controlling(if seeded { MODIFIERS.to_vec() } else { vec![] });
        if !seeded {
            for usage in MODIFIERS {
                assert_eq!(transitions(&f.send(key(usage, true, f.now))).len(), 1);
            }
        }
        let out = f.feed(1, key(ESC, true, time(1)));
        assert_returns(&out);
        assert!(
            transitions(&out)
                .iter()
                .all(|(_, _, _, held, down)| *held != Held::Key(ESC) && !down)
        );
        assert_eq!(transitions(&out).len(), if seeded { 0 } else { 3 });
        assert_end(&out, B, session, EndReason::Released);
        f.ended(2, capture);
        assert!(f.hud().is_empty());
        assert!(f.send(key(ESC, false, f.now)).is_empty());
    }
    let mut f = Fixture::new(config(), 2);
    f.controlling(MODIFIERS.to_vec());
    f.send(key(MODIFIERS[0], false, f.now));
    assert_eq!(transitions(&f.send(key(ESC, true, f.now))).len(), 1);
}

#[test]
fn router_drops_duplicate_downs_unknown_ups_and_preserves_input_sequence() {
    let mut f = Fixture::new(config(), 2);
    let (session, _) = f.controlling(vec![]);
    assert!(f.send(key(KEY, false, f.now)).is_empty());
    assert_eq!(
        transitions(&f.send(key(KEY, true, f.now))),
        vec![(B, session, 1, Held::Key(KEY), true)]
    );
    assert!(f.send(key(KEY, true, f.now)).is_empty());
    assert_eq!(transitions(&f.send(button(true, f.now)))[0].2, 2);
    assert!(f.send(button(true, f.now)).is_empty());
    let delta = ScrollDelta {
        v120_x: 0,
        v120_y: 120,
        pixels: None,
        phase: ScrollPhase::Discrete,
        stop_x: false,
        stop_y: false,
    };
    assert_eq!(
        f.send(Input::Capture(CaptureEvent::Scroll { delta, at: f.now })),
        vec![Output::SendInput {
            peer: B,
            msg: InputMessage::Scroll {
                session,
                seq: 3,
                delta
            }
        }]
    );
    assert_eq!(transitions(&f.send(key(KEY, false, f.now)))[0].2, 4);
    assert!(f.send(key(KEY, false, f.now)).is_empty());
}

#[test]
fn ctrl_c_uses_target_profile_and_heartbeats_list_mapped_keys() {
    let ctrl = HidUsage::keyboard(0xE0);
    let gui = HidUsage::keyboard(0xE3);
    let c = HidUsage::keyboard(0x06);
    for profile in [RemapProfile::None, RemapProfile::SwapCtrlGui] {
        let mut cfg = config();
        if profile != RemapProfile::None {
            cfg.remap.insert(B, profile);
        }
        let mut f = Fixture::new(cfg, 2);
        // Locally handled keys never acquire a remote mapping.
        assert!(f.send(key(ctrl, true, f.now)).is_empty());
        assert!(f.send(key(ctrl, false, f.now)).is_empty());
        let (session, _) = f.controlling(vec![]);
        let mapped = profile.map(ctrl);
        let mut out = f.send(key(ctrl, true, f.now));
        assert!(f.send(key(ctrl, true, f.now)).is_empty());
        // An unknown physical up must not release another key's mapped usage.
        assert!(f.send(key(gui, false, f.now)).is_empty());
        out.extend(f.send(key(c, true, f.now)));
        out.extend(f.send(key(c, false, f.now)));
        out.extend(f.send(key(ctrl, false, f.now)));
        let expected: Vec<_> = [(mapped, true), (c, true), (c, false), (mapped, false)]
            .into_iter()
            .enumerate()
            .map(|(index, (usage, down))| Output::SendInput {
                peer: B,
                msg: InputMessage::Key {
                    session,
                    seq: index as u32 + 1,
                    usage,
                    down,
                },
            })
            .collect();
        assert_eq!(out, expected);
        assert!(f.send(key(ctrl, false, f.now)).is_empty());

        f.send(key(ctrl, true, f.now));
        assert_eq!(
            heartbeat(&f.send(Input::Tick)),
            (session, 6, vec![mapped], vec![])
        );
        assert_eq!(
            transitions(&f.send(key(ctrl, false, f.now))),
            vec![(B, session, 7, Held::Key(mapped), false)]
        );
    }
}

#[test]
fn crossing_releases_remapped_key_on_old_target_and_drops_its_physical_up() {
    let ctrl = HidUsage::keyboard(0xE0);
    let gui = HidUsage::keyboard(0xE3);
    for next_profile in [RemapProfile::None, RemapProfile::SwapCtrlGui] {
        let mut cfg = config();
        cfg.remap.insert(B, RemapProfile::SwapCtrlGui);
        if next_profile != RemapProfile::None {
            cfg.remap.insert(C, next_profile);
        }
        let mut f = Fixture::new(cfg, 3);
        f.up(C);
        let (session, _) = f.controlling(vec![]);
        assert_eq!(
            transitions(&f.send(key(ctrl, true, f.now))),
            vec![(B, session, 1, Held::Key(gui), true)]
        );
        let out = f.raw(1, 2000.0);
        // Switching ends B's session through release_all before C is activated.
        assert_eq!(
            transitions(&out),
            vec![(B, session, 2, Held::Key(gui), false)]
        );
        assert_end(&out, B, session, EndReason::Released);
        let (peer, next, _, _, _) = start(&out);
        assert_eq!(peer, C);
        assert!(
            f.send(control(C, ControlMessage::ControlStarted { session: next }))
                .is_empty()
        );

        let mapped = next_profile.map(gui);
        assert_eq!(
            transitions(&f.send(key(gui, true, f.now))),
            vec![(C, next, 1, Held::Key(mapped), true)]
        );
        assert!(f.send(key(ctrl, false, f.now)).is_empty());
        assert_eq!(
            heartbeat(&f.send(Input::Tick)),
            (next, 2, vec![mapped], vec![])
        );
        assert_eq!(
            transitions(&f.send(key(gui, false, f.now))),
            vec![(C, next, 3, Held::Key(mapped), false)]
        );
        assert_eq!(
            transitions(&f.send(key(ctrl, true, f.now))),
            vec![(C, next, 4, Held::Key(next_profile.map(ctrl)), true)]
        );
        assert_eq!(
            transitions(&f.send(key(ctrl, false, f.now))),
            vec![(C, next, 5, Held::Key(next_profile.map(ctrl)), false)]
        );
    }
}

#[test]
fn control_end_releases_mapped_key_and_later_physical_up_sends_nothing() {
    let ctrl = HidUsage::keyboard(0xE0);
    let gui = HidUsage::keyboard(0xE3);
    for command in [Some(Command::ReleaseControl), Some(Command::Panic), None] {
        let mut cfg = config();
        cfg.remap.insert(B, RemapProfile::SwapCtrlGui);
        let mut f = Fixture::new(cfg, 2);
        let (session, capture) = f.controlling(vec![MODIFIERS[1], MODIFIERS[2]]);
        assert_eq!(
            transitions(&f.send(key(ctrl, true, f.now))),
            vec![(B, session, 1, Held::Key(gui), true)]
        );
        let input = command.map_or_else(|| key(ESC, true, f.now), Input::Command);
        let out = f.send(input);
        assert_returns(&out);
        assert_eq!(
            transitions(&out),
            vec![(B, session, 2, Held::Key(gui), false)]
        );
        assert_end(
            &out,
            B,
            session,
            if command == Some(Command::Panic) {
                EndReason::Panic
            } else {
                EndReason::Released
            },
        );
        assert!(f.send(key(ctrl, false, f.now)).is_empty());
        f.ended(1, capture);
        assert!(f.send(key(ctrl, false, f.now)).is_empty());

        f.send(Input::Command(Command::Rearm));
        let (next, _) = f.controlling(vec![]);
        assert_eq!(
            transitions(&f.send(key(gui, true, f.now))),
            vec![(B, next, 1, Held::Key(ctrl), true)]
        );
        assert!(f.send(key(ctrl, false, f.now)).is_empty());
        assert_eq!(
            transitions(&f.send(key(gui, false, f.now))),
            vec![(B, next, 2, Held::Key(ctrl), false)]
        );
    }
}

#[test]
fn heartbeat_cadence_is_50_ms_held_and_250_ms_idle() {
    let mut f = Fixture::new(config(), 2);
    let (session, _) = f.controlling(vec![]);
    assert_eq!(f.engine.next_deadline(), Some(time(0)));
    let first = heartbeat(&f.feed(0, Input::Tick));
    assert_eq!(first, (session, 1, vec![], vec![]));
    f.send(discrete(B, InputMessage::Ack { session, seq: 1 }));
    assert_eq!(f.engine.next_deadline(), Some(time(250)));
    assert!(f.feed(249, Input::Tick).is_empty());
    let (_, seq, _, _) = heartbeat(&f.feed(250, Input::Tick));
    f.send(discrete(B, InputMessage::Ack { session, seq }));
    f.feed(251, key(KEY, true, time(251)));
    f.send(button(true, f.now));
    assert_eq!(f.engine.next_deadline(), Some(time(300)));
    assert!(f.feed(299, Input::Tick).is_empty());
    let (_, seq, keys, buttons) = heartbeat(&f.feed(300, Input::Tick));
    assert_eq!(keys, vec![KEY]);
    assert_eq!(buttons, vec![MouseButton::PRIMARY]);
    f.send(discrete(B, InputMessage::Ack { session, seq }));
    assert_eq!(f.engine.next_deadline(), Some(time(350)));
    let (_, seq, _, _) = heartbeat(&f.feed(350, Input::Tick));
    f.send(discrete(B, InputMessage::Ack { session, seq }));
    f.feed(351, key(KEY, false, time(351)));
    let up = transitions(&f.send(button(false, f.now)))[0].2;
    f.send(discrete(B, InputMessage::Ack { session, seq: up }));
    assert_eq!(f.engine.next_deadline(), Some(time(600)));
    assert!(f.feed(599, Input::Tick).is_empty());
    assert_eq!(heartbeat(&f.feed(600, Input::Tick)).2, vec![]);
}

#[test]
fn excess_keys_are_released_and_never_omitted_from_heartbeat() {
    let mut f = Fixture::new(config(), 2);
    f.controlling(vec![]);
    for id in 0..MAX_HELD_KEYS {
        f.send(key(
            HidUsage {
                page: 0x0c,
                id: id as u16,
            },
            true,
            f.now,
        ));
    }
    let excess = HidUsage {
        page: 0x0c,
        id: 100,
    };
    let out = f.send(key(excess, true, f.now));
    let ts = transitions(&out);
    assert_eq!(ts.len(), 2);
    assert!(ts[0].4);
    assert!(!ts[1].4);
    assert_eq!(heartbeat(&f.send(Input::Tick)).2.len(), MAX_HELD_KEYS);
    assert!(f.send(key(excess, false, f.now)).is_empty());
}

#[test]
fn ack_deadline_uses_rtt_and_ignores_wrong_stale_and_unsent_acks() {
    let mut f = Fixture::new(config(), 2);
    let (session, _) = f.controlling(vec![]);
    f.send(Input::Tick);
    assert_eq!(
        f.engine.next_deadline(),
        Some(time(150).saturating_add(Duration::from_nanos(1)))
    );
    for (peer, session, seq) in [
        (C, session, 1),
        (B, SessionId(999), 1),
        (B, session, 999),
        (B, session, 0),
    ] {
        f.send(discrete(peer, InputMessage::Ack { session, seq }));
        assert_eq!(
            f.engine.next_deadline(),
            Some(time(150).saturating_add(Duration::from_nanos(1)))
        );
    }
    f.send(Input::PeerRtt {
        peer: B,
        rtt: Duration::from_millis(100),
    });
    assert_eq!(f.engine.next_deadline(), Some(time(250))); // Heartbeat before the 400 ms ACK deadline.
    let (_, seq, _, _) = heartbeat(&f.feed(250, Input::Tick));
    assert_eq!(
        f.engine.next_deadline(),
        Some(time(400).saturating_add(Duration::from_nanos(1)))
    );
    f.feed(251, discrete(B, InputMessage::Ack { session, seq: 1 }));
    assert_eq!(f.engine.next_deadline(), Some(time(500)));
    f.send(discrete(B, InputMessage::Ack { session, seq: 1 }));
    assert_eq!(f.engine.next_deadline(), Some(time(500)));
    f.send(discrete(B, InputMessage::Ack { session, seq }));
    assert_eq!(f.engine.next_deadline(), Some(time(500)));
}

#[test]
fn ack_loss_returns_home_and_equality_reschedules_without_a_busy_loop() {
    let mut f = Fixture::new(config(), 2);
    let (session, capture) = f.controlling(vec![]);
    f.send(key(KEY, true, f.now));
    f.send(Input::Tick);
    assert_eq!(f.engine.next_deadline(), Some(time(50)));
    let out = f.feed(150, Input::Tick);
    assert!(!out.contains(&Output::Notice(Notice::LostConnection(B))));
    let after_boundary = time(150).saturating_add(Duration::from_nanos(1));
    assert_eq!(f.engine.next_deadline(), Some(after_boundary));
    let out = f.at(after_boundary, Input::Tick);
    assert_returns(&out);
    assert!(out.contains(&Output::Notice(Notice::LostConnection(B))));
    assert_eq!(transitions(&out).len(), 1);
    assert_end(&out, B, session, EndReason::LinkLost);
    assert_eq!(f.ended(151, capture), vec![Output::HideOverlay(HUD)]);
}

#[test]
fn status_override_returns_home_releases_held_and_uses_distinct_notice() {
    let mut f = Fixture::new(config(), 2);
    let (session, capture) = f.controlling(vec![]);
    f.send(key(KEY, true, f.now));
    f.send(button(true, f.now));
    let out = f.feed(
        10,
        discrete(
            B,
            InputMessage::Status {
                session,
                status: TargetStatus::LocalOverride,
            },
        ),
    );
    assert_returns(&out);
    assert_eq!(out.first(), Some(&Output::Notice(Notice::LocalOverride(B))));
    assert!(
        !out.iter()
            .any(|o| matches!(o, Output::Notice(Notice::ControlReleased { .. })))
    );
    let released: BTreeSet<_> = transitions(&out)
        .into_iter()
        .map(|(peer, s, _, held, down)| {
            assert_eq!((peer, s, down), (B, session, false));
            held
        })
        .collect();
    assert_eq!(
        released,
        BTreeSet::from([Held::Key(KEY), Held::Button(MouseButton::PRIMARY)])
    );
    assert_end(&out, B, session, EndReason::Released);
    assert_eq!(f.engine.established(), None);
    assert!(f.raw(11, 10.0).is_empty());
    assert_eq!(f.ended(12, capture), vec![Output::HideOverlay(HUD)]);
    for input in [
        discrete(
            B,
            InputMessage::Status {
                session,
                status: TargetStatus::LocalOverride,
            },
        ),
        discrete(
            B,
            InputMessage::Status {
                session,
                status: TargetStatus::Resumed,
            },
        ),
        control(
            B,
            ControlMessage::EndControl {
                session,
                reason: EndReason::Released,
            },
        ),
    ] {
        assert!(f.send(input).is_empty());
    }
    assert!(f.feed(1010, Input::Tick).is_empty());
}

#[test]
fn local_override_guards_every_outgoing_portal_for_150_ms_then_allows_crossing() {
    for destination in [B, C] {
        let mut f = Fixture::new(config(), 3);
        let positions = [
            (A, PointMm::zero()),
            (B, PointMm::new(100.0, 0.0)),
            (C, PointMm::new(0.0, 100.0)),
        ];
        f.layout = Layout::new(
            positions
                .iter()
                .map(|(node, origin)| Placed {
                    id: GlobalDisplayId {
                        node: *node,
                        display: DisplayId(1),
                    },
                    geometry: display(1).geometry,
                    origin: *origin,
                })
                .collect(),
            config().layout,
        )
        .unwrap();
        let to_b = f
            .layout
            .portals()
            .iter()
            .find(|p| p.from.node == A && p.to.node == B)
            .unwrap()
            .id;
        let tested = f
            .layout
            .portals()
            .iter()
            .find(|p| p.from.node == A && p.to.node == destination)
            .unwrap()
            .id;
        f.portal = to_b;
        f.send(Input::Layout(
            positions
                .into_iter()
                .map(|(node, origin)| Placement {
                    node,
                    display: DisplayId(1),
                    origin,
                    version: 2,
                })
                .collect(),
        ));
        f.up(C);
        let (session, capture) = f.controlling(vec![]);
        assert_eq!(f.engine.portals().len(), 2);
        f.feed(
            10,
            discrete(
                B,
                InputMessage::Status {
                    session,
                    status: TargetStatus::LocalOverride,
                },
            ),
        );
        f.ended(11, capture);
        // Even a release arriving within the guard cannot re-arm a portal early.
        assert!(
            f.feed(
                12,
                Input::Capture(CaptureEvent::EdgeReleased {
                    portal: tested,
                    at: time(12)
                })
            )
            .is_empty()
        );
        for at in [13, 159] {
            assert!(
                f.feed(
                    at,
                    Input::Capture(CaptureEvent::EdgePressed {
                        portal: tested,
                        position: 0.5,
                        at: time(at)
                    })
                )
                .is_empty()
            );
        }
        assert!(f.engine.armed());
        let out = f.feed(
            160,
            Input::Capture(CaptureEvent::EdgePressed {
                portal: tested,
                position: 0.5,
                at: time(160),
            }),
        );
        assert!(shows_hud(&out));
        let out = f.send(Input::Overlay(OverlayEvent::Visible(HUD)));
        assert_eq!(start(&out).0, destination);
    }
}

#[test]
fn target_local_activity_hands_over_between_real_engines_with_every_down_released() {
    use crosspane_engine::{Engine, InjectCmd};
    use crosspane_input::journal::MemoryJournal;

    fn deliver(from: NodeId, to: &mut Engine, out: &[Output], now: MonoTime) -> Vec<Output> {
        let mut replies = Vec::new();
        for output in out {
            let event = match output {
                Output::SendControl { msg, .. } => LinkEvent::Control {
                    peer: from,
                    msg: msg.clone(),
                },
                Output::SendInput { msg, .. } => LinkEvent::Input {
                    peer: from,
                    msg: msg.clone(),
                },
                Output::SendMotion { msg, .. } => LinkEvent::Motion {
                    peer: from,
                    msg: *msg,
                },
                _ => continue,
            };
            replies.extend(to.handle(Input::Link(event), now));
        }
        replies
    }

    fn inject(
        engine: &mut Engine,
        out: &[Output],
        now: MonoTime,
        held: &mut BTreeSet<Held>,
        counts: &mut BTreeMap<Held, (u32, u32)>,
    ) {
        for output in out {
            let Output::Inject { id, cmd } = output else {
                continue;
            };
            let transition = match cmd {
                InjectCmd::Key { usage, down } => Some((Held::Key(*usage), *down)),
                InjectCmd::Button { button, down } => Some((Held::Button(*button), *down)),
                InjectCmd::ReleaseAll => {
                    for item in std::mem::take(held) {
                        counts.entry(item).or_default().1 += 1;
                    }
                    None
                }
                _ => None,
            };
            if let Some((item, down)) = transition {
                if down {
                    assert!(held.insert(item), "duplicate down: {item:?}");
                    counts.entry(item).or_default().0 += 1;
                } else {
                    assert!(held.remove(&item), "unpaired up: {item:?}");
                    counts.entry(item).or_default().1 += 1;
                }
            }
            assert!(
                engine
                    .handle(Input::InjectDone { id: *id, ok: true }, now)
                    .is_empty()
            );
        }
    }

    let (mut controller, portal) = exclusive_engine_with(config());
    let (mut target, _) = Engine::new(
        EngineConfig::new(B),
        Box::new(MemoryJournal::default()),
        Box::new(MemoryJournal::default()),
        time(0),
    )
    .unwrap();
    for input in [
        Input::LocalDisplays(vec![display(1)]),
        Input::PeerUp { peer: A },
        Input::Session(SessionEvent::State(PERMITTED)),
        Input::Grants([(A, [crosspane_protocol::msg::Capability::InputAccept].into())].into()),
    ] {
        target.handle(input, time(0));
    }
    let mut held = BTreeSet::new();
    let mut counts = BTreeMap::new();
    let hud = controller.handle(edge_pressed(portal, time(0)), time(0));
    assert!(shows_hud(&hud));
    let start_out = controller.handle(Input::Overlay(OverlayEvent::Visible(HUD)), time(0));
    let session = start(&start_out).1;
    let accepted = deliver(A, &mut target, &start_out, time(0));
    inject(&mut target, &accepted, time(0), &mut held, &mut counts);
    let capture_out = deliver(B, &mut controller, &accepted, time(0));
    let capture = capture_out
        .iter()
        .find_map(|o| match o {
            Output::BeginCapture {
                id, portal: from, ..
            } => {
                assert_eq!(*from, portal);
                Some(*id)
            }
            _ => None,
        })
        .unwrap();
    controller.handle(
        Input::Capture(CaptureEvent::Started { id: capture }),
        time(0),
    );
    controller.handle(
        Input::CaptureBegun {
            id: capture,
            result: Ok(CaptureStart {
                held_keys: vec![],
                lock_keys: LockKeys::default(),
            }),
        },
        time(0),
    );
    assert_eq!(controller.control_established(), Some(B));
    assert_eq!(target.controlled_by(), Some(A));
    for input in [
        key(KEY, true, time(1)),
        key(OTHER_KEY, true, time(1)),
        button(true, time(1)),
    ] {
        let out = controller.handle(input, time(1));
        let injected = deliver(A, &mut target, &out, time(1));
        inject(&mut target, &injected, time(1), &mut held, &mut counts);
        deliver(B, &mut controller, &injected, time(1));
    }
    assert_eq!(held.len(), 3);
    let override_out = target.handle(
        Input::Capture(CaptureEvent::LocalActivity { at: time(10) }),
        time(10),
    );
    assert_eq!(target.controlled_by(), None);
    assert!(override_out.contains(&Output::Notice(Notice::ControlEnded(A))));
    assert!(
        !override_out
            .iter()
            .any(|o| matches!(o, Output::Notice(Notice::LocalOverride(_))))
    );
    assert!(
        override_out.iter().all(|o| !matches!(
            o,
            Output::Inject {
                cmd: InjectCmd::MoveTo { .. },
                ..
            }
        )),
        "the target keeps its physical pointer"
    );
    inject(&mut target, &override_out, time(10), &mut held, &mut counts);
    assert!(held.is_empty());
    assert_eq!(
        counts,
        BTreeMap::from([
            (Held::Key(KEY), (1, 1)),
            (Held::Key(OTHER_KEY), (1, 1)),
            (Held::Button(MouseButton::PRIMARY), (1, 1)),
        ])
    );
    let returned = deliver(B, &mut controller, &override_out, time(10));
    assert_eq!(controller.controlling(), None);
    assert_eq!(controller.control_established(), None);
    assert_returns(&returned);
    assert!(returned.contains(&Output::Notice(Notice::LocalOverride(B))));
    assert!(
        !returned
            .iter()
            .any(|o| matches!(o, Output::Notice(Notice::ControlReleased { .. })))
    );
    assert_end(&returned, B, session, EndReason::Released);
    // The native pointer remained parked at the capture's departing portal throughout.
    assert!(
        !returned
            .iter()
            .any(|o| matches!(o, Output::ReleaseAndWarp { .. }))
    );
    assert!(deliver(A, &mut target, &returned, time(10)).is_empty());
    assert_eq!(
        controller.handle(
            Input::Capture(CaptureEvent::Ended {
                id: capture,
                reason: CaptureEnd::Requested,
            }),
            time(11)
        ),
        vec![Output::HideOverlay(HUD)]
    );
    assert!(
        controller
            .handle(edge_pressed(portal, time(12)), time(12))
            .is_empty()
    );
    assert!(target.handle(Input::Tick, time(1011)).is_empty());
    assert!(controller.handle(Input::Tick, time(1011)).is_empty());
    assert!(
        target
            .handle(
                Input::Capture(CaptureEvent::LocalActivity { at: time(1012) }),
                time(1012)
            )
            .is_empty()
    );
}

#[test]
fn status_resumed_keeps_control_and_refusal_returns() {
    let mut f = Fixture::new(config(), 2);
    let (session, _) = f.controlling(vec![]);
    assert_eq!(motions(&f.raw(1, 1.0)).len(), 1);
    assert!(
        f.send(discrete(
            B,
            InputMessage::Status {
                session,
                status: TargetStatus::Resumed
            }
        ))
        .is_empty()
    );
    let out = f.send(discrete(
        B,
        InputMessage::Status {
            session,
            status: TargetStatus::Refused(Refusal::SecureInput),
        },
    ));
    assert_returns(&out);
    assert!(out.contains(&Output::Notice(Notice::Refused {
        peer: B,
        reason: Refusal::SecureInput
    })));
}

#[test]
fn target_end_returns_without_echo_and_uses_correct_notice() {
    for reason in [EndReason::TargetLocked, EndReason::Released] {
        let mut f = Fixture::new(config(), 2);
        let (session, _) = f.controlling(vec![]);
        f.send(key(KEY, true, f.now));
        assert!(
            f.send(control(C, ControlMessage::EndControl { session, reason }))
                .is_empty()
        );
        assert!(
            f.send(control(
                B,
                ControlMessage::EndControl {
                    session: SessionId(999),
                    reason
                }
            ))
            .is_empty()
        );
        let out = f.send(control(B, ControlMessage::EndControl { session, reason }));
        assert_returns(&out);
        assert_eq!(transitions(&out).len(), 1);
        assert!(
            out.contains(&Output::Notice(if reason == EndReason::TargetLocked {
                Notice::TargetLocked(B)
            } else {
                Notice::ControlEnded(B)
            }))
        );
        assert!(!out.iter().any(|o| matches!(o, Output::SendControl { .. })));
    }
}

#[test]
fn closed_link_skips_sends_and_forgets_held_items() {
    let mut f = Fixture::new(config(), 3);
    let (_, capture) = f.controlling(vec![]);
    f.send(key(KEY, true, f.now));
    f.send(button(true, f.now));
    assert!(
        f.send(Input::Link(LinkEvent::Closed {
            peer: C,
            error: LinkError::Closed
        }))
        .is_empty()
    );
    let out = f.send(Input::Link(LinkEvent::Closed {
        peer: B,
        error: LinkError::Closed,
    }));
    assert_returns(&out);
    assert!(
        !out.iter()
            .any(|o| matches!(o, Output::SendInput { .. } | Output::SendControl { .. }))
    );
    f.ended(1, capture);
    f.controlling(vec![]);
    assert_eq!(transitions(&f.send(key(KEY, true, f.now))).len(), 1);
    assert_eq!(transitions(&f.send(button(true, f.now))).len(), 1);
}

#[test]
fn hud_loss_lock_sleep_and_capture_loss_release_everything() {
    for input in [
        Input::Overlay(OverlayEvent::Unavailable(HUD)),
        Input::Session(SessionEvent::State(SessionState {
            lock: LockState::Locked,
            ..PERMITTED
        })),
        Input::Session(SessionEvent::WillSleep),
        Input::Capture(CaptureEvent::Ended {
            id: CaptureId(1),
            reason: CaptureEnd::Lost,
        }),
        Input::Capture(CaptureEvent::Ended {
            id: CaptureId(1),
            reason: CaptureEnd::Aborted,
        }),
    ] {
        let mut f = Fixture::new(config(), 2);
        let (session, _) = f.controlling(vec![]);
        f.send(key(KEY, true, f.now));
        f.send(button(true, f.now));
        let already_ended = matches!(input, Input::Capture(CaptureEvent::Ended { .. }));
        let locked = matches!(input, Input::Session(_));
        let out = f.feed(1, input);
        assert_eq!(transitions(&out).len(), 2);
        assert_end(
            &out,
            B,
            session,
            if locked {
                EndReason::ControllerLocked
            } else {
                EndReason::Released
            },
        );
        if already_ended {
            assert!(!out.iter().any(|o| matches!(o, Output::EndCapture { .. })));
            assert!(out.contains(&Output::HideOverlay(HUD)));
        } else {
            assert_returns(&out);
        }
    }
}

#[test]
fn keyboard_blinding_waits_for_ended_fence() {
    let mut f = Fixture::new(config(), 2);
    let (_, capture) = f.controlling(vec![]);
    f.send(key(KEY, true, f.now));
    assert!(
        f.send(Input::Capture(CaptureEvent::KeyboardBlinded(true)))
            .is_empty()
    );
    assert!(
        f.send(Input::Capture(CaptureEvent::KeyboardBlinded(false)))
            .is_empty()
    );
    let out = f.send(Input::Capture(CaptureEvent::Ended {
        id: capture,
        reason: CaptureEnd::Lost,
    }));
    assert_eq!(transitions(&out).len(), 1);
    assert!(out.contains(&Output::HideOverlay(HUD)));
}

#[test]
fn return_timeout_hides_once_and_late_capture_results_do_not_activate() {
    let mut f = Fixture::new(config(), 2);
    let (_, capture) = f.begin();
    assert_returns(&f.feed(1, Input::Command(Command::ReleaseControl)));
    assert_eq!(f.engine.next_deadline(), Some(time(301)));
    assert!(
        f.feed(2, Input::Capture(CaptureEvent::Started { id: capture }))
            .is_empty()
    );
    assert_eq!(
        f.feed(
            3,
            Input::CaptureBegun {
                id: capture,
                result: Ok(CaptureStart {
                    held_keys: vec![],
                    lock_keys: LockKeys::default()
                }),
            }
        ),
        vec![Output::EndCapture { warp_to: None }],
    );
    assert!(f.feed(300, Input::Tick).is_empty());
    assert_eq!(
        f.feed(301, Input::Tick),
        vec![
            Output::EndCapture { warp_to: None },
            Output::HideOverlay(HUD)
        ]
    );
    assert_eq!(f.engine.next_deadline(), None);
    assert!(f.ended(302, capture).is_empty());
    let mut f = Fixture::new(config(), 2);
    let (_, capture) = f.begin();
    f.send(Input::Command(Command::ReleaseControl));
    assert_eq!(
        f.send(Input::CaptureBegun {
            id: capture,
            result: Err(Failure::Locked)
        }),
        vec![Output::HideOverlay(HUD)]
    );
}

#[test]
fn commands_release_and_panic_disarm_and_rearm_restores_portals() {
    for (command, reason) in [
        (Command::ReleaseControl, EndReason::Released),
        (Command::Panic, EndReason::Panic),
    ] {
        let mut f = Fixture::new(config(), 2);
        let (session, capture) = f.controlling(vec![]);
        f.send(key(KEY, true, f.now));
        f.send(button(true, f.now));
        let out = f.feed(1, Input::Command(command));
        assert_returns(&out);
        assert_end(&out, B, session, reason);
        assert_eq!(transitions(&out).len(), 2);
        assert_eq!(
            out.contains(&Output::Notice(Notice::Panic)),
            command == Command::Panic
        );
        assert!(out.contains(&Output::SetPortals(vec![])));
        f.ended(2, capture);
        assert!(f.hud().is_empty());
        assert_eq!(
            f.send(Input::Command(Command::Rearm)),
            vec![Output::SetPortals(f.layout.capture_portals(A))]
        );
        assert!(!f.hud().is_empty());
    }
}

#[test]
fn hotkey_release_is_immediate_hold_panics_once_and_requires_fresh_press_to_rearm() {
    let mut f = Fixture::new(config(), 2);
    let (session, capture) = f.controlling(vec![]);
    let out = f.feed(10, Input::Hotkey(HotkeyEvent::Pressed { at: time(10) }));
    assert_returns(&out);
    assert_end(&out, B, session, EndReason::Released);
    assert_eq!(f.engine.next_deadline(), Some(time(310)));
    f.ended(11, capture);
    assert_eq!(f.engine.next_deadline(), Some(time(1010)));
    assert!(f.feed(1009, Input::Tick).is_empty());
    assert_eq!(
        f.feed(1010, Input::Tick),
        vec![Output::EngineGate(false), Output::Notice(Notice::Panic)]
    );
    assert_eq!(f.engine.next_deadline(), None);
    assert!(f.feed(1011, Input::Tick).is_empty());
    assert!(
        f.feed(
            1012,
            Input::Hotkey(HotkeyEvent::Released { at: time(1012) })
        )
        .is_empty()
    );
    assert!(f.hud().is_empty());
    assert!(
        f.feed(1020, Input::Hotkey(HotkeyEvent::Pressed { at: time(1020) }))
            .is_empty()
    );
    assert_eq!(
        f.feed(
            1021,
            Input::Hotkey(HotkeyEvent::Released { at: time(1021) })
        ),
        vec![
            Output::EngineGate(true),
            Output::SetPortals(f.layout.capture_portals(A))
        ]
    );
    assert_eq!(f.engine.next_deadline(), None);
    assert!(!f.hud().is_empty());
}

#[test]
fn hotkey_timer_cancels_and_pre_disarm_press_cannot_rearm() {
    let mut f = Fixture::new(config(), 2);
    f.up(B);
    f.feed(1, Input::Hotkey(HotkeyEvent::Pressed { at: time(1) }));
    assert_eq!(f.engine.next_deadline(), Some(time(1001)));
    f.feed(2, Input::Command(Command::Panic));
    assert!(
        f.feed(3, Input::Hotkey(HotkeyEvent::Released { at: time(3) }))
            .is_empty()
    );
    assert_eq!(f.engine.next_deadline(), None);
    assert!(f.feed(1001, Input::Tick).is_empty());
    assert!(f.hud().is_empty());
    f.feed(1010, Input::Hotkey(HotkeyEvent::Pressed { at: time(1010) }));
    assert!(
        f.feed(
            1011,
            Input::Hotkey(HotkeyEvent::Released { at: time(1011) })
        )
        .contains(&Output::SetPortals(f.layout.capture_portals(A)))
    );

    // A fresh press may start during Returning; the release must happen in Idle.
    let mut f = Fixture::new(config(), 2);
    let (_, capture) = f.controlling(vec![]);
    f.feed(1, Input::Command(Command::Panic));
    f.feed(2, Input::Hotkey(HotkeyEvent::Pressed { at: time(2) }));
    f.ended(3, capture);
    assert!(
        f.feed(4, Input::Hotkey(HotkeyEvent::Released { at: time(4) }))
            .contains(&Output::SetPortals(f.layout.capture_portals(A)))
    );

    // Delivery order does not turn the original captured chord into a fresh re-arm press.
    let mut f = Fixture::new(config(), 2);
    let (_, capture) = f.controlling(MODIFIERS.to_vec());
    f.feed(1, key(ESC, true, time(1)));
    f.ended(2, capture);
    f.send(Input::Hotkey(HotkeyEvent::Pressed { at: time(1) }));
    assert!(
        f.feed(3, Input::Hotkey(HotkeyEvent::Released { at: time(3) }))
            .is_empty()
    );
    assert!(f.hud().is_empty());
}

#[test]
fn ticks_at_exact_next_deadline_always_advance_or_finish() {
    for stage in 0..6 {
        let mut cfg = config();
        cfg.push_to_cross = if stage == 0 {
            Duration::from_millis(50)
        } else {
            Duration::ZERO
        };
        let mut f = Fixture::new(cfg, 2);
        match stage {
            0 | 1 => {
                f.up(B);
                f.hud();
            }
            2 => {
                f.handshake();
            }
            3 => {
                f.begin();
            }
            4 => {
                f.controlling(vec![]);
                f.send(key(KEY, true, f.now));
            }
            _ => {
                f.controlling(vec![]);
                f.send(Input::Hotkey(HotkeyEvent::Pressed { at: f.now }));
            }
        }
        let mut ticks = 0;
        while let Some(deadline) = f.engine.next_deadline() {
            assert!(deadline >= f.now);
            f.at(deadline, Input::Tick);
            assert!(
                f.engine.next_deadline().is_none_or(|next| next > deadline),
                "stage {stage}: deadline failed to advance"
            );
            ticks += 1;
            assert!(ticks < 20, "stage {stage}: deadlines never drained");
        }
        assert!(ticks > 0);
    }
}

#[test]
fn release_command_in_idle_is_a_no_op_even_with_pending_push() {
    let mut f = Fixture::new(config(), 2);
    f.up(B);
    assert!(f.send(Input::Command(Command::ReleaseControl)).is_empty());
    assert!(matches!(f.hud().as_slice(), [Output::ShowOverlay { .. }]));

    let mut cfg = config();
    cfg.push_to_cross = Duration::from_millis(50);
    let mut f = Fixture::new(cfg, 2);
    f.up(B);
    f.hud();
    assert!(
        f.feed(10, Input::Command(Command::ReleaseControl))
            .is_empty()
    );
    assert_eq!(f.engine.next_deadline(), Some(time(50)));
    assert!(matches!(
        f.feed(50, Input::Tick).as_slice(),
        [Output::ShowOverlay { .. }]
    ));

    let mut f = Fixture::new(config(), 2);
    f.up(B);
    f.send(Input::Command(Command::Panic));
    assert!(f.send(Input::Command(Command::ReleaseControl)).is_empty());
    assert!(f.hud().is_empty());
}

#[test]
fn fresh_chord_taps_toggle_arming_in_idle() {
    let mut f = Fixture::new(config(), 2);
    f.up(B);
    assert_eq!(
        f.feed(10, Input::Hotkey(HotkeyEvent::Pressed { at: time(10) })),
        vec![Output::SetPortals(vec![])]
    );
    assert!(
        f.feed(11, Input::Hotkey(HotkeyEvent::Released { at: time(11) }))
            .is_empty()
    );
    assert!(f.hud().is_empty());
    assert!(
        f.feed(20, Input::Hotkey(HotkeyEvent::Pressed { at: time(20) }))
            .is_empty()
    );
    assert_eq!(
        f.feed(21, Input::Hotkey(HotkeyEvent::Released { at: time(21) })),
        vec![
            Output::EngineGate(true),
            Output::SetPortals(f.layout.capture_portals(A))
        ]
    );
    assert!(!f.hud().is_empty());
}

#[test]
fn captured_chord_pair_cannot_rearm_even_with_a_later_platform_timestamp() {
    let mut f = Fixture::new(config(), 2);
    let (_, capture) = f.controlling(MODIFIERS.to_vec());
    assert_returns(&f.feed(10, key(ESC, true, time(10))));
    f.ended(10, capture);
    assert!(
        f.feed(12, Input::Hotkey(HotkeyEvent::Pressed { at: time(11) }))
            .is_empty()
    );
    assert!(
        f.feed(300, Input::Hotkey(HotkeyEvent::Released { at: time(300) }))
            .is_empty()
    );
    assert!(f.hud().is_empty());
    // Freshness comes from the completed authoritative pair, never a timestamp comparison.
    f.feed(310, Input::Hotkey(HotkeyEvent::Pressed { at: time(0) }));
    assert!(
        f.feed(311, Input::Hotkey(HotkeyEvent::Released { at: time(1) }))
            .contains(&Output::SetPortals(f.layout.capture_portals(A)))
    );
    assert!(!f.hud().is_empty());
}

#[test]
fn orphan_capture_success_is_ended_in_idle_returning_and_controlling() {
    let start = CaptureStart {
        held_keys: vec![],
        lock_keys: LockKeys::default(),
    };
    let mut f = Fixture::new(config(), 2);
    assert_eq!(
        f.send(Input::CaptureBegun {
            id: CaptureId(999),
            result: Ok(start.clone())
        }),
        vec![Output::EndCapture { warp_to: None }]
    );
    let (_, capture) = f.begin();
    f.feed(1, Input::Overlay(OverlayEvent::Unavailable(HUD)));
    assert_eq!(
        f.feed(301, Input::Tick),
        vec![
            Output::EndCapture { warp_to: None },
            Output::HideOverlay(HUD)
        ]
    );
    assert_eq!(
        f.feed(
            400,
            Input::CaptureBegun {
                id: capture,
                result: Ok(start.clone())
            }
        ),
        vec![Output::EndCapture { warp_to: None }]
    );
    assert_eq!(f.engine.next_deadline(), None);

    let mut f = Fixture::new(config(), 2);
    let (_, capture) = f.controlling(vec![]);
    assert_eq!(
        f.send(Input::CaptureBegun {
            id: capture,
            result: Ok(start.clone())
        }),
        vec![Output::EndCapture { warp_to: None }]
    );
    f.send(Input::Command(Command::ReleaseControl));
    assert_eq!(
        f.send(Input::CaptureBegun {
            id: capture,
            result: Ok(start)
        }),
        vec![Output::EndCapture { warp_to: None }]
    );
}

#[test]
fn keys_after_started_are_merged_with_activation_held_keys() {
    for seeded in [false, true] {
        let mut f = Fixture::new(config(), 2);
        let (session, capture) = f.begin();
        f.send(Input::Capture(CaptureEvent::Started { id: capture }));
        let early = if seeded {
            &MODIFIERS[2..]
        } else {
            &MODIFIERS[..]
        };
        for usage in early {
            assert!(f.send(key(*usage, true, f.now)).is_empty());
        }
        let held_keys = if seeded {
            MODIFIERS[..2].to_vec()
        } else {
            vec![]
        };
        assert!(
            f.send(Input::CaptureBegun {
                id: capture,
                result: Ok(CaptureStart {
                    held_keys,
                    lock_keys: LockKeys::default()
                })
            })
            .is_empty()
        );
        let out = f.feed(1, key(ESC, true, time(1)));
        assert_returns(&out);
        assert_end(&out, B, session, EndReason::Released);
        assert!(transitions(&out).is_empty());
        assert!(out.contains(&Output::SetPortals(vec![])));
    }
}

#[test]
fn buttons_after_started_block_crossings_until_their_physical_ups() {
    for released_before_activation in [false, true] {
        let mut f = Fixture::new(config(), 3);
        f.up(C);
        let (_, capture) = f.begin();
        f.send(Input::Capture(CaptureEvent::Started { id: capture }));
        assert!(f.send(button(true, f.now)).is_empty());
        if released_before_activation {
            assert!(f.send(button(false, f.now)).is_empty());
        }
        f.send(Input::CaptureBegun {
            id: capture,
            result: Ok(CaptureStart {
                held_keys: vec![],
                lock_keys: LockKeys::default(),
            }),
        });
        if !released_before_activation {
            assert!(f.raw(1, 2000.0).is_empty());
            // The edge back home is disarmed right after entering (WP-1.39): move in first.
            f.raw(2, 50.0);
            assert!(f.raw(3, -2000.0).is_empty());
            assert!(f.send(button(false, f.now)).is_empty()); // Its down was never forwarded.
        }
        assert_eq!(start(&f.raw(4, 2000.0)).0, C);
    }
}

#[test]
fn capture_activation_times_out_after_one_second_and_cancels_pending_capture() {
    for started in [false, true] {
        let mut f = Fixture::new(config(), 2);
        let (session, capture) = f.begin();
        if started {
            f.send(Input::Capture(CaptureEvent::Started { id: capture }));
        }
        assert_eq!(f.engine.next_deadline(), Some(time(1000)));
        assert!(f.feed(999, Input::Tick).is_empty());
        let out = f.feed(1000, Input::Tick);
        assert_returns(&out);
        assert_end(&out, B, session, EndReason::Released);
        assert_eq!(f.engine.next_deadline(), Some(time(1300)));
        assert_eq!(
            f.feed(1300, Input::Tick),
            vec![
                Output::EndCapture { warp_to: None },
                Output::HideOverlay(HUD)
            ]
        );
        assert_eq!(f.engine.next_deadline(), None);
    }
    // A late success cannot beat the timeout simply because Tick has not arrived yet.
    let mut f = Fixture::new(config(), 2);
    let (session, capture) = f.begin();
    let out = f.feed(
        1000,
        Input::CaptureBegun {
            id: capture,
            result: Ok(CaptureStart {
                held_keys: vec![],
                lock_keys: LockKeys::default(),
            }),
        },
    );
    assert_returns(&out);
    assert_end(&out, B, session, EndReason::Released);
    assert!(f.raw(1001, 1.0).is_empty());
}

#[test]
fn accelerator_starts_fresh_for_every_activation_and_target_switch() {
    for second_sample_at in [3, 1000] {
        let mut cfg = config();
        cfg.accel.max_gain = 4.0;
        let mut f = Fixture::new(cfg, 2);
        let (_, capture) = f.controlling(vec![]);
        assert!((motions(&f.raw(1, 10.0))[0].1.position.x - 25.0).abs() < 1e-8);
        f.send(Input::Command(Command::ReleaseControl));
        f.ended(2, capture);
        f.send(Input::Command(Command::Rearm));
        f.controlling(vec![]);
        assert!((motions(&f.raw(second_sample_at, 10.0))[0].1.position.x - 25.0).abs() < 1e-8);
    }
    let mut cfg = config();
    cfg.accel.max_gain = 4.0;
    let mut f = Fixture::new(cfg, 3);
    f.up(C);
    f.controlling(vec![]);
    f.raw(1, 10.0);
    let (_, session, _, _, _) = start(&f.raw(2, 2000.0));
    f.feed(3, control(C, ControlMessage::ControlStarted { session }));
    assert!((motions(&f.raw(4, 10.0))[0].1.position.x - 25.0).abs() < 1e-8);
}

#[test]
fn panic_marks_current_hold_fired_for_command_and_hold_paths() {
    for command in [false, true] {
        let mut f = Fixture::new(config(), 2);
        f.up(B);
        f.feed(1, Input::Hotkey(HotkeyEvent::Pressed { at: time(1) }));
        let out = if command {
            f.feed(2, Input::Command(Command::Panic))
        } else {
            f.feed(1001, Input::Tick)
        };
        assert_eq!(
            out.iter()
                .filter(|o| **o == Output::Notice(Notice::Panic))
                .count(),
            1
        );
        assert_eq!(f.engine.next_deadline(), None);
        assert!(f.feed(1002, Input::Tick).is_empty());
        assert!(f.feed(2002, Input::Tick).is_empty());
        assert!(
            f.feed(
                2003,
                Input::Hotkey(HotkeyEvent::Released { at: time(2003) })
            )
            .is_empty()
        );
        assert!(f.hud().is_empty());
    }
}

#[test]
fn link_closure_in_hud_handshake_and_capture_wait_cancels_without_sending() {
    for stage in 0..3 {
        let mut f = Fixture::new(config(), 2);
        let capture = match stage {
            0 => {
                f.up(B);
                f.hud();
                None
            }
            1 => {
                f.handshake();
                None
            }
            _ => Some(f.begin().1),
        };
        let out = f.feed(
            1,
            Input::Link(LinkEvent::Closed {
                peer: B,
                error: LinkError::Closed,
            }),
        );
        assert!(out.contains(&Output::Notice(Notice::LostConnection(B))));
        assert!(out.contains(&Output::SetPortals(vec![])));
        assert!(!out.iter().any(|o| matches!(
            o,
            Output::SendInput { .. } | Output::SendControl { .. } | Output::BeginCapture { .. }
        )));
        if let Some(capture) = capture {
            assert_returns(&out);
            assert_eq!(f.engine.next_deadline(), Some(time(301)));
            assert_eq!(f.ended(2, capture), vec![Output::HideOverlay(HUD)]);
        } else {
            assert!(out.contains(&Output::HideOverlay(HUD)));
            assert!(!out.iter().any(|o| matches!(o, Output::EndCapture { .. })));
            assert_eq!(f.engine.next_deadline(), None);
        }
        assert!(f.hud().is_empty());
    }
}

#[test]
fn layout_rebuild_returns_home_if_active_pointer_or_hud_display_disappears() {
    for change in 0..4 {
        let mut f = Fixture::new(config(), 2);
        let (session, capture) = f.controlling(vec![]);
        f.send(key(KEY, true, f.now));
        f.send(button(true, f.now));
        let input = match change {
            0 => Input::LocalDisplays(vec![]),
            1 => Input::PeerDisplays {
                peer: B,
                displays: vec![],
            },
            2 => Input::Layout(vec![Placement {
                node: A,
                display: DisplayId(1),
                origin: PointMm::zero(),
                version: 2,
            }]),
            _ => Input::Layout(vec![
                Placement {
                    node: A,
                    display: DisplayId(1),
                    origin: PointMm::zero(),
                    version: 2,
                },
                Placement {
                    node: B,
                    display: DisplayId(1),
                    origin: PointMm::zero(),
                    version: 2,
                },
            ]),
        };
        let out = f.feed(1, input);
        assert_returns(&out);
        assert_end(&out, B, session, EndReason::Released);
        assert_eq!(transitions(&out).len(), 2);
        assert!(
            transitions(&out)
                .iter()
                .all(|(peer, sent_session, _, _, down)| *peer == B
                    && *sent_session == session
                    && !down)
        );
        assert!(out.contains(&Output::SetPortals(vec![])));
        assert!(f.raw(2, 1.0).is_empty());
        assert_eq!(f.ended(3, capture), vec![Output::HideOverlay(HUD)]);
    }
}

// Adapted from the lead reviewer's interleaved host property test.
#[derive(Default)]
struct Model {
    held: BTreeMap<NodeId, BTreeMap<Held, SessionId>>,
    closed: BTreeSet<NodeId>,
    sessions: BTreeMap<SessionId, NodeId>,
    last_session: Option<(NodeId, SessionId)>,
    // host
    pending: VecDeque<Input>,
    capture_active: Option<CaptureId>,
    last_capture: Option<CaptureId>,
    hud_shown: bool,
    hud_visible: bool,
    ended_sessions: BTreeSet<SessionId>,
    violations: Vec<String>,
}

impl Model {
    fn observe(&mut self, o: &Output, rng: u8) {
        match o {
            Output::ShowOverlay { id, .. } if *id == HUD => {
                if !self.hud_shown {
                    self.hud_shown = true;
                    self.hud_visible = false;
                    self.pending
                        .push_back(Input::Overlay(OverlayEvent::Visible(HUD)));
                }
            }
            Output::HideOverlay(id) if *id == HUD => {
                if self.capture_active.is_some() {
                    self.violations
                        .push("HUD hidden while capture active".into());
                }
                self.hud_shown = false;
                self.hud_visible = false;
                // drop pending Visible for old HUD
                self.pending
                    .retain(|i| !matches!(i, Input::Overlay(OverlayEvent::Visible(_))));
            }
            Output::SendControl {
                peer,
                msg: ControlMessage::StartControl { session, .. },
            } => {
                if self.closed.contains(peer) {
                    self.violations.push("StartControl on closed link".into());
                }
                if !self.hud_visible {
                    self.violations
                        .push("StartControl before HUD visible".into());
                }
                self.sessions.insert(*session, *peer);
                self.last_session = Some((*peer, *session));
                self.pending.push_back(control(
                    *peer,
                    ControlMessage::ControlStarted { session: *session },
                ));
            }
            Output::SendControl {
                peer,
                msg: ControlMessage::EndControl { session, .. },
            } => {
                if self.closed.contains(peer) {
                    self.violations.push("EndControl on closed link".into());
                }
                if self.held.get(peer).is_some_and(|h| !h.is_empty()) {
                    self.violations
                        .push(format!("EndControl with held items on {peer:?}"));
                }
                self.ended_sessions.insert(*session);
            }
            Output::BeginCapture { id, .. } => {
                if !self.hud_visible {
                    self.violations
                        .push("BeginCapture before HUD visible".into());
                }
                self.last_capture = Some(*id);
                // host executes begin immediately; the response is delivered later
                if rng.is_multiple_of(5) {
                    self.pending.push_back(Input::CaptureBegun {
                        id: *id,
                        result: Err(Failure::Other),
                    });
                } else {
                    self.capture_active = Some(*id);
                    let begun = Input::CaptureBegun {
                        id: *id,
                        result: Ok(CaptureStart {
                            held_keys: vec![],
                            lock_keys: LockKeys::default(),
                        }),
                    };
                    let started = Input::Capture(CaptureEvent::Started { id: *id });
                    if rng.is_multiple_of(2) {
                        self.pending.push_back(started);
                        self.pending.push_back(begun);
                    } else {
                        self.pending.push_back(begun);
                        self.pending.push_back(started);
                    }
                }
            }
            Output::EndCapture { .. } => {
                if let Some(id) = self.capture_active.take() {
                    self.pending.push_back(Input::Capture(CaptureEvent::Ended {
                        id,
                        reason: CaptureEnd::Requested,
                    }));
                }
            }
            Output::SendMotion { peer, .. } => {
                if self.closed.contains(peer) {
                    self.violations.push("motion on closed link".into());
                }
            }
            Output::SendInput { peer, msg } => {
                if self.closed.contains(peer) {
                    self.violations.push("SendInput on closed link".into());
                }
                let (item, down, session) = match msg {
                    InputMessage::Key {
                        usage,
                        down,
                        session,
                        ..
                    } => (Some(Held::Key(*usage)), *down, *session),
                    InputMessage::Button {
                        button,
                        down,
                        session,
                        ..
                    } => (Some(Held::Button(*button)), *down, *session),
                    InputMessage::Scroll { session, .. }
                    | InputMessage::State { session, .. }
                    | InputMessage::LockKeys { session, .. } => (None, false, *session),
                    _ => return,
                };
                if self.sessions.get(&session) != Some(peer) {
                    self.violations
                        .push("input for unknown session/peer".into());
                }
                if self.ended_sessions.contains(&session) {
                    self.violations.push("SendInput after EndControl".into());
                }
                if let Some(item) = item {
                    let h = self.held.entry(*peer).or_default();
                    if down {
                        if h.insert(item, session).is_some() {
                            self.violations.push("duplicate down".into());
                        }
                        if !self.hud_visible {
                            self.violations.push("down while HUD not visible".into());
                        }
                    } else {
                        match h.remove(&item) {
                            None => self.violations.push("stray up".into()),
                            Some(s) if s != session => {
                                self.violations.push("up in other session".into())
                            }
                            _ => {}
                        }
                    }
                }
                if let InputMessage::State {
                    held_keys,
                    held_buttons,
                    ..
                } = msg
                {
                    let listed: BTreeSet<Held> = held_keys
                        .iter()
                        .copied()
                        .map(Held::Key)
                        .chain(held_buttons.iter().copied().map(Held::Button))
                        .collect();
                    let actual: BTreeSet<Held> = self
                        .held
                        .get(peer)
                        .map(|h| h.keys().copied().collect())
                        .unwrap_or_default();
                    if listed != actual {
                        self.violations
                            .push(format!("heartbeat mismatch {listed:?} vs {actual:?}"));
                    }
                }
            }
            _ => {}
        }
    }
}

fn step(f: &mut Fixture, m: &mut Model, input: Input, rng: u8) {
    // apply inputs that change the model's view of the host
    if let Input::Overlay(OverlayEvent::Visible(_)) = &input
        && m.hud_shown
    {
        m.hud_visible = true;
    }
    if let Input::Overlay(OverlayEvent::Unavailable(_)) = &input {
        m.hud_visible = false;
    }
    let out = f.send(input);
    for o in &out {
        m.observe(o, rng);
    }
    // invariant 5: a capture that is active on the host must have a visible HUD
    if m.capture_active.is_some() && !m.hud_visible {
        m.violations
            .push("capture active without visible HUD".into());
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2_000, failure_persistence: None, ..ProptestConfig::default() })]
    #[test]
    fn interleaved_host_responses_preserve_hud_routing_and_deadline_invariants(
        events in prop::collection::vec((0u8..46, 0u8..12, any::<bool>(), 0u64..120, any::<u8>()), 1..250)
    ) {
        let mut f = Fixture::new(config(), 3);
        let mut m = Model::default();
        step(&mut f, &mut m, Input::PeerUp { peer: B }, 0);
        step(&mut f, &mut m, Input::PeerUp { peer: C }, 0);
        for (kind, n, down, elapsed, rng) in events {
            f.now = f.now.saturating_add(Duration::from_millis(elapsed));
            let rare = rng.is_multiple_of(4);
            let input = match kind {
                0..=9 => key(HidUsage::keyboard(4 + u16::from(n)), down, f.now),
                10..=12 => Input::Capture(CaptureEvent::Button { button: MouseButton(1 + n % 3), down, at: f.now }),
                13 | 14 => motion(if down { 2_000.0 } else { -2_000.0 }, 0.0, MotionKind::Unaccelerated, f.now),
                15 => motion(f64::from(n), f64::from(n) - 6.0, MotionKind::Unaccelerated, f.now),
                16..=27 => { if let Some(i) = m.pending.pop_front() { i } else { f.edge(f64::from(n) / 11.0) } }
                28 => { let d = f.engine.next_deadline(); if let Some(d) = d && d > f.now { f.now = d; } Input::Tick }
                29 if rare => Input::Link(LinkEvent::Closed { peer: if down { B } else { C }, error: LinkError::Closed }),
                30 => Input::PeerUp { peer: if down { B } else { C } },
                31 if rare => { if let Some((p, s)) = m.last_session { control(p, ControlMessage::EndControl { session: s, reason: EndReason::TargetLocked }) } else { Input::Tick } }
                32 if rare => { if let Some((p, s)) = m.last_session { control(p, ControlMessage::ControlRefused { session: s, reason: Refusal::Busy }) } else { Input::Tick } }
                33 if rare => { if let Some((p, s)) = m.last_session { discrete(p, InputMessage::Status { session: s, status: if down { TargetStatus::Refused(Refusal::Locked) } else { TargetStatus::LocalOverride } }) } else { Input::Tick } }
                34 if rare => Input::Command(match n % 3 { 0 => Command::ReleaseControl, 1 => Command::Panic, _ => Command::Rearm }),
                35 => Input::Command(Command::Rearm),
                36 if rare => Input::Hotkey(if down { HotkeyEvent::Pressed { at: f.now } } else { HotkeyEvent::Released { at: f.now } }),
                37 => f.edge(f64::from(n) / 11.0),
                38 if rare => Input::Overlay(if down { OverlayEvent::Visible(HUD) } else { OverlayEvent::Unavailable(HUD) }),
                39 if rare => match n % 3 { 0 => Input::Session(SessionEvent::State(PERMITTED)), 1 => Input::Session(SessionEvent::State(SessionState { lock: LockState::Locked, ..PERMITTED })), _ => Input::Session(SessionEvent::WillSleep) },
                40 if rare => Input::Capture(CaptureEvent::Ended { id: m.last_capture.unwrap_or(CaptureId(1)), reason: CaptureEnd::Lost }),
                41 => { if let Some((p, s)) = m.last_session { discrete(p, InputMessage::Ack { session: s, seq: u32::from(n) }) } else { Input::Tick } }
                42 if rare => Input::Session(SessionEvent::Woke),
                43 | 44 => Input::Session(SessionEvent::State(PERMITTED)),
                _ => Input::Tick,
            };
            // keep the model honest about what the platform did on events it originates
            if let Input::Capture(CaptureEvent::Ended { id, .. }) = &input && m.capture_active == Some(*id) {
                m.capture_active = None;
            }
            if let Input::Link(LinkEvent::Closed { peer, .. }) = &input {
                m.closed.insert(*peer);
                m.held.remove(peer);
            }
            if let Input::PeerUp { peer } = &input { m.closed.remove(peer); }
            if let Input::Overlay(OverlayEvent::Unavailable(_)) = &input { /* platform lost the overlay */ }
            step(&mut f, &mut m, input, rng);
            prop_assert!(m.violations.is_empty(), "{:?}", m.violations);
            // a perfect host delivers a Tick whenever the deadline is due; it must make progress
            let mut spins = 0;
            while let Some(d) = f.engine.next_deadline() {
                if d > f.now { break; }
                step(&mut f, &mut m, Input::Tick, rng);
                spins += 1;
                prop_assert!(spins < 4, "busy loop: deadline {:?} <= now {:?} after {} ticks", d, f.now, spins);
            }
            prop_assert!(m.violations.is_empty(), "{:?}", m.violations);
        }
        // drain: deliver everything, release, then let timers run
        for _ in 0..40 {
            while let Some(i) = m.pending.pop_front() {
                if let Input::Capture(CaptureEvent::Ended { id, .. }) = &i && m.capture_active == Some(*id) { m.capture_active = None; }
                step(&mut f, &mut m, i, 1);
            }
            step(&mut f, &mut m, Input::Command(Command::ReleaseControl), 1);
            if let Some(d) = f.engine.next_deadline() && d > f.now { f.now = d; }
            step(&mut f, &mut m, Input::Tick, 1);
            prop_assert!(m.violations.is_empty(), "{:?}", m.violations);
        }
        for (peer, h) in &m.held {
            prop_assert!(h.is_empty(), "left held on {:?}: {:?}", peer, h);
        }
    }
}

fn exclusive_engine_with(config: EngineConfig) -> (crosspane_engine::Engine, PortalId) {
    use crosspane_input::journal::MemoryJournal;
    let f = Fixture::new(config.clone(), 2);
    let (mut engine, _) = crosspane_engine::Engine::new(
        config,
        Box::new(MemoryJournal::default()),
        Box::new(MemoryJournal::default()),
        time(0),
    )
    .unwrap();
    for input in [
        Input::LocalDisplays(vec![display(1)]),
        Input::PeerDisplays {
            peer: B,
            displays: vec![display(1)],
        },
        Input::Layout(vec![
            Placement {
                node: A,
                display: DisplayId(1),
                origin: PointMm::zero(),
                version: 1,
            },
            Placement {
                node: B,
                display: DisplayId(1),
                origin: PointMm::new(100.0, 0.0),
                version: 1,
            },
        ]),
        Input::PeerUp { peer: B },
        Input::Session(SessionEvent::State(PERMITTED)),
        Input::Grants(BTreeMap::from([(
            B,
            BTreeSet::from([crosspane_protocol::msg::Capability::InputAccept]),
        )])),
    ] {
        engine.handle(input, time(0));
    }
    (engine, f.portal)
}

fn incoming_start() -> Input {
    control(
        B,
        ControlMessage::StartControl {
            session: SessionId(90),
            entry_display: DisplayId(1),
            entry: PointDevice::new(500.0, 500.0),
            lock_keys: LockKeys::default(),
        },
    )
}

fn exclusive_step(engine: &mut crosspane_engine::Engine, input: Input, ms: u64) -> Vec<Output> {
    exclusive_step_at(engine, input, time(ms))
}

fn exclusive_step_at(
    engine: &mut crosspane_engine::Engine,
    input: Input,
    now: MonoTime,
) -> Vec<Output> {
    let out = engine.handle(input, now);
    assert!(engine.controlling().is_none() || engine.controlled_by().is_none());
    if engine.controlled_by().is_some() {
        assert!(!out.iter().any(|o| matches!(
            o,
            Output::BeginCapture { .. }
                | Output::SendControl {
                    msg: ControlMessage::StartControl { .. },
                    ..
                }
        )));
    }
    out
}

#[test]
fn incoming_control_cancels_pending_hud_and_push_without_outgoing_capture() {
    for push_delay in [0, 100] {
        let mut cfg = config();
        cfg.push_to_cross = Duration::from_millis(push_delay);
        let (mut engine, portal) = exclusive_engine_with(cfg);
        exclusive_step(
            &mut engine,
            Input::Capture(CaptureEvent::EdgePressed {
                portal,
                position: 0.5,
                at: time(0),
            }),
            0,
        );
        let out = exclusive_step(&mut engine, incoming_start(), 1);
        assert_eq!(engine.controlled_by(), Some(B));
        if push_delay == 0 {
            assert!(out.contains(&Output::HideOverlay(HUD)));
        }
        exclusive_step(&mut engine, Input::Overlay(OverlayEvent::Visible(HUD)), 2);
        exclusive_step(&mut engine, Input::Tick, 200);
        exclusive_step(
            &mut engine,
            Input::Capture(CaptureEvent::EdgePressed {
                portal,
                position: 0.5,
                at: time(201),
            }),
            201,
        );
    }
}

#[test]
fn refused_incoming_control_does_not_cancel_a_pending_crossing() {
    // B may not control this node (no input grant): its StartControl is refused and must not
    // pre-empt this node's own crossing to B.
    let (mut engine, portal) = exclusive_engine_with(config());
    engine.handle(Input::Grants(BTreeMap::new()), time(0));
    let out = exclusive_step(
        &mut engine,
        Input::Capture(CaptureEvent::EdgePressed {
            portal,
            position: 0.5,
            at: time(0),
        }),
        0,
    );
    assert!(
        out.iter()
            .any(|o| matches!(o, Output::ShowOverlay { id, .. } if *id == HUD))
    );
    let out = exclusive_step(&mut engine, incoming_start(), 1);
    assert!(out.iter().any(|o| matches!(
        o,
        Output::SendControl {
            msg: ControlMessage::ControlRefused {
                reason: Refusal::Permission,
                ..
            },
            ..
        }
    )));
    assert!(!out.contains(&Output::HideOverlay(HUD)));
    assert_eq!(engine.controlled_by(), None);
    let out = exclusive_step(&mut engine, Input::Overlay(OverlayEvent::Visible(HUD)), 2);
    assert!(out.iter().any(|o| matches!(
        o,
        Output::SendControl {
            msg: ControlMessage::StartControl { .. },
            ..
        }
    )));
}

#[test]
fn incoming_control_cancels_sent_handshake_and_ends_late_acknowledgement() {
    let (mut engine, portal) = exclusive_engine_with(config());
    exclusive_step(
        &mut engine,
        Input::Capture(CaptureEvent::EdgePressed {
            portal,
            position: 0.5,
            at: time(0),
        }),
        0,
    );
    let session = start(&exclusive_step(
        &mut engine,
        Input::Overlay(OverlayEvent::Visible(HUD)),
        0,
    ))
    .1;
    let out = exclusive_step(&mut engine, incoming_start(), 1);
    assert_end(&out, B, session, EndReason::Released);
    let out = exclusive_step(
        &mut engine,
        control(B, ControlMessage::ControlStarted { session }),
        2,
    );
    assert_end(&out, B, session, EndReason::Released);
    exclusive_step(&mut engine, Input::Overlay(OverlayEvent::Visible(HUD)), 3);
    exclusive_step(&mut engine, Input::Tick, 200);
    assert_eq!(engine.controlling(), None);
    assert_eq!(engine.controlled_by(), Some(B));
}

#[test]
fn incoming_control_is_busy_after_outgoing_acknowledgement_and_activation() {
    let (mut engine, portal) = exclusive_engine_with(config());
    exclusive_step(
        &mut engine,
        Input::Capture(CaptureEvent::EdgePressed {
            portal,
            position: 0.5,
            at: time(0),
        }),
        0,
    );
    let session = start(&exclusive_step(
        &mut engine,
        Input::Overlay(OverlayEvent::Visible(HUD)),
        0,
    ))
    .1;
    let out = exclusive_step(
        &mut engine,
        control(B, ControlMessage::ControlStarted { session }),
        1,
    );
    let capture = out
        .iter()
        .find_map(|o| {
            if let Output::BeginCapture { id, .. } = o {
                Some(*id)
            } else {
                None
            }
        })
        .unwrap();
    for input in [
        None,
        Some(Input::CaptureBegun {
            id: capture,
            result: Ok(CaptureStart {
                held_keys: vec![],
                lock_keys: LockKeys::default(),
            }),
        }),
    ] {
        if let Some(input) = input {
            exclusive_step(&mut engine, input, 2);
        }
        let out = exclusive_step(&mut engine, incoming_start(), 3);
        assert!(out.contains(&Output::SendControl {
            peer: B,
            msg: ControlMessage::ControlRefused {
                session: SessionId(90),
                reason: Refusal::Busy
            }
        }));
        assert_eq!(engine.controlled_by(), None);
        assert_eq!(engine.controlling(), Some(B));
    }
}

#[test]
fn pointer_return_guards_only_the_return_portal_for_150_ms() {
    for (delay, crosses) in [(100, false), (200, true)] {
        let mut f = Fixture::new(config(), 2);
        let (_, capture) = f.controlling(vec![]);
        // Rearm the entry edge after WP-1.39 hysteresis before returning home.
        f.raw(1, 50.0);
        f.raw(2, -60.0);
        f.ended(3, capture);
        let edge = f.edge(0.5);
        let out = f.feed(2 + delay, edge);
        assert_eq!(
            out.iter()
                .any(|o| matches!(o, Output::ShowOverlay { id, .. } if *id == HUD)),
            crosses
        );
        let out = f.send(Input::Overlay(OverlayEvent::Visible(HUD)));
        assert_eq!(
            out.iter().any(|o| matches!(
                o,
                Output::SendControl {
                    msg: ControlMessage::StartControl { .. },
                    ..
                }
            )),
            crosses
        );
    }
    let mut f = Fixture::new(config(), 3);
    f.up(C);
    let placements = vec![
        Placement {
            node: A,
            display: DisplayId(1),
            origin: PointMm::zero(),
            version: 1,
        },
        Placement {
            node: B,
            display: DisplayId(1),
            origin: PointMm::new(100.0, 0.0),
            version: 1,
        },
        Placement {
            node: C,
            display: DisplayId(1),
            origin: PointMm::new(-100.0, 0.0),
            version: 1,
        },
    ];
    f.send(Input::Layout(placements.clone()));
    let layout = Layout::new(
        placements
            .iter()
            .map(|p| Placed {
                id: GlobalDisplayId {
                    node: p.node,
                    display: p.display,
                },
                geometry: display(1).geometry,
                origin: p.origin,
            })
            .collect(),
        config().layout,
    )
    .unwrap();
    f.portal = layout
        .portals()
        .iter()
        .find(|p| p.from.node == A && p.to.node == B)
        .unwrap()
        .id;
    let other = layout
        .portals()
        .iter()
        .find(|p| p.from.node == A && p.to.node == C)
        .unwrap()
        .id;
    let (_, capture) = f.controlling(vec![]);
    // Rearm the entry edge after WP-1.39 hysteresis before returning home.
    f.raw(1, 50.0);
    f.raw(2, -60.0);
    f.ended(3, capture);
    f.feed(
        52,
        Input::Capture(CaptureEvent::EdgePressed {
            portal: other,
            position: 0.5,
            at: time(52),
        }),
    );
    assert_eq!(
        start(&f.send(Input::Overlay(OverlayEvent::Visible(HUD)))).0,
        C
    );
}

#[test]
fn pointer_return_guard_survives_layout_portal_id_reassignment() {
    let mut f = Fixture::new(config(), 3);
    f.up(C);
    let original_id = f.portal;
    let (_, capture) = f.controlling(vec![]);
    // Rearm the entry edge after WP-1.39 hysteresis before returning home.
    f.raw(1, 50.0);
    f.raw(2, -60.0);
    f.ended(3, capture);

    // Adding a portal on A's left sorts it before the existing A -> B portal,
    // reassigning the original ID to A -> C while A -> B remains in place.
    let placements: Vec<_> = [(A, 0.0), (B, 100.0), (C, -100.0)]
        .into_iter()
        .map(|(node, x)| Placement {
            node,
            display: DisplayId(1),
            origin: PointMm::new(x, 0.0),
            version: 2,
        })
        .collect();
    let layout = Layout::new(
        placements
            .iter()
            .map(|p| Placed {
                id: GlobalDisplayId {
                    node: p.node,
                    display: p.display,
                },
                geometry: display(1).geometry,
                origin: p.origin,
            })
            .collect(),
        config().layout,
    )
    .unwrap();
    f.feed(26, Input::Layout(placements));
    let guarded = layout
        .portals()
        .iter()
        .find(|p| p.from.node == A && p.to.node == B)
        .unwrap()
        .id;
    let other = layout
        .portals()
        .iter()
        .find(|p| p.from.node == A && p.to.node == C)
        .unwrap()
        .id;
    assert_ne!(guarded, original_id);
    assert_eq!(other, original_id);
    let edge = |portal| {
        Input::Capture(CaptureEvent::EdgePressed {
            portal,
            position: 0.5,
            at: time(52),
        })
    };
    assert!(f.feed(52, edge(guarded)).is_empty());
    assert!(
        f.send(Input::Overlay(OverlayEvent::Visible(HUD)))
            .is_empty()
    );
    let out = f.send(edge(other));
    assert!(
        out.iter()
            .any(|o| matches!(o, Output::ShowOverlay { id, .. } if *id == HUD))
    );
    assert_eq!(
        start(&f.send(Input::Overlay(OverlayEvent::Visible(HUD)))).0,
        C
    );
}

// WP-2.41: portals restored under the pointer after an incoming E1 session.

fn incoming_start_as(session: u64) -> Input {
    control(
        B,
        ControlMessage::StartControl {
            session: SessionId(session),
            entry_display: DisplayId(1),
            entry: PointDevice::new(500.0, 500.0),
            lock_keys: LockKeys::default(),
        },
    )
}

fn incoming_end(session: u64) -> Input {
    control(
        B,
        ControlMessage::EndControl {
            session: SessionId(session),
            reason: EndReason::Released,
        },
    )
}

fn edge_pressed(portal: PortalId, at: MonoTime) -> Input {
    Input::Capture(CaptureEvent::EdgePressed {
        portal,
        position: 0.5,
        at,
    })
}

fn edge_released(portal: PortalId, at: MonoTime) -> Input {
    Input::Capture(CaptureEvent::EdgeReleased { portal, at })
}

fn shows_hud(out: &[Output]) -> bool {
    out.iter()
        .any(|o| matches!(o, Output::ShowOverlay { id, .. } if *id == HUD))
}

fn sends_start(out: &[Output]) -> bool {
    out.iter().any(|o| {
        matches!(
            o,
            Output::SendControl {
                msg: ControlMessage::StartControl { .. },
                ..
            }
        )
    })
}

// An engine and the time of its latest input: delivery to the engine never goes backwards.
struct Host {
    engine: crosspane_engine::Engine,
    last: MonoTime,
}

impl std::ops::Deref for Host {
    type Target = crosspane_engine::Engine;
    fn deref(&self) -> &Self::Target {
        &self.engine
    }
}

impl Host {
    // This node (A) with B up and allowed to control it; returns the A -> B portal.
    fn with(config: EngineConfig) -> (Host, PortalId) {
        let (engine, portal) = exclusive_engine_with(config);
        let host = Host {
            engine,
            last: time(0),
        };
        (host, portal)
    }

    // Deliver `input` at `at`, which is never earlier than the previous delivery.
    fn step(&mut self, input: Input, at: MonoTime) -> Vec<Output> {
        assert!(
            at >= self.last,
            "time went backwards: {at:?} after {:?}",
            self.last
        );
        self.last = at;
        exclusive_step_at(&mut self.engine, input, at)
    }
}

// B starts controlling this node: it becomes a target and its portals are removed.
fn begin_incoming(host: &mut Host, session: u64, at: MonoTime) {
    host.step(incoming_start_as(session), at);
    assert_eq!(host.controlled_by(), Some(B));
}

// B ends the session (as after a refused capture). Returns the outputs of the end, which restore
// the portals.
fn finish_incoming(host: &mut Host, session: u64, at: MonoTime) -> Vec<Output> {
    let out = host.step(incoming_end(session), at);
    assert_eq!(host.controlled_by(), None);
    out
}

// A whole session from `start_ms` to `end_ms`, with nothing in between.
fn incoming_session(host: &mut Host, session: u64, start_ms: u64, end_ms: u64) -> Vec<Output> {
    assert!(start_ms < end_ms);
    begin_incoming(host, session, time(start_ms));
    finish_incoming(host, session, time(end_ms))
}

fn restored(out: &[Output]) -> Vec<CapturePortal> {
    out.iter()
        .rev()
        .find_map(|o| match o {
            Output::SetPortals(portals) => Some(portals.clone()),
            _ => None,
        })
        .expect("SetPortals")
}

// A press against a restored portal changes nothing at all: no push, HUD or handshake.
fn assert_ignored(host: &mut Host, portal: PortalId, at: MonoTime) {
    let out = host.step(edge_pressed(portal, at), at);
    assert!(out.is_empty(), "press at {at:?} was not ignored: {out:?}");
    assert_eq!(host.controlling(), None);
}

// A deliberate press: the HUD appears, and its confirmation sends the handshake to B.
fn assert_crosses(host: &mut Host, portal: PortalId, at: MonoTime) {
    let out = host.step(edge_pressed(portal, at), at);
    assert!(shows_hud(&out), "press at {at:?} did not cross: {out:?}");
    let out = host.step(Input::Overlay(OverlayEvent::Visible(HUD)), at);
    assert!(sends_start(&out), "no handshake after the HUD: {out:?}");
}

#[test]
fn restored_portal_ignores_presses_until_released_then_crosses_normally() {
    for push_delay in [0, 100] {
        let mut cfg = config();
        cfg.push_to_cross = Duration::from_millis(push_delay);
        let (mut host, portal) = Host::with(cfg);
        let out = incoming_session(&mut host, 90, 1, 5);
        let restored = restored(&out);
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].id, portal);
        // The injected pointer still rests at the entry edge: every press repeats, none crosses,
        // however long the push delay has been satisfied.
        for ms in [6, 106, 206, 500, 900] {
            assert_ignored(&mut host, portal, time(ms));
        }
        // With no HUD requested there is nothing for a late confirmation to continue.
        assert!(
            host.step(Input::Overlay(OverlayEvent::Visible(HUD)), time(901))
                .is_empty()
        );
        assert!(!sends_start(&host.step(Input::Tick, time(950))));
        // The first release re-arms the portal; a fresh press crosses normally.
        assert!(
            host.step(edge_released(portal, time(960)), time(960))
                .is_empty()
        );
        if push_delay > 0 {
            // The push-to-cross delay counts from the fresh press, as before.
            let out = host.step(edge_pressed(portal, time(970)), time(970));
            assert!(!shows_hud(&out) && !sends_start(&out), "{out:?}");
            assert_crosses(&mut host, portal, time(970 + push_delay));
        } else {
            assert_crosses(&mut host, portal, time(970));
        }
        // The handshake to B is under way.
        assert_eq!(host.controlling(), Some(B));
    }
}

#[test]
fn restored_portal_rearms_after_the_fallback_without_a_release() {
    let (mut host, portal) = Host::with(config());
    incoming_session(&mut host, 90, 1, 5);
    let rearm = time(5).saturating_add(REARM_FALLBACK);
    assert_ignored(&mut host, portal, time(6));
    assert_ignored(&mut host, portal, time(500));
    // Ignored presses neither extend nor shorten the window.
    assert_ignored(
        &mut host,
        portal,
        MonoTime::from_nanos(rearm.as_nanos() - 1),
    );
    assert_crosses(&mut host, portal, rearm);
}

#[test]
fn failed_incoming_sessions_never_start_a_reverse_crossing_until_a_release() {
    // Codex trace (finding 3): the peer starts control, its capture is refused, it ends the
    // session, and the restored edge keeps reporting presses under the stationary pointer.
    let (mut host, portal) = Host::with(config());
    // Delivery times only ever increase: each session starts 300 ms after the previous one.
    for (index, start) in [10u64, 310, 610].into_iter().enumerate() {
        let session = 90 + index as u64;
        // The peer starts control: this node becomes its target and its portals are removed.
        begin_incoming(&mut host, session, time(start));
        // A press while controlled is dropped by the engine, as the portals are gone.
        let out = host.step(edge_pressed(portal, time(start + 20)), time(start + 20));
        assert!(out.is_empty(), "session {session}: {out:?}");
        assert_eq!(host.controlled_by(), Some(B));
        // The peer's capture is refused and it ends the session: the portals are restored.
        let out = finish_incoming(&mut host, session, time(start + 50));
        assert_eq!(restored(&out).len(), 1);
        // The restored edge keeps reporting presses under the stationary pointer.
        for ms in [start + 51, start + 150] {
            assert_ignored(&mut host, portal, time(ms));
        }
        let out = host.step(Input::Tick, time(start + 200));
        assert!(!sends_start(&out), "session {session}");
    }
    // The fallback counts from the last restore (660 ms): by now the first session's (1060 ms)
    // and the second's (1360 ms) have passed, but this one's (1660 ms) hasn't.
    for ms in [811, 1100, 1500] {
        assert_ignored(&mut host, portal, time(ms));
    }
    // The first release re-arms the portal, well before the fallback.
    host.step(edge_released(portal, time(1501)), time(1501));
    assert_crosses(&mut host, portal, time(1502));
}

#[test]
fn portals_that_appear_after_the_restore_are_not_disarmed() {
    let (mut host, portal) = Host::with(config());
    begin_incoming(&mut host, 90, time(1));
    // The link drops during the session: B's portal isn't among those restored.
    host.step(
        Input::Link(LinkEvent::Closed {
            peer: B,
            error: LinkError::Closed,
        }),
        time(2),
    );
    assert_eq!(host.controlled_by(), None);
    // B reconnects: its portal is new, so a press crosses at once.
    host.step(Input::PeerUp { peer: B }, time(3));
    assert_crosses(&mut host, portal, time(4));
}

// A joins C on its left within the fallback window: sorting hands A -> B's old portal ID to
// A -> C. Returns the engine (restored at 5 ms), A -> B's new portal and A -> C's.
fn restored_then_joined_by_c() -> (Host, PortalId, PortalId) {
    let (mut host, original) = Host::with(config());
    incoming_session(&mut host, 90, 1, 5);
    let placements = placements(&[(A, 0.0, 0.0), (B, 100.0, 0.0), (C, -100.0, 0.0)], 2);
    let layout = layout_from(&placements);
    let (a_to_b, a_to_c) = (portal_to(&layout, B), portal_to(&layout, C));
    assert_eq!(a_to_c, original, "premise: the old ID now names A -> C");
    assert_ne!(a_to_b, original);
    host.step(
        Input::PeerDisplays {
            peer: C,
            displays: vec![display(1)],
        },
        time(6),
    );
    host.step(Input::Layout(placements), time(7));
    host.step(Input::PeerUp { peer: C }, time(8));
    (host, a_to_b, a_to_c)
}

// One display per node at the given positions (mm), e.g. C at (-100, 0) is left of A.
fn placements(nodes: &[(NodeId, f64, f64)], version: u64) -> Vec<Placement> {
    nodes
        .iter()
        .map(|&(node, x, y)| Placement {
            node,
            display: DisplayId(1),
            origin: PointMm::new(x, y),
            version,
        })
        .collect()
}

fn layout_from(placements: &[Placement]) -> Layout {
    Layout::new(
        placements
            .iter()
            .map(|p| Placed {
                id: GlobalDisplayId {
                    node: p.node,
                    display: p.display,
                },
                geometry: display(1).geometry,
                origin: p.origin,
            })
            .collect(),
        config().layout,
    )
    .unwrap()
}

// The ID of A's portal to `node`'s display in `layout`.
fn portal_to(layout: &Layout, node: NodeId) -> PortalId {
    layout
        .portals()
        .iter()
        .find(|p| p.from.node == A && p.to.node == node)
        .unwrap()
        .id
}

#[test]
fn layout_change_keeps_only_the_same_restored_portal_disarmed() {
    let (mut host, a_to_b, _) = restored_then_joined_by_c();
    // The portal that was restored stays disarmed under its new ID, until its own release.
    assert_ignored(&mut host, a_to_b, time(9));
    assert_ignored(&mut host, a_to_b, time(500));
    host.step(edge_released(a_to_b, time(501)), time(501));
    assert_crosses(&mut host, a_to_b, time(502));
}

#[test]
fn a_release_re_arms_only_its_own_portal_and_unrelated_portals_cross() {
    let (mut host, a_to_b, a_to_c) = restored_then_joined_by_c();
    // C wasn't restored (it joined afterwards): unaffected by the disarm, and its release is
    // not A -> B's.
    host.step(edge_released(a_to_c, time(9)), time(9));
    assert_ignored(&mut host, a_to_b, time(10));
    assert_crosses(&mut host, a_to_c, time(11));
}

#[test]
fn a_restored_portal_that_goes_away_is_armed_when_it_returns() {
    let (mut host, portal) = Host::with(config());
    incoming_session(&mut host, 90, 1, 5);
    let b_at = |x: f64, version| Input::Layout(placements(&[(A, 0.0, 0.0), (B, x, 0.0)], version));
    // Moving B away removes the shared edge, then putting it back restores the portal: only
    // portals still offered keep a disarm, so this one is new again.
    let out = host.step(b_at(300.0, 2), time(6));
    assert_eq!(restored(&out), vec![]);
    let out = host.step(b_at(100.0, 3), time(7));
    assert_eq!(restored(&out)[0].id, portal);
    assert_crosses(&mut host, portal, time(8));
}

// The incident ordering: the session start removed the portals, so the Linux capture emitted an
// `EdgeReleased` stamped at the start (11 ms). The refused session was over (restore at 60 ms)
// before that event reached the engine (62 ms).
fn restored_with_a_stale_release_in_flight() -> (Host, PortalId) {
    let (mut host, portal) = Host::with(config());
    incoming_session(&mut host, 90, 10, 60);
    let out = host.step(edge_released(portal, time(11)), time(62));
    assert!(out.is_empty());
    (host, portal)
}

#[test]
fn a_release_stamped_before_the_restore_does_not_re_arm_but_a_later_one_does() {
    let (mut host, portal) = restored_with_a_stale_release_in_flight();
    // The stale release left the portal disarmed: the restored strip's presses still don't cross.
    assert_ignored(&mut host, portal, time(63));
    // Nor does one stamped an instant before the restore, however late it is delivered.
    let just_before = MonoTime::from_nanos(time(60).as_nanos() - 1);
    let out = host.step(edge_released(portal, just_before), time(64));
    assert!(out.is_empty());
    assert_ignored(&mut host, portal, time(65));
    // The pointer really leaving the restored strip re-arms it.
    let out = host.step(edge_released(portal, time(70)), time(70));
    assert!(out.is_empty());
    assert_crosses(&mut host, portal, time(71));
}

#[test]
fn a_release_stamped_at_the_restore_instant_re_arms() {
    let (mut host, portal) = Host::with(config());
    incoming_session(&mut host, 90, 10, 60);
    // Not earlier than the restore, so not stale.
    host.step(edge_released(portal, time(60)), time(62));
    assert_crosses(&mut host, portal, time(63));
}

#[test]
fn a_stale_release_leaves_the_fallback_as_the_backstop() {
    let (mut host, portal) = restored_with_a_stale_release_in_flight();
    let rearm = time(60).saturating_add(REARM_FALLBACK);
    assert_ignored(&mut host, portal, time(63));
    assert_ignored(
        &mut host,
        portal,
        MonoTime::from_nanos(rearm.as_nanos() - 1),
    );
    assert_crosses(&mut host, portal, rearm);
}

// C on A's left, B on its right and D above it: A's portals sort Left, Right, Top, so their IDs
// are 1 (A -> C), 2 (A -> B) and 3 (A -> D). All three are restored at 5 ms.
struct Around {
    host: Host,
    // The layout after C goes away, and A -> B's and A -> D's portals in it.
    without_c: Vec<Placement>,
    b_after: PortalId,
    d_after: PortalId,
    // A -> B's portal before C went away (ID 2).
    b_before: PortalId,
}

fn restored_around_a() -> Around {
    let d = NodeId([4; 32]);
    let with_c = placements(
        &[
            (A, 0.0, 0.0),
            (B, 100.0, 0.0),
            (C, -100.0, 0.0),
            (d, 0.0, -100.0),
        ],
        2,
    );
    let without_c = placements(&[(A, 0.0, 0.0), (B, 100.0, 0.0), (d, 0.0, -100.0)], 3);
    let (before, after) = (layout_from(&with_c), layout_from(&without_c));
    let (b_before, b_after, d_after) = (
        portal_to(&before, B),
        portal_to(&after, B),
        portal_to(&after, d),
    );
    // Premise: with C gone, A -> D shifts into the ID A -> B had.
    assert_eq!(d_after, b_before);
    assert_ne!(b_after, b_before);

    let (mut host, _) = Host::with(config());
    for peer in [C, d] {
        host.step(
            Input::PeerDisplays {
                peer,
                displays: vec![display(1)],
            },
            time(0),
        );
    }
    host.step(Input::Layout(with_c), time(0));
    for peer in [C, d] {
        host.step(Input::PeerUp { peer }, time(0));
    }
    let out = incoming_session(&mut host, 90, 1, 5);
    assert_eq!(restored(&out).len(), 3);
    Around {
        host,
        without_c,
        b_after,
        d_after,
        b_before,
    }
}

#[test]
fn a_queued_release_cannot_re_arm_another_portal_after_an_id_reassignment() {
    let Around {
        mut host,
        without_c,
        b_after,
        d_after,
        b_before,
    } = restored_around_a();
    // The platform stamped a release for A -> B (ID 2) at 9 ms; it is still queued.
    let queued = edge_released(b_before, time(9));
    // At 10 ms C goes away. An earlier portal is removed, so A -> D shifts into ID 2.
    host.step(Input::Layout(without_c), time(10));
    // Delivered at 11 ms, ID 2 names A -> D, which is not the portal the platform meant: the
    // release is ambiguous and re-arms nothing.
    assert!(host.step(queued, time(11)).is_empty());
    assert_ignored(&mut host, d_after, time(12));
    assert_ignored(&mut host, b_after, time(12));
    // A release stamped after the change is unambiguous and re-arms its own portal only.
    assert!(
        host.step(edge_released(d_after, time(13)), time(13))
            .is_empty()
    );
    assert_ignored(&mut host, b_after, time(14));
    assert_crosses(&mut host, d_after, time(15));
}

#[test]
fn a_mapping_change_leaves_each_entry_its_original_fallback_deadline() {
    let Around {
        mut host,
        without_c,
        b_after,
        d_after,
        ..
    } = restored_around_a();
    host.step(Input::Layout(without_c), time(10));
    // Neither the restore time (5 ms) nor the mapping change (10 ms) moves the 1 s deadline.
    let rearm = time(5).saturating_add(REARM_FALLBACK);
    assert_ignored(&mut host, b_after, time(500));
    assert_ignored(
        &mut host,
        d_after,
        MonoTime::from_nanos(rearm.as_nanos() - 1),
    );
    assert_crosses(&mut host, d_after, rearm);
}

#[test]
fn a_rebuild_that_moves_no_portal_does_not_make_a_queued_release_ambiguous() {
    let Around {
        mut host, b_before, ..
    } = restored_around_a();
    let queued = edge_released(b_before, time(9));
    // An unrelated display refresh at 10 ms rebuilds the layout with every portal where it was.
    host.step(
        Input::PeerDisplays {
            peer: B,
            displays: vec![display(1)],
        },
        time(10),
    );
    assert!(host.step(queued, time(11)).is_empty());
    assert_crosses(&mut host, b_before, time(12));
}
