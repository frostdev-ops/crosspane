//! One owned passive pixel fixture. No enumeration, pointer movement or foreign HWND fields.
#![allow(unsafe_code, clippy::unwrap_used, clippy::expect_used)]
use std::{
    mem::size_of,
    ptr::{null, null_mut},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::{Dwm::*, Gdi::*},
    System::{LibraryLoader::GetModuleHandleW, Threading::*},
    UI::{HiDpi::*, WindowsAndMessaging::*},
};

#[derive(Clone, Debug, serde::Serialize)]
pub struct Facts {
    pub outer: [i32; 4],
    pub visible: [i32; 4],
    pub visible_now: bool,
}
struct PhysicalScope(DPI_AWARENESS_CONTEXT);
impl PhysicalScope {
    fn new() -> Self {
        // SAFETY: only this fixture thread's awareness changes; Drop restores its exact prior value.
        let prior =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        assert!(!prior.is_null());
        Self(prior)
    }
}
impl Drop for PhysicalScope {
    fn drop(&mut self) {
        // SAFETY: restore only this same thread's retained awareness context.
        let restored = unsafe { SetThreadDpiAwarenessContext(self.0) };
        assert!(!restored.is_null());
    }
}
fn created() -> u64 {
    let (mut c, mut e, mut k, mut u) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: own process pseudo-handle and exact initialized metadata outputs, no mutation.
    let status = unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) };
    assert_ne!(status, 0);
    (u64::from(c.dwHighDateTime) << 32) | u64::from(c.dwLowDateTime)
}
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
struct Paint {
    animate: Arc<AtomicBool>,
    phase: u32,
}
unsafe extern "system" fn procedure(window: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        // SAFETY: Win32 supplies the CREATESTRUCT for this registered owned fixture class.
        let state = unsafe { (*(l as *const CREATESTRUCTW)).lpCreateParams };
        // SAFETY: this pointer stays alive on the owning GUI thread until after DestroyWindow.
        unsafe {
            SetWindowLongPtrW(window, GWLP_USERDATA, state as isize);
        }
    }
    // SAFETY: our class is used only by the fixture we created; no foreign HWND is queried.
    let state = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *mut Paint;
    match message {
        WM_GETMINMAXINFO => {
            // SAFETY: exact native minimum-size output structure provided for our fixture callback.
            unsafe {
                (*(l as *mut MINMAXINFO)).ptMinTrackSize = POINT { x: 360, y: 280 };
            }
            return 0;
        }
        WM_TIMER if !state.is_null() => {
            // SAFETY: state has one GUI-thread owner; only its atomic flag is shared externally.
            unsafe {
                if (*state).animate.load(Ordering::Acquire) {
                    (*state).phase = (*state).phase.wrapping_add(1);
                }
                InvalidateRect(window, null(), 0);
            }
            return 0;
        }
        WM_PAINT if !state.is_null() => {
            let mut ps = PAINTSTRUCT::default();
            let mut client = RECT::default();
            // SAFETY: balanced paint operations and fixed client RECT on our own fixture window only.
            unsafe {
                let dc = BeginPaint(window, &mut ps);
                assert!(!dc.is_null());
                assert_ne!(GetClientRect(window, &mut client), 0);
                let colours = if (*state).phase.is_multiple_of(2) {
                    [(220, 20, 20), (20, 220, 20), (20, 20, 220), (220, 220, 220)]
                } else {
                    [(20, 220, 20), (220, 20, 20), (220, 220, 220), (20, 20, 220)]
                };
                for (i, (r, g, b)) in colours.into_iter().enumerate() {
                    let brush = CreateSolidBrush(r | (g << 8) | (b << 16));
                    assert!(!brush.is_null());
                    let x = client.right / 2;
                    let y = client.bottom / 2;
                    let quadrant = RECT {
                        left: if i % 2 == 0 { 0 } else { x },
                        top: if i < 2 { 0 } else { y },
                        right: if i % 2 == 0 { x } else { client.right },
                        bottom: if i < 2 { y } else { client.bottom },
                    };
                    assert_ne!(FillRect(dc, &quadrant, brush), 0);
                    assert_ne!(DeleteObject(brush), 0);
                }
                assert_ne!(EndPaint(window, &ps), 0);
            }
            return 0;
        }
        _ => {}
    }
    // SAFETY: forwards this owned fixture class callback to the public default procedure.
    unsafe { DefWindowProcW(window, message, w, l) }
}
pub struct OwnedWindow {
    window: usize,
    pid: u32,
    tid: u32,
    created: u64,
    animate: Arc<AtomicBool>,
    stop: mpsc::Sender<()>,
    done: mpsc::Receiver<()>,
    worker: Option<thread::JoinHandle<()>>,
}
impl OwnedWindow {
    pub fn new() -> Self {
        let animate = Arc::new(AtomicBool::new(false));
        let native_animate = animate.clone();
        let (ready, receive) = mpsc::sync_channel(1);
        let (stop, requested) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let worker = thread::spawn(move || {
            let _dpi = PhysicalScope::new();
            let mut state = Box::new(Paint {
                animate: native_animate,
                phase: 0,
            });
            let class = wide(&format!("CrosspaneW25bOwned{}", std::process::id()));
            let title = wide("WP-W2.5b owned pixel fixture");
            // SAFETY: register only this own module's unique fixture class, initialized WNDCLASSW.
            let (instance, window, tid) = unsafe {
                let instance = GetModuleHandleW(null());
                assert!(!instance.is_null());
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(procedure),
                    hInstance: instance,
                    lpszClassName: class.as_ptr(),
                    ..Default::default()
                };
                assert_ne!(RegisterClassW(&wc), 0);
                let window = CreateWindowExW(
                    0,
                    class.as_ptr(),
                    title.as_ptr(),
                    WS_OVERLAPPEDWINDOW,
                    64,
                    64,
                    480,
                    360,
                    null_mut(),
                    null_mut(),
                    instance,
                    (&mut *state as *mut Paint).cast(),
                );
                assert!(!window.is_null());
                ShowWindow(window, SW_SHOWNOACTIVATE);
                UpdateWindow(window);
                assert_ne!(SetTimer(window, 1, 100, None), 0);
                (instance, window, GetCurrentThreadId())
            };
            ready.send((window as usize, tid)).unwrap();
            while matches!(requested.try_recv(), Err(mpsc::TryRecvError::Empty)) {
                let mut message = MSG::default();
                // SAFETY: pump only this exact fixture HWND on its owning thread, no global input.
                unsafe {
                    while PeekMessageW(&mut message, window, 0, 0, PM_REMOVE) != 0 {
                        TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
                thread::sleep(Duration::from_millis(5));
            }
            // SAFETY: stop only the timer/window/class created above on this same thread.
            unsafe {
                KillTimer(window, 1);
                assert_ne!(DestroyWindow(window), 0);
                assert_ne!(UnregisterClassW(class.as_ptr(), instance), 0);
            }
            drop(state);
            let _ = finished.send(());
        });
        let (window, tid) = receive
            .recv_timeout(Duration::from_secs(3))
            .expect("owned fixture window readiness");
        Self {
            window,
            pid: std::process::id(),
            tid,
            created: created(),
            animate,
            stop,
            done,
            worker: Some(worker),
        }
    }
    pub fn claim(&self) -> serde_json::Value {
        let root = std::path::PathBuf::from(std::env::var_os("CROSSPANE_E2_FIXTURE_ROOT").unwrap());
        let executable = root.join("fixture/owned-window.exe");
        assert_eq!(
            executable.canonicalize().unwrap(),
            std::env::current_exe().unwrap().canonicalize().unwrap()
        );
        serde_json::json!({"pid":self.pid,"process_created":self.created,"executable":executable})
    }
    pub fn facts(&self) -> Facts {
        let _dpi = PhysicalScope::new();
        let window = self.window as HWND;
        let mut pid = 0;
        // SAFETY: only metadata is read until this exact own window/process creation is re-admitted.
        let tid = unsafe { GetWindowThreadProcessId(window, &mut pid) };
        assert_eq!((pid, tid), (self.pid, self.tid));
        assert_eq!(created(), self.created);
        let (mut outer, mut visible) = (RECT::default(), RECT::default());
        // SAFETY: exact owned HWND was freshly admitted, physical RECT buffers have exact lengths.
        let visible_now = unsafe {
            assert_ne!(GetWindowRect(window, &mut outer), 0);
            assert!(
                DwmGetWindowAttribute(
                    window,
                    DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                    (&mut visible as *mut RECT).cast(),
                    size_of::<RECT>() as u32
                ) >= 0
            );
            IsWindowVisible(window) != 0
        };
        Facts {
            outer: [outer.left, outer.top, outer.right, outer.bottom],
            visible: [visible.left, visible.top, visible.right, visible.bottom],
            visible_now,
        }
    }
    pub fn animate(&self, enabled: bool) {
        self.animate.store(enabled, Ordering::Release);
    }
}
impl Drop for OwnedWindow {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if self.done.recv_timeout(Duration::from_secs(3)).is_ok() {
            if let Some(worker) = self.worker.take() {
                assert!(worker.join().is_ok());
            }
        } else if !thread::panicking() {
            panic!("owned fixture window cleanup timeout");
        }
    }
}
