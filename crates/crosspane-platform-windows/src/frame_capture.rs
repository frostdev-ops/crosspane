//! Public WGC window and retained-monitor mirror capture. No picker or permission prompt.
//!
//! The unpackaged backend keeps the capture border required. Borderless consent needs
//! the packaging capability and an attended RequestAccessAsync path (W4.1).
//! Control waits are bounded; an already executing GPU/native call cannot be preempted.
#![allow(unsafe_code)]

use crate::{
    model::frame_capture::{
        Latest, MonitorSnapshotReader, MonitorTarget, TargetState, crop_rect, end_reason,
        failure_reason, refresh_monitor, resolve_monitor, surface_ready, validate_crop,
    },
    model::popup,
    window::{NativeWindow, PopupObserver, WindowResolver},
};
use crosspane_platform::{
    CaptureTarget, EventSink, Frame, FrameCapture, FrameEvent, FrameImage, IoGate, NativeImage,
    Permission, PlatformError, StreamEndReason, StreamId,
};
use crosspane_types::{
    geom::{PixelRect, PixelSize},
    id::WindowId,
};
use std::{
    any::Any,
    collections::BTreeMap,
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::{
    Foundation::TypedEventHandler,
    Graphics::{
        Capture::{
            Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem,
            GraphicsCaptureSession,
        },
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
        SizeInt32,
    },
    Win32::{
        Foundation::{E_ACCESSDENIED, HMODULE, HWND},
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::*,
            Dxgi::{
                Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
                DXGI_ERROR_WAS_STILL_DRAWING, IDXGIDevice,
            },
            Gdi::HMONITOR,
        },
        System::WinRT::{
            Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
            Graphics::Capture::IGraphicsCaptureItemInterop,
            RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize,
        },
        UI::WindowsAndMessaging::IsIconic,
    },
    core::{Interface, factory},
};

const BOUND: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(5);

#[cfg(test)]
fn popup_diagnostics() -> bool {
    std::env::var("CROSSPANE_WINDOWS_POPUP_PROBE").as_deref() == Ok("1")
        && std::env::var("CROSSPANE_WINDOWS_POPUP_SELECTOR").as_deref() == Ok("menu")
}

#[cfg(test)]
static POPUP_LAST_STAGE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

#[cfg(test)]
fn popup_stage(code: u8) {
    if !popup_diagnostics() {
        return;
    }
    let flag = match code {
        11 => 1 << 0,
        12 => 1 << 1,
        21 => 1 << 2,
        30 => 1 << 3,
        31 => 1 << 4,
        32 => 1 << 5,
        33 => 1 << 6,
        40 => 1 << 7,
        41 => 1 << 8,
        42 => 1 << 9,
        43 => 1 << 10,
        44 => 1 << 11,
        60 => 1 << 12,
        62 => 1 << 13,
        70 => 1 << 14,
        71 => 1 << 15,
        72 => 1 << 16,
        73 => 1 << 17,
        _ => return,
    };
    // Last observed boundary, never a claim that a native call blocked or closed successfully.
    POPUP_LAST_STAGE.store(code, Ordering::Relaxed);
    static SEEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if SEEN.fetch_or(flag, Ordering::Relaxed) & flag != 0 {
        return;
    }
    static LINES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    if LINES
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
            (n < 64).then(|| n + 1)
        })
        .is_err()
    {
        return;
    }
    use std::io::Write as _;
    let mut output = std::io::stdout().lock();
    let _ = writeln!(output, "W22C_CAPTURE_STAGE code={code}");
    let _ = output.flush();
}

#[cfg(test)]
fn popup_stop_witness(done: bool, joined: bool, fault: bool) {
    use std::io::Write as _;
    let mut output = std::io::stdout().lock();
    let last_stage = POPUP_LAST_STAGE.load(Ordering::Relaxed);
    let _ = writeln!(
        output,
        "W22C_CAPTURE_STOP done={} join_attempted={} joined={} fault={} last_stage={last_stage}",
        u8::from(done),
        u8::from(done),
        u8::from(joined),
        u8::from(fault)
    );
    let _ = output.flush();
}

fn backend(operation: &'static str) -> PlatformError {
    PlatformError::Backend(operation.into())
}
fn api(operation: &'static str, error: windows::core::Error) -> PlatformError {
    if error.code() == E_ACCESSDENIED {
        PlatformError::PermissionDenied(Permission::ScreenRecording)
    } else {
        PlatformError::Backend(format!(
            "{operation}: HRESULT 0x{:08x}",
            error.code().0 as u32
        ))
    }
}
fn handle(window: NativeWindow) -> HWND {
    HWND(window.hwnd as usize as *mut _)
}
fn size(value: SizeInt32) -> Result<PixelSize, PlatformError> {
    if value.Width <= 0 || value.Height <= 0 {
        return Err(backend("empty WGC content"));
    }
    Ok(PixelSize::new(value.Width as u32, value.Height as u32))
}

struct Shared {
    gate: Arc<IoGate>,
    resolver: WindowResolver,
    monitor_reader: Option<MonitorSnapshotReader>,
    alive: AtomicBool,
    fault: AtomicBool,
    #[cfg(feature = "gpu")]
    gpu: Mutex<Option<Arc<crate::gpu::WindowsGpu>>>,
    #[cfg(test)]
    popup_probes: Mutex<BTreeMap<StreamId, PopupProbeSnapshot>>,
}
impl Shared {
    fn permitted(&self, epoch: u64) -> bool {
        self.alive.load(Ordering::Acquire)
            && !self.fault.load(Ordering::Acquire)
            && self.gate.is_open()
            && self.gate.epoch() == epoch
    }
}

struct Call {
    until: Instant,
    abandoned: Arc<AtomicBool>,
    epoch: u64,
    operation: Operation,
    reply: mpsc::SyncSender<Result<(), PlatformError>>,
}
impl Call {
    fn check(&self, shared: &Shared) -> Result<(), PlatformError> {
        #[cfg(feature = "gpu")]
        let setup = matches!(self.operation, Operation::Gpu(_));
        #[cfg(not(feature = "gpu"))]
        let setup = false;
        // Device construction observes no captured surface and does not open the node gate.
        if !shared.alive.load(Ordering::Acquire)
            || shared.fault.load(Ordering::Acquire)
            || (!setup && !shared.permitted(self.epoch))
        {
            return Err(PlatformError::Locked);
        }
        if self.abandoned.load(Ordering::Acquire) || Instant::now() >= self.until {
            return Err(PlatformError::Timeout);
        }
        Ok(())
    }
}
enum Operation {
    #[cfg(feature = "gpu")]
    Gpu(wgpu::Features),
    Start {
        id: StreamId,
        target: CaptureTarget,
        crop: Option<PixelRect>,
        fps: u32,
        sink: Arc<dyn EventSink<FrameEvent>>,
    },
    Crop(StreamId, Option<PixelRect>),
    Stop(StreamId),
    #[cfg(test)]
    QualifyPopup {
        stream: StreamId,
        hwnd: u64,
        region: PixelRect,
        expected: [u8; 4],
    },
}

#[cfg(test)]
#[derive(Clone, Debug)]
#[allow(dead_code)] // Numeric private diagnostics consumed only by the owned ignored harness.
pub(crate) struct PopupProbeItem {
    pub candidate: popup::Candidate,
    pub content: Option<PixelSize>,
    pub clip: Option<popup::Blit>,
    pub alpha: popup::AlphaCounts,
}
#[cfg(test)]
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub(crate) struct PopupProbeSnapshot {
    pub snapshot: popup::Snapshot,
    pub root_content: PixelSize,
    pub popups: Vec<PopupProbeItem>,
    pub alpha: popup::Alpha,
}

/// Command facade. W2.5 supplies `WindowSource::resolver()` from the same live source.
pub struct WindowsFrameCapture {
    shared: Arc<Shared>,
    commands: mpsc::SyncSender<Call>,
    done: mpsc::Receiver<()>,
    thread: Option<JoinHandle<()>>,
    next: u64,
}
impl fmt::Debug for WindowsFrameCapture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WindowsFrameCapture")
            .finish_non_exhaustive()
    }
}
impl WindowsFrameCapture {
    /// Enable a source-only device before streams exist; CPU WGC remains available on refusal.
    #[cfg(feature = "gpu")]
    pub fn enable_gpu(
        &self,
        wanted: wgpu::Features,
    ) -> Result<Arc<crate::gpu::WindowsGpu>, PlatformError> {
        self.call(Operation::Gpu(wanted))?;
        self.shared
            .gpu
            .lock()
            .map_err(|_| backend("GPU initialization poisoned"))?
            .clone()
            .ok_or_else(|| backend("GPU initialization missing"))
    }
    pub fn new(gate: Arc<IoGate>, resolver: WindowResolver) -> Result<Self, PlatformError> {
        Self::with_reader(gate, resolver, None)
    }

    /// The platform retains WindowsDisplays; this weak reader supplies fresh coherent
    /// facts for display streams without adding a render/platform dependency or cache.
    #[cfg_attr(test, allow(dead_code))] // Existing private include fixtures use the window facade.
    pub fn new_with_monitor_reader(
        gate: Arc<IoGate>,
        resolver: WindowResolver,
        monitor_reader: MonitorSnapshotReader,
    ) -> Result<Self, PlatformError> {
        Self::with_reader(gate, resolver, Some(monitor_reader))
    }

    fn with_reader(
        gate: Arc<IoGate>,
        resolver: WindowResolver,
        monitor_reader: Option<MonitorSnapshotReader>,
    ) -> Result<Self, PlatformError> {
        let shared = Arc::new(Shared {
            gate,
            resolver,
            monitor_reader,
            alive: AtomicBool::new(true),
            fault: AtomicBool::new(false),
            #[cfg(feature = "gpu")]
            gpu: Mutex::new(None),
            #[cfg(test)]
            popup_probes: Mutex::new(BTreeMap::new()),
        });
        let (commands, receive) = mpsc::sync_channel(1);
        let (ready, initialized) = mpsc::sync_channel(1);
        let (finish, done) = mpsc::sync_channel(1);
        let owned = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("crosspane-wgc".into())
            .spawn(move || {
                if catch_unwind(AssertUnwindSafe(|| worker(&owned, receive, ready))).is_err() {
                    owned.fault.store(true, Ordering::Release);
                }
                owned.alive.store(false, Ordering::Release);
                let _ = finish.send(());
            })
            .map_err(|_| backend("WGC worker spawn"))?;
        let mut capture = Self {
            shared,
            commands,
            done,
            thread: Some(thread),
            next: 1,
        };
        match initialized.recv_timeout(BOUND) {
            Ok(Ok(())) => Ok(capture),
            Ok(Err(error)) => {
                capture.shared.alive.store(false, Ordering::Release);
                capture.thread.take();
                Err(error)
            }
            Err(_) => {
                capture.shared.alive.store(false, Ordering::Release);
                capture.thread.take();
                Err(PlatformError::Timeout)
            }
        }
    }

    fn call(&self, operation: Operation) -> Result<(), PlatformError> {
        let epoch = self.shared.gate.epoch();
        #[cfg(feature = "gpu")]
        let setup = matches!(operation, Operation::Gpu(_));
        #[cfg(not(feature = "gpu"))]
        let setup = false;
        if !setup && !self.shared.permitted(epoch) {
            return Err(PlatformError::Locked);
        }
        let until = Instant::now() + BOUND;
        let abandoned = Arc::new(AtomicBool::new(false));
        let (reply, receive) = mpsc::sync_channel(1);
        self.commands
            .try_send(Call {
                until,
                abandoned: Arc::clone(&abandoned),
                epoch,
                operation,
                reply,
            })
            .map_err(|_| backend("WGC worker busy"))?;
        match receive.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok(result) => result,
            Err(_) => {
                abandoned.store(true, Ordering::Release);
                Err(PlatformError::Timeout)
            }
        }
    }

    #[cfg(test)]
    #[allow(dead_code)] // Used only by the separately compiled, ignored fixture harness.
    pub(crate) fn stop_verified(mut self) -> bool {
        self.shared.alive.store(false, Ordering::Release);
        let Some(thread) = self.thread.take() else {
            return false;
        };
        let done = self.done.recv_timeout(BOUND).is_ok();
        let joined = done && thread.join().is_ok();
        let result = joined && !self.shared.fault.load(Ordering::Acquire);
        #[cfg(test)]
        if popup_diagnostics() {
            // Diagnostic load is separate; the original short-circuit result stays unchanged.
            popup_stop_witness(done, joined, self.shared.fault.load(Ordering::Acquire));
        }
        result
    }

    #[cfg(test)]
    #[allow(dead_code)] // Only the separately compiled Limited popup harness calls this hook.
    pub(crate) fn popup_probe_snapshot(&self, stream: StreamId) -> Option<PopupProbeSnapshot> {
        self.shared.popup_probes.lock().ok()?.get(&stream).cloned()
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn qualify_popup_premultiplied(
        &mut self,
        stream: StreamId,
        hwnd: u64,
        region: PixelRect,
        expected: [u8; 4],
    ) -> Result<(), PlatformError> {
        self.call(Operation::QualifyPopup {
            stream,
            hwnd,
            region,
            expected,
        })
    }
}
impl FrameCapture for WindowsFrameCapture {
    fn start(
        &mut self,
        target: CaptureTarget,
        crop: Option<PixelRect>,
        max_fps: u32,
        sink: Arc<dyn EventSink<FrameEvent>>,
    ) -> Result<StreamId, PlatformError> {
        let epoch = self.shared.gate.epoch();
        if !self.shared.permitted(epoch) {
            return Err(PlatformError::Locked);
        }
        match target {
            CaptureTarget::Window(_) => {}
            CaptureTarget::Display(_) if self.shared.monitor_reader.is_some() => {}
            CaptureTarget::Display(_) => {
                return Err(PlatformError::Unsupported("Windows display capture"));
            }
            _ => return Err(PlatformError::Unsupported("Windows capture target")),
        }
        validate_crop(crop)?;
        let _ = Latest::<()>::new(max_fps)?;
        // Resolve on the bounded worker, before any WGC activation, never on the caller.
        let id = StreamId(self.next);
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| backend("WGC stream IDs exhausted"))?;
        self.call(Operation::Start {
            id,
            target,
            crop,
            fps: max_fps,
            sink,
        })?;
        Ok(id)
    }
    fn set_crop(&mut self, stream: StreamId, crop: Option<PixelRect>) -> Result<(), PlatformError> {
        validate_crop(crop)?;
        self.call(Operation::Crop(stream, crop))
    }
    fn stop(&mut self, stream: StreamId) -> Result<(), PlatformError> {
        self.call(Operation::Stop(stream))
    }
}
impl Drop for WindowsFrameCapture {
    fn drop(&mut self) {
        self.shared.alive.store(false, Ordering::Release);
        if let Some(thread) = self.thread.take()
            && self.done.recv_timeout(BOUND).is_ok()
        {
            let _ = thread.join();
        }
    }
}

struct Runtime;
impl Runtime {
    fn new() -> Result<Self, PlatformError> {
        // SAFETY: balances this worker's successful MTA initialization on the same thread.
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.map_err(|e| api("RoInitialize", e))?;
        Ok(Self)
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        #[cfg(test)]
        popup_stage(72);
        // SAFETY: only this thread's successful RoInitialize is balanced, after WinRT objects drop.
        unsafe { RoUninitialize() };
        #[cfg(test)]
        popup_stage(73);
    }
}
struct Gpu {
    device: ID3D11Device,
    context: Arc<Mutex<ID3D11DeviceContext>>,
}
// The non-Send WinRT wrapper remains on its MTA worker. Native D3D11 objects
// have binding-provided Send/Sync; all immediate-context access is serialized.
struct Graphics {
    gpu: Arc<Gpu>,
    capture: IDirect3DDevice,
}
impl Graphics {
    fn new() -> Result<Self, PlatformError> {
        let mut device = None;
        let mut context = None;
        #[cfg(feature = "gpu")]
        let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
        #[cfg(not(feature = "gpu"))]
        let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
        // SAFETY: initialized output Options; no SINGLETHREADED flag, context access serialized.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                flags,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .or_else(|error| {
            if flags == D3D11_CREATE_DEVICE_BGRA_SUPPORT {
                return Err(error);
            }
            // SAFETY: identical initialized outputs; CPU WGC preserves the previous BGRA-only fallback.
            unsafe {
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
            }
        })
        .map_err(|e| api("D3D11CreateDevice", e))?;
        let device = device.ok_or_else(|| backend("missing D3D device"))?;
        let context = context.ok_or_else(|| backend("missing D3D context"))?;
        let dxgi: IDXGIDevice = device.cast().map_err(|e| api("D3D DXGI interface", e))?;
        // SAFETY: wraps this owned D3D11 device in the documented WinRT interface.
        let capture = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
            .and_then(|d| d.cast())
            .map_err(|e| api("WinRT D3D device", e))?;
        Ok(Self {
            gpu: Arc::new(Gpu {
                device,
                context: Arc::new(Mutex::new(context)),
            }),
            capture,
        })
    }
}
impl Gpu {
    fn texture(&self, size: PixelSize, staging: bool) -> Result<ID3D11Texture2D, PlatformError> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: size.width,
            Height: size.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: if staging {
                D3D11_USAGE_STAGING
            } else {
                D3D11_USAGE_DEFAULT
            },
            CPUAccessFlags: if staging {
                D3D11_CPU_ACCESS_READ.0 as u32
            } else {
                0
            },
            ..Default::default()
        };
        let mut texture = None;
        // SAFETY: valid single-sample BGRA descriptor; D3D validates supported dimensions.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .map_err(|e| api("create capture texture", e))?;
        texture.ok_or_else(|| backend("missing capture texture"))
    }
}

struct Image {
    size: PixelSize,
    texture: ID3D11Texture2D,
    gpu: Arc<Gpu>,
    gate: Arc<IoGate>,
    epoch: u64,
    #[cfg(feature = "gpu")]
    surface: Option<Arc<crate::gpu::Surface>>,
}
impl fmt::Debug for Image {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WgcImage")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}
impl Image {
    fn permitted(&self) -> bool {
        self.gate.is_open() && self.gate.epoch() == self.epoch
    }
}
struct Mapped<'a> {
    context: &'a ID3D11DeviceContext,
    texture: &'a ID3D11Texture2D,
}
impl Drop for Mapped<'_> {
    fn drop(&mut self) {
        // SAFETY: balances exactly the successful Map of this texture's only subresource.
        unsafe { self.context.Unmap(self.texture, 0) };
    }
}
impl NativeImage for Image {
    fn size(&self) -> PixelSize {
        self.size
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn read(&self, f: &mut dyn FnMut(&[u8], u32)) -> Result<(), PlatformError> {
        if !self.permitted() {
            return Err(PlatformError::Locked);
        }
        #[cfg(feature = "gpu")]
        if self
            .surface
            .as_ref()
            .is_some_and(|surface| !surface.readable())
        {
            return Err(backend("GPU image handoff incomplete"));
        }
        let until = Instant::now() + BOUND;
        let context = loop {
            match self.gpu.context.try_lock() {
                Ok(context) => break context,
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(backend("D3D context poisoned"));
                }
                Err(std::sync::TryLockError::WouldBlock) => {}
            }
            if !self.permitted() {
                return Err(PlatformError::Locked);
            }
            if Instant::now() >= until {
                return Err(PlatformError::Timeout);
            }
            thread::sleep(Duration::from_millis(1));
        };
        let staging = self.gpu.texture(self.size, true)?;
        if !self.permitted() {
            return Err(PlatformError::Locked);
        }
        // SAFETY: same-device identical-size/format single-subresource textures; context locked.
        unsafe { context.CopyResource(&staging, &self.texture) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        loop {
            if !self.permitted() {
                return Err(PlatformError::Locked);
            }
            if Instant::now() >= until {
                return Err(PlatformError::Timeout);
            }
            // SAFETY: valid CPU-readable staging texture/output, nonblocking GPU readiness check.
            match unsafe {
                context.Map(
                    &staging,
                    0,
                    D3D11_MAP_READ,
                    D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                    Some(&mut mapped),
                )
            } {
                Ok(()) => break,
                Err(e) if e.code() == DXGI_ERROR_WAS_STILL_DRAWING => {
                    thread::sleep(Duration::from_millis(1))
                }
                Err(e) => return Err(api("map captured image", e)),
            }
        }
        let _mapping = Mapped {
            context: &context,
            texture: &staging,
        };
        let row = self
            .size
            .width
            .checked_mul(4)
            .ok_or_else(|| backend("capture row overflow"))?;
        let len = (self.size.height as usize)
            .checked_mul(row as usize)
            .filter(|n| *n <= isize::MAX as usize)
            .ok_or_else(|| backend("capture mapping overflow"))?;
        if mapped.pData.is_null() || mapped.RowPitch < row {
            return Err(backend("invalid capture mapping"));
        }
        if !self.permitted() {
            return Err(PlatformError::Locked);
        }
        // Exclude driver row padding: only copied pixels are exposed to the consumer.
        let mut pixels = zeroize::Zeroizing::new(vec![0_u8; len]);
        for y in 0..self.size.height as usize {
            let offset = y
                .checked_mul(mapped.RowPitch as usize)
                .filter(|offset| {
                    offset
                        .checked_add(row as usize)
                        .is_some_and(|n| n <= isize::MAX as usize)
                })
                .ok_or_else(|| backend("capture row offset overflow"))?;
            // SAFETY: Map supplies these initialized pixel bytes for each validated texture row.
            let source = unsafe {
                std::slice::from_raw_parts(mapped.pData.cast::<u8>().add(offset), row as usize)
            };
            pixels[y * row as usize..(y + 1) * row as usize].copy_from_slice(source);
        }
        if !self.permitted() {
            return Err(PlatformError::Locked);
        }
        f(&pixels, row);
        Ok(())
    }
}

/// Downcasts only our own retained capture image. The lease holds queue ownership until drop.
#[cfg(feature = "gpu")]
pub fn capture_lease<'a>(
    image: &'a dyn NativeImage,
    gpu: &'a crate::gpu::WindowsGpu,
) -> Result<Option<crate::gpu::Lease<'a>>, PlatformError> {
    let Some(image) = image.as_any().downcast_ref::<Image>() else {
        return Ok(None);
    };
    let Some(surface) = &image.surface else {
        return Ok(None);
    };
    if !surface.belongs(gpu) {
        return Err(backend("capture GPU device mismatch"));
    }
    match surface.begin(gpu, image.gate.clone(), image.epoch) {
        Ok(lease) => Ok(Some(lease)),
        Err(error) => {
            if !matches!(error, PlatformError::Locked) {
                gpu.retire();
            }
            Err(error)
        }
    }
}

struct Held(Direct3D11CaptureFrame);
fn frame_result(
    result: windows::core::Result<Direct3D11CaptureFrame>,
) -> windows::core::Result<Option<Direct3D11CaptureFrame>> {
    match result {
        Ok(frame) => Ok(Some(frame)),
        // windows-core 0.62.2 represents successful/null WinRT results as Error::empty (S_OK).
        Err(error) if error.code().0 == 0 => Ok(None),
        Err(error) => Err(error),
    }
}
impl Drop for Held {
    fn drop(&mut self) {
        let _ = self.0.Close();
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Binding {
    Window { id: WindowId, native: NativeWindow },
    Display(MonitorTarget),
}
impl Binding {
    fn resolve(shared: &Shared, target: CaptureTarget) -> Result<Self, PlatformError> {
        match target {
            CaptureTarget::Window(id) => shared
                .resolver
                .resolve(id)
                .map(|native| Self::Window { id, native })
                .ok_or(PlatformError::NotFound),
            CaptureTarget::Display(id) => {
                let reader = shared
                    .monitor_reader
                    .as_ref()
                    .ok_or(PlatformError::Unsupported(
                        "Windows display capture without monitor reader",
                    ))?;
                resolve_monitor(&reader()?, id).map(Self::Display)
            }
            _ => Err(PlatformError::Unsupported("Windows capture target")),
        }
    }

    fn refresh(&self, shared: &Shared) -> Result<Self, PlatformError> {
        match self {
            Self::Window { id, .. } => {
                let fresh = Self::resolve(shared, CaptureTarget::Window(*id))?;
                if *self == fresh {
                    Ok(fresh)
                } else {
                    Err(PlatformError::NotFound)
                }
            }
            Self::Display(monitor) => {
                let reader = shared
                    .monitor_reader
                    .as_ref()
                    .ok_or(PlatformError::Unsupported(
                        "Windows display capture without monitor reader",
                    ))?;
                refresh_monitor(&reader()?, monitor).map(Self::Display)
            }
        }
    }

    fn unchanged(&self, shared: &Shared) -> bool {
        self.refresh(shared).is_ok_and(|fresh| fresh == *self)
    }

    fn state(&self) -> TargetState {
        match self {
            Self::Window { native, .. } => {
                // SAFETY: query only the freshly identity-checked window, no mutation.
                if unsafe { IsIconic(handle(*native)) }.as_bool() {
                    TargetState::Minimized
                } else {
                    TargetState::Live
                }
            }
            Self::Display(_) => TargetState::Live,
        }
    }
}

struct PopupAccess<'a> {
    shared: &'a Shared,
    epoch: u64,
    until: Instant,
    observer: Option<(&'a mut PopupObserver, &'a popup::Snapshot)>,
}
impl PopupAccess<'_> {
    fn ready(&self) -> Result<bool, PlatformError> {
        if !self.shared.permitted(self.epoch) {
            return Err(PlatformError::Locked);
        }
        Ok(Instant::now() < self.until)
    }
    fn local(&self) -> Result<(), PlatformError> {
        if self.ready()? {
            Ok(())
        } else {
            Err(PlatformError::Timeout)
        }
    }
    /// Must only run outside the D3D context lock; all observer native calls may reenter.
    fn check(&mut self) -> Result<(), PlatformError> {
        self.local()?;
        if let Some((observer, snapshot)) = &mut self.observer
            && !observer.unchanged(snapshot, self.until)
        {
            return Err(backend("popup admission changed"));
        }
        self.local()
    }
}

/// Read only the already admitted surface region. A single composition turn shares its
/// two-second deadline; cancellation surrounds driver calls but cannot preempt a driver.
fn read_region(
    access: &mut PopupAccess<'_>,
    gpu: &Gpu,
    texture: &ID3D11Texture2D,
    surface: PixelSize,
    roi: PixelRect,
) -> Result<Option<zeroize::Zeroizing<Vec<u8>>>, PlatformError> {
    access.check()?;
    if !surface_ready(surface, roi)? {
        return Ok(None);
    }
    let size = PixelSize::new(roi.width() as u32, roi.height() as u32);
    let count = popup::bytes(size).map_err(|_| backend("popup readback budget"))?;
    if count > popup::WORKING_BYTES {
        return Ok(None);
    }
    access.check()?; // Before creating another staging allocation.
    let staging = gpu.texture(size, true)?;
    access.check()?; // Exact topology before source Copy; no context lock held here.
    let context = match gpu.context.try_lock() {
        Ok(context) => context,
        Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
        Err(std::sync::TryLockError::Poisoned(_)) => return Err(backend("D3D context poisoned")),
    };
    let area = D3D11_BOX {
        left: roi.min.x as u32,
        top: roi.min.y as u32,
        front: 0,
        right: roi.max.x as u32,
        bottom: roi.max.y as u32,
        back: 1,
    };
    access.local()?;
    // SAFETY: equal BGRA single-sample same-device textures, verified source ROI and locked context.
    unsafe { context.CopySubresourceRegion(&staging, 0, 0, 0, 0, texture, 0, Some(&area)) };
    drop(context);
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    let context = loop {
        access.check()?; // Each Map attempt freshly corroborates the snapshot outside the lock.
        let context = match gpu.context.try_lock() {
            Ok(context) => context,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(backend("D3D context poisoned"));
            }
        };
        access.local()?;
        // SAFETY: own CPU-readable staging texture and initialized exact output; nonblocking Map.
        match unsafe {
            context.Map(
                &staging,
                0,
                D3D11_MAP_READ,
                D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                Some(&mut mapped),
            )
        } {
            Ok(()) => break context,
            Err(e) if e.code() == DXGI_ERROR_WAS_STILL_DRAWING => {
                drop(context);
                thread::sleep(Duration::from_millis(1))
            }
            Err(e) => return Err(api("popup map", e)),
        }
    };
    let mapping = Mapped {
        context: &context,
        texture: &staging,
    };
    let row = size
        .width
        .checked_mul(4)
        .ok_or_else(|| backend("popup readback row"))?;
    if mapped.pData.is_null() || mapped.RowPitch < row {
        return Err(backend("popup mapping"));
    }
    access.local()?; // A slow successful Map cannot authorize another allocation after expiry.
    let mut pixels = zeroize::Zeroizing::new(vec![0; count]);
    for y in 0..size.height as usize {
        access.local()?;
        let offset = y
            .checked_mul(mapped.RowPitch as usize)
            .filter(|n| {
                n.checked_add(row as usize)
                    .is_some_and(|n| n <= isize::MAX as usize)
            })
            .ok_or_else(|| backend("popup mapping offset"))?;
        // SAFETY: Map supplies initialized bytes for each verified texture row; padding is excluded.
        let source = unsafe {
            std::slice::from_raw_parts(mapped.pData.cast::<u8>().add(offset), row as usize)
        };
        pixels[y * row as usize..(y + 1) * row as usize].copy_from_slice(source);
    }
    drop(mapping);
    drop(context);
    access.check()?; // Before any caller can commit these bytes to its cache.
    Ok(Some(pixels))
}

struct PopupCapture {
    candidate: popup::Candidate,
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    token: i64,
    closed: Arc<AtomicBool>,
    pixels: Option<zeroize::Zeroizing<Vec<u8>>>,
    content: Option<PixelSize>,
    counts: popup::AlphaCounts,
}
impl Drop for PopupCapture {
    fn drop(&mut self) {
        self.pixels.take();
        #[cfg(test)]
        popup_stage(30);
        let _ = self.session.Close();
        #[cfg(test)]
        popup_stage(31);
        let _ = self.pool.Close();
        #[cfg(test)]
        popup_stage(32);
        let _ = self.item.RemoveClosed(self.token);
        #[cfg(test)]
        popup_stage(33);
    }
}
impl PopupCapture {
    fn new(
        access: &mut PopupAccess<'_>,
        graphics: &Graphics,
        candidate: popup::Candidate,
    ) -> Result<Self, PlatformError> {
        access.check()?;
        let interop: IGraphicsCaptureItemInterop =
            factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
                .map_err(|e| api("popup item interop", e))?;
        access.check()?;
        // SAFETY: exact freshly process/thread/owner/generation-admitted popup HWND; no picker.
        let item: GraphicsCaptureItem = unsafe {
            interop.CreateForWindow(HWND(candidate.token.identity.hwnd as usize as *mut _))
        }
        .map_err(|e| api("popup item", e))?;
        access.check()?;
        let pool_size = item.Size().map_err(|e| api("popup item size", e))?;
        if size(pool_size)? != candidate.geometry.content {
            return Err(backend("popup capture origin mismatch"));
        }
        access.check()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &graphics.capture,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            pool_size,
        )
        .map_err(|e| api("popup pool", e))?;
        let session = match access.check().and_then(|()| {
            pool.CreateCaptureSession(&item)
                .map_err(|e| api("popup session", e))
        }) {
            Ok(session) => session,
            Err(e) => {
                let _ = pool.Close();
                return Err(e);
            }
        };
        let closed = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&closed);
        let handler = TypedEventHandler::new(move |_, _| {
            signal.store(true, Ordering::Release);
            Ok(())
        });
        let token = match access.check().and_then(|()| {
            item.Closed(&handler)
                .map_err(|e| api("popup close event", e))
        }) {
            Ok(token) => token,
            Err(e) => {
                let _ = session.Close();
                let _ = pool.Close();
                return Err(e);
            }
        };
        let capture = Self {
            candidate,
            item,
            pool,
            session,
            token,
            closed,
            pixels: None,
            content: None,
            counts: popup::AlphaCounts::default(),
        };
        access.check()?;
        capture
            .session
            .SetIsCursorCaptureEnabled(false)
            .map_err(|e| api("popup cursor disable", e))?;
        access.check()?;
        capture
            .session
            .SetIsBorderRequired(true)
            .map_err(|e| api("popup border required", e))?;
        access.check()?;
        capture
            .session
            .SetIncludeSecondaryWindows(false)
            .map_err(|e| api("popup secondary disable", e))?;
        access.check()?;
        if capture
            .session
            .IncludeSecondaryWindows()
            .map_err(|e| api("popup secondary state", e))?
        {
            return Err(backend("popup session admission"));
        }
        access.check()?;
        if !capture
            .session
            .IsBorderRequired()
            .map_err(|e| api("popup border state", e))?
        {
            return Err(backend("popup session admission"));
        }
        access.check()?;
        capture
            .session
            .StartCapture()
            .map_err(|e| api("popup capture start", e))?;
        access.check()?;
        Ok(capture)
    }

    fn update(
        &mut self,
        access: &mut PopupAccess<'_>,
        graphics: &Graphics,
    ) -> Result<bool, PlatformError> {
        access.check()?;
        if self.closed.load(Ordering::Acquire) {
            return Err(PlatformError::NotFound);
        }
        let mut latest = None;
        for _ in 0..4 {
            access.check()?;
            match frame_result(self.pool.TryGetNextFrame()).map_err(|e| api("popup dequeue", e))? {
                Some(frame) => {
                    latest = Some(Held(frame));
                }
                None => break,
            }
        }
        let Some(held) = latest else {
            return Ok(false);
        };
        access.check()?; // Dequeued frame has not yet been acquired/copied into our cache.
        let content = size(
            held.0
                .ContentSize()
                .map_err(|e| api("popup content size", e))?,
        )?;
        if content != self.candidate.geometry.content {
            return Err(backend("popup capture origin mismatch"));
        }
        access.check()?;
        let surface: IDirect3DDxgiInterfaceAccess = held
            .0
            .Surface()
            .and_then(|s| s.cast())
            .map_err(|e| api("popup surface", e))?;
        access.check()?;
        // SAFETY: documented retrieval from this admitted frame's retained surface.
        let texture: ID3D11Texture2D =
            unsafe { surface.GetInterface() }.map_err(|e| api("popup texture", e))?;
        access.check()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: owned live texture and initialized descriptor output.
        unsafe { texture.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM || desc.SampleDesc.Count != 1 {
            return Err(backend("popup surface format"));
        }
        let roi = PixelRect::new(
            (0, 0).into(),
            (content.width as i32, content.height as i32).into(),
        );
        if let Some(pixels) = read_region(
            access,
            &graphics.gpu,
            &texture,
            PixelSize::new(desc.Width, desc.Height),
            roi,
        )? {
            access.check()?;
            if self.closed.load(Ordering::Acquire) {
                return Err(PlatformError::NotFound);
            }
            self.pixels = Some(pixels);
            self.content = Some(content);
            return Ok(true);
        }
        Ok(false)
    }
}

struct Baseline {
    geometry: popup::Geometry,
    crop: PixelRect,
    size: PixelSize,
    texture: ID3D11Texture2D,
}
struct PopupState {
    observer: PopupObserver,
    snapshot: Option<popup::Snapshot>,
    sources: BTreeMap<u64, PopupCapture>,
    baseline: Option<Baseline>,
    dirty: bool,
    alpha: popup::Alpha,
    reported: Option<(usize, popup::Refusal)>,
    alpha_reported: bool,
}
impl Drop for PopupState {
    fn drop(&mut self) {
        self.sources.clear();
        self.baseline.take();
        // The same-thread observer/hooks and process handle drop only after all popup captures.
    }
}
impl PopupState {
    fn new(
        native: NativeWindow,
        resolver: WindowResolver,
        until: Instant,
    ) -> Result<Self, PlatformError> {
        Ok(Self {
            observer: PopupObserver::new(native, resolver, until)?,
            snapshot: None,
            sources: BTreeMap::new(),
            baseline: None,
            dirty: true,
            alpha: popup::Alpha::Threshold128,
            reported: None,
            alpha_reported: false,
        })
    }
    fn refresh(
        &mut self,
        shared: &Shared,
        graphics: &Graphics,
        epoch: u64,
        root_size: PixelSize,
        until: Instant,
    ) -> Result<(), PlatformError> {
        let mut snapshot = self.observer.snapshot(until)?;
        if snapshot.geometry.content != root_size {
            snapshot.popups.clear();
            snapshot.reason = popup::Refusal::Geometry;
            self.baseline = None;
        }
        if self.snapshot.as_ref() != Some(&snapshot) {
            #[cfg(test)]
            if snapshot.popups.is_empty() {
                popup_stage(60);
            }
            self.dirty = true;
            if self
                .baseline
                .as_ref()
                .is_some_and(|base| base.geometry != snapshot.geometry)
            {
                self.baseline = None;
            }
        }
        self.sources.retain(|hwnd, source| {
            snapshot
                .popups
                .iter()
                .any(|p| p.token.identity.hwnd == *hwnd && p == &source.candidate)
        });
        let mut refused = snapshot.refused;
        let mut reason = snapshot.reason;
        for candidate in &snapshot.popups {
            let mut access = PopupAccess {
                shared,
                epoch,
                until,
                observer: Some((&mut self.observer, &snapshot)),
            };
            access.check()?; // Do not admit/dequeue the next candidate after this turn expires.
            let hwnd = candidate.token.identity.hwnd;
            if let std::collections::btree_map::Entry::Vacant(entry) = self.sources.entry(hwnd) {
                match PopupCapture::new(&mut access, graphics, candidate.clone()) {
                    Ok(source) => {
                        entry.insert(source);
                    }
                    Err(PlatformError::Locked) => return Err(PlatformError::Locked),
                    Err(PlatformError::Timeout) => return Err(PlatformError::Timeout),
                    Err(_) => {
                        refused += 1;
                        reason = popup::Refusal::Capture;
                    }
                }
            }
            if let Some(source) = self.sources.get_mut(&hwnd) {
                match source.update(&mut access, graphics) {
                    Ok(changed) => self.dirty |= changed,
                    Err(PlatformError::Locked) => return Err(PlatformError::Locked),
                    Err(PlatformError::Timeout) => return Err(PlatformError::Timeout),
                    Err(_) => {
                        self.sources.remove(&hwnd);
                        refused += 1;
                        reason = popup::Refusal::Capture;
                        self.dirty = true;
                    }
                }
            }
            access.check()?; // Changed snapshot retires all admissions via caller invalidation.
        }
        let report = (refused, reason);
        if self.reported != Some(report) && report.1 != popup::Refusal::None {
            eprintln!(
                "WINDOWS_POPUP refused={} reason={}",
                report.0,
                report.1.code()
            );
        }
        self.reported = Some(report);
        self.snapshot = Some(snapshot);
        Ok(())
    }
    fn invalidate(&mut self) {
        self.sources.clear();
        self.snapshot = None;
        self.dirty = true;
    }
    fn baseline(
        &mut self,
        shared: &Shared,
        graphics: &Graphics,
        epoch: u64,
        texture: &ID3D11Texture2D,
        crop: PixelRect,
        until: Instant,
    ) -> Result<bool, PlatformError> {
        // False defers this fresh root for retry; true preserves the existing root-only fallback.
        // A newer root frame must never leave an older A baseline available after B delivery.
        let previous = self.baseline.take();
        let Some(snapshot) = &self.snapshot else {
            return Ok(true);
        };
        if popup::bytes(snapshot.geometry.content)
            .ok()
            .and_then(|n| n.checked_mul(3))
            .is_none_or(|n| n > popup::WORKING_BYTES)
        {
            return Ok(true);
        }
        let mut access = PopupAccess {
            shared,
            epoch,
            until,
            observer: Some((&mut self.observer, snapshot)),
        };
        access.check()?;
        let output = PixelSize::new(crop.width() as u32, crop.height() as u32);
        let base = match previous {
            Some(base) if base.geometry == snapshot.geometry && base.crop == crop => base,
            previous => {
                drop(previous); // Keep the admitted 3R live-resource reservation.
                access.check()?;
                Baseline {
                    geometry: snapshot.geometry,
                    crop,
                    size: output,
                    texture: graphics.gpu.texture(output, false)?,
                }
            }
        };
        access.check()?;
        let context = match graphics.gpu.context.try_lock() {
            Ok(context) => context,
            Err(std::sync::TryLockError::WouldBlock) => {
                #[cfg(test)]
                popup_stage(11);
                return Ok(false);
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(backend("D3D context poisoned"));
            }
        };
        access.local()?;
        let area = D3D11_BOX {
            left: crop.min.x as u32,
            top: crop.min.y as u32,
            front: 0,
            right: crop.max.x as u32,
            bottom: crop.max.y as u32,
            back: 1,
        };
        // SAFETY: root source bounds checked by caller; same-device/format free private baseline.
        unsafe {
            context.CopySubresourceRegion(&base.texture, 0, 0, 0, 0, texture, 0, Some(&area))
        };
        drop(context);
        access.check()?;
        self.baseline = Some(base);
        self.dirty = true;
        #[cfg(test)]
        popup_stage(12);
        Ok(true)
    }
    fn clean_texture(&self, crop: PixelRect) -> Option<ID3D11Texture2D> {
        let base = self.baseline.as_ref()?;
        (base.crop == crop
            && self
                .snapshot
                .as_ref()
                .is_none_or(|snapshot| base.geometry == snapshot.geometry))
        .then(|| base.texture.clone())
    }
    fn compose(
        &mut self,
        output: PixelSize,
        crop: PixelRect,
        shared: &Shared,
        graphics: &Graphics,
        epoch: u64,
        until: Instant,
    ) -> Result<Option<zeroize::Zeroizing<Vec<u8>>>, PlatformError> {
        let (Some(base), Some(snapshot)) = (&self.baseline, &self.snapshot) else {
            return Ok(None);
        };
        if base.geometry != snapshot.geometry
            || base.crop != crop
            || base.size != output
            || !self.sources.values().any(|source| source.pixels.is_some())
        {
            return Ok(None);
        }
        let full = PixelRect::new(
            (0, 0).into(),
            (output.width as i32, output.height as i32).into(),
        );
        let mut access = PopupAccess {
            shared,
            epoch,
            until,
            observer: Some((&mut self.observer, snapshot)),
        };
        let Some(baseline) = read_region(&mut access, &graphics.gpu, &base.texture, output, full)?
        else {
            return Ok(None);
        };
        let mut keys = Vec::new();
        let mut layers = Vec::new();
        for candidate in &snapshot.popups {
            access.local()?;
            if let Some(source) = self.sources.get(&candidate.token.identity.hwnd)
                && let Some(input) = &source.pixels
                && let Some(blit) = popup::clip(snapshot.geometry, candidate.geometry, crop)
                    .map_err(|_| backend("popup clip geometry"))?
            {
                keys.push(candidate.token.identity.hwnd);
                layers.push(popup::Layer {
                    pixels: input,
                    size: candidate.geometry.content,
                    blit,
                });
            }
        }
        access.check()?;
        let (pixels, counts) = popup::rebuild(&baseline, output, &layers, self.alpha)
            .map_err(|_| backend("popup composite geometry"))?;
        access.check()?;
        for (hwnd, counts) in keys.into_iter().zip(counts) {
            if counts.fractional != 0
                && self.alpha == popup::Alpha::Threshold128
                && !self.alpha_reported
            {
                eprintln!("WINDOWS_POPUP alpha=threshold128 status=U");
                self.alpha_reported = true;
            }
            if let Some(source) = self.sources.get_mut(&hwnd) {
                source.counts = counts;
            }
        }
        Ok(Some(pixels))
    }
    fn unchanged(&mut self, until: Instant) -> bool {
        self.snapshot
            .as_ref()
            .is_some_and(|snapshot| self.observer.unchanged(snapshot, until))
    }
    #[cfg(test)]
    fn qualify(
        &mut self,
        hwnd: u64,
        region: PixelRect,
        expected: [u8; 4],
        until: Instant,
    ) -> Result<(), PlatformError> {
        if !self.unchanged(until) || expected[3] != 128 || expected[..3].iter().any(|c| *c > 128) {
            return Err(backend("popup alpha qualification"));
        }
        let source = self.sources.get(&hwnd).ok_or(PlatformError::NotFound)?;
        let pixels = source
            .pixels
            .as_ref()
            .ok_or_else(|| backend("popup alpha not ready"))?;
        let size = source.candidate.geometry.content;
        if !surface_ready(size, region)? || region.min.x < 0 || region.min.y < 0 {
            return Err(backend("popup alpha region"));
        }
        for y in region.min.y as usize..region.max.y as usize {
            if Instant::now() >= until {
                return Err(PlatformError::Timeout);
            }
            for x in region.min.x as usize..region.max.x as usize {
                let offset = (y * size.width as usize + x) * 4;
                if pixels[offset..offset + 4] != expected {
                    return Err(PlatformError::Unsupported(
                        "popup fractional alpha convention",
                    ));
                }
            }
        }
        if !self.unchanged(until) {
            return Err(backend("popup alpha qualification"));
        }
        self.alpha = popup::Alpha::Premultiplied;
        self.dirty = true;
        Ok(())
    }
    #[cfg(test)]
    fn probe(
        &self,
        root_content: PixelSize,
        crop: Option<PixelRect>,
    ) -> Option<PopupProbeSnapshot> {
        let snapshot = self.snapshot.clone()?;
        let roi = crop_rect(root_content, crop)?;
        let popups = snapshot
            .popups
            .iter()
            .map(|candidate| {
                let source = self.sources.get(&candidate.token.identity.hwnd);
                PopupProbeItem {
                    candidate: candidate.clone(),
                    content: source.and_then(|s| s.content),
                    clip: popup::clip(snapshot.geometry, candidate.geometry, roi)
                        .ok()
                        .flatten(),
                    alpha: source.map_or_else(popup::AlphaCounts::default, |s| s.counts),
                }
            })
            .collect();
        Some(PopupProbeSnapshot {
            snapshot,
            root_content,
            popups,
            alpha: self.alpha,
        })
    }
}

struct Stream {
    binding: Binding,
    epoch: u64,
    abandoned: Arc<AtomicBool>,
    sink: Arc<dyn EventSink<FrameEvent>>,
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    token: i64,
    closed: Arc<AtomicBool>,
    pool_size: SizeInt32,
    latest: Latest<Held>,
    crop: Option<PixelRect>,
    slots: Vec<Arc<Image>>,
    cursor: crate::cursor::StreamCursor,
    popups: Option<PopupState>,
    delivery_popup: Option<(popup::Snapshot, Instant)>,
}
impl Drop for Stream {
    fn drop(&mut self) {
        #[cfg(test)]
        popup_stage(40);
        self.popups.take();
        #[cfg(test)]
        popup_stage(41);
        self.latest.clear();
        let _ = self.session.Close();
        #[cfg(test)]
        popup_stage(42);
        let _ = self.pool.Close();
        #[cfg(test)]
        popup_stage(43);
        let _ = self.item.RemoveClosed(self.token);
        #[cfg(test)]
        popup_stage(44);
    }
}
impl Stream {
    fn new(
        shared: &Shared,
        graphics: &Graphics,
        call: &Call,
        target: CaptureTarget,
        crop: Option<PixelRect>,
        fps: u32,
        sink: Arc<dyn EventSink<FrameEvent>>,
    ) -> Result<Self, PlatformError> {
        call.check(shared)?;
        let mut binding = Binding::resolve(shared, target)?;
        call.check(shared)?;
        if binding.state() == TargetState::Minimized {
            return Err(backend("WGC target minimized"));
        }
        let interop: IGraphicsCaptureItemInterop =
            factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
                .map_err(|e| api("WGC item interop", e))?;
        call.check(shared)?;
        binding = binding.refresh(shared)?;
        call.check(shared)?;
        let item: GraphicsCaptureItem = match &binding {
            Binding::Window { native, .. } => {
                // SAFETY: fresh resolver-admitted window identity, no picker or foreign source.
                unsafe { interop.CreateForWindow(handle(*native)) }
                    .map_err(|e| api("WGC window item", e))?
            }
            Binding::Display(monitor) => {
                // SAFETY: exact retained DisplayId from a fresh coherent W1.5c observation.
                // HMONITOR is observation-scoped; rechecked before starting and each delivery.
                unsafe { interop.CreateForMonitor(HMONITOR(monitor.handle as *mut _)) }
                    .map_err(|e| api("WGC monitor item", e))?
            }
        };
        let pool_size = item.Size().map_err(|e| api("WGC item size", e))?;
        size(pool_size)?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &graphics.capture,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            pool_size,
        )
        .map_err(|e| api("WGC frame pool", e))?;
        let session = match pool.CreateCaptureSession(&item) {
            Ok(session) => session,
            Err(e) => {
                let _ = pool.Close();
                return Err(api("WGC session", e));
            }
        };
        let closed = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&closed);
        let handler = TypedEventHandler::new(move |_, _| {
            signal.store(true, Ordering::Release);
            Ok(())
        });
        let token = match item.Closed(&handler) {
            Ok(token) => token,
            Err(e) => {
                let _ = session.Close();
                let _ = pool.Close();
                return Err(api("WGC close event", e));
            }
        };
        let popups = match &binding {
            Binding::Window { native, .. } => {
                match PopupState::new(*native, shared.resolver.clone(), call.until) {
                    Ok(popups) => Some(popups),
                    Err(_) => {
                        eprintln!("WINDOWS_POPUP refused=1 reason=observer");
                        None
                    }
                }
            }
            Binding::Display(_) => None,
        };
        let mut stream = Self {
            binding,
            epoch: call.epoch,
            abandoned: Arc::clone(&call.abandoned),
            sink,
            item,
            pool,
            session,
            token,
            closed,
            pool_size,
            latest: Latest::new(fps)?,
            crop,
            slots: Vec::new(),
            cursor: crate::cursor::StreamCursor::default(),
            popups,
            delivery_popup: None,
        };
        stream
            .session
            .SetIsCursorCaptureEnabled(false)
            .map_err(|e| api("disable WGC cursor", e))?;
        // Leave the border REQUIRED. Setter readback cannot establish borderless consent.
        if matches!(stream.binding, Binding::Window { .. }) {
            stream
                .session
                .SetIncludeSecondaryWindows(false)
                .map_err(|e| api("disable root secondary capture", e))?;
            if stream
                .session
                .IncludeSecondaryWindows()
                .map_err(|e| api("root secondary capture state", e))?
            {
                return Err(backend("root secondary capture enabled"));
            }
        }
        call.check(shared)?;
        stream.refresh(shared)?;
        call.check(shared)?;
        stream
            .session
            .StartCapture()
            .map_err(|e| api("start WGC", e))?;
        call.check(shared)?;
        Ok(stream)
    }

    fn refresh(&mut self, shared: &Shared) -> Result<(), PlatformError> {
        if !shared.permitted(self.epoch) {
            return Err(PlatformError::Locked);
        }
        self.binding = self.binding.refresh(shared)?;
        if !shared.permitted(self.epoch) {
            return Err(PlatformError::Locked);
        }
        Ok(())
    }

    fn poll(
        &mut self,
        shared: &Shared,
        graphics: &Graphics,
        _start: Instant,
    ) -> Result<Option<Frame>, PlatformError> {
        let until = Instant::now() + BOUND;
        self.delivery_popup = None;
        let access = PopupAccess {
            shared,
            epoch: self.epoch,
            until,
            observer: None,
        };
        if !access.ready()? {
            return Ok(None);
        }
        #[cfg(feature = "gpu")]
        if let Ok(gpu) = shared.gpu.lock()
            && let Some(gpu) = gpu.as_ref()
        {
            let _ = gpu.healthy();
        }
        // Drain a bounded number, replacing/releasing older pool frames immediately.
        for _ in 0..4 {
            if !access.ready()? {
                return Ok(None);
            }
            match frame_result(self.pool.TryGetNextFrame()) {
                Ok(Some(frame)) => {
                    let held = Held(frame);
                    if !access.ready()? {
                        return Ok(None);
                    }
                    let content = held
                        .0
                        .ContentSize()
                        .map_err(|e| api("WGC content size", e))?;
                    size(content)?;
                    if content != self.pool_size {
                        self.latest.clear();
                        drop(held);
                        if !access.ready()? {
                            return Ok(None);
                        }
                        self.refresh(shared)?;
                        if !shared.permitted(self.epoch) {
                            return Err(PlatformError::Locked);
                        }
                        if !access.ready()? {
                            return Ok(None);
                        }
                        self.pool
                            .Recreate(
                                &graphics.capture,
                                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                                2,
                                content,
                            )
                            .map_err(|e| api("recreate WGC pool", e))?;
                        self.pool_size = content;
                        self.slots.clear();
                        if let Some(popups) = &mut self.popups {
                            popups.invalidate();
                            popups.baseline = None;
                        }
                        break;
                    }
                    if !access.ready()? {
                        return Ok(None);
                    }
                    drop(self.latest.push(held));
                }
                Ok(None) => break,
                Err(e) => return Err(api("dequeue WGC frame", e)),
            }
        }
        #[cfg(test)]
        let popup_pixels = std::env::var("CROSSPANE_WINDOWS_WGC_DESCRIPTOR").as_deref() != Ok("1");
        #[cfg(not(test))]
        let popup_pixels = true;
        if !access.ready()? {
            return Ok(None);
        }
        if popup_pixels && let Some(popups) = &mut self.popups {
            match popups.refresh(shared, graphics, self.epoch, size(self.pool_size)?, until) {
                Ok(()) => {}
                Err(PlatformError::Locked) => return Err(PlatformError::Locked),
                Err(_) => {
                    popups.invalidate();
                }
            }
        }
        if !access.ready()? {
            return Ok(None);
        }
        let now = crate::clock::now();
        if !self.latest.due(now) {
            return Ok(None);
        }
        let Some(roi) = crop_rect(size(self.pool_size)?, self.crop) else {
            self.latest.clear();
            return Ok(None);
        };
        let output = PixelSize::new(roi.width() as u32, roi.height() as u32);
        #[cfg(feature = "gpu")]
        if self.slots.iter().any(|image| {
            image
                .surface
                .as_ref()
                .is_some_and(|surface| !surface.healthy())
        }) {
            // Old resources retire, never become CPU readback. A fresh healthy D3D11 slot may replace them.
            // SAFETY: read-only status query on the retained WGC device.
            unsafe { graphics.gpu.device.GetDeviceRemovedReason() }
                .map_err(|e| api("capture device removed", e))?;
            self.slots.clear();
        }
        if self.slots.first().is_none_or(|image| image.size != output) {
            self.slots.clear();
            for _ in 0..2 {
                if !access.ready()? {
                    self.slots.clear();
                    return Ok(None);
                }
                #[cfg(feature = "gpu")]
                let surface = shared
                    .gpu
                    .lock()
                    .map_err(|_| backend("GPU context poisoned"))?
                    .as_ref()
                    .filter(|gpu| gpu.healthy().is_ok())
                    .and_then(|gpu| crate::gpu::Surface::new(gpu.clone(), output).ok());
                let texture = {
                    #[cfg(feature = "gpu")]
                    if let Some(surface) = &surface {
                        surface.shared.texture.clone()
                    } else {
                        if !access.ready()? {
                            self.slots.clear();
                            return Ok(None);
                        }
                        graphics.gpu.texture(output, false)?
                    }
                    #[cfg(not(feature = "gpu"))]
                    {
                        if !access.ready()? {
                            self.slots.clear();
                            return Ok(None);
                        }
                        graphics.gpu.texture(output, false)?
                    }
                };
                self.slots.push(Arc::new(Image {
                    size: output,
                    texture,
                    gpu: Arc::clone(&graphics.gpu),
                    gate: Arc::clone(&shared.gate),
                    epoch: self.epoch,
                    #[cfg(feature = "gpu")]
                    surface,
                }));
            }
        }
        let Some(image) = self
            .slots
            .iter()
            .find(|image| {
                let free = Arc::strong_count(image) == 1;
                #[cfg(feature = "gpu")]
                {
                    free && image
                        .surface
                        .as_ref()
                        .is_none_or(|surface| surface.free(true))
                }
                #[cfg(not(feature = "gpu"))]
                {
                    free
                }
            })
            .cloned()
        else {
            return Ok(None);
        };
        if !access.ready()? {
            return Ok(None);
        }
        let mut held = self.latest.take(now);
        let texture = if let Some(held) = &held {
            let surface: IDirect3DDxgiInterfaceAccess = held
                .0
                .Surface()
                .and_then(|s| s.cast())
                .map_err(|e| api("WGC surface interface", e))?;
            if !access.ready()? {
                return Ok(None);
            }
            // SAFETY: documented interface retrieval from this frame's owned D3D11 surface.
            let texture: ID3D11Texture2D =
                unsafe { surface.GetInterface() }.map_err(|e| api("WGC texture", e))?;
            texture
        } else if let Some(popups) = &self.popups {
            if !popups.dirty {
                return Ok(None);
            }
            let Some(texture) = popups.clean_texture(roi) else {
                return Ok(None);
            };
            texture
        } else {
            return Ok(None);
        };
        if !access.ready()? {
            return Ok(None);
        }
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: live same-device frame texture and initialized descriptor output.
        unsafe { texture.GetDesc(&mut desc) };
        #[cfg(test)]
        if std::env::var("CROSSPANE_WINDOWS_WGC_DESCRIPTOR").as_deref() == Ok("1") {
            let content = held
                .as_ref()
                .ok_or_else(|| backend("diagnostic cached frame"))?
                .0
                .ContentSize()
                .map_err(|e| api("diagnostic content size", e))?;
            eprintln!(
                "OWNED_WGC descriptor width={} height={} format={} samples={} pool={}x{} content={}x{} roi=({},{})..({},{})",
                desc.Width,
                desc.Height,
                desc.Format.0,
                desc.SampleDesc.Count,
                self.pool_size.Width,
                self.pool_size.Height,
                content.Width,
                content.Height,
                roi.min.x,
                roi.min.y,
                roi.max.x,
                roi.max.y
            );
        }
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM || desc.SampleDesc.Count != 1 {
            return Err(backend("WGC texture/content mismatch"));
        }
        let source_roi = if held.is_some() {
            roi
        } else {
            PixelRect::new(
                (0, 0).into(),
                (output.width as i32, output.height as i32).into(),
            )
        };
        if !surface_ready(PixelSize::new(desc.Width, desc.Height), source_roi)? {
            return Ok(None);
        }
        #[cfg(test)]
        if std::env::var("CROSSPANE_WINDOWS_WGC_DESCRIPTOR").as_deref() == Ok("1") {
            return Ok(None); // Owned metadata only: never copy/map pixels.
        }
        if !access.ready()? {
            return Ok(None);
        }
        self.refresh(shared)?;
        if let Some(popups) = &mut self.popups
            && held.is_some()
        {
            if popups.snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.geometry.content == size(self.pool_size).unwrap_or(PixelSize::new(0, 0))
            }) {
                match popups.baseline(shared, graphics, self.epoch, &texture, roi, until) {
                    Ok(true) => {}
                    Ok(false) => {
                        // Keep the exact unclosed B in the existing newest-frame slot.
                        if let Some(held) = held.take() {
                            drop(self.latest.push(held));
                        }
                        return Ok(None);
                    }
                    Err(PlatformError::Locked) => return Err(PlatformError::Locked),
                    Err(_) => popups.invalidate(),
                }
            } else {
                // This fresh B may be delivered root-only. Do not later recompose older A.
                popups.baseline = None;
            }
        }
        if !access.ready()? {
            return Ok(None);
        }
        let composite = if let Some(popups) = &mut self.popups {
            match popups.compose(output, roi, shared, graphics, self.epoch, until) {
                Ok(pixels) => pixels,
                Err(PlatformError::Locked) => return Err(PlatformError::Locked),
                Err(_) => {
                    popups.invalidate();
                    None
                }
            }
        } else {
            None
        };
        // Fresh topology at the upload boundary, outside the D3D lock.
        if let Some(popups) = &mut self.popups
            && composite.is_some()
            && !popups.unchanged(until)
        {
            popups.invalidate();
            return Ok(None);
        }
        if !access.ready()? {
            return Ok(None);
        }
        let context = match graphics.gpu.context.try_lock() {
            Ok(context) => context,
            Err(std::sync::TryLockError::WouldBlock) => {
                // Retry this fresh B; no stale A restore or additional held slot.
                if let Some(held) = held.take() {
                    drop(self.latest.push(held));
                }
                #[cfg(test)]
                popup_stage(21);
                return Ok(None);
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(backend("D3D context poisoned"));
            }
        };
        let area = D3D11_BOX {
            left: source_roi.min.x as u32,
            top: source_roi.min.y as u32,
            front: 0,
            right: source_roi.max.x as u32,
            bottom: source_roi.max.y as u32,
            back: 1,
        };
        if !access.ready()? {
            return Ok(None);
        }
        if let Some(pixels) = &composite {
            let row = output
                .width
                .checked_mul(4)
                .ok_or_else(|| backend("popup upload stride"))?;
            // SAFETY: bounded initialized full BGRA rows, free same-device DEFAULT output slot.
            // UpdateSubresource snapshots source bytes before returning; no delivered slot is written.
            unsafe {
                context.UpdateSubresource(&image.texture, 0, None, pixels.as_ptr().cast(), row, 0)
            };
        } else {
            // SAFETY: verified source bounds, equal BGRA format, distinct same-device output texture.
            unsafe {
                context.CopySubresourceRegion(&image.texture, 0, 0, 0, 0, &texture, 0, Some(&area))
            };
        }
        drop(context);
        // All native acquisition used secondary=false. Revalidate numeric topology before
        // publication; changed ownership/geometry never delivers the provisional composition.
        if let Some(popups) = &mut self.popups
            && composite.is_some()
            && !popups.unchanged(until)
        {
            popups.invalidate();
            return Ok(None);
        }
        if !access.ready()? {
            return Ok(None);
        }
        self.refresh(shared)?;
        if !access.ready()? {
            return Ok(None);
        }
        #[cfg(feature = "gpu")]
        if let Some(surface) = &image.surface {
            let context = match graphics.gpu.context.try_lock() {
                Ok(context) => context,
                Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(backend("D3D context poisoned"));
                }
            };
            if !access.ready()? {
                return Ok(None);
            }
            surface.copied(&context)?;
        }
        #[cfg(test)]
        let cached_root = held.is_none();
        drop(held);
        if !access.ready()? {
            return Ok(None);
        }
        if composite.is_some() {
            self.delivery_popup = self
                .popups
                .as_ref()
                .and_then(|popups| popups.snapshot.clone().map(|snapshot| (snapshot, until)));
        }
        if let Some(popups) = &mut self.popups {
            #[cfg(test)]
            if cached_root && popups.sources.is_empty() {
                popup_stage(62);
            }
            popups.dirty = !popups.sources.is_empty() && composite.is_none();
        }
        Ok(Some(Frame {
            size: output,
            image: FrameImage::Native(image),
            damage: None,
            at: now,
        }))
    }
}

fn emit(sink: &Arc<dyn EventSink<FrameEvent>>, event: FrameEvent) -> bool {
    catch_unwind(AssertUnwindSafe(|| sink.send(event))).is_ok()
}
fn end(
    shared: &Shared,
    streams: &mut BTreeMap<StreamId, Stream>,
    id: StreamId,
    reason: StreamEndReason,
) {
    if let Some(stream) = streams.remove(&id) {
        #[cfg(test)]
        if let Ok(mut probes) = shared.popup_probes.lock() {
            probes.remove(&id);
        }
        let sink = Arc::clone(&stream.sink);
        let epoch = stream.epoch;
        drop(stream); // close native sources before terminal delivery
        let reason = end_reason(
            shared.gate.is_open(),
            shared.gate.epoch() == epoch,
            TargetState::Live,
        )
        .unwrap_or(reason);
        emit(&sink, FrameEvent::Ended { stream: id, reason });
    }
}

fn worker(
    shared: &Shared,
    receive: mpsc::Receiver<Call>,
    ready: mpsc::SyncSender<Result<(), PlatformError>>,
) {
    let _runtime = match Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let graphics = match Graphics::new() {
        Ok(graphics) => graphics,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    match GraphicsCaptureSession::IsSupported() {
        Ok(true) => {}
        Ok(false) => {
            let _ = ready.send(Err(PlatformError::Unsupported("WGC unavailable")));
            return;
        }
        Err(e) => {
            let _ = ready.send(Err(api("WGC support", e)));
            return;
        }
    }
    let start = Instant::now();
    let mut streams = BTreeMap::new();
    let _ = ready.send(Ok(()));
    while shared.alive.load(Ordering::Acquire) && !shared.fault.load(Ordering::Acquire) {
        crate::window::pump_popups();
        if let Ok(call) = receive.recv_timeout(POLL) {
            let result = call.check(shared).and_then(|()| match &call.operation {
                #[cfg(feature = "gpu")]
                Operation::Gpu(wanted) => {
                    if !streams.is_empty() {
                        return Err(backend("GPU initialization after stream start"));
                    }
                    let gpu = crate::gpu::WindowsGpu::new(
                        graphics.gpu.device.clone(),
                        graphics.gpu.context.clone(),
                        *wanted,
                    )?;
                    call.check(shared)?;
                    *shared
                        .gpu
                        .lock()
                        .map_err(|_| backend("GPU context poisoned"))? = Some(gpu);
                    Ok(())
                }
                Operation::Start {
                    id,
                    target,
                    crop,
                    fps,
                    sink,
                } => {
                    let stream = Stream::new(
                        shared,
                        &graphics,
                        &call,
                        *target,
                        *crop,
                        *fps,
                        Arc::clone(sink),
                    )?;
                    streams.insert(*id, stream);
                    Ok(())
                }
                Operation::Crop(id, crop) => {
                    let stream = streams.get_mut(id).ok_or(PlatformError::NotFound)?;
                    stream.refresh(shared)?;
                    call.check(shared)?;
                    stream.latest.clear();
                    stream.crop = *crop;
                    stream.slots.clear();
                    if let Some(popups) = &mut stream.popups {
                        popups.baseline = None;
                        popups.dirty = true;
                    }
                    Ok(())
                }
                #[cfg(test)]
                Operation::QualifyPopup {
                    stream,
                    hwnd,
                    region,
                    expected,
                } => {
                    let stream = streams.get_mut(stream).ok_or(PlatformError::NotFound)?;
                    stream.refresh(shared)?;
                    call.check(shared)?;
                    stream
                        .popups
                        .as_mut()
                        .ok_or(PlatformError::NotFound)?
                        .qualify(*hwnd, *region, *expected, call.until)?;
                    call.check(shared)
                }
                Operation::Stop(id) => {
                    if !streams.contains_key(id) {
                        return Err(PlatformError::NotFound);
                    }
                    end(shared, &mut streams, *id, StreamEndReason::Requested);
                    Ok(())
                }
            });
            let _ = call.reply.send(result);
        }
        let ids: Vec<_> = streams.keys().copied().collect();
        for id in ids {
            let Some(stream) = streams.get_mut(&id) else {
                continue;
            };
            if stream.closed.load(Ordering::Acquire) {
                end(shared, &mut streams, id, StreamEndReason::TargetGone);
                continue;
            }
            if let Err(error) = stream.refresh(shared) {
                let reason = failure_reason(
                    shared.permitted(stream.epoch),
                    shared.gate.epoch() == stream.epoch,
                    &error,
                );
                end(shared, &mut streams, id, reason);
                continue;
            }
            let target = stream.binding.state();
            if let Some(reason) = end_reason(
                shared.gate.is_open(),
                shared.gate.epoch() == stream.epoch,
                target,
            ) {
                end(shared, &mut streams, id, reason);
                continue;
            }
            if stream.abandoned.load(Ordering::Acquire) {
                end(shared, &mut streams, id, StreamEndReason::Requested);
                continue;
            }
            let binding = stream.binding.clone();
            let shape = size(stream.pool_size)
                .ok()
                .and_then(|content| match &binding {
                    Binding::Window { native, .. } => {
                        stream.cursor.sample(*native, content, stream.crop, || {
                            shared.permitted(stream.epoch)
                        })
                    }
                    Binding::Display(monitor) => {
                        stream
                            .cursor
                            .sample_monitor(monitor.bounds, content, stream.crop, || {
                                shared.permitted(stream.epoch) && binding.unchanged(shared)
                            })
                    }
                });
            if let Some(shape) = shape
                && shared.permitted(stream.epoch)
                && binding.unchanged(shared)
                && shared.permitted(stream.epoch)
            {
                let event = match shape {
                    crate::model::cursor::Shape::Image(image) => FrameEvent::Cursor {
                        stream: id,
                        cursor: Some(image),
                    },
                    crate::model::cursor::Shape::Hidden => FrameEvent::Cursor {
                        stream: id,
                        cursor: None,
                    },
                    crate::model::cursor::Shape::Default => {
                        FrameEvent::CursorDefault { stream: id }
                    }
                };
                // Cursor observation/consumer failure never ends the pixel stream.
                emit(&stream.sink, event);
            }
            let polled = stream.poll(shared, &graphics, start);
            #[cfg(test)]
            if let Ok(mut probes) = shared.popup_probes.lock() {
                if let Some(probe) = stream.popups.as_ref().and_then(|popups| {
                    popups.probe(
                        size(stream.pool_size).unwrap_or(PixelSize::new(0, 0)),
                        stream.crop,
                    )
                }) {
                    probes.insert(id, probe);
                } else {
                    probes.remove(&id);
                }
            }
            match polled {
                Ok(Some(mut frame)) => {
                    if !shared.permitted(stream.epoch) {
                        end(shared, &mut streams, id, StreamEndReason::Blocked);
                        continue;
                    }
                    if stream
                        .delivery_popup
                        .as_ref()
                        .is_some_and(|(_, until)| Instant::now() >= *until)
                    {
                        stream.delivery_popup = None;
                        if let Some(popups) = &mut stream.popups {
                            popups.invalidate();
                        }
                        #[cfg(test)]
                        if let Ok(mut probes) = shared.popup_probes.lock() {
                            probes.remove(&id);
                        }
                        continue;
                    }
                    if let Err(error) = stream.refresh(shared) {
                        let reason = failure_reason(
                            shared.permitted(stream.epoch),
                            shared.gate.epoch() == stream.epoch,
                            &error,
                        );
                        end(shared, &mut streams, id, reason);
                        continue;
                    }
                    frame.at = crate::clock::now();
                    if !shared.permitted(stream.epoch) {
                        end(shared, &mut streams, id, StreamEndReason::Blocked);
                        continue;
                    }
                    if let Some((snapshot, until)) = stream.delivery_popup.take() {
                        let valid = stream
                            .popups
                            .as_mut()
                            .is_some_and(|popups| popups.observer.unchanged(&snapshot, until));
                        if !valid {
                            if let Some(popups) = &mut stream.popups {
                                popups.invalidate();
                            }
                            #[cfg(test)]
                            if let Ok(mut probes) = shared.popup_probes.lock() {
                                probes.remove(&id);
                            }
                            continue; // Drop provisional pixels; recomposition remains dirty.
                        }
                        if Instant::now() >= until {
                            if let Some(popups) = &mut stream.popups {
                                popups.dirty = true; // Keep the dropped composition pending for retry.
                            }
                            continue;
                        }
                    }
                    if !shared.permitted(stream.epoch) {
                        end(shared, &mut streams, id, StreamEndReason::Blocked);
                        continue;
                    }
                    stream.latest.delivered(frame.at);
                    if !emit(&stream.sink, FrameEvent::Frame { stream: id, frame }) {
                        end(shared, &mut streams, id, StreamEndReason::Failed);
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    #[cfg(test)]
                    eprintln!("OWNED_WGC stream failure: {error}");
                    let reason = failure_reason(
                        shared.permitted(stream.epoch),
                        shared.gate.epoch() == stream.epoch,
                        &error,
                    );
                    end(shared, &mut streams, id, reason);
                }
            }
        }
    }
    let reason = if !shared.gate.is_open() {
        StreamEndReason::Blocked
    } else if shared.fault.load(Ordering::Acquire) {
        StreamEndReason::Failed
    } else {
        StreamEndReason::Requested
    };
    #[cfg(test)]
    popup_stage(70);
    for id in streams.keys().copied().collect::<Vec<_>>() {
        end(shared, &mut streams, id, reason);
    }
    #[cfg(test)]
    popup_stage(71);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_null_frame_is_empty_not_stream_failure() {
        // SAFETY: the pinned Type conversion rejects null without querying/dereferencing it.
        let empty = unsafe {
            <Direct3D11CaptureFrame as windows::core::Type<Direct3D11CaptureFrame>>::from_abi(
                std::ptr::null_mut(),
            )
        };
        assert!(frame_result(empty).is_ok_and(|frame| frame.is_none()));
        assert!(frame_result(Err(windows::core::Error::from_hresult(E_ACCESSDENIED))).is_err());
    }
}
