#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crosspane_engine::e2::E2;
use crosspane_engine::{
    Command, Engine, EngineConfig, Failure, InjectCmd, InjectId, Input, Notice, Output,
    ProjectionKey, ProxyEvent,
};
use crosspane_input::Held;
use crosspane_input::journal::{Journal, JournalError, MemoryJournal};
use crosspane_platform::{
    CaptureTarget, LockState, Parked, ParkingKind as PlatformParking, SessionEvent, SessionState,
    StreamEndReason, StreamId, WindowEvent, WindowInfo, WindowRole, WindowState,
};
use crosspane_protocol::link::{LinkError, LinkEvent};
use crosspane_protocol::msg::{Capability, ControlMessage, InputMessage, Refusal};
use crosspane_protocol::projection::{
    ParkingKind, ProjInput, ProjectionEndReason as Reason, ProjectionMessage as Message,
    WindowSummary,
};
use crosspane_types::color::ColorSpace;
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{
    DisplayGeometry, PixelRect, PixelSize, PointDevice, PointLogical, RectLogical, SizeLogical,
    SizeMm, VectorLogical,
};
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::id::{DisplayId, NodeId, ProjectionId, WindowId};
use crosspane_types::input::{ScrollDelta, ScrollPhase};
use crosspane_types::time::MonoTime;
use proptest::prelude::*;

const A: NodeId = NodeId([1; 32]);
const B: NodeId = NodeId([2; 32]);
const C: NodeId = NodeId([3; 32]);
const WINDOW: WindowId = WindowId(10);
const WINDOW2: WindowId = WindowId(11);
const DISPLAY: DisplayId = DisplayId(4);
const KEY: HidUsage = HidUsage::keyboard(4);
const BUTTON: MouseButton = MouseButton(1);
const ID: ProjectionId = ProjectionId(1);
const OPEN: SessionState = SessionState {
    lock: LockState::Unlocked,
    active: Some(true),
};

fn ms(n: u64) -> MonoTime {
    MonoTime::from_nanos(n * 1_000_000)
}
fn key(source: NodeId) -> ProjectionKey {
    ProjectionKey {
        source,
        projection: ID,
    }
}
fn size() -> PixelSize {
    PixelSize::new(640, 480)
}
fn control(peer: NodeId, msg: Message) -> Input {
    Input::Link(LinkEvent::Control {
        peer,
        msg: ControlMessage::Projection(msg),
    })
}
fn input(peer: NodeId, msg: ProjInput) -> Input {
    Input::Link(LinkEvent::Input {
        peer,
        msg: InputMessage::Proj(msg),
    })
}
fn start() -> Message {
    Message::Start {
        projection: ID,
        window: WindowSummary {
            title: "fixture".into(),
            app_id: "test".into(),
        },
        size: size(),
    }
}
fn accepted() -> Message {
    Message::Accepted {
        projection: ID,
        size: size(),
        scale: 2.0,
    }
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
fn press(seq: u32, down: bool) -> ProjInput {
    ProjInput::Key {
        projection: ID,
        seq,
        usage: KEY,
        down,
    }
}
fn button(seq: u32, down: bool, position: PointDevice) -> ProjInput {
    ProjInput::Button {
        projection: ID,
        seq,
        button: BUTTON,
        down,
        position,
    }
}
fn heartbeat(seq: u32, keys: Vec<HidUsage>) -> ProjInput {
    ProjInput::Held {
        projection: ID,
        seq,
        keys,
        buttons: vec![],
    }
}
fn delta() -> ScrollDelta {
    ScrollDelta {
        pixels: Some(VectorLogical::new(1.0, 2.0)),
        v120_x: 0,
        v120_y: 120,
        phase: ScrollPhase::Discrete,
        stop_x: false,
        stop_y: false,
    }
}

fn window(id: WindowId) -> WindowInfo {
    WindowInfo {
        id,
        title: "fixture".into(),
        app_id: "test".into(),
        pid: None,
        display: Some(DISPLAY),
        frame: RectLogical::new(PointLogical::zero(), SizeLogical::new(320.25, 240.25)),
        state: WindowState::Normal,
        role: WindowRole::Toplevel,
        parent: None,
    }
}

fn parked(window: WindowId, kind: PlatformParking, size: PixelSize) -> Parked {
    Parked {
        window,
        kind,
        display: DISPLAY,
        content: PixelRect::new(
            crosspane_types::geom::euclid::Point2D::new(20, 30),
            crosspane_types::geom::euclid::Point2D::new(
                20 + size.width as i32,
                30 + size.height as i32,
            ),
        ),
    }
}

#[derive(Default)]
struct JournalState {
    held: BTreeSet<Held>,
    fail_up: bool,
    fail_down: bool,
}
#[derive(Clone, Default)]
struct SharedJournal(Arc<Mutex<JournalState>>);
impl Journal for SharedJournal {
    fn record_down(&mut self, item: Held) -> Result<(), JournalError> {
        let mut state = self.0.lock().unwrap();
        state.held.insert(item);
        if state.fail_down {
            return Err(std::io::Error::other("test down").into());
        }
        Ok(())
    }
    fn record_up(&mut self, item: Held) -> Result<(), JournalError> {
        let mut state = self.0.lock().unwrap();
        if state.fail_up {
            return Err(std::io::Error::other("test up").into());
        }
        state.held.remove(&item);
        Ok(())
    }
    fn held(&self) -> Result<Vec<Held>, JournalError> {
        Ok(self.0.lock().unwrap().held.iter().copied().collect())
    }
}

struct Fixture {
    e2: E2,
    journal: SharedJournal,
}
impl Fixture {
    fn new(node: NodeId) -> Self {
        let journal = SharedJournal::default();
        let (e2, out) =
            E2::new(&EngineConfig::new(node), Box::new(journal.clone()), ms(0)).unwrap();
        assert!(out.is_empty());
        Self { e2, journal }
    }
    fn ready(node: NodeId, peer: NodeId) -> Self {
        let mut f = Self::new(node);
        f.handle(Input::Session(SessionEvent::State(OPEN)), 0);
        f.handle(Input::PeerUp { peer }, 0);
        f.handle(
            Input::Grants(
                [(
                    peer,
                    [Capability::WindowShare, Capability::WindowPresent].into(),
                )]
                .into(),
            ),
            0,
        );
        f.handle(Input::Windows(WindowEvent::Added(window(WINDOW))), 0);
        f.handle(Input::Windows(WindowEvent::Focused(Some(WINDOW))), 0);
        f
    }
    fn handle(&mut self, input: Input, now: u64) -> Vec<Output> {
        self.at(input, ms(now))
    }
    fn at(&mut self, input: Input, now: MonoTime) -> Vec<Output> {
        let mut out = Vec::new();
        self.e2.handle(&input, now, &mut out);
        out
    }
    fn confirm(&mut self, out: &[Output], ok: bool, now: u64) -> Vec<Output> {
        let mut more = Vec::new();
        for (id, _) in injections(out) {
            more.extend(self.handle(Input::InjectDone { id, ok }, now));
        }
        more
    }
    fn source(kind: PlatformParking) -> Self {
        let mut f = Self::ready(A, B);
        f.handle(
            Input::Command(Command::Project {
                window: WINDOW,
                to: B,
            }),
            0,
        );
        f.handle(control(B, accepted()), 0);
        f.handle(
            Input::Parked {
                window: WINDOW,
                result: Ok(parked(WINDOW, kind, size())),
            },
            0,
        );
        f.handle(
            Input::CaptureStarted {
                projection: ID,
                result: Ok(StreamId(1)),
            },
            0,
        );
        f
    }
    fn destination() -> Self {
        let mut f = Self::ready(B, A);
        f.handle(control(A, start()), 0);
        f.handle(
            Input::ProxyOpened {
                key: key(A),
                result: Ok((size(), 2.0)),
            },
            0,
        );
        f
    }
    fn proxy(&mut self, event: ProxyEvent, now: u64) -> Vec<Output> {
        self.handle(Input::Proxy { key: key(A), event }, now)
    }
    fn held(&self) -> Vec<Held> {
        self.journal.held().unwrap()
    }
}

fn injections(out: &[Output]) -> Vec<(InjectId, InjectCmd)> {
    out.iter()
        .filter_map(|o| match o {
            Output::Inject { id, cmd } => Some((*id, cmd.clone())),
            _ => None,
        })
        .collect()
}
fn commands(out: &[Output]) -> Vec<InjectCmd> {
    injections(out).into_iter().map(|(_, cmd)| cmd).collect()
}
fn messages(out: &[Output]) -> Vec<Message> {
    out.iter()
        .filter_map(|o| match o {
            Output::SendControl {
                msg: ControlMessage::Projection(m),
                ..
            } => Some(m.clone()),
            _ => None,
        })
        .collect()
}
fn inputs(out: &[Output]) -> Vec<ProjInput> {
    out.iter()
        .filter_map(|o| match o {
            Output::SendInput {
                msg: InputMessage::Proj(m),
                ..
            } => Some(m.clone()),
            _ => None,
        })
        .collect()
}
fn up() -> InjectCmd {
    InjectCmd::Key {
        usage: KEY,
        down: false,
    }
}

#[test]
fn source_happy_path_resize_coalescing_and_ordered_close() {
    let mut f = Fixture::ready(A, B);
    f.handle(
        Input::LocalDisplays(vec![DisplayInfo {
            id: DISPLAY,
            name: "test".into(),
            geometry: DisplayGeometry {
                physical_size: SizeMm::new(200.0, 100.0),
                pixel_size: size(),
                scale: 2.0,
                logical_origin: PointLogical::zero(),
            },
            refresh_millihz: 60_000,
            color_space: ColorSpace::Srgb,
            hdr: false,
        }]),
        0,
    );
    let offer = f.handle(
        Input::Command(Command::Project {
            window: WINDOW,
            to: B,
        }),
        0,
    );
    assert!(
        matches!(messages(&offer).as_slice(), [Message::Start { projection: ID, size, .. }] if *size == PixelSize::new(641, 481))
    );
    assert_eq!(f.e2.next_deadline(), Some(ms(10_000)));
    assert_eq!(
        f.handle(control(B, accepted()), 1),
        vec![Output::Park {
            window: WINDOW,
            size: size(),
            scale: 2.0
        }]
    );
    let p = parked(WINDOW, PlatformParking::Twin, size());
    let out = f.handle(
        Input::Parked {
            window: WINDOW,
            result: Ok(p),
        },
        2,
    );
    assert_eq!(
        out[0],
        Output::StartCapture {
            projection: ID,
            peer: B,
            target: CaptureTarget::Display(DISPLAY),
            crop: Some(p.content),
            max_fps: 60
        }
    );
    assert_eq!(
        messages(&out),
        vec![Message::Geometry {
            projection: ID,
            size: size(),
            parking: ParkingKind::Twin
        }]
    );
    assert!(matches!(
        out[2],
        Output::Notice(Notice::ProjectionStarted { .. })
    ));
    assert!(f.handle(input(B, press(1, true)), 2).is_empty());
    f.handle(
        Input::CaptureStarted {
            projection: ID,
            result: Ok(StreamId(7)),
        },
        3,
    );
    assert_eq!(
        f.handle(
            control(
                B,
                Message::Focus {
                    projection: ID,
                    focused: true
                }
            ),
            3
        ),
        vec![Output::ActivateWindow { window: WINDOW }]
    );
    assert!(
        f.handle(
            control(
                B,
                Message::Focus {
                    projection: ID,
                    focused: false
                }
            ),
            3
        )
        .is_empty()
    );
    assert_eq!(
        f.handle(control(B, Message::KeyFrameRequest { projection: ID }), 3),
        vec![Output::RequestKeyFrame { projection: ID }]
    );
    let mut changed = window(WINDOW);
    changed.title = "new title".into();
    assert_eq!(
        messages(&f.handle(Input::Windows(WindowEvent::Changed(changed)), 3)),
        vec![Message::Title {
            projection: ID,
            title: "new title".into()
        }]
    );
    let resized = PixelSize::new(800, 600);
    assert_eq!(
        f.handle(
            control(
                B,
                Message::Resize {
                    projection: ID,
                    size: resized,
                    scale: 1.5
                }
            ),
            4
        ),
        vec![Output::ResizeParked {
            window: WINDOW,
            size: resized,
            scale: 1.5
        }]
    );
    f.handle(
        control(
            B,
            Message::Resize {
                projection: ID,
                size: size(),
                scale: 1.0,
            },
        ),
        5,
    );
    f.handle(
        control(
            B,
            Message::Resize {
                projection: ID,
                size: PixelSize::new(900, 700),
                scale: 1.0,
            },
        ),
        6,
    );
    let p = parked(WINDOW, PlatformParking::Twin, resized);
    let out = f.handle(
        Input::Parked {
            window: WINDOW,
            result: Ok(p),
        },
        7,
    );
    assert_eq!(
        out[0],
        Output::SetCaptureCrop {
            stream: StreamId(7),
            crop: Some(p.content)
        }
    );
    assert_eq!(
        messages(&out),
        vec![Message::Geometry {
            projection: ID,
            size: resized,
            parking: ParkingKind::Twin
        }]
    );
    assert_eq!(
        out.last(),
        Some(&Output::ResizeParked {
            window: WINDOW,
            size: PixelSize::new(900, 700),
            scale: 1.0
        })
    );
    f.handle(
        Input::Parked {
            window: WINDOW,
            result: Ok(p),
        },
        8,
    );
    f.handle(Input::Windows(WindowEvent::Focused(Some(WINDOW))), 9);
    f.handle(input(B, press(2, true)), 9);
    let out = f.handle(
        control(
            B,
            Message::Close {
                projection: ID,
                reason: Reason::Returned,
            },
        ),
        10,
    );
    assert_eq!(commands(&out), vec![up()]);
    assert_eq!(
        &out[1..],
        &[
            Output::StopCapture {
                stream: StreamId(7)
            },
            Output::Restore { window: WINDOW },
            Output::Notice(Notice::ProjectionEnded {
                key: key(A),
                reason: Reason::Returned
            })
        ]
    );
    f.confirm(&out, true, 10);
    assert!(f.held().is_empty());
    assert_eq!(f.e2.next_deadline(), None);
}

#[test]
fn source_refusals_and_offer_timeout() {
    let mut f = Fixture::new(A);
    let project = Input::Command(Command::Project {
        window: WINDOW,
        to: B,
    });
    assert_eq!(
        f.handle(project.clone(), 0),
        vec![Output::Notice(Notice::ProjectionRefused {
            peer: B,
            reason: Refusal::Permission
        })]
    );
    f.handle(
        Input::Grants([(B, [Capability::WindowShare].into())].into()),
        0,
    );
    assert_eq!(
        f.handle(project.clone(), 0),
        vec![Output::Notice(Notice::ProjectionRefused {
            peer: B,
            reason: Refusal::Locked
        })]
    );
    f.handle(Input::Session(SessionEvent::State(OPEN)), 0);
    assert_eq!(
        f.handle(project.clone(), 0),
        vec![Output::Notice(Notice::ProjectionRefused {
            peer: B,
            reason: Refusal::Busy
        })]
    );
    f.handle(Input::PeerUp { peer: B }, 0);
    assert!(messages(&f.handle(project.clone(), 0)).is_empty()); // unknown window
    f.handle(Input::Windows(WindowEvent::Added(window(WINDOW))), 0);
    assert_eq!(messages(&f.handle(project.clone(), 0)).len(), 1);
    assert_eq!(
        f.handle(project.clone(), 0),
        vec![Output::Notice(Notice::ProjectionRefused {
            peer: B,
            reason: Refusal::Busy
        })]
    );
    let deadline = f.e2.next_deadline().unwrap();
    let out = f.at(Input::Tick, deadline);
    assert!(out.contains(&Output::Notice(Notice::ProjectionRefused {
        peer: B,
        reason: Refusal::Busy
    })));
    assert!(matches!(
        messages(&out).as_slice(),
        [Message::End {
            projection: ID,
            reason: Reason::Failed
        }]
    ));
    assert_eq!(f.e2.next_deadline(), None);
    assert!(matches!(
        messages(&f.handle(project, 10_001)).as_slice(),
        [Message::Start {
            projection: ProjectionId(2),
            ..
        }]
    ));
    let out = f.handle(
        control(
            B,
            Message::Refused {
                projection: ProjectionId(2),
                reason: Refusal::Permission,
            },
        ),
        10_002,
    );
    assert!(out.contains(&Output::Notice(Notice::ProjectionRefused {
        peer: B,
        reason: Refusal::Permission
    })));
    assert!(messages(&out).is_empty());
}

#[test]
fn twin_and_mirror_mapping_clamping_focus_and_sequences() {
    for kind in [PlatformParking::Twin, PlatformParking::Mirror] {
        let mut f = Fixture::source(kind);
        let out = f.handle(
            input(
                B,
                ProjInput::Motion {
                    projection: ID,
                    seq: 1,
                    position: PointDevice::new(5.0, 7.0),
                },
            ),
            0,
        );
        assert_eq!(
            commands(&out),
            vec![InjectCmd::MoveTo {
                display: DISPLAY,
                position: PointDevice::new(25.0, 37.0)
            }]
        );
        let out = f.handle(
            input(B, button(2, true, PointDevice::new(-100.0, 1000.0))),
            0,
        );
        assert_eq!(
            commands(&out),
            vec![
                InjectCmd::MoveTo {
                    display: DISPLAY,
                    position: PointDevice::new(20.0, 509.0)
                },
                InjectCmd::Button {
                    button: BUTTON,
                    down: true
                }
            ]
        );
        f.handle(Input::Windows(WindowEvent::Focused(Some(WindowId(99)))), 0);
        let out = f.handle(input(B, press(3, true)), 0);
        assert_eq!(out[0], Output::ActivateWindow { window: WINDOW });
        assert!(commands(&out).is_empty());
        assert!(!f.held().contains(&Held::Key(KEY)));
        assert!(f.handle(input(B, press(3, false)), 0).is_empty());
        assert!(f.handle(input(C, press(100, false)), 0).is_empty());
        f.handle(Input::Windows(WindowEvent::Focused(Some(WINDOW))), 0);
        let out = f.handle(input(B, press(4, true)), 0);
        assert_eq!(
            commands(&out),
            vec![InjectCmd::Key {
                usage: KEY,
                down: true
            }]
        );
        let out = f.handle(input(B, press(5, false)), 0);
        assert_eq!(out.len(), 1);
        assert_eq!(commands(&out), vec![up()]);
        let out = f.handle(
            input(
                B,
                ProjInput::Scroll {
                    projection: ID,
                    seq: 6,
                    position: PointDevice::new(8.0, 9.0),
                    delta: delta(),
                },
            ),
            0,
        );
        assert_eq!(
            commands(&out),
            vec![
                InjectCmd::MoveTo {
                    display: DISPLAY,
                    position: PointDevice::new(28.0, 39.0)
                },
                InjectCmd::Scroll(delta())
            ]
        );
    }
    let mut f = Fixture::ready(A, B);
    f.handle(
        Input::Command(Command::Project {
            window: WINDOW,
            to: B,
        }),
        0,
    );
    f.handle(control(B, accepted()), 0);
    let out = f.handle(
        Input::Parked {
            window: WINDOW,
            result: Ok(parked(WINDOW, PlatformParking::Mirror, size())),
        },
        0,
    );
    assert_eq!(
        out[0],
        Output::StartCapture {
            projection: ID,
            peer: B,
            target: CaptureTarget::Window(WINDOW),
            crop: None,
            max_fps: 60
        }
    );
}

#[test]
fn lease_and_missing_heartbeat_release_at_exact_deadline() {
    let mut f = Fixture::source(PlatformParking::Twin);
    f.handle(input(B, press(1, true)), 0);
    f.handle(input(B, heartbeat(2, vec![KEY])), 50);
    let deadline = f.e2.next_deadline().unwrap();
    assert_eq!(deadline.as_nanos(), ms(350).as_nanos() + 1);
    assert!(commands(&f.handle(Input::Tick, 350)).is_empty());
    let out = f.at(Input::Tick, deadline);
    assert_eq!(commands(&out), vec![up()]);
    assert_eq!(f.held(), vec![Held::Key(KEY)]);
    f.confirm(&out, true, 351);
    assert!(f.held().is_empty());
    f.handle(input(B, press(3, true)), 400);
    let out = f.handle(input(B, heartbeat(4, vec![])), 450);
    assert_eq!(commands(&out), vec![up()]);
    f.confirm(&out, true, 450);
    assert_eq!(f.e2.next_deadline(), None);
}

#[test]
fn failed_release_retries_after_end_and_stale_completion_keeps_later_press() {
    let mut superseded = Fixture::source(PlatformParking::Twin);
    superseded.handle(input(B, press(1, true)), 0);
    let old_up = superseded.handle(input(B, press(2, false)), 1);
    superseded.confirm(&old_up, false, 1);
    assert_eq!(superseded.e2.next_deadline(), Some(ms(51)));
    superseded.handle(input(B, press(3, true)), 2);
    assert_eq!(
        superseded.e2.next_deadline().unwrap().as_nanos(),
        ms(302).as_nanos() + 1
    );
    superseded.confirm(&old_up, true, 3);
    assert_eq!(superseded.held(), vec![Held::Key(KEY)]);
    let mut f = Fixture::source(PlatformParking::Twin);
    f.handle(input(B, press(1, true)), 0);
    let old = f.handle(input(B, press(2, false)), 1);
    f.handle(input(B, press(3, true)), 2);
    let current = f.handle(input(B, press(4, false)), 3);
    f.confirm(&old, true, 4);
    assert_eq!(f.held(), vec![Held::Key(KEY)]);
    f.confirm(&current, false, 4);
    f.handle(Input::Command(Command::Return(key(A))), 5);
    assert_eq!(f.e2.next_deadline(), Some(ms(54)));
    let out = f.handle(Input::Tick, 54);
    assert_eq!(commands(&out), vec![up()]);
    f.confirm(&out, false, 54);
    assert_eq!(f.e2.next_deadline(), Some(ms(104)));
    let out = f.handle(Input::Tick, 104);
    assert_eq!(f.held(), vec![Held::Key(KEY)]);
    f.confirm(&out, true, 104);
    assert!(f.held().is_empty());
    assert_eq!(f.e2.next_deadline(), None);
}

#[test]
fn shared_journal_keeps_other_projection_and_retries_journal_errors() {
    let mut f = Fixture::source(PlatformParking::Twin);
    let second_window = WindowId(11);
    let second_id = ProjectionId(2);
    f.handle(Input::Windows(WindowEvent::Added(window(second_window))), 0);
    f.handle(
        Input::Command(Command::Project {
            window: second_window,
            to: B,
        }),
        0,
    );
    f.handle(
        control(
            B,
            Message::Accepted {
                projection: second_id,
                size: size(),
                scale: 1.0,
            },
        ),
        0,
    );
    f.handle(
        Input::Parked {
            window: second_window,
            result: Ok(parked(second_window, PlatformParking::Twin, size())),
        },
        0,
    );
    f.handle(
        Input::CaptureStarted {
            projection: second_id,
            result: Ok(StreamId(2)),
        },
        0,
    );
    f.handle(input(B, press(1, true)), 0);
    f.handle(Input::Windows(WindowEvent::Focused(Some(second_window))), 0);
    f.handle(
        input(
            B,
            ProjInput::Key {
                projection: second_id,
                seq: 1,
                usage: KEY,
                down: true,
            },
        ),
        0,
    );
    let out = f.handle(Input::Command(Command::Return(key(A))), 1);
    f.confirm(&out, true, 1);
    assert_eq!(f.held(), vec![Held::Key(KEY)]);
    let out = f.handle(
        Input::Command(Command::Return(ProjectionKey {
            source: A,
            projection: second_id,
        })),
        2,
    );
    f.journal.0.lock().unwrap().fail_up = true;
    f.confirm(&out, true, 2);
    assert_eq!(f.held(), vec![Held::Key(KEY)]);
    let retry = f.handle(Input::Tick, 52);
    assert_eq!(commands(&retry), vec![up()]);
    f.journal.0.lock().unwrap().fail_up = false;
    f.confirm(&retry, true, 52);
    assert!(f.held().is_empty());

    let mut f = Fixture::source(PlatformParking::Twin);
    f.handle(input(B, button(1, true, PointDevice::zero())), 0);
    f.journal.0.lock().unwrap().fail_down = true;
    let out = f.handle(input(B, press(2, true)), 1);
    assert_eq!(
        commands(&out),
        vec![
            up(),
            InjectCmd::Button {
                button: BUTTON,
                down: false
            }
        ]
    );
    assert!(out.contains(&Output::Restore { window: WINDOW }));
    f.confirm(&out, true, 1);
    assert!(f.held().is_empty());

    let mut f = Fixture::source(PlatformParking::Twin);
    f.handle(input(B, press(1, true)), 0);
    let out = f.handle(input(B, press(2, false)), 1);
    f.journal.0.lock().unwrap().fail_up = true;
    let ended = f.confirm(&out, true, 2);
    assert!(ended.contains(&Output::Restore { window: WINDOW }));
    assert!(f.confirm(&out, true, 2).is_empty());
    f.journal.0.lock().unwrap().fail_up = false;
    let retry = f.handle(Input::Tick, 52);
    f.confirm(&retry, true, 52);
    assert!(f.held().is_empty());
}

#[test]
fn crash_recovery_is_first_retries_and_has_independent_e1_ids() {
    let mut journal = SharedJournal::default();
    journal.record_down(Held::Key(KEY)).unwrap();
    journal.record_down(Held::Button(BUTTON)).unwrap();
    let (e2, out) = E2::new(&EngineConfig::new(A), Box::new(journal.clone()), ms(0)).unwrap();
    assert_eq!(
        commands(&out),
        vec![InjectCmd::Recover {
            keys: vec![KEY],
            buttons: vec![BUTTON]
        }]
    );
    let mut f = Fixture {
        e2,
        journal: journal.clone(),
    };
    f.confirm(&out, false, 0);
    let retry = f.handle(Input::Tick, 50);
    assert_eq!(
        commands(&retry),
        vec![InjectCmd::Recover {
            keys: vec![KEY],
            buttons: vec![BUTTON]
        }]
    );
    assert_ne!(injections(&out)[0].0, injections(&retry)[0].0);
    f.confirm(&retry, true, 50);
    assert!(f.held().is_empty());
    let mut e1_journal = MemoryJournal::default();
    e1_journal.record_down(Held::Key(KEY)).unwrap();
    journal.record_down(Held::Key(KEY)).unwrap();
    let (mut engine, out) = Engine::new(
        EngineConfig::new(A),
        Box::new(e1_journal),
        Box::new(journal.clone()),
        ms(0),
    )
    .unwrap();
    let ids = injections(&out);
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0].0, ids[1].0);
    engine.handle(
        Input::InjectDone {
            id: ids[0].0,
            ok: true,
        },
        ms(0),
    );
    assert_eq!(journal.held().unwrap(), vec![Held::Key(KEY)]);
    engine.handle(
        Input::InjectDone {
            id: ids[1].0,
            ok: true,
        },
        ms(0),
    );
    assert!(journal.held().unwrap().is_empty());
}

#[test]
fn source_end_paths_release_stop_restore_and_ignore_unrelated_peers() {
    for (event, reason, sent) in [
        (closed(B), Reason::LinkLost, false),
        (locked(), Reason::Locked, true),
        (
            Input::Session(SessionEvent::WillSleep),
            Reason::Locked,
            true,
        ),
        (Input::Grants(Default::default()), Reason::Revoked, true),
        (
            Input::Windows(WindowEvent::Removed(WINDOW)),
            Reason::WindowClosed,
            true,
        ),
        (
            Input::Command(Command::Return(key(A))),
            Reason::Returned,
            true,
        ),
        (
            Input::CaptureEnded {
                stream: StreamId(1),
                reason: StreamEndReason::Blocked,
            },
            Reason::Locked,
            true,
        ),
        (
            Input::CaptureEnded {
                stream: StreamId(1),
                reason: StreamEndReason::TargetGone,
            },
            Reason::WindowClosed,
            true,
        ),
        (
            Input::CaptureEnded {
                stream: StreamId(1),
                reason: StreamEndReason::Failed,
            },
            Reason::Failed,
            true,
        ),
    ] {
        let mut f = Fixture::source(PlatformParking::Twin);
        f.handle(input(B, press(1, true)), 0);
        assert!(f.handle(closed(C), 0).is_empty());
        assert!(
            f.handle(
                control(
                    C,
                    Message::Close {
                        projection: ID,
                        reason
                    }
                ),
                0
            )
            .is_empty()
        );
        assert!(
            f.handle(
                Input::CaptureEnded {
                    stream: StreamId(1),
                    reason: StreamEndReason::Requested
                },
                0
            )
            .is_empty()
        );
        let out = f.handle(event, 1);
        assert_eq!(commands(&out), vec![up()]);
        assert_eq!(
            out[1],
            Output::StopCapture {
                stream: StreamId(1)
            }
        );
        assert_eq!(out[2], Output::Restore { window: WINDOW });
        assert_eq!(messages(&out).len(), usize::from(sent));
        assert_eq!(
            out.last(),
            Some(&Output::Notice(Notice::ProjectionEnded {
                key: key(A),
                reason
            }))
        );
        f.confirm(&out, true, 1);
        assert!(f.handle(input(B, press(2, true)), 2).is_empty());
        assert!(f.held().is_empty());
    }
}

#[test]
fn parking_capture_failures_and_late_results_are_cleaned_up() {
    for failure in [true, false] {
        let mut f = Fixture::ready(A, B);
        f.handle(
            Input::Command(Command::Project {
                window: WINDOW,
                to: B,
            }),
            0,
        );
        f.handle(control(B, accepted()), 0);
        let out = if failure {
            f.handle(
                Input::Parked {
                    window: WINDOW,
                    result: Err(Failure::Other),
                },
                1,
            )
        } else {
            f.handle(
                Input::Parked {
                    window: WINDOW,
                    result: Ok(parked(WINDOW, PlatformParking::Twin, size())),
                },
                1,
            );
            f.handle(
                Input::CaptureStarted {
                    projection: ID,
                    result: Err(Failure::Other),
                },
                1,
            )
        };
        assert!(out.contains(&Output::Restore { window: WINDOW }));
        assert_eq!(
            messages(&out),
            vec![Message::End {
                projection: ID,
                reason: Reason::Failed
            }]
        );
        assert!(matches!(
            messages(&f.handle(
                Input::Command(Command::Project {
                    window: WINDOW,
                    to: B
                }),
                2
            ))
            .as_slice(),
            [Message::Start {
                projection: ProjectionId(2),
                ..
            }]
        ));
    }
    let mut f = Fixture::ready(A, B);
    f.handle(
        Input::Command(Command::Project {
            window: WINDOW,
            to: B,
        }),
        0,
    );
    f.handle(control(B, accepted()), 0);
    f.handle(locked(), 1);
    assert_eq!(
        f.handle(
            Input::Parked {
                window: WINDOW,
                result: Ok(parked(WINDOW, PlatformParking::Twin, size()))
            },
            2
        ),
        vec![Output::Restore { window: WINDOW }]
    );
    assert_eq!(
        f.handle(
            Input::CaptureStarted {
                projection: ID,
                result: Ok(StreamId(9))
            },
            2
        ),
        vec![Output::StopCapture {
            stream: StreamId(9)
        }]
    );
}

#[test]
fn destination_grants_open_accept_refusal_and_geometry() {
    let mut f = Fixture::new(B);
    assert_eq!(
        messages(&f.handle(control(A, start()), 0)),
        vec![Message::Refused {
            projection: ID,
            reason: Refusal::Permission
        }]
    );
    f.handle(
        Input::Grants([(A, [Capability::WindowPresent].into())].into()),
        0,
    );
    assert_eq!(
        messages(&f.handle(control(A, start()), 0)),
        vec![Message::Refused {
            projection: ID,
            reason: Refusal::Locked
        }]
    );
    f.handle(Input::Session(SessionEvent::State(OPEN)), 0);
    let out = f.handle(control(A, start()), 0);
    assert_eq!(
        out,
        vec![Output::OpenProxy {
            key: key(A),
            title: "fixture".into(),
            app_id: "test".into(),
            size: size()
        }]
    );
    assert!(f.handle(control(A, start()), 0).is_empty());
    assert!(
        inputs(&f.proxy(
            ProxyEvent::Key {
                usage: KEY,
                down: true
            },
            0
        ))
        .is_empty()
    );
    assert_eq!(
        messages(&f.handle(
            Input::ProxyOpened {
                key: key(A),
                result: Ok((size(), 2.0))
            },
            0
        )),
        vec![accepted()]
    );
    assert_eq!(
        f.handle(
            control(
                A,
                Message::Geometry {
                    projection: ID,
                    size: size(),
                    parking: ParkingKind::Twin
                }
            ),
            0
        ),
        vec![Output::ProxyGeometry {
            key: key(A),
            size: size(),
            parking: ParkingKind::Twin
        }]
    );
    assert_eq!(
        f.handle(
            control(
                A,
                Message::Title {
                    projection: ID,
                    title: "changed".into()
                }
            ),
            0
        ),
        vec![Output::ProxyTitle {
            key: key(A),
            title: "changed".into()
        }]
    );
    let mut f = Fixture::ready(B, A);
    f.handle(control(A, start()), 0);
    let out = f.handle(
        Input::ProxyOpened {
            key: key(A),
            result: Err(Failure::Other),
        },
        1,
    );
    assert_eq!(
        messages(&out),
        vec![Message::Refused {
            projection: ID,
            reason: Refusal::InjectorFailed
        }]
    );
    assert!(out.contains(&Output::CloseProxy { key: key(A) }));
    assert_eq!(f.e2.next_deadline(), None);
}

#[test]
fn destination_tracks_held_drops_duplicates_and_focus_loss_sends_ups() {
    let mut f = Fixture::destination();
    let key_down = ProxyEvent::Key {
        usage: KEY,
        down: true,
    };
    let button_down = ProxyEvent::Button {
        button: BUTTON,
        down: true,
        position: PointDevice::new(3.0, 4.0),
    };
    assert_eq!(inputs(&f.proxy(key_down.clone(), 0)), vec![press(1, true)]);
    assert!(f.proxy(key_down, 0).is_empty());
    assert_eq!(
        inputs(&f.proxy(button_down.clone(), 0)),
        vec![button(2, true, PointDevice::new(3.0, 4.0))]
    );
    assert!(f.proxy(button_down, 0).is_empty());
    let out = f.proxy(ProxyEvent::Focus(false), 1);
    assert_eq!(
        messages(&out),
        vec![Message::Focus {
            projection: ID,
            focused: false
        }]
    );
    assert_eq!(
        inputs(&out),
        vec![
            press(3, false),
            button(4, false, PointDevice::new(3.0, 4.0))
        ]
    );
    assert!(
        f.proxy(
            ProxyEvent::Key {
                usage: KEY,
                down: false
            },
            2
        )
        .is_empty()
    );
    assert!(
        f.proxy(
            ProxyEvent::Button {
                button: BUTTON,
                down: false,
                position: PointDevice::zero()
            },
            2
        )
        .is_empty()
    );
    assert_eq!(
        messages(&f.proxy(ProxyEvent::Focus(true), 3)),
        vec![Message::Focus {
            projection: ID,
            focused: true
        }]
    );
}

#[test]
fn motion_slots_keep_latest_and_button_scroll_flush_preserves_order() {
    let mut f = Fixture::destination();
    let first = f.proxy(
        ProxyEvent::Motion {
            position: PointDevice::new(1.0, 2.0),
        },
        0,
    );
    assert!(matches!(
        inputs(&first).as_slice(),
        [ProjInput::Motion { seq: 1, .. }]
    ));
    f.proxy(
        ProxyEvent::Motion {
            position: PointDevice::new(5.0, 6.0),
        },
        1,
    );
    let deadline = f.e2.next_deadline().unwrap();
    assert_eq!(deadline.as_nanos(), 8_333_334);
    let out = f.at(Input::Tick, deadline);
    assert_eq!(
        inputs(&out),
        vec![ProjInput::Motion {
            projection: ID,
            seq: 2,
            position: PointDevice::new(5.0, 6.0)
        }]
    );
    f.proxy(
        ProxyEvent::Motion {
            position: PointDevice::new(7.0, 8.0),
        },
        9,
    );
    assert_eq!(f.e2.next_deadline().unwrap().as_nanos(), 16_666_668);
    let out = f.proxy(
        ProxyEvent::Button {
            button: BUTTON,
            down: true,
            position: PointDevice::new(9.0, 10.0),
        },
        10,
    );
    assert!(matches!(
        inputs(&out).as_slice(),
        [
            ProjInput::Motion { seq: 3, .. },
            ProjInput::Button { seq: 4, .. }
        ]
    ));
    f.proxy(
        ProxyEvent::Motion {
            position: PointDevice::new(11.0, 12.0),
        },
        11,
    );
    let out = f.proxy(
        ProxyEvent::Scroll {
            delta: delta(),
            position: PointDevice::zero(),
        },
        12,
    );
    assert!(matches!(
        inputs(&out).as_slice(),
        [
            ProjInput::Motion { seq: 5, .. },
            ProjInput::Scroll { seq: 6, .. }
        ]
    ));
    let mut f = Fixture::destination();
    let mut sent = Vec::new();
    let mut now = MonoTime::ZERO;
    for n in 0..120 {
        let out = f.at(
            Input::Proxy {
                key: key(A),
                event: ProxyEvent::Motion {
                    position: PointDevice::new(n as f64, 0.0),
                },
            },
            now,
        );
        if inputs(&out)
            .iter()
            .any(|m| matches!(m, ProjInput::Motion { .. }))
        {
            sent.push(now);
            continue;
        }
        // Heartbeats may precede the motion slot. Deliver both timers without moving the
        // simulated clock backwards, and require every pending motion to reach a flush.
        for _ in 0..3 {
            let deadline = f.e2.next_deadline().unwrap();
            assert!(deadline >= now);
            now = deadline;
            if inputs(&f.at(Input::Tick, deadline))
                .iter()
                .any(|m| matches!(m, ProjInput::Motion { .. }))
            {
                sent.push(deadline);
                break;
            }
        }
    }
    assert_eq!(sent.len(), 120);
    assert!(
        sent.windows(2)
            .all(|w| w[1].saturating_duration_since(w[0]) >= Duration::from_nanos(8_333_334))
    );
}

#[test]
fn heartbeat_resize_and_keyframe_cadences_use_exact_deadlines() {
    let mut f = Fixture::destination();
    assert_eq!(f.e2.next_deadline(), Some(ms(250)));
    assert_eq!(
        inputs(&f.handle(Input::Tick, 250)),
        vec![heartbeat(1, vec![])]
    );
    f.proxy(
        ProxyEvent::Key {
            usage: KEY,
            down: true,
        },
        251,
    );
    assert_eq!(f.e2.next_deadline(), Some(ms(300)));
    assert_eq!(
        inputs(&f.handle(Input::Tick, 300)),
        vec![heartbeat(3, vec![KEY])]
    );
    assert_eq!(f.e2.next_deadline(), Some(ms(350)));
    f.proxy(
        ProxyEvent::Key {
            usage: KEY,
            down: false,
        },
        301,
    );
    assert_eq!(f.e2.next_deadline(), Some(ms(550)));
    assert_eq!(
        inputs(&f.handle(Input::Tick, 550)),
        vec![heartbeat(5, vec![])]
    );
    let resize = |n| ProxyEvent::Resized {
        size: PixelSize::new(n, 480),
        scale: 1.5,
    };
    assert_eq!(messages(&f.proxy(resize(700), 551)).len(), 1);
    assert!(messages(&f.proxy(resize(800), 552)).is_empty());
    f.proxy(resize(900), 553);
    assert_eq!(f.e2.next_deadline(), Some(ms(601)));
    assert_eq!(
        messages(&f.handle(Input::Tick, 601)),
        vec![Message::Resize {
            projection: ID,
            size: PixelSize::new(900, 480),
            scale: 1.5
        }]
    );
    assert_eq!(
        messages(&f.handle(Input::MediaError { key: key(A) }, 601)),
        vec![Message::KeyFrameRequest { projection: ID }]
    );
    assert!(f.handle(Input::MediaError { key: key(A) }, 800).is_empty());
    assert_eq!(
        messages(&f.handle(Input::MediaError { key: key(A) }, 801)).len(),
        1
    );
}

#[test]
fn destination_end_paths_send_ups_before_close_and_forget_timers() {
    for (event, reason, echo, ups) in [
        (
            control(
                A,
                Message::End {
                    projection: ID,
                    reason: Reason::WindowClosed,
                },
            ),
            Reason::WindowClosed,
            false,
            true,
        ),
        (
            Input::Command(Command::Return(key(A))),
            Reason::Returned,
            true,
            true,
        ),
        (
            Input::Proxy {
                key: key(A),
                event: ProxyEvent::CloseRequested,
            },
            Reason::Returned,
            true,
            true,
        ),
        (
            Input::Proxy {
                key: key(A),
                event: ProxyEvent::Lost,
            },
            Reason::Failed,
            true,
            true,
        ),
        (closed(A), Reason::LinkLost, false, false),
        (locked(), Reason::Locked, true, true),
        (
            Input::Session(SessionEvent::WillSleep),
            Reason::Locked,
            true,
            true,
        ),
        (
            Input::Grants(Default::default()),
            Reason::Revoked,
            true,
            true,
        ),
    ] {
        let mut f = Fixture::destination();
        f.proxy(
            ProxyEvent::Key {
                usage: KEY,
                down: true,
            },
            0,
        );
        f.proxy(
            ProxyEvent::Motion {
                position: PointDevice::zero(),
            },
            0,
        );
        let out = f.handle(event, 1);
        assert_eq!(inputs(&out).len(), usize::from(ups));
        if ups {
            assert!(matches!(out[0], Output::SendInput { .. }));
        }
        if echo {
            assert_eq!(
                messages(&out),
                vec![Message::Close {
                    projection: ID,
                    reason
                }]
            );
        } else {
            assert!(messages(&out).is_empty());
        }
        assert_eq!(
            &out[out.len() - 2..],
            &[
                Output::CloseProxy { key: key(A) },
                Output::Notice(Notice::ProjectionEnded {
                    key: key(A),
                    reason
                })
            ]
        );
        assert_eq!(f.e2.next_deadline(), None);
        assert!(f.handle(Input::Tick, 1000).is_empty());
    }
}

#[test]
fn reciprocal_id_one_projections_end_independently_and_ignore_wrong_direction() {
    let mut a = Fixture::source(PlatformParking::Twin);
    let mut b = Fixture::ready(B, A);
    b.handle(
        Input::Command(Command::Project {
            window: WINDOW,
            to: A,
        }),
        0,
    );
    a.handle(control(B, start()), 0);
    let accepted = a.handle(
        Input::ProxyOpened {
            key: key(B),
            result: Ok((size(), 1.0)),
        },
        0,
    );
    b.handle(control(A, messages(&accepted)[0].clone()), 0);
    b.handle(
        Input::Parked {
            window: WINDOW,
            result: Ok(parked(WINDOW, PlatformParking::Twin, size())),
        },
        0,
    );
    b.handle(
        Input::CaptureStarted {
            projection: ID,
            result: Ok(StreamId(1)),
        },
        0,
    );
    b.handle(control(A, start()), 0);
    b.handle(
        Input::ProxyOpened {
            key: key(A),
            result: Ok((size(), 1.0)),
        },
        0,
    );
    a.handle(input(B, press(1, true)), 0);
    b.handle(input(A, press(1, true)), 0);
    // End targets A's destination only; Close targets B's source only.
    let out = b.handle(Input::Command(Command::Return(key(B))), 1);
    assert_eq!(
        messages(&out),
        vec![Message::End {
            projection: ID,
            reason: Reason::Returned
        }]
    );
    b.confirm(&out, true, 1);
    let out = a.handle(control(B, messages(&out)[0].clone()), 1);
    assert!(out.contains(&Output::CloseProxy { key: key(B) }));
    assert!(!out.contains(&Output::Restore { window: WINDOW }));
    assert_eq!(a.held(), vec![Held::Key(KEY)]);
    assert!(
        a.handle(
            control(
                B,
                Message::Title {
                    projection: ID,
                    title: "orphan".into()
                }
            ),
            2
        )
        .is_empty()
    );
    let out = b.handle(Input::Command(Command::Return(key(A))), 2);
    assert_eq!(
        messages(&out),
        vec![Message::Close {
            projection: ID,
            reason: Reason::Returned
        }]
    );
    let out = a.handle(control(B, messages(&out)[0].clone()), 2);
    assert_eq!(commands(&out), vec![up()]);
    assert!(out.contains(&Output::StopCapture {
        stream: StreamId(1)
    }));
    assert!(out.contains(&Output::Restore { window: WINDOW }));
    assert!(!out.contains(&Output::CloseProxy { key: key(B) }));
    a.confirm(&out, true, 2);
    assert!(a.held().is_empty());
    assert!(
        a.handle(
            control(
                B,
                Message::End {
                    projection: ID,
                    reason: Reason::Failed
                }
            ),
            3
        )
        .is_empty()
    );
    assert!(
        a.handle(
            control(
                B,
                Message::Close {
                    projection: ID,
                    reason: Reason::Failed
                }
            ),
            3
        )
        .is_empty()
    );
    assert_eq!(a.e2.next_deadline(), None);
    assert_eq!(b.e2.next_deadline(), None);
}

fn offered() -> Fixture {
    let mut f = Fixture::ready(A, B);
    f.handle(
        Input::Command(Command::Project {
            window: WINDOW,
            to: B,
        }),
        0,
    );
    f
}

fn startup(stage: u8) -> Fixture {
    let mut f = offered();
    if stage >= 1 {
        f.handle(control(B, accepted()), 10);
    }
    if stage >= 2 {
        f.handle(
            Input::Parked {
                window: WINDOW,
                result: Ok(parked(WINDOW, PlatformParking::Twin, size())),
            },
            20,
        );
    }
    if stage >= 3 {
        f.handle(
            Input::CaptureStarted {
                projection: ID,
                result: Ok(StreamId(1)),
            },
            30,
        );
    }
    f
}

#[test]
fn late_parking_and_resize_results_always_restore_even_on_failure() {
    for resize in [false, true] {
        for ok in [false, true] {
            let mut f = startup(if resize { 3 } else { 1 });
            if resize {
                f.handle(
                    control(
                        B,
                        Message::Resize {
                            projection: ID,
                            size: PixelSize::new(800, 600),
                            scale: 1.0,
                        },
                    ),
                    40,
                );
            }
            let ended = f.handle(Input::Command(Command::Return(key(A))), 50);
            assert!(ended.contains(&Output::Restore { window: WINDOW }));
            assert_eq!(f.e2.next_deadline(), Some(ms(5050)));
            let result = if ok {
                Ok(parked(WINDOW, PlatformParking::Twin, size()))
            } else {
                Err(Failure::Other)
            };
            assert_eq!(
                f.handle(
                    Input::Parked {
                        window: WINDOW,
                        result
                    },
                    60
                ),
                vec![Output::Restore { window: WINDOW }]
            );
            assert_eq!(f.e2.next_deadline(), None);
            assert!(matches!(
                messages(&f.handle(
                    Input::Command(Command::Project {
                        window: WINDOW,
                        to: B
                    }),
                    61
                ))
                .as_slice(),
                [Message::Start { .. }]
            ));
        }
    }
}

#[test]
fn unanswered_parking_expires_and_removed_windows_clear_pending_cleanup() {
    for resize in [false, true] {
        let mut f = startup(if resize { 3 } else { 1 });
        if resize {
            f.handle(
                control(
                    B,
                    Message::Resize {
                        projection: ID,
                        size: size(),
                        scale: 1.0,
                    },
                ),
                40,
            );
        }
        f.handle(Input::Command(Command::Return(key(A))), 50);
        let busy = f.handle(
            Input::Command(Command::Project {
                window: WINDOW,
                to: B,
            }),
            5049,
        );
        assert_eq!(
            busy,
            vec![Output::Notice(Notice::ProjectionRefused {
                peer: B,
                reason: Refusal::Busy
            })]
        );
        assert!(f.handle(Input::Tick, 5049).is_empty());
        assert_eq!(
            f.handle(Input::Tick, 5050),
            vec![Output::Restore { window: WINDOW }]
        );
        assert_eq!(f.e2.next_deadline(), None);
        assert!(matches!(
            messages(&f.handle(
                Input::Command(Command::Project {
                    window: WINDOW,
                    to: B
                }),
                5051
            ))
            .as_slice(),
            [Message::Start { .. }]
        ));
    }
    for ended in [false, true] {
        let mut f = startup(1);
        if ended {
            f.handle(Input::Command(Command::Return(key(A))), 50);
        }
        f.handle(Input::Windows(WindowEvent::Removed(WINDOW)), 60);
        assert_eq!(f.e2.next_deadline(), None);
        assert!(
            f.handle(
                Input::Parked {
                    window: WINDOW,
                    result: Err(Failure::Other)
                },
                70
            )
            .is_empty()
        );
        assert!(f.handle(Input::Tick, 10000).is_empty());
        f.handle(Input::Windows(WindowEvent::Added(window(WINDOW))), 10000);
        assert!(matches!(
            messages(&f.handle(
                Input::Command(Command::Project {
                    window: WINDOW,
                    to: B
                }),
                10000
            ))
            .as_slice(),
            [Message::Start { .. }]
        ));
    }
}

#[test]
fn startup_resizes_keep_latest_size_and_scale_until_capture_is_live() {
    for stage in 0..3 {
        for (wanted, scale, expected) in [
            (PixelSize::new(800, 600), 2.0, true),
            (size(), 1.5, true),
            (size(), 2.0, false),
        ] {
            let mut f = startup(stage);
            for (size, scale) in [(PixelSize::new(700, 500), 3.0), (wanted, scale)] {
                assert!(
                    f.handle(
                        control(
                            B,
                            Message::Resize {
                                projection: ID,
                                size,
                                scale
                            }
                        ),
                        40
                    )
                    .is_empty()
                );
            }
            if stage < 1 {
                f.handle(control(B, accepted()), 50);
            }
            if stage < 2 {
                f.handle(
                    Input::Parked {
                        window: WINDOW,
                        result: Ok(parked(WINDOW, PlatformParking::Twin, size())),
                    },
                    60,
                );
            }
            let out = f.handle(
                Input::CaptureStarted {
                    projection: ID,
                    result: Ok(StreamId(1)),
                },
                70,
            );
            assert_eq!(
                out,
                if expected {
                    vec![Output::ResizeParked {
                        window: WINDOW,
                        size: wanted,
                        scale,
                    }]
                } else {
                    vec![]
                }
            );
        }
    }
    // Actual parking geometry, rather than the requested size, decides whether resizing is needed.
    let mut f = startup(1);
    f.handle(
        control(
            B,
            Message::Resize {
                projection: ID,
                size: size(),
                scale: 2.0,
            },
        ),
        40,
    );
    f.handle(
        Input::Parked {
            window: WINDOW,
            result: Ok(parked(
                WINDOW,
                PlatformParking::Twin,
                PixelSize::new(639, 480),
            )),
        },
        50,
    );
    assert_eq!(
        f.handle(
            Input::CaptureStarted {
                projection: ID,
                result: Ok(StreamId(1))
            },
            60
        ),
        vec![Output::ResizeParked {
            window: WINDOW,
            size: size(),
            scale: 2.0
        }]
    );
}

#[test]
fn source_rejects_invalid_sizes_without_parking_or_overwriting_valid_resize() {
    for bad in [
        PixelSize::new(0, 1),
        PixelSize::new(1, 0),
        PixelSize::new(16385, 1),
        PixelSize::new(1, 16385),
        PixelSize::new(u32::MAX, u32::MAX),
    ] {
        let mut f = offered();
        let out = f.handle(
            control(
                B,
                Message::Accepted {
                    projection: ID,
                    size: bad,
                    scale: 1.0,
                },
            ),
            1,
        );
        assert_eq!(
            messages(&out),
            vec![Message::End {
                projection: ID,
                reason: Reason::Failed
            }]
        );
        assert!(
            !out.iter()
                .any(|o| matches!(o, Output::Park { .. } | Output::Restore { .. }))
        );
        assert_eq!(f.e2.next_deadline(), None);
        for stage in 0..4 {
            let mut f = startup(stage);
            assert!(
                f.handle(
                    control(
                        B,
                        Message::Resize {
                            projection: ID,
                            size: bad,
                            scale: 1.0
                        }
                    ),
                    50
                )
                .is_empty()
            );
        }
    }
    for valid in [PixelSize::new(1, 1), PixelSize::new(16384, 16384)] {
        let mut f = offered();
        assert_eq!(
            f.handle(
                control(
                    B,
                    Message::Accepted {
                        projection: ID,
                        size: valid,
                        scale: 1.0
                    }
                ),
                1
            ),
            vec![Output::Park {
                window: WINDOW,
                size: valid,
                scale: 1.0
            }]
        );
    }
    let mut f = startup(2);
    f.handle(
        control(
            B,
            Message::Resize {
                projection: ID,
                size: size(),
                scale: 1.0,
            },
        ),
        40,
    );
    f.handle(
        control(
            B,
            Message::Resize {
                projection: ID,
                size: PixelSize::new(0, 0),
                scale: 2.0,
            },
        ),
        41,
    );
    assert_eq!(
        f.handle(
            Input::CaptureStarted {
                projection: ID,
                result: Ok(StreamId(1))
            },
            50
        ),
        vec![Output::ResizeParked {
            window: WINDOW,
            size: size(),
            scale: 1.0
        }]
    );
}

#[test]
fn source_focus_and_keyframe_controls_require_live_stage() {
    for stage in 0..4 {
        let mut f = startup(stage);
        let focus = f.handle(
            control(
                B,
                Message::Focus {
                    projection: ID,
                    focused: true,
                },
            ),
            40,
        );
        let keyframe = f.handle(control(B, Message::KeyFrameRequest { projection: ID }), 40);
        assert_eq!(
            focus,
            if stage == 3 {
                vec![Output::ActivateWindow { window: WINDOW }]
            } else {
                vec![]
            }
        );
        assert_eq!(
            keyframe,
            if stage == 3 {
                vec![Output::RequestKeyFrame { projection: ID }]
            } else {
                vec![]
            }
        );
    }
}

#[test]
fn startup_liveness_timeouts_fail_at_the_exact_deadline_and_clean_late_results() {
    for stage in [1, 2] {
        let mut f = startup(stage);
        let deadline = ms(if stage == 1 { 10010 } else { 10020 });
        assert_eq!(f.e2.next_deadline(), Some(deadline));
        assert!(
            f.at(Input::Tick, MonoTime::from_nanos(deadline.as_nanos() - 1))
                .is_empty()
        );
        let out = f.at(Input::Tick, deadline);
        assert_eq!(
            messages(&out),
            vec![Message::End {
                projection: ID,
                reason: Reason::Failed
            }]
        );
        assert!(out.contains(&Output::Restore { window: WINDOW }));
        if stage == 1 {
            assert_eq!(f.e2.next_deadline(), Some(ms(15010)));
            assert_eq!(
                f.handle(
                    Input::Parked {
                        window: WINDOW,
                        result: Err(Failure::Other)
                    },
                    10011
                ),
                vec![Output::Restore { window: WINDOW }]
            );
        } else {
            assert_eq!(
                f.handle(
                    Input::CaptureStarted {
                        projection: ID,
                        result: Ok(StreamId(1))
                    },
                    10021
                ),
                vec![Output::StopCapture {
                    stream: StreamId(1)
                }]
            );
        }
        assert_eq!(f.e2.next_deadline(), None);
    }
    let mut f = Fixture::ready(B, A);
    f.handle(control(A, start()), 20);
    assert_eq!(f.e2.next_deadline(), Some(ms(10020)));
    assert!(f.handle(Input::Tick, 10019).is_empty());
    let out = f.handle(Input::Tick, 10020);
    assert_eq!(
        messages(&out),
        vec![Message::Close {
            projection: ID,
            reason: Reason::Failed
        }]
    );
    assert!(out.contains(&Output::CloseProxy { key: key(A) }));
    assert!(out.contains(&Output::Notice(Notice::ProjectionEnded {
        key: key(A),
        reason: Reason::Failed
    })));
    assert_eq!(f.e2.next_deadline(), None);
    assert_eq!(
        f.handle(
            Input::ProxyOpened {
                key: key(A),
                result: Ok((size(), 1.0))
            },
            10021
        ),
        vec![Output::CloseProxy { key: key(A) }]
    );
}

#[test]
fn key_downs_wait_for_confirmed_focus_and_activation_is_throttled_but_ups_do_not_focus() {
    let mut f = Fixture::source(PlatformParking::Twin);
    f.handle(Input::Windows(WindowEvent::Focused(None)), 0);
    assert_eq!(
        f.handle(input(B, press(1, true)), 0),
        vec![Output::ActivateWindow { window: WINDOW }]
    );
    assert!(f.held().is_empty());
    assert_eq!(f.e2.next_deadline(), None);
    for (seq, now) in [(2, 1), (3, 299)] {
        assert!(f.handle(input(B, press(seq, true)), now).is_empty());
        assert!(f.held().is_empty());
    }
    assert_eq!(
        f.handle(input(B, press(4, true)), 300),
        vec![Output::ActivateWindow { window: WINDOW }]
    );
    assert!(f.handle(input(B, press(5, false)), 301).is_empty());
    assert!(f.handle(input(B, press(6, true)), 599).is_empty());
    assert_eq!(
        f.handle(input(B, press(7, true)), 600),
        vec![Output::ActivateWindow { window: WINDOW }]
    );
    assert!(f.held().is_empty());
    f.handle(Input::Windows(WindowEvent::Focused(Some(WINDOW))), 601);
    assert_eq!(
        commands(&f.handle(input(B, press(8, true)), 601)),
        vec![InjectCmd::Key {
            usage: KEY,
            down: true
        }]
    );
    f.handle(Input::Windows(WindowEvent::Focused(Some(WINDOW2))), 602);
    let up = f.handle(input(B, press(9, false)), 602);
    assert_eq!(commands(&up), vec![crate::up()]);
    assert!(
        !up.iter()
            .any(|o| matches!(o, Output::ActivateWindow { .. }))
    );
    f.confirm(&up, true, 602);
    assert!(f.held().is_empty());
}

#[test]
fn outgoing_window_text_is_truncated_on_utf8_boundaries() {
    let mut f = Fixture::ready(A, B);
    let mut info = window(WINDOW);
    info.title = "a".repeat(1023) + "é";
    info.app_id = "b".repeat(1022) + "é" + "z";
    f.handle(Input::Windows(WindowEvent::Changed(info)), 0);
    let out = f.handle(
        Input::Command(Command::Project {
            window: WINDOW,
            to: B,
        }),
        0,
    );
    let messages = messages(&out);
    let Message::Start { window, .. } = &messages[0] else {
        panic!("missing Start")
    };
    assert_eq!(window.title, "a".repeat(1023));
    assert_eq!(window.app_id, "b".repeat(1022) + "é");
    crosspane_protocol::wire::encode_control(
        &ControlMessage::Projection(messages[0].clone()),
        &mut Vec::new(),
    )
    .unwrap();
    let mut info = crate::window(WINDOW);
    info.title = "🦀".repeat(300);
    let out = f.handle(Input::Windows(WindowEvent::Changed(info)), 1);
    assert_eq!(
        crate::messages(&out),
        vec![Message::Title {
            projection: ID,
            title: "🦀".repeat(256)
        }]
    );
}

#[test]
fn destination_caps_held_items_releases_excess_keys_and_ignores_invalid_buttons() {
    let mut f = Fixture::destination();
    for id in 4..36 {
        let out = f.proxy(
            ProxyEvent::Key {
                usage: HidUsage::keyboard(id),
                down: true,
            },
            0,
        );
        assert!(matches!(
            inputs(&out).as_slice(),
            [ProjInput::Key { down: true, .. }]
        ));
    }
    for id in 36..40 {
        let usage = HidUsage::keyboard(id);
        assert!(
            matches!(inputs(&f.proxy(ProxyEvent::Key { usage, down: true }, 0)).as_slice(), [ProjInput::Key { usage: sent, down: false, .. }] if *sent == usage)
        );
    }
    for button in 1..=16 {
        assert!(matches!(
            inputs(&f.proxy(
                ProxyEvent::Button {
                    button: MouseButton(button),
                    down: true,
                    position: PointDevice::zero()
                },
                0
            ))
            .as_slice(),
            [ProjInput::Button { down: true, .. }]
        ));
    }
    let deadline = f.e2.next_deadline();
    for button in (0..=u8::MAX).filter(|button| !(1..=16).contains(button)) {
        for down in [true, false] {
            assert!(
                f.proxy(
                    ProxyEvent::Button {
                        button: MouseButton(button),
                        down,
                        position: PointDevice::new(900.0, 800.0),
                    },
                    1,
                )
                .is_empty()
            );
            assert_eq!(f.e2.next_deadline(), deadline);
        }
    }
    let out = f.handle(Input::Tick, 50);
    let held = inputs(&out);
    assert!(
        matches!(held.as_slice(), [ProjInput::Held { seq: 53, keys, buttons, .. }] if keys.len() == 32 && buttons.len() == 16 && buttons.iter().all(|b| (1..=16).contains(&b.0)))
    );
    for msg in held {
        crosspane_protocol::wire::encode_input(&InputMessage::Proj(msg), &mut Vec::new()).unwrap();
    }
    let out = f.proxy(ProxyEvent::Focus(false), 51);
    assert_eq!(inputs(&out).len(), 48);
    assert!(inputs(&out).iter().all(|msg| matches!(
        msg,
        ProjInput::Key { down: false, .. } | ProjInput::Button { down: false, .. }
    )));
    assert!(inputs(&out).iter().all(|msg| match msg {
        ProjInput::Button { position, .. } => *position == PointDevice::zero(),
        _ => true,
    }));
}

#[test]
fn destination_cap_counts_pending_opens_per_peer_and_frees_ended_slots() {
    let mut f = Fixture::ready(B, A);
    f.handle(
        Input::Grants(
            [
                (A, [Capability::WindowPresent].into()),
                (C, [Capability::WindowPresent].into()),
            ]
            .into(),
        ),
        0,
    );
    for id in 1..=16 {
        let key = ProjectionKey {
            source: A,
            projection: ProjectionId(id),
        };
        let out = f.handle(
            control(
                A,
                Message::Start {
                    projection: key.projection,
                    window: WindowSummary {
                        title: String::new(),
                        app_id: String::new(),
                    },
                    size: size(),
                },
            ),
            0,
        );
        assert!(matches!(out.as_slice(), [Output::OpenProxy { .. }]));
        if id % 2 == 0 {
            f.handle(
                Input::ProxyOpened {
                    key,
                    result: Ok((size(), 1.0)),
                },
                0,
            );
        }
    }
    assert!(f.handle(control(A, start()), 0).is_empty());
    let start17 = Message::Start {
        projection: ProjectionId(17),
        window: WindowSummary {
            title: String::new(),
            app_id: String::new(),
        },
        size: size(),
    };
    assert_eq!(
        messages(&f.handle(control(A, start17.clone()), 0)),
        vec![Message::Refused {
            projection: ProjectionId(17),
            reason: Refusal::Busy
        }]
    );
    assert!(matches!(
        f.handle(control(C, start17.clone()), 0).as_slice(),
        [Output::OpenProxy { .. }]
    ));
    f.handle(
        control(
            A,
            Message::End {
                projection: ID,
                reason: Reason::Returned,
            },
        ),
        1,
    );
    assert!(matches!(
        f.handle(control(A, start17), 1).as_slice(),
        [Output::OpenProxy { .. }]
    ));
}

#[test]
fn pending_proxy_ignores_geometry_and_title_but_honours_close_and_lost() {
    for (event, reason) in [
        (ProxyEvent::CloseRequested, Reason::Returned),
        (ProxyEvent::Lost, Reason::Failed),
    ] {
        let mut f = Fixture::ready(B, A);
        f.handle(control(A, start()), 0);
        assert!(
            f.handle(
                control(
                    A,
                    Message::Geometry {
                        projection: ID,
                        size: size(),
                        parking: ParkingKind::Twin
                    }
                ),
                0
            )
            .is_empty()
        );
        assert!(
            f.handle(
                control(
                    A,
                    Message::Title {
                        projection: ID,
                        title: "pending".into()
                    }
                ),
                0
            )
            .is_empty()
        );
        let out = f.proxy(event, 1);
        assert_eq!(
            messages(&out),
            vec![Message::Close {
                projection: ID,
                reason
            }]
        );
        assert!(out.contains(&Output::CloseProxy { key: key(A) }));
        assert_eq!(f.e2.next_deadline(), None);
        assert_eq!(
            f.handle(
                Input::ProxyOpened {
                    key: key(A),
                    result: Ok((size(), 1.0))
                },
                2
            ),
            vec![Output::CloseProxy { key: key(A) }]
        );
    }
    let mut f = Fixture::ready(B, A);
    f.handle(control(A, start()), 0);
    let out = f.handle(
        Input::ProxyOpened {
            key: key(A),
            result: Err(Failure::Other),
        },
        1,
    );
    assert!(out.contains(&Output::Notice(Notice::ProjectionRefused {
        peer: A,
        reason: Refusal::InjectorFailed
    })));
}

#[derive(Default)]
struct Platform {
    held: BTreeSet<Held>,
    parked: BTreeSet<WindowId>,
    ever_parked: BTreeSet<WindowId>,
    streams: BTreeSet<StreamId>,
    proxies: BTreeSet<ProjectionKey>,
    asleep: bool,
    fresh: bool,
    permitted: bool,
    connected: bool,
    next_stream: u64,
}

struct Simulation {
    nodes: [Fixture; 2],
    platform: [Platform; 2],
    queue: VecDeque<(usize, Input)>,
    pending: Vec<(usize, Input)>,
    now: MonoTime,
    failures: bool,
    delay: bool,
    network: bool,
}

impl Simulation {
    fn new() -> Self {
        let mut nodes = [Fixture::ready(A, B), Fixture::ready(B, A)];
        for f in &mut nodes {
            f.handle(Input::Windows(WindowEvent::Added(window(WINDOW2))), 0);
        }
        Self {
            nodes,
            platform: std::array::from_fn(|_| Platform {
                permitted: true,
                fresh: true,
                connected: true,
                ..Platform::default()
            }),
            queue: VecDeque::new(),
            pending: Vec::new(),
            now: ms(0),
            failures: false,
            delay: false,
            network: true,
        }
    }

    fn response(&mut self, node: usize, input: Input) {
        if self.delay {
            self.pending.push((node, input));
        } else {
            self.queue.push_back((node, input));
        }
    }

    fn step(&mut self, node: usize, input: Input) {
        let platform = &mut self.platform[node];
        match &input {
            Input::Session(SessionEvent::State(state)) => {
                platform.permitted = state.permits_io();
                platform.fresh = true;
            }
            Input::Session(SessionEvent::WillSleep) => platform.asleep = true,
            Input::Session(SessionEvent::Woke) => {
                platform.asleep = false;
                platform.fresh = false;
            }
            Input::Link(LinkEvent::Closed { .. }) => platform.connected = false,
            Input::PeerUp { .. } => platform.connected = true,
            _ => {}
        }
        let tick = matches!(input, Input::Tick);
        let out = self.nodes[node].at(input, self.now);
        if tick {
            assert!(
                self.nodes[node]
                    .e2
                    .next_deadline()
                    .is_none_or(|deadline| deadline > self.now),
                "Tick livelock at {:?}",
                self.now
            );
        }
        let allowed = self.platform[node].permitted
            && self.platform[node].fresh
            && !self.platform[node].asleep;
        for output in out {
            match output {
                Output::SendControl { msg, .. } => {
                    if self.network && self.platform.iter().all(|p| p.connected) {
                        self.queue.push_back((
                            1 - node,
                            Input::Link(LinkEvent::Control {
                                peer: [A, B][node],
                                msg,
                            }),
                        ));
                    }
                }
                Output::SendInput { msg, .. } => {
                    if self.network && self.platform.iter().all(|p| p.connected) {
                        self.queue.push_back((
                            1 - node,
                            Input::Link(LinkEvent::Input {
                                peer: [A, B][node],
                                msg,
                            }),
                        ));
                    }
                }
                Output::Park { window, size, .. } | Output::ResizeParked { window, size, .. } => {
                    assert!(allowed, "parking while blocked");
                    // Model even partially failed parking as an obligation to restore.
                    self.platform[node].parked.insert(window);
                    self.platform[node].ever_parked.insert(window);
                    let result = if self.failures {
                        Err(Failure::Other)
                    } else {
                        Ok(parked(
                            window,
                            if node == 0 {
                                PlatformParking::Twin
                            } else {
                                PlatformParking::Mirror
                            },
                            size,
                        ))
                    };
                    self.response(node, Input::Parked { window, result });
                }
                Output::Restore { window } => {
                    assert!(
                        self.platform[node].ever_parked.contains(&window),
                        "Restore for a never-parked window"
                    );
                    self.platform[node].parked.remove(&window);
                }
                Output::ActivateWindow { window } => {
                    self.response(node, Input::Windows(WindowEvent::Focused(Some(window))));
                }
                Output::StartCapture { projection, .. } => {
                    assert!(allowed, "capture while blocked");
                    self.platform[node].next_stream += 1;
                    let stream = StreamId(self.platform[node].next_stream);
                    let result = if self.failures {
                        Err(Failure::Other)
                    } else {
                        self.platform[node].streams.insert(stream);
                        Ok(stream)
                    };
                    self.response(node, Input::CaptureStarted { projection, result });
                }
                Output::StopCapture { stream } => {
                    self.platform[node].streams.remove(&stream);
                }
                Output::OpenProxy { key, size, .. } => {
                    assert!(allowed);
                    self.platform[node].proxies.insert(key);
                    let result = if self.failures {
                        Err(Failure::Other)
                    } else {
                        Ok((size, 1.5))
                    };
                    self.response(node, Input::ProxyOpened { key, result });
                }
                Output::CloseProxy { key } => {
                    self.platform[node].proxies.remove(&key);
                }
                Output::Inject { id, cmd } => {
                    let press = matches!(
                        cmd,
                        InjectCmd::Key { down: true, .. } | InjectCmd::Button { down: true, .. }
                    );
                    assert!(!press || allowed, "press while session blocks I/O");
                    let ok = !self.failures;
                    if ok {
                        match cmd {
                            InjectCmd::Key { usage, down } => {
                                transition_fake(
                                    &mut self.platform[node].held,
                                    Held::Key(usage),
                                    down,
                                );
                            }
                            InjectCmd::Button { button, down } => {
                                transition_fake(
                                    &mut self.platform[node].held,
                                    Held::Button(button),
                                    down,
                                );
                            }
                            InjectCmd::Recover { keys, buttons } => {
                                for item in keys
                                    .into_iter()
                                    .map(Held::Key)
                                    .chain(buttons.into_iter().map(Held::Button))
                                {
                                    self.platform[node].held.remove(&item);
                                }
                            }
                            InjectCmd::ReleaseAll => self.platform[node].held.clear(),
                            _ => {}
                        }
                    }
                    self.response(node, Input::InjectDone { id, ok });
                }
                _ => {}
            }
            self.assert_journal();
        }
        self.assert_journal();
    }

    fn assert_journal(&self) {
        for node in 0..2 {
            let held = self.nodes[node].held();
            assert!(
                self.platform[node]
                    .held
                    .iter()
                    .all(|item| held.contains(item)),
                "unreleased input lost its journal record"
            );
        }
    }

    fn drain(&mut self, max: usize) {
        for _ in 0..max {
            let Some((node, input)) = self.queue.pop_front() else {
                return;
            };
            if matches!(
                input,
                Input::Link(LinkEvent::Control { .. } | LinkEvent::Input { .. })
            ) && !self.platform.iter().all(|p| p.connected)
            {
                continue;
            }
            self.step(node, input);
        }
    }

    fn finish(&mut self) {
        self.failures = false;
        self.delay = false;
        for node in 0..2 {
            self.step(node, locked());
        }
        // Executions happened at submission time. Deliver their actual old outcomes, rather than
        // changing a failed release into a successful completion during final cleanup.
        self.queue.extend(std::mem::take(&mut self.pending));
        self.drain(10_000);
        assert!(self.queue.is_empty());
        for _ in 0..5 {
            self.now = self.now.saturating_add(Duration::from_millis(500));
            for node in 0..2 {
                self.step(node, Input::Tick);
            }
            self.drain(10_000);
        }
        for node in 0..2 {
            assert!(
                self.platform[node].held.is_empty(),
                "down lacked a successful up"
            );
            assert!(self.nodes[node].held().is_empty());
            assert!(self.platform[node].parked.is_empty(), "Park lacked Restore");
            assert!(
                self.platform[node].streams.is_empty(),
                "successful StartCapture lacked StopCapture"
            );
            assert!(
                self.platform[node].proxies.is_empty(),
                "OpenProxy lacked CloseProxy"
            );
            assert_eq!(self.nodes[node].e2.next_deadline(), None);
        }
    }
}

fn transition_fake(held: &mut BTreeSet<Held>, item: Held, down: bool) {
    if down {
        held.insert(item);
    } else {
        held.remove(&item);
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1_000, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn wired_roles_preserve_release_resource_and_deadline_invariants(
        events in prop::collection::vec((0u8..26, any::<u8>(), 0u16..400, any::<bool>(), any::<bool>()), 1..150)
    ) {
        let mut sim = Simulation::new();
        // Every case starts two windows in both directions; random events can interrupt startup
        // phase, delay responses, and fail platform operations independently of network delivery.
        for node in 0..2 {
            for window in [WINDOW, WINDOW2] { sim.step(node, Input::Command(Command::Project { window, to: [B, A][node] })); }
        }
        sim.drain(1_000);
        for node in 0..2 {
            for projection in [ID, ProjectionId(2)] {
            let proxy_key = ProjectionKey { source: [B, A][node], projection };
            sim.step(node, Input::Proxy { key: proxy_key, event: ProxyEvent::Key { usage: KEY, down: true } });
            sim.step(node, Input::Proxy { key: proxy_key, event: ProxyEvent::Button { button: BUTTON, down: true, position: PointDevice::zero() } });
            }
        }
        sim.drain(1_000);
        for (kind, arg, elapsed, failure, delay) in events {
            sim.now = sim.now.saturating_add(Duration::from_millis(u64::from(elapsed)));
            sim.failures = failure;
            sim.delay = delay;
            let node = usize::from((arg >> 2) % 2);
            let proxy_key = ProjectionKey { source: [B, A][node], projection: ProjectionId(1 + u64::from(arg % 4)) };
            let chosen_window = if arg & 1 == 0 { WINDOW } else { WINDOW2 };
            let usage = HidUsage::keyboard(4 + u16::from(arg % 4));
            let event = match kind {
                0 => Input::Command(Command::Project { window: chosen_window, to: [B, A][node] }),
                1 => locked(),
                2 => Input::Session(SessionEvent::State(OPEN)),
                3 => Input::Session(SessionEvent::WillSleep),
                4 => Input::Session(SessionEvent::Woke),
                5 => closed([B, A][node]),
                6 => Input::PeerUp { peer: [B, A][node] },
                7 => Input::Grants(Default::default()),
                8 => Input::Grants([([B, A][node], [Capability::WindowShare, Capability::WindowPresent].into())].into()),
                9 => Input::Proxy { key: proxy_key, event: ProxyEvent::Key { usage, down: true } },
                10 => Input::Proxy { key: proxy_key, event: ProxyEvent::Key { usage, down: false } },
                11 | 12 => Input::Proxy { key: proxy_key, event: ProxyEvent::Button { button: BUTTON, down: kind == 11, position: PointDevice::new(f64::from(arg), -3.0) } },
                13 => Input::Proxy { key: proxy_key, event: ProxyEvent::Motion { position: PointDevice::new(f64::from(arg), 900.0) } },
                14 => Input::Proxy { key: proxy_key, event: ProxyEvent::Scroll { delta: delta(), position: PointDevice::zero() } },
                15 => Input::Proxy { key: proxy_key, event: ProxyEvent::Resized { size: PixelSize::new(640 + u32::from(arg), 480), scale: 1.5 } },
                16 => Input::Proxy { key: proxy_key, event: ProxyEvent::Focus(arg % 2 == 0) },
                17 => Input::Proxy { key: proxy_key, event: ProxyEvent::CloseRequested },
                18 => Input::Proxy { key: proxy_key, event: ProxyEvent::Lost },
                19 => Input::Command(Command::Return(ProjectionKey { source: [A, B][node], projection: proxy_key.projection })),
                20 => Input::MediaError { key: proxy_key },
                21 => Input::Windows(WindowEvent::Focused(Some(WindowId(99)))),
                22 => Input::Windows(WindowEvent::Removed(chosen_window)),
                23 => Input::Windows(WindowEvent::Added(window(chosen_window))),
                24 if !sim.pending.is_empty() => {
                    let (target, input) = sim.pending.swap_remove(usize::from(arg) % sim.pending.len());
                    sim.step(target, input);
                    Input::Tick
                }
                _ => Input::Tick,
            };
            sim.step(node, event);
            sim.drain(usize::from(arg % 8));
            if arg % 7 == 0 && let Some(deadline) = sim.nodes[node].e2.next_deadline() {
                    sim.now = sim.now.max(deadline);
                    sim.step(node, Input::Tick);
            }
        }
        sim.finish();
    }

    #[test]
    fn lease_only_ticks_release_two_windows_per_node_without_network(
        elapsed in prop::collection::vec(0u16..500, 1..30)
    ) {
        let mut sim = Simulation::new();
        for node in 0..2 {
            for window in [WINDOW, WINDOW2] {
                sim.step(node, Input::Command(Command::Project { window, to: [B, A][node] }));
            }
        }
        sim.drain(1_000);
        for node in 0..2 {
            for (projection, window) in [(ID, WINDOW), (ProjectionId(2), WINDOW2)] {
                sim.step(node, Input::Windows(WindowEvent::Focused(Some(window))));
                sim.step(node, input([B, A][node], ProjInput::Key {
                    projection, seq: 1, usage: HidUsage::keyboard(4 + projection.0 as u16), down: true,
                }));
                sim.step(node, input([B, A][node], ProjInput::Button {
                    projection, seq: 2, button: MouseButton(projection.0 as u8), down: true,
                    position: PointDevice::zero(),
                }));
            }
        }
        sim.drain(1_000);
        for node in 0..2 { assert_eq!(sim.platform[node].held.len(), 4); }
        assert!(sim.queue.is_empty());
        sim.network = false;
        for duration in elapsed {
            sim.now = sim.now.saturating_add(Duration::from_millis(u64::from(duration)));
            for node in 0..2 { sim.step(node, Input::Tick); }
            sim.drain(1_000); // Only platform InjectDone acknowledgements; network is disabled.
        }
        sim.now = sim.now.max(ms(301));
        for node in 0..2 { sim.step(node, Input::Tick); }
        sim.drain(1_000);
        for node in 0..2 {
            assert!(sim.platform[node].held.is_empty());
            assert!(sim.nodes[node].held().is_empty());
            assert_eq!(sim.platform[node].parked.len(), 2);
            assert_eq!(sim.platform[node].streams.len(), 2);
            assert_eq!(sim.platform[node].proxies.len(), 2);
        }
        sim.finish();
    }

}
