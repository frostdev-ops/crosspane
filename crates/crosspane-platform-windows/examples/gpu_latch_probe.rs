//! Lead probe: which GPU admission step fails on this machine (no window is captured).
//! `cargo run -p crosspane-platform-windows --features gpu --example gpu_latch_probe`
//!
//! It also exercises teardown: the capture facade and its GPU context are dropped normally on
//! the way out (an access violation there used to force an early process exit).
#[cfg(all(windows, feature = "gpu"))]
fn main() -> Result<(), String> {
    use crosspane_platform::IoGate;
    use crosspane_platform_windows::{
        displays::WindowsDisplays, frame_capture::WindowsFrameCapture, window::WindowsWindowSource,
    };
    use crosspane_types::geom::PixelSize;
    let displays = WindowsDisplays::new().map_err(|e| format!("displays: {e:?}"))?;
    let source = WindowsWindowSource::new(displays.ids(), displays.monitor_reader())
        .map_err(|e| format!("window source: {e:?}"))?;
    let capture = WindowsFrameCapture::new(IoGate::new(), source.resolver())
        .map_err(|e| format!("capture: {e:?}"))?;
    let gpu = match capture.enable_gpu(wgpu::Features::empty()) {
        Ok(gpu) => gpu,
        Err(error) => {
            eprintln!("enable_gpu: REFUSED {error:?}");
            return Ok(());
        }
    };
    eprintln!("enable_gpu: ok {gpu:?}");
    for size in [PixelSize::new(1280, 720), PixelSize::new(1920, 1080)] {
        for _ in 0..3 {
            for (step, result) in gpu.diagnose(size) {
                eprintln!("{size:?} {step}: {result:?}");
            }
        }
    }
    drop(gpu);
    drop(capture);
    eprintln!("teardown: ok");
    Ok(())
}
#[cfg(not(all(windows, feature = "gpu")))]
fn main() {}
