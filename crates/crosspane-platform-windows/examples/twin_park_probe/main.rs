//! Windows M2 park probe (WP-W3.2). Modes: `fixture` runs the owned test window in a child
//! process; `store-selftest` round-trips the file journal store in a temp directory. The M2 rows
//! run only on the owned fixture window, under a Limited token and a 60 s watchdog. Each row prints
//! one `TWIN_PARK_ROW name=… ok=… ms=… <facts>` line:
//! - `absent` (P1): no driver installed; park is refused with the absent reason in under 100 ms.
//! - `park` (P2): twin placement, monitor capture, resizes, maximized refusal, then restore.
//! - `crash` (P3): park, record the fixture identity, then terminate this process (exit 137).
//! - `recover` (P4): startup recovery after `crash`, then close the fixture.
//! - `remode` (V3): three cycles of a twin re-mode (resize to 1.5, then 1.0) with one monitor stream
//!   kept live. The stream ends or restarts on the new twin, and the window is resolved throughout.
//! - `dpi_place` (V6): the fixture runs with `--dpi-resize` and is parked on a 1.5-scale twin. The
//!   placer then moves it to a real monitor, and the visible top-left is checked within 1 px.
//!
//! Non-Windows builds have no probe.
#![allow(unsafe_code)]

#[cfg(windows)]
mod fixture;
#[cfg(windows)]
mod store;

#[cfg(windows)]
mod rows {
    use super::{
        fixture::{FixtureChild, close_owned},
        store::FileStore,
    };
    use crosspane_platform::{
        CaptureTarget, FrameCapture, FrameEvent, IoGate, ParkingKind, PlatformError,
        StreamEndReason, StreamId, WindowParking, WindowSource,
    };
    use crosspane_platform_windows::{
        displays::WindowsDisplays,
        drag::WindowsRestorePlacer,
        frame_capture::WindowsFrameCapture,
        model::{
            drag::RestorePlacer,
            journal::{JOURNAL_NAME, JournalFile},
            parking::{Journal, NativeIdentity},
            twin::{TWIN_ABSENT_REASON, TWIN_UNAVAILABLE_REASON},
            twin_parking::TWIN_MAXIMIZED_REASON,
        },
        parking::MirrorJournalStore,
        twin::TwinClient,
        twin_parking::{
            TwinParkingRecovery, WINDOW_COMMITTED_NAME, WINDOW_PENDING_NAME, WindowsTwinParking,
        },
        window::{OwnedProcessAllowlist, OwnedProcessClaim, WindowResolver, WindowsWindowSource},
    };
    use crosspane_types::{
        geom::{PixelRect, PixelSize, PointDevice},
        id::{DisplayId, WindowId},
    };
    use std::{
        fmt, fs,
        io::Write,
        mem::size_of,
        path::{Path, PathBuf},
        process::ExitCode,
        ptr::null_mut,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread,
        time::{Duration, Instant},
    };
    use windows_sys::{
        Win32::{
            Foundation::{CloseHandle, HANDLE, HWND, LPARAM, RECT},
            Graphics::{
                Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
                Gdi::{
                    GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONULL, MONITORINFO,
                    MonitorFromWindow,
                },
            },
            Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation},
            System::Threading::{GetCurrentProcess, OpenProcessToken, TerminateProcess},
            UI::{
                HiDpi::{
                    DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                    GetDpiForMonitor, MDT_EFFECTIVE_DPI, SetThreadDpiAwarenessContext,
                },
                WindowsAndMessaging::{
                    EnumWindows, GetWindowRect, GetWindowThreadProcessId, IsWindow, IsWindowVisible,
                },
            },
        },
        core::BOOL,
    };

    /// The fixture's window title (`fixture.rs` TITLE). Only the admitted fixture is searched.
    const FIXTURE_TITLE: &str = "CrosspaneOwnedM2";
    /// The scratch directory: window journals, the twin ledger and the crash record.
    const SCRATCH: &str = "crosspane-twin-park-probe";
    /// The identity and original rect that `crash` leaves for `recover`.
    const FIXTURE_STATE: &str = "fixture.json";
    /// The exit code of the simulated crash.
    const CRASH_CODE: u32 = 137;
    /// A row that outlives this exits the probe with 124.
    const WATCHDOG: Duration = Duration::from_secs(60);
    /// The `remode` row's re-mode count: three cycles of a resize to 1.5 and then to 1.0.
    const REMODES: u32 = 6;
    /// How long a re-mode's old monitor stream may take to end before it counts as `None`.
    const OLD_END_WAIT: Duration = Duration::from_secs(3);
    /// How long a fresh monitor stream may take to deliver its first frame.
    const FIRST_FRAME_WAIT: Duration = Duration::from_secs(5);
    /// The `remode` row's window resolution period.
    const SAMPLE_PERIOD: Duration = Duration::from_millis(5);

    /// One `TWIN_PARK_ROW` line. `ms` is the row's timed operation; facts are `key=value` pairs.
    pub(super) struct Row {
        name: &'static str,
        ms: u128,
        facts: Vec<String>,
    }

    impl Row {
        fn new(name: &'static str) -> Self {
            Self {
                name,
                ms: 0,
                facts: Vec::new(),
            }
        }

        fn fact(&mut self, key: &str, value: impl fmt::Display) {
            self.facts.push(format!("{key}={value}"));
        }

        fn emit(&self, ok: bool, failure: Option<&str>) {
            let mut line = format!("TWIN_PARK_ROW name={} ok={ok} ms={}", self.name, self.ms);
            for fact in &self.facts {
                line.push(' ');
                line.push_str(fact);
            }
            if let Some(stage) = failure {
                line.push_str(&format!(" fail={stage:?}"));
            }
            println!("{line}");
            let _ = std::io::stdout().flush();
        }
    }

    /// Exits the probe with 124 if the row outlives [`WATCHDOG`]. Dropping the sender cancels it.
    fn watchdog(name: &'static str) -> mpsc::Sender<()> {
        let (cancel, expired) = mpsc::channel::<()>();
        thread::spawn(move || {
            if let Err(mpsc::RecvTimeoutError::Timeout) = expired.recv_timeout(WATCHDOG) {
                println!("TWIN_PARK_ROW name={name} ok=false ms=0 fail=\"watchdog\"");
                std::process::exit(124);
            }
        });
        cancel
    }

    /// Refuses an elevated or unreadable token before any row touches a window.
    fn limited() -> Result<(), String> {
        let mut token: HANDLE = null_mut();
        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0_u32;
        // SAFETY: opens this process's own token, reads it into exact local buffers, and closes it.
        unsafe {
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err("process token unavailable".to_owned());
            }
            let read = GetTokenInformation(
                token,
                TokenElevation,
                (&mut elevation as *mut TOKEN_ELEVATION).cast(),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut returned,
            ) != 0;
            let _ = CloseHandle(token);
            if !read {
                return Err("token elevation unreadable".to_owned());
            }
        }
        if elevation.TokenIsElevated != 0 {
            return Err("elevated token refused".to_owned());
        }
        Ok(())
    }

    /// Physical-pixel geometry for this thread, as the fixture uses. Drop restores the context.
    struct PhysicalScope(DPI_AWARENESS_CONTEXT);

    impl PhysicalScope {
        fn new() -> Result<Self, String> {
            // SAFETY: changes only this thread's DPI context; Drop restores the captured one.
            let previous =
                unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
            if previous.is_null() {
                return Err("PMv2 context unavailable".to_owned());
            }
            Ok(Self(previous))
        }
    }

    impl Drop for PhysicalScope {
        fn drop(&mut self) {
            // SAFETY: restores the context this same thread captured in `new`.
            unsafe { SetThreadDpiAwarenessContext(self.0) };
        }
    }

    /// Runs one row on the main thread under physical geometry, then prints its line. A failing
    /// row names its stage and exits 1.
    pub(super) fn run(name: &'static str, body: fn(&mut Row) -> Result<(), String>) -> ExitCode {
        let mut row = Row::new(name);
        let _cancel = watchdog(name);
        let outcome = PhysicalScope::new().and_then(|_physical| {
            limited()?;
            body(&mut row)
        });
        match outcome {
            Ok(()) => {
                row.emit(true, None);
                ExitCode::SUCCESS
            }
            Err(stage) => {
                row.emit(false, Some(stage.as_str()));
                ExitCode::FAILURE
            }
        }
    }

    fn at<T, E: fmt::Display>(result: Result<T, E>, stage: &str) -> Result<T, String> {
        result.map_err(|error| format!("{stage}: {error}"))
    }

    fn ensure(ok: bool, stage: &str) -> Result<(), String> {
        if ok { Ok(()) } else { Err(stage.to_owned()) }
    }

    /// The scratch directory, created if missing. Rows reuse it, so `crash` and `recover` share it.
    fn scratch() -> Result<PathBuf, String> {
        let dir = std::env::temp_dir().join(SCRATCH);
        at(fs::create_dir_all(&dir), "scratch directory")?;
        Ok(dir)
    }

    /// The M2 facade over the window journal, with the twin ledger beside it. Twins are enabled.
    fn facade(dir: &Path) -> Result<WindowsTwinParking, String> {
        let store = FileStore::new(
            dir.to_path_buf(),
            WINDOW_COMMITTED_NAME,
            WINDOW_PENDING_NAME,
        );
        at(
            WindowsTwinParking::new(Box::new(store), dir.join(JOURNAL_NAME), true),
            "twin parking",
        )
    }

    /// Both journals hold no entries and no twins. A missing file counts as empty.
    fn journals_empty(dir: &Path) -> Result<bool, String> {
        let mut store = FileStore::new(
            dir.to_path_buf(),
            WINDOW_COMMITTED_NAME,
            WINDOW_PENDING_NAME,
        );
        let images = at(store.read(), "window journal read")?;
        let (window, _pending) = at(Journal::load(&images), "window journal")?;
        let ledger = at(JournalFile::open(&dir.join(JOURNAL_NAME)), "twin ledger")?;
        Ok(window.entries().is_empty()
            && ledger.twins().count() == 0
            && ledger.entries().count() == 0)
    }

    /// Our own active display paths right now, read through a fresh client on the driver.
    fn own_paths() -> Result<usize, String> {
        let mut client = at(TwinClient::open(), "twin client")?;
        at(client.own_path_count(), "own display paths")
    }

    /// Admits the fixture process by its identity, so the source sees only its windows.
    fn admit(identity: &NativeIdentity) -> Result<OwnedProcessAllowlist, String> {
        let claim = OwnedProcessClaim {
            pid: identity.pid,
            process_created: identity.process_created,
            executable: at(std::env::current_exe(), "probe executable")?,
        };
        at(
            OwnedProcessAllowlist::admit(vec![claim]),
            "fixture admission",
        )
    }

    fn source_for(
        displays: &WindowsDisplays,
        identity: &NativeIdentity,
    ) -> Result<WindowsWindowSource, String> {
        let allowlist = admit(identity)?;
        at(
            WindowsWindowSource::new_restricted(
                displays.ids(),
                displays.monitor_reader(),
                allowlist,
            ),
            "window source",
        )
    }

    /// The one window of the admitted fixture. Titles are matched here and never printed.
    fn target(source: &WindowsWindowSource) -> Result<WindowId, String> {
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            let found: Vec<WindowId> = at(source.windows(), "window list")?
                .iter()
                .filter(|window| window.title == FIXTURE_TITLE)
                .map(|window| window.id)
                .collect();
            if let [only] = found.as_slice() {
                return Ok(*only);
            }
            if Instant::now() >= until {
                return Err(format!("fixture window listed {} times", found.len()));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Everything a parking row starts from. Fields drop in declaration order, so the facade goes
    /// first and its twin lane closes while the fixture is still alive.
    struct Setup {
        parking: WindowsTwinParking,
        displays: WindowsDisplays,
        source: WindowsWindowSource,
        fixture: FixtureChild,
        window: WindowId,
        original: [i32; 4],
        startup: TwinParkingRecovery,
    }

    /// Startup recovery first, then the owned fixture, its admitted source and the bound facade.
    fn prepare(dir: &Path, row: &mut Row) -> Result<Setup, String> {
        prepare_with(dir, row, FixtureChild::spawn)
    }

    /// [`prepare`], with the fixture child started by `spawn`.
    fn prepare_with(
        dir: &Path,
        row: &mut Row,
        spawn: fn() -> Result<FixtureChild, PlatformError>,
    ) -> Result<Setup, String> {
        let mut parking = facade(dir)?;
        let startup = at(parking.recover_startup(), "startup recovery")?;
        row.fact("driver_present", startup.driver_present);
        let mut fixture = at(spawn(), "fixture spawn")?;
        let original = at(fixture.rect(), "fixture rect")?;
        let displays = at(WindowsDisplays::new(), "displays")?;
        let source = source_for(&displays, &fixture.identity)?;
        let window = target(&source)?;
        at(
            parking.bind_source(source.resolver(), displays.ids(), displays.monitor_reader()),
            "bind source",
        )?;
        Ok(Setup {
            parking,
            displays,
            source,
            fixture,
            window,
            original,
            startup,
        })
    }

    /// The first monitor frame cropped to `crop`, as its size. The stream is stopped either way.
    fn first_frame(
        capture: &mut WindowsFrameCapture,
        target: CaptureTarget,
        crop: PixelRect,
    ) -> Result<PixelSize, String> {
        let (send, receive) = mpsc::channel::<FrameEvent>();
        let sink = Arc::new(move |event: FrameEvent| {
            let _ = send.send(event);
        });
        let stream = at(
            capture.start(target, Some(crop), 10, sink),
            "monitor capture start",
        )?;
        let until = Instant::now() + Duration::from_secs(5);
        let first = loop {
            match receive.recv_timeout(until.saturating_duration_since(Instant::now())) {
                Ok(FrameEvent::Frame { stream: id, frame }) if id == stream => {
                    break Ok(frame.size);
                }
                Ok(FrameEvent::Ended { stream: id, .. }) if id == stream => {
                    break Err("capture ended before a frame".to_owned());
                }
                Ok(_) => {}
                Err(_) => break Err("no frame within 5 s".to_owned()),
            }
        };
        let _ = capture.stop(stream);
        first
    }

    /// A scan of one monitor's top-level windows. Read-only.
    struct Scan {
        monitor: usize,
        found: Vec<(u64, u32)>,
    }

    /// `EnumWindows` callback: records each visible, uncloaked top-level window on the monitor.
    unsafe extern "system" fn scan_window(window: HWND, value: LPARAM) -> BOOL {
        // SAFETY: `value` is the `Scan` that `windows_on` lent to this synchronous enumeration.
        let scan = unsafe { &mut *(value as *mut Scan) };
        let mut cloaked = 0_u32;
        // SAFETY: writes one u32 into a local of the stated size; reads the cloak state only.
        let cloak = unsafe {
            DwmGetWindowAttribute(
                window,
                DWMWA_CLOAKED as u32,
                (&mut cloaked as *mut u32).cast(),
                size_of::<u32>() as u32,
            )
        };
        // SAFETY: reads visibility only.
        let visible = unsafe { IsWindowVisible(window) } != 0;
        // SAFETY: reads the monitor the window is on; MONITOR_DEFAULTTONULL never creates one.
        let monitor = unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONULL) } as usize;
        if visible && cloak == 0 && cloaked == 0 && monitor == scan.monitor {
            let mut pid = 0_u32;
            // SAFETY: reads the owning process of a window handle into a local; no state changes.
            unsafe { GetWindowThreadProcessId(window, &mut pid) };
            scan.found.push((window as usize as u64, pid));
        }
        1
    }

    /// The visible, uncloaked top-level windows on `monitor`, as (HWND, PID). Titles are never read
    /// into output; the caller prints counts only.
    fn windows_on(monitor: usize) -> Result<Vec<(u64, u32)>, String> {
        let mut scan = Scan {
            monitor,
            found: Vec::new(),
        };
        // SAFETY: EnumWindows calls `scan_window` on this thread before it returns, and `scan`
        // lives for that whole call.
        let completed =
            unsafe { EnumWindows(Some(scan_window), (&mut scan as *mut Scan) as LPARAM) };
        ensure(completed != 0, "window enumeration refused")?;
        Ok(scan.found)
    }

    /// The monitor's size in physical pixels.
    fn monitor_size(monitor: usize) -> Result<(i32, i32), String> {
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: `info` is a local whose cbSize is set as the API requires; the handle is read only.
        let okay = unsafe { GetMonitorInfoW(monitor as HMONITOR, &mut info) } != 0;
        ensure(okay, "twin monitor geometry unavailable")?;
        let rect = info.rcMonitor;
        Ok((rect.right - rect.left, rect.bottom - rect.top))
    }

    /// The outer rect of the admitted fixture window, after its owner is checked.
    fn owned_rect(identity: &NativeIdentity) -> Result<[i32; 4], String> {
        let window = identity.hwnd as usize as HWND;
        let mut pid = 0_u32;
        // SAFETY: reads the owner of a handle the fixture reported; no window state changes.
        let thread = unsafe { GetWindowThreadProcessId(window, &mut pid) };
        ensure(
            thread == identity.tid && pid == identity.pid,
            "fixture window identity changed",
        )?;
        let mut outer = RECT::default();
        // SAFETY: reads the geometry of the window verified just above into a local.
        let okay = unsafe { GetWindowRect(window, &mut outer) } != 0;
        ensure(okay, "fixture rect unavailable")?;
        Ok([outer.left, outer.top, outer.right, outer.bottom])
    }

    /// Polls up to 5 s for the window to be destroyed. Returns whether it is gone.
    fn window_gone(hwnd: u64) -> bool {
        let window = hwnd as usize as HWND;
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            // SAFETY: asks only whether the handle still names a window.
            if unsafe { IsWindow(window) } == 0 {
                return true;
            }
            if Instant::now() >= until {
                return false;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// P1: no driver. Park is refused with the absent reason in under 100 ms, and nothing is
    /// journaled.
    pub(super) fn absent(row: &mut Row) -> Result<(), String> {
        let dir = scratch()?;
        let mut setup = prepare(&dir, row)?;
        ensure(
            !setup.startup.driver_present,
            "driver present; the absent row needs no driver",
        )?;
        let started = Instant::now();
        let refused = setup
            .parking
            .park(setup.window, PixelSize::new(640, 360), 1.0);
        let park_ms = started.elapsed().as_millis();
        row.ms = park_ms;
        let absent = matches!(
            refused,
            Err(PlatformError::Unsupported(reason)) if reason == TWIN_ABSENT_REASON
        );
        row.fact("park_absent_refusal", absent);
        row.fact("park_ms", park_ms);
        ensure(absent, "park was not refused with the absent reason")?;
        ensure(park_ms < 100, "park took 100 ms or more")?;
        let empty = journals_empty(&dir)?;
        row.fact("journals_empty", empty);
        ensure(empty, "a journal holds entries after the refusal")?;
        at(setup.fixture.close(), "fixture close")?;
        Ok(())
    }

    /// P2: the twin hides the window and shows it to monitor capture at its content size. The twin
    /// is always 1920x1080 (R-F2), so a resize to 960x540 or to 1600x900 keeps the same display
    /// and mode; the 1600x900 window content is checked. A maximized window is refused with no
    /// twin. Restore puts the exact original rect back, leaves both journals empty and no own
    /// display paths.
    pub(super) fn park(row: &mut Row) -> Result<(), String> {
        let dir = scratch()?;
        let mut setup = prepare(&dir, row)?;
        ensure(
            setup.startup.driver_present,
            "driver absent; the park row needs the driver",
        )?;
        let started = Instant::now();
        let parked = at(
            setup
                .parking
                .park(setup.window, PixelSize::new(640, 360), 1.0),
            "park",
        )?;
        row.ms = started.elapsed().as_millis();
        row.fact("kind", format!("{:?}", parked.kind));
        ensure(parked.kind == ParkingKind::Twin, "park is not a twin")?;

        let listed = at(setup.displays.snapshot(), "display list")?
            .displays
            .iter()
            .any(|display| display.id == parked.display);
        row.fact("twin_listed", listed);
        ensure(!listed, "the twin is listed in Displays")?;

        let monitor = at(setup.displays.monitor(parked.display), "twin monitor")?;
        let on_twin = windows_on(monitor)?;
        let fixture = (setup.fixture.identity.hwnd, setup.fixture.identity.pid);
        row.fact("visible_on_twin", on_twin.len());
        row.fact("only_fixture_on_twin", on_twin == [fixture]);
        ensure(
            on_twin == [fixture],
            "the twin does not hold exactly the fixture",
        )?;

        let twin = monitor_size(monitor)?;
        row.fact("twin", format!("{}x{}", twin.0, twin.1));
        ensure(twin == (1920, 1080), "the first twin is not 1920x1080")?;

        let gate = IoGate::new();
        gate.set_engine_permits(true);
        gate.set_session_permits(true);
        let mut capture = at(
            WindowsFrameCapture::new_with_monitor_reader(
                gate,
                setup.source.resolver(),
                setup.displays.monitor_snapshot_reader(),
            ),
            "monitor capture",
        )?;
        let content = parked.content;
        let expected = PixelSize::new(content.width() as u32, content.height() as u32);
        let frame = first_frame(
            &mut capture,
            CaptureTarget::Display(parked.display),
            content,
        )?;
        drop(capture);
        row.fact("capture", format!("{}x{}", frame.width, frame.height));
        ensure(frame == expected, "the first frame is not the content size")?;

        // The twin is always 1920x1080, so a resize to 960x540 keeps the display and the mode.
        let within = at(
            setup
                .parking
                .resize(setup.window, PixelSize::new(960, 540), 1.0),
            "resize to 960x540",
        )?;
        let within_monitor = at(
            setup.displays.monitor(within.display),
            "resized twin monitor",
        )?;
        let within_size = monitor_size(within_monitor)?;
        let within_same = within.kind == ParkingKind::Twin && within.display == parked.display;
        row.fact("resize_960_same_display", within_same);
        row.fact(
            "resize_960_twin",
            format!("{}x{}", within_size.0, within_size.1),
        );
        ensure(
            within_same,
            "a resize to 960x540 changed the display or the kind",
        )?;
        ensure(
            within_size == (1920, 1080),
            "the twin is not 1920x1080 after a resize to 960x540",
        )?;

        // A resize to 1600x900 still fits the 1920x1080 twin, so the display does not change.
        let large = at(
            setup
                .parking
                .resize(setup.window, PixelSize::new(1600, 900), 1.0),
            "resize to 1600x900",
        )?;
        let large_monitor = at(
            setup.displays.monitor(large.display),
            "resized twin monitor",
        )?;
        let large_size = monitor_size(large_monitor)?;
        let large_same = large.kind == ParkingKind::Twin && large.display == parked.display;
        let large_content = (large.content.width(), large.content.height());
        row.fact("resize_1600_same_display", large_same);
        row.fact(
            "resize_1600_twin",
            format!("{}x{}", large_size.0, large_size.1),
        );
        row.fact(
            "resize_1600_content",
            format!("{}x{}", large_content.0, large_content.1),
        );
        ensure(
            large_same,
            "a resize to 1600x900 changed the display or the kind",
        )?;
        ensure(
            large_size == (1920, 1080),
            "the twin is not 1920x1080 after a resize to 1600x900",
        )?;
        ensure(
            large_content == (1600, 900),
            "the window content is not 1600x900 after a resize to 1600x900",
        )?;

        at(setup.parking.restore(setup.window), "restore")?;
        let back = at(setup.fixture.rect(), "fixture rect after restore")?;
        row.fact("restore_original_rect", back == setup.original);
        ensure(
            back == setup.original,
            "restore did not return the original rect",
        )?;
        let empty = journals_empty(&dir)?;
        row.fact("journals_empty", empty);
        ensure(empty, "a journal holds entries after restore")?;
        let own = own_paths()?;
        row.fact("own_paths", own);
        ensure(own == 0, "own display paths remain after restore")?;

        // Maximize is last because the fixture has no un-maximize; the rect check is done above.
        // The owned fixture may not maximize from another process on every desktop (VM rows did
        // not); the maximized refusal itself is covered by the model tests. Report it as [U].
        if setup.fixture.maximize().is_err() {
            row.fact("maximized_refused", "U");
            at(setup.fixture.close(), "fixture close")?;
            row.fact("closed", true);
            return Ok(());
        }
        let refused = setup
            .parking
            .park(setup.window, PixelSize::new(640, 360), 1.0);
        let maximized = matches!(
            refused,
            Err(PlatformError::Unsupported(reason)) if reason == TWIN_MAXIMIZED_REASON
        );
        row.fact("maximized_refused", maximized);
        ensure(
            maximized,
            "a maximized park was not refused with the maximized reason",
        )?;
        let none = own_paths()? == 0 && journals_empty(&dir)?;
        row.fact("maximized_no_twin", none);
        ensure(none, "a maximized refusal left a twin or a journal entry")?;

        at(setup.fixture.close(), "fixture close")?;
        row.fact("closed", true);
        Ok(())
    }

    /// What `crash` recorded: the fixture identity and its original outer rect.
    struct Crashed {
        identity: NativeIdentity,
        rect: [i32; 4],
    }

    fn read_crash(dir: &Path) -> Result<Crashed, String> {
        let bytes = at(
            fs::read(dir.join(FIXTURE_STATE)),
            "fixture state (run the crash row first)",
        )?;
        let value: serde_json::Value = at(serde_json::from_slice(&bytes), "fixture state")?;
        let number = |key: &str| {
            value[key]
                .as_u64()
                .ok_or_else(|| format!("fixture state field {key}"))
        };
        let identity = NativeIdentity {
            hwnd: number("hwnd")?,
            pid: at(u32::try_from(number("pid")?), "fixture pid")?,
            tid: at(u32::try_from(number("tid")?), "fixture tid")?,
            process_created: number("created")?,
        };
        let rect = value["rect"]
            .as_array()
            .and_then(|values| {
                values
                    .iter()
                    .map(|value| value.as_i64().and_then(|n| i32::try_from(n).ok()))
                    .collect::<Option<Vec<i32>>>()
            })
            .and_then(|values| <[i32; 4]>::try_from(values).ok())
            .ok_or_else(|| "fixture state rect".to_owned())?;
        Ok(Crashed { identity, rect })
    }

    /// P3: park, record the fixture identity and its original rect, then end this process without
    /// unwinding. The fixture child has no kill-on-close job, so it keeps running for `recover`.
    pub(super) fn crash(row: &mut Row) -> Result<(), String> {
        let dir = scratch()?;
        let mut setup = prepare(&dir, row)?;
        ensure(
            setup.startup.driver_present,
            "driver absent; the crash row needs the driver",
        )?;
        let started = Instant::now();
        let parked = at(
            setup
                .parking
                .park(setup.window, PixelSize::new(640, 360), 1.0),
            "park",
        )?;
        row.ms = started.elapsed().as_millis();
        ensure(parked.kind == ParkingKind::Twin, "park is not a twin")?;
        let state = serde_json::json!({
            "hwnd": setup.fixture.identity.hwnd,
            "pid": setup.fixture.identity.pid,
            "tid": setup.fixture.identity.tid,
            "created": setup.fixture.identity.process_created,
            "rect": setup.original,
        });
        at(
            fs::write(dir.join(FIXTURE_STATE), state.to_string()),
            "fixture state",
        )?;
        row.fact("kind", format!("{:?}", parked.kind));
        row.fact("state_written", true);
        row.emit(true, None);
        // SAFETY: terminates only this process, through its own pseudo-handle, to simulate a crash.
        let refused = unsafe { TerminateProcess(GetCurrentProcess(), CRASH_CODE) } == 0;
        std::process::exit(if refused { 1 } else { CRASH_CODE as i32 })
    }

    /// P4: startup recovery after `crash` restores the window to its original rect and clears the
    /// journaled twin. Then the fixture is closed through `close_owned`.
    pub(super) fn recover(row: &mut Row) -> Result<(), String> {
        let dir = scratch()?;
        let crashed = read_crash(&dir)?;
        let mut parking = facade(&dir)?;
        let started = Instant::now();
        let report = at(parking.recover_startup(), "startup recovery")?;
        row.ms = started.elapsed().as_millis();
        row.fact("restored", report.windows.restored);
        row.fact("twins_journaled", report.twins_journaled);
        ensure(
            report.windows.restored == 1,
            "window restores are not exactly 1",
        )?;
        ensure(
            report.twins_journaled == 1,
            "journaled twins are not exactly 1",
        )?;
        let admitted = admit(&crashed.identity)?;
        let rect = owned_rect(&crashed.identity)?;
        row.fact("restore_original_rect", rect == crashed.rect);
        ensure(
            rect == crashed.rect,
            "the fixture is not at its original rect",
        )?;
        let empty = journals_empty(&dir)?;
        row.fact("journals_empty", empty);
        ensure(empty, "a journal holds entries after recovery")?;
        at(
            close_owned(crashed.identity.hwnd, crashed.identity.pid),
            "fixture close",
        )?;
        let closed = window_gone(crashed.identity.hwnd);
        row.fact("closed", closed);
        ensure(closed, "the fixture is still open after WM_CLOSE")?;
        drop(admitted);
        let _ = fs::remove_file(dir.join(FIXTURE_STATE));
        Ok(())
    }

    /// The monitor capture the `remode` row runs, the queue its streams report to, and the stream
    /// kept live. `ends` keeps every end seen, so a wait that reads past one still finds it.
    struct Feed {
        capture: WindowsFrameCapture,
        send: mpsc::Sender<FrameEvent>,
        events: mpsc::Receiver<FrameEvent>,
        live: Option<StreamId>,
        ends: Vec<(StreamId, StreamEndReason)>,
    }

    impl Feed {
        /// Starts a monitor stream on the twin `display`, cropped to `crop`, at 10 fps.
        fn start(&mut self, display: DisplayId, crop: PixelRect) -> Result<StreamId, String> {
            let tx = self.send.clone();
            let sink = Arc::new(move |event: FrameEvent| {
                let _ = tx.send(event);
            });
            at(
                self.capture
                    .start(CaptureTarget::Display(display), Some(crop), 10, sink),
                "monitor capture start",
            )
        }

        /// The next queued event before `until`, or `None`. An end is kept in `ends`.
        fn next(&mut self, until: Instant) -> Option<FrameEvent> {
            let event = self
                .events
                .recv_timeout(until.saturating_duration_since(Instant::now()))
                .ok()?;
            if let FrameEvent::Ended { stream, reason } = &event {
                self.ends.push((*stream, *reason));
            }
            Some(event)
        }

        /// The first frame of `stream`, as its size. Events of other streams are skipped.
        fn await_frame(&mut self, stream: StreamId) -> Result<PixelSize, String> {
            let until = Instant::now() + FIRST_FRAME_WAIT;
            loop {
                match self.next(until) {
                    Some(FrameEvent::Frame { stream: id, frame }) if id == stream => {
                        return Ok(frame.size);
                    }
                    Some(FrameEvent::Ended { stream: id, .. }) if id == stream => {
                        return Err("capture ended before a frame".to_owned());
                    }
                    Some(_) => {}
                    None => return Err("no frame within 5 s".to_owned()),
                }
            }
        }

        /// The end reason of `stream` within [`OLD_END_WAIT`], or `None` while it still runs.
        fn await_end(&mut self, stream: StreamId) -> Option<StreamEndReason> {
            if let Some((_, reason)) = self.ends.iter().find(|(id, _)| *id == stream) {
                return Some(*reason);
            }
            let until = Instant::now() + OLD_END_WAIT;
            loop {
                match self.next(until) {
                    Some(FrameEvent::Ended { stream: id, reason }) if id == stream => {
                        return Some(reason);
                    }
                    Some(_) => {}
                    None => return None,
                }
            }
        }
    }

    /// Resolves the fixture window every [`SAMPLE_PERIOD`] on its own thread, until `finish`.
    struct Sampler {
        stop: Arc<AtomicBool>,
        worker: thread::JoinHandle<(u64, u64)>,
    }

    impl Sampler {
        fn start(resolver: WindowResolver, window: WindowId) -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            let worker = thread::spawn(move || {
                let (mut samples, mut missing) = (0_u64, 0_u64);
                while !flag.load(Ordering::Acquire) {
                    samples += 1;
                    if resolver.resolve(window).is_none() {
                        missing += 1;
                    }
                    thread::sleep(SAMPLE_PERIOD);
                }
                (samples, missing)
            });
            Self { stop, worker }
        }

        /// Stops the sampler. Returns the samples taken and the `None` results, or `None` if the
        /// sampler thread panicked.
        fn finish(self) -> Option<(u64, u64)> {
            self.stop.store(true, Ordering::Release);
            self.worker.join().ok()
        }
    }

    /// The effective DPI of `display`, a twin or a real monitor, or `None` when it can't be read.
    fn display_dpi(displays: &WindowsDisplays, display: DisplayId) -> Option<u32> {
        let monitor = displays.monitor(display).ok()?;
        let (mut horizontal, mut vertical) = (0_u32, 0_u32);
        // SAFETY: writes two u32 locals through valid pointers; the monitor handle is only read.
        let status = unsafe {
            GetDpiForMonitor(
                monitor as HMONITOR,
                MDT_EFFECTIVE_DPI,
                &mut horizontal,
                &mut vertical,
            )
        };
        (status == 0).then_some(horizontal)
    }

    /// A DPI as a number, or `U` when it is unknown.
    fn dpi_text(dpi: Option<u32>) -> String {
        dpi.map_or_else(|| "U".to_owned(), |dpi| dpi.to_string())
    }

    /// The effective DPI of the twin `display` as a number, or `U` when it can't be read.
    fn twin_dpi(displays: &WindowsDisplays, display: DisplayId) -> String {
        dpi_text(display_dpi(displays, display))
    }

    /// What the re-mode cycles counted. `old_end` and `twin_dpi` hold one entry per re-mode.
    #[derive(Default)]
    struct Remodes {
        ok: u32,
        twin_unavailable: u32,
        retained: u32,
        restart_frame: u32,
        same_display: u32,
        old_end: Vec<&'static str>,
        twin_dpi: Vec<String>,
    }

    /// The name the row reports for a stream's end reason.
    fn end_name(reason: StreamEndReason) -> &'static str {
        match reason {
            StreamEndReason::TargetGone => "TargetGone",
            StreamEndReason::Failed => "Failed",
            StreamEndReason::Blocked => "Blocked",
            StreamEndReason::Requested => "Requested",
            _ => "Other",
        }
    }

    /// The content size of a parked window on its twin, as the park row computes it.
    fn content_size(content: PixelRect) -> PixelSize {
        PixelSize::new(content.width() as u32, content.height() as u32)
    }

    /// The six re-modes. Each one lets the old monitor stream end (or stops it), then starts a fresh
    /// stream on the new twin. A failed re-mode ends the cycles, because the window is no longer
    /// parked and there is no twin to restart on.
    fn remode_cycles(
        setup: &mut Setup,
        feed: &mut Feed,
        tally: &mut Remodes,
        mut display: DisplayId,
        mut content: PixelRect,
    ) -> Result<(), String> {
        let stream = feed.start(display, content)?;
        feed.live = Some(stream);
        let first = feed.await_frame(stream)?;
        ensure(
            first == content_size(content),
            "the live stream's first frame is not the content size",
        )?;
        for index in 0..REMODES {
            let scale = if index % 2 == 0 { 1.5 } else { 1.0 };
            let old = feed.live.take();
            let resized = setup
                .parking
                .resize(setup.window, PixelSize::new(640, 360), scale);
            let old_end = match old {
                None => "NoStream",
                Some(stream) => match feed.await_end(stream) {
                    Some(reason) => end_name(reason),
                    None => {
                        let _ = feed.capture.stop(stream);
                        "None"
                    }
                },
            };
            tally.old_end.push(old_end);
            match resized {
                Ok(parked) => {
                    tally.ok += 1;
                    if parked.kind == ParkingKind::Twin && parked.display == display {
                        tally.same_display += 1;
                    }
                    tally
                        .twin_dpi
                        .push(twin_dpi(&setup.displays, parked.display));
                    display = parked.display;
                    content = parked.content;
                    if let Ok(stream) = feed.start(display, content) {
                        feed.live = Some(stream);
                        if feed.await_frame(stream) == Ok(content_size(content)) {
                            tally.restart_frame += 1;
                        }
                    }
                }
                Err(error) => {
                    let unavailable = matches!(
                        &error,
                        PlatformError::Unsupported(reason) if *reason == TWIN_UNAVAILABLE_REASON
                    );
                    if unavailable {
                        tally.twin_unavailable += 1;
                    }
                    if error.to_string().contains("journal retained") {
                        tally.retained += 1;
                    }
                    tally.twin_dpi.push("U".to_owned());
                    return Err(format!("re-mode {} at scale {scale}: {error}", index + 1));
                }
            }
        }
        Ok(())
    }

    /// V3 `remode`: a twin re-mode keeps the window's projection (R-F1), and the window stays
    /// resolvable through it (R1). The fixture is parked 640x360 at 1.0 with one monitor stream
    /// live on its twin, and a sampler resolves the window every 5 ms. Three cycles then resize to
    /// 640x360 at 1.5 and at 1.0. After each re-mode the old stream's end is recorded, and a fresh
    /// stream on the new twin must deliver a frame of the content size. Then the window is restored
    /// and the journals and own display paths are checked. Counts and sizes only, never titles.
    pub(super) fn remode(row: &mut Row) -> Result<(), String> {
        let dir = scratch()?;
        let mut setup = prepare(&dir, row)?;
        ensure(
            setup.startup.driver_present,
            "driver absent; the remode row needs the driver",
        )?;
        let parked = at(
            setup
                .parking
                .park(setup.window, PixelSize::new(640, 360), 1.0),
            "park",
        )?;
        row.fact("kind", format!("{:?}", parked.kind));
        ensure(parked.kind == ParkingKind::Twin, "park is not a twin")?;

        let gate = IoGate::new();
        gate.set_engine_permits(true);
        gate.set_session_permits(true);
        let capture = at(
            WindowsFrameCapture::new_with_monitor_reader(
                gate,
                setup.source.resolver(),
                setup.displays.monitor_snapshot_reader(),
            ),
            "monitor capture",
        )?;
        let (send, events) = mpsc::channel::<FrameEvent>();
        let mut feed = Feed {
            capture,
            send,
            events,
            live: None,
            ends: Vec::new(),
        };
        let sampler = Sampler::start(setup.source.resolver(), setup.window);
        let mut tally = Remodes::default();
        let started = Instant::now();
        let cycled = remode_cycles(
            &mut setup,
            &mut feed,
            &mut tally,
            parked.display,
            parked.content,
        );
        row.ms = started.elapsed().as_millis();

        // Cleanup runs on every path: the live stream, the sampler, then the window.
        if let Some(stream) = feed.live.take() {
            let _ = feed.capture.stop(stream);
        }
        let sampled = sampler.finish();
        let restored = at(setup.parking.restore(setup.window), "restore");
        let back = setup.fixture.rect().ok();
        let rect_exact = back == Some(setup.original);
        let empty = journals_empty(&dir).unwrap_or(false);
        let own = own_paths().ok();
        let closed = at(setup.fixture.close(), "fixture close");

        let checks = [
            ("remode_ok", tally.ok == REMODES),
            ("twin_unavailable", tally.twin_unavailable == 0),
            ("retained", tally.retained == 0),
            ("restart_frame", tally.restart_frame == REMODES),
            (
                "resolve_none",
                sampled.is_some_and(|(_, missing)| missing == 0),
            ),
            ("rect_exact", rect_exact),
            ("journals_empty", empty),
            ("own_paths", own == Some(0)),
        ];
        let failure = cycled
            .err()
            .or(restored.err())
            .or(closed.err())
            .or_else(|| {
                checks
                    .iter()
                    .find(|(_, held)| !held)
                    .map(|(name, _)| format!("{name} not as asserted"))
            });

        row.fact("remode_ok", format!("{}/{REMODES}", tally.ok));
        row.fact("twin_unavailable", tally.twin_unavailable);
        row.fact("retained", tally.retained);
        row.fact(
            "restart_frame",
            format!("{}/{REMODES}", tally.restart_frame),
        );
        row.fact(
            "resolve_none",
            sampled.map_or_else(|| "U".to_owned(), |(_, missing)| missing.to_string()),
        );
        row.fact("rect_exact", u8::from(rect_exact));
        row.fact("journals_empty", u8::from(empty));
        row.fact(
            "own_paths",
            own.map_or_else(|| "U".to_owned(), |count| count.to_string()),
        );
        row.fact("old_end", tally.old_end.join(","));
        row.fact("same_display", format!("{}/{REMODES}", tally.same_display));
        row.fact("twin_dpi", tally.twin_dpi.join(","));
        row.fact(
            "resolve_samples",
            sampled.map_or_else(|| "U".to_owned(), |(samples, _)| samples.to_string()),
        );
        row.fact("RESULT", if failure.is_none() { "PASS" } else { "FAIL" });
        match failure {
            None => Ok(()),
            Some(stage) => Err(stage),
        }
    }

    /// The `dpi_place` placement requests this offset from the real monitor's work-area top-left,
    /// in device pixels. It lies inside the work area, so the placer's clamp leaves it unchanged.
    const PLACE_OFFSET: (i32, i32) = (37, 41);
    /// How long one placement may take, read-back included.
    const PLACE_WAIT: Duration = Duration::from_secs(5);

    /// The visible (DWM extended-frame) rectangle of the admitted fixture, after its owner is
    /// checked, as `[left, top, right, bottom]`.
    fn owned_visible(identity: &NativeIdentity) -> Result<[i32; 4], String> {
        let window = identity.hwnd as usize as HWND;
        let mut pid = 0_u32;
        // SAFETY: reads the owner of a handle the fixture reported; no window state changes.
        let thread = unsafe { GetWindowThreadProcessId(window, &mut pid) };
        ensure(
            thread == identity.tid && pid == identity.pid,
            "fixture window identity changed",
        )?;
        let mut bounds = RECT::default();
        // SAFETY: writes one RECT into a local of the stated size; reads the frame bounds only.
        let status = unsafe {
            DwmGetWindowAttribute(
                window,
                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                (&mut bounds as *mut RECT).cast(),
                size_of::<RECT>() as u32,
            )
        };
        ensure(status >= 0, "fixture visible bounds unavailable")?;
        Ok([bounds.left, bounds.top, bounds.right, bounds.bottom])
    }

    /// The full and work-area rectangles of a monitor, each as `[left, top, right, bottom]`.
    fn monitor_rects(monitor: usize) -> Result<([i32; 4], [i32; 4]), String> {
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: `info` has cbSize set as the API requires; the handle is only read.
        let okay = unsafe { GetMonitorInfoW(monitor as HMONITOR, &mut info) } != 0;
        ensure(okay, "monitor geometry unavailable")?;
        let rect = |rect: RECT| [rect.left, rect.top, rect.right, rect.bottom];
        Ok((rect(info.rcMonitor), rect(info.rcWork)))
    }

    /// `WxH` of a `[left, top, right, bottom]` rectangle.
    fn size_text(rect: [i32; 4]) -> String {
        format!("{}x{}", rect[2] - rect[0], rect[3] - rect[1])
    }

    /// What one `dpi_place` placement observed. `exercised` is false when the twin and the real
    /// monitor share a DPI, or one is unreadable, so the mixed-DPI move cannot be established.
    struct Landing {
        exercised: bool,
        placed: bool,
    }

    /// One `WindowsRestorePlacer::place` from the twin to the first real monitor, at the work-area
    /// offset. The visible top-left is read back and checked against the request within 1 px.
    fn land(setup: &Setup, twin: DisplayId, row: &mut Row) -> Result<Landing, String> {
        let real = at(setup.displays.snapshot(), "display list")?
            .displays
            .iter()
            .map(|display| display.id)
            .find(|id| *id != twin)
            .ok_or_else(|| "no real monitor besides the twin".to_owned())?;
        let monitor = at(setup.displays.monitor(real), "real monitor")?;
        let (full, work) = monitor_rects(monitor)?;
        let requested = (work[0] + PLACE_OFFSET.0, work[1] + PLACE_OFFSET.1);
        let origin = PointDevice::new(
            f64::from(requested.0 - full[0]),
            f64::from(requested.1 - full[1]),
        );
        let (twin_dpi, real_dpi) = (
            display_dpi(&setup.displays, twin),
            display_dpi(&setup.displays, real),
        );
        row.fact("twin_dpi", dpi_text(twin_dpi));
        row.fact("real_dpi", dpi_text(real_dpi));
        let exercised = matches!((twin_dpi, real_dpi), (Some(a), Some(b)) if a != b);
        let before = owned_visible(&setup.fixture.identity)?;
        let mut placer = WindowsRestorePlacer::new(
            setup.source.resolver(),
            setup.displays.ids(),
            setup.displays.monitor_reader(),
        );
        let placed = placer.place(setup.window, real, origin, Instant::now() + PLACE_WAIT);
        let after = owned_visible(&setup.fixture.identity)?;
        let dx = i64::from(after[0]) - i64::from(requested.0);
        let dy = i64::from(after[1]) - i64::from(requested.1);
        if let Err(error) = &placed {
            row.fact("place_error", error);
        }
        row.fact("visible_before", size_text(before));
        row.fact("visible_after", size_text(after));
        row.fact("dx", dx);
        row.fact("dy", dy);
        // `place` reports no recompute count, so the number of recomputes is not observable here.
        row.fact("recomputes", "U");
        let landed = placed.is_ok() && dx.abs() <= 1 && dy.abs() <= 1;
        row.fact("placed", landed);
        Ok(Landing {
            exercised,
            placed: landed,
        })
    }

    /// V6 `dpi_place`: the fixture runs with `--dpi-resize` and is parked on a 1.5-scale twin. The
    /// placer then moves it onto the first real monitor, so the move changes DPI and the fixture's
    /// `WM_DPICHANGED` resize is what the placer's read-back must recompute for (W1.6b Low 2).
    /// `RESULT=U` when the DPIs match or cannot be read. Otherwise `PASS` needs the visible
    /// top-left within 1 px of the request. The park is always restored and the fixture always
    /// closed; a cleanup failure is a `FAIL`. Counts, sizes and offsets only.
    pub(super) fn dpi_place(row: &mut Row) -> Result<(), String> {
        let dir = scratch()?;
        let mut setup = prepare_with(&dir, row, FixtureChild::spawn_dpi_resize)?;
        ensure(
            setup.startup.driver_present,
            "driver absent; the dpi_place row needs the driver",
        )?;
        let started = Instant::now();
        let parked = at(
            setup
                .parking
                .park(setup.window, PixelSize::new(640, 360), 1.5),
            "park at scale 1.5",
        )?;
        row.fact("kind", format!("{:?}", parked.kind));
        let landed = if parked.kind == ParkingKind::Twin {
            land(&setup, parked.display, row)
        } else {
            Err("park is not a twin".to_owned())
        };
        row.ms = started.elapsed().as_millis();

        // Cleanup runs on every path: the park is restored, its journals and paths are checked,
        // then the fixture closes.
        let restored = at(setup.parking.restore(setup.window), "restore");
        let empty = journals_empty(&dir).unwrap_or(false);
        let own = own_paths().ok();
        let closed = at(setup.fixture.close(), "fixture close");
        row.fact("journals_empty", empty);
        row.fact(
            "own_paths",
            own.map_or_else(|| "U".to_owned(), |count| count.to_string()),
        );
        row.fact("closed", closed.is_ok());
        let cleanup = restored
            .err()
            .or(closed.err())
            .or_else(|| {
                (!empty).then(|| "journals not empty after restore (or unreadable)".to_owned())
            })
            .or_else(|| {
                (own != Some(0))
                    .then(|| "own display paths not 0 after restore (or unreadable)".to_owned())
            });

        let exercised = landed.as_ref().is_ok_and(|landing| landing.exercised);
        let placed = landed.as_ref().is_ok_and(|landing| landing.placed);
        let failure = cleanup.or_else(|| match &landed {
            Err(stage) => Some(stage.clone()),
            Ok(_) if exercised && !placed => {
                Some("placement not within 1 px of the request".to_owned())
            }
            Ok(_) => None,
        });
        let result = if failure.is_some() {
            "FAIL"
        } else if exercised {
            "PASS"
        } else {
            "U"
        };
        row.fact("RESULT", result);
        match failure {
            None => Ok(()),
            Some(stage) => Err(stage),
        }
    }
}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    use std::process::ExitCode;

    match std::env::args().nth(1).as_deref() {
        Some("fixture") => match std::env::args().nth(2).as_deref() {
            None => fixture::run_fixture(false),
            Some("--dpi-resize") => fixture::run_fixture(true),
            Some(_) => ExitCode::from(2),
        },
        Some("store-selftest") => match store::selftest() {
            Ok(()) => {
                println!("STORE_SELFTEST ok");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("STORE_SELFTEST failed: {error}");
                ExitCode::FAILURE
            }
        },
        Some("absent") => rows::run("absent", rows::absent),
        Some("park") => rows::run("park", rows::park),
        Some("crash") => rows::run("crash", rows::crash),
        Some("recover") => rows::run("recover", rows::recover),
        Some("remode") => rows::run("remode", rows::remode),
        Some("dpi_place") => rows::run("dpi_place", rows::dpi_place),
        _ => {
            println!("unsupported mode");
            ExitCode::from(2)
        }
    }
}

#[cfg(not(windows))]
fn main() {}
