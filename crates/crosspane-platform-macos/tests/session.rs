#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, mpsc};
use std::time::Duration;

use crosspane_platform::{IoGate, LockState, SessionEvent, SessionEvents, SessionState};
use crosspane_platform_macos::session::MacSession;

#[test]
fn read_only_session() {
    let gate = IoGate::new();
    gate.set_engine_permits(true);
    let mut session = MacSession::new(gate.clone()).expect("read-only session backend");
    let state = session.state();
    eprintln!(
        "read-only caller session: {state:?}; gate open: {}",
        gate.is_open()
    );
    if std::env::var_os("CROSSPANE_SESSION_GUI").as_deref() == Some(std::ffi::OsStr::new("1")) {
        assert_eq!(
            state,
            SessionState {
                lock: LockState::Unlocked,
                active: Some(true)
            }
        );
    }
    if state.lock == LockState::Unknown {
        assert!(!gate.is_open());
    }
    let (tx, rx) = mpsc::channel();
    session
        .subscribe(Arc::new(move |event| {
            tx.send(event).expect("test receiver")
        }))
        .expect("subscribe");
    assert!(matches!(
        rx.recv_timeout(Duration::from_secs(2))
            .expect("initial state"),
        SessionEvent::State(_)
    ));
    assert!(session.subscribe(Arc::new(|_| {})).is_err());
    drop(session);
    assert!(!gate.is_open());
    assert!(rx.recv_timeout(Duration::from_millis(600)).is_err());
}
