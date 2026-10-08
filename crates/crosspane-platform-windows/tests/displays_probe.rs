//! Limited, bounded geometry/clock probe. WGC admission precedes every HWND field read.
#![cfg(windows)]
#![allow(unsafe_code, clippy::unwrap_used, clippy::expect_used)]

use crosspane_platform::{
    CaptureTarget, Chord, Displays, FrameCapture, FrameEvent, GlobalHotkeys, HotkeyEvent, IoGate,
    StreamEndReason, WindowSource,
};
use crosspane_platform_windows::hotkey::WindowsHotkeys;
pub use crosspane_platform_windows::{clock, inject, model, twin};
use crosspane_types::hid::HidUsage;
use std::{
    mem::size_of,
    ptr::{null, null_mut},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    Security::*,
    System::{LibraryLoader::GetModuleHandleW, Threading::*},
    UI::{HiDpi::*, Input::*, WindowsAndMessaging::*},
};

#[path = "../src/cursor.rs"]
mod cursor;
#[path = "../src/displays.rs"]
mod displays;
#[cfg_attr(feature = "gpu", allow(dead_code))]
#[path = "../src/frame_capture.rs"]
mod frame_capture;
#[cfg(feature = "gpu")]
#[allow(dead_code)]
#[path = "../src/gpu.rs"]
mod gpu;
#[path = "../src/window.rs"]
mod window;

#[test]
fn included_fixture_adapter_keeps_the_shared_allocator_constructor() {
    let _constructor = window::WindowsWindowSource::new;
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn limited() {
    let mut token = null_mut();
    let mut elevation = TOKEN_ELEVATION::default();
    let mut bytes = 0;
    // SAFETY: read-only own process token query; exact output size and closed handle.
    unsafe {
        assert_ne!(
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token),
            0
        );
        let success = GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut bytes,
        );
        assert_ne!(CloseHandle(token), 0);
        assert_ne!(success, 0);
    }
    assert_eq!(
        elevation.TokenIsElevated, 0,
        "probe requires Limited win-gui route"
    );
}
struct Watchdog {
    done: mpsc::Sender<()>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Watchdog {
    fn new() -> Self {
        let (done, receive) = mpsc::channel();
        let thread = thread::spawn(move || {
            if receive.recv_timeout(Duration::from_secs(15)).is_err() {
                eprintln!("OWNED_DISPLAY_CLOCK watchdog expired; own process exits");
                std::process::exit(124);
            }
        });
        Self {
            done,
            thread: Some(thread),
        }
    }
}
impl Drop for Watchdog {
    fn drop(&mut self) {
        let _ = self.done.send(());
        if let Some(thread) = self.thread.take() {
            assert!(thread.join().is_ok());
        }
    }
}

struct ColourWindow {
    hwnd: HWND,
    class: Vec<u16>,
    instance: HINSTANCE,
    previous_dpi: DPI_AWARENESS_CONTEXT,
}
impl ColourWindow {
    fn new() -> Self {
        // SAFETY: only this owned fixture thread's context, restored during same-thread cleanup.
        let previous_dpi =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        assert!(!previous_dpi.is_null());
        // SAFETY: own process module borrowed, not a closable handle.
        let instance = unsafe { GetModuleHandleW(null()) };
        assert!(!instance.is_null());
        let class = wide(&format!(
            "Crosspane.DisplayClockFixture.{}",
            std::process::id()
        ));
        let info = WNDCLASSW {
            lpfnWndProc: Some(colour_proc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..Default::default()
        };
        // SAFETY: private class/static callback and owned valid UTF16 strings.
        assert_ne!(unsafe { RegisterClassW(&info) }, 0);
        let mut fixture = Self {
            hwnd: null_mut(),
            class,
            instance,
            previous_dpi,
        };
        // SAFETY: create and show ONLY our own known-colour fixture; no activation or input.
        unsafe {
            fixture.hwnd = CreateWindowExW(
                WS_EX_NOACTIVATE,
                fixture.class.as_ptr(),
                wide("owned colour clock fixture").as_ptr(),
                WS_OVERLAPPEDWINDOW,
                30,
                30,
                220,
                180,
                null_mut(),
                null_mut(),
                instance,
                null(),
            );
            assert!(!fixture.hwnd.is_null());
            ShowWindow(fixture.hwnd, SW_SHOWNOACTIVATE);
            UpdateWindow(fixture.hwnd);
        }
        fixture
    }
    fn identity(&self) -> model::window::Identity {
        let (mut created, mut exited, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        // SAFETY: only our own process/created HWND, authenticated by direct construction.
        unsafe {
            assert_ne!(
                GetProcessTimes(
                    GetCurrentProcess(),
                    &mut created,
                    &mut exited,
                    &mut kernel,
                    &mut user
                ),
                0
            );
            let mut pid = 0;
            let tid = GetWindowThreadProcessId(self.hwnd, &mut pid);
            assert_eq!(pid, GetCurrentProcessId());
            assert_eq!(tid, GetCurrentThreadId());
            model::window::Identity {
                hwnd: self.hwnd as usize as u64,
                pid,
                tid,
                process_created: (u64::from(created.dwHighDateTime) << 32)
                    | u64::from(created.dwLowDateTime),
            }
        }
    }
    fn pump(&self) {
        let mut message = MSG::default();
        // SAFETY: pump only the current fixture thread's queue; no input is posted.
        unsafe {
            while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
    fn close(&mut self) {
        // SAFETY: destroy/unregister only our own window/class on its owner thread.
        unsafe {
            if !self.hwnd.is_null() {
                assert_ne!(DestroyWindow(self.hwnd), 0);
                self.hwnd = null_mut();
            }
            if !self.class.is_empty() {
                assert_ne!(UnregisterClassW(self.class.as_ptr(), self.instance), 0);
                self.class.clear();
            }
            if !self.previous_dpi.is_null() {
                assert!(!SetThreadDpiAwarenessContext(self.previous_dpi).is_null());
                self.previous_dpi = null_mut();
            }
        }
    }
}
impl Drop for ColourWindow {
    fn drop(&mut self) {
        self.close();
    }
}

/// Keep the fixture's owner pumping while the source reads its same-process title.
/// Main-thread failure still requests same-thread HWND/context cleanup; the process
/// watchdog independently bounds any native call that cannot return.
struct ColourFixture {
    identity: model::window::Identity,
    stop: Arc<AtomicBool>,
    done: mpsc::Receiver<bool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl ColourFixture {
    fn new() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let (ready, identity) = mpsc::sync_channel(1);
        let (complete, done) = mpsc::sync_channel(1);
        let thread = thread::spawn(move || {
            let mut fixture = ColourWindow::new();
            let until = Instant::now() + Duration::from_secs(10);
            if ready.send(fixture.identity()).is_ok() {
                while !stopping.load(Ordering::Acquire) && Instant::now() < until {
                    fixture.pump();
                    thread::sleep(Duration::from_millis(5));
                }
            }
            fixture.close();
            let _ = complete.send(true);
        });
        Self {
            identity: identity.recv_timeout(Duration::from_secs(2)).unwrap(),
            stop,
            done,
            thread: Some(thread),
        }
    }
    fn close(&mut self) -> bool {
        self.stop.store(true, Ordering::Release);
        if self.thread.is_none() {
            return true;
        }
        if self.done.recv_timeout(Duration::from_secs(2)) != Ok(true) {
            eprintln!("OWNED_DISPLAY_CLOCK fixture cleanup unverified");
            return false;
        }
        self.thread
            .take()
            .is_some_and(|thread| thread.join().is_ok())
    }
}
impl Drop for ColourFixture {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
// SAFETY: callback is registered ONLY on the colour fixture created by this test.
unsafe extern "system" fn colour_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // SAFETY: paint only our own client/DC, balancing Begin/EndPaint and deleting own brush.
    unsafe {
        if message == WM_PAINT {
            let mut paint = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut paint);
            let mut bounds = RECT::default();
            GetClientRect(hwnd, &mut bounds);
            let brush = CreateSolidBrush(0x00339966);
            FillRect(dc, &bounds, brush);
            DeleteObject(brush);
            EndPaint(hwnd, &paint);
            return 0;
        }
        DefWindowProcW(hwnd, message, w, l)
    }
}

#[test]
#[ignore = "Limited owned colour fixture; only explicit win-gui opt-in"]
fn limited_monitor_and_shared_epoch_probe() {
    assert_eq!(
        std::env::var("CROSSPANE_WINDOWS_DISPLAYS_PROBE").as_deref(),
        Ok("1")
    );
    limited();
    let _watchdog = Watchdog::new();
    let mut displays = displays::WindowsDisplays::new().unwrap();
    let snapshot = displays.snapshot().unwrap();
    assert!(!snapshot.probes.is_empty());
    for (display, probe) in snapshot.displays.iter().zip(&snapshot.probes) {
        println!(
            "OWNED_DISPLAY id={} rect={:?} dpi={}",
            display.id.0, probe.rc_monitor, probe.dpi
        );
        assert!(snapshot.monitors.contains_key(&display.id));
    }
    let (send, events) = mpsc::channel();
    displays
        .subscribe(Arc::new(move |event| {
            let _ = send.send(event);
        }))
        .unwrap();
    assert!(
        events.recv_timeout(Duration::from_secs(2)).unwrap() == displays.displays().unwrap(),
        "monitor snapshot changed during initial delivery"
    );
    let fresh = displays.monitor_refresh()().unwrap();
    assert!(
        fresh.0 == snapshot.probes,
        "native refresh changed during probe"
    );
    for display in &snapshot.displays {
        assert_eq!(
            displays.monitor(display.id).unwrap(),
            snapshot.monitors[&display.id]
        );
    }

    let mut hotkeys = WindowsHotkeys::new().unwrap();
    hotkeys
        .set_chord(&Chord {
            modifiers: vec![HidUsage::keyboard(0xe0), HidUsage::keyboard(0xe2)],
            key: HidUsage::keyboard(0x29),
        })
        .unwrap();
    let (send, events) = mpsc::channel();
    let before_hotkey = clock::now();
    hotkeys
        .subscribe(Arc::new(move |event| {
            let _ = send.send(event);
        }))
        .unwrap();
    let event = events.recv_timeout(Duration::from_secs(2)).unwrap();
    let after_hotkey = clock::now();
    let at = match event {
        HotkeyEvent::Pressed { at } | HotkeyEvent::Released { at } => at,
    };
    assert!(before_hotkey <= at && at <= after_hotkey);
    println!(
        "SHARED_EPOCH hotkey_delta_ns={} state=initial-only",
        after_hotkey.as_nanos() - at.as_nanos()
    );
    drop(hotkeys);
    // SAFETY: read-only process registration inventory, proving the backend removed its own keyboard registration.
    let mut count = 0;
    assert_eq!(
        // SAFETY: read-only process registration count, no device/window fields.
        unsafe {
            GetRegisteredRawInputDevices(null_mut(), &mut count, size_of::<RAWINPUTDEVICE>() as u32)
        },
        0
    );
    assert_eq!(count, 0);

    let mut fixture = ColourFixture::new();
    let identity = fixture.identity;
    // Exact allowlist is installed before the included adapter can read any foreign fields.
    let source = window::WindowsWindowSource::for_fixture(
        displays.ids(),
        displays.monitor_reader(),
        identity,
    )
    .unwrap();
    let windows = source.windows().unwrap();
    assert_eq!(windows.len(), 1);
    let gate = IoGate::new();
    gate.set_engine_permits(true);
    gate.set_session_permits(true);
    let mut capture =
        frame_capture::WindowsFrameCapture::new(Arc::clone(&gate), source.resolver()).unwrap();
    let (send, frames) = mpsc::channel();
    let before_frame = clock::now();
    let stream = capture
        .start(
            CaptureTarget::Window(windows[0].id),
            None,
            10,
            Arc::new(move |event| {
                let _ = send.send(event);
            }),
        )
        .unwrap();
    let until = Instant::now() + Duration::from_secs(4);
    let frame = loop {
        match frames.recv_timeout(Duration::from_millis(10)) {
            Ok(FrameEvent::Frame { stream: id, frame }) if id == stream => break frame,
            Ok(FrameEvent::Ended { reason, .. }) => panic!("own fixture ended: {reason:?}"),
            _ => assert!(Instant::now() < until, "own fixture frame deadline"),
        }
    };
    let after_frame = clock::now();
    assert!(before_frame <= frame.at && frame.at <= after_frame);
    println!(
        "SHARED_EPOCH frame_delta_ns={} owned_frame_size={:?}",
        after_frame.as_nanos() - frame.at.as_nanos(),
        frame.size
    );
    drop(frame);
    capture.stop(stream).unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        match frames
            .recv_timeout(until.saturating_duration_since(Instant::now()))
            .unwrap()
        {
            FrameEvent::Ended { stream: id, reason } if id == stream => {
                assert_eq!(reason, StreamEndReason::Requested);
                break;
            }
            FrameEvent::Frame { stream: id, frame } if id == stream => drop(frame),
            _ => panic!("unexpected own capture stream"),
        }
    }
    assert!(frame_capture::WindowsFrameCapture::stop_verified(capture));
    assert!(window::WindowsWindowSource::stop_verified(source));
    let owned_hwnd = identity.hwnd as usize as HWND;
    assert!(fixture.close());
    // SAFETY: verify only our previously constructed HWND was destroyed.
    assert_eq!(unsafe { IsWindow(owned_hwnd) }, 0);
    assert!(displays::WindowsDisplays::stop_verified(displays));
    println!(
        "OWNED_DISPLAY_CLOCK cleanup=verified source/capture/fixture/observer/delivery/raw-keyboard; capture_source=shared-clock"
    );
}
