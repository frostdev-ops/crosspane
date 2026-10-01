//! Read-only desktop logind smoke test. No compositor access and no lock/unlock requests.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use crosspane_platform::{IoGate, LockState, SessionEvent, SessionEvents};
use crosspane_platform_linux::logind::LogindSession;

#[test]
fn desktop_session_is_unlocked_and_active() {
    // CI runners have a system bus but no graphical session: require both.
    let graphical = matches!(
        std::env::var("XDG_SESSION_TYPE").as_deref(),
        Ok("wayland" | "x11")
    );
    if std::env::var_os("XDG_SESSION_ID").is_none()
        || !graphical
        || (std::env::var_os("DBUS_SYSTEM_BUS_ADDRESS").is_none()
            && !Path::new("/run/dbus/system_bus_socket").exists())
    {
        eprintln!("skipped: needs a graphical logind session and the system D-Bus");
        return;
    }
    let gate = IoGate::new();
    gate.set_engine_permits(true);
    let start = Instant::now();
    let mut session = LogindSession::new(gate.clone(), None).unwrap();
    assert!(start.elapsed() < Duration::from_secs(2));
    let state = session.state();
    assert_eq!(state.lock, LockState::Unlocked);
    assert_eq!(state.active, Some(true));
    assert!(gate.is_open());
    let (tx, rx) = mpsc::channel();
    let caller = std::thread::current().id();
    session
        .subscribe(Arc::new(move |event| {
            tx.send((std::thread::current().id(), event)).unwrap();
        }))
        .unwrap();
    let (delivery_thread, event) = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_ne!(delivery_thread, caller);
    assert_eq!(event, SessionEvent::State(state));
    assert!(session.subscribe(Arc::new(|_| {})).is_err());
    eprintln!(
        "read-only logind state: {state:?}; gate open: {}",
        gate.is_open()
    );
    let start = Instant::now();
    drop(session);
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(!gate.is_open());
    assert_eq!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected));
}
