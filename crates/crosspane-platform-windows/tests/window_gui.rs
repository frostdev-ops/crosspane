//! Limited, own-fixture-only probe. Both native bodies are ignored by cargo test.
#![cfg(windows)]
#![allow(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crosspane_platform::{WindowEvent, WindowRole, WindowSource, WindowState};
use crosspane_platform_windows::model;
use model::{
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
    UI::{HiDpi::*, WindowsAndMessaging::*},
};

#[path = "../src/window.rs"]
mod adapter;

#[test]
fn source_is_send_and_factory_requires_the_shared_display_map() {
    fn send<T: Send>() {}
    send::<adapter::WindowsWindowSource>();
    let _: fn(
        Arc<Mutex<DisplayIds>>,
        adapter::MonitorReader,
    ) -> Result<adapter::WindowsWindowSource, crosspane_platform::PlatformError> =
        adapter::WindowsWindowSource::new;
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
    // SAFETY: query only this process's token and close its sole output handle.
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
            &mut bytes,
        );
        assert_ne!(CloseHandle(token), 0);
        assert_ne!(okay, 0);
    }
    assert_eq!(
        elevation.TokenIsElevated, 0,
        "native probe forbidden through elevated SSH"
    );
}

struct Watchdog(mpsc::Sender<()>);
impl Watchdog {
    fn new(seconds: u64) -> Self {
        let (cancel, receive) = mpsc::channel();
        thread::spawn(move || {
            if receive.recv_timeout(Duration::from_secs(seconds)).is_err() {
                eprintln!("OWNED WINDOW PROBE WATCHDOG FAILED");
                std::process::exit(124);
            }
        });
        Self(cancel)
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
        // SAFETY: private unnamed job, containing only the child we create below.
        let job = unsafe { CreateJobObjectW(null(), null()) };
        assert!(!job.is_null());
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: exact initialized job limit structure and our owned job handle.
        let configured = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        assert_ne!(configured, 0);
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "fixture_server", "--nocapture"])
            .env("CROSSPANE_WINDOW_FIXTURE_NONCE", nonce)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let child = match child {
            Ok(child) => child,
            Err(error) => {
                // SAFETY: no assigned child exists; close only our job.
                unsafe { CloseHandle(job) };
                panic!("fixture spawn failed: {error}");
            }
        };
        let (send, lines) = mpsc::channel();
        let mut fixture = Self {
            child,
            job,
            lines,
            reader: None,
        };
        // SAFETY: job and process handles refer only to the fixture just spawned.
        let assigned = unsafe { AssignProcessToJobObject(job, fixture.child.as_raw_handle()) };
        assert_ne!(assigned, 0);
        let stdout = fixture.child.stdout.take().unwrap();
        fixture.reader = Some(thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if send.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
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
                .expect("owned fixture response timeout");
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
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < until, "fixture did not exit");
            thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success());
        if let Some(reader) = self.reader.take() {
            reader.join().unwrap();
        }
        // SAFETY: the fixture exited; closing the private job cannot affect another process.
        assert_ne!(unsafe { CloseHandle(self.job) }, 0);
        self.job = null_mut();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if !self.job.is_null() {
            // SAFETY: only our fixture belongs to this kill-on-close job.
            unsafe { CloseHandle(self.job) };
            self.job = null_mut();
            // Wait only for our own child; the enclosing watchdog also bounds cleanup.
            let _ = self.child.wait();
        }
    }
}

fn wait_for(mut condition: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(2);
    while !condition() {
        assert!(Instant::now() < until, "owned window observation timed out");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "Limited own-fixture probe; win-gui only, explicit opt-in"]
fn limited_owned_window_source() {
    assert_eq!(
        std::env::var("CROSSPANE_WINDOWS_WINDOW_PROBE").as_deref(),
        Ok("1")
    );
    limited();
    let _watchdog = Watchdog::new(18);
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
    // The private inherited pipe, trusted child image and nonce admit this HWND
    // BEFORE these field reads. No reported/other HWND is ever queried.
    let window = hello.hwnd as usize as HWND;
    let mut pid = 0;
    let mut class = [0_u16; 256];
    // SAFETY: only the authenticated fixture HWND is queried.
    unsafe {
        assert_eq!(GetWindowThreadProcessId(window, &mut pid), hello.tid);
        assert_eq!(pid, fixture.child.id());
        assert!(GetClassNameW(window, class.as_mut_ptr(), class.len() as i32) > 0);
    }
    assert_eq!(string(&class), format!("Crosspane.WindowFixture.{nonce}"));
    let probe = MonitorProbe {
        device_path: "owned-fixture-monitor".into(),
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
    let ids = Arc::new(Mutex::new(DisplayIds::default()));
    let expected = model::window::logical_frame(
        hello.frame,
        probe.rc_monitor,
        &probe.name,
        std::slice::from_ref(&probe),
        &mut ids.lock().unwrap(),
    )
    .unwrap();
    let identity = Identity {
        hwnd: hello.hwnd,
        pid: hello.pid,
        tid: hello.tid,
        process_created: hello.created,
    };
    let mut source = adapter::WindowsWindowSource::for_fixture(
        ids,
        Arc::new(move || Ok(vec![probe.clone()])),
        identity,
    )
    .unwrap();
    let windows = source.windows().unwrap();
    assert_eq!(windows.len(), 1);
    let first = &windows[0];
    assert_eq!(first.role, WindowRole::Toplevel);
    assert_eq!(first.state, WindowState::Normal);
    assert_eq!(first.display, Some(expected.0));
    assert_eq!(first.frame, expected.1);
    let id = first.id;
    let (send, events) = mpsc::channel();
    source
        .subscribe(Arc::new(move |event| {
            let _ = send.send(event);
        }))
        .unwrap();
    assert!(
        matches!(events.recv_timeout(Duration::from_secs(1)).unwrap(), WindowEvent::Added(w) if w.id == id)
    );
    fixture.command("rename");
    wait_for(|| source.windows().unwrap()[0].title == "owned-renamed");
    fixture.command("move");
    wait_for(|| source.windows().unwrap()[0].frame != first.frame);
    fixture.command("minimize");
    wait_for(|| source.windows().unwrap()[0].state == WindowState::Minimized);
    fixture.command("restore");
    wait_for(|| source.windows().unwrap()[0].state == WindowState::Normal);
    // Guard BEFORE requesting activation: no focus request with foreign foreground.
    // SAFETY: comparison of foreground handle only, no foreign fields/content.
    let foreground = unsafe { GetForegroundWindow() };
    assert_eq!(
        foreground, window,
        "fixture lost foreground; activation trial aborted"
    );
    source
        .activate(id)
        .expect("documented activation refused; no focus workaround permitted");
    // SAFETY: read-only foreground identity check.
    assert_eq!(unsafe { GetForegroundWindow() }, window);
    source.freeze_fixture_fields().unwrap();
    fixture.command("close");
    wait_for(|| source.windows().unwrap().is_empty());
    let observed: Vec<_> = events.try_iter().collect();
    assert!(
        observed
            .iter()
            .filter(|e| matches!(e, WindowEvent::Changed(w) if w.id == id))
            .count()
            >= 3
    );
    assert!(
        observed
            .iter()
            .any(|e| matches!(e, WindowEvent::Removed(w) if *w == id))
    );
    assert!(
        source.stop_verified(),
        "observer window/hook/class cleanup failed"
    );
    fixture.finish();
    // SAFETY: closure verification for the authenticated fixture HWND only.
    assert_eq!(unsafe { IsWindow(window) }, 0);
    println!(
        "OWNED_WINDOW listed=1 role/state/frame=ok rename/move/minimize=changed activation=confirmed close=removed hooks/thread/job/fixture=clean"
    );
}

unsafe extern "system" fn fixture_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_DESTROY {
        // SAFETY: posts termination only to this fixture's message queue.
        unsafe { PostQuitMessage(0) };
        return 0;
    }
    // SAFETY: registered fixture procedure receives validated User32 parameters.
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

#[test]
#[ignore = "private child fixture, invoked only by the Limited probe"]
fn fixture_server() {
    let nonce =
        std::env::var("CROSSPANE_WINDOW_FIXTURE_NONCE").expect("private fixture admission missing");
    limited();
    let _watchdog = Watchdog::new(16);
    // SAFETY: PMv2 applies only to this owned fixture thread, not system/user settings.
    let dpi = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    assert!(!dpi.is_null());
    let class = wide(&format!("Crosspane.WindowFixture.{nonce}"));
    let title = wide("owned fixture");
    // SAFETY: retrieves only our own executable module handle.
    let instance = unsafe { GetModuleHandleW(null()) };
    let wc = WNDCLASSW {
        lpfnWndProc: Some(fixture_proc),
        hInstance: instance,
        lpszClassName: class.as_ptr(),
        ..Default::default()
    };
    // SAFETY: class name/procedure have valid lifetimes until explicit unregister.
    assert_ne!(unsafe { RegisterClassW(&wc) }, 0);
    // SAFETY: creates only our blank normal window; no foreign HWND or content.
    let window = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            100,
            100,
            320,
            220,
            null_mut(),
            null_mut(),
            instance,
            null(),
        )
    };
    assert!(!window.is_null());
    let mut monitor = MONITORINFOEXW::default();
    monitor.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    let mut frame = RECT::default();
    let mut creation = FILETIME::default();
    let mut other = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: fixed buffers for read-only metadata of our own window/process/monitor.
    let hello = unsafe {
        assert_ne!(
            GetMonitorInfoW(
                MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST),
                &mut monitor.monitorInfo
            ),
            0
        );
        assert!(
            DwmGetWindowAttribute(
                window,
                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                (&mut frame as *mut RECT).cast(),
                size_of::<RECT>() as u32
            ) >= 0
        );
        assert_ne!(
            GetProcessTimes(
                GetCurrentProcess(),
                &mut creation,
                &mut other,
                &mut kernel,
                &mut user
            ),
            0
        );
        Hello {
            nonce,
            hwnd: window as usize as u64,
            pid: GetCurrentProcessId(),
            tid: GetCurrentThreadId(),
            created: (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime),
            frame: coordinates(frame),
            monitor: Monitor {
                name: string(&monitor.szDevice),
                bounds: coordinates(monitor.monitorInfo.rcMonitor),
                work: coordinates(monitor.monitorInfo.rcWork),
                dpi: GetDpiForWindow(window),
            },
        }
    };
    println!("HELLO {}", serde_json::to_string(&hello).unwrap());
    std::io::stdout().flush().unwrap();
    let (send, commands) = mpsc::channel();
    let input = thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            if let Ok(line) = line {
                if send.send(line).is_err() {
                    break;
                }
            } else {
                break;
            }
        }
    });
    let until = Instant::now() + Duration::from_secs(14);
    let mut closing = false;
    let mut message = MSG::default();
    while Instant::now() < until {
        // SAFETY: pumps only this fixture thread's queue.
        unsafe {
            while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                if message.message == WM_QUIT {
                    closing = true;
                    break;
                }
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        if closing {
            break;
        }
        if let Ok(command) = commands.try_recv() {
            // SAFETY: all mutations target only this process's own live fixture window.
            unsafe {
                match command.as_str() {
                    "rename" => {
                        assert_ne!(SetWindowTextW(window, wide("owned-renamed").as_ptr()), 0);
                    }
                    "move" => {
                        assert_ne!(
                            SetWindowPos(
                                window,
                                null_mut(),
                                120,
                                120,
                                0,
                                0,
                                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE
                            ),
                            0
                        );
                    }
                    "minimize" => {
                        ShowWindow(window, SW_MINIMIZE);
                    }
                    "restore" => {
                        ShowWindow(window, SW_RESTORE);
                    }
                    "close" => {
                        assert_ne!(DestroyWindow(window), 0);
                        closing = true;
                    }
                    _ => panic!("unknown private fixture command"),
                }
            }
            println!("ACK {command}");
            std::io::stdout().flush().unwrap();
        }
        thread::sleep(Duration::from_millis(10));
    }
    // SAFETY: clean up only our own fixture if its internal deadline fired.
    unsafe {
        if IsWindow(window) != 0 {
            assert_ne!(DestroyWindow(window), 0);
        }
        assert_ne!(UnregisterClassW(class.as_ptr(), instance), 0);
    }
    // The private stdin reader exits once the parent drops its pipe after close.
    drop(input);
    assert!(closing, "fixture's internal deadline expired");
}
