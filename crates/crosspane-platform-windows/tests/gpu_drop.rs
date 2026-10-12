//! Drop regressions for `WindowsGpu`: creating one on the default adapter and dropping it
//! (with and without a surface handoff in between) must end the process normally, not with
//! STATUS_ACCESS_VIOLATION. Live GPU, no window and no capture; run non-elevated:
//! `cargo test -p crosspane-platform-windows --features gpu --test gpu_drop -- --ignored --nocapture`
//! Each case should be run on its own (`--exact`), because a fault ends the whole process.
#![cfg(all(windows, feature = "gpu"))]
#![allow(unsafe_code, clippy::unwrap_used, clippy::expect_used, dead_code)]

use crosspane_platform::IoGate;
use crosspane_platform_windows::model;
use crosspane_types::geom::PixelSize;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::HMODULE,
    Graphics::{
        Direct3D::D3D_DRIVER_TYPE_HARDWARE,
        Direct3D11::{
            D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
            D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
        },
        Direct3D12::ID3D12Fence,
    },
};

#[allow(dead_code)]
#[path = "../src/gpu.rs"]
mod gpu;

/// The default-adapter D3D11 device the capture backend builds (`frame_capture::Graphics`).
fn capture_device() -> (ID3D11Device, ID3D11DeviceContext) {
    let mut device = None;
    let mut context = None;
    // SAFETY: initialized output Options; same flags and default adapter as the capture backend.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .expect("D3D11CreateDevice");
    (device.expect("device"), context.expect("context"))
}

/// Prints the faulting address, module and a backtrace for a native fault, then lets the
/// process die the way it would have.
fn report_faults() {
    use std::ffi::c_void;
    use windows_sys::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        GetModuleFileNameW, GetModuleHandleExW,
    };
    // Declared by hand: the windows-sys type lives behind a feature this crate does not enable.
    #[repr(C)]
    struct ExceptionRecord {
        code: i32,
        flags: u32,
        chain: *mut ExceptionRecord,
        address: *mut c_void,
        parameters: u32,
        information: [usize; 15],
    }
    #[repr(C)]
    struct ExceptionPointers {
        record: *mut ExceptionRecord,
        context: *mut c_void,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn AddVectoredExceptionHandler(
            first: u32,
            handler: Option<unsafe extern "system" fn(*mut ExceptionPointers) -> i32>,
        ) -> *mut c_void;
    }
    unsafe extern "system" fn handler(info: *mut ExceptionPointers) -> i32 {
        // SAFETY: the OS passes valid exception pointers to a vectored handler.
        let (code, address, parameters) = unsafe {
            let record = &*(*info).record;
            (
                record.code as u32,
                record.address as usize,
                record.information,
            )
        };
        // 0xc0000005 access violation, 0xc0000374 heap corruption, 0xc0000409 fast fail.
        if matches!(code, 0xc000_0005 | 0xc000_0374 | 0xc000_0409 | 0xc000_00fd) {
            let mut module = std::ptr::null_mut();
            let mut name = [0u16; 260];
            // SAFETY: address lookup of a loaded module, no refcount change; bounded buffer.
            let (base, len) = unsafe {
                GetModuleHandleExW(
                    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                        | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                    address as *const u16,
                    &mut module,
                );
                (
                    module as usize,
                    GetModuleFileNameW(module, name.as_mut_ptr(), name.len() as u32) as usize,
                )
            };
            eprintln!(
                "FAULT code={code:#010x} at {address:#x} in {} (base {base:#x}, offset {:#x}); access kind={} target={:#x}",
                String::from_utf16_lossy(&name[..len]),
                address.wrapping_sub(base),
                parameters[0],
                parameters[1]
            );
            eprintln!("{}", std::backtrace::Backtrace::force_capture());
        }
        0 // EXCEPTION_CONTINUE_SEARCH
    }
    // SAFETY: registers a process-wide handler that only prints and continues the search.
    unsafe { AddVectoredExceptionHandler(1, Some(handler)) };
}

fn new_gpu() -> Option<Arc<gpu::WindowsGpu>> {
    let (device, context) = capture_device();
    match gpu::WindowsGpu::new(
        device,
        Arc::new(Mutex::new(context)),
        wgpu::Features::empty(),
    ) {
        Ok(gpu) => Some(gpu),
        Err(error) => {
            println!("WindowsGpu unavailable on this machine: {error:?}");
            None
        }
    }
}

#[test]
#[ignore = "live GPU: create-then-drop a WindowsGpu; --ignored --nocapture, one case per process"]
fn bare_create_then_drop() {
    report_faults();
    let Some(gpu) = new_gpu() else { return };
    println!("created {gpu:?}");
    drop(gpu);
    println!("dropped");
}

#[test]
#[ignore = "live GPU: one surface handoff, then drop; --ignored --nocapture, one case per process"]
fn surface_handoff_then_drop() {
    report_faults();
    let Some(gpu) = new_gpu() else { return };
    for (step, result) in gpu.diagnose(PixelSize::new(1280, 720)) {
        println!("{step}: {result:?}");
    }
    drop(gpu);
    println!("dropped");
}

#[test]
#[ignore = "live GPU: surface begin/finish, surface dropped before the gpu; one case per process"]
fn surface_dropped_before_the_gpu() {
    report_faults();
    let Some(gpu) = new_gpu() else { return };
    let surface = gpu::Surface::new(gpu.clone(), PixelSize::new(1280, 720)).expect("surface");
    let gate = IoGate::new();
    gate.set_session_permits(true);
    gate.set_engine_permits(true);
    let epoch = gate.epoch();
    {
        let mut lease = surface.begin(&gpu, gate, epoch).expect("begin");
        lease.finish().expect("finish");
    }
    drop(surface);
    println!("surface dropped");
    drop(gpu);
    println!("dropped");
}

#[test]
#[ignore = "live GPU: repeated create/drop cycles (the rebuild-after-removal pattern)"]
fn repeated_create_then_drop() {
    report_faults();
    for round in 0..5 {
        let Some(gpu) = new_gpu() else { return };
        let _ = gpu.diagnose(PixelSize::new(640, 360));
        drop(gpu);
        println!("round {round} dropped");
    }
}

#[test]
#[ignore = "live GPU: retire (as after device removal) and drop the retired instance"]
fn retired_gpu_then_drop() {
    report_faults();
    let Some(gpu) = new_gpu() else { return };
    gpu.retire();
    let _ = gpu.healthy();
    drop(gpu);
    println!("dropped");
}

/// Counts how many times its owner was released.
struct Counted(Arc<AtomicUsize>);
impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn signal(fence: &ID3D12Fence, value: u64) {
    // SAFETY: CPU-side signal of a live, unshared-in-this-test fence we own.
    unsafe { fence.Signal(value) }.expect("signal");
}

#[test]
#[ignore = "live GPU: deferred releases leave with the context (no Arc cycle through the retained list)"]
fn retained_resources_are_released_with_the_context() {
    report_faults();
    let Some(gpu) = new_gpu() else { return };
    let weak = Arc::downgrade(&gpu);
    let fence = gpu.fence().expect("fence");
    let released = Arc::new(AtomicUsize::new(0));
    gpu.hold(fence.d12.clone(), 1, Counted(released.clone()));
    gpu.healthy().expect("healthy");
    assert_eq!(
        released.load(Ordering::SeqCst),
        0,
        "fence has not passed yet"
    );
    signal(&fence.d12, 1);
    drop(gpu);
    assert_eq!(released.load(Ordering::SeqCst), 1, "released exactly once");
    assert!(
        weak.upgrade().is_none(),
        "a retained entry must not keep its own context alive"
    );
}

#[test]
#[ignore = "live GPU: a retired context abandons unfinished work instead of blocking its drop"]
fn retired_context_abandons_unfinished_work_without_blocking() {
    report_faults();
    let Some(gpu) = new_gpu() else { return };
    let weak = Arc::downgrade(&gpu);
    let fence = gpu.fence().expect("fence");
    let released = Arc::new(AtomicUsize::new(0));
    gpu.hold(fence.d12.clone(), 7, Counted(released.clone()));
    gpu.retire();
    let started = Instant::now();
    drop(gpu);
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "a retired context must not wait for a fence that may never pass"
    );
    assert_eq!(
        released.load(Ordering::SeqCst),
        0,
        "work that may still be running is leaked, never released early"
    );
    assert!(weak.upgrade().is_none());
}

#[test]
#[ignore = "live GPU: a healthy context waits for in-flight work before releasing it"]
fn healthy_context_waits_for_in_flight_work() {
    report_faults();
    let Some(gpu) = new_gpu() else { return };
    let fence = gpu.fence().expect("fence");
    let released = Arc::new(AtomicUsize::new(0));
    gpu.hold(fence.d12.clone(), 3, Counted(released.clone()));
    let late = fence.d12.clone();
    let signaller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        signal(&late, 3);
    });
    let started = Instant::now();
    drop(gpu);
    signaller.join().expect("signaller");
    assert!(
        started.elapsed() >= Duration::from_millis(150),
        "waited for the fence"
    );
    assert_eq!(released.load(Ordering::SeqCst), 1);
}

#[test]
#[ignore = "live GPU: a surface that holds the last reference tears its resources down first"]
fn surface_holding_the_last_reference() {
    report_faults();
    let Some(gpu) = new_gpu() else { return };
    let surface = gpu::Surface::new(gpu.clone(), PixelSize::new(1280, 720)).expect("surface");
    let gate = IoGate::new();
    gate.set_session_permits(true);
    gate.set_engine_permits(true);
    let epoch = gate.epoch();
    {
        let mut lease = surface.begin(&gpu, gate, epoch).expect("begin");
        lease.finish().expect("finish");
    }
    drop(gpu);
    drop(surface);
    println!("dropped");
}

#[test]
#[ignore = "live GPU: the capture facade with a GPU context tears down (gpu_latch_probe used to exit early)"]
fn capture_facade_with_gpu_drops_cleanly() {
    use crosspane_platform_windows::{
        displays::WindowsDisplays, frame_capture::WindowsFrameCapture, window::WindowsWindowSource,
    };
    report_faults();
    let displays = WindowsDisplays::new().expect("displays");
    let source =
        WindowsWindowSource::new(displays.ids(), displays.monitor_reader()).expect("window source");
    let capture = WindowsFrameCapture::new(IoGate::new(), source.resolver()).expect("capture");
    match capture.enable_gpu(wgpu::Features::empty()) {
        Ok(gpu) => {
            println!("enable_gpu ok {gpu:?}");
            drop(gpu);
        }
        Err(error) => println!("enable_gpu refused on this machine: {error:?}"),
    }
    drop(capture);
    println!("dropped");
}
