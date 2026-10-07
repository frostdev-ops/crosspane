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
        Self::launch_server(nonce, "colour_fixture_server", None)
    }
    fn launch_popup(nonce: &str, kind: &str) -> Self {
        assert!(matches!(kind, "menu" | "tooltip" | "dialog"));
        Self::launch_server(nonce, "popup_fixture_server", Some(kind))
    }
    fn launch_server(nonce: &str, server: &str, kind: Option<&str>) -> Self {
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
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--ignored", "--exact", server, "--nocapture"])
            .env("CROSSPANE_WGC_FIXTURE_NONCE", nonce)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(kind) = kind {
            command.env("CROSSPANE_WGC_POPUP_FIXTURE_KIND", kind);
        }
        let child = command.spawn();
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
        if kind.is_some() {
            let mut in_job = 0;
            // SAFETY: original just-created process handle and this exact private job;
            // child server remains behind START and has not created a window.
            assert_ne!(
                // SAFETY: original retained child and exact private job; START still fences GUI creation.
                unsafe { IsProcessInJob(fixture.child.as_raw_handle(), job, &mut in_job) },
                0
            );
            assert_ne!(in_job, 0);
            // SAFETY: query liveness only on the original retained child handle.
            assert_eq!(
                // SAFETY: query only the original retained child handle for a zero-time liveness check.
                unsafe { WaitForSingleObject(fixture.child.as_raw_handle(), 0) },
                WAIT_TIMEOUT
            );
        }
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
        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: only this retained private job is queried after the original child exit.
        assert_ne!(
            // SAFETY: query only the retained private job after original child exit; buffer has the exact public size.
            unsafe {
                QueryInformationJobObject(
                    self.job,
                    JobObjectBasicAccountingInformation,
                    (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                    size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    null_mut(),
                )
            },
            0
        );
        assert_eq!(
            accounting.ActiveProcesses, 0,
            "original private job still active"
        );
        println!("OWNED_WGC original_child_exit=0 private_job_active=0");
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

#[derive(Serialize, Deserialize)]
struct PopupHello {
    hwnd: u64,
    pid: u32,
    tid: u32,
    created: u64,
    frame: [i32; 4],
    class: String,
    style: u32,
    ex_style: u32,
    owner: u64,
}
#[repr(C, align(4))]
struct PopupDialogTemplate {
    header: DLGTEMPLATE,
    menu: u16,
    class: u16,
    title: u16,
}
struct PopupSurface {
    hwnd: HWND,
    instance: HINSTANCE,
    class: Vec<u16>,
    registered: bool,
}
impl Drop for PopupSurface {
    fn drop(&mut self) {
        // SAFETY: only this guard's created popup/dialog and private registered class.
        unsafe {
            if !self.hwnd.is_null() {
                assert_ne!(DestroyWindow(self.hwnd), 0);
            }
            if self.registered {
                assert_ne!(UnregisterClassW(self.class.as_ptr(), self.instance), 0);
            }
        }
    }
}
// SAFETY: registered only for generated owned synthetic popup windows.
unsafe extern "system" fn popup_colour_proc(
    hwnd: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    if message == WM_PAINT {
        // SAFETY: generated own client pixels only; every acquired brush/DC is released.
        unsafe {
            let mut paint = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut paint);
            let mut rect = RECT::default();
            assert_ne!(GetClientRect(hwnd, &mut rect), 0);
            let brush = CreateSolidBrush(0x00ff00ff);
            assert!(!brush.is_null());
            assert_ne!(FillRect(dc, &rect, brush), 0);
            assert_ne!(DeleteObject(brush), 0);
            assert_ne!(EndPaint(hwnd, &paint), 0);
        }
        return 0;
    }
    // SAFETY: unchanged callback arguments for this own private class.
    unsafe { DefWindowProcW(hwnd, message, wp, lp) }
}
// SAFETY: installed only on the real modeless #32770 dialog created from our template.
unsafe extern "system" fn popup_dialog_proc(
    hwnd: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> isize {
    if message == WM_PAINT {
        // SAFETY: generated own dialog pixels; no default/focus message is forwarded.
        let _ = unsafe { popup_colour_proc(hwnd, message, wp, lp) };
        return 1;
    }
    0
}
fn generated_alpha_popup(hwnd: HWND) {
    const WIDTH: usize = 240;
    const HEIGHT: usize = 120;
    struct Dib {
        dc: HDC,
        bitmap: HBITMAP,
        previous: HGDIOBJ,
        bits: *mut std::ffi::c_void,
    }
    impl Drop for Dib {
        fn drop(&mut self) {
            // SAFETY: flush/zero/release only the own memory DC and fixed DIB allocation.
            unsafe {
                GdiFlush();
                if !self.bits.is_null() {
                    std::ptr::write_bytes(self.bits.cast::<u8>(), 0, WIDTH * HEIGHT * 4);
                }
                if !self.previous.is_null() && self.previous as isize != -1 {
                    SelectObject(self.dc, self.previous);
                }
                if !self.bitmap.is_null() {
                    DeleteObject(self.bitmap);
                }
                DeleteDC(self.dc);
            }
        }
    }
    // SAFETY: private memory DC; NULL argument does not acquire/copy a screen DC.
    let dc = unsafe { CreateCompatibleDC(null_mut()) };
    assert!(!dc.is_null());
    let mut dib = Dib {
        dc,
        bitmap: null_mut(),
        previous: null_mut(),
        bits: null_mut(),
    };
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: WIDTH as i32,
            biHeight: -(HEIGHT as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: fixed exact initialized 32bpp top-down DIB descriptor and output pointer.
    dib.bitmap =
        unsafe { CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut dib.bits, null_mut(), 0) };
    assert!(!dib.bitmap.is_null() && !dib.bits.is_null());
    // SAFETY: uniquely written live DIB allocation has exactly WIDTH*HEIGHT*4 bytes.
    let pixels =
        unsafe { std::slice::from_raw_parts_mut(dib.bits.cast::<u8>(), WIDTH * HEIGHT * 4) };
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let alpha = if y < 24 && x < 24 {
                0
            } else if y < 24 && x < 48 {
                128
            } else {
                255
            };
            pixels[(y * WIDTH + x) * 4..(y * WIDTH + x + 1) * 4]
                .copy_from_slice(&[alpha, 0, alpha, alpha]);
        }
    }
    // SAFETY: only our bitmap and memory DC; retain/restore the prior selected object.
    dib.previous = unsafe { SelectObject(dc, dib.bitmap) };
    assert!(!dib.previous.is_null() && dib.previous as isize != -1);
    let extent = SIZE {
        cx: WIDTH as i32,
        cy: HEIGHT as i32,
    };
    let origin = POINT::default();
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    // SAFETY: update only our created layered window from this generated bitmap;
    // NULL destination DC uses default palette, never screen/foreign pixel acquisition.
    assert_ne!(
        // SAFETY: update only our created layered HWND from generated DIB pixels; NULL destination DC acquires no screen pixels.
        unsafe {
            UpdateLayeredWindow(
                hwnd,
                null_mut(),
                null(),
                &extent,
                dc,
                &origin,
                0,
                &blend,
                ULW_ALPHA,
            )
        },
        0
    );
    // SAFETY: flush only this fixture thread's own GDI batch before DIB retirement.
    assert_ne!(unsafe { GdiFlush() }, 0);
}
fn original_created(process: HANDLE) -> u64 {
    let mut created = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: only the exact original retained created process handle is queried.
    assert_ne!(
        // SAFETY: query only this exact original retained process handle into initialized FILETIME outputs.
        unsafe { GetProcessTimes(process, &mut created, &mut exit, &mut kernel, &mut user) },
        0
    );
    (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime)
}

#[test]
#[ignore = "private START-fenced own child only, launched by exactly one released popup selector"]
fn popup_fixture_server() {
    limited();
    let _watchdog = Watchdog::new(60);
    let nonce = std::env::var("CROSSPANE_WGC_FIXTURE_NONCE").unwrap();
    assert!(nonce.len() < 100 && nonce.bytes().all(|b| b.is_ascii_digit() || b == b'-'));
    let kind = std::env::var("CROSSPANE_WGC_POPUP_FIXTURE_KIND").unwrap();
    assert!(matches!(kind.as_str(), "menu" | "tooltip" | "dialog"));
    let mut start = String::new();
    std::io::stdin().read_line(&mut start).unwrap();
    assert_eq!(start, "START\n");
    println!("ACK START");
    std::io::stdout().flush().unwrap();
    // No UI existed before START: parent assigned/corroborated our original process job.
    // SAFETY: thread-only DPI context, restored before this own creator thread returns.
    let previous =
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    assert!(!previous.is_null());
    // SAFETY: own executable's borrowed module handle only.
    let instance = unsafe { GetModuleHandleW(null()) };
    let class = wide(&format!("Crosspane.WgcPopupFixture.{nonce}"));
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(fixture_proc),
        hInstance: instance,
        lpszClassName: class.as_ptr(),
        ..Default::default()
    };
    // SAFETY: register only this child's private root class/callback.
    assert_ne!(unsafe { RegisterClassExW(&wc) }, 0);
    // SAFETY: create our own initially hidden root, no foreign parent/menu/content.
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            wide("generated W2 popup root").as_ptr(),
            WS_OVERLAPPEDWINDOW,
            100,
            100,
            640,
            480,
            null_mut(),
            null_mut(),
            instance,
            null(),
        )
    };
    assert!(!hwnd.is_null());
    let mut root = OwnedWindow {
        hwnd,
        class,
        instance,
    };
    // SAFETY: display/paint own root without activation or injected input.
    unsafe {
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        UpdateWindow(hwnd);
    }
    let mut frame = RECT::default();
    let mut monitor = MONITORINFOEXW::default();
    monitor.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    let mut pid = 0;
    // SAFETY: all queried fields belong to our created own window/process/display metadata.
    let (tid, created) = unsafe {
        let tid = GetWindowThreadProcessId(hwnd, &mut pid);
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
        (tid, original_created(GetCurrentProcess()))
    };
    // SAFETY: DPI query concerns only this exact created own root.
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let hello = Hello {
        nonce,
        hwnd: hwnd as usize as u64,
        pid,
        tid,
        created,
        frame: coordinates(frame),
        monitor: Monitor {
            name: string(&monitor.szDevice),
            bounds: coordinates(monitor.monitorInfo.rcMonitor),
            work: coordinates(monitor.monitorInfo.rcWork),
            dpi,
        },
    };
    println!("HELLO {}", serde_json::to_string(&hello).unwrap());
    std::io::stdout().flush().unwrap();
    let (send, receive) = mpsc::sync_channel(8);
    thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else {
                break;
            };
            if line.len() > 64 || !matches!(line.as_str(), "OPEN" | "DISMISS" | "CLOSE") {
                break;
            }
            if send.send(line).is_err() {
                break;
            }
        }
    });
    let until = Instant::now() + Duration::from_secs(30);
    let mut popup: Option<PopupSurface> = None;
    let mut opened = false;
    while Instant::now() < until {
        // SAFETY: dispatch only this own creator thread's private message queue.
        unsafe {
            let mut message = MSG::default();
            for _ in 0..128 {
                if PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) == 0 {
                    break;
                }
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        if let Ok(command) = receive.try_recv() {
            match command.as_str() {
                "OPEN" => {
                    assert!(!opened);
                    opened = true;
                    let popup_class = wide(&format!("Crosspane.WgcSynthetic.{kind}"));
                    let dialog = kind == "dialog";
                    let window = if dialog {
                        let template = PopupDialogTemplate {
                            header: DLGTEMPLATE {
                                style: WS_POPUP | WS_CAPTION | WS_SYSMENU | DS_MODALFRAME as u32,
                                dwExtendedStyle: WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                                cdit: 0,
                                x: 0,
                                y: 0,
                                cx: 120,
                                cy: 60,
                            },
                            menu: 0,
                            class: 0,
                            title: 0,
                        };
                        // SAFETY: aligned complete fixed template, exact own root owner,
                        // initially hidden modeless dialog with no focus or controls.
                        unsafe {
                            CreateDialogIndirectParamW(
                                instance,
                                &template.header,
                                hwnd,
                                Some(popup_dialog_proc),
                                0,
                            )
                        }
                    } else {
                        let wc = WNDCLASSEXW {
                            cbSize: size_of::<WNDCLASSEXW>() as u32,
                            lpfnWndProc: Some(popup_colour_proc),
                            hInstance: instance,
                            lpszClassName: popup_class.as_ptr(),
                            ..Default::default()
                        };
                        // SAFETY: only this own synthetic class, fixed generated pixels.
                        assert_ne!(unsafe { RegisterClassExW(&wc) }, 0);
                        // SAFETY: own generated layered popup and exact own root owner.
                        unsafe {
                            CreateWindowExW(
                                WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
                                popup_class.as_ptr(),
                                wide("generated synthetic popup").as_ptr(),
                                WS_POPUP,
                                frame.right - 120,
                                frame.top + 100,
                                240,
                                120,
                                hwnd,
                                null_mut(),
                                instance,
                                null(),
                            )
                        }
                    };
                    assert!(!window.is_null());
                    popup = Some(PopupSurface {
                        hwnd: window,
                        instance,
                        class: popup_class,
                        registered: !dialog,
                    });
                    // SAFETY: own exact created surface, physical position and no activation.
                    assert_ne!(
                        // SAFETY: position only this exact created fixture HWND, with fixed physical bounds and no activation.
                        unsafe {
                            SetWindowPos(
                                window,
                                null_mut(),
                                frame.right - 120,
                                frame.top + 100,
                                240,
                                120,
                                SWP_NOACTIVATE | SWP_NOZORDER,
                            )
                        },
                        0
                    );
                    if !dialog {
                        generated_alpha_popup(window);
                    }
                    // SAFETY: show/repaint only this own surface without focus or input.
                    unsafe {
                        ShowWindow(window, SW_SHOWNOACTIVATE);
                        if dialog {
                            UpdateWindow(window);
                        }
                    }
                    let mut bounds = RECT::default();
                    let mut class = [0u16; 128];
                    let mut child_pid = 0;
                    // SAFETY: exact returned own HWND and its same-process root owner only.
                    let facts = unsafe {
                        let tid = GetWindowThreadProcessId(window, &mut child_pid);
                        assert_eq!(child_pid, pid);
                        assert_eq!(tid, hello.tid);
                        assert_eq!(GetWindow(window, GW_OWNER), hwnd);
                        assert_eq!(
                            DwmGetWindowAttribute(
                                window,
                                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                                (&mut bounds as *mut RECT).cast(),
                                size_of::<RECT>() as u32
                            ),
                            0
                        );
                        assert!(GetClassNameW(window, class.as_mut_ptr(), class.len() as i32) > 0);
                        PopupHello {
                            hwnd: window as usize as u64,
                            pid,
                            tid,
                            created,
                            frame: coordinates(bounds),
                            class: string(&class),
                            style: GetWindowLongPtrW(window, GWL_STYLE) as u32,
                            ex_style: GetWindowLongPtrW(window, GWL_EXSTYLE) as u32,
                            owner: hwnd as usize as u64,
                        }
                    };
                    println!("POPUP {}", serde_json::to_string(&facts).unwrap());
                }
                "DISMISS" => {
                    assert!(popup.is_some());
                    drop(popup.take());
                }
                "CLOSE" => {
                    assert!(popup.is_none());
                    // SAFETY: destroy only this retained own root before original process exits.
                    assert_ne!(unsafe { DestroyWindow(hwnd) }, 0);
                    root.hwnd = null_mut();
                }
                _ => unreachable!(),
            }
            println!("ACK {command}");
            std::io::stdout().flush().unwrap();
            if command == "CLOSE" {
                break;
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(root.hwnd.is_null(), "owned popup row internal deadline");
    drop(popup);
    drop(root);
    // SAFETY: restores only this private creator thread's previous DPI context.
    unsafe {
        SetThreadDpiAwarenessContext(previous);
    }
}

fn popup_identity(hello: &Hello) -> Identity {
    Identity {
        hwnd: hello.hwnd,
        pid: hello.pid,
        tid: hello.tid,
        process_created: hello.created,
    }
}
fn popup_source(hello: &Hello, identity: Identity) -> Verified<window::WindowsWindowSource> {
    let probe = MonitorProbe {
        device_path: "owned-popup-monitor".into(),
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
    Verified::new(
        window::WindowsWindowSource::for_fixture(
            Arc::new(Mutex::new(DisplayIds::default())),
            Arc::new(move || Ok(vec![probe.clone()])),
            identity,
        )
        .unwrap(),
        window::WindowsWindowSource::stop_verified,
        "popup_source",
    )
}
struct OwnedPixels {
    size: crosspane_types::geom::PixelSize,
    bytes: Vec<u8>,
}
impl OwnedPixels {
    fn from_frame(frame: &Frame) -> Self {
        let width = frame.size.width as usize;
        let height = frame.size.height as usize;
        let bytes = width
            .checked_mul(height)
            .and_then(|n| n.checked_mul(4))
            .unwrap();
        assert!(bytes > 0 && bytes <= 16 * 1024 * 1024);
        let pixels = frame
            .with_pixels(|pixels, stride| {
                let stride = stride as usize;
                assert!(stride >= width * 4 && pixels.len() >= (height - 1) * stride + width * 4);
                let mut tight = Vec::with_capacity(bytes);
                for y in 0..height {
                    tight.extend_from_slice(&pixels[y * stride..y * stride + width * 4]);
                }
                tight
            })
            .unwrap();
        Self {
            size: frame.size,
            bytes: pixels,
        }
    }
    fn magenta(&self) -> usize {
        self.bytes
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[..3] == [255, 0, 255])
            .count()
    }
    fn at(&self, x: i32, y: i32) -> [u8; 4] {
        assert!(x >= 0 && y >= 0 && x < self.size.width as i32 && y < self.size.height as i32);
        let at = (y as usize * self.size.width as usize + x as usize) * 4;
        self.bytes[at..at + 4].try_into().unwrap()
    }
    fn png(&self, directory: &std::path::Path, label: &str) {
        assert!(matches!(
            label,
            "closed"
                | "open"
                | "qualified"
                | "crop-open"
                | "crop-dismissed"
                | "dismissed"
                | "dialog"
        ));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(directory.join(format!("{label}.png")))
            .unwrap();
        let mut rgba = self.bytes.clone();
        for p in rgba.as_chunks_mut::<4>().0 {
            p.swap(0, 2);
            // Frame's BGRA alpha is ignored by contract; export the composed BGR opaquely.
            p[3] = 255;
        }
        let mut encoder = png::Encoder::new(file, self.size.width, self.size.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&rgba).unwrap();
        writer.finish().unwrap();
        rgba.fill(0);
    }
}
impl Drop for OwnedPixels {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}
struct OwnedSessionGuard {
    foreground: HWND,
    last_input: u32,
}
impl OwnedSessionGuard {
    fn read_input() -> u32 {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
        let mut info = LASTINPUTINFO {
            cbSize: size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        // SAFETY: read-only session aggregate in exact initialized buffer; no characters,
        // key/button state, hooks or input tick values are stored in any receipt.
        assert_ne!(
            // SAFETY: read only the session aggregate into its initialized buffer; no input contents or tick values are logged.
            unsafe { GetLastInputInfo(&mut info) },
            0,
            "session aggregate unavailable"
        );
        info.dwTime
    }
    fn new() -> Self {
        // SAFETY: opaque foreground handle equality only, never foreign fields/pixels.
        Self {
            // SAFETY: compare an opaque foreground HWND only; no foreign fields or pixels are queried.
            foreground: unsafe { GetForegroundWindow() },
            last_input: Self::read_input(),
        }
    }
    fn check(&self) {
        // SAFETY: opaque foreground equality only; changed aggregate refuses the row.
        assert!(
            // SAFETY: compare only the current opaque foreground handle with the row baseline.
            unsafe { GetForegroundWindow() } == self.foreground,
            "foreground changed"
        );
        assert!(
            Self::read_input() == self.last_input,
            "real input aggregate changed"
        );
    }
}
fn owned_row_frame(
    events: &mpsc::Receiver<FrameEvent>,
    stream: StreamId,
    stage: &str,
    count: &mut usize,
    deadline: Instant,
    session: &OwnedSessionGuard,
) -> Frame {
    assert!(
        Instant::now() < deadline && *count < 120,
        "owned row/frame budget"
    );
    session.check();
    let frame = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "owned row deadline at {stage}");
        match events
            .recv_timeout(remaining.min(Duration::from_secs(2)))
            .unwrap_or_else(|_| panic!("owned row frame deadline at {stage}"))
        {
            FrameEvent::Frame { stream: id, frame } if id == stream => break frame,
            FrameEvent::Ended { stream: id, reason } if id == stream => {
                panic!("owned row ended at {stage}: {reason:?}")
            }
            _ => {}
        }
    };
    *count += 1;
    session.check();
    assert!(Instant::now() < deadline && *count <= 120);
    frame
}
fn authenticated_popup(fixture: &Fixture, hello: &Hello, popup: &PopupHello) {
    assert_eq!(popup.pid, fixture.child.id());
    assert_eq!(popup.pid, hello.pid);
    assert_eq!(popup.tid, hello.tid);
    assert_eq!(popup.created, hello.created);
    assert_eq!(popup.owner, hello.hwnd);
    assert_ne!(popup.hwnd, 0);
    assert_eq!(
        original_created(fixture.child.as_raw_handle()),
        popup.created
    );
    let hwnd = popup.hwnd as usize as HWND;
    let mut pid = 0;
    let mut class = [0u16; 128];
    let mut bounds = RECT::default();
    // SAFETY: original live child/nonce/pipe proof precedes all field APIs; this
    // exact returned own popup is independently corroborated before class/bounds.
    unsafe {
        assert_eq!(
            WaitForSingleObject(fixture.child.as_raw_handle(), 0),
            WAIT_TIMEOUT
        );
        assert_eq!(GetWindowThreadProcessId(hwnd, &mut pid), popup.tid);
        assert_eq!(pid, popup.pid);
        assert_eq!(GetWindow(hwnd, GW_OWNER) as usize as u64, popup.owner);
        assert!(GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32) > 0);
        assert_eq!(GetWindowLongPtrW(hwnd, GWL_STYLE) as u32, popup.style);
        assert_eq!(GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32, popup.ex_style);
        assert_eq!(
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                (&mut bounds as *mut RECT).cast(),
                size_of::<RECT>() as u32
            ),
            0
        );
    }
    assert_eq!(string(&class), popup.class);
    assert_eq!(coordinates(bounds), popup.frame);
}
fn saved_snapshot(
    directory: &std::path::Path,
    stage: &str,
    snapshot: &frame_capture::PopupProbeSnapshot,
    pixels: &OwnedPixels,
    frames: usize,
) {
    let root = &snapshot.snapshot;
    let popups: Vec<_> = snapshot.popups.iter().map(|p| serde_json::json!({
        "hwnd":p.candidate.token.identity.hwnd,"pid":p.candidate.token.identity.pid,
        "tid":p.candidate.token.identity.tid,"created":p.candidate.token.identity.process_created,
        "generation":p.candidate.token.generation,"bounds":p.candidate.geometry.bounds,
        "content":p.content,"clip":p.clip.map(|c|serde_json::json!({"source":c.source,"destination":c.destination})),
        "alpha0":p.alpha.transparent,"alpha255":p.alpha.opaque,"fractional":p.alpha.fractional
    })).collect();
    let report = serde_json::json!({"stage":stage,"root_hwnd":root.root.identity.hwnd,
        "root_bounds":root.geometry.bounds,"root_content":snapshot.root_content,
        "output_size":pixels.size,"magenta_pixels":pixels.magenta(),"frames":frames,
        "refusal":root.reason.code(),"refused":root.refused,"popups":popups,
        "alpha_mode":format!("{:?}",snapshot.alpha)});
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join(format!("{stage}.json")))
        .unwrap();
    serde_json::to_writer_pretty(file, &report).unwrap();
}
fn owned_popup_sink(
    send: mpsc::SyncSender<FrameEvent>,
    observed: Arc<std::sync::atomic::AtomicUsize>,
    gate: Arc<IoGate>,
) -> Arc<dyn crosspane_platform::EventSink<FrameEvent>> {
    Arc::new(move |event| {
        if matches!(&event, FrameEvent::Frame { .. }) {
            use std::sync::atomic::Ordering;
            if observed
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                    (n < 120).then_some(n + 1)
                })
                .is_err()
            {
                gate.set_engine_permits(false);
                return;
            }
        }
        let _ = send.try_send(event);
    })
}
fn run_owned_popup_row(kind: &str) {
    use model::popup::{self, Alpha};
    assert_eq!(
        std::env::var("CROSSPANE_WINDOWS_POPUP_PROBE").as_deref(),
        Ok("1")
    );
    assert_eq!(
        std::env::var("CROSSPANE_WINDOWS_POPUP_SELECTOR").as_deref(),
        Ok(kind)
    );
    limited();
    let _watchdog = Watchdog::new(60);
    let deadline = Instant::now() + Duration::from_secs(30);
    let session = OwnedSessionGuard::new();
    let nonce = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
    assert_eq!(
        cwd.file_name().unwrap(),
        "WP-W2.2c",
        "owned mirror working directory required"
    );
    let selector_directory = cwd.join("target/wp-notes/owned-popup-rows").join(kind);
    std::fs::create_dir_all(&selector_directory).unwrap();
    let _consumed = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(selector_directory.join("consumed"))
        .unwrap();
    let directory = selector_directory.join(&nonce);
    std::fs::create_dir(&directory).unwrap();
    let mut fixture = Fixture::launch_popup(&nonce, kind);
    fixture.command("START");
    let hello: Hello = serde_json::from_str(&fixture.response("HELLO ")).unwrap();
    assert_eq!(hello.nonce, nonce);
    assert_eq!(hello.pid, fixture.child.id());
    assert_eq!(
        original_created(fixture.child.as_raw_handle()),
        hello.created
    );
    let mut pid = 0;
    let mut class = [0u16; 256];
    // SAFETY: authenticated original child's own root, PID/TID first, no other HWND field query.
    unsafe {
        assert_eq!(
            GetWindowThreadProcessId(hello.hwnd as usize as HWND, &mut pid),
            hello.tid
        );
        assert_eq!(pid, hello.pid);
        assert!(
            GetClassNameW(
                hello.hwnd as usize as HWND,
                class.as_mut_ptr(),
                class.len() as i32
            ) > 0
        );
    }
    assert_eq!(string(&class), format!("Crosspane.WgcPopupFixture.{nonce}"));
    let source = popup_source(&hello, popup_identity(&hello));
    let root_window = source.windows().unwrap();
    assert_eq!(root_window.len(), 1);
    let root_target = root_window[0].id;
    let gate = IoGate::new();
    gate.set_engine_permits(true);
    gate.set_session_permits(true);
    let mut capture = Verified::new(
        frame_capture::WindowsFrameCapture::new(Arc::clone(&gate), source.resolver()).unwrap(),
        frame_capture::WindowsFrameCapture::stop_verified,
        "popup_capture",
    );
    let (send, events) = mpsc::sync_channel(16);
    let observed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let sink = owned_popup_sink(send, Arc::clone(&observed), Arc::clone(&gate));
    let mut stream = capture
        .start(
            CaptureTarget::Window(root_target),
            None,
            10,
            Arc::clone(&sink),
        )
        .unwrap();
    let mut frames = 0;
    let closed = OwnedPixels::from_frame(&owned_row_frame(
        &events,
        stream,
        "closed",
        &mut frames,
        deadline,
        &session,
    ));
    assert_eq!(closed.magenta(), 0);
    closed.png(&directory, "closed");
    let root_size = closed.size;
    let closed_snapshot = capture.popup_probe_snapshot(stream).unwrap();
    assert_eq!(closed_snapshot.root_content, root_size);
    assert!(closed_snapshot.popups.is_empty());
    saved_snapshot(&directory, "closed", &closed_snapshot, &closed, frames);
    // Dialog root drains use only saved pure expectations after stop removes
    // the stream. Matching StreamId is bound to the originally owned root;
    // Frame has size/pixels, not a second HWND/token/geometry payload.
    let check_dialog_root_frame = |frame: &Frame, expected: &frame_capture::PopupProbeSnapshot| {
        assert!(expected.snapshot.root.identity == popup_identity(&hello));
        assert!(expected.snapshot.geometry == closed_snapshot.snapshot.geometry);
        assert!(expected.root_content == root_size && expected.popups.is_empty());
        let pixels = OwnedPixels::from_frame(frame);
        assert!(pixels.size == root_size && pixels.magenta() == 0);
        assert!(
            pixels
                .bytes
                .as_chunks::<4>()
                .0
                .iter()
                .zip(closed.bytes.as_chunks::<4>().0.iter())
                .all(|(a, b)| a[..3] == b[..3]),
            "Dialog root frame was not pristine"
        );
        pixels
    };
    let wait_dialog_root_end = |events: &mpsc::Receiver<FrameEvent>,
                                stream: StreamId,
                                expected: &frame_capture::PopupProbeSnapshot,
                                count: &mut usize| {
        let until = deadline.min(Instant::now() + Duration::from_secs(5));
        loop {
            session.check();
            let remaining = until.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "Dialog root terminal deadline");
            match events.recv_timeout(remaining) {
                Ok(FrameEvent::Frame { stream: id, frame }) if id == stream => {
                    assert!(*count < 120 && Instant::now() < deadline);
                    *count += 1;
                    drop(check_dialog_root_frame(&frame, expected));
                    session.check();
                }
                Ok(FrameEvent::Ended { stream: id, reason }) if id == stream => {
                    assert_eq!(reason, StreamEndReason::Requested);
                    session.check();
                    assert!(Instant::now() < deadline && *count <= 120);
                    break;
                }
                Ok(_) => {}
                Err(_) => panic!("Dialog root terminal receiver deadline or disconnect"),
            }
        }
    };
    writeln!(fixture.child.stdin.as_mut().unwrap(), "OPEN").unwrap();
    fixture.child.stdin.as_mut().unwrap().flush().unwrap();
    let popup: PopupHello = serde_json::from_str(&fixture.response("POPUP ")).unwrap();
    assert_eq!(fixture.response("ACK "), "OPEN");
    authenticated_popup(&fixture, &hello, &popup);
    let mut dialog_last_root = None;
    if kind == "dialog" {
        assert_eq!(popup.class, "#32770");
        assert_ne!(popup.style & WS_POPUP, 0);
        assert_ne!(popup.style & WS_CAPTION, 0);
        let dialog_source = popup_source(
            &hello,
            Identity {
                hwnd: popup.hwnd,
                pid: popup.pid,
                tid: popup.tid,
                process_created: popup.created,
            },
        );
        let dialog_windows = dialog_source.windows().unwrap();
        assert_eq!(dialog_windows.len(), 1);
        assert_eq!(
            dialog_windows[0].role,
            crosspane_platform::WindowRole::Dialog
        );
        let mut dialog_capture = Verified::new(
            frame_capture::WindowsFrameCapture::new(Arc::clone(&gate), dialog_source.resolver())
                .unwrap(),
            frame_capture::WindowsFrameCapture::stop_verified,
            "dialog_capture",
        );
        let (send, dialog_events) = mpsc::sync_channel(16);
        let dialog_sink = owned_popup_sink(send, Arc::clone(&observed), Arc::clone(&gate));
        let dialog_stream = dialog_capture
            .start(
                CaptureTarget::Window(dialog_windows[0].id),
                None,
                10,
                dialog_sink,
            )
            .unwrap();
        let dialog = OwnedPixels::from_frame(&owned_row_frame(
            &dialog_events,
            dialog_stream,
            "dialog",
            &mut frames,
            deadline,
            &session,
        ));
        assert!(dialog.magenta() > 0);
        dialog.png(&directory, "dialog");
        assert_eq!(dialog.size.width, (popup.frame[2] - popup.frame[0]) as u32);
        assert_eq!(dialog.size.height, (popup.frame[3] - popup.frame[1]) as u32);
        // A fresh root stream establishes exclusion after the real Dialog opened;
        // no assumption that an unchanged root emits another WGC frame.
        capture.stop(stream).unwrap();
        wait_dialog_root_end(&events, stream, &closed_snapshot, &mut frames);
        stream = capture
            .start(
                CaptureTarget::Window(root_target),
                None,
                10,
                Arc::clone(&sink),
            )
            .unwrap();
        let opened = OwnedPixels::from_frame(&owned_row_frame(
            &events,
            stream,
            "dialog owner",
            &mut frames,
            deadline,
            &session,
        ));
        assert_eq!(opened.size, root_size);
        assert_eq!(opened.magenta(), 0);
        opened.png(&directory, "open");
        let snapshot = capture.popup_probe_snapshot(stream).unwrap();
        assert!(snapshot.snapshot.root.identity == popup_identity(&hello));
        assert!(snapshot.snapshot.geometry == closed_snapshot.snapshot.geometry);
        assert!(snapshot.root_content == root_size && snapshot.popups.is_empty());
        assert!(
            opened
                .bytes
                .as_chunks::<4>()
                .0
                .iter()
                .zip(closed.bytes.as_chunks::<4>().0.iter())
                .all(|(a, b)| a[..3] == b[..3]),
            "Dialog owner open frame was not pristine"
        );
        saved_snapshot(&directory, "open", &snapshot, &opened, frames);
        let dialog_snapshot = dialog_capture.popup_probe_snapshot(dialog_stream).unwrap();
        saved_snapshot(&directory, "dialog", &dialog_snapshot, &dialog, frames);
        dialog_source.freeze_fixture_fields().unwrap();
        fixture.command("DISMISS");
        wait_end(&dialog_events, dialog_stream, StreamEndReason::TargetGone);
        assert!(dialog_capture.finish());
        assert!(dialog_source.finish());
        dialog_last_root = Some((opened, snapshot));
    } else {
        assert_eq!(popup.class, format!("Crosspane.WgcSynthetic.{kind}"));
        assert_ne!(popup.ex_style & WS_EX_LAYERED, 0);
        assert_eq!(popup.style & WS_CAPTION, 0);
        let (open, snapshot) = loop {
            let pixels = OwnedPixels::from_frame(&owned_row_frame(
                &events,
                stream,
                "popup open",
                &mut frames,
                deadline,
                &session,
            ));
            let snapshot = capture.popup_probe_snapshot(stream).unwrap();
            if pixels.magenta() > 0
                && snapshot.popups.len() == 1
                && snapshot.popups[0].content.is_some()
                && snapshot.popups[0].clip.is_some()
            {
                break (pixels, snapshot);
            }
        };
        assert_eq!(open.size, root_size);
        let item = &snapshot.popups[0];
        let popup_geometry = item.candidate.geometry;
        assert_eq!(item.candidate.token.identity.hwnd, popup.hwnd);
        assert_eq!(item.candidate.token.identity.pid, hello.pid);
        assert_eq!(item.candidate.token.identity.tid, hello.tid);
        assert_eq!(item.candidate.token.identity.process_created, hello.created);
        assert_eq!(item.content.unwrap(), item.candidate.geometry.content);
        let all = PixelRect::new(
            (0, 0).into(),
            (root_size.width as i32, root_size.height as i32).into(),
        );
        let expected = popup::clip(snapshot.snapshot.geometry, item.candidate.geometry, all)
            .unwrap()
            .unwrap();
        assert_eq!(item.clip.unwrap(), expected);
        assert!(
            i64::from(expected.source.max.x) - i64::from(expected.source.min.x)
                < i64::from(item.candidate.geometry.content.width),
            "fixture must genuinely overhang"
        );
        assert_eq!(
            source.windows().unwrap().len(),
            1,
            "synthetic popups are never separately projectable"
        );
        for y in 0..root_size.height as i32 {
            for x in 0..root_size.width as i32 {
                if x < expected.destination.min.x
                    || x >= expected.destination.max.x
                    || y < expected.destination.min.y
                    || y >= expected.destination.max.y
                {
                    assert_eq!(
                        open.at(x, y)[..3],
                        closed.at(x, y)[..3],
                        "popup pixel outside proven clip"
                    );
                }
            }
        }
        open.png(&directory, "open");
        saved_snapshot(&directory, "open", &snapshot, &open, frames);
        let qualification = capture.qualify_popup_premultiplied(
            stream,
            popup.hwnd,
            PixelRect::new((24, 0).into(), (48, 24).into()),
            [128, 0, 128, 128],
        );
        if qualification.is_err() && !matches!(&qualification, Err(PlatformError::Unsupported(_))) {
            panic!("unexpected owned alpha qualification failure: {qualification:?}");
        }
        let qualified = if qualification.is_ok() {
            while events.try_recv().is_ok() {}
            OwnedPixels::from_frame(&owned_row_frame(
                &events,
                stream,
                "alpha",
                &mut frames,
                deadline,
                &session,
            ))
        } else {
            // Known byte mismatch is U and does not dirty the capture. Preserve the
            // already observed threshold frame; do not invent a new native delivery.
            OwnedPixels {
                size: open.size,
                bytes: open.bytes.clone(),
            }
        };
        let snapshot = capture.popup_probe_snapshot(stream).unwrap();
        assert!(
            qualified.magenta() > 0,
            "unqualified alpha must not drop opaque popup pixels"
        );
        if qualification.is_ok() {
            assert_eq!(snapshot.alpha, Alpha::Premultiplied);
        } else if matches!(&qualification, Err(PlatformError::Unsupported(_))) {
            assert_eq!(snapshot.alpha, Alpha::Threshold128);
            println!("OWNED_POPUP fractional_alpha=U threshold128");
        } else {
            panic!("unexpected owned alpha qualification failure: {qualification:?}");
        }
        let offset_x = popup_geometry.bounds.min.x - snapshot.snapshot.geometry.bounds.min.x;
        let offset_y = popup_geometry.bounds.min.y - snapshot.snapshot.geometry.bounds.min.y;
        assert_eq!(
            qualified.at(offset_x + 8, offset_y + 8)[..3],
            closed.at(offset_x + 8, offset_y + 8)[..3]
        );
        assert_eq!(
            qualified.at(offset_x + 72, offset_y + 8)[..3],
            [255, 0, 255]
        );
        if qualification.is_ok() {
            let previous = closed.at(offset_x + 32, offset_y + 8);
            let actual = qualified.at(offset_x + 32, offset_y + 8);
            let blended = [128u32, 0, 128];
            for channel in 0..3 {
                assert_eq!(
                    actual[channel],
                    (blended[channel] + (u32::from(previous[channel]) * 127 + 127) / 255).min(255)
                        as u8
                );
            }
        } else if snapshot.popups[0].alpha.fractional > 0 {
            assert_eq!(
                qualified.at(offset_x + 32, offset_y + 8)[..3],
                [128, 0, 128]
            );
        }
        qualified.png(&directory, "qualified");
        saved_snapshot(&directory, "qualified", &snapshot, &qualified, frames);
        let crop = PixelRect::new(
            (root_size.width as i32 - 130, 100).into(),
            (root_size.width as i32 - 30, 180).into(),
        );
        // Existing set_crop invalidates its ROI baseline and needs a fresh root frame.
        // Keep same-root sessions sequential; reuse only received own frame evidence.
        capture.stop(stream).unwrap();
        wait_end(&events, stream, StreamEndReason::Requested);
        let (crop_send, crop_events) = mpsc::sync_channel(16);
        let crop_sink = owned_popup_sink(crop_send, Arc::clone(&observed), Arc::clone(&gate));
        let crop_stream = capture
            .start(
                CaptureTarget::Window(root_target),
                Some(crop),
                10,
                crop_sink,
            )
            .unwrap();
        let (cropped, crop_snapshot) = loop {
            let pixels = OwnedPixels::from_frame(&owned_row_frame(
                &crop_events,
                crop_stream,
                "popup crop",
                &mut frames,
                deadline,
                &session,
            ));
            let snapshot = capture.popup_probe_snapshot(crop_stream).unwrap();
            if pixels.size.width == 100
                && pixels.size.height == 80
                && pixels.magenta() > 0
                && snapshot.popups.first().is_some_and(|p| {
                    p.clip == popup::clip(snapshot.snapshot.geometry, popup_geometry, crop).unwrap()
                })
            {
                break (pixels, snapshot);
            }
        };
        cropped.png(&directory, "crop-open");
        saved_snapshot(&directory, "crop-open", &crop_snapshot, &cropped, frames);
        fixture.command("DISMISS");
        let (crop_dismissed, crop_snapshot) = loop {
            let pixels = OwnedPixels::from_frame(&owned_row_frame(
                &crop_events,
                crop_stream,
                "cropped dismissal",
                &mut frames,
                deadline,
                &session,
            ));
            let snapshot = capture.popup_probe_snapshot(crop_stream).unwrap();
            if pixels.size.width == 100
                && pixels.size.height == 80
                && pixels.magenta() == 0
                && snapshot.popups.is_empty()
            {
                break (pixels, snapshot);
            }
        };
        for y in 0..80 {
            for x in 0..100 {
                assert_eq!(
                    crop_dismissed.at(x, y)[..3],
                    closed.at(x + crop.min.x, y + crop.min.y)[..3]
                );
            }
        }
        crop_dismissed.png(&directory, "crop-dismissed");
        saved_snapshot(
            &directory,
            "crop-dismissed",
            &crop_snapshot,
            &crop_dismissed,
            frames,
        );
        capture.stop(crop_stream).unwrap();
        wait_end(&crop_events, crop_stream, StreamEndReason::Requested);
        stream = capture
            .start(
                CaptureTarget::Window(root_target),
                None,
                10,
                Arc::clone(&sink),
            )
            .unwrap();
    }
    let (dismissed, snapshot) = if let Some((mut last, mut last_snapshot)) = dialog_last_root {
        // The independent Dialog already reached TargetGone. An unchanged root
        // need not emit a new frame: observe its receiver, retaining last pixels.
        let root_token = last_snapshot.snapshot.root;
        let root_geometry = last_snapshot.snapshot.geometry;
        assert!(root_token.identity == popup_identity(&hello));
        assert!(last.size == root_size && last.magenta() == 0);
        assert!(last_snapshot.root_content == root_size && last_snapshot.popups.is_empty());
        assert!(
            last.bytes
                .as_chunks::<4>()
                .0
                .iter()
                .zip(closed.bytes.as_chunks::<4>().0.iter())
                .all(|(a, b)| a[..3] == b[..3]),
            "Dialog owner last frame was not pristine"
        );
        let observe_until = deadline.min(Instant::now() + Duration::from_secs(2));
        let mut delivered = 0usize;
        loop {
            assert!(Instant::now() < deadline && frames < 120);
            session.check();
            let remaining = observe_until.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match events.recv_timeout(remaining) {
                Ok(FrameEvent::Frame { stream: id, frame }) if id == stream => {
                    frames += 1;
                    delivered += 1;
                    let pixels = OwnedPixels::from_frame(&frame);
                    let current = capture.popup_probe_snapshot(stream).unwrap();
                    assert!(pixels.size == root_size && pixels.magenta() == 0);
                    assert!(current.snapshot.root == root_token);
                    assert!(current.snapshot.geometry == root_geometry);
                    assert!(current.root_content == root_size && current.popups.is_empty());
                    assert!(
                        pixels
                            .bytes
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .zip(closed.bytes.as_chunks::<4>().0.iter())
                            .all(|(a, b)| a[..3] == b[..3]),
                        "Dialog pixels or root drift during dismissal observation"
                    );
                    last = pixels;
                    last_snapshot = current;
                }
                Ok(FrameEvent::Ended { stream: id, .. }) if id == stream => {
                    panic!("Dialog owner stream ended during dismissal observation");
                }
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("Dialog owner receiver disconnected during dismissal observation");
                }
            }
        }
        session.check();
        assert!(Instant::now() < deadline && frames <= 120);
        println!(
            "OWNED_DIALOG root_frames_after_dismiss={delivered} retained_last={}",
            usize::from(delivered == 0)
        );
        (last, last_snapshot)
    } else {
        loop {
            let pixels = OwnedPixels::from_frame(&owned_row_frame(
                &events,
                stream,
                "dismissed",
                &mut frames,
                deadline,
                &session,
            ));
            let snapshot = capture.popup_probe_snapshot(stream).unwrap();
            if pixels.size == root_size && pixels.magenta() == 0 && snapshot.popups.is_empty() {
                break (pixels, snapshot);
            }
        }
    };
    assert!(
        dismissed
            .bytes
            .as_chunks::<4>()
            .0
            .iter()
            .zip(closed.bytes.as_chunks::<4>().0.iter())
            .all(|(a, b)| a[..3] == b[..3]),
        "dismissal failed to restore pristine root pixels"
    );
    dismissed.png(&directory, "dismissed");
    saved_snapshot(&directory, "dismissed", &snapshot, &dismissed, frames);
    capture.stop(stream).unwrap();
    if kind == "dialog" {
        wait_dialog_root_end(&events, stream, &snapshot, &mut frames);
    } else {
        wait_end(&events, stream, StreamEndReason::Requested);
    }
    assert!(capture.finish());
    source.freeze_fixture_fields().unwrap();
    fixture.command("CLOSE");
    assert!(source.finish());
    fixture.finish();
    session.check();
    assert!(
        Instant::now() < deadline,
        "owned row total completion deadline"
    );
    let observed_frames = observed.load(std::sync::atomic::Ordering::Relaxed);
    assert!(observed_frames <= 120);
    println!(
        "OWNED_POPUP selector={kind} closed/open/dismissed=verified owner_pixels_only original_child_exit=0 private_job_active=0 consumed_frames={frames} observed_frames={observed_frames}"
    );
}
#[test]
#[ignore = "exact ROOT-released Limited menu selector only; owned pixels/geometry/job evidence"]
fn limited_owned_popup_menu_composite_clip_dismiss() {
    run_owned_popup_row("menu");
}
#[test]
#[ignore = "exact ROOT-released Limited tooltip selector only; owned pixels/geometry/job evidence"]
fn limited_owned_popup_tooltip_composite_clip_dismiss() {
    run_owned_popup_row("tooltip");
}
#[test]
#[ignore = "exact ROOT-released Limited genuine Dialog selector only; independent capture path"]
fn limited_owned_dialog_existing_capture_path() {
    run_owned_popup_row("dialog");
}
