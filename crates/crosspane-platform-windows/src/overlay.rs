//! Owned, click-through Windows HUDs. No agent wiring, input injection or window activation.
//!
//! `[E]` Layered + TRANSPARENT passes mouse input underneath; NOACTIVATE and SWP_NOACTIVATE
//! preserve focus: <https://learn.microsoft.com/en-us/windows/win32/winmsg/window-features>.
//! `[E]` Public virtual-desktop APIs move only these owned windows:
//! <https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nn-shobjidl_core-ivirtualdesktopmanager>.
//! Lead amendment: follow the current virtual desktop rather than private pinning. A desktop
//! notification accelerates a 100 ms poll; a switch can briefly hide the HUD. Any failed move,
//! cloaking or presentation loss emits Unavailable. No foreign title or content is queried.
//! `[U]` EVENT_SYSTEM_DESKTOPSWITCH is an input-desktop notification, not a documented virtual
//! desktop notification; the poll is required for virtual desktops. Secure desktops are never
//! entered. Arbitrary topmost-window occlusion cannot be proven by IsWindowVisible/DWM.
//! `[P]` A stalled native call retires evidence at 500 ms; the worker retains its resources and
//! destroys its owned windows when that call returns. Drop waits at most two seconds.
//! `[U]` Probe artifacts are the exact premultiplied SOURCE BUFFER supplied to
//! UpdateLayeredWindow, paired with native window checks; they are not on-screen captures.

#![allow(unsafe_code)]

use std::cell::Cell;
use std::collections::BTreeMap;
use std::fmt;
use std::mem::{size_of, size_of_val};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crosspane_platform::{EventSink, Overlay, OverlayEvent, OverlayHost, OverlayId, PlatformError};
use crosspane_types::{display::DisplayInfo, id::DisplayId};
use windows::Win32::Foundation::HWND as ComHwnd;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize,
};
use windows::Win32::UI::Shell::{IVirtualDesktopManager, VirtualDesktopManager};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::{Dwm::*, Gdi::*};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::{Accessibility::*, HiDpi::*, WindowsAndMessaging::*};

use crate::model::geometry::{DisplayIds, MonitorProbe, displays};
use crate::model::overlay::{
    OverlayModel, Placement, compose, follow_current, placement, text_line,
};

const POLL: Duration = Duration::from_millis(100);
const QUEUE: usize = 32;
const STYLE: u32 =
    WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;

fn error(code: &'static str) -> PlatformError {
    PlatformError::Backend(code.into())
}
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

#[derive(Clone)]
struct Monitor {
    probe: MonitorProbe,
    display: DisplayInfo,
}

fn monitors(probes: &[MonitorProbe], ids: &mut DisplayIds) -> Result<Vec<Monitor>, PlatformError> {
    if probes.is_empty() || probes.len() > 32 {
        return Err(error("invalid overlay monitor count"));
    }
    let layout = displays(probes, ids).map_err(|_| error("invalid overlay display layout"))?;
    let mut sorted: Vec<_> = probes.iter().filter(|p| !p.twin).cloned().collect();
    sorted.sort_by(|a, b| a.device_path.cmp(&b.device_path));
    Ok(sorted
        .into_iter()
        .zip(layout.displays)
        .map(|(probe, display)| Monitor { probe, display })
        .collect())
}

struct Data {
    model: OverlayModel,
    sink: Option<Arc<dyn EventSink<OverlayEvent>>>,
    monitors: Vec<Monitor>,
}

struct Shared {
    data: Mutex<Data>,
    emission: Mutex<()>,
    stopped: AtomicBool,
    epoch: Instant,
}

impl Shared {
    fn data(&self) -> MutexGuard<'_, Data> {
        self.data.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn now(&self) -> u64 {
        self.epoch.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    }
    fn send(&self, sink: Option<Arc<dyn EventSink<OverlayEvent>>>, events: Vec<OverlayEvent>) {
        if let Some(sink) = sink {
            for event in events {
                if catch_unwind(AssertUnwindSafe(|| sink.send(event))).is_err() {
                    self.stopped.store(true, Ordering::Release);
                    self.data().sink = None;
                    break;
                }
            }
        }
    }
    fn observe(&self, id: OverlayId, generation: u64, visible: bool) {
        let _emission = self.emission.lock().unwrap_or_else(|e| e.into_inner());
        if self.stopped.load(Ordering::Acquire) {
            return;
        }
        let mut data = self.data();
        let events = data.model.observe(id, generation, visible, self.now());
        let sink = data.sink.clone();
        drop(data);
        self.send(sink, events);
    }
    fn expire(&self) {
        let _emission = self.emission.lock().unwrap_or_else(|e| e.into_inner());
        let mut data = self.data();
        let events = data.model.expire(self.now());
        let sink = data.sink.clone();
        drop(data);
        self.send(sink, events);
    }
    fn shutdown(&self) {
        self.stopped.store(true, Ordering::Release);
        let _emission = self.emission.lock().unwrap_or_else(|e| e.into_inner());
        let mut data = self.data();
        let events = data.model.shutdown();
        let sink = data.sink.take();
        drop(data);
        self.send(sink, events);
    }
}

enum Request {
    Show(OverlayId, Overlay, u64),
    Hide(OverlayId),
    Refresh,
    #[cfg(test)]
    SourceBuffer(OverlayId, mpsc::Sender<Result<Vec<u8>, PlatformError>>),
}

/// A Send handle; all HWNDs, GDI objects and COM interfaces stay on one PMv2 worker.
pub struct WindowsOverlay {
    shared: Arc<Shared>,
    commands: mpsc::SyncSender<Request>,
    done: mpsc::Receiver<()>,
    thread: Option<JoinHandle<()>>,
    watchdog: Option<JoinHandle<()>>,
}

impl fmt::Debug for WindowsOverlay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WindowsOverlay(..)")
    }
}

impl WindowsOverlay {
    /// Caller supplies the same monitor observations and retained ID allocator as DisplayHost.
    /// Native monitor name, bounds, work area and DPI are rechecked before presentation.
    pub fn new(probes: &[MonitorProbe], ids: &mut DisplayIds) -> Result<Self, PlatformError> {
        let shared = Arc::new(Shared {
            data: Mutex::new(Data {
                model: OverlayModel::default(),
                sink: None,
                monitors: monitors(probes, ids)?,
            }),
            emission: Mutex::new(()),
            stopped: AtomicBool::new(false),
            epoch: Instant::now(),
        });
        let (commands, receiver) = mpsc::sync_channel(QUEUE);
        let (ready, init) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let worker = shared.clone();
        let thread = thread::Builder::new()
            .name("windows-overlay".into())
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    let mut host = match Host::new() {
                        Ok(host) => host,
                        Err(e) => {
                            let _ = ready.send(Err(e));
                            return;
                        }
                    };
                    if ready.send(Ok(())).is_err() {
                        return;
                    }
                    host.run(&receiver, &worker);
                }));
                if result.is_err() {
                    eprintln!("crosspane-overlay: worker-failed");
                }
                worker.shutdown();
                let _ = finished.send(());
            })
            .map_err(|_| error("overlay worker unavailable"))?;
        match init.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                shared.shutdown();
                return Err(e);
            }
            Err(_) => {
                shared.shutdown();
                return Err(PlatformError::Timeout);
            }
        }
        let watch = shared.clone();
        let watchdog = match thread::Builder::new()
            .name("overlay-watchdog".into())
            .spawn(move || {
                while !watch.stopped.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(25));
                    watch.expire();
                }
            }) {
            Ok(thread) => thread,
            Err(_) => {
                shared.shutdown();
                return Err(error("overlay watchdog unavailable"));
            }
        };
        Ok(Self {
            shared,
            commands,
            done,
            thread: Some(thread),
            watchdog: Some(watchdog),
        })
    }

    /// Replaces caller observations without reassigning historical DisplayIds. Existing HUDs
    /// are reconfigured by the worker and must produce fresh presentation evidence.
    pub fn update_displays(
        &mut self,
        probes: &[MonitorProbe],
        ids: &mut DisplayIds,
    ) -> Result<(), PlatformError> {
        let monitors = monitors(probes, ids)?;
        let mut data = self.shared.data();
        if self.shared.stopped.load(Ordering::Acquire) {
            return Err(error("overlay host stopped"));
        }
        self.commands
            .try_send(Request::Refresh)
            .map_err(|_| error("overlay command queue unavailable"))?;
        data.monitors = monitors;
        Ok(())
    }
}

impl OverlayHost for WindowsOverlay {
    fn subscribe(&mut self, sink: Arc<dyn EventSink<OverlayEvent>>) -> Result<(), PlatformError> {
        let _emission = self
            .shared
            .emission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut data = self.shared.data();
        if self.shared.stopped.load(Ordering::Acquire) {
            return Err(error("overlay host stopped"));
        }
        if data.sink.is_some() {
            return Err(error("overlay subscription already installed"));
        }
        data.sink = Some(sink.clone());
        let events = data.model.replay();
        drop(data);
        self.shared.send(Some(sink), events);
        Ok(())
    }
    fn show(&mut self, id: OverlayId, overlay: &Overlay) -> Result<(), PlatformError> {
        let mut data = self.shared.data();
        if !data
            .monitors
            .iter()
            .any(|m| m.display.id == overlay.display)
        {
            return Err(PlatformError::NotFound);
        }
        let previous = data.model.clone();
        let generation = data.model.show(id, self.shared.now())?;
        let mut overlay = overlay.clone();
        overlay.text = text_line(&overlay.text);
        if self
            .commands
            .try_send(Request::Show(id, overlay, generation))
            .is_err()
        {
            data.model = previous;
            return Err(error("overlay command queue unavailable"));
        }
        Ok(())
    }
    fn hide(&mut self, id: OverlayId) -> Result<(), PlatformError> {
        let mut data = self.shared.data();
        if !data.model.contains(id) {
            return Ok(());
        }
        self.commands
            .try_send(Request::Hide(id))
            .map_err(|_| error("overlay command queue unavailable"))?;
        data.model.hide(id)
    }
}

impl Drop for WindowsOverlay {
    fn drop(&mut self) {
        self.shared.shutdown();
        if self.done.recv_timeout(Duration::from_secs(2)).is_ok()
            && let Some(thread) = self.thread.take()
        {
            let _ = thread.join();
        }
        if let Some(thread) = self.watchdog.take() {
            let _ = thread.join();
        }
    }
}

thread_local! { static CHANGED: Cell<bool> = const { Cell::new(false) }; }

unsafe extern "system" fn event(_: HWINEVENTHOOK, _: u32, _: HWND, _: i32, _: i32, _: u32, _: u32) {
    CHANGED.set(true);
}

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match message {
        WM_NCHITTEST => HTTRANSPARENT as LRESULT,
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_DISPLAYCHANGE | WM_DPICHANGED | WM_DESTROY => {
            CHANGED.set(true);
            0
        }
        WM_CLOSE => 0,
        // SAFETY: forwards the exact Win32 callback arguments to the default procedure.
        _ => unsafe { DefWindowProcW(hwnd, message, w, l) },
    }
}

struct Class {
    name: Vec<u16>,
    instance: HINSTANCE,
    hook: HWINEVENTHOOK,
    previous_dpi: DPI_AWARENESS_CONTEXT,
}
impl Class {
    fn new() -> Result<Self, PlatformError> {
        // SAFETY: changes only this owned worker's DPI context; no process/global setting.
        let previous_dpi =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        if previous_dpi.is_null() {
            return Err(error("PMv2 context unavailable"));
        }
        let mut class = Self {
            name: wide(&format!(
                "CrosspaneOwnedOverlay-{:?}",
                thread::current().id()
            )),
            instance: null_mut(),
            hook: null_mut(),
            previous_dpi,
        };
        // SAFETY: this process's module; class descriptor and UTF-16 name remain alive.
        unsafe {
            class.instance = GetModuleHandleW(null());
            let descriptor = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: class.instance,
                lpszClassName: class.name.as_ptr(),
                ..Default::default()
            };
            if class.instance.is_null() || RegisterClassW(&descriptor) == 0 {
                return Err(error("overlay class unavailable"));
            }
            class.hook = SetWinEventHook(
                EVENT_SYSTEM_DESKTOPSWITCH,
                EVENT_SYSTEM_DESKTOPSWITCH,
                null_mut(),
                Some(event),
                0,
                0,
                WINEVENT_OUTOFCONTEXT,
            );
        }
        if class.hook.is_null() {
            return Err(error("desktop observer unavailable"));
        }
        Ok(class)
    }
    fn window(&self, style: u32, position: Placement) -> Result<HWND, PlatformError> {
        // SAFETY: class is owned by this thread, no parent/owner or activation style is used.
        let hwnd = unsafe {
            CreateWindowExW(
                style,
                self.name.as_ptr(),
                wide("Crosspane HUD").as_ptr(),
                WS_POPUP,
                position.origin[0],
                position.origin[1],
                position.size[0],
                position.size[1],
                null_mut(),
                null_mut(),
                self.instance,
                null(),
            )
        };
        if hwnd.is_null() {
            Err(error("overlay window unavailable"))
        } else {
            Ok(hwnd)
        }
    }
}
impl Drop for Class {
    fn drop(&mut self) {
        // SAFETY: hook/class belong to this worker; all its windows were dropped first.
        unsafe {
            if !self.hook.is_null() {
                let _ = UnhookWinEvent(self.hook);
            }
            if !self.instance.is_null() {
                let _ = UnregisterClassW(self.name.as_ptr(), self.instance);
            }
            let _ = SetThreadDpiAwarenessContext(self.previous_dpi);
        }
    }
}

struct Apartment;
impl Apartment {
    fn new() -> Result<Self, PlatformError> {
        // SAFETY: initializes COM only on this newly owned thread; balanced by Drop.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok() }
            .map_err(|_| error("overlay COM unavailable"))?;
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: balances this thread's successful CoInitializeEx after COM objects drop.
        unsafe { CoUninitialize() };
    }
}

struct Window {
    hwnd: HWND,
    canvas: Box<Canvas>,
    position: Placement,
}
impl Window {
    fn matches_rect(&self) -> Result<bool, PlatformError> {
        let mut rect = RECT::default();
        // SAFETY: only this worker's live owned HWND and a correctly sized stack output.
        if unsafe { GetWindowRect(self.hwnd, &mut rect) } == 0 {
            return Err(error("overlay rectangle unavailable"));
        }
        let [x, y] = self.position.origin.map(i64::from);
        let [width, height] = self.position.size.map(i64::from);
        Ok(
            [rect.left, rect.top, rect.right, rect.bottom].map(i64::from)
                == [x, y, x + width, y + height],
        )
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: this thread owns hwnd; Canvas remains alive through native destruction.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

struct Entry {
    window: Window,
    overlay: Overlay,
    generation: u64,
}
struct Host {
    entries: BTreeMap<OverlayId, Entry>,
    manager: IVirtualDesktopManager,
    class: Class,
    _apartment: Apartment,
}
impl Host {
    fn new() -> Result<Self, PlatformError> {
        let apartment = Apartment::new()?;
        // SAFETY: public COM class on initialized owned apartment; retained on this thread only.
        let manager =
            unsafe { CoCreateInstance(&VirtualDesktopManager, None, CLSCTX_INPROC_SERVER) }
                .map_err(|_| error("virtual desktop manager unavailable"))?;
        Ok(Self {
            entries: BTreeMap::new(),
            manager,
            class: Class::new()?,
            _apartment: apartment,
        })
    }
    fn run(&mut self, receiver: &mpsc::Receiver<Request>, shared: &Shared) {
        let mut checked = Instant::now();
        while !shared.stopped.load(Ordering::Acquire) {
            for _ in 0..QUEUE {
                match receiver.try_recv() {
                    Ok(Request::Show(id, overlay, generation)) => {
                        if shared.data().model.current(id, generation) {
                            let success = self.show(id, &overlay, generation, shared).is_ok();
                            shared.observe(id, generation, success);
                        }
                    }
                    Ok(Request::Hide(id)) => {
                        self.entries.remove(&id);
                    }
                    Ok(Request::Refresh) => {
                        let requests: Vec<_> = self
                            .entries
                            .iter()
                            .map(|(id, e)| (*id, e.overlay.clone(), e.generation))
                            .collect();
                        for (id, overlay, generation) in requests {
                            let next = {
                                let mut data = shared.data();
                                if !data.model.current(id, generation) {
                                    continue;
                                }
                                data.model.show(id, shared.now())
                            };
                            let Ok(generation) = next else {
                                continue;
                            };
                            let success = self.show(id, &overlay, generation, shared).is_ok();
                            shared.observe(id, generation, success);
                        }
                    }
                    #[cfg(test)]
                    Ok(Request::SourceBuffer(id, reply)) => {
                        let result = self
                            .entries
                            .get(&id)
                            .ok_or(PlatformError::NotFound)
                            .and_then(|e| {
                                if !self.check(e, shared)? {
                                    return Err(error("owned overlay native checks failed"));
                                }
                                println!(
                                    "OWNED_NATIVE_EVIDENCE_{}=visible:true,cloaked:0,rect:matched,current-desktop:true,origin:{:?},size:{:?}",
                                    id.0, e.window.position.origin, e.window.position.size
                                );
                                e.window.source_buffer()
                            });
                        let _ = reply.send(result);
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return,
                }
            }
            let mut message = MSG::default();
            // SAFETY: pumps only this worker thread's messages; callback never unwinds.
            unsafe {
                for _ in 0..64 {
                    if PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) == 0 {
                        break;
                    }
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            if checked.elapsed() >= POLL || CHANGED.replace(false) {
                checked = Instant::now();
                self.entries
                    .retain(|id, entry| shared.data().model.current(*id, entry.generation));
                for (id, entry) in &self.entries {
                    let visible = self.check(entry, shared).unwrap_or(false);
                    if !visible {
                        eprintln!("crosspane-overlay: unavailable");
                    }
                    shared.observe(*id, entry.generation, visible);
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
    fn show(
        &mut self,
        id: OverlayId,
        overlay: &Overlay,
        generation: u64,
        shared: &Shared,
    ) -> Result<(), PlatformError> {
        let monitor = selected(shared, overlay.display)?;
        validate_monitor(&monitor.probe)?;
        let scale = monitor.display.geometry.scale;
        let measure = Canvas::new([1, 1], scale)?;
        let width = measure.measure(&overlay.text)? / scale;
        let position = placement(overlay, &monitor.probe, &monitor.display.geometry, width)?;
        let mut canvas = Box::new(Canvas::new(position.size, scale)?);
        canvas.draw(overlay, scale)?;
        let hwnd = self.class.window(STYLE, position)?;
        let window = Window {
            hwnd,
            canvas,
            position,
        };
        // SAFETY: own HWND and GDI resources stay alive; source DIB is premultiplied.
        unsafe {
            if GetDpiForWindow(hwnd) != monitor.probe.dpi {
                return Err(error("overlay DPI changed"));
            }
            let destination = POINT {
                x: position.origin[0],
                y: position.origin[1],
            };
            let size = SIZE {
                cx: position.size[0],
                cy: position.size[1],
            };
            let source = POINT::default();
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            if UpdateLayeredWindow(
                hwnd,
                null_mut(),
                &destination,
                &size,
                window.canvas.dc,
                &source,
                0,
                &blend,
                ULW_ALPHA,
            ) == 0
            {
                return Err(error("layered presentation failed"));
            }
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            if SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
            ) == 0
            {
                return Err(error("overlay placement failed"));
            }
        }
        self.follow(hwnd)?;
        // SAFETY: flushes only this application's queued DWM updates before visibility evidence.
        if unsafe { DwmFlush() } < 0 {
            return Err(error("overlay presentation flush failed"));
        }
        let entry = Entry {
            window,
            overlay: overlay.clone(),
            generation,
        };
        if !self.check(&entry, shared)? {
            return Err(error("overlay not presented"));
        }
        self.entries.insert(id, entry);
        Ok(())
    }
    fn follow(&self, hwnd: HWND) -> Result<(), PlatformError> {
        follow_current(
            || {
                // SAFETY: membership query uses only this worker's own live HWND.
                unsafe {
                    self.manager
                        .IsWindowOnCurrentVirtualDesktop(ComHwnd(hwnd))
                        .map(|current| current.as_bool())
                        .map_err(|_| error("desktop membership unavailable"))
                }
            },
            || {
                // SAFETY: foreground is queried for GUID only; move targets only our owned HWND.
                unsafe {
                    let foreground = GetForegroundWindow();
                    let desktop = if !foreground.is_null() {
                        self.manager.GetWindowDesktopId(ComHwnd(foreground)).ok()
                    } else {
                        None
                    };
                    let desktop = match desktop.filter(|id| *id != windows::core::GUID::zeroed()) {
                        Some(desktop) => desktop,
                        None => {
                            let helper = self.class.window(
                                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                                Placement {
                                    origin: [0, 0],
                                    size: [1, 1],
                                    scale: 1.0,
                                },
                            )?;
                            let result = self.manager.GetWindowDesktopId(ComHwnd(helper));
                            let _ = DestroyWindow(helper);
                            result.map_err(|_| error("current desktop unavailable"))?
                        }
                    };
                    if desktop == windows::core::GUID::zeroed() {
                        return Err(error("current desktop unavailable"));
                    }
                    self.manager
                        .MoveWindowToDesktop(ComHwnd(hwnd), &desktop)
                        .map_err(|_| error("owned desktop move failed"))?;
                    if DwmFlush() < 0 {
                        return Err(error("desktop presentation flush failed"));
                    }
                    Ok(())
                }
            },
        )
    }
    fn check(&self, entry: &Entry, shared: &Shared) -> Result<bool, PlatformError> {
        let monitor = selected(shared, entry.overlay.display)?;
        validate_monitor(&monitor.probe)?;
        self.follow(entry.window.hwnd)?;
        let mut cloaked: u32 = 0;
        // SAFETY: own live window, correctly sized stack output; no foreign capture or content.
        unsafe {
            if DwmGetWindowAttribute(
                entry.window.hwnd,
                DWMWA_CLOAKED as u32,
                (&mut cloaked as *mut u32).cast(),
                size_of_val(&cloaked) as u32,
            ) < 0
            {
                return Err(error("overlay cloaking unavailable"));
            }
            Ok(cloaked == 0
                && IsWindowVisible(entry.window.hwnd) != 0
                && GetDpiForWindow(entry.window.hwnd) == monitor.probe.dpi
                && entry.window.matches_rect()?)
        }
    }
}

fn selected(shared: &Shared, id: DisplayId) -> Result<Monitor, PlatformError> {
    shared
        .data()
        .monitors
        .iter()
        .find(|m| m.display.id == id)
        .cloned()
        .ok_or(PlatformError::NotFound)
}

unsafe extern "system" fn monitor_callback(
    handle: HMONITOR,
    _: HDC,
    _: *mut RECT,
    value: LPARAM,
) -> i32 {
    // SAFETY: EnumDisplayMonitors invokes synchronously; value points to the caller's live vector.
    unsafe {
        let mut info = MONITORINFOEXW {
            monitorInfo: MONITORINFO {
                cbSize: size_of::<MONITORINFOEXW>() as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        if GetMonitorInfoW(handle, (&mut info as *mut MONITORINFOEXW).cast()) == 0 {
            return 0;
        }
        let observations = &mut *(value as *mut Vec<MONITORINFOEXW>);
        if observations.len() >= 32 {
            return 0;
        }
        observations.push(info);
    }
    1
}

fn validate_monitor(probe: &MonitorProbe) -> Result<(), PlatformError> {
    let mut observed = Vec::<MONITORINFOEXW>::new();
    // SAFETY: callback borrows only this stack vector during the synchronous enumeration.
    if unsafe {
        EnumDisplayMonitors(
            null_mut(),
            null(),
            Some(monitor_callback),
            (&mut observed as *mut Vec<_>) as isize,
        )
    } == 0
    {
        return Err(error("monitor enumeration unavailable"));
    }
    let found = observed.iter().any(|info| {
        let length = info
            .szDevice
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(info.szDevice.len());
        let m = info.monitorInfo.rcMonitor;
        let w = info.monitorInfo.rcWork;
        String::from_utf16_lossy(&info.szDevice[..length]) == probe.name
            && [m.left, m.top, m.right, m.bottom] == probe.rc_monitor
            && [w.left, w.top, w.right, w.bottom] == probe.rc_work
    });
    if found {
        Ok(())
    } else {
        Err(PlatformError::NotFound)
    }
}

struct Canvas {
    dc: HDC,
    bitmap: HBITMAP,
    font: HFONT,
    old_bitmap: HGDIOBJ,
    old_font: HGDIOBJ,
    bits: *mut u8,
    size: [i32; 2],
    length: usize,
}
impl Canvas {
    fn new(size: [i32; 2], scale: f64) -> Result<Self, PlatformError> {
        let length = i64::from(size[0])
            .checked_mul(i64::from(size[1]))
            .and_then(|n| n.checked_mul(4))
            .filter(|n| *n > 0 && *n <= 16 * 1024 * 1024)
            .ok_or_else(|| error("overlay bitmap out of bounds"))? as usize;
        let mut result = Self {
            dc: null_mut(),
            bitmap: null_mut(),
            font: null_mut(),
            old_bitmap: null_mut(),
            old_font: null_mut(),
            bits: null_mut(),
            size,
            length,
        };
        // SAFETY: private memory DC/DIB; bounded top-down 32bpp dimensions and initialized output.
        unsafe {
            result.dc = CreateCompatibleDC(null_mut());
            if result.dc.is_null() {
                return Err(error("overlay DC unavailable"));
            }
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: size[0],
                    biHeight: -size[1],
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..Default::default()
                },
                ..Default::default()
            };
            result.bitmap = CreateDIBSection(
                result.dc,
                &info,
                DIB_RGB_COLORS,
                (&mut result.bits as *mut *mut u8).cast(),
                null_mut(),
                0,
            );
            if result.bitmap.is_null() || result.bits.is_null() {
                return Err(error("overlay DIB unavailable"));
            }
            result.old_bitmap = SelectObject(result.dc, result.bitmap);
            result.font = CreateFontW(
                -(14.0 * scale).round() as i32,
                0,
                0,
                0,
                FW_NORMAL as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                ANTIALIASED_QUALITY as u32,
                DEFAULT_PITCH as u32,
                wide("Segoe UI").as_ptr(),
            );
            if result.font.is_null() {
                return Err(error("overlay font unavailable"));
            }
            result.old_font = SelectObject(result.dc, result.font);
            if result.old_bitmap.is_null() || result.old_font.is_null() {
                return Err(error("overlay GDI selection failed"));
            }
            std::slice::from_raw_parts_mut(result.bits, length).fill(0);
        }
        Ok(result)
    }
    fn measure(&self, text: &str) -> Result<f64, PlatformError> {
        let text: Vec<_> = text.encode_utf16().collect();
        let mut size = SIZE::default();
        // SAFETY: owned DC/font, valid bounded UTF-16 slice and sized output.
        if unsafe { GetTextExtentPoint32W(self.dc, text.as_ptr(), text.len() as i32, &mut size) }
            == 0
        {
            return Err(error("overlay text measurement failed"));
        }
        Ok(f64::from(size.cx))
    }
    fn draw(&mut self, overlay: &Overlay, scale: f64) -> Result<(), PlatformError> {
        let text: Vec<_> = overlay.text.encode_utf16().collect();
        let mut rect = RECT {
            left: (24.0 * scale).round() as i32,
            top: 0,
            right: self.size[0] - (12.0 * scale).round() as i32,
            bottom: self.size[1],
        };
        // SAFETY: draws only into the owned initialized DIB; flush precedes reading GDI output.
        unsafe {
            if SetBkMode(self.dc, TRANSPARENT as i32) == 0
                || SetTextColor(self.dc, 0x00ff_ffff) == CLR_INVALID
            {
                return Err(error("overlay text setup failed"));
            }
            if !text.is_empty()
                && DrawTextW(
                    self.dc,
                    text.as_ptr(),
                    text.len() as i32,
                    &mut rect,
                    DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
                ) == 0
            {
                return Err(error("overlay text draw failed"));
            }
            if GdiFlush() == 0 {
                return Err(error("overlay GDI flush failed"));
            }
            let pixels = std::slice::from_raw_parts_mut(self.bits, self.length);
            compose(pixels, self.size, scale, overlay.accent)?;
        }
        Ok(())
    }
}
impl Drop for Canvas {
    fn drop(&mut self) {
        // SAFETY: restores selections before deleting only owned GDI resources.
        unsafe {
            if !self.dc.is_null() {
                if !self.old_font.is_null() {
                    let _ = SelectObject(self.dc, self.old_font);
                }
                if !self.old_bitmap.is_null() {
                    let _ = SelectObject(self.dc, self.old_bitmap);
                }
            }
            if !self.font.is_null() {
                let _ = DeleteObject(self.font);
            }
            if !self.bitmap.is_null() {
                let _ = DeleteObject(self.bitmap);
            }
            if !self.dc.is_null() {
                let _ = DeleteDC(self.dc);
            }
        }
    }
}

#[cfg(test)]
impl Window {
    fn source_buffer(&self) -> Result<Vec<u8>, PlatformError> {
        if self.canvas.length > 512 * 1024 {
            return Err(error("owned source buffer too large"));
        }
        // SAFETY: only this worker's HWND and the live owned DIB allocation whose exact
        // premultiplied bytes were supplied to UpdateLayeredWindow. No on-screen pixels.
        unsafe {
            if GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) as u32 & STYLE != STYLE
                || GetForegroundWindow() == self.hwnd
            {
                return Err(error("owned overlay interaction invariant failed"));
            }
            let pixels = std::slice::from_raw_parts(self.canvas.bits, self.canvas.length);
            if !pixels
                .as_chunks::<4>()
                .0
                .iter()
                .any(|p| p[0] != 0 || p[1] != 0 || p[2] != 0)
            {
                return Err(error("owned source buffer contains no rendering"));
            }
            let mut bmp = vec![0u8; 54];
            bmp[..2].copy_from_slice(b"BM");
            bmp[2..6].copy_from_slice(&((54 + pixels.len()) as u32).to_le_bytes());
            bmp[10..14].copy_from_slice(&54u32.to_le_bytes());
            bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
            bmp[18..22].copy_from_slice(&self.canvas.size[0].to_le_bytes());
            bmp[22..26].copy_from_slice(&(-self.canvas.size[1]).to_le_bytes());
            bmp[26..28].copy_from_slice(&1u16.to_le_bytes());
            bmp[28..30].copy_from_slice(&32u16.to_le_bytes());
            bmp.extend_from_slice(pixels);
            Ok(bmp)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crosspane_platform::{OverlayAnchor, Rgb8};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    fn fake() -> (WindowsOverlay, mpsc::Receiver<Request>, DisplayId) {
        let probe = MonitorProbe {
            device_path: "fake".into(),
            name: "fake".into(),
            rc_monitor: [0, 0, 1920, 1080],
            rc_work: [0, 0, 1920, 1040],
            primary: true,
            dpi: 96,
            refresh_millihz: 60_000,
            edid: None,
            twin: false,
            quarter_turns: 0,
        };
        let monitors = monitors(&[probe], &mut DisplayIds::default()).unwrap();
        let id = monitors[0].display.id;
        let shared = Arc::new(Shared {
            data: Mutex::new(Data {
                model: OverlayModel::default(),
                sink: None,
                monitors,
            }),
            emission: Mutex::new(()),
            stopped: AtomicBool::new(false),
            epoch: Instant::now(),
        });
        let (commands, receiver) = mpsc::sync_channel(QUEUE);
        let (done, completed) = mpsc::channel();
        done.send(()).unwrap();
        (
            WindowsOverlay {
                shared,
                commands,
                done: completed,
                thread: None,
                watchdog: None,
            },
            receiver,
            id,
        )
    }
    fn overlay(display: DisplayId) -> Overlay {
        Overlay {
            display,
            anchor: OverlayAnchor::TopCenter,
            text: "Input → owned peer".into(),
            accent: Rgb8 {
                r: 85,
                g: 180,
                b: 240,
            },
        }
    }

    #[test]
    fn fake_commands_are_bounded_and_acceptance_never_claims_presentation() {
        let (mut host, receiver, display) = fake();
        let (sender, events) = mpsc::channel();
        host.subscribe(Arc::new(move |event| {
            sender.send(event).unwrap();
        }))
        .unwrap();
        assert!(host.subscribe(Arc::new(|_| {})).is_err());
        for id in 0..32 {
            host.show(OverlayId(id), &overlay(display)).unwrap();
        }
        assert!(host.show(OverlayId(32), &overlay(display)).is_err());
        assert!(events.try_recv().is_err());
        let Request::Show(id, _, generation) = receiver.recv().unwrap() else {
            panic!("wrong fake request");
        };
        host.shared.observe(id, generation, true);
        assert_eq!(events.recv().unwrap(), OverlayEvent::Visible(id));
        host.shared.observe(id, generation, false);
        assert_eq!(events.recv().unwrap(), OverlayEvent::Unavailable(id));
        host.shared.observe(id, generation, true);
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn fake_hide_unknown_and_replacement_cancel_stale_native_completion() {
        let (mut host, receiver, display) = fake();
        host.hide(OverlayId(99)).unwrap();
        assert!(receiver.try_recv().is_err());
        host.show(OverlayId(1), &overlay(display)).unwrap();
        let Request::Show(id, _, old) = receiver.recv().unwrap() else {
            panic!("wrong fake request");
        };
        host.show(id, &overlay(display)).unwrap();
        let Request::Show(_, _, new) = receiver.recv().unwrap() else {
            panic!("wrong fake request");
        };
        host.shared.observe(id, old, true);
        assert!(host.shared.data().model.replay().is_empty());
        host.shared.observe(id, new, true);
        host.hide(id).unwrap();
        host.shared.observe(id, new, true);
        assert!(host.shared.data().model.replay().is_empty());
        assert!(host.show(id, &overlay(DisplayId(0))).is_err());
    }

    #[test]
    fn full_queue_preserves_previous_generation_and_late_subscribe_replays_once() {
        let (mut host, receiver, display) = fake();
        let id = OverlayId(5);
        for _ in 0..QUEUE {
            host.show(id, &overlay(display)).unwrap();
        }
        assert!(host.show(id, &overlay(display)).is_err());
        assert!(host.hide(id).is_err());
        assert!(
            host.hide(OverlayId(99)).is_ok(),
            "unknown hide needs no queue slot"
        );
        let mut last = 0;
        while let Ok(Request::Show(_, _, generation)) = receiver.try_recv() {
            last = generation;
        }
        host.shared.observe(id, last, true);
        let (sender, events) = mpsc::channel();
        host.subscribe(Arc::new(move |event| {
            sender.send(event).unwrap();
        }))
        .unwrap();
        assert_eq!(events.recv().unwrap(), OverlayEvent::Visible(id));
        assert!(events.try_recv().is_err());
        drop(host);
        assert_eq!(events.recv().unwrap(), OverlayEvent::Unavailable(id));
        assert!(events.try_recv().is_err());
    }

    fn limited() -> bool {
        let mut token = null_mut();
        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0;
        // SAFETY: read-only query of this probe's own process token; handle is closed below.
        unsafe {
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return false;
            }
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                (&mut elevation as *mut TOKEN_ELEVATION).cast(),
                size_of_val(&elevation) as u32,
                &mut returned,
            ) != 0;
            let _ = CloseHandle(token);
            ok && returned as usize == size_of_val(&elevation) && elevation.TokenIsElevated == 0
        }
    }

    unsafe extern "system" fn probe_monitor(
        handle: HMONITOR,
        _: HDC,
        _: *mut RECT,
        value: LPARAM,
    ) -> i32 {
        // SAFETY: callback is synchronous; outputs live only during owned fixture enumeration.
        unsafe {
            let mut info = MONITORINFOEXW {
                monitorInfo: MONITORINFO {
                    cbSize: size_of::<MONITORINFOEXW>() as u32,
                    ..Default::default()
                },
                ..Default::default()
            };
            if GetMonitorInfoW(handle, (&mut info as *mut MONITORINFOEXW).cast()) == 0 {
                return 0;
            }
            if info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY == 0 {
                return 1;
            }
            let mut x = 0;
            let mut y = 0;
            if GetDpiForMonitor(handle, MDT_EFFECTIVE_DPI, &mut x, &mut y) < 0 || x != y {
                return 0;
            }
            let length = info.szDevice.iter().position(|c| *c == 0).unwrap();
            let m = info.monitorInfo.rcMonitor;
            let w = info.monitorInfo.rcWork;
            *(value as *mut Option<MonitorProbe>) = Some(MonitorProbe {
                device_path: "owned-probe-monitor".into(),
                name: String::from_utf16_lossy(&info.szDevice[..length]),
                rc_monitor: [m.left, m.top, m.right, m.bottom],
                rc_work: [w.left, w.top, w.right, w.bottom],
                primary: true,
                dpi: x,
                refresh_millihz: 60_000,
                edid: None,
                twin: false,
                quarter_turns: 0,
            });
        }
        1
    }
    fn base64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut output = String::new();
        for block in bytes.chunks(3) {
            let value = (u32::from(block[0]) << 16)
                | (u32::from(*block.get(1).unwrap_or(&0)) << 8)
                | u32::from(*block.get(2).unwrap_or(&0));
            for index in 0..4 {
                output.push(if index > block.len() {
                    '='
                } else {
                    ALPHABET[((value >> (18 - 6 * index)) & 63) as usize] as char
                });
            }
        }
        output
    }

    #[test]
    #[ignore = "Limited win-gui only; explicit owned-overlay probe opt-in required"]
    fn owned_overlay_source_buffer_probe() {
        assert_eq!(
            std::env::var("CROSSPANE_WINDOWS_OVERLAY_PROBE").as_deref(),
            Ok("1")
        );
        assert!(limited(), "overlay probe refuses elevated token");
        let _dpi = Class::new().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut probe = None::<MonitorProbe>;
        assert_ne!(
            // SAFETY: only read-only monitor enumeration; no screen capture or display mutation.
            unsafe {
                EnumDisplayMonitors(
                    null_mut(),
                    null(),
                    Some(probe_monitor),
                    (&mut probe as *mut Option<_>) as isize,
                )
            },
            0
        );
        let mut ids = DisplayIds::default();
        let probe = probe.unwrap();
        let display = ids.assign(&probe.device_path).unwrap();
        let mut host = WindowsOverlay::new(&[probe], &mut ids).unwrap();
        let (sender, events) = mpsc::channel();
        host.subscribe(Arc::new(move |event| {
            let _ = sender.send(event);
        }))
        .unwrap();
        let anchors = [
            OverlayAnchor::TopCenter,
            OverlayAnchor::TopRight,
            OverlayAnchor::BottomRight,
            OverlayAnchor::Center,
        ];
        let labels = [
            "Input → owned target",
            "Receiving input",
            "Projected window",
            "Crosspane indicator",
        ];
        let mut total = 0;
        for (index, anchor) in anchors.into_iter().enumerate() {
            assert!(Instant::now() < deadline);
            let id = OverlayId(index as u32 + 1);
            let mut value = overlay(display);
            value.anchor = anchor;
            value.text = labels[index].into();
            host.show(id, &value).unwrap();
            assert_eq!(
                events.recv_timeout(Duration::from_secs(1)).unwrap(),
                OverlayEvent::Visible(id)
            );
            thread::sleep(Duration::from_secs(1));
            let (sender, reply) = mpsc::channel();
            host.commands
                .try_send(Request::SourceBuffer(id, sender))
                .unwrap();
            let bmp = reply.recv_timeout(Duration::from_secs(1)).unwrap().unwrap();
            total += bmp.len();
            assert!(total < 2 * 1024 * 1024);
            println!("OWNED_SOURCE_BUFFER_BMP_{index}={}", base64(&bmp));
            host.hide(id).unwrap();
            let (sender, reply) = mpsc::channel();
            host.commands
                .try_send(Request::SourceBuffer(id, sender))
                .unwrap();
            assert!(matches!(
                reply.recv_timeout(Duration::from_secs(1)).unwrap(),
                Err(PlatformError::NotFound)
            ));
        }
        host.shared.shutdown();
        assert!(host.done.recv_timeout(Duration::from_secs(1)).is_ok());
        if let Some(thread) = host.thread.take() {
            thread.join().unwrap();
        }
        drop(host);
        println!("OWNED_SOURCE_BUFFER_PROBE=PASS");
    }
}
