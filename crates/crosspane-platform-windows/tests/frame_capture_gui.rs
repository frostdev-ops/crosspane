//! Limited, authenticated colour fixture only; never run through elevated cargo.
#![cfg(windows)]
#![allow(unsafe_code, clippy::unwrap_used, clippy::expect_used)]

use crosspane_platform::{
    CaptureTarget, Frame, FrameCapture, FrameEvent, IoGate, PlatformError, StreamEndReason,
    StreamId, WindowSource,
};
pub use crosspane_platform_windows::clock;
use crosspane_platform_windows::model::{
    self,
    geometry::{DisplayIds, MonitorProbe},
    window::Identity,
};
use crosspane_types::{geom::PixelRect, id::WindowId};
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
    System::{
        Diagnostics::ToolHelp::*, JobObjects::*, LibraryLoader::GetModuleHandleW, Threading::*,
    },
    UI::{HiDpi::*, WindowsAndMessaging::*},
};

#[path = "../src/cursor.rs"]
mod cursor;
#[path = "../src/frame_capture.rs"]
mod frame_capture;
#[path = "../src/window.rs"]
mod window;

#[test]
fn capture_facade_is_send_and_uses_the_authoritative_resolver() {
    fn send<T: Send>() {}
    send::<frame_capture::WindowsFrameCapture>();
    let _: fn(
        Arc<IoGate>,
        window::WindowResolver,
    ) -> Result<frame_capture::WindowsFrameCapture, PlatformError> =
        frame_capture::WindowsFrameCapture::new;
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
        "WGC probe requires Limited win-gui route"
    );
}
struct Watchdog(mpsc::Sender<()>);
impl Watchdog {
    fn new(seconds: u64) -> Self {
        let (send, receive) = mpsc::channel();
        thread::spawn(move || {
            if receive.recv_timeout(Duration::from_secs(seconds)).is_err() {
                eprintln!("OWNED WGC WATCHDOG EXPIRED");
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
                "colour_fixture_server",
                "--nocapture",
            ])
            .env("CROSSPANE_WGC_FIXTURE_NONCE", nonce)
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
            eprintln!("OWNED_WGC failure cleanup: job_closed={closed} fixture_exited={exited}");
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
            eprintln!("OWNED_WGC failure cleanup: {}_joined={stopped}", self.name);
        }
    }
}

#[test]
fn failure_cleanup_settles_each_owned_worker_exactly_once() {
    fn stop(calls: Arc<std::sync::atomic::AtomicUsize>) -> bool {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        true
    }
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let result = std::panic::catch_unwind({
        let calls = Arc::clone(&calls);
        move || {
            let _capture = Verified::new(Arc::clone(&calls), stop, "fake_capture");
            let _source = Verified::new(calls, stop, "fake_source");
            panic!("fake capture failure");
        }
    });
    assert!(result.is_err());
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(Verified::new(Arc::clone(&calls), stop, "fake_capture").finish());
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
}

#[test]
#[ignore = "Limited own-executable inventory only, no window/capture queries"]
fn limited_own_capture_process_cleanup_inventory() {
    assert_eq!(
        std::env::var("CROSSPANE_WINDOWS_WGC_CLEANUP").as_deref(),
        Ok("1")
    );
    limited();
    let _watchdog = Watchdog::new(5);
    let path = std::env::current_exe().unwrap();
    let own_name: Vec<u16> = path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .encode_utf16()
        .collect();
    let prior = std::env::var("CROSSPANE_WINDOWS_WGC_PRIOR_EXE").unwrap();
    assert_eq!(prior, "frame_capture_gui-1032e4c3bf023542.exe");
    let prior_name: Vec<u16> = prior.encode_utf16().collect();
    let mut others = 0;
    // SAFETY: read-only process snapshot; compare names to OUR executable before using IDs,
    // never query foreign processes/windows or log any enumeration fields.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        assert_ne!(snapshot, INVALID_HANDLE_VALUE);
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut more = Process32FirstW(snapshot, &mut entry);
        while more != 0 {
            let end = entry
                .szExeFile
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(entry.szExeFile.len());
            if (entry.szExeFile[..end] == own_name || entry.szExeFile[..end] == prior_name)
                && entry.th32ProcessID != GetCurrentProcessId()
            {
                others += 1;
            }
            more = Process32NextW(snapshot, &mut entry);
        }
        assert_eq!(GetLastError(), ERROR_NO_MORE_FILES);
        assert_ne!(CloseHandle(snapshot), 0);
    }
    assert_eq!(others, 0, "own probe/fixture process residue");
    println!("OWNED_WGC process_cleanup other_own_executables=0");
}
fn next_frame(events: &mpsc::Receiver<FrameEvent>, stream: StreamId, stage: &str) -> Frame {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        match events
            .recv_timeout(until.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|_| panic!("own WGC frame deadline at {stage}"))
        {
            FrameEvent::Frame { stream: id, frame } if id == stream => return frame,
            FrameEvent::Ended { stream: id, reason } if id == stream => {
                panic!("own WGC ended before frame at {stage}: {reason:?}")
            }
            _ => {}
        }
    }
}
fn wait_end(events: &mpsc::Receiver<FrameEvent>, stream: StreamId, expected: StreamEndReason) {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        match events
            .recv_timeout(until.saturating_duration_since(Instant::now()))
            .expect("own WGC terminal deadline")
        {
            FrameEvent::Ended { stream: id, reason } if id == stream => {
                assert_eq!(reason, expected);
                break;
            }
            _ => {}
        }
    }
}
fn sample(frame: &Frame, x: u32, y: u32) -> [u8; 3] {
    assert!(x < frame.size.width && y < frame.size.height);
    frame
        .with_pixels(|pixels, stride| {
            let offset = y as usize * stride as usize + x as usize * 4;
            pixels[offset..offset + 3].try_into().unwrap()
        })
        .unwrap()
}
fn wait_visible(source: &window::WindowsWindowSource) {
    let until = Instant::now() + Duration::from_secs(2);
    while source.windows().unwrap()[0].state != crosspane_platform::WindowState::Normal {
        assert!(Instant::now() < until, "fixture restore deadline");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "Limited authenticated colour fixture only; explicit win-gui opt-in"]
fn limited_owned_wgc_pixels_crop_pacing_resize_gate_minimize_close() {
    assert_eq!(
        std::env::var("CROSSPANE_WINDOWS_WGC_PROBE").as_deref(),
        Ok("1")
    );
    limited();
    let _watchdog = Watchdog::new(35);
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
    assert_ne!(hello.hwnd, 0);
    // Authentication via the trusted child/pipe/nonce precedes every HWND field API.
    let hwnd = hello.hwnd as usize as HWND;
    let mut pid = 0;
    let mut class = [0u16; 256];
    // SAFETY: only the authenticated fixture HWND is queried.
    unsafe {
        assert_eq!(GetWindowThreadProcessId(hwnd, &mut pid), hello.tid);
        assert_eq!(pid, hello.pid);
        assert!(GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32) > 0);
    }
    assert_eq!(string(&class), format!("Crosspane.WgcFixture.{nonce}"));
    let probe = MonitorProbe {
        device_path: "owned-wgc-monitor".into(),
        name: hello.monitor.name,
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
    let windows = source.windows().unwrap();
    assert_eq!(windows.len(), 1);
    let target = windows[0].id;
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
    let (send, events) = mpsc::channel();
    let sink: Arc<dyn crosspane_platform::EventSink<FrameEvent>> = Arc::new(move |event| {
        let _ = send.send(event);
    });
    assert!(matches!(
        capture.start(
            CaptureTarget::Window(WindowId(target.0 ^ u64::MAX)),
            None,
            10,
            Arc::clone(&sink)
        ),
        Err(PlatformError::NotFound)
    ));
    assert!(matches!(
        capture.start(
            CaptureTarget::Display(crosspane_types::id::DisplayId(1)),
            None,
            10,
            Arc::clone(&sink)
        ),
        Err(PlatformError::Unsupported(_))
    ));
    let stream = capture
        .start(CaptureTarget::Window(target), None, 10, Arc::clone(&sink))
        .unwrap();
    if std::env::var("CROSSPANE_WINDOWS_WGC_DESCRIPTOR").as_deref() == Ok("1") {
        thread::sleep(Duration::from_millis(200));
        fixture.command("resize");
        thread::sleep(Duration::from_millis(700));
        let reason = match capture.stop(stream) {
            Ok(()) => StreamEndReason::Requested,
            Err(PlatformError::NotFound) => StreamEndReason::Failed,
            Err(error) => panic!("owned diagnostic stop: {error}"),
        };
        wait_end(&events, stream, reason);
        assert!(capture.finish());
        source.freeze_fixture_fields().unwrap();
        fixture.command("close");
        assert!(source.finish());
        fixture.finish();
        // SAFETY: own authenticated fixture existence only, never a foreign query.
        assert_eq!(unsafe { IsWindow(hwnd) }, 0);
        println!("OWNED_WGC descriptor_only no_pixel_copy cleanup=capture/source/job/fixture");
        return;
    }
    let first = next_frame(&events, stream, "initial");
    assert!(first.native().is_some());
    assert!(first.cpu_pixels().is_none());
    assert_eq!(first.size.width, (hello.frame[2] - hello.frame[0]) as u32);
    assert_eq!(first.size.height, (hello.frame[3] - hello.frame[1]) as u32);
    assert_eq!(
        sample(&first, first.size.width / 4, first.size.height / 2),
        [0, 0, 255]
    );
    assert_eq!(
        sample(&first, first.size.width * 3 / 4, first.size.height / 2),
        [0, 255, 0]
    );
    let original = first.size;
    let mut last = first.at;
    drop(first);
    for _ in 0..5 {
        let frame = next_frame(&events, stream, "pacing");
        assert!(
            frame.at.as_nanos() - last.as_nanos() >= 100_000_000,
            "delivery faster than 10 fps"
        );
        last = frame.at;
    }
    fixture.command("resize");
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let frame = next_frame(&events, stream, "resize");
        if frame.size != original {
            assert!(frame.size.width > original.width);
            break;
        }
        assert!(Instant::now() < until, "WGC recreation deadline");
    }
    capture
        .set_crop(
            stream,
            Some(PixelRect::new((20, 40).into(), (80, 100).into())),
        )
        .unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    let cropped = loop {
        let frame = next_frame(&events, stream, "crop");
        if frame.size.width == 60 && frame.size.height == 60 {
            break frame;
        }
        assert!(Instant::now() < until, "crop deadline");
    };
    assert_eq!(sample(&cropped, 30, 30), [0, 0, 255]);
    capture.stop(stream).unwrap();
    wait_end(&events, stream, StreamEndReason::Requested);
    let stream = capture
        .start(CaptureTarget::Window(target), None, 10, Arc::clone(&sink))
        .unwrap();
    drop(next_frame(&events, stream, "gate close"));
    gate.set_engine_permits(false);
    assert!(matches!(
        cropped.with_pixels(|_, _| ()),
        Err(PlatformError::Locked)
    ));
    drop(cropped);
    wait_end(&events, stream, StreamEndReason::Blocked);
    gate.set_engine_permits(true);
    let stream = capture
        .start(CaptureTarget::Window(target), None, 10, Arc::clone(&sink))
        .unwrap();
    drop(next_frame(&events, stream, "minimize"));
    fixture.command("minimize");
    wait_end(&events, stream, StreamEndReason::Failed);
    fixture.command("restore");
    wait_visible(&source);
    let stream = capture
        .start(CaptureTarget::Window(target), None, 10, Arc::clone(&sink))
        .unwrap();
    drop(next_frame(&events, stream, "close"));
    source.freeze_fixture_fields().unwrap();
    fixture.command("close");
    wait_end(&events, stream, StreamEndReason::TargetGone);
    assert!(capture.finish());
    assert!(source.finish());
    fixture.finish();
    // SAFETY: verifies closure only for the previously authenticated fixture handle.
    let exists = unsafe { IsWindow(hwnd) };
    assert_eq!(exists, 0);
    println!(
        "OWNED_WGC native=BGRA8 size/pixels=ok known_samples=red,green crop=60x60 fps<=10 resize=recreated stop=Requested gate=Blocked minimize=Failed close=TargetGone cleanup=source/capture/job/fixture border=required"
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
            UnregisterClassW(self.class.as_ptr(), self.instance);
        }
    }
}

#[test]
#[ignore = "private child colour server, invoked only by Limited WGC probe"]
fn colour_fixture_server() {
    limited();
    let _watchdog = Watchdog::new(32);
    let nonce = std::env::var("CROSSPANE_WGC_FIXTURE_NONCE").expect("private child nonce");
    assert!(nonce.len() < 100 && nonce.bytes().all(|b| b.is_ascii_digit() || b == b'-'));
    let class = wide(&format!("Crosspane.WgcFixture.{nonce}"));
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
            0,
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
        class,
        instance,
    };
    // SAFETY: shows/repaints only our fixture without activating any window.
    unsafe {
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        UpdateWindow(hwnd);
    }
    let mut pid = 0;
    let mut created = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let mut frame = RECT::default();
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
        Hello {
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
                    "resize" => {
                        assert_ne!(
                            SetWindowPos(
                                hwnd,
                                null_mut(),
                                0,
                                0,
                                600,
                                420,
                                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE
                            ),
                            0
                        );
                        InvalidateRect(hwnd, null(), 0);
                        UpdateWindow(hwnd);
                    }
                    "minimize" => {
                        ShowWindow(hwnd, SW_MINIMIZE);
                    }
                    "restore" => {
                        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                        InvalidateRect(hwnd, null(), 0);
                        UpdateWindow(hwnd);
                    }
                    "close" => {
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
    drop(owned);
    // SAFETY: restores this thread's original DPI context only.
    unsafe { SetThreadDpiAwarenessContext(previous) };
}
