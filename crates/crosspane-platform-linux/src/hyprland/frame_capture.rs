//! CPU output and window capture through ext-image-copy-capture-v1. Native objects belong to one thread.
//!
//! Hyprland 0.56.2 cannot capture compositor-drawn themed or cursor-shape-v1 cursors. In
//! `src/managers/screenshare/CursorshareSession.cpp`, `render()` clears the cursor frame:
//! ```text
//! } else if (!cursorImage.pBuffer || !cursorImage.surface || !cursorImage.bufferTex) {
//!     // render clear when cursor is probably hidden
//! ```
//! Only client-surface cursors provide pixels. Themed cursors have no client surface and produce
//! transparent frames, indistinguishable from a truly hidden cursor. While entered, those frames
//! report `FrameEvent::CursorDefault`; this backend never reports a hidden cursor.
//!
//! **Cursor capture is off unless [`HyprlandFrameCapture::set_cursor_capture`] turns it on.**
//! Hyprland 0.56.2 crashed (SEGV in `CCursorshareSession::copy` → `sendPresentationTime`, a frame
//! used after it was freed) when a layer surface was destroyed while a cursor session was active
//! (2026-10-01, the live session). Without a cursor session that compositor path is never reached;
//! the destination then keeps its default cursor (02 §3.3).

use std::collections::HashMap;
use std::fs::File;
use std::io::ErrorKind;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crosspane_platform::{
    CaptureTarget, CursorImage, EventSink, Frame, FrameCapture, FrameEvent, FrameImage, IoGate,
    PlatformError, StreamEndReason, StreamId,
};
use crosspane_types::geom::{PixelRect, PixelSize, euclid::point2};
use crosspane_types::id::{DisplayId, WindowId};
use crosspane_types::time::MonoTime;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self as handle_protocol, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self as list_protocol, ExtForeignToplevelListV1},
};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1,
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_cursor_session_v1::{
        self as cursor_protocol, ExtImageCopyCaptureCursorSessionV1,
    },
    ext_image_copy_capture_frame_v1::{self as frame_protocol, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{ExtImageCopyCaptureManagerV1, Options},
    ext_image_copy_capture_session_v1::{self as session_protocol, ExtImageCopyCaptureSessionV1},
};

#[cfg(feature = "gpu")]
use crate::dmabuf::{Export, Gpu, Image};
#[cfg(feature = "gpu")]
use std::sync::Mutex;
#[cfg(feature = "gpu")]
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_feedback_v1::{self, ZwpLinuxDmabufFeedbackV1},
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};

use super::ipc::HyprIpc;

// Leave scheduling margin inside the frozen two-second call bound. Poll even when a capture is
// indefinitely waiting for source damage: IoGate has no notification API.
const CALL_TIMEOUT: Duration = Duration::from_millis(1800);
const GATE_POLL: Duration = Duration::from_millis(10);
const CURSOR_INTERVAL: Duration = Duration::from_nanos(1_000_000_000_u64.div_ceil(30));
/// Unknown/stale Twin geometry never supplies video; stop if it cannot converge in this bound.
// Leave 200 ms for runtime scheduling and event delivery within the two-second hold budget.
pub const TWIN_INCOHERENT: Duration = Duration::from_millis(1800);

/// A bounded IPC snapshot of the *unclipped* window in its own Twin output. IPC reports layout
/// goals; frame constraints must agree before those goals can be used for video.
#[derive(Clone, Debug, PartialEq)]
pub struct TwinGeometry {
    pub window: WindowId,
    pub display: DisplayId,
    pub origin: (i32, i32),
    pub size: PixelSize,
    pub extent: PixelSize,
    pub content: PixelRect,
}

impl TwinGeometry {
    pub fn from_env(window: WindowId, display: DisplayId) -> Result<Self, PlatformError> {
        let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").map_err(backend)?;
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or(PlatformError::NotFound)?;
        let ipc = HyprIpc::new(
            &signature,
            std::path::Path::new(&runtime),
            Duration::from_millis(50),
        );
        let geometry = Self::read(&ipc, window)?;
        if geometry.display != display {
            return Err(PlatformError::NotFound);
        }
        Ok(geometry)
    }

    fn read(ipc: &HyprIpc, window: WindowId) -> Result<Self, PlatformError> {
        let client =
            super::parking::client_snapshot(ipc, window)?.ok_or(PlatformError::NotFound)?;
        let name = format!("{}{:x}", super::parking::OUTPUT_PREFIX, window.0);
        let monitor =
            super::parking::monitor_snapshot(ipc, &name)?.ok_or(PlatformError::NotFound)?;
        if client["mapped"].as_bool() != Some(true)
            || client["monitor"] != monitor["id"]
            || client
                .pointer("/workspace/name")
                .and_then(serde_json::Value::as_str)
                != Some(format!("{}{:x}", super::parking::WORKSPACE_PREFIX, window.0).as_str())
        {
            return Err(backend("unverified Twin binding"));
        }
        // Reuse parking's conversion, but reject unknown/overflowing values instead of its
        // layout defaults. Bound inputs so subtraction, scaling and rectangle addition are safe.
        let fields = [
            &client["at"][0],
            &client["at"][1],
            &client["size"][0],
            &client["size"][1],
            &monitor["x"],
            &monitor["y"],
            &monitor["scale"],
        ];
        if fields.iter().any(|v| {
            !v.as_f64()
                .is_some_and(|n| n.is_finite() && n.abs() < f64::from(i32::MAX) / 32.0)
        }) || !monitor["scale"]
            .as_f64()
            .is_some_and(|s| (0.25..=8.0).contains(&s))
        {
            return Err(backend("unknown Twin geometry"));
        }
        let parked = super::parking::parked_from(window, &client, &monitor)?;
        let extent = super::parking::output_extent(&monitor)?;
        let actual = super::parking::window_rect(
            &client,
            &monitor,
            monitor["scale"].as_f64().ok_or(PlatformError::NotFound)?,
        );
        Ok(Self {
            window,
            display: parked.display,
            origin: (actual.min.x, actual.min.y),
            size: actual
                .size()
                .try_cast()
                .ok_or_else(|| backend("invalid Twin size"))?,
            extent: extent.size().cast(),
            content: parked
                .content
                .intersection(&extent)
                .ok_or(PlatformError::NotFound)?,
        })
    }

    /// Only the exact clipped Parked.content may be routed. R stays in the engine; Q is local
    /// to the Window buffer, including negative-origin clipping and output padding/bars.
    pub fn map_crop(&self, r: PixelRect) -> Result<PixelRect, PlatformError> {
        if self.content != r {
            return Err(backend("incoherent Twin content crop"));
        }
        Ok(r.translate(crosspane_types::geom::euclid::vec2(
            -self.origin.0,
            -self.origin.1,
        )))
    }
}

/// Bounded command handle for all output and window capture streams on one Wayland connection.
#[derive(Debug)]
pub struct HyprlandFrameCapture {
    #[cfg(feature = "gpu")]
    gpu: Arc<Mutex<Option<Gpu>>>,
    #[cfg(feature = "gpu")]
    main_device: Arc<Mutex<Option<u64>>>,
    commands: mpsc::Sender<Command>,
    lookups: mpsc::Sender<Lookup>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    next_id: u64,
    gate: Arc<IoGate>,
    cursors: Arc<AtomicBool>,
    twins: HashMap<StreamId, WindowId>,
}

struct Lookup {
    target: CaptureTarget,
    strict_twin: bool,
    deadline: Instant,
    reply: mpsc::SyncSender<Result<ResolvedTarget, PlatformError>>,
}

enum ResolvedTarget {
    Output(String),
    Window {
        id: WindowId,
        app_id: String,
        title: String,
        twin: Option<TwinGeometry>,
    },
}

fn resolve_window(
    ipc: &HyprIpc,
    id: WindowId,
    strict_twin: bool,
) -> Result<ResolvedTarget, PlatformError> {
    let clients = ipc.json("clients")?;
    let client = clients
        .as_array()
        .and_then(|clients| {
            clients.iter().find(|client| {
                client["stableId"].as_str().and_then(parse_identifier) == Some(id.0)
                    && client["mapped"].as_bool() == Some(true)
            })
        })
        .ok_or(PlatformError::NotFound)?;
    let title = client["title"]
        .as_str()
        .ok_or_else(|| backend("missing client title"))?;
    let class = client["class"]
        .as_str()
        .ok_or_else(|| backend("missing client class"))?;
    Ok(ResolvedTarget::Window {
        id,
        title: title.into(),
        app_id: if class.is_empty() {
            client["initialClass"].as_str().unwrap_or(class)
        } else {
            class
        }
        .into(),
        // Ordinary Window(None) needs no Twin output or workspace, even during restoration.
        twin: if strict_twin {
            Some(TwinGeometry::read(ipc, id)?)
        } else {
            None
        },
    })
}

enum Request {
    Start {
        id: StreamId,
        target: ResolvedTarget,
        crop: Option<PixelRect>,
        max_fps: u32,
        sink: Arc<dyn EventSink<FrameEvent>>,
    },
    Crop(StreamId, Option<PixelRect>, Option<TwinGeometry>),
    Stop(StreamId),
}

struct Command {
    request: Request,
    deadline: Instant,
    reply: mpsc::Sender<Result<(), PlatformError>>,
}

impl HyprlandFrameCapture {
    /// Connect to `$WAYLAND_DISPLAY`; requires the output source and image copy extensions.
    pub fn new(gate: Arc<IoGate>, ipc: HyprIpc) -> Result<Self, PlatformError> {
        #[cfg(feature = "gpu")]
        let gpu = Arc::new(Mutex::new(None));
        #[cfg(feature = "gpu")]
        let worker_gpu = gpu.clone();
        #[cfg(feature = "gpu")]
        let main_device = Arc::new(Mutex::new(None));
        #[cfg(feature = "gpu")]
        let worker_main_device = main_device.clone();
        let (commands, receiver) = mpsc::channel();
        let (lookups, work) = mpsc::channel::<Lookup>();
        let worker_lookups = lookups.clone();
        let (ready, result) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = shutdown.clone();
        let worker_gate = gate.clone();
        let cursors = Arc::new(AtomicBool::new(false));
        let worker_cursors = cursors.clone();
        let thread = std::thread::Builder::new()
            .name("hypr-frames".into())
            .spawn(move || {
                let mut worker = match Worker::new(
                    worker_gate,
                    worker_cursors,
                    worker_lookups,
                    &worker_shutdown,
                    #[cfg(feature = "gpu")]
                    worker_gpu,
                    #[cfg(feature = "gpu")]
                    worker_main_device,
                ) {
                    Ok(worker) => worker,
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                if ready.send(Ok(())).is_err() {
                    return;
                }
                let result =
                    catch_unwind(AssertUnwindSafe(|| worker.run(&receiver, &worker_shutdown)));
                let reason = match result {
                    Ok(Ok(())) => StreamEndReason::Requested,
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "frame capture connection failed");
                        StreamEndReason::Failed
                    }
                    Err(_) => {
                        tracing::warn!("frame capture worker panicked");
                        StreamEndReason::Failed
                    }
                };
                worker.state.end_all(reason);
                let _ = worker.connection.flush();
            })
            .map_err(backend)?;
        match result.recv_timeout(CALL_TIMEOUT) {
            Ok(Ok(())) => (),
            Ok(Err(error)) => {
                let _ = thread.join();
                return Err(error);
            }
            Err(_) => {
                shutdown.store(true, Ordering::Release);
                return Err(PlatformError::Timeout);
            }
        }

        // IPC may have an arbitrarily long configured timeout. Keep it off the capture thread
        // and bound the caller's wait independently, so it cannot delay gate enforcement.
        if let Err(error) = std::thread::Builder::new()
            .name("hypr-frame-ids".into())
            .spawn(move || {
                while let Ok(lookup) = work.recv() {
                    let result = if Instant::now() >= lookup.deadline {
                        Err(PlatformError::Timeout)
                    } else {
                        match lookup.target {
                            CaptureTarget::Display(display) => {
                                ipc.monitor_ids().and_then(|monitors| {
                                    monitors
                                        .into_iter()
                                        .find(|(_, id)| *id == display.0)
                                        .map(|(name, _)| ResolvedTarget::Output(name))
                                        .ok_or(PlatformError::NotFound)
                                })
                            }
                            CaptureTarget::Window(id) => {
                                resolve_window(&ipc, id, lookup.strict_twin)
                            }
                            _ => Err(PlatformError::Unsupported("capture target")),
                        }
                    };
                    let _ = lookup.reply.send(result);
                }
            })
        {
            shutdown.store(true, Ordering::Release);
            let _ = thread.join();
            return Err(backend(error));
        }
        Ok(Self {
            #[cfg(feature = "gpu")]
            gpu,
            #[cfg(feature = "gpu")]
            main_device,
            commands,
            lookups,
            shutdown,
            thread: Some(thread),
            next_id: 1,
            gate,
            cursors,
            twins: HashMap::new(),
        })
    }

    /// Open a Vulkan device on the compositor's linux-dmabuf main device, requesting supported
    /// `wanted` features. Streams started from now on may use DMA-BUF; errors leave shm enabled.
    #[cfg(feature = "gpu")]
    pub fn enable_gpu(
        &self,
        wanted: wgpu::Features,
    ) -> Result<(wgpu::Device, wgpu::Queue), PlatformError> {
        let node = self
            .main_device
            .lock()
            .map_err(backend)?
            .ok_or(PlatformError::Unsupported(
                "linux-dmabuf v4 main device required",
            ))?;
        let gpu = Gpu::open(node, wanted)?;
        let result = (gpu.device.clone(), gpu.queue.clone());
        *self.gpu.lock().map_err(backend)? = Some(gpu);
        Ok(result)
    }

    /// Report cursor shapes (`FrameEvent::Cursor` / `CursorDefault`) for streams started from now
    /// on. Off by default: see the module documentation for the Hyprland crash it avoids.
    pub fn set_cursor_capture(&self, enabled: bool) {
        self.cursors.store(enabled, Ordering::Release);
    }

    fn call(&self, request: Request, deadline: Instant) -> Result<(), PlatformError> {
        let (reply, result) = mpsc::channel();
        self.commands
            .send(Command {
                request,
                deadline,
                reply,
            })
            .map_err(|_| backend("capture worker unavailable"))?;
        receive(&result, deadline)?
    }
}

impl FrameCapture for HyprlandFrameCapture {
    fn start(
        &mut self,
        target: CaptureTarget,
        crop: Option<PixelRect>,
        max_fps: u32,
        sink: Arc<dyn EventSink<FrameEvent>>,
    ) -> Result<StreamId, PlatformError> {
        if !self.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        if max_fps == 0 {
            return Err(backend("max_fps must be positive"));
        }
        let deadline = Instant::now() + CALL_TIMEOUT;
        let (reply, result) = mpsc::sync_channel(1);
        self.lookups
            .send(Lookup {
                target,
                strict_twin: matches!(target, CaptureTarget::Window(_)) && crop.is_some(),
                deadline,
                reply,
            })
            .map_err(|_| backend("monitor lookup unavailable"))?;
        let target = receive(&result, deadline)??;
        let twin = match &target {
            ResolvedTarget::Window {
                twin: Some(twin), ..
            } if crop.is_some() => Some(twin.window),
            _ => None,
        };
        let id = StreamId(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| backend("stream IDs exhausted"))?;
        self.call(
            Request::Start {
                id,
                target,
                crop,
                max_fps,
                sink,
            },
            deadline,
        )?;
        if let Some(window) = twin {
            self.twins.insert(id, window);
        }
        Ok(id)
    }

    fn set_crop(&mut self, stream: StreamId, crop: Option<PixelRect>) -> Result<(), PlatformError> {
        let deadline = Instant::now() + CALL_TIMEOUT;
        let twin = if let Some(&window) = self.twins.get(&stream) {
            let (reply, result) = mpsc::sync_channel(1);
            self.lookups
                .send(Lookup {
                    target: CaptureTarget::Window(window),
                    strict_twin: true,
                    deadline,
                    reply,
                })
                .map_err(backend)?;
            match receive(&result, deadline)?? {
                ResolvedTarget::Window {
                    twin: Some(twin), ..
                } => Some(twin),
                _ => return Err(PlatformError::NotFound),
            }
        } else {
            None
        };
        self.call(Request::Crop(stream, crop, twin), deadline)
    }

    fn stop(&mut self, stream: StreamId) -> Result<(), PlatformError> {
        self.twins.remove(&stream);
        self.call(Request::Stop(stream), Instant::now() + CALL_TIMEOUT)
    }
}

impl Drop for HyprlandFrameCapture {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn receive<T>(receiver: &mpsc::Receiver<T>, deadline: Instant) -> Result<T, PlatformError> {
    receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => PlatformError::Timeout,
            mpsc::RecvTimeoutError::Disconnected => backend("capture worker unavailable"),
        })
}

fn backend(error: impl std::fmt::Display) -> PlatformError {
    PlatformError::Backend(format!("frame capture: {error}"))
}

#[derive(Default)]
struct Constraints {
    size: Option<PixelSize>,
    formats: Vec<wl_shm::Format>,
    #[cfg(feature = "gpu")]
    device: Option<u64>,
    #[cfg(feature = "gpu")]
    dmabuf_formats: Vec<(u32, Vec<u64>)>,
}

struct Buffer {
    file: File,
    proxy: wl_buffer::WlBuffer,
    size: PixelSize,
    stride: u32,
    format: wl_shm::Format,
}

impl Buffer {
    fn new(
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<State>,
        constraints: &Constraints,
    ) -> Result<Self, PlatformError> {
        Self::with_format(shm, qh, constraints, choose_format(&constraints.formats)?)
    }

    fn with_format(
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<State>,
        constraints: &Constraints,
        format: wl_shm::Format,
    ) -> Result<Self, PlatformError> {
        let size = constraints
            .size
            .ok_or_else(|| backend("missing buffer size"))?;
        let stride = size
            .width
            .checked_mul(4)
            .ok_or_else(|| backend("buffer stride overflow"))?;
        let length = stride
            .checked_mul(size.height)
            .and_then(|length| i32::try_from(length).ok())
            .filter(|length| *length > 0)
            .ok_or_else(|| backend("invalid SHM buffer size"))?;
        let file = File::from(
            rustix::fs::memfd_create("crosspane-frames", rustix::fs::MemfdFlags::CLOEXEC)
                .map_err(backend)?,
        );
        file.set_len(length as u64).map_err(backend)?;
        let pool = shm.create_pool(file.as_fd(), length, qh, ());
        let proxy = pool.create_buffer(
            0,
            size.width as i32,
            size.height as i32,
            stride as i32,
            format,
            qh,
            (),
        );
        pool.destroy();
        Ok(Self {
            file,
            proxy,
            size,
            stride,
            format,
        })
    }

    fn copy(&self, crop: Option<PixelRect>) -> Result<(PixelSize, Arc<[u8]>), PlatformError> {
        let rect = capture_rect(self.size, crop)?;
        let width = (rect.max.x - rect.min.x) as u32;
        let height = (rect.max.y - rect.min.y) as u32;
        let row_len = width as usize * 4;
        let mut pixels = Vec::new();
        pixels
            .try_reserve_exact(row_len * height as usize)
            .map_err(backend)?;
        pixels.resize(row_len * height as usize, 0);
        // pread avoids an unsafe shared-memory mapping. ready gives exclusive buffer access until
        // the next capture request; no compositor writes can race these row copies.
        for (y, row) in pixels.chunks_exact_mut(row_len).enumerate() {
            let offset =
                (rect.min.y as u64 + y as u64) * u64::from(self.stride) + rect.min.x as u64 * 4;
            self.file.read_exact_at(row, offset).map_err(backend)?;
        }
        Ok((PixelSize::new(width, height), pixels.into()))
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        self.proxy.destroy();
    }
}

#[cfg(feature = "gpu")]
struct Slot {
    export: Export,
    proxy: wl_buffer::WlBuffer,
    free: Arc<AtomicBool>,
    pending: AtomicBool,
}
#[cfg(feature = "gpu")]
impl Drop for Slot {
    fn drop(&mut self) {
        // On connection teardown a pending frame is cancelled by disconnect, not wl_buffer
        // destruction. The compositor keeps its imported dma-buf reference through cancellation.
        if !self.pending.load(Ordering::Acquire) {
            self.proxy.destroy();
        }
    }
}
#[cfg(feature = "gpu")]
struct Ring {
    slots: Vec<Slot>,
    size: PixelSize,
    gpu: Gpu,
}
#[cfg(feature = "gpu")]
impl Ring {
    fn new(
        gpu: &Gpu,
        dmabuf: &ZwpLinuxDmabufV1,
        qh: &QueueHandle<State>,
        constraints: &Constraints,
    ) -> Result<Option<Self>, PlatformError> {
        if constraints.device != Some(gpu.node) {
            return Ok(None);
        }
        let size = constraints
            .size
            .ok_or_else(|| backend("missing DMA-BUF size"))?;
        for (format, offered) in &constraints.dmabuf_formats {
            if ![0x34325258, 0x34325241].contains(format) {
                continue;
            }
            let modifiers = gpu.modifiers(offered)?;
            if modifiers.is_empty() {
                continue;
            }
            let mut slots = Vec::new();
            for _ in 0..4 {
                let export = Export::new(gpu, size, &modifiers)?;
                let params = dmabuf.create_params(qh, ());
                params.add(
                    export.fd.as_fd(),
                    0,
                    export.offset,
                    export.stride,
                    (export.modifier >> 32) as u32,
                    export.modifier as u32,
                );
                let proxy = params.create_immed(
                    size.width as i32,
                    size.height as i32,
                    *format,
                    zwp_linux_buffer_params_v1::Flags::empty(),
                    qh,
                    (),
                );
                params.destroy();
                tracing::debug!(
                    modifier = format_args!("{:#018x}", export.modifier),
                    "allocated DMA-BUF capture slot"
                );
                slots.push(Slot {
                    export,
                    proxy,
                    free: Arc::new(AtomicBool::new(true)),
                    pending: AtomicBool::new(false),
                });
            }
            return Ok(Some(Self {
                slots,
                size,
                gpu: gpu.clone(),
            }));
        }
        Ok(None)
    }
    fn acquire(&self) -> Option<usize> {
        // Drive release callbacks without waiting for GPU work.
        let _ = self.gpu.device.poll(wgpu::PollType::Poll);
        self.slots.iter().position(|slot| {
            if slot
                .free
                .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                slot.pending.store(true, Ordering::Release);
                true
            } else {
                false
            }
        })
    }
    fn release(&self, index: usize) {
        self.slots[index].pending.store(false, Ordering::Release);
        self.slots[index].free.store(true, Ordering::Release);
    }
    fn image(
        &mut self,
        index: usize,
        rect: PixelRect,
    ) -> Result<Arc<dyn crosspane_platform::NativeImage>, PlatformError> {
        self.slots[index].pending.store(false, Ordering::Release);
        Ok(Arc::new(Image {
            texture: self.slots[index].export.texture(&self.gpu, self.size)?,
            gpu: self.gpu.clone(),
            size: PixelSize::new(
                (rect.max.x - rect.min.x) as u32,
                (rect.max.y - rect.min.y) as u32,
            ),
            origin: (rect.min.x as u32, rect.min.y as u32),
            free: self.slots[index].free.clone(),
            modifier: self.slots[index].export.modifier,
        }))
    }
}

fn choose_format(formats: &[wl_shm::Format]) -> Result<wl_shm::Format, PlatformError> {
    [wl_shm::Format::Xrgb8888, wl_shm::Format::Argb8888]
        .into_iter()
        .find(|format| formats.contains(format))
        .ok_or(PlatformError::Unsupported("BGRA-compatible SHM required"))
}

fn capture_rect(size: PixelSize, crop: Option<PixelRect>) -> Result<PixelRect, PlatformError> {
    let width = i32::try_from(size.width).map_err(backend)?;
    let height = i32::try_from(size.height).map_err(backend)?;
    let rect = crop.unwrap_or_else(|| PixelRect::new(point2(0, 0), point2(width, height)));
    if rect.min.x < 0
        || rect.min.y < 0
        || rect.max.x > width
        || rect.max.y > height
        || rect.max.x <= rect.min.x
        || rect.max.y <= rect.min.y
    {
        return Err(backend(format!(
            "crop must be nonempty and inside the output: size={size:?}, crop={crop:?}"
        )));
    }
    Ok(rect)
}

/// The crop clamped to the buffer. Across a resize the crop and the buffer briefly disagree (the
/// output's mode changes before the owner's next `set_crop`); that is never fatal. `None` if
/// nothing of the crop is inside the buffer.
fn clamped_crop(size: PixelSize, crop: Option<PixelRect>) -> Option<PixelRect> {
    let width = i32::try_from(size.width).ok()?;
    let height = i32::try_from(size.height).ok()?;
    let full = PixelRect::new(point2(0, 0), point2(width, height));
    let rect = crop.map_or(Some(full), |crop| crop.intersection(&full))?;
    (rect.max.x > rect.min.x && rect.max.y > rect.min.y).then_some(rect)
}

fn translate_damage(damage: &[[i32; 4]], crop: PixelRect) -> Vec<PixelRect> {
    damage
        .iter()
        .filter_map(|&[x, y, width, height]| {
            if width <= 0 || height <= 0 {
                return None;
            }
            let min_x = i64::from(x).max(i64::from(crop.min.x));
            let min_y = i64::from(y).max(i64::from(crop.min.y));
            let max_x = (i64::from(x) + i64::from(width)).min(i64::from(crop.max.x));
            let max_y = (i64::from(y) + i64::from(height)).min(i64::from(crop.max.y));
            (max_x > min_x && max_y > min_y).then(|| {
                PixelRect::new(
                    point2(
                        (min_x - i64::from(crop.min.x)) as i32,
                        (min_y - i64::from(crop.min.y)) as i32,
                    ),
                    point2(
                        (max_x - i64::from(crop.min.x)) as i32,
                        (max_y - i64::from(crop.min.y)) as i32,
                    ),
                )
            })
        })
        .collect()
}

fn timestamp(seconds_hi: u32, seconds_lo: u32, nanos: u32) -> Option<MonoTime> {
    if nanos >= 1_000_000_000 {
        return None;
    }
    ((u64::from(seconds_hi) << 32) | u64::from(seconds_lo))
        .checked_mul(1_000_000_000)?
        .checked_add(u64::from(nanos))
        .map(MonoTime::from_nanos)
}

fn now() -> Result<MonoTime, PlatformError> {
    let time = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let seconds = u64::try_from(time.tv_sec).map_err(backend)?;
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(time.tv_nsec as u64))
        .map(MonoTime::from_nanos)
        .ok_or_else(|| backend("monotonic timestamp overflow"))
}

struct PendingStart {
    deadline: Instant,
    reply: mpsc::Sender<Result<(), PlatformError>>,
}

struct Capture {
    #[cfg(feature = "gpu")]
    slot: Option<usize>,
    proxy: ExtImageCopyCaptureFrameV1,
    damage: Vec<[i32; 4]>,
    at: Option<MonoTime>,
    ready: bool,
    constraints_revision: u64,
}

#[derive(Clone, Copy)]
struct CursorData(StreamId);

struct CursorFrame {
    proxy: ExtImageCopyCaptureFrameV1,
    // A hotspot received after ready belongs to the next image, not this one.
    ready_hotspot: Option<(i32, i32)>,
}

#[derive(Default)]
struct CursorHistory {
    // The outer None means this stream has never reported a cursor; the inner None is default.
    last: Option<Option<CursorImage>>,
}

impl CursorHistory {
    fn changed(&mut self, image: &Option<CursorImage>) -> bool {
        if self.last.as_ref() == Some(image) {
            return false;
        }
        self.last = Some(image.clone());
        true
    }
}

fn cursor_event(stream: StreamId, cursor: Option<CursorImage>) -> FrameEvent {
    match cursor {
        Some(image) => FrameEvent::Cursor {
            stream,
            cursor: Some(image),
        },
        None => FrameEvent::CursorDefault { stream },
    }
}

struct CursorCapture {
    proxy: ExtImageCopyCaptureCursorSessionV1,
    session: ExtImageCopyCaptureSessionV1,
    entered: bool,
    hotspot: (i32, i32),
    incoming: Constraints,
    constraints: Option<Constraints>,
    constraints_pending: bool,
    reallocate: bool,
    buffer: Option<Buffer>,
    frame: Option<CursorFrame>,
    next_slot: Instant,
    history: CursorHistory,
}

impl CursorCapture {
    fn new(
        manager: &ExtImageCopyCaptureManagerV1,
        source: &ExtImageCaptureSourceV1,
        pointer: &wl_pointer::WlPointer,
        qh: &QueueHandle<State>,
        id: StreamId,
    ) -> Self {
        let proxy = manager.create_pointer_cursor_session(source, pointer, qh, CursorData(id));
        // Exactly one base capture session for this cursor session's entire lifetime.
        let session = proxy.get_capture_session(qh, CursorData(id));
        Self {
            proxy,
            session,
            entered: false,
            hotspot: (0, 0),
            incoming: Constraints::default(),
            constraints: None,
            constraints_pending: true,
            reallocate: false,
            buffer: None,
            frame: None,
            next_slot: Instant::now(),
            history: CursorHistory::default(),
        }
    }

    fn cancel_frame(&mut self) {
        if let Some(frame) = self.frame.take() {
            frame.proxy.destroy();
        }
    }

    fn destroy(mut self) {
        self.cancel_frame();
        self.session.destroy();
        self.proxy.destroy();
        self.buffer.take();
    }

    fn deadline(&self) -> Option<Instant> {
        (self.entered
            && self.frame.is_none()
            && self.constraints.is_some()
            && !self.constraints_pending)
            .then_some(self.next_slot)
    }

    fn advance(
        &mut self,
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<State>,
        id: StreamId,
        gate: &IoGate,
    ) -> Result<Option<Option<CursorImage>>, PlatformError> {
        let mut changed = None;
        if let Some(hotspot) = self.frame.as_ref().and_then(|frame| frame.ready_hotspot) {
            self.cancel_frame();
            self.next_slot = Instant::now() + CURSOR_INTERVAL;
            if self.entered && !self.constraints_pending {
                let buffer = self
                    .buffer
                    .as_ref()
                    .ok_or_else(|| backend("missing cursor buffer"))?;
                let (size, pixels) = buffer.copy(None)?;
                let image = cursor_image(size, hotspot, &pixels, buffer.format)?;
                if self.history.changed(&image) {
                    changed = Some(image);
                }
            }
        }
        if self.frame.is_some()
            || !self.entered
            || self.constraints_pending
            || Instant::now() < self.next_slot
        {
            return Ok(changed);
        }
        if self.reallocate {
            let constraints = self
                .constraints
                .as_ref()
                .ok_or_else(|| backend("missing cursor constraints"))?;
            // Preserve alpha if the compositor offers both formats. XRGB's unused byte is
            // explicitly made opaque during conversion; it is not an alpha channel.
            let format = [wl_shm::Format::Argb8888, wl_shm::Format::Xrgb8888]
                .into_iter()
                .find(|format| constraints.formats.contains(format))
                .ok_or(PlatformError::Unsupported(
                    "BGRA-compatible cursor SHM required",
                ))?;
            self.buffer = Some(Buffer::with_format(shm, qh, constraints, format)?);
            self.reallocate = false;
        }
        if !gate.is_open() {
            return Ok(None);
        }
        if let Some(buffer) = &self.buffer {
            let proxy = self.session.create_frame(qh, CursorData(id));
            proxy.attach_buffer(&buffer.proxy);
            proxy.damage_buffer(0, 0, buffer.size.width as i32, buffer.size.height as i32);
            proxy.capture();
            self.frame = Some(CursorFrame {
                proxy,
                ready_hotspot: None,
            });
            self.next_slot = Instant::now() + CURSOR_INTERVAL;
        }
        Ok(changed)
    }
}

fn cursor_image(
    size: PixelSize,
    hotspot: (i32, i32),
    pixels: &[u8],
    format: wl_shm::Format,
) -> Result<Option<CursorImage>, PlatformError> {
    (size.width as usize)
        .checked_mul(size.height as usize)
        .and_then(|length| length.checked_mul(4))
        .filter(|length| *length > 0 && *length == pixels.len())
        .ok_or_else(|| backend("invalid cursor pixels"))?;
    let has_alpha = format == wl_shm::Format::Argb8888;
    if has_alpha && pixels.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 0) {
        // Transparency cannot distinguish a hidden cursor from a compositor-drawn cursor.
        return Ok(None);
    }
    let longest = size.width.max(size.height);
    let scaled = if longest > 256 {
        PixelSize::new(
            (u64::from(size.width) * 256 / u64::from(longest)).max(1) as u32,
            (u64::from(size.height) * 256 / u64::from(longest)).max(1) as u32,
        )
    } else {
        size
    };
    let mut straight = Vec::new();
    straight
        .try_reserve_exact(scaled.width as usize * scaled.height as usize * 4)
        .map_err(backend)?;
    for y in 0..scaled.height {
        let source_y = u64::from(y) * u64::from(size.height) / u64::from(scaled.height);
        for x in 0..scaled.width {
            let source_x = u64::from(x) * u64::from(size.width) / u64::from(scaled.width);
            let offset = (source_y * u64::from(size.width) + source_x) as usize * 4;
            let pixel = &pixels[offset..offset + 4];
            let alpha = if has_alpha { pixel[3] } else { 255 };
            if alpha == 0 {
                straight.extend_from_slice(&[0; 4]);
            } else {
                for &channel in &pixel[..3] {
                    let channel =
                        (u32::from(channel) * 255 + u32::from(alpha) / 2) / u32::from(alpha);
                    straight.push(channel.min(255) as u8);
                }
                straight.push(alpha);
            }
        }
    }
    let scale_hotspot = |coordinate: i32, original: u32, scaled: u32| {
        let clamped = (coordinate.max(0) as u32).min(original - 1);
        (u64::from(clamped) * u64::from(scaled) / u64::from(original)) as u32
    };
    Ok(Some(CursorImage {
        size: scaled,
        hotspot: (
            scale_hotspot(hotspot.0, size.width, scaled.width),
            scale_hotspot(hotspot.1, size.height, scaled.height),
        ),
        pixels: straight.into(),
    }))
}

struct Stream {
    #[cfg(feature = "gpu")]
    padding: PaddingPool,
    #[cfg(feature = "gpu")]
    gpu: Option<Gpu>,
    #[cfg(feature = "gpu")]
    ring: Option<Ring>,
    #[cfg(feature = "gpu")]
    gpu_disabled: bool,
    #[cfg(feature = "gpu")]
    gpu_ready: bool,
    output: Option<u32>,
    twin: Option<TwinCheck>,
    cursor_source: Option<ExtImageCaptureSourceV1>,
    toplevel: Option<ExtForeignToplevelHandleV1>,
    source: ExtImageCaptureSourceV1,
    session: ExtImageCopyCaptureSessionV1,
    sink: Arc<dyn EventSink<FrameEvent>>,
    crop: Option<PixelRect>,
    pending: Option<PendingStart>,
    incoming: Constraints,
    constraints: Option<Constraints>,
    reallocate: bool,
    buffer: Option<Buffer>,
    capture: Option<Capture>,
    held: Option<Frame>,
    next_slot: Instant,
    interval: Duration,
    full_damage: bool,
    constraints_revision: u64,
    /// Consecutive `failed` frames with an unspecific reason (Hyprland reports those across output
    /// mode changes); the stream ends only after several in a row.
    unknown_failures: u32,
    cursor_started: bool,
    cursor: Option<CursorCapture>,
}

/// Consecutive unspecific frame failures before a stream ends with `Failed`.
const MAX_UNKNOWN_FAILURES: u32 = 5;

impl Stream {
    fn destroy(mut self) {
        if let Some(cursor) = self.cursor.take() {
            cursor.destroy();
        }
        if let Some(capture) = self.capture.take() {
            capture.proxy.destroy();
        }
        self.session.destroy();
        self.source.destroy();
        if let Some(source) = self.cursor_source.take() {
            source.destroy();
        }
        self.buffer.take();
    }
}

struct TwinCheck {
    geometry: TwinGeometry,
    checking: Option<mpsc::Receiver<Result<ResolvedTarget, PlatformError>>>,
    bad_since: Option<Instant>,
    padding_reported: bool,
}

#[cfg(feature = "gpu")]
#[derive(Default)]
struct PaddingPool {
    // Never discard a busy slot on resize: retained frames still count against this bound.
    slots: Vec<(wgpu::Texture, Arc<AtomicBool>)>,
}

#[cfg(feature = "gpu")]
impl PaddingPool {
    fn acquire(
        &mut self,
        device: &wgpu::Device,
        size: PixelSize,
    ) -> Option<(wgpu::Texture, Arc<AtomicBool>)> {
        // Image::drop marks its lease free only after consumption and submitted GPU reads.
        let index = self
            .slots
            .iter()
            .position(|(_, free)| free.load(Ordering::Acquire));
        let index = match index {
            Some(index) => index,
            None if self.slots.len() < 4 => {
                self.slots.push((
                    padding_texture(device, size),
                    Arc::new(AtomicBool::new(true)),
                ));
                self.slots.len() - 1
            }
            None => return None,
        };
        let (texture, free) = &mut self.slots[index];
        if texture.width() != size.width || texture.height() != size.height {
            *texture = padding_texture(device, size);
        }
        free.store(false, Ordering::Release);
        Some((texture.clone(), free.clone()))
    }
}

impl TwinCheck {
    fn ready(
        &mut self,
        lookups: &mpsc::Sender<Lookup>,
        size: PixelSize,
        crop: Option<PixelRect>,
        now: Instant,
    ) -> Result<bool, PlatformError> {
        let since = *self.bad_since.get_or_insert(now);
        if now.duration_since(since) >= TWIN_INCOHERENT {
            return Err(backend("Twin geometry stayed incoherent for 1800 ms"));
        }
        if let Some(checking) = &self.checking {
            match checking.try_recv() {
                Ok(Ok(ResolvedTarget::Window {
                    twin: Some(current),
                    ..
                })) => {
                    self.checking = None;
                    if current == self.geometry && twin_roi(&current, size, crop).is_some() {
                        self.bad_since = None;
                        return Ok(true);
                    }
                }
                Ok(Err(PlatformError::NotFound)) => {
                    return Err(PlatformError::NotFound);
                }
                Err(mpsc::TryRecvError::Empty) => return Ok(false),
                _ => self.checking = None,
            }
        }
        let (reply, result) = mpsc::sync_channel(1);
        lookups
            .send(Lookup {
                target: CaptureTarget::Window(self.geometry.window),
                strict_twin: true,
                deadline: now + CALL_TIMEOUT,
                reply,
            })
            .map_err(backend)?;
        self.checking = Some(result);
        Ok(false)
    }
}

/// Keep the existing R rectangle, including parking's integer-scale approximation. Q may
/// extend on any side of the Window buffer. Buffer constraints must first agree with the
/// actual-scale IPC dimensions within one rounding pixel; padding cannot mask a stale resize.
fn twin_roi(
    geometry: &TwinGeometry,
    size: PixelSize,
    crop: Option<PixelRect>,
) -> Option<(PixelRect, PixelRect)> {
    let expected = geometry.map_crop(geometry.content).ok()?;
    let wanted = crop?;
    if wanted != expected
        || size.width.abs_diff(geometry.size.width) > 1
        || size.height.abs_diff(geometry.size.height) > 1
    {
        return None;
    }
    let actual = wanted.intersection(&PixelRect::from_size(size.cast()))?;
    Some((wanted, actual))
}

/// Apply one geometric padding rule to SHM and native video: Q intersect Window supplies pixels,
/// placed at intersection.min-Q.min; every other R pixel is opaque black, on all four sides.
/// Native padding stays on the same GPU;
/// the source ROI is Q, and its ring slot is released only after the copy submission completes.
fn pad_twin_image(
    size: PixelSize,
    image: FrameImage,
    wanted: PixelRect,
    actual: PixelRect,
    #[cfg(feature = "gpu")] pool: &mut PaddingPool,
) -> Result<Option<(PixelSize, FrameImage)>, PlatformError> {
    let offset = (actual.min.x - wanted.min.x, actual.min.y - wanted.min.y);
    let wanted: PixelSize = wanted
        .size()
        .try_cast()
        .ok_or_else(|| backend("invalid Twin padding size"))?;
    if size == wanted && offset == (0, 0) {
        return Ok(Some((size, image)));
    }
    if offset.0 < 0
        || offset.1 < 0
        || offset.0 as u64 + u64::from(size.width) > u64::from(wanted.width)
        || offset.1 as u64 + u64::from(size.height) > u64::from(wanted.height)
    {
        return Err(backend("invalid Twin padding intersection"));
    }
    let image = match image {
        FrameImage::Cpu { stride, pixels } => {
            let length = (wanted.width as usize)
                .checked_mul(wanted.height as usize)
                .and_then(|n| n.checked_mul(4))
                .ok_or_else(|| backend("Twin padding overflow"))?;
            let mut padded = Vec::new();
            padded.try_reserve_exact(length).map_err(backend)?;
            padded.resize(length, 0);
            for alpha in padded.iter_mut().skip(3).step_by(4) {
                *alpha = 255;
            }
            for y in 0..size.height as usize {
                let from = y * stride as usize;
                let to = ((y + offset.1 as usize) * wanted.width as usize + offset.0 as usize) * 4;
                padded[to..to + size.width as usize * 4]
                    .copy_from_slice(&pixels[from..from + size.width as usize * 4]);
            }
            FrameImage::Cpu {
                stride: wanted.width * 4,
                pixels: padded.into(),
            }
        }
        #[cfg(feature = "gpu")]
        FrameImage::Native(native) => {
            let source = native
                .as_any()
                .downcast_ref::<Image>()
                .ok_or_else(|| backend("unknown Twin native image"))?;
            let _ = source.gpu.device.poll(wgpu::PollType::Poll);
            let Some((texture, free)) = pool.acquire(&source.gpu.device, wanted) else {
                return Ok(None); // Skip; retained native frames already occupy all four leases.
            };
            pad_twin_texture(
                &source.gpu.device,
                &source.gpu.queue,
                &source.texture,
                source.origin,
                size,
                offset,
                &texture,
            );
            FrameImage::Native(Arc::new(Image {
                texture,
                gpu: source.gpu.clone(),
                size: wanted,
                origin: (0, 0),
                free,
                modifier: 0,
            }))
        }
        #[cfg(not(feature = "gpu"))]
        FrameImage::Native(_) => {
            return Err(backend("native Twin padding unavailable"));
        }
    };
    Ok(Some((wanted, image)))
}

#[cfg(feature = "gpu")]
fn padding_texture(device: &wgpu::Device, wanted: PixelSize) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("Twin geometric padding"),
        size: wgpu::Extent3d {
            width: wanted.width,
            height: wanted.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Bgra8Unorm,
        usage: wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    })
}

#[cfg(feature = "gpu")]
fn pad_twin_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    source: &wgpu::Texture,
    origin: (u32, u32),
    size: PixelSize,
    offset: (i32, i32),
    texture: &wgpu::Texture,
) {
    let mut encoder = device.create_command_encoder(&Default::default());
    let view = texture.create_view(&Default::default());
    drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: &view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                store: wgpu::StoreOp::Store,
            },
        })],
        ..Default::default()
    }));
    encoder.copy_texture_to_texture(
        wgpu::TexelCopyTextureInfo {
            origin: wgpu::Origin3d {
                x: origin.0,
                y: origin.1,
                z: 0,
            },
            ..source.as_image_copy()
        },
        wgpu::TexelCopyTextureInfo {
            origin: wgpu::Origin3d {
                x: offset.0 as u32,
                y: offset.1 as u32,
                z: 0,
            },
            ..texture.as_image_copy()
        },
        wgpu::Extent3d {
            width: size.width,
            height: size.height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);
}

struct Output {
    proxy: wl_output::WlOutput,
    name: String,
}

#[derive(Default)]
struct ToplevelProperties {
    identifier: String,
    app_id: String,
    title: String,
}

struct Toplevel {
    proxy: ExtForeignToplevelHandleV1,
    current: Option<ToplevelProperties>,
    pending: ToplevelProperties,
}

fn parse_identifier(value: &str) -> Option<u64> {
    let value = value.strip_prefix("0x").unwrap_or(value);
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(value, 16).ok()
}

struct State {
    #[cfg(feature = "gpu")]
    gpu: Arc<Mutex<Option<Gpu>>>,
    #[cfg(feature = "gpu")]
    main_device: Arc<Mutex<Option<u64>>>,
    #[cfg(feature = "gpu")]
    dmabuf: Option<ZwpLinuxDmabufV1>,
    #[cfg(feature = "gpu")]
    feedback: Option<ZwpLinuxDmabufFeedbackV1>,
    // Retain pending buffers on stream cancellation until the compositor reports completion.
    #[cfg(feature = "gpu")]
    retired: HashMap<wayland_client::backend::ObjectId, (ExtImageCopyCaptureFrameV1, Ring)>,
    gate: Arc<IoGate>,
    /// Whether new streams open a cursor session.
    cursors: Arc<AtomicBool>,
    manager: Option<ExtImageCopyCaptureManagerV1>,
    sources: Option<ExtOutputImageCaptureSourceManagerV1>,
    toplevel_list: Option<ExtForeignToplevelListV1>,
    window_sources: Option<ExtForeignToplevelImageCaptureSourceManagerV1>,
    toplevels: Vec<Toplevel>,
    shm: Option<wl_shm::WlShm>,
    seat: Option<wl_seat::WlSeat>,
    seat_name: Option<u32>,
    pointer: Option<wl_pointer::WlPointer>,
    outputs: HashMap<u32, Output>,
    streams: HashMap<StreamId, Stream>,
    synced: bool,
}

impl State {
    fn destroy_stream(&mut self, stream: Stream) {
        #[cfg(feature = "gpu")]
        let mut stream = stream;
        #[cfg(feature = "gpu")]
        if stream
            .capture
            .as_ref()
            .is_some_and(|c| !c.ready && c.slot.is_some())
        {
            use wayland_client::Proxy;
            if let (Some(capture), Some(ring)) = (stream.capture.take(), stream.ring.take()) {
                self.retired
                    .insert(capture.proxy.id(), (capture.proxy, ring));
            }
        }
        #[cfg(feature = "gpu")]
        if let Some(ring) = &stream.ring {
            // Any pending capture was moved to retired above. Remaining slots are terminal.
            for slot in &ring.slots {
                slot.pending.store(false, Ordering::Release);
            }
        }
        stream.destroy();
    }

    fn stop_cursor(&mut self, id: StreamId, error: impl std::fmt::Display) {
        if let Some(stream) = self.streams.get_mut(&id)
            && let Some(cursor) = stream.cursor.take()
        {
            tracing::debug!(stream = id.0, %error, "cursor reporting stopped");
            cursor.destroy();
        }
    }

    fn release_pointer(&mut self) {
        let ids: Vec<_> = self.streams.keys().copied().collect();
        for id in ids {
            self.stop_cursor(id, "seat pointer unavailable");
        }
        if let Some(pointer) = self.pointer.take()
            && wayland_client::Proxy::version(&pointer) >= 3
        {
            pointer.release();
        }
    }

    fn end(&mut self, id: StreamId, reason: StreamEndReason) {
        if let Some(mut stream) = self.streams.remove(&id) {
            if stream.twin.is_some() && reason != StreamEndReason::Requested {
                tracing::debug!(stream = id.0, ?reason, "Twin Window capture ended");
            }
            let reason = if self.gate.is_open() {
                reason
            } else {
                StreamEndReason::Blocked
            };
            if let Some(pending) = stream.pending.take() {
                let error = match reason {
                    StreamEndReason::Blocked => PlatformError::Locked,
                    StreamEndReason::TargetGone => PlatformError::NotFound,
                    _ => backend("capture ended before start completed"),
                };
                let _ = pending.reply.send(Err(error));
            } else {
                // Dispose the native session before reporting its terminal event.
                let sink = stream.sink.clone();
                self.destroy_stream(stream);
                emit(&sink, FrameEvent::Ended { stream: id, reason });
                return;
            }
            self.destroy_stream(stream);
        }
    }

    fn end_all(&mut self, reason: StreamEndReason) {
        let ids: Vec<_> = self.streams.keys().copied().collect();
        for id in ids {
            self.end(id, reason);
        }
    }

    fn check_gate(&mut self) {
        if !self.gate.is_open() {
            self.end_all(StreamEndReason::Blocked);
        }
    }

    fn start_failed(&mut self, id: StreamId, error: PlatformError) {
        if !self.gate.is_open() {
            self.end_all(StreamEndReason::Blocked);
            return;
        }
        if let Some(mut stream) = self.streams.remove(&id) {
            if stream.twin.is_some() {
                tracing::debug!(stream = id.0, %error, "Twin Window capture failed");
            }
            if let Some(pending) = stream.pending.take() {
                let _ = pending.reply.send(Err(error));
            } else {
                let sink = stream.sink.clone();
                self.destroy_stream(stream);
                emit(
                    &sink,
                    FrameEvent::Ended {
                        stream: id,
                        reason: StreamEndReason::Failed,
                    },
                );
                return;
            }
            self.destroy_stream(stream);
        }
    }
}

fn flush_window_request(connection: &Connection) -> Result<bool, PlatformError> {
    match connection.flush() {
        Ok(()) => Ok(true),
        Err(wayland_client::backend::WaylandError::Io(error))
            if error.kind() == ErrorKind::WouldBlock =>
        {
            Ok(false)
        }
        Err(error) => Err(backend(error)),
    }
}

fn deliver_window(stream: &mut Stream, id: StreamId, gate: &IoGate) {
    if stream.toplevel.is_some()
        && gate.is_open()
        && Instant::now() >= stream.next_slot
        && let Some(frame) = stream.held.take()
    {
        emit(&stream.sink, FrameEvent::Frame { stream: id, frame });
        stream.next_slot = Instant::now() + stream.interval;
    }
}

fn emit(sink: &Arc<dyn EventSink<FrameEvent>>, event: FrameEvent) {
    if catch_unwind(AssertUnwindSafe(|| sink.send(event))).is_err() {
        tracing::warn!("frame event sink panicked");
    }
}

struct Worker {
    connection: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    lookups: mpsc::Sender<Lookup>,
}

impl Worker {
    fn new(
        gate: Arc<IoGate>,
        cursors: Arc<AtomicBool>,
        lookups: mpsc::Sender<Lookup>,
        shutdown: &AtomicBool,
        #[cfg(feature = "gpu")] gpu: Arc<Mutex<Option<Gpu>>>,
        #[cfg(feature = "gpu")] main_device: Arc<Mutex<Option<u64>>>,
    ) -> Result<Self, PlatformError> {
        let connection = Connection::connect_to_env().map_err(backend)?;
        let queue = connection.new_event_queue();
        let qh = queue.handle();
        connection.display().get_registry(&qh, ());
        let mut worker = Self {
            connection,
            queue,
            qh,
            lookups,
            state: State {
                #[cfg(feature = "gpu")]
                gpu,
                #[cfg(feature = "gpu")]
                main_device,
                #[cfg(feature = "gpu")]
                dmabuf: None,
                #[cfg(feature = "gpu")]
                feedback: None,
                #[cfg(feature = "gpu")]
                retired: HashMap::new(),
                gate,
                cursors,
                manager: None,
                sources: None,
                toplevel_list: None,
                window_sources: None,
                toplevels: Vec::new(),
                shm: None,
                seat: None,
                seat_name: None,
                pointer: None,
                outputs: HashMap::new(),
                streams: HashMap::new(),
                synced: false,
            },
        };
        let deadline = Instant::now() + CALL_TIMEOUT;
        // Discover globals, then receive the v4 output names from the bindings they created.
        for _ in 0..2 {
            worker.state.synced = false;
            worker.connection.display().sync(&worker.qh, ());
            while !worker.state.synced {
                if shutdown.load(Ordering::Acquire) || Instant::now() >= deadline {
                    return Err(PlatformError::Timeout);
                }
                worker.pump(deadline)?;
            }
        }
        if worker.state.manager.is_none() || worker.state.sources.is_none() {
            return Err(PlatformError::Unsupported(
                "ext image copy/output source required",
            ));
        }
        if worker.state.shm.is_none() {
            return Err(PlatformError::Unsupported("wl_shm required"));
        }
        Ok(worker)
    }

    fn run(
        &mut self,
        commands: &mpsc::Receiver<Command>,
        shutdown: &AtomicBool,
    ) -> Result<(), PlatformError> {
        while !shutdown.load(Ordering::Acquire) {
            self.state.check_gate();
            self.queue
                .dispatch_pending(&mut self.state)
                .map_err(backend)?;
            while let Ok(command) = commands.try_recv() {
                self.state.check_gate();
                self.command(command);
            }
            let ids: Vec<_> = self.state.streams.keys().copied().collect();
            for id in ids {
                self.state.check_gate();
                if let Err(error) = self.advance(id) {
                    if matches!(error, PlatformError::NotFound) {
                        self.state.end(id, StreamEndReason::TargetGone);
                    } else {
                        self.state.start_failed(id, error);
                    }
                }
                self.state.check_gate();
                if let Err(error) = self.advance_cursor(id) {
                    self.state.stop_cursor(id, error);
                }
            }
            let deadline = self
                .state
                .streams
                .values()
                .flat_map(|stream| {
                    [
                        ((stream.capture.is_none() && stream.constraints.is_some())
                            || stream.held.is_some())
                        .then_some(stream.next_slot),
                        stream.cursor.as_ref().and_then(CursorCapture::deadline),
                    ]
                    .into_iter()
                    .flatten()
                })
                .min()
                .unwrap_or_else(|| Instant::now() + GATE_POLL)
                .min(Instant::now() + GATE_POLL);
            self.pump(deadline)?;
        }
        Ok(())
    }

    fn command(&mut self, command: Command) {
        let result = if Instant::now() >= command.deadline {
            Err(PlatformError::Timeout)
        } else {
            match command.request {
                Request::Start {
                    id,
                    target,
                    crop,
                    max_fps,
                    sink,
                } => {
                    let result = self.begin(id, &target, crop, max_fps, sink);
                    if result.is_ok() {
                        if let Some(stream) = self.state.streams.get_mut(&id) {
                            stream.pending = Some(PendingStart {
                                deadline: command.deadline,
                                reply: command.reply,
                            });
                        }
                        return;
                    }
                    result
                }
                Request::Crop(id, crop, twin) => self
                    .state
                    .streams
                    .get_mut(&id)
                    .ok_or(PlatformError::NotFound)
                    .and_then(|stream| {
                        if stream.toplevel.is_some() && stream.twin.is_none() && crop.is_some() {
                            return Err(backend(
                                "cropped Window capture requires a verified Twin binding",
                            ));
                        }
                        // Validated against the buffer at use (clamped): the output may be
                        // resizing right now, so the current constraints can be stale.
                        if crop.is_some_and(|c| {
                            c.max.x <= c.min.x
                                || c.max.y <= c.min.y
                                || (stream.twin.is_none() && (c.min.x < 0 || c.min.y < 0))
                        }) {
                            return Err(backend("crop must be nonempty and non-negative"));
                        }
                        stream.held = None;
                        if let Some(check) = &mut stream.twin {
                            check.geometry = twin.ok_or(PlatformError::NotFound)?;
                            check.checking = None;
                            if stream
                                .constraints
                                .as_ref()
                                .and_then(|c| c.size)
                                .is_some_and(|size| twin_roi(&check.geometry, size, crop).is_some())
                            {
                                check.bad_since = None;
                            } else {
                                check.bad_since.get_or_insert(Instant::now());
                            }
                        }
                        stream.crop = crop;
                        stream.full_damage = true;
                        Ok(())
                    }),
                Request::Stop(id) => {
                    if self.state.streams.contains_key(&id) {
                        self.state.end(id, StreamEndReason::Requested);
                        Ok(())
                    } else {
                        Err(PlatformError::NotFound)
                    }
                }
            }
        };
        let _ = command.reply.send(result);
    }

    fn begin(
        &mut self,
        id: StreamId,
        target: &ResolvedTarget,
        crop: Option<PixelRect>,
        max_fps: u32,
        sink: Arc<dyn EventSink<FrameEvent>>,
    ) -> Result<(), PlatformError> {
        if !self.state.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        // Cropped Window requests are the explicit Twin route. A binding disappearing between
        // the agent's snapshot and this lookup must fail, never become an ordinary Window crop.
        let twin = match target {
            ResolvedTarget::Window { id, twin, .. } if crop.is_some() => Some(
                twin.as_ref()
                    .filter(|t| t.window == *id)
                    .ok_or_else(|| {
                        backend("cropped Window capture requires a verified Twin binding")
                    })?
                    .clone(),
            ),
            _ => None,
        };
        let bound_output = twin
            .as_ref()
            .map(|twin| {
                let name = format!("CROSSPANE-{:x}", twin.window.0);
                self.state
                    .outputs
                    .iter()
                    .find(|(_, o)| o.name == name)
                    .map(|(&id, native)| (id, native.proxy.clone()))
                    .ok_or(PlatformError::NotFound)
            })
            .transpose()?;
        let (mut output, toplevel, source) = match target {
            ResolvedTarget::Output(name) => {
                let (&output, native) = self
                    .state
                    .outputs
                    .iter()
                    .find(|(_, output)| output.name == *name)
                    .ok_or(PlatformError::NotFound)?;
                let source = self
                    .state
                    .sources
                    .as_ref()
                    .ok_or(PlatformError::Unsupported("output capture source required"))?
                    .create_source(&native.proxy, &self.qh, ());
                (Some(output), None, source)
            }
            ResolvedTarget::Window {
                id, app_id, title, ..
            } => {
                let manager =
                    self.state
                        .window_sources
                        .as_ref()
                        .ok_or(PlatformError::Unsupported(
                            "toplevel capture source required",
                        ))?;
                let direct = self.state.toplevels.iter().find(|toplevel| {
                    toplevel
                        .current
                        .as_ref()
                        .is_some_and(|p| parse_identifier(&p.identifier) == Some(id.0))
                });
                let native = if let Some(native) = direct {
                    native
                } else if twin.is_some() {
                    // Twin identity is exact; a title/app match cannot prove this parked Window.
                    return Err(PlatformError::NotFound);
                } else {
                    let mut matches = self.state.toplevels.iter().filter(|toplevel| {
                        toplevel
                            .current
                            .as_ref()
                            .is_some_and(|p| p.app_id == *app_id && p.title == *title)
                    });
                    let native = matches.next().ok_or(PlatformError::NotFound)?;
                    if matches.next().is_some() {
                        return Err(PlatformError::NotFound);
                    }
                    native
                };
                let source = manager.create_source(&native.proxy, &self.qh, ());
                (None, Some(native.proxy.clone()), source)
            }
        };
        let cursor_source = if let Some((id, native)) = bound_output {
            output = Some(id);
            Some(
                self.state
                    .sources
                    .as_ref()
                    .ok_or(PlatformError::Unsupported("output cursor source required"))?
                    .create_source(&native, &self.qh, ()),
            )
        } else {
            None
        };
        let manager = self
            .state
            .manager
            .as_ref()
            .ok_or(PlatformError::Unsupported("image copy capture required"))?;
        let session = manager.create_session(&source, Options::empty(), &self.qh, id);
        self.state.streams.insert(
            id,
            Stream {
                #[cfg(feature = "gpu")]
                padding: PaddingPool::default(),
                #[cfg(feature = "gpu")]
                gpu: self.state.gpu.lock().map_err(backend)?.clone(),
                #[cfg(feature = "gpu")]
                ring: None,
                #[cfg(feature = "gpu")]
                gpu_disabled: false,
                #[cfg(feature = "gpu")]
                gpu_ready: false,
                output,
                twin: twin.map(|geometry| TwinCheck {
                    geometry,
                    checking: None,
                    bad_since: None,
                    padding_reported: false,
                }),
                cursor_source,
                toplevel,
                source,
                session,
                sink,
                crop,
                pending: None,
                incoming: Constraints::default(),
                constraints: None,
                reallocate: false,
                buffer: None,
                capture: None,
                held: None,
                next_slot: Instant::now(),
                interval: Duration::from_nanos(1_000_000_000_u64.div_ceil(u64::from(max_fps))),
                full_damage: true,
                unknown_failures: 0,
                constraints_revision: 0,
                cursor_started: false,
                cursor: None,
            },
        );
        Ok(())
    }

    fn advance_cursor(&mut self, id: StreamId) -> Result<(), PlatformError> {
        let Some(stream) = self.state.streams.get_mut(&id) else {
            return Ok(());
        };
        // The output's constraints and first buffer must be ready before adding its cursor
        // session. An unavailable cursor path is attempted only once per stream.
        if stream.pending.is_some() || (stream.toplevel.is_some() && stream.cursor_source.is_none())
        {
            return Ok(());
        }
        if !stream.cursor_started {
            stream.cursor_started = true;
            match (&self.state.manager, &self.state.pointer) {
                _ if !self.state.cursors.load(Ordering::Acquire) => (),
                (Some(manager), Some(pointer)) => {
                    stream.cursor = Some(CursorCapture::new(
                        manager,
                        stream.cursor_source.as_ref().unwrap_or(&stream.source),
                        pointer,
                        &self.qh,
                        id,
                    ));
                }
                _ => tracing::debug!(
                    stream = id.0,
                    "cursor capture manager or seat pointer unavailable"
                ),
            }
        }
        let Some(cursor) = stream.cursor.as_mut() else {
            return Ok(());
        };
        let shm = self
            .state
            .shm
            .as_ref()
            .ok_or(PlatformError::Unsupported("cursor wl_shm required"))?;
        let changed = cursor.advance(shm, &self.qh, id, &self.state.gate)?;
        if !self.state.gate.is_open() {
            self.state.end_all(StreamEndReason::Blocked);
            return Ok(());
        }
        if let Some(cursor) = changed {
            emit(&stream.sink, cursor_event(id, cursor));
        }
        Ok(())
    }

    fn advance(&mut self, id: StreamId) -> Result<(), PlatformError> {
        let Some(stream) = self.state.streams.get_mut(&id) else {
            return Ok(());
        };
        if stream
            .pending
            .as_ref()
            .is_some_and(|pending| Instant::now() >= pending.deadline)
        {
            return Err(PlatformError::Timeout);
        }
        if stream.twin.as_ref().is_some_and(|t| {
            t.bad_since
                .is_some_and(|at| at.elapsed() >= TWIN_INCOHERENT)
        }) {
            return Err(backend("Twin geometry stayed incoherent for 1800 ms"));
        }
        if stream.capture.as_ref().is_some_and(|capture| capture.ready) {
            let roi = if let Some(twin) = &mut stream.twin {
                let size = stream
                    .constraints
                    .as_ref()
                    .and_then(|c| c.size)
                    .ok_or_else(|| backend("missing Twin constraints"))?;
                if !twin.ready(&self.lookups, size, stream.crop, Instant::now())? {
                    stream.held = None;
                    return Ok(());
                }
                twin_roi(&twin.geometry, size, stream.crop)
            } else {
                None
            };
            stream.unknown_failures = 0;
            let capture = stream
                .capture
                .take()
                .ok_or_else(|| backend("missing ready frame"))?;
            capture.proxy.destroy();
            // A resize can supersede an already captured buffer. Do not deliver old geometry,
            // especially if set_crop now addresses the new output dimensions.
            if capture.constraints_revision != stream.constraints_revision {
                #[cfg(feature = "gpu")]
                if let (Some(ring), Some(slot)) = (&stream.ring, capture.slot) {
                    ring.release(slot);
                }
                stream.next_slot = Instant::now();
                return Ok(());
            }
            #[cfg(feature = "gpu")]
            let native = if let (Some(ring), Some(slot)) = (&mut stream.ring, capture.slot) {
                stream.gpu_ready = true;
                if let Some(rect) = roi
                    .map(|(_, actual)| actual)
                    .or_else(|| clamped_crop(ring.size, stream.crop))
                {
                    Some(ring.image(slot, rect)?)
                } else {
                    ring.release(slot);
                    return Ok(());
                }
            } else {
                None
            };
            #[cfg(not(feature = "gpu"))]
            let native: Option<Arc<dyn crosspane_platform::NativeImage>> = None;
            let (size, image, rect) = if let Some(image) = native {
                let size = image.size();
                (
                    size,
                    crosspane_platform::FrameImage::Native(image),
                    PixelRect::new(point2(0, 0), point2(size.width as i32, size.height as i32)),
                )
            } else {
                let buffer = stream
                    .buffer
                    .as_ref()
                    .ok_or_else(|| backend("missing frame buffer"))?;
                let Some(rect) = roi
                    .map(|(_, actual)| actual)
                    .or_else(|| clamped_crop(buffer.size, stream.crop))
                else {
                    return Ok(());
                };
                let (size, pixels) = buffer.copy(Some(rect))?;
                (
                    size,
                    crosspane_platform::FrameImage::Cpu {
                        stride: size.width * 4,
                        pixels,
                    },
                    rect,
                )
            };
            let (size, image) = if let Some((wanted, actual)) = roi {
                if wanted != actual {
                    stream.full_damage = true;
                    let padding = (actual.min.x - wanted.min.x)
                        .max(actual.min.y - wanted.min.y)
                        .max(wanted.max.x - actual.max.x)
                        .max(wanted.max.y - actual.max.y);
                    if let Some(twin) = &mut stream.twin
                        && padding > 1
                        && !twin.padding_reported
                    {
                        tracing::info!(
                            "twin crop extends beyond the window buffer by {padding} px"
                        );
                        twin.padding_reported = true;
                    }
                }
                let Some(padded) = pad_twin_image(
                    size,
                    image,
                    wanted,
                    actual,
                    #[cfg(feature = "gpu")]
                    &mut stream.padding,
                )?
                else {
                    return Ok(());
                };
                padded
            } else {
                (size, image)
            };
            let at = capture.at.map(Ok).unwrap_or_else(now)?;
            let damage = if matches!(image, crosspane_platform::FrameImage::Native(_)) {
                None
            } else if stream.full_damage {
                Some(vec![PixelRect::new(
                    point2(0, 0),
                    point2(size.width as i32, size.height as i32),
                )])
            } else if capture.damage.is_empty() {
                None
            } else {
                Some(translate_damage(&capture.damage, rect))
            };
            stream.full_damage = false;
            if !self.state.gate.is_open() {
                self.state.end_all(StreamEndReason::Blocked);
                return Ok(());
            }
            let frame = Frame {
                size,
                image,
                damage,
                at,
            };
            if stream.toplevel.is_some() {
                // Captures stay outstanding; only delivery is throttled. Replacing a held
                // frame must describe all changed pixels relative to the last delivered frame.
                let mut frame = frame;
                if stream.held.is_some() && frame.native().is_none() {
                    frame.damage = Some(vec![PixelRect::new(
                        point2(0, 0),
                        point2(size.width as i32, size.height as i32),
                    )]);
                }
                stream.held = Some(frame);
            } else {
                emit(&stream.sink, FrameEvent::Frame { stream: id, frame });
                stream.next_slot = Instant::now() + stream.interval;
            }
        }
        if stream.capture.is_some()
            || (stream.toplevel.is_none() && Instant::now() < stream.next_slot)
        {
            if stream.toplevel.is_some() && !flush_window_request(&self.connection)? {
                return Ok(());
            }
            deliver_window(stream, id, &self.state.gate);
            return Ok(());
        }
        if stream.reallocate {
            let constraints = stream
                .constraints
                .as_ref()
                .ok_or_else(|| backend("missing buffer constraints"))?;
            constraints
                .size
                .ok_or_else(|| backend("missing buffer size"))?;
            let shm = self
                .state
                .shm
                .as_ref()
                .ok_or(PlatformError::Unsupported("wl_shm required"))?;
            #[cfg(feature = "gpu")]
            {
                stream.ring = None;
                if !stream.gpu_disabled
                    && let (Some(gpu), Some(dmabuf)) = (&stream.gpu, &self.state.dmabuf)
                {
                    stream.ring = Ring::new(gpu, dmabuf, &self.qh, constraints)?;
                }
            }
            #[cfg(feature = "gpu")]
            let native = stream.ring.is_some();
            #[cfg(not(feature = "gpu"))]
            let native = false;
            stream.buffer = if native {
                None
            } else {
                Some(Buffer::new(shm, &self.qh, constraints)?)
            };
            stream.reallocate = false;
            stream.full_damage = true;
            if let Some(pending) = stream.pending.take()
                && pending.reply.send(Ok(())).is_err()
            {
                self.state.end(id, StreamEndReason::Requested);
                return Ok(());
            }
        }
        #[cfg(feature = "gpu")]
        if let Some(ring) = &stream.ring {
            if !self.state.gate.is_open() {
                self.state.end_all(StreamEndReason::Blocked);
                return Ok(());
            }
            if let Some(slot) = ring.acquire() {
                let proxy = stream.session.create_frame(&self.qh, id);
                proxy.attach_buffer(&ring.slots[slot].proxy);
                proxy.damage_buffer(0, 0, ring.size.width as i32, ring.size.height as i32);
                proxy.capture();
                stream.capture = Some(Capture {
                    slot: Some(slot),
                    proxy,
                    damage: Vec::new(),
                    at: None,
                    ready: false,
                    constraints_revision: stream.constraints_revision,
                });
            } else {
                stream.next_slot = Instant::now() + stream.interval;
            }
        }
        if let Some(buffer) = &stream.buffer {
            if !self.state.gate.is_open() {
                self.state.end_all(StreamEndReason::Blocked);
                return Ok(());
            }
            let proxy = stream.session.create_frame(&self.qh, id);
            proxy.attach_buffer(&buffer.proxy);
            proxy.damage_buffer(0, 0, buffer.size.width as i32, buffer.size.height as i32);
            proxy.capture();
            stream.capture = Some(Capture {
                #[cfg(feature = "gpu")]
                slot: None,
                proxy,
                damage: Vec::new(),
                at: None,
                ready: false,
                constraints_revision: stream.constraints_revision,
            });
        }
        if stream.toplevel.is_some() {
            // Put the replacement request on the wire before exposing ready to a caller
            // which may immediately resize or update the source.
            if !flush_window_request(&self.connection)? {
                return Ok(());
            }
            deliver_window(stream, id, &self.state.gate);
        }
        Ok(())
    }

    fn pump(&mut self, deadline: Instant) -> Result<(), PlatformError> {
        self.state.check_gate();
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(backend)?;
        let writable = match self.connection.flush() {
            Ok(()) => false,
            Err(wayland_client::backend::WaylandError::Io(error))
                if error.kind() == ErrorKind::WouldBlock =>
            {
                true
            }
            Err(error) => return Err(backend(error)),
        };
        if let Some(guard) = self.connection.prepare_read() {
            let mut fds = [PollFd::new(
                &self.connection,
                PollFlags::IN
                    | if writable {
                        PollFlags::OUT
                    } else {
                        PollFlags::empty()
                    },
            )];
            let timeout = Timespec::try_from(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(GATE_POLL),
            )
            .map_err(backend)?;
            match poll(&mut fds, Some(&timeout)) {
                Ok(_)
                    if fds[0]
                        .revents()
                        .intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR) =>
                {
                    match guard.read() {
                        Ok(_) => (),
                        Err(wayland_client::backend::WaylandError::Io(error))
                            if error.kind() == ErrorKind::WouldBlock => {}
                        Err(error) => return Err(backend(error)),
                    }
                }
                Ok(_) | Err(rustix::io::Errno::INTR) => (),
                Err(error) => return Err(backend(error)),
            }
        }
        self.state.check_gate();
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(backend)?;
        Ok(())
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        state.check_gate();
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                #[cfg(feature = "gpu")]
                "zwp_linux_dmabuf_v1" if version >= 4 => {
                    let dmabuf: ZwpLinuxDmabufV1 = registry.bind(name, 4, qh, ());
                    state.feedback = Some(dmabuf.get_default_feedback(qh, ()));
                    state.dmabuf = Some(dmabuf);
                }
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "ext_image_copy_capture_manager_v1" => {
                    state.manager = Some(registry.bind(name, 1, qh, ()))
                }
                "ext_output_image_capture_source_manager_v1" => {
                    state.sources = Some(registry.bind(name, 1, qh, ()))
                }
                "ext_foreign_toplevel_list_v1" => {
                    state.toplevel_list = Some(registry.bind(name, 1, qh, ()))
                }
                "ext_foreign_toplevel_image_capture_source_manager_v1" => {
                    state.window_sources = Some(registry.bind(name, 1, qh, ()))
                }
                "wl_seat" if state.seat_name.is_none() => {
                    state.seat_name = Some(name);
                    state.seat = Some(registry.bind(name, version.min(9), qh, ()));
                }
                "wl_output" if version >= 4 => {
                    state.outputs.insert(
                        name,
                        Output {
                            proxy: registry.bind(name, 4, qh, name),
                            name: String::new(),
                        },
                    );
                }
                _ => (),
            },
            wl_registry::Event::GlobalRemove { name } => {
                if state.seat_name == Some(name) {
                    state.release_pointer();
                    if let Some(seat) = state.seat.take()
                        && wayland_client::Proxy::version(&seat) >= 5
                    {
                        seat.release();
                    }
                }
                let ids: Vec<_> = state
                    .streams
                    .iter()
                    .filter_map(|(&id, stream)| (stream.output == Some(name)).then_some(id))
                    .collect();
                for id in ids {
                    state.end(id, StreamEndReason::TargetGone);
                }
                if let Some(output) = state.outputs.remove(&name) {
                    output.proxy.release();
                }
            }
            _ => (),
        }
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ExtForeignToplevelListV1,
        event: list_protocol::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.check_gate();
        match event {
            list_protocol::Event::Toplevel { toplevel } => state.toplevels.push(Toplevel {
                proxy: toplevel,
                current: None,
                pending: ToplevelProperties::default(),
            }),
            list_protocol::Event::Finished => {
                let ids: Vec<_> = state
                    .streams
                    .iter()
                    .filter_map(|(&id, s)| s.toplevel.is_some().then_some(id))
                    .collect();
                for id in ids {
                    state.end(id, StreamEndReason::TargetGone);
                }
                for toplevel in state.toplevels.drain(..) {
                    toplevel.proxy.destroy();
                }
                proxy.destroy();
                state.toplevel_list = None;
            }
            _ => (),
        }
    }
    wayland_client::event_created_child!(State, ExtForeignToplevelListV1, [0 => (ExtForeignToplevelHandleV1, ())]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ExtForeignToplevelHandleV1,
        event: handle_protocol::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.check_gate();
        if matches!(event, handle_protocol::Event::Closed) {
            let ids: Vec<_> = state
                .streams
                .iter()
                .filter_map(|(&id, s)| (s.toplevel.as_ref() == Some(proxy)).then_some(id))
                .collect();
            for id in ids {
                state.end(id, StreamEndReason::TargetGone);
            }
            state.toplevels.retain(|t| &t.proxy != proxy);
            proxy.destroy();
            return;
        }
        let Some(toplevel) = state.toplevels.iter_mut().find(|t| &t.proxy == proxy) else {
            return;
        };
        match event {
            handle_protocol::Event::Identifier { identifier } => {
                toplevel.pending.identifier = identifier
            }
            handle_protocol::Event::Title { title } => toplevel.pending.title = title,
            handle_protocol::Event::AppId { app_id } => toplevel.pending.app_id = app_id,
            handle_protocol::Event::Done => {
                toplevel.current = Some(ToplevelProperties {
                    identifier: toplevel.pending.identifier.clone(),
                    app_id: toplevel.pending.app_id.clone(),
                    title: toplevel.pending.title.clone(),
                })
            }
            _ => (),
        }
    }
}

delegate_noop!(State: ignore ExtForeignToplevelImageCaptureSourceManagerV1);

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        state.check_gate();
        if state.seat.as_ref() != Some(seat) {
            return;
        }
        if let wl_seat::Event::Capabilities { capabilities } = event {
            let has_pointer = matches!(capabilities, WEnum::Value(value)
                if value.contains(wl_seat::Capability::Pointer));
            if has_pointer && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            } else if !has_pointer {
                state.release_pointer();
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name: output_name } = event
            && let Some(output) = state.outputs.get_mut(name)
        {
            output.name = output_name;
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.synced = true;
    }
}

impl Dispatch<ExtImageCopyCaptureCursorSessionV1, CursorData> for State {
    fn event(
        state: &mut Self,
        proxy: &ExtImageCopyCaptureCursorSessionV1,
        event: cursor_protocol::Event,
        data: &CursorData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.check_gate();
        let Some(cursor) = state
            .streams
            .get_mut(&data.0)
            .and_then(|stream| stream.cursor.as_mut())
        else {
            return;
        };
        if &cursor.proxy != proxy {
            return;
        }
        match event {
            cursor_protocol::Event::Enter => cursor.entered = true,
            cursor_protocol::Event::Leave => {
                cursor.entered = false;
                // A paused/off-output frame can be blank indefinitely. Cancel it instead of
                // accidentally reporting that blank image after a later enter.
                cursor.cancel_frame();
            }
            cursor_protocol::Event::Hotspot { x, y } => cursor.hotspot = (x, y),
            _ => (), // Position is deliberately not part of cursor shape reporting.
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, CursorData> for State {
    fn event(
        state: &mut Self,
        proxy: &ExtImageCopyCaptureSessionV1,
        event: session_protocol::Event,
        data: &CursorData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.check_gate();
        let Some(cursor) = state
            .streams
            .get_mut(&data.0)
            .and_then(|stream| stream.cursor.as_mut())
        else {
            return;
        };
        if &cursor.session != proxy {
            return;
        }
        match event {
            session_protocol::Event::BufferSize { width, height } => {
                cursor.constraints_pending = true;
                cursor.incoming.size = Some(PixelSize::new(width, height));
            }
            session_protocol::Event::ShmFormat { format } => {
                cursor.constraints_pending = true;
                if let WEnum::Value(format) = format {
                    cursor.incoming.formats.push(format);
                }
            }
            session_protocol::Event::DmabufDevice { .. }
            | session_protocol::Event::DmabufFormat { .. } => cursor.constraints_pending = true,
            session_protocol::Event::Done => {
                // Destroy the old frame before replacing its buffer. No new frame uses a partial
                // constraint batch, or the size/format of a superseded batch.
                cursor.cancel_frame();
                cursor.constraints = Some(std::mem::take(&mut cursor.incoming));
                cursor.constraints_pending = false;
                cursor.reallocate = true;
            }
            session_protocol::Event::Stopped => {
                state.stop_cursor(data.0, "cursor capture session stopped");
            }
            _ => (),
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, CursorData> for State {
    fn event(
        state: &mut Self,
        proxy: &ExtImageCopyCaptureFrameV1,
        event: frame_protocol::Event,
        data: &CursorData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.check_gate();
        let Some(cursor) = state
            .streams
            .get_mut(&data.0)
            .and_then(|stream| stream.cursor.as_mut())
        else {
            return;
        };
        let Some(frame) = cursor.frame.as_mut() else {
            return;
        };
        if &frame.proxy != proxy {
            return;
        }
        match event {
            frame_protocol::Event::Ready => frame.ready_hotspot = Some(cursor.hotspot),
            frame_protocol::Event::Failed { reason } => {
                state.stop_cursor(data.0, format_args!("cursor frame failed: {reason:?}"));
            }
            _ => (),
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, StreamId> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: session_protocol::Event,
        id: &StreamId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.check_gate();
        let Some(stream) = state.streams.get_mut(id) else {
            return;
        };
        match event {
            session_protocol::Event::BufferSize { width, height } => {
                stream.incoming.size = Some(PixelSize::new(width, height))
            }
            session_protocol::Event::ShmFormat {
                format: WEnum::Value(format),
            } => stream.incoming.formats.push(format),
            #[cfg(feature = "gpu")]
            session_protocol::Event::DmabufDevice { device } => {
                stream.incoming.device = device.as_slice().try_into().ok().map(u64::from_ne_bytes);
            }
            #[cfg(feature = "gpu")]
            session_protocol::Event::DmabufFormat { format, modifiers } => {
                if modifiers.len() % 8 == 0 {
                    stream.incoming.dmabuf_formats.push((
                        format,
                        modifiers
                            .as_chunks::<8>()
                            .0
                            .iter()
                            .map(|m| u64::from_ne_bytes(*m))
                            .collect(),
                    ));
                }
            }
            session_protocol::Event::Done => {
                if stream.toplevel.is_some() {
                    stream.held = None;
                    #[cfg(feature = "gpu")]
                    let can_cancel = stream.capture.as_ref().is_none_or(|c| c.slot.is_none());
                    #[cfg(not(feature = "gpu"))]
                    let can_cancel = true;
                    if can_cancel && let Some(capture) = stream.capture.take() {
                        capture.proxy.destroy();
                    }
                }
                stream.constraints = Some(std::mem::take(&mut stream.incoming));
                stream.constraints_revision = stream.constraints_revision.wrapping_add(1);
                stream.reallocate = true;
            }
            session_protocol::Event::Stopped => state.end(*id, StreamEndReason::TargetGone),
            _ => (),
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, StreamId> for State {
    fn event(
        state: &mut Self,
        proxy: &ExtImageCopyCaptureFrameV1,
        event: frame_protocol::Event,
        id: &StreamId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.check_gate();
        #[cfg(feature = "gpu")]
        {
            use wayland_client::Proxy;
            if state.retired.contains_key(&proxy.id()) {
                if matches!(
                    event,
                    frame_protocol::Event::Ready | frame_protocol::Event::Failed { .. }
                ) && let Some((frame, ring)) = state.retired.remove(&proxy.id())
                {
                    for slot in &ring.slots {
                        slot.pending.store(false, Ordering::Release);
                    }
                    frame.destroy();
                    drop(ring);
                }
                return;
            }
        }
        let Some(stream) = state.streams.get_mut(id) else {
            return;
        };
        let Some(capture) = &mut stream.capture else {
            return;
        };
        if &capture.proxy != proxy {
            return;
        }
        match event {
            frame_protocol::Event::Damage {
                x,
                y,
                width,
                height,
            } => capture.damage.push([x, y, width, height]),
            frame_protocol::Event::PresentationTime {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
            } => capture.at = timestamp(tv_sec_hi, tv_sec_lo, tv_nsec),
            frame_protocol::Event::Ready => capture.ready = true,
            frame_protocol::Event::Failed { reason } => {
                let revision = capture.constraints_revision;
                #[cfg(feature = "gpu")]
                let gpu_slot = capture.slot;
                #[cfg(feature = "gpu")]
                if gpu_slot.is_some() {
                    tracing::debug!(stream = id.0, ?reason, "DMA-BUF capture failed");
                }
                #[cfg(feature = "gpu")]
                if let (Some(ring), Some(slot)) = (&stream.ring, gpu_slot) {
                    ring.release(slot);
                }
                if let Some(capture) = stream.capture.take() {
                    capture.proxy.destroy();
                }
                #[cfg(feature = "gpu")]
                if gpu_slot.is_some()
                    && !stream.gpu_ready
                    && matches!(
                        reason,
                        WEnum::Value(
                            frame_protocol::FailureReason::BufferConstraints
                                | frame_protocol::FailureReason::Unknown
                        )
                    )
                {
                    tracing::info!(
                        stream = id.0,
                        ?reason,
                        "first DMA-BUF capture failed; staying on shm"
                    );
                    stream.gpu_disabled = true;
                    stream.ring = None;
                    stream.reallocate = stream.constraints.is_some();
                    return;
                }
                #[cfg(feature = "gpu")]
                if gpu_slot.is_some() {
                    stream.ring = None;
                }
                match reason {
                    WEnum::Value(frame_protocol::FailureReason::BufferConstraints) => {
                        // Constraints are sent as independent session batches. Wait for done before
                        // allocating; never invent dimensions from output mode notifications.
                        stream.buffer.take();
                        if revision == stream.constraints_revision {
                            stream.constraints = None;
                        }
                        stream.reallocate = stream.constraints.is_some();
                        stream.full_damage = true;
                    }
                    WEnum::Value(frame_protocol::FailureReason::Stopped) => {
                        state.end(*id, StreamEndReason::TargetGone)
                    }
                    // Unspecific failures happen across output mode changes: treat them like a
                    // constraint change and retry, unless they keep happening.
                    _ if stream.unknown_failures + 1 < MAX_UNKNOWN_FAILURES => {
                        stream.unknown_failures += 1;
                        stream.buffer.take();
                        stream.reallocate = stream.constraints.is_some();
                        stream.full_damage = true;
                    }
                    _ => state.end(*id, StreamEndReason::Failed),
                }
            }
            _ => (),
        }
    }
}

#[cfg(feature = "gpu")]
impl Dispatch<ZwpLinuxDmabufFeedbackV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwpLinuxDmabufFeedbackV1,
        event: zwp_linux_dmabuf_feedback_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_linux_dmabuf_feedback_v1::Event::MainDevice { device } = event
            && let Ok(bytes) = device.as_slice().try_into()
            && let Ok(mut main) = state.main_device.lock()
        {
            *main = Some(u64::from_ne_bytes(bytes));
        }
    }
}
#[cfg(feature = "gpu")]
delegate_noop!(State: ignore ZwpLinuxDmabufV1);
#[cfg(feature = "gpu")]
delegate_noop!(State: ignore ZwpLinuxBufferParamsV1);

delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore wl_pointer::WlPointer);
delegate_noop!(State: ignore ExtImageCopyCaptureManagerV1);
delegate_noop!(State: ignore ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(State: ignore ExtImageCaptureSourceV1);

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // Client-side proxies over an owned socket pair exercise real source/session selection
    // without a compositor, global environment changes, or a physical pointer.
    fn fake_worker() -> (Worker, std::os::unix::net::UnixStream) {
        let (client, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let connection = Connection::from_socket(client).unwrap();
        let queue = connection.new_event_queue();
        let qh = queue.handle();
        let registry = connection.display().get_registry(&qh, ());
        let gate = IoGate::new();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        let (lookups, _) = mpsc::channel();
        let toplevel = registry.bind::<ExtForeignToplevelHandleV1, _, _>(8, 1, &qh, ());
        let state = State {
            #[cfg(feature = "gpu")]
            gpu: Arc::new(Mutex::new(None)),
            #[cfg(feature = "gpu")]
            main_device: Arc::new(Mutex::new(None)),
            #[cfg(feature = "gpu")]
            dmabuf: None,
            #[cfg(feature = "gpu")]
            feedback: None,
            #[cfg(feature = "gpu")]
            retired: HashMap::new(),
            gate,
            cursors: Arc::new(AtomicBool::new(false)),
            manager: Some(registry.bind(1, 1, &qh, ())),
            sources: Some(registry.bind(2, 1, &qh, ())),
            window_sources: Some(registry.bind(3, 1, &qh, ())),
            shm: Some(registry.bind(4, 1, &qh, ())),
            pointer: Some(registry.bind(5, 1, &qh, ())),
            seat: None,
            seat_name: None,
            toplevel_list: None,
            toplevels: vec![Toplevel {
                proxy: toplevel,
                current: Some(ToplevelProperties {
                    identifier: "a".to_owned(),
                    app_id: "fixture".to_owned(),
                    title: "fixture".to_owned(),
                }),
                pending: ToplevelProperties::default(),
            }],
            outputs: HashMap::from([(
                7,
                Output {
                    proxy: registry.bind(7, 4, &qh, 7),
                    name: "CROSSPANE-a".to_owned(),
                },
            )]),
            streams: HashMap::new(),
            synced: true,
        };
        (
            Worker {
                connection,
                queue,
                qh,
                state,
                lookups,
            },
            peer,
        )
    }

    #[test]
    fn twin_video_uses_window_source_and_opt_in_separate_output_cursor() {
        use wayland_client::Proxy;
        let (mut worker, _peer) = fake_worker();
        let geometry = twin((20, 40), PixelSize::new(300, 200), PixelSize::new(640, 480));
        let target = ResolvedTarget::Window {
            id: geometry.window,
            app_id: "fixture".to_owned(),
            title: "fixture".to_owned(),
            twin: Some(geometry),
        };
        let crop = Some(PixelRect::new(point2(0, 0), point2(300, 200)));
        let sink: Arc<dyn EventSink<FrameEvent>> = Arc::new(|_| {});
        worker
            .begin(StreamId(1), &target, crop, 60, sink.clone())
            .unwrap();
        let stream = &worker.state.streams[&StreamId(1)];
        assert!(stream.toplevel.is_some());
        assert_eq!(stream.output, Some(7));
        assert_ne!(
            stream.source.id(),
            stream.cursor_source.as_ref().unwrap().id()
        );
        worker.advance_cursor(StreamId(1)).unwrap();
        assert!(worker.state.streams[&StreamId(1)].cursor.is_none());
        worker.state.cursors.store(true, Ordering::Release);
        worker
            .begin(StreamId(2), &target, crop, 60, sink.clone())
            .unwrap();
        worker.advance_cursor(StreamId(2)).unwrap();
        assert!(worker.state.streams[&StreamId(2)].cursor.is_some());
        worker.begin(StreamId(3), &target, None, 60, sink).unwrap();
        worker.advance_cursor(StreamId(3)).unwrap();
        assert!(worker.state.streams[&StreamId(3)].cursor.is_none());
        assert!(worker.state.streams[&StreamId(3)].output.is_none());
        worker.state.end_all(StreamEndReason::Requested);
    }

    fn fake_twin_target(size: PixelSize) -> ResolvedTarget {
        ResolvedTarget::Window {
            id: WindowId(10),
            app_id: "fixture".into(),
            title: "fixture".into(),
            twin: Some(twin((0, 0), size, PixelSize::new(640, 480))),
        }
    }

    fn video_constraints(worker: &mut Worker, id: StreamId, size: PixelSize) {
        let session = worker.state.streams[&id].session.clone();
        for event in [
            session_protocol::Event::BufferSize {
                width: size.width,
                height: size.height,
            },
            session_protocol::Event::ShmFormat {
                format: WEnum::Value(wl_shm::Format::Argb8888),
            },
            session_protocol::Event::Done,
        ] {
            <State as Dispatch<ExtImageCopyCaptureSessionV1, StreamId>>::event(
                &mut worker.state,
                &session,
                event,
                &id,
                &worker.connection,
                &worker.qh,
            );
        }
        worker.advance(id).unwrap();
    }

    fn deliver_fake_window(worker: &mut Worker, id: StreamId, pixels: &[u8]) {
        let stream = worker.state.streams.get_mut(&id).unwrap();
        stream
            .buffer
            .as_ref()
            .unwrap()
            .file
            .write_all_at(pixels, 0)
            .unwrap();
        let capture = stream.capture.as_ref().unwrap().proxy.clone();
        let geometry = stream.twin.as_ref().unwrap().geometry.clone();
        let (send, checking) = mpsc::sync_channel(1);
        send.send(Ok(ResolvedTarget::Window {
            id: geometry.window,
            app_id: "fixture".into(),
            title: "fixture".into(),
            twin: Some(geometry),
        }))
        .unwrap();
        stream.twin.as_mut().unwrap().checking = Some(checking);
        stream.next_slot = Instant::now() - Duration::from_secs(1);
        <State as Dispatch<ExtImageCopyCaptureFrameV1, StreamId>>::event(
            &mut worker.state,
            &capture,
            frame_protocol::Event::Ready,
            &id,
            &worker.connection,
            &worker.qh,
        );
        worker.advance(id).unwrap();
    }

    #[test]
    fn window_protocol_start_pending_and_midstream_failures_clean_up_without_output_fallback() {
        for pending in [true, false] {
            let (mut worker, _peer) = fake_worker();
            let size = PixelSize::new(3, 2);
            let (send, events) = mpsc::channel();
            let (reply, result) = mpsc::channel();
            worker.command(Command {
                request: Request::Start {
                    id: StreamId(1),
                    target: fake_twin_target(size),
                    crop: Some(PixelRect::from_size(size.cast())),
                    max_fps: 60,
                    sink: Arc::new(move |e| {
                        let _ = send.send(e);
                    }),
                },
                deadline: Instant::now() + CALL_TIMEOUT,
                reply,
            });
            assert!(result.try_recv().is_err());
            assert!(worker.state.streams[&StreamId(1)].toplevel.is_some());
            if pending {
                let session = worker.state.streams[&StreamId(1)].session.clone();
                <State as Dispatch<ExtImageCopyCaptureSessionV1, StreamId>>::event(
                    &mut worker.state,
                    &session,
                    session_protocol::Event::Stopped,
                    &StreamId(1),
                    &worker.connection,
                    &worker.qh,
                );
                assert!(result.try_recv().unwrap().is_err());
                assert!(events.try_recv().is_err());
            } else {
                video_constraints(&mut worker, StreamId(1), size);
                assert!(result.try_recv().unwrap().is_ok());
                for failure in 0..MAX_UNKNOWN_FAILURES {
                    let capture = worker.state.streams[&StreamId(1)]
                        .capture
                        .as_ref()
                        .unwrap()
                        .proxy
                        .clone();
                    <State as Dispatch<ExtImageCopyCaptureFrameV1, StreamId>>::event(
                        &mut worker.state,
                        &capture,
                        frame_protocol::Event::Failed {
                            reason: WEnum::Value(frame_protocol::FailureReason::Unknown),
                        },
                        &StreamId(1),
                        &worker.connection,
                        &worker.qh,
                    );
                    if failure + 1 < MAX_UNKNOWN_FAILURES {
                        worker.advance(StreamId(1)).unwrap();
                    }
                }
                assert!(matches!(
                    events.try_recv().unwrap(),
                    FrameEvent::Ended {
                        stream: StreamId(1),
                        reason: StreamEndReason::Failed
                    }
                ));
            }
            assert!(worker.state.streams.is_empty());
            assert!(events.try_recv().is_err());
        }
    }

    #[test]
    fn successful_transparent_and_denial_window_frames_pass_backend_roi_unchanged() {
        let (mut worker, _peer) = fake_worker();
        let size = PixelSize::new(3, 2);
        let (send, events) = mpsc::channel();
        worker
            .begin(
                StreamId(1),
                &fake_twin_target(size),
                Some(PixelRect::from_size(size.cast())),
                60,
                Arc::new(move |e| {
                    let _ = send.send(e);
                }),
            )
            .unwrap();
        video_constraints(&mut worker, StreamId(1), size);
        for pixels in [vec![0; 24], [37, 91, 201, 255, 190, 60, 22, 255].repeat(3)] {
            deliver_fake_window(&mut worker, StreamId(1), &pixels);
            let FrameEvent::Frame { stream, frame } = events.try_recv().unwrap() else {
                panic!("successful policy frame rejected");
            };
            assert_eq!(stream, StreamId(1));
            assert_eq!(frame.size, size);
            let FrameImage::Cpu {
                pixels: actual,
                stride,
            } = frame.image
            else {
                panic!("not SHM");
            };
            assert_eq!((stride, &*actual), (12, &*pixels));
            assert!(worker.state.streams[&stream].toplevel.is_some());
            assert!(events.try_recv().is_err());
        }
        worker.state.end_all(StreamEndReason::Requested);
    }

    #[test]
    fn twin_video_and_output_cursor_deliver_together_and_cursor_failure_only_degrades_cursor() {
        use std::io::Read;
        use wayland_client::Proxy;
        let (mut worker, mut peer) = fake_worker();
        worker.state.cursors.store(true, Ordering::Release);
        let size = PixelSize::new(3, 2);
        let id = StreamId(1);
        let (send, events) = mpsc::channel();
        worker
            .begin(
                id,
                &fake_twin_target(size),
                Some(PixelRect::from_size(size.cast())),
                60,
                Arc::new(move |e| {
                    let _ = send.send(e);
                }),
            )
            .unwrap();
        video_constraints(&mut worker, id, size);
        deliver_fake_window(&mut worker, id, &[0; 24]);
        assert!(matches!(
            events.try_recv().unwrap(),
            FrameEvent::Frame { .. }
        ));
        worker.advance_cursor(id).unwrap();
        worker.connection.flush().unwrap();
        peer.set_nonblocking(true).unwrap();
        let mut wire = Vec::new();
        loop {
            let mut bytes = [0; 4096];
            match peer.read(&mut bytes) {
                Ok(0) => break,
                Ok(n) => wire.extend_from_slice(&bytes[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => panic!("fake protocol peer: {e}"),
            }
        }
        let manager = worker.state.manager.as_ref().unwrap().id().protocol_id();
        let stream = &worker.state.streams[&id];
        let mut video_source = None;
        let mut cursor_source = None;
        let word = |bytes: &[u8]| u32::from_ne_bytes(bytes[..4].try_into().unwrap());
        let mut offset = 0;
        while offset < wire.len() {
            let header = word(&wire[offset + 4..]);
            let length = (header >> 16) as usize;
            assert!(length >= 8 && offset + length <= wire.len());
            if word(&wire[offset..]) == manager {
                match header & 0xffff {
                    0 => video_source = Some(word(&wire[offset + 12..])),
                    1 => cursor_source = Some(word(&wire[offset + 12..])),
                    _ => (),
                }
            }
            offset += length;
        }
        assert_eq!(video_source, Some(stream.source.id().protocol_id()));
        assert_eq!(
            cursor_source,
            Some(stream.cursor_source.as_ref().unwrap().id().protocol_id())
        );
        assert_ne!(video_source, cursor_source);
        let cursor = worker.state.streams[&id]
            .cursor
            .as_ref()
            .unwrap()
            .proxy
            .clone();
        for event in [
            cursor_protocol::Event::Enter,
            cursor_protocol::Event::Hotspot { x: 1, y: 0 },
        ] {
            <State as Dispatch<ExtImageCopyCaptureCursorSessionV1, CursorData>>::event(
                &mut worker.state,
                &cursor,
                event,
                &CursorData(id),
                &worker.connection,
                &worker.qh,
            );
        }
        let session = worker.state.streams[&id]
            .cursor
            .as_ref()
            .unwrap()
            .session
            .clone();
        for event in [
            session_protocol::Event::BufferSize {
                width: 2,
                height: 1,
            },
            session_protocol::Event::ShmFormat {
                format: WEnum::Value(wl_shm::Format::Argb8888),
            },
            session_protocol::Event::Done,
        ] {
            <State as Dispatch<ExtImageCopyCaptureSessionV1, CursorData>>::event(
                &mut worker.state,
                &session,
                event,
                &CursorData(id),
                &worker.connection,
                &worker.qh,
            );
        }
        worker.advance_cursor(id).unwrap();
        let cursor = worker.state.streams[&id].cursor.as_ref().unwrap();
        cursor
            .buffer
            .as_ref()
            .unwrap()
            .file
            .write_all_at(&[9, 19, 29, 255, 39, 49, 59, 255], 0)
            .unwrap();
        let frame = cursor.frame.as_ref().unwrap().proxy.clone();
        <State as Dispatch<ExtImageCopyCaptureFrameV1, CursorData>>::event(
            &mut worker.state,
            &frame,
            frame_protocol::Event::Ready,
            &CursorData(id),
            &worker.connection,
            &worker.qh,
        );
        worker.advance_cursor(id).unwrap();
        let FrameEvent::Cursor {
            stream,
            cursor: Some(shape),
        } = events.try_recv().unwrap()
        else {
            panic!("cursor event absent");
        };
        assert_eq!(stream, id);
        assert_eq!(shape.hotspot, (1, 0));
        assert_eq!(&*shape.pixels, &[9, 19, 29, 255, 39, 49, 59, 255]);
        <State as Dispatch<ExtImageCopyCaptureSessionV1, CursorData>>::event(
            &mut worker.state,
            &session,
            session_protocol::Event::Stopped,
            &CursorData(id),
            &worker.connection,
            &worker.qh,
        );
        assert!(worker.state.streams[&id].cursor.is_none());
        deliver_fake_window(&mut worker, id, &[0; 24]);
        assert!(matches!(
            events.try_recv().unwrap(),
            FrameEvent::Frame { .. }
        ));
        assert!(events.try_recv().is_err());
        worker.state.end_all(StreamEndReason::Requested);
    }

    #[test]
    fn twin_output_and_window_loss_end_without_an_output_fallback() {
        for output_loss in [true, false] {
            let (mut worker, _peer) = fake_worker();
            let geometry = twin((0, 0), PixelSize::new(300, 200), PixelSize::new(640, 480));
            let target = ResolvedTarget::Window {
                id: geometry.window,
                app_id: "fixture".to_owned(),
                title: "fixture".to_owned(),
                twin: Some(geometry),
            };
            let (send, events) = mpsc::channel();
            let sink: Arc<dyn EventSink<FrameEvent>> = Arc::new(move |e| {
                let _ = send.send(e);
            });
            worker
                .begin(
                    StreamId(1),
                    &target,
                    Some(PixelRect::new(point2(0, 0), point2(300, 200))),
                    60,
                    sink,
                )
                .unwrap();
            if output_loss {
                let registry = worker.connection.display().get_registry(&worker.qh, ());
                <State as Dispatch<wl_registry::WlRegistry, ()>>::event(
                    &mut worker.state,
                    &registry,
                    wl_registry::Event::GlobalRemove { name: 7 },
                    &(),
                    &worker.connection,
                    &worker.qh,
                );
            } else {
                let handle = worker.state.toplevels[0].proxy.clone();
                <State as Dispatch<ExtForeignToplevelHandleV1, ()>>::event(
                    &mut worker.state,
                    &handle,
                    handle_protocol::Event::Closed,
                    &(),
                    &worker.connection,
                    &worker.qh,
                );
            }
            assert!(matches!(
                events.try_recv().unwrap(),
                FrameEvent::Ended {
                    stream: StreamId(1),
                    reason: StreamEndReason::TargetGone
                }
            ));
            assert!(worker.state.streams.is_empty());
        }
        let (mut worker, _peer) = fake_worker();
        worker.state.window_sources = None;
        assert!(matches!(
            worker.begin(
                StreamId(1),
                &ResolvedTarget::Window {
                    id: WindowId(10),
                    app_id: "fixture".to_owned(),
                    title: "fixture".to_owned(),
                    twin: None,
                },
                None,
                60,
                Arc::new(|_| {})
            ),
            Err(PlatformError::Unsupported(_))
        ));
        assert!(worker.state.streams.is_empty());
    }

    #[test]
    fn twin_binding_disappearing_between_agent_snapshot_and_lookup_never_starts_unbound() {
        for lost in ["workspace", "output", "native window"] {
            let (mut worker, _peer) = fake_worker();
            // The agent's earlier snapshot was valid and produced this Window-local Q.
            let geometry = twin((20, 40), PixelSize::new(300, 200), PixelSize::new(640, 480));
            let q = geometry.map_crop(geometry.content).unwrap();
            let mut target = ResolvedTarget::Window {
                id: geometry.window,
                app_id: "fixture".into(),
                title: "fixture".into(),
                twin: Some(geometry),
            };
            if lost == "workspace" {
                // The backend's later client lookup sees the Window outside its Twin workspace.
                if let ResolvedTarget::Window { twin, .. } = &mut target {
                    *twin = None;
                }
            } else if lost == "output" {
                // Or the native output disappeared after IPC lookup but before session creation.
                worker.state.outputs.remove(&7);
            } else {
                worker.state.toplevels[0]
                    .current
                    .as_mut()
                    .unwrap()
                    .identifier = "b".into();
            }
            assert!(
                worker
                    .begin(StreamId(1), &target, Some(q), 60, Arc::new(|_| {}))
                    .is_err()
            );
            assert!(
                worker.state.streams.is_empty(),
                "no unbound session may escape"
            );
            // Mirror Window(None) remains an ordinary Window stream in both cases.
            worker
                .begin(StreamId(2), &target, None, 60, Arc::new(|_| {}))
                .unwrap();
            assert!(worker.state.streams[&StreamId(2)].output.is_none());
            assert!(worker.state.streams[&StreamId(2)].twin.is_none());
            let (reply, result) = mpsc::channel();
            worker.command(Command {
                request: Request::Crop(StreamId(2), Some(q), None),
                deadline: Instant::now() + CALL_TIMEOUT,
                reply,
            });
            assert!(result.try_recv().unwrap().is_err());
            worker.state.end_all(StreamEndReason::Requested);
        }
    }

    fn twin(origin: (i32, i32), size: PixelSize, extent: PixelSize) -> TwinGeometry {
        TwinGeometry {
            window: WindowId(10),
            display: DisplayId(2),
            origin,
            size,
            extent,
            content: PixelRect::from_origin_and_size(point2(origin.0, origin.1), size.cast())
                .intersection(&PixelRect::from_size(extent.cast()))
                .unwrap(),
        }
    }

    #[test]
    fn twin_mapping_offset_bars_padding_and_partial_negative_clipping() {
        for (origin, size, extent, r, q) in [
            (
                (20, 40),
                (300, 200),
                (640, 480),
                (20, 40, 320, 240),
                (0, 0, 300, 200),
            ),
            (
                (-20, -30),
                (300, 200),
                (640, 480),
                (0, 0, 280, 170),
                (20, 30, 300, 200),
            ),
            (
                (20, 40),
                (700, 500),
                (640, 480),
                (20, 40, 640, 480),
                (0, 0, 620, 440),
            ),
        ] {
            let rect = |(x, y, w, h)| PixelRect::new(point2(x, y), point2(w, h));
            let geometry = twin(
                origin,
                PixelSize::new(size.0, size.1),
                PixelSize::new(extent.0, extent.1),
            );
            assert_eq!(geometry.map_crop(rect(r)).unwrap(), rect(q));
            assert!(geometry.map_crop(rect((r.0 + 1, r.1, r.2, r.3))).is_err());
        }
    }

    #[test]
    fn twin_fractional_snapshot_reuses_parking_geometry_and_rejects_unknown_origin() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("cp-twin-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("hypr/fake")).unwrap();
        let socket = UnixListener::bind(dir.join("hypr/fake/.socket.sock")).unwrap();
        let client = serde_json::json!({"stableId":"a","mapped":true,"monitor":2,
            "workspace":{"name":"crosspane-a"},"at":[1048626,30],"size":[300,200]});
        let monitor = serde_json::json!({"name":"CROSSPANE-a","id":2,"scale":1.25,
            "x":1048576,"y":0,"width":640,"height":480});
        let expected = super::super::parking::parked_from(WindowId(10), &client, &monitor).unwrap();
        let mut unknown = client.clone();
        unknown["at"] = serde_json::Value::Null;
        let server = std::thread::spawn(move || {
            for (command, reply) in [
                ("j/clients", serde_json::json!([client])),
                ("j/monitors", serde_json::json!([monitor.clone()])),
                ("j/clients", serde_json::json!([unknown])),
                ("j/monitors", serde_json::json!([monitor])),
            ] {
                let (mut stream, _) = socket.accept().unwrap();
                let mut bytes = [0; 64];
                let n = stream.read(&mut bytes).unwrap();
                assert_eq!(&bytes[..n], command.as_bytes());
                stream.write_all(reply.to_string().as_bytes()).unwrap();
            }
        });
        let ipc = HyprIpc::new("fake", &dir, Duration::from_millis(100));
        let geometry = TwinGeometry::read(&ipc, WindowId(10)).unwrap();
        assert_eq!(geometry.origin, (63, 38));
        assert_eq!(geometry.size, PixelSize::new(375, 250));
        assert_eq!(
            geometry.map_crop(expected.content).unwrap(),
            PixelRect::new(point2(-13, -8), point2(287, 192))
        );
        assert!(TwinGeometry::read(&ipc, WindowId(10)).is_err());
        server.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn twin_stale_snapshot_holds_one_bounded_lookup_then_ends_at_two_seconds() {
        assert_eq!(TWIN_INCOHERENT, Duration::from_millis(1800));
        let geometry = twin((0, 0), PixelSize::new(300, 200), PixelSize::new(640, 480));
        let crop = Some(PixelRect::new(point2(0, 0), point2(300, 200)));
        let mut check = TwinCheck {
            geometry: geometry.clone(),
            checking: None,
            bad_since: None,
            padding_reported: false,
        };
        let (send, requests) = mpsc::channel();
        let at = Instant::now();
        assert!(!check.ready(&send, geometry.size, crop, at).unwrap());
        let request = requests.try_recv().unwrap();
        for millis in 1..100 {
            assert!(
                !check
                    .ready(
                        &send,
                        geometry.size,
                        crop,
                        at + Duration::from_millis(millis)
                    )
                    .unwrap()
            );
        }
        assert!(requests.try_recv().is_err());
        let mut stale = geometry.clone();
        stale.origin.0 = 1;
        request
            .reply
            .send(Ok(ResolvedTarget::Window {
                id: geometry.window,
                app_id: String::new(),
                title: String::new(),
                twin: Some(stale),
            }))
            .unwrap();
        assert!(
            !check
                .ready(&send, geometry.size, crop, at + Duration::from_secs(1))
                .unwrap()
        );
        requests
            .try_recv()
            .unwrap()
            .reply
            .send(Err(PlatformError::Timeout))
            .unwrap();
        assert!(
            !check
                .ready(&send, geometry.size, crop, at + Duration::from_millis(1799))
                .unwrap()
        );
        assert!(
            check
                .ready(&send, geometry.size, crop, at + Duration::from_millis(1800))
                .is_err()
        );
    }

    #[test]
    fn twin_geometry_recovers_only_with_the_exact_crop_and_constraints() {
        let geometry = twin((-10, 0), PixelSize::new(300, 200), PixelSize::new(640, 480));
        let crop = Some(PixelRect::new(point2(10, 0), point2(300, 200)));
        let mut check = TwinCheck {
            geometry: geometry.clone(),
            checking: None,
            bad_since: None,
            padding_reported: false,
        };
        let (send, requests) = mpsc::channel();
        let at = Instant::now();
        let respond = |request: Lookup| {
            request
                .reply
                .send(Ok(ResolvedTarget::Window {
                    id: geometry.window,
                    app_id: String::new(),
                    title: String::new(),
                    twin: Some(geometry.clone()),
                }))
                .unwrap()
        };
        assert!(!check.ready(&send, geometry.size, crop, at).unwrap());
        respond(requests.try_recv().unwrap());
        assert!(
            !check
                .ready(
                    &send,
                    geometry.size,
                    Some(PixelRect::new(point2(0, 0), point2(290, 200))),
                    at + Duration::from_millis(1)
                )
                .unwrap()
        );
        respond(requests.try_recv().unwrap());
        assert!(
            check
                .ready(&send, geometry.size, crop, at + Duration::from_millis(2))
                .unwrap()
        );
        assert!(check.bad_since.is_none());
        assert!(twin_roi(&geometry, geometry.size, None).is_none());
        assert!(
            twin_roi(
                &geometry,
                geometry.size,
                Some(PixelRect::new(point2(0, 0), point2(300, 200)))
            )
            .is_none()
        );
    }

    #[test]
    fn stale_buffer_dimensions_hold_instead_of_becoming_geometric_padding() {
        let geometry = twin((0, 0), PixelSize::new(600, 400), PixelSize::new(800, 600));
        let crop = Some(geometry.map_crop(geometry.content).unwrap());
        for size in [
            PixelSize::new(300, 200),
            PixelSize::new(598, 400),
            PixelSize::new(600, 402),
        ] {
            assert!(twin_roi(&geometry, size, crop).is_none());
        }
        for size in [
            geometry.size,
            PixelSize::new(599, 399),
            PixelSize::new(601, 401),
        ] {
            assert!(twin_roi(&geometry, size, crop).is_some());
        }
        let (send, requests) = mpsc::channel();
        let at = Instant::now();
        let mut check = TwinCheck {
            geometry: geometry.clone(),
            checking: None,
            bad_since: None,
            padding_reported: false,
        };
        assert!(
            !check
                .ready(&send, PixelSize::new(300, 200), crop, at)
                .unwrap()
        );
        requests
            .try_recv()
            .unwrap()
            .reply
            .send(Ok(ResolvedTarget::Window {
                id: geometry.window,
                app_id: String::new(),
                title: String::new(),
                twin: Some(geometry),
            }))
            .unwrap();
        assert!(
            !check
                .ready(
                    &send,
                    PixelSize::new(300, 200),
                    crop,
                    at + Duration::from_millis(1799)
                )
                .unwrap()
        );
        assert!(
            check
                .ready(
                    &send,
                    PixelSize::new(300, 200),
                    crop,
                    at + Duration::from_millis(1800)
                )
                .is_err()
        );
    }

    #[test]
    fn ordinary_window_ipc_lookup_ignores_missing_twin_output_but_cropped_lookup_requires_it() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("cp-window-lookup-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("hypr/fake")).unwrap();
        let socket = UnixListener::bind(dir.join("hypr/fake/.socket.sock")).unwrap();
        let client = serde_json::json!({"stableId":"a","mapped":true,"monitor":2,
            "workspace":{"name":"crosspane-a"},"title":"fixture","class":"fixture"});
        let server = std::thread::spawn(move || {
            for _ in 0..4 {
                let (mut stream, _) = socket.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut bytes = [0; 64];
                let n = stream.read(&mut bytes).unwrap();
                let value = if std::str::from_utf8(&bytes[..n])
                    .unwrap()
                    .contains("clients")
                {
                    serde_json::json!([client])
                } else {
                    serde_json::json!([])
                };
                stream.write_all(value.to_string().as_bytes()).unwrap();
            }
        });
        let ipc = HyprIpc::new("fake", &dir, Duration::from_millis(50));
        assert!(matches!(
            resolve_window(&ipc, WindowId(10), false).unwrap(),
            ResolvedTarget::Window { twin: None, .. }
        ));
        assert!(matches!(
            resolve_window(&ipc, WindowId(10), true),
            Err(PlatformError::NotFound)
        ));
        server.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn twin_unverified_binding_holds_until_the_same_deadline() {
        let geometry = twin((0, 0), PixelSize::new(300, 200), PixelSize::new(640, 480));
        let crop = Some(geometry.content);
        let mut check = TwinCheck {
            geometry: geometry.clone(),
            checking: None,
            bad_since: None,
            padding_reported: false,
        };
        let (send, requests) = mpsc::channel();
        let at = Instant::now();
        assert!(!check.ready(&send, geometry.size, crop, at).unwrap());
        requests
            .try_recv()
            .unwrap()
            .reply
            .send(Ok(ResolvedTarget::Window {
                id: geometry.window,
                app_id: String::new(),
                title: String::new(),
                twin: None,
            }))
            .unwrap();
        assert!(
            !check
                .ready(&send, geometry.size, crop, at + Duration::from_millis(100))
                .unwrap()
        );
        assert!(
            check
                .ready(&send, geometry.size, crop, at + TWIN_INCOHERENT)
                .is_err()
        );
    }

    #[test]
    fn twin_geometric_padding_is_opaque_and_preserves_requested_size_and_damage_origin() {
        let geometry = twin((20, 40), PixelSize::new(3, 2), PixelSize::new(640, 480));
        let q = PixelRect::new(point2(0, 0), point2(3, 2));
        assert_eq!(
            twin_roi(&geometry, PixelSize::new(2, 1), Some(q))
                .unwrap()
                .0,
            q
        );
        assert!(twin_roi(&geometry, PixelSize::new(0, 0), Some(q)).is_none());
        let (size, image) = pad_twin_image(
            PixelSize::new(2, 1),
            FrameImage::Cpu {
                stride: 8,
                pixels: Arc::from([1, 2, 3, 4, 5, 6, 7, 8]),
            },
            q,
            PixelRect::new(point2(0, 0), point2(2, 1)),
            #[cfg(feature = "gpu")]
            &mut PaddingPool::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(size, geometry.size);
        let FrameImage::Cpu { stride, pixels } = image else {
            panic!("not CPU")
        };
        assert_eq!(stride, 12);
        assert_eq!(
            &*pixels,
            &[
                1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255
            ]
        );
        assert_eq!(
            translate_damage(
                &[[10, 0, 5, 5]],
                PixelRect::new(point2(10, 0), point2(20, 10))
            ),
            vec![PixelRect::new(point2(0, 0), point2(5, 5))]
        );
        let wanted = PixelRect::new(point2(-13, -8), point2(287, 192));
        let actual = PixelRect::new(point2(0, 0), point2(287, 192));
        let (_, image) = pad_twin_image(
            actual.size().cast(),
            FrameImage::Cpu {
                stride: 287 * 4,
                pixels: Arc::from(vec![7; 287 * 192 * 4]),
            },
            wanted,
            actual,
            #[cfg(feature = "gpu")]
            &mut PaddingPool::default(),
        )
        .unwrap()
        .unwrap();
        let FrameImage::Cpu { pixels, stride } = image else {
            panic!("not CPU")
        };
        for y in 0..200 {
            for x in 0..300 {
                let at = y * stride as usize + x * 4;
                assert_eq!(
                    &pixels[at..at + 4],
                    if x < 13 || y < 8 {
                        &[0, 0, 0, 255]
                    } else {
                        &[7; 4]
                    }
                );
            }
        }
    }

    fn image(size: PixelSize, hotspot: (i32, i32), pixels: &[u8]) -> Option<CursorImage> {
        cursor_image(size, hotspot, pixels, wl_shm::Format::Argb8888).unwrap()
    }

    #[cfg(feature = "gpu")]
    #[test]
    fn twin_gpu_geometric_padding_copies_native_roi_and_clears_opaque_black() {
        use std::future::Future;
        use std::task::{Context, Poll, Wake, Waker};
        struct ThreadWake(std::thread::Thread);
        impl Wake for ThreadWake {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        fn wait<T>(future: impl Future<Output = T>) -> T {
            let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
            let mut context = Context::from_waker(&waker);
            let mut future = std::pin::pin!(future);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match future.as_mut().poll(&mut context) {
                    Poll::Ready(value) => return value,
                    Poll::Pending => {
                        assert!(Instant::now() < deadline);
                        std::thread::park_timeout(Duration::from_millis(5));
                    }
                }
            }
        }
        // Headless Vulkan, no compositor, capture source, DRM import or display surface.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let Ok(adapter) = wait(instance.request_adapter(&wgpu::RequestAdapterOptions {
            force_fallback_adapter: false,
            ..Default::default()
        })) else {
            eprintln!("skipped: headless Vulkan adapter unavailable");
            return;
        };
        let (device, queue) = wait(adapter.request_device(&Default::default())).unwrap();
        let source = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: 8,
                height: 6,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let mut pixels = Vec::new();
        for y in 0..6 {
            for x in 0..8 {
                pixels.extend([x as u8, y as u8, 99, 255]);
            }
        }
        queue.write_texture(
            source.as_image_copy(),
            &pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(32),
                rows_per_image: None,
            },
            source.size(),
        );
        let geometry = TwinGeometry {
            window: WindowId(10),
            display: DisplayId(2),
            origin: (13, 8),
            size: PixelSize::new(4, 3),
            extent: PixelSize::new(100, 100),
            content: PixelRect::new(point2(0, 0), point2(20, 15)),
        };
        let q = geometry.map_crop(geometry.content).unwrap();
        let (wanted, actual) = twin_roi(&geometry, geometry.size, Some(q)).unwrap();
        let offset = (actual.min.x - wanted.min.x, actual.min.y - wanted.min.y);
        let texture = padding_texture(&device, wanted.size().cast());
        pad_twin_texture(
            &device,
            &queue,
            &source,
            (2, 1),
            actual.size().cast(),
            offset,
            &texture,
        );
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: 256 * 15,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: None,
                },
            },
            texture.size(),
        );
        queue.submit([encoder.finish()]);
        let (send, result) = mpsc::sync_channel(1);
        readback.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = send.send(r);
        });
        device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(Duration::from_secs(2)),
            })
            .unwrap();
        result
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        let mapped = readback.slice(..).get_mapped_range().unwrap();
        for y in 0..15 {
            for x in 0..20 {
                let expected = if (13..17).contains(&x) && (8..11).contains(&y) {
                    [x - 13 + 2, y - 8 + 1, 99, 255]
                } else {
                    [0, 0, 0, 255]
                };
                assert_eq!(&mapped[y as usize * 256 + x as usize * 4..][..4], &expected);
            }
        }
        eprintln!("headless Vulkan ROI(2,1) -> R offset(13,8), all four black borders exact");

        #[derive(Debug)]
        struct RetainedPadding {
            texture: wgpu::Texture,
            size: PixelSize,
            free: Arc<AtomicBool>,
            queue: wgpu::Queue,
        }
        impl crosspane_platform::NativeImage for RetainedPadding {
            fn size(&self) -> PixelSize {
                self.size
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
            fn read(&self, _: &mut dyn FnMut(&[u8], u32)) -> Result<(), PlatformError> {
                Err(PlatformError::Unsupported("lease fixture"))
            }
        }
        impl Drop for RetainedPadding {
            fn drop(&mut self) {
                let free = self.free.clone();
                let texture = self.texture.clone();
                // Same consumption + completion lease contract as the production Image::drop.
                self.queue.on_submitted_work_done(move || {
                    drop(texture);
                    free.store(true, Ordering::Release);
                });
            }
        }
        let mut pool = PaddingPool::default();
        let mut retained = Vec::new();
        for _ in 0..4 {
            let (texture, free) = pool.acquire(&device, PixelSize::new(20, 15)).unwrap();
            retained.push(FrameImage::Native(Arc::new(RetainedPadding {
                texture,
                free,
                queue: queue.clone(),
                size: PixelSize::new(20, 15),
            })));
        }
        device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(Duration::from_secs(2)),
            })
            .unwrap();
        for _ in 0..100 {
            assert!(pool.acquire(&device, PixelSize::new(30, 25)).is_none());
        }
        assert_eq!(pool.slots.len(), 4); // Even resize cannot allocate around retained frames.
        drop(retained.pop());
        // Consumption alone does not recycle: the GPU-completion callback still needs polling.
        assert!(pool.acquire(&device, PixelSize::new(30, 25)).is_none());
        device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(Duration::from_secs(2)),
            })
            .unwrap();
        let (texture, free) = pool.acquire(&device, PixelSize::new(30, 25)).unwrap();
        assert_eq!((texture.width(), texture.height()), (30, 25));
        assert!(!free.load(Ordering::Acquire));
        assert_eq!(pool.slots.len(), 4);
    }

    #[test]
    fn cursor_unpremultiply() {
        let cursor = image(
            PixelSize::new(4, 1),
            (0, 0),
            &[32, 64, 128, 128, 17, 23, 99, 255, 200, 20, 0, 0, 2, 1, 0, 1],
        )
        .unwrap();
        assert_eq!(
            cursor.pixels.as_ref(),
            &[
                64, 128, 255, 128, 17, 23, 99, 255, 0, 0, 0, 0, 255, 255, 0, 1
            ]
        );
        let xrgb = cursor_image(
            PixelSize::new(1, 1),
            (0, 0),
            &[17, 23, 99, 0],
            wl_shm::Format::Xrgb8888,
        )
        .unwrap()
        .unwrap();
        assert_eq!(xrgb.pixels.as_ref(), &[17, 23, 99, 255]);
    }

    #[test]
    fn cursor_transparent_frame_uses_default() {
        let stream = StreamId(1);
        for pixels in [[0; 8], [99, 22, 33, 0, 0, 0, 0, 0]] {
            assert!(matches!(
                cursor_event(stream, image(PixelSize::new(2, 1), (0, 0), &pixels)),
                FrameEvent::CursorDefault { stream: id } if id == stream
            ));
        }
        assert!(matches!(
            cursor_event(stream, image(PixelSize::new(2, 1), (0, 0), &[0, 0, 0, 0, 0, 0, 0, 1])),
            FrameEvent::Cursor { stream: id, cursor: Some(_) } if id == stream
        ));
    }

    #[test]
    fn cursor_hotspot_clamp() {
        let pixels = [255; 3 * 2 * 4];
        assert_eq!(
            image(PixelSize::new(3, 2), (-10, i32::MAX), &pixels)
                .unwrap()
                .hotspot,
            (0, 1)
        );
        assert_eq!(
            image(PixelSize::new(3, 2), (2, 0), &pixels)
                .unwrap()
                .hotspot,
            (2, 0)
        );
    }

    #[test]
    fn cursor_downscale_300_by_200() {
        let mut pixels = vec![255; 300 * 200 * 4];
        let offset = (100 * 300 + 150) * 4;
        pixels[offset..offset + 4].copy_from_slice(&[10, 20, 30, 255]);
        let cursor = image(PixelSize::new(300, 200), (150, 100), &pixels).unwrap();
        assert_eq!(cursor.size, PixelSize::new(256, 170));
        assert_eq!(cursor.hotspot, (128, 85));
        assert_eq!(cursor.pixels.len(), 256 * 170 * 4);
        let offset = (85 * 256 + 128) * 4;
        assert_eq!(&cursor.pixels[offset..offset + 4], &[10, 20, 30, 255]);
        assert_eq!(
            image(PixelSize::new(300, 200), (i32::MAX, i32::MAX), &pixels)
                .unwrap()
                .hotspot,
            (255, 169)
        );
    }

    #[test]
    fn cursor_same_as_last_suppression() {
        let mut history = CursorHistory::default();
        let cursor = image(PixelSize::new(2, 1), (0, 0), &[255; 8]);
        assert!(history.changed(&cursor));
        assert!(!history.changed(&cursor));
        let hotspot = image(PixelSize::new(2, 1), (1, 0), &[255; 8]);
        assert!(history.changed(&hotspot));
        assert!(!history.changed(&hotspot));
        let pixels = image(PixelSize::new(2, 1), (1, 0), &[254; 8]);
        assert!(history.changed(&pixels));
        assert!(history.changed(&None));
        assert!(!history.changed(&None));
        assert!(history.changed(&cursor));
        let stream = StreamId(1);
        let mut image_default_image = CursorHistory::default();
        let events: Vec<_> = [cursor.clone(), None, cursor.clone()]
            .into_iter()
            .filter(|image| image_default_image.changed(image))
            .map(|image| cursor_event(stream, image))
            .collect();
        assert!(matches!(
            events.as_slice(),
            [FrameEvent::Cursor { stream: first, cursor: Some(first_image) },
             FrameEvent::CursorDefault { stream: default },
             FrameEvent::Cursor { stream: last, cursor: Some(last_image) }]
                if *first == stream && *default == stream && *last == stream && first_image == last_image
        ));
        let mut default_default = CursorHistory::default();
        let events: Vec<_> = [None, None]
            .into_iter()
            .filter(|image| default_default.changed(image))
            .map(|image| cursor_event(stream, image))
            .collect();
        assert!(matches!(
            events.as_slice(),
            [FrameEvent::CursorDefault { stream: id }] if *id == stream
        ));
    }

    #[test]
    fn damage_translation_into_crop_space() {
        let crop = PixelRect::new(point2(10, 20), point2(30, 40));
        assert_eq!(
            translate_damage(
                &[
                    [0, 0, 15, 25],
                    [25, 35, 20, 20],
                    [30, 20, 5, 5],
                    [10, 40, 5, 5],
                    [15, 25, 0, 5],
                    [15, 25, 5, -1],
                    [i32::MIN, i32::MIN, i32::MAX, i32::MAX],
                    [20, 30, i32::MAX, i32::MAX],
                ],
                crop
            ),
            vec![
                PixelRect::new(point2(0, 0), point2(5, 5)),
                PixelRect::new(point2(15, 15), point2(20, 20)),
                PixelRect::new(point2(10, 10), point2(20, 20)),
            ]
        );
        assert!(translate_damage(&[], crop).is_empty());
    }

    #[test]
    fn format_choice() {
        use wl_shm::Format::*;
        assert_eq!(choose_format(&[Argb8888, Xrgb8888]).unwrap(), Xrgb8888);
        assert_eq!(choose_format(&[Argb8888]).unwrap(), Argb8888);
        assert_eq!(choose_format(&[Xrgb8888]).unwrap(), Xrgb8888);
        assert!(matches!(
            choose_format(&[Abgr8888, Rgb565]),
            Err(PlatformError::Unsupported(_))
        ));
        assert!(matches!(
            choose_format(&[]),
            Err(PlatformError::Unsupported(_))
        ));
    }
}
