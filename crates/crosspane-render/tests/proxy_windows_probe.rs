#![cfg(target_os = "windows")]
//! Compile an owned fixture; run its executable only through the lead's limited win-gui.sh.

use std::{path::Path, process::Command};

use anyhow::{Context, Result, ensure};

const DRIVER: &str = r#"
use std::{ffi::c_void, path::PathBuf, sync::{Arc, Mutex, mpsc}, time::{Duration, Instant}};
use crosspane_render::proxy::{HostCommand, HostEvent, HostHandle, ProxyHost};
use crosspane_types::geom::{PixelRect, PixelSize, euclid::point2};
use tracing::{Event, Metadata, Subscriber, span::{Attributes, Id, Record}};

type Handle = *mut c_void;
#[repr(C)] #[derive(Default)] struct Rect { left: i32, top: i32, right: i32, bottom: i32 }
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
    fn PostMessageW(window: Handle, message: u32, wparam: usize, lparam: isize) -> i32;
    fn PrintWindow(window: Handle, dc: Handle, flags: u32) -> i32;
}
#[link(name = "kernel32")]
unsafe extern "system" { fn GetCurrentThreadId() -> u32; }
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
        unsafe { (&mut *(data as *mut Vec<isize>)).push(window as isize); }
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
fn wait(events: &mpsc::Receiver<HostEvent>, mut accept: impl FnMut(&HostEvent) -> bool) -> HostEvent {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let event = events.recv_timeout(deadline.saturating_duration_since(Instant::now())).expect("event deadline");
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
        if accept(&event) { return event; }
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
fn check(handle: &HostHandle, events: &mpsc::Receiver<HostEvent>, adapter: &AdapterLog, output: PathBuf) {
    const ID: u64 = 1;
    handle.send(HostCommand::Open { id: ID, title: "Crosspane Windows fixture".into(),
        size: PixelSize::new(320, 240), accent: [80, 200, 60],
        place: None }).expect("open");
    wait(events, |e| matches!(e, HostEvent::Opened { .. }));
    let window = on_host(handle, owned_window);
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
    on_host(handle, move || screenshot(window, output));
    handle.send(HostCommand::SetCursor { id: ID, size: PixelSize::new(16, 16), hotspot: (2, 3),
        pixels: Arc::from([0, 0, 255, 255].repeat(256)) }).expect("custom cursor");
    handle.send(HostCommand::DefaultCursor { id: ID }).expect("default cursor");
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
    let focused = on_host(handle, move || {
        let hwnd = window as Handle; verify_owned(hwnd);
        // SAFETY: restore and request focus only for the retained fixture HWND.
        unsafe { ShowWindow(hwnd, 9); SetForegroundWindow(hwnd) != 0 }
    });
    if focused {
        wait(events, |e| matches!(e, HostEvent::Focus { focused: true, .. }));
    } else {
        eprintln!("restore focus: OS refused SetForegroundWindow; initial focus and minimize loss verified");
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
fn main() {
    if std::env::var("CROSSPANE_WINDOWS_PROXY_GUI").as_deref() != Ok("1") {
        eprintln!("SKIP: run only through limited win-gui.sh with CROSSPANE_WINDOWS_PROXY_GUI=1"); return;
    }
    let output = std::env::args_os().nth(1).map(PathBuf::from).expect("owned screenshot output path");
    let adapter = AdapterLog(Arc::new(Mutex::new(None)));
    tracing::subscriber::set_global_default(adapter.clone()).expect("fixture subscriber");
    let (host, handle) = ProxyHost::new().expect("main-thread host");
    let (send, events) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(&handle, &events, &adapter, output)));
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
    for name in ["crosspane_render", "crosspane_types", "tracing"] {
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
