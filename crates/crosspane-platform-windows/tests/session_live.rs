//! Explicitly ignored: native observation runs only as a Limited process through win-gui.
#![cfg(windows)]
#![allow(unsafe_code, clippy::unwrap_used)]

use std::mem::size_of;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use crosspane_platform::{IoGate, LockState, SessionEvent, SessionEvents};
use crosspane_platform_windows::session::WindowsSession;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

struct CancelWatchdog(mpsc::Sender<()>);
impl Drop for CancelWatchdog {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

#[test]
#[ignore = "Limited initial-state probe; run this binary only through win-gui with --ignored --exact"]
fn limited_initial_state_and_subscription() {
    let (cancel, cancelled) = mpsc::channel();
    let watchdog = std::thread::spawn(move || {
        if matches!(
            cancelled.recv_timeout(Duration::from_secs(8)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ) {
            eprintln!("Windows session probe exceeded own deadline");
            std::process::exit(124);
        }
    });
    let cancellation = CancelWatchdog(cancel);
    let mut token = std::ptr::null_mut();
    let mut elevation = TOKEN_ELEVATION::default();
    let mut bytes = 0;
    // SAFETY: query only this test process's token into an owned output handle.
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    assert_ne!(opened, 0);
    // SAFETY: exact initialized structure/output sizes; read-only own elevation query.
    let queried = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut bytes,
        )
    };
    // SAFETY: close only the token handle opened by this test, once.
    let closed = unsafe { CloseHandle(token) };
    assert_ne!(queried, 0);
    assert_ne!(closed, 0);
    assert_eq!(
        elevation.TokenIsElevated, 0,
        "native probe is forbidden under elevated SSH"
    );
    println!("TOKEN elevated=false");
    let gate = IoGate::new();
    gate.set_engine_permits(true);
    let mut session = WindowsSession::new(gate.clone()).unwrap();
    let state = session.state();
    println!("STATE {state:?} gate_open={}", gate.is_open());
    assert_eq!(state.lock, LockState::Unlocked);
    assert_eq!(state.active, Some(true));
    let (events, received) = mpsc::channel();
    session
        .subscribe(Arc::new(move |event| {
            let _ = events.send(event);
        }))
        .unwrap();
    let initial = received.recv_timeout(Duration::from_secs(1)).unwrap();
    println!("INITIAL {initial:?}");
    assert_eq!(initial, SessionEvent::State(state));
    drop(session);
    assert!(!gate.is_open());
    println!("DROPPED gate_open=false; no lock/sleep/UAC transition invoked");
    drop(cancellation);
    watchdog.join().unwrap();
}
