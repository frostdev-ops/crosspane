#![allow(clippy::unwrap_used, clippy::expect_used)]

use crosspane_engine::{
    Command, Engine, EngineConfig, Failure, InjectCmd, Input, Output, ProjectionKey, ProxyEvent,
};
use crosspane_input::journal::MemoryJournal;
use crosspane_platform::{
    CaptureEvent, CaptureId, CaptureStart, EndReason, LockState, MotionKind, OverlayEvent, Parked,
    ParkingKind, SessionEvent, SessionState, StreamId, WindowEvent, WindowInfo, WindowRole,
    WindowState,
};
use crosspane_protocol::link::{LinkError, LinkEvent};
use crosspane_protocol::msg::{Capability, ControlMessage, InputMessage, Placement};
use crosspane_protocol::projection::{ProjectionMessage as Message, ProxyPlacement};
use crosspane_testkit::{DragSeat, PhysicalRelease};
use crosspane_types::color::ColorSpace;
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{
    DisplayGeometry, PixelRect, PixelSize, PointDevice, PointLogical, PointMm, RectLogical,
    SizeLogical, SizeMm,
};
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::id::{DisplayId, NodeId, ProjectionId, WindowId};
use crosspane_types::input::LockKeys;
use crosspane_types::time::MonoTime;
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const A: NodeId = NodeId([1; 32]);
const B: NodeId = NodeId([2; 32]);
const C: NodeId = NodeId([3; 32]);
const OPEN: SessionState = SessionState {
    lock: LockState::Unlocked,
    active: Some(true),
};
fn ms(n: u64) -> MonoTime {
    MonoTime::from_nanos(n * 1_000_000)
}
fn control(peer: NodeId, msg: Message) -> Input {
    Input::Link(LinkEvent::Control {
        peer,
        msg: ControlMessage::Projection(msg),
    })
}
fn display(width: u32, scale: f64) -> DisplayInfo {
    DisplayInfo {
        id: DisplayId(1),
        name: "fake".into(),
        geometry: DisplayGeometry {
            physical_size: SizeMm::new(100.0, 100.0),
            pixel_size: PixelSize::new(width, 1000),
            scale,
            logical_origin: PointLogical::zero(),
        },
        refresh_millihz: 60_000,
        color_space: ColorSpace::Srgb,
        hdr: false,
    }
}
fn window(id: u64) -> WindowInfo {
    WindowInfo {
        id: WindowId(id),
        title: "fixture".into(),
        app_id: "fake".into(),
        pid: None,
        display: Some(DisplayId(1)),
        frame: RectLogical::new(PointLogical::zero(), SizeLogical::new(320.0, 200.0)),
        state: WindowState::Normal,
        role: WindowRole::Toplevel,
        parent: None,
    }
}

struct Harness {
    engines: [Engine; 2],
    now: u64,
    portal: crosspane_platform::PortalId,
    window: WindowId,
    scales: [f64; 2],
    capture: Option<CaptureId>,
    started: bool,
    settled: bool,
    begun: bool,
    pending_up: bool,
    awaiting_drop: bool,
    ordinary: bool,
    hud_ack: bool,
    hud_visible: bool,
    handshake_ack: bool,
    portal_ack: bool,
    pending_huds: Vec<usize>,
    pending_handshakes: Vec<(usize, Input)>,
    ended: Vec<CaptureId>,
    effect_start: usize,
    trace: Vec<(usize, Output)>,
    seat: DragSeat,
    keys: BTreeSet<(NodeId, HidUsage)>,
    key_totals: BTreeMap<(NodeId, HidUsage), (u32, u32)>,
    placed: bool,
    arm_ack: bool,
    move_ack: bool,
    drop_motion: bool,
    fail_down: bool,
    shrink: Option<PixelSize>,
    origin: Option<PointDevice>,
    pending_moves: Vec<(usize, crosspane_engine::InjectId)>,
    pending_arms: Vec<(usize, ProjectionKey, u32)>,
    opened: Option<(usize, ProjectionKey, PixelSize, ProxyPlacement)>,
    parked_size: Option<PixelSize>,
}
impl Harness {
    fn new(enabled: bool, feature: bool, width: u32, scale: f64) -> Self {
        let engines = [A, B].map(|node| {
            let mut config = EngineConfig::new(node);
            config.drag_across = enabled;
            config.accel.base_mm_per_unit = 0.1;
            config.accel.max_gain = 1.0;
            Engine::new(
                config,
                Box::new(MemoryJournal::default()),
                Box::new(MemoryJournal::default()),
                ms(0),
            )
            .unwrap()
            .0
        });
        let mut h = Self {
            engines,
            now: 0,
            portal: crosspane_platform::PortalId(0),
            window: WindowId(10),
            scales: [1.0, scale],
            capture: None,
            started: false,
            settled: false,
            begun: false,
            pending_up: false,
            awaiting_drop: false,
            ordinary: false,
            hud_ack: true,
            hud_visible: false,
            handshake_ack: true,
            portal_ack: true,
            pending_huds: vec![],
            pending_handshakes: vec![],
            ended: vec![],
            effect_start: 0,
            trace: vec![],
            seat: DragSeat::default(),
            keys: BTreeSet::new(),
            key_totals: BTreeMap::new(),
            placed: true,
            arm_ack: true,
            move_ack: true,
            drop_motion: false,
            fail_down: false,
            shrink: None,
            origin: None,
            pending_moves: vec![],
            pending_arms: vec![],
            opened: None,
            parked_size: None,
        };
        for index in 0..2 {
            let peer = [B, A][index];
            h.feed(index, Input::Session(SessionEvent::State(OPEN)));
            h.feed(
                index,
                Input::Grants(
                    [(
                        peer,
                        [
                            Capability::InputAccept,
                            Capability::WindowShare,
                            Capability::WindowPresent,
                        ]
                        .into_iter()
                        .collect(),
                    )]
                    .into_iter()
                    .collect(),
                ),
            );
            h.feed(
                index,
                Input::LocalDisplays(vec![if index == 0 {
                    display(1000, 1.0)
                } else {
                    display(width, scale)
                }]),
            );
            h.feed(
                index,
                Input::PeerDisplays {
                    peer,
                    displays: vec![if index == 0 {
                        display(width, scale)
                    } else {
                        display(1000, 1.0)
                    }],
                },
            );
            h.feed(index, Input::PeerUp { peer });
            h.feed(
                index,
                Input::DragPeer {
                    peer,
                    available: feature,
                },
            );
            h.feed(
                index,
                Input::Windows(WindowEvent::Added(window(if index == 0 { 10 } else { 20 }))),
            );
            h.feed(
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
        h.portal = h
            .trace
            .iter()
            .rev()
            .find_map(|(index, output)| match output {
                Output::SetPortals(portals) if *index == 0 => portals.first().map(|p| p.id),
                _ => None,
            })
            .unwrap();
        h.trace.clear();
        h
    }
    fn feed(&mut self, index: usize, input: Input) {
        if matches!(&input, Input::CaptureBegun { id, .. } if self.capture == Some(*id)) {
            self.begun = true;
        }
        if let Input::Capture(CaptureEvent::Started { id }) = &input
            && self.capture.is_none_or(|known| known <= *id)
        {
            self.capture = Some(*id);
            self.started = true;
        }
        if matches!(&input, Input::Overlay(OverlayEvent::Visible(id)) if *id == crosspane_engine::io::HUD)
        {
            self.hud_visible = true;
        }
        let outputs = self.engines[index].handle(input, ms(self.now));
        self.drive(index, outputs);
    }
    fn drive(&mut self, index: usize, outputs: Vec<Output>) {
        let mut queue: VecDeque<_> = outputs.into_iter().map(|o| (index, o)).collect();
        while let Some((index, output)) = queue.pop_front() {
            let node = [A, B][index];
            let other = 1 - index;
            self.trace.push((index, output.clone()));
            let mut callbacks = vec![];
            match output {
                Output::SetPortals(portals) if self.portal_ack => callbacks.push((
                    index,
                    Input::PortalsSet {
                        ids: portals.iter().map(|p| p.id).collect(),
                        result: Ok(()),
                    },
                )),
                Output::ShowOverlay { id, .. } => {
                    if id == crosspane_engine::io::HUD {
                        self.hud_visible = false;
                        if !self.hud_ack {
                            self.pending_huds.push(index);
                        }
                    }
                    if id != crosspane_engine::io::HUD || self.hud_ack {
                        callbacks.push((index, Input::Overlay(OverlayEvent::Visible(id))));
                    }
                }
                Output::HideOverlay(id) if id == crosspane_engine::io::HUD => {
                    self.hud_visible = false
                }
                Output::SendControl { peer, msg } if peer == [A, B][other] => {
                    let callback = (
                        other,
                        Input::Link(LinkEvent::Control {
                            peer: node,
                            msg: msg.clone(),
                        }),
                    );
                    if matches!(msg, ControlMessage::ControlStarted { .. }) && !self.handshake_ack {
                        self.pending_handshakes.push(callback);
                    } else {
                        callbacks.push(callback);
                    }
                }
                Output::SendInput { peer, msg } if peer == [A, B][other] => {
                    if matches!(msg, InputMessage::PressAt { .. }) {
                        self.seat.press_at();
                    }
                    if let InputMessage::State { held_buttons, .. } = &msg
                        && held_buttons.contains(&MouseButton::PRIMARY)
                        && !self.ordinary
                    {
                        assert_eq!(self.seat.presses, 1, "primary routed before PressAt");
                    }
                    callbacks.push((other, Input::Link(LinkEvent::Input { peer: node, msg })));
                }
                Output::SendMotion { peer, msg } if !self.drop_motion && peer == [A, B][other] => {
                    callbacks.push((other, Input::Link(LinkEvent::Motion { peer: node, msg })))
                }
                Output::BeginDrag { id, .. } => {
                    assert!(self.hud_visible, "capture before visible HUD");
                    self.capture = Some(id);
                    self.started = false;
                    self.settled = false;
                    self.begun = false;
                }
                Output::BeginCapture { id, .. } => {
                    self.capture = Some(id);
                    self.started = true;
                    self.seat.ordinary_capture();
                    callbacks.extend(self.capture_callbacks(id));
                }
                Output::EndCapture { .. } => {
                    self.seat.end_capture();
                    let started = std::mem::take(&mut self.started);
                    if let Some(id) = self.capture.take().filter(|_| started) {
                        self.ended.push(id);
                        callbacks.push((
                            index,
                            Input::Capture(CaptureEvent::Ended {
                                id,
                                reason: EndReason::Requested,
                            }),
                        ));
                    }
                }
                Output::OpenProxy {
                    key, size, place, ..
                } => {
                    let size = self.shrink.unwrap_or(size);
                    callbacks.push((
                        index,
                        Input::ProxyOpened {
                            key,
                            result: Ok((size, self.scales[index])),
                        },
                    ));
                    callbacks.push((
                        index,
                        Input::ProxyWindow {
                            key,
                            window: WindowId(100 + key.projection.0),
                        },
                    ));
                    if let Some(place) = place {
                        self.opened = Some((index, key, size, place));
                        if self.placed {
                            callbacks.push((index, self.placement(key, size, place)));
                        }
                    }
                }
                Output::Park { window, size, .. } | Output::ResizeParked { window, size, .. } => {
                    let size = self.parked_size.unwrap_or(size);
                    callbacks.push((
                        index,
                        Input::Parked {
                            window,
                            result: Ok(Parked {
                                window,
                                kind: ParkingKind::Mirror,
                                display: DisplayId(1),
                                content: PixelRect::new(
                                    crosspane_types::geom::euclid::Point2D::zero(),
                                    crosspane_types::geom::euclid::Point2D::new(
                                        size.width as i32,
                                        size.height as i32,
                                    ),
                                ),
                            }),
                        },
                    ))
                }
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
                        } => {
                            if !self.fail_down {
                                if self.seat.awaiting_continuation() {
                                    self.seat.consume_arm(node);
                                } else {
                                    assert!(self.ordinary, "primary injected before PressAt");
                                }
                                self.seat.down(node);
                            }
                        }
                        InjectCmd::Button {
                            button: MouseButton::PRIMARY,
                            down: false,
                        } => self.seat.up(node),
                        InjectCmd::Key { usage, down } => {
                            if down {
                                assert!(self.keys.insert((node, usage)));
                                self.key_totals.entry((node, usage)).or_default().0 += 1;
                            } else if self.keys.remove(&(node, usage)) {
                                self.key_totals.entry((node, usage)).or_default().1 += 1;
                            }
                        }
                        InjectCmd::ReleaseAll => {
                            self.seat.release_all(node);
                            let keys: Vec<_> = self
                                .keys
                                .iter()
                                .filter(|(peer, _)| *peer == node)
                                .copied()
                                .collect();
                            for key in keys {
                                self.keys.remove(&key);
                                self.key_totals.entry(key).or_default().1 += 1;
                            }
                        }
                        _ => {}
                    }
                    if !self.move_ack && matches!(cmd, InjectCmd::MoveTo { .. }) {
                        self.pending_moves.push((index, id));
                    } else {
                        callbacks.push((
                            index,
                            Input::InjectDone {
                                id,
                                ok: !(self.fail_down
                                    && matches!(cmd, InjectCmd::Button { down: true, .. })),
                            },
                        ));
                    }
                }
                Output::ArmDrag { key, token, .. } => {
                    self.seat.arm(node, key.projection.0, token);
                    if self.arm_ack {
                        callbacks.push((
                            index,
                            Input::DragArmed {
                                key,
                                token,
                                ok: true,
                            },
                        ));
                    } else {
                        self.pending_arms.push((index, key, token));
                    }
                }
                Output::DisarmDrag { key, token } => {
                    self.seat.disarm(node, key.projection.0, token)
                }
                _ => {}
            }
            for (target, input) in callbacks {
                if matches!(&input, Input::Overlay(OverlayEvent::Visible(id)) if *id == crosspane_engine::io::HUD)
                {
                    self.hud_visible = true;
                }
                queue.extend(
                    self.engines[target]
                        .handle(input, ms(self.now))
                        .into_iter()
                        .map(|o| (target, o)),
                );
            }
        }
    }
    fn capture_callbacks(&self, id: CaptureId) -> Vec<(usize, Input)> {
        vec![
            (0, Input::Capture(CaptureEvent::Started { id })),
            (
                0,
                Input::CaptureBegun {
                    id,
                    result: Ok(CaptureStart {
                        held_keys: vec![],
                        lock_keys: LockKeys::default(),
                    }),
                },
            ),
        ]
    }
    fn placement(&self, key: ProjectionKey, size: PixelSize, place: ProxyPlacement) -> Input {
        Input::Proxy {
            key,
            event: ProxyEvent::Placed {
                display: Some(place.display),
                origin: self
                    .origin
                    .unwrap_or(PointDevice::new(f64::from(place.x), f64::from(place.y))),
                size,
            },
        }
    }
    fn drag_at(&mut self) {
        self.feed(
            0,
            Input::Capture(CaptureEvent::DragAtEdge {
                portal: self.portal,
                position: 0.5,
                window: self.window,
                grab: PointDevice::new(20.0, -12.0),
                at: ms(self.now),
            }),
        );
    }
    fn begin(&mut self, failure: Option<Failure>) {
        self.start_push();
        self.complete_begin(failure);
    }
    fn start_push(&mut self) {
        self.effect_start = self.trace.len();
        if !self.seat.physical {
            self.seat.original_down(A);
        } else {
            self.seat.gesture();
        }
        self.drag_at();
        self.now += 250;
        self.drag_at();
    }
    fn complete_begin(&mut self, failure: Option<Failure>) {
        let id = self.capture.unwrap();
        if let Some(failure) = failure {
            self.awaiting_drop = failure == Failure::PointerButtonHeld;
            self.feed(
                0,
                Input::CaptureBegun {
                    id,
                    result: Err(failure),
                },
            );
            self.capture = None;
            if self.settled {
                self.seat.end_capture();
            }
            self.pending_up = false;
        } else {
            if !self.settled {
                self.settle();
            }
            self.started();
            self.feed(
                0,
                Input::CaptureBegun {
                    id,
                    result: Ok(CaptureStart {
                        held_keys: vec![],
                        lock_keys: LockKeys::default(),
                    }),
                },
            );
        }
    }
    fn settle(&mut self) {
        self.seat.settle(A);
        self.settled = true;
    }
    fn started(&mut self) {
        let id = self.capture.unwrap();
        self.started = true;
        self.feed(0, Input::Capture(CaptureEvent::Started { id }));
        if std::mem::take(&mut self.pending_up) {
            self.capture_up();
        }
    }
    fn capture_up(&mut self) {
        self.feed(
            0,
            Input::Capture(CaptureEvent::Button {
                button: MouseButton::PRIMARY,
                down: false,
                at: ms(self.now),
            }),
        );
    }
    fn motion(&mut self, dx: f64, dy: f64) {
        self.feed(
            0,
            Input::Capture(CaptureEvent::Motion {
                dx,
                dy,
                kind: MotionKind::Accelerated {
                    display: DisplayId(1),
                },
                at: ms(self.now),
            }),
        );
    }
    fn physical_up(&mut self) {
        match self.seat.physical_up() {
            Some(PhysicalRelease::Captured) if self.started => self.capture_up(),
            Some(PhysicalRelease::Captured) => self.pending_up = true,
            Some(PhysicalRelease::Native) if self.awaiting_drop => self.emit_drop(),
            Some(PhysicalRelease::Native) => self.feed(
                0,
                Input::Capture(CaptureEvent::EdgeReleased {
                    portal: self.portal,
                    at: ms(self.now),
                }),
            ),
            Some(PhysicalRelease::SuppressedTail) | None => {}
        }
    }
    fn dropped(&mut self) {
        self.physical_up();
    }
    fn emit_drop(&mut self) {
        self.awaiting_drop = false;
        self.feed(
            0,
            Input::Capture(CaptureEvent::DragDroppedAtEdge {
                portal: self.portal,
                position: 0.5,
                window: self.window,
                grab: PointDevice::new(20.0, -12.0),
                at: ms(self.now),
            }),
        );
    }
    fn commits(&self) -> usize {
        self.trace[self.effect_start..]
            .iter()
            .filter(|(_, o)| {
                matches!(
                    o,
                    Output::SendControl {
                        msg: ControlMessage::Projection(
                            Message::Start { .. }
                                | Message::StartAt { .. }
                                | Message::ReturnAt { .. }
                        ),
                        ..
                    }
                )
            })
            .count()
    }
    fn effects(&self) -> usize {
        self.trace[self.effect_start..]
            .iter()
            .filter(|(_, o)| {
                matches!(
                    o,
                    Output::OpenProxy { .. }
                        | Output::CloseProxy { .. }
                        | Output::Park { .. }
                        | Output::ResizeParked { .. }
                        | Output::Restore { .. }
                        | Output::SendControl {
                            msg: ControlMessage::Projection(
                                Message::Start { .. }
                                    | Message::StartAt { .. }
                                    | Message::ReturnAt { .. }
                                    | Message::End { .. }
                                    | Message::Close { .. }
                            ),
                            ..
                        }
                )
            })
            .count()
    }
    fn clean_now(&mut self) {
        assert!(!self.seat.armed(), "arm survived cancellation");
        assert!(
            !self.seat.has_hold(B),
            "remote primary survived cancellation"
        );
        let before = self.trace.len();
        self.now += 60;
        self.feed(0, Input::Tick);
        assert!(self.trace[before..].iter().all(|(_, o)| !matches!(o, Output::SendInput { msg: InputMessage::State { held_buttons, .. }, .. } if held_buttons.contains(&MouseButton::PRIMARY))));
    }
    fn key(&self) -> ProjectionKey {
        self.opened.unwrap().1
    }
    fn token(&self) -> u32 {
        self.trace
            .iter()
            .find_map(|(_, o)| match o {
                Output::SendControl {
                    msg: ControlMessage::Projection(Message::StartAt { token, .. }),
                    ..
                } => Some(*token),
                _ => None,
            })
            .unwrap()
    }
    fn ready(&mut self, token: u32) {
        let key = self.key();
        self.feed(
            0,
            control(
                B,
                Message::DragReady {
                    projection: key.projection,
                    token,
                    display: DisplayId(1),
                    position: PointDevice::new(80.0, 400.0),
                },
            ),
        );
    }
    fn place_now(&mut self) {
        let (index, key, size, place) = self.opened.unwrap();
        self.feed(index, self.placement(key, size, place));
    }
    fn proxy_from_b(&mut self) {
        self.feed(
            1,
            Input::Command(Command::Project {
                window: WindowId(20),
                to: A,
                place: None,
            }),
        );
        self.window = WindowId(101);
        self.effect_start = self.trace.len();
    }
    fn finish(&mut self) {
        self.physical_up();
        self.feed(0, Input::Command(Command::ReleaseControl));
        self.feed(1, Input::Command(Command::Panic));
        self.now += 2500;
        self.feed(0, Input::Tick);
        self.feed(1, Input::Tick);
        assert!(self.seat.balanced(), "{:?}", self.seat);
        assert!(self.keys.is_empty());
        assert!(self.key_totals.values().all(|(down, up)| down == up));
    }
}

#[test]
fn offer_requires_config_and_negotiated_peer() {
    for (enabled, feature) in [(false, true), (true, false)] {
        let mut h = Harness::new(enabled, feature, 1000, 1.0);
        h.drag_at();
        h.now = 250;
        h.drag_at();
        assert!(h.capture.is_none());
        assert_eq!(h.commits(), 0);
    }
}
#[test]
fn project_permissions_unknown_windows_and_other_peers_are_not_drags() {
    let mut h = Harness::new(true, true, 1000, 1.0);
    h.feed(0, Input::Grants(BTreeMap::new()));
    h.drag_at();
    h.now = 250;
    h.drag_at();
    assert!(h.capture.is_none());
    let mut h = Harness::new(true, true, 1000, 1.0);
    h.window = WindowId(999);
    h.drag_at();
    h.now = 250;
    h.drag_at();
    assert!(h.capture.is_none());
    let mut h = Harness::new(true, true, 1000, 1.0);
    h.proxy_from_b();
    h.feed(
        0,
        Input::ProxyWindow {
            key: ProjectionKey {
                source: C,
                projection: ProjectionId(1),
            },
            window: h.window,
        },
    );
    h.drag_at();
    h.now = 250;
    h.drag_at();
    assert!(h.capture.is_none());
}
#[test]
fn stable_push_waits_250ms_and_edge_release_or_changed_classification_cancels() {
    let mut h = Harness::new(true, true, 1000, 1.0);
    h.drag_at();
    h.now = 249;
    h.drag_at();
    assert!(h.capture.is_none());
    h.feed(
        0,
        Input::Capture(CaptureEvent::EdgeReleased {
            portal: h.portal,
            at: ms(h.now),
        }),
    );
    h.now = 250;
    h.drag_at();
    assert!(h.capture.is_none());
    h.feed(0, Input::Windows(WindowEvent::Added(window(11))));
    h.window = WindowId(11);
    h.now = 500;
    h.drag_at();
    assert!(h.capture.is_none());
}
#[test]
fn failed_begin_drag_never_becomes_plain_crossing() {
    let mut h = Harness::new(true, true, 1000, 1.0);
    h.begin(Some(Failure::Other));
    h.now = 1000;
    h.feed(0, Input::Tick);
    assert!(
        !h.trace
            .iter()
            .any(|(_, o)| matches!(o, Output::BeginCapture { .. }))
    );
    assert_eq!(h.commits(), 0);
    h.finish();
}

#[test]
fn late_pointer_button_held_cannot_enter_awaiting_drop() {
    let mut h = Harness::new(true, true, 1000, 1.0);
    h.seat.original_down(A);
    h.drag_at();
    h.now += 250;
    h.drag_at();
    let id = h.capture.unwrap();
    h.now += 1000;
    h.feed(
        0,
        Input::CaptureBegun {
            id,
            result: Err(Failure::PointerButtonHeld),
        },
    );
    h.dropped();
    assert_eq!(h.commits(), 0);
    assert!(
        !h.trace
            .iter()
            .any(|(_, o)| matches!(o, Output::BeginCapture { .. }))
    );
    h.finish();
}
#[test]
fn held_at_seat_is_absent_from_heartbeat_and_other_inputs_route() {
    let mut h = Harness::new(true, true, 1000, 1.0);
    h.begin(None);
    h.now = 500;
    h.feed(0, Input::Tick);
    assert!(h.trace.iter().all(|(_, o)| !matches!(o, Output::SendInput { msg: InputMessage::State { held_buttons, .. }, .. } if held_buttons.contains(&MouseButton::PRIMARY))));
    h.feed(
        0,
        Input::Capture(CaptureEvent::Key {
            usage: HidUsage::keyboard(4),
            down: true,
            at: ms(h.now),
        }),
    );
    assert!(h.keys.contains(&(B, HidUsage::keyboard(4))));
    for down in [true, false] {
        h.feed(
            0,
            Input::Capture(CaptureEvent::Button {
                button: MouseButton(2),
                down,
                at: ms(h.now),
            }),
        );
    }
    assert!(h.trace.iter().any(|(_, o)| matches!(
        o,
        Output::SendInput {
            msg: InputMessage::Button {
                button: MouseButton(2),
                down: true,
                ..
            },
            ..
        }
    )));
    h.finish();
}
#[test]
fn perpendicular_logical_distance_handles_density_tangent_and_out_and_back() {
    let mut h = Harness::new(true, true, 1500, 1.5);
    h.begin(None);
    h.motion(0.0, 200.0);
    h.motion(30.0, 0.0);
    h.motion(-30.0, 0.0);
    h.motion(30.0, 0.0);
    assert_eq!(h.commits(), 0);
    h.motion(18.1, 0.0);
    assert_eq!(h.commits(), 1);
    assert_eq!(h.seat.presses, 1);
    h.finish();
}
#[test]
fn release_and_escape_abort_without_project_or_return_and_escape_up_is_swallowed() {
    for (back, escape) in [(false, false), (false, true), (true, false), (true, true)] {
        let mut h = Harness::new(true, true, 1000, 1.0);
        if back {
            h.proxy_from_b();
        }
        h.begin(None);
        if escape {
            for down in [true, false] {
                h.feed(
                    0,
                    Input::Capture(CaptureEvent::Key {
                        usage: HidUsage::keyboard(0x29),
                        down,
                        at: ms(h.now),
                    }),
                );
            }
        } else {
            h.physical_up();
        }
        h.motion(80.0, 0.0);
        assert_eq!(h.commits(), 0);
        assert_eq!(h.effects(), 0);
        assert_eq!(h.seat.presses, 0);
        assert!(!h.trace.iter().any(|(_, o)| matches!(o, Output::SendInput { msg: InputMessage::Key { usage, .. }, .. } if *usage == HidUsage::keyboard(0x29))));
        h.clean_now();
        h.finish();
    }
}
#[test]
fn own_placement_ready_and_reliable_press_survive_lost_final_datagram() {
    let mut h = Harness::new(true, true, 1500, 1.5);
    h.origin = Some(PointDevice::new(200.0, 300.0));
    h.drop_motion = true;
    h.begin(None);
    h.motion(50.0, 0.0);
    let expected = PointDevice::new(230.0, 308.0);
    assert!(h.trace.iter().any(|(_, o)| matches!(o, Output::SendInput { msg: InputMessage::PressAt { position, .. }, .. } if *position == expected)));
    assert!(h.trace.iter().any(|(i, o)| *i == 1 && matches!(o, Output::Inject { cmd: InjectCmd::MoveTo { position, .. }, .. } if *position == expected)));
    h.finish();
}
#[test]
fn wrong_late_and_repeated_ready_never_press_again() {
    let mut h = Harness::new(true, true, 1000, 1.0);
    h.placed = false;
    h.begin(None);
    h.motion(50.0, 0.0);
    h.ready(h.token() + 1);
    assert_eq!(h.seat.presses, 0);
    h.place_now();
    assert_eq!(h.seat.presses, 1);
    h.ready(h.token());
    assert_eq!(h.seat.presses, 1);
    h.finish();
    let mut h = Harness::new(true, true, 1000, 1.0);
    h.placed = false;
    h.begin(None);
    h.motion(50.0, 0.0);
    h.now += 1000;
    h.feed(0, Input::Tick);
    h.place_now();
    assert_eq!(h.seat.presses, 0);
    h.finish();
}
#[test]
fn held_motion_is_applied_after_ready_and_release_or_timeout_drops_permanently() {
    for release in [false, true] {
        let mut h = Harness::new(true, true, 1000, 1.0);
        h.placed = false;
        h.begin(None);
        h.motion(50.0, 0.0);
        let at = h.trace.len();
        h.motion(20.0, 10.0);
        assert!(
            !h.trace[at..]
                .iter()
                .any(|(_, o)| matches!(o, Output::SendMotion { .. }))
        );
        if release {
            h.physical_up();
        } else {
            h.now += 1000;
            h.feed(0, Input::Tick);
        }
        assert!(h.trace.iter().any(|(_, o)| matches!(
            o,
            Output::SendControl {
                msg: ControlMessage::Projection(Message::DragCancel { .. }),
                ..
            }
        )));
        h.place_now();
        assert_eq!(h.seat.presses, 0);
        h.finish();
    }
    let mut h = Harness::new(true, true, 1000, 1.0);
    h.placed = false;
    h.begin(None);
    h.motion(50.0, 0.0);
    h.motion(20.0, 10.0);
    h.place_now();
    let press = h
        .trace
        .iter()
        .find_map(|(_, o)| match o {
            Output::SendInput {
                msg: InputMessage::PressAt { position, .. },
                ..
            } => Some(*position),
            _ => None,
        })
        .unwrap();
    assert!(h.trace.iter().any(|(_, o)| matches!(o, Output::SendMotion { msg, .. } if (msg.position.x - press.x - 20.0).abs() < 0.01 && (msg.position.y - press.y - 10.0).abs() < 0.01)));
    h.finish();
}
#[test]
fn shrunk_content_and_unknown_display_never_ready_or_press() {
    let mut h = Harness::new(true, true, 1000, 1.0);
    h.shrink = Some(PixelSize::new(4, 4));
    h.begin(None);
    h.motion(50.0, 0.0);
    assert_eq!(h.seat.presses, 0);
    assert!(!h.trace.iter().any(|(_, o)| matches!(
        o,
        Output::SendControl {
            msg: ControlMessage::Projection(Message::DragReady { .. }),
            ..
        }
    )));
    h.finish();
    let mut h = Harness::new(true, true, 1000, 1.0);
    h.placed = false;
    h.begin(None);
    h.motion(50.0, 0.0);
    let key = h.key();
    h.feed(
        1,
        Input::Proxy {
            key,
            event: ProxyEvent::Placed {
                display: Some(DisplayId(99)),
                origin: PointDevice::zero(),
                size: PixelSize::new(320, 200),
            },
        },
    );
    assert_eq!(h.seat.presses, 0);
    h.finish();
}
#[test]
fn return_at_uses_scaled_grab_and_never_continues_the_back_drag() {
    let mut h = Harness::new(true, true, 1500, 1.5);
    h.proxy_from_b();
    h.begin(None);
    h.motion(50.0, 0.0);
    assert!(h.trace.iter().any(|(i, o)| *i == 1 && matches!(o, Output::Restore { place: Some(place), .. } if !place.drag && place.y >= 500)));
    assert_eq!(h.seat.presses, 0);
    h.finish();
}
#[test]
fn target_waits_for_move_then_arm_and_flushes_queued_input_in_order() {
    let mut h = Harness::new(true, true, 1000, 1.0);
    h.begin(None);
    h.move_ack = false;
    h.arm_ack = false;
    h.motion(50.0, 0.0);
    assert_eq!(h.seat.totals(B).0, 0);
    for down in [true, false] {
        h.feed(
            0,
            Input::Capture(CaptureEvent::Key {
                usage: HidUsage::keyboard(4),
                down,
                at: ms(h.now),
            }),
        );
    }
    assert_eq!(h.key_totals.get(&(B, HidUsage::keyboard(4))), None);
    let (index, id) = h.pending_moves.pop().unwrap();
    h.feed(index, Input::InjectDone { id, ok: true });
    assert_eq!(h.seat.totals(B).0, 0);
    let (index, key, token) = h.pending_arms.pop().unwrap();
    h.feed(
        index,
        Input::DragArmed {
            key,
            token,
            ok: true,
        },
    );
    assert_eq!(h.seat.totals(B).0, 1);
    assert_eq!(h.key_totals[&(B, HidUsage::keyboard(4))], (1, 1));
    h.finish();
}
#[test]
fn failed_move_failed_arm_and_queued_primary_up_never_inject_down() {
    for case in 0..3 {
        let mut h = Harness::new(true, true, 1000, 1.0);
        h.begin(None);
        h.move_ack = case != 0;
        h.arm_ack = false;
        h.motion(50.0, 0.0);
        if case == 0 {
            let (i, id) = h.pending_moves.pop().unwrap();
            h.feed(i, Input::InjectDone { id, ok: false });
        } else {
            let (i, key, token) = h.pending_arms.pop().unwrap();
            if case == 1 {
                h.feed(
                    i,
                    Input::DragArmed {
                        key,
                        token,
                        ok: false,
                    },
                );
            } else {
                h.physical_up();
                h.feed(
                    i,
                    Input::DragArmed {
                        key,
                        token,
                        ok: true,
                    },
                );
            }
        }
        assert_eq!(h.seat.totals(B).0, 0);
        h.clean_now();
        h.finish();
    }
}
#[test]
fn target_500ms_bound_disarms_and_late_arm_ack_cannot_revive() {
    let mut h = Harness::new(true, true, 1000, 1.0);
    h.begin(None);
    h.arm_ack = false;
    h.motion(50.0, 0.0);
    let (i, key, token) = h.pending_arms.pop().unwrap();
    h.now += 500;
    h.feed(1, Input::Tick);
    h.clean_now();
    h.feed(
        i,
        Input::DragArmed {
            key,
            token,
            ok: true,
        },
    );
    assert_eq!(h.seat.totals(B).0, 0);
    h.finish();
}
#[test]
fn cancel_session_projection_lock_link_and_panic_disarm_pending_arm() {
    for case in 0..6 {
        let mut h = Harness::new(true, true, 1000, 1.0);
        h.begin(None);
        h.arm_ack = false;
        h.motion(50.0, 0.0);
        let key = h.key();
        let token = h.token();
        match case {
            0 => h.feed(
                1,
                control(
                    A,
                    Message::DragCancel {
                        projection: key.projection,
                        token,
                    },
                ),
            ),
            1 => h.feed(0, Input::Command(Command::ReleaseControl)),
            2 => h.feed(0, Input::Command(Command::Return(key))),
            3 => h.feed(
                1,
                Input::Session(SessionEvent::State(SessionState {
                    lock: LockState::Locked,
                    ..OPEN
                })),
            ),
            4 => {
                h.feed(
                    1,
                    Input::Link(LinkEvent::Closed {
                        peer: A,
                        error: LinkError::Closed,
                    }),
                );
                h.feed(
                    0,
                    Input::Link(LinkEvent::Closed {
                        peer: B,
                        error: LinkError::Closed,
                    }),
                );
            }
            _ => h.feed(1, Input::Command(Command::Panic)),
        }
        assert!(!h.seat.armed(), "case {case}: arm survived ending");
        h.clean_now();
        let (i, key, token) = h.pending_arms.pop().unwrap();
        h.feed(
            i,
            Input::DragArmed {
                key,
                token,
                ok: true,
            },
        );
        assert_eq!(h.seat.totals(B).0, 0);
        h.finish();
    }
}
#[test]
fn drop_at_edge_projects_without_continuation_then_crosses_normally() {
    let mut h = Harness::new(true, true, 1000, 1.0);
    h.begin(Some(Failure::PointerButtonHeld));
    assert!(h.trace.iter().any(|(_, o)| matches!(o, Output::ShowOverlay { overlay, .. } if overlay.text.starts_with("Release to move fixture"))));
    h.dropped();
    assert!(h.trace.iter().any(|(_, o)| matches!(o, Output::SendControl { msg: ControlMessage::Projection(Message::StartAt { place, .. }), .. } if !place.drag)));
    assert!(
        h.trace
            .iter()
            .any(|(_, o)| matches!(o, Output::BeginCapture { .. }))
    );
    assert_eq!(h.engines[0].control_established(), Some(B));
    assert_eq!(h.seat.presses, 0);
    h.finish();
}
#[test]
fn drop_at_edge_back_returns_without_press() {
    let mut h = Harness::new(true, true, 1000, 1.0);
    h.proxy_from_b();
    h.begin(Some(Failure::PointerButtonHeld));
    h.dropped();
    assert_eq!(h.commits(), 1);
    assert_eq!(h.seat.presses, 0);
    assert_eq!(h.engines[0].control_established(), Some(B));
    h.finish();
}

#[test]
fn retile_before_drop_keeps_the_gesture_and_places_using_fresh_size_and_scale() {
    let mut h = Harness::new(true, true, 1000, 1.0);
    h.begin(Some(Failure::PointerButtonHeld));
    h.feed(0, Input::LocalDisplays(vec![display(1000, 2.0)]));
    let mut retiled = window(10);
    retiled.frame.size = SizeLogical::new(600.0, 700.0);
    h.feed(0, Input::Windows(WindowEvent::Changed(retiled)));
    // The backend can report another edge sample while waiting for release.
    h.drag_at();
    h.dropped();
    assert_eq!(h.commits(), 1);
    assert!(h.trace.iter().any(|(node, output)| matches!(
        (node, output),
        (0, Output::SendControl {
            msg: ControlMessage::Projection(Message::StartAt { size, place, .. }), ..
        }) if *size == PixelSize::new(600, 700) && place.y == 300 && !place.drag
    )));
    h.emit_drop();
    assert_eq!(h.commits(), 1, "a replay cannot create another projection");
    assert_eq!(h.seat.presses, 0);
    h.finish();
}

#[test]
fn retile_after_drop_projects_once_and_actual_parked_geometry_still_updates() {
    let mut h = Harness::new(true, true, 1000, 1.0);
    h.begin(Some(Failure::PointerButtonHeld));
    h.dropped();
    let initial = h
        .trace
        .iter()
        .find_map(|(node, output)| match (node, output) {
            (
                0,
                Output::SendControl {
                    msg: ControlMessage::Projection(Message::StartAt { size, place, .. }),
                    ..
                },
            ) => Some((*size, *place)),
            _ => None,
        })
        .unwrap();
    assert_eq!(initial.0, PixelSize::new(320, 200));
    assert_eq!(initial.1.y, 492, "latest geometry available at commit");
    let actual = PixelSize::new(600, 700);
    h.parked_size = Some(actual);
    let mut retiled = window(10);
    retiled.frame.size = SizeLogical::new(600.0, 700.0);
    h.feed(0, Input::Windows(WindowEvent::Changed(retiled)));
    assert!(h.trace.iter().any(|(node, output)| matches!(
        (node, output),
        (0, Output::SendControl {
            msg: ControlMessage::Projection(Message::Geometry { size, .. }), ..
        }) if *size == actual
    )));
    h.emit_drop();
    assert_eq!(h.commits(), 1);
    assert_eq!(h.seat.presses, 0);
    h.finish();
}

#[test]
fn awaiting_drop_rejects_changed_window_kind_and_portal() {
    for case in 0..3 {
        let mut h = Harness::new(true, true, 1000, 1.0);
        h.begin(Some(Failure::PointerButtonHeld));
        match case {
            0 => {
                h.feed(0, Input::Windows(WindowEvent::Added(window(11))));
                h.window = WindowId(11);
            }
            1 => {
                h.proxy_from_b();
                h.window = WindowId(10);
                h.feed(
                    0,
                    Input::ProxyWindow {
                        key: ProjectionKey {
                            source: B,
                            projection: ProjectionId(1),
                        },
                        window: h.window,
                    },
                );
                h.effect_start = h.trace.len();
            }
            _ => h.portal = crosspane_platform::PortalId(u32::MAX),
        }
        h.dropped();
        assert_eq!(h.commits(), 0, "case {case}");
        assert_eq!(h.seat.presses, 0);
        assert!(!h.hud_visible);
        h.finish();
    }
}

#[test]
fn retile_during_hud_or_capture_activation_refreshes_successful_drag_geometry() {
    for before_hud in [true, false] {
        let mut h = Harness::new(true, true, 1000, 1.0);
        h.hud_ack = !before_hud;
        h.start_push();
        assert_eq!(h.capture.is_none(), before_hud);
        h.feed(0, Input::LocalDisplays(vec![display(1000, 2.0)]));
        let mut retiled = window(10);
        retiled.frame.size = SizeLogical::new(600.0, 700.0);
        h.feed(0, Input::Windows(WindowEvent::Changed(retiled)));
        h.drag_at();
        if before_hud {
            h.feed(
                0,
                Input::Overlay(OverlayEvent::Visible(crosspane_engine::io::HUD)),
            );
        }
        h.complete_begin(None);
        h.motion(50.0, 0.0);
        assert_eq!(h.commits(), 1);
        assert!(
            h.trace.iter().any(|(node, output)| matches!(
                (node, output),
                (0, Output::SendControl {
                    msg: ControlMessage::Projection(Message::StartAt { size, place, .. }), ..
                }) if *size == PixelSize::new(600, 700) && place.y == 300 && place.drag
            )),
            "{:#?}",
            h.trace
        );
        assert!(h.trace.iter().any(|(node, output)| matches!(
            (node, output), (0, Output::Park { size, scale, .. })
            if *size == PixelSize::new(600, 700) && *scale == 1.0
        )));
        h.finish();
    }
}
#[test]
fn awaiting_drop_edge_release_timeout_classification_and_refusal_cancel() {
    for case in 0..4 {
        let mut h = Harness::new(true, true, 1000, 1.0);
        h.begin(Some(Failure::PointerButtonHeld));
        match case {
            0 => h.feed(
                0,
                Input::Capture(CaptureEvent::EdgeReleased {
                    portal: h.portal,
                    at: ms(h.now),
                }),
            ),
            1 => {
                h.now += 10000;
                h.feed(0, Input::Tick);
            }
            2 => {
                h.feed(0, Input::Windows(WindowEvent::Added(window(11))));
                h.window = WindowId(11);
                h.drag_at();
            }
            _ => h.feed(0, Input::Grants(BTreeMap::new())),
        }
        h.dropped();
        assert_eq!(h.commits(), 0);
        assert_eq!(h.seat.presses, 0);
        h.finish();
    }
}
#[test]
fn disabled_drag_has_exact_legacy_trace() {
    legacy_golden(&[(7, 2), (5, 1)]);
}

#[test]
fn arm_expiry_and_failed_down_leave_no_routed_primary_or_arm() {
    let mut h = Harness::new(true, true, 1000, 1.0);
    h.begin(None);
    h.fail_down = true;
    h.motion(50.0, 0.0);
    assert_eq!(h.seat.totals(B).0, 0);
    assert_eq!(h.engines[0].controlling(), None);
    h.finish();
    let mut h = Harness::new(true, true, 1000, 1.0);
    h.begin(None);
    h.motion(50.0, 0.0);
    h.now += 2000;
    h.feed(1, Input::Tick);
    assert!(
        h.trace
            .iter()
            .any(|(_, o)| matches!(o, Output::DisarmDrag { .. }))
    );
    h.finish();
}

#[test]
fn feature_removal_and_project_refusal_end_the_gesture() {
    for refusal in [false, true] {
        let mut h = Harness::new(true, true, 1000, 1.0);
        h.placed = false;
        h.begin(None);
        if refusal {
            h.feed(
                1,
                Input::Grants(
                    [(A, [Capability::InputAccept].into_iter().collect())]
                        .into_iter()
                        .collect(),
                ),
            );
            h.motion(50.0, 0.0);
        } else {
            h.feed(
                0,
                Input::DragPeer {
                    peer: B,
                    available: false,
                },
            );
        }
        h.motion(80.0, 0.0);
        assert_eq!(h.seat.presses, 0);
        assert_eq!(h.engines[0].controlling(), None);
        assert!(
            !h.trace
                .iter()
                .any(|(_, o)| matches!(o, Output::Park { .. }))
        );
        h.finish();
    }
}

#[test]
fn subsequent_gesture_has_fresh_projection_and_token_after_return() {
    let mut h = Harness::new(true, true, 1000, 1.0);
    h.begin(None);
    h.motion(50.0, 0.0);
    let key = h.key();
    h.finish();
    h.feed(0, Input::Command(Command::Return(key)));
    h.feed(0, Input::Command(Command::Rearm));
    h.now += 1000;
    h.feed(
        0,
        Input::Capture(CaptureEvent::EdgeReleased {
            portal: h.portal,
            at: ms(h.now),
        }),
    );
    h.begin(None);
    h.motion(50.0, 0.0);
    let tokens: Vec<_> = h
        .trace
        .iter()
        .filter_map(|(_, o)| match o {
            Output::SendControl {
                msg:
                    ControlMessage::Projection(Message::StartAt {
                        projection, token, ..
                    }),
                ..
            } => Some((*projection, *token)),
            _ => None,
        })
        .collect();
    assert_eq!(tokens.len(), 2);
    assert_ne!(tokens[0].0, tokens[1].0);
    assert_ne!(tokens[0].1, tokens[1].1);
    h.finish();
}

#[test]
fn synthesized_proxy_client_up_does_not_end_physical_seat_hold() {
    let mut h = Harness::new(true, true, 1000, 1.0);
    h.begin(None);
    h.motion(50.0, 0.0);
    let key = h.key();
    h.feed(
        1,
        Input::Proxy {
            key,
            event: ProxyEvent::Button {
                button: MouseButton::PRIMARY,
                down: false,
                position: PointDevice::zero(),
            },
        },
    );
    assert!(h.seat.physical);
    assert_eq!(h.seat.totals(B), (1, 0));
    h.physical_up();
    assert_eq!(h.seat.totals(B), (1, 1));
    h.finish();
}

#[test]
fn hud_visibility_precedes_capture_and_unavailable_never_settles() {
    for unavailable in [false, true] {
        let mut h = Harness::new(true, true, 1000, 1.0);
        h.hud_ack = false;
        h.start_push();
        assert!(h.capture.is_none());
        assert_eq!(h.seat.totals(A), (1, 0));
        h.feed(
            0,
            Input::Overlay(if unavailable {
                OverlayEvent::Unavailable(crosspane_engine::io::HUD)
            } else {
                OverlayEvent::Visible(crosspane_engine::io::HUD)
            }),
        );
        if unavailable {
            assert!(h.capture.is_none());
            assert_eq!(h.effects(), 0);
        } else {
            h.complete_begin(None);
            assert_eq!(h.seat.totals(A), (1, 1));
        }
        h.finish();
    }
}

#[test]
fn invalidated_activation_ends_late_capture_without_projecting() {
    for changed in [false, true] {
        let mut h = Harness::new(true, true, 1000, 1.0);
        h.start_push();
        let id = h.capture.unwrap();
        if changed {
            h.window = WindowId(999);
            h.drag_at();
        } else {
            h.feed(
                0,
                Input::Capture(CaptureEvent::EdgeReleased {
                    portal: h.portal,
                    at: ms(h.now),
                }),
            );
        }
        assert!(h.capture.is_none());
        // A late successful backend activation really settled the native down: the engine must
        // end that capture, and the backend suppresses the physical tail after its Ended fence.
        h.seat.settle(A);
        h.feed(0, Input::Capture(CaptureEvent::Started { id }));
        h.feed(
            0,
            Input::CaptureBegun {
                id,
                result: Ok(CaptureStart {
                    held_keys: vec![],
                    lock_keys: LockKeys::default(),
                }),
            },
        );
        h.motion(80.0, 0.0);
        assert_eq!(h.effects(), 0);
        assert_eq!(h.engines[0].control_established(), None);
        h.clean_now();
        h.finish();
        assert_eq!(h.seat.totals(A), (1, 1));
    }
}

#[test]
fn cancelled_failed_activation_without_ended_allows_another_gesture_even_after_timeout() {
    for (late, settled) in [(false, false), (false, true), (true, false), (true, true)] {
        let mut h = Harness::new(true, true, 1000, 1.0);
        h.start_push();
        let id = h.capture.unwrap();
        if settled {
            h.settle();
        }
        h.feed(
            0,
            Input::Capture(CaptureEvent::EdgeReleased {
                portal: h.portal,
                at: ms(h.now),
            }),
        );
        assert!(
            h.ended.is_empty(),
            "activation never Started, so no Ended is owed"
        );
        if late {
            h.now += 2000;
            h.feed(0, Input::Tick);
        }
        h.feed(
            0,
            Input::CaptureBegun {
                id,
                result: Err(Failure::Other),
            },
        );
        assert!(h.ended.is_empty());
        assert_eq!(h.effects(), 0);
        h.physical_up();
        h.now += 1000;
        h.begin(None);
        assert!(h.capture.is_some_and(|next| next > id));
        h.motion(50.0, 0.0);
        assert_eq!(h.seat.presses, 1);
        h.finish();
        assert_eq!(h.seat.totals(A), (2, 2));
    }
}

#[test]
fn incoming_session_clears_dwell_and_awaiting_drop_before_portal_ack() {
    for dropping in [false, true] {
        let mut h = Harness::new(true, true, 1000, 1.0);
        if dropping {
            h.begin(Some(Failure::PointerButtonHeld));
        } else {
            h.seat.original_down(A);
            h.drag_at();
        }
        h.portal_ack = false;
        let before = h.trace.len();
        h.feed(
            0,
            Input::Link(LinkEvent::Control {
                peer: B,
                msg: ControlMessage::StartControl {
                    session: crosspane_types::id::SessionId(99),
                    entry_display: DisplayId(1),
                    entry: PointDevice::new(500.0, 500.0),
                    lock_keys: LockKeys::default(),
                },
            }),
        );
        h.now += 1000;
        h.feed(0, Input::Tick);
        assert_eq!(h.engines[0].controlled_by(), Some(B));
        assert!(
            !h.trace[before..]
                .iter()
                .any(|(_, o)| matches!(o, Output::BeginDrag { .. }))
        );
        if dropping {
            assert!(h.trace[before..].iter().any(
                |(_, o)| matches!(o, Output::HideOverlay(id) if *id == crosspane_engine::io::HUD)
            ));
        }
        h.feed(0, Input::Command(Command::Panic));
        h.finish();
    }
}

#[test]
fn tail_ends_with_capture_and_a_fresh_capture_click_is_not_swallowed() {
    let mut h = Harness::new(true, true, 1000, 1.0);
    h.begin(None);
    h.feed(
        0,
        Input::Capture(CaptureEvent::Key {
            usage: HidUsage::keyboard(0x29),
            down: true,
            at: ms(h.now),
        }),
    );
    h.feed(0, Input::Command(Command::ReleaseControl));
    assert!(h.seat.physical);
    h.clean_now();
    h.physical_up();
    assert_eq!(h.seat.totals(A), (1, 1));
    h.ordinary = true;
    h.feed(0, Input::Command(Command::Rearm));
    h.now += 1000;
    h.feed(
        0,
        Input::Capture(CaptureEvent::EdgeReleased {
            portal: h.portal,
            at: ms(h.now),
        }),
    );
    ordinary_cross(&mut h);
    h.seat.captured_down();
    h.feed(
        0,
        Input::Capture(CaptureEvent::Button {
            button: MouseButton::PRIMARY,
            down: true,
            at: ms(h.now),
        }),
    );
    assert!(h.seat.has_hold(B));
    h.physical_up();
    assert!(!h.seat.has_hold(B));
    assert_eq!(h.seat.totals(B), (1, 1));
    h.finish();
}

#[test]
fn second_gesture_after_uncaptured_ending_reuses_one_native_hold() {
    let mut h = Harness::new(true, true, 1000, 1.0);
    h.hud_ack = false;
    h.start_push();
    h.feed(
        0,
        Input::Overlay(OverlayEvent::Unavailable(crosspane_engine::io::HUD)),
    );
    assert!(h.seat.physical);
    assert_eq!(h.effects(), 0);
    h.clean_now();
    h.now += 1000;
    h.hud_ack = true;
    h.feed(
        0,
        Input::Capture(CaptureEvent::EdgeReleased {
            portal: h.portal,
            at: ms(h.now),
        }),
    );
    h.begin(None);
    h.motion(50.0, 0.0);
    assert_eq!(h.seat.presses, 1);
    h.finish();
    assert_eq!(h.seat.totals(A), (1, 1));
}

#[test]
fn replay_at_a_portal_clamps_normal_motion_and_keeps_tangent() {
    let mut h = Harness::new(true, true, 1000, 1.0);
    h.placed = false;
    h.begin(None);
    h.motion(50.0, 0.0);
    h.motion(-100.0, 20.0);
    h.ready(h.token());
    let position = h
        .trace
        .iter()
        .rev()
        .find_map(|(_, o)| match o {
            Output::SendMotion { msg, .. } => Some(msg.position),
            _ => None,
        })
        .unwrap();
    assert_eq!(position, PointDevice::new(0.0, 420.0));
    h.finish();
}

#[test]
fn cancelled_arm_does_not_swallow_a_new_sessions_ordinary_click() {
    let mut h = Harness::new(true, true, 1000, 1.0);
    h.arm_ack = false;
    h.begin(None);
    h.motion(50.0, 0.0);
    h.feed(0, Input::Command(Command::ReleaseControl));
    h.clean_now();
    h.physical_up();
    h.ordinary = true;
    let session = crosspane_types::id::SessionId(99);
    h.feed(
        1,
        Input::Link(LinkEvent::Control {
            peer: A,
            msg: ControlMessage::StartControl {
                session,
                entry_display: DisplayId(1),
                entry: PointDevice::new(500.0, 500.0),
                lock_keys: LockKeys::default(),
            },
        }),
    );
    h.seat.ordinary_capture();
    h.seat.captured_down();
    h.feed(
        1,
        Input::Link(LinkEvent::Input {
            peer: A,
            msg: InputMessage::Button {
                session,
                seq: 1,
                button: MouseButton::PRIMARY,
                down: true,
            },
        }),
    );
    assert!(h.seat.has_hold(B));
    assert_eq!(h.seat.physical_up(), Some(PhysicalRelease::Captured));
    h.feed(
        1,
        Input::Link(LinkEvent::Input {
            peer: A,
            msg: InputMessage::Button {
                session,
                seq: 2,
                button: MouseButton::PRIMARY,
                down: false,
            },
        }),
    );
    assert_eq!(h.seat.totals(B), (1, 1));
    assert!(!h.seat.has_hold(B));
    h.feed(
        1,
        Input::Link(LinkEvent::Control {
            peer: A,
            msg: ControlMessage::EndControl {
                session,
                reason: crosspane_protocol::msg::EndReason::Released,
            },
        }),
    );
    h.finish();
}

#[test]
fn delayed_arm_acks_keep_both_clocks_and_existing_key_lease_alive() {
    let mut h = Harness::new(true, true, 1000, 1.0);
    h.begin(None);
    h.feed(
        0,
        Input::Capture(CaptureEvent::Key {
            usage: HidUsage::keyboard(4),
            down: true,
            at: ms(h.now),
        }),
    );
    h.arm_ack = false;
    h.motion(50.0, 0.0);
    for _ in 0..4 {
        h.now += 60;
        h.feed(0, Input::Tick);
        h.feed(1, Input::Tick);
    }
    assert_eq!(h.engines[0].control_established(), Some(B));
    assert!(h.keys.contains(&(B, HidUsage::keyboard(4))));
    assert_eq!(h.seat.totals(B).0, 0);
    let (i, key, token) = h.pending_arms.pop().unwrap();
    h.feed(
        i,
        Input::DragArmed {
            key,
            token,
            ok: true,
        },
    );
    assert_eq!(h.seat.totals(B).0, 1);
    h.finish();
}

#[test]
fn all_projection_and_return_effects_are_counted_including_legacy_paths() {
    let mut h = Harness::new(true, true, 1000, 1.0);
    h.feed(
        0,
        Input::Command(Command::Project {
            window: WindowId(10),
            to: B,
            place: None,
        }),
    );
    assert_eq!(h.commits(), 1);
    assert!(h.effects() >= 3);
    let before = h.effects();
    h.feed(
        0,
        Input::Command(Command::Return(ProjectionKey {
            source: A,
            projection: ProjectionId(1),
        })),
    );
    assert!(h.effects() > before);
    assert!(
        h.trace
            .iter()
            .any(|(_, o)| matches!(o, Output::Restore { .. }))
    );
    assert!(
        h.trace
            .iter()
            .any(|(_, o)| matches!(o, Output::CloseProxy { .. }))
    );
    h.finish();
}

#[test]
fn release_escape_lock_and_refusal_between_settlement_and_activation_never_project() {
    for stage in 0..3 {
        for ending in 0..4 {
            let mut h = Harness::new(true, true, 1000, 1.0);
            h.handshake_ack = false;
            h.start_push();
            let id = h.capture.unwrap();
            h.settle();
            if stage >= 1 {
                h.started();
            }
            if stage >= 2 {
                h.feed(
                    0,
                    Input::CaptureBegun {
                        id,
                        result: Ok(CaptureStart {
                            held_keys: vec![],
                            lock_keys: LockKeys::default(),
                        }),
                    },
                );
            }
            match ending {
                0 => h.physical_up(),
                1 => {
                    if h.started {
                        h.feed(
                            0,
                            Input::Capture(CaptureEvent::Key {
                                usage: HidUsage::keyboard(0x29),
                                down: true,
                                at: ms(h.now),
                            }),
                        );
                    } else {
                        h.feed(
                            0,
                            Input::Capture(CaptureEvent::EdgeReleased {
                                portal: h.portal,
                                at: ms(h.now),
                            }),
                        );
                    }
                }
                2 => h.feed(
                    0,
                    Input::Session(SessionEvent::State(SessionState {
                        lock: LockState::Locked,
                        ..OPEN
                    })),
                ),
                _ if stage < 2 => h.complete_begin(Some(Failure::Other)),
                _ => {
                    let (
                        _,
                        Input::Link(LinkEvent::Control {
                            msg: ControlMessage::ControlStarted { session },
                            ..
                        }),
                    ) = h.pending_handshakes.first().unwrap()
                    else {
                        panic!("missing handshake");
                    };
                    h.feed(
                        0,
                        Input::Link(LinkEvent::Control {
                            peer: B,
                            msg: ControlMessage::ControlRefused {
                                session: *session,
                                reason: crosspane_protocol::msg::Refusal::Permission,
                            },
                        }),
                    );
                }
            }
            if h.capture.is_some() && !h.started {
                h.started();
            }
            h.feed(
                0,
                Input::CaptureBegun {
                    id,
                    result: Ok(CaptureStart {
                        held_keys: vec![],
                        lock_keys: LockKeys::default(),
                    }),
                },
            );
            for (i, input) in std::mem::take(&mut h.pending_handshakes) {
                h.feed(i, input);
            }
            h.motion(80.0, 0.0);
            assert_eq!(h.effects(), 0, "stage {stage}, ending {ending}");
            h.clean_now();
            h.finish();
            assert_eq!(h.seat.totals(A), (1, 1));
        }
    }
}

fn ordinary_cross(h: &mut Harness) {
    for dt in [0, 250] {
        h.now += dt;
        h.feed(
            0,
            Input::Capture(CaptureEvent::EdgePressed {
                portal: h.portal,
                position: 0.5,
                at: ms(h.now),
            }),
        );
    }
    assert_eq!(h.engines[0].control_established(), Some(B));
}

// Preserved pre-drag E1 trace: visible HUD -> ordinary handshake -> capture; absolute motion,
// reliable key/button pairs, then release. Values are literal legacy outcomes, not a second engine.
fn legacy_golden(motions: &[(u8, u8)]) {
    let mut h = Harness::new(false, true, 1000, 1.0);
    h.ordinary = true;
    h.drag_at();
    ordinary_cross(&mut h);
    let mut expected = vec![
        "hud".to_string(),
        "start".into(),
        "inject-motion:1:0,500".into(),
        "inject-lock:None,None,None".into(),
        "capture".into(),
    ];
    let (mut x, mut y) = (0u32, 500u32);
    for (pair, &(dx, dy)) in motions.iter().enumerate() {
        let count = pair as u32 + 1;
        h.drag_at();
        h.motion(f64::from(dx), f64::from(dy));
        x += u32::from(dx);
        y += u32::from(dy);
        expected.push(format!("motion:{x},{y}"));
        expected.push(format!("inject-motion:1:{x},{y}"));
        for down in [true, false] {
            h.feed(
                0,
                Input::Capture(CaptureEvent::Key {
                    usage: HidUsage::keyboard(4),
                    down,
                    at: ms(h.now),
                }),
            );
            expected.push(format!("key:4:{down}"));
            expected.push(format!("inject-key:4:{down}"));
        }
        assert_eq!(h.key_totals[&(B, HidUsage::keyboard(4))], (count, count));
        assert!(!h.keys.contains(&(B, HidUsage::keyboard(4))));
        h.seat.captured_down();
        h.feed(
            0,
            Input::Capture(CaptureEvent::Button {
                button: MouseButton::PRIMARY,
                down: true,
                at: ms(h.now),
            }),
        );
        expected.push("primary:true".into());
        expected.push("inject-primary:true".into());
        h.physical_up();
        expected.push("primary:false".into());
        expected.push("inject-primary:false".into());
        assert_eq!(h.seat.totals(B), (count, count));
        assert!(
            !h.seat.has_hold(B),
            "ordinary up must settle before session teardown"
        );
    }
    h.feed(0, Input::Command(Command::ReleaseControl));
    expected.extend(["capture-end".into(), "end".into()]);
    let actual: Vec<_> = h
        .trace
        .iter()
        .filter_map(|(i, o)| {
            if *i != 0 {
                return match o {
                    Output::Inject { cmd, .. } => Some(match cmd {
                        InjectCmd::MoveTo { display, position } => format!(
                            "inject-motion:{}:{:.0},{:.0}",
                            display.0, position.x, position.y
                        ),
                        InjectCmd::LockKeys(keys) => format!(
                            "inject-lock:{:?},{:?},{:?}",
                            keys.caps_lock, keys.num_lock, keys.scroll_lock
                        ),
                        InjectCmd::Key { usage, down } => format!("inject-key:{}:{down}", usage.id),
                        InjectCmd::Button {
                            button: MouseButton::PRIMARY,
                            down,
                        } => format!("inject-primary:{down}"),
                        InjectCmd::ReleaseAll => "inject-release-all".into(),
                        other => format!("unexpected-injection:{other:?}"),
                    }),
                    _ => None,
                };
            }
            match o {
                Output::ShowOverlay { id, .. } if *id == crosspane_engine::io::HUD => {
                    Some("hud".to_string())
                }
                Output::SendControl {
                    msg: ControlMessage::StartControl { .. },
                    ..
                } => Some("start".into()),
                Output::BeginCapture { .. } => Some("capture".into()),
                Output::EndCapture { .. } => Some("capture-end".into()),
                Output::SendControl {
                    msg: ControlMessage::EndControl { .. },
                    ..
                } => Some("end".into()),
                Output::SendMotion { msg, .. } => Some(format!(
                    "motion:{:.0},{:.0}",
                    msg.position.x, msg.position.y
                )),
                Output::SendInput {
                    msg: InputMessage::Key { usage, down, .. },
                    ..
                } => Some(format!("key:{}:{down}", usage.id)),
                Output::SendInput {
                    msg:
                        InputMessage::Button {
                            button: MouseButton::PRIMARY,
                            down,
                            ..
                        },
                    ..
                } => Some(format!("primary:{down}")),
                Output::BeginDrag { .. } => Some("unexpected-drag".into()),
                _ => None,
            }
        })
        .collect();
    assert_eq!(actual, expected);
    assert_eq!(h.effects(), 0);
    h.finish();
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn random_drag_interleavings_balance_original_local_and_remote_presses(back in any::<bool>(), events in prop::collection::vec((0u8..25, 0u64..120), 0..100)) {
        let mut h = Harness::new(true, true, 1500, 1.5); h.placed = false; h.arm_ack = false; h.hud_ack = false; h.handshake_ack = false;
        if back { h.proxy_from_b(); }
        h.start_push();
        let mut aborted_before_commit = false; let mut safety_ending = false; let mut activation = None;
        for (event, dt) in events {
            h.now += dt;
            safety_ending |= back && matches!(event, 9 | 10 | 13 | 15 | 16);
            let release_commits_drop = h.awaiting_drop && h.seat.physical;
            aborted_before_commit |= h.effects() == 0 && (matches!(event, 8 | 10 | 13 | 15) || (event == 24 && !h.started)
                || (matches!(event, 2 | 11) && !release_commits_drop) || (event == 3 && h.seat.presses == 0));
            match event {
                0 => h.motion(0.0, 30.0), 1 => h.motion(30.0, 0.0), 2 => h.physical_up(),
                3 => { if h.started { h.feed(0, Input::Capture(CaptureEvent::Key { usage: HidUsage::keyboard(0x29), down: true, at: ms(h.now) })); } else { h.feed(0, Input::Capture(CaptureEvent::EdgeReleased { portal: h.portal, at: ms(h.now) })); } },
                4 => { if h.started { h.feed(0, Input::Capture(CaptureEvent::Key { usage: HidUsage::keyboard(0x29), down: false, at: ms(h.now) })); } },
                5 => { if h.opened.is_some() { h.place_now(); } }
                6 => { if h.opened.is_some() { h.ready(h.token() + 1); } }
                7 => { if let Some((i, key, token)) = h.pending_arms.pop() { h.feed(i, Input::DragArmed { key, token, ok: dt % 2 == 0 }); } }
                8 => h.feed(0, Input::Command(Command::ReleaseControl)),
                9 => h.feed(1, Input::Command(Command::Panic)),
                10 => h.feed(0, Input::Session(SessionEvent::State(SessionState { lock: LockState::Locked, ..OPEN }))),
                11 => h.dropped(),
                13 => { h.feed(0, Input::Link(LinkEvent::Closed { peer: B, error: LinkError::Closed })); h.feed(1, Input::Link(LinkEvent::Closed { peer: A, error: LinkError::Closed })); }
                14 => { if h.opened.is_some() { let key = h.key(); h.feed(1, control(A, Message::DragCancel { projection: key.projection, token: h.token() })); } }
                15 => h.feed(0, Input::Grants(BTreeMap::new())),
                16 => h.feed(1, Input::Grants([(A, [Capability::InputAccept].into_iter().collect())].into_iter().collect())),
                17 => { if let Some(i) = h.pending_huds.pop() { h.feed(i, Input::Overlay(OverlayEvent::Visible(crosspane_engine::io::HUD))); } }
                18 => { if let Some(id) = h.capture { activation = Some(id); if !h.settled && h.seat.has_hold(A) && h.seat.physical { h.settle(); } } }
                19 => { if h.capture.is_some() && h.settled && !h.started { h.started(); } }
                20 => { if let Some(id) = h.capture.filter(|_| h.settled && !h.begun) { h.feed(0, Input::CaptureBegun { id, result: Ok(CaptureStart { held_keys: vec![], lock_keys: LockKeys::default() }) }); } else if h.capture.is_none() && let Some(id) = activation { h.feed(0, Input::CaptureBegun { id, result: Ok(CaptureStart { held_keys: vec![], lock_keys: LockKeys::default() }) }); } }
                21 => { if h.capture.is_some() && !h.begun { let failure = if !h.settled && dt % 2 == 0 { Failure::PointerButtonHeld } else { Failure::Other }; h.complete_begin(Some(failure)); } }
                22 => { if let Some((i, input)) = h.pending_handshakes.pop() { h.feed(i, input); } }
                24 => { if !h.started { h.window = WindowId(999); h.drag_at(); } }
                _ => { h.feed(0, Input::Tick); h.feed(1, Input::Tick); }
            }
            prop_assert!(h.seat.presses <= 1);
            if aborted_before_commit {
                prop_assert_eq!(h.commits(), 0);
                // An existing Back projection must still restore on panic, lock or link loss.
                // Those required safety effects are distinct from a drag return; ordinary aborts
                // have no projection/return effect of any kind.
                if !safety_ending { prop_assert_eq!(h.effects(), 0); }
            }
            if matches!(event, 8 | 9 | 10 | 13 | 14 | 15) { h.clean_now(); }
        }
        h.finish(); prop_assert!(h.seat.balanced());
    }
    #[test]
    fn disabled_drag_random_routes_match_preserved_legacy_golden(motions in prop::collection::vec((0u8..10, 0u8..5), 1..20)) {
        legacy_golden(&motions);
    }
}
