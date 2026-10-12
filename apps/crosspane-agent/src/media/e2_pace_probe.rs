//! Live E2 source-throughput probe (ignored; needs a desktop session and a GPU).
//!
//! It opens its own animated Win32 window, captures it with the production `WindowsFrameCapture`
//! and feeds the production `encode_loop`, then prints where the time goes:
//!
//! - phase A: raw WGC frame arrival, with the GPU slots and without (no encoder involved);
//! - phase B: the full media path (capture, hash, tiles, H.264) with a recording transport, with
//!   per-frame stage timings from the `frame stages` trace events.
//!
//! ```text
//! cargo test -p crosspane-agent --features video --target-dir target\e2pace --release \
//!     e2_pace_probe -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `E2_PACE_SECS` (seconds per phase, default 6), `E2_PACE_SIZE` (`WxH`, default 1000x700) and
//! `E2_PACE_POS` (`x,y`, default 100,100) tune the run. The window is closed at the end; the
//! process exits without dropping the GPU context (a known teardown fault, not this probe's).
#![allow(unsafe_code, clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crosspane_media::wire::read_codec;
use crosspane_platform::{
    CaptureTarget, EventSink, FrameCapture, FrameEvent, IoGate, StreamId, WindowSource,
};
use crosspane_platform_windows::{
    displays::WindowsDisplays, frame_capture::WindowsFrameCapture, window::WindowsWindowSource,
};
use std::io::Write as _;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Gdi::*,
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::*,
};

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

static TICK: AtomicUsize = AtomicUsize::new(0);
static PAINTS: AtomicUsize = AtomicUsize::new(0);

/// Paints scrolling lines of changing text, like a terminal printing a timestamp.
unsafe extern "system" fn proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: the procedure only touches its own window, its client DC and GDI objects it frees.
    unsafe {
        match message {
            WM_TIMER => {
                if wparam == 3 {
                    // Once a second: how often the window really painted (the probe's yardstick).
                    let paints = PAINTS.swap(0, Ordering::Relaxed);
                    if let Ok(path) = std::env::var("E2_PACE_STATS") {
                        let _ = std::fs::write(
                            path,
                            format!(
                                "{paints} paints/s, {} ticks\n",
                                TICK.load(Ordering::Relaxed)
                            ),
                        );
                    }
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
                        "{:08}  2026-10-12T19:05:{:02}.{:03}Z  line {row} of the log",
                        tick.wrapping_sub(row),
                        (tick / 60) % 60,
                        (tick * 16) % 1000
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

/// Shows the animated window and pumps its messages for `lifetime`, then closes it. Runs in the
/// probe's child process: Crosspane never captures its own process's windows.
fn run_window(title: &str, size: (i32, i32), at: (i32, i32), lifetime: Duration) {
    // SAFETY: a private class and window owned by this thread, which also pumps its messages.
    unsafe {
        let instance = GetModuleHandleW(null());
        let class = wide(&format!("Crosspane.E2Pace.{}", std::process::id()));
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
            at.0,
            at.1,
            size.0,
            size.1,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        assert!(!hwnd.is_null());
        SetTimer(hwnd, 1, env_num("E2_PACE_TICK_MS", 8) as u32, None);
        SetTimer(hwnd, 2, lifetime.as_millis() as u32, None);
        SetTimer(hwnd, 3, 1000, None);
        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
            if message.message == WM_TIMER && message.wParam == 2 {
                DestroyWindow(hwnd);
                continue;
            }
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        UnregisterClassW(class.as_ptr(), instance);
    }
}

/// The child process half of the probe: just the window.
#[test]
#[ignore = "child of e2_pace_probe: shows the animated window"]
fn e2_pace_window_child() {
    let title = std::env::var("E2_PACE_TITLE").expect("E2_PACE_TITLE");
    let size = env_pair("E2_PACE_SIZE", (1000, 700), 'x');
    let at = env_pair("E2_PACE_POS", (100, 100), ',');
    run_window(
        &title,
        size,
        at,
        Duration::from_secs(env_num("E2_PACE_LIFETIME", 60)),
    );
}

fn stats_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("e2pace-stats-{}.txt", std::process::id()))
}

struct AnimWindow {
    child: std::process::Child,
}
impl AnimWindow {
    fn spawn(title: &str, lifetime: Duration) -> Self {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "e2_pace_window_child",
                "--ignored",
                "--exact",
                "media::e2_pace_probe::e2_pace_window_child",
                "--nocapture",
            ])
            .env("E2_PACE_TITLE", title)
            .env("E2_PACE_STATS", stats_path())
            .env("E2_PACE_LIFETIME", lifetime.as_secs().to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn the window child");
        Self { child }
    }
    fn close(&mut self) {
        // Our own child, started above; it also closes itself when its lifetime ends.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Drop for AnimWindow {
    fn drop(&mut self) {
        self.close();
    }
}

fn env_num(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
fn env_pair(name: &str, default: (i32, i32), sep: char) -> (i32, i32) {
    std::env::var(name)
        .ok()
        .and_then(|v| {
            let (a, b) = v.split_once(sep)?;
            Some((a.parse().ok()?, b.parse().ok()?))
        })
        .unwrap_or(default)
}

fn stats(label: &str, at: &[Instant], secs: f64) {
    let mut gaps: Vec<f64> = at
        .windows(2)
        .map(|w| w[1].duration_since(w[0]).as_secs_f64() * 1000.0)
        .collect();
    gaps.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pick = |q: f64| {
        gaps.get(((gaps.len() as f64 - 1.0) * q) as usize)
            .copied()
            .unwrap_or(0.0)
    };
    println!(
        "{label}: {} events in {secs:.1} s = {:.1}/s; gap ms p50 {:.1} p90 {:.1} p99 {:.1} max {:.1}",
        at.len(),
        at.len() as f64 / secs,
        pick(0.5),
        pick(0.9),
        pick(0.99),
        gaps.last().copied().unwrap_or(0.0),
    );
}

fn open_gate() -> Arc<IoGate> {
    let gate = IoGate::new();
    gate.set_session_permits(true);
    gate.set_engine_permits(true);
    gate
}

/// Phase A: raw frame arrival from the capture backend alone.
fn raw_arrival(
    capture: &mut WindowsFrameCapture,
    window: crosspane_types::id::WindowId,
    label: &str,
    secs: u64,
) {
    let arrivals: Arc<Mutex<Vec<(Instant, PixelSize)>>> = Arc::default();
    let sink_arrivals = arrivals.clone();
    let sink: Arc<dyn EventSink<FrameEvent>> = Arc::new(move |event: FrameEvent| {
        if let FrameEvent::Frame { frame, .. } = event {
            sink_arrivals
                .lock()
                .unwrap()
                .push((Instant::now(), frame.size));
        }
    });
    let stream = capture
        .start(CaptureTarget::Window(window), None, 60, sink)
        .expect("start capture");
    std::thread::sleep(Duration::from_secs(secs));
    capture.stop(stream).expect("stop capture");
    let arrivals = arrivals.lock().unwrap();
    let times: Vec<Instant> = arrivals.iter().map(|(at, _)| *at).collect();
    stats(&format!("A {label}"), &times, secs as f64);
    println!(
        "A {label}: the window itself: {}",
        std::fs::read_to_string(stats_path())
            .unwrap_or_default()
            .trim()
    );
    if let Some((_, size)) = arrivals.last() {
        println!("A {label}: frame size {}x{}", size.width, size.height);
    }
}

struct Recorder {
    sent: Mutex<Vec<(Instant, usize, &'static str)>>,
}
impl SourceOutput for Recorder {
    fn send_media(&self, _peer: NodeId, data: Arc<[u8]>) -> Result<(), LinkError> {
        let kind = match read_codec(&data) {
            Ok(Codec::Tiles) => "tiles",
            Ok(Codec::H264) => "h264",
            Ok(Codec::Cursor) => "cursor",
            Err(_) => "?",
        };
        self.sent
            .lock()
            .unwrap()
            .push((Instant::now(), data.len(), kind));
        Ok(())
    }
}

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Phase B: capture -> production `encode_loop` -> recording transport.
fn pipeline(
    capture: &mut WindowsFrameCapture,
    window: crosspane_types::id::WindowId,
    gpu: &Arc<crosspane_platform_windows::gpu::WindowsGpu>,
    region: bool,
    use_gpu: bool,
    secs: u64,
    log: &LogBuffer,
) {
    let label = format!("B region={region} gpu={use_gpu}");
    let codecs: Arc<dyn VideoCodecs> = if use_gpu {
        Arc::new(crosspane_platform_windows::video::MfCodecs::new().with_gpu(gpu.clone()))
    } else {
        Arc::new(crosspane_platform_windows::video::MfCodecs::new())
    };
    let setup = VideoSetup {
        codecs: Some(codecs),
        gpu: use_gpu.then(|| crate::platform::GpuDevice {
            device: gpu.device.clone(),
            queue: gpu.queue.clone(),
            windows: gpu.clone(),
        }),
    };
    let (sender, receive) = source_channel();
    let frames = sender.frames.clone();
    let recorder = Arc::new(Recorder {
        sent: Mutex::new(Vec::new()),
    });
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let recorder = recorder.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            encode_loop(&receive, frames.as_ref(), recorder.as_ref(), &setup, &stop);
        })
    };
    let marker = log.0.lock().unwrap().len();
    let source = sender.open_capture().unwrap();
    sender
        .send(SourceCmd::Start {
            capture: source.clone(),
            projection: ProjectionId(1),
            peer: NodeId([3; 32]),
            video: true,
            region,
            cursor: false,
            bits_per_second: 8_000_000,
        })
        .map_err(|_| ())
        .unwrap();
    let arrivals: Arc<Mutex<Vec<Instant>>> = Arc::default();
    let sink_arrivals = arrivals.clone();
    let sink_sender = sender.clone();
    let sink_capture = source.clone();
    let sink: Arc<dyn EventSink<FrameEvent>> = Arc::new(move |event: FrameEvent| {
        if let FrameEvent::Frame { frame, .. } = event {
            sink_arrivals.lock().unwrap().push(Instant::now());
            let _ = sink_sender.send(SourceCmd::Frame {
                capture: sink_capture.clone(),
                frame,
            });
        }
    });
    let stream: StreamId = capture
        .start(CaptureTarget::Window(window), None, 60, sink)
        .expect("start capture");
    std::thread::sleep(Duration::from_secs(secs));
    capture.stop(stream).expect("stop capture");
    sender.stop(&source);
    stop.store(true, Ordering::Release);
    let _ = thread.join();
    stats(
        &format!("{label} captured"),
        &arrivals.lock().unwrap(),
        secs as f64,
    );
    let sent = recorder.sent.lock().unwrap();
    let all: Vec<Instant> = sent.iter().map(|(at, _, _)| *at).collect();
    stats(&format!("{label} sent (all)"), &all, secs as f64);
    for kind in ["h264", "tiles"] {
        let of: Vec<Instant> = sent
            .iter()
            .filter(|(_, _, k)| *k == kind)
            .map(|(at, _, _)| *at)
            .collect();
        let bytes: usize = sent
            .iter()
            .filter(|(_, _, k)| *k == kind)
            .map(|(_, n, _)| n)
            .sum();
        stats(
            &format!("{label} sent {kind} ({} kB)", bytes / 1024),
            &of,
            secs as f64,
        );
    }
    let text = String::from_utf8_lossy(&log.0.lock().unwrap()[marker..]).into_owned();
    let count = |needle: &str| text.matches(needle).count();
    println!(
        "{label}: codec chosen x{}, video on x{}, GPU-input refusals x{}, 'video failed' x{}",
        count("codec chosen"),
        count("video on"),
        count("GPU MF input refused"),
        count("video failed")
    );
    // Aggregate the per-frame stage events.
    let mut sums: BTreeMap<String, (u64, f64, f64)> = BTreeMap::new();
    for line in text.lines().filter(|l| l.contains("frame stages")) {
        for part in line.split_whitespace() {
            if let Some((name, value)) = part.split_once('=')
                && name.ends_with("_us")
                && let Ok(us) = value.parse::<f64>()
            {
                let entry = sums.entry(name.to_owned()).or_default();
                entry.0 += 1;
                entry.1 += us;
                entry.2 = entry.2.max(us);
            }
        }
    }
    for (name, (n, total, max)) in sums {
        println!(
            "{label} stage {name}: n={n} mean {:.2} ms max {:.2} ms",
            total / n as f64 / 1000.0,
            max / 1000.0
        );
    }
    for line in text
        .lines()
        .filter(|l| {
            l.contains("codec chosen")
                || l.contains("candidate refused")
                || l.contains("video on")
                || l.contains("GPU MF input")
        })
        .take(12)
    {
        println!("{label} log: {}", &line[..line.len().min(220)]);
    }
}

#[test]
#[ignore = "live probe: animates a window, captures it and encodes it; --ignored --nocapture"]
fn e2_pace_probe() {
    let log = LogBuffer::default();
    let subscriber = {
        let writer = log.clone();
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_target(true)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish()
    };
    tracing::subscriber::set_global_default(subscriber).ok();
    let secs = env_num("E2_PACE_SECS", 6);
    let size = env_pair("E2_PACE_SIZE", (1000, 700), 'x');
    let at = env_pair("E2_PACE_POS", (100, 100), ',');
    let title = format!("CPE2PACE probe {}", std::process::id());
    let mut window = AnimWindow::spawn(&title, Duration::from_secs(secs * 8 + 60));
    let displays = WindowsDisplays::new().expect("displays");
    let source =
        WindowsWindowSource::new(displays.ids(), displays.monitor_reader()).expect("window source");
    let mut id = None;
    for _ in 0..50 {
        id = source
            .windows()
            .ok()
            .and_then(|list| list.into_iter().find(|w| w.title.contains(&title)))
            .map(|w| w.id);
        if id.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let id = id.expect("probe window never appeared in the window source");
    println!("window {title} id {id:?} size {size:?} at {at:?}; {secs} s per phase");
    std::thread::sleep(Duration::from_millis(500));

    // The capture worker's poll wait: a 5 ms `recv_timeout` is only as fine as the system timer.
    {
        let (_keep, wait) = mpsc::channel::<()>();
        let started = Instant::now();
        for _ in 0..40 {
            let _ = wait.recv_timeout(Duration::from_millis(5));
        }
        println!(
            "recv_timeout(5 ms) takes {:.1} ms on average (system timer tick, not 5)",
            started.elapsed().as_secs_f64() * 1000.0 / 40.0
        );
    }

    let gate = open_gate();
    let mut cpu_capture =
        WindowsFrameCapture::new(gate.clone(), source.resolver()).expect("capture");
    raw_arrival(&mut cpu_capture, id, "CPU slots (no enable_gpu)", secs);

    let mut capture = WindowsFrameCapture::new(gate, source.resolver()).expect("capture");
    let gpu = capture
        .enable_gpu(wgpu::Features::empty())
        .expect("enable_gpu");
    raw_arrival(&mut capture, id, "GPU slots", secs);
    if std::env::var("E2_PACE_RAW_ONLY").is_err() {
        let mut runs = vec![(false, true), (true, true)];
        if std::env::var("E2_PACE_CPU").is_ok() {
            runs.push((true, false));
        }
        for (region, use_gpu) in runs {
            pipeline(&mut capture, id, &gpu, region, use_gpu, secs, &log);
        }
    }
    window.close();
    let _ = std::io::stdout().flush();
    // Dropping the GPU context faults in teardown (known, outside this probe).
    std::process::exit(0);
}
