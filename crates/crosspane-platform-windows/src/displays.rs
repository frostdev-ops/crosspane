//! One native monitor owner for Displays, geometry consumers and HMONITOR resolution.
//! Each request reacquires facts; the allocator is held only after native calls finish.
//! DPI comes from never-shown PMv2 helper windows, NOT GetDpiForMonitor: Microsoft's
//! documentation forbids that API on a per-monitor-aware caller.
//! <https://learn.microsoft.com/windows/win32/api/shellscalingapi/nf-shellscalingapi-getdpiformonitor>
//! <https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-getdpiforwindow>
//! Active desktop targets, including virtual targets, are retained by actual CCD path.
//! Mirrored/ambiguous identities are refused rather than fabricated. No output is created.
#![allow(unsafe_code)]

pub use crate::model::{displays::MonitorSnapshot, frame_capture::MonitorSnapshotReader};
use crate::{
    inject::MonitorRefresh,
    model::{
        displays::{Delivery, NativeMonitor, TargetPath, join_paths, read_snapshot},
        geometry::{DisplayIds, MonitorProbe},
    },
    window::MonitorReader,
};
use crosspane_platform::{Displays, EventSink, PlatformError};
use crosspane_types::{display::DisplayInfo, id::DisplayId};
use std::{
    mem::size_of,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr::{null, null_mut},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Devices::Display::*,
    Foundation::*,
    Graphics::Gdi::*,
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentProcessId},
    UI::{HiDpi::*, WindowsAndMessaging::*},
};

const BOUND: Duration = crate::model::displays::READ_BOUND;
const RECONCILE: Duration = Duration::from_millis(500);
const LIMIT: usize = 128;
static CLASSES: AtomicU64 = AtomicU64::new(0);

fn backend(message: &'static str) -> PlatformError {
    PlatformError::Backend(message.into())
}
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
fn text(chars: &[u16]) -> Result<String, PlatformError> {
    let length = chars
        .iter()
        .position(|c| *c == 0)
        .ok_or_else(|| backend("unterminated monitor identity"))?;
    String::from_utf16(&chars[..length]).map_err(|_| backend("invalid monitor identity encoding"))
}
fn rect(r: RECT) -> [i32; 4] {
    [r.left, r.top, r.right, r.bottom]
}

struct Subscriber {
    sink: Arc<dyn EventSink<Vec<DisplayInfo>>>,
    delivery: Delivery,
}
#[derive(Default)]
struct DispatchState {
    subscribers: Vec<Subscriber>,
    stopping: bool,
}
#[derive(Default)]
struct Dispatch {
    state: Mutex<DispatchState>,
    wake: Condvar,
}
impl Dispatch {
    fn subscribe(
        &self,
        sink: Arc<dyn EventSink<Vec<DisplayInfo>>>,
        current: Vec<DisplayInfo>,
    ) -> Result<(), PlatformError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| backend("monitor delivery poisoned"))?;
        if state.stopping || state.subscribers.len() >= 32 {
            return Err(backend("monitor subscription unavailable"));
        }
        state.subscribers.push(Subscriber {
            sink,
            delivery: Delivery::new(current),
        });
        self.wake.notify_one();
        Ok(())
    }
    fn publish(&self, current: Vec<DisplayInfo>) {
        if let Ok(mut state) = self.state.lock() {
            for subscriber in &mut state.subscribers {
                subscriber.delivery.publish(current.clone());
            }
            self.wake.notify_one();
        }
    }
    fn stop(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.stopping = true;
            for subscriber in &mut state.subscribers {
                subscriber.delivery.publish(Vec::new());
            }
            self.wake.notify_one();
        }
    }
    fn run(&self) {
        loop {
            let pending = {
                let Ok(mut state) = self.state.lock() else {
                    return;
                };
                loop {
                    let next = state.subscribers.iter_mut().find_map(|subscriber| {
                        subscriber
                            .delivery
                            .take()
                            .map(|snapshot| (Arc::clone(&subscriber.sink), snapshot))
                    });
                    if next.is_some() || state.stopping {
                        if next.is_none() {
                            state.subscribers.clear();
                        }
                        break next;
                    }
                    let Ok(next) = self.wake.wait(state) else {
                        return;
                    };
                    state = next;
                }
            };
            let Some((sink, snapshot)) = pending else {
                return;
            };
            // No owner/allocator lock is held across a potentially reentrant sink.
            if catch_unwind(AssertUnwindSafe(|| sink.send(snapshot))).is_err() {
                eprintln!("Windows monitor subscriber panicked");
            }
        }
    }
}

struct Request {
    until: Instant,
    abandoned: Arc<AtomicBool>,
    subscribe: Option<Arc<dyn EventSink<Vec<DisplayInfo>>>>,
    reply: mpsc::SyncSender<Result<MonitorSnapshot, PlatformError>>,
}
struct Owner {
    ids: Arc<Mutex<DisplayIds>>,
    requests: mpsc::SyncSender<Request>,
    alive: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    fault: Arc<AtomicBool>,
    dispatch: Arc<Dispatch>,
    native: Option<JoinHandle<()>>,
    delivery: Option<JoinHandle<()>>,
}
impl Owner {
    fn request(
        &self,
        subscribe: Option<Arc<dyn EventSink<Vec<DisplayInfo>>>>,
    ) -> Result<MonitorSnapshot, PlatformError> {
        if !self.alive.load(Ordering::Acquire) || self.stop.load(Ordering::Acquire) {
            return Err(backend("monitor observer unavailable"));
        }
        let until = Instant::now() + BOUND;
        let abandoned = Arc::new(AtomicBool::new(false));
        let (reply, result) = mpsc::sync_channel(1);
        self.requests
            .try_send(Request {
                until,
                abandoned: Arc::clone(&abandoned),
                subscribe,
                reply,
            })
            .map_err(|_| backend("monitor observer busy"))?;
        match result.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok(result) => {
                if !self.alive.load(Ordering::Acquire) || self.stop.load(Ordering::Acquire) {
                    Err(backend("monitor observer unavailable"))
                } else {
                    result
                }
            }
            Err(_) => {
                abandoned.store(true, Ordering::Release);
                Err(PlatformError::Timeout)
            }
        }
    }
    fn stop_owned(&mut self) -> bool {
        self.stop.store(true, Ordering::Release);
        self.dispatch.stop();
        let until = Instant::now() + BOUND;
        while Instant::now() < until
            && (self
                .native
                .as_ref()
                .is_some_and(|thread| !thread.is_finished())
                || self
                    .delivery
                    .as_ref()
                    .is_some_and(|thread| !thread.is_finished()))
        {
            thread::sleep(Duration::from_millis(5));
        }
        let mut clean = true;
        for handle in [&mut self.native, &mut self.delivery] {
            if handle.as_ref().is_some_and(|thread| thread.is_finished()) {
                if let Some(thread) = handle.take() {
                    clean &= thread.join().is_ok();
                }
            } else if handle.is_some() {
                // An uninterruptible native call/callback retains its own resources until exit.
                clean = false;
            }
        }
        clean && !self.fault.load(Ordering::Acquire)
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        if !self.stop_owned() {
            eprintln!("Windows monitor cleanup not verified; owned workers retained");
        }
    }
}

/// Share this owner's reader/refresh/allocator with every geometry consumer.
pub struct WindowsDisplays {
    owner: Arc<Owner>,
}
impl std::fmt::Debug for WindowsDisplays {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsDisplays")
            .field("alive", &self.owner.alive.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}
impl WindowsDisplays {
    pub fn new() -> Result<Self, PlatformError> {
        let ids = Arc::new(Mutex::new(DisplayIds::default()));
        let alive = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let fault = Arc::new(AtomicBool::new(false));
        let dispatch = Arc::new(Dispatch::default());
        let (requests, incoming) = mpsc::sync_channel(8);
        let (ready, startup) = mpsc::sync_channel(1);
        let delivered = Arc::clone(&dispatch);
        let delivery = thread::Builder::new()
            .name("crosspane-monitors-delivery".into())
            .spawn(move || delivered.run())
            .map_err(|_| backend("spawn monitor delivery failed"))?;
        let worker_ids = Arc::clone(&ids);
        let worker_alive = Arc::clone(&alive);
        let worker_stop = Arc::clone(&stop);
        let worker_fault = Arc::clone(&fault);
        let observed = Arc::clone(&dispatch);
        let native = match thread::Builder::new()
            .name("crosspane-monitors".into())
            .spawn(move || {
                let _exit = Exit {
                    alive: worker_alive,
                    dispatch: Arc::clone(&observed),
                };
                match Native::new(worker_fault) {
                    Ok(mut native) => {
                        if ready.send(Ok(())).is_ok() {
                            native.run(incoming, &worker_ids, &worker_stop, &observed);
                        }
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                    }
                }
            }) {
            Ok(thread) => thread,
            Err(_) => {
                dispatch.stop();
                let _ = delivery.join();
                return Err(backend("spawn monitor observer failed"));
            }
        };
        let backend = Self {
            owner: Arc::new(Owner {
                ids,
                requests,
                alive,
                stop,
                fault,
                dispatch,
                native: Some(native),
                delivery: Some(delivery),
            }),
        };
        startup
            .recv_timeout(BOUND)
            .map_err(|_| PlatformError::Timeout)??;
        backend.snapshot()?;
        Ok(backend)
    }
    /// A fresh coherent native observation, never a last-good cache.
    pub fn snapshot(&self) -> Result<MonitorSnapshot, PlatformError> {
        self.owner.request(None)
    }
    pub fn ids(&self) -> Arc<Mutex<DisplayIds>> {
        Arc::clone(&self.owner.ids)
    }
    #[cfg_attr(test, allow(dead_code))] // Existing private include fixtures use only probe readers.
    pub fn monitor_snapshot_reader(&self) -> MonitorSnapshotReader {
        let owner = Arc::downgrade(&self.owner);
        Arc::new(move || {
            owner
                .upgrade()
                .ok_or_else(|| backend("monitor owner dropped"))?
                .request(None)
        })
    }
    pub fn monitor_reader(&self) -> MonitorReader {
        // Weak helpers cannot form owner -> sink -> helper -> owner cycles. The platform
        // must retain WindowsDisplays; after it drops, fresh observations refuse.
        let owner = Arc::downgrade(&self.owner);
        Arc::new(move || {
            owner
                .upgrade()
                .ok_or_else(|| backend("monitor owner dropped"))?
                .request(None)
                .map(|snapshot| snapshot.probes)
        })
    }
    pub fn monitor_refresh(&self) -> MonitorRefresh {
        let owner = Arc::downgrade(&self.owner);
        Arc::new(move || {
            owner
                .upgrade()
                .ok_or_else(|| backend("monitor owner dropped"))?
                .request(None)
                .map(|snapshot| (snapshot.probes, snapshot.ids))
        })
    }
    /// The returned handle is observation-scoped, not an owned monitor resource.
    pub fn monitor(&self, id: DisplayId) -> Result<usize, PlatformError> {
        self.snapshot()?
            .monitors
            .get(&id)
            .copied()
            .ok_or(PlatformError::NotFound)
    }
    #[cfg(test)]
    #[allow(dead_code)] // Also compiled separately by the ignored Limited fixture harness.
    pub(crate) fn stop_verified(self) -> bool {
        let Ok(mut owner) = Arc::try_unwrap(self.owner) else {
            return false;
        };
        owner.stop_owned()
    }
}
impl Displays for WindowsDisplays {
    fn displays(&self) -> Result<Vec<DisplayInfo>, PlatformError> {
        self.snapshot().map(|snapshot| snapshot.displays)
    }
    fn subscribe(
        &mut self,
        sink: Arc<dyn EventSink<Vec<DisplayInfo>>>,
    ) -> Result<(), PlatformError> {
        self.owner.request(Some(sink)).map(|_| ())
    }
}
struct Exit {
    alive: Arc<AtomicBool>,
    dispatch: Arc<Dispatch>,
}
impl Drop for Exit {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        self.dispatch.publish(Vec::new());
    }
}

struct Native {
    class: Vec<u16>,
    instance: HINSTANCE,
    observer: HWND,
    old_dpi: DPI_AWARENESS_CONTEXT,
    dirty: Arc<AtomicBool>,
    fault: Arc<AtomicBool>,
}
impl Native {
    fn new(fault: Arc<AtomicBool>) -> Result<Self, PlatformError> {
        // SAFETY: change only this dedicated thread, restoring its prior context on every exit.
        let old_dpi =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        if old_dpi.is_null() {
            return Err(backend("monitor PMv2 context unavailable"));
        }
        let mut native = Self {
            class: Vec::new(),
            instance: null_mut(),
            observer: null_mut(),
            old_dpi,
            dirty: Arc::new(AtomicBool::new(true)),
            fault,
        };
        // SAFETY: borrow the process module, not an owning handle.
        native.instance = unsafe { GetModuleHandleW(null()) };
        if native.instance.is_null() {
            return Err(backend("monitor module unavailable"));
        }
        // SAFETY: process identity only, used for an owned unique class name.
        let pid = unsafe { GetCurrentProcessId() };
        native.class = wide(&format!(
            "Crosspane.Monitors.{pid}.{}",
            CLASSES.fetch_add(1, Ordering::Relaxed)
        ));
        let class = WNDCLASSW {
            lpfnWndProc: Some(observer_proc),
            hInstance: native.instance,
            lpszClassName: native.class.as_ptr(),
            ..Default::default()
        };
        // SAFETY: owned class with a static callback and live UTF16 name.
        if unsafe { RegisterClassW(&class) } == 0 {
            native.class.clear();
            return Err(backend("register monitor observer failed"));
        }
        // SAFETY: own ordinary top-level tool window, no WS_VISIBLE/parent/activation.
        // The flag remains alive until after this HWND is destroyed on this thread.
        native.observer = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                native.class.as_ptr(),
                wide("").as_ptr(),
                WS_POPUP,
                0,
                0,
                1,
                1,
                null_mut(),
                null_mut(),
                native.instance,
                Arc::as_ptr(&native.dirty).cast(),
            )
        };
        if native.observer.is_null() {
            return Err(backend("create monitor observer failed"));
        }
        Ok(native)
    }
    fn pump(&self) -> bool {
        let mut message = MSG::default();
        // SAFETY: only this owned thread's queue and static registered procedure.
        unsafe {
            while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                if message.message == WM_QUIT {
                    return false;
                }
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        // SAFETY: read-only existence check of our own observer, never a foreign HWND.
        unsafe { IsWindow(self.observer) != 0 }
    }
    fn snapshot(
        &self,
        ids: &Mutex<DisplayIds>,
        until: Instant,
    ) -> Result<MonitorSnapshot, PlatformError> {
        read_snapshot(
            &mut || {
                if Instant::now() >= until {
                    return Err(PlatformError::Timeout);
                }
                let result = self.read_native()?;
                if Instant::now() >= until {
                    return Err(PlatformError::Timeout);
                }
                Ok(result)
            },
            ids,
        )
    }
    fn run(
        &mut self,
        incoming: mpsc::Receiver<Request>,
        ids: &Mutex<DisplayIds>,
        stop: &AtomicBool,
        dispatch: &Dispatch,
    ) {
        let mut previous = None;
        let mut last = Instant::now() - RECONCILE;
        while !stop.load(Ordering::Acquire) && self.pump() {
            match incoming.recv_timeout(Duration::from_millis(20)) {
                Ok(request) => {
                    if request.abandoned.load(Ordering::Acquire) || Instant::now() >= request.until
                    {
                        continue;
                    }
                    let mut result = self.snapshot(ids, request.until);
                    if request.abandoned.load(Ordering::Acquire) {
                        continue;
                    }
                    if let Ok(snapshot) = &result {
                        publish_changed(dispatch, &mut previous, &snapshot.displays);
                        if let Some(sink) = request.subscribe
                            && let Err(error) = dispatch.subscribe(sink, snapshot.displays.clone())
                        {
                            result = Err(error);
                        }
                    } else {
                        publish_changed(dispatch, &mut previous, &[]);
                    }
                    let _ = request.reply.try_send(result);
                    last = Instant::now();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if self.dirty.swap(false, Ordering::AcqRel) || last.elapsed() >= RECONCILE {
                let snapshot = self.snapshot(ids, Instant::now() + BOUND);
                publish_changed(
                    dispatch,
                    &mut previous,
                    snapshot
                        .as_ref()
                        .map(|s| s.displays.as_slice())
                        .unwrap_or(&[]),
                );
                last = Instant::now();
            }
        }
    }
    fn read_native(&self) -> Result<Vec<NativeMonitor>, PlatformError> {
        let before = paths()?;
        let mut enumeration = Enumeration {
            monitors: Vec::with_capacity(LIMIT),
            failed: false,
        };
        // SAFETY: callback borrows this live stack context synchronously, with bounded capacity.
        if unsafe {
            EnumDisplayMonitors(
                null_mut(),
                null(),
                Some(enum_monitor),
                (&mut enumeration as *mut Enumeration) as isize,
            )
        } == 0
            || enumeration.failed
        {
            return Err(backend("enumerate monitors failed"));
        }
        let mut monitors = Vec::with_capacity(enumeration.monitors.len());
        for (handle, info) in enumeration.monitors {
            let dpi = self.dpi(handle, info.monitorInfo.rcMonitor)?;
            monitors.push(NativeMonitor {
                handle: handle as usize,
                probe: MonitorProbe {
                    device_path: String::new(),
                    name: text(&info.szDevice)?,
                    rc_monitor: rect(info.monitorInfo.rcMonitor),
                    rc_work: rect(info.monitorInfo.rcWork),
                    primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
                    dpi,
                    refresh_millihz: 0,
                    edid: None,
                    twin: false,
                    quarter_turns: 0,
                },
            });
        }
        if paths()? != before {
            return Err(PlatformError::Timeout);
        }
        join_paths(monitors, &before)
    }
    fn dpi(&self, monitor: HMONITOR, bounds: RECT) -> Result<u32, PlatformError> {
        // SAFETY: create a never-shown 1x1 helper wholly inside the observed monitor, PMv2
        // inherited from this thread. No existing window or output is moved/modified.
        let helper = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                wide("STATIC").as_ptr(),
                wide("").as_ptr(),
                WS_POPUP,
                bounds.left,
                bounds.top,
                1,
                1,
                null_mut(),
                null_mut(),
                self.instance,
                null(),
            )
        };
        if helper.is_null() {
            return Err(backend("create monitor DPI helper failed"));
        }
        // SAFETY: query only our live PMv2 helper; reject a changed monitor before using DPI.
        let observed = unsafe { MonitorFromWindow(helper, MONITOR_DEFAULTTONULL) };
        // SAFETY: documented read on our own per-monitor-aware HWND; zero means failure.
        let dpi = unsafe { GetDpiForWindow(helper) };
        // SAFETY: destroy only the helper created on this same thread above.
        if unsafe { DestroyWindow(helper) } == 0 {
            self.fault.store(true, Ordering::Release);
            return Err(backend("monitor DPI helper cleanup failed"));
        }
        if observed != monitor || dpi == 0 {
            return Err(backend("monitor DPI observation changed"));
        }
        Ok(dpi)
    }
}
impl Drop for Native {
    fn drop(&mut self) {
        // SAFETY: cleanup on the owning thread; callback's flag is still alive.
        unsafe {
            if !self.observer.is_null() && DestroyWindow(self.observer) == 0 {
                self.fault.store(true, Ordering::Release);
                // A failed destroy cannot free callback storage before the OS tears down
                // the thread's window. Retain it; cleanup is explicitly not verified.
                std::mem::forget(Arc::clone(&self.dirty));
            }
            if !self.class.is_empty() && UnregisterClassW(self.class.as_ptr(), self.instance) == 0 {
                self.fault.store(true, Ordering::Release);
            }
            if SetThreadDpiAwarenessContext(self.old_dpi).is_null() {
                self.fault.store(true, Ordering::Release);
            }
        }
    }
}
fn publish_changed(
    dispatch: &Dispatch,
    previous: &mut Option<Vec<DisplayInfo>>,
    displays: &[DisplayInfo],
) {
    if previous.as_deref() != Some(displays) {
        *previous = Some(displays.to_vec());
        dispatch.publish(displays.to_vec());
    }
}
// SAFETY: receives only messages for our registered class; userdata is its live retained flag.
unsafe extern "system" fn observer_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // SAFETY: WM_NCCREATE's CREATESTRUCTW pointer is provided synchronously by CreateWindowExW.
    unsafe {
        if message == WM_NCCREATE {
            let create = &*(l as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        if matches!(message, WM_DISPLAYCHANGE | WM_DPICHANGED | WM_SETTINGCHANGE) {
            let dirty = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const AtomicBool;
            if !dirty.is_null() {
                (*dirty).store(true, Ordering::Release);
            }
        }
        DefWindowProcW(hwnd, message, w, l)
    }
}
struct Enumeration {
    monitors: Vec<(HMONITOR, MONITORINFOEXW)>,
    failed: bool,
}
// SAFETY: EnumDisplayMonitors invokes synchronously with the initialized Enumeration context.
unsafe extern "system" fn enum_monitor(
    handle: HMONITOR,
    _: HDC,
    _: *mut RECT,
    data: LPARAM,
) -> i32 {
    // SAFETY: callback receives the exact live stack context and initialized structure output.
    unsafe {
        let enumeration = &mut *(data as *mut Enumeration);
        if enumeration.monitors.len() >= LIMIT {
            enumeration.failed = true;
            return 0;
        }
        let mut info = MONITORINFOEXW {
            monitorInfo: MONITORINFO {
                cbSize: size_of::<MONITORINFOEXW>() as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        if GetMonitorInfoW(handle, &mut info.monitorInfo) == 0 {
            enumeration.failed = true;
            return 0;
        }
        enumeration.monitors.push((handle, info));
        1
    }
}
/// The last own adapter interface `paths()` saw. A transient CM failure reuses it, so live twins
/// are not published as real displays for one snapshot.
static OWN_ADAPTER: Mutex<Option<String>> = Mutex::new(None);

/// This snapshot's own adapter interface: the live value when it is readable, else the last one.
fn own_adapter_or_last() -> Option<String> {
    let live = crate::twin::own_adapter_interface();
    let mut last = OWN_ADAPTER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match live {
        Some(live) => {
            *last = Some(live.clone());
            Some(live)
        }
        None => last.clone(),
    }
}

fn paths() -> Result<Vec<TargetPath>, PlatformError> {
    // Our own adapter's interface path is the only identity that marks a twin. Without a live or
    // remembered driver interface no adapter is queried, and every path stays visible.
    let own_adapter = own_adapter_or_last();
    'attempt: for _ in 0..3 {
        let (mut path_count, mut mode_count) = (0, 0);
        // SAFETY: initialized count outputs, active paths only, no system mutation.
        if unsafe {
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
        } != ERROR_SUCCESS
        {
            return Err(backend("query monitor buffer sizes failed"));
        }
        if path_count as usize > LIMIT || mode_count as usize > LIMIT * 3 {
            return Err(backend("monitor topology exceeds bound"));
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count.max(1) as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count.max(1) as usize];
        // SAFETY: initialized bounded arrays with their exact capacities; no topology output
        // for QDC_ONLY_ACTIVE_PATHS. CCD does not DPI-virtualize returned mode information.
        let result = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                null_mut(),
            )
        };
        if result == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        if result != ERROR_SUCCESS
            || path_count as usize > paths.len()
            || mode_count as usize > modes.len()
        {
            return Err(backend("query active monitor paths failed"));
        }
        let mut result = Vec::with_capacity(path_count as usize);
        // Each distinct target adapter LUID is queried once, keyed by (HighPart, LowPart).
        let mut adapter_marks: Vec<((i32, u32), bool)> = Vec::new();
        for path in paths.into_iter().take(path_count as usize) {
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                    size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                    adapterId: path.sourceInfo.adapterId,
                    id: path.sourceInfo.id,
                },
                ..Default::default()
            };
            let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                    size: size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                    adapterId: path.targetInfo.adapterId,
                    id: path.targetInfo.id,
                },
                ..Default::default()
            };
            // SAFETY: exact initialized source/target request structures, read-only CCD queries.
            let results = unsafe {
                (
                    DisplayConfigGetDeviceInfo(&mut source.header),
                    DisplayConfigGetDeviceInfo(&mut target.header),
                )
            };
            if results != (0, 0) {
                return Err(backend("query monitor device identity failed"));
            }
            let quarter_turns = match path.targetInfo.rotation {
                DISPLAYCONFIG_ROTATION_IDENTITY => 0,
                DISPLAYCONFIG_ROTATION_ROTATE90 => 1,
                DISPLAYCONFIG_ROTATION_ROTATE180 => 2,
                DISPLAYCONFIG_ROTATION_ROTATE270 => 3,
                _ => return Err(backend("invalid monitor rotation")),
            };
            let twin = match own_adapter.as_deref() {
                None => false,
                Some(own) => {
                    let luid = path.targetInfo.adapterId;
                    let key = (luid.HighPart, luid.LowPart);
                    match adapter_marks.iter().find(|(known, _)| *known == key) {
                        Some(&(_, marked)) => marked,
                        None => {
                            let mut adapter = DISPLAYCONFIG_ADAPTER_NAME {
                                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME,
                                    size: size_of::<DISPLAYCONFIG_ADAPTER_NAME>() as u32,
                                    adapterId: luid,
                                    id: 0,
                                },
                                ..Default::default()
                            };
                            // SAFETY: exact initialized request structure, read-only CCD query.
                            // A failure is most likely an adapter removed mid-snapshot, so the
                            // bounded attempt loop starts over. Nothing is marked from it.
                            if unsafe { DisplayConfigGetDeviceInfo(&mut adapter.header) } != 0 {
                                continue 'attempt;
                            }
                            // Only the interface path is compared. A foreign name is neither kept
                            // nor printed. A malformed path cannot equal our validated interface,
                            // so that adapter is not ours.
                            let marked = text(&adapter.adapterDevicePath).is_ok_and(|path| {
                                crate::model::twin::interface_path_eq(&path, own)
                            });
                            adapter_marks.push((key, marked));
                            marked
                        }
                    }
                }
            };
            result.push(TargetPath {
                source_name: text(&source.viewGdiDeviceName)?,
                device_path: text(&target.monitorDevicePath)?,
                display_name: text(&target.monitorFriendlyDeviceName)?,
                refresh_numerator: path.targetInfo.refreshRate.Numerator,
                refresh_denominator: path.targetInfo.refreshRate.Denominator,
                quarter_turns,
                twin,
            });
        }
        result.sort_by(|a, b| {
            a.source_name
                .cmp(&b.source_name)
                .then_with(|| a.device_path.cmp(&b.device_path))
        });
        return Ok(result);
    }
    Err(PlatformError::Timeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscriber_callbacks_hold_no_owner_or_delivery_lock() {
        let dispatch = Arc::new(Dispatch::default());
        let (sent, received) = mpsc::channel();
        let checked = Arc::clone(&dispatch);
        dispatch
            .subscribe(
                Arc::new(move |event| {
                    assert!(checked.state.try_lock().is_ok());
                    let _ = sent.send(event);
                }),
                Vec::new(),
            )
            .unwrap();
        let delivered = Arc::clone(&dispatch);
        let thread = thread::spawn(move || delivered.run());
        assert!(received.recv_timeout(BOUND).unwrap().is_empty());
        dispatch.stop();
        assert!(thread.join().is_ok());
    }

    #[test]
    fn bounded_subscribers_refuse_overflow_and_post_stop_admission() {
        let dispatch = Dispatch::default();
        for _ in 0..32 {
            dispatch.subscribe(Arc::new(|_| {}), Vec::new()).unwrap();
        }
        assert!(dispatch.subscribe(Arc::new(|_| {}), Vec::new()).is_err());
        dispatch.stop();
        assert!(dispatch.subscribe(Arc::new(|_| {}), Vec::new()).is_err());
    }

    #[test]
    fn completed_snapshot_rechecks_terminal_observer_state() {
        let (requests, incoming) = mpsc::sync_channel::<Request>(8);
        let alive = Arc::new(AtomicBool::new(true));
        let gone = Arc::clone(&alive);
        let worker = thread::spawn(move || {
            let request = incoming.recv().unwrap();
            gone.store(false, Ordering::Release);
            request
                .reply
                .send(Ok(MonitorSnapshot {
                    probes: Vec::new(),
                    ids: DisplayIds::default(),
                    displays: Vec::new(),
                    monitors: Default::default(),
                }))
                .unwrap();
        });
        let owner = Owner {
            ids: Arc::new(Mutex::new(DisplayIds::default())),
            requests,
            alive,
            stop: Arc::new(AtomicBool::new(false)),
            fault: Arc::new(AtomicBool::new(false)),
            dispatch: Arc::new(Dispatch::default()),
            native: None,
            delivery: None,
        };
        assert!(matches!(
            owner.request(None),
            Err(PlatformError::Backend(_))
        ));
        assert!(worker.join().is_ok());
    }

    #[test]
    fn reader_and_refresh_cannot_keep_a_dropped_owner_alive() {
        let (requests, incoming) = mpsc::sync_channel::<Request>(8);
        let backend = WindowsDisplays {
            owner: Arc::new(Owner {
                ids: Arc::new(Mutex::new(DisplayIds::default())),
                requests,
                alive: Arc::new(AtomicBool::new(true)),
                stop: Arc::new(AtomicBool::new(false)),
                fault: Arc::new(AtomicBool::new(false)),
                dispatch: Arc::new(Dispatch::default()),
                native: None,
                delivery: None,
            }),
        };
        let reader = backend.monitor_reader();
        let refresh = backend.monitor_refresh();
        drop(backend);
        assert!(matches!(reader(), Err(PlatformError::Backend(_))));
        assert!(matches!(refresh(), Err(PlatformError::Backend(_))));
        assert!(incoming.try_recv().is_err());
    }

    #[test]
    fn subscriber_holding_a_reader_cannot_form_an_owner_cycle() {
        let (requests, incoming) = mpsc::sync_channel::<Request>(8);
        let backend = WindowsDisplays {
            owner: Arc::new(Owner {
                ids: Arc::new(Mutex::new(DisplayIds::default())),
                requests,
                alive: Arc::new(AtomicBool::new(true)),
                stop: Arc::new(AtomicBool::new(false)),
                fault: Arc::new(AtomicBool::new(false)),
                dispatch: Arc::new(Dispatch::default()),
                native: None,
                delivery: None,
            }),
        };
        let gone = Arc::downgrade(&backend.owner);
        let reader = backend.monitor_reader();
        backend
            .owner
            .dispatch
            .subscribe(
                Arc::new(move |_| {
                    let _ = &reader;
                }),
                Vec::new(),
            )
            .unwrap();
        drop(backend);
        assert!(gone.upgrade().is_none());
        assert!(matches!(
            incoming.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }
}
