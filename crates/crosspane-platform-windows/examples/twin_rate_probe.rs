//! Read-only twin-rate probe: which DXGI adapter owns the twin display, and how fast WGC delivers
//! frames of a window on a normal monitor versus the same window moved onto the twin.
//!
//! ```text
//! cargo run -p crosspane-platform-windows --features gpu --release --example twin_rate_probe
//! ```
//!
//! It touches only its own child process window (spawned by re-running this binary with
//! `--child`). Nothing is created, removed or moved except that window. `TWIN_RATE_SECS`
//! (seconds per phase, default 6) tunes the run; `TWIN_RATE_SIZE` (`WxH`, default 640x360) sizes
//! the child window; `TWIN_RATE_TWIN_POS` (`x,y`) overrides the twin placement (default: the
//! twin's bottom-right corner).
#![allow(unsafe_code, clippy::unwrap_used, clippy::expect_used)]

#[cfg(all(windows, feature = "gpu"))]
mod imp {
    use crosspane_platform::{
        CaptureTarget, EventSink, FrameCapture, FrameEvent, IoGate, WindowSource,
    };
    use crosspane_platform_windows::{
        displays::WindowsDisplays, frame_capture::WindowsFrameCapture, window::WindowsWindowSource,
    };
    use std::ptr::{null, null_mut};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
    use windows_sys::Win32::{
        Devices::Display::*,
        Foundation::{ERROR_SUCCESS, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{HiDpi::*, WindowsAndMessaging::*},
    };

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }
    fn text(units: &[u16]) -> String {
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        String::from_utf16_lossy(&units[..end])
    }
    fn env_num(name: &str, default: u64) -> u64 {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }
    fn env_pair(name: &str) -> Option<(i32, i32)> {
        let value = std::env::var(name).ok()?;
        let (a, b) = value.split_once(['x', ','])?;
        Some((a.parse().ok()?, b.parse().ok()?))
    }

    static TICK: AtomicUsize = AtomicUsize::new(0);
    static PAINTS: AtomicUsize = AtomicUsize::new(0);

    fn stats_path() -> std::path::PathBuf {
        std::env::temp_dir().join("twin-rate-stats.txt")
    }

    unsafe extern "system" fn proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // SAFETY: only this window, its client DC and GDI objects it frees.
        unsafe {
            match message {
                WM_TIMER => {
                    if wparam == 3 {
                        let paints = PAINTS.swap(0, Ordering::Relaxed);
                        let _ = std::fs::write(stats_path(), format!("{paints}"));
                        return 0;
                    }
                    TICK.fetch_add(1, Ordering::Relaxed);
                    InvalidateRect(hwnd, null(), 0);
                    0
                }
                WM_PAINT => {
                    PAINTS.fetch_add(1, Ordering::Relaxed);
                    let mut paint: PAINTSTRUCT = std::mem::zeroed();
                    let dc = BeginPaint(hwnd, &mut paint);
                    let mut rect: RECT = std::mem::zeroed();
                    GetClientRect(hwnd, &mut rect);
                    let black = CreateSolidBrush(0x0010_1010);
                    FillRect(dc, &rect, black);
                    DeleteObject(black);
                    let font = CreateFontW(
                        18,
                        0,
                        0,
                        0,
                        400,
                        0,
                        0,
                        0,
                        0,
                        0,
                        0,
                        ANTIALIASED_QUALITY as u32,
                        0,
                        wide("Consolas").as_ptr(),
                    );
                    let old = SelectObject(dc, font);
                    SetBkMode(dc, TRANSPARENT as i32);
                    SetTextColor(dc, 0x00e0_e0e0);
                    let tick = TICK.load(Ordering::Relaxed);
                    let rows = (rect.bottom / 20).max(1) as usize;
                    for row in 0..rows {
                        let line: Vec<u16> =
                            format!("{:08}  twin rate probe line {row}", tick.wrapping_sub(row))
                                .encode_utf16()
                                .collect();
                        TextOutW(dc, 8, 4 + row as i32 * 20, line.as_ptr(), line.len() as i32);
                    }
                    SelectObject(dc, old);
                    DeleteObject(font);
                    EndPaint(hwnd, &paint);
                    0
                }
                WM_DESTROY => {
                    PostQuitMessage(0);
                    0
                }
                _ => DefWindowProcW(hwnd, message, wparam, lparam),
            }
        }
    }

    /// The child process: one animated window for `lifetime`, then it closes itself.
    fn run_child(title: &str, size: (i32, i32), lifetime: Duration, flip: bool) {
        // SAFETY: a private class and window owned and pumped by this thread.
        unsafe {
            SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let instance = GetModuleHandleW(null());
            let class = wide("Crosspane.TwinRate");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(proc),
                hInstance: instance,
                lpszClassName: class.as_ptr(),
                ..std::mem::zeroed()
            };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                wide(title).as_ptr(),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                100,
                100,
                size.0,
                size.1,
                null_mut(),
                null_mut(),
                instance,
                null(),
            );
            assert!(!hwnd.is_null());
            if !flip {
                SetTimer(hwnd, 1, 8, None);
            }
            SetTimer(hwnd, 2, lifetime.as_millis() as u32, None);
            SetTimer(hwnd, 3, 1000, None);
            let mut message: MSG = std::mem::zeroed();
            if flip {
                run_flip(hwnd, size);
                return;
            }
            while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
                if message.message == WM_TIMER && message.wParam == 2 {
                    DestroyWindow(hwnd);
                    continue;
                }
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }

    /// A flip-model D3D11 window presenting with vsync (what Windows Terminal and browsers do).
    /// Counts presents in `PAINTS`; runs until the lifetime timer fires.
    unsafe fn run_flip(hwnd: HWND, _size: (i32, i32)) {
        use windows::Win32::Foundation::{HMODULE, HWND as WinHwnd};
        use windows::Win32::Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::*,
            Dxgi::{Common::*, *},
        };
        use windows::core::Interface;
        // SAFETY: private device, swapchain and views owned by this thread; the window is ours.
        unsafe {
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .expect("flip device");
            let device = device.expect("device");
            let context = context.expect("context");
            let dxgi: IDXGIDevice = device.cast().expect("dxgi device");
            let adapter = dxgi.GetAdapter().expect("adapter");
            let desc = adapter.GetDesc().expect("desc");
            let _ = std::fs::write(
                std::env::temp_dir().join("twin-rate-flip-adapter.txt"),
                text(&desc.Description),
            );
            let factory: IDXGIFactory2 = adapter.GetParent().expect("factory2");
            let swap_desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: 0,
                Height: 0,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                ..Default::default()
            };
            let chain = factory
                .CreateSwapChainForHwnd(&device, WinHwnd(hwnd), &swap_desc, None, None)
                .expect("swapchain");
            let mut message: MSG = std::mem::zeroed();
            let mut frame = 0u32;
            'outer: loop {
                while PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) != 0 {
                    if message.message == WM_QUIT {
                        break 'outer;
                    }
                    if message.message == WM_TIMER && message.wParam == 2 {
                        DestroyWindow(hwnd);
                        continue;
                    }
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                let texture: ID3D11Texture2D = chain.GetBuffer(0).expect("buffer");
                let mut view = None;
                device
                    .CreateRenderTargetView(&texture, None, Some(&mut view))
                    .expect("rtv");
                let shade = (frame % 255) as f32 / 255.0;
                context
                    .ClearRenderTargetView(&view.expect("view"), &[shade, 0.4, 1.0 - shade, 1.0]);
                frame = frame.wrapping_add(7);
                if chain.Present(1, DXGI_PRESENT(0)).is_err() {
                    break;
                }
                PAINTS.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    struct Luid(i32, u32);
    impl std::fmt::Display for Luid {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{:08x}:{:08x}", self.0, self.1)
        }
    }

    /// One active display path as the display-config API reports it.
    struct Path {
        gdi: String,
        friendly: String,
        target_adapter: Luid,
        refresh: (u32, u32),
        monitor_path: String,
        adapter_path: String,
    }

    fn display_paths() -> Vec<Path> {
        let (mut path_count, mut mode_count) = (0, 0);
        // SAFETY: initialized count outputs, active paths only, read-only queries.
        unsafe {
            if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
                != ERROR_SUCCESS
            {
                return Vec::new();
            }
            let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count.max(1) as usize];
            let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count.max(1) as usize];
            if QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                null_mut(),
            ) != ERROR_SUCCESS
            {
                return Vec::new();
            }
            let mut out = Vec::new();
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
                if DisplayConfigGetDeviceInfo(&mut source.header) != 0
                    || DisplayConfigGetDeviceInfo(&mut target.header) != 0
                {
                    continue;
                }
                let mut adapter = DISPLAYCONFIG_ADAPTER_NAME {
                    header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                        r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME,
                        size: size_of::<DISPLAYCONFIG_ADAPTER_NAME>() as u32,
                        adapterId: path.targetInfo.adapterId,
                        id: 0,
                    },
                    ..Default::default()
                };
                let adapter_path = if DisplayConfigGetDeviceInfo(&mut adapter.header) == 0 {
                    text(&adapter.adapterDevicePath)
                } else {
                    String::new()
                };
                out.push(Path {
                    monitor_path: text(&target.monitorDevicePath),
                    adapter_path,
                    gdi: text(&source.viewGdiDeviceName),
                    friendly: text(&target.monitorFriendlyDeviceName),
                    target_adapter: Luid(
                        path.targetInfo.adapterId.HighPart,
                        path.targetInfo.adapterId.LowPart,
                    ),
                    refresh: (
                        path.targetInfo.refreshRate.Numerator,
                        path.targetInfo.refreshRate.Denominator,
                    ),
                });
            }
            out
        }
    }

    /// Step 1: DXGI adapters and their outputs; returns the twin's desktop rect when found.
    fn adapters_and_twin() -> Option<(i32, i32, i32, i32)> {
        println!("== display paths (QueryDisplayConfig, active) ==");
        let paths = display_paths();
        for p in &paths {
            println!(
                "path {} friendly={:?} target adapter {} refresh {}/{}\n    monitor {}\n    adapter {}",
                p.gdi,
                p.friendly,
                p.target_adapter,
                p.refresh.0,
                p.refresh.1,
                p.monitor_path,
                p.adapter_path
            );
        }
        // The friendly name can be empty (the driver's EDID carries no name descriptor here);
        // the driver's own device-path strings identify it too.
        let marks = |s: &str| {
            let s = s.to_ascii_lowercase();
            s.contains("crosspane") || s.contains("cpd")
        };
        let twin = paths.iter().find(|p| {
            p.friendly.contains("Crosspane") || marks(&p.monitor_path) || marks(&p.adapter_path)
        });
        match twin {
            Some(p) => println!(
                "twin by friendly name: {} (target adapter {}, {:.2} Hz)",
                p.gdi,
                p.target_adapter,
                p.refresh.0 as f64 / p.refresh.1.max(1) as f64
            ),
            None => println!("twin by friendly name: NOT FOUND"),
        }
        println!("== DXGI adapters ==");
        let mut twin_rect = None;
        // SAFETY: read-only DXGI enumeration on interfaces owned here.
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().expect("factory");
            let mut index = 0;
            while let Ok(adapter) = factory.EnumAdapters1(index) {
                let desc = adapter.GetDesc1().expect("adapter desc");
                println!(
                    "adapter #{index}: {:?} luid {} flags {:#x} vram {} MiB",
                    text(&desc.Description),
                    Luid(desc.AdapterLuid.HighPart, desc.AdapterLuid.LowPart),
                    desc.Flags,
                    desc.DedicatedVideoMemory / (1024 * 1024)
                );
                let mut out = 0;
                while let Ok(output) = adapter.EnumOutputs(out) {
                    let od = output.GetDesc().expect("output desc");
                    let name = text(&od.DeviceName);
                    let r = od.DesktopCoordinates;
                    let is_twin = twin.is_some_and(|p| p.gdi == name);
                    println!(
                        "    output {name} rect ({},{})-({},{}) attached={} {}",
                        r.left,
                        r.top,
                        r.right,
                        r.bottom,
                        od.AttachedToDesktop.as_bool(),
                        if is_twin { "<== TWIN" } else { "" }
                    );
                    if is_twin {
                        twin_rect = Some((r.left, r.top, r.right, r.bottom));
                    }
                    out += 1;
                }
                index += 1;
            }
        }
        if let Some(p) = twin {
            // SAFETY: read-only GDI query of a device name we just read.
            unsafe {
                let mut mode: DEVMODEW = std::mem::zeroed();
                mode.dmSize = size_of::<DEVMODEW>() as u16;
                let name = wide(&p.gdi);
                if EnumDisplaySettingsW(name.as_ptr(), ENUM_CURRENT_SETTINGS, &mut mode) != 0 {
                    let pos = mode.Anonymous1.Anonymous2.dmPosition;
                    twin_rect = Some((
                        pos.x,
                        pos.y,
                        pos.x + mode.dmPelsWidth as i32,
                        pos.y + mode.dmPelsHeight as i32,
                    ));
                    println!(
                        "GDI mode for the twin: {}x{} at ({},{}) {} Hz",
                        mode.dmPelsWidth, mode.dmPelsHeight, pos.x, pos.y, mode.dmDisplayFrequency
                    );
                }
            }
        }
        twin_rect
    }

    fn find_window(title: &str) -> HWND {
        // SAFETY: read-only lookup by title.
        unsafe { FindWindowW(null(), wide(title).as_ptr()) }
    }

    fn move_window(hwnd: HWND, at: (i32, i32), size: (i32, i32)) {
        // SAFETY: moves only the probe's own child window, bottom of the z-order, no activation.
        unsafe {
            SetWindowPos(
                hwnd,
                HWND_BOTTOM,
                at.0,
                at.1,
                size.0,
                size.1,
                SWP_NOACTIVATE,
            );
        }
    }

    /// The window's rect and the GDI name of the monitor holding most of it.
    fn where_is(hwnd: HWND) -> String {
        // SAFETY: read-only queries on the probe's own window.
        unsafe {
            let mut rect: RECT = std::mem::zeroed();
            GetWindowRect(hwnd, &mut rect);
            let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            let mut info: MONITORINFOEXW = std::mem::zeroed();
            info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            GetMonitorInfoW(monitor, &mut info as *mut _ as *mut MONITORINFO);
            format!(
                "({},{})-({},{}) on monitor {}",
                rect.left,
                rect.top,
                rect.right,
                rect.bottom,
                text(&info.szDevice)
            )
        }
    }

    fn arrivals(
        capture: &mut WindowsFrameCapture,
        window: crosspane_types::id::WindowId,
        label: &str,
        secs: u64,
    ) {
        let seen: Arc<Mutex<Vec<(Instant, u32, u32)>>> = Arc::default();
        let sink_seen = seen.clone();
        let sink: Arc<dyn EventSink<FrameEvent>> = Arc::new(move |event: FrameEvent| {
            if let FrameEvent::Frame { frame, .. } = event {
                sink_seen.lock().unwrap().push((
                    Instant::now(),
                    frame.size.width,
                    frame.size.height,
                ));
            }
        });
        let _ = std::fs::remove_file(stats_path());
        let stream = match capture.start(CaptureTarget::Window(window), None, 60, sink) {
            Ok(stream) => stream,
            Err(error) => {
                println!("{label}: start FAILED {error:?}");
                return;
            }
        };
        std::thread::sleep(Duration::from_secs(secs));
        capture.stop(stream).expect("stop capture");
        let seen = seen.lock().unwrap();
        let mut gaps: Vec<f64> = seen
            .windows(2)
            .map(|w| w[1].0.duration_since(w[0].0).as_secs_f64() * 1000.0)
            .collect();
        gaps.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pick = |q: f64| {
            gaps.get(((gaps.len() as f64 - 1.0) * q) as usize)
                .copied()
                .unwrap_or(0.0)
        };
        let size = seen.last().map(|s| (s.1, s.2));
        println!(
            "{label}: {} frames in {secs} s = {:.1} fps; gap ms p50 {:.1} p90 {:.1} max {:.1}; frame {:?}; window painted {} paints/s",
            seen.len(),
            seen.len() as f64 / secs as f64,
            pick(0.5),
            pick(0.9),
            gaps.last().copied().unwrap_or(0.0),
            size,
            std::fs::read_to_string(stats_path())
                .unwrap_or_default()
                .trim()
        );
    }

    pub fn main() {
        let args: Vec<String> = std::env::args().collect();
        if args.get(1).map(String::as_str) == Some("--child") {
            let secs = args[3].parse().unwrap_or(120);
            let size = env_pair("TWIN_RATE_SIZE").unwrap_or((640, 360));
            let flip = args.get(4).map(String::as_str) == Some("flip");
            run_child(&args[2], size, Duration::from_secs(secs), flip);
            return;
        }
        // SAFETY: process-wide DPI mode, set before any window or monitor query.
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let secs = env_num("TWIN_RATE_SECS", 6);
        let size = env_pair("TWIN_RATE_SIZE").unwrap_or((640, 360));
        let twin_rect = adapters_and_twin();
        if std::env::var("TWIN_RATE_LIST_ONLY").is_ok() {
            return;
        }

        let kinds = std::env::var("TWIN_RATE_KINDS").unwrap_or_else(|_| "gdi,flip".into());

        let displays = WindowsDisplays::new().expect("displays");
        let source = WindowsWindowSource::new(displays.ids(), displays.monitor_reader())
            .expect("window source");
        let gate = IoGate::new();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        let mut cpu = WindowsFrameCapture::new(gate.clone(), source.resolver()).expect("capture");
        let mut gpu_capture = WindowsFrameCapture::new(gate, source.resolver()).expect("capture");
        let gpu = gpu_capture
            .enable_gpu(wgpu::Features::empty())
            .expect("enable_gpu");
        println!("== agent capture/GPU device ==");
        println!("WindowsGpu: {gpu:?}");
        println!("(capture device is D3D11CreateDevice(adapter=None), i.e. the default adapter)");

        // Read-only attach: measure WGC delivery for an existing window (never moved or resized).
        if let Ok(wanted) = std::env::var("TWIN_RATE_ATTACH") {
            let found = source
                .windows()
                .ok()
                .and_then(|l| l.into_iter().find(|w| w.title.contains(&wanted)));
            match found {
                Some(w) => {
                    println!(
                        "attached to {:?} {:?} frame {:?} state {:?}",
                        w.title, w.id, w.frame, w.state
                    );
                    arrivals(&mut cpu, w.id, "attached CPU slots", secs);
                    arrivals(&mut gpu_capture, w.id, "attached GPU slots", secs);
                }
                None => println!("no window titled {wanted:?} in the window source"),
            }
            std::process::exit(0);
        }

        for kind in kinds.split(',') {
            let flip = kind == "flip";
            println!("######## window kind: {kind} ########");
            let title = format!("CPTWINRATE {kind} {}", std::process::id());
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--child",
                    &title,
                    &(secs * 8 + 60).to_string(),
                    if flip { "flip" } else { "gdi" },
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn child");
            let mut hwnd = null_mut();
            for _ in 0..50 {
                hwnd = find_window(&title);
                if !hwnd.is_null() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            assert!(!hwnd.is_null(), "child window never appeared");
            move_window(hwnd, (100, 100), size);
            let find_id = || {
                for _ in 0..30 {
                    if let Some(w) = source
                        .windows()
                        .ok()
                        .and_then(|l| l.into_iter().find(|w| w.title.contains(&title)))
                    {
                        return Some(w.id);
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                None
            };
            let id = find_id().expect("child window not in the window source");
            std::thread::sleep(Duration::from_millis(500));
            if flip {
                println!(
                    "flip child renders on {:?}",
                    std::fs::read_to_string(
                        std::env::temp_dir().join("twin-rate-flip-adapter.txt")
                    )
                    .unwrap_or_default()
                );
            }

            println!("== WGC arrival rate, {kind} window on a normal monitor ==");
            println!("window at {}", where_is(hwnd));
            arrivals(&mut cpu, id, &format!("{kind} primary CPU slots"), secs);
            arrivals(
                &mut gpu_capture,
                id,
                &format!("{kind} primary GPU slots"),
                secs,
            );

            match twin_rect {
                Some((l, t, r, b)) => {
                    let tsize = ((r - l).min(size.0), (b - t).min(size.1));
                    let at = env_pair("TWIN_RATE_TWIN_POS").unwrap_or((r - tsize.0, b - tsize.1));
                    println!("== moving only the probe window to the twin at {at:?} {tsize:?} ==");
                    move_window(hwnd, at, tsize);
                    std::thread::sleep(Duration::from_millis(1500));
                    println!("window at {}", where_is(hwnd));
                    match find_id() {
                        Some(id2) => {
                            println!("window id after move: {id2:?} (before {id:?})");
                            arrivals(&mut cpu, id2, &format!("{kind} twin CPU slots"), secs);
                            arrivals(
                                &mut gpu_capture,
                                id2,
                                &format!("{kind} twin GPU slots"),
                                secs,
                            );
                        }
                        None => println!("window source no longer lists the moved window"),
                    }
                    move_window(hwnd, (100, 100), size);
                    std::thread::sleep(Duration::from_millis(500));
                }
                None => println!("twin rect unknown; skipped the twin phase"),
            }
            let _ = child.kill();
            let _ = child.wait();
            std::thread::sleep(Duration::from_millis(500));
        }
        // Dropping the GPU context faults in teardown (known); exit like the pace probe.
        std::process::exit(0);
    }
}

#[cfg(all(windows, feature = "gpu"))]
fn main() {
    imp::main();
}
#[cfg(not(all(windows, feature = "gpu")))]
fn main() {}
