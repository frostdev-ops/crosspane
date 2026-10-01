#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // Test failures deliberately panic.

use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use crosspane_platform::{
    CaptureEvent, CaptureId, CapturePortal, Edge, EndReason, InputCapture, IoGate, PlatformError,
    PortalId,
};
use crosspane_platform_macos::capture::MacCapture;
use crosspane_types::id::DisplayId;
use objc2_core_graphics::{CGDisplayBounds, CGDisplayPixelsWide, CGEvent, CGMainDisplayID};

// The lead opts in in an attended GUI session. Never enable this from ordinary acceptance runs.
static LIVE_SERIAL: Mutex<()> = Mutex::new(());

fn live() -> bool {
    if std::env::var("CROSSPANE_MAC_LIVE").as_deref() == Ok("1") {
        true
    } else {
        eprintln!("skipped: live capture requires CROSSPANE_MAC_LIVE=1 in the lead's GUI session");
        false
    }
}

fn open_gate() -> Arc<IoGate> {
    let gate = IoGate::new();
    gate.set_session_permits(true);
    gate.set_engine_permits(true);
    gate
}

fn subscribed(gate: Arc<IoGate>) -> (MacCapture, mpsc::Receiver<CaptureEvent>) {
    let mut capture = MacCapture::new(gate).expect("both TCC grants and GUI session required");
    let display = CGMainDisplayID();
    let bounds = CGDisplayBounds(display);
    let scale = CGDisplayPixelsWide(display) as f64 / bounds.size.width;
    capture
        .set_portals(&[CapturePortal {
            id: PortalId(1),
            display: DisplayId(display),
            edge: Edge::Left,
            from: 0.0,
            to: bounds.size.height * scale,
        }])
        .unwrap();
    let (sender, receiver) = mpsc::channel();
    capture
        .subscribe(Arc::new(move |event| {
            let _ = sender.send(event);
        }))
        .unwrap();
    assert!(matches!(
        receiver.recv_timeout(Duration::from_millis(50)).unwrap(),
        CaptureEvent::LockKeys(_)
    ));
    assert!(matches!(
        receiver.recv_timeout(Duration::from_millis(50)).unwrap(),
        CaptureEvent::KeyboardBlinded(false)
    ));
    (capture, receiver)
}

fn assert_ended(receiver: &mpsc::Receiver<CaptureEvent>, id: CaptureId, reason: EndReason) {
    let deadline = Instant::now() + Duration::from_millis(50);
    loop {
        let event = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        if matches!(event, CaptureEvent::Ended { id: got, reason: why } if got == id && why == reason)
        {
            break;
        }
    }
}

#[test]
fn live_new_succeeds() {
    if !live() {
        return;
    }
    let _serial = LIVE_SERIAL.lock().unwrap();
    let _capture =
        MacCapture::new(IoGate::new()).expect("signed responsible process needs both TCC grants");
}

#[test]
fn live_begin_end_restores_cursor() {
    if !live() {
        return;
    }
    let _serial = LIVE_SERIAL.lock().unwrap();
    let (mut capture, receiver) = subscribed(open_gate());
    let before_event = CGEvent::new(None).unwrap();
    let before = CGEvent::location(Some(&before_event));
    let started = Instant::now();
    capture.begin(CaptureId(1), PortalId(1)).unwrap();
    assert!(started.elapsed() < Duration::from_millis(50));
    assert!(matches!(
        receiver.recv_timeout(Duration::from_millis(50)).unwrap(),
        CaptureEvent::Started { id: CaptureId(1) }
    ));
    let ended = Instant::now();
    capture.end(None).unwrap();
    assert!(ended.elapsed() < Duration::from_millis(50));
    assert_ended(&receiver, CaptureId(1), EndReason::Requested);
    let after_event = CGEvent::new(None).unwrap();
    let after = CGEvent::location(Some(&after_event));
    assert_eq!(
        before, after,
        "cursor must remain at its frozen position; keep the physical pointer still"
    );
    // Lead also observes cursor visibility/restored physical movement. CGCursorIsVisible is
    // deprecated as unsupported; no supported public query exposes association/hide count.
}

#[test]
fn live_abort_from_another_thread_within_fifty_ms() {
    if !live() {
        return;
    }
    let _serial = LIVE_SERIAL.lock().unwrap();
    let (mut capture, receiver) = subscribed(open_gate());
    capture.begin(CaptureId(2), PortalId(1)).unwrap();
    assert!(matches!(
        receiver.recv_timeout(Duration::from_millis(50)).unwrap(),
        CaptureEvent::Started { id: CaptureId(2) }
    ));
    let abort = capture.abort_handle();
    let elapsed = std::thread::spawn(move || {
        let start = Instant::now();
        abort.abort();
        abort.abort(); // Idempotent.
        start.elapsed()
    })
    .join()
    .unwrap();
    assert!(elapsed < Duration::from_millis(50));
    assert_ended(&receiver, CaptureId(2), EndReason::Aborted);
}

#[test]
fn live_closed_gate_refuses_begin() {
    if !live() {
        return;
    }
    let _serial = LIVE_SERIAL.lock().unwrap();
    let mut capture = MacCapture::new(IoGate::new()).unwrap();
    assert!(matches!(
        capture.begin(CaptureId(3), PortalId(1)),
        Err(PlatformError::Locked)
    ));
}
