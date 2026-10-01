//! CPU output capture through ext-image-copy-capture-v1. Native objects belong to one thread.

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
    CaptureTarget, EventSink, Frame, FrameCapture, FrameEvent, IoGate, PlatformError,
    StreamEndReason, StreamId,
};
use crosspane_types::geom::{PixelRect, PixelSize, euclid::point2};
use crosspane_types::id::DisplayId;
use crosspane_types::time::MonoTime;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_output, wl_registry, wl_shm, wl_shm_pool,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self as frame_protocol, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{ExtImageCopyCaptureManagerV1, Options},
    ext_image_copy_capture_session_v1::{self as session_protocol, ExtImageCopyCaptureSessionV1},
};

use super::ipc::HyprIpc;

// Leave scheduling margin inside the frozen two-second call bound. Poll even when a capture is
// indefinitely waiting for source damage: IoGate has no notification API.
const CALL_TIMEOUT: Duration = Duration::from_millis(1800);
const GATE_POLL: Duration = Duration::from_millis(10);

/// Bounded command handle for all output capture streams on one Wayland connection.
#[derive(Debug)]
pub struct HyprlandFrameCapture {
    commands: mpsc::Sender<Command>,
    lookups: mpsc::Sender<Lookup>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    next_id: u64,
    gate: Arc<IoGate>,
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
        let thread = std::thread::Builder::new()
            .name("hypr-frames".into())
            .spawn(move || {
                let mut worker = match Worker::new(worker_gate, &worker_shutdown) {
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
        })
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
}

impl Buffer {
    fn new(
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<State>,
        constraints: &Constraints,
    ) -> Result<Self, PlatformError> {
        let size = constraints
            .size
            .ok_or_else(|| backend("missing buffer size"))?;
        let format = choose_format(&constraints.formats)?;
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
}

impl Stream {
    fn destroy(mut self) {
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
    manager: Option<ExtImageCopyCaptureManagerV1>,
    sources: Option<ExtOutputImageCaptureSourceManagerV1>,
    shm: Option<wl_shm::WlShm>,
    outputs: HashMap<u32, Output>,
    streams: HashMap<StreamId, Stream>,
    synced: bool,
}

impl State {
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
    fn new(gate: Arc<IoGate>, shutdown: &AtomicBool) -> Result<Self, PlatformError> {
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
                manager: None,
                sources: None,
                shm: None,
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
            }
            let deadline = self
                .state
                .streams
                .values()
                .filter(|stream| stream.capture.is_none() && stream.constraints.is_some())
                .map(|stream| stream.next_slot)
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
                        if let Some(size) = stream
                            .constraints
                            .as_ref()
                            .and_then(|constraints| constraints.size)
                        {
                            capture_rect(size, crop)?;
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
                constraints_revision: 0,
            },
        );
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
            let rect = capture_rect(buffer.size, stream.crop)?;
            let (size, pixels) = buffer.copy(stream.crop)?;
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
            capture_rect(
                constraints
                    .size
                    .ok_or_else(|| backend("missing buffer size"))?,
                stream.crop,
            )?;
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
delegate_noop!(State: ignore ExtImageCopyCaptureManagerV1);
delegate_noop!(State: ignore ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(State: ignore ExtImageCaptureSourceV1);

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

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
