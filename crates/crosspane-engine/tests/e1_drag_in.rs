#![allow(clippy::unwrap_used, clippy::expect_used)]

use crosspane_engine::{
    Command, Engine, EngineConfig, InjectCmd, Input, Notice, Output, ProxyEvent,
};
use crosspane_input::journal::MemoryJournal;
use crosspane_platform::{
    CaptureEvent, CaptureId, CaptureStart, EndReason, LockState, MotionKind, OverlayEvent, Parked,
    ParkingKind, SessionEvent, SessionState, StreamId, WindowEvent, WindowInfo, WindowRole,
    WindowState,
};
use crosspane_protocol::link::{LinkError, LinkEvent};
use crosspane_protocol::msg::{
    Capability, ControlMessage, InputMessage, Placement, Refusal, TargetStatus,
};
use crosspane_protocol::projection::{
    ProjectionEndReason, ProjectionMessage as Message, ProxyPlacement,
};
use crosspane_testkit::{DragInOrder, DragSeat};
use crosspane_types::color::ColorSpace;
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{
    DisplayGeometry, PixelRect, PixelSize, PointDevice, PointLogical, PointMm, RectLogical,
    SizeLogical, SizeMm,
};
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::id::{DisplayId, NodeId, WindowId};
use crosspane_types::input::LockKeys;
use crosspane_types::time::MonoTime;
use proptest::prelude::*;
use std::collections::VecDeque;

const A: NodeId = NodeId([1; 32]);
const B: NodeId = NodeId([2; 32]);
const OPEN: SessionState = SessionState {
    lock: LockState::Unlocked,
    active: Some(true),
};
fn ms(n: u64) -> MonoTime {
    MonoTime::from_nanos(n * 1_000_000)
}
fn display() -> DisplayInfo {
    DisplayInfo {
        id: DisplayId(1),
        name: "pure drag fixture".into(),
        geometry: DisplayGeometry {
            physical_size: SizeMm::new(100.0, 100.0),
            pixel_size: PixelSize::new(1000, 1000),
            scale: 1.0,
            logical_origin: PointLogical::zero(),
        },
        refresh_millihz: 60000,
        color_space: ColorSpace::Srgb,
        hdr: false,
    }
}
fn window(id: u64) -> WindowInfo {
    WindowInfo {
        id: WindowId(id),
        title: "authored fixture".into(),
        app_id: "fake".into(),
        pid: None,
        display: Some(DisplayId(1)),
        frame: RectLogical::new(PointLogical::zero(), SizeLogical::new(320.0, 200.0)),
        state: WindowState::Normal,
        role: WindowRole::Toplevel,
        parent: None,
    }
}
struct Rig {
    engines: [Engine; 2],
    now: u64,
    portal: crosspane_platform::PortalId,
    capture: [Option<CaptureId>; 2],
    trace: Vec<(usize, Output)>,
    seat: DragSeat,
    order: DragInOrder,
    reorder: bool,
    late: Vec<(usize, Input)>,
}
impl Rig {
    fn new() -> Self {
        let engines = [A, B].map(|node| {
            let mut config = EngineConfig::new(node);
            config.drag_across = true;
            config.accel.base_mm_per_unit = 0.1;
            config.accel.max_gain = 1.0;
            Engine::new(
                config,
                Box::<MemoryJournal>::default(),
                Box::<MemoryJournal>::default(),
                ms(0),
            )
            .unwrap()
            .0
        });
        let mut rig = Self {
            engines,
            now: 0,
            portal: crosspane_platform::PortalId(0),
            capture: [None; 2],
            trace: Vec::new(),
            seat: DragSeat::default(),
            order: DragInOrder::default(),
            reorder: false,
            late: Vec::new(),
        };
        for index in 0..2 {
            let peer = [B, A][index];
            rig.feed(index, Input::Session(SessionEvent::State(OPEN)));
            rig.feed(
                index,
                Input::Grants(
                    [(
                        peer,
                        [
                            Capability::InputAccept,
                            Capability::WindowBrowse,
                            Capability::WindowShare,
                            Capability::WindowPresent,
                        ]
                        .into(),
                    )]
                    .into(),
                ),
            );
            rig.feed(index, Input::LocalDisplays(vec![display()]));
            rig.feed(
                index,
                Input::PeerDisplays {
                    peer,
                    displays: vec![display()],
                },
            );
            rig.feed(index, Input::PeerUp { peer });
            rig.feed(
                index,
                Input::DragInPeer {
                    peer,
                    available: true,
                },
            );
            rig.feed(
                index,
                Input::Windows(WindowEvent::Added(window(if index == 0 { 10 } else { 20 }))),
            );
            rig.feed(
                index,
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
            );
        }
        rig.portal = rig
            .trace
            .iter()
            .rev()
            .find_map(|(index, output)| match output {
                Output::SetPortals(portals) if *index == 0 => {
                    portals.first().map(|portal| portal.id)
                }
                _ => None,
            })
            .unwrap();
        rig.trace.clear();
        rig
    }
    fn feed(&mut self, index: usize, input: Input) {
        let outputs = self.engines[index].handle(input, ms(self.now));
        let mut queue: VecDeque<_> = outputs.into_iter().map(|output| (index, output)).collect();
        while let Some((index, output)) = queue.pop_front() {
            let node = [A, B][index];
            let other = 1 - index;
            self.trace.push((index, output.clone()));
            let mut callbacks = Vec::new();
            match output {
                Output::SetPortals(portals) => callbacks.push((
                    index,
                    Input::PortalsSet {
                        ids: portals.iter().map(|p| p.id).collect(),
                        result: Ok(()),
                    },
                )),
                Output::ShowOverlay { id, .. } => {
                    callbacks.push((index, Input::Overlay(OverlayEvent::Visible(id))))
                }
                Output::SendControl { peer, msg } if peer == [A, B][other] => {
                    if index == 0
                        && let ControlMessage::Projection(Message::PullAt { place, .. }) = &msg
                    {
                        self.order.drop_under_pointer(place.drag);
                    }
                    let input = (
                        other,
                        Input::Link(LinkEvent::Control {
                            peer: node,
                            msg: msg.clone(),
                        }),
                    );
                    if index == 0
                        && self.reorder
                        && matches!(msg, ControlMessage::EndControl { .. })
                    {
                        self.late.push(input);
                    } else {
                        callbacks.push(input);
                    }
                }
                Output::SendInput { peer, msg } if peer == [A, B][other] => {
                    if index == 0
                        && let InputMessage::Button {
                            button: MouseButton::PRIMARY,
                            down,
                            ..
                        } = &msg
                    {
                        self.order.primary(*down);
                    }
                    let input = (
                        other,
                        Input::Link(LinkEvent::Input {
                            peer: node,
                            msg: msg.clone(),
                        }),
                    );
                    if index == 0
                        && self.reorder
                        && matches!(
                            msg,
                            InputMessage::Button {
                                button: MouseButton::PRIMARY,
                                down: false,
                                ..
                            }
                        )
                    {
                        self.late.push(input);
                    } else {
                        callbacks.push(input);
                    }
                }
                Output::SendMotion { peer, msg } if peer == [A, B][other] => {
                    callbacks.push((other, Input::Link(LinkEvent::Motion { peer: node, msg })))
                }
                Output::BeginCapture { id, .. } => {
                    self.capture[index] = Some(id);
                    self.seat.ordinary_capture();
                    callbacks.push((index, Input::Capture(CaptureEvent::Started { id })));
                    callbacks.push((
                        index,
                        Input::CaptureBegun {
                            id,
                            result: Ok(CaptureStart {
                                held_keys: Vec::new(),
                                lock_keys: LockKeys::default(),
                            }),
                        },
                    ));
                }
                Output::EndCapture { .. } => {
                    self.seat.end_capture();
                    if let Some(id) = self.capture[index].take() {
                        callbacks.push((
                            index,
                            Input::Capture(CaptureEvent::Ended {
                                id,
                                reason: EndReason::Requested,
                            }),
                        ));
                    }
                }
                Output::OpenProxy { key, size, .. } => {
                    callbacks.push((
                        index,
                        Input::ProxyOpened {
                            key,
                            result: Ok((size, 1.0)),
                        },
                    ));
                    callbacks.push((
                        index,
                        Input::ProxyWindow {
                            key,
                            window: WindowId(100 + key.projection.0),
                        },
                    ));
                }
                Output::ProxyGeometry { key, size, .. } => callbacks.push((
                    index,
                    Input::Proxy {
                        key,
                        event: ProxyEvent::Resized { size, scale: 1.0 },
                    },
                )),
                Output::Park { window, size, .. } | Output::ResizeParked { window, size, .. } => {
                    callbacks.push((
                        index,
                        Input::Parked {
                            window,
                            result: Ok(Parked {
                                window,
                                kind: ParkingKind::Mirror,
                                fullscreen: false,
                                display: DisplayId(1),
                                content: PixelRect::new(
                                    (0, 0).into(),
                                    (size.width as i32, size.height as i32).into(),
                                ),
                            }),
                        },
                    ))
                }
                Output::Restore {
                    place: Some(place), ..
                } if index == 0 => self.order.drop_under_pointer(place.drag),
                Output::StartCapture { projection, .. } => callbacks.push((
                    index,
                    Input::CaptureStarted {
                        projection,
                        result: Ok(StreamId(projection.0)),
                    },
                )),
                Output::Inject { id, cmd } => {
                    match cmd {
                        InjectCmd::Button {
                            button: MouseButton::PRIMARY,
                            down: true,
                        } => self.seat.down(node),
                        InjectCmd::Button {
                            button: MouseButton::PRIMARY,
                            down: false,
                        } => self.seat.up(node),
                        InjectCmd::ReleaseAll => self.seat.release_all(node),
                        _ => {}
                    }
                    callbacks.push((index, Input::InjectDone { id, ok: true }));
                }
                _ => {}
            }
            for (index, input) in callbacks {
                queue.extend(
                    self.engines[index]
                        .handle(input, ms(self.now))
                        .into_iter()
                        .map(|output| (index, output)),
                );
            }
        }
    }
    fn control(&mut self) {
        for offset in [0, 250] {
            self.now += offset;
            self.feed(
                0,
                Input::Capture(CaptureEvent::EdgePressed {
                    portal: self.portal,
                    position: 0.5,
                    at: ms(self.now),
                }),
            );
        }
        assert_eq!(self.engines[0].control_established(), Some(B));
        self.motion(200.0); // Leave the remote entry hysteresis before crossing back.
    }
    fn motion(&mut self, dx: f64) {
        self.feed(
            0,
            Input::Capture(CaptureEvent::Motion {
                dx,
                dy: 0.0,
                kind: MotionKind::Accelerated {
                    display: DisplayId(1),
                },
                at: ms(self.now),
            }),
        );
    }
    fn primary(&mut self, down: bool) {
        if down {
            self.seat.captured_down();
        } else {
            let _ = self.seat.physical_up();
        }
        self.feed(
            0,
            Input::Capture(CaptureEvent::Button {
                button: MouseButton::PRIMARY,
                down,
                at: ms(self.now),
            }),
        );
    }
    fn fact(&mut self, window: u64) {
        self.feed(
            1,
            Input::Capture(CaptureEvent::NativeMove {
                window: WindowId(window),
                grab: PointDevice::new(10.0, 20.0),
                size: PixelSize::new(320, 200),
                at: ms(self.now),
            }),
        );
    }
    fn cross(&mut self) {
        self.motion(-250.0);
    }
    fn age(&mut self, elapsed: u64) {
        let until = self.now + elapsed;
        while self.now < until {
            self.now = (self.now + 20).min(until);
            for index in 0..2 {
                self.feed(index, Input::Tick);
            }
        }
    }
    fn flush_late(&mut self) {
        for (index, input) in std::mem::take(&mut self.late) {
            self.feed(index, input);
        }
    }
    fn drops(&self) -> usize {
        self.trace
            .iter()
            .filter(|(index, output)| {
                *index == 0
                    && matches!(
                        output,
                        Output::SendControl {
                            msg: ControlMessage::Projection(Message::PullAt { .. }),
                            ..
                        } | Output::Restore { place: Some(_), .. }
                    )
            })
            .count()
    }
    fn cleanup(&mut self) {
        self.feed(0, Input::Command(Command::ReleaseControl));
        self.flush_late();
        if self.seat.physical {
            self.primary(false);
        }
    }
}

#[test]
fn peer_window_drops_after_reliable_up_and_uses_real_pull_flow() {
    let mut rig = Rig::new();
    rig.control();
    rig.primary(true);
    rig.fact(20);
    rig.cross();
    assert_eq!(rig.drops(), 1);
    assert_eq!((rig.order.downs, rig.order.ups), (1, 1));
    let place = rig
        .trace
        .iter()
        .find_map(|(index, output)| match output {
            Output::OpenProxy {
                place: Some(place), ..
            } if *index == 0 => Some(*place),
            _ => None,
        })
        .unwrap();
    assert!(!place.drag);
    assert!(rig.trace.iter().any(|(index, output)| *index == 1
        && matches!(
            output,
            Output::Park {
                window: WindowId(20),
                ..
            }
        )));
    assert!(!rig.trace.iter().any(|(_, output)| matches!(
        output,
        Output::ArmDrag { .. }
            | Output::BeginDrag { .. }
            | Output::SendInput {
                msg: InputMessage::PressAt { .. },
                ..
            }
    )));
    rig.primary(false);
    assert_eq!(rig.order.ups, 1);
    assert_eq!(rig.seat.totals(B), (1, 1));
}

#[test]
fn seat_window_proxy_returns_home_after_up_and_closes_returned() {
    let mut rig = Rig::new();
    rig.feed(
        0,
        Input::Command(Command::Project {
            window: WindowId(10),
            to: B,
            place: None,
        }),
    );
    let key = rig
        .trace
        .iter()
        .find_map(|(index, output)| match output {
            Output::OpenProxy { key, .. } if *index == 1 => Some(*key),
            _ => None,
        })
        .unwrap();
    rig.control();
    rig.primary(true);
    rig.fact(100 + key.projection.0);
    rig.cross();
    assert_eq!(rig.drops(), 1);
    assert_eq!(rig.order.ups, 1);
    assert!(rig.trace.iter().any(|(index, output)| *index == 0
        && matches!(
            output,
            Output::Restore {
                window: WindowId(10),
                place: Some(ProxyPlacement { drag: false, .. })
            }
        )));
    assert!(rig.trace.iter().any(|(index, output)| *index == 1 && matches!(output, Output::Notice(Notice::ProjectionEnded { key: ended, reason: ProjectionEndReason::Returned }) if *ended == key)));
    rig.primary(false);
    assert_eq!(rig.order.ups, 1);
    assert_eq!(rig.seat.totals(B), (1, 1));
}

#[test]
fn no_fact_preserves_the_existing_button_held_crossing_cancel() {
    let mut rig = Rig::new();
    rig.control();
    rig.primary(true);
    rig.cross();
    assert_eq!(rig.drops(), 0);
    assert_eq!(rig.engines[0].control_established(), Some(B));
    rig.cleanup();
    assert_eq!(rig.seat.totals(B), (1, 1));
}

#[test]
fn pull_before_cross_stream_up_releases_target_once_before_park() {
    let mut rig = Rig::new();
    rig.control();
    rig.primary(true);
    rig.fact(20);
    rig.reorder = true;
    rig.cross();
    let up = rig
        .trace
        .iter()
        .position(|(index, output)| {
            *index == 1
                && matches!(
                    output,
                    Output::Inject {
                        cmd: InjectCmd::Button {
                            button: MouseButton::PRIMARY,
                            down: false
                        },
                        ..
                    }
                )
        })
        .unwrap();
    let park = rig
        .trace
        .iter()
        .position(|(index, output)| *index == 1 && matches!(output, Output::Park { .. }))
        .unwrap();
    assert!(up < park);
    assert_eq!(rig.seat.totals(B), (1, 1));
    rig.flush_late();
    rig.primary(false);
    assert_eq!(rig.seat.totals(B), (1, 1));
}

#[test]
fn automatic_refusal_is_one_existing_notice_not_an_app_browse_result() {
    let mut rig = Rig::new();
    rig.control();
    rig.primary(true);
    rig.fact(20);
    rig.feed(
        1,
        Input::Grants([(A, [Capability::InputAccept, Capability::WindowShare].into())].into()),
    );
    rig.cross();
    assert_eq!(rig.drops(), 1);
    assert_eq!(
        rig.trace
            .iter()
            .filter(|(index, output)| *index == 0
                && matches!(
                    output,
                    Output::Notice(Notice::ProjectionRefused {
                        peer: B,
                        reason: Refusal::Permission
                    })
                ))
            .count(),
        1
    );
    assert!(
        !rig.trace
            .iter()
            .any(|(index, output)| *index == 0 && matches!(output, Output::BrowseResult { .. }))
    );
    assert!(
        !rig.trace
            .iter()
            .any(|(_, output)| matches!(output, Output::Park { .. }))
    );
    rig.primary(false);
    assert_eq!(rig.seat.totals(B), (1, 1));
}

#[test]
fn target_fact_requires_control_negotiation_and_primary_lease_and_is_rate_limited() {
    let mut rig = Rig::new();
    rig.fact(20);
    rig.control();
    rig.fact(20);
    let moves = |rig: &Rig| {
        rig.trace
            .iter()
            .filter(|(index, output)| {
                *index == 1
                    && matches!(
                        output,
                        Output::SendInput {
                            msg: InputMessage::Status {
                                status: TargetStatus::NativeMove { .. },
                                ..
                            },
                            ..
                        }
                    )
            })
            .count()
    };
    assert_eq!(moves(&rig), 0);
    rig.primary(true);
    rig.feed(
        1,
        Input::DragInPeer {
            peer: A,
            available: false,
        },
    );
    rig.fact(20);
    assert_eq!(moves(&rig), 0);
    rig.feed(
        1,
        Input::DragInPeer {
            peer: A,
            available: true,
        },
    );
    rig.fact(20);
    assert_eq!(moves(&rig), 1);
    rig.age(10);
    rig.fact(20);
    assert_eq!(moves(&rig), 1);
    rig.age(10);
    rig.fact(20);
    assert_eq!(moves(&rig), 2);
    rig.feed(
        1,
        Input::Capture(CaptureEvent::NativeMove {
            window: WindowId(21),
            grab: PointDevice::new(11.0, 20.0),
            size: PixelSize::new(320, 200),
            at: ms(rig.now),
        }),
    );
    assert_eq!(moves(&rig), 3);
    assert_eq!(
        rig.trace
            .iter()
            .filter(|(_, output)| matches!(
                output,
                Output::SendInput {
                    msg: InputMessage::Status {
                        status: TargetStatus::NativeMoveEnded {
                            window: WindowId(20)
                        },
                        ..
                    },
                    ..
                }
            ))
            .count(),
        1
    );
    rig.cleanup();
    let previous = rig.trace.len();
    rig.fact(21);
    assert!(!rig.trace[previous..].iter().any(|(_, output)| matches!(
        output,
        Output::SendInput {
            msg: InputMessage::Status {
                status: TargetStatus::NativeMoveEnded { .. } | TargetStatus::NativeMove { .. },
                ..
            },
            ..
        }
    )));
}

#[test]
fn drag_in_negotiation_is_independent_of_drag_out() {
    let mut rig = Rig::new();
    rig.control();
    rig.primary(true);
    for (index, peer) in [(0, B), (1, A)] {
        rig.feed(
            index,
            Input::DragPeer {
                peer,
                available: false,
            },
        );
    }
    rig.fact(20);
    rig.cross();
    assert_eq!(rig.drops(), 1);
    rig.cleanup();
    assert_eq!(rig.seat.totals(B), (1, 1));
}

#[test]
fn escape_is_swallowed_and_a_later_fact_rearms() {
    let mut rig = Rig::new();
    rig.control();
    rig.primary(true);
    rig.fact(20);
    let after_fact = rig.trace.len();
    for down in [true, false] {
        rig.feed(
            0,
            Input::Capture(CaptureEvent::Key {
                usage: HidUsage::keyboard(0x29),
                down,
                at: ms(rig.now),
            }),
        );
    }
    assert!(
        !rig.trace[after_fact..]
            .iter()
            .any(|(_, output)| matches!(output,
        Output::SendInput { msg: InputMessage::Key { usage, .. }, .. }
            if *usage == HidUsage::keyboard(0x29)))
    );
    rig.age(20);
    rig.fact(20);
    rig.cross();
    assert_eq!(rig.drops(), 1);
    rig.cleanup();
    assert_eq!(rig.seat.totals(B), (1, 1));
}

#[test]
fn target_fact_expires_the_primary_lease_even_without_a_tick() {
    let mut rig = Rig::new();
    rig.control();
    rig.primary(true);
    rig.fact(20);
    let previous = rig.trace.len();
    rig.now += 1000; // Deliberately omit the scheduled Tick and its lease refresh.
    rig.fact(20);
    assert!(!rig.trace[previous..].iter().any(|(_, output)| matches!(
        output,
        Output::SendInput {
            msg: InputMessage::Status {
                status: TargetStatus::NativeMove { .. },
                ..
            },
            ..
        }
    )));
    rig.cleanup();
    assert_eq!(rig.seat.totals(B), (1, 1));
}

proptest! {
    #[test]
    fn current_fact_is_the_only_commit_path(age in 0u64..601, fault in 0u8..12, reorder in any::<bool>()) {
        let mut rig = Rig::new(); rig.control(); rig.primary(true); rig.fact(20); rig.age(age); rig.reorder = reorder;
        match fault {
            1 => { for down in [true, false] { rig.feed(0, Input::Capture(CaptureEvent::Key { usage: HidUsage::keyboard(0x29), down, at: ms(rig.now) })); } }
            2 => rig.feed(1, Input::Capture(CaptureEvent::NativeMoveEnded { window: WindowId(20), at: ms(rig.now) })),
            3 => rig.feed(0, Input::Session(SessionEvent::State(SessionState { lock: LockState::Locked, active: Some(true) }))),
            4 => { let session = rig.trace.iter().rev().find_map(|(_, output)| match output { Output::SendInput { msg: InputMessage::Button { session, down: true, .. }, .. } => Some(*session), _ => None }).unwrap();
                rig.feed(0, Input::Link(LinkEvent::Input { peer: B, msg: InputMessage::Status { session, status: TargetStatus::Refused(Refusal::SecureInput) } })); }
            5 => rig.feed(0, Input::DragInPeer { peer: B, available: false }),
            6 => rig.feed(0, Input::Capture(CaptureEvent::Button { button: MouseButton::SECONDARY, down: true, at: ms(rig.now) })),
            7 => rig.feed(0, Input::Capture(CaptureEvent::Key { usage: HidUsage::keyboard(4), down: true, at: ms(rig.now) })),
            8 => rig.primary(false),
            9 => { for (index, peer) in [(0, B), (1, A)] { rig.feed(index, Input::Link(LinkEvent::Closed { peer, error: LinkError::Closed })); } }
            10 => rig.feed(0, Input::Command(Command::Panic)),
            11 => rig.feed(0, Input::Command(Command::ReleaseControl)),
            _ => {}
        }
        rig.cross(); prop_assert_eq!(rig.drops(), usize::from(fault == 0 && age <= 500));
        rig.cleanup(); prop_assert_eq!(rig.seat.totals(B), (1, 1));
        prop_assert_eq!(rig.order.ups, u32::from(fault != 9));
    }
}
