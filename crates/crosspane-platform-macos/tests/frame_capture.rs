#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use block2::RcBlock;
use crosspane_platform::{
    CaptureTarget, FrameCapture, FrameEvent, IoGate, Permission, PlatformError, StreamEndReason,
};
use crosspane_platform_macos::clock;
use crosspane_platform_macos::frame_capture::MacFrameCapture;
use crosspane_types::geom::PixelSize;
use crosspane_types::id::WindowId;
use objc2::AnyThread;
use objc2_core_graphics::{CGEvent, CGPreflightScreenCaptureAccess};
use objc2_core_media::CMClock;
use objc2_foundation::NSError;
use objc2_screen_capture_kit::{SCContentFilter, SCShareableContent};

// Public declaration in Apple's mach/mach_time.h; observation only, never posts a CGEvent.
unsafe extern "C" {
    fn mach_absolute_time() -> u64;
}

#[test]
fn missing_screen_recording_is_permission_denied() {
    // The spec asks for an empirical CGEvent clock comparison. Creating an unposted generic
    // event observes current state; it does not inject input or install a tap.
    let event = CGEvent::new(None).expect("CGEventCreate(NULL)");
    let event_timestamp = CGEvent::timestamp(Some(&event));
    // SAFETY: Public Mach function takes no arguments and only reads a monotonic counter.
    let mach_ticks = unsafe { mach_absolute_time() };
    let now = clock::now().as_nanos();
    // SAFETY: Public CoreMedia conversion functions accept the current host clock ticks/time.
    let round_trip = unsafe {
        CMClock::convert_host_time_to_system_units(CMClock::make_host_time_from_system_units(
            mach_ticks,
        ))
    };
    assert!(
        round_trip.abs_diff(mach_ticks) <= 1,
        "CoreMedia host time round-trip"
    );
    eprintln!("CoreMedia host-time conversion round-trip: {mach_ticks} -> {round_trip} mach ticks");
    let tick_error = clock::from_ticks(event_timestamp).as_nanos().abs_diff(now);
    let ns_error = event_timestamp.abs_diff(now);
    eprintln!(
        "clock probe: CGEvent={event_timestamp}, mach_ticks={mach_ticks}, clock_now_ns={now}, tick_error_ns={tick_error}, ns_error_ns={ns_error}"
    );
    if event_timestamp == 0 {
        eprintln!(
            "clock probe unavailable: CGEventCreate(NULL) has a zero timestamp; no input observation or injection attempted"
        );
    } else {
        assert!(
            tick_error.min(ns_error) < 1_000_000_000,
            "CGEvent clock matches neither ticks nor nanoseconds"
        );
        eprintln!(
            "clock probe: CGEventGetTimestamp matches {}",
            if tick_error < ns_error {
                "mach ticks"
            } else {
                "nanoseconds"
            }
        );
    }

    if CGPreflightScreenCaptureAccess() {
        eprintln!(
            "skipped: responsible process already has Screen Recording; denial cannot be asserted"
        );
        return;
    }
    let result = MacFrameCapture::new(IoGate::new());
    eprintln!("constructor without Screen Recording: {result:?}");
    assert!(matches!(
        result,
        Err(PlatformError::PermissionDenied(Permission::ScreenRecording))
    ));
}

fn applescript(script: &str) -> String {
    let output = std::process::Command::new("/usr/bin/osascript")
        .args(["-e", script])
        .output()
        .expect("run TextEdit fixture AppleScript");
    assert!(
        output.status.success(),
        "TextEdit fixture: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("AppleScript UTF-8")
        .trim()
        .to_owned()
}

// Lead-run only: controls exactly the disposable TextEdit window created by this test.
struct TextEditWindow(u64);

impl Drop for TextEditWindow {
    fn drop(&mut self) {
        let _ = std::process::Command::new("/usr/bin/osascript")
            .args([
                "-e",
                &format!(
                    "tell application \"TextEdit\" to close window id {} saving no",
                    self.0
                ),
            ])
            .output();
    }
}

fn window_size(id: u64) -> PixelSize {
    let (tx, rx) = mpsc::channel();
    let callback = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // SAFETY: SCK supplies live nullable completion arguments. All objects are borrowed only
            // inside this callback, and only scalar geometry is sent to the libtest thread.
            let size = unsafe {
                assert!(error.is_null(), "SCK enumeration failed in live fixture");
                let content = content.as_ref().expect("shareable content");
                content
                    .windows()
                    .iter()
                    .find(|w| u64::from(w.windowID()) == id)
                    .map(|window| {
                        let filter = SCContentFilter::initWithDesktopIndependentWindow(
                            SCContentFilter::alloc(),
                            &window,
                        );
                        let scale = f64::from(filter.pointPixelScale());
                        let frame = window.frame();
                        PixelSize::new(
                            (frame.size.width * scale).round() as u32,
                            (frame.size.height * scale).round() as u32,
                        )
                    })
            };
            let _ = tx.send(size);
        },
    );
    // SAFETY: Correct completion signature; SCK copies the block. No AppKit run loop is needed.
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&callback) };
    rx.recv_timeout(Duration::from_secs(2))
        .expect("bounded content enumeration")
        .expect("TextEdit window in SCK")
}

#[test]
fn live_textedit_capture_resize_and_blocked_end() {
    if std::env::var("CROSSPANE_MAC_LIVE").as_deref() != Ok("1") {
        eprintln!(
            "skipped: TextEdit capture, resize, and gate-close test requires CROSSPANE_MAC_LIVE=1 in the GUI session"
        );
        return;
    }
    assert!(
        CGPreflightScreenCaptureAccess(),
        "lead must run under the granted Crosspane.app responsible identity"
    );
    let id: u64 = applescript("tell application \"TextEdit\"\nset d to make new document with properties {text:\"Crosspane capture test fixture\"}\nreturn id of first window whose document is d\nend tell")
        .parse().expect("TextEdit CGWindowID");
    let fixture = TextEditWindow(id);
    applescript(&format!(
        "tell application \"TextEdit\" to set bounds of window id {id} to {{120, 120, 520, 420}}"
    ));
    let expected = window_size(id);
    let gate = IoGate::new();
    gate.set_session_permits(true);
    gate.set_engine_permits(true);
    let mut capture = MacFrameCapture::new(gate.clone()).expect("Screen Recording granted");
    let (tx, rx) = mpsc::channel();
    let stream = capture
        .start(
            CaptureTarget::Window(WindowId(id)),
            None,
            30,
            Arc::new(move |event| {
                let _ = tx.send(event);
            }),
        )
        .expect("start TextEdit capture");

    // Static windows can produce idle rather than complete samples. Animate only this test's
    // own document with AppleEvents (never post synthetic input) so >=5 complete frames is real.
    let start = Instant::now();
    let mut frames = 0;
    let mut change = 0;
    while start.elapsed() < Duration::from_secs(1) {
        change += 1;
        applescript(&format!(
            "tell application \"TextEdit\" to set text of document of window id {id} to \"Crosspane fixture {change}\""
        ));
        std::thread::sleep(Duration::from_millis(80));
        for event in rx.try_iter() {
            if let FrameEvent::Frame {
                stream: received,
                frame,
            } = event
            {
                assert_eq!(received, stream);
                assert_eq!(frame.size, expected);
                assert_eq!(frame.stride, frame.size.width * 4);
                assert_eq!(
                    frame.pixels.len(),
                    frame.stride as usize * frame.size.height as usize
                );
                assert!(frame.at <= clock::now());
                frames += 1;
            }
        }
    }
    assert!(
        frames >= 5,
        "received {frames} complete TextEdit frames in one second"
    );
    applescript(&format!(
        "tell application \"TextEdit\" to set bounds of window id {id} to {{120, 120, 720, 620}}"
    ));
    let resized = window_size(id);
    assert_ne!(resized, expected);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let event = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("frame follows resize");
        if let FrameEvent::Frame { frame, .. } = event
            && frame.size == resized
        {
            break;
        }
    }
    gate.set_engine_permits(false);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("gate close ends capture")
        {
            FrameEvent::Ended {
                stream: received,
                reason,
            } => {
                assert_eq!(received, stream);
                assert_eq!(reason, StreamEndReason::Blocked);
                break;
            }
            FrameEvent::Frame { .. } => {} // Frames queued before closing the gate.
            _ => panic!("unexpected frame event"),
        }
    }
    assert!(
        rx.recv_timeout(Duration::from_millis(100)).is_err(),
        "no frames or duplicate end after Blocked"
    );
    drop(capture);
    drop(fixture);
}
