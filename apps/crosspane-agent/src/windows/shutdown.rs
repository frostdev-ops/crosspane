//! Scoped console and session-end observers. Callbacks publish only atomic state.

use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::Console::{
    CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    SetConsoleCtrlHandler,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, MSG,
    PostMessageW, PostQuitMessage, RegisterClassW, TranslateMessage, UnregisterClassW, WM_CLOSE,
    WM_DESTROY, WM_ENDSESSION, WM_QUERYENDSESSION, WNDCLASSW,
};

static ACTIVE: AtomicBool = AtomicBool::new(false);
static FINISHED: AtomicBool = AtomicBool::new(true);
static SIGNALS: AtomicU32 = AtomicU32::new(0);

fn end_session() {
    SIGNALS.fetch_add(1, Ordering::Release);
    let deadline = Instant::now() + Duration::from_millis(4_500);
    while !FINISHED.load(Ordering::Acquire) {
        if Instant::now() >= deadline {
            super::process::exit_without_handlers(1);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

unsafe extern "system" fn console(kind: u32) -> i32 {
    match kind {
        CTRL_C_EVENT | CTRL_BREAK_EVENT => {
            SIGNALS.fetch_add(1, Ordering::Release);
            1
        }
        CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => {
            end_session();
            1
        }
        _ => 0,
    }
}

unsafe extern "system" fn window(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION if wparam != 0 => {
            end_session();
            0
        }
        WM_CLOSE => {
            // SAFETY: this callback runs on the thread that owns this observer HWND.
            unsafe { DestroyWindow(hwnd) };
            0
        }
        WM_DESTROY => {
            // SAFETY: terminates only this observer thread's message loop.
            unsafe { PostQuitMessage(0) };
            0
        }
        _ => {
            // SAFETY: arguments are the unmodified values provided by the system callback.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
    }
}

struct Observer {
    hwnd: HWND,
    instance: windows_sys::Win32::Foundation::HINSTANCE,
    class: Vec<u16>,
}

impl Drop for Observer {
    fn drop(&mut self) {
        // SAFETY: the observer's HWND and registered class belong to this thread. DestroyWindow
        // is harmless if WM_CLOSE has already destroyed the HWND; the class has no live windows.
        unsafe {
            DestroyWindow(self.hwnd);
            UnregisterClassW(self.class.as_ptr(), self.instance);
        }
    }
}

fn observer(ready: Sender<Result<usize>>) {
    let prepared = (|| -> Result<Observer> {
        let class: Vec<u16> = format!("CrosspaneShutdown{}", std::process::id())
            .encode_utf16()
            .chain(Some(0))
            .collect();
        // SAFETY: null names the current executable module, borrowed for the process lifetime.
        let instance = unsafe { GetModuleHandleW(null_mut()) };
        ensure!(!instance.is_null(), "read shutdown observer module");
        let definition = WNDCLASSW {
            lpfnWndProc: Some(window),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..Default::default()
        };
        // SAFETY: the class definition and its null-terminated name stay alive through the call.
        let registered = unsafe { RegisterClassW(&definition) };
        ensure!(
            registered != 0,
            "register shutdown observer: {}",
            std::io::Error::last_os_error()
        );
        let mut guard = Observer {
            hwnd: null_mut(),
            instance,
            class,
        };
        // SAFETY: registered private class, no parent, invisible top-level window. A message-only
        // window would not receive the session-end broadcast, so this is a hidden top-level HWND.
        guard.hwnd = unsafe {
            CreateWindowExW(
                0,
                guard.class.as_ptr(),
                guard.class.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                instance,
                null_mut(),
            )
        };
        ensure!(
            !guard.hwnd.is_null(),
            "create shutdown observer: {}",
            std::io::Error::last_os_error()
        );
        Ok(guard)
    })();
    let observer = match prepared {
        Ok(observer) => observer,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    if ready.send(Ok(observer.hwnd as usize)).is_err() {
        return;
    }
    let mut message = MSG::default();
    loop {
        // SAFETY: writable message storage and this thread's unfiltered message queue.
        let result = unsafe { GetMessageW(&mut message, null_mut(), 0, 0) };
        if result <= 0 {
            break;
        }
        // SAFETY: message was returned by GetMessageW and is dispatched on its owning thread.
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

#[derive(Debug)]
pub(crate) struct Watch {
    hwnd: usize,
    observer: Option<JoinHandle<()>>,
    dispatch: Option<JoinHandle<()>>,
    console: bool,
}

impl Watch {
    pub(crate) fn install(stop: Sender<()>) -> Result<Self> {
        Self::with_callback(move || {
            let _ = stop.send(());
        })
    }

    pub(crate) fn for_agent(events: Sender<crate::agent::Event>) -> Result<Self> {
        Self::with_callback(move || {
            crate::platform::exit_deadline(Duration::from_secs(5));
            let _ = events.send(crate::agent::Event::Shutdown);
        })
    }

    fn with_callback(mut stop: impl FnMut() + Send + 'static) -> Result<Self> {
        ensure!(
            ACTIVE
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "a shutdown observer is already installed"
        );
        SIGNALS.store(0, Ordering::Release);
        FINISHED.store(false, Ordering::Release);
        let mut watch = Self {
            hwnd: 0,
            observer: None,
            dispatch: None,
            console: false,
        };
        let (ready, result) = std::sync::mpsc::channel();
        watch.observer = Some(
            std::thread::Builder::new()
                .name("session-end".into())
                .spawn(move || observer(ready))
                .context("spawn session-end observer")?,
        );
        // A successful observer owns an HWND until acknowledged. recv has no timeout to avoid
        // abandoning a live private HWND; the startup thread performs only bounded local calls.
        watch.hwnd = result
            .recv()
            .context("session-end observer ended during startup")??;
        // SAFETY: function has the ABI required by SetConsoleCtrlHandler, and uses only static
        // atomics. Drop removes exactly this registration before relinquishing singleton state.
        let installed = unsafe { SetConsoleCtrlHandler(Some(console), 1) };
        ensure!(
            installed != 0,
            "install console shutdown handler: {}",
            std::io::Error::last_os_error()
        );
        watch.console = true;
        watch.dispatch = Some(
            std::thread::Builder::new()
                .name("stop-signals".into())
                .spawn(move || {
                    let mut delivered = 0;
                    while !FINISHED.load(Ordering::Acquire) {
                        let observed = SIGNALS.load(Ordering::Acquire);
                        if observed > delivered {
                            if delivered != 0 || observed > 1 {
                                super::process::exit_without_handlers(1);
                            }
                            delivered = observed;
                            stop();
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                })
                .context("spawn shutdown dispatcher")?,
        );
        Ok(watch)
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        FINISHED.store(true, Ordering::Release);
        if self.console {
            // SAFETY: removes the exact handler installed by this guard; static storage survives.
            unsafe { SetConsoleCtrlHandler(Some(console), 0) };
        }
        if self.hwnd != 0 {
            // SAFETY: posts only to the private observer HWND; the observer owns its destruction.
            unsafe { PostMessageW(self.hwnd as HWND, WM_CLOSE, 0, 0) };
        }
        if let Some(dispatch) = self.dispatch.take() {
            let _ = dispatch.join();
        }
        if let Some(observer) = self.observer.take() {
            let _ = observer.join();
        }
        ACTIVE.store(false, Ordering::Release);
    }
}
