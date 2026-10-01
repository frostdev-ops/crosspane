#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(unexpected_cfgs)]
#![cfg_attr(crosspane_cursor_driver, allow(dead_code, unused_imports))]

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
                let (pixels, stride) = frame.to_cpu().unwrap();
                assert_eq!(stride, frame.size.width * 4);
                assert_eq!(pixels.len(), stride as usize * frame.size.height as usize);
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
            FrameEvent::Frame { .. } | FrameEvent::Cursor { .. } => {} // Queued before gate close.
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

#[test]
fn live_display_native_buffers_sixty_seconds() {
    use crosspane_platform::Frame;
    use crosspane_platform_macos::frame_capture::held_capture_buffers;
    use crosspane_types::id::DisplayId;
    use objc2_core_graphics::CGMainDisplayID;
    use std::collections::VecDeque;
    use std::hash::{DefaultHasher, Hasher};
    use std::sync::Mutex;

    if std::env::var("CROSSPANE_MAC_LIVE").as_deref() != Ok("1") {
        eprintln!(
            "skipped: 60 s display retention/CPU test requires CROSSPANE_MAC_LIVE=1 in the lead's GUI session"
        );
        return;
    }
    assert!(CGPreflightScreenCaptureAccess());
    // Apple's public time.h defines CLOCK_PROCESS_CPUTIME_ID as 12.
    unsafe extern "C" {
        fn clock_gettime_nsec_np(clock: u32) -> u64;
    }
    let cpu_time = || {
        // SAFETY: Public observation-only process clock, no pointers or mutable state.
        unsafe { clock_gettime_nsec_np(12) }
    };
    let fixture = TextEditWindow(applescript("tell application \"TextEdit\"\nset d to make new document with properties {text:\"Crosspane native capture fixture\"}\nreturn id of first window whose document is d\nend tell").parse().unwrap());
    let gate = IoGate::new();
    gate.set_session_permits(true);
    gate.set_engine_permits(true);
    let mut capture = MacFrameCapture::new(gate).unwrap();
    let held = Arc::new(Mutex::new(VecDeque::<Frame>::new()));
    let received = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let sink_held = Arc::clone(&held);
    let sink_received = Arc::clone(&received);
    let stream = capture
        .start(
            CaptureTarget::Display(DisplayId(CGMainDisplayID())),
            None,
            30,
            Arc::new(move |event| {
                match event {
                    FrameEvent::Frame { frame, .. } => {
                        let mut held = sink_held.lock().unwrap();
                        // The callback's incoming buffer plus the previous two is at most three.
                        assert!(held_capture_buffers() <= 3);
                        if held.len() == 2 {
                            held.pop_front();
                        }
                        held.push_back(frame);
                        sink_received.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    FrameEvent::Ended {
                        reason: StreamEndReason::Requested,
                        ..
                    } => {}
                    FrameEvent::Ended { reason, .. } => panic!("display stream ended: {reason:?}"),
                    _ => {}
                }
            }),
        )
        .unwrap();
    let start = Instant::now();
    let mut frames = 0;
    let mut totals = [0u64; 2];
    let mut checksum = 0;
    let mut last_count = 0;
    let mut change = 0;
    let mut last_arrival = Instant::now();
    while start.elapsed() < Duration::from_secs(60) {
        change += 1;
        applescript(&format!(
            "tell application \"TextEdit\" to set text of document of window id {} to \"Crosspane native fixture {}\"",
            fixture.0, change
        ));
        std::thread::sleep(Duration::from_millis(100));
        let count = received.load(std::sync::atomic::Ordering::Relaxed);
        if count != last_count {
            last_count = count;
            last_arrival = Instant::now();
        }
        assert!(
            last_arrival.elapsed() < Duration::from_secs(3),
            "SCK stalled with two buffers held"
        );
        let held = held.lock().unwrap();
        if let Some(frame) = held.back() {
            let hash_rows = |pixels: &[u8], stride: u32| {
                let mut hash = DefaultHasher::new();
                for row in pixels
                    .chunks(stride as usize)
                    .take(frame.size.height as usize)
                {
                    hash.write(&row[..frame.size.width as usize * 4]);
                }
                hash.finish()
            };
            let before = cpu_time();
            let native_hash = frame.with_pixels(hash_rows).unwrap();
            totals[0] += cpu_time() - before;
            let before = cpu_time();
            let (pixels, stride) = frame.to_cpu().unwrap();
            let copied_hash = hash_rows(&pixels, stride);
            totals[1] += cpu_time() - before;
            assert_eq!(native_hash, copied_hash);
            checksum ^= native_hash;
            frames += 1;
        }
    }
    assert!(frames >= 300, "only {frames} processed samples in 60 s");
    assert!(last_count >= 300, "only {last_count} captures in 60 s");
    eprintln!(
        "display native: received={last_count}, measured={frames}, process CPU ns/frame read+hash={}, copy+hash={}, checksum={checksum}",
        totals[0] / frames,
        totals[1] / frames
    );
    capture.stop(stream).unwrap();
    drop(capture);
    held.lock().unwrap().clear();
    assert_eq!(held_capture_buffers(), 0);
    drop(fixture);
}

// Like private_vdisplay.rs, compile this file as a small driver: libtest owns main, but
// the cursor watcher's AppKit calls require a running main-thread application loop.
#[cfg(not(crosspane_cursor_driver))]
#[test]
fn live_main_display_cursor() {
    if std::env::var("CROSSPANE_MAC_LIVE").as_deref() != Ok("1") {
        eprintln!(
            "skipped: cursor capture requires CROSSPANE_MAC_LIVE=1 in the lead's GUI session"
        );
        return;
    }
    let deps = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let executable = deps.join(format!("capture-cursor-gui-{}", std::process::id()));
    let mut compiler = std::process::Command::new("rustc");
    compiler
        .args(["--edition=2024", "--cfg", "crosspane_cursor_driver", "-L"])
        .arg(format!("dependency={}", deps.display()));
    for name in [
        "crosspane_platform",
        "crosspane_platform_macos",
        "crosspane_types",
        "block2",
        "objc2",
        "objc2_app_kit",
        "objc2_core_graphics",
        "objc2_core_media",
        "objc2_foundation",
        "objc2_screen_capture_kit",
    ] {
        let prefix = format!("lib{name}-");
        let version = if name == "objc2" {
            Some("objc2-0.6.4/".to_owned())
        } else if name.starts_with("objc2_") {
            Some(format!("{}-0.3.2/", name.replace('_', "-")))
        } else {
            None
        };
        let library = std::fs::read_dir(&deps)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(&prefix)
                    && path.extension().is_some_and(|ext| ext == "rlib")
            })
            .filter(|path| {
                version.as_ref().is_none_or(|version| {
                    let stem = path.file_stem().unwrap().to_string_lossy();
                    let info = deps.join(format!("{}.d", stem.trim_start_matches("lib")));
                    std::fs::read_to_string(info).is_ok_and(|info| info.contains(version))
                })
            })
            .max_by_key(|path| path.metadata().unwrap().modified().unwrap())
            .unwrap_or_else(|| panic!("missing compiled dependency {name}"));
        compiler
            .arg("--extern")
            .arg(format!("{name}={}", library.display()));
    }
    assert!(
        compiler
            .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/frame_capture.rs"))
            .arg("-o")
            .arg(&executable)
            .status()
            .unwrap()
            .success()
    );
    let status = std::process::Command::new(&executable)
        .env("CROSSPANE_MAC_LIVE", "1")
        .status()
        .unwrap();
    std::fs::remove_file(executable).unwrap();
    assert!(status.success(), "cursor GUI driver failed: {status}");
}

#[cfg(crosspane_cursor_driver)]
fn main() {
    assert_eq!(std::env::var("CROSSPANE_MAC_LIVE").as_deref(), Ok("1"));
    std::thread::spawn(|| {
        let result = std::panic::catch_unwind(cursor_capture_test);
        std::process::exit(if result.is_ok() { 0 } else { 1 });
    });
    crosspane_platform_macos::main_thread::run_app().expect("AppKit run loop");
}

#[cfg(crosspane_cursor_driver)]
fn cursor_capture_test() {
    use crosspane_types::id::DisplayId;
    use objc2_app_kit::NSCursor;
    use objc2_core_graphics::{
        CGDisplayBounds, CGError, CGMainDisplayID, CGWarpMouseCursorPosition,
    };
    use objc2_foundation::{NSArray, NSPoint};
    use objc2_screen_capture_kit::SCWindow;

    assert!(
        CGPreflightScreenCaptureAccess(),
        "lead must use a granted responsible identity"
    );
    let display = CGMainDisplayID();
    let bounds = CGDisplayBounds(display);
    assert!(bounds.size.width > 0.0 && bounds.size.height > 0.0);
    let (tx, response) = mpsc::channel();
    let callback = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // SAFETY: Live nullable completion arguments are borrowed only during this callback;
            // all SCK objects stay on its thread. Only the scalar backing scale crosses threads.
            let scale = unsafe {
                assert!(error.is_null());
                content
                    .as_ref()
                    .expect("SCK content")
                    .displays()
                    .iter()
                    .find(|d| d.displayID() == display)
                    .map(|d| {
                        SCContentFilter::initWithDisplay_excludingWindows(
                            SCContentFilter::alloc(),
                            &d,
                            &NSArray::<SCWindow>::new(),
                        )
                        .pointPixelScale()
                    })
            };
            tx.send(scale).unwrap();
        },
    );
    // SAFETY: Documented completion signature; SCK copies the block for asynchronous use.
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&callback) };
    let scale = f64::from(
        response
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap(),
    );
    let gate = IoGate::new();
    gate.set_session_permits(true);
    gate.set_engine_permits(true);
    let mut capture = MacFrameCapture::new(gate).expect("Screen Recording granted");
    let (tx, rx) = mpsc::channel();
    let stream = capture
        .start(
            CaptureTarget::Display(DisplayId(display)),
            None,
            30,
            Arc::new(move |event| {
                let _ = tx.send(event);
            }),
        )
        .expect("main display capture");
    // Lead-run only: the WP explicitly permits pointer warping in this live test.
    assert_eq!(
        CGWarpMouseCursorPosition(NSPoint::new(bounds.origin.x + 10.0, bounds.origin.y + 10.0,)),
        CGError::Success
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    let (image, points) = loop {
        match rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("Some(cursor) within two seconds")
        {
            FrameEvent::Cursor {
                stream: received,
                cursor: Some(image),
            } => {
                assert_eq!(received, stream);
                let points = crosspane_platform_macos::main_thread::on_main(
                    Duration::from_millis(100),
                    |_| {
                        #[allow(deprecated)]
                        let cursor = NSCursor::currentSystemCursor();
                        eprintln!("GUI currentSystemCursor: {cursor:?}");
                        cursor
                            .expect("system cursor available in GUI")
                            .image()
                            .size()
                    },
                )
                .unwrap();
                let width = (points.width * scale).round();
                let height = (points.height * scale).round();
                let fit = (256.0 / width.max(height)).min(1.0);
                let expected =
                    PixelSize::new((width * fit).round() as u32, (height * fit).round() as u32);
                // A cursor queued before the warp can have the previous app's shape. Require
                // a sample matching the now-stationary pointer's source at the SCK density.
                if image.size == expected {
                    break (image, points);
                }
            }
            FrameEvent::Ended { reason, .. } => panic!("capture ended: {reason:?}"),
            _ => {}
        }
    };
    assert!(image.size.width > 0 && image.size.height > 0);
    assert!(image.size.width <= 256 && image.size.height <= 256);
    assert_eq!(
        image.pixels.len(),
        image.size.width as usize * image.size.height as usize * 4
    );
    assert!(image.hotspot.0 < image.size.width && image.hotspot.1 < image.size.height);
    assert!(
        image
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[3] == 255)
    );
    eprintln!(
        "GUI cursor: source={points:?}, scale={scale}, pixels={:?}, hotspot={:?}",
        image.size, image.hotspot
    );
    capture.stop(stream).unwrap();
    loop {
        if let FrameEvent::Ended {
            stream: received,
            reason,
        } = rx.recv_timeout(Duration::from_secs(2)).unwrap()
        {
            assert_eq!(received, stream);
            assert_eq!(reason, StreamEndReason::Requested);
            break;
        }
    }
    assert!(
        rx.recv_timeout(Duration::from_millis(250)).is_err(),
        "no events after Ended"
    );
    let dropping = Instant::now();
    drop(capture);
    assert!(
        dropping.elapsed() < Duration::from_secs(1),
        "cursor watcher stops within one second"
    );
}
