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
    window::{NativeWindow, WindowResolver},
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
        if !shared.permitted(self.epoch) {
            return Err(PlatformError::Locked);
        }
        if self.abandoned.load(Ordering::Acquire) || Instant::now() >= self.until {
            return Err(PlatformError::Timeout);
        }
        Ok(())
    }
}
enum Operation {
    Start {
        id: StreamId,
        target: CaptureTarget,
        crop: Option<PixelRect>,
        fps: u32,
        sink: Arc<dyn EventSink<FrameEvent>>,
    },
    Crop(StreamId, Option<PixelRect>),
    Stop(StreamId),
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
        if !self.shared.permitted(epoch) {
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
        self.done.recv_timeout(BOUND).is_ok()
            && thread.join().is_ok()
            && !self.shared.fault.load(Ordering::Acquire)
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
        // SAFETY: only this thread's successful RoInitialize is balanced, after WinRT objects drop.
        unsafe { RoUninitialize() };
    }
}
struct Gpu {
    device: ID3D11Device,
    context: Mutex<ID3D11DeviceContext>,
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
        // SAFETY: initialized output Options; no SINGLETHREADED flag, context access serialized.
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
                context: Mutex::new(context),
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
}
impl Drop for Stream {
    fn drop(&mut self) {
        self.latest.clear();
        let _ = self.session.Close();
        let _ = self.pool.Close();
        let _ = self.item.RemoveClosed(self.token);
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
        };
        stream
            .session
            .SetIsCursorCaptureEnabled(false)
            .map_err(|e| api("disable WGC cursor", e))?;
        // Leave the border REQUIRED. Setter readback cannot establish borderless consent.
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
        // Drain a bounded number, replacing/releasing older pool frames immediately.
        for _ in 0..4 {
            match frame_result(self.pool.TryGetNextFrame()) {
                Ok(Some(frame)) => {
                    let held = Held(frame);
                    let content = held
                        .0
                        .ContentSize()
                        .map_err(|e| api("WGC content size", e))?;
                    size(content)?;
                    if content != self.pool_size {
                        self.latest.clear();
                        drop(held);
                        self.refresh(shared)?;
                        if !shared.permitted(self.epoch) {
                            return Err(PlatformError::Locked);
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
                        break;
                    }
                    drop(self.latest.push(held));
                }
                Ok(None) => break,
                Err(e) => return Err(api("dequeue WGC frame", e)),
            }
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
        if self.slots.first().is_none_or(|image| image.size != output) {
            self.slots.clear();
            for _ in 0..2 {
                self.slots.push(Arc::new(Image {
                    size: output,
                    texture: graphics.gpu.texture(output, false)?,
                    gpu: Arc::clone(&graphics.gpu),
                    gate: Arc::clone(&shared.gate),
                    epoch: self.epoch,
                }));
            }
        }
        let Some(image) = self
            .slots
            .iter()
            .find(|image| Arc::strong_count(image) == 1)
            .cloned()
        else {
            return Ok(None);
        };
        let Some(held) = self.latest.take(now) else {
            return Ok(None);
        };
        let access: IDirect3DDxgiInterfaceAccess = held
            .0
            .Surface()
            .and_then(|s| s.cast())
            .map_err(|e| api("WGC surface interface", e))?;
        // SAFETY: documented interface retrieval from this frame's owned D3D11 surface.
        let texture: ID3D11Texture2D =
            unsafe { access.GetInterface() }.map_err(|e| api("WGC texture", e))?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: live same-device frame texture and initialized descriptor output.
        unsafe { texture.GetDesc(&mut desc) };
        #[cfg(test)]
        if std::env::var("CROSSPANE_WINDOWS_WGC_DESCRIPTOR").as_deref() == Ok("1") {
            let content = held
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
        if !surface_ready(PixelSize::new(desc.Width, desc.Height), roi)? {
            return Ok(None);
        }
        #[cfg(test)]
        if std::env::var("CROSSPANE_WINDOWS_WGC_DESCRIPTOR").as_deref() == Ok("1") {
            return Ok(None); // Owned metadata only: never copy/map pixels.
        }
        self.refresh(shared)?;
        let context = match graphics.gpu.context.try_lock() {
            Ok(context) => context,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(backend("D3D context poisoned"));
            }
        };
        let area = D3D11_BOX {
            left: roi.min.x as u32,
            top: roi.min.y as u32,
            front: 0,
            right: roi.max.x as u32,
            bottom: roi.max.y as u32,
            back: 1,
        };
        if !shared.permitted(self.epoch) {
            return Err(PlatformError::Locked);
        }
        // SAFETY: verified source bounds, equal BGRA format, distinct same-device output texture.
        unsafe {
            context.CopySubresourceRegion(&image.texture, 0, 0, 0, 0, &texture, 0, Some(&area))
        };
        drop(context);
        drop(held);
        if !shared.permitted(self.epoch) {
            return Err(PlatformError::Locked);
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
        if let Ok(call) = receive.recv_timeout(POLL) {
            let result = call.check(shared).and_then(|()| match &call.operation {
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
                    Ok(())
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
            match stream.poll(shared, &graphics, start) {
                Ok(Some(mut frame)) => {
                    if !shared.permitted(stream.epoch) {
                        end(shared, &mut streams, id, StreamEndReason::Blocked);
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
    for id in streams.keys().copied().collect::<Vec<_>>() {
        end(shared, &mut streams, id, reason);
    }
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
