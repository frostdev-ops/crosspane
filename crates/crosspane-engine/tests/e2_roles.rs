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
/// What a destination sends when a suspended projection resumes: `Accepted`, then a fresh
/// numbered `Resize` for the same size (so a request the disconnect discarded can't strand the
/// correlation of the source's answers).
fn resume_messages(size: PixelSize, scale: f64, request: u32) -> Vec<Message> {
    vec![
        Message::Accepted {
            projection: ID,
            size,
            scale,
        },
        Message::Resize {
            projection: ID,
            request,
            size,
            scale,
        },
    ]
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
                place: None,
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
/// What a Twin-parked source sends to answer request `answers` with the window at `size`.
fn sent_geometry(size: PixelSize, answers: u32) -> Output {
    Output::SendControl {
        peer: B,
        msg: ControlMessage::Projection(Message::Geometry {
            projection: ID,
            size,
            parking: ParkingKind::Twin,
            answers,
        }),
    }
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
            place: None,
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
            parking: ParkingKind::Twin,
            answers: 0
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
    // Another window has focus on the source, so focusing the proxy activates this one.
    f.handle(Input::Windows(WindowEvent::Focused(Some(WindowId(99)))), 3);
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
                    request: 1,
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
                request: 2,
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
                request: 3,
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
        // It answers the request that was running (1), not the newer one queued behind it (3).
        vec![Message::Geometry {
            projection: ID,
            size: resized,
            parking: ParkingKind::Twin,
            answers: 1
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
            Output::Restore {
                window: WINDOW,
                place: None
            },
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
        place: None,
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
            vec![InjectCmd::MoveTo {
                display: DISPLAY,
                position: PointDevice::new(20.0, 509.0)
            }]
        );
        assert_eq!(
            commands(&f.confirm(&out, true, 0)),
            vec![InjectCmd::Button {
                button: BUTTON,
                down: true
            }]
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
            vec![InjectCmd::MoveTo {
                display: DISPLAY,
                position: PointDevice::new(28.0, 39.0)
            }]
        );
        assert_eq!(
            commands(&f.confirm(&out, true, 0)),
            vec![InjectCmd::Scroll(delta())]
        );
    }
    let mut f = Fixture::ready(A, B);
    f.handle(
        Input::Command(Command::Project {
            window: WINDOW,
            to: B,
            place: None,
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
            place: None,
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
    let out = f.handle(input(B, button(1, true, PointDevice::zero())), 0);
    f.confirm(&out, true, 0);
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
    assert!(out.contains(&Output::Restore {
        window: WINDOW,
        place: None
    }));
    f.confirm(&out, true, 1);
    assert!(f.held().is_empty());

    let mut f = Fixture::source(PlatformParking::Twin);
    f.handle(input(B, press(1, true)), 0);
    let out = f.handle(input(B, press(2, false)), 1);
    f.journal.0.lock().unwrap().fail_up = true;
    let ended = f.confirm(&out, true, 2);
    assert!(ended.contains(&Output::Restore {
        window: WINDOW,
        place: None
    }));
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
        let link_lost = matches!(event, Input::Link(LinkEvent::Closed { .. }));
        let mut out = f.handle(event, 1);
        if link_lost {
            assert_eq!(out.len(), 2); // Immediate release and StopCapture, then grace.
            out.extend(f.handle(Input::Tick, 20_001));
        }
        assert_eq!(commands(&out), vec![up()]);
        assert_eq!(
            out[1],
            Output::StopCapture {
                stream: StreamId(1)
            }
        );
        assert_eq!(
            out[2],
            Output::Restore {
                window: WINDOW,
                place: None
            }
        );
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
                place: None,
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
        assert!(out.contains(&Output::Restore {
            window: WINDOW,
            place: None
        }));
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
                    to: B,
                    place: None,
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
            place: None,
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
        vec![Output::Restore {
            window: WINDOW,
            place: None
        }]
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
            size: size(),
            place: None,
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
    // The first geometry tells the host the parking kind, even at the proxy's own size.
    assert_eq!(
        f.handle(
            control(
                A,
                Message::Geometry {
                    projection: ID,
                    size: size(),
                    parking: ParkingKind::Twin,
                    answers: 0
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
    // The source's content differs from the proxy (an app minimum, say). The proxy follows it,
    // and reporting that size back is not a new request; a size of the user's own still is.
    let content = PixelSize::new(size().width + 52, size().height);
    assert_eq!(
        f.handle(
            control(
                A,
                Message::Geometry {
                    projection: ID,
                    size: content,
                    parking: ParkingKind::Twin,
                    answers: 0
                },
            ),
            0,
        ),
        vec![Output::ProxyGeometry {
            key: key(A),
            size: content,
            parking: ParkingKind::Twin
        }]
    );
    assert!(
        messages(&f.proxy(
            ProxyEvent::Resized {
                size: content,
                scale: 2.0
            },
            100
        ))
        .is_empty()
    );
    assert_eq!(
        messages(&f.proxy(
            ProxyEvent::Resized {
                size: size(),
                scale: 2.0
            },
            200
        )),
        // The source is at a different size than it was asked for, so the user's size is a
        // genuine request, not a repeat.
        vec![Message::Resize {
            projection: ID,
            request: 1,
            size: size(),
            scale: 2.0
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
            request: 2,
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
        let link_lost = matches!(event, Input::Link(LinkEvent::Closed { .. }));
        let mut out = f.handle(event, 1);
        if link_lost {
            assert!(out.is_empty());
            out.extend(f.handle(Input::Tick, 20_001));
        }
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
            place: None,
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
    assert!(!out.contains(&Output::Restore {
        window: WINDOW,
        place: None
    }));
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
    assert!(out.contains(&Output::Restore {
        window: WINDOW,
        place: None
    }));
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
            place: None,
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
                            request: 1,
                            size: PixelSize::new(800, 600),
                            scale: 1.0,
                        },
                    ),
                    40,
                );
            }
            let ended = f.handle(Input::Command(Command::Return(key(A))), 50);
            assert!(ended.contains(&Output::Restore {
                window: WINDOW,
                place: None
            }));
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
                vec![Output::Restore {
                    window: WINDOW,
                    place: None
                }]
            );
            assert_eq!(f.e2.next_deadline(), None);
            assert!(matches!(
                messages(&f.handle(
                    Input::Command(Command::Project {
                        window: WINDOW,
                        to: B,
                        place: None,
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
                        request: 1,
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
                place: None,
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
            vec![Output::Restore {
                window: WINDOW,
                place: None
            }]
        );
        assert_eq!(f.e2.next_deadline(), None);
        assert!(matches!(
            messages(&f.handle(
                Input::Command(Command::Project {
                    window: WINDOW,
                    to: B,
                    place: None,
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
                    to: B,
                    place: None,
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
            for (request, (size, scale)) in [(PixelSize::new(700, 500), 3.0), (wanted, scale)]
                .into_iter()
                .enumerate()
            {
                assert!(
                    f.handle(
                        control(
                            B,
                            Message::Resize {
                                projection: ID,
                                request: request as u32 + 1,
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
                    // Already the actual size and scale: answered at once, no platform work.
                    vec![sent_geometry(size(), 2)]
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
                request: 1,
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
fn source_refuses_invalid_sizes_without_parking_but_answers_them() {
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
            let out = f.handle(
                control(
                    B,
                    Message::Resize {
                        projection: ID,
                        request: 1,
                        size: bad,
                        scale: 1.0,
                    },
                ),
                50,
            );
            // No platform work either way. Live: answered at once with the actual size, so the
            // destination never waits for an answer that can't come. Before that it is queued,
            // and answered the moment the window is live.
            if stage == 3 {
                assert_eq!(out, vec![sent_geometry(size(), 1)]);
            } else {
                assert!(out.is_empty());
                if stage < 1 {
                    f.handle(control(B, accepted()), 51);
                }
                if stage < 2 {
                    f.handle(
                        Input::Parked {
                            window: WINDOW,
                            result: Ok(parked(WINDOW, PlatformParking::Twin, size())),
                        },
                        52,
                    );
                }
                let out = f.handle(
                    Input::CaptureStarted {
                        projection: ID,
                        result: Ok(StreamId(1)),
                    },
                    53,
                );
                assert_eq!(out, vec![sent_geometry(size(), 1)]);
            }
            // The watermark moved: an older request is stale, a newer one is processed.
            assert!(
                f.handle(
                    control(
                        B,
                        Message::Resize {
                            projection: ID,
                            request: 1,
                            size: PixelSize::new(800, 600),
                            scale: 1.0,
                        },
                    ),
                    60,
                )
                .is_empty()
            );
            assert_eq!(
                f.handle(
                    control(
                        B,
                        Message::Resize {
                            projection: ID,
                            request: 2,
                            size: PixelSize::new(800, 600),
                            scale: 1.0,
                        },
                    ),
                    61,
                ),
                vec![Output::ResizeParked {
                    window: WINDOW,
                    size: PixelSize::new(800, 600),
                    scale: 1.0
                }]
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
                request: 1,
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
                request: 2,
                size: PixelSize::new(0, 0),
                scale: 2.0,
            },
        ),
        41,
    );
    // The refused request replaced the queued valid one: neither does platform work, and the
    // newest (the refused one) is answered when the window goes live.
    assert_eq!(
        f.handle(
            Input::CaptureStarted {
                projection: ID,
                result: Ok(StreamId(1))
            },
            50
        ),
        vec![sent_geometry(size(), 2)]
    );
}

#[test]
fn keys_follow_focus_into_the_projected_apps_own_popups_only() {
    let with = |id, pid, display| WindowInfo {
        pid: Some(pid),
        display: Some(display),
        ..window(id)
    };
    for (focus, pid, display, injects) in [
        // The app's own popup on the display the window is parked on (Safari's suggestions).
        (WindowId(50), 7, DISPLAY, true),
        // The same app's window elsewhere, and another app on the twin: keys stay back.
        (WindowId(51), 7, DisplayId(99), false),
        (WindowId(52), 8, DISPLAY, false),
    ] {
        let mut f = startup(3);
        f.handle(
            Input::Windows(WindowEvent::Changed(with(WINDOW, 7, DISPLAY))),
            39,
        );
        f.handle(
            Input::Windows(WindowEvent::Added(with(focus, pid, display))),
            39,
        );
        f.handle(Input::Windows(WindowEvent::Focused(Some(focus))), 40);
        let out = f.handle(input(B, press(1, true)), 41);
        assert_eq!(
            out.iter().any(|o| matches!(o, Output::Inject { .. })),
            injects,
            "{focus:?}: {out:?}"
        );
        if injects {
            f.handle(input(B, press(2, false)), 42);
        }
    }
}

#[test]
fn early_focus_activates_when_live_and_a_focused_window_is_left_alone() {
    let focus = |focused| {
        control(
            B,
            Message::Focus {
                projection: ID,
                focused,
            },
        )
    };
    // The proxy has focus before the capture is live: remembered, acted on when it is.
    let mut f = startup(2);
    f.handle(Input::Windows(WindowEvent::Focused(Some(WindowId(99)))), 35);
    assert!(f.handle(focus(true), 36).is_empty());
    let live = f.handle(
        Input::CaptureStarted {
            projection: ID,
            result: Ok(StreamId(1)),
        },
        40,
    );
    assert!(live.contains(&Output::ActivateWindow { window: WINDOW }));
    // Focus taken back before it went live: nothing to activate.
    let mut f = startup(2);
    f.handle(Input::Windows(WindowEvent::Focused(Some(WindowId(99)))), 35);
    f.handle(focus(true), 36);
    f.handle(focus(false), 37);
    let live = f.handle(
        Input::CaptureStarted {
            projection: ID,
            result: Ok(StreamId(1)),
        },
        40,
    );
    assert!(!live.contains(&Output::ActivateWindow { window: WINDOW }));
    // The window is already focused (window sources report only changes): no activation, and
    // keys go straight through instead of waiting for a confirmation that never comes.
    let mut f = startup(3);
    assert!(f.handle(focus(true), 40).is_empty());
    let keys = f.handle(input(B, press(1, true)), 41);
    assert!(
        keys.iter().any(|o| matches!(o, Output::Inject { .. })),
        "{keys:?}"
    );
    f.handle(input(B, press(2, false)), 42);
}

#[test]
fn source_focus_and_keyframe_controls_require_live_stage() {
    for stage in 0..4 {
        let mut f = startup(stage);
        f.handle(Input::Windows(WindowEvent::Focused(Some(WindowId(99)))), 35);
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
        assert!(out.contains(&Output::Restore {
            window: WINDOW,
            place: None
        }));
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
                vec![Output::Restore {
                    window: WINDOW,
                    place: None
                }]
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
            place: None,
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
                        parking: ParkingKind::Twin,
                        answers: 0
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
                Output::Restore { window, .. } => {
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
            for window in [WINDOW, WINDOW2] { sim.step(node, Input::Command(Command::Project { window, to: [B, A][node], place: None })); }
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
                0 => Input::Command(Command::Project { window: chosen_window, to: [B, A][node], place: None }),
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
                sim.step(node, Input::Command(Command::Project { window, to: [B, A][node], place: None }));
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

#[test]
fn a_parked_window_that_moves_by_itself_is_parked_again_at_the_wanted_size() {
    let mut f = startup(3);
    let mut moved = window(WINDOW);
    // A bar appeared on the twin display: the work area, and the window with it, moved down.
    moved.frame = RectLogical::new(
        PointLogical::new(0.0, 26.0),
        SizeLogical::new(320.25, 214.25),
    );
    let out = f.handle(Input::Windows(WindowEvent::Changed(moved.clone())), 80);
    assert_eq!(
        out,
        vec![Output::ResizeParked {
            window: WINDOW,
            size: size(),
            scale: 2.0,
        }]
    );
    // While that resize runs, further moves wait for its result.
    moved.frame = RectLogical::new(
        PointLogical::new(0.0, 0.0),
        SizeLogical::new(320.25, 240.25),
    );
    assert!(
        f.handle(Input::Windows(WindowEvent::Changed(moved.clone())), 90)
            .is_empty()
    );
    f.handle(
        Input::Parked {
            window: WINDOW,
            result: Ok(parked(WINDOW, PlatformParking::Twin, size())),
        },
        100,
    );
    // The same frame again (a title change, say) is not a move.
    moved.title = "renamed".into();
    let out = f.handle(Input::Windows(WindowEvent::Changed(moved)), 110);
    assert!(
        !out.iter().any(|o| matches!(o, Output::ResizeParked { .. })),
        "{out:?}"
    );
}

#[test]
fn focus_goes_back_to_the_previous_window_when_the_proxy_loses_focus() {
    let mut f = startup(3);
    let other = WindowId(WINDOW.0 + 100);
    f.handle(Input::Windows(WindowEvent::Added(window(other))), 40);
    f.handle(Input::Windows(WindowEvent::Focused(Some(other))), 41);
    let out = f.handle(
        control(
            B,
            Message::Focus {
                projection: ID,
                focused: true,
            },
        ),
        50,
    );
    assert!(
        out.contains(&Output::ActivateWindow { window: WINDOW }),
        "{out:?}"
    );
    let out = f.handle(
        control(
            B,
            Message::Focus {
                projection: ID,
                focused: false,
            },
        ),
        60,
    );
    assert!(
        out.contains(&Output::ActivateWindow { window: other }),
        "{out:?}"
    );
    // Only once: a second loss of focus has nothing to restore.
    let out = f.handle(
        control(
            B,
            Message::Focus {
                projection: ID,
                focused: false,
            },
        ),
        70,
    );
    assert!(
        !out.iter()
            .any(|o| matches!(o, Output::ActivateWindow { .. })),
        "{out:?}"
    );
}

#[test]
fn grace_round_trip_keeps_parking_and_proxy_and_resumes_input() {
    for kind in [PlatformParking::Twin, PlatformParking::Mirror] {
        let mut source = Fixture::source(kind);
        let mut destination = Fixture::destination();
        for event in [
            ProxyEvent::Key {
                usage: KEY,
                down: true,
            },
            ProxyEvent::Button {
                button: BUTTON,
                down: true,
                position: PointDevice::zero(),
            },
        ] {
            for msg in inputs(&destination.proxy(event, 10)) {
                let out = source.handle(input(B, msg), 10);
                source.confirm(&out, true, 10);
            }
        }
        let dropped = source.handle(closed(B), 20);
        assert_eq!(
            commands(&dropped),
            vec![
                up(),
                InjectCmd::Button {
                    button: BUTTON,
                    down: false
                }
            ]
        );
        assert_eq!(
            dropped.last(),
            Some(&Output::StopCapture {
                stream: StreamId(1)
            })
        );
        assert_eq!(dropped.len(), 3);
        source.confirm(&dropped, true, 20);
        assert!(source.held().is_empty());
        assert!(destination.handle(closed(A), 20).is_empty());
        assert_eq!(source.e2.next_deadline(), Some(ms(20_020)));
        assert_eq!(destination.e2.next_deadline(), Some(ms(20_020)));
        assert!(source.handle(input(B, press(99, true)), 21).is_empty());
        assert!(
            destination
                .proxy(
                    ProxyEvent::Key {
                        usage: KEY,
                        down: true
                    },
                    21
                )
                .is_empty()
        );
        assert!(
            destination
                .handle(Input::MediaError { key: key(A) }, 21)
                .is_empty()
        );
        assert!(source.handle(Input::PeerUp { peer: C }, 22).is_empty());
        assert!(source.handle(control(C, accepted()), 22).is_empty());

        let mut info = window(WINDOW);
        info.title = "updated while suspended".into();
        info.frame.size = SizeLogical::new(420.0, 260.0);
        assert!(
            source
                .handle(Input::Windows(WindowEvent::Changed(info)), 30)
                .is_empty()
        );
        let resumed = source.handle(Input::PeerUp { peer: B }, 100);
        assert_eq!(
            messages(&resumed),
            vec![Message::Start {
                projection: ID,
                window: WindowSummary {
                    title: "updated while suspended".into(),
                    app_id: "test".into()
                },
                size: PixelSize::new(420, 260),
            }]
        );
        assert_eq!(resumed.len(), 1);
        assert!(source.handle(Input::PeerUp { peer: B }, 101).is_empty());
        assert!(
            destination
                .handle(Input::PeerUp { peer: A }, 100)
                .is_empty()
        );
        let accepted_out = destination.handle(control(A, messages(&resumed)[0].clone()), 101);
        assert_eq!(
            accepted_out,
            resume_messages(size(), 2.0, 1)
                .into_iter()
                .map(|msg| Output::SendControl {
                    peer: A,
                    msg: ControlMessage::Projection(msg),
                })
                .collect::<Vec<_>>()
        );
        let capture = source.handle(control(B, accepted()), 102);
        let parking = if kind == PlatformParking::Twin {
            ParkingKind::Twin
        } else {
            ParkingKind::Mirror
        };
        assert_eq!(
            capture,
            vec![
                Output::StartCapture {
                    projection: ID,
                    peer: B,
                    target: if kind == PlatformParking::Twin {
                        CaptureTarget::Display(DISPLAY)
                    } else {
                        CaptureTarget::Window(WINDOW)
                    },
                    crop: if kind == PlatformParking::Twin {
                        Some(parked(WINDOW, kind, size()).content)
                    } else {
                        None
                    },
                    max_fps: 60,
                },
                Output::SendControl {
                    peer: B,
                    msg: ControlMessage::Projection(Message::Geometry {
                        projection: ID,
                        size: size(),
                        parking,
                        answers: 0
                    })
                },
            ]
        );
        source.handle(
            Input::CaptureStarted {
                projection: ID,
                result: Ok(StreamId(2)),
            },
            103,
        );
        // The pre-drop held set was cleared: old physical ups are dropped, fresh downs flow.
        assert!(
            destination
                .proxy(
                    ProxyEvent::Key {
                        usage: KEY,
                        down: false
                    },
                    104
                )
                .is_empty()
        );
        for msg in inputs(&destination.proxy(
            ProxyEvent::Key {
                usage: KEY,
                down: true,
            },
            105,
        )) {
            assert_eq!(
                commands(&source.handle(input(B, msg), 105)),
                vec![InjectCmd::Key {
                    usage: KEY,
                    down: true
                }]
            );
        }
        assert_eq!(source.held(), vec![Held::Key(KEY)]);
    }
}

#[test]
fn grace_expires_at_exact_deadline_with_the_original_link_lost_cleanup() {
    let mut source = Fixture::source(PlatformParking::Twin);
    let mut destination = Fixture::destination();
    source.handle(input(B, press(1, true)), 50);
    let dropped = source.handle(closed(B), 100);
    source.confirm(&dropped, true, 100);
    assert!(destination.handle(closed(A), 100).is_empty());
    assert_eq!(source.e2.next_deadline(), Some(ms(20_100)));
    assert_eq!(destination.e2.next_deadline(), Some(ms(20_100)));
    assert!(source.handle(Input::Tick, 20_099).is_empty());
    assert!(destination.handle(Input::Tick, 20_099).is_empty());
    assert_eq!(
        source.handle(Input::Tick, 20_100),
        vec![
            Output::Restore {
                window: WINDOW,
                place: None
            },
            Output::Notice(Notice::ProjectionEnded {
                key: key(A),
                reason: Reason::LinkLost
            }),
        ]
    );
    assert_eq!(
        destination.handle(Input::Tick, 20_100),
        vec![
            Output::CloseProxy { key: key(A) },
            Output::Notice(Notice::ProjectionEnded {
                key: key(A),
                reason: Reason::LinkLost
            }),
        ]
    );
    assert_eq!(source.e2.next_deadline(), None);
    assert_eq!(destination.e2.next_deadline(), None);
    assert!(source.handle(Input::PeerUp { peer: B }, 20_101).is_empty());
    assert!(source.handle(Input::Tick, 30_000).is_empty());
    assert!(destination.handle(Input::Tick, 30_000).is_empty());
}

#[test]
fn grace_only_applies_after_offered_and_all_later_startup_stages_suspend() {
    let mut f = offered();
    assert_eq!(
        f.handle(closed(B), 100),
        vec![Output::Notice(Notice::ProjectionEnded {
            key: key(A),
            reason: Reason::LinkLost,
        })]
    );
    assert_eq!(f.e2.next_deadline(), None);
    assert!(f.handle(Input::PeerUp { peer: B }, 101).is_empty());
    for stage in 1..=3 {
        let mut f = startup(stage);
        let out = f.handle(closed(B), 100);
        assert_eq!(
            out,
            if stage == 3 {
                vec![Output::StopCapture {
                    stream: StreamId(1),
                }]
            } else {
                vec![]
            }
        );
        assert_eq!(f.e2.next_deadline(), Some(ms(20_100)));
        let end = f.handle(Input::Tick, 20_100);
        assert_eq!(
            end,
            vec![
                Output::Restore {
                    window: WINDOW,
                    place: None
                },
                Output::Notice(Notice::ProjectionEnded {
                    key: key(A),
                    reason: Reason::LinkLost
                }),
            ]
        );
        // Pending platform operations retain the existing bounded cleanup after ending.
        if stage == 1 {
            assert_eq!(f.e2.next_deadline(), Some(ms(25_100)));
            assert_eq!(
                f.handle(
                    Input::Parked {
                        window: WINDOW,
                        result: Err(Failure::Other)
                    },
                    20_101
                ),
                vec![Output::Restore {
                    window: WINDOW,
                    place: None
                }]
            );
        }
        if stage == 2 {
            assert_eq!(
                f.handle(
                    Input::CaptureStarted {
                        projection: ID,
                        result: Ok(StreamId(9))
                    },
                    20_101
                ),
                vec![Output::StopCapture {
                    stream: StreamId(9)
                }]
            );
        }
        assert_eq!(f.e2.next_deadline(), None);
    }
}

#[test]
fn grace_source_return_window_close_lock_sleep_panic_and_revocation_end_locally() {
    for (event, reason) in [
        (Input::Command(Command::Return(key(A))), Reason::Returned),
        (
            Input::Windows(WindowEvent::Removed(WINDOW)),
            Reason::WindowClosed,
        ),
        (locked(), Reason::Locked),
        (Input::Session(SessionEvent::WillSleep), Reason::Locked),
        (Input::Command(Command::Panic), Reason::Returned),
        (Input::Grants(Default::default()), Reason::Revoked),
    ] {
        let mut f = Fixture::source(PlatformParking::Twin);
        f.handle(closed(B), 100);
        assert_eq!(
            f.handle(event, 101),
            vec![
                Output::Restore {
                    window: WINDOW,
                    place: None
                },
                Output::Notice(Notice::ProjectionEnded {
                    key: key(A),
                    reason
                }),
            ]
        );
        assert_eq!(f.e2.next_deadline(), None);
        assert!(f.handle(Input::PeerUp { peer: B }, 102).is_empty());
    }
}

#[test]
fn grace_destination_return_proxy_close_lock_sleep_panic_and_revocation_end_locally() {
    for (event, reason) in [
        (Input::Command(Command::Return(key(A))), Reason::Returned),
        (
            Input::Proxy {
                key: key(A),
                event: ProxyEvent::CloseRequested,
            },
            Reason::Returned,
        ),
        (
            Input::Proxy {
                key: key(A),
                event: ProxyEvent::Lost,
            },
            Reason::Failed,
        ),
        (locked(), Reason::Locked),
        (Input::Session(SessionEvent::WillSleep), Reason::Locked),
        (Input::Command(Command::Panic), Reason::Returned),
        (Input::Grants(Default::default()), Reason::Revoked),
    ] {
        let mut f = Fixture::destination();
        f.proxy(
            ProxyEvent::Key {
                usage: KEY,
                down: true,
            },
            50,
        );
        f.handle(closed(A), 100);
        assert_eq!(
            f.handle(event, 101),
            vec![
                Output::CloseProxy { key: key(A) },
                Output::Notice(Notice::ProjectionEnded {
                    key: key(A),
                    reason
                }),
            ]
        );
        assert_eq!(f.e2.next_deadline(), None);
    }
}

#[test]
fn grace_drops_restart_the_deadline_in_suspension_and_during_resume() {
    let mut source = Fixture::source(PlatformParking::Twin);
    let mut destination = Fixture::destination();
    source.handle(closed(B), 100);
    destination.handle(closed(A), 100);
    assert!(source.handle(closed(B), 10_000).is_empty());
    assert!(destination.handle(closed(A), 10_000).is_empty());
    assert_eq!(source.e2.next_deadline(), Some(ms(30_000)));
    assert_eq!(destination.e2.next_deadline(), Some(ms(30_000)));
    assert!(source.handle(Input::Tick, 20_100).is_empty());
    assert!(destination.handle(Input::Tick, 20_100).is_empty());
    assert_eq!(
        messages(&source.handle(Input::PeerUp { peer: B }, 21_000)).len(),
        1
    );
    assert_eq!(source.e2.next_deadline(), Some(ms(31_000)));
    assert!(source.handle(closed(B), 22_000).is_empty());
    destination.handle(closed(A), 22_000);
    assert_eq!(source.e2.next_deadline(), Some(ms(42_000)));
    assert_eq!(destination.e2.next_deadline(), Some(ms(42_000)));
    for stream in 2..=3 {
        let now = 23_000 + (stream - 2) * 1_000;
        assert_eq!(
            messages(&source.handle(Input::PeerUp { peer: B }, now)).len(),
            1
        );
        assert_eq!(
            messages(&destination.handle(control(A, start()), now)),
            resume_messages(size(), 2.0, stream as u32 - 1)
        );
        assert!(matches!(
            source.handle(control(B, accepted()), now)[0],
            Output::StartCapture { .. }
        ));
        source.handle(
            Input::CaptureStarted {
                projection: ID,
                result: Ok(StreamId(stream)),
            },
            now,
        );
        assert_eq!(
            source.handle(closed(B), now + 10),
            vec![Output::StopCapture {
                stream: StreamId(stream)
            }]
        );
        assert!(destination.handle(closed(A), now + 10).is_empty());
    }
    assert_eq!(source.e2.next_deadline(), Some(ms(44_010)));
    assert_eq!(destination.e2.next_deadline(), Some(ms(44_010)));
}

#[test]
fn grace_resume_resizes_changed_size_or_scale_before_capture() {
    for kind in [PlatformParking::Twin, PlatformParking::Mirror] {
        for (new_size, scale) in [(PixelSize::new(800, 600), 2.0), (size(), 1.5)] {
            let mut f = Fixture::source(kind);
            f.handle(closed(B), 100);
            f.handle(Input::PeerUp { peer: B }, 200);
            assert_eq!(
                f.handle(
                    control(
                        B,
                        Message::Accepted {
                            projection: ID,
                            size: new_size,
                            scale
                        }
                    ),
                    201
                ),
                vec![Output::ResizeParked {
                    window: WINDOW,
                    size: new_size,
                    scale
                }]
            );
            let out = f.handle(
                Input::Parked {
                    window: WINDOW,
                    result: Ok(parked(WINDOW, kind, new_size)),
                },
                202,
            );
            assert!(matches!(out[0], Output::StartCapture { .. }));
            assert_eq!(
                messages(&out),
                vec![Message::Geometry {
                    projection: ID,
                    size: new_size,
                    parking: if kind == PlatformParking::Twin {
                        ParkingKind::Twin
                    } else {
                        ParkingKind::Mirror
                    },
                    answers: 0,
                }]
            );
            assert_eq!(out.len(), 2);
            assert!(
                f.handle(
                    Input::CaptureStarted {
                        projection: ID,
                        result: Ok(StreamId(2))
                    },
                    203
                )
                .is_empty()
            );
            assert_eq!(
                commands(&f.handle(input(B, press(1, true)), 204)),
                vec![InjectCmd::Key {
                    usage: KEY,
                    down: true
                }]
            );
        }
    }
}

#[test]
fn grace_resume_refusal_or_offer_timeout_restores_with_link_lost() {
    for refused in [false, true] {
        let mut f = Fixture::source(PlatformParking::Twin);
        f.handle(closed(B), 100);
        f.handle(Input::PeerUp { peer: B }, 200);
        let out = if refused {
            f.handle(
                control(
                    B,
                    Message::Refused {
                        projection: ID,
                        reason: Refusal::Busy,
                    },
                ),
                201,
            )
        } else {
            assert_eq!(f.e2.next_deadline(), Some(ms(10_200)));
            assert!(f.handle(Input::Tick, 10_199).is_empty());
            f.handle(Input::Tick, 10_200)
        };
        assert_eq!(
            out[0],
            Output::Restore {
                window: WINDOW,
                place: None
            }
        );
        assert_eq!(
            out.last(),
            Some(&Output::Notice(Notice::ProjectionEnded {
                key: key(A),
                reason: Reason::LinkLost
            }))
        );
        assert_eq!(
            messages(&out),
            if refused {
                vec![]
            } else {
                vec![Message::End {
                    projection: ID,
                    reason: Reason::LinkLost,
                }]
            }
        );
        assert_eq!(f.e2.next_deadline(), None);
    }
}

#[test]
fn grace_destination_accepts_the_latest_proxy_size_without_reopening() {
    let mut f = Fixture::destination();
    // A resize still in the 50 ms coalescing slot must also become the resume size.
    f.proxy(
        ProxyEvent::Resized {
            size: PixelSize::new(700, 500),
            scale: 1.5,
        },
        1,
    );
    f.proxy(
        ProxyEvent::Resized {
            size: PixelSize::new(710, 510),
            scale: 1.5,
        },
        2,
    );
    f.handle(closed(A), 3);
    let current = PixelSize::new(900, 700);
    assert!(
        f.proxy(
            ProxyEvent::Resized {
                size: current,
                scale: 1.25
            },
            4
        )
        .is_empty()
    );
    for event in [
        ProxyEvent::Focus(true),
        ProxyEvent::Focus(false),
        ProxyEvent::Key {
            usage: KEY,
            down: true,
        },
        ProxyEvent::Button {
            button: BUTTON,
            down: true,
            position: PointDevice::zero(),
        },
        ProxyEvent::Motion {
            position: PointDevice::zero(),
        },
        ProxyEvent::Scroll {
            position: PointDevice::zero(),
            delta: delta(),
        },
    ] {
        assert!(f.proxy(event, 5).is_empty());
    }
    assert!(f.handle(Input::Tick, 100).is_empty());
    let out = f.handle(control(A, start()), 200);
    // The resume asks again, for the size the proxy has now, with the next request number.
    assert_eq!(messages(&out), resume_messages(current, 1.25, 2));
    assert_eq!(out.len(), 2);
    assert!(f.handle(control(A, start()), 201).is_empty());
    let heartbeat_out = f.handle(Input::Tick, 450);
    assert_eq!(inputs(&heartbeat_out).len(), 1);
    assert!(
        matches!(&inputs(&heartbeat_out)[0], ProjInput::Held { keys, buttons, .. } if keys.is_empty() && buttons.is_empty())
    );
    assert_eq!(
        messages(&f.handle(Input::MediaError { key: key(A) }, 451)),
        vec![Message::KeyFrameRequest { projection: ID }]
    );
}

#[test]
fn grace_pending_proxy_open_finishes_without_sending_on_the_closed_link() {
    for opens_before_start in [false, true] {
        let mut f = Fixture::ready(B, A);
        f.handle(control(A, start()), 0);
        f.handle(closed(A), 1);
        if opens_before_start {
            assert!(
                f.handle(
                    Input::ProxyOpened {
                        key: key(A),
                        result: Ok((size(), 2.0))
                    },
                    2
                )
                .is_empty()
            );
        }
        let out = f.handle(control(A, start()), 3);
        if opens_before_start {
            assert_eq!(messages(&out), resume_messages(size(), 2.0, 1));
        } else {
            assert!(out.is_empty());
            assert_eq!(f.e2.next_deadline(), Some(ms(10_003)));
            assert_eq!(
                messages(&f.handle(
                    Input::ProxyOpened {
                        key: key(A),
                        result: Ok((size(), 2.0))
                    },
                    4
                )),
                vec![accepted()]
            );
        }
    }
}

#[test]
fn grace_resume_waits_for_old_parking_and_capture_results_and_drops_queued_resize() {
    for stage in 1..=3 {
        let mut f = startup(stage);
        if stage == 3 {
            f.handle(
                control(
                    B,
                    Message::Resize {
                        projection: ID,
                        request: 1,
                        size: PixelSize::new(700, 500),
                        scale: 2.0,
                    },
                ),
                40,
            );
            f.handle(
                control(
                    B,
                    Message::Resize {
                        projection: ID,
                        request: 2,
                        size: PixelSize::new(900, 700),
                        scale: 2.0,
                    },
                ),
                41,
            );
        }
        f.handle(closed(B), 50);
        f.handle(Input::PeerUp { peer: B }, 100);
        assert!(f.handle(control(B, accepted()), 101).is_empty());
        let out = if stage == 2 {
            let out = f.handle(
                Input::CaptureStarted {
                    projection: ID,
                    result: Ok(StreamId(99)),
                },
                102,
            );
            assert_eq!(
                out[0],
                Output::StopCapture {
                    stream: StreamId(99)
                }
            );
            out[1..].to_vec()
        } else {
            let out = f.handle(
                Input::Parked {
                    window: WINDOW,
                    result: Ok(parked(
                        WINDOW,
                        PlatformParking::Twin,
                        if stage == 3 {
                            PixelSize::new(700, 500)
                        } else {
                            size()
                        },
                    )),
                },
                102,
            );
            if stage == 3 {
                assert_eq!(
                    out,
                    vec![Output::ResizeParked {
                        window: WINDOW,
                        size: size(),
                        scale: 2.0
                    }]
                );
                f.handle(
                    Input::Parked {
                        window: WINDOW,
                        result: Ok(parked(WINDOW, PlatformParking::Twin, size())),
                    },
                    103,
                )
            } else {
                out
            }
        };
        assert!(matches!(out[0], Output::StartCapture { .. }));
        assert_eq!(out.len(), 2);
        assert!(
            f.handle(
                Input::CaptureStarted {
                    projection: ID,
                    result: Ok(StreamId(2))
                },
                104
            )
            .is_empty()
        );
    }
}

#[test]
fn grace_late_platform_results_while_suspended_keep_parking_and_stop_capture() {
    for stage in 1..=2 {
        let mut f = startup(stage);
        f.handle(closed(B), 50);
        let out = if stage == 1 {
            f.handle(
                Input::Parked {
                    window: WINDOW,
                    result: Ok(parked(WINDOW, PlatformParking::Twin, size())),
                },
                51,
            )
        } else {
            f.handle(
                Input::CaptureStarted {
                    projection: ID,
                    result: Ok(StreamId(99)),
                },
                51,
            )
        };
        assert_eq!(
            out,
            if stage == 1 {
                vec![]
            } else {
                vec![Output::StopCapture {
                    stream: StreamId(99),
                }]
            }
        );
        assert_eq!(f.e2.next_deadline(), Some(ms(20_050)));
        f.handle(Input::PeerUp { peer: B }, 100);
        assert!(matches!(
            f.handle(control(B, accepted()), 101)[0],
            Output::StartCapture { .. }
        ));
    }
}

#[test]
fn grace_resume_preserves_failed_release_retries_and_stale_completion_generations() {
    let mut f = Fixture::source(PlatformParking::Twin);
    f.handle(input(B, press(1, true)), 10);
    let out = f.handle(input(B, button(2, true, PointDevice::zero())), 10);
    f.confirm(&out, true, 10);
    let dropped = f.handle(closed(B), 20);
    let old = injections(&dropped)[0].0;
    f.confirm(&dropped, false, 20);
    f.handle(Input::PeerUp { peer: B }, 30);
    assert_eq!(f.e2.next_deadline(), Some(ms(70)));
    let retries = f.handle(Input::Tick, 70);
    assert_eq!(
        commands(&retries),
        vec![
            up(),
            InjectCmd::Button {
                button: BUTTON,
                down: false
            }
        ]
    );
    f.handle(control(B, accepted()), 71);
    f.handle(
        Input::CaptureStarted {
            projection: ID,
            result: Ok(StreamId(2)),
        },
        72,
    );
    f.handle(input(B, press(3, true)), 73);
    f.handle(Input::InjectDone { id: old, ok: true }, 74);
    f.confirm(&retries, true, 74);
    assert_eq!(f.held(), vec![Held::Key(KEY)]);
    let release = f.handle(input(B, press(4, false)), 75);
    assert_eq!(commands(&release), vec![up()]);
    f.confirm(&release, true, 75);
    assert!(f.held().is_empty());
}

#[test]
fn grace_expired_projections_cannot_resume_without_a_tick() {
    let mut source = Fixture::source(PlatformParking::Twin);
    let mut destination = Fixture::destination();
    source.handle(closed(B), 100);
    destination.handle(closed(A), 100);
    assert_eq!(
        source.handle(Input::PeerUp { peer: B }, 20_100),
        vec![
            Output::Restore {
                window: WINDOW,
                place: None
            },
            Output::Notice(Notice::ProjectionEnded {
                key: key(A),
                reason: Reason::LinkLost
            }),
        ]
    );
    assert_eq!(
        destination.handle(control(A, start()), 20_100),
        vec![
            Output::CloseProxy { key: key(A) },
            Output::Notice(Notice::ProjectionEnded {
                key: key(A),
                reason: Reason::LinkLost
            }),
        ]
    );
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1_000, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn grace_random_drop_releases_every_injected_key_and_button_immediately(
        events in prop::collection::vec((any::<bool>(), any::<bool>()), 1..100),
        drop_seed in any::<usize>(),
    ) {
        let mut f = Fixture::source(PlatformParking::Twin);
        let mut injected = BTreeSet::new();
        let drop_at = drop_seed % (events.len() + 1);
        for (index, (is_key, down)) in events.into_iter().take(drop_at).enumerate() {
            let seq = index as u32 + 1;
            let msg = if is_key { press(seq, down) } else { button(seq, down, PointDevice::zero()) };
            let mut out = f.handle(input(B, msg), index as u64);
            // The targeting answer can issue the deferred button, as the agent's settle does.
            out.extend(f.confirm(&out, true, index as u64));
            for cmd in commands(&out) {
                match cmd {
                    InjectCmd::Key { usage, down } => transition_fake(&mut injected, Held::Key(usage), down),
                    InjectCmd::Button { button, down } => transition_fake(&mut injected, Held::Button(button), down),
                    _ => {},
                }
            }
            f.confirm(&out, true, index as u64);
        }
        let held_at_drop = injected.clone();
        let now = drop_at as u64 + 1;
        let out = f.handle(closed(B), now);
        prop_assert_eq!(commands(&out).len(), held_at_drop.len());
        let releases: BTreeSet<_> = commands(&out).into_iter().map(|cmd| match cmd {
            InjectCmd::Key { usage, down: false } => Held::Key(usage),
            InjectCmd::Button { button, down: false } => Held::Button(button),
            _ => panic!("drop emitted something other than a release"),
        }).collect();
        prop_assert_eq!(&releases, &held_at_drop);
        for item in releases { injected.remove(&item); }
        prop_assert!(injected.is_empty());
        prop_assert_eq!(out.len(), commands(&out).len() + 1);
        prop_assert_eq!(out.last(), Some(&Output::StopCapture { stream: StreamId(1) }));
        f.confirm(&out, true, now);
        prop_assert!(f.held().is_empty());
        prop_assert_eq!(f.e2.next_deadline(), Some(ms(now + 20_000)));
    }
}
