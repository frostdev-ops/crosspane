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
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    mem::size_of,
    path::PathBuf,
    ptr::{null, null_mut},
    rc::{Rc, Weak},
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
    popup,
    window::{self, Identity, Observation, Windows},
    winevent::{self, RawWinEvent},
};

const BOUND: Duration = Duration::from_secs(2);
const RAW_LIMIT: usize = 4096;
/// Must return current physical monitor facts within the platform call bound.
pub type MonitorReader = Arc<dyn Fn() -> Result<Vec<MonitorProbe>, PlatformError> + Send + Sync>;

/// A currently admitted native identity. `generation` is the opaque observed-lifetime
/// token (`WindowId.0`), not a sequence number and never an HWND encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeWindow {
    pub hwnd: u64,
    pub pid: u32,
    pub tid: u32,
    pub process_created: u64,
    pub generation: u64,
}

/// Read-only access to the source's one authoritative, live identity table.
#[derive(Clone)]
pub struct WindowResolver {
    state: Arc<State>,
}

impl fmt::Debug for WindowResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WindowResolver").finish_non_exhaustive()
    }
}

impl WindowResolver {
    pub fn resolve(&self, id: WindowId) -> Option<NativeWindow> {
        self.resolve_with(id, |expected| {
            // Restricted sources must admit PID/lifetime/image BEFORE even IsWindow or fields.
            if !self
                .state
                .admission
                .as_ref()
                .is_none_or(|a| a.allows(expected.hwnd))
            {
                return None;
            }
            // SAFETY: checks only the admitted handle's existence before fresh identity queries.
            if unsafe { IsWindow(hwnd(expected.hwnd)) } == 0 {
                return None;
            }
            let fresh = identity(hwnd(expected.hwnd), Some(expected)).ok()?.0;
            self.state
                .admission
                .as_ref()
                .is_none_or(|a| a.allows(expected.hwnd))
                .then_some(fresh)
        })
    }

    fn resolve_with(
        &self,
        id: WindowId,
        query: impl FnOnce(Identity) -> Option<Identity>,
    ) -> Option<NativeWindow> {
        let healthy = || {
            self.state.alive.load(Ordering::Acquire)
                && !self.state.fault.load(Ordering::Acquire)
                && self
                    .state
                    .updated
                    .lock()
                    .is_ok_and(|updated| updated.elapsed() < BOUND)
        };
        if !healthy() {
            return None;
        }
        let expected = self.state.windows.lock().ok()?.identity(id)?;
        let fresh = query(expected)?;
        if fresh != expected
            || !healthy()
            || self.state.windows.lock().ok()?.identity(id) != Some(expected)
        {
            return None;
        }
        Some(NativeWindow {
            hwnd: fresh.hwnd,
            pid: fresh.pid,
            tid: fresh.tid,
            process_created: fresh.process_created,
            generation: id.0,
        })
    }
}

struct State {
    admission: Option<Admission>,
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
    pub fn resolver(&self) -> WindowResolver {
        WindowResolver {
            state: Arc::clone(&self.state),
        }
    }

    pub fn new(
        ids: Arc<Mutex<DisplayIds>>,
        monitors: MonitorReader,
    ) -> Result<Self, PlatformError> {
        Self::start(ids, monitors, None)
    }

    /// Own claims are independently opened and pinned; no inherited handle is trusted.
    /// All nonadmitted HWNDs are filtered by PID before any window field is acquired.
    #[cfg_attr(test, allow(dead_code))] // Other existing probes compile this adapter in isolation.
    pub fn new_restricted(
        ids: Arc<Mutex<DisplayIds>>,
        monitors: MonitorReader,
        allowlist: OwnedProcessAllowlist,
    ) -> Result<Self, PlatformError> {
        Self::start(
            ids,
            monitors,
            Some(Admission::Restricted(Arc::new(allowlist))),
        )
    }

    fn start(
        ids: Arc<Mutex<DisplayIds>>,
        monitors: MonitorReader,
        admission: Option<Admission>,
    ) -> Result<Self, PlatformError> {
        let state = Arc::new(State {
            admission: admission.clone(),
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
            Some(Admission::Fixture {
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

/// An explicitly owned process claim, supplied by the scratch launcher.
#[derive(Clone, Debug)]
pub struct OwnedProcessClaim {
    pub pid: u32,
    pub process_created: u64,
    pub executable: PathBuf,
}
/// Independent read-only process handles retained for the entire restricted source lifetime.
pub struct OwnedProcessAllowlist {
    processes: Vec<OwnedProcess>,
}
impl fmt::Debug for OwnedProcessAllowlist {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedProcessAllowlist")
            .field("claims", &self.processes.len())
            .finish_non_exhaustive()
    }
}
struct OwnedProcess {
    claim: OwnedProcessClaim,
    // A Windows process handle is thread-independent. Store its integer representation so no
    // non-Send native pointer or unsafe Send impl crosses the source/resolver thread boundary.
    handle: usize,
}
impl Drop for OwnedProcess {
    fn drop(&mut self) {
        // SAFETY: exactly one owner closes this successful OpenProcess handle after all Arc users.
        unsafe { CloseHandle(self.handle as HANDLE) };
    }
}
impl OwnedProcess {
    fn verify(&self) -> bool {
        let process = self.handle as HANDLE;
        // SAFETY: query only the process independently retained at claim admission; zero wait.
        if unsafe { WaitForSingleObject(process, 0) } != WAIT_TIMEOUT {
            return false;
        }
        let mut created = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        // SAFETY: valid retained process handle and initialized exact FILETIME output storage.
        if unsafe { GetProcessTimes(process, &mut created, &mut exit, &mut kernel, &mut user) } == 0
            || ((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
                != self.claim.process_created
        {
            return false;
        }
        let mut image = vec![0_u16; 32768];
        let mut length = image.len() as u32;
        // SAFETY: only the admitted, alive creation-matched process image is queried, bounded buffer.
        if unsafe { QueryFullProcessImageNameW(process, 0, image.as_mut_ptr(), &mut length) } == 0 {
            return false;
        }
        let image = PathBuf::from(String::from_utf16_lossy(&image[..length as usize]));
        std::fs::canonicalize(image).is_ok_and(|image| image == self.claim.executable)
    }
}
fn admitted_pid<T>(
    pid: impl FnOnce() -> u32,
    find: impl FnOnce(u32) -> Option<T>,
    verify: impl FnOnce(T) -> bool,
) -> bool {
    let pid = pid();
    pid != 0 && find(pid).is_some_and(verify)
}
impl OwnedProcessAllowlist {
    #[cfg_attr(test, allow(dead_code))] // Other existing probes compile this adapter in isolation.
    pub fn admit(claims: Vec<OwnedProcessClaim>) -> Result<Self, PlatformError> {
        if claims.is_empty() || claims.len() > 128 {
            return Err(backend("owned process claims"));
        }
        let mut processes: Vec<OwnedProcess> = Vec::with_capacity(claims.len());
        for mut claim in claims {
            if claim.pid == 0
                || claim.process_created == 0
                || !claim.executable.is_absolute()
                || processes.iter().any(|p| p.claim.pid == claim.pid)
            {
                return Err(backend("owned process claims"));
            }
            claim.executable = std::fs::canonicalize(&claim.executable)
                .map_err(|_| backend("owned process image"))?;
            // SAFETY: caller claimed this exact PID; rights permit metadata and lifetime queries only.
            let handle = unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                    0,
                    claim.pid,
                )
            };
            if handle.is_null() {
                return Err(backend("owned process admission"));
            }
            let process = OwnedProcess {
                claim,
                handle: handle as usize,
            };
            if !process.verify() {
                return Err(backend("owned process identity"));
            }
            processes.push(process);
        }
        Ok(Self { processes })
    }
    fn expected_for(&self, window: u64) -> Result<Identity, PlatformError> {
        let mut pid = 0;
        // SAFETY: the ONLY not-yet-admitted HWND query is PID/TID metadata.
        let tid = unsafe { GetWindowThreadProcessId(hwnd(window), &mut pid) };
        let process = self.processes.iter().find(|p| p.claim.pid == pid);
        if tid == 0 || !admitted_pid(|| pid, |_| process, OwnedProcess::verify) {
            return Err(PlatformError::NotFound);
        }
        let process = process.ok_or(PlatformError::NotFound)?;
        let mut after_pid = 0;
        // SAFETY: recheck metadata after the owned process/lifetime/image queries.
        let after_tid = unsafe { GetWindowThreadProcessId(hwnd(window), &mut after_pid) };
        if after_pid != pid || after_tid != tid {
            return Err(PlatformError::NotFound);
        }
        Ok(Identity {
            hwnd: window,
            pid,
            tid,
            process_created: process.claim.process_created,
        })
    }
    fn allows(&self, window: u64) -> bool {
        self.expected_for(window).is_ok()
    }
}
#[derive(Clone)]
enum Admission {
    #[cfg_attr(test, allow(dead_code))] // Existing exact-HWND probes use Fixture admission.
    Restricted(Arc<OwnedProcessAllowlist>),
    #[cfg(test)]
    Fixture {
        hwnds: BTreeSet<u64>,
        pid: u32,
        identity: Identity,
    },
}
impl Admission {
    fn allows(&self, window: u64) -> bool {
        match self {
            Self::Restricted(allowlist) => allowlist.allows(window),
            #[cfg(test)]
            Self::Fixture { hwnds, .. } => hwnds.contains(&window),
        }
    }
    fn expected_for(&self, window: u64) -> Result<Identity, PlatformError> {
        match self {
            Self::Restricted(a) => a.expected_for(window),
            #[cfg(test)]
            Self::Fixture {
                identity, hwnds, ..
            } => {
                if hwnds.contains(&window) {
                    Ok(*identity)
                } else {
                    Err(PlatformError::NotFound)
                }
            }
        }
    }
    fn hook_pid(&self) -> u32 {
        match self {
            Self::Restricted(a) if a.processes.len() == 1 => a.processes[0].claim.pid,
            Self::Restricted(_) => 0,
            #[cfg(test)]
            Self::Fixture { pid, .. } => *pid,
        }
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
        let pid = context.admission.as_ref().map_or(0, Admission::hook_pid);
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

fn with_identity_owner<T>(
    window: u64,
    pid: u32,
    tid: u32,
    expected: Option<Identity>,
    query: impl FnOnce() -> Result<T, PlatformError>,
) -> Result<T, PlatformError> {
    if tid == 0
        || pid == 0
        || expected.is_some_and(|e| e.pid != pid || e.tid != tid || e.hwnd != window)
    {
        return Err(PlatformError::NotFound);
    }
    query()
}
fn identity(window: HWND, expected: Option<Identity>) -> Result<(Identity, String), PlatformError> {
    let mut pid = 0;
    // SAFETY: PID/TID metadata only; reject changed restricted ownership before any process image.
    let tid = unsafe { GetWindowThreadProcessId(window, &mut pid) };
    with_identity_owner(number(window), pid, tid, expected, || {
        identity_metadata(window, pid, tid, expected)
    })
}
fn identity_metadata(
    window: HWND,
    pid: u32,
    tid: u32,
    expected: Option<Identity>,
) -> Result<(Identity, String), PlatformError> {
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

fn admitted_field<T>(
    expected: Identity,
    admission: Option<&Admission>,
    query: impl FnOnce() -> T,
) -> Result<T, PlatformError> {
    if let Some(a) = admission
        && a.expected_for(expected.hwnd)? != expected
    {
        return Err(PlatformError::NotFound);
    }
    Ok(query())
}

fn snapshot(
    window: HWND,
    native: &Native,
    monitors: &[MonitorProbe],
    ids: &Mutex<DisplayIds>,
    expected: Option<Identity>,
    admission: Option<&Admission>,
) -> Result<Observation, PlatformError> {
    let (before, app_id) = identity(window, expected)?;
    let mut bounds = RECT::default();
    let mut cloaked = 0_u32;
    let bounds_ok = admitted_field(before, admission, || {
        // SAFETY: exact RECT output storage, restricted ownership revalidated before this query.
        (unsafe {
            DwmGetWindowAttribute(
                window,
                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                (&mut bounds as *mut RECT).cast(),
                size_of::<RECT>() as u32,
            )
        }) >= 0
    })?;
    let cloaked_ok = admitted_field(before, admission, || {
        // SAFETY: exact u32 output storage, restricted ownership revalidated before this query.
        (unsafe {
            DwmGetWindowAttribute(
                window,
                DWMWA_CLOAKED as u32,
                (&mut cloaked as *mut u32).cast(),
                size_of::<u32>() as u32,
            )
        }) >= 0
    })?;
    if !bounds_ok || !cloaked_ok {
        return Err(backend("window geometry"));
    }
    let mut monitor = MONITORINFOEXW::default();
    monitor.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    let native_monitor = admitted_field(before, admission, || {
        // SAFETY: admitted target monitor observation only.
        unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST) }
    })?;
    let monitor_ok = admitted_field(before, admission, || {
        // SAFETY: exact extended monitor output storage after target ownership revalidation.
        unsafe { GetMonitorInfoW(native_monitor, &mut monitor.monitorInfo) }
    })?;
    if monitor_ok == 0 {
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
    let title_len = admitted_field(before, admission, || {
        // SAFETY: bounded caption buffer after exact restricted owner revalidation.
        unsafe { GetWindowTextW(window, title.as_mut_ptr(), title.len() as i32) }
    })?;
    let class_len = admitted_field(before, admission, || {
        // SAFETY: bounded class buffer after exact restricted owner revalidation.
        unsafe { GetClassNameW(window, class.as_mut_ptr(), class.len() as i32) }
    })?;
    let style = admitted_field(before, admission, || {
        // SAFETY: read-only admitted target style.
        unsafe { GetWindowLongPtrW(window, GWL_STYLE) as u32 }
    })?;
    let ex_style = admitted_field(before, admission, || {
        // SAFETY: read-only admitted target extended style.
        unsafe { GetWindowLongPtrW(window, GWL_EXSTYLE) as u32 }
    })?;
    let owner = admitted_field(before, admission, || {
        // SAFETY: admitted target ownership-handle query only.
        unsafe { GetWindow(window, GW_OWNER) }
    })?;
    let root = admitted_field(before, admission, || {
        // SAFETY: admitted target ancestor-handle comparison only.
        unsafe { GetAncestor(window, GA_ROOT) == window }
    })?;
    let visible = admitted_field(before, admission, || {
        // SAFETY: admitted target visibility query only.
        unsafe { IsWindowVisible(window) != 0 }
    })?;
    let iconic = admitted_field(before, admission, || {
        // SAFETY: admitted target iconic-state query only.
        unsafe { IsIconic(window) != 0 }
    })?;
    let desktop = native
        .desktop
        .as_ref()
        .ok_or_else(|| backend("virtual desktop manager"))?;
    let current_desktop = admitted_field(before, admission, || {
        // SAFETY: documented read-only desktop-membership query on this native thread.
        unsafe { desktop.IsWindowOnCurrentVirtualDesktop(windows::Win32::Foundation::HWND(window)) }
    })?
    .ok()
    .map(|b| b.as_bool());
    admitted_field(before, admission, || ())?;
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
    let expected = context
        .admission
        .as_ref()
        .map(|a| a.expected_for(id))
        .transpose();
    let result = expected.and_then(|expected| {
        snapshot(
            hwnd(id),
            native,
            monitors,
            ids,
            expected,
            context.admission.as_ref(),
        )
    });
    let events = match result {
        Ok(_) if !context.allows(id) => Vec::new(),
        Ok(observation) => context
            .state
            .windows
            .lock()
            .map_err(|_| backend("window observer state"))?
            .observe(observation),
        Err(_) => {
            // SAFETY: this is an admitted HWND; absence is checked without reading content.
            if context.allows(id) && unsafe { IsWindow(hwnd(id)) } == 0 {
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
    let observation = snapshot(
        hwnd(expected.hwnd),
        native,
        probes,
        ids,
        Some(expected),
        context.admission.as_ref(),
    )?;
    if !context.allows(expected.hwnd) {
        return Err(PlatformError::NotFound);
    }
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
    if !context.allows(expected.hwnd)
        || identity(hwnd(expected.hwnd), Some(expected))?.0 != expected
    {
        return Err(PlatformError::NotFound);
    }
    call.check(&context.state)?;
    // SAFETY: documented conditional focus request; never ALT/AttachThreadInput or a bypass.
    unsafe { SetForegroundWindow(hwnd(expected.hwnd)) };
    loop {
        call.check(&context.state)?;
        if !context.allows(expected.hwnd)
            || identity(hwnd(expected.hwnd), Some(expected))?.0 != expected
        {
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
    let mut reads = window::MonitorReads::default();
    while state.alive.load(Ordering::Acquire) && !state.fault.load(Ordering::Acquire) {
        let step = (|| -> Result<(), PlatformError> {
            pump();
            if last_scan.elapsed() >= Duration::from_millis(250) {
                let read = reader();
                let at = MonoTime::from_nanos(
                    context.start.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
                );
                match (reads.record(read.is_ok(), at), read) {
                    (window::MonitorRead::Scan, Ok(fresh)) => {
                        probes = fresh;
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
                    }
                    // The first read still fails closed (startup unchanged). A lasting
                    // outage faults.
                    (outcome, Err(error))
                        if !initialized || outcome == window::MonitorRead::Fault =>
                    {
                        return Err(error);
                    }
                    // A topology change failed one coherent read. No frame is mapped until a
                    // read succeeds; every admitted identity stays resolvable.
                    _ => probes.clear(),
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

// Private, per-root capture observation. It never enters the projectable window table.
// Hooks and callbacks live only on the capture worker's message-loop thread.
// Existing window-only integration fixtures source-include this adapter without frame_capture.
// Suppress only these new private popup items in test copies; product dead_code stays active.
#[cfg_attr(test, allow(dead_code))]
struct PopupQueue {
    root: NativeWindow,
    events: RefCell<VecDeque<(u32, u64)>>,
    fault: std::cell::Cell<bool>,
}
thread_local! {
    static POPUP_HOOKS: RefCell<BTreeMap<usize, Weak<PopupQueue>>> = const { RefCell::new(BTreeMap::new()) };
}

#[cfg_attr(test, allow(dead_code))]
unsafe extern "system" fn popup_callback(
    hook: HWINEVENTHOOK,
    event: u32,
    window: HWND,
    object: i32,
    child: i32,
    event_tid: u32,
    _: u32,
) {
    // No native field calls and no outstanding RefCell borrow across reentrancy.
    let queue = POPUP_HOOKS.with(|hooks| {
        hooks
            .try_borrow()
            .ok()
            .and_then(|hooks| hooks.get(&(hook as usize)).and_then(Weak::upgrade))
    });
    let Some(queue) = queue else {
        return;
    };
    if window.is_null()
        || object != winevent::OBJID_WINDOW
        || child != winevent::CHILDID_SELF
        || event_tid != queue.root.tid
    {
        return;
    }
    if let Ok(mut events) = queue.events.try_borrow_mut() {
        if events.len() < popup::EVENT_CAP {
            events.push_back((event, number(window)));
        } else {
            queue.fault.set(true);
        }
    } else {
        queue.fault.set(true);
    }
}

/// Same-thread capture observer; an Rc prevents sending native hooks across threads.
#[cfg_attr(test, allow(dead_code))]
pub(crate) struct PopupObserver {
    root: NativeWindow,
    resolver: WindowResolver,
    process: HANDLE,
    hooks: Vec<HWINEVENTHOOK>,
    queue: Rc<PopupQueue>,
    generations: popup::Generations,
}
impl fmt::Debug for PopupObserver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PopupObserver").finish_non_exhaustive()
    }
}

impl Drop for PopupObserver {
    fn drop(&mut self) {
        for hook in self.hooks.drain(..) {
            // SAFETY: exact retained hook, removed on its installing thread while queue lives.
            let removed = unsafe { UnhookWinEvent(hook) } != 0;
            POPUP_HOOKS.with(|hooks| {
                hooks.borrow_mut().remove(&(hook as usize));
            });
            if !removed {
                // A later callback has no registered state and therefore cannot dereference us.
                self.queue.fault.set(true);
            }
        }
        // SAFETY: exactly this observer's successful metadata/lifetime process handle.
        unsafe { CloseHandle(self.process) };
    }
}

#[cfg_attr(test, allow(dead_code))]
struct ThreadDpi(DPI_AWARENESS_CONTEXT);
#[cfg_attr(test, allow(dead_code))]
impl ThreadDpi {
    fn physical() -> Result<Self, PlatformError> {
        // SAFETY: changes only this worker thread; scope restores the exact previous context.
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        if previous.is_null() {
            Err(backend("popup DPI context"))
        } else {
            Ok(Self(previous))
        }
    }
}
impl Drop for ThreadDpi {
    fn drop(&mut self) {
        // SAFETY: balances this same thread's scoped awareness change after physical queries.
        unsafe { SetThreadDpiAwarenessContext(self.0) };
    }
}

#[cfg_attr(test, allow(dead_code))]
impl PopupObserver {
    pub(crate) fn new(
        root: NativeWindow,
        resolver: WindowResolver,
        until: Instant,
    ) -> Result<Self, PlatformError> {
        popup_deadline(until)?;
        if resolver.resolve(WindowId(root.generation)) != Some(root) {
            return Err(PlatformError::NotFound);
        }
        popup_deadline(until)?;
        // SAFETY: already resolver-admitted root PID; read-only identity/lifetime rights only.
        let process = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                root.pid,
            )
        };
        if process.is_null() {
            return Err(backend("popup process identity"));
        }
        let mut observer = Self {
            root,
            resolver,
            process,
            hooks: Vec::new(),
            generations: popup::Generations::default(),
            queue: Rc::new(PopupQueue {
                root,
                events: RefCell::new(VecDeque::new()),
                fault: std::cell::Cell::new(false),
            }),
        };
        if !observer.root_valid(until) {
            return Err(PlatformError::NotFound);
        }
        for (first, last) in [
            (
                winevent::EVENT_OBJECT_CREATE,
                winevent::EVENT_OBJECT_REORDER,
            ),
            (
                winevent::EVENT_OBJECT_LOCATIONCHANGE,
                winevent::EVENT_OBJECT_LOCATIONCHANGE,
            ),
            (
                winevent::EVENT_OBJECT_CLOAKED,
                winevent::EVENT_OBJECT_UNCLOAKED,
            ),
        ] {
            popup_deadline(until)?;
            // SAFETY: static extern-system callback, exact nonzero PID/TID, same thread pumps it.
            let hook = unsafe {
                SetWinEventHook(
                    first,
                    last,
                    null_mut(),
                    Some(popup_callback),
                    root.pid,
                    root.tid,
                    winevent::WINEVENT_OUTOFCONTEXT,
                )
            };
            if hook.is_null() {
                return Err(backend("popup event hook"));
            }
            observer.hooks.push(hook);
            POPUP_HOOKS.with(|hooks| {
                hooks
                    .borrow_mut()
                    .insert(hook as usize, Rc::downgrade(&observer.queue));
            });
        }
        Ok(observer)
    }

    fn process_valid(&self, until: Instant) -> bool {
        if Instant::now() >= until {
            return false;
        }
        // SAFETY: read-only zero wait on our independently retained process handle.
        if unsafe { WaitForSingleObject(self.process, 0) } != WAIT_TIMEOUT {
            return false;
        }
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        if Instant::now() >= until {
            return false;
        }
        // SAFETY: initialized exact FILETIME outputs and a live own query handle.
        (unsafe {
            GetProcessTimes(
                self.process,
                &mut created,
                &mut exited,
                &mut kernel,
                &mut user,
            )
        }) != 0
            && Instant::now() < until
            && ((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
                == self.root.process_created
    }

    fn root_valid(&self, until: Instant) -> bool {
        self.process_valid(until)
            && self.resolver.resolve(WindowId(self.root.generation)) == Some(self.root)
            && Instant::now() < until
    }

    fn identity(&self, window: u64, until: Instant) -> Result<Identity, PlatformError> {
        popup_deadline(until)?;
        let mut pid = 0;
        // SAFETY: the only not-yet-admitted HWND query is creator PID/TID metadata.
        let tid = unsafe { GetWindowThreadProcessId(hwnd(window), &mut pid) };
        if window == 0 || pid != self.root.pid || tid != self.root.tid || !self.process_valid(until)
        {
            return Err(PlatformError::NotFound);
        }
        let allowed = match self.resolver.state.admission.as_ref() {
            #[cfg(test)]
            Some(Admission::Fixture { identity, .. }) => {
                *identity
                    == Identity {
                        hwnd: self.root.hwnd,
                        pid: self.root.pid,
                        tid: self.root.tid,
                        process_created: self.root.process_created,
                    }
            }
            admission => admission.is_none_or(|a| a.allows(window)),
        };
        popup_deadline(until)?;
        if !allowed {
            return Err(PlatformError::NotFound);
        }
        Ok(Identity {
            hwnd: window,
            pid,
            tid,
            process_created: self.root.process_created,
        })
    }

    pub(crate) fn unchanged(&mut self, expected: &popup::Snapshot, until: Instant) -> bool {
        self.snapshot(until).is_ok_and(|fresh| fresh == *expected)
    }

    fn geometry(
        &self,
        identity: Identity,
        until: Instant,
    ) -> Result<popup::Geometry, PlatformError> {
        if self.identity(identity.hwnd, until)? != identity {
            return Err(PlatformError::NotFound);
        }
        let mut bounds = RECT::default();
        popup_deadline(until)?;
        // SAFETY: own-admitted exact HWND and initialized RECT storage; physical DWM screen bounds.
        if unsafe {
            DwmGetWindowAttribute(
                hwnd(identity.hwnd),
                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                (&mut bounds as *mut RECT).cast(),
                size_of::<RECT>() as u32,
            )
        } < 0
        {
            return Err(backend("popup capture geometry"));
        }
        if self.identity(identity.hwnd, until)? != identity {
            return Err(PlatformError::NotFound);
        }
        popup_deadline(until)?;
        // SAFETY: only the freshly admitted own target's DPI, used as a label, not another scaling.
        let dpi = unsafe { GetDpiForWindow(hwnd(identity.hwnd)) };
        let width = u32::try_from(i64::from(bounds.right) - i64::from(bounds.left))
            .map_err(|_| backend("popup capture geometry"))?;
        let height = u32::try_from(i64::from(bounds.bottom) - i64::from(bounds.top))
            .map_err(|_| backend("popup capture geometry"))?;
        let geometry = popup::Geometry {
            bounds: crosspane_types::geom::PixelRect::new(
                (bounds.left, bounds.top).into(),
                (bounds.right, bounds.bottom).into(),
            ),
            content: crosspane_types::geom::PixelSize::new(width, height),
            dpi,
        };
        if !popup::geometry_valid(geometry) || self.identity(identity.hwnd, until)? != identity {
            return Err(backend("popup capture geometry"));
        }
        Ok(geometry)
    }

    fn candidate(
        &mut self,
        window: u64,
        root: popup::Token,
        until: Instant,
    ) -> Result<popup::Candidate, PlatformError> {
        let identity = self.identity(window, until)?;
        let raw = hwnd(window);
        popup_deadline(until)?;
        // SAFETY: own PID/TID/process corroborated before class/style/visibility/geometry fields.
        let style = unsafe { GetWindowLongPtrW(raw, GWL_STYLE) as u32 };
        self.identity(window, until)?;
        popup_deadline(until)?;
        // SAFETY: freshly creator/process-admitted own extended style only.
        let ex_style = unsafe { GetWindowLongPtrW(raw, GWL_EXSTYLE) as u32 };
        self.identity(window, until)?;
        popup_deadline(until)?;
        // SAFETY: freshly creator/process-admitted own visibility only.
        let visible = unsafe { IsWindowVisible(raw) != 0 };
        self.identity(window, until)?;
        popup_deadline(until)?;
        // SAFETY: freshly creator/process-admitted own iconic state only.
        let iconic = unsafe { IsIconic(raw) != 0 };
        let mut cloaked = 0_u32;
        let mut class = [0_u16; 256];
        if self.identity(window, until)? != identity {
            return Err(PlatformError::NotFound);
        }
        popup_deadline(until)?;
        // SAFETY: bounded own class buffer, no caption or content query.
        let length = unsafe { GetClassNameW(raw, class.as_mut_ptr(), class.len() as i32) };
        if length <= 0 {
            return Err(backend("popup class classification"));
        }
        if self.identity(window, until)? != identity {
            return Err(PlatformError::NotFound);
        }
        popup_deadline(until)?;
        // SAFETY: exact initialized u32 output for only the re-admitted own HWND.
        if unsafe {
            DwmGetWindowAttribute(
                raw,
                DWMWA_CLOAKED as u32,
                (&mut cloaked as *mut u32).cast(),
                size_of::<u32>() as u32,
            )
        } < 0
        {
            return Err(backend("popup visibility"));
        }
        let class = String::from_utf16_lossy(&class[..length as usize]);
        let kind = if matches!(class.as_str(), "IME" | "MSCTFIME UI" | "CiceroUIWndFrame") {
            popup::Kind::Ime
        } else if class == "#32770" || style & WS_CAPTION == WS_CAPTION {
            popup::Kind::Dialog
        } else if style & WS_CHILD == 0
            && (style & WS_POPUP != 0 || ex_style & WS_EX_TOOLWINDOW != 0)
        {
            popup::Kind::Popup
        } else {
            popup::Kind::Other
        };
        if self.identity(window, until)? != identity {
            return Err(PlatformError::NotFound);
        }
        let token = self
            .generations
            .observe(identity)
            .map_err(|_| backend("popup observed generation"))?;
        let mut owners = Vec::new();
        let mut seen = BTreeSet::from([window]);
        let mut link = window;
        for _ in 0..popup::OWNER_CAP {
            self.identity(link, until)?;
            popup_deadline(until)?;
            // SAFETY: ownership relationship of only an admitted same-PID/TID link.
            let owner = number(unsafe { GetWindow(hwnd(link), GW_OWNER) });
            if owner == 0 || !seen.insert(owner) {
                return Err(backend("popup owner chain"));
            }
            let owner_identity = self.identity(owner, until)?;
            let owner_token = if owner == root.identity.hwnd {
                if owner_identity != root.identity {
                    return Err(PlatformError::NotFound);
                }
                root
            } else {
                self.generations
                    .observe(owner_identity)
                    .map_err(|_| backend("popup observed generation"))?
            };
            owners.push(owner_token);
            if owner_token == root {
                break;
            }
            link = owner;
        }
        let geometry = self.geometry(identity, until)?;
        if self.identity(window, until)? != identity {
            return Err(PlatformError::NotFound);
        }
        Ok(popup::Candidate {
            token,
            owners,
            geometry,
            kind,
            visible: visible && !iconic && cloaked == 0,
            topmost: ex_style & WS_EX_TOPMOST != 0,
        })
    }

    pub(crate) fn snapshot(&mut self, until: Instant) -> Result<popup::Snapshot, PlatformError> {
        popup_deadline(until)?;
        let _dpi = ThreadDpi::physical()?;
        if !self.root_valid(until) {
            return Err(PlatformError::NotFound);
        }
        let root = popup::Token {
            identity: Identity {
                hwnd: self.root.hwnd,
                pid: self.root.pid,
                tid: self.root.tid,
                process_created: self.root.process_created,
            },
            generation: self.root.generation,
        };
        let geometry = self.geometry(root.identity, until)?;
        let mut snapshot = popup::Snapshot {
            root,
            geometry,
            popups: Vec::new(),
            refused: 0,
            reason: popup::Refusal::None,
        };
        if self.queue.fault.get() {
            snapshot.reason = popup::Refusal::Events;
            return Ok(snapshot);
        }
        for (event, window) in self.queue.events.borrow_mut().drain(..) {
            if matches!(
                event,
                winevent::EVENT_OBJECT_CREATE
                    | winevent::EVENT_OBJECT_DESTROY
                    | winevent::EVENT_OBJECT_HIDE
            ) {
                self.generations.retire(window);
            }
        }
        let mut enumeration = PopupEnumeration {
            root: self.root,
            windows: Vec::new(),
            overflow: false,
            until,
        };
        popup_deadline(until)?;
        // SAFETY: exact retained root's UI thread; synchronous callback reads creator metadata only.
        let complete = unsafe {
            EnumThreadWindows(
                self.root.tid,
                Some(popup_enumerate),
                (&mut enumeration as *mut PopupEnumeration) as LPARAM,
            )
        } != 0;
        if !complete || enumeration.overflow {
            snapshot.reason = popup::Refusal::Cap;
            snapshot.refused = 1;
            return Ok(snapshot);
        }
        let observed: BTreeSet<_> = enumeration.windows.iter().copied().collect();
        self.generations.retain(&observed);
        let mut candidates = Vec::new();
        for window in enumeration.windows {
            popup_deadline(until)?;
            if window == self.root.hwnd {
                continue;
            }
            if let Ok(candidate) = self.candidate(window, root, until)
                && candidate.kind == popup::Kind::Popup
                && candidate.visible
            {
                candidates.push(candidate);
            }
        }
        let (selected, refused, reason) = popup::select(root, geometry.content, candidates);
        snapshot.refused = refused;
        snapshot.reason = reason;
        let mut edges = Vec::new();
        let mut remaining = popup::STACK_CAP;
        for candidate in &selected {
            let mut current = candidate.token.identity.hwnd;
            let mut seen = BTreeSet::from([current]);
            while remaining != 0 {
                popup_deadline(until)?;
                remaining -= 1;
                // SAFETY: admitted start; subsequent handles only walk relationships/equality.
                // Foreign handles supply NO PID/class/style/geometry/title/content/logging data.
                let above = number(unsafe { GetWindow(hwnd(current), GW_HWNDPREV) });
                if above == 0 || above == self.root.hwnd {
                    break;
                }
                if !seen.insert(above) {
                    snapshot.reason = popup::Refusal::Stack;
                    return Ok(snapshot);
                }
                if selected.iter().any(|p| p.token.identity.hwnd == above) {
                    edges.push((candidate.token.identity.hwnd, above));
                }
                current = above;
            }
        }
        match popup::order(selected, &edges) {
            Ok(popups) => snapshot.popups = popups,
            Err(reason) => {
                snapshot.reason = reason;
                snapshot.refused += 1;
            }
        }
        // Corroborate every admitted identity, owner chain and geometry after z-order observation.
        for candidate in snapshot.popups.clone() {
            if self.candidate(candidate.token.identity.hwnd, root, until)? != candidate {
                snapshot.popups.clear();
                snapshot.reason = popup::Refusal::Stale;
                break;
            }
        }
        if !self.root_valid(until) || self.geometry(root.identity, until)? != geometry {
            snapshot.popups.clear();
            snapshot.reason = popup::Refusal::Stale;
        }
        popup_deadline(until)?;
        Ok(snapshot)
    }
}

#[cfg_attr(test, allow(dead_code))]
struct PopupEnumeration {
    root: NativeWindow,
    windows: Vec<u64>,
    overflow: bool,
    until: Instant,
}
#[cfg_attr(test, allow(dead_code))]
unsafe extern "system" fn popup_enumerate(window: HWND, parameter: LPARAM) -> i32 {
    // SAFETY: EnumThreadWindows synchronously borrows this exact live stack context.
    let enumeration = unsafe { &mut *(parameter as *mut PopupEnumeration) };
    if Instant::now() >= enumeration.until {
        enumeration.overflow = true;
        return 0;
    }
    let mut pid = 0;
    // SAFETY: creator metadata only, preceding all potential own-window fields.
    let tid = unsafe { GetWindowThreadProcessId(window, &mut pid) };
    if pid == enumeration.root.pid && tid == enumeration.root.tid {
        if enumeration.windows.len() == popup::CANDIDATE_CAP {
            enumeration.overflow = true;
            return 0;
        }
        enumeration.windows.push(number(window));
    }
    1
}

/// Pump only the capture worker's own message queue so its exact-PID/TID hooks can run.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn pump_popups() {
    pump();
}

#[cfg_attr(test, allow(dead_code))]
fn popup_deadline(until: Instant) -> Result<(), PlatformError> {
    if Instant::now() >= until {
        Err(PlatformError::Timeout)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(clippy::unwrap_used)]
    fn state() -> Arc<State> {
        Arc::new(State {
            admission: None,
            windows: Mutex::new(Windows::default()),
            updated: Mutex::new(Instant::now()),
            alive: AtomicBool::new(true),
            fault: AtomicBool::new(false),
            cleaned: AtomicBool::new(false),
            fields_open: AtomicBool::new(true),
        })
    }

    #[test]
    fn restricted_snapshot_changed_pid_refuses_before_process_image_or_fields() {
        let expected = Identity {
            hwnd: 7,
            pid: 8,
            tid: 9,
            process_created: 10,
        };
        let calls = RefCell::new(Vec::new());
        calls.borrow_mut().push("initial-owned-admission");
        let result = with_identity_owner(7, 88, 99, Some(expected), || {
            calls.borrow_mut().push("process-image");
            calls.borrow_mut().push("caption/class/fields");
            Ok(())
        });
        assert!(matches!(result, Err(PlatformError::NotFound)));
        assert_eq!(*calls.borrow(), ["initial-owned-admission"]);
    }

    #[test]
    fn restricted_pid_admission_precedes_all_fields_and_lifetime_checks() {
        use std::cell::RefCell;
        let calls = RefCell::new(Vec::new());
        let admitted = admitted_pid(
            || {
                calls.borrow_mut().push("pid");
                99
            },
            |pid| (pid == 7).then_some(()),
            |()| {
                calls.borrow_mut().push("process/creation/image");
                true
            },
        );
        if admitted {
            calls.borrow_mut().push("caption/class/fields");
        }
        assert_eq!(*calls.borrow(), ["pid"]);
        for alive_and_matching in [false, true] {
            calls.borrow_mut().clear();
            let admitted = admitted_pid(
                || {
                    calls.borrow_mut().push("pid");
                    7
                },
                |pid| (pid == 7).then_some(()),
                |()| {
                    calls.borrow_mut().push("process/creation/image");
                    alive_and_matching
                },
            );
            if admitted {
                calls.borrow_mut().push("caption/class/fields");
            }
            assert_eq!(
                calls.borrow().contains(&"caption/class/fields"),
                alive_and_matching
            );
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn window_callback_allowlist_and_object_filter_precede_any_field_api() {
        let state = state();
        let context = Arc::new(Context {
            raw: Mutex::new(VecDeque::new()),
            state,
            start: Instant::now(),
            admission: Some(Admission::Fixture {
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

    #[allow(clippy::unwrap_used)]
    fn admitted() -> (Arc<State>, WindowId, Identity) {
        use crosspane_types::{
            geom::{PointLogical, RectLogical, SizeLogical},
            id::DisplayId,
        };
        let shared = state();
        let identity = Identity {
            hwnd: 7,
            pid: 8,
            tid: 9,
            process_created: 10,
        };
        let mut table = shared.windows.lock().unwrap();
        table.observe(Observation {
            identity,
            title: "fixture".into(),
            app_id: "fixture.exe".into(),
            class: "fixture".into(),
            style: 0x00c0_0000,
            ex_style: 0,
            owner: None,
            root: true,
            visible: true,
            iconic: false,
            cloaked: false,
            current_desktop: Some(true),
            display: DisplayId(1),
            frame: RectLogical::new(PointLogical::new(0.0, 0.0), SizeLogical::new(10.0, 10.0)),
            fills_monitor: false,
        });
        let id = table.list()[0].id;
        drop(table);
        (shared, id, identity)
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn capture_resolver_requires_fresh_native_identity() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<WindowResolver>();
        let _: fn(&WindowResolver, WindowId) -> Option<NativeWindow> = WindowResolver::resolve;
        let _: fn(&WindowsWindowSource) -> WindowResolver = WindowsWindowSource::resolver;
        let (state, id, expected) = admitted();
        let resolver = WindowResolver { state };
        assert!(
            resolver
                .resolve_with(WindowId(id.0 ^ u64::MAX), |_| {
                    panic!("unadmitted ID must not query any native fields")
                })
                .is_none()
        );
        let good = resolver.resolve_with(id, Some).unwrap();
        assert_eq!(good.hwnd, expected.hwnd);
        assert_eq!(good.generation, id.0);
        for field in 0..3 {
            let mut changed = expected;
            match field {
                0 => changed.pid += 1,
                1 => changed.tid += 1,
                _ => changed.process_created += 1,
            }
            assert!(resolver.resolve_with(id, |_| Some(changed)).is_none());
        }
        assert!(resolver.resolve_with(id, |_| None).is_none());
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn capture_resolver_rechecks_live_generation_after_query() {
        let (state, id, _) = admitted();
        let resolver = WindowResolver {
            state: Arc::clone(&state),
        };
        assert!(
            resolver
                .resolve_with(id, |identity| {
                    state
                        .windows
                        .lock()
                        .unwrap()
                        .close(identity.hwnd, Some(identity));
                    Some(identity)
                })
                .is_none()
        );
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn capture_resolver_refuses_source_loss_stall_and_query_fault() {
        for variant in 0..3 {
            let (state, id, _) = admitted();
            let resolver = WindowResolver {
                state: Arc::clone(&state),
            };
            if variant == 0 {
                state.alive.store(false, Ordering::Release);
            }
            if variant == 1 {
                *state.updated.lock().unwrap() = Instant::now() - BOUND;
            }
            assert!(
                resolver
                    .resolve_with(id, |identity| {
                        if variant == 2 {
                            state.fault.store(true, Ordering::Release);
                        }
                        Some(identity)
                    })
                    .is_none()
            );
        }
    }
}
