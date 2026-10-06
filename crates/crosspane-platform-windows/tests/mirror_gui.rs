//! Limited owned-process/window-only M1 probe. Never execute via elevated cargo.
#![cfg(windows)]
#![allow(unsafe_code, clippy::unwrap_used, clippy::expect_used)]
use crosspane_platform::{
    CaptureTarget, FrameCapture, FrameEvent, IoGate, WindowParking, WindowSource,
};
use crosspane_platform_windows::{
    displays::WindowsDisplays,
    frame_capture::WindowsFrameCapture,
    model, session,
    window::{self, OwnedProcessAllowlist, OwnedProcessClaim},
};
use crosspane_types::{geom::PixelSize, id::WindowId};
use model::parking::{Journal, NativeIdentity};
use parking::{MirrorJournalImages, MirrorJournalStore};
use std::{
    mem::size_of,
    ptr::{null, null_mut},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::{Dwm::*, Gdi::COLOR_WINDOW},
    Security::*,
    System::{LibraryLoader::GetModuleHandleW, Threading::*},
    UI::{HiDpi::*, WindowsAndMessaging::*},
};
#[path = "../src/parking.rs"]
mod parking;

fn limited() {
    let mut token = null_mut();
    let mut elevation = TOKEN_ELEVATION::default();
    let mut n = 0;
    // SAFETY: query this process's own token only, close its sole query handle.
    unsafe {
        assert_ne!(
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token),
            0
        );
        let okay = GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut n,
        );
        assert_ne!(CloseHandle(token), 0);
        assert_ne!(okay, 0);
    }
    assert_eq!(
        elevation.TokenIsElevated, 0,
        "Limited win-gui route required"
    );
}
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
unsafe extern "system" fn procedure(window: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // SAFETY: callback belongs exclusively to our registered fixture class.
    unsafe { DefWindowProcW(window, message, w, l) }
}
struct PhysicalScope(DPI_AWARENESS_CONTEXT);
impl PhysicalScope {
    fn new() -> Self {
        // SAFETY: affects this harness thread only; retained prior context is restored by Drop.
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        assert!(!previous.is_null());
        Self(previous)
    }
}
impl Drop for PhysicalScope {
    fn drop(&mut self) {
        // SAFETY: restore this same thread's retained context even while assertion unwinding.
        assert!(!unsafe { SetThreadDpiAwarenessContext(self.0) }.is_null());
    }
}
// Resources stay on the GUI thread. Every cleanup stage is attempted even if one API refuses.
struct FixtureWindow {
    class: Vec<u16>,
    module: HINSTANCE,
    window: HWND,
}
impl FixtureWindow {
    fn finish(&mut self) -> bool {
        // SAFETY: only handles created by this same fixture thread; never enumerate or kill others.
        let destroyed = self.window.is_null() || unsafe { DestroyWindow(self.window) } != 0;
        if destroyed {
            self.window = null_mut();
        }
        // SAFETY: private registered fixture class, retained name/module until native removal.
        let removed = self.class.is_empty()
            || unsafe { UnregisterClassW(self.class.as_ptr(), self.module) } != 0;
        if removed {
            self.class.clear();
        }
        destroyed && removed
    }
}
impl Drop for FixtureWindow {
    fn drop(&mut self) {
        if !self.finish() {
            eprintln!("OWNED_M1 native fixture cleanup incomplete");
        }
    }
}
struct Fixture {
    identity: NativeIdentity,
    stop: mpsc::Sender<()>,
    done: mpsc::Receiver<bool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Fixture {
    fn new() -> Self {
        Self::create(false)
    }
    fn passive_marker() -> Self {
        Self::create(true)
    }
    fn create(passive: bool) -> Self {
        let (ready, initialized) = mpsc::sync_channel(1);
        let (stop, receive) = mpsc::channel();
        let (finished, done) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let title = wide("CrosspaneOwnedM1");
            // SAFETY: PMv2 affects only this fixture thread; all resources created here are owned.
            unsafe {
                let previous =
                    SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
                assert!(!previous.is_null());
                let module = GetModuleHandleW(null());
                let class = wide(&format!(
                    "Crosspane.OwnedM1.{}.{}",
                    GetCurrentProcessId(),
                    GetCurrentThreadId()
                ));
                let definition = WNDCLASSW {
                    lpfnWndProc: Some(procedure),
                    hInstance: module,
                    lpszClassName: class.as_ptr(),
                    hbrBackground: (COLOR_WINDOW + 1) as usize as _,
                    ..Default::default()
                };
                assert_ne!(RegisterClassW(&definition), 0);
                let mut resources = FixtureWindow {
                    class,
                    module,
                    window: null_mut(),
                };
                let window = CreateWindowExW(
                    if passive { WS_EX_NOACTIVATE } else { 0 },
                    resources.class.as_ptr(),
                    title.as_ptr(),
                    WS_OVERLAPPEDWINDOW | if passive { 0 } else { WS_VISIBLE },
                    100,
                    100,
                    416,
                    338,
                    null_mut(),
                    null_mut(),
                    module,
                    null(),
                );
                resources.window = window;
                assert!(!window.is_null());
                if passive {
                    ShowWindow(window, SW_SHOWNOACTIVATE);
                }
                let mut created = FILETIME::default();
                let mut exit = FILETIME::default();
                let mut kernel = FILETIME::default();
                let mut user = FILETIME::default();
                assert_ne!(
                    GetProcessTimes(
                        GetCurrentProcess(),
                        &mut created,
                        &mut exit,
                        &mut kernel,
                        &mut user
                    ),
                    0
                );
                ready
                    .send(NativeIdentity {
                        hwnd: window as usize as u64,
                        pid: GetCurrentProcessId(),
                        tid: GetCurrentThreadId(),
                        process_created: (u64::from(created.dwHighDateTime) << 32)
                            | u64::from(created.dwLowDateTime),
                    })
                    .unwrap();
                let mut message = MSG::default();
                while receive.try_recv().is_err() {
                    while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                        TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                let removed = resources.finish();
                let context_restored = !SetThreadDpiAwarenessContext(previous).is_null();
                let okay = removed && context_restored;
                let _ = finished.send(okay);
            }
        });
        let identity = match initialized.recv_timeout(Duration::from_secs(3)) {
            Ok(identity) => identity,
            Err(error) => {
                let _ = stop.send(());
                if done.recv_timeout(Duration::from_secs(3)).is_ok() {
                    let _ = worker.join();
                }
                panic!("owned fixture initialization refused: {error}");
            }
        };
        Self {
            identity,
            stop,
            done,
            worker: Some(worker),
        }
    }
    fn hwnd(&self) -> HWND {
        self.identity.hwnd as usize as HWND
    }
    fn facts(&self) -> ([i32; 4], [i32; 4], bool) {
        let _physical = PhysicalScope::new();
        let mut pid = 0;
        let mut outer = RECT::default();
        let mut visible = RECT::default();
        // SAFETY: first revalidate own retained process creation and exact fixture HWND/PID/TID.
        unsafe {
            assert_eq!(
                GetWindowThreadProcessId(self.hwnd(), &mut pid),
                self.identity.tid
            );
            assert_eq!(pid, self.identity.pid);
            let mut created = FILETIME::default();
            let mut exit = FILETIME::default();
            let mut kernel = FILETIME::default();
            let mut user = FILETIME::default();
            assert_ne!(
                GetProcessTimes(
                    GetCurrentProcess(),
                    &mut created,
                    &mut exit,
                    &mut kernel,
                    &mut user
                ),
                0
            );
            assert_eq!(
                (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime),
                self.identity.process_created
            );
            assert_ne!(GetWindowRect(self.hwnd(), &mut outer), 0);
            assert!(
                DwmGetWindowAttribute(
                    self.hwnd(),
                    DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                    (&mut visible as *mut RECT).cast(),
                    size_of::<RECT>() as u32
                ) >= 0
            );
            (
                [outer.left, outer.top, outer.right, outer.bottom],
                [visible.left, visible.top, visible.right, visible.bottom],
                IsWindowVisible(self.hwnd()) != 0,
            )
        }
    }
    fn finish(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = self.stop.send(());
            assert!(self.done.recv_timeout(Duration::from_secs(3)).unwrap());
            worker.join().unwrap();
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = self.stop.send(());
            if self.done.recv_timeout(Duration::from_secs(3)).is_ok() {
                let _ = worker.join();
            } else {
                eprintln!("OWNED_M1 fixture cleanup incomplete");
            }
        }
    }
}
#[derive(Clone, Default)]
struct Store(Arc<Mutex<MirrorJournalImages>>);
impl MirrorJournalStore for Store {
    fn read(&mut self) -> Result<MirrorJournalImages, crosspane_platform::PlatformError> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn commit(&mut self, b: &[u8]) -> Result<(), crosspane_platform::PlatformError> {
        let mut s = self.0.lock().unwrap();
        s.pending = Some(b.to_vec());
        s.committed = Some(b.to_vec());
        Ok(())
    }
}
fn claim(identity: NativeIdentity) -> OwnedProcessClaim {
    OwnedProcessClaim {
        pid: identity.pid,
        process_created: identity.process_created,
        executable: std::env::current_exe().unwrap(),
    }
}
fn source(displays: &WindowsDisplays, identity: NativeIdentity) -> window::WindowsWindowSource {
    window::WindowsWindowSource::new_restricted(
        displays.ids(),
        displays.monitor_reader(),
        OwnedProcessAllowlist::admit(vec![claim(identity)]).unwrap(),
    )
    .unwrap()
}
fn target(source: &window::WindowsWindowSource) -> WindowId {
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        let found: Vec<_> = source
            .windows()
            .unwrap()
            .into_iter()
            .filter(|w| w.title == "CrosspaneOwnedM1")
            .collect();
        if found.len() == 1 {
            return found[0].id;
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(10));
    }
}
fn capture_size(capture: &mut WindowsFrameCapture, id: WindowId, w: u32, h: u32) {
    let (send, receive) = mpsc::channel();
    let stream = capture
        .start(
            CaptureTarget::Window(id),
            None,
            10,
            Arc::new(move |e| {
                let _ = send.send(e);
            }),
        )
        .unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    let mut matched = false;
    while Instant::now() < until {
        match receive
            .recv_timeout(until.saturating_duration_since(Instant::now()))
            .unwrap()
        {
            FrameEvent::Frame { stream: s, frame } if s == stream => {
                if frame.size == PixelSize::new(w, h) {
                    matched = true;
                    break;
                }
            }
            FrameEvent::Ended { stream: s, .. } if s == stream => panic!("owned WGC ended"),
            _ => {}
        }
    }
    capture.stop(stream).unwrap();
    assert!(matched, "physical visible/WGC frame size mismatch");
}
#[test]
#[ignore = "Limited owned fixture M1/recovery/WGC only; explicit win-gui opt-in"]
fn limited_owned_mirror_recovery_and_wgc_geometry() {
    assert_eq!(
        std::env::var("CROSSPANE_WINDOWS_MIRROR_PROBE").as_deref(),
        Ok("1")
    );
    limited();
    let (cancel, deadline) = mpsc::channel();
    thread::spawn(move || {
        if deadline.recv_timeout(Duration::from_secs(35)).is_err() {
            eprintln!("OWNED_M1 watchdog expired");
            std::process::exit(124);
        }
    });
    // No source/capture/hook/winit precedes the initial native recovery stage.
    let store = Store::default();
    let mut parking = parking::WindowsMirrorParking::new(Box::new(store.clone())).unwrap();
    assert_eq!(parking.recover_startup().unwrap().pending, 0);
    let mut fixture = Fixture::new();
    let original = fixture.facts();
    assert!(original.2);
    let displays = WindowsDisplays::new().unwrap();
    let source = source(&displays, fixture.identity);
    let id = target(&source);
    let resolver = source.resolver();
    parking
        .bind_source(resolver.clone(), displays.ids(), displays.monitor_reader())
        .unwrap();
    let w = (original.1[2] - original.1[0]) as u32;
    let h = (original.1[3] - original.1[1]) as u32;
    let parked = parking.park(id, PixelSize::new(w, h), 1.75).unwrap();
    assert_eq!(fixture.facts(), original);
    assert_eq!(
        (parked.content.width(), parked.content.height()),
        (w as i32, h as i32)
    );
    let gate = IoGate::new();
    gate.set_engine_permits(true);
    gate.set_session_permits(true);
    let mut capture = WindowsFrameCapture::new(gate, source.resolver()).unwrap();
    capture_size(&mut capture, id, w, h);
    let resized = parking.resize(id, PixelSize::new(500, 360), 2.5).unwrap();
    let resized_facts = fixture.facts();
    assert!(resized_facts.2);
    assert_eq!(resized_facts.1[..2], original.1[..2]);
    assert_eq!(
        (resized.content.width(), resized.content.height()),
        (
            resized_facts.1[2] - resized_facts.1[0],
            resized_facts.1[3] - resized_facts.1[1]
        )
    );
    capture_size(
        &mut capture,
        id,
        resized.content.width() as u32,
        resized.content.height() as u32,
    );
    parking.restore(id).unwrap();
    assert_eq!(fixture.facts(), original);
    assert!(
        Journal::load(&store.0.lock().unwrap())
            .unwrap()
            .0
            .entries()
            .is_empty()
    );
    parking.park(id, PixelSize::new(w, h), 1.0).unwrap();
    parking.resize(id, PixelSize::new(470, 330), 1.0).unwrap();
    drop(capture);
    drop(parking);
    drop(source);
    assert!(resolver.resolve(id).is_none());
    drop(resolver);
    drop(displays);
    // Simulate crash journal handoff without fabricating a new opaque WindowId. No source or
    // capture is live during either failure or the subsequent exact native-tuple recovery.
    let before = store.0.lock().unwrap().clone();
    let mut recovered = parking::WindowsMirrorParking::new(Box::new(store.clone())).unwrap();
    parking::fixture_dpi_failure();
    assert!(recovered.recover_startup().is_err());
    assert_eq!(store.0.lock().unwrap().committed, before.committed);
    drop(recovered);
    let proof_before = parking::fixture_dpi_proof();
    let mut recovered = parking::WindowsMirrorParking::new(Box::new(store.clone())).unwrap();
    let report = recovered.recover_startup().unwrap();
    let proof_after = parking::fixture_dpi_proof();
    assert_eq!((report.restored, report.retired, report.pending), (1, 0, 0));
    assert!(
        proof_after.0 > proof_before.0
            && proof_after.1 > proof_before.1
            && proof_after.2 > proof_before.2
    );
    assert_eq!(fixture.facts(), original);
    assert!(
        Journal::load(&store.0.lock().unwrap())
            .unwrap()
            .0
            .entries()
            .is_empty()
    );
    drop(recovered);
    let mut wrong = claim(fixture.identity);
    wrong.process_created += 1;
    assert!(OwnedProcessAllowlist::admit(vec![wrong]).is_err());
    fixture.finish();
    cancel.send(()).unwrap();
    println!(
        "OWNED_M1 park_visible_in_place=true resize_actual=true destination_dpi_not_rescaled=true decorated_wgc_size_matches=true restore_original=true journal_empty=true prehost_queries_and_mutation_pmv2=true prior_thread_context_restored=true context_failure_retained=true source_drop_refused=true creation_mismatch_refused=true owned_cleanup=true"
    );
}

/// The parking facade supplies this exact owned marker handle. Retain its thread/process tuple
/// before querying any of its metadata; no source title/class/content or foreign fields are read.
struct MarkerClaim {
    window: u64,
    tid: u32,
    created: u64,
}
impl MarkerClaim {
    fn new(window: u64, source: NativeIdentity) -> Self {
        assert_ne!(window, 0);
        let mut pid = 0;
        // SAFETY: facade returned an own marker; admission reads only its PID/TID first.
        let tid = unsafe { GetWindowThreadProcessId(window as usize as HWND, &mut pid) };
        assert_eq!(pid, source.pid);
        assert_ne!(tid, 0);
        assert_ne!(tid, source.tid);
        let claim = Self {
            window,
            tid,
            created: source.process_created,
        };
        assert!(claim.alive());
        claim
    }
    fn hwnd(&self) -> HWND {
        self.window as usize as HWND
    }
    fn alive(&self) -> bool {
        let mut pid = 0;
        let mut created = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        // SAFETY: exact own marker admission precedes fields; query only this process's creation.
        unsafe {
            GetWindowThreadProcessId(self.hwnd(), &mut pid) == self.tid
                && pid == GetCurrentProcessId()
                && GetProcessTimes(
                    GetCurrentProcess(),
                    &mut created,
                    &mut exit,
                    &mut kernel,
                    &mut user,
                ) != 0
                && ((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
                    == self.created
        }
    }
    fn visible(&self) -> bool {
        if !self.alive() {
            return false;
        }
        // SAFETY: metadata only of this freshly admitted own marker.
        unsafe { IsWindowVisible(self.hwnd()) != 0 }
    }
    fn matches(&self, fixture: &Fixture) -> bool {
        let _physical = PhysicalScope::new();
        if !self.alive() {
            return false;
        }
        let expected = fixture.facts().1;
        let mut actual = RECT::default();
        // SAFETY: own marker and freshly admitted fixture only; adjacency returns a handle only.
        unsafe {
            let style = GetWindowLongPtrW(self.hwnd(), GWL_EXSTYLE) as u32;
            let required = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
            AreDpiAwarenessContextsEqual(
                GetWindowDpiAwarenessContext(self.hwnd()),
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            ) != 0
                && GetWindowRect(self.hwnd(), &mut actual) != 0
                && [actual.left, actual.top, actual.right, actual.bottom] == expected
                && style & required == required
                && (style & WS_EX_TOPMOST != 0)
                    == (GetWindowLongPtrW(fixture.hwnd(), GWL_EXSTYLE) as u32 & WS_EX_TOPMOST != 0)
                && GetWindowLongPtrW(self.hwnd(), GWL_STYLE) as u32 & WS_POPUP != 0
                && IsWindowVisible(self.hwnd()) != 0
                && GetWindow(fixture.hwnd(), GW_HWNDPREV) == self.hwnd()
                && GetForegroundWindow() != self.hwnd()
        }
    }
}
fn marker_wait(mut predicate: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        if predicate() {
            return;
        }
        assert!(Instant::now() < until, "owned marker metadata deadline");
        thread::sleep(Duration::from_millis(10));
    }
}
fn marker_target(source: &window::WindowsWindowSource, identity: NativeIdentity) -> WindowId {
    let resolver = source.resolver();
    let mut target = None;
    marker_wait(|| {
        let found: Vec<_> = source
            .windows()
            .unwrap()
            .into_iter()
            .filter(|w| {
                resolver.resolve(w.id).is_some_and(|n| {
                    (n.hwnd, n.pid, n.tid, n.process_created)
                        == (
                            identity.hwnd,
                            identity.pid,
                            identity.tid,
                            identity.process_created,
                        )
                })
            })
            .map(|w| w.id)
            .collect();
        if found.len() == 1 {
            target = Some(found[0]);
            true
        } else {
            false
        }
    });
    target.unwrap()
}
fn marker_journal_empty(store: &Store) {
    let images = store.0.lock().unwrap().clone();
    assert!(images.pending.is_some() && images.committed.is_some());
    assert_eq!(images.pending, images.committed);
    let (journal, interrupted) = Journal::load(&images).unwrap();
    assert!(!interrupted && journal.entries().is_empty());
}
#[test]
#[ignore = "Limited owned marker metadata only; no capture/socket/input; explicit win-gui opt-in"]
fn limited_owned_mirror_marker_metadata() {
    assert_eq!(
        std::env::var("CROSSPANE_WINDOWS_MARKER_PROBE").as_deref(),
        Ok("1")
    );
    limited();
    let (cancel, deadline) = mpsc::channel();
    thread::spawn(move || {
        if matches!(
            deadline.recv_timeout(Duration::from_secs(35)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ) {
            eprintln!("OWNED_MARKER watchdog expired");
            std::process::exit(124);
        }
    });
    // Process-private canonical journal images; W3.2a already verified filesystem durability.
    // Recovery remains before any native source or the lazy runtime marker/session hooks.
    let store = Store::default();
    let mut parking = parking::WindowsMirrorParking::new(Box::new(store.clone())).unwrap();
    assert_eq!(parking.recover_startup().unwrap().pending, 0);
    let mut fixture = Fixture::passive_marker();
    let original = fixture.facts();
    assert!(original.2);
    let displays = WindowsDisplays::new().unwrap();
    let source = source(&displays, fixture.identity);
    let id = marker_target(&source, fixture.identity);
    parking
        .bind_source(source.resolver(), displays.ids(), displays.monitor_reader())
        .unwrap();
    let size = PixelSize::new(
        (original.1[2] - original.1[0]) as u32,
        (original.1[3] - original.1[1]) as u32,
    );
    parking.park(id, size, 1.0).unwrap();
    let marker = MarkerClaim::new(parking.fixture_marker(id).unwrap(), fixture.identity);
    marker_wait(|| marker.matches(&fixture));
    // Each operation targets this exact already admitted fixture HWND; passive flags preserve
    // owner focus/order. No pointer/keyboard injection, screenshots, WGC or sockets are used.
    fixture.facts();
    assert_ne!(
        // SAFETY: only owned fixture geometry, no source z-order or focus change.
        unsafe {
            SetWindowPos(
                fixture.hwnd(),
                null_mut(),
                140,
                120,
                500,
                380,
                SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOOWNERZORDER,
            )
        },
        0
    );
    marker_wait(|| marker.matches(&fixture));
    fixture.facts();
    // SAFETY: minimise only this owned passive fixture without activation.
    unsafe {
        ShowWindow(fixture.hwnd(), SW_SHOWMINNOACTIVE);
    }
    marker_wait(|| !marker.visible());
    fixture.facts();
    // SAFETY: restore only this owned passive fixture, explicitly without activation.
    unsafe {
        ShowWindow(fixture.hwnd(), SW_SHOWNOACTIVATE);
    }
    marker_wait(|| marker.matches(&fixture));
    fixture.facts();
    // SAFETY: hide only this owned fixture.
    unsafe {
        ShowWindow(fixture.hwnd(), SW_HIDE);
    }
    marker_wait(|| !marker.visible());
    fixture.facts();
    // SAFETY: show only this owned passive fixture without activation.
    unsafe {
        ShowWindow(fixture.hwnd(), SW_SHOWNOACTIVATE);
    }
    marker_wait(|| marker.matches(&fixture));
    parking.restore(id).unwrap();
    marker_wait(|| !marker.alive());
    assert_eq!(parking.fixture_marker(id).unwrap(), 0);
    assert_eq!(fixture.facts(), original);
    marker_journal_empty(&store);
    // Re-park is real M1 with a fresh marker; repeated restore remains idempotent.
    parking.park(id, size, 1.0).unwrap();
    let second = MarkerClaim::new(parking.fixture_marker(id).unwrap(), fixture.identity);
    marker_wait(|| second.matches(&fixture));
    parking.restore(id).unwrap();
    marker_wait(|| !second.alive());
    parking.restore(id).unwrap();
    marker_journal_empty(&store);
    drop(parking);
    drop(source);
    drop(displays);
    fixture.finish();
    cancel.send(()).unwrap();
    println!(
        "OWNED_MARKER park=true move_resize=true minimized_hidden=true restored_shown=true hidden_hidden=true shown_shown=true rect_exact=true adjacency_exact=true marker_pmv2=true passive_styles=true source_non_topmost_band=true marker_removed=true repark=true journal_both_empty=true owned_cleanup=true capture=false sockets=false input=false"
    );
}
