//! CPU output capture through ext-image-copy-capture-v1. Native objects belong to one thread.
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
    CaptureTarget, CursorImage, EventSink, Frame, FrameCapture, FrameEvent, IoGate, PlatformError,
    StreamEndReason, StreamId,
};
use crosspane_types::geom::{PixelRect, PixelSize, euclid::point2};
use crosspane_types::id::DisplayId;
use crosspane_types::time::MonoTime;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::ext::image_capture_source::v1::client::{
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

use super::ipc::HyprIpc;

// Leave scheduling margin inside the frozen two-second call bound. Poll even when a capture is
// indefinitely waiting for source damage: IoGate has no notification API.
const CALL_TIMEOUT: Duration = Duration::from_millis(1800);
const GATE_POLL: Duration = Duration::from_millis(10);
const CURSOR_INTERVAL: Duration = Duration::from_nanos(1_000_000_000_u64.div_ceil(30));

/// Bounded command handle for all output capture streams on one Wayland connection.
#[derive(Debug)]
pub struct HyprlandFrameCapture {
    commands: mpsc::Sender<Command>,
    lookups: mpsc::Sender<Lookup>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    next_id: u64,
    gate: Arc<IoGate>,
    cursors: Arc<AtomicBool>,
}

struct Lookup {
    display: DisplayId,
    deadline: Instant,
    reply: mpsc::Sender<Result<String, PlatformError>>,
}

enum Request {
    Start {
        id: StreamId,
        output: String,
        crop: Option<PixelRect>,
        max_fps: u32,
        sink: Arc<dyn EventSink<FrameEvent>>,
    },
    Crop(StreamId, Option<PixelRect>),
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
        let (commands, receiver) = mpsc::channel();
        let (ready, result) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = shutdown.clone();
        let worker_gate = gate.clone();
        let cursors = Arc::new(AtomicBool::new(false));
        let worker_cursors = cursors.clone();
        let thread = std::thread::Builder::new()
            .name("hypr-frames".into())
            .spawn(move || {
                let mut worker = match Worker::new(worker_gate, worker_cursors, &worker_shutdown) {
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
        let (lookups, work) = mpsc::channel::<Lookup>();
        if let Err(error) = std::thread::Builder::new()
            .name("hypr-frame-ids".into())
            .spawn(move || {
                while let Ok(lookup) = work.recv() {
                    let result = if Instant::now() >= lookup.deadline {
                        Err(PlatformError::Timeout)
                    } else {
                        ipc.monitor_ids().and_then(|monitors| {
                            monitors
                                .into_iter()
                                .find(|(_, id)| *id == lookup.display.0)
                                .map(|(name, _)| name)
                                .ok_or(PlatformError::NotFound)
                        })
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
            commands,
            lookups,
            shutdown,
            thread: Some(thread),
            next_id: 1,
            gate,
            cursors,
        })
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
        let CaptureTarget::Display(display) = target else {
            return Err(PlatformError::Unsupported("output capture only"));
        };
        if !self.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        if max_fps == 0 {
            return Err(backend("max_fps must be positive"));
        }
        let deadline = Instant::now() + CALL_TIMEOUT;
        let (reply, result) = mpsc::channel();
        self.lookups
            .send(Lookup {
                display,
                deadline,
                reply,
            })
            .map_err(|_| backend("monitor lookup unavailable"))?;
        let output = receive(&result, deadline)??;
        let id = StreamId(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| backend("stream IDs exhausted"))?;
        self.call(
            Request::Start {
                id,
                output,
                crop,
                max_fps,
                sink,
            },
            deadline,
        )?;
        Ok(id)
    }

    fn set_crop(&mut self, stream: StreamId, crop: Option<PixelRect>) -> Result<(), PlatformError> {
        self.call(Request::Crop(stream, crop), Instant::now() + CALL_TIMEOUT)
    }

    fn stop(&mut self, stream: StreamId) -> Result<(), PlatformError> {
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
    output: u32,
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
        self.buffer.take();
    }
}

struct Output {
    proxy: wl_output::WlOutput,
    name: String,
}

struct State {
    gate: Arc<IoGate>,
    /// Whether new streams open a cursor session.
    cursors: Arc<AtomicBool>,
    manager: Option<ExtImageCopyCaptureManagerV1>,
    sources: Option<ExtOutputImageCaptureSourceManagerV1>,
    shm: Option<wl_shm::WlShm>,
    seat: Option<wl_seat::WlSeat>,
    seat_name: Option<u32>,
    pointer: Option<wl_pointer::WlPointer>,
    outputs: HashMap<u32, Output>,
    streams: HashMap<StreamId, Stream>,
    synced: bool,
}

impl State {
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
                stream.destroy();
                emit(&sink, FrameEvent::Ended { stream: id, reason });
                return;
            }
            stream.destroy();
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
            if let Some(pending) = stream.pending.take() {
                let _ = pending.reply.send(Err(error));
            } else {
                let sink = stream.sink.clone();
                stream.destroy();
                emit(
                    &sink,
                    FrameEvent::Ended {
                        stream: id,
                        reason: StreamEndReason::Failed,
                    },
                );
                return;
            }
            stream.destroy();
        }
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
}

impl Worker {
    fn new(
        gate: Arc<IoGate>,
        cursors: Arc<AtomicBool>,
        shutdown: &AtomicBool,
    ) -> Result<Self, PlatformError> {
        let connection = Connection::connect_to_env().map_err(backend)?;
        let queue = connection.new_event_queue();
        let qh = queue.handle();
        connection.display().get_registry(&qh, ());
        let mut worker = Self {
            connection,
            queue,
            qh,
            state: State {
                gate,
                cursors,
                manager: None,
                sources: None,
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
                    self.state.start_failed(id, error);
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
                        (stream.capture.is_none() && stream.constraints.is_some())
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
                    output,
                    crop,
                    max_fps,
                    sink,
                } => {
                    let result = self.begin(id, &output, crop, max_fps, sink);
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
                Request::Crop(id, crop) => self
                    .state
                    .streams
                    .get_mut(&id)
                    .ok_or(PlatformError::NotFound)
                    .and_then(|stream| {
                        // Validated against the buffer at use (clamped): the output may be
                        // resizing right now, so the current constraints can be stale.
                        if crop.is_some_and(|c| {
                            c.max.x <= c.min.x || c.max.y <= c.min.y || c.min.x < 0 || c.min.y < 0
                        }) {
                            return Err(backend("crop must be nonempty and non-negative"));
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
        name: &str,
        crop: Option<PixelRect>,
        max_fps: u32,
        sink: Arc<dyn EventSink<FrameEvent>>,
    ) -> Result<(), PlatformError> {
        if !self.state.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        let (&output, native) = self
            .state
            .outputs
            .iter()
            .find(|(_, output)| output.name == name)
            .ok_or(PlatformError::NotFound)?;
        let source = self
            .state
            .sources
            .as_ref()
            .ok_or(PlatformError::Unsupported("output capture source required"))?
            .create_source(&native.proxy, &self.qh, ());
        let manager = self
            .state
            .manager
            .as_ref()
            .ok_or(PlatformError::Unsupported("image copy capture required"))?;
        let session = manager.create_session(&source, Options::empty(), &self.qh, id);
        self.state.streams.insert(
            id,
            Stream {
                output,
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
        if stream.pending.is_some() {
            return Ok(());
        }
        if !stream.cursor_started {
            stream.cursor_started = true;
            match (&self.state.manager, &self.state.pointer) {
                _ if !self.state.cursors.load(Ordering::Acquire) => (),
                (Some(manager), Some(pointer)) => {
                    stream.cursor = Some(CursorCapture::new(
                        manager,
                        &stream.source,
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
        if stream.capture.as_ref().is_some_and(|capture| capture.ready) {
            stream.unknown_failures = 0;
            let capture = stream
                .capture
                .take()
                .ok_or_else(|| backend("missing ready frame"))?;
            capture.proxy.destroy();
            // A resize can supersede an already captured buffer. Do not deliver old geometry,
            // especially if set_crop now addresses the new output dimensions.
            if capture.constraints_revision != stream.constraints_revision {
                stream.next_slot = Instant::now();
                return Ok(());
            }
            let buffer = stream
                .buffer
                .as_ref()
                .ok_or_else(|| backend("missing frame buffer"))?;
            let Some(rect) = clamped_crop(buffer.size, stream.crop) else {
                return Ok(());
            };
            let (size, pixels) = buffer.copy(Some(rect))?;
            let at = capture.at.map(Ok).unwrap_or_else(now)?;
            let damage = if stream.full_damage {
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
            emit(
                &stream.sink,
                FrameEvent::Frame {
                    stream: id,
                    frame: Frame {
                        size,
                        stride: size.width * 4,
                        pixels,
                        damage,
                        at,
                    },
                },
            );
            stream.next_slot = Instant::now() + stream.interval;
        }
        if stream.capture.is_some() || Instant::now() < stream.next_slot {
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
            stream.buffer = Some(Buffer::new(shm, &self.qh, constraints)?);
            stream.reallocate = false;
            stream.full_damage = true;
            if let Some(pending) = stream.pending.take()
                && pending.reply.send(Ok(())).is_err()
            {
                self.state.end(id, StreamEndReason::Requested);
                return Ok(());
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
                proxy,
                damage: Vec::new(),
                at: None,
                ready: false,
                constraints_revision: stream.constraints_revision,
            });
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
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "ext_image_copy_capture_manager_v1" => {
                    state.manager = Some(registry.bind(name, 1, qh, ()))
                }
                "ext_output_image_capture_source_manager_v1" => {
                    state.sources = Some(registry.bind(name, 1, qh, ()))
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
                    .filter_map(|(&id, stream)| (stream.output == name).then_some(id))
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
            session_protocol::Event::Done => {
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
                if let Some(capture) = stream.capture.take() {
                    capture.proxy.destroy();
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

    fn image(size: PixelSize, hotspot: (i32, i32), pixels: &[u8]) -> Option<CursorImage> {
        cursor_image(size, hotspot, pixels, wl_shm::Format::Argb8888).unwrap()
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
