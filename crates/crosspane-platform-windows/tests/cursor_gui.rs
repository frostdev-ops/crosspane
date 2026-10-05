//! Limited, authenticated colour fixture only; never run through elevated cargo.
#![cfg(windows)]
#![allow(unsafe_code, clippy::unwrap_used, clippy::expect_used)]

use crosspane_platform::{
    CaptureTarget, CursorImage, FrameCapture, FrameEvent, IoGate, StreamEndReason, StreamId,
    WindowSource,
};
pub use crosspane_platform_windows::clock;
use crosspane_platform_windows::model::{
    self,
    geometry::{DisplayIds, MonitorProbe},
    window::Identity,
};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, BufReader, Write},
    mem::size_of,
    os::windows::io::AsRawHandle,
    process::{Child, Command, Stdio},
    ptr::{null, null_mut},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::{Dwm::*, Gdi::*},
    Security::*,
    System::{JobObjects::*, LibraryLoader::GetModuleHandleW, Threading::*},
    UI::{HiDpi::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

#[path = "../src/cursor.rs"]
mod cursor;
#[path = "../src/frame_capture.rs"]
mod frame_capture;
#[path = "../src/window.rs"]
mod window;

#[test]
fn cursor_sampling_has_rust_only_state_and_the_existing_capture_facade() {
    fn send<T: Send>() {}
    send::<cursor::StreamCursor>();
    send::<frame_capture::WindowsFrameCapture>();
    let _source_factory = window::WindowsWindowSource::new;
}

#[derive(Serialize, Deserialize)]
struct Hello {
    nonce: String,
    hwnd: u64,
    pid: u32,
    tid: u32,
    created: u64,
    frame: [i32; 4],
    monitor: Monitor,
    outside: u64,
    outside_frame: [i32; 4],
}
#[derive(Serialize, Deserialize)]
struct Monitor {
    name: String,
    bounds: [i32; 4],
    work: [i32; 4],
    dpi: u32,
}
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
fn coordinates(r: RECT) -> [i32; 4] {
    [r.left, r.top, r.right, r.bottom]
}
fn string(text: &[u16]) -> String {
    String::from_utf16_lossy(&text[..text.iter().position(|c| *c == 0).unwrap_or(text.len())])
}
fn limited() {
    let mut token = null_mut();
    let mut elevation = TOKEN_ELEVATION::default();
    let mut bytes = 0;
    // SAFETY: own process token, initialized exact output buffer; owned query handle closed.
    unsafe {
        assert_ne!(
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token),
            0
        );
        let result = GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut bytes,
        );
        assert_ne!(CloseHandle(token), 0);
        assert_ne!(result, 0);
    }
    assert_eq!(
        elevation.TokenIsElevated, 0,
        "CURSOR probe requires Limited win-gui route"
    );
}
struct Watchdog(mpsc::Sender<()>);
impl Watchdog {
    fn new(seconds: u64) -> Self {
        let (send, receive) = mpsc::channel();
        thread::spawn(move || {
            if receive.recv_timeout(Duration::from_secs(seconds)).is_err() {
                eprintln!("OWNED CURSOR WATCHDOG EXPIRED");
                std::process::exit(124);
            }
        });
        Self(send)
    }
}
impl Drop for Watchdog {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}
struct Fixture {
    child: Child,
    job: HANDLE,
    lines: mpsc::Receiver<String>,
    reader: Option<thread::JoinHandle<()>>,
    authenticated: Option<[usize; 2]>,
}
impl Fixture {
    fn launch(nonce: &str) -> Self {
        // SAFETY: private unnamed job containing only our spawned child.
        let job = unsafe { CreateJobObjectW(null(), null()) };
        assert!(!job.is_null());
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: exact initialized limits structure and private job handle.
        let result = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        assert_ne!(result, 0);
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "cursor_fixture_server",
                "--nocapture",
            ])
            .env("CROSSPANE_CURSOR_FIXTURE_NONCE", nonce)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let child = match child {
            Ok(child) => child,
            Err(error) => {
                // SAFETY: no child was created/assigned; close only this job handle.
                unsafe { CloseHandle(job) };
                panic!("owned fixture spawn: {error}");
            }
        };
        let (send, lines) = mpsc::channel();
        let mut fixture = Self {
            child,
            job,
            lines,
            reader: None,
            authenticated: None,
        };
        // SAFETY: only the just-spawned child is assigned to the private job.
        let assigned = unsafe { AssignProcessToJobObject(job, fixture.child.as_raw_handle()) };
        assert_ne!(assigned, 0);
        let stdout = fixture.child.stdout.take().unwrap();
        fixture.reader = Some(thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                if send.send(line).is_err() {
                    break;
                }
            }
        }));
        fixture
    }
    fn response(&self, prefix: &str) -> String {
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            let line = self
                .lines
                .recv_timeout(until.saturating_duration_since(Instant::now()))
                .expect("fixture response deadline");
            if let Some(body) = line.strip_prefix(prefix) {
                return body.into();
            }
            if line.starts_with("OWN_") {
                println!("{line}");
            }
        }
    }
    fn command(&mut self, command: &str) {
        writeln!(self.child.stdin.as_mut().unwrap(), "{command}").unwrap();
        self.child.stdin.as_mut().unwrap().flush().unwrap();
        assert_eq!(self.response("ACK "), command);
    }
    fn finish(mut self) {
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(Instant::now() < until, "fixture exit deadline");
            thread::sleep(Duration::from_millis(10));
        }
        if let Some(reader) = self.reader.take() {
            reader.join().unwrap();
        }
        // SAFETY: child exited; no other process belongs to this job.
        let closed = unsafe { CloseHandle(self.job) };
        assert_ne!(closed, 0);
        self.job = null_mut();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if !self.job.is_null() {
            // SAFETY: only our child belongs to this kill-on-close job.
            let closed = unsafe { CloseHandle(self.job) } != 0;
            self.job = null_mut();
            let until = Instant::now() + Duration::from_secs(2);
            let exited = loop {
                match self.child.try_wait() {
                    Ok(Some(_)) => break true,
                    Err(_) => break false,
                    Ok(None) if Instant::now() < until => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Ok(None) => break false,
                }
            };
            if exited && let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
            // SAFETY: existence only, after job exit, for our previously authenticated HWNDs.
            // A reused/nonzero handle is uncertain; never query its fields or claim ownership.
            let windows_gone = self
                .authenticated
                .map(|handles| unsafe { handles.map(|hwnd| IsWindow(hwnd as HWND) == 0) });
            eprintln!(
                "OWNED_CURSOR failure cleanup: job_closed={closed} fixture_exited={exited} owned_windows_gone={windows_gone:?}"
            );
        }
    }
}

struct Verified<T> {
    value: Option<T>,
    stop: fn(T) -> bool,
    name: &'static str,
}
impl<T> Verified<T> {
    fn new(value: T, stop: fn(T) -> bool, name: &'static str) -> Self {
        Self {
            value: Some(value),
            stop,
            name,
        }
    }
    fn finish(mut self) -> bool {
        (self.stop)(self.value.take().unwrap())
    }
}
impl<T> std::ops::Deref for Verified<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.value.as_ref().unwrap()
    }
}
impl<T> std::ops::DerefMut for Verified<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.value.as_mut().unwrap()
    }
}
impl<T> Drop for Verified<T> {
    fn drop(&mut self) {
        if let Some(value) = self.value.take() {
            let stopped = (self.stop)(value);
            eprintln!(
                "OWNED_CURSOR failure cleanup: {}_joined={stopped}",
                self.name
            );
        }
    }
}

thread_local! {
    static OWN_CURSOR: std::cell::Cell<HCURSOR> = const { std::cell::Cell::new(null_mut()) };
    static OWN_PAINTED: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
}
struct CustomCursor(HCURSOR);
impl CustomCursor {
    fn colour() -> Self {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: 8,
                biHeight: -4,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = null_mut();
        // SAFETY: private 8x4 top-down 32bpp DIB; writable pointer is owned until bitmap deletion.
        let colour = unsafe {
            CreateDIBSection(
                null_mut(),
                &info,
                DIB_RGB_COLORS,
                &mut pixels,
                null_mut(),
                0,
            )
        };
        assert!(!colour.is_null() && !pixels.is_null());
        // SAFETY: exact 128-byte owned DIB; known fixture pixels only.
        unsafe { std::slice::from_raw_parts_mut(pixels.cast::<u8>(), 128) }
            .copy_from_slice(&known_colour());
        // SAFETY: own 8x4 1bpp mask; CreateBitmap consumes 2-byte-aligned source rows.
        let mask = unsafe { CreateBitmap(8, 4, 1, 1, [0u8; 8].as_ptr().cast()) };
        assert!(!mask.is_null());
        let facts = ICONINFO {
            fIcon: 0,
            xHotspot: 3,
            yHotspot: 1,
            hbmMask: mask,
            hbmColor: colour,
        };
        // SAFETY: CreateIconIndirect copies both private bitmaps; delete source GDI objects after.
        let handle = unsafe { CreateIconIndirect(&facts) };
        // SAFETY: source objects owned here, never selected in any DC, copied by the API above.
        unsafe {
            assert_ne!(DeleteObject(mask), 0);
            assert_ne!(DeleteObject(colour), 0);
        }
        assert!(!handle.is_null());
        Self(handle)
    }
    fn mono() -> Self {
        // SAFETY: private 4x2 1bpp AND/XOR rows; 2-byte native row alignment.
        let bitmap = unsafe { CreateBitmap(4, 2, 1, 1, [0x30u8, 0, 0x50, 0].as_ptr().cast()) };
        assert!(!bitmap.is_null());
        let facts = ICONINFO {
            fIcon: 0,
            xHotspot: 1,
            yHotspot: 0,
            hbmMask: bitmap,
            hbmColor: null_mut(),
        };
        // SAFETY: copies the own bitmap into a private cursor, source immediately released.
        let handle = unsafe { CreateIconIndirect(&facts) };
        // SAFETY: own mask bitmap, not selected into a DC; CreateIconIndirect made its copy.
        unsafe {
            assert_ne!(DeleteObject(bitmap), 0);
        }
        assert!(!handle.is_null());
        Self(handle)
    }
}
impl Drop for CustomCursor {
    fn drop(&mut self) {
        // SAFETY: only private created cursors, after selecting the borrowed standard arrow.
        unsafe {
            assert_ne!(DestroyCursor(self.0), 0);
        }
    }
}
fn known_colour() -> Vec<u8> {
    (0..32)
        .flat_map(|i| {
            if i % 2 == 0 {
                [32, 64, 128, 128]
            } else {
                [3, 4, 5, 255]
            }
        })
        .collect()
}
fn wait_shape(events: &mpsc::Receiver<FrameEvent>, id: StreamId) -> Option<CursorImage> {
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        match events
            .recv_timeout(until.saturating_duration_since(Instant::now()))
            .expect("owned cursor deadline")
        {
            FrameEvent::Cursor { stream, cursor } if stream == id => return cursor,
            FrameEvent::CursorDefault { stream } if stream == id => {
                panic!("known fixture shape unexpectedly unreadable")
            }
            FrameEvent::Ended { reason, .. } => panic!("own cursor capture ended {reason:?}"),
            _ => {}
        }
    }
}
fn silent(events: &mpsc::Receiver<FrameEvent>, duration: Duration) {
    let until = Instant::now() + duration;
    while Instant::now() < until {
        match events.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok(FrameEvent::Frame { .. }) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Ok(_) => panic!("unexpected cursor/terminal event"),
            Err(error) => panic!("own cursor sink {error}"),
        }
    }
}
fn move_owned(
    hwnd: HWND,
    identity: &Hello,
    expected: [i32; 4],
    point: (i32, i32),
    child: &Child,
    gate: &IoGate,
) {
    limited();
    let epoch = gate.epoch();
    assert!(gate.is_open());
    let mut pid = 0;
    let mut frame = RECT::default();
    // SAFETY: authenticated owned handle only; identity and fresh rectangle checked before input.
    unsafe {
        assert_eq!(GetWindowThreadProcessId(hwnd, &mut pid), identity.tid);
        assert_eq!(pid, identity.pid);
        let mut class = [0u16; 256];
        assert!(GetClassNameW(hwnd, class.as_mut_ptr(), 256) > 0);
        assert_eq!(
            string(&class),
            format!("Crosspane.CursorFixture.{}", identity.nonce)
        );
        let mut created = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let mut status = 0;
        assert_ne!(
            GetProcessTimes(
                child.as_raw_handle(),
                &mut created,
                &mut exit,
                &mut kernel,
                &mut user
            ),
            0
        );
        assert_eq!(
            (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime),
            identity.created
        );
        assert_ne!(GetExitCodeProcess(child.as_raw_handle(), &mut status), 0);
        assert_eq!(status, STILL_ACTIVE as u32);
        assert_eq!(
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                (&mut frame as *mut RECT).cast(),
                size_of::<RECT>() as u32
            ),
            0
        );
        assert_eq!(coordinates(frame), expected);
        assert!(
            point.0 > frame.left
                && point.0 < frame.right - 1
                && point.1 > frame.top
                && point.1 < frame.bottom - 1
        );
        let left = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let top = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        assert!(width > 1 && height > 1);
        let x = i64::from(point.0) - i64::from(left);
        let y = i64::from(point.1) - i64::from(top);
        assert!(x >= 0 && y >= 0 && x < i64::from(width) && y < i64::from(height));
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: (x * 65535 / i64::from(width - 1)) as i32,
                    dy: (y * 65535 / i64::from(height - 1)) as i32,
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    dwExtraInfo: 0x43555253,
                    ..Default::default()
                },
            },
        };
        // Allowed root metadata only. Abort if an owner window now occludes the intended point.
        assert_eq!(
            GetAncestor(
                WindowFromPoint(POINT {
                    x: point.0,
                    y: point.1
                }),
                GA_ROOT
            ),
            hwnd
        );
        assert!(gate.is_open() && gate.epoch() == epoch);
        assert_eq!(SendInput(1, &input, size_of::<INPUT>() as i32), 1);
    }
    let until = Instant::now() + Duration::from_secs(1);
    loop {
        let mut cursor = CURSORINFO {
            cbSize: size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: allowed global guard metadata only; no foreign HWND field queries.
        unsafe {
            assert_ne!(GetCursorInfo(&mut cursor), 0);
            if GetAncestor(WindowFromPoint(cursor.ptScreenPos), GA_ROOT) == hwnd {
                assert!(
                    cursor.ptScreenPos.x >= expected[0]
                        && cursor.ptScreenPos.x < expected[2]
                        && cursor.ptScreenPos.y >= expected[1]
                        && cursor.ptScreenPos.y < expected[3]
                );
                break;
            }
        }
        assert!(Instant::now() < until, "owned fixture pointer admission");
        thread::sleep(Duration::from_millis(5));
    }
}
fn diagnose_owned(hello: &Hello, child: &Child) {
    limited();
    let own = [hello.hwnd as usize as HWND, hello.outside as usize as HWND];
    let mut created = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let mut status = 0;
    // SAFETY: held handle to our authenticated child, no foreign process query.
    unsafe {
        assert_ne!(
            GetProcessTimes(
                child.as_raw_handle(),
                &mut created,
                &mut exit,
                &mut kernel,
                &mut user
            ),
            0
        );
        assert_eq!(
            (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime),
            hello.created
        );
        assert_ne!(GetExitCodeProcess(child.as_raw_handle(), &mut status), 0);
        assert_eq!(status, STILL_ACTIVE as u32);
        assert_ne!(
            AreDpiAwarenessContextsEqual(
                GetThreadDpiAwarenessContext(),
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2
            ),
            0
        );
    }
    for (index, hwnd) in own.into_iter().enumerate() {
        let mut pid = 0;
        let mut class = [0u16; 256];
        let mut frame = RECT::default();
        let mut client = RECT::default();
        let mut cloak = 0u32;
        // SAFETY: identity is validated before any other field of these owned HWNDs is read.
        unsafe {
            assert_eq!(GetWindowThreadProcessId(hwnd, &mut pid), hello.tid);
            assert_eq!(pid, hello.pid);
            assert!(GetClassNameW(hwnd, class.as_mut_ptr(), 256) > 0);
            assert_eq!(
                string(&class),
                format!("Crosspane.CursorFixture.{}", hello.nonce)
            );
            assert_ne!(IsWindow(hwnd), 0);
            assert_eq!(
                DwmGetWindowAttribute(
                    hwnd,
                    DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                    (&mut frame as *mut RECT).cast(),
                    size_of::<RECT>() as u32
                ),
                0
            );
            assert_ne!(GetClientRect(hwnd, &mut client), 0);
            let cloak_ok = DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED as u32,
                (&mut cloak as *mut u32).cast(),
                size_of::<u32>() as u32,
            ) == 0;
            println!(
                "OWN_METADATA fixture={index} limited=true identity=true pmv2=true visible={} enabled={} cloak_known={cloak_ok} cloak={cloak} exstyle={} frame={:?} client={:?} dpi={}",
                IsWindowVisible(hwnd) != 0,
                IsWindowEnabled(hwnd) != 0,
                GetWindowLongPtrW(hwnd, GWL_EXSTYLE),
                coordinates(frame),
                coordinates(client),
                GetDpiForWindow(hwnd)
            );
            for (point_index, (x, y)) in [
                (client.right / 2, client.bottom / 2),
                (client.right / 4, client.bottom / 4),
                (client.right * 3 / 4, client.bottom * 3 / 4),
            ]
            .into_iter()
            .enumerate()
            {
                let mut point = POINT { x, y };
                assert_ne!(ClientToScreen(hwnd, &mut point), 0);
                let inside = point.x > frame.left
                    && point.x < frame.right - 1
                    && point.y > frame.top
                    && point.y < frame.bottom - 1;
                // Allowed point/root metadata ONLY: never query the returned foreign HWND.
                let root = GetAncestor(WindowFromPoint(point), GA_ROOT);
                println!(
                    "OWN_POINT fixture={index} point={point_index} position=({}, {}) inside={inside} root_self={} root_other_owned={} root_null={} root_foreign={}",
                    point.x,
                    point.y,
                    root == hwnd,
                    root == own[1 - index],
                    root.is_null(),
                    !root.is_null() && root != own[0] && root != own[1]
                );
            }
        }
    }
}
#[test]
#[ignore = "Limited authenticated cursor fixtures only; explicit win-gui opt-in"]
fn limited_owned_wgc_cursor_shapes_hide_outside_and_gdi_cleanup() {
    assert_eq!(
        std::env::var("CROSSPANE_WINDOWS_CURSOR_PROBE").as_deref(),
        Ok("1")
    );
    limited();
    let _watchdog = Watchdog::new(35);
    // SAFETY: physical coordinate thread context, restored before exit.
    let prior = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    assert!(!prior.is_null());
    let nonce = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let mut fixture = Fixture::launch(&nonce);
    let hello: Hello = serde_json::from_str(&fixture.response("HELLO ")).unwrap();
    assert_eq!(hello.nonce, nonce);
    assert_eq!(hello.pid, fixture.child.id());
    assert!(hello.hwnd != 0 && hello.outside != 0 && hello.hwnd != hello.outside);
    let hwnd = hello.hwnd as usize as HWND;
    let outside = hello.outside as usize as HWND;
    // Both handles authenticated from our child pipe/nonce; the backend admits ONLY the target.
    for own in [hwnd, outside] {
        let mut pid = 0;
        let mut class = [0u16; 256];
        // SAFETY: authenticated own handles only, no foreign title/content enumeration.
        unsafe {
            assert_eq!(GetWindowThreadProcessId(own, &mut pid), hello.tid);
            assert_eq!(pid, hello.pid);
            assert!(GetClassNameW(own, class.as_mut_ptr(), 256) > 0);
        }
        assert_eq!(string(&class), format!("Crosspane.CursorFixture.{nonce}"));
    }
    fixture.authenticated = Some([hwnd as usize, outside as usize]);
    if std::env::var("CROSSPANE_CURSOR_DIAGNOSTIC").as_deref() == Ok("1") {
        fixture.command("metadata");
        diagnose_owned(&hello, &fixture.child);
        fixture.command("close");
        let gdi = fixture.response("GDI ");
        fixture.finish();
        // SAFETY: formerly authenticated own HWND existence only; own context restoration.
        unsafe {
            assert_eq!(IsWindow(hwnd), 0);
            assert_eq!(IsWindow(outside), 0);
            SetThreadDpiAwarenessContext(prior);
        }
        println!(
            "OWN_METADATA PASS SendInput_calls=0 gdi_fixture={gdi} cleanup=two_windows/job/fixture"
        );
        return;
    }
    let probe = MonitorProbe {
        device_path: "owned-cursor-monitor".into(),
        name: hello.monitor.name.clone(),
        rc_monitor: hello.monitor.bounds,
        rc_work: hello.monitor.work,
        primary: true,
        dpi: hello.monitor.dpi,
        refresh_millihz: 60000,
        edid: None,
        twin: false,
        quarter_turns: 0,
    };
    let source = window::WindowsWindowSource::for_fixture(
        Arc::new(Mutex::new(DisplayIds::default())),
        Arc::new(move || Ok(vec![probe.clone()])),
        Identity {
            hwnd: hello.hwnd,
            pid: hello.pid,
            tid: hello.tid,
            process_created: hello.created,
        },
    )
    .unwrap();
    let source = Verified::new(source, window::WindowsWindowSource::stop_verified, "source");
    let target = source.windows().unwrap()[0].id;
    let gate = IoGate::new();
    gate.set_engine_permits(true);
    gate.set_session_permits(true);
    let capture =
        frame_capture::WindowsFrameCapture::new(Arc::clone(&gate), source.resolver()).unwrap();
    let mut capture = Verified::new(
        capture,
        frame_capture::WindowsFrameCapture::stop_verified,
        "capture",
    );
    // SAFETY: own process GDI count only, no object enumeration.
    let gdi_before = unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };
    let (send, events) = mpsc::channel();
    let sink: Arc<dyn crosspane_platform::EventSink<FrameEvent>> = Arc::new(move |event| {
        let _ = send.send(event);
    });
    let centre = |r: [i32; 4]| ((r[0] + r[2]) / 2, (r[1] + r[3]) / 2);
    move_owned(
        outside,
        &hello,
        hello.outside_frame,
        centre(hello.outside_frame),
        &fixture.child,
        &gate,
    );
    let stream = capture
        .start(CaptureTarget::Window(target), None, 10, sink)
        .unwrap();
    silent(&events, Duration::from_millis(120));
    move_owned(
        hwnd,
        &hello,
        hello.frame,
        centre(hello.frame),
        &fixture.child,
        &gate,
    );
    let arrow = wait_shape(&events, stream).unwrap();
    assert!(arrow.size.width <= 256 && arrow.size.height <= 256);
    silent(&events, Duration::from_millis(100));
    fixture.command("ibeam");
    let ibeam = wait_shape(&events, stream).unwrap();
    assert!(ibeam != arrow, "I-beam did not change the arrow shape");
    fixture.command("custom");
    let image = wait_shape(&events, stream).unwrap();
    assert_eq!(image.size, crosspane_types::geom::PixelSize::new(8, 4));
    assert_eq!(image.hotspot, (3, 1));
    let expected: Vec<u8> = (0..32)
        .flat_map(|i| {
            if i % 2 == 0 {
                [64, 128, 255, 128]
            } else {
                [3, 4, 5, 255]
            }
        })
        .collect();
    assert!(
        image.pixels.as_ref() == expected,
        "known custom cursor bytes differ"
    );
    fixture.command("mono");
    let mono = wait_shape(&events, stream).unwrap();
    assert_eq!(mono.size, crosspane_types::geom::PixelSize::new(4, 1));
    assert_eq!(mono.hotspot, (1, 0));
    assert!(
        mono.pixels.as_ref()
            == [
                0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 0, 128, 128, 128, 255
            ],
        "known monochrome cursor bytes differ"
    );
    fixture.command("hide");
    assert!(wait_shape(&events, stream).is_none());
    silent(&events, Duration::from_millis(100));
    move_owned(
        outside,
        &hello,
        hello.outside_frame,
        centre(hello.outside_frame),
        &fixture.child,
        &gate,
    );
    fixture.command("custom");
    silent(&events, Duration::from_millis(120));
    move_owned(
        hwnd,
        &hello,
        hello.frame,
        centre(hello.frame),
        &fixture.child,
        &gate,
    );
    assert!(
        wait_shape(&events, stream) == Some(image),
        "reentry shape differs"
    );
    gate.set_engine_permits(false);
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        match events
            .recv_timeout(until.saturating_duration_since(Instant::now()))
            .unwrap()
        {
            FrameEvent::Ended { reason, .. } => {
                assert_eq!(reason, StreamEndReason::Blocked);
                break;
            }
            FrameEvent::Cursor { .. } | FrameEvent::CursorDefault { .. } => {
                panic!("cursor after gate close")
            }
            _ => {}
        }
    }
    assert!(capture.finish());
    source.freeze_fixture_fields().unwrap();
    fixture.command("close");
    let gdi = fixture.response("GDI ");
    assert!(source.finish());
    fixture.finish();
    // SAFETY: owned process object count and formerly authenticated HWND existence only.
    let gdi_after = unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };
    assert!(gdi_after <= gdi_before, "own GDI objects leaked");
    // SAFETY: existence checks only for both authenticated fixture HWNDs, own thread context.
    unsafe {
        assert_eq!(IsWindow(hwnd), 0);
        assert_eq!(IsWindow(outside), 0);
        SetThreadDpiAwarenessContext(prior);
    }
    println!(
        "OWNED_CURSOR arrow/Ibeam/custom/mono/hide/reentry/outside/gate PASS density=1:1 dpi={} gdi_controller={}->{} gdi_fixture={} ShowCursor_calls=0 cleanup=capture/source/two_windows/job/fixture",
        hello.monitor.dpi, gdi_before, gdi_after, gdi
    );
}
// SAFETY: procedure receives messages only for the private fixture class/own HWND.
unsafe extern "system" fn fixture_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: paints only this fixture's own client DC; every brush/DC is released.
    unsafe {
        if message == WM_SETCURSOR {
            SetCursor(OWN_CURSOR.with(|c| c.get()));
            return 1;
        }
        if message == WM_PAINT {
            let mut paint = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut paint);
            let mut rect = RECT::default();
            GetClientRect(hwnd, &mut rect);
            let middle = rect.right / 2;
            let red = CreateSolidBrush(0x000000ff);
            let green = CreateSolidBrush(0x0000ff00);
            let left = RECT {
                right: middle,
                ..rect
            };
            let right = RECT {
                left: middle,
                ..rect
            };
            FillRect(dc, &left, red);
            FillRect(dc, &right, green);
            DeleteObject(red);
            DeleteObject(green);
            EndPaint(hwnd, &paint);
            OWN_PAINTED.with(|painted| {
                let mut painted = painted.borrow_mut();
                if !painted.contains(&(hwnd as usize)) {
                    painted.push(hwnd as usize);
                }
            });
            return 0;
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}
struct OwnedWindow {
    hwnd: HWND,
    class: Vec<u16>,
    instance: HINSTANCE,
}
impl Drop for OwnedWindow {
    fn drop(&mut self) {
        // SAFETY: destroys only our created fixture and unregisters only its private class.
        unsafe {
            if !self.hwnd.is_null() {
                DestroyWindow(self.hwnd);
            }
            if !self.class.is_empty() {
                UnregisterClassW(self.class.as_ptr(), self.instance);
            }
        }
    }
}

#[test]
#[ignore = "private child colour server, invoked only by Limited CURSOR probe"]
fn cursor_fixture_server() {
    limited();
    let _watchdog = Watchdog::new(32);
    let nonce = std::env::var("CROSSPANE_CURSOR_FIXTURE_NONCE").expect("private child nonce");
    assert!(nonce.len() < 100 && nonce.bytes().all(|b| b.is_ascii_digit() || b == b'-'));
    let class = wide(&format!("Crosspane.CursorFixture.{nonce}"));
    // SAFETY: sets PMv2 on this fixture thread only, not global display configuration.
    let previous =
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    assert!(!previous.is_null());
    // SAFETY: borrows this fixture executable's module handle.
    let instance = unsafe { GetModuleHandleW(null()) };
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(fixture_proc),
        hInstance: instance,
        lpszClassName: class.as_ptr(),
        ..Default::default()
    };
    // SAFETY: stable private class and procedure for this process only.
    assert_ne!(unsafe { RegisterClassExW(&wc) }, 0);
    // SAFETY: creates our own titled top-level fixture, with no external parent or menu.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST,
            class.as_ptr(),
            wide("owned colour fixture").as_ptr(),
            WS_OVERLAPPEDWINDOW,
            80,
            80,
            480,
            360,
            null_mut(),
            null_mut(),
            instance,
            null(),
        )
    };
    assert!(!hwnd.is_null());
    let mut owned = OwnedWindow {
        hwnd,
        class: class.clone(),
        instance,
    };
    // SAFETY: second private top-level fixture used only for the outside-content test.
    let outside_hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST,
            class.as_ptr(),
            wide("owned outside cursor fixture").as_ptr(),
            WS_OVERLAPPEDWINDOW,
            580,
            80,
            280,
            360,
            null_mut(),
            null_mut(),
            instance,
            null(),
        )
    };
    assert!(!outside_hwnd.is_null());
    let mut outside = OwnedWindow {
        hwnd: outside_hwnd,
        class: Vec::new(),
        instance,
    };
    // SAFETY: own fixture process object count only, before creating any custom cursor.
    let gdi_before = unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };
    let custom = CustomCursor::colour();
    let mono = CustomCursor::mono();
    // SAFETY: own thread, borrowed standard shared cursor; never destroyed.
    let arrow = unsafe { LoadCursorW(null_mut(), IDC_ARROW) };
    // SAFETY: borrowed standard shared cursor selected only by our fixture, never destroyed.
    let ibeam = unsafe { LoadCursorW(null_mut(), IDC_IBEAM) };
    assert!(!arrow.is_null() && !ibeam.is_null());
    OWN_CURSOR.with(|c| c.set(arrow));
    // SAFETY: shows/repaints only our fixtures without activating any window.
    unsafe {
        let mut startup = STARTUPINFOW::default();
        GetStartupInfoW(&mut startup);
        println!(
            "OWN_STARTUP show_override={} show={} pmv2={}",
            startup.dwFlags & STARTF_USESHOWWINDOW != 0,
            startup.wShowWindow,
            AreDpiAwarenessContextsEqual(
                GetThreadDpiAwarenessContext(),
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2
            ) != 0
        );
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        ShowWindow(outside_hwnd, SW_SHOWNOACTIVATE);
        UpdateWindow(hwnd);
        UpdateWindow(outside_hwnd);
    }
    let mut pid = 0;
    let mut created = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let mut frame = RECT::default();
    let mut outside_frame = RECT::default();
    let mut monitor = MONITORINFOEXW::default();
    monitor.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: all native reads concern this process's own window/process/monitor facts.
    let hello = unsafe {
        let tid = GetWindowThreadProcessId(hwnd, &mut pid);
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
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                (&mut frame as *mut RECT).cast(),
                size_of::<RECT>() as u32
            ),
            0
        );
        assert_ne!(
            GetMonitorInfoW(
                MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST),
                (&mut monitor as *mut MONITORINFOEXW).cast()
            ),
            0
        );
        assert_eq!(
            DwmGetWindowAttribute(
                outside_hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                (&mut outside_frame as *mut RECT).cast(),
                size_of::<RECT>() as u32
            ),
            0
        );
        Hello {
            outside: outside_hwnd as usize as u64,
            outside_frame: coordinates(outside_frame),
            nonce,
            hwnd: hwnd as usize as u64,
            pid,
            tid,
            created: (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime),
            frame: coordinates(frame),
            monitor: Monitor {
                name: string(&monitor.szDevice),
                bounds: coordinates(monitor.monitorInfo.rcMonitor),
                work: coordinates(monitor.monitorInfo.rcWork),
                dpi: GetDpiForWindow(hwnd),
            },
        }
    };
    println!("HELLO {}", serde_json::to_string(&hello).unwrap());
    std::io::stdout().flush().unwrap();
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else {
                break;
            };
            if send.send(line).is_err() {
                break;
            }
        }
    });
    let until = Instant::now() + Duration::from_secs(30);
    let mut paint_at = Instant::now();
    while Instant::now() < until {
        let mut message = MSG::default();
        // SAFETY: bounded pump for this private fixture thread's own queue.
        unsafe {
            for _ in 0..128 {
                if PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) == 0 {
                    break;
                }
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        if Instant::now() >= paint_at {
            // SAFETY: repaint only this known-colour fixture, without input or activation.
            unsafe {
                InvalidateRect(hwnd, null(), 0);
                UpdateWindow(hwnd);
            }
            paint_at = Instant::now() + Duration::from_millis(16);
        }
        if let Ok(command) = receive.try_recv() {
            // SAFETY: mutations are exclusively to the fixture created/owned above.
            unsafe {
                match command.as_str() {
                    "metadata" => {
                        let painted = OWN_PAINTED.with(|painted| painted.borrow().len());
                        println!(
                            "OWN_READINESS visible_target={} visible_outside={} painted_windows={painted}",
                            IsWindowVisible(hwnd) != 0,
                            IsWindowVisible(outside_hwnd) != 0
                        );
                    }
                    "arrow" | "ibeam" | "custom" | "mono" | "hide" => {
                        let cursor = match command.as_str() {
                            "arrow" => arrow,
                            "ibeam" => ibeam,
                            "custom" => custom.0,
                            "mono" => mono.0,
                            _ => null_mut(),
                        };
                        OWN_CURSOR.with(|c| c.set(cursor));
                        SetCursor(cursor);
                    }
                    "close" => {
                        OWN_CURSOR.with(|c| c.set(arrow));
                        SetCursor(arrow);
                        assert_ne!(DestroyWindow(outside_hwnd), 0);
                        outside.hwnd = null_mut();
                        assert_ne!(DestroyWindow(hwnd), 0);
                        owned.hwnd = null_mut();
                    }
                    _ => panic!("unknown private fixture command"),
                }
            }
            println!("ACK {command}");
            std::io::stdout().flush().unwrap();
            if command == "close" {
                break;
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(owned.hwnd.is_null(), "fixture internal deadline exceeded");
    drop(mono);
    drop(custom);
    // SAFETY: own fixture process GDI count only, no foreign resource enumeration.
    let gdi_after = unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };
    println!("GDI {} {}", gdi_before, gdi_after);
    std::io::stdout().flush().unwrap();
    assert!(gdi_after <= gdi_before);
    drop(outside);
    drop(owned);
    // SAFETY: restores this thread's original DPI context only.
    unsafe { SetThreadDpiAwarenessContext(previous) };
}
