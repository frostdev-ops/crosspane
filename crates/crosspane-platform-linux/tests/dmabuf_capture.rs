//! GPU capture acceptance. All clients and IPC target only the named nested compositor.
#![cfg(feature = "gpu")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crosspane_platform::{CaptureTarget, Frame, FrameCapture, FrameEvent, IoGate};
use crosspane_platform_linux::dmabuf::texture_of;
use crosspane_platform_linux::hyprland::{frame_capture::HyprlandFrameCapture, ipc::HyprIpc};
use crosspane_types::geom::{PixelRect, euclid::point2};
use crosspane_types::id::DisplayId;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

struct Nested {
    ipc: HyprIpc,
    _lock: std::fs::File,
}
impl std::ops::Deref for Nested {
    type Target = HyprIpc;
    fn deref(&self) -> &Self::Target {
        &self.ipc
    }
}
fn nested() -> Option<Nested> {
    if std::env::var("CROSSPANE_NESTED_HYPR").as_deref() != Ok("1") {
        eprintln!("skipped: requires CROSSPANE_NESTED_HYPR=1 in gpu228 nested environment");
        return None;
    }
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").unwrap();
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(std::env::temp_dir().join(format!("crosspane-dmabuf-{signature}.lock")))
        .unwrap();
    // nextest runs separate processes; all five tests share one explicitly named compositor.
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive).unwrap();
    Some(Nested {
        ipc: HyprIpc::from_env().unwrap(),
        _lock: lock,
    })
}
fn gate() -> Arc<IoGate> {
    let gate = IoGate::new();
    gate.set_session_permits(true);
    gate.set_engine_permits(true);
    gate
}
fn frame(events: &mpsc::Receiver<FrameEvent>) -> Frame {
    match events.recv_timeout(Duration::from_secs(3)).unwrap() {
        FrameEvent::Frame { frame, .. } => frame,
        other => panic!("unexpected capture event: {other:?}"),
    }
}

fn start(
    ipc: &HyprIpc,
    gpu: bool,
    crop: Option<PixelRect>,
) -> (HyprlandFrameCapture, mpsc::Receiver<FrameEvent>) {
    let mut capture = HyprlandFrameCapture::new(gate(), ipc.clone()).unwrap();
    if gpu {
        let (device, _) = capture.enable_gpu(wgpu::Features::empty()).unwrap();
        eprintln!("capture device features: {:?}", device.features());
    }
    let display = DisplayId(ipc.monitor_ids().unwrap()[0].1);
    let (send, events) = mpsc::channel();
    capture
        .start(
            CaptureTarget::Display(display),
            crop,
            60,
            Arc::new(move |e| {
                let _ = send.send(e);
            }),
        )
        .unwrap();
    (capture, events)
}
struct Fixture {
    child: Child,
    ipc: HyprIpc,
    address: String,
    animated: bool,
}
impl Fixture {
    fn animate(&mut self) {
        self.ipc.eval(&format!(r#"
            local phase = 0
            crosspane_gpu228_timer = hl.timer(function()
                phase = (phase + 1) % 100
                hl.dispatch(hl.dsp.window.move({{x=20+phase,y=20,relative=false,window="address:{}"}}))
            end, {{timeout=8,type="repeat"}})
        "#, self.address)).unwrap();
        self.animated = true;
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if self.animated {
            let _ = self
                .ipc
                .eval("crosspane_gpu228_timer:set_enabled(false); crosspane_gpu228_timer = nil");
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn fixture(ipc: &HyprIpc) -> Fixture {
    let binary = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/crosspane-testapp");
    if !binary.exists() {
        assert!(
            Command::new("cargo")
                .args(["build", "--locked", "-p", "crosspane-testapp"])
                .status()
                .unwrap()
                .success()
        );
    }
    let child = Command::new(binary)
        .args(["window", "--title", "gpu228-pattern", "--size", "300x200"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let address = loop {
        let clients = ipc.json("clients").unwrap();
        if let Some(address) = clients
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["title"] == "gpu228-pattern")
            .and_then(|c| c["address"].as_str())
        {
            break address.to_owned();
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    };
    ipc.dispatch(&format!(
        r#"hl.dsp.window.float({{action="enable",window="address:{address}"}})"#
    ))
    .unwrap();
    ipc.dispatch(&format!(
        r#"hl.dsp.window.move({{x=0,y=0,relative=false,window="address:{address}"}})"#
    ))
    .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    Fixture {
        child,
        ipc: ipc.clone(),
        address,
        animated: false,
    }
}

#[test]
fn exactness() {
    let Some(ipc) = nested() else { return };
    let _fixture = fixture(&ipc);
    let crop = Some(PixelRect::new(point2(10, 10), point2(200, 150)));
    let (_shm, shm_events) = start(&ipc, false, crop);
    let reference = frame(&shm_events).to_cpu().unwrap();
    assert!(
        reference
            .0
            .as_chunks::<4>()
            .0
            .iter()
            .any(|p| p != &reference.0[..4]),
        "pattern crop is uniform"
    );
    let (_gpu, gpu_events) = start(&ipc, true, crop);
    let gpu_frame = frame(&gpu_events);
    let image = gpu_frame.native().expect("expected DMA-BUF frame");
    eprintln!("captured image: {image:?}");
    let (texture, origin) = texture_of(image.as_ref()).unwrap();
    assert_eq!(origin, (10, 10));
    assert_eq!(texture.format(), wgpu::TextureFormat::Bgra8Unorm);
    assert_eq!(gpu_frame.damage, None);
    let actual = gpu_frame.to_cpu().unwrap();
    assert_eq!(
        actual, reference,
        "DMA-BUF bytes after ready differ from shm"
    );
    eprintln!("DMA-BUF read after ready is byte-exact without extra synchronisation");
}

struct Failures(Arc<std::sync::atomic::AtomicUsize>);
impl tracing::Subscriber for Failures {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct Reason(bool);
        impl tracing::field::Visit for Reason {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "reason" && format!("{value:?}").contains("BufferConstraints") {
                    self.0 = true;
                }
            }
        }
        let mut reason = Reason(false);
        event.record(&mut reason);
        if reason.0 {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

fn fd_count() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
}

#[test]
fn soak() {
    let Some(ipc) = nested() else { return };
    let failures = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    tracing::subscriber::set_global_default(Failures(failures.clone())).unwrap();
    let mut fixture = fixture(&ipc);
    fixture.animate();
    let (capture, events) = start(&ipc, true, None);
    let initial = frame(&events);
    assert!(initial.native().is_some());
    drop(initial);
    // Warm up every slot and the readback/map path before the fd baseline.
    for _ in 0..8 {
        frame(&events).to_cpu().unwrap();
    }
    let baseline = fd_count();
    let begin = Instant::now();
    let mut held = std::collections::VecDeque::<(Frame, Arc<[u8]>)>::new();
    for _ in 0..1000 {
        if held.len() == 2 {
            let (old, bytes) = held.pop_front().unwrap();
            assert_eq!(old.to_cpu().unwrap().0, bytes, "held slot was overwritten");
        }
        let next = frame(&events);
        assert!(
            next.native().is_some(),
            "DMA-BUF stream fell back during soak"
        );
        let contents = next.to_cpu().unwrap().0;
        held.push_back((next, contents));
    }
    let average = begin.elapsed().as_secs_f64() / 1000.0;
    eprintln!(
        "1000 ready frames; average interval {average:.6}s; fd baseline {baseline}, after {}",
        fd_count()
    );
    assert!(
        average <= 2.0 / 60.0,
        "ready average exceeds two frame intervals"
    );
    assert_eq!(fd_count(), baseline, "fd growth during capture");
    assert_eq!(
        failures.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "buffer_constraints failures during soak"
    );
    drop(held);
    drop(capture);
    drop(events);
}

#[test]
fn re_mode() {
    let Some(ipc) = nested() else { return };
    let mut fixture = fixture(&ipc);
    fixture.animate();
    let (capture, events) = start(&ipc, true, None);
    let before = frame(&events);
    assert!(before.native().is_some());
    let name = ipc.monitor_ids().unwrap()[0].0.clone();
    let target_width = before.size.width.saturating_sub(16).max(64);
    let target_height = before.size.height.saturating_sub(16).max(64);
    let command = format!(
        r#"hl.monitor({{output="{name}", mode="{target_width}x{target_height}@60", scale=1}})"#
    );
    assert_eq!(
        ipc.request(&format!("eval {command}")).unwrap().trim(),
        "ok"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let next = frame(&events);
        assert!(next.native().is_some());
        if next.size != before.size {
            eprintln!(
                "re-mode {:?} -> {:?}: native frames resumed",
                before.size, next.size
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "nested output mode did not change"
        );
    }
    let restore = format!(
        r#"hl.monitor({{output="{name}", mode="{}x{}@60", scale=1}})"#,
        before.size.width, before.size.height
    );
    assert_eq!(
        ipc.request(&format!("eval {restore}")).unwrap().trim(),
        "ok"
    );
    drop(capture);
}

#[test]
fn fallback() {
    let Some(ipc) = nested() else { return };
    if std::env::var("CROSSPANE_TEST_EMPTY_DMABUF_MODIFIERS").as_deref() != Ok("1") {
        drop(ipc);
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "fallback", "--nocapture"])
            .env("CROSSPANE_TEST_EMPTY_DMABUF_MODIFIERS", "1")
            .output()
            .unwrap();
        eprintln!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success());
        return;
    }
    let mut fixture = fixture(&ipc);
    fixture.animate();
    let (_capture, events) = start(&ipc, true, None);
    for _ in 0..4 {
        assert!(
            frame(&events).cpu_pixels().is_some(),
            "forced empty modifier intersection must use shm"
        );
    }
}

#[test]
fn texture_and_crop() {
    let Some(ipc) = nested() else { return };
    let crop = Some(PixelRect::new(point2(7, 9), point2(107, 109)));
    let (_capture, events) = start(&ipc, true, crop);
    let next = frame(&events);
    let image = next.native().unwrap();
    assert_eq!(texture_of(image.as_ref()).unwrap().1, (7, 9));
    let (_shm, events) = start(&ipc, false, crop);
    assert!(frame(&events).native().is_none());
    #[derive(Debug)]
    struct Other;
    impl crosspane_platform::NativeImage for Other {
        fn size(&self) -> crosspane_types::geom::PixelSize {
            crosspane_types::geom::PixelSize::new(1, 1)
        }
        fn read(
            &self,
            _: &mut dyn FnMut(&[u8], u32),
        ) -> Result<(), crosspane_platform::PlatformError> {
            Err(crosspane_platform::PlatformError::Unsupported("test"))
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }
    assert!(texture_of(&Other).is_none());
}
