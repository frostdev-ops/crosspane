//! Windows native drag classification on the existing capture owner thread.
//! `[E]` MOVESIZE events describe move OR resize; the pure model admits only stable content
//! geometry following a primary-only physical gesture. No OLE, title or text acquisition.
//! `[P]` Genuine cached WindowIds are resolved by the existing WindowSource, never allocated here.
//! `[U]` WinEvent delivery/native queries can lag. Settlement requires a matching actual END;
//! one accepted SendInput UP alone is insufficient. The D-6 placer only moves restored windows.

#![allow(unsafe_code)]

use crate::{
    inject::DragSettlement,
    model::{
        drag::{Detector, EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZESTART, WindowFact},
        window::Identity,
    },
    window::{NativeWindow, WindowResolver, WindowsWindowSource},
};
use crosspane_platform::{
    CaptureEvent, CapturePortal, IoGate, PlatformError, PortalId, WindowSource,
};
use crosspane_types::{geom::PixelRect, id::WindowId, time::MonoTime};
use std::{
    cell::RefCell,
    collections::VecDeque,
    ptr::null_mut,
    sync::{Arc, Mutex},
};
use windows_sys::Win32::{
    Foundation::{HWND, POINT, RECT},
    Graphics::Gdi::ClientToScreen,
    UI::{
        Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
        WindowsAndMessaging::*,
    },
};

/// Same source as capture/injection. Source locks cover cached-ID copies only and are released
/// before resolver/native calls. Fixed order is source then internal injection Driver; LL
/// callbacks take neither. There is no outer pointer mutex or second injection source.
pub struct DragConfig {
    source: Arc<Mutex<WindowsWindowSource>>,
    resolver: WindowResolver,
    pub(crate) settlement: DragSettlement,
}
impl std::fmt::Debug for DragConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DragConfig(..)")
    }
}
impl DragConfig {
    pub fn new(
        source: Arc<Mutex<WindowsWindowSource>>,
        settlement: DragSettlement,
    ) -> Result<Self, PlatformError> {
        let resolver = source.try_lock().map_err(|_| unavailable())?.resolver();
        Ok(Self {
            source,
            resolver,
            settlement,
        })
    }
    pub(crate) fn same_gate(&self, gate: &Arc<IoGate>) -> bool {
        self.settlement.same_gate(gate)
    }
    fn admitted(&self, hwnd: u64) -> Option<WindowId> {
        let ids: Vec<_> = self
            .source
            .try_lock()
            .ok()?
            .windows()
            .ok()?
            .into_iter()
            .map(|w| w.id)
            .collect();
        ids.into_iter()
            .find(|id| self.resolver.resolve(*id).is_some_and(|w| w.hwnd == hwnd))
    }
    pub(crate) fn resolve(&self, id: WindowId) -> Option<NativeWindow> {
        self.resolver.resolve(id)
    }
    fn fact(&self, id: WindowId) -> Option<WindowFact> {
        let original = self.resolve(id)?;
        let hwnd = original.hwnd as HWND;
        let mut rect = RECT::default();
        // SAFETY: read-only client coordinates on an already admitted live identity; owner
        // thread is PMv2. Revalidation after the reads rejects HWND reuse/geometry loss.
        unsafe {
            if IsWindowVisible(hwnd) == 0
                || IsIconic(hwnd) != 0
                || GetClientRect(hwnd, &mut rect) == 0
            {
                return None;
            }
            let mut points = [
                POINT {
                    x: rect.left,
                    y: rect.top,
                },
                POINT {
                    x: rect.right,
                    y: rect.bottom,
                },
            ];
            if ClientToScreen(hwnd, &mut points[0]) == 0
                || ClientToScreen(hwnd, &mut points[1]) == 0
            {
                return None;
            }
            // Read before the revalidation so the DWM frame is covered by the same identity check.
            let frame = visible_bounds(hwnd).ok()?;
            if self.resolve(id) != Some(original) {
                return None;
            }
            Some(WindowFact {
                window: id,
                identity: Identity {
                    hwnd: original.hwnd,
                    pid: original.pid,
                    tid: original.tid,
                    process_created: original.process_created,
                },
                content: PixelRect::new(
                    crosspane_types::geom::euclid::point2(points[0].x, points[0].y),
                    crosspane_types::geom::euclid::point2(points[1].x, points[1].y),
                ),
                frame: PixelRect::new(
                    crosspane_types::geom::euclid::point2(frame[0], frame[1]),
                    crosspane_types::geom::euclid::point2(frame[2], frame[3]),
                ),
            })
        }
    }
}
fn unavailable() -> PlatformError {
    PlatformError::Backend("Windows drag observation unavailable".into())
}

#[derive(Default)]
struct Events {
    rows: VecDeque<(u32, u64)>,
    lost: bool,
}
thread_local! { static EVENTS: RefCell<Events> = RefCell::new(Events::default()); }
unsafe extern "system" fn lifecycle(
    _: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    object: i32,
    child: i32,
    _: u32,
    _: u32,
) {
    if object != OBJID_WINDOW || child != CHILDID_SELF as i32 || hwnd.is_null() {
        return;
    }
    EVENTS.with(|events| {
        let mut events = events.borrow_mut();
        if events.rows.len() >= 64 {
            events.lost = true;
        } else {
            events.rows.push_back((event, hwnd as u64));
        }
    });
}

pub(crate) struct Observer {
    pub(crate) config: DragConfig,
    native_hook: HWINEVENTHOOK,
    detector: Detector,
    current: Option<WindowId>,
    awaiting: Option<(WindowId, NativeWindow)>,
    ended: bool,
}
impl Observer {
    pub(crate) fn new(config: DragConfig) -> Result<Self, PlatformError> {
        EVENTS.with(|e| *e.borrow_mut() = Events::default());
        // SAFETY: out-of-context callback runs only on this already-owned message pump; it
        // stores bounded scalar lifecycle facts and never hooks keyboard/mouse a second time.
        let native_hook = unsafe {
            SetWinEventHook(
                EVENT_SYSTEM_MOVESIZESTART,
                EVENT_SYSTEM_MOVESIZEEND,
                null_mut(),
                Some(lifecycle),
                0,
                0,
                WINEVENT_OUTOFCONTEXT,
            )
        };
        if native_hook.is_null() {
            return Err(unavailable());
        }
        Ok(Self {
            config,
            native_hook,
            detector: Detector::default(),
            current: None,
            awaiting: None,
            ended: false,
        })
    }
    pub(crate) fn set_portals(
        &mut self,
        portals: &[(CapturePortal, PixelRect)],
        at: MonoTime,
    ) -> Result<Vec<CaptureEvent>, PlatformError> {
        self.detector.set_portals(portals, at)
    }
    pub(crate) fn sample(
        &mut self,
        point: (i32, i32),
        delta: Option<(f64, f64)>,
        primary_only: bool,
        at: MonoTime,
    ) -> Result<Vec<CaptureEvent>, PlatformError> {
        let (rows, lost) = EVENTS.with(|e| {
            let mut e = e.borrow_mut();
            (std::mem::take(&mut e.rows), e.lost)
        });
        if lost {
            self.detector.end(at);
            return Err(unavailable());
        }
        let mut events = Vec::new();
        for (kind, hwnd) in rows {
            if kind == EVENT_SYSTEM_MOVESIZESTART {
                if self.awaiting.is_some() {
                    continue;
                }
                self.current = self.config.admitted(hwnd);
                if let Some(fact) = self.current.and_then(|id| self.config.fact(id)) {
                    events.extend(self.detector.start(fact, point, primary_only, at));
                }
            } else if kind == EVENT_SYSTEM_MOVESIZEEND {
                if let Some((id, original)) = self.awaiting {
                    if original.hwnd == hwnd && self.config.resolve(id) == Some(original) {
                        self.ended = true;
                    }
                } else if self
                    .current
                    .and_then(|id| self.config.resolve(id))
                    .is_some_and(|w| w.hwnd == hwnd)
                {
                    events.extend(self.detector.end(at));
                    self.current = None;
                }
            }
        }
        if self.awaiting.is_none() {
            events.extend(self.detector.sample(
                self.current.and_then(|id| self.config.fact(id)),
                point,
                delta,
                primary_only,
                at,
            ));
        }
        Ok(events)
    }
    pub(crate) fn target(
        &self,
        portal: PortalId,
    ) -> Result<(WindowId, NativeWindow), PlatformError> {
        let fact = self
            .detector
            .at_edge(portal)
            .ok_or(PlatformError::NotFound)?;
        let native = self
            .config
            .resolve(fact.window)
            .ok_or(PlatformError::NotFound)?;
        if self.config.fact(fact.window) != Some(fact) {
            return Err(PlatformError::NotFound);
        }
        Ok((fact.window, native))
    }
    pub(crate) fn consume(&mut self, target: (WindowId, NativeWindow)) {
        self.detector.consume();
        self.awaiting = Some(target);
        self.ended = false;
    }
    pub(crate) fn ended(&self, target: NativeWindow) -> bool {
        self.ended
            && self.awaiting.is_some_and(|(id, original)| {
                original == target && self.config.resolve(id) == Some(target)
            })
    }
    pub(crate) fn retire(&mut self, at: MonoTime) -> Vec<CaptureEvent> {
        self.awaiting = None;
        self.current = None;
        self.ended = false;
        self.detector.end(at)
    }
}
impl Drop for Observer {
    fn drop(&mut self) {
        // SAFETY: only this owner's hook is removed before its callback TLS is cleared.
        unsafe {
            UnhookWinEvent(self.native_hook);
        }
        EVENTS.with(|e| *e.borrow_mut() = Events::default());
    }
}

use crate::{
    model::{
        drag::{PLACE_RECOMPUTES, RestorePlacer, frame_changed, placed_origin, placement_monitor},
        geometry::DisplayIds,
    },
    window::MonitorReader,
};
use crosspane_types::{geom::PointDevice, id::DisplayId};
use std::{
    mem::size_of,
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Graphics::{
        Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
        Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow},
    },
    UI::HiDpi::{
        DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        SetThreadDpiAwarenessContext,
    },
};

/// Poll interval for the DWM read-back of a placed window (D-6).
const PLACE_POLL: Duration = Duration::from_millis(10);

/// Native DRAG-v0 D-6 placement of an already restored window: PMv2-scoped physical geometry,
/// one asynchronous no-size move, and a bounded DWM read-back. Never parks, restores or journals.
pub struct WindowsRestorePlacer {
    resolver: WindowResolver,
    ids: Arc<Mutex<DisplayIds>>,
    monitors: MonitorReader,
}
impl std::fmt::Debug for WindowsRestorePlacer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WindowsRestorePlacer(..)")
    }
}
impl WindowsRestorePlacer {
    pub fn new(
        resolver: WindowResolver,
        ids: Arc<Mutex<DisplayIds>>,
        monitors: MonitorReader,
    ) -> Self {
        Self {
            resolver,
            ids,
            monitors,
        }
    }
}
impl RestorePlacer for WindowsRestorePlacer {
    fn place(
        &mut self,
        window: WindowId,
        display: DisplayId,
        origin: PointDevice,
        deadline: Instant,
    ) -> Result<(), PlatformError> {
        if Instant::now() >= deadline {
            return Err(PlatformError::Timeout);
        }
        let native = self
            .resolver
            .resolve(window)
            .ok_or(PlatformError::NotFound)?;
        // The reader runs first; the identity lock is taken afterwards and never held across it.
        let probes = (self.monitors)()?;
        let monitor = {
            let mut ids = self
                .ids
                .lock()
                .map_err(|_| PlatformError::Backend("placement display identities".into()))?;
            placement_monitor(display, &probes, &mut ids)?
        };
        if Instant::now() >= deadline {
            return Err(PlatformError::Timeout);
        }
        let _dpi = DpiScope::new()?;
        let hwnd = native.hwnd as HWND;
        // SAFETY: read-only state queries on the freshly resolved window; no window or system change.
        let (exists, shown, style) = unsafe {
            (
                IsWindow(hwnd) != 0,
                IsWindowVisible(hwnd) != 0 && IsIconic(hwnd) == 0 && IsZoomed(hwnd) == 0,
                GetWindowLongPtrW(hwnd, GWL_STYLE) as u32,
            )
        };
        if !exists {
            return Err(PlatformError::NotFound);
        }
        if !shown {
            return Err(PlatformError::Unsupported(
                "placement of a minimized, maximized or hidden window",
            ));
        }
        let mut window_rect = RECT::default();
        // SAFETY: exact RECT output buffer for the resolved window, under PMv2.
        if unsafe { GetWindowRect(hwnd, &mut window_rect) } == 0 {
            return Err(PlatformError::Backend("placement geometry unknown".into()));
        }
        let outer = corners(window_rect);
        let visible = visible_bounds(hwnd)?;
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: read-only facts of the window's current monitor, into an exact output structure.
        let monitor_read = unsafe {
            GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut info)
        };
        if monitor_read == 0 {
            return Err(PlatformError::Backend("placement monitor unknown".into()));
        }
        if visible == corners(info.rcMonitor) && (style & WS_CAPTION) == 0 {
            return Err(PlatformError::Unsupported(
                "placement of a fullscreen window",
            ));
        }
        if self.resolver.resolve(window) != Some(native) {
            return Err(PlatformError::NotFound);
        }
        let mut target = placed_origin(outer, visible, origin, &monitor)?;
        if near((visible[0], visible[1]), target.visible) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(PlatformError::Timeout);
        }
        post_move(hwnd, target.outer)?;
        // The frame the current target was derived from. A mixed-DPI app that re-lays out on
        // `WM_DPICHANGED` changes its visible size or border offset (W1.6b Low 2), so the target is
        // re-derived from the new frame, at most `PLACE_RECOMPUTES` times.
        let (mut basis_outer, mut basis_visible) = (outer, visible);
        let mut recomputes = 0_u32;
        loop {
            if self.resolver.resolve(window) != Some(native) {
                return Err(PlatformError::NotFound);
            }
            let now_visible = visible_bounds(hwnd)?;
            if near((now_visible[0], now_visible[1]), target.visible) {
                return Ok(());
            }
            if recomputes < PLACE_RECOMPUTES {
                let now_outer = outer_bounds(hwnd)?;
                if frame_changed(basis_outer, basis_visible, now_outer, now_visible) {
                    target = placed_origin(now_outer, now_visible, origin, &monitor)?;
                    if Instant::now() >= deadline {
                        return Err(PlatformError::Timeout);
                    }
                    if self.resolver.resolve(window) != Some(native) {
                        return Err(PlatformError::NotFound);
                    }
                    post_move(hwnd, target.outer)?;
                    (basis_outer, basis_visible) = (now_outer, now_visible);
                    recomputes += 1;
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(PlatformError::Timeout);
            }
            thread::sleep(PLACE_POLL.min(deadline.saturating_duration_since(now)));
        }
    }
}

/// PMv2 for one native call; the thread's previous context is restored on every exit.
struct DpiScope(DPI_AWARENESS_CONTEXT);
impl DpiScope {
    fn new() -> Result<Self, PlatformError> {
        // SAFETY: affects only this call's thread; the prior context is retained for Drop.
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        if previous.is_null() {
            return Err(PlatformError::Backend(
                "placement PMv2 context unavailable".into(),
            ));
        }
        Ok(Self(previous))
    }
}
impl Drop for DpiScope {
    fn drop(&mut self) {
        // SAFETY: restores exactly the context captured by the successful setter, on this thread.
        unsafe { SetThreadDpiAwarenessContext(self.0) };
    }
}

/// DWM extended-frame bounds (the visible rectangle) of a freshly resolved window.
fn visible_bounds(hwnd: HWND) -> Result<[i32; 4], PlatformError> {
    let mut bounds = RECT::default();
    // SAFETY: exact RECT output buffer for the same freshly resolved window.
    let status = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            (&mut bounds as *mut RECT).cast(),
            size_of::<RECT>() as u32,
        )
    };
    if status < 0 {
        return Err(PlatformError::Backend("placement geometry unknown".into()));
    }
    Ok(corners(bounds))
}

/// GetWindowRect (outer) of a freshly resolved window, read by the placement read-back.
fn outer_bounds(hwnd: HWND) -> Result<[i32; 4], PlatformError> {
    let mut window_rect = RECT::default();
    // SAFETY: exact RECT output buffer for the same freshly resolved window, under PMv2.
    if unsafe { GetWindowRect(hwnd, &mut window_rect) } == 0 {
        return Err(PlatformError::Backend("placement geometry unknown".into()));
    }
    Ok(corners(window_rect))
}

/// The one asynchronous, no-size move of the outer rectangle that placement issues. Callers check
/// the deadline and the window identity first.
fn post_move(hwnd: HWND, outer: (i32, i32)) -> Result<(), PlatformError> {
    // SAFETY: the freshly resolved, PMv2-scoped window only. The asynchronous move is posted to
    // its owning thread and changes no size, z-order, owner, activation or show state.
    let moved = unsafe {
        SetWindowPos(
            hwnd,
            null_mut(),
            outer.0,
            outer.1,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_ASYNCWINDOWPOS,
        )
    };
    if moved == 0 {
        return Err(PlatformError::Backend("placement refused".into()));
    }
    Ok(())
}

fn corners(rect: RECT) -> [i32; 4] {
    [rect.left, rect.top, rect.right, rect.bottom]
}

/// Within one device pixel on both axes: the D-6 read-back tolerance.
fn near(actual: (i32, i32), target: (i32, i32)) -> bool {
    (i64::from(actual.0) - i64::from(target.0)).abs() <= 1
        && (i64::from(actual.1) - i64::from(target.1)).abs() <= 1
}

#[cfg(test)]
mod probe {
    #[cfg(test)]
    mod limited_probe {
        use crate::model::{
            geometry::{DisplayIds, MonitorProbe},
            window::Identity,
        };
        use crosspane_platform::{CaptureEvent, CapturePortal, Edge, PortalId, WindowSource};
        use crosspane_types::{
            geom::{PixelRect, euclid::point2},
            time::MonoTime,
        };
        use std::{
            mem::size_of,
            ptr::{null, null_mut},
            sync::{
                Arc, Mutex,
                atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
                mpsc,
            },
            thread,
            time::{Duration, Instant},
        };
        use windows_sys::Win32::{
            Foundation::*,
            Graphics::Gdi::*,
            Security::*,
            System::{LibraryLoader::GetModuleHandleW, Threading::*},
            UI::{Accessibility::*, HiDpi::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
        };

        // Used only by this exact ignored test executable; no product authority or physical seed.
        static OWN: AtomicUsize = AtomicUsize::new(0);
        static OWN_THREAD: AtomicU32 = AtomicU32::new(0);
        static ESC_OWED: AtomicBool = AtomicBool::new(false);
        static PAIR: AtomicUsize = AtomicUsize::new(0);
        static REFUSED: AtomicBool = AtomicBool::new(false);

        fn limited() {
            let mut token = null_mut();
            let mut elevation = TOKEN_ELEVATION::default();
            let mut kind = 0_i32;
            let mut bytes = 0;
            // SAFETY: read only this process's token, with initialized exact buffers.
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
                ) != 0
                    && GetTokenInformation(
                        token,
                        TokenElevationType,
                        (&mut kind as *mut i32).cast(),
                        size_of::<i32>() as u32,
                        &mut bytes,
                    ) != 0;
                CloseHandle(token);
                assert!(okay);
            }
            assert_eq!(
                elevation.TokenIsElevated, 0,
                "elevated tooling must not run this probe"
            );
            assert_eq!(kind, TokenElevationTypeLimited);
        }
        fn owned_foreground(window: HWND) -> bool {
            let mut pid = 0;
            // SAFETY: handle equality first; fields are read ONLY from our created allowlisted HWND.
            unsafe {
                !window.is_null()
                    && window as usize == OWN.load(Ordering::Acquire)
                    && GetForegroundWindow() == window
                    && IsWindow(window) != 0
                    && GetWindowThreadProcessId(window, &mut pid)
                        == OWN_THREAD.load(Ordering::Acquire)
                    && pid == GetCurrentProcessId()
            }
        }
        fn esc(up: bool) -> INPUT {
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: 0,
                        wScan: 0x01,
                        dwFlags: KEYEVENTF_SCANCODE | if up { KEYEVENTF_KEYUP } else { 0 },
                        time: 0,
                        dwExtraInfo: 0x43504445,
                    },
                },
            }
        }
        fn release_esc(window: HWND) {
            if ESC_OWED.load(Ordering::Acquire) && owned_foreground(window) {
                let input = esc(true);
                // SAFETY: only our previously submitted ESC down is owed, and fresh own foreground
                // was verified. No generic cleanup keys or foreign target are permitted.
                if unsafe { SendInput(1, &input, size_of::<INPUT>() as i32) } == 1 {
                    ESC_OWED.store(false, Ordering::Release);
                }
            }
        }
        unsafe extern "system" fn timer(window: HWND, _: u32, id: usize, _: u32) {
            // SAFETY: timer belongs only to our authenticated fixture.
            unsafe {
                KillTimer(window, id);
            }
            if !owned_foreground(window) || !all_up() {
                REFUSED.store(true, Ordering::Release);
                return;
            }
            let pair = [esc(false), esc(true)];
            ESC_OWED.store(true, Ordering::Release);
            // SAFETY: paired ESC scan codes only, after fresh own foreground/identity admission.
            let count = unsafe { SendInput(2, pair.as_ptr(), size_of::<INPUT>() as i32) };
            PAIR.store(count as usize, Ordering::Release);
            if count == 0 || count == 2 {
                ESC_OWED.store(false, Ordering::Release);
            }
            release_esc(window);
        }
        struct Guard {
            window: HWND,
            class: Vec<u16>,
            instance: HINSTANCE,
            old_dpi: DPI_AWARENESS_CONTEXT,
        }
        impl Drop for Guard {
            fn drop(&mut self) {
                release_esc(self.window);
                // SAFETY: cleanup only this fixture window/class and restore our own thread DPI.
                unsafe {
                    KillTimer(self.window, 1);
                    DestroyWindow(self.window);
                    UnregisterClassW(self.class.as_ptr(), self.instance);
                    SetThreadDpiAwarenessContext(self.old_dpi);
                }
                OWN.store(0, Ordering::Release);
            }
        }
        fn pump() {
            // SAFETY: bounded dispatch on this fixture's own message queue.
            unsafe {
                let mut message = MSG::default();
                for _ in 0..256 {
                    if PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) == 0 {
                        break;
                    }
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        }
        fn pumped<T>(receive: &mpsc::Receiver<T>) -> T {
            let until = Instant::now() + Duration::from_secs(2);
            loop {
                if let Ok(result) = receive.try_recv() {
                    return result;
                }
                assert!(Instant::now() < until, "owned observer operation timed out");
                pump();
                thread::sleep(Duration::from_millis(2));
            }
        }
        fn all_up() -> bool {
            // SAFETY: read-only aggregate key/button state, never typed contents. Zero is not
            // independent positive hook proof; this conservative status check can only refuse.
            unsafe {
                [1, 2, 4, 5, 6, 0x1b, 0x10, 0x11, 0x12, 0x5b, 0x5c]
                    .into_iter()
                    .all(|vk| GetAsyncKeyState(vk) >= 0)
            }
        }

        #[test]
        #[ignore = "Limited own-fixture only; win-gui with explicit opt-in"]
        fn limited_owned_move_size_no_physical_primary() {
            assert_eq!(
                std::env::var("CROSSPANE_WINDOWS_DRAG_PROBE").as_deref(),
                Ok("1")
            );
            limited();
            let (done, receive) = mpsc::channel();
            let watchdog = thread::spawn(move || {
                if receive.recv_timeout(Duration::from_secs(10)).is_err() {
                    release_esc(OWN.load(Ordering::Acquire) as HWND);
                    // Process exit destroys only our windows/hooks; no foreign process is signalled.
                    eprintln!("OWNED DRAG PROBE WATCHDOG FAILED");
                    std::process::exit(124);
                }
            });
            assert!(all_up(), "held input refuses owned probe");
            // SAFETY: change only this fixture thread's DPI context.
            let old_dpi =
                unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
            assert!(!old_dpi.is_null());
            let class: Vec<_> = format!("Crosspane.DragFixture.{}", std::process::id())
                .encode_utf16()
                .chain(Some(0))
                .collect();
            // SAFETY: our own module, blank class and owned window only. Creation admits the HWND
            // BEFORE any field read. No force-focus/foreground workaround is used.
            let fixture = unsafe {
                let instance = GetModuleHandleW(null());
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(DefWindowProcW),
                    hInstance: instance,
                    lpszClassName: class.as_ptr(),
                    hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                    ..Default::default()
                };
                assert_ne!(RegisterClassW(&wc), 0);
                let window = CreateWindowExW(
                    0,
                    class.as_ptr(),
                    class.as_ptr(),
                    WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                    100,
                    100,
                    360,
                    220,
                    null_mut(),
                    null_mut(),
                    instance,
                    null(),
                );
                assert!(!window.is_null());
                OWN.store(window as usize, Ordering::Release);
                OWN_THREAD.store(GetCurrentThreadId(), Ordering::Release);
                Guard {
                    window,
                    class,
                    instance,
                    old_dpi,
                }
            };
            let until = Instant::now() + Duration::from_secs(1);
            while !owned_foreground(fixture.window) && Instant::now() < until {
                pump();
                thread::sleep(Duration::from_millis(5));
            }
            assert!(
                owned_foreground(fixture.window),
                "own foreground unavailable; no input submitted"
            );
            let mut creation = FILETIME::default();
            let mut other = FILETIME::default();
            let mut kernel = FILETIME::default();
            let mut user = FILETIME::default();
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            // SAFETY: read only our own process/window/monitor metadata.
            let identity = unsafe {
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
                assert_ne!(
                    GetMonitorInfoW(
                        MonitorFromWindow(fixture.window, MONITOR_DEFAULTTONEAREST),
                        &mut info.monitorInfo
                    ),
                    0
                );
                Identity {
                    hwnd: fixture.window as u64,
                    pid: GetCurrentProcessId(),
                    tid: GetCurrentThreadId(),
                    process_created: (u64::from(creation.dwHighDateTime) << 32)
                        | u64::from(creation.dwLowDateTime),
                }
            };
            let coords = |r: RECT| [r.left, r.top, r.right, r.bottom];
            let probe = MonitorProbe {
                device_path: "owned-drag-fixture-monitor".into(),
                name: String::from_utf16_lossy(
                    &info.szDevice[..info
                        .szDevice
                        .iter()
                        .position(|c| *c == 0)
                        .unwrap_or(info.szDevice.len())],
                ),
                rc_monitor: coords(info.monitorInfo.rcMonitor),
                rc_work: coords(info.monitorInfo.rcWork),
                primary: true,
                // SAFETY: DPI of the admitted owned fixture only.
                dpi: unsafe { GetDpiForWindow(fixture.window) },
                refresh_millihz: 60000,
                edid: None,
                twin: false,
                quarter_turns: 0,
            };
            let (send, receive) = mpsc::channel();
            let acquisition = thread::spawn(move || {
                let source = crate::window::WindowsWindowSource::for_fixture(
                    Arc::new(Mutex::new(DisplayIds::default())),
                    Arc::new(move || Ok(vec![probe.clone()])),
                    identity,
                );
                let _ = send.send(source);
            });
            let source = pumped(&receive).unwrap();
            acquisition.join().unwrap();
            assert_eq!(source.windows().unwrap().len(), 1);
            let id = source.windows().unwrap()[0].id;
            assert!(source.resolver().resolve(id).is_some());
            // SAFETY: callback delivery is restricted to OUR PID before the callback sees HWNDs.
            let hook = unsafe {
                SetWinEventHook(
                    super::super::EVENT_SYSTEM_MOVESIZESTART,
                    super::super::EVENT_SYSTEM_MOVESIZEEND,
                    null_mut(),
                    Some(super::super::lifecycle),
                    identity.pid,
                    0,
                    WINEVENT_OUTOFCONTEXT,
                )
            };
            assert!(!hook.is_null());
            struct Hook(HWINEVENTHOOK);
            impl Drop for Hook {
                fn drop(&mut self) {
                    // SAFETY: remove only our PID-filtered hook.
                    unsafe {
                        UnhookWinEvent(self.0);
                    }
                }
            }
            let hook = Hook(hook);
            for command in [SC_MOVE, SC_SIZE] {
                assert!(owned_foreground(fixture.window));
                assert!(all_up());
                assert!(source.resolver().resolve(id).is_some());
                super::super::EVENTS.with(|events| *events.borrow_mut() = Default::default());
                PAIR.store(0, Ordering::Release);
                REFUSED.store(false, Ordering::Release);
                // SAFETY: exact own-window timer/control. The timer independently re-admits
                // foreground before paired ESC. No pointer move/down or fake physical fact.
                unsafe {
                    assert_ne!(SetTimer(fixture.window, 1, 180, Some(timer)), 0);
                    SendMessageW(fixture.window, WM_SYSCOMMAND, command as usize, 0);
                }
                let until = Instant::now() + Duration::from_millis(400);
                while Instant::now() < until {
                    pump();
                    thread::sleep(Duration::from_millis(5));
                }
                assert!(
                    !REFUSED.load(Ordering::Acquire),
                    "lost own foreground; trial refused"
                );
                assert_eq!(PAIR.load(Ordering::Acquire), 2);
                assert!(!ESC_OWED.load(Ordering::Acquire));
                assert!(all_up());
                let rows = super::super::EVENTS
                    .with(|events| std::mem::take(&mut events.borrow_mut().rows));
                println!(
                    "OWNED diagnostic command={} lifecycle-rows={} paired-ESC={}",
                    command,
                    rows.len(),
                    PAIR.load(Ordering::Acquire)
                );
                assert_eq!(
                    rows.iter()
                        .filter(
                            |(event, hwnd)| *event == super::super::EVENT_SYSTEM_MOVESIZESTART
                                && *hwnd == identity.hwnd
                        )
                        .count(),
                    1
                );
                assert_eq!(
                    rows.iter()
                        .filter(
                            |(event, hwnd)| *event == super::super::EVENT_SYSTEM_MOVESIZEEND
                                && *hwnd == identity.hwnd
                        )
                        .count(),
                    1
                );
                let mut rect = RECT::default();
                let mut cursor = POINT::default();
                // SAFETY: client coordinates only from the same admitted fixture identity.
                let points = unsafe {
                    assert_ne!(GetClientRect(fixture.window, &mut rect), 0);
                    let mut points = [
                        POINT {
                            x: rect.left,
                            y: rect.top,
                        },
                        POINT {
                            x: rect.right,
                            y: rect.bottom,
                        },
                    ];
                    assert_ne!(ClientToScreen(fixture.window, &mut points[0]), 0);
                    assert_ne!(ClientToScreen(fixture.window, &mut points[1]), 0);
                    assert_ne!(GetCursorPos(&mut cursor), 0);
                    points
                };
                assert!(source.resolver().resolve(id).is_some());
                let frame = super::super::visible_bounds(fixture.window).unwrap();
                let fact = crate::model::drag::WindowFact {
                    window: id,
                    identity,
                    content: PixelRect::new(
                        point2(points[0].x, points[0].y),
                        point2(points[1].x, points[1].y),
                    ),
                    frame: PixelRect::new(point2(frame[0], frame[1]), point2(frame[2], frame[3])),
                };
                let mut detector = crate::model::drag::Detector::default();
                let portal = PortalId(1);
                detector
                    .set_portals(
                        &[(
                            CapturePortal {
                                id: portal,
                                display: source.windows().unwrap()[0].display.unwrap(),
                                edge: Edge::Right,
                                from: 0.,
                                to: f64::from(rect.bottom - rect.top),
                            },
                            PixelRect::new(
                                point2(points[1].x - 1, points[0].y),
                                point2(points[1].x, points[1].y),
                            ),
                        )],
                        MonoTime::from_nanos(0),
                    )
                    .unwrap();
                let events =
                    detector.start(fact, (cursor.x, cursor.y), false, MonoTime::from_nanos(0));
                assert!(
                    events
                        .iter()
                        .all(|event| !matches!(event, CaptureEvent::DragAtEdge { .. }))
                );
                assert!(
                    detector
                        .sample(
                            Some(fact),
                            (cursor.x, cursor.y),
                            None,
                            false,
                            MonoTime::from_nanos(1)
                        )
                        .is_empty()
                );
                assert!(detector.at_edge(portal).is_none());
                println!(
                    "OWNED MOVESIZE command={} START=1 END=1 ESC=paired no-physical-primary=no-drag held-status=up",
                    command & 0xfff0
                );
            }
            drop(hook);
            let (send, receive) = mpsc::channel();
            let cleanup = thread::spawn(move || {
                let _ = send.send(source.stop_verified());
            });
            assert!(pumped(&receive));
            cleanup.join().unwrap();
            let hwnd = fixture.window;
            drop(fixture);
            // SAFETY: verify only our destroyed handle; never enumerate foreign windows.
            assert_eq!(unsafe { IsWindow(hwnd) }, 0);
            done.send(()).unwrap();
            watchdog.join().unwrap();
            println!(
                "OWNED cleanup window=destroyed PID-filtered-source=stopped hook=removed physical-positive=UNRUN-W6.1"
            );
        }
    }
}
