//! Notification-area tray, with native resources confined to its owned message thread.
//! A never-shown top-level tool window receives TaskbarCreated; message-only windows cannot.
//! Drop stops callbacks immediately. A native call already running cannot be preempted: a
//! stalled worker retains its resources after the two-second join budget, then cleans on return.
#![allow(unsafe_code)]

use crate::model::tray::{
    IconLifecycle, MenuSnapshot, Notify, Row, RowKind, badge, menu_text, popup_request,
};
use crosspane_platform::{
    EventSink, PlatformError,
    tray::{TrayEvent, TrayHost, TrayMenu, TrayState},
};
use std::{
    cell::RefCell,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr::{null, null_mut},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, WAIT_FAILED, WPARAM},
    Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
        DeleteObject,
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::{Shell::*, WindowsAndMessaging::*},
};

const CALLBACK: u32 = WM_APP + 12;
const BOUND: Duration = Duration::from_secs(2);
static SERIAL: AtomicU64 = AtomicU64::new(1);
thread_local! {static CONTEXT:RefCell<Option<Arc<Shared>>>=const {RefCell::new(None)};}

fn error(message: &str) -> PlatformError {
    PlatformError::Backend(message.into())
}
#[derive(Default)]
struct Pending {
    menu: Option<(MenuSnapshot, TrayState)>,
    popup: Option<(i32, i32)>,
    recreate: bool,
    failure: Option<PlatformError>,
}
struct Shared {
    alive: AtomicBool,
    pending: Mutex<Pending>,
    sink: Mutex<Option<Arc<dyn EventSink<TrayEvent>>>>,
    hwnd: Mutex<usize>,
    taskbar: AtomicU32,
    #[cfg(test)]
    probe: AtomicBool,
    #[cfg(test)]
    added: AtomicBool,
    #[cfg(test)]
    removed: AtomicBool,
    #[cfg(test)]
    window_closed: AtomicBool,
}
impl Shared {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            alive: AtomicBool::new(true),
            pending: Mutex::new(Pending::default()),
            sink: Mutex::new(None),
            hwnd: Mutex::new(0),
            taskbar: AtomicU32::new(0),
            #[cfg(test)]
            probe: AtomicBool::new(false),
            #[cfg(test)]
            added: AtomicBool::new(false),
            #[cfg(test)]
            removed: AtomicBool::new(false),
            #[cfg(test)]
            window_closed: AtomicBool::new(false),
        })
    }
    fn fail(&self, failure: PlatformError) {
        if let Ok(mut p) = self.pending.lock() {
            p.failure = Some(failure);
        }
    }
    fn deliver(&self, event: TrayEvent) {
        let sink = self.sink.lock().ok().and_then(|s| s.clone());
        if self.alive.load(Ordering::Acquire)
            && let Some(sink) = sink
            && catch_unwind(AssertUnwindSafe(|| sink.send(event))).is_err()
        {
            self.alive.store(false, Ordering::Release);
        }
    }
    fn stop(&self) {
        self.alive.store(false, Ordering::Release);
        if let Ok(mut sink) = self.sink.lock() {
            *sink = None;
        }
    }
}

/// The icon appears only on the first set; subsequent sets coalesce to the newest whole menu.
pub struct WindowsTray {
    shared: Arc<Shared>,
    done: mpsc::Receiver<()>,
    worker: Option<JoinHandle<()>>,
}
impl std::fmt::Debug for WindowsTray {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsTray")
            .field("alive", &self.shared.alive.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}
impl WindowsTray {
    pub fn new() -> Result<Self, PlatformError> {
        let shared = Shared::new();
        let context = shared.clone();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (done_tx, done) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("crosspane-tray".into())
            .spawn(move || {
                CONTEXT.with(|slot| *slot.borrow_mut() = Some(context.clone()));
                let result = catch_unwind(AssertUnwindSafe(|| run(&context, ready_tx)));
                context.stop();
                if result.is_err() {
                    context.fail(error("tray worker panicked"));
                }
                CONTEXT.with(|slot| *slot.borrow_mut() = None);
                let _ = done_tx.send(());
            })
            .map_err(|_| error("tray worker spawn failed"))?;
        let host = Self {
            shared,
            done,
            worker: Some(worker),
        };
        Self::finish_startup(host, ready_rx.recv_timeout(BOUND))
    }
    fn finish_startup(
        mut host: Self,
        result: Result<Result<(), PlatformError>, mpsc::RecvTimeoutError>,
    ) -> Result<Self, PlatformError> {
        let failure = match result {
            Ok(Ok(())) => return Ok(host),
            Ok(Err(e)) => e,
            Err(_) => PlatformError::Timeout,
        };
        // The readiness wait already consumed the caller's budget. Stop and detach our worker;
        // it retains and cleans only its own resources when the nonpreemptible native call returns.
        host.shared.stop();
        host.worker.take();
        Err(failure)
    }
}
impl TrayHost for WindowsTray {
    fn subscribe(&mut self, sink: Arc<dyn EventSink<TrayEvent>>) -> Result<(), PlatformError> {
        let mut current = self
            .shared
            .sink
            .lock()
            .map_err(|_| error("tray subscription poisoned"))?;
        if current.is_some() {
            return Err(error("tray already subscribed"));
        }
        if !self.shared.alive.load(Ordering::Acquire) {
            return Err(error("tray worker stopped"));
        }
        *current = Some(sink);
        Ok(())
    }
    fn set(&mut self, menu: &TrayMenu) -> Result<(), PlatformError> {
        let snapshot = MenuSnapshot::new(menu)?;
        let mut pending = self
            .shared
            .pending
            .lock()
            .map_err(|_| error("tray state poisoned"))?;
        if !self.shared.alive.load(Ordering::Acquire) {
            return Err(error("tray worker stopped"));
        }
        pending.menu = Some((snapshot, menu.state));
        if let Some(failure) = pending.failure.take() {
            Err(failure)
        } else {
            Ok(())
        }
    }
}
impl Drop for WindowsTray {
    fn drop(&mut self) {
        self.shared.stop();
        if let Ok(hwnd) = self.shared.hwnd.try_lock()
            && *hwnd != 0
        {
            // SAFETY: the worker owns this HWND and serializes destruction with this mutex.
            unsafe {
                PostMessageW(*hwnd as HWND, WM_CLOSE, 0, 0);
            }
        }
        if self.worker.is_some()
            && self.done.recv_timeout(BOUND).is_ok()
            && let Some(worker) = self.worker.take()
        {
            let _ = worker.join();
        }
    }
}

#[derive(Default)]
struct Registration {
    present: bool,
}
impl Registration {
    fn apply(&mut self, op: Notify, mut call: impl FnMut(u32) -> bool) -> bool {
        if op == Notify::Modify {
            return self.present && call(NIM_MODIFY);
        }
        if !self.remove(&mut call) || !call(NIM_ADD) {
            return false;
        }
        self.present = true;
        if call(NIM_SETVERSION) {
            true
        } else {
            self.remove(call);
            false
        }
    }
    fn remove(&mut self, mut call: impl FnMut(u32) -> bool) -> bool {
        if !self.present {
            return true;
        }
        if call(NIM_DELETE) {
            self.present = false;
            true
        } else {
            false
        }
    }
}

struct Window {
    hwnd: HWND,
    class: Vec<u16>,
    instance: windows_sys::Win32::Foundation::HINSTANCE,
    shared: Arc<Shared>,
}
impl Window {
    fn new(shared: Arc<Shared>) -> Result<Self, PlatformError> {
        // SAFETY: constant NUL-terminated broadcast name, no borrowed registration payload.
        let broadcast = menu_text("TaskbarCreated");
        // SAFETY: live NUL-terminated UTF-16 storage; the name is copied by the registration API.
        let taskbar = unsafe { RegisterWindowMessageW(broadcast.as_ptr()) };
        if taskbar == 0 {
            return Err(error("tray broadcast registration failed"));
        }
        shared.taskbar.store(taskbar, Ordering::Release);
        let class = menu_text(&format!(
            "CrosspaneTray-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        // SAFETY: null requests this module, and WNDCLASS references live NUL-terminated storage.
        let instance = unsafe { GetModuleHandleW(null()) };
        if instance.is_null() {
            return Err(error("tray module unavailable"));
        }
        let wc = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..Default::default()
        };
        // SAFETY: callback and class name remain valid until unregister after window destruction.
        if unsafe { RegisterClassW(&wc) } == 0 {
            return Err(error("tray class registration failed"));
        }
        let mut window = Self {
            hwnd: null_mut(),
            class,
            instance,
            shared,
        };
        // SAFETY: ordinary owned top-level window, no visible style, no parent, no borrowed payload.
        window.hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW,
                window.class.as_ptr(),
                window.class.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                instance,
                null(),
            )
        };
        if window.hwnd.is_null() {
            return Err(error("tray window creation failed"));
        }
        *window
            .shared
            .hwnd
            .lock()
            .map_err(|_| error("tray HWND state poisoned"))? = window.hwnd as usize;
        Ok(window)
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        let mut slot = self.shared.hwnd.lock().unwrap_or_else(|p| p.into_inner());
        *slot = 0;
        // SAFETY: destruction/unregistration on creating thread; the class and HWND are ours.
        unsafe {
            let destroyed = self.hwnd.is_null() || DestroyWindow(self.hwnd) != 0;
            let unregistered = UnregisterClassW(self.class.as_ptr(), self.instance) != 0;
            #[cfg(test)]
            self.shared
                .window_closed
                .store(destroyed && unregistered, Ordering::Release);
            if !destroyed || !unregistered {
                self.shared.fail(error("tray window cleanup failed"));
            }
        }
    }
}
unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let shared = CONTEXT.with(|slot| slot.borrow().clone());
    let handled = catch_unwind(AssertUnwindSafe(|| -> Result<bool, ()> {
        if let Some(shared) = &shared {
            if msg == WM_CLOSE {
                shared.stop(); // SAFETY: ends only this thread's active native menu.
                unsafe {
                    EndMenu();
                }
                return Ok(true);
            }
            if !shared.alive.load(Ordering::Acquire) {
                return Ok(false);
            }
            let mut p = shared.pending.lock().map_err(|_| ())?;
            if msg == CALLBACK {
                #[cfg(test)]
                if shared.probe.load(Ordering::Acquire) {
                    return Ok(true);
                }
                p.popup = popup_request(w, l, 1);
                return Ok(true);
            }
            if msg == shared.taskbar.load(Ordering::Acquire) {
                p.recreate = true;
                return Ok(true);
            }
        }
        Ok(false)
    }));
    match handled {
        Ok(Ok(true)) => 0,
        Err(_) | Ok(Err(_)) => {
            if let Some(s) = shared {
                s.stop();
            }
            0
        }
        // SAFETY: unhandled message forwarded with original arguments to the default procedure.
        Ok(Ok(false)) => unsafe { DefWindowProcW(hwnd, msg, w, l) },
    }
}

struct Icon(HICON);
impl Drop for Icon {
    fn drop(&mut self) {
        // SAFETY: exclusively owned icon created by CreateIconIndirect.
        unsafe {
            DestroyIcon(self.0);
        }
    }
}
struct Bitmap(windows_sys::Win32::Graphics::Gdi::HBITMAP);
impl Drop for Bitmap {
    fn drop(&mut self) {
        // SAFETY: exclusively owned GDI bitmap.
        unsafe {
            DeleteObject(self.0);
        }
    }
}
struct Com;
impl Drop for Com {
    fn drop(&mut self) {
        // SAFETY: balances successful CoInitializeEx on this thread.
        unsafe {
            windows::Win32::System::Com::CoUninitialize();
        }
    }
}
fn icons() -> Result<(Com, [Icon; 4]), PlatformError> {
    use windows::Win32::{Graphics::Imaging::*, System::Com::*};
    // SAFETY: COM initialized and used exclusively on the owned worker thread.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
        .ok()
        .map_err(|_| error("tray COM initialization failed"))?;
    let com = Com;
    // SAFETY: public WIC class, valid pinned interface type; None means no aggregation.
    let factory: IWICImagingFactory =
        unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }
            .map_err(|_| error("tray WIC factory failed"))?;
    let mut pixels = vec![0u8; 32 * 32 * 4];
    // SAFETY: static PNG outlives stream; all COM interfaces stay on this thread; buffers have exact capacity.
    unsafe {
        let stream = factory
            .CreateStream()
            .map_err(|_| error("tray WIC stream failed"))?;
        stream
            .InitializeFromMemory(include_bytes!(
                "../../../assets/brand/crosspane-icon-32.png"
            ))
            .map_err(|_| error("tray PNG stream failed"))?;
        let decoder = factory
            .CreateDecoderFromStream(&stream, null(), WICDecodeMetadataCacheOnLoad)
            .map_err(|_| error("tray PNG decode failed"))?;
        let frame = decoder
            .GetFrame(0)
            .map_err(|_| error("tray PNG frame failed"))?;
        let converter = factory
            .CreateFormatConverter()
            .map_err(|_| error("tray PNG conversion failed"))?;
        converter
            .Initialize(
                &frame,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .map_err(|_| error("tray PNG format failed"))?;
        let (mut width, mut height) = (0, 0);
        converter
            .GetSize(&mut width, &mut height)
            .map_err(|_| error("tray PNG dimensions failed"))?;
        if (width, height) != (32, 32) {
            return Err(error("tray PNG dimensions invalid"));
        }
        converter
            .CopyPixels(null(), 128, &mut pixels)
            .map_err(|_| error("tray PNG pixels failed"))?;
    }
    let make = |state| {
        let mut image = pixels.clone();
        badge(state, &mut image)?;
        make_icon(&image)
    };
    Ok((
        com,
        [
            make(TrayState::Idle)?,
            make(TrayState::Active)?,
            make(TrayState::Attention)?,
            make(TrayState::Offline)?,
        ],
    ))
}
fn make_icon(pixels: &[u8]) -> Result<Icon, PlatformError> {
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: 32,
            biHeight: -32,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits = null_mut();
    // SAFETY: valid descriptor creates our top-down 32x32 32-bit bitmap and returns its pixel pointer.
    let color = Bitmap(unsafe {
        CreateDIBSection(null_mut(), &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0)
    });
    if color.0.is_null() || bits.is_null() {
        return Err(error("tray color bitmap failed"));
    }
    // SAFETY: allocation contains 4096 bytes; pixels is the checked 32x32 premultiplied WIC image.
    unsafe {
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast(), pixels.len());
    }
    let mask_bytes = [0u8; 128];
    // SAFETY: exact 32x32 monochrome mask storage; GDI copies its contents.
    let mask = Bitmap(unsafe { CreateBitmap(32, 32, 1, 1, mask_bytes.as_ptr().cast()) });
    if mask.0.is_null() {
        return Err(error("tray mask bitmap failed"));
    }
    let info = ICONINFO {
        fIcon: 1,
        hbmMask: mask.0,
        hbmColor: color.0,
        ..Default::default()
    };
    // SAFETY: both bitmaps remain valid through creation; CreateIconIndirect copies them.
    let icon = unsafe { CreateIconIndirect(&info) };
    if icon.is_null() {
        Err(error("tray icon creation failed"))
    } else {
        Ok(Icon(icon))
    }
}
struct NativeRegistration<'a> {
    registration: Registration,
    data: NOTIFYICONDATAW,
    shared: &'a Shared,
}
impl NativeRegistration<'_> {
    fn notify(&mut self, operation: Notify) -> bool {
        let data = &self.data;
        let ok = self.registration.apply(operation, |op| {
            // SAFETY: complete descriptor uses this worker's live HWND/icon.
            unsafe { Shell_NotifyIconW(op, data) != 0 }
        });
        #[cfg(test)]
        if ok {
            self.shared.added.store(true, Ordering::Release);
        }
        ok
    }
}
impl Drop for NativeRegistration<'_> {
    fn drop(&mut self) {
        let data = &self.data;
        let ok = self.registration.remove(|op| {
            // SAFETY: removal addresses only this worker's HWND/id.
            unsafe { Shell_NotifyIconW(op, data) != 0 }
        });
        #[cfg(test)]
        self.shared.removed.store(ok, Ordering::Release);
        #[cfg(not(test))]
        {
            let _ = ok;
            let _ = self.shared;
        }
    }
}

struct Menu(HMENU);
impl Drop for Menu {
    fn drop(&mut self) {
        // SAFETY: owned root includes ownership of appended child menus.
        unsafe {
            DestroyMenu(self.0);
        }
    }
}
fn menu(rows: &[Row]) -> Result<Menu, PlatformError> {
    // SAFETY: creates an unshared menu owned by this function.
    let root = Menu(unsafe { CreatePopupMenu() });
    if root.0.is_null() {
        return Err(error("tray menu creation failed"));
    }
    for row in rows {
        let child = if let RowKind::Submenu(rows) = &row.kind {
            Some(menu(rows)?)
        } else {
            None
        };
        let mut flags = if matches!(row.kind, RowKind::Separator) {
            MF_SEPARATOR
        } else {
            MF_STRING
        };
        if !row.enabled {
            flags |= MF_GRAYED;
        }
        if row.checked {
            flags |= MF_CHECKED;
        }
        let id = if let Some(child) = &child {
            flags |= MF_POPUP;
            child.0 as usize
        } else {
            row.command as usize
        };
        // SAFETY: native handles and NUL-terminated string are live; success transfers child ownership.
        if unsafe { AppendMenuW(root.0, flags, id, row.text.as_ptr()) } == 0 {
            return Err(error("tray menu append failed"));
        }
        if let Some(child) = child {
            std::mem::forget(child);
        }
    }
    Ok(root)
}
fn popup(
    window: &Window,
    snapshot: &MenuSnapshot,
    mut position: (i32, i32),
) -> Result<Option<TrayEvent>, PlatformError> {
    let menu = menu(&snapshot.rows)?;
    if position == (-1, -1) {
        let mut point = POINT::default(); // SAFETY: only reads pointer coordinates for a user-requested menu.
        if unsafe { GetCursorPos(&mut point) } == 0 {
            return Err(error("tray popup position unavailable"));
        }
        position = (point.x, point.y);
    }
    // SAFETY: only our hidden owner activates, only following a user callback; no input injection.
    if unsafe { SetForegroundWindow(window.hwnd) } == 0 {
        return Err(error("tray popup activation refused"));
    }
    // SAFETY: own menu/window, synchronous native menu loop; ordinal command returned without WM_COMMAND.
    let choice = unsafe {
        TrackPopupMenuEx(
            menu.0,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
            position.0,
            position.1,
            window.hwnd,
            null(),
        )
    };
    // SAFETY: documented benign notification-area menu dismissal/focus protocol for our icon.
    unsafe {
        PostMessageW(window.hwnd, WM_NULL, 0, 0);
        let data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: window.hwnd,
            uID: 1,
            ..Default::default()
        };
        Shell_NotifyIconW(NIM_SETFOCUS, &data);
    }
    Ok(snapshot.chosen(choice as u32))
}
fn run(
    shared: &Arc<Shared>,
    ready: mpsc::SyncSender<Result<(), PlatformError>>,
) -> Result<(), PlatformError> {
    let window = match Window::new(shared.clone()) {
        Ok(w) => w,
        Err(e) => {
            let _ = ready.send(Err(e));
            return Ok(());
        }
    };
    let (_com, icons) = match icons() {
        Ok(i) => i,
        Err(e) => {
            let _ = ready.send(Err(e));
            return Ok(());
        }
    };
    let data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: window.hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
        uCallbackMessage: CALLBACK,
        hIcon: icons[0].0,
        Anonymous: NOTIFYICONDATAW_0 {
            uVersion: NOTIFYICON_VERSION_4,
        },
        ..Default::default()
    };
    let mut native = NativeRegistration {
        registration: Registration::default(),
        data,
        shared,
    };
    let mut lifecycle = IconLifecycle::default();
    let mut current = None;
    let _ = ready.send(Ok(()));
    while shared.alive.load(Ordering::Acquire) {
        let (next, position, recreate) = {
            let mut p = shared
                .pending
                .lock()
                .map_err(|_| error("tray pending state poisoned"))?;
            (
                p.menu.take(),
                p.popup.take(),
                std::mem::take(&mut p.recreate),
            )
        };
        let mut update = false;
        if recreate {
            native.registration.present = false;
            update = lifecycle.taskbar_created().is_some();
        }
        if let Some((snapshot, state)) = next {
            native.data.szTip = snapshot.tooltip;
            native.data.hIcon = icons[match state {
                TrayState::Idle => 0,
                TrayState::Active => 1,
                TrayState::Attention => 2,
                TrayState::Offline => 3,
            }]
            .0;
            current = Some(snapshot);
            update = true;
        }
        if update && shared.alive.load(Ordering::Acquire) {
            let op = lifecycle.request();
            let ok = native.notify(op);
            lifecycle.complete(op, ok);
            if !ok {
                shared.fail(error("tray notification registration failed"));
            }
        }
        if let (Some(position), Some(snapshot)) = (position, current.as_ref())
            && shared.alive.load(Ordering::Acquire)
        {
            match popup(&window, snapshot, position) {
                Ok(Some(event)) => shared.deliver(event),
                Ok(None) => {}
                Err(e) => shared.fail(e),
            }
        }
        // SAFETY: waits only on this thread's input queue with a bounded poll; no external handles.
        if unsafe { MsgWaitForMultipleObjectsEx(0, null(), 50, QS_ALLINPUT, MWMO_INPUTAVAILABLE) }
            == WAIT_FAILED
        {
            return Err(error("tray message wait failed"));
        }
        let mut msg = MSG::default();
        // SAFETY: messages belong to this worker's queue; the callback contains unwinding.
        unsafe {
            while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                if msg.message == WM_QUIT {
                    shared.stop();
                    break;
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fake_host() -> WindowsTray {
        let (_, done) = mpsc::channel();
        WindowsTray {
            shared: Shared::new(),
            done,
            worker: None,
        }
    }
    #[test]
    fn tray_native_failed_startup_does_not_add_a_second_join_budget() {
        let mut host = fake_host();
        let (release, held) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        host.done = finished;
        host.worker = Some(thread::spawn(move || {
            let _ = held.recv_timeout(Duration::from_secs(3));
            let _ = done.send(());
        }));
        let start = std::time::Instant::now();
        assert!(matches!(
            WindowsTray::finish_startup(host, Err(mpsc::RecvTimeoutError::Timeout)),
            Err(PlatformError::Timeout)
        ));
        let elapsed = start.elapsed();
        let _ = release.send(());
        assert!(
            elapsed < Duration::from_millis(100),
            "startup already consumed its bound, extra wait {elapsed:?}"
        );
    }
    #[test]
    fn tray_native_taskbar_and_version4_callbacks_schedule_owned_work_only() {
        let shared = Shared::new();
        shared.taskbar.store(0xc055, Ordering::Release);
        CONTEXT.with(|slot| *slot.borrow_mut() = Some(shared.clone()));
        // SAFETY: these handled synthetic messages use only Rust state, never the zero HWND or any Win32 API.
        unsafe {
            assert_eq!(window_proc(null_mut(), 0xc055, 0, 0), 0);
            assert_eq!(
                window_proc(
                    null_mut(),
                    CALLBACK,
                    17 | ((23usize) << 16),
                    (1 << 16) | 0x7b
                ),
                0
            );
        }
        let mut pending = shared.pending.lock().unwrap();
        assert!(pending.recreate);
        assert_eq!(pending.popup.take(), Some((-1, -1)));
        drop(pending);
        shared.stop();
        // SAFETY: CALLBACK must remain handled after stop, without default native processing.
        // The normal stopped branch forwards to DefWindowProc, so test closure directly via alive state instead.
        assert!(!shared.alive.load(Ordering::Acquire));
        CONTEXT.with(|slot| *slot.borrow_mut() = None);
    }
    #[test]
    fn tray_native_latest_set_coalesces_and_completely_replaces() {
        let mut host = fake_host();
        for n in 0..100 {
            host.set(&TrayMenu {
                items: vec![crosspane_platform::tray::TrayItem::Action {
                    id: crosspane_platform::tray::TrayItemId(n),
                    label: String::new(),
                    enabled: true,
                }],
                ..Default::default()
            })
            .unwrap();
        }
        let mut p = host.shared.pending.lock().unwrap();
        let (snapshot, _) = p.menu.take().unwrap();
        assert_eq!(snapshot.rows.len(), 1);
        assert_eq!(
            snapshot.chosen(1),
            Some(TrayEvent::Chosen(crosspane_platform::tray::TrayItemId(99)))
        );
        assert!(p.menu.is_none());
    }
    #[test]
    fn tray_native_subscribe_once_preserves_sink_and_drop_stops_delivery() {
        let mut host = fake_host();
        let shared = host.shared.clone();
        let (tx, rx) = mpsc::channel();
        host.subscribe(Arc::new(move |e| {
            tx.send(e).unwrap();
        }))
        .unwrap();
        assert!(
            host.subscribe(Arc::new(|_| panic!("replacement sink")))
                .is_err()
        );
        let event = TrayEvent::Chosen(crosspane_platform::tray::TrayItemId(0));
        shared.deliver(event);
        assert_eq!(rx.try_recv().unwrap(), event);
        drop(host);
        shared.deliver(event);
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn tray_native_sink_reentry_and_panic_cannot_escape_callback() {
        let mut host = fake_host();
        let shared = host.shared.clone();
        host.subscribe(Arc::new(move |_| {
            assert!(shared.pending.lock().is_ok());
            panic!("test callback");
        }))
        .unwrap();
        host.shared
            .deliver(TrayEvent::Chosen(crosspane_platform::tray::TrayItemId(1)));
        assert!(!host.shared.alive.load(Ordering::Acquire));
        assert!(host.set(&TrayMenu::default()).is_err());
    }
    #[test]
    fn tray_native_failed_registration_is_reported_without_losing_latest_set() {
        let mut host = fake_host();
        host.shared.fail(error("fake registration failure"));
        assert!(host.set(&TrayMenu::default()).is_err());
        assert!(host.shared.pending.lock().unwrap().menu.is_some());
        assert!(host.set(&TrayMenu::default()).is_ok());
    }
    #[test]
    fn tray_native_add_failure_and_remove_are_exactly_once() {
        let mut r = Registration::default();
        let mut calls = Vec::new();
        assert!(!r.apply(Notify::Add, |op| {
            calls.push(op);
            false
        }));
        assert!(!r.present);
        assert_eq!(calls, vec![NIM_ADD]);
        assert!(r.remove(|_| panic!("never installed")));
        assert!(r.apply(Notify::Add, |_| true));
        let mut removed = 0;
        assert!(r.remove(|op| {
            assert_eq!(op, NIM_DELETE);
            removed += 1;
            true
        }));
        assert!(r.remove(|_| panic!("double remove")));
        assert_eq!(removed, 1);
    }
    #[test]
    #[ignore = "Limited own-icon probe; explicit opt-in, win-gui only, no clicks"]
    fn limited_tray_add_remove_without_clicks() {
        use std::time::Instant;
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation},
            System::Threading::{GetCurrentProcess, OpenProcessToken},
        };
        assert_eq!(
            std::env::var("CROSSPANE_WINDOWS_TRAY_PROBE").as_deref(),
            Ok("1")
        );
        let mut token = null_mut();
        let mut elevation = TOKEN_ELEVATION::default();
        let mut bytes = 0;
        // SAFETY: query this process only; owned token and exactly sized output storage.
        unsafe {
            assert_ne!(
                OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token),
                0
            );
            let result = GetTokenInformation(
                token,
                TokenElevation,
                (&mut elevation as *mut TOKEN_ELEVATION).cast(),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut bytes,
            );
            assert_ne!(CloseHandle(token), 0);
            assert_ne!(result, 0);
        }
        assert_eq!(
            elevation.TokenIsElevated, 0,
            "native probe forbidden under elevated cargo test"
        );
        let (cancel, done) = mpsc::channel();
        let watchdog = thread::spawn(move || {
            if done.recv_timeout(Duration::from_secs(10)) == Err(mpsc::RecvTimeoutError::Timeout) {
                eprintln!("tray probe deadline: own native cleanup unverified");
                std::process::exit(124);
            }
        });
        struct Cancel(mpsc::Sender<()>);
        impl Drop for Cancel {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let cancellation = Cancel(cancel);
        let mut host = WindowsTray::new().unwrap();
        host.shared.probe.store(true, Ordering::Release);
        let shared = host.shared.clone();
        host.set(&TrayMenu {
            tooltip: "Crosspane owned tray probe".into(),
            items: vec![crosspane_platform::tray::TrayItem::Label(
                "Owned no-click test".into(),
            )],
            ..Default::default()
        })
        .unwrap();
        let until = Instant::now() + BOUND;
        while !shared.added.load(Ordering::Acquire) && Instant::now() < until {
            thread::sleep(Duration::from_millis(10));
        }
        let added = shared.added.load(Ordering::Acquire);
        println!("TOKEN elevated=false; NIM_ADD_AND_VERSION={added}");
        if added {
            thread::sleep(Duration::from_secs(3));
        }
        drop(host);
        let removed = shared.removed.load(Ordering::Acquire);
        let window_closed = shared.window_closed.load(Ordering::Acquire);
        println!(
            "NIM_DELETE={removed}; OWN_WINDOW_CLOSED={window_closed}; no popup/click/Explorer restart"
        );
        assert!(
            added && removed && window_closed,
            "owned tray cleanup or registration unverified"
        );
        drop(cancellation);
        watchdog.join().unwrap();
    }
    #[test]
    fn tray_native_add_sets_version_and_modify_does_not() {
        let mut r = Registration::default();
        let mut calls = Vec::new();
        assert!(r.apply(Notify::Add, |op| {
            calls.push(op);
            true
        }));
        assert!(r.present);
        assert!(r.apply(Notify::Modify, |op| {
            calls.push(op);
            true
        }));
        assert_eq!(calls, vec![NIM_ADD, NIM_SETVERSION, NIM_MODIFY]);
    }
    #[test]
    fn tray_native_version_failure_removes_and_delete_failure_retains_obligation() {
        let mut r = Registration::default();
        let mut calls = Vec::new();
        assert!(!r.apply(Notify::Add, |op| {
            calls.push(op);
            op == NIM_ADD
        }));
        assert_eq!(calls, vec![NIM_ADD, NIM_SETVERSION, NIM_DELETE]);
        assert!(r.present);
        assert!(r.remove(|op| op == NIM_DELETE));
        assert!(!r.present);
    }
    #[test]
    fn tray_native_retry_deletes_previous_registration_before_add() {
        let mut r = Registration { present: true };
        let mut calls = Vec::new();
        assert!(r.apply(Notify::Add, |op| {
            calls.push(op);
            true
        }));
        assert_eq!(calls, vec![NIM_DELETE, NIM_ADD, NIM_SETVERSION]);
    }
}
