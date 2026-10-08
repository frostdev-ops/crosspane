//! Owned M2 fixture: one overlapped window, created and pumped by a child process of this probe.
//! The parent acts only on a window whose owning process and thread match the identity the child
//! reported, so it never touches a window it did not create.
#![allow(dead_code)] // T21 consumes the parent-side controls; the child mode is used now.
#![allow(unsafe_code)]

use crosspane_platform::PlatformError;
use crosspane_platform_windows::model::parking::NativeIdentity;
use std::{
    io::{BufRead, BufReader},
    iter::once,
    process::{Child, Command, ExitCode, Stdio},
    ptr::{null, null_mut},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{FILETIME, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Gdi::COLOR_WINDOW,
    System::{
        LibraryLoader::GetModuleHandleW,
        Threading::{GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId, GetProcessTimes},
    },
    UI::{
        HiDpi::{
            DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            SetThreadDpiAwarenessContext,
        },
        WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetWindowRect,
            GetWindowThreadProcessId, IsWindow, IsZoomed, MSG, PM_REMOVE, PeekMessageW,
            PostMessageW, PostQuitMessage, RegisterClassW, SW_MAXIMIZE, SWP_NOACTIVATE,
            SWP_NOZORDER, SetWindowPos, ShowWindow, TranslateMessage, UnregisterClassW, WM_CLOSE,
            WM_DESTROY, WM_DPICHANGED, WM_QUIT, WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
        },
    },
};

const TITLE: &str = "CrosspaneOwnedM2";
/// The child gives up on its own after this long, so a lost parent cannot leave it running.
const LIFETIME: Duration = Duration::from_secs(120);
/// How long the parent waits for the identity line.
const READY: Duration = Duration::from_secs(5);
/// How long the parent waits for the child to exit after `WM_CLOSE`.
const EXIT: Duration = Duration::from_secs(5);
/// How long the parent waits for the maximized state to appear.
const MAXIMIZE: Duration = Duration::from_secs(5);

/// Whether the window answers `WM_DPICHANGED` with the suggested rectangle (`--dpi-resize`). Set
/// once by [`run_window`] before the window exists; this process creates only that one window.
static DPI_RESIZE: AtomicBool = AtomicBool::new(false);

/// Child mode: creates the owned window, prints one `FIXTURE` identity line, then pumps messages
/// until the window closes or [`LIFETIME`] passes. This process creates no other window.
/// `dpi_resize` is the `--dpi-resize` flag: the window then applies the rectangle that
/// `WM_DPICHANGED` suggests, as a per-monitor-v2 app does. Without it the message is defaulted.
pub fn run_fixture(dpi_resize: bool) -> ExitCode {
    match run_window(dpi_resize) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("FIXTURE failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_window(dpi_resize: bool) -> Result<(), PlatformError> {
    // SAFETY: PMv2 affects only this process's main thread, the only thread that owns a window
    // here. The process ends with the fixture, so the prior context is not restored.
    let previous =
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    if previous.is_null() {
        return Err(backend("fixture PMv2 context unavailable"));
    }
    // Stored before the window exists, so the first message it receives already sees the flag.
    DPI_RESIZE.store(dpi_resize, Ordering::Release);
    // SAFETY: returns this executable's module handle, which stays valid for the process lifetime.
    let module = unsafe { GetModuleHandleW(null()) };
    // SAFETY: reads this process's and thread's identifiers.
    let (pid, tid) = unsafe { (GetCurrentProcessId(), GetCurrentThreadId()) };
    let class = wide(&format!("Crosspane.OwnedM2.{pid}.{tid}"));
    let definition = WNDCLASSW {
        lpfnWndProc: Some(procedure),
        hInstance: module,
        lpszClassName: class.as_ptr(),
        hbrBackground: (COLOR_WINDOW + 1) as usize as _,
        ..Default::default()
    };
    // SAFETY: registers a class whose name buffer and procedure stay valid until the guard below
    // unregisters it.
    if unsafe { RegisterClassW(&definition) } == 0 {
        return Err(backend("fixture class registration refused"));
    }
    let mut owned = Owned {
        class,
        module,
        window: null_mut(),
    };
    let title = wide(TITLE);
    // SAFETY: creates one overlapped window in the private class registered above.
    owned.window = unsafe {
        CreateWindowExW(
            0,
            owned.class.as_ptr(),
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
        )
    };
    if owned.window.is_null() {
        return Err(backend("fixture window creation refused"));
    }
    let created = process_created()?;
    println!(
        "FIXTURE hwnd={} pid={pid} tid={tid} created={created}",
        owned.window as usize as u64
    );
    pump();
    Ok(())
}

/// Sets PMv2 on this thread for the scope, so geometry reads are physical pixels. Drop restores
/// the context captured by `new`.
struct PhysicalScope(DPI_AWARENESS_CONTEXT);

impl PhysicalScope {
    fn new() -> Result<Self, PlatformError> {
        // SAFETY: affects only this thread's DPI context; the prior context is kept for Drop.
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        if previous.is_null() {
            return Err(backend("PMv2 context unavailable for geometry read"));
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

/// Destroys the fixture window and unregisters its class on every exit path.
struct Owned {
    class: Vec<u16>,
    module: HINSTANCE,
    window: HWND,
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: checks only the fixture's own window handle, which WM_CLOSE may already have
        // destroyed.
        if !self.window.is_null() && unsafe { IsWindow(self.window) } != 0 {
            // SAFETY: destroys only the window this fixture created, verified live just above.
            unsafe { DestroyWindow(self.window) };
        }
        // SAFETY: unregisters this fixture's private class once its window is gone.
        unsafe { UnregisterClassW(self.class.as_ptr(), self.module) };
    }
}

/// The fixture's only window procedure. Destroying the window posts `WM_QUIT`, which ends the loop.
/// With `--dpi-resize`, `WM_DPICHANGED` moves and resizes the window to the suggested rectangle.
unsafe extern "system" fn procedure(window: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if message == WM_DESTROY {
        // SAFETY: posts a quit to this thread's own queue, which only this fixture uses.
        unsafe { PostQuitMessage(0) };
        return 0;
    }
    if message == WM_DPICHANGED && DPI_RESIZE.load(Ordering::Acquire) {
        // SAFETY: on WM_DPICHANGED, lParam points to the suggested RECT for this message only. It
        // is copied out here, before the message returns.
        let suggested = unsafe { *(l as *const RECT) };
        // SAFETY: moves and resizes only this fixture window, to the rectangle the system
        // suggested. Z-order and activation are left unchanged.
        unsafe {
            SetWindowPos(
                window,
                null_mut(),
                suggested.left,
                suggested.top,
                suggested.right - suggested.left,
                suggested.bottom - suggested.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
        };
        return 0;
    }
    // SAFETY: forwards this fixture window's message to the default procedure unchanged.
    unsafe { DefWindowProcW(window, message, w, l) }
}

/// Drains this thread's queue until `WM_QUIT` or the lifetime runs out.
fn pump() {
    let deadline = Instant::now() + LIFETIME;
    let mut message = MSG::default();
    loop {
        // SAFETY: drains only this thread's queue into a local message.
        while unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) } != 0 {
            if message.message == WM_QUIT {
                return;
            }
            // SAFETY: dispatches a message just read from this thread's own queue.
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        if Instant::now() >= deadline {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn process_created() -> Result<u64, PlatformError> {
    let mut created = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: queries this process's own times into local out-parameters.
    let okay = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    if okay == 0 {
        return Err(backend("fixture process creation time unavailable"));
    }
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

/// A running fixture child and the identity it reported. Every window action goes through
/// [`FixtureChild::owned_window`], so the parent never acts on a window it did not verify.
#[derive(Debug)]
pub struct FixtureChild {
    child: Child,
    pub identity: NativeIdentity,
}

impl FixtureChild {
    /// Starts a fixture child from this same executable and reads its identity line within five
    /// seconds. A child that fails to report is killed.
    pub fn spawn() -> Result<Self, PlatformError> {
        Self::spawn_with(false)
    }

    /// Like [`FixtureChild::spawn`], but the child runs with `--dpi-resize`, so it applies the
    /// rectangle that `WM_DPICHANGED` suggests.
    pub fn spawn_dpi_resize() -> Result<Self, PlatformError> {
        Self::spawn_with(true)
    }

    fn spawn_with(dpi_resize: bool) -> Result<Self, PlatformError> {
        let exe = std::env::current_exe().map_err(io_error)?;
        let mut command = Command::new(exe);
        command.arg("fixture");
        if dpi_resize {
            command.arg("--dpi-resize");
        }
        let mut child = command
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(io_error)?;
        match read_identity(&mut child) {
            Ok(identity) => Ok(Self { child, identity }),
            Err(error) => {
                reap(&mut child);
                Err(error)
            }
        }
    }

    /// The fixture window's outer rectangle as `[left, top, right, bottom]`, in physical pixels
    /// whatever the parent's own DPI awareness.
    pub fn rect(&mut self) -> Result<[i32; 4], PlatformError> {
        let window = self.owned_window()?;
        let _physical = PhysicalScope::new()?;
        let mut outer = RECT::default();
        // SAFETY: reads the geometry of the fixture window verified by owned_window.
        if unsafe { GetWindowRect(window, &mut outer) } == 0 {
            return Err(backend("fixture rect unavailable"));
        }
        Ok([outer.left, outer.top, outer.right, outer.bottom])
    }

    /// Maximizes the fixture window and polls up to five seconds for the zoomed state.
    pub fn maximize(&mut self) -> Result<(), PlatformError> {
        let window = self.owned_window()?;
        // SAFETY: shows only the fixture window verified by owned_window. The return value is
        // the prior visibility, not success, so the zoomed state is polled below.
        unsafe { ShowWindow(window, SW_MAXIMIZE) };
        let until = Instant::now() + MAXIMIZE;
        loop {
            // SAFETY: reads the zoomed state of the verified fixture window.
            if unsafe { IsZoomed(window) } != 0 {
                return Ok(());
            }
            if Instant::now() >= until {
                return Err(backend("fixture did not maximize within 5 s"));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Posts `WM_CLOSE` to the fixture window and waits up to five seconds for the child to exit.
    /// A child still running after that is killed and reported as an error.
    pub fn close(&mut self) -> Result<(), PlatformError> {
        self.owned_window()?;
        close_owned(self.identity.hwnd, self.identity.pid)?;
        let until = Instant::now() + EXIT;
        loop {
            match self.child.try_wait().map_err(io_error)? {
                Some(_) => return Ok(()),
                None if Instant::now() < until => thread::sleep(Duration::from_millis(20)),
                None => {
                    reap(&mut self.child);
                    return Err(backend("fixture did not exit after WM_CLOSE; killed"));
                }
            }
        }
    }

    /// Returns the fixture window only while the child runs and the window still belongs to the
    /// recorded process and thread.
    fn owned_window(&mut self) -> Result<HWND, PlatformError> {
        if self.child.try_wait().map_err(io_error)?.is_some() {
            return Err(backend("fixture exited; no window action"));
        }
        match owner_thread(self.identity.hwnd, self.identity.pid) {
            Some(thread) if thread == self.identity.tid => Ok(self.identity.hwnd as usize as HWND),
            _ => Err(backend("fixture window ownership changed; no action")),
        }
    }
}

impl Drop for FixtureChild {
    fn drop(&mut self) {
        // A child still running when the parent drops it is killed, so no window outlives the
        // probe.
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            reap(&mut self.child);
        }
    }
}

/// Posts `WM_CLOSE` to `hwnd` only when process `pid` owns it. Any other window is refused.
pub fn close_owned(hwnd: u64, pid: u32) -> Result<(), PlatformError> {
    owner_thread(hwnd, pid)
        .ok_or_else(|| backend("window is not owned by the fixture process; not closed"))?;
    // SAFETY: posts WM_CLOSE to a window whose owning process was verified as `pid` just above.
    if unsafe { PostMessageW(hwnd as usize as HWND, WM_CLOSE, 0, 0) } == 0 {
        return Err(backend("fixture close refused"));
    }
    Ok(())
}

/// The thread that owns `hwnd` when the owning process is `pid`. `None` for an invalid window or
/// a window of any other process.
fn owner_thread(hwnd: u64, pid: u32) -> Option<u32> {
    let mut owner = 0;
    // SAFETY: reads the owning process and thread of a handle. No window state changes.
    let thread = unsafe { GetWindowThreadProcessId(hwnd as usize as HWND, &mut owner) };
    (thread != 0 && owner == pid).then_some(thread)
}

fn read_identity(child: &mut Child) -> Result<NativeIdentity, PlatformError> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| backend("fixture stdout missing"))?;
    let (sender, receiver) = mpsc::channel();
    // The fixture prints only the identity line, so the pipe can close once that line is read.
    thread::spawn(move || {
        let mut line = String::new();
        let read = BufReader::new(stdout).read_line(&mut line).map(|_| line);
        let _ = sender.send(read);
    });
    match receiver.recv_timeout(READY) {
        Ok(Ok(line)) => {
            parse_identity(&line).ok_or_else(|| backend("fixture identity line malformed"))
        }
        Ok(Err(error)) => Err(io_error(error)),
        Err(_) => Err(backend("fixture identity line not received within 5 s")),
    }
}

/// Parses `FIXTURE hwnd= pid= tid= created=`. Any other shape is refused.
fn parse_identity(line: &str) -> Option<NativeIdentity> {
    let mut fields = line.split_whitespace();
    if fields.next()? != "FIXTURE" {
        return None;
    }
    let (mut hwnd, mut pid, mut tid, mut created) = (None, None, None, None);
    for field in fields {
        let (key, value) = field.split_once('=')?;
        match key {
            "hwnd" => hwnd = Some(value.parse().ok()?),
            "pid" => pid = Some(value.parse().ok()?),
            "tid" => tid = Some(value.parse().ok()?),
            "created" => created = Some(value.parse().ok()?),
            _ => return None,
        }
    }
    Some(NativeIdentity {
        hwnd: hwnd?,
        pid: pid?,
        tid: tid?,
        process_created: created?,
    })
}

/// Kills and collects a child this probe started, so no fixture outlives a failed step.
fn reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(once(0)).collect()
}

fn backend(text: &str) -> PlatformError {
    PlatformError::Backend(text.to_owned())
}

fn io_error(error: std::io::Error) -> PlatformError {
    PlatformError::Backend(format!("fixture I/O: {error}"))
}
