#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use crosspane_engine::e1::target::TargetE1;
use crosspane_engine::io::TARGET_INDICATOR;
use crosspane_engine::{Command, EngineConfig, InjectCmd, InjectId, Input, Notice, Output};
use crosspane_input::Held;
use crosspane_input::journal::{Journal, JournalError, MemoryJournal};
use crosspane_platform::{
    CaptureEvent, LockState, Overlay, OverlayAnchor, Rgb8, SessionEvent, SessionState,
};
use crosspane_protocol::link::{LinkError, LinkEvent};
use crosspane_protocol::msg::{
    Capability, ControlMessage, EndReason, InputMessage, PointerMessage, Refusal, TargetStatus,
};
use crosspane_types::geom::PointDevice;
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::id::{DisplayId, NodeId, SessionId};
use crosspane_types::input::{LockKeys, ScrollDelta, ScrollPhase};
use crosspane_types::time::MonoTime;
use proptest::prelude::*;

const PEER: NodeId = NodeId([1; 32]);
const OTHER: NodeId = NodeId([2; 32]);
const SESSION: SessionId = SessionId(10);
const DISPLAY: DisplayId = DisplayId(3);
const KEY: HidUsage = HidUsage::keyboard(4);
const KEY2: HidUsage = HidUsage::keyboard(5);
const BUTTON: MouseButton = MouseButton(1);
const UNLOCKED: SessionState = SessionState {
    lock: LockState::Unlocked,
    active: Some(true),
};

fn ms(time: u64) -> MonoTime {
    MonoTime::from_nanos(time * 1_000_000)
}

fn after_ms(time: u64) -> MonoTime {
    MonoTime::from_nanos(ms(time).as_nanos() + 1)
}

fn control(peer: NodeId, msg: ControlMessage) -> Input {
    Input::Link(LinkEvent::Control { peer, msg })
}

fn start(peer: NodeId, session: SessionId) -> Input {
    control(
        peer,
        ControlMessage::StartControl {
            session,
            entry_display: DISPLAY,
            entry: PointDevice::new(50.0, 60.0),
            lock_keys: locks(),
        },
    )
}

fn end(peer: NodeId, session: SessionId) -> Input {
    control(
        peer,
        ControlMessage::EndControl {
            session,
            reason: EndReason::Released,
        },
    )
}

fn message(msg: InputMessage) -> Input {
    Input::Link(LinkEvent::Input { peer: PEER, msg })
}

fn key(seq: u32, usage: HidUsage, down: bool) -> Input {
    message(InputMessage::Key {
        session: SESSION,
        seq,
        usage,
        down,
    })
}

fn button(seq: u32, down: bool) -> Input {
    message(InputMessage::Button {
        session: SESSION,
        seq,
        button: BUTTON,
        down,
    })
}

fn heartbeat(seq: u32, held_keys: Vec<HidUsage>, held_buttons: Vec<MouseButton>) -> Input {
    message(InputMessage::State {
        session: SESSION,
        seq,
        held_keys,
        held_buttons,
    })
}

fn motion(seq: u32) -> Input {
    Input::Link(LinkEvent::Motion {
        peer: PEER,
        msg: PointerMessage {
            session: SESSION,
            seq,
            display: DISPLAY,
            position: PointDevice::new(f64::from(seq), 7.0),
        },
    })
}

fn activity() -> Input {
    Input::Capture(CaptureEvent::LocalActivity { at: MonoTime::ZERO })
}

fn locks() -> LockKeys {
    LockKeys {
        caps_lock: Some(true),
        num_lock: Some(false),
        scroll_lock: None,
    }
}

fn scroll() -> ScrollDelta {
    ScrollDelta {
        v120_x: 0,
        v120_y: 120,
        pixels: None,
        phase: ScrollPhase::Discrete,
        stop_x: false,
        stop_y: false,
    }
}

fn ack(seq: u32) -> Output {
    Output::SendInput {
        peer: PEER,
        msg: InputMessage::Ack {
            session: SESSION,
            seq,
        },
    }
}

fn status(status: TargetStatus) -> Output {
    Output::SendInput {
        peer: PEER,
        msg: InputMessage::Status {
            session: SESSION,
            status,
        },
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

#[derive(Debug, Default)]
struct JournalState {
    memory: MemoryJournal,
    fail_down: bool,
    fail_up: bool,
    fail_read: bool,
}

#[derive(Clone, Debug, Default)]
struct SharedJournal(Arc<Mutex<JournalState>>);

impl Journal for SharedJournal {
    fn record_down(&mut self, item: Held) -> Result<(), JournalError> {
        let mut state = self.0.lock().unwrap();
        if state.fail_down {
            return Err(std::io::Error::other("test down failure").into());
        }
        state.memory.record_down(item)
    }

    fn record_up(&mut self, item: Held) -> Result<(), JournalError> {
        let mut state = self.0.lock().unwrap();
        if state.fail_up {
            return Err(std::io::Error::other("test up failure").into());
        }
        state.memory.record_up(item)
    }

    fn held(&self) -> Result<Vec<Held>, JournalError> {
        let state = self.0.lock().unwrap();
        if state.fail_read {
            return Err(std::io::Error::other("test read failure").into());
        }
        state.memory.held()
    }
}

struct Fixture {
    target: TargetE1,
    journal: SharedJournal,
}

impl Fixture {
    fn new() -> Self {
        let journal = SharedJournal::default();
        let config = EngineConfig::new(NodeId([0; 32]));
        let (target, out) =
            TargetE1::new(&config, Box::new(journal.clone()), MonoTime::ZERO).unwrap();
        assert!(out.is_empty());
        Self { target, journal }
    }

    fn handle(&mut self, input: Input, now: u64) -> Vec<Output> {
        self.handle_at(input, ms(now))
    }

    fn handle_at(&mut self, input: Input, now: MonoTime) -> Vec<Output> {
        let mut out = Vec::new();
        self.target.handle(&input, now, &mut out);
        out
    }

    fn grant(&mut self) {
        assert!(
            self.handle(
                Input::Grants(
                    [
                        (PEER, BTreeSet::from([Capability::InputAccept])),
                        (OTHER, BTreeSet::from([Capability::InputAccept])),
                    ]
                    .into(),
                ),
                0,
            )
            .is_empty()
        );
    }

    fn ready(&mut self) {
        self.grant();
        assert!(
            self.handle(Input::Session(SessionEvent::State(UNLOCKED)), 0)
                .is_empty()
        );
    }

    fn active() -> Self {
        let mut fixture = Self::new();
        fixture.ready();
        let out = fixture.handle(start(PEER, SESSION), 0);
        fixture.confirm(&out, true, 0);
        fixture
    }

    fn confirm(&mut self, out: &[Output], ok: bool, now: u64) {
        for (id, _) in injections(out) {
            assert!(self.handle(Input::InjectDone { id, ok }, now).is_empty());
        }
    }

    fn held(&self) -> Vec<Held> {
        self.journal.held().unwrap()
    }
}

fn refused(peer: NodeId, session: SessionId, reason: Refusal) -> Vec<Output> {
    vec![
        Output::SendControl {
            peer,
            msg: ControlMessage::ControlRefused { session, reason },
        },
        Output::Notice(Notice::Refused { peer, reason }),
    ]
}

fn ended(reason: Option<EndReason>, notice: Notice) -> Vec<Output> {
    let mut out = vec![
        Output::HideOverlay(TARGET_INDICATOR),
        Output::MonitorLocalActivity(false),
    ];
    if let Some(reason) = reason {
        out.push(Output::SendControl {
            peer: PEER,
            msg: ControlMessage::EndControl {
                session: SESSION,
                reason,
            },
        });
    }
    out.push(Output::Notice(notice));
    out
}

#[test]
fn accept_outputs_are_ordered_and_ids_start_at_one() {
    let mut f = Fixture::new();
    f.ready();
    assert_eq!(
        f.handle(start(PEER, SESSION), 0),
        vec![
            Output::Inject {
                id: InjectId(1),
                cmd: InjectCmd::MoveTo {
                    display: DISPLAY,
                    position: PointDevice::new(50.0, 60.0),
                },
            },
            Output::Inject {
                id: InjectId(2),
                cmd: InjectCmd::LockKeys(locks()),
            },
            Output::ShowOverlay {
                id: TARGET_INDICATOR,
                overlay: Overlay {
                    display: DISPLAY,
                    anchor: OverlayAnchor::TopCenter,
                    text: format!("Controlled from {}", PEER.short()),
                    accent: Rgb8 {
                        r: 0x3b,
                        g: 0x82,
                        b: 0xf6
                    },
                },
            },
            Output::MonitorLocalActivity(true),
            Output::SendControl {
                peer: PEER,
                msg: ControlMessage::ControlStarted { session: SESSION },
            },
            Output::Notice(Notice::ControlledBy(PEER)),
        ]
    );
    assert_eq!(f.target.next_deadline(), None);
    assert!(!format!("{:?}", f.target).is_empty());
}

#[test]
fn permission_refusal_precedes_lock_and_busy() {
    let mut f = Fixture::new();
    assert_eq!(
        f.handle(start(PEER, SESSION), 0),
        refused(PEER, SESSION, Refusal::Permission)
    );
    f.ready();
    f.handle(start(PEER, SESSION), 0);
    f.handle(
        Input::Grants([(PEER, BTreeSet::from([Capability::InputAccept]))].into()),
        0,
    );
    assert_eq!(
        f.handle(start(OTHER, SESSION), 0),
        refused(OTHER, SESSION, Refusal::Permission)
    );
}

#[test]
fn locked_unknown_and_inactive_sessions_refuse_control() {
    let mut f = Fixture::new();
    f.grant();
    assert_eq!(
        f.handle(start(PEER, SESSION), 0),
        refused(PEER, SESSION, Refusal::Locked)
    );
    for state in [
        SessionState {
            lock: LockState::Locked,
            ..UNLOCKED
        },
        SessionState {
            lock: LockState::Unknown,
            ..UNLOCKED
        },
        SessionState {
            active: None,
            ..UNLOCKED
        },
        SessionState {
            active: Some(false),
            ..UNLOCKED
        },
    ] {
        f.handle(Input::Session(SessionEvent::State(state)), 0);
        assert_eq!(
            f.handle(start(PEER, SESSION), 0),
            refused(PEER, SESSION, Refusal::Locked)
        );
        assert!(f.handle(key(1, KEY, true), 0).is_empty());
    }
}

#[test]
fn another_controller_is_busy() {
    let mut f = Fixture::active();
    assert_eq!(
        f.handle(start(OTHER, SESSION), 0),
        refused(OTHER, SESSION, Refusal::Busy)
    );
    assert_eq!(
        commands(&f.handle(key(1, KEY, true), 0)),
        vec![InjectCmd::Key {
            usage: KEY,
            down: true
        }]
    );
}

#[test]
fn same_controller_restart_releases_old_session_before_acceptance() {
    let mut f = Fixture::active();
    f.handle(key(1, KEY, true), 0);
    let out = f.handle(start(PEER, SessionId(11)), 1);
    assert_eq!(commands(&out)[0], InjectCmd::ReleaseAll);
    assert_eq!(
        &out[1..5],
        ended(Some(EndReason::Released), Notice::ControlEnded(PEER))
    );
    assert_eq!(
        commands(&out)[1..],
        [
            InjectCmd::MoveTo {
                display: DISPLAY,
                position: PointDevice::new(50.0, 60.0)
            },
            InjectCmd::LockKeys(locks())
        ]
    );
    assert_eq!(
        out.last(),
        Some(&Output::Notice(Notice::ControlledBy(PEER)))
    );
    assert!(f.handle(key(2, KEY, false), 1).is_empty());
    f.confirm(&out, true, 1);
    assert!(f.held().is_empty());
}

#[test]
fn key_button_scroll_lock_keys_and_every_ack() {
    let mut f = Fixture::active();
    for (input, seq, cmd) in [
        (
            key(1, KEY, true),
            1,
            InjectCmd::Key {
                usage: KEY,
                down: true,
            },
        ),
        (
            button(2, true),
            2,
            InjectCmd::Button {
                button: BUTTON,
                down: true,
            },
        ),
        (
            message(InputMessage::Scroll {
                session: SESSION,
                seq: 3,
                delta: scroll(),
            }),
            3,
            InjectCmd::Scroll(scroll()),
        ),
        (
            message(InputMessage::LockKeys {
                session: SESSION,
                seq: 4,
                keys: locks(),
            }),
            4,
            InjectCmd::LockKeys(locks()),
        ),
        (
            key(5, KEY, false),
            5,
            InjectCmd::Key {
                usage: KEY,
                down: false,
            },
        ),
        (
            button(6, false),
            6,
            InjectCmd::Button {
                button: BUTTON,
                down: false,
            },
        ),
    ] {
        let out = f.handle(input, 0);
        assert_eq!(commands(&out), vec![cmd]);
        assert_eq!(out.last(), Some(&ack(seq)));
        f.confirm(&out, true, 0);
    }
    assert!(f.held().is_empty());
}

#[test]
fn duplicate_down_and_unheld_up_only_ack() {
    let mut f = Fixture::active();
    f.handle(key(1, KEY, true), 0);
    assert_eq!(f.held(), vec![Held::Key(KEY)]);
    assert_eq!(f.handle(key(2, KEY, true), 0), vec![ack(2)]);
    assert_eq!(f.handle(key(3, KEY2, false), 0), vec![ack(3)]);
    f.handle(button(4, true), 0);
    assert_eq!(f.handle(button(5, true), 0), vec![ack(5)]);
    let out = f.handle(button(6, false), 0);
    f.confirm(&out, true, 0);
    assert_eq!(f.handle(button(7, false), 0), vec![ack(7)]);
}

#[test]
fn unrelated_peers_sessions_ack_status_and_controller_inputs_are_ignored() {
    let mut f = Fixture::active();
    for input in [
        Input::Link(LinkEvent::Input {
            peer: OTHER,
            msg: InputMessage::Key {
                session: SESSION,
                seq: 1,
                usage: KEY,
                down: true,
            },
        }),
        message(InputMessage::Key {
            session: SessionId(99),
            seq: 1,
            usage: KEY,
            down: true,
        }),
        message(InputMessage::Ack {
            session: SESSION,
            seq: 1,
        }),
        message(InputMessage::Status {
            session: SESSION,
            status: TargetStatus::Resumed,
        }),
        end(OTHER, SESSION),
        end(PEER, SessionId(99)),
        Input::Capture(CaptureEvent::Key {
            usage: KEY,
            down: true,
            at: MonoTime::ZERO,
        }),
        Input::PeerUp { peer: PEER },
        Input::Command(Command::Rearm),
        Input::InjectDone {
            id: InjectId(9999),
            ok: true,
        },
    ] {
        assert!(f.handle(input, 0).is_empty());
    }
    assert!(f.held().is_empty());
}

#[test]
fn heartbeat_releases_missing_items_sorted_and_confirms_only_after_success() {
    let mut f = Fixture::active();
    f.handle(button(1, true), 0);
    f.handle(key(2, KEY2, true), 0);
    f.handle(key(3, KEY, true), 0);
    let keep = f.handle(heartbeat(4, vec![KEY], vec![]), 50);
    assert_eq!(
        commands(&keep),
        vec![
            InjectCmd::Key {
                usage: KEY2,
                down: false
            },
            InjectCmd::Button {
                button: BUTTON,
                down: false
            }
        ]
    );
    assert_eq!(keep.last(), Some(&ack(4)));
    assert_eq!(
        f.held(),
        vec![Held::Key(KEY), Held::Key(KEY2), Held::Button(BUTTON)]
    );
    f.confirm(&keep, true, 50);
    assert_eq!(f.held(), vec![Held::Key(KEY)]);
    assert_eq!(f.target.next_deadline(), Some(after_ms(350)));
    let empty = f.handle(heartbeat(5, vec![], vec![]), 60);
    f.confirm(&empty, true, 60);
    assert!(f.held().is_empty());
    assert_eq!(f.target.next_deadline(), None);
    assert_eq!(
        f.handle(heartbeat(6, vec![KEY2], vec![BUTTON]), 70),
        vec![ack(6)]
    );
    assert!(f.held().is_empty());
}

#[test]
fn lease_expires_after_300_ms_without_heartbeat() {
    let mut f = Fixture::active();
    f.handle(key(1, KEY, true), 10);
    f.handle(button(2, true), 20);
    assert_eq!(f.target.next_deadline(), Some(after_ms(310)));
    assert!(f.handle(Input::Tick, 310).is_empty());
    let out = f.handle(Input::Tick, 311);
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
            }
        ]
    );
    assert_eq!(f.target.next_deadline(), None);
    f.confirm(&out, true, 311);
    assert!(f.held().is_empty());
    assert!(f.handle(Input::Tick, 312).is_empty());
}

#[test]
fn motion_is_monotonic_matches_session_and_does_not_ack() {
    let mut f = Fixture::active();
    assert!(f.handle(motion(0), 0).is_empty());
    assert_eq!(
        commands(&f.handle(motion(4), 0)),
        vec![InjectCmd::MoveTo {
            display: DISPLAY,
            position: PointDevice::new(4.0, 7.0)
        }]
    );
    assert!(f.handle(motion(4), 0).is_empty());
    assert!(f.handle(motion(3), 0).is_empty());
    for (peer, session) in [(OTHER, SESSION), (PEER, SessionId(99))] {
        assert!(
            f.handle(
                Input::Link(LinkEvent::Motion {
                    peer,
                    msg: PointerMessage {
                        session,
                        seq: 100,
                        display: DISPLAY,
                        position: PointDevice::new(0.0, 0.0)
                    }
                }),
                0
            )
            .is_empty()
        );
    }
    assert_eq!(f.handle(motion(5), 0).len(), 1);
}

#[test]
fn controller_end_releases_without_reply_and_clears_session() {
    let mut f = Fixture::active();
    f.handle(key(1, KEY, true), 0);
    let out = f.handle(end(PEER, SESSION), 1);
    assert_eq!(commands(&out), vec![InjectCmd::ReleaseAll]);
    assert_eq!(&out[1..], ended(None, Notice::ControlEnded(PEER)));
    f.confirm(&out, true, 1);
    assert!(f.held().is_empty());
    assert!(f.handle(end(PEER, SESSION), 2).is_empty());
    assert!(f.handle(key(2, KEY, true), 2).is_empty());
}

#[test]
fn link_closed_ends_only_its_controller_without_reply() {
    let mut f = Fixture::active();
    assert!(
        f.handle(
            Input::Link(LinkEvent::Closed {
                peer: OTHER,
                error: LinkError::Closed
            }),
            0
        )
        .is_empty()
    );
    f.handle(button(1, true), 0);
    let out = f.handle(
        Input::Link(LinkEvent::Closed {
            peer: PEER,
            error: LinkError::Closed,
        }),
        1,
    );
    assert_eq!(commands(&out), vec![InjectCmd::ReleaseAll]);
    assert_eq!(&out[1..], ended(None, Notice::ControlEnded(PEER)));
    f.confirm(&out, true, 1);
    assert!(f.held().is_empty());
}

#[test]
fn locking_unknown_or_inactive_ends_with_specific_notice_and_releases() {
    for state in [
        SessionState {
            lock: LockState::Locked,
            ..UNLOCKED
        },
        SessionState {
            lock: LockState::Unknown,
            ..UNLOCKED
        },
        SessionState {
            active: None,
            ..UNLOCKED
        },
        SessionState {
            active: Some(false),
            ..UNLOCKED
        },
    ] {
        let mut f = Fixture::active();
        f.handle(key(1, KEY, true), 0);
        let out = f.handle(Input::Session(SessionEvent::State(state)), 1);
        assert_eq!(commands(&out), vec![InjectCmd::ReleaseAll]);
        assert_eq!(
            &out[1..],
            ended(Some(EndReason::TargetLocked), Notice::TargetLocked(PEER))
        );
        f.confirm(&out, true, 1);
        assert!(f.held().is_empty());
        assert!(f.handle(key(2, KEY, true), 2).is_empty());
    }
}

#[test]
fn sleep_and_wake_require_fresh_state_even_if_state_arrived_while_asleep() {
    let mut f = Fixture::active();
    f.handle(button(1, true), 0);
    let out = f.handle(Input::Session(SessionEvent::WillSleep), 1);
    assert_eq!(commands(&out), vec![InjectCmd::ReleaseAll]);
    assert_eq!(
        &out[1..],
        ended(Some(EndReason::TargetLocked), Notice::TargetLocked(PEER))
    );
    f.confirm(&out, true, 1);
    f.handle(Input::Session(SessionEvent::State(UNLOCKED)), 2);
    assert_eq!(
        f.handle(start(PEER, SESSION), 2),
        refused(PEER, SESSION, Refusal::Locked)
    );
    f.handle(Input::Session(SessionEvent::Woke), 3);
    assert_eq!(
        f.handle(start(PEER, SESSION), 3),
        refused(PEER, SESSION, Refusal::Locked)
    );
    f.handle(Input::Session(SessionEvent::State(UNLOCKED)), 4);
    assert_eq!(
        f.handle(start(PEER, SESSION), 4).last(),
        Some(&Output::Notice(Notice::ControlledBy(PEER)))
    );
}

#[test]
fn revocation_ends_session_but_unrelated_grants_do_not() {
    let mut f = Fixture::active();
    assert!(
        f.handle(
            Input::Grants([(PEER, BTreeSet::from([Capability::InputAccept]))].into()),
            0
        )
        .is_empty()
    );
    f.handle(key(1, KEY, true), 0);
    let out = f.handle(
        Input::Grants([(PEER, BTreeSet::from([Capability::WindowShare]))].into()),
        1,
    );
    assert_eq!(commands(&out), vec![InjectCmd::ReleaseAll]);
    assert_eq!(
        &out[1..],
        ended(Some(EndReason::Revoked), Notice::ControlEnded(PEER))
    );
    f.confirm(&out, true, 1);
    assert!(f.held().is_empty());
    assert_eq!(
        f.handle(start(PEER, SESSION), 2),
        refused(PEER, SESSION, Refusal::Permission)
    );
}

#[test]
fn local_override_releases_ends_and_never_resumes() {
    let mut f = Fixture::active();
    f.handle(key(1, KEY, true), 0);
    f.handle(button(2, true), 0);
    let out = f.handle(activity(), 10);
    assert_eq!(commands(&out), vec![InjectCmd::ReleaseAll]);
    let mut expected = vec![status(TargetStatus::LocalOverride)];
    expected.extend(ended(None, Notice::ControlEnded(PEER)));
    assert_eq!(&out[1..], expected);
    assert!(!f.target.is_controlled());
    assert_eq!(f.target.controller(), None);
    assert_eq!(f.target.next_deadline(), None);
    // Journal records stay until the release is confirmed, exactly as on controller release.
    assert_eq!(f.held(), vec![Held::Key(KEY), Held::Button(BUTTON)]);
    f.confirm(&out, true, 10);
    for input in [
        key(3, KEY2, true),
        button(4, true),
        key(5, KEY, false),
        button(6, false),
        message(InputMessage::Scroll {
            session: SESSION,
            seq: 7,
            delta: scroll(),
        }),
        message(InputMessage::LockKeys {
            session: SESSION,
            seq: 8,
            keys: locks(),
        }),
        heartbeat(9, vec![KEY2], vec![BUTTON]),
        motion(100),
        end(PEER, SESSION),
    ] {
        assert!(f.handle(input, 20).is_empty());
    }
    assert!(f.held().is_empty());
    assert!(f.handle(activity(), 500).is_empty());
    assert!(f.handle(Input::Tick, 1010).is_empty());
    assert!(f.handle(Input::Tick, 1500).is_empty());
    assert_eq!(f.target.next_deadline(), None);
    assert!(f.handle(key(10, KEY2, true), 1500).is_empty());
    assert!(f.handle(motion(101), 1500).is_empty());
}

#[test]
fn local_override_without_held_items_still_releases_and_ends_once() {
    let mut f = Fixture::active();
    let out = f.handle(activity(), 0);
    assert_eq!(commands(&out), vec![InjectCmd::ReleaseAll]);
    let mut expected = vec![status(TargetStatus::LocalOverride)];
    expected.extend(ended(None, Notice::ControlEnded(PEER)));
    assert_eq!(&out[1..], expected);
    assert!(!f.target.is_controlled());
    assert!(f.handle(activity(), 1).is_empty());
}

#[test]
fn local_activity_without_a_session_does_nothing() {
    let mut f = Fixture::new();
    assert!(f.handle(activity(), 0).is_empty());
    f.ready();
    assert!(f.handle(activity(), 1).is_empty());
    assert!(!f.target.is_controlled());
    assert!(f.held().is_empty());
    assert_eq!(f.target.next_deadline(), None);
}

#[test]
fn panic_releases_and_ends_with_panic_reason() {
    let mut f = Fixture::active();
    f.handle(key(1, KEY, true), 0);
    let out = f.handle(Input::Command(Command::Panic), 1);
    assert_eq!(commands(&out), vec![InjectCmd::ReleaseAll]);
    assert_eq!(
        &out[1..],
        ended(Some(EndReason::Panic), Notice::ControlEnded(PEER))
    );
    f.confirm(&out, true, 1);
    assert!(f.held().is_empty());
    assert!(f.handle(Input::Command(Command::Panic), 2).is_empty());
    assert!(f.handle(activity(), 2).is_empty());
}

fn recovering() -> (Fixture, Vec<Output>) {
    let mut journal = SharedJournal::default();
    journal.record_down(Held::Button(BUTTON)).unwrap();
    journal.record_down(Held::Key(KEY2)).unwrap();
    journal.record_down(Held::Key(KEY)).unwrap();
    let (target, out) = TargetE1::new(
        &EngineConfig::new(NodeId([0; 32])),
        Box::new(journal.clone()),
        ms(7),
    )
    .unwrap();
    (Fixture { target, journal }, out)
}

#[test]
fn crash_recovery_precedes_permission_and_clears_journal_only_on_success() {
    let (mut f, out) = recovering();
    assert_eq!(
        injections(&out),
        vec![(
            InjectId(1),
            InjectCmd::Recover {
                keys: vec![KEY, KEY2],
                buttons: vec![BUTTON]
            }
        )]
    );
    assert_eq!(
        f.held(),
        vec![Held::Key(KEY), Held::Key(KEY2), Held::Button(BUTTON)]
    );
    assert_eq!(f.target.next_deadline(), Some(ms(57)));
    assert!(f.handle(Input::Tick, 57).is_empty());
    f.confirm(&out, true, 57);
    assert!(f.held().is_empty());
    assert_eq!(f.target.next_deadline(), None);
    assert!(f.handle(Input::Tick, 100).is_empty());
}

#[test]
fn failed_recovery_retries_same_items_every_tick_until_success() {
    let (mut f, out) = recovering();
    f.confirm(&out, false, 8);
    assert_eq!(f.target.next_deadline(), Some(ms(58)));
    let retry = f.handle(Input::Tick, 58);
    assert_eq!(commands(&retry), commands(&out));
    assert_eq!(injections(&retry)[0].0, InjectId(2));
    let later = f.handle(Input::Tick, 108);
    assert_eq!(commands(&later), commands(&out));
    f.confirm(&retry, true, 108);
    assert!(f.held().is_empty());
    f.confirm(&later, false, 109);
    assert_eq!(f.target.next_deadline(), None);
    assert!(f.handle(Input::Tick, 159).is_empty());
}

#[test]
fn recovery_journal_failure_retains_items_for_retry_and_open_failure_propagates() {
    let (mut f, out) = recovering();
    f.journal.0.lock().unwrap().fail_up = true;
    f.confirm(&out, true, 8);
    assert_eq!(f.held().len(), 3);
    let retry = f.handle(Input::Tick, 58);
    f.journal.0.lock().unwrap().fail_up = false;
    f.confirm(&retry, true, 58);
    assert!(f.held().is_empty());
    f.journal.0.lock().unwrap().fail_read = true;
    assert!(TargetE1::new(&EngineConfig::new(PEER), Box::new(f.journal), ms(59)).is_err());
}

#[test]
fn failed_key_button_and_release_all_retry_once_per_tick_even_when_locked() {
    let mut f = Fixture::active();
    f.handle(key(1, KEY, true), 0);
    f.handle(button(2, true), 0);
    let key_up = f.handle(key(3, KEY, false), 1);
    let button_up = f.handle(button(4, false), 1);
    f.confirm(&key_up, false, 1);
    f.confirm(&button_up, false, 1);
    assert_eq!(f.target.next_deadline(), Some(ms(51)));
    let ending = f.handle(
        Input::Session(SessionEvent::State(SessionState {
            lock: LockState::Locked,
            ..UNLOCKED
        })),
        2,
    );
    assert!(commands(&ending).is_empty());
    let retry = f.handle(Input::Tick, 52);
    assert_eq!(commands(&retry), vec![InjectCmd::ReleaseAll]);
    assert_eq!(f.held(), vec![Held::Key(KEY), Held::Button(BUTTON)]);
    f.confirm(&retry, false, 52);
    let later = f.handle(Input::Tick, 102);
    assert_eq!(commands(&later), vec![InjectCmd::ReleaseAll]);
    f.confirm(&later, true, 102);
    assert!(f.held().is_empty());
    assert_eq!(f.target.next_deadline(), None);
    assert!(f.handle(Input::Tick, 152).is_empty());
}

#[test]
fn retry_release_all_accounts_for_newly_held_items_too() {
    let mut f = Fixture::active();
    f.handle(key(1, KEY, true), 0);
    let up = f.handle(key(2, KEY, false), 1);
    f.confirm(&up, false, 1);
    f.handle(button(3, true), 2);
    let retry = f.handle(Input::Tick, 51);
    assert_eq!(commands(&retry), vec![InjectCmd::ReleaseAll]);
    f.confirm(&retry, true, 51);
    assert!(f.held().is_empty());
    assert_eq!(f.target.next_deadline(), None);
    assert_eq!(f.handle(button(4, false), 52), vec![ack(4)]);
}

#[test]
fn delayed_release_confirmation_cannot_clear_a_later_press_or_release() {
    let mut f = Fixture::active();
    f.handle(key(1, KEY, true), 0);
    let old_up = f.handle(key(2, KEY, false), 1);
    f.handle(key(3, KEY, true), 2);
    let new_up = f.handle(key(4, KEY, false), 3);
    f.confirm(&old_up, true, 4);
    assert_eq!(f.held(), vec![Held::Key(KEY)]);
    f.confirm(&old_up, false, 4);
    f.confirm(&new_up, false, 4);
    let retry = f.handle(Input::Tick, 54);
    f.confirm(&retry, true, 54);
    assert!(f.held().is_empty());
}

#[test]
fn failed_press_waits_for_heartbeat_or_lease_and_keeps_journal_record() {
    let mut f = Fixture::active();
    let press = f.handle(key(1, KEY, true), 0);
    f.confirm(&press, false, 0);
    assert_eq!(f.held(), vec![Held::Key(KEY)]);
    assert_eq!(f.target.next_deadline(), Some(after_ms(300)));
    let release = f.handle(heartbeat(2, vec![], vec![]), 50);
    assert_eq!(
        commands(&release),
        vec![InjectCmd::Key {
            usage: KEY,
            down: false
        }]
    );
    f.confirm(&release, true, 50);
    assert!(f.held().is_empty());
}

#[test]
fn down_journal_failure_ends_session_releases_all_and_acks_without_pressing() {
    let mut f = Fixture::active();
    f.handle(button(1, true), 0);
    f.journal.0.lock().unwrap().fail_down = true;
    let out = f.handle(key(2, KEY, true), 1);
    assert_eq!(commands(&out), vec![InjectCmd::ReleaseAll]);
    assert_eq!(
        &out[1..5],
        ended(Some(EndReason::Released), Notice::ControlEnded(PEER))
    );
    assert_eq!(out.last(), Some(&ack(2)));
    f.confirm(&out, true, 1);
    assert!(f.held().is_empty());
    assert!(f.handle(key(3, KEY, true), 2).is_empty());
}

#[test]
fn release_journal_failure_ends_session_and_retries_until_recorded() {
    let mut f = Fixture::active();
    f.handle(key(1, KEY, true), 0);
    let up = f.handle(key(2, KEY, false), 1);
    f.journal.0.lock().unwrap().fail_up = true;
    let id = injections(&up)[0].0;
    let failed = f.handle(Input::InjectDone { id, ok: true }, 2);
    assert!(commands(&failed).is_empty());
    assert_eq!(
        failed,
        ended(Some(EndReason::Released), Notice::ControlEnded(PEER))
    );
    assert_eq!(f.held(), vec![Held::Key(KEY)]);
    f.journal.0.lock().unwrap().fail_up = false;
    let retry = f.handle(Input::Tick, 52);
    f.confirm(&retry, true, 52);
    assert!(f.held().is_empty());
}

#[test]
fn local_override_clears_lease_deadline_and_keeps_release_retry() {
    let mut f = Fixture::active();
    f.handle(key(1, KEY, true), 10);
    assert_eq!(f.target.next_deadline(), Some(after_ms(310)));
    let out = f.handle(activity(), 20);
    assert_eq!(f.target.next_deadline(), None);
    f.confirm(&out, false, 20);
    assert_eq!(f.target.next_deadline(), Some(ms(70)));
    let retry = f.handle(Input::Tick, 70);
    f.confirm(&retry, true, 70);
    assert_eq!(f.target.next_deadline(), None);
    assert!(f.handle(Input::Tick, 1020).is_empty());
}

#[test]
fn ticks_exactly_at_next_deadline_release_without_livelock() {
    let mut f = Fixture::active();
    let mut injector = FakeInjector::default();
    let down = f.handle(key(1, KEY, true), 0);
    for (_, cmd) in injections(&down) {
        injector.apply(&cmd, true);
    }
    let mut previous = MonoTime::ZERO;
    for attempt in 0..3 {
        let deadline = f.target.next_deadline().expect("a release is due");
        assert!(deadline > previous, "the simulator must advance time");
        if attempt == 0 {
            assert_eq!(deadline, after_ms(300));
        }
        let out = f.handle_at(Input::Tick, deadline);
        assert_eq!(injections(&out).len(), 1);
        let ok = attempt == 2;
        for (id, cmd) in injections(&out) {
            injector.apply(&cmd, ok);
            assert!(
                f.handle_at(Input::InjectDone { id, ok }, deadline)
                    .is_empty()
            );
        }
        previous = deadline;
    }
    assert!(injector.held.is_empty());
    assert!(f.held().is_empty());
    assert_eq!(f.target.next_deadline(), None);
    assert!(f.handle_at(Input::Tick, previous).is_empty());
}

#[test]
fn second_session_press_has_a_fresh_lease_after_a_long_idle_period() {
    let mut f = Fixture::active();
    f.handle(heartbeat(1, vec![], vec![]), 0);
    f.handle(end(PEER, SESSION), 1_000);
    let second_session = SessionId(11);
    let accepted = f.handle(start(PEER, second_session), 60_000);
    assert_eq!(
        accepted.last(),
        Some(&Output::Notice(Notice::ControlledBy(PEER)))
    );
    let down = f.handle(
        message(InputMessage::Key {
            session: second_session,
            seq: 1,
            usage: KEY,
            down: true,
        }),
        60_010,
    );
    assert_eq!(
        commands(&down),
        vec![InjectCmd::Key {
            usage: KEY,
            down: true
        }]
    );
    let deadline = f.target.next_deadline().unwrap();
    assert!(deadline >= ms(60_310));
    assert_eq!(deadline, after_ms(60_310));
    assert!(f.handle(Input::Tick, 60_011).is_empty());
    assert_eq!(f.held(), vec![Held::Key(KEY)]);
    let release = f.handle_at(Input::Tick, deadline);
    assert_eq!(
        commands(&release),
        vec![InjectCmd::Key {
            usage: KEY,
            down: false
        }]
    );
}

#[test]
fn persistent_record_up_failure_cannot_form_a_completion_feedback_loop() {
    let mut f = Fixture::active();
    f.handle(key(1, KEY, true), 0);
    f.handle(button(2, true), 0);
    let up = f.handle(key(3, KEY, false), 1);
    f.journal.0.lock().unwrap().fail_up = true;
    let first_id = injections(&up)[0].0;
    let mut next_id = first_id;
    let mut emitted = 0;
    for attempt in 0..1_000 {
        let out = f.handle(
            Input::InjectDone {
                id: next_id,
                ok: true,
            },
            2,
        );
        let requests = injections(&out);
        emitted += requests.len();
        if attempt == 0 {
            assert_eq!(
                out.last(),
                Some(&Output::Notice(Notice::ControlEnded(PEER)))
            );
            assert!(out.contains(&Output::SendControl {
                peer: PEER,
                msg: ControlMessage::EndControl {
                    session: SESSION,
                    reason: EndReason::Released
                },
            }));
        }
        next_id = requests.first().map_or(first_id, |(id, _)| *id);
    }
    // The remaining button must join the timer retry, with no injections from completions.
    assert!(emitted <= 1);
    assert_eq!(emitted, 0);
    for tick in 1..=10 {
        let now = 2 + tick * 50;
        let out = f.handle(Input::Tick, now);
        assert_eq!(commands(&out), vec![InjectCmd::ReleaseAll]);
        let id = injections(&out)[0].0;
        assert!(f.handle(Input::InjectDone { id, ok: true }, now).is_empty());
    }
    assert_eq!(f.held(), vec![Held::Key(KEY), Held::Button(BUTTON)]);
}

#[test]
fn key_after_old_local_override_timeout_cannot_resume_without_a_new_session() {
    let mut f = Fixture::active();
    let ended = f.handle(activity(), 0);
    f.confirm(&ended, true, 0);
    let out = f.handle(key(1, KEY, true), 1_001);
    assert!(out.is_empty());
    assert!(f.held().is_empty());
    assert_eq!(f.target.next_deadline(), None);
    assert!(f.handle(key(2, KEY, true), 1_002).is_empty());
}

#[test]
fn untracked_injections_do_not_accumulate_pending_results() {
    let mut f = Fixture::active();
    let mut ids = Vec::new();
    for seq in 1..=1_000 {
        for input in [
            motion(seq),
            message(InputMessage::Scroll {
                session: SESSION,
                seq,
                delta: scroll(),
            }),
            message(InputMessage::LockKeys {
                session: SESSION,
                seq,
                keys: locks(),
            }),
            key(seq, KEY, true),
        ] {
            ids.extend(
                injections(&f.handle(input, 0))
                    .into_iter()
                    .map(|(id, _)| id),
            );
        }
    }
    assert!(format!("{:?}", f.target).contains("pending_count: 0"));
    for id in ids {
        assert!(f.handle(Input::InjectDone { id, ok: false }, 0).is_empty());
    }
    assert_eq!(f.target.next_deadline(), Some(after_ms(300)));
    let up = f.handle(key(1_001, KEY, false), 1);
    assert!(format!("{:?}", f.target).contains("pending_count: 1"));
    f.confirm(&up, true, 1);
    assert!(f.held().is_empty());
    assert!(format!("{:?}", f.target).contains("pending_count: 0"));
}

#[derive(Default)]
struct FakeInjector {
    held: BTreeSet<Held>,
}

impl FakeInjector {
    fn apply(&mut self, cmd: &InjectCmd, ok: bool) {
        if !ok {
            return;
        }
        match cmd {
            InjectCmd::Key { usage, down } => self.transition(Held::Key(*usage), *down),
            InjectCmd::Button { button, down } => self.transition(Held::Button(*button), *down),
            InjectCmd::ReleaseAll => self.held.clear(),
            InjectCmd::Recover { keys, buttons } => {
                for key in keys {
                    self.held.remove(&Held::Key(*key));
                }
                for button in buttons {
                    self.held.remove(&Held::Button(*button));
                }
            }
            _ => {}
        }
    }

    fn transition(&mut self, item: Held, down: bool) {
        if down {
            self.held.insert(item);
        } else {
            self.held.remove(&item);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 2_000,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn no_forbidden_injection_and_nothing_held_after_end(
        events in prop::collection::vec((0u8..24, any::<u8>(), 0u16..400, any::<bool>()), 1..160)
    ) {
        let mut f = Fixture::new();
        let mut injector = FakeInjector::default();
        let mut state = SessionState { lock: LockState::Locked, active: None };
        let mut asleep = false;
        let mut fresh = false;
        let mut granted = false;
        let mut active = false;
        let mut now = 0;
        let mut seq = 0;
        let mut pending = Vec::new();
        let mut last_id = 0;
        for (kind, arg, elapsed, ok) in events {
            now += u64::from(elapsed);
            seq += 1;
            let usage = HidUsage::keyboard(4 + u16::from(arg % 4));
            let input = match kind {
                0 => { state = UNLOCKED; fresh = true; Input::Session(SessionEvent::State(state)) }
                1 => { state = SessionState { lock: LockState::Locked, ..UNLOCKED }; fresh = true; Input::Session(SessionEvent::State(state)) }
                2 => { state = SessionState { lock: LockState::Unknown, active: None }; fresh = true; Input::Session(SessionEvent::State(state)) }
                3 => { asleep = true; Input::Session(SessionEvent::WillSleep) }
                4 => { asleep = false; fresh = false; Input::Session(SessionEvent::Woke) }
                5 => { granted = true; Input::Grants([(PEER, BTreeSet::from([Capability::InputAccept]))].into()) }
                6 => { granted = false; Input::Grants(Default::default()) }
                7 | 8 => start(PEER, SESSION),
                9 => key(seq, usage, true),
                10 => key(seq, usage, false),
                11 => button(seq, true),
                12 => button(seq, false),
                13 => heartbeat(seq, vec![], vec![]),
                14 => heartbeat(seq, vec![usage], vec![BUTTON]),
                15 => activity(),
                16 => motion(u32::from(arg)),
                17 => end(PEER, SESSION),
                18 => Input::Link(LinkEvent::Closed { peer: PEER, error: LinkError::Closed }),
                19 => Input::Command(Command::Panic),
                20 => message(InputMessage::Scroll { session: SESSION, seq, delta: scroll() }),
                21 => message(InputMessage::LockKeys { session: SESSION, seq, keys: locks() }),
                22 if !pending.is_empty() => {
                    let index = usize::from(arg) % pending.len();
                    let (id, ok) = pending.swap_remove(index);
                    Input::InjectDone { id, ok }
                }
                _ => Input::Tick,
            };
            let permitted = state.permits_io() && !asleep && fresh;
            let out = f.handle(input, now);
            for output in &out {
                match output {
                    Output::Inject { id, cmd } => {
                        prop_assert!(id.0 > last_id);
                        last_id = id.0;
                        let release = matches!(cmd, InjectCmd::Key { down: false, .. } | InjectCmd::Button { down: false, .. } | InjectCmd::ReleaseAll | InjectCmd::Recover { .. });
                        prop_assert!(release || permitted, "forbidden injection: {cmd:?}");
                        match cmd {
                            InjectCmd::Key { usage, down: true } => prop_assert!(active && f.held().contains(&Held::Key(*usage))),
                            InjectCmd::Button { button, down: true } => prop_assert!(active && f.held().contains(&Held::Button(*button))),
                            _ => {}
                        }
                        // Decide the real execution result now, including failed releases. Only
                        // successful commands affect the fake; deliver that same result later.
                        injector.apply(cmd, ok);
                        if arg % 2 == 0 {
                            // Immediate failed results let subsequent random ticks exercise
                            // retries during the sequence, as well as during final cleanup.
                            prop_assert!(
                                f.handle(Input::InjectDone { id: *id, ok }, now).is_empty(),
                                "completion unexpectedly emitted outputs"
                            );
                        } else {
                            pending.push((*id, ok));
                        }
                    }
                    Output::SendControl { msg: ControlMessage::ControlStarted { .. }, .. } => {
                        prop_assert!(permitted && granted);
                        active = true;
                    }
                    Output::Notice(Notice::ControlEnded(_) | Notice::TargetLocked(_)) => {
                        active = false;
                    }
                    _ => {}
                }
            }
            // A failed release may leave physical state held after the session ends. It must
            // remain journaled until an actually successful retry, even with delayed results.
            let journaled = f.held();
            prop_assert!(injector.held.iter().all(|item| journaled.contains(item)));
        }
        let out = f.handle(Input::Command(Command::Panic), now + 1);
        for (_, cmd) in injections(&out) { injector.apply(&cmd, true); }
        // A final successful retry must also clear all journaled releases.
        f.confirm(&out, true, now + 1);
        // Report the real outcomes of executions left in flight. Failed releases must enter
        // the timer retry path rather than being falsely confirmed at the end of the run.
        for (id, ok) in pending {
            prop_assert!(
                commands(&f.handle(Input::InjectDone { id, ok }, now + 1)).is_empty(),
                "completion unexpectedly emitted injections"
            );
        }
        let retry = f.handle(Input::Tick, now + 51);
        for (_, cmd) in injections(&retry) { injector.apply(&cmd, true); }
        f.confirm(&retry, true, now + 51);
        prop_assert!(injector.held.is_empty());
        prop_assert!(f.held().is_empty());
    }
}
