//! Display-capture rate probe: how fast does a *monitor* capture deliver frames, on the Crosspane
//! IDD twin versus a real monitor, through the production `WindowsFrameCapture` and through
//! stand-alone raw WGC sessions (so one setting can be varied at a time) and Desktop Duplication.
//!
//! ```text
//! cargo run -p crosspane-platform-windows --features gpu --release --example twin_display_probe
//! ```
//!
//! Read-only on the twin: it only captures. On a real monitor it spawns one animated child window
//! of its own (this binary re-run with `--child`) and closes it again. Environment:
//!
//! - `TWIN_DISP_SECS`: seconds per measurement (default 6).
//! - `TWIN_DISP_FPS`: the `max_fps` the production capture is started with (default 60).
//! - `TWIN_DISP_TARGETS`: comma list of `twin`, `primary`, `other` (default `twin,primary`).
//! - `TWIN_DISP_MODES`: comma list of `prod` (production, CPU slots), `prodgpu` (production with
//!   the GPU context, needs the `gpu` feature), `raw` (raw WGC variants), `dda` (default
//!   `prod,prodgpu,raw,dda`).
//! - `TWIN_DISP_RAW`: comma list of raw WGC variants (default `b2,b2m0,b4m0`). A variant is `b<N>`
//!   frame-pool buffers, optional `m<ms>` MinUpdateInterval in ms, optional `a<i>` D3D device on
//!   DXGI adapter `i` instead of the default adapter, optional `c` cursor capture on.
//! - `TWIN_DISP_CROP`: `x0,y0,x1,y1` crop (display-local pixels) for the production capture.
//! - `TWIN_DISP_KINDS`: child window kinds for real monitors, `gdi`, `flip` (default `flip,gdi`).
//! - `TWIN_DISP_TOPMOST`: keep the child window above other windows (default: bottom of the z-order).
//! - `TWIN_DISP_LIST_ONLY`: print the adapter and monitor topology and exit.
#![allow(unsafe_code, clippy::unwrap_used, clippy::expect_used)]

#[cfg(windows)]
mod child {
    //! The animated child window used on real monitors (windows-sys only; one process per window).
    use std::ptr::{null, null_mut};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use windows_sys::Win32::{
        Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{HiDpi::*, WindowsAndMessaging::*},
    };

    pub type Hwnd = HWND;

    pub fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }
    pub fn text(units: &[u16]) -> String {
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        String::from_utf16_lossy(&units[..end])
    }

    static TICK: AtomicUsize = AtomicUsize::new(0);
    static PAINTS: AtomicUsize = AtomicUsize::new(0);

    pub fn stats_path() -> std::path::PathBuf {
        std::env::temp_dir().join("twin-display-stats.txt")
    }

    pub fn dpi_aware() {
        // SAFETY: process-wide DPI mode, set before any window or monitor query.
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
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
                        let line: Vec<u16> = format!(
                            "{:08}  twin display probe line {row}",
                            tick.wrapping_sub(row)
                        )
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
    pub fn run(title: &str, size: (i32, i32), lifetime: Duration, flip: bool) {
        // SAFETY: a private class and window owned and pumped by this thread.
        unsafe {
            SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let instance = GetModuleHandleW(null());
            let class = wide("Crosspane.TwinDisplay");
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
            if flip {
                run_flip(hwnd);
                return;
            }
            let mut message: MSG = std::mem::zeroed();
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
    unsafe fn run_flip(hwnd: HWND) {
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
                    if message.message == WM_TIMER {
                        if message.wParam == 2 {
                            DestroyWindow(hwnd);
                            continue;
                        }
                        if message.wParam == 3 {
                            let paints = PAINTS.swap(0, Ordering::Relaxed);
                            let _ = std::fs::write(stats_path(), format!("{paints}"));
                            continue;
                        }
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

    pub fn find(title: &str) -> Hwnd {
        // SAFETY: read-only lookup by title.
        unsafe { FindWindowW(null(), wide(title).as_ptr()) }
    }

    /// Moves the probe's child. `topmost` keeps it above other windows (a covered window is
    /// not composed, so a capture of its monitor would see nothing change); the default is the
    /// bottom of the z-order so the probe stays out of the way.
    pub fn move_to(hwnd: Hwnd, at: (i32, i32), size: (i32, i32), topmost: bool) {
        // SAFETY: moves only the probe's own child window, no activation.
        unsafe {
            SetWindowPos(
                hwnd,
                if topmost { HWND_TOPMOST } else { HWND_BOTTOM },
                at.0,
                at.1,
                size.0,
                size.1,
                SWP_NOACTIVATE,
            );
        }
    }

    /// The window's rect and the GDI name of the monitor holding most of it.
    pub fn where_is(hwnd: Hwnd) -> String {
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
}

#[cfg(windows)]
mod imp {
    use super::child;
    use crosspane_platform::{CaptureTarget, EventSink, FrameCapture, FrameEvent, IoGate};
    use crosspane_platform_windows::{
        displays::WindowsDisplays, frame_capture::WindowsFrameCapture, window::WindowsWindowSource,
    };
    use crosspane_types::{geom::PixelRect, id::DisplayId};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use windows::{
        Foundation::{TimeSpan, TypedEventHandler},
        Graphics::{
            Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem},
            DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
        },
        Win32::{
            Foundation::HMODULE,
            Graphics::{
                Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN},
                Direct3D11::{
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice,
                    ID3D11Device,
                },
                Dxgi::{
                    CreateDXGIFactory1, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
                    IDXGIAdapter, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1, IDXGIOutput,
                    IDXGIOutput1,
                },
                Gdi::HMONITOR,
            },
            System::WinRT::{
                Direct3D11::CreateDirect3D11DeviceFromDXGIDevice,
                Graphics::Capture::IGraphicsCaptureItemInterop, RO_INIT_MULTITHREADED,
                RoInitialize,
            },
        },
        core::{Interface, factory},
    };
    use windows_sys::Win32::{
        Devices::Display::*,
        Foundation::ERROR_SUCCESS,
        Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW},
    };

    fn env_num(name: &str, default: u64) -> u64 {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }
    fn env_list(name: &str, default: &str) -> Vec<String> {
        std::env::var(name)
            .unwrap_or_else(|_| default.into())
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }
    fn env_crop() -> Option<PixelRect> {
        let value = std::env::var("TWIN_DISP_CROP").ok()?;
        let parts: Vec<i32> = value
            .split(',')
            .filter_map(|p| p.trim().parse().ok())
            .collect();
        match parts[..] {
            [a, b, c, d] => Some(PixelRect::new((a, b).into(), (c, d).into())),
            _ => None,
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
        target_adapter: Luid,
        refresh: (u32, u32),
        monitor_path: String,
        adapter_path: String,
    }

    fn display_paths() -> Vec<Path> {
        use std::ptr::null_mut;
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
                    child::text(&adapter.adapterDevicePath)
                } else {
                    String::new()
                };
                out.push(Path {
                    monitor_path: child::text(&target.monitorDevicePath),
                    adapter_path,
                    gdi: child::text(&source.viewGdiDeviceName),
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

    /// One DXGI output, with the adapter that lists it.
    struct OutputRow {
        adapter_index: u32,
        adapter_name: String,
        adapter_luid: (i32, u32),
        gdi: String,
        rect: (i32, i32, i32, i32),
    }

    fn dxgi_outputs() -> Vec<OutputRow> {
        let mut rows = Vec::new();
        // SAFETY: read-only DXGI enumeration on interfaces owned here.
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().expect("factory");
            let mut index = 0;
            while let Ok(adapter) = factory.EnumAdapters1(index) {
                let desc = adapter.GetDesc1().expect("adapter desc");
                let mut out = 0;
                let mut any = false;
                while let Ok(output) = adapter.EnumOutputs(out) {
                    any = true;
                    let od = output.GetDesc().expect("output desc");
                    let r = od.DesktopCoordinates;
                    rows.push(OutputRow {
                        adapter_index: index,
                        adapter_name: child::text(&desc.Description),
                        adapter_luid: (desc.AdapterLuid.HighPart, desc.AdapterLuid.LowPart),
                        gdi: child::text(&od.DeviceName),
                        rect: (r.left, r.top, r.right, r.bottom),
                    });
                    out += 1;
                }
                if !any {
                    rows.push(OutputRow {
                        adapter_index: index,
                        adapter_name: child::text(&desc.Description),
                        adapter_luid: (desc.AdapterLuid.HighPart, desc.AdapterLuid.LowPart),
                        gdi: String::new(),
                        rect: (0, 0, 0, 0),
                    });
                }
                index += 1;
            }
        }
        rows
    }

    fn output_for(gdi: &str) -> Option<(u32, IDXGIOutput)> {
        // SAFETY: read-only DXGI enumeration on interfaces owned here.
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
            let mut index = 0;
            while let Ok(adapter) = factory.EnumAdapters1(index) {
                let mut out = 0;
                while let Ok(output) = adapter.EnumOutputs(out) {
                    if let Ok(desc) = output.GetDesc()
                        && child::text(&desc.DeviceName) == gdi
                    {
                        return Some((index, output));
                    }
                    out += 1;
                }
                index += 1;
            }
        }
        None
    }

    fn device_on(adapter_index: Option<u32>) -> (ID3D11Device, String, Luid) {
        // SAFETY: owned outputs, read-only adapter queries.
        unsafe {
            let adapter: Option<IDXGIAdapter> = adapter_index.map(|i| {
                let factory: IDXGIFactory1 = CreateDXGIFactory1().expect("factory");
                let adapter: IDXGIAdapter1 = factory.EnumAdapters1(i).expect("adapter index");
                adapter.cast().expect("adapter")
            });
            let mut device = None;
            D3D11CreateDevice(
                adapter.as_ref(),
                if adapter.is_some() {
                    D3D_DRIVER_TYPE_UNKNOWN
                } else {
                    D3D_DRIVER_TYPE_HARDWARE
                },
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
            .expect("D3D11CreateDevice");
            let device = device.expect("device");
            let dxgi: IDXGIDevice = device.cast().expect("dxgi device");
            let desc = dxgi
                .GetAdapter()
                .expect("device adapter")
                .GetDesc()
                .expect("device adapter desc");
            (
                device,
                child::text(&desc.Description),
                Luid(desc.AdapterLuid.HighPart, desc.AdapterLuid.LowPart),
            )
        }
    }

    /// Arrival statistics from a list of arrival instants.
    fn report(label: &str, times: &[Instant], secs: u64, extra: &str) {
        let mut gaps: Vec<f64> = times
            .windows(2)
            .map(|w| w[1].duration_since(w[0]).as_secs_f64() * 1000.0)
            .collect();
        gaps.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pick = |q: f64| {
            gaps.get(((gaps.len() as f64 - 1.0) * q) as usize)
                .copied()
                .unwrap_or(0.0)
        };
        let span = match (times.first(), times.last()) {
            (Some(a), Some(b)) if times.len() > 1 => b.duration_since(*a).as_secs_f64(),
            _ => 0.0,
        };
        println!(
            "{label}: {} frames in {secs} s = {:.1} fps (span rate {:.1}); gap ms p50 {:.1} p90 {:.1} max {:.1}{extra}",
            times.len(),
            times.len() as f64 / secs as f64,
            if span > 0.0 {
                (times.len() - 1) as f64 / span
            } else {
                0.0
            },
            pick(0.5),
            pick(0.9),
            gaps.last().copied().unwrap_or(0.0),
        );
    }

    fn paints() -> String {
        format!(
            "; child painted {} paints/s",
            std::fs::read_to_string(child::stats_path())
                .unwrap_or_default()
                .trim()
        )
    }

    /// The production capture of one display, frames dropped on arrival.
    fn prod(
        capture: &mut WindowsFrameCapture,
        id: DisplayId,
        crop: Option<PixelRect>,
        fps: u32,
        label: &str,
        secs: u64,
        child_running: bool,
    ) {
        let seen: Arc<Mutex<Vec<(Instant, u32, u32)>>> = Arc::default();
        let ended: Arc<Mutex<Vec<(Instant, String)>>> = Arc::default();
        let sink_seen = seen.clone();
        let sink_ended = ended.clone();
        let sink: Arc<dyn EventSink<FrameEvent>> = Arc::new(move |event: FrameEvent| match event {
            FrameEvent::Frame { frame, .. } => {
                sink_seen.lock().unwrap().push((
                    Instant::now(),
                    frame.size.width,
                    frame.size.height,
                ));
            }
            FrameEvent::Ended { reason, .. } => {
                sink_ended
                    .lock()
                    .unwrap()
                    .push((Instant::now(), format!("{reason:?}")));
            }
            _ => {}
        });
        let _ = std::fs::remove_file(child::stats_path());
        let (calls0, nanos0) = reader_counters();
        let cpu0 = process_cpu();
        let began = Instant::now();
        let stream = match capture.start(CaptureTarget::Display(id), crop, fps, sink) {
            Ok(stream) => stream,
            Err(error) => {
                println!("{label}: start FAILED {error:?}");
                return;
            }
        };
        std::thread::sleep(Duration::from_secs(secs));
        let stopped = capture.stop(stream);
        let cpu = (process_cpu() - cpu0).as_secs_f64() / began.elapsed().as_secs_f64() * 100.0;
        let (calls1, nanos1) = reader_counters();
        let seen = seen.lock().unwrap();
        let times: Vec<Instant> = seen.iter().map(|s| s.0).collect();
        let size = seen.last().map(|s| (s.1, s.2));
        let calls = calls1 - calls0;
        let ended = ended
            .lock()
            .unwrap()
            .iter()
            .map(|(at, reason)| {
                format!("{reason} after {} ms", at.duration_since(began).as_millis())
            })
            .collect::<Vec<_>>();
        report(
            label,
            &times,
            secs,
            &format!(
                "; frame {size:?}; probe process CPU {cpu:.0}% of one core; monitor snapshot reads {calls} ({:.1}/s, {:.1} ms avg, {} per delivered frame); stream ended early: {ended:?}{}{}",
                calls as f64 / secs as f64,
                if calls > 0 {
                    (nanos1 - nanos0) as f64 / calls as f64 / 1e6
                } else {
                    0.0
                },
                if times.is_empty() {
                    "n/a".to_string()
                } else {
                    format!("{:.1}", calls as f64 / times.len() as f64)
                },
                if stopped.is_err() {
                    " (stop: stream already gone)"
                } else {
                    ""
                },
                if child_running {
                    paints()
                } else {
                    String::new()
                }
            ),
        );
    }

    /// Kernel plus user CPU time this process has used so far.
    fn process_cpu() -> Duration {
        use windows_sys::Win32::{
            Foundation::FILETIME,
            System::Threading::{GetCurrentProcess, GetProcessTimes},
        };
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
        // SAFETY: valid out-pointers, the pseudo-handle of this process.
        unsafe {
            GetProcessTimes(
                GetCurrentProcess(),
                &mut created,
                &mut exited,
                &mut kernel,
                &mut user,
            );
        }
        let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
        Duration::from_nanos((ticks(kernel) + ticks(user)) * 100)
    }

    static READS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static READ_NANOS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    fn reader_counters() -> (u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        (READS.load(Relaxed), READ_NANOS.load(Relaxed))
    }
    /// The production snapshot reader, counting and timing every call the capture worker makes.
    fn counted_reader(
        reader: crosspane_platform_windows::model::frame_capture::MonitorSnapshotReader,
    ) -> crosspane_platform_windows::model::frame_capture::MonitorSnapshotReader {
        use std::sync::atomic::Ordering::Relaxed;
        Arc::new(move || {
            let at = Instant::now();
            let result = reader();
            READS.fetch_add(1, Relaxed);
            READ_NANOS.fetch_add(at.elapsed().as_nanos() as u64, Relaxed);
            result
        })
    }

    /// A raw WGC variant: `b<N>` buffers, `m<ms>` MinUpdateInterval, `a<i>` adapter, `c` cursor.
    struct Variant {
        buffers: i32,
        min_ms: Option<u64>,
        adapter: Option<u32>,
        cursor: bool,
    }
    fn parse_variant(text: &str) -> Variant {
        let mut variant = Variant {
            buffers: 2,
            min_ms: None,
            adapter: None,
            cursor: false,
        };
        let mut chars = text.chars().peekable();
        while let Some(letter) = chars.next() {
            let mut digits = String::new();
            while let Some(d) = chars.peek().filter(|c| c.is_ascii_digit()) {
                digits.push(*d);
                chars.next();
            }
            let number: u64 = digits.parse().unwrap_or(0);
            match letter {
                'b' => variant.buffers = number as i32,
                'm' => variant.min_ms = Some(number),
                'a' => variant.adapter = Some(number as u32),
                'c' => variant.cursor = true,
                _ => {}
            }
        }
        variant
    }

    fn raw_wgc(
        label: &str,
        hmonitor: usize,
        variant: &Variant,
        secs: u64,
        child_running: bool,
        print_defaults: bool,
    ) {
        let (device, adapter_name, luid) = device_on(variant.adapter);
        let dxgi: IDXGIDevice = device.cast().expect("dxgi");
        // SAFETY: wraps this owned D3D11 device in the documented WinRT interface.
        let winrt: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
            .and_then(|d| d.cast())
            .expect("winrt device");
        let interop: IGraphicsCaptureItemInterop =
            factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().expect("interop");
        // SAFETY: the HMONITOR comes from the displays observation made moments ago.
        let item: GraphicsCaptureItem =
            match unsafe { interop.CreateForMonitor(HMONITOR(hmonitor as *mut _)) } {
                Ok(item) => item,
                Err(error) => {
                    println!("{label}: CreateForMonitor FAILED {error:?}");
                    return;
                }
            };
        let size = item.Size().expect("item size");
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            variant.buffers,
            size,
        )
        .expect("pool");
        let session = pool.CreateCaptureSession(&item).expect("session");
        if print_defaults {
            println!(
                "  WGC session defaults: MinUpdateInterval {:?} (100 ns units), IsCursorCaptureEnabled {:?}, IsBorderRequired {:?}",
                session.MinUpdateInterval().map(|t| t.Duration),
                session.IsCursorCaptureEnabled(),
                session.IsBorderRequired()
            );
        }
        session
            .SetIsCursorCaptureEnabled(variant.cursor)
            .expect("cursor");
        if let Some(ms) = variant.min_ms
            && let Err(error) = session.SetMinUpdateInterval(TimeSpan {
                Duration: ms as i64 * 10_000,
            })
        {
            println!("{label}: SetMinUpdateInterval FAILED {error:?}");
        }
        let seen: Arc<Mutex<Vec<(Instant, i64)>>> = Arc::default();
        let sink_seen = seen.clone();
        let handler =
            TypedEventHandler::<Direct3D11CaptureFramePool, windows::core::IInspectable>::new(
                move |sender, _| {
                    if let Some(pool) = sender.as_ref()
                        && let Ok(frame) = pool.TryGetNextFrame()
                    {
                        let sys = frame.SystemRelativeTime().map(|t| t.Duration).unwrap_or(0);
                        sink_seen.lock().unwrap().push((Instant::now(), sys));
                        let _ = frame.Close();
                    }
                    Ok(())
                },
            );
        let token = pool.FrameArrived(&handler).expect("FrameArrived");
        let _ = std::fs::remove_file(child::stats_path());
        session.StartCapture().expect("start");
        std::thread::sleep(Duration::from_secs(secs));
        let _ = session.Close();
        let _ = pool.RemoveFrameArrived(token);
        let _ = pool.Close();
        let seen = seen.lock().unwrap();
        let times: Vec<Instant> = seen.iter().map(|s| s.0).collect();
        // The compositor's own timestamps: the cadence at which DWM produced the frames.
        let mut sys_gaps: Vec<f64> = seen
            .windows(2)
            .map(|w| (w[1].1 - w[0].1) as f64 / 10_000.0)
            .collect();
        sys_gaps.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let sys_p50 = sys_gaps
            .get(sys_gaps.len().saturating_sub(1) / 2)
            .copied()
            .unwrap_or(0.0);
        report(
            &format!(
                "{label} [device {adapter_name:?} {luid}, pool {}x{}]",
                size.Width, size.Height
            ),
            &times,
            secs,
            &format!(
                "; DWM timestamp gap p50 {sys_p50:.1} ms{}",
                if child_running {
                    paints()
                } else {
                    String::new()
                }
            ),
        );
    }

    /// Desktop Duplication of the output with this GDI name, on the adapter that lists it.
    fn dda(label: &str, gdi: &str, secs: u64, child_running: bool) {
        let Some((adapter_index, output)) = output_for(gdi) else {
            println!("{label}: no DXGI output named {gdi}");
            return;
        };
        let (device, adapter_name, luid) = device_on(Some(adapter_index));
        let output1: IDXGIOutput1 = output.cast().expect("IDXGIOutput1");
        // SAFETY: owned device and output; the duplication is released below.
        let duplication = match unsafe { output1.DuplicateOutput(&device) } {
            Ok(duplication) => duplication,
            Err(error) => {
                println!("{label}: DuplicateOutput FAILED {error:?}");
                return;
            }
        };
        let _ = std::fs::remove_file(child::stats_path());
        let started = Instant::now();
        let (mut presents, mut timeouts, mut accumulated) = (Vec::new(), 0u32, 0u64);
        let mut acquired = 0u32;
        while started.elapsed() < Duration::from_secs(secs) {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;
            // SAFETY: valid out-pointers; the frame is released right after the acquire.
            match unsafe { duplication.AcquireNextFrame(100, &mut info, &mut resource) } {
                Ok(()) => {
                    acquired += 1;
                    accumulated += u64::from(info.AccumulatedFrames);
                    if info.LastPresentTime != 0 {
                        presents.push(Instant::now());
                    }
                    drop(resource);
                    // SAFETY: pairs the successful acquire above.
                    let _ = unsafe { duplication.ReleaseFrame() };
                }
                Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => timeouts += 1,
                Err(error) => {
                    println!("{label}: AcquireNextFrame FAILED {error:?}");
                    break;
                }
            }
        }
        report(
            &format!("{label} [device {adapter_name:?} {luid}]"),
            &presents,
            secs,
            &format!(
                "; acquires {acquired} (with desktop update {}), accumulated {accumulated}, 100 ms timeouts {timeouts}{}",
                presents.len(),
                if child_running {
                    paints()
                } else {
                    String::new()
                }
            ),
        );
    }

    struct Run<'a> {
        capture_cpu: &'a mut WindowsFrameCapture,
        #[cfg(feature = "gpu")]
        capture_gpu: &'a mut WindowsFrameCapture,
        modes: Vec<String>,
        raw: Vec<String>,
        secs: u64,
        fps: u32,
        crop: Option<PixelRect>,
    }

    impl Run<'_> {
        fn all(&mut self, name: &str, id: DisplayId, gdi: &str, hmonitor: usize, child: bool) {
            let secs = self.secs;
            let mut first_raw = true;
            for mode in self.modes.clone() {
                match mode.as_str() {
                    "prod" => prod(
                        self.capture_cpu,
                        id,
                        self.crop,
                        self.fps,
                        &format!("{name} production CPU slots"),
                        secs,
                        child,
                    ),
                    #[cfg(feature = "gpu")]
                    "prodgpu" => prod(
                        self.capture_gpu,
                        id,
                        self.crop,
                        self.fps,
                        &format!("{name} production GPU slots"),
                        secs,
                        child,
                    ),
                    "raw" => {
                        for text in self.raw.clone() {
                            raw_wgc(
                                &format!("{name} raw WGC {text}"),
                                hmonitor,
                                &parse_variant(&text),
                                secs,
                                child,
                                std::mem::take(&mut first_raw),
                            );
                        }
                    }
                    "dda" => dda(&format!("{name} Desktop Duplication"), gdi, secs, child),
                    _ => {}
                }
            }
        }
    }

    pub fn main() {
        let args: Vec<String> = std::env::args().collect();
        if args.get(1).map(String::as_str) == Some("--child") {
            let secs = args[3].parse().unwrap_or(120);
            let flip = args.get(4).map(String::as_str) == Some("flip");
            child::run(&args[2], (640, 360), Duration::from_secs(secs), flip);
            return;
        }
        child::dpi_aware();
        // SAFETY: balanced by process exit; the probe never uninitializes.
        let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
        let secs = env_num("TWIN_DISP_SECS", 6);
        let fps = env_num("TWIN_DISP_FPS", 60) as u32;

        println!("== display paths (QueryDisplayConfig, active) ==");
        for p in display_paths() {
            println!(
                "path {} target adapter {} refresh {}/{}\n    monitor {}\n    adapter {}",
                p.gdi, p.target_adapter, p.refresh.0, p.refresh.1, p.monitor_path, p.adapter_path
            );
        }
        println!("== DXGI adapters and outputs ==");
        let outputs = dxgi_outputs();
        for o in &outputs {
            println!(
                "adapter #{} {:?} luid {}  output {:?} rect {:?}",
                o.adapter_index,
                o.adapter_name,
                Luid(o.adapter_luid.0, o.adapter_luid.1),
                o.gdi,
                o.rect
            );
        }
        let (_device, default_name, default_luid) = device_on(None);
        println!(
            "capture device (D3D11CreateDevice adapter=None, as frame_capture.rs Graphics::new): {default_name:?} luid {default_luid}"
        );

        let displays = WindowsDisplays::new().expect("displays");
        let snapshot = displays.snapshot().expect("snapshot");
        println!("== crosspane monitor snapshot ==");
        for p in &snapshot.probes {
            let mut ids = snapshot.ids.clone();
            let id = ids.assign(&p.device_path).ok();
            let owner = outputs.iter().find(|o| o.gdi == p.name);
            println!(
                "probe {} id {:?} rc {:?} primary {} twin {} refresh {} mHz dpi {}\n    device {}\n    DXGI owner: {}",
                p.name,
                id,
                p.rc_monitor,
                p.primary,
                p.twin,
                p.refresh_millihz,
                p.dpi,
                p.device_path,
                owner.map_or("none".to_string(), |o| format!(
                    "adapter #{} {:?} {}{}",
                    o.adapter_index,
                    o.adapter_name,
                    Luid(o.adapter_luid.0, o.adapter_luid.1),
                    if Luid(o.adapter_luid.0, o.adapter_luid.1).to_string()
                        == default_luid.to_string()
                    {
                        " (= capture device adapter)"
                    } else {
                        " (!= capture device adapter)"
                    }
                ))
            );
        }
        if std::env::var("TWIN_DISP_LIST_ONLY").is_ok() {
            // SAFETY: GDI mode query of a name we just read.
            unsafe {
                for p in &snapshot.probes {
                    let mut mode: DEVMODEW = std::mem::zeroed();
                    mode.dmSize = size_of::<DEVMODEW>() as u16;
                    let name = child::wide(&p.name);
                    if EnumDisplaySettingsW(name.as_ptr(), ENUM_CURRENT_SETTINGS, &mut mode) != 0 {
                        println!("GDI mode {}: {} Hz", p.name, mode.dmDisplayFrequency);
                    }
                }
            }
            return;
        }

        let source = WindowsWindowSource::new(displays.ids(), displays.monitor_reader())
            .expect("window source");
        let gate = IoGate::new();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        let mut capture_cpu = WindowsFrameCapture::new_with_monitor_reader(
            gate.clone(),
            source.resolver(),
            counted_reader(displays.monitor_snapshot_reader()),
        )
        .expect("capture");
        #[cfg(feature = "gpu")]
        let mut capture_gpu = {
            let capture = WindowsFrameCapture::new_with_monitor_reader(
                gate.clone(),
                source.resolver(),
                counted_reader(displays.monitor_snapshot_reader()),
            )
            .expect("capture");
            let gpu = capture
                .enable_gpu(wgpu::Features::empty())
                .expect("enable_gpu");
            println!("WindowsGpu: {gpu:?}");
            capture
        };
        let mut run = Run {
            capture_cpu: &mut capture_cpu,
            #[cfg(feature = "gpu")]
            capture_gpu: &mut capture_gpu,
            modes: env_list("TWIN_DISP_MODES", "prod,prodgpu,raw,dda"),
            raw: env_list("TWIN_DISP_RAW", "b2,b2m0,b4m0"),
            secs,
            fps,
            crop: env_crop(),
        };

        let kinds = env_list("TWIN_DISP_KINDS", "flip,gdi");
        for target in env_list("TWIN_DISP_TARGETS", "twin,primary") {
            let probe = match target.as_str() {
                "twin" => snapshot.probes.iter().find(|p| p.twin),
                "primary" => snapshot.probes.iter().find(|p| p.primary && !p.twin),
                // The twin's closest relative: a non-primary 60 Hz landscape monitor.
                _ => snapshot
                    .probes
                    .iter()
                    .filter(|p| !p.primary && !p.twin)
                    .max_by_key(|p| {
                        let [l, t, r, b] = p.rc_monitor;
                        (r - l > b - t, p.refresh_millihz <= 60_000)
                    }),
            };
            let Some(probe) = probe else {
                println!("######## target {target}: no such monitor ########");
                continue;
            };
            let mut ids = snapshot.ids.clone();
            let id = ids.assign(&probe.device_path).expect("display id");
            let hmonitor = displays.monitor(id).expect("hmonitor");
            println!(
                "######## target {target}: {} {:?} rc {:?} {} mHz ########",
                probe.name, id, probe.rc_monitor, probe.refresh_millihz
            );
            if probe.twin {
                run.all("twin", id, &probe.name, hmonitor, false);
                continue;
            }
            for kind in &kinds {
                let flip = kind == "flip";
                println!("-- {target} monitor with an animated {kind} child window --");
                let title = format!("CPTWINDISP {kind} {}", std::process::id());
                let mut proc = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--child",
                        &title,
                        &(secs * 40 + 60).to_string(),
                        if flip { "flip" } else { "gdi" },
                    ])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .expect("spawn child");
                let mut hwnd = std::ptr::null_mut();
                for _ in 0..50 {
                    hwnd = child::find(&title);
                    if !hwnd.is_null() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                assert!(!hwnd.is_null(), "child window never appeared");
                let at = (probe.rc_monitor[0] + 100, probe.rc_monitor[1] + 100);
                child::move_to(
                    hwnd,
                    at,
                    (640, 360),
                    std::env::var("TWIN_DISP_TOPMOST").is_ok(),
                );
                std::thread::sleep(Duration::from_millis(1500));
                println!("child at {}", child::where_is(hwnd));
                run.all(&format!("{target}/{kind}"), id, &probe.name, hmonitor, true);
                let _ = proc.kill();
                let _ = proc.wait();
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        // Dropping the GPU context faults in teardown (known); exit like the other probes.
        std::process::exit(0);
    }
}

#[cfg(windows)]
fn main() {
    imp::main();
}
#[cfg(not(windows))]
fn main() {}
