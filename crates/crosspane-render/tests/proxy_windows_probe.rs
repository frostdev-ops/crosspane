#![cfg(target_os = "windows")]
//! Compile an owned fixture; run its executable only through the lead's limited win-gui.sh.

use std::{path::Path, process::Command};

use anyhow::{Context, Result, ensure};

const DRIVER: &str = r#"
use std::{ffi::c_void, path::PathBuf, sync::{Arc, Mutex, mpsc}, time::{Duration, Instant}};
use crosspane_render::proxy::{HostCommand, HostEvent, HostHandle, HostMonitorMapping, HostPlace, ProxyHost};
use crosspane_types::geom::{DisplayGeometry, PointDevice, PointLogical, SizeMm};
use winit::dpi::{LogicalPosition, PhysicalPosition};
use crosspane_types::geom::{PixelRect, PixelSize, euclid::point2};
use tracing::{Event, Metadata, Subscriber, span::{Attributes, Id, Record}};

type Handle = *mut c_void;
#[repr(C)] struct NativePoint { x: i32, y: i32 }
#[repr(C)] #[derive(Clone, Copy)] struct RawDevice { page: u16, usage: u16, flags: u32, target: Handle }
impl Default for RawDevice { fn default() -> Self { Self { page: 0, usage: 0, flags: 0, target: std::ptr::null_mut() } } }
#[repr(C)] #[derive(Default)] struct Rect { left: i32, top: i32, right: i32, bottom: i32 }
#[repr(C)] struct MonitorInfo { size: u32, monitor: Rect, work: Rect, flags: u32, name: [u16; 32] }
#[repr(C)] struct BitmapInfo {
    size: u32, width: i32, height: i32, planes: u16, bit_count: u16, compression: u32,
    image_size: u32, x_pixels: i32, y_pixels: i32, used: u32, important: u32, colours: [u32; 1],
}
#[link(name = "user32")]
unsafe extern "system" {
    fn EnumThreadWindows(thread: u32, callback: unsafe extern "system" fn(Handle, isize) -> i32, data: isize) -> i32;
    fn GetWindowThreadProcessId(window: Handle, process: *mut u32) -> u32;
    fn GetClassNameW(window: Handle, text: *mut u16, len: i32) -> i32;
    fn GetWindowLongPtrW(window: Handle, index: i32) -> isize;
    fn GetWindow(window: Handle, command: u32) -> Handle;
    fn GetClientRect(window: Handle, rect: *mut Rect) -> i32;
    fn GetWindowRect(window: Handle, rect: *mut Rect) -> i32;
    fn GetWindowDpiAwarenessContext(window: Handle) -> Handle;
    fn AreDpiAwarenessContextsEqual(first: Handle, second: Handle) -> i32;
    fn SetWindowPos(window: Handle, after: Handle, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> i32;
    fn ShowWindow(window: Handle, command: i32) -> i32;
    fn SetForegroundWindow(window: Handle) -> i32;
    fn GetForegroundWindow() -> Handle;
    fn IsWindow(window: Handle) -> i32;
    fn IsIconic(window: Handle) -> i32;
    fn IsZoomed(window: Handle) -> i32;
    fn GetRegisteredRawInputDevices(devices: *mut RawDevice, count: *mut u32, size: u32) -> u32;
    fn PostMessageW(window: Handle, message: u32, wparam: usize, lparam: isize) -> i32;
    fn SendMessageW(window: Handle, message: u32, wparam: usize, lparam: isize) -> isize;
    fn MonitorFromWindow(window: Handle, flags: u32) -> Handle;
    fn GetMonitorInfoW(monitor: Handle, info: *mut MonitorInfo) -> i32;
    fn GetDpiForWindow(window: Handle) -> u32;
    fn ClientToScreen(window: Handle, point: *mut NativePoint) -> i32;
    fn PrintWindow(window: Handle, dc: Handle, flags: u32) -> i32;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThreadId() -> u32;
    fn GetCurrentProcess() -> Handle;
    fn CloseHandle(handle: Handle) -> i32;
}
#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
    fn GetTokenInformation(token: Handle, class: u32, value: *mut c_void, bytes: u32, returned: *mut u32) -> i32;
}
#[link(name = "gdi32")]
unsafe extern "system" {
    fn CreateCompatibleDC(dc: Handle) -> Handle;
    fn CreateDIBSection(dc: Handle, info: *const BitmapInfo, usage: u32, bits: *mut *mut c_void, section: Handle, offset: u32) -> Handle;
    fn SelectObject(dc: Handle, object: Handle) -> Handle;
    fn DeleteObject(object: Handle) -> i32;
    fn DeleteDC(dc: Handle) -> i32;
}

#[derive(Clone)] struct AdapterLog(Arc<Mutex<Option<String>>>);
impl Subscriber for AdapterLog {
    fn enabled(&self, _: &Metadata<'_>) -> bool { true }
    fn new_span(&self, _: &Attributes<'_>) -> Id { Id::from_u64(1) }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        struct Visitor(Option<String>);
        impl tracing::field::Visit for Visitor {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "adapter" { self.0 = Some(format!("{value:?}")); }
            }
        }
        let mut visitor = Visitor(None);
        event.record(&mut visitor);
        if let Some(adapter) = visitor.0 { *self.0.lock().expect("adapter log") = Some(adapter); }
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

unsafe extern "system" fn find_proxy(window: Handle, data: isize) -> i32 {
    let mut process = 0;
    // SAFETY: callback HWND belongs to the current thread; process points to writable storage.
    unsafe { GetWindowThreadProcessId(window, &mut process); }
    if process != std::process::id() { return 1; }
    let mut class = [0u16; 256];
    // SAFETY: owned-process HWND, writable class buffer of the advertised capacity.
    let len = unsafe { GetClassNameW(window, class.as_mut_ptr(), class.len() as i32) };
    if len > 0 && String::from_utf16_lossy(&class[..len as usize]) == "CrosspaneProxy" {
        // SAFETY: data is the live Vec passed synchronously to EnumThreadWindows below.
        unsafe {
            let windows = &mut *(data as *mut Vec<isize>);
            // The seventeenth own match is the bounded refusal sentinel; never inspect more.
            windows.push(window as isize);
            if windows.len() > 16 { return 0; }
        }
    }
    1
}
fn owned_window() -> isize {
    let mut windows = Vec::<isize>::new();
    // SAFETY: enumerate only this fixture's host thread; callback's Vec outlives synchronous call.
    unsafe { EnumThreadWindows(GetCurrentThreadId(), find_proxy, &mut windows as *mut _ as isize); }
    assert_eq!(windows.len(), 1, "exactly one fixture proxy");
    windows[0]
}
fn verify_owned(window: Handle) {
    let mut process = 0;
    // SAFETY: retained fixture HWND, output is writable; no foreign-window enumeration.
    unsafe { GetWindowThreadProcessId(window, &mut process); }
    assert_eq!(process, std::process::id(), "retained fixture HWND still owned");
}
fn on_host<T: Send + 'static>(handle: &HostHandle, action: impl FnOnce() -> T + Send + 'static) -> T {
    let (send, recv) = mpsc::sync_channel(1);
    handle.send(HostCommand::Run(Box::new(move || { let _ = send.send(action()); }))).expect("host action");
    recv.recv_timeout(Duration::from_secs(15)).expect("host action result")
}
fn wait(events: &mpsc::Receiver<HostEvent>, accept: impl FnMut(&HostEvent) -> bool) -> HostEvent {
    try_wait(events, Instant::now() + Duration::from_secs(15), accept).expect("event deadline")
}
// Returns None at `until`; only the caller knows whether that is a failure.
fn try_wait(events: &mpsc::Receiver<HostEvent>, until: Instant, mut accept: impl FnMut(&HostEvent) -> bool) -> Option<HostEvent> {
    loop {
        let event = match events.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok(event) => event,
            Err(mpsc::RecvTimeoutError::Timeout) => return None,
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("host event channel closed"),
        };
        match &event {
            HostEvent::OpenFailed { error, .. } => panic!("open failed: {error}"),
            HostEvent::Lost { .. } => panic!("fixture proxy lost"),
            HostEvent::Opened { size, scale, .. } | HostEvent::Resized { size, scale, .. } => eprintln!("geometry: {size:?}, scale={scale}"),
            HostEvent::Focus { focused, .. } => eprintln!("focus: {focused}"),
            HostEvent::Placed { visible, .. } => eprintln!("placement visibility: {visible}"),
            HostEvent::CloseRequested { .. } => eprintln!("close-returns: requested"),
            HostEvent::Presented { frames, .. } => eprintln!("presented: {frames}"),
            _ => {} // Never log keys, text, pointer contents, or foreign window data.
        }
        if accept(&event) { return Some(event); }
    }
}
fn screenshot(window: isize, output: PathBuf) {
    let window = window as Handle;
    verify_owned(window);
    let mut rect = Rect::default();
    // SAFETY: owned live HWND and writable client rectangle.
    assert_ne!(unsafe { GetClientRect(window, &mut rect) }, 0);
    let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
    assert!(width > 0 && height > 0 && width <= 2048 && height <= 2048);
    let info = BitmapInfo { size: 40, width, height: -height, planes: 1, bit_count: 32,
        compression: 0, image_size: 0, x_pixels: 0, y_pixels: 0, used: 0, important: 0, colours: [0] };
    let mut bits = std::ptr::null_mut();
    // SAFETY: create a memory DC only, never a desktop/window DC or a screen capture.
    let dc = unsafe { CreateCompatibleDC(std::ptr::null_mut()) };
    assert!(!dc.is_null());
    // SAFETY: valid memory DC and BITMAPINFO; output points to the created DIB's pixel allocation.
    let bitmap = unsafe { CreateDIBSection(dc, &info, 0, &mut bits, std::ptr::null_mut(), 0) };
    if bitmap.is_null() || bits.is_null() {
        // SAFETY: fixture-owned DC is no longer used.
        unsafe { DeleteDC(dc); }
        panic!("CreateDIBSection failed");
    }
    // SAFETY: valid fixture-owned GDI handles; original selection is restored before deletion.
    let previous = unsafe { SelectObject(dc, bitmap) };
    // SAFETY: captures only retained, PID-verified fixture HWND into the owned memory bitmap.
    let captured = unsafe { PrintWindow(window, dc, 3) };
    // SAFETY: 32-bit top-down DIB allocation is width * height * 4 bytes and remains alive here.
    let pixels = unsafe { std::slice::from_raw_parts(bits.cast::<u8>(), (width * height * 4) as usize) }.to_vec();
    // SAFETY: restore the prior object before deleting fixture-owned bitmap and memory DC.
    unsafe { SelectObject(dc, previous); DeleteObject(bitmap); DeleteDC(dc); }
    assert_ne!(captured, 0, "PrintWindow own client area");
    let center = ((height / 2 * width + width / 2) * 4) as usize;
    assert_eq!(&pixels[center..center + 3], &[30, 140, 220], "PrintWindow captured synthetic frame");
    let mut bmp = Vec::new();
    bmp.extend_from_slice(b"BM"); bmp.extend_from_slice(&(54u32 + pixels.len() as u32).to_le_bytes());
    bmp.extend_from_slice(&[0; 4]); bmp.extend_from_slice(&54u32.to_le_bytes());
    bmp.extend_from_slice(&40u32.to_le_bytes()); bmp.extend_from_slice(&width.to_le_bytes());
    bmp.extend_from_slice(&(-height).to_le_bytes()); bmp.extend_from_slice(&1u16.to_le_bytes());
    bmp.extend_from_slice(&32u16.to_le_bytes()); bmp.extend_from_slice(&[0; 24]); bmp.extend_from_slice(&pixels);
    std::fs::write(&output, bmp).expect("fixture screenshot");
    eprintln!("screenshot: {} ({width}x{height}); synthetic center verified", output.display());
}
fn close_owned(handle: &HostHandle, id: u64) {
    handle.send(HostCommand::Close { id }).expect("engine close");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let closed = on_host(handle, || {
            let mut remaining = Vec::<isize>::new();
            // SAFETY: only this fixture's host thread, with synchronous callback storage.
            unsafe { EnumThreadWindows(GetCurrentThreadId(), find_proxy, &mut remaining as *mut _ as isize); }
            remaining.is_empty()
        });
        if closed { break; }
        assert!(Instant::now() < deadline, "owned proxy destruction");
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn monitor_row(window: isize) -> HostMonitorMapping {
    let hwnd = window as Handle; verify_owned(hwnd);
    let mut info = MonitorInfo { size: std::mem::size_of::<MonitorInfo>() as u32,
        monitor: Rect::default(), work: Rect::default(), flags: 0, name: [0; 32] };
    // SAFETY: own retained HWND selects its monitor; writable correctly-sized monitor metadata.
    let dpi = unsafe { assert_ne!(GetMonitorInfoW(MonitorFromWindow(hwnd, 2), &mut info), 0); GetDpiForWindow(hwnd) };
    let len = info.name.iter().position(|c| *c == 0).unwrap_or(info.name.len());
    HostMonitorMapping { id: 42, native_id: String::from_utf16(&info.name[..len]).expect("GDI name"),
        physical_origin: PhysicalPosition::new(info.monitor.left, info.monitor.top),
        geometry: DisplayGeometry { physical_size: SizeMm::new(500.0, 300.0),
            pixel_size: PixelSize::new((info.monitor.right - info.monitor.left) as u32,
                (info.monitor.bottom - info.monitor.top) as u32), scale: f64::from(dpi) / 96.0,
            logical_origin: PointLogical::zero() } }
}
fn open_placed(handle: &HostHandle, events: &mpsc::Receiver<HostEvent>, id: u64, row: &HostMonitorMapping, origin: (i32, i32)) -> isize {
    let logical = row.geometry.device_to_logical(PointDevice::new(f64::from(origin.0), f64::from(origin.1)));
    handle.send(HostCommand::Open { id, title: "Crosspane placement fixture".into(), size: PixelSize::new(320, 240),
        accent: [80, 200, 60], place: Some(HostPlace { content: LogicalPosition::new(logical.x, logical.y) }) }).expect("placed open");
    wait(events, |e| matches!(e, HostEvent::Opened { id: opened, size, .. } if *opened == id && *size == PixelSize::new(320, 240)));
    let expected = PointDevice::new(f64::from(origin.0), f64::from(origin.1));
    wait(events, |e| matches!(e, HostEvent::Placed { id: placed, monitor: Some(42), origin, size, .. }
        if *placed == id && (origin.x - expected.x).abs() <= 1.0 && (origin.y - expected.y).abs() <= 1.0 && *size == PixelSize::new(320, 240)));
    let window = on_host(handle, owned_window);
    let physical = row.physical_origin;
    on_host(handle, move || {
        let hwnd = window as Handle; verify_owned(hwnd); let mut point = NativePoint { x: 0, y: 0 };
        // SAFETY: own retained fixture HWND and initialized writable client origin.
        assert_ne!(unsafe { ClientToScreen(hwnd, &mut point) }, 0);
        assert!((point.x - physical.x - origin.0).abs() <= 1 && (point.y - physical.y - origin.1).abs() <= 1);
    });
    eprintln!("owned logical placement verified: monitor=42, physical_size=320x240");
    window
}
fn check(handle: &HostHandle, events: &mpsc::Receiver<HostEvent>, adapter: &AdapterLog, output: PathBuf,
    mapping: Arc<Mutex<Option<Vec<HostMonitorMapping>>>>) {
    const ID: u64 = 4;
    handle.send(HostCommand::Open { id: 1, title: "Crosspane Windows fixture".into(),
        size: PixelSize::new(320, 240), accent: [80, 200, 60],
        place: Some(HostPlace { content: LogicalPosition::new(100.0, 100.0) }) }).expect("degraded open");
    wait(events, |e| matches!(e, HostEvent::Opened { id: 1, .. }));
    wait(events, |e| matches!(e, HostEvent::Placed { id: 1, monitor: None, .. }));
    let window = on_host(handle, owned_window);
    let row = on_host(handle, move || monitor_row(window));
    close_owned(handle, 1);
    *mapping.lock().expect("fixture mapping") = Some(vec![row.clone(), row.clone()]);
    handle.send(HostCommand::Open { id: 2, title: "Crosspane invalid mapping fixture".into(),
        size: PixelSize::new(320, 240), accent: [80, 200, 60],
        place: Some(HostPlace { content: LogicalPosition::new(100.0, 100.0) }) }).expect("invalid mapping open");
    wait(events, |e| matches!(e, HostEvent::Opened { id: 2, .. }));
    wait(events, |e| matches!(e, HostEvent::Placed { id: 2, monitor: None, .. }));
    close_owned(handle, 2);
    eprintln!("missing and invalid mapping: opened via OS placement");
    *mapping.lock().expect("fixture mapping") = Some(vec![row.clone()]);
    let _window = open_placed(handle, events, 3, &row, (80, 100));
    close_owned(handle, 3);
    let mut changed = row.clone(); changed.geometry.scale *= 2.0;
    changed.geometry.logical_origin = PointLogical::new(100.0, 50.0);
    *mapping.lock().expect("fixture mapping") = Some(vec![changed.clone()]);
    let window = open_placed(handle, events, ID, &changed, (120, 140));
    eprintln!("simulated mapping scale/origin change preserved physical pixels");
    *mapping.lock().expect("fixture mapping") = Some(vec![row.clone()]);
    for dpi in [192usize, 96usize] {
        on_host(handle, move || {
            let hwnd = window as Handle; verify_owned(hwnd); let mut rect = Rect::default();
            // SAFETY: own HWND only; synchronous owned WM_DPICHANGED with live RECT storage.
            unsafe { assert_ne!(GetWindowRect(hwnd, &mut rect), 0);
                SendMessageW(hwnd, 0x02e0, dpi | (dpi << 16), &rect as *const Rect as isize); }
        });
        wait(events, |e| {
            if let HostEvent::Resized { id: resized, size, scale } = e {
                if *resized == ID && (*scale - dpi as f64 / 96.0).abs() < 0.001 {
                    assert_eq!(*size, PixelSize::new(320, 240), "no interim automatic DPI resize report");
                    return true;
                }
            }
            false
        });
    }
    eprintln!("owned synthetic DPI messages: regenerated buffered writer preserved 320x240 without interim resize");
    on_host(handle, move || {
        let hwnd = window as Handle; verify_owned(hwnd); let mut rect = Rect::default();
        for dpi in [192usize, 288usize] {
            // SAFETY: batched synthetic DPI generations target only the retained fixture HWND;
            // each synchronous SendMessage consumes the live initialized RECT before reuse.
            unsafe { assert_ne!(GetWindowRect(hwnd, &mut rect), 0);
                SendMessageW(hwnd, 0x02e0, dpi | (dpi << 16), &rect as *const Rect as isize); }
        }
    });
    wait(events, |e| {
        if let HostEvent::Resized { id: resized, size, scale } = e {
            if *resized == ID {
                assert_eq!(*size, PixelSize::new(320, 240), "batched DPI has no interim resize report");
                return (*scale - 3.0).abs() < 0.001;
            }
        }
        false
    });
    handle.send(HostCommand::SetContentSize { id: ID, size: PixelSize::new(360, 280) }).expect("newer source baseline");
    on_host(handle, move || {
        let hwnd = window as Handle; verify_owned(hwnd); let mut rect = Rect::default();
        // SAFETY: own fixture HWND only, latest source request precedes this synchronous DPI change.
        unsafe { assert_ne!(GetWindowRect(hwnd, &mut rect), 0);
            SendMessageW(hwnd, 0x02e0, 96usize | (96usize << 16), &rect as *const Rect as isize); }
    });
    wait(events, |e| {
        if let HostEvent::Resized { id: resized, size, scale } = e {
            if *resized == ID && (*scale - 1.0).abs() < 0.001 {
                assert_eq!(*size, PixelSize::new(360, 280), "newer source supersedes DPI baseline");
                return true;
            }
        }
        false
    });
    eprintln!("owned batched DPI generations and newer source request superseded old baseline");

    on_host(handle, move || {
        let hwnd = window as Handle; verify_owned(hwnd);
        // SAFETY: style, owner and DPI context are read from only the retained fixture HWND.
        unsafe {
            let style = GetWindowLongPtrW(hwnd, -16) as u32;
            assert_eq!(style & 0x00cf0000, 0x00cf0000, "decorated, resizable overlapped window");
            assert!(GetWindow(hwnd, 4).is_null(), "frozen model opens independent toplevel");
            assert_ne!(AreDpiAwarenessContextsEqual(GetWindowDpiAwarenessContext(hwnd), -4isize as Handle), 0, "per-monitor-v2 DPI");
        }
    });
    let adapter = adapter.0.lock().expect("adapter log").clone().expect("actual surface adapter report");
    assert!(adapter.contains("Dx12"), "DX12 surface adapter: {adapter}");
    eprintln!("actual proxy adapter: {adapter}");
    let size = PixelSize::new(320, 240);
    handle.send(HostCommand::Frame { id: ID, size, pixels: Arc::from([30, 140, 220, 255].repeat((size.width * size.height) as usize)),
        dirty: vec![PixelRect::new(point2(0, 0), point2(320, 240))] }).expect("frame");
    wait(events, |e| matches!(e, HostEvent::Presented { .. }));
    // Native destination resize, independent of the source's corrective SetContentSize.
    on_host(handle, move || {
        let hwnd = window as Handle; verify_owned(hwnd);
        let (mut outer, mut inner) = (Rect::default(), Rect::default());
        // SAFETY: own HWND only; preserve frame margins and change only its own content dimensions.
        unsafe {
            assert_ne!(GetWindowRect(hwnd, &mut outer), 0); assert_ne!(GetClientRect(hwnd, &mut inner), 0);
            assert_ne!(SetWindowPos(hwnd, std::ptr::null_mut(), 0, 0,
                400 + outer.right - outer.left - inner.right,
                300 + outer.bottom - outer.top - inner.bottom, 0x0016), 0);
        }
    });
    wait(events, |e| matches!(e, HostEvent::Resized { size, .. } if *size == PixelSize::new(400, 300)));
    handle.send(HostCommand::SetContentSize { id: ID, size: PixelSize::new(360, 280) }).expect("source correction");
    wait(events, |e| matches!(e, HostEvent::Resized { size, .. } if *size == PixelSize::new(360, 280)));
    let before_monitor = on_host(handle, move || {
        verify_owned(window as Handle);
        // SAFETY: query only retained own window's monitor.
        unsafe { MonitorFromWindow(window as Handle, 2) as isize }
    });
    handle.send(HostCommand::SetFullscreen { id: ID, fullscreen: true }).expect("fullscreen");
    wait(events, |e| matches!(e, HostEvent::Fullscreen { id: ID, fullscreen: true }));
    on_host(handle, move || {
        let hwnd = window as Handle; verify_owned(hwnd); let mut rect = Rect::default();
        let mut info = MonitorInfo { size: std::mem::size_of::<MonitorInfo>() as u32,
            monitor: Rect::default(), work: Rect::default(), flags: 0, name: [0; 32] };
        // SAFETY: own HWND and its monitor metadata only; writable initialized output.
        unsafe { let monitor = MonitorFromWindow(hwnd, 2); assert_eq!(monitor as isize, before_monitor);
            assert_ne!(GetMonitorInfoW(monitor, &mut info), 0); assert_ne!(GetWindowRect(hwnd, &mut rect), 0); }
        assert_eq!((rect.left, rect.top, rect.right, rect.bottom),
            (info.monitor.left, info.monitor.top, info.monitor.right, info.monitor.bottom));
    });
    handle.send(HostCommand::SetFullscreen { id: ID, fullscreen: false }).expect("exit fullscreen");
    wait(events, |e| matches!(e, HostEvent::Resized { id: ID, size, .. } if *size == PixelSize::new(360, 280)));
    eprintln!("owned fullscreen retained current native monitor and restored 360x280");
    // Fullscreen reconfigures the swapchain; observe a fresh own frame before PrintWindow.
    while events.try_recv().is_ok() {}
    let size = PixelSize::new(360, 280);
    handle.send(HostCommand::Frame { id: ID, size,
        pixels: Arc::from([30, 140, 220, 255].repeat((size.width * size.height) as usize)),
        dirty: vec![PixelRect::new(point2(0, 0), point2(360, 280))] }).expect("capture frame");
    wait(events, |e| matches!(e, HostEvent::Presented { id: ID, .. }));
    on_host(handle, move || screenshot(window, output));
    handle.send(HostCommand::SetCursor { id: ID, size: PixelSize::new(16, 16), hotspot: (2, 3),
        pixels: Arc::from([0, 0, 255, 255].repeat(256)) }).expect("custom cursor");
    handle.send(HostCommand::DefaultCursor { id: ID }).expect("default cursor");
    // Proxies open passively, so one refusal-tolerant foreground request comes before the minimise.
    let requested = on_host(handle, move || {
        let hwnd = window as Handle; verify_owned(hwnd);
        // SAFETY: one foreground request for the retained fixture HWND only; refusal is tolerated.
        unsafe { SetForegroundWindow(hwnd) != 0 }
    });
    let gained = requested && try_wait(events, Instant::now() + Duration::from_secs(15),
        |e| matches!(e, HostEvent::Focus { id: ID, focused: true })).is_some();
    if gained {
        on_host(handle, move || {
            let hwnd = window as Handle; verify_owned(hwnd);
            // SAFETY: minimize only fixture HWND; no injection or foreign focus target.
            unsafe { ShowWindow(hwnd, 6); }
        });
        let (mut focus_lost, mut hidden) = (false, false);
        wait(events, |event| {
            focus_lost |= matches!(event, HostEvent::Focus { focused: false, .. });
            hidden |= matches!(event, HostEvent::Placed { visible: false, .. });
            focus_lost && hidden
        });
        on_host(handle, move || {
            let hwnd = window as Handle; verify_owned(hwnd);
            // SAFETY: restore only the retained fixture HWND; no further focus request.
            unsafe { ShowWindow(hwnd, 9); }
        });
        eprintln!("focus gained; minimise then produced focus loss and hidden placement");
    } else {
        // The minimise is skipped here: its only check is the focus loss it would have to produce.
        eprintln!("[U] foreground refused; minimise-loss not observed");
        if requested { eprintln!("no focus-true within the deadline after an accepted request"); }
    }
    on_host(handle, move || {
        let hwnd = window as Handle; verify_owned(hwnd);
        // SAFETY: WM_CLOSE is posted only to the retained fixture window.
        assert_ne!(unsafe { PostMessageW(hwnd, 0x0010, 0, 0) }, 0);
    });
    wait(events, |e| matches!(e, HostEvent::CloseRequested { .. }));
    on_host(handle, move || verify_owned(window as Handle)); // close returns; host did not destroy source.
    handle.send(HostCommand::Close { id: ID }).expect("engine close acknowledgement");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let closed = on_host(handle, || {
            let mut remaining = Vec::<isize>::new();
            // SAFETY: current fixture thread only; live callback storage for synchronous enumeration.
            unsafe { EnumThreadWindows(GetCurrentThreadId(), find_proxy, &mut remaining as *mut _ as isize); }
            remaining.is_empty()
        });
        if closed { break; }
        assert!(Instant::now() < deadline, "close acknowledgement eventually destroyed own proxy");
        // Winit defers native destruction until it can dispatch the next Windows message.
        std::thread::sleep(Duration::from_millis(20));
    }
    eprintln!("owned Windows proxy probe: passed");
}

#[derive(Default)] struct RowEvents {
    focus: [bool; 3], gains: [u32; 3], losses: [u32; 3],
    down: [u32; 3], up: [u32; 3], sizes: [Option<PixelSize>; 3], count: u32,
}
impl RowEvents {
    fn observe(&mut self, event: HostEvent) {
        self.count += 1;
        assert!(self.count <= 512, "fixed own row event cap");
        match event {
            HostEvent::Opened { id, size, .. } | HostEvent::Resized { id, size, .. } => {
                assert!((1..=2).contains(&id)); self.sizes[id as usize] = Some(size);
            }
            HostEvent::Focus { id, focused } => {
                assert!((1..=2).contains(&id)); let i = id as usize;
                assert_ne!(self.focus[i], focused, "one observed focus event per transition");
                if !focused { assert_eq!(self.down[i], self.up[i], "held key releases precede focus false"); }
                self.focus[i] = focused;
                if focused { self.gains[i] += 1; } else { self.losses[i] += 1; }
            }
            HostEvent::Key { id, down, .. } => {
                assert!((1..=2).contains(&id)); let i = id as usize;
                if down { assert!(self.focus[i], "key down requires observed focus"); self.down[i] += 1; }
                else { self.up[i] += 1; }
            }
            HostEvent::OpenFailed { .. } | HostEvent::Lost { .. } | HostEvent::CloseRequested { .. } => panic!("owned row backend/interference failure"),
            _ => {} // No usage/text/pointer or unknown native strings are emitted.
        }
    }
}
fn row_host<T: Send + 'static>(handle: &HostHandle, until: Instant, action: impl FnOnce() -> T + Send + 'static) -> T {
    assert!(Instant::now() < until, "row deadline before admission");
    let (send, recv) = mpsc::sync_channel(1);
    handle.send(HostCommand::Run(Box::new(move || { let _ = send.send(action()); }))).expect("own row host action");
    recv.recv_timeout(until.saturating_duration_since(Instant::now())).expect("own row action deadline")
}
fn row_wait(events: &mpsc::Receiver<HostEvent>, stats: &mut RowEvents, until: Instant, mut ready: impl FnMut(&RowEvents) -> bool) {
    loop {
        if ready(stats) { return; }
        let event = events.recv_timeout(until.saturating_duration_since(Instant::now())).expect("own row event deadline");
        stats.observe(event);
    }
}
fn row_drain(events: &mpsc::Receiver<HostEvent>, stats: &mut RowEvents, until: Instant) {
    let quiet = (Instant::now() + Duration::from_millis(150)).min(until);
    loop {
        assert!(Instant::now() < until, "own row deadline");
        match events.recv_timeout(quiet.saturating_duration_since(Instant::now())) {
            Ok(event) => stats.observe(event),
            Err(mpsc::RecvTimeoutError::Timeout) => return,
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("own host disconnected"),
        }
        if Instant::now() >= quiet { return; }
    }
}
fn row_windows() -> Vec<isize> {
    let mut windows = Vec::new();
    // SAFETY: only the current owning host thread; the initialized Vec outlives synchronous callback.
    unsafe { EnumThreadWindows(GetCurrentThreadId(), find_proxy, &mut windows as *mut _ as isize); }
    assert!(windows.len() <= 16, "own-thread enumeration cap"); windows
}
fn row_verify(window: isize) {
    let hwnd = window as Handle; let mut pid = 0;
    // SAFETY: handle identity query only, initialized writable PID; no foreign metadata read.
    let tid = unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    assert!(pid == std::process::id(), "own PID identity refused");
    // SAFETY: current owning host thread pseudo identity and authenticated own HWND only.
    unsafe { assert!(tid == GetCurrentThreadId(), "own TID identity refused"); assert_ne!(IsWindow(hwnd), 0); }
    let mut class = [0u16; 64];
    // SAFETY: only after exact own PID/thread/liveness proof; fixed writable class buffer.
    let len = unsafe { GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32) };
    assert!(len > 0 && len < class.len() as i32);
    assert!(&class[..len as usize] == "CrosspaneProxy".encode_utf16().collect::<Vec<_>>().as_slice(), "own class identity refused");
}
fn row_open(handle: &HostHandle, events: &mpsc::Receiver<HostEvent>, stats: &mut RowEvents, until: Instant, id: u64) -> isize {
    let before = row_host(handle, until, row_windows);
    handle.send(HostCommand::Open { id, title: "Owned W2.4c fixture".into(), size: PixelSize::new(320, 240),
        accent: [40, 120, 200], place: Some(HostPlace { content: LogicalPosition::new(100.0 + id as f64 * 40.0, 100.0) }) }).expect("own row open");
    row_wait(events, stats, until, |s| s.sizes[id as usize].is_some());
    row_host(handle, until, move || {
        let after = row_windows(); let created: Vec<_> = after.into_iter().filter(|h| !before.contains(h)).collect();
        assert_eq!(created.len(), 1, "unique newly created own HWND"); row_verify(created[0]); created[0]
    })
}
fn row_activate(handle: &HostHandle, until: Instant, hwnd: isize) -> bool {
    row_host(handle, until, move || {
        row_verify(hwnd);
        // SAFETY: one documented activation attempt only on this host's freshly corroborated own HWND.
        let requested = unsafe { SetForegroundWindow(hwnd as Handle) } != 0;
        // SAFETY: opaque foreground equality only; no metadata is read from any foreign HWND.
        requested && unsafe { GetForegroundWindow() } == hwnd as Handle
    })
}
fn row_foreground(handle: &HostHandle, until: Instant, hwnd: isize) {
    row_host(handle, until, move || {
        row_verify(hwnd);
        // SAFETY: only opaque equality with the authenticated own HWND; no foreign metadata.
        assert!((unsafe { GetForegroundWindow() }) == hwnd as Handle, "own foreground changed");
    });
}
fn row_show(handle: &HostHandle, until: Instant, hwnd: isize, command: i32) {
    row_host(handle, until, move || {
        row_verify(hwnd);
        // SAFETY: fixed own fixture action; return value describes previous visibility, not success.
        unsafe { ShowWindow(hwnd as Handle, command); }
    });
}
fn row_client(handle: &HostHandle, until: Instant, hwnd: isize) -> (PixelSize, bool, bool) {
    row_host(handle, until, move || {
        row_verify(hwnd); let mut rect = Rect::default();
        // SAFETY: exact own HWND after identity proof and correctly sized initialized output.
        unsafe { assert_ne!(GetClientRect(hwnd as Handle, &mut rect), 0); }
        assert!(rect.right >= rect.left && rect.bottom >= rect.top);
        let size = PixelSize::new((rect.right - rect.left) as u32, (rect.bottom - rect.top) as u32);
        // SAFETY: state reads only from the authenticated own fixture HWND.
        (size, unsafe { IsZoomed(hwnd as Handle) } != 0, unsafe { IsIconic(hwnd as Handle) } != 0)
    })
}
fn row_key(handle: &HostHandle, until: Instant, hwnd: isize, down: bool) {
    row_host(handle, until, move || {
        row_verify(hwnd);
        // SAFETY: fixed test-owned key/scancode message only to our own HWND, never physical input.
        assert_ne!(unsafe { PostMessageW(hwnd as Handle, if down { 0x0100 } else { 0x0101 }, 0x41,
            (1isize | (0x1eisize << 16)) | if down { 0 } else { (1isize << 30) | (1isize << 31) }) }, 0);
    });
}
fn row_close(handle: &HostHandle, until: Instant, hwnd: isize, id: u64) {
    row_host(handle, until, move || row_verify(hwnd));
    handle.send(HostCommand::Close { id }).expect("own row close");
    loop {
        let gone = row_host(handle, until, move || {
            // SAFETY: existence only for the exact previously retained own HWND; no adoption or metadata.
            (unsafe { IsWindow(hwnd as Handle) }) == 0
        });
        if gone { break; }
        assert!(Instant::now() < until, "own HWND retirement deadline");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn fixed_row(handle: &HostHandle, events: &mpsc::Receiver<HostEvent>, row: &str) -> bool {
    let until = Instant::now() + Duration::from_secs(30); let mut stats = RowEvents::default();
    let a = row_open(handle, events, &mut stats, until, 1);
    if !row_activate(handle, until, a) { row_close(handle, until, a, 1); println!("W24C_ROW_U code=1"); return false; }
    row_wait(events, &mut stats, until, |s| s.focus[1]);
    let b = row_open(handle, events, &mut stats, until, 2);
    row_drain(events, &mut stats, until); row_foreground(handle, until, a);
    assert!(!stats.focus[2], "passive second proxy never activates");
    if row == "focus" {
        row_key(handle, until, a, true); row_wait(events, &mut stats, until, |s| s.down[1] == 1);
        if !row_activate(handle, until, b) { row_close(handle, until, b, 2); row_close(handle, until, a, 1); println!("W24C_ROW_U code=1"); return false; }
        row_wait(events, &mut stats, until, |s| !s.focus[1] && s.focus[2]);
        assert_eq!(stats.up[1], 1, "held release precedes loss");
        row_key(handle, until, a, true); row_key(handle, until, a, false); row_drain(events, &mut stats, until);
        assert_eq!(stats.down[1], 1, "unfocused queued key refused");
        row_show(handle, until, b, 6); row_wait(events, &mut stats, until, |s| !s.focus[2]);
        row_key(handle, until, b, true); row_key(handle, until, b, false); row_drain(events, &mut stats, until);
        assert_eq!(stats.down[2], 0, "minimized queued key refused");
        row_show(handle, until, b, 9);
        if !row_activate(handle, until, b) { row_close(handle, until, b, 2); row_close(handle, until, a, 1); println!("W24C_ROW_U code=1"); return false; }
        row_wait(events, &mut stats, until, |s| s.focus[2]);
        assert_eq!((stats.gains[2], stats.losses[2]), (2, 1));
        assert_eq!(stats.gains[1], stats.losses[1] + u32::from(stats.focus[1]), "all observed own A transitions balance");
    } else {
        assert_eq!(row, "no-theft");
        let normal = PixelSize::new(360, 260);
        handle.send(HostCommand::SetContentSize { id: 2, size: normal }).expect("own normal resize");
        row_wait(events, &mut stats, until, |s| s.sizes[2] == Some(normal)); row_foreground(handle, until, a);
        row_show(handle, until, b, 3); row_drain(events, &mut stats, until);
        let maximized = row_client(handle, until, b); assert!(maximized.1 && !maximized.2);
        if !row_activate(handle, until, a) { row_close(handle, until, b, 2); row_close(handle, until, a, 1); println!("W24C_ROW_U code=1"); return false; }
        handle.send(HostCommand::SetContentSize { id: 2, size: PixelSize::new(720, 380) }).expect("ignored maximized resize");
        assert_eq!(row_client(handle, until, b), maximized); row_drain(events, &mut stats, until); row_foreground(handle, until, a);
        assert_eq!(stats.sizes[2], Some(maximized.0), "reported maximized geometry is actual");
        row_show(handle, until, b, 6); row_drain(events, &mut stats, until);
        let minimized = row_client(handle, until, b); assert!(minimized.2);
        handle.send(HostCommand::SetContentSize { id: 2, size: PixelSize::new(740, 400) }).expect("ignored minimized resize");
        assert_eq!(row_client(handle, until, b), minimized); row_drain(events, &mut stats, until); row_foreground(handle, until, a);
        assert_eq!(stats.sizes[2], Some(minimized.0), "reported minimized geometry is actual");
        row_show(handle, until, b, 9); row_drain(events, &mut stats, until);
        // Restoring a minimized maximized window may first restore maximized state.
        if row_client(handle, until, b).1 { row_show(handle, until, b, 9); }
        row_wait(events, &mut stats, until, |s| s.sizes[2] == Some(normal));
        let restored = row_client(handle, until, b); assert_eq!(restored, (normal, false, false));
        if !row_activate(handle, until, a) { row_close(handle, until, b, 2); row_close(handle, until, a, 1); println!("W24C_ROW_U code=1"); return false; }
        let final_size = PixelSize::new(380, 280);
        handle.send(HostCommand::SetContentSize { id: 2, size: final_size }).expect("own final normal resize");
        row_wait(events, &mut stats, until, |s| s.sizes[2] == Some(final_size)); row_foreground(handle, until, a);
        assert_eq!(row_client(handle, until, b).0, final_size);
    }
    row_close(handle, until, b, 2); row_close(handle, until, a, 1); row_drain(events, &mut stats, until);
    println!("W24C_ROW_PASS row={} gains_a={} losses_a={} gains_b={} losses_b={} downs_a={} ups_a={} downs_b={} ups_b={} events={} hwnd_retired=2",
        row, stats.gains[1], stats.losses[1], stats.gains[2], stats.losses[2], stats.down[1], stats.up[1], stats.down[2], stats.up[2], stats.count);
    true
}
fn row_limited() {
    let mut token = std::ptr::null_mut();
    // SAFETY: current borrowed process pseudo-handle and initialized token output; query rights only.
    assert_ne!(unsafe { OpenProcessToken(GetCurrentProcess(), 8, &mut token) }, 0);
    struct Token(Handle);
    impl Drop for Token { fn drop(&mut self) {
        // SAFETY: exactly one owned valid token handle, never a pseudo-handle.
        unsafe { CloseHandle(self.0); }
    } }
    let token = Token(token);
    // Exact generated TOKEN_INFORMATION_CLASS constants: Elevation=20, UIAccess=26.
    // 27 is TokenMandatoryPolicy and does not measure UIAccess.
    for class in [20u32, 26u32] {
        let mut value = 1u32; let mut returned = 0u32;
        // SAFETY: fixed DWORD TokenElevation/TokenUIAccess layout and exact writable size.
        assert_ne!(unsafe { GetTokenInformation(token.0, class, (&mut value as *mut u32).cast(), 4, &mut returned) }, 0);
        assert_eq!(returned, 4); assert_eq!(value, 0, "non-elevated/non-UIAccess fixture required");
    }
}
fn row_no_raw_registration() {
    // RAWINPUTDEVICE is two WORDs, one DWORD and one HWND, naturally aligned under repr(C).
    assert_eq!(std::mem::size_of::<RawDevice>(), if cfg!(target_pointer_width = "64") { 16 } else { 12 });
    let mut count = 0u32;
    // SAFETY: own-process registration metadata only; documented null-buffer size query.
    let result = unsafe { GetRegisteredRawInputDevices(std::ptr::null_mut(), &mut count, std::mem::size_of::<RawDevice>() as u32) };
    assert_ne!(result, u32::MAX); assert!(count <= 16, "own registration count cap");
    if count != 0 {
        let mut devices = vec![RawDevice::default(); count as usize];
        // SAFETY: exactly count initialized generated-equivalent repr(C) registration entries;
        // own-process metadata only, no raw packet/device name/owner process is queried.
        let copied = unsafe { GetRegisteredRawInputDevices(devices.as_mut_ptr(), &mut count, std::mem::size_of::<RawDevice>() as u32) };
        assert_ne!(copied, u32::MAX); assert!(copied as usize <= devices.len());
        assert!(devices[..copied as usize].iter().all(|d| d.page != 1 || !matches!(d.usage, 0 | 2 | 6)), "host Never must remove keyboard/mouse registrations before dispatch");
    }
    println!("W24C_NO_RAW keyboard=0 mouse=0");
}
fn run_row(row: String, nonce: String) {
    assert!(matches!(row.as_str(), "focus" | "no-theft"));
    assert!(nonce.len() == 32 && nonce.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    row_limited();
    let (finished, done) = mpsc::sync_channel(1);
    let watchdog = std::thread::spawn(move || {
        if done.recv_timeout(Duration::from_secs(60)).is_err() { std::process::exit(2); }
    });
    use std::io::Read;
    let mut start = Vec::new(); std::io::stdin().lock().take(7).read_to_end(&mut start).expect("private START pipe");
    assert_eq!(start, b"START\n", "created inherited private protocol only");
    let (host, handle) = ProxyHost::new().expect("owned main-thread row host");
    row_no_raw_registration(); // BEFORE run/dispatch or any visible fixture window.
    let (send, events) = mpsc::sync_channel(512);
    let worker = std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fixed_row(&handle, &events, &row)));
        let _ = handle.send(HostCommand::Shutdown); result
    });
    let queue_failed = Arc::new(std::sync::atomic::AtomicBool::new(false)); let callback_failed = queue_failed.clone();
    host.run(Box::new(move |event| {
        if send.try_send(event).is_err() { callback_failed.store(true, std::sync::atomic::Ordering::Release); }
    })).expect("owned row host loop");
    let result = worker.join().expect("owned row worker");
    assert!(!queue_failed.load(std::sync::atomic::Ordering::Acquire), "bounded own event queue");
    finished.send(()).expect("own watchdog retirement"); watchdog.join().expect("joined own watchdog");
    match result { Ok(true) => {}, Ok(false) => std::process::exit(3), Err(_) => std::process::exit(1) }
}

fn main() {
    if std::env::var("CROSSPANE_WINDOWS_PROXY_GUI").as_deref() != Ok("1") {
        eprintln!("SKIP: run only through limited win-gui.sh with CROSSPANE_WINDOWS_PROXY_GUI=1"); return;
    }
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|s| s == "--row") {
        assert!(args.len() == 4 && args[2] == "--run", "fixed own row selectors");
        run_row(args[1].clone(), args[3].clone()); return;
    }
    let output = std::env::args_os().nth(1).map(PathBuf::from).expect("owned screenshot output path");
    let adapter = AdapterLog(Arc::new(Mutex::new(None)));
    tracing::subscriber::set_global_default(adapter.clone()).expect("fixture subscriber");
    let (mut host, handle) = ProxyHost::new().expect("main-thread host");
    let mapping = Arc::new(Mutex::new(None::<Vec<HostMonitorMapping>>));
    let provider = mapping.clone();
    host.set_placement_mapping(Arc::new(move || provider.lock().ok().and_then(|rows| rows.clone())));
    let (send, events) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(&handle, &events, &adapter, output, mapping)));
        let _ = handle.send(HostCommand::Shutdown); result.is_ok()
    });
    host.run(Box::new(move |event| { let _ = send.send(event); })).expect("host event loop");
    if !worker.join().expect("fixture worker") { std::process::exit(1); }
}
"#;

fn library(deps: &Path, name: &str) -> Result<std::path::PathBuf> {
    std::fs::read_dir(deps)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_stem().is_some_and(|stem| {
                let stem = stem.to_string_lossy();
                stem.starts_with(&format!("{name}-")) || stem.starts_with(&format!("lib{name}-"))
            }) && path.extension().is_some_and(|ext| ext == "rlib")
        })
        .max_by_key(|path| {
            path.metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
        })
        .with_context(|| format!("missing {name} test dependency"))
}

#[test]
fn proxy_windows_fixture_compiles_without_running_gui() -> Result<()> {
    let executable = std::env::current_exe()?;
    let deps = executable.parent().context("test deps directory")?;
    let source = deps.join("proxy-windows-driver.rs");
    let driver = deps.join("proxy-windows-driver.exe");
    std::fs::write(&source, DRIVER)?;
    let mut compiler = Command::new("rustc");
    compiler
        .args([
            "--edition=2024",
            "-Dwarnings",
            "--crate-name",
            "proxy_windows_driver",
            "-L",
        ])
        .arg(format!("dependency={}", deps.display()));
    // Reuse native link search paths emitted by Opus and Windows target build scripts.
    for build in std::fs::read_dir(deps.parent().context("debug directory")?.join("build"))? {
        let build = build?;
        if let Ok(output) = std::fs::read_to_string(build.path().join("output")) {
            for line in output.lines() {
                if let Some(search) = line.strip_prefix("cargo:rustc-link-search=native=") {
                    compiler.arg("-L").arg(format!("native={search}"));
                }
            }
        }
    }
    for name in ["crosspane_render", "crosspane_types", "tracing", "winit"] {
        compiler
            .arg("--extern")
            .arg(format!("{name}={}", library(deps, name)?.display()));
    }
    let output = compiler.arg(&source).arg("-o").arg(&driver).output()?;
    std::fs::remove_file(source)?;
    ensure!(
        output.status.success(),
        "driver compile failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    eprintln!("owned Windows fixture compiled: {}", driver.display());
    Ok(())
}
