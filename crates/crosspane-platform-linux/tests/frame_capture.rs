//! E2 acceptance against dedicated nested compositors; never connect to the owner's session.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use crosspane_platform::{
    CaptureTarget, Frame, FrameCapture, FrameEvent, IoGate, PlatformError, PointerInjector,
    StreamEndReason, StreamId,
};
use crosspane_platform_linux::hyprland::frame_capture::HyprlandFrameCapture;
use crosspane_platform_linux::hyprland::inject::connect;
use crosspane_platform_linux::hyprland::ipc::HyprIpc;
use crosspane_types::geom::{PixelRect, PixelSize, PointDevice, euclid::point2};
use crosspane_types::id::{DisplayId, WindowId};

fn nested_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/hypr-nested.sh")
}

struct Nest(String);

impl Drop for Nest {
    fn drop(&mut self) {
        assert!(
            Command::new(nested_script())
                .args(["stop", "--name", &self.0])
                .status()
                .unwrap()
                .success()
        );
    }
}

// Other platform acceptance tests move windows and outputs concurrently. Re-execute this test
// inside its own nest, without changing process-global environment variables.
fn dedicated(test: &str) -> Option<HyprIpc> {
    if std::env::var("CROSSPANE_NESTED_HYPR").as_deref() != Ok("1") {
        eprintln!("skipped: needs CROSSPANE_NESTED_HYPR=1 from scripts/hypr-nested.sh env");
        return None;
    }
    if std::env::var("CROSSPANE_FRAME_CHILD_TEST").as_deref() == Ok(test) {
        return Some(HyprIpc::from_env().unwrap());
    }
    let nest = Nest(format!("wp-2-8-{test}-{}", std::process::id()));
    let parent = std::env::var("CROSSPANE_PARENT_WAYLAND_DISPLAY")
        .unwrap_or_else(|_| std::env::var("WAYLAND_DISPLAY").unwrap());
    assert!(
        Command::new(nested_script())
            .args(["start", "--name", &nest.0])
            .env("WAYLAND_DISPLAY", parent)
            .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
            .env_remove("WAYLAND_SOCKET")
            .status()
            .unwrap()
            .success()
    );
    let exports = Command::new(nested_script())
        .args(["env", "--name", &nest.0])
        .output()
        .unwrap();
    assert!(exports.status.success());
    let mut child = Command::new(std::env::current_exe().unwrap());
    child
        .args(["--exact", test, "--nocapture"])
        .env("CROSSPANE_FRAME_CHILD_TEST", test)
        .env_remove("WAYLAND_SOCKET");
    for line in String::from_utf8(exports.stdout).unwrap().lines() {
        let (key, value) = line
            .strip_prefix("export ")
            .unwrap()
            .split_once('=')
            .unwrap();
        assert!(matches!(
            key,
            "WAYLAND_DISPLAY" | "HYPRLAND_INSTANCE_SIGNATURE" | "CROSSPANE_NESTED_HYPR"
        ));
        assert!(
            value
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        );
        child.env(key, value);
    }
    let result = child.output().unwrap();
    eprintln!(
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(result.status.success(), "dedicated capture test failed");
    None
}

fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(Instant::now() < deadline, "fixture/compositor timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct Fixture {
    child: Child,
    ipc: HyprIpc,
    address: String,
    log: PathBuf,
    reference: PathBuf,
    rgb: Vec<u8>,
}

impl Fixture {
    fn new(ipc: &HyprIpc) -> Self {
        let binary = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("crosspane-testapp");
        if !binary.is_file() {
            assert!(
                Command::new("cargo")
                    .args(["build", "--locked", "-p", "crosspane-testapp"])
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let prefix = std::env::temp_dir().join(format!("crosspane-frame-{}", std::process::id()));
        let log = prefix.with_extension("jsonl");
        let reference = prefix.with_extension("ppm");
        assert!(
            Command::new(&binary)
                .args(["pattern", "--size", "300x200", "--out"])
                .arg(&reference)
                .status()
                .unwrap()
                .success()
        );
        let ppm = std::fs::read(&reference).unwrap();
        let header = b"P6\n300 200\n255\n";
        assert!(ppm.starts_with(header));
        let child = Command::new(binary)
            .args([
                "window",
                "--title",
                "crosspane-frame-fixture",
                "--size",
                "300x200",
                "--events",
            ])
            .arg(&log)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let mut fixture = Self {
            child,
            ipc: ipc.clone(),
            address: String::new(),
            log,
            reference,
            rgb: ppm[header.len()..].to_vec(),
        };
        wait_until(|| {
            fixture.address = ipc
                .json("clients")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .find(|client| client["title"] == "crosspane-frame-fixture")
                .and_then(|client| client["address"].as_str())
                .unwrap_or_default()
                .to_owned();
            !fixture.address.is_empty()
        });
        ipc.dispatch(&format!(
            r#"hl.dsp.window.float({{action="enable",window="address:{}"}})"#,
            fixture.address
        ))
        .unwrap();
        ipc.dispatch(&format!(
            r#"hl.dsp.window.resize({{x=300,y=200,relative=false,window="address:{}"}})"#,
            fixture.address
        ))
        .unwrap();
        fixture.move_to(0, 0);
        wait_until(|| {
            std::fs::read_to_string(&fixture.log)
                .unwrap_or_default()
                .contains("\"event\":\"ready\"")
        });
        std::thread::sleep(Duration::from_millis(200));
        fixture
    }

    fn move_to(&self, x: i32, y: i32) {
        self.ipc
            .dispatch(&format!(
                r#"hl.dsp.window.move({{x={x},y={y},relative=false,window="address:{}"}})"#,
                self.address
            ))
            .unwrap();
    }

    fn reference_rgb(&self, x: u32, y: u32) -> [u8; 3] {
        let offset = ((y * 300 + x) * 3) as usize;
        self.rgb[offset..offset + 3].try_into().unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.log);
        let _ = std::fs::remove_file(&self.reference);
    }
}

fn open_gate() -> Arc<IoGate> {
    let gate = IoGate::new();
    gate.set_session_permits(true);
    gate.set_engine_permits(true);
    gate
}

fn rgb(frame: &Frame, x: u32, y: u32) -> [u8; 3] {
    let offset = (y * frame.stride + x * 4) as usize;
    [
        frame.pixels[offset + 2],
        frame.pixels[offset + 1],
        frame.pixels[offset],
    ]
}

fn frame_matching(
    events: &mpsc::Receiver<FrameEvent>,
    stream: StreamId,
    predicate: impl Fn(&Frame) -> bool,
) -> Frame {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap()
        {
            FrameEvent::Frame { stream: id, frame } if id == stream && predicate(&frame) => {
                return frame;
            }
            FrameEvent::Frame { .. } => (),
            FrameEvent::Cursor { .. } | FrameEvent::CursorDefault { .. } => (),
            other => panic!("unexpected event: {other:?}"),
        }
    }
}

fn ended(events: &mpsc::Receiver<FrameEvent>, stream: StreamId, reason: StreamEndReason) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap()
        {
            FrameEvent::Frame { .. } => (),
            FrameEvent::Cursor { .. } | FrameEvent::CursorDefault { .. } => (),
            FrameEvent::Ended {
                stream: id,
                reason: why,
            } => {
                assert_eq!((id, why), (stream, reason));
                return;
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
}

#[test]
fn pixels_damage_crop_gate_and_stop() {
    let Some(ipc) = dedicated("pixels_damage_crop_gate_and_stop") else {
        return;
    };
    let fixture = Fixture::new(&ipc);
    let display = DisplayId(ipc.monitor_ids().unwrap().remove(0).1);
    let gate = open_gate();
    let mut capture = HyprlandFrameCapture::new(gate.clone(), ipc.clone()).unwrap();
    let (send, events) = mpsc::channel();
    let sink = Arc::new(move |event| {
        let _ = send.send(event);
    });
    assert!(matches!(
        capture.start(CaptureTarget::Window(WindowId(1)), None, 60, sink.clone()),
        Err(PlatformError::Unsupported(_))
    ));
    assert!(matches!(
        capture.start(
            CaptureTarget::Display(DisplayId(u32::MAX)),
            None,
            60,
            sink.clone()
        ),
        Err(PlatformError::NotFound)
    ));
    let started = Instant::now();
    let stream = capture
        .start(CaptureTarget::Display(display), None, 60, sink.clone())
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    let points = [
        (0, 5, [255, 0, 0]),
        (1, 1, [255, 255, 255]),
        (2, 1, [0, 0, 0]),
        (3, 3, [255, 255, 255]),
        (4, 3, [0, 0, 0]),
        (5, 4, [0, 0, 0]),
        (5, 5, [255, 255, 255]),
    ];
    let full = frame_matching(&events, stream, |_| true);
    for &(x, y, expected) in &points {
        assert_eq!(
            rgb(&full, x, y),
            expected,
            "reference position ({x}, {y}), frame {:?}, clients {}",
            full.size,
            ipc.json("clients").unwrap()
        );
    }
    // Parent tiling can resize a nest while this test runs. Sample the visible reference pattern,
    // and use the captured dimensions instead of comparing two asynchronous geometry snapshots.
    assert!(full.size.width >= 12 && full.size.height >= 12);
    assert_eq!(full.stride, full.size.width * 4);
    assert_eq!(full.pixels.len(), (full.stride * full.size.height) as usize);
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let now_ns = now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64;
    assert!(full.at.as_nanos().abs_diff(now_ns) < 2_000_000_000);

    while events.try_recv().is_ok() {}
    fixture.move_to(2, 2);
    let moved = frame_matching(&events, stream, |frame| {
        points
            .iter()
            .all(|&(x, y, expected)| rgb(frame, x + 2, y + 2) == expected)
    });
    assert!(!moved.damage.as_ref().unwrap().is_empty());
    assert!(moved.at >= full.at);

    let crop = PixelRect::new(point2(4, 4), point2(12, 12));
    let started = Instant::now();
    capture.set_crop(stream, Some(crop)).unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    fixture.move_to(3, 2);
    let cropped = frame_matching(&events, stream, |frame| {
        frame.size == PixelSize::new(8, 8)
            && (0..8)
                .all(|y| (0..8).all(|x| rgb(frame, x, y) == fixture.reference_rgb(x + 1, y + 2)))
    });
    assert_eq!(cropped.stride, 32);
    assert_eq!(cropped.pixels.len(), 256);
    let second = capture
        .start(CaptureTarget::Display(display), None, 60, sink.clone())
        .unwrap();
    frame_matching(&events, second, |_| true);
    while events.try_recv().is_ok() {}
    gate.set_engine_permits(false);
    let mut blocked = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    while blocked.len() < 2 {
        match events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap()
        {
            FrameEvent::Ended { stream, reason } => {
                assert_eq!(reason, StreamEndReason::Blocked);
                blocked.push(stream);
            }
            FrameEvent::Frame { .. } if blocked.is_empty() => (),
            FrameEvent::Cursor { .. } if blocked.is_empty() => (),
            FrameEvent::CursorDefault { .. } if blocked.is_empty() => (),
            other => panic!("frame after gate shutdown: {other:?}"),
        }
    }
    blocked.sort();
    assert_eq!(blocked, vec![stream, second]);
    assert!(events.recv_timeout(Duration::from_millis(100)).is_err());
    assert!(matches!(
        capture.start(CaptureTarget::Display(display), None, 60, sink.clone()),
        Err(PlatformError::Locked)
    ));
    gate.set_engine_permits(true);
    fixture.move_to(4, 2);
    assert!(events.recv_timeout(Duration::from_millis(100)).is_err());

    let stream = capture
        .start(CaptureTarget::Display(display), None, 60, sink)
        .unwrap();
    frame_matching(&events, stream, |_| true);
    let started = Instant::now();
    capture.stop(stream).unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    ended(&events, stream, StreamEndReason::Requested);
    fixture.move_to(5, 2);
    assert!(events.recv_timeout(Duration::from_millis(100)).is_err());
}

fn hyprctl_output(arguments: &[&str]) {
    assert_eq!(std::env::var("CROSSPANE_NESTED_HYPR").as_deref(), Ok("1"));
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").unwrap();
    // The spec explicitly calls for hyprctl output create/remove in tests. Product IPC remains
    // exclusively HyprIpc::monitor_ids; every test command names the dedicated nest and times out.
    let result = Command::new("timeout")
        .args(["2", "hyprctl", "-i", &signature, "output"])
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(String::from_utf8(result.stdout).unwrap().trim(), "ok");
}

struct ExtraOutput(String);
impl Drop for ExtraOutput {
    fn drop(&mut self) {
        hyprctl_output(&["remove", &self.0]);
    }
}

#[test]
fn removed_output_ends_target_gone() {
    let Some(ipc) = dedicated("removed_output_ends_target_gone") else {
        return;
    };
    let gate = open_gate();
    let mut capture = HyprlandFrameCapture::new(gate, ipc.clone()).unwrap();
    let before = ipc.monitor_ids().unwrap();
    hyprctl_output(&["create", "wayland"]);
    let mut output = None;
    wait_until(|| {
        output = ipc
            .monitor_ids()
            .unwrap()
            .into_iter()
            .find(|pair| !before.contains(pair));
        output.is_some()
    });
    let (name, id) = output.unwrap();
    let output = ExtraOutput(name);
    let (send, events) = mpsc::channel();
    let stream = capture
        .start(
            CaptureTarget::Display(DisplayId(id)),
            None,
            60,
            Arc::new(move |event| {
                let _ = send.send(event);
            }),
        )
        .unwrap();
    frame_matching(&events, stream, |_| true);
    drop(output);
    ended(&events, stream, StreamEndReason::TargetGone);
    assert!(events.recv_timeout(Duration::from_millis(100)).is_err());
}

#[test]
fn continuously_animating_window_at_60_fps() {
    let Some(ipc) = dedicated("continuously_animating_window_at_60_fps") else {
        return;
    };
    let fixture = Fixture::new(&ipc);
    let display = DisplayId(ipc.monitor_ids().unwrap().remove(0).1);
    let mut capture = HyprlandFrameCapture::new(open_gate(), ipc.clone()).unwrap();
    let (send, events) = mpsc::channel();
    let stream = capture
        .start(
            CaptureTarget::Display(display),
            None,
            60,
            Arc::new(move |event| {
                let _ = send.send((Instant::now(), event));
            }),
        )
        .unwrap();
    // The reference app is static. Animate its actual window at 125 Hz using a compositor timer,
    // without modifying the fixture or adding an animation dependency to the adapter crate.
    ipc.eval(&format!(
        r#"
        local phase = 0
        crosspane_wp28_timer = hl.timer(function()
            phase = (phase + 1) % 100
            hl.dispatch(hl.dsp.window.move({{x=20+phase,y=20,relative=false,window="address:{}"}}))
        end, {{timeout=8,type="repeat"}})
    "#,
        fixture.address
    ))
    .unwrap();
    let start = Instant::now();
    let deadline = start + Duration::from_secs(3);
    let mut delivered = Vec::new();
    let mut damaged = 0;
    while let Ok((at, event)) =
        events.recv_timeout(deadline.saturating_duration_since(Instant::now()))
    {
        match event {
            FrameEvent::Cursor { .. } | FrameEvent::CursorDefault { .. } => (),
            FrameEvent::Frame { stream: id, frame } => {
                assert_eq!(id, stream);
                if at >= start {
                    delivered.push(at);
                }
                if frame
                    .damage
                    .as_ref()
                    .is_some_and(|damage| !damage.is_empty())
                {
                    damaged += 1;
                }
            }
            other => panic!("capture ended during animation: {other:?}"),
        }
        if Instant::now() >= deadline {
            break;
        }
    }
    ipc.eval("crosspane_wp28_timer:set_enabled(false); crosspane_wp28_timer = nil")
        .unwrap();
    let fps = delivered.len() as f64 / start.elapsed().as_secs_f64();
    assert!(delivered.len() >= 2 && damaged > 0);
    assert!(
        delivered
            .windows(2)
            .all(|pair| pair[1].duration_since(pair[0])
                >= Duration::from_nanos(1_000_000_000_u64.div_ceil(60)))
    );
    assert!(fps <= 60.0, "delivered {fps:.2} fps");
    eprintln!(
        "WP-2.8 max_fps=60: {} frames, {:.3} seconds, {fps:.2} fps",
        delivered.len(),
        start.elapsed().as_secs_f64()
    );
    capture.stop(stream).unwrap();
}

#[test]
fn cursor_image_and_stop() {
    let Some(ipc) = dedicated("cursor_image_and_stop") else {
        return;
    };
    let display = DisplayId(ipc.monitor_ids().unwrap().remove(0).1);
    let gate = open_gate();
    // Ensure the nest has a pointer device before the capture worker binds its seat.
    let (_keys, mut pointer) = connect(gate.clone(), ipc.clone()).unwrap();
    let mut capture = HyprlandFrameCapture::new(gate, ipc.clone()).unwrap();
    let (send, events) = mpsc::channel();
    let stream = capture
        .start(
            CaptureTarget::Display(display),
            None,
            60,
            Arc::new(move |event| {
                let _ = send.send(event);
            }),
        )
        .unwrap();
    // Preserve the first cursor event queued during start: an unchanged cursor may never
    // produce a second frame. Obtain dimensions without consuming capture events.
    let monitors = ipc.json("monitors").unwrap();
    let monitor = monitors
        .as_array()
        .unwrap()
        .iter()
        .find(|monitor| monitor["id"].as_u64() == Some(u64::from(display.0)))
        .unwrap();
    let width = monitor["width"].as_f64().unwrap();
    let height = monitor["height"].as_f64().unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    pointer
        .move_to(display, PointDevice::new(width / 2.0, height / 2.0))
        .unwrap();
    let image = loop {
        match events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap()
        {
            FrameEvent::Cursor {
                stream: id,
                cursor: Some(image),
            } => {
                assert_eq!(id, stream);
                break Some(image);
            }
            FrameEvent::CursorDefault { stream: id } => {
                assert_eq!(id, stream);
                break None;
            }
            FrameEvent::Frame { .. } => (),
            other => panic!("unexpected cursor capture event: {other:?}"),
        }
    };
    if let Some(image) = image {
        assert!(image.size.width > 0 && image.size.width <= 256);
        assert!(image.size.height > 0 && image.size.height <= 256);
        assert_eq!(
            image.pixels.len(),
            image.size.width as usize * image.size.height as usize * 4
        );
        assert!(image.hotspot.0 < image.size.width && image.hotspot.1 < image.size.height);
        assert!(
            image
                .pixels
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[3] == 255)
        );
    }

    capture.stop(stream).unwrap();
    // Drain events already delivered before stop; Ended is the stream's terminal barrier.
    ended(&events, stream, StreamEndReason::Requested);
    pointer
        .move_to(display, PointDevice::new(width / 4.0, height / 4.0))
        .unwrap();
    assert!(events.recv_timeout(Duration::from_millis(250)).is_err());
}
