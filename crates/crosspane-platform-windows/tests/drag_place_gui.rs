//! Limited owned-fixture placed-return probe (WP-W1.6b T4, DRAG-v0 D-6). It creates, moves and
//! destroys only its own fixture window and never reads or moves another window. Never execute via
//! elevated cargo.
#![cfg(windows)]
#![allow(unsafe_code, clippy::unwrap_used, clippy::expect_used)]
use crosspane_platform::{PlatformError, WindowParking, WindowSource};
use crosspane_platform_windows::{
    displays::WindowsDisplays,
    drag::WindowsRestorePlacer,
    model::{
        drag::PlacedRestore,
        geometry::MonitorProbe,
        parking::{Journal, NativeIdentity},
    },
    parking::{MirrorJournalImages, MirrorJournalStore, WindowsMirrorParking},
    window::{self, OwnedProcessAllowlist, OwnedProcessClaim},
};
use crosspane_types::{
    geom::{PixelSize, PointDevice},
    id::{DisplayId, WindowId},
};
use std::{
    mem::size_of,
    ptr::{null, null_mut},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::{Dwm::*, Gdi::COLOR_WINDOW},
    Security::*,
    System::{LibraryLoader::GetModuleHandleW, Threading::*},
    UI::{HiDpi::*, Input::KeyboardAndMouse::GetAsyncKeyState, WindowsAndMessaging::*},
};

const TITLE: &str = "CrosspaneOwnedPlace";
const WATCHDOG: Duration = Duration::from_secs(30);
/// No monitor is assigned this id; the probe checks that before relying on it.
const UNKNOWN_DISPLAY: DisplayId = DisplayId(u32::MAX);

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
/// Aggregate key/button state of the common modifiers and buttons; never typed contents.
fn all_up() -> bool {
    // SAFETY: read-only aggregate key/button state. Zero is not independent positive hook proof;
    // this conservative status check can only refuse.
    unsafe {
        [1, 2, 4, 5, 6, 0x1b, 0x10, 0x11, 0x12, 0x5b, 0x5c]
            .into_iter()
            .all(|vk| GetAsyncKeyState(vk) >= 0)
    }
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
            eprintln!("OWNED_PLACE native fixture cleanup incomplete");
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
        let (ready, initialized) = mpsc::sync_channel(1);
        let (stop, receive) = mpsc::channel();
        let (finished, done) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let title = wide(TITLE);
            // SAFETY: PMv2 affects only this fixture thread; all resources created here are owned.
            unsafe {
                let previous =
                    SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
                assert!(!previous.is_null());
                let module = GetModuleHandleW(null());
                let class = wide(&format!(
                    "Crosspane.OwnedPlace.{}.{}",
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
                    0,
                    resources.class.as_ptr(),
                    title.as_ptr(),
                    WS_OVERLAPPEDWINDOW | WS_VISIBLE,
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
    /// Outer (`GetWindowRect`) rectangle, DWM extended-frame rectangle, and visibility.
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
    /// Consumes the fixture; the return value is whether every native resource was removed.
    fn finish(mut self) -> bool {
        let Some(worker) = self.worker.take() else {
            return false;
        };
        let _ = self.stop.send(());
        let received = self.done.recv_timeout(Duration::from_secs(3));
        let okay = matches!(received, Ok(true));
        if received.is_ok() {
            let _ = worker.join();
        }
        okay
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = self.stop.send(());
            if self.done.recv_timeout(Duration::from_secs(3)).is_ok() {
                let _ = worker.join();
            } else {
                eprintln!("OWNED_PLACE fixture cleanup incomplete");
            }
        }
    }
}
#[derive(Clone, Default)]
struct Store(Arc<Mutex<MirrorJournalImages>>);
impl MirrorJournalStore for Store {
    fn read(&mut self) -> Result<MirrorJournalImages, PlatformError> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn commit(&mut self, document: &[u8]) -> Result<(), PlatformError> {
        let mut images = self.0.lock().unwrap();
        images.pending = Some(document.to_vec());
        images.committed = Some(document.to_vec());
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
            .filter(|w| w.title == TITLE)
            .collect();
        if found.len() == 1 {
            return found[0].id;
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(10));
    }
}
/// `[width, height]` of a `[left, top, right, bottom]` rectangle.
fn size_of_rect(rect: [i32; 4]) -> (i32, i32) {
    (rect[2] - rect[0], rect[3] - rect[1])
}
/// The non-twin monitor holding the centre of `rect`. Its allocator id must be `display`.
fn monitor_at(displays: &WindowsDisplays, display: DisplayId, rect: [i32; 4]) -> MonitorProbe {
    let probes = (displays.monitor_reader())().unwrap();
    let (x, y) = ((rect[0] + rect[2]) / 2, (rect[1] + rect[3]) / 2);
    let mut holding = probes.iter().filter(|p| {
        !p.twin
            && (p.rc_monitor[0]..p.rc_monitor[2]).contains(&x)
            && (p.rc_monitor[1]..p.rc_monitor[3]).contains(&y)
    });
    let probe = holding
        .next()
        .expect("owned fixture centre is on a non-twin monitor")
        .clone();
    assert!(
        holding.next().is_none(),
        "owned fixture centre on one monitor"
    );
    let ids_shared = displays.ids();
    let mut ids = ids_shared.lock().unwrap();
    assert_eq!(ids.assign(&probe.device_path).ok(), Some(display));
    probe
}
/// True when no current monitor is assigned `display`.
fn unassigned(displays: &WindowsDisplays, display: DisplayId) -> bool {
    let probes = (displays.monitor_reader())().unwrap();
    let ids_shared = displays.ids();
    let mut ids = ids_shared.lock().unwrap();
    probes
        .iter()
        .all(|p| ids.assign(&p.device_path).ok() != Some(display))
}
fn journal_empty(store: &Store) -> bool {
    let images = store.0.lock().unwrap().clone();
    matches!(Journal::load(&images), Ok((journal, false)) if journal.entries().is_empty())
}

#[test]
#[ignore = "Limited owned fixture placed restore only; explicit win-gui opt-in"]
fn limited_owned_placed_restore() {
    if std::env::var("CROSSPANE_WINDOWS_PLACE_PROBE").as_deref() != Ok("1") {
        println!("SKIP limited_owned_placed_restore: CROSSPANE_WINDOWS_PLACE_PROBE is not 1");
        return;
    }
    limited();
    let (cancel, deadline) = mpsc::channel();
    thread::spawn(move || {
        if matches!(
            deadline.recv_timeout(WATCHDOG),
            Err(mpsc::RecvTimeoutError::Timeout)
        ) {
            eprintln!("OWNED_PLACE watchdog expired");
            std::process::exit(124);
        }
    });
    let held_before = all_up();
    // Startup recovery precedes the owned fixture, the source and every placement.
    let store = Store::default();
    let mut parking = WindowsMirrorParking::new(Box::new(store.clone())).unwrap();
    assert_eq!(parking.recover_startup().unwrap().pending, 0);
    let fixture = Fixture::new();
    let original = fixture.facts();
    assert!(original.2);
    let displays = WindowsDisplays::new().unwrap();
    let source = source(&displays, fixture.identity);
    let id = target(&source);
    let resolver = source.resolver();
    parking
        .bind_source(resolver.clone(), displays.ids(), displays.monitor_reader())
        .unwrap();
    let (w, h) = size_of_rect(original.1);
    let size = PixelSize::new(w as u32, h as u32);
    // Step 4: park in place, then resize the parked content to 500x360.
    let parked = parking.park(id, size, 1.0).unwrap();
    assert_eq!(fixture.facts(), original);
    assert_eq!((parked.content.width(), parked.content.height()), (w, h));
    let resized_parked = parking.resize(id, PixelSize::new(500, 360), 1.0).unwrap();
    let resized = fixture.facts();
    assert!(resized.2);
    assert_eq!(
        (
            resized_parked.content.width(),
            resized_parked.content.height()
        ),
        size_of_rect(resized.1)
    );
    assert_ne!(size_of_rect(resized.1), (w, h));
    // Step 5: the placed return wraps the same parking, with a recording report.
    let reports = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&reports);
    let mut placed = PlacedRestore::new(
        parking,
        WindowsRestorePlacer::new(resolver.clone(), displays.ids(), displays.monitor_reader()),
        Box::new(move |_: &PlatformError| {
            counter.fetch_add(1, Ordering::SeqCst);
        }),
    );
    // Step 6: restore, then place the top-left at work area + (37, 41).
    let monitor = monitor_at(&displays, parked.display, resized.1);
    let work = monitor.rc_work;
    let mon = monitor.rc_monitor;
    let expected = (work[0] + 37, work[1] + 41);
    let origin = PointDevice::new(
        f64::from(expected.0 - mon[0]),
        f64::from(expected.1 - mon[1]),
    );
    placed.restore_at(id, parked.display, origin).unwrap();
    let restored = fixture.facts();
    let placed_ok = restored.2
        && (restored.1[0], restored.1[1]) == expected
        && size_of_rect(restored.1) == (w, h);
    assert!(placed_ok, "placed restore top-left or original size");
    assert_eq!(reports.load(Ordering::SeqCst), 0);
    assert!(journal_empty(&store), "journal after placed restore");
    // Step 7: park again, then ask for (1e6, 1e6); the top-left clamps to work bottom-right.
    placed.park(id, size, 1.0).unwrap();
    placed
        .restore_at(id, parked.display, PointDevice::new(1e6, 1e6))
        .unwrap();
    let clamped_facts = fixture.facts();
    let corner = (work[2] - w, work[3] - h);
    let clamped_ok = clamped_facts.2
        && (clamped_facts.1[0], clamped_facts.1[1]) == corner
        && size_of_rect(clamped_facts.1) == (w, h);
    assert!(clamped_ok, "clamped restore at work bottom-right");
    assert_eq!(reports.load(Ordering::SeqCst), 0);
    assert!(journal_empty(&store), "journal after clamped restore");
    // Step 8: park again; an unknown display restores in place and reports the refusal once.
    let before_park = fixture.facts();
    placed.park(id, size, 1.0).unwrap();
    assert!(unassigned(&displays, UNKNOWN_DISPLAY));
    placed
        .restore_at(id, UNKNOWN_DISPLAY, PointDevice::new(0.0, 0.0))
        .unwrap();
    let after_unknown = fixture.facts();
    let kept_ok = after_unknown.2
        && after_unknown.0 == before_park.0
        && after_unknown.1 == before_park.1
        && reports.load(Ordering::SeqCst) == 1;
    assert!(kept_ok, "unknown display keeps the restored rectangle");
    let journal_final = journal_empty(&store);
    assert!(journal_final, "journal after unknown-display restore");
    // Step 9: no key or button was held before or after the owned placement.
    let held_up = held_before && all_up();
    assert!(held_up, "held input before or after the owned placement");
    drop(placed);
    drop(source);
    assert!(resolver.resolve(id).is_none());
    drop(resolver);
    drop(displays);
    let owned_cleanup = fixture.finish();
    assert!(owned_cleanup, "owned fixture cleanup");
    let _ = cancel.send(());
    let held_status = if held_up { "up" } else { "held" };
    println!(
        "OWNED_PLACE placed={placed_ok} clamped={clamped_ok} unknown_display_kept_restore={kept_ok} journal_empty={journal_final} held_status={held_status} owned_cleanup={owned_cleanup}"
    );
}
