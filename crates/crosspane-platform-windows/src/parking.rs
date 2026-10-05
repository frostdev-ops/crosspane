//! Windows M1: visible, in-place mirror parking with a separate durable native-tuple journal.
//! A bounded serial facade does not claim to cancel a synchronous native call already running.
//! After abandonment no further mutation is started; uncertain recovery evidence is retained.
#![allow(unsafe_code)]

pub use crate::model::parking::{MirrorJournalImages, MirrorJournalStore, MirrorRecovery};
use crate::{
    model::{
        geometry::DisplayIds,
        journal::Show,
        parking::{
            self, Controller, MirrorEntry, NativeIdentity, NativePort, Observed, RestoreOutcome,
        },
    },
    window::{MonitorReader, WindowResolver},
};
use crosspane_platform::{Parked, PlatformError, WindowParking};
use crosspane_types::{geom::PixelSize, id::WindowId};
use std::{
    fmt,
    mem::{size_of, size_of_val},
    ptr::null_mut,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
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
    Security::*,
    System::{StationsAndDesktops::*, Threading::*},
    UI::{HiDpi::*, WindowsAndMessaging::*},
};

const BOUND: Duration = Duration::from_secs(2);
fn backend(text: &'static str) -> PlatformError {
    PlatformError::Backend(text.into())
}
fn hwnd(identity: NativeIdentity) -> HWND {
    identity.hwnd as usize as HWND
}
fn rect(r: RECT) -> [i32; 4] {
    [r.left, r.top, r.right, r.bottom]
}
fn text(s: &[u16]) -> String {
    String::from_utf16_lossy(&s[..s.iter().position(|c| *c == 0).unwrap_or(s.len())])
}
fn with_restore_monitor(
    preflight: Result<(), PlatformError>,
    active: impl FnOnce() -> bool,
    restore: impl FnOnce() -> Result<Observed, PlatformError>,
) -> Result<RestoreOutcome, PlatformError> {
    preflight?;
    if !active() {
        return Ok(RestoreOutcome::MonitorGone);
    }
    restore().map(RestoreOutcome::Restored)
}
struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: each successful OpenProcess/OpenProcessToken handle has exactly one owner.
        unsafe { CloseHandle(self.0) };
    }
}
#[cfg(test)]
static FAIL_NEXT_DPI: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static DPI_READS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static DPI_MUTATIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static DPI_RESTORED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
#[allow(dead_code)] // Used by the separately compiled Limited fixture.
pub(crate) fn fixture_dpi_failure() {
    FAIL_NEXT_DPI.store(true, Ordering::Release);
}
#[cfg(test)]
#[allow(dead_code)] // Used by the separately compiled Limited fixture.
pub(crate) fn fixture_dpi_proof() -> (usize, usize, usize) {
    (
        DPI_READS.load(Ordering::Acquire),
        DPI_MUTATIONS.load(Ordering::Acquire),
        DPI_RESTORED.load(Ordering::Acquire),
    )
}
#[cfg(test)]
fn fixture_check_dpi(counter: &std::sync::atomic::AtomicUsize) -> Result<(), PlatformError> {
    // SAFETY: query only this probe's native worker thread context, no window/system change.
    if unsafe {
        AreDpiAwarenessContextsEqual(
            GetThreadDpiAwarenessContext(),
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        )
    } == 0
    {
        return Err(backend("owned mirror proof: PMv2 missing"));
    }
    counter.fetch_add(1, Ordering::AcqRel);
    Ok(())
}
/// PMv2 is scoped even before winit establishes process DPI behavior.
struct DpiScope(DPI_AWARENESS_CONTEXT);
impl DpiScope {
    fn new() -> Result<Self, PlatformError> {
        #[cfg(test)]
        if FAIL_NEXT_DPI.swap(false, Ordering::AcqRel) {
            return Err(backend("owned injected context failure; journal retained"));
        }
        // SAFETY: affects this dedicated parking thread only; the prior context is retained.
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        if previous.is_null() {
            return Err(backend("mirror PMv2 context unavailable; journal retained"));
        }
        Ok(Self(previous))
    }
}
impl Drop for DpiScope {
    fn drop(&mut self) {
        // SAFETY: restores exactly this thread's context captured by the successful setter.
        unsafe { SetThreadDpiAwarenessContext(self.0) };
        #[cfg(test)]
        // SAFETY: compares this same thread's actual restored context with its captured prior value.
        if unsafe { AreDpiAwarenessContextsEqual(GetThreadDpiAwarenessContext(), self.0) } != 0 {
            DPI_RESTORED.fetch_add(1, Ordering::AcqRel);
        }
    }
}
struct Desktop(Option<IVirtualDesktopManager>);
impl Desktop {
    fn new() -> Result<Self, PlatformError> {
        // SAFETY: initializes COM on the native owner thread; balanced by this RAII scope.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .map_err(|_| backend("mirror COM"))?;
        // SAFETY: documented read-only virtual-desktop manager, kept on this thread.
        match unsafe { CoCreateInstance(&VirtualDesktopManager, None, CLSCTX_INPROC_SERVER) } {
            Ok(manager) => Ok(Self(Some(manager))),
            Err(_) => {
                // SAFETY: balances the successful initialization on this thread.
                unsafe { CoUninitialize() };
                Err(backend("mirror virtual desktop query"))
            }
        }
    }
    fn contains(&self, window: HWND) -> bool {
        self.0.as_ref().is_some_and(|manager| {
            // SAFETY: read-only query after exact target identity admission; no desktop switch.
            unsafe {
                manager.IsWindowOnCurrentVirtualDesktop(windows::Win32::Foundation::HWND(window))
            }
            .is_ok_and(|value| value.as_bool())
        })
    }
}
impl Drop for Desktop {
    fn drop(&mut self) {
        self.0.take();
        // SAFETY: balances only this scope's successful initialization after the interface drops.
        unsafe { CoUninitialize() };
    }
}
fn desktop_name(desktop: HDESK) -> Option<String> {
    let mut name = [0_u16; 128];
    let mut needed = 0;
    // SAFETY: bounded read-only name query, no enumeration or content.
    (unsafe {
        GetUserObjectInformationW(
            desktop,
            UOI_NAME,
            name.as_mut_ptr().cast(),
            size_of_val(&name) as u32,
            &mut needed,
        )
    } != 0)
        .then(|| text(&name))
}
fn default_desktop() -> bool {
    // SAFETY: opens the input desktop for names only; never switches it.
    let input = unsafe { OpenInputDesktop(0, 0, DESKTOP_READOBJECTS) };
    if input.is_null() {
        return false;
    }
    // SAFETY: borrowed current-thread desktop handle is never closed.
    let current = unsafe { GetThreadDesktop(GetCurrentThreadId()) };
    let okay = desktop_name(input).as_deref() == Some("Default")
        && desktop_name(current).as_deref() == Some("Default");
    // SAFETY: closes only this owned name-query desktop handle.
    unsafe { CloseDesktop(input) };
    okay
}
fn integrity(process: HANDLE) -> Result<u32, PlatformError> {
    let mut token = null_mut();
    // SAFETY: read-only token query on the freshly pinned process.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(PlatformError::SecureInput);
    }
    let token = Handle(token);
    let mut needed = 0;
    // SAFETY: zero-byte size query writes only its length output.
    unsafe { GetTokenInformation(token.0, TokenIntegrityLevel, null_mut(), 0, &mut needed) };
    if needed < size_of::<TOKEN_MANDATORY_LABEL>() as u32 || needed > 4096 {
        return Err(PlatformError::SecureInput);
    }
    let mut buffer = vec![0_usize; (needed as usize).div_ceil(size_of::<usize>())];
    // SAFETY: aligned bounded token buffer, and SID pointers are checked to stay within it.
    unsafe {
        if GetTokenInformation(
            token.0,
            TokenIntegrityLevel,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        ) == 0
        {
            return Err(PlatformError::SecureInput);
        }
        let sid = (*(buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()))
            .Label
            .Sid;
        let begin = buffer.as_ptr() as usize;
        let end = begin + buffer.len() * size_of::<usize>();
        let address = sid as usize;
        if address < begin || address.checked_add(8).is_none_or(|p| p > end) {
            return Err(PlatformError::SecureInput);
        }
        let count = *GetSidSubAuthorityCount(sid);
        if count == 0
            || address
                .checked_add(8 + usize::from(count) * 4)
                .is_none_or(|p| p > end)
            || IsValidSid(sid) == 0
        {
            return Err(PlatformError::SecureInput);
        }
        Ok(*GetSidSubAuthority(sid, u32::from(count - 1)))
    }
}
struct Pinned {
    identity: NativeIdentity,
    process: Handle,
}
impl Pinned {
    /// None is only confirmed missing/dead/reused; denied/unknown query errors retain evidence.
    fn open(identity: NativeIdentity) -> Result<Option<Self>, PlatformError> {
        let window = hwnd(identity);
        let mut pid = 0;
        // SAFETY: exact persisted/current-source HWND, identity metadata only before fields.
        let tid = unsafe { GetWindowThreadProcessId(window, &mut pid) };
        if tid == 0 || pid == 0 {
            // SAFETY: existence-only read of this exact recorded handle.
            return if unsafe { IsWindow(window) } == 0 {
                Ok(None)
            } else {
                Err(backend("mirror target identity unknown"))
            };
        }
        if pid != identity.pid || tid != identity.tid {
            return Ok(None);
        }
        // SAFETY: metadata/lifetime rights only, target PID already matches the admitted tuple.
        let process = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        if process.is_null() {
            // SAFETY: immediate thread-local error from OpenProcess; invalid PID is confirmed gone.
            return if unsafe { GetLastError() } == ERROR_INVALID_PARAMETER {
                Ok(None)
            } else {
                Err(PlatformError::SecureInput)
            };
        }
        let pinned = Self {
            identity,
            process: Handle(process),
        };
        if !pinned.matches()? {
            return Ok(None);
        }
        // SAFETY: borrowed caller process pseudo-handle is queried but never closed.
        if integrity(pinned.process.0)? > integrity(unsafe { GetCurrentProcess() })? {
            return Err(PlatformError::SecureInput);
        }
        Ok(Some(pinned))
    }
    fn matches(&self) -> Result<bool, PlatformError> {
        // SAFETY: zero wait on the retained process proves only its lifetime, no input or mutation.
        match unsafe { WaitForSingleObject(self.process.0, 0) } {
            WAIT_OBJECT_0 => return Ok(false),
            WAIT_TIMEOUT => {}
            _ => return Err(backend("mirror process lifetime unknown")),
        }
        let mut created = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        // SAFETY: valid retained process and initialized FILETIME outputs.
        if unsafe {
            GetProcessTimes(
                self.process.0,
                &mut created,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        } == 0
        {
            return Err(backend("mirror process time unknown"));
        }
        let mut pid = 0;
        // SAFETY: exact admitted HWND identity query only.
        let tid = unsafe { GetWindowThreadProcessId(hwnd(self.identity), &mut pid) };
        Ok(pid == self.identity.pid
            && tid == self.identity.tid
            && ((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
                == self.identity.process_created)
    }
}
struct Binding {
    resolver: WindowResolver,
    ids: Arc<Mutex<DisplayIds>>,
    monitors: MonitorReader,
}
struct Shared {
    alive: AtomicBool,
    fault: AtomicBool,
}
struct Deadline {
    until: Instant,
    abandoned: Arc<AtomicBool>,
}
struct Port {
    binding: Option<Binding>,
    shared: Arc<Shared>,
    deadline: Option<Deadline>,
}
impl Port {
    fn verify_resolver(
        &mut self,
        identity: NativeIdentity,
        id: Option<WindowId>,
    ) -> Result<(), PlatformError> {
        if let Some(id) = id
            && self.resolve(id)? != identity
        {
            return Err(PlatformError::NotFound);
        }
        Ok(())
    }
    fn read(&mut self, pinned: &Pinned, id: Option<WindowId>) -> Result<Observed, PlatformError> {
        self.check()?;
        self.verify_resolver(pinned.identity, id)?;
        if !default_desktop() {
            return Err(PlatformError::SecureInput);
        }
        let desktop = Desktop::new()?;
        let window = hwnd(pinned.identity);
        if !desktop.contains(window) {
            return Err(PlatformError::SecureInput);
        }
        #[cfg(test)]
        fixture_check_dpi(&DPI_READS)?;
        let mut outer = RECT::default();
        let mut visible = RECT::default();
        let mut cloaked = 0_u32;
        // SAFETY: identity/integrity/default/current-desktop admitted target, exact initialized buffers.
        if unsafe { GetWindowRect(window, &mut outer) } == 0
            // SAFETY: exact RECT output buffer for the same identity-admitted target.
            || unsafe {
                DwmGetWindowAttribute(
                    window,
                    DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                    (&mut visible as *mut RECT).cast(),
                    size_of::<RECT>() as u32,
                )
            } < 0
            // SAFETY: exact u32 output buffer for the same identity-admitted target.
            || unsafe {
                DwmGetWindowAttribute(
                    window,
                    DWMWA_CLOAKED as u32,
                    (&mut cloaked as *mut u32).cast(),
                    size_of::<u32>() as u32,
                )
            } < 0
        {
            return Err(backend("mirror physical geometry unknown"));
        }
        parking::rect_size(rect(outer))?;
        parking::rect_size(rect(visible))?;
        let mut monitor = MONITORINFOEXW::default();
        monitor.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        // SAFETY: read-only monitor facts for the exact current target, fixed extended output structure.
        if unsafe {
            GetMonitorInfoW(
                MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST),
                &mut monitor.monitorInfo,
            )
        } == 0
        {
            return Err(backend("mirror monitor unknown"));
        }
        // SAFETY: read-only target state/awareness queries after tuple and integrity admission.
        let (eligible, show, dpi, awareness, style) = unsafe {
            (
                IsWindowVisible(window) != 0 && IsIconic(window) == 0 && cloaked == 0,
                if IsZoomed(window) != 0 {
                    Show::Maximized
                } else {
                    Show::Normal
                },
                GetDpiForWindow(window),
                GetAwarenessFromDpiAwarenessContext(GetWindowDpiAwarenessContext(window)),
                GetWindowLongPtrW(window, GWL_STYLE) as u32,
            )
        };
        if dpi == 0 || awareness == DPI_AWARENESS_INVALID {
            return Err(backend("mirror target DPI unknown"));
        }
        let fullscreen =
            rect(visible) == rect(monitor.monitorInfo.rcMonitor) && style & WS_CAPTION == 0;
        let (geometry, monitor_path) = if let Some(id) = id {
            let binding = self
                .binding
                .as_ref()
                .ok_or(PlatformError::Unsupported("unbound Windows mirror parking"))?;
            let probes = (binding.monitors)()?;
            let (geometry, path) = parking::actual_geometry(
                id,
                rect(visible),
                &text(&monitor.szDevice),
                rect(monitor.monitorInfo.rcMonitor),
                &probes,
                &mut *binding
                    .ids
                    .lock()
                    .map_err(|_| backend("mirror display identities"))?,
                fullscreen,
            )?;
            (Some(geometry), path)
        } else {
            // Startup has no opaque WindowId or display allocator. No synthetic Parked is made.
            (None, String::new())
        };
        if !pinned.matches()? {
            return Err(PlatformError::NotFound);
        }
        self.verify_resolver(pinned.identity, id)?;
        self.check()?;
        Ok(Observed {
            outer: rect(outer),
            visible: rect(visible),
            monitor_path,
            show,
            dpi,
            eligible,
            fullscreen,
            geometry,
        })
    }
    fn preflight(
        &mut self,
        identity: NativeIdentity,
        show: Show,
        id: Option<WindowId>,
    ) -> Result<(Pinned, Observed), PlatformError> {
        self.check()?;
        self.verify_resolver(identity, id)?;
        let pinned = Pinned::open(identity)?.ok_or(PlatformError::NotFound)?;
        let before = self.read(&pinned, id)?;
        if !before.eligible {
            return Err(PlatformError::Locked);
        }
        if before.show != show {
            return Err(PlatformError::Unsupported(
                "Windows mirror show-state restoration",
            ));
        }
        Ok((pinned, before))
    }
    fn move_checked(
        &mut self,
        identity: NativeIdentity,
        outer: [i32; 4],
        show: Show,
        id: Option<WindowId>,
    ) -> Result<Observed, PlatformError> {
        let (pinned, before) = self.preflight(identity, show, id)?;
        let size = parking::rect_size(outer)?;
        // Queries can reenter native code. The fresh resolver, token and cancellation fences are
        // repeated immediately before the synchronous call. There is no ASYNCWINDOWPOS fallback.
        self.verify_resolver(identity, id)?;
        if !pinned.matches()? {
            return Err(PlatformError::NotFound);
        }
        // SAFETY: fresh target token and caller token queries only, no privilege change.
        if integrity(pinned.process.0)? > integrity(unsafe { GetCurrentProcess() })?
            || !default_desktop()
        {
            return Err(PlatformError::SecureInput);
        }
        self.check()?;
        #[cfg(test)]
        fixture_check_dpi(&DPI_MUTATIONS)?;
        // SAFETY: exact freshly admitted tuple, durable original/phase already published. Preserve
        // activation/z-order/owner order; physical PMv2 rectangle, no show/style/desktop change.
        if unsafe {
            SetWindowPos(
                hwnd(identity),
                null_mut(),
                outer[0],
                outer[1],
                size.width as i32,
                size.height as i32,
                SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOOWNERZORDER,
            )
        } == 0
        {
            return Err(backend("mirror window resize refused; journal retained"));
        }
        self.check()?;
        let actual = self.read(&pinned, id)?;
        if !actual.eligible {
            return Err(PlatformError::Locked);
        }
        if id.is_some()
            && show == Show::Normal
            && outer[0] == before.outer[0]
            && outer[1] == before.outer[1]
            && actual.visible[..2] != before.visible[..2]
        {
            return Err(backend(
                "mirror resize origin not preserved; journal retained",
            ));
        }
        Ok(actual)
    }
}
impl NativePort for Port {
    fn check(&self) -> Result<(), PlatformError> {
        if !self.shared.alive.load(Ordering::Acquire) || self.shared.fault.load(Ordering::Acquire) {
            return Err(backend("mirror owner unavailable; journal retained"));
        }
        if self
            .deadline
            .as_ref()
            .is_some_and(|d| d.abandoned.load(Ordering::Acquire) || Instant::now() >= d.until)
        {
            return Err(PlatformError::Timeout);
        }
        Ok(())
    }
    fn resolve(&mut self, id: WindowId) -> Result<NativeIdentity, PlatformError> {
        self.check()?;
        let resolved = self
            .binding
            .as_ref()
            .ok_or(PlatformError::Unsupported("unbound Windows mirror parking"))?
            .resolver
            .resolve(id)
            .ok_or(PlatformError::NotFound)?;
        self.check()?;
        Ok(NativeIdentity {
            hwnd: resolved.hwnd,
            pid: resolved.pid,
            tid: resolved.tid,
            process_created: resolved.process_created,
        })
    }
    fn inspect(
        &mut self,
        identity: NativeIdentity,
        id: Option<WindowId>,
    ) -> Result<Option<Observed>, PlatformError> {
        self.check()?;
        // Confirm native retirement before runtime resolver checks, whose source can have closed.
        let Some(pinned) = Pinned::open(identity)? else {
            return Ok(None);
        };
        self.read(&pinned, id).map(Some)
    }
    fn resize(
        &mut self,
        identity: NativeIdentity,
        outer: [i32; 4],
        id: WindowId,
    ) -> Result<Observed, PlatformError> {
        self.move_checked(identity, outer, Show::Normal, Some(id))
    }
    fn restore(
        &mut self,
        entry: &MirrorEntry,
        id: Option<WindowId>,
    ) -> Result<RestoreOutcome, PlatformError> {
        self.check()?;
        let r = entry.original.rect_physical;
        let original = RECT {
            left: r[0],
            top: r[1],
            right: r[2],
            bottom: r[3],
        };
        // Publication may block/reenter. Strict identity/UIPI/desktop/show admission is fresh
        // before monitor absence can become a pending result; mutation repeats admission below.
        let preflight = self
            .preflight(entry.identity, entry.original.show, id)
            .map(|_| ());
        with_restore_monitor(
            preflight,
            || {
                // SAFETY: canonical validated physical original RECT under scoped PMv2.
                // Read-only active-monitor intersection creates no source, observer or display ID.
                !unsafe { MonitorFromRect(&original, MONITOR_DEFAULTTONULL) }.is_null()
            },
            || self.move_checked(entry.identity, r, entry.original.show, id),
        )
    }
}
enum Operation {
    Bind(Binding),
    Startup,
    Park(WindowId, PixelSize, f64),
    Resize(WindowId, PixelSize, f64),
    Geometry(WindowId),
    Fullscreen(WindowId, bool),
    Restore(WindowId),
    Recover,
}
enum Reply {
    Unit,
    Geometry(Parked),
    Recovery(MirrorRecovery),
    Windows(Vec<WindowId>),
}
struct Call {
    deadline: Deadline,
    reply: mpsc::SyncSender<Result<Reply, PlatformError>>,
    operation: Operation,
}
/// Send facade to one serial owner. Native handle objects never leave that thread.
pub struct WindowsMirrorParking {
    shared: Arc<Shared>,
    commands: mpsc::SyncSender<Call>,
    done: mpsc::Receiver<()>,
    worker: Option<JoinHandle<()>>,
}
impl fmt::Debug for WindowsMirrorParking {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WindowsMirrorParking")
            .finish_non_exhaustive()
    }
}
impl WindowsMirrorParking {
    pub fn new(store: Box<dyn MirrorJournalStore>) -> Result<Self, PlatformError> {
        let shared = Arc::new(Shared {
            alive: AtomicBool::new(true),
            fault: AtomicBool::new(false),
        });
        let port = Port {
            shared: shared.clone(),
            binding: None,
            deadline: None,
        };
        // Journal reads/republishing are pure/private IO. No DPI, source, capture or hook exists yet.
        let mut controller = Controller::new(store, port)?;
        let (commands, receive) = mpsc::sync_channel::<Call>(1);
        let (finished, done) = mpsc::sync_channel(1);
        let worker_shared = shared.clone();
        let worker = thread::Builder::new()
            .name("crosspane-mirror".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    while worker_shared.alive.load(Ordering::Acquire)
                        && !worker_shared.fault.load(Ordering::Acquire)
                    {
                        let call = match receive.recv_timeout(Duration::from_millis(25)) {
                            Ok(c) => c,
                            Err(mpsc::RecvTimeoutError::Timeout) => continue,
                            Err(_) => break,
                        };
                        controller.port.deadline = Some(call.deadline);
                        let result = controller.port.check().and_then(|()| {
                            if let Operation::Bind(binding) = call.operation {
                                if controller.port.binding.is_some() {
                                    return Err(backend("mirror source already bound"));
                                }
                                controller.port.binding = Some(binding);
                                controller.bind();
                                return Ok(Reply::Unit);
                            }
                            let _dpi = DpiScope::new()?;
                            match call.operation {
                                Operation::Startup => {
                                    controller.recover_startup().map(Reply::Recovery)
                                }
                                Operation::Park(id, size, scale) => {
                                    controller.park(id, size, scale).map(Reply::Geometry)
                                }
                                Operation::Resize(id, size, scale) => {
                                    controller.resize(id, size, scale).map(Reply::Geometry)
                                }
                                Operation::Geometry(id) => {
                                    controller.geometry(id).map(Reply::Geometry)
                                }
                                Operation::Fullscreen(id, on) => {
                                    controller.set_fullscreen(id, on).map(|()| Reply::Unit)
                                }
                                Operation::Restore(id) => {
                                    controller.restore(id).map(|()| Reply::Unit)
                                }
                                Operation::Recover => controller.recover().map(Reply::Windows),
                                Operation::Bind(_) => unreachable!(),
                            }
                        });
                        let _ = call.reply.send(result);
                        controller.port.deadline = None;
                    }
                }));
                if result.is_err() {
                    worker_shared.fault.store(true, Ordering::Release);
                }
                worker_shared.alive.store(false, Ordering::Release);
                let _ = finished.send(());
            })
            .map_err(|_| backend("mirror owner thread"))?;
        Ok(Self {
            shared,
            commands,
            done,
            worker: Some(worker),
        })
    }
    fn call(&self, operation: Operation) -> Result<Reply, PlatformError> {
        if !self.shared.alive.load(Ordering::Acquire) || self.shared.fault.load(Ordering::Acquire) {
            return Err(backend("mirror owner unavailable"));
        }
        let until = Instant::now() + BOUND;
        let abandoned = Arc::new(AtomicBool::new(false));
        let (reply, receive) = mpsc::sync_channel(1);
        self.commands
            .try_send(Call {
                deadline: Deadline {
                    until,
                    abandoned: abandoned.clone(),
                },
                reply,
                operation,
            })
            .map_err(|_| backend("mirror owner busy"))?;
        match receive.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok(result) => result,
            Err(_) => {
                abandoned.store(true, Ordering::Release);
                self.shared.fault.store(true, Ordering::Release);
                Err(PlatformError::Timeout)
            }
        }
    }
    pub fn recover_startup(&mut self) -> Result<MirrorRecovery, PlatformError> {
        match self.call(Operation::Startup)? {
            Reply::Recovery(r) => Ok(r),
            _ => Err(backend("mirror reply")),
        }
    }
    pub fn bind_source(
        &mut self,
        resolver: WindowResolver,
        ids: Arc<Mutex<DisplayIds>>,
        monitors: MonitorReader,
    ) -> Result<(), PlatformError> {
        match self.call(Operation::Bind(Binding {
            resolver,
            ids,
            monitors,
        }))? {
            Reply::Unit => Ok(()),
            _ => Err(backend("mirror reply")),
        }
    }
    fn geometry_reply(&self, op: Operation) -> Result<Parked, PlatformError> {
        match self.call(op)? {
            Reply::Geometry(g) => Ok(g),
            _ => Err(backend("mirror reply")),
        }
    }
}
impl WindowParking for WindowsMirrorParking {
    fn park(
        &mut self,
        window: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError> {
        self.geometry_reply(Operation::Park(window, size, scale))
    }
    fn resize(
        &mut self,
        window: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError> {
        self.geometry_reply(Operation::Resize(window, size, scale))
    }
    fn geometry(&self, window: WindowId) -> Result<Parked, PlatformError> {
        self.geometry_reply(Operation::Geometry(window))
    }
    fn set_fullscreen(&mut self, window: WindowId, on: bool) -> Result<(), PlatformError> {
        match self.call(Operation::Fullscreen(window, on))? {
            Reply::Unit => Ok(()),
            _ => Err(backend("mirror reply")),
        }
    }
    fn restore(&mut self, window: WindowId) -> Result<(), PlatformError> {
        match self.call(Operation::Restore(window))? {
            Reply::Unit => Ok(()),
            _ => Err(backend("mirror reply")),
        }
    }
    fn recover(&mut self) -> Result<Vec<WindowId>, PlatformError> {
        match self.call(Operation::Recover)? {
            Reply::Windows(w) => Ok(w),
            _ => Err(backend("mirror reply")),
        }
    }
}
impl Drop for WindowsMirrorParking {
    fn drop(&mut self) {
        self.shared.alive.store(false, Ordering::Release);
        if let Some(worker) = self.worker.take()
            && self.done.recv_timeout(BOUND).is_ok()
        {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_original_monitor_never_invokes_restore_mutation() {
        let calls = std::cell::Cell::new(0);
        let result = with_restore_monitor(
            Ok(()),
            || false,
            || {
                calls.set(calls.get() + 1);
                Err(PlatformError::SecureInput)
            },
        );
        assert!(matches!(result, Ok(RestoreOutcome::MonitorGone)));
        assert_eq!(calls.get(), 0);
        let result = with_restore_monitor(
            Ok(()),
            || true,
            || {
                calls.set(calls.get() + 1);
                Err(PlatformError::SecureInput)
            },
        );
        assert!(matches!(result, Err(PlatformError::SecureInput)));
        assert_eq!(calls.get(), 1);
    }
    #[test]
    fn strict_preflight_failure_precedes_even_an_absent_monitor_query() {
        let monitor_calls = std::cell::Cell::new(0);
        let mutation_calls = std::cell::Cell::new(0);
        let result = with_restore_monitor(
            Err(PlatformError::SecureInput),
            || {
                monitor_calls.set(monitor_calls.get() + 1);
                false
            },
            || {
                mutation_calls.set(mutation_calls.get() + 1);
                unreachable!("strict preflight failure must precede mutation")
            },
        );
        assert!(matches!(result, Err(PlatformError::SecureInput)));
        assert_eq!(monitor_calls.get(), 0);
        assert_eq!(mutation_calls.get(), 0);
    }
    #[test]
    #[allow(clippy::unwrap_used)]
    fn facade_timeout_abandons_queue_and_poison_prevents_a_second_native_request() {
        let shared = Arc::new(Shared {
            alive: AtomicBool::new(true),
            fault: AtomicBool::new(false),
        });
        let (commands, receive) = mpsc::sync_channel::<Call>(1);
        let (_, done) = mpsc::channel();
        let facade = WindowsMirrorParking {
            shared: shared.clone(),
            commands,
            done,
            worker: None,
        };
        let (finished, observe) = mpsc::channel();
        let worker = thread::spawn(move || {
            let call = receive.recv().unwrap();
            observe
                .recv_timeout(BOUND + Duration::from_secs(1))
                .unwrap();
            assert!(call.deadline.abandoned.load(Ordering::Acquire));
            assert!(Instant::now() >= call.deadline.until);
            assert!(receive.try_recv().is_err());
        });
        assert!(matches!(
            facade.call(Operation::Recover),
            Err(PlatformError::Timeout)
        ));
        assert!(shared.fault.load(Ordering::Acquire));
        assert!(facade.call(Operation::Recover).is_err());
        finished.send(()).unwrap();
        worker.join().unwrap();
    }
}
