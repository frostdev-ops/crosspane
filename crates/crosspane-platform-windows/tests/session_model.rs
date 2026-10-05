use std::time::Duration;

use crosspane_platform::{IoGate, LockState, SessionEvent, SessionState};
use crosspane_platform_windows::model::session::{
    Reading, SessionModel, Signal, UNKNOWN, missed_sleep,
};

const OPEN: Reading = Reading {
    console: Some(true),
    connected: Some(true),
    default_desktop: Some(true),
};

fn observe(model: &mut SessionModel, reading: Reading) -> Vec<SessionEvent> {
    model.complete(model.begin_read(), reading)
}

fn open() -> SessionModel {
    let mut model = SessionModel::default();
    observe(&mut model, OPEN);
    assert!(model.state().permits_io());
    model
}

#[test]
fn session_initial_and_every_incomplete_read_are_unknown() {
    let mut model = SessionModel::default();
    assert_eq!(model.state(), UNKNOWN);
    for reading in [
        Reading::UNKNOWN,
        Reading {
            console: None,
            ..OPEN
        },
        Reading {
            connected: None,
            ..OPEN
        },
        Reading {
            default_desktop: None,
            ..OPEN
        },
    ] {
        observe(&mut model, OPEN);
        assert_eq!(
            observe(&mut model, reading),
            vec![SessionEvent::State(UNKNOWN)]
        );
    }
}

#[test]
fn session_only_default_active_connected_session_permits_io() {
    for console in [false, true] {
        for connected in [false, true] {
            for default in [false, true] {
                let mut model = SessionModel::default();
                observe(
                    &mut model,
                    Reading {
                        console: Some(console),
                        connected: Some(connected),
                        default_desktop: Some(default),
                    },
                );
                assert_eq!(model.state().permits_io(), console && connected && default);
                assert_eq!(model.state().active, Some(console && connected));
                if !default {
                    assert_eq!(model.state().lock, LockState::Locked);
                }
            }
        }
    }
}

#[test]
fn session_lock_closes_before_events_and_unlock_requires_validation() {
    let gate = IoGate::new();
    gate.set_engine_permits(true);
    let mut model = open();
    gate.set_session_permits(model.state().permits_io());
    for signal in [Signal::Lock, Signal::Unlock] {
        let events = model.signal(signal);
        gate.set_session_permits(model.state().permits_io());
        assert!(
            !gate.is_open(),
            "gate must be closed when notification is delivered"
        );
        assert!(!events.is_empty());
    }
    observe(&mut model, OPEN);
    assert!(model.state().permits_io());
}

#[test]
fn session_lock_read_before_desktop_transition_cannot_reopen() {
    let mut model = open();
    model.signal(Signal::Lock);
    observe(&mut model, OPEN);
    assert_eq!(model.state().lock, LockState::Locked);
    observe(
        &mut model,
        Reading {
            default_desktop: Some(false),
            ..OPEN
        },
    );
    observe(&mut model, OPEN); // Correct a missed unlock only after observing the locked desktop.
    assert!(model.state().permits_io());
}

#[test]
fn session_deactivation_and_reconnect_never_reuse_old_permission() {
    let mut model = open();
    model.signal(Signal::Inactive);
    assert_eq!(model.state().active, Some(false));
    assert!(!model.state().permits_io());
    model.signal(Signal::Refresh);
    assert_eq!(model.state(), UNKNOWN);
    observe(&mut model, OPEN);
    assert!(model.state().permits_io());
}

#[test]
fn session_sleep_stays_closed_and_wake_requires_a_later_fresh_read() {
    let mut model = open();
    let old = model.begin_read();
    assert_eq!(
        model.signal(Signal::Sleep),
        vec![SessionEvent::WillSleep, SessionEvent::State(UNKNOWN)]
    );
    observe(&mut model, OPEN);
    assert!(!model.state().permits_io());
    let sleeping = model.begin_read();
    assert_eq!(
        model.signal(Signal::Wake),
        vec![SessionEvent::Woke, SessionEvent::State(UNKNOWN)]
    );
    assert!(model.complete(old, OPEN).is_empty());
    assert!(model.complete(sleeping, OPEN).is_empty());
    assert!(!model.state().permits_io());
    assert_eq!(
        observe(&mut model, OPEN),
        vec![SessionEvent::State(SessionState {
            lock: LockState::Unlocked,
            active: Some(true)
        })]
    );
}

#[test]
fn session_every_signal_invalidates_an_inflight_read() {
    for signal in [
        Signal::Lock,
        Signal::Unlock,
        Signal::Inactive,
        Signal::Refresh,
        Signal::Sleep,
        Signal::Wake,
        Signal::MissedSleep,
        Signal::Lost,
    ] {
        let mut model = open();
        let before = model.begin_read();
        model.signal(signal);
        assert!(model.complete(before, OPEN).is_empty());
        assert!(!model.state().permits_io());
    }
}

#[test]
fn session_observation_death_is_terminal() {
    let mut model = open();
    assert_eq!(
        model.signal(Signal::Lost),
        vec![SessionEvent::State(UNKNOWN)]
    );
    observe(&mut model, OPEN);
    model.signal(Signal::Wake);
    assert!(!model.state().permits_io());
}

#[test]
fn session_unannounced_sleep_and_clock_adjustment_close_before_revalidation() {
    let half = Duration::from_millis(500);
    assert!(!missed_sleep(half, half, half));
    assert!(missed_sleep(Duration::from_secs(3), half, half));
    assert!(missed_sleep(half, Duration::from_secs(10), half));
    assert!(missed_sleep(half, half, Duration::from_secs(10)));
    let mut model = open();
    let before = model.begin_read();
    model.signal(Signal::MissedSleep);
    assert!(model.complete(before, OPEN).is_empty());
    assert!(!model.state().permits_io());
    observe(&mut model, OPEN);
    assert!(model.state().permits_io());
}

#[test]
fn session_poll_delay_cannot_clear_an_announced_sleep_latch() {
    let mut model = open();
    model.signal(Signal::Sleep);
    let events = model.signal(Signal::MissedSleep);
    assert!(!events.contains(&SessionEvent::Woke));
    observe(&mut model, OPEN);
    assert!(!model.state().permits_io());
    model.signal(Signal::Wake);
    observe(&mut model, OPEN);
    assert!(model.state().permits_io());
}
