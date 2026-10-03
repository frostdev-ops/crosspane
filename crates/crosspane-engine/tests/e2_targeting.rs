#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use crosspane_engine::e2::E2;
use crosspane_engine::{
    Command, EngineConfig, InjectCmd, InjectId, Input, Notice, Output, ProjectionKey,
};
use crosspane_input::Held;
use crosspane_input::journal::{Journal, JournalError};
use crosspane_platform::{
    LockState, Parked, ParkingKind, SessionEvent, SessionState, StreamId, WindowEvent, WindowInfo,
    WindowRole, WindowState,
};
use crosspane_protocol::link::{LinkError, LinkEvent};
use crosspane_protocol::msg::{Capability, ControlMessage, InputMessage};
use crosspane_protocol::projection::{ProjInput, ProjectionEndReason as Reason, ProjectionMessage};
use crosspane_types::geom::{
    PixelRect, PixelSize, PointDevice, PointLogical, RectLogical, SizeLogical,
};
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::id::{DisplayId, NodeId, ProjectionId, WindowId};
use crosspane_types::input::{ScrollDelta, ScrollPhase};
use crosspane_types::time::MonoTime;

const OWNER: NodeId = NodeId([1; 32]);
const PEER: NodeId = NodeId([2; 32]);
const ID: ProjectionId = ProjectionId(1);
const DISPLAY_A: DisplayId = DisplayId(1);
const DISPLAY_B: DisplayId = DisplayId(2);
const BUTTON: MouseButton = MouseButton(1);
const KEY: HidUsage = HidUsage::keyboard(4);

#[derive(Clone, Default)]
struct SharedJournal(Arc<Mutex<BTreeSet<Held>>>);

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
        Ok(self.0.lock().unwrap().iter().copied().collect())
    }
}

struct Fixture {
    e2: E2,
    journal: SharedJournal,
    now: MonoTime,
}

impl Fixture {
    fn new() -> Self {
        let journal = SharedJournal::default();
        let (e2, out) = E2::new(
            &EngineConfig::new(OWNER),
            Box::new(journal.clone()),
            MonoTime::ZERO,
        )
        .unwrap();
        assert!(out.is_empty());
        let mut fixture = Self {
            e2,
            journal,
            now: MonoTime::ZERO,
        };
        fixture.handle(Input::Session(SessionEvent::State(SessionState {
            lock: LockState::Unlocked,
            active: Some(true),
        })));
        fixture.handle(Input::PeerUp { peer: PEER });
        fixture.handle(Input::Grants(
            [(PEER, [Capability::WindowShare].into())].into(),
        ));
        fixture.project(WindowId(10), ID, DISPLAY_B);
        fixture
    }

    fn project(&mut self, window: WindowId, projection: ProjectionId, display: DisplayId) {
        self.project_to(window, projection, display, PEER);
    }

    fn project_to(
        &mut self,
        window: WindowId,
        projection: ProjectionId,
        display: DisplayId,
        peer: NodeId,
    ) {
        self.handle(Input::Windows(WindowEvent::Added(WindowInfo {
            id: window,
            title: "targeting fixture".into(),
            app_id: "test".into(),
            pid: None,
            display: Some(display),
            frame: RectLogical::new(PointLogical::zero(), SizeLogical::new(640.0, 480.0)),
            state: WindowState::Normal,
            role: WindowRole::Toplevel,
            parent: None,
        })));
        self.handle(Input::Windows(WindowEvent::Focused(Some(window))));
        self.handle(Input::Command(Command::Project {
            window,
            to: peer,
            place: None,
        }));
        self.handle(Input::Link(LinkEvent::Control {
            peer,
            msg: ControlMessage::Projection(ProjectionMessage::Accepted {
                projection,
                size: PixelSize::new(640, 480),
                scale: 1.0,
            }),
        }));
        self.handle(Input::Parked {
            window,
            result: Ok(Parked {
                fullscreen: false,
                window,
                kind: ParkingKind::Twin,
                display,
                content: PixelRect::new(
                    crosspane_types::geom::euclid::Point2D::new(0, 0),
                    crosspane_types::geom::euclid::Point2D::new(640, 480),
                ),
            }),
        });
        self.handle(Input::CaptureStarted {
            projection,
            result: Ok(StreamId(projection.0)),
        });
    }

    fn handle(&mut self, input: Input) -> Vec<Output> {
        let mut out = Vec::new();
        self.e2.handle(&input, self.now, &mut out);
        out
    }

    fn at(&mut self, milliseconds: u64, input: Input) -> Vec<Output> {
        self.now = MonoTime::from_nanos(milliseconds * 1_000_000);
        self.handle(input)
    }

    fn input(&mut self, msg: ProjInput) -> Vec<Output> {
        self.input_from(PEER, msg)
    }

    fn input_from(&mut self, peer: NodeId, msg: ProjInput) -> Vec<Output> {
        self.handle(Input::Link(LinkEvent::Input {
            peer,
            msg: InputMessage::Proj(msg),
        }))
    }

    fn done(&mut self, id: InjectId, ok: bool) -> Vec<Output> {
        self.handle(Input::InjectDone { id, ok })
    }

    fn held(&self) -> Vec<Held> {
        self.journal.held().unwrap()
    }
}

fn button(seq: u32, down: bool) -> ProjInput {
    ProjInput::Button {
        projection: ID,
        seq,
        button: BUTTON,
        down,
        position: PointDevice::new(10.0, 20.0),
    }
}

fn motion(projection: ProjectionId, seq: u32, x: f64) -> ProjInput {
    ProjInput::Motion {
        projection,
        seq,
        position: PointDevice::new(x, 20.0),
    }
}

fn key(seq: u32, down: bool) -> ProjInput {
    ProjInput::Key {
        projection: ID,
        seq,
        usage: KEY,
        down,
    }
}

fn scroll(seq: u32) -> ProjInput {
    ProjInput::Scroll {
        projection: ID,
        seq,
        position: PointDevice::new(10.0, 20.0),
        delta: ScrollDelta {
            pixels: None,
            v120_x: 0,
            v120_y: 120,
            phase: ScrollPhase::Discrete,
            stop_x: false,
            stop_y: false,
        },
    }
}

fn injections(out: &[Output]) -> Vec<(InjectId, InjectCmd)> {
    out.iter()
        .filter_map(|output| match output {
            Output::Inject { id, cmd } => Some((*id, cmd.clone())),
            _ => None,
        })
        .collect()
}

fn commands(out: &[Output]) -> Vec<InjectCmd> {
    injections(out).into_iter().map(|(_, cmd)| cmd).collect()
}

fn targeting(out: &[Output]) -> InjectId {
    let injected = injections(out);
    assert_eq!(
        injected.len(),
        1,
        "only the targeting move is issued: {out:?}"
    );
    assert!(matches!(
        injected[0].1,
        InjectCmd::MoveTo {
            display: DISPLAY_B,
            ..
        }
    ));
    injected[0].0
}

#[test]
fn rejected_targeting_drops_press_release_and_scroll() {
    let mut f = Fixture::new();
    let other = ProjectionId(2);
    f.project(WindowId(11), other, DISPLAY_A);
    let out = f.input(motion(other, 1, 30.0));
    assert!(matches!(
        commands(&out).as_slice(),
        [InjectCmd::MoveTo {
            display: DISPLAY_A,
            ..
        }]
    ));
    f.done(injections(&out)[0].0, true);
    let out = f.input(button(1, true));
    let id = targeting(&out);
    assert!(f.held().is_empty());
    assert!(f.done(id, false).is_empty());
    assert!(f.held().is_empty());
    assert!(f.input(button(2, false)).is_empty());
    let out = f.input(scroll(3));
    let id = targeting(&out);
    assert!(f.done(id, false).is_empty());
    assert!(f.held().is_empty());
}

#[test]
fn accepted_targeting_records_press_and_held_release_is_unconditional() {
    for move_ok in [true, false] {
        let mut f = Fixture::new();
        let out = f.input(button(1, true));
        let id = targeting(&out);
        assert!(f.held().is_empty());
        let out = f.done(id, true);
        assert_eq!(
            commands(&out),
            vec![InjectCmd::Button {
                button: BUTTON,
                down: true
            }]
        );
        assert_eq!(f.held(), vec![Held::Button(BUTTON)]);
        let out = f.input(button(2, false));
        assert_eq!(
            commands(&out),
            vec![
                InjectCmd::MoveTo {
                    display: DISPLAY_B,
                    position: PointDevice::new(10.0, 20.0)
                },
                InjectCmd::Button {
                    button: BUTTON,
                    down: false
                },
            ]
        );
        let injected = injections(&out);
        assert!(f.done(injected[0].0, move_ok).is_empty());
        assert!(f.done(injected[1].0, true).is_empty());
        assert!(f.held().is_empty());
        let out = f.input(scroll(3));
        let id = targeting(&out);
        let out = f.done(id, true);
        assert!(matches!(commands(&out).as_slice(), [InjectCmd::Scroll(_)]));
    }
}

#[test]
fn targeting_global_fifo_preserves_order_on_success_and_failure() {
    for ok in [true, false] {
        let mut f = Fixture::new();
        let other = ProjectionId(2);
        f.project(WindowId(11), other, DISPLAY_A);
        f.handle(Input::Windows(WindowEvent::Focused(Some(WindowId(10)))));
        let out = f.input(button(1, true));
        let id = targeting(&out);
        assert!(f.input(motion(ID, 2, 30.0)).is_empty());
        assert!(f.input(key(3, true)).is_empty());
        assert!(f.input(button(4, false)).is_empty());
        let out = f.input(motion(other, 1, 40.0));
        assert!(out.is_empty());
        let out = f.done(id, ok);
        let mut expected = Vec::new();
        if ok {
            expected.push(InjectCmd::Button {
                button: BUTTON,
                down: true,
            });
        }
        expected.extend([
            InjectCmd::MoveTo {
                display: DISPLAY_B,
                position: PointDevice::new(30.0, 20.0),
            },
            InjectCmd::Key {
                usage: KEY,
                down: true,
            },
        ]);
        if ok {
            expected.extend([
                InjectCmd::MoveTo {
                    display: DISPLAY_B,
                    position: PointDevice::new(10.0, 20.0),
                },
                InjectCmd::Button {
                    button: BUTTON,
                    down: false,
                },
            ]);
        }
        expected.push(InjectCmd::MoveTo {
            display: DISPLAY_A,
            position: PointDevice::new(40.0, 20.0),
        });
        assert_eq!(commands(&out), expected);
    }
}

#[test]
fn targeting_teardown_discards_pending_actions_and_stale_results() {
    for pending in [button(1, true), scroll(1)] {
        for teardown in [
            Input::Command(Command::Return(ProjectionKey {
                source: OWNER,
                projection: ID,
            })),
            Input::Command(Command::Panic),
            Input::Link(LinkEvent::Closed {
                peer: PEER,
                error: LinkError::Closed,
            }),
        ] {
            let mut f = Fixture::new();
            let out = f.input(pending.clone());
            let id = targeting(&out);
            assert!(f.input(key(2, true)).is_empty());
            assert!(f.input(button(3, false)).is_empty());
            let out = f.handle(teardown);
            assert!(commands(&out).is_empty());
            assert!(f.done(id, true).is_empty());
            assert!(f.held().is_empty());
            assert!(f.input(button(4, false)).is_empty());
        }
    }
}

#[test]
fn targeting_results_match_only_their_move_and_fifo_waits_again_for_scroll() {
    let mut f = Fixture::new();
    let out = f.input(button(1, true));
    let old = targeting(&out);
    assert!(f.input(scroll(2)).is_empty());
    assert!(f.input(button(3, false)).is_empty());
    let out = f.done(old, false);
    let new = targeting(&out);
    assert_ne!(old, new);
    assert!(f.done(old, true).is_empty());
    assert!(f.held().is_empty());
    let out = f.done(new, true);
    assert!(matches!(commands(&out).as_slice(), [InjectCmd::Scroll(_)]));
    assert!(f.done(new, true).is_empty());
}

#[test]
fn targeting_queue_overflow_ends_source_and_releases_everything_held() {
    let mut f = Fixture::new();
    f.input(key(1, true));
    let out = f.input(button(2, true));
    let id = targeting(&out);
    f.done(id, true);
    let out = f.input(scroll(3));
    let pending = targeting(&out);
    for seq in 4..68 {
        assert!(f.input(motion(ID, seq, 40.0)).is_empty());
    }
    let out = f.input(motion(ID, 68, 50.0));
    assert!(out.iter().any(|output| matches!(
        output,
        Output::Notice(Notice::ProjectionEnded {
            reason: Reason::Failed,
            ..
        })
    )));
    assert_eq!(
        commands(&out),
        vec![
            InjectCmd::Key {
                usage: KEY,
                down: false
            },
            InjectCmd::Button {
                button: BUTTON,
                down: false
            },
        ]
    );
    assert!(f.done(pending, true).is_empty());
    for (id, _) in injections(&out) {
        f.done(id, true);
    }
    assert!(f.held().is_empty());
}

#[test]
fn held_heartbeats_stay_immediate_and_lease_release_all_cancels_targeting() {
    let mut f = Fixture::new();
    f.input(key(1, true));
    let out = f.input(button(2, true));
    let id = targeting(&out);
    let out = f.input(ProjInput::Held {
        projection: ID,
        seq: 3,
        keys: vec![],
        buttons: vec![],
    });
    assert_eq!(
        commands(&out),
        vec![InjectCmd::Key {
            usage: KEY,
            down: false
        }]
    );
    let out = f.done(id, true);
    assert_eq!(
        commands(&out),
        vec![InjectCmd::Button {
            button: BUTTON,
            down: true
        }]
    );
    let out = f.input(scroll(4));
    let pending = targeting(&out);
    let out = f.at(301, Input::Tick);
    assert!(commands(&out).contains(&InjectCmd::Button {
        button: BUTTON,
        down: false
    }));
    assert!(f.done(pending, true).is_empty());

    // Release-all or overdue targeting retirement can coincide with an older failed up's retry.
    for time in [301, 791] {
        let mut f = Fixture::new();
        f.input(key(1, true));
        let out = f.input(button(2, true));
        f.done(targeting(&out), true);
        let out = f.input(key(3, false));
        f.at(
            251,
            Input::InjectDone {
                id: injections(&out)[0].0,
                ok: false,
            },
        );
        f.now = MonoTime::from_nanos(290_000_000);
        let out = f.input(scroll(4));
        let pending = targeting(&out);
        let out = f.at(time, Input::Tick);
        assert_eq!(
            commands(&out),
            vec![
                InjectCmd::Button {
                    button: BUTTON,
                    down: false
                },
                InjectCmd::Key {
                    usage: KEY,
                    down: false
                },
            ],
            "a fresh retirement up must not also be retried in this tick: {out:?}"
        );
        if time == 791 {
            assert!(out.iter().any(|output| matches!(output,
                Output::Notice(Notice::ProjectionEnded { key, reason: Reason::Failed }) if key.projection == ID
            )));
        }
        assert!(f.done(pending, true).is_empty());
        let released = injections(&out);
        assert!(f.done(released[0].0, false).is_empty());
        assert!(f.done(released[1].0, true).is_empty());
        assert_eq!(f.held(), vec![Held::Button(BUTTON)]);
        assert!(f.at(time + 49, Input::Tick).is_empty());
        let retry = f.at(time + 50, Input::Tick);
        assert_eq!(
            commands(&retry),
            vec![InjectCmd::Button {
                button: BUTTON,
                down: false
            }]
        );
        assert!(f.done(injections(&retry)[0].0, true).is_empty());
        assert!(f.held().is_empty());
        assert!(f.at(time + 100, Input::Tick).is_empty());
    }
}

#[test]
fn ordinary_source_end_absorbs_due_retries_without_repeating_fresh_releases() {
    for returning in [true, false] {
        let mut f = Fixture::new();
        f.input(key(1, true));
        let out = f.input(button(2, true));
        f.done(targeting(&out), true);
        let released = f.input(key(3, false));
        f.at(
            251,
            Input::InjectDone {
                id: injections(&released)[0].0,
                ok: false,
            },
        );
        let event = if returning {
            Input::Command(Command::Return(ProjectionKey {
                source: OWNER,
                projection: ID,
            }))
        } else {
            Input::Windows(WindowEvent::Removed(WindowId(10)))
        };
        let mut out = f.at(301, event);
        out.extend(f.handle(Input::Tick));
        assert_eq!(
            commands(&out),
            vec![
                InjectCmd::Button {
                    button: BUTTON,
                    down: false
                },
                InjectCmd::Key {
                    usage: KEY,
                    down: false
                },
            ]
        );
        let releases = injections(&out);
        assert!(f.done(releases[0].0, true).is_empty());
        assert!(f.done(releases[1].0, false).is_empty());
        assert_eq!(f.held(), vec![Held::Key(KEY)]);
        let out = f.at(351, Input::Tick);
        assert_eq!(
            commands(&out),
            vec![InjectCmd::Key {
                usage: KEY,
                down: false
            }]
        );
        assert!(f.done(injections(&out)[0].0, true).is_empty());
        assert!(f.held().is_empty());
        assert!(f.at(401, Input::Tick).is_empty());
    }
}

#[test]
fn competing_projection_motion_cannot_change_the_deferred_actions_target() {
    for pending in [button(1, true), scroll(1)] {
        let mut f = Fixture::new();
        let other = ProjectionId(2);
        f.project(WindowId(11), other, DISPLAY_A);
        let mut active = (DISPLAY_A, PointDevice::new(30.0, 20.0));
        let out = f.input(pending);
        let id = targeting(&out);
        let execute = |outputs: &[Output], active: &mut (DisplayId, PointDevice)| {
            let mut actions = Vec::new();
            for command in commands(outputs) {
                match command {
                    InjectCmd::MoveTo { display, position } => *active = (display, position),
                    InjectCmd::Button { .. } | InjectCmd::Scroll(_) => {
                        actions.push((command, *active))
                    }
                    _ => {}
                }
            }
            actions
        };
        assert!(execute(&out, &mut active).is_empty());
        let competing = f.input(motion(other, 1, 40.0));
        assert!(execute(&competing, &mut active).is_empty());
        let out = f.done(id, true);
        let actions = execute(&out, &mut active);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].1, (DISPLAY_B, PointDevice::new(10.0, 20.0)));
        assert_eq!(active, (DISPLAY_A, PointDevice::new(40.0, 20.0)));
    }
}

#[test]
fn targeting_ack_timeout_ends_owner_despite_heartbeats_and_resumes_other_projection() {
    for pending in [button(1, true), scroll(1)] {
        let mut f = Fixture::new();
        let other = ProjectionId(2);
        f.project(WindowId(11), other, DISPLAY_A);
        let out = f.input(pending);
        let id = targeting(&out);
        assert_eq!(
            f.e2.next_deadline(),
            Some(MonoTime::from_nanos(500_000_000))
        );
        assert!(f.input(key(2, true)).is_empty());
        assert!(f.input(motion(other, 1, 40.0)).is_empty());
        for (seq, time) in (3..).zip([100, 200, 300, 400, 499]) {
            f.now = MonoTime::from_nanos(time * 1_000_000);
            assert!(
                f.input(ProjInput::Held {
                    projection: ID,
                    seq,
                    keys: vec![],
                    buttons: vec![],
                })
                .is_empty()
            );
        }
        assert_eq!(
            f.e2.next_deadline(),
            Some(MonoTime::from_nanos(500_000_000))
        );
        let out = f.at(500, Input::Tick);
        assert!(out.iter().any(|output| matches!(output,
            Output::Notice(Notice::ProjectionEnded { key, reason: Reason::Failed }) if key.projection == ID
        )));
        assert_eq!(
            commands(&out),
            vec![InjectCmd::MoveTo {
                display: DISPLAY_A,
                position: PointDevice::new(40.0, 20.0),
            }]
        );
        assert!(f.done(id, true).is_empty());
        assert!(f.held().is_empty());
    }
}

#[test]
fn overdue_targeting_timeout_precedes_lease_expiry_on_tick_and_completion() {
    for tick in [true, false] {
        for pending in [button(1, true), scroll(1)] {
            let mut f = Fixture::new();
            let other = ProjectionId(2);
            f.project(WindowId(11), other, DISPLAY_A);
            let id = targeting(&f.input(pending));
            assert!(f.input(key(2, true)).is_empty());
            assert!(f.input(motion(other, 1, 40.0)).is_empty());
            for (seq, time) in [(3, 100), (4, 200)] {
                f.now = MonoTime::from_nanos(time * 1_000_000);
                assert!(
                    f.input(ProjInput::Held {
                        projection: ID,
                        seq,
                        keys: vec![],
                        buttons: vec![],
                    })
                    .is_empty()
                );
            }
            let event = if tick {
                Input::Tick
            } else {
                Input::InjectDone { id, ok: true }
            };
            let out = f.at(501, event);
            assert!(out.iter().any(|output| matches!(output,
                Output::Notice(Notice::ProjectionEnded { key, reason: Reason::Failed }) if key.projection == ID
            )), "overdue targeting must fail its owner: {out:?}");
            assert_eq!(
                commands(&out),
                vec![InjectCmd::MoveTo {
                    display: DISPLAY_A,
                    position: PointDevice::new(40.0, 20.0),
                }]
            );
            assert!(f.at(502, Input::InjectDone { id, ok: true }).is_empty());
            assert!(f.input(button(5, true)).is_empty());
            assert!(f.held().is_empty());
        }
    }
}

#[test]
fn empty_lease_expiry_discards_press_and_fifo_before_late_completion() {
    let mut f = Fixture::new();
    let other = ProjectionId(2);
    f.project(WindowId(11), other, DISPLAY_A);
    f.input(ProjInput::Held {
        projection: ID,
        seq: 1,
        keys: vec![],
        buttons: vec![],
    });
    f.now = MonoTime::from_nanos(290_000_000);
    let out = f.input(button(2, true));
    let id = targeting(&out);
    assert!(f.input(key(3, true)).is_empty());
    assert!(f.input(motion(ID, 4, 30.0)).is_empty());
    assert!(f.input(motion(other, 1, 40.0)).is_empty());
    assert_eq!(
        f.e2.next_deadline(),
        Some(MonoTime::from_nanos(300_000_001))
    );
    let out = f.at(301, Input::Tick);
    assert_eq!(
        commands(&out),
        vec![InjectCmd::MoveTo {
            display: DISPLAY_A,
            position: PointDevice::new(40.0, 20.0),
        }]
    );
    assert!(
        !out.iter()
            .any(|output| matches!(output, Output::Notice(Notice::ProjectionEnded { .. })))
    );
    let out = f.at(302, Input::InjectDone { id, ok: true });
    assert!(out.is_empty());
    assert!(f.held().is_empty());
    assert!(f.input(button(5, false)).is_empty());
}

#[test]
fn suspension_discards_targeting_and_fifo_before_first_completion_after_full_resume() {
    for pending in [button(1, true), scroll(1)] {
        let mut f = Fixture::new();
        let other = ProjectionId(2);
        f.project(WindowId(11), other, DISPLAY_A);
        let out = f.input(pending);
        let id = targeting(&out);
        assert!(f.input(key(2, true)).is_empty());
        assert!(f.input(button(3, false)).is_empty());
        assert!(f.input(motion(other, 1, 40.0)).is_empty());
        let out = f.handle(Input::Link(LinkEvent::Closed {
            peer: PEER,
            error: LinkError::Closed,
        }));
        assert!(commands(&out).is_empty());
        f.handle(Input::PeerUp { peer: PEER });
        f.handle(Input::Link(LinkEvent::Control {
            peer: PEER,
            msg: ControlMessage::Projection(ProjectionMessage::Accepted {
                projection: ID,
                size: PixelSize::new(640, 480),
                scale: 1.0,
            }),
        }));
        f.handle(Input::CaptureStarted {
            projection: ID,
            result: Ok(StreamId(3)),
        });
        assert!(f.done(id, true).is_empty());
        assert!(f.held().is_empty());
        assert!(f.input(button(4, false)).is_empty());
        // Fresh input proves the source is Live again and not merely rejecting every result.
        let out = f.input(button(5, true));
        let new = targeting(&out);
        assert_ne!(id, new);
        assert_eq!(
            commands(&f.done(new, true)),
            vec![InjectCmd::Button {
                button: BUTTON,
                down: true
            }]
        );
    }
}

#[test]
fn returning_waiting_projection_keeps_other_projection_entries_in_arrival_order() {
    let mut f = Fixture::new();
    let other = ProjectionId(2);
    f.project(WindowId(11), other, DISPLAY_A);
    let out = f.input(button(1, true));
    let id = targeting(&out);
    assert!(f.input(motion(other, 1, 40.0)).is_empty());
    assert!(f.input(key(2, true)).is_empty());
    assert!(f.input(motion(other, 2, 50.0)).is_empty());
    let out = f.handle(Input::Command(Command::Return(ProjectionKey {
        source: OWNER,
        projection: ID,
    })));
    assert_eq!(
        commands(&out),
        vec![
            InjectCmd::MoveTo {
                display: DISPLAY_A,
                position: PointDevice::new(40.0, 20.0)
            },
            InjectCmd::MoveTo {
                display: DISPLAY_A,
                position: PointDevice::new(50.0, 20.0)
            },
        ]
    );
    assert!(f.done(id, true).is_empty());
}

#[test]
fn teardown_discards_expired_queued_keys_before_return_or_suspension_drain() {
    for returning in [true, false] {
        for pending in [button(1, true), scroll(1)] {
            let mut f = Fixture::new();
            let other_peer = NodeId([3; 32]);
            let expired = ProjectionId(2);
            let survivor = ProjectionId(3);
            f.handle(Input::PeerUp { peer: other_peer });
            f.handle(Input::Grants(
                [
                    (PEER, [Capability::WindowShare].into()),
                    (other_peer, [Capability::WindowShare].into()),
                ]
                .into(),
            ));
            f.project_to(WindowId(11), expired, DISPLAY_A, other_peer);
            f.project_to(WindowId(12), survivor, DISPLAY_A, other_peer);
            f.handle(Input::Windows(WindowEvent::Focused(Some(WindowId(11)))));
            assert!(
                f.input_from(
                    other_peer,
                    ProjInput::Held {
                        projection: expired,
                        seq: 1,
                        keys: vec![],
                        buttons: vec![],
                    },
                )
                .is_empty()
            );
            f.now = MonoTime::from_nanos(290_000_000);
            let id = targeting(&f.input(pending));
            assert!(
                f.input_from(other_peer, motion(survivor, 1, 40.0))
                    .is_empty()
            );
            assert!(
                f.input_from(
                    other_peer,
                    ProjInput::Key {
                        projection: expired,
                        seq: 2,
                        usage: KEY,
                        down: true,
                    },
                )
                .is_empty()
            );
            assert!(f.input(key(2, true)).is_empty());
            assert!(
                f.input_from(other_peer, motion(survivor, 2, 50.0))
                    .is_empty()
            );
            let event = if returning {
                Input::Command(Command::Return(ProjectionKey {
                    source: OWNER,
                    projection: ID,
                }))
            } else {
                Input::Link(LinkEvent::Closed {
                    peer: PEER,
                    error: LinkError::Closed,
                })
            };
            let out = f.at(301, event);
            assert_eq!(
                commands(&out),
                vec![
                    InjectCmd::MoveTo {
                        display: DISPLAY_A,
                        position: PointDevice::new(40.0, 20.0),
                    },
                    InjectCmd::MoveTo {
                        display: DISPLAY_A,
                        position: PointDevice::new(50.0, 20.0),
                    },
                ],
                "only valid survivor input may drain: {out:?}"
            );
            assert!(f.held().is_empty());
            assert!(f.at(302, Input::InjectDone { id, ok: true }).is_empty());
            assert!(
                f.input_from(
                    other_peer,
                    ProjInput::Key {
                        projection: expired,
                        seq: 3,
                        usage: KEY,
                        down: false,
                    },
                )
                .is_empty()
            );
            assert!(f.held().is_empty());
        }
    }
}
