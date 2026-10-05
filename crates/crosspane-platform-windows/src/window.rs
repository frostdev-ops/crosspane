//! Public Win32 window observation. Native objects stay on one message-loop thread.
//!
//! The caller supplies its retained display allocator and fresh monitor snapshots;
//! this adapter never constructs a second display identity scheme. IDs describe
//! *observed* lifetimes: Win32 offers no atomic HWND generation. Missed same-PID,
//! same-TID reuse remains a residual; incoherent reads/activation fail closed.
//! Caller waits are at most two seconds. An already running native/COM call cannot
//! be preempted; timed-out queued activation is abandoned, and Drop never joins
//! an unresponsive native call indefinitely.
#![allow(unsafe_code)]

use std::{
    cell::RefCell,
    collections::{BTreeSet, VecDeque},
    fmt,
    mem::size_of,
    ptr::{null, null_mut},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crosspane_platform::{EventSink, PlatformError, WindowEvent, WindowInfo, WindowSource};
use crosspane_types::{id::WindowId, time::MonoTime};
use windows::Win32::{
    System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize,
    },
    UI::Shell::{IVirtualDesktopManager, VirtualDesktopManager},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::{Dwm::*, Gdi::*},
    System::{LibraryLoader::GetModuleHandleW, StationsAndDesktops::*, Threading::*},
    UI::{Accessibility::*, HiDpi::*, WindowsAndMessaging::*},
};

use crate::model::{
    geometry::{DisplayIds, MonitorProbe},
    window::{self, Identity, Observation, Windows},
    winevent::{self, RawWinEvent},
};

const BOUND: Duration = Duration::from_secs(2);
const RAW_LIMIT: usize = 4096;
/// Must return current physical monitor facts within the platform call bound.
pub type MonitorReader = Arc<dyn Fn() -> Result<Vec<MonitorProbe>, PlatformError> + Send + Sync>;

struct State {
    windows: Mutex<Windows>,
    updated: Mutex<Instant>,
    alive: AtomicBool,
    fault: AtomicBool,
    cleaned: AtomicBool,
    fields_open: AtomicBool,
}

struct Call {
    until: Instant,
    abandoned: Arc<AtomicBool>,
    reply: mpsc::SyncSender<Result<(), PlatformError>>,
    operation: Operation,
}
impl Call {
    fn check(&self, state: &State) -> Result<(), PlatformError> {
        if self.abandoned.load(Ordering::Acquire) || Instant::now() >= self.until {
            return Err(PlatformError::Timeout);
        }
        if !state.alive.load(Ordering::Acquire) || state.fault.load(Ordering::Acquire) {
            return Err(backend("window observer unavailable"));
        }
        Ok(())
    }
}
enum Operation {
    Subscribe(Arc<dyn EventSink<WindowEvent>>),
    Activate(WindowId),
    #[cfg(test)]
    FreezeFixtureFields,
}

/// A Send command facade; no HWND, COM interface, hook or desktop handle crosses threads.
pub struct WindowsWindowSource {
    state: Arc<State>,
    commands: mpsc::SyncSender<Call>,
    done: mpsc::Receiver<()>,
    thread: Option<JoinHandle<()>>,
}

impl fmt::Debug for WindowsWindowSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WindowsWindowSource")
            .field("alive", &self.state.alive.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl WindowsWindowSource {
    pub fn new(
        ids: Arc<Mutex<DisplayIds>>,
        monitors: MonitorReader,
    ) -> Result<Self, PlatformError> {
        Self::start(ids, monitors, None)
    }

    fn start(
        ids: Arc<Mutex<DisplayIds>>,
        monitors: MonitorReader,
        admission: Option<Admission>,
    ) -> Result<Self, PlatformError> {
        let state = Arc::new(State {
            windows: Mutex::new(Windows::default()),
            updated: Mutex::new(Instant::now()),
            alive: AtomicBool::new(true),
            fault: AtomicBool::new(false),
            cleaned: AtomicBool::new(false),
            fields_open: AtomicBool::new(true),
        });
        let (commands, receive) = mpsc::sync_channel(1);
        let (ready, initialized) = mpsc::sync_channel(1);
        let (finished, done) = mpsc::sync_channel(1);
        let shared = Arc::clone(&state);
        let thread = thread::Builder::new()
            .name("crosspane-windows".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(&shared, receive, ids, monitors, admission, ready)
                }));
                if result.is_err() {
                    shared.fault.store(true, Ordering::Release);
                }
                shared.alive.store(false, Ordering::Release);
                let _ = finished.send(());
            })
            .map_err(|_| backend("window observer thread"))?;
        let mut source = Self {
            state,
            commands,
            done,
            thread: Some(thread),
        };
        match initialized.recv_timeout(BOUND) {
            Ok(Ok(())) => Ok(source),
            Ok(Err(error)) => {
                source.state.alive.store(false, Ordering::Release);
                source.thread.take();
                Err(error)
            }
            Err(_) => {
                source.state.alive.store(false, Ordering::Release);
                source.thread.take();
                Err(PlatformError::Timeout)
            }
        }
    }

    fn healthy(&self) -> Result<(), PlatformError> {
        if !self.state.alive.load(Ordering::Acquire) || self.state.fault.load(Ordering::Acquire) {
            return Err(backend("window observer unavailable"));
        }
        if self
            .state
            .updated
            .lock()
            .map_err(|_| backend("window observer state"))?
            .elapsed()
            >= BOUND
        {
            return Err(PlatformError::Timeout);
        }
        Ok(())
    }

    fn call(&self, operation: Operation) -> Result<(), PlatformError> {
        self.healthy()?;
        let (reply, receive) = mpsc::sync_channel(1);
        let abandoned = Arc::new(AtomicBool::new(false));
        let until = Instant::now() + BOUND;
        self.commands
            .try_send(Call {
                until,
                abandoned: Arc::clone(&abandoned),
                reply,
                operation,
            })
            .map_err(|_| backend("window observer busy"))?;
        match receive.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok(result) => result,
            Err(_) => {
                abandoned.store(true, Ordering::Release);
                Err(PlatformError::Timeout)
            }
        }
    }

    fn stop(&mut self) {
        self.state.alive.store(false, Ordering::Release);
        if let Some(thread) = self.thread.take()
            && self.done.recv_timeout(BOUND).is_ok()
        {
            let _ = thread.join();
        }
    }

    // Probe admission is compiled only into the test crate. Authentication is
    // performed by the private fixture pipe before this constructor is called.
    #[cfg(test)]
    #[allow(dead_code)] // Used by the separately compiled, ignored fixture harness.
    pub(crate) fn for_fixture(
        ids: Arc<Mutex<DisplayIds>>,
        monitors: MonitorReader,
        identity: Identity,
    ) -> Result<Self, PlatformError> {
        Self::start(
            ids,
            monitors,
            Some(Admission {
                hwnds: BTreeSet::from([identity.hwnd]),
                pid: identity.pid,
                identity,
            }),
        )
    }

    #[cfg(test)]
    #[allow(dead_code)] // Used by the separately compiled, ignored fixture harness.
    pub(crate) fn stop_verified(mut self) -> bool {
        self.stop();
        self.state.cleaned.load(Ordering::Acquire)
    }

    #[cfg(test)]
    #[allow(dead_code)] // Fixture close starts only after this worker-side field-query fence.
    pub(crate) fn freeze_fixture_fields(&self) -> Result<(), PlatformError> {
        self.call(Operation::FreezeFixtureFields)
    }
}

impl WindowSource for WindowsWindowSource {
    fn windows(&self) -> Result<Vec<WindowInfo>, PlatformError> {
        self.healthy()?;
        Ok(self
            .state
            .windows
            .lock()
            .map_err(|_| backend("window observer state"))?
            .list())
    }
    fn focused(&self) -> Result<Option<WindowId>, PlatformError> {
        self.healthy()?;
        Ok(self
            .state
            .windows
            .lock()
            .map_err(|_| backend("window observer state"))?
            .focused())
    }
    fn activate(&mut self, window: WindowId) -> Result<(), PlatformError> {
        self.call(Operation::Activate(window))
    }
    fn subscribe(&mut self, sink: Arc<dyn EventSink<WindowEvent>>) -> Result<(), PlatformError> {
        self.call(Operation::Subscribe(sink))
    }
}
impl Drop for WindowsWindowSource {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.stop();
        }
    }
}

fn backend(context: &'static str) -> PlatformError {
    PlatformError::Backend(context.into())
}
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
fn hwnd(id: u64) -> HWND {
    id as usize as HWND
}
fn number(window: HWND) -> u64 {
    window as usize as u64
}
fn rect(r: RECT) -> [i32; 4] {
    [r.left, r.top, r.right, r.bottom]
}
fn string(text: &[u16]) -> String {
    String::from_utf16_lossy(&text[..text.iter().position(|c| *c == 0).unwrap_or(text.len())])
}

#[derive(Clone)]
struct Admission {
    hwnds: BTreeSet<u64>,
    pid: u32,
    identity: Identity,
}
impl Admission {
    fn allows(&self, window: u64) -> bool {
        self.hwnds.contains(&window)
    }
}

struct Context {
    raw: Mutex<VecDeque<RawWinEvent>>,
    state: Arc<State>,
    start: Instant,
    admission: Option<Admission>,
}
impl Context {
    fn allows(&self, window: u64) -> bool {
        self.admission.as_ref().is_none_or(|a| a.allows(window))
    }
}
thread_local! { static CONTEXT: RefCell<Option<Arc<Context>>> = const { RefCell::new(None) }; }

unsafe extern "system" fn event_callback(
    _: HWINEVENTHOOK,
    event: u32,
    window: HWND,
    object: i32,
    child: i32,
    _: u32,
    _: u32,
) {
    // Copy the Arc before any Win32 call can reenter; callbacks acquire no field data.
    CONTEXT.with(|slot| {
        let context = slot.borrow().clone();
        if let Some(c) = context {
            let id = number(window);
            if id == 0
                || object != winevent::OBJID_WINDOW
                || child != winevent::CHILDID_SELF
                || !c.allows(id)
            {
                return;
            }
            if let Ok(mut raw) = c.raw.try_lock() {
                if raw.len() < RAW_LIMIT {
                    raw.push_back(RawWinEvent {
                        event,
                        hwnd: id,
                        id_object: object,
                        id_child: child,
                        at: MonoTime::from_nanos(
                            c.start.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
                        ),
                    });
                } else {
                    c.state.fault.store(true, Ordering::Release);
                }
            } else {
                c.state.fault.store(true, Ordering::Release);
            }
        }
    });
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: this is the registered procedure; parameters come from User32.
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

struct Native {
    window: HWND,
    class: Vec<u16>,
    hooks: Vec<HWINEVENTHOOK>,
    desktop: Option<IVirtualDesktopManager>,
    state: Arc<State>,
    dpi: DPI_AWARENESS_CONTEXT,
}
impl Native {
    fn new(context: &Arc<Context>) -> Result<Self, PlatformError> {
        // SAFETY: PMv2 affects only our owned observer thread's coordinate queries.
        let dpi =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        if dpi.is_null() {
            return Err(backend("window DPI context"));
        }
        // SAFETY: initializes COM only on this dedicated native thread.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .map_err(|_| backend("window COM initialization"))?;
        // SAFETY: documented in-process class; interface is kept on this thread.
        let desktop =
            match unsafe { CoCreateInstance(&VirtualDesktopManager, None, CLSCTX_INPROC_SERVER) } {
                Ok(desktop) => desktop,
                Err(_) => {
                    // SAFETY: balances the successful initialization above.
                    unsafe { CoUninitialize() };
                    return Err(backend("virtual desktop manager"));
                }
            };
        // SAFETY: null asks for the current executable module.
        let module = unsafe { GetModuleHandleW(null()) };
        // SAFETY: retrieves identifiers of this thread/process only.
        let (pid, tid) = unsafe { (GetCurrentProcessId(), GetCurrentThreadId()) };
        let class = wide(&format!("Crosspane.WindowObserver.{pid}.{tid}"));
        let wc = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: module,
            lpszClassName: class.as_ptr(),
            ..Default::default()
        };
        // SAFETY: all class pointers live through unregister; procedure has the correct ABI.
        if unsafe { RegisterClassW(&wc) } == 0 {
            drop(desktop);
            // SAFETY: balances successful COM initialization.
            unsafe { CoUninitialize() };
            return Err(backend("window observer class"));
        }
        // SAFETY: owned ordinary top-level tool window, never shown, no parent.
        let window = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW,
                class.as_ptr(),
                class.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                module,
                null(),
            )
        };
        if window.is_null() {
            drop(desktop);
            // SAFETY: no live class window exists; balances this thread's resources.
            unsafe {
                UnregisterClassW(class.as_ptr(), module);
                CoUninitialize();
            }
            return Err(backend("window observer creation"));
        }
        let mut native = Self {
            window,
            class,
            hooks: Vec::new(),
            desktop: Some(desktop),
            state: Arc::clone(&context.state),
            dpi,
        };
        let pid = context.admission.as_ref().map_or(0, |a| a.pid);
        for (first, last) in [
            (
                winevent::EVENT_SYSTEM_FOREGROUND,
                winevent::EVENT_SYSTEM_FOREGROUND,
            ),
            (
                winevent::EVENT_SYSTEM_MINIMIZESTART,
                winevent::EVENT_SYSTEM_MINIMIZEEND,
            ),
            (winevent::EVENT_OBJECT_CREATE, winevent::EVENT_OBJECT_HIDE),
            (
                winevent::EVENT_OBJECT_LOCATIONCHANGE,
                winevent::EVENT_OBJECT_NAMECHANGE,
            ),
            (
                winevent::EVENT_OBJECT_CLOAKED,
                winevent::EVENT_OBJECT_UNCLOAKED,
            ),
        ] {
            // SAFETY: callback has static lifetime and this thread pumps its queue; probe hooks are PID-filtered.
            let hook = unsafe {
                SetWinEventHook(
                    first,
                    last,
                    null_mut(),
                    Some(event_callback),
                    pid,
                    0,
                    winevent::WINEVENT_OUTOFCONTEXT,
                )
            };
            if hook.is_null() {
                return Err(backend("window event hook"));
            }
            native.hooks.push(hook);
        }
        Ok(native)
    }
}
impl Drop for Native {
    fn drop(&mut self) {
        let mut cleaned = true;
        for hook in self.hooks.drain(..) {
            // SAFETY: each hook was installed and is removed on its owner thread.
            cleaned &= unsafe { UnhookWinEvent(hook) } != 0;
        }
        // SAFETY: only this observer's window/class and COM initialization are released.
        unsafe {
            cleaned &= DestroyWindow(self.window) != 0;
            cleaned &= UnregisterClassW(self.class.as_ptr(), GetModuleHandleW(null())) != 0;
        }
        self.desktop.take();
        // SAFETY: all COM interfaces were released on this initialized thread.
        unsafe { CoUninitialize() };
        // SAFETY: restores this thread's prior DPI context after native queries cease.
        unsafe { SetThreadDpiAwarenessContext(self.dpi) };
        self.state.cleaned.store(cleaned, Ordering::Release);
    }
}

fn identity(window: HWND, expected: Option<Identity>) -> Result<(Identity, String), PlatformError> {
    let mut pid = 0;
    // SAFETY: reads metadata only; pid points to a writable scalar.
    let tid = unsafe { GetWindowThreadProcessId(window, &mut pid) };
    if tid == 0 || pid == 0 {
        return Err(PlatformError::NotFound);
    }
    if expected.is_some_and(|e| e.pid != pid || e.tid != tid || e.hwnd != number(window)) {
        return Err(PlatformError::NotFound);
    }
    // SAFETY: read-only limited process query, no process-memory access.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(backend("window process identity"));
    }
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let mut name = vec![0_u16; 32768];
    let mut length = name.len() as u32;
    // SAFETY: valid owned query handle and initialized, adequately sized output buffers.
    let timed =
        unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) } != 0;
    let actual = Identity {
        hwnd: number(window),
        pid,
        tid,
        process_created: (u64::from(created.dwHighDateTime) << 32)
            | u64::from(created.dwLowDateTime),
    };
    let coherent = expected.is_none_or(|e| e == actual);
    // SAFETY: the owned query handle is identity-checked before requesting its executable path.
    let okay = timed
        && coherent
        && unsafe { QueryFullProcessImageNameW(process, 0, name.as_mut_ptr(), &mut length) } != 0;
    // SAFETY: closes the sole owned process handle, including failure paths.
    unsafe { CloseHandle(process) };
    if !okay {
        if !coherent {
            return Err(PlatformError::NotFound);
        }
        return Err(backend("window process metadata"));
    }
    let app = String::from_utf16_lossy(&name[..length as usize])
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or("")
        .to_owned();
    Ok((actual, app))
}

fn snapshot(
    window: HWND,
    native: &Native,
    monitors: &[MonitorProbe],
    ids: &Mutex<DisplayIds>,
    expected: Option<Identity>,
) -> Result<Observation, PlatformError> {
    let (before, app_id) = identity(window, expected)?;
    let mut bounds = RECT::default();
    let mut cloaked = 0_u32;
    // SAFETY: both DWM attributes use initialized fixed-size output buffers.
    let okay = unsafe {
        DwmGetWindowAttribute(
            window,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            (&mut bounds as *mut RECT).cast(),
            size_of::<RECT>() as u32,
        ) >= 0
            && DwmGetWindowAttribute(
                window,
                DWMWA_CLOAKED as u32,
                (&mut cloaked as *mut u32).cast(),
                size_of::<u32>() as u32,
            ) >= 0
    };
    if !okay {
        return Err(backend("window geometry"));
    }
    let mut monitor = MONITORINFOEXW::default();
    monitor.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: public monitor metadata read into its exact extended structure.
    if unsafe {
        GetMonitorInfoW(
            MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST),
            &mut monitor.monitorInfo,
        )
    } == 0
    {
        return Err(backend("window monitor"));
    }
    let (display, frame) = window::logical_frame(
        rect(bounds),
        rect(monitor.monitorInfo.rcMonitor),
        &string(&monitor.szDevice),
        monitors,
        &mut *ids.lock().map_err(|_| backend("display identities"))?,
    )
    .map_err(|_| backend("window display geometry"))?;
    let mut title = vec![0_u16; 32768];
    let mut class = [0_u16; 256];
    // SAFETY: read-only User32 caption/class queries with bounded output buffers.
    // GetWindowText retrieves foreign process captions without sending WM_GETTEXT.
    let (title_len, class_len, style, ex_style, owner, root, visible, iconic) = unsafe {
        (
            GetWindowTextW(window, title.as_mut_ptr(), title.len() as i32),
            GetClassNameW(window, class.as_mut_ptr(), class.len() as i32),
            GetWindowLongPtrW(window, GWL_STYLE) as u32,
            GetWindowLongPtrW(window, GWL_EXSTYLE) as u32,
            GetWindow(window, GW_OWNER),
            GetAncestor(window, GA_ROOT) == window,
            IsWindowVisible(window) != 0,
            IsIconic(window) != 0,
        )
    };
    // SAFETY: read-only documented desktop-membership call on this native thread.
    let current_desktop = unsafe {
        native
            .desktop
            .as_ref()
            .ok_or_else(|| backend("virtual desktop manager"))?
            .IsWindowOnCurrentVirtualDesktop(windows::Win32::Foundation::HWND(window))
    }
    .ok()
    .map(|b| b.as_bool());
    if identity(window, Some(before))?.0 != before {
        return Err(backend("window identity changed"));
    }
    Ok(Observation {
        identity: before,
        title: String::from_utf16_lossy(&title[..title_len.max(0) as usize]),
        app_id,
        class: String::from_utf16_lossy(&class[..class_len.max(0) as usize]),
        style,
        ex_style,
        owner: (!owner.is_null()).then(|| number(owner)),
        root,
        visible,
        iconic,
        cloaked: cloaked != 0,
        current_desktop,
        display,
        frame,
        fills_monitor: rect(bounds) == rect(monitor.monitorInfo.rcMonitor),
    })
}

unsafe extern "system" fn enumerate(window: HWND, parameter: LPARAM) -> i32 {
    // SAFETY: EnumWindows synchronously passes our live Enumeration pointer.
    let enumeration = unsafe { &mut *(parameter as *mut Enumeration<'_>) };
    let id = number(window);
    // Crucially this test allowlist comparison precedes EVERY foreign field API.
    if id != number(enumeration.observer) && enumeration.context.allows(id) {
        enumeration.windows.push(id);
    }
    1
}
struct Enumeration<'a> {
    observer: HWND,
    context: &'a Context,
    windows: Vec<u64>,
}

fn emit(events: Vec<WindowEvent>, sink: &Option<Arc<dyn EventSink<WindowEvent>>>) {
    if let Some(sink) = sink {
        for event in events {
            sink.send(event);
        }
    }
}

fn observe(
    id: u64,
    context: &Context,
    native: &Native,
    monitors: &[MonitorProbe],
    ids: &Mutex<DisplayIds>,
    sink: &Option<Arc<dyn EventSink<WindowEvent>>>,
) -> Result<(), PlatformError> {
    if !context.allows(id)
        || id == number(native.window)
        || !context.state.fields_open.load(Ordering::Acquire)
    {
        return Ok(());
    }
    let result = snapshot(
        hwnd(id),
        native,
        monitors,
        ids,
        context.admission.as_ref().map(|a| a.identity),
    );
    let events = match result {
        Ok(observation) => context
            .state
            .windows
            .lock()
            .map_err(|_| backend("window observer state"))?
            .observe(observation),
        Err(_) => {
            // SAFETY: this is an admitted HWND; absence is checked without reading content.
            if unsafe { IsWindow(hwnd(id)) } == 0 {
                context
                    .state
                    .windows
                    .lock()
                    .map_err(|_| backend("window observer state"))?
                    .close(id, None)
            } else {
                Vec::new()
            }
        }
    };
    emit(events, sink);
    Ok(())
}

fn desktop_name(desktop: HDESK) -> Option<String> {
    let mut text = [0_u16; 128];
    let mut needed = 0;
    // SAFETY: read-only name query with a fixed initialized output buffer.
    (unsafe {
        GetUserObjectInformationW(
            desktop,
            UOI_NAME,
            text.as_mut_ptr().cast(),
            size_of_val(&text) as u32,
            &mut needed,
        )
    } != 0)
        .then(|| string(&text))
}
fn default_desktop() -> bool {
    // SAFETY: opens the input desktop for name reads only, never switches it.
    let input = unsafe { OpenInputDesktop(0, 0, DESKTOP_READOBJECTS) };
    if input.is_null() {
        return false;
    }
    // SAFETY: queries this thread's desktop handle; it is borrowed, not closed here.
    let current = unsafe { GetThreadDesktop(GetCurrentThreadId()) };
    let okay = desktop_name(input).as_deref() == Some("Default")
        && desktop_name(current).as_deref() == Some("Default");
    // SAFETY: closes only the owned input-desktop query handle.
    unsafe { CloseDesktop(input) };
    okay
}

fn activate(
    id: WindowId,
    call: &Call,
    context: &Context,
    native: &Native,
    probes: &[MonitorProbe],
    ids: &Mutex<DisplayIds>,
) -> Result<(), PlatformError> {
    call.check(&context.state)?;
    let expected = context
        .state
        .windows
        .lock()
        .map_err(|_| backend("window observer state"))?
        .identity(id)
        .ok_or(PlatformError::NotFound)?;
    if !context.allows(expected.hwnd) {
        return Err(PlatformError::NotFound);
    }
    if !context.state.fields_open.load(Ordering::Acquire) {
        return Err(PlatformError::Locked);
    }
    let observation = snapshot(hwnd(expected.hwnd), native, probes, ids, Some(expected))?;
    let state = observation.state();
    if !window::may_activate(
        expected,
        observation.identity,
        observation.current_desktop,
        default_desktop(),
        state,
    ) {
        return Err(PlatformError::Locked);
    }
    // Native queries may pump callbacks and invalidate the observer after queue admission.
    call.check(&context.state)?;
    // SAFETY: only the identity-checked, current-desktop, non-hidden target is restored.
    // A parked/hidden window never reaches this branch; no desktop switches occur.
    if observation.iconic {
        // SAFETY: same validated non-hidden, current-desktop target as above.
        unsafe { ShowWindowAsync(hwnd(expected.hwnd), SW_RESTORE) };
    }
    if identity(hwnd(expected.hwnd), Some(expected))?.0 != expected {
        return Err(PlatformError::NotFound);
    }
    call.check(&context.state)?;
    // SAFETY: documented conditional focus request; never ALT/AttachThreadInput or a bypass.
    unsafe { SetForegroundWindow(hwnd(expected.hwnd)) };
    loop {
        call.check(&context.state)?;
        if identity(hwnd(expected.hwnd), Some(expected))?.0 != expected {
            return Err(PlatformError::NotFound);
        }
        call.check(&context.state)?;
        // SAFETY: foreground handle observation only, no foreign content query.
        if unsafe { GetForegroundWindow() } == hwnd(expected.hwnd) {
            call.check(&context.state)?;
            return Ok(());
        }
        pump();
        thread::sleep(Duration::from_millis(10));
    }
}

fn pump() {
    let mut message = MSG::default();
    // SAFETY: pumps only this observer thread's message queue and registered procedures.
    unsafe {
        for _ in 0..128 {
            if PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) == 0 {
                break;
            }
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

fn run(
    state: &Arc<State>,
    receive: mpsc::Receiver<Call>,
    ids: Arc<Mutex<DisplayIds>>,
    reader: MonitorReader,
    admission: Option<Admission>,
    ready: mpsc::SyncSender<Result<(), PlatformError>>,
) {
    let context = Arc::new(Context {
        raw: Mutex::new(VecDeque::new()),
        state: Arc::clone(state),
        start: Instant::now(),
        admission,
    });
    CONTEXT.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&context)));
    let native = match Native::new(&context) {
        Ok(native) => native,
        Err(error) => {
            let _ = ready.send(Err(error));
            CONTEXT.with(|s| s.borrow_mut().take());
            return;
        }
    };
    let mut sink = None;
    let mut last_scan = Instant::now() - Duration::from_secs(1);
    let mut initialized = false;
    let mut ready = Some(ready);
    let mut probes = Vec::new();
    while state.alive.load(Ordering::Acquire) && !state.fault.load(Ordering::Acquire) {
        let step = (|| -> Result<(), PlatformError> {
            pump();
            if last_scan.elapsed() >= Duration::from_millis(250) {
                probes = reader()?;
                let mut enumeration = Enumeration {
                    observer: native.window,
                    context: &context,
                    windows: Vec::new(),
                };
                // SAFETY: synchronous callback borrows this stack-local enumeration only.
                if unsafe {
                    EnumWindows(
                        Some(enumerate),
                        (&mut enumeration as *mut Enumeration<'_>) as LPARAM,
                    )
                } == 0
                {
                    return Err(backend("window enumeration"));
                }
                for id in enumeration.windows {
                    observe(id, &context, &native, &probes, &ids, &sink)?;
                }
                // Also retain/check already admitted hidden HWNDs omitted by EnumWindows.
                let table = state
                    .windows
                    .lock()
                    .map_err(|_| backend("window observer state"))?;
                let known: Vec<_> = table
                    .list()
                    .iter()
                    .filter_map(|w| table.identity(w.id))
                    .map(|i| i.hwnd)
                    .collect();
                drop(table);
                for id in known {
                    observe(id, &context, &native, &probes, &ids, &sink)?;
                }
                last_scan = Instant::now();
            }
            let raw: Vec<_> = context
                .raw
                .lock()
                .map_err(|_| backend("window event queue"))?
                .drain(..)
                .collect();
            for event in raw {
                let (events, refresh) = state
                    .windows
                    .lock()
                    .map_err(|_| backend("window observer state"))?
                    .event(event);
                emit(events, &sink);
                if refresh {
                    observe(event.hwnd, &context, &native, &probes, &ids, &sink)?;
                }
            }
            let now = MonoTime::from_nanos(
                context.start.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
            );
            let due = state
                .windows
                .lock()
                .map_err(|_| backend("window observer state"))?
                .due(now);
            for id in due {
                observe(id, &context, &native, &probes, &ids, &sink)?;
            }
            // SAFETY: compares only the foreground handle, never its foreign fields.
            let foreground = number(unsafe { GetForegroundWindow() });
            let events = state
                .windows
                .lock()
                .map_err(|_| backend("window observer state"))?
                .focus(Some(foreground));
            emit(events, &sink);
            *state
                .updated
                .lock()
                .map_err(|_| backend("window observer state"))? = Instant::now();
            if !initialized {
                initialized = true;
                if let Some(ready) = ready.take() {
                    let _ = ready.send(Ok(()));
                }
            }
            if let Ok(call) = receive.try_recv() {
                let result = call.check(state).and_then(|()| match &call.operation {
                    Operation::Subscribe(new_sink) if sink.is_none() => {
                        let table = state
                            .windows
                            .lock()
                            .map_err(|_| backend("window observer state"))?;
                        let mut events: Vec<_> =
                            table.list().into_iter().map(WindowEvent::Added).collect();
                        events.push(WindowEvent::Focused(table.focused()));
                        drop(table);
                        sink = Some(Arc::clone(new_sink));
                        emit(events, &sink);
                        Ok(())
                    }
                    Operation::Subscribe(_) => Err(backend("window subscription already set")),
                    Operation::Activate(id) => {
                        activate(*id, &call, &context, &native, &probes, &ids)
                    }
                    #[cfg(test)]
                    Operation::FreezeFixtureFields => {
                        state.fields_open.store(false, Ordering::Release);
                        Ok(())
                    }
                });
                let _ = call.reply.send(result);
            }
            Ok(())
        })();
        if let Err(error) = step {
            state.fault.store(true, Ordering::Release);
            if let Some(ready) = ready.take() {
                let _ = ready.send(Err(error));
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    drop(native);
    CONTEXT.with(|slot| slot.borrow_mut().take());
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(clippy::unwrap_used)]
    fn state() -> Arc<State> {
        Arc::new(State {
            windows: Mutex::new(Windows::default()),
            updated: Mutex::new(Instant::now()),
            alive: AtomicBool::new(true),
            fault: AtomicBool::new(false),
            cleaned: AtomicBool::new(false),
            fields_open: AtomicBool::new(true),
        })
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn window_callback_allowlist_and_object_filter_precede_any_field_api() {
        let state = state();
        let context = Arc::new(Context {
            raw: Mutex::new(VecDeque::new()),
            state,
            start: Instant::now(),
            admission: Some(Admission {
                hwnds: BTreeSet::from([7]),
                pid: 8,
                identity: Identity {
                    hwnd: 7,
                    pid: 8,
                    tid: 9,
                    process_created: 10,
                },
            }),
        });
        CONTEXT.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&context)));
        // SAFETY: fake numeric HWNDs are only compared/enqueued; this callback calls no Win32 API.
        unsafe {
            event_callback(
                null_mut(),
                winevent::EVENT_OBJECT_SHOW,
                hwnd(99),
                0,
                0,
                0,
                0,
            );
            event_callback(
                null_mut(),
                winevent::EVENT_OBJECT_SHOW,
                hwnd(7),
                -4,
                0,
                0,
                0,
            );
            event_callback(null_mut(), winevent::EVENT_OBJECT_SHOW, hwnd(7), 0, 1, 0, 0);
            event_callback(null_mut(), winevent::EVENT_OBJECT_SHOW, hwnd(7), 0, 0, 0, 0);
        }
        let raw = context.raw.lock().unwrap();
        assert_eq!(raw.len(), 1);
        assert_eq!(raw[0].hwnd, 7);
        drop(raw);
        CONTEXT.with(|slot| slot.borrow_mut().take());
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn window_callback_overflow_is_bounded_and_observation_fails_closed() {
        let state = state();
        let context = Arc::new(Context {
            raw: Mutex::new(VecDeque::new()),
            state: Arc::clone(&state),
            start: Instant::now(),
            admission: None,
        });
        CONTEXT.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&context)));
        for _ in 0..=RAW_LIMIT {
            // SAFETY: fake callback admission/enqueue only, no native field access.
            unsafe {
                event_callback(null_mut(), winevent::EVENT_OBJECT_SHOW, hwnd(7), 0, 0, 0, 0);
            }
        }
        assert_eq!(context.raw.lock().unwrap().len(), RAW_LIMIT);
        assert!(state.fault.load(Ordering::Acquire));
        let (commands, _) = mpsc::sync_channel(1);
        let (_, done) = mpsc::channel();
        let source = WindowsWindowSource {
            state,
            commands,
            done,
            thread: None,
        };
        assert!(source.windows().is_err());
        assert!(source.focused().is_err());
        CONTEXT.with(|slot| slot.borrow_mut().take());
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn window_facade_timeout_marks_queued_activation_abandoned() {
        let state = state();
        let (commands, receive) = mpsc::sync_channel(1);
        let (_, done) = mpsc::channel();
        let source = WindowsWindowSource {
            state,
            commands,
            done,
            thread: None,
        };
        let (caller_finished, finished) = mpsc::channel();
        let worker = thread::spawn(move || {
            let call = receive.recv().unwrap();
            finished
                .recv_timeout(BOUND + Duration::from_secs(1))
                .unwrap();
            assert!(call.abandoned.load(Ordering::Acquire));
            assert!(Instant::now() >= call.until);
        });
        let start = Instant::now();
        assert!(matches!(
            source.call(Operation::Activate(WindowId(7))),
            Err(PlatformError::Timeout)
        ));
        assert!(start.elapsed() < BOUND + Duration::from_millis(200));
        caller_finished.send(()).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn window_facade_rejects_stalled_observation_before_queueing() {
        let state = state();
        if let Ok(mut updated) = state.updated.lock() {
            *updated = Instant::now() - BOUND;
        }
        let (commands, receive) = mpsc::sync_channel(1);
        let (_, done) = mpsc::channel();
        let source = WindowsWindowSource {
            state,
            commands,
            done,
            thread: None,
        };
        assert!(matches!(
            source.call(Operation::Activate(WindowId(7))),
            Err(PlatformError::Timeout)
        ));
        assert!(receive.try_recv().is_err());
    }

    #[test]
    fn window_queued_activation_rechecks_fault_lifetime_and_abandonment_before_mutation() {
        let state = state();
        let (reply, _) = mpsc::sync_channel(1);
        let mut call = Call {
            until: Instant::now() + BOUND,
            abandoned: Arc::new(AtomicBool::new(false)),
            reply,
            operation: Operation::Activate(WindowId(7)),
        };
        assert!(call.check(&state).is_ok());
        // A callback can overflow while the worker is in an intervening native query.
        state.fault.store(true, Ordering::Release);
        assert!(call.check(&state).is_err());
        state.fault.store(false, Ordering::Release);
        state.alive.store(false, Ordering::Release);
        assert!(call.check(&state).is_err());
        state.alive.store(true, Ordering::Release);
        call.abandoned.store(true, Ordering::Release);
        assert!(matches!(call.check(&state), Err(PlatformError::Timeout)));
        call.abandoned.store(false, Ordering::Release);
        call.until = Instant::now();
        assert!(matches!(call.check(&state), Err(PlatformError::Timeout)));
    }
}
