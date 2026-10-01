//! ScreenCaptureKit CPU capture. Native control objects belong to a background worker;
//! sample callbacks run on a private serial dispatch queue, never the AppKit main queue.

mod cursor;
mod native;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use crosspane_media::codec::NativeInput;
use crosspane_platform::{
    CaptureTarget, EventSink, FrameCapture, FrameEvent, IoGate, NativeImage, Permission,
    PermissionState, PlatformError, StreamEndReason, StreamId,
};
use crosspane_types::geom::{PixelRect, PixelSize};
use objc2::MainThreadMarker;
use objc2_core_media::CMSampleBuffer;

use crate::permissions;

const TIMEOUT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(20);
const RESIZE_POLL: Duration = Duration::from_millis(250);

/// A captured frame as encoder input: VideoToolbox converts SCK's IOSurface itself. `None` when
/// `image` isn't a frame this crate captured.
pub fn capture_input(image: &Arc<dyn NativeImage>) -> Option<Arc<dyn NativeInput>> {
    image.as_any().downcast_ref::<native::SckImage>()?;
    Some(Arc::new(native::CaptureInput(Arc::clone(image))))
}

/// How many captured buffers are alive now (all streams); for tests and diagnostics.
pub fn held_capture_buffers() -> usize {
    native::HELD_BUFFERS.load(Ordering::Relaxed)
}

pub(crate) use native::{CaptureInput, SckImage};

struct Delivery {
    sink: Arc<dyn EventSink<FrameEvent>>,
    active: bool,
    scale: f64,
    crop: Option<PixelRect>,
    interval: Duration,
    last: Option<Instant>,
    size: Option<PixelSize>,
    full_damage: bool,
    cursor: cursor::StreamCursor,
}

struct Shared {
    gate: Arc<IoGate>,
    // ponytail: one lock serializes copies and events; split per stream if copy throughput matters.
    deliveries: Mutex<BTreeMap<StreamId, Delivery>>,
    commands: mpsc::Sender<Command>,
    cursor_wake: Condvar,
    shutdown: AtomicBool,
}

impl Shared {
    fn permitted(&self) -> bool {
        self.gate.is_open() && screen_recording_granted()
    }

    fn end(&self, id: StreamId, reason: StreamEndReason) -> bool {
        let mut deliveries = self.deliveries.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(delivery) = deliveries.remove(&id) {
            self.cursor_wake.notify_all();
            if delivery.active {
                let reason = if self.permitted() {
                    reason
                } else {
                    StreamEndReason::Blocked
                };
                delivery.sink.send(FrameEvent::Ended { stream: id, reason });
            }
            true
        } else {
            false
        }
    }

    fn end_all(&self, reason: StreamEndReason) {
        let mut deliveries = self.deliveries.lock().unwrap_or_else(|e| e.into_inner());
        for (stream, delivery) in std::mem::take(&mut *deliveries) {
            if delivery.active {
                delivery.sink.send(FrameEvent::Ended { stream, reason });
            }
        }
        self.cursor_wake.notify_all();
    }

    fn frame(&self, id: StreamId, sample: &CMSampleBuffer) {
        if !self.permitted() {
            self.end_all(StreamEndReason::Blocked);
            let _ = self.commands.send(Command::Wake);
            return;
        }
        let mut deliveries = self.deliveries.lock().unwrap_or_else(|e| e.into_inner());
        let Some(delivery) = deliveries.get_mut(&id).filter(|d| d.active) else {
            return;
        };
        let now = Instant::now();
        if delivery
            .last
            .is_some_and(|last| now.duration_since(last) < delivery.interval)
        {
            // Damage is relative to the preceding OS frame, not the preceding delivered frame.
            delivery.full_damage = true;
            return;
        }
        let Some(mut frame) = native::native_sample(sample, delivery.scale, delivery.crop) else {
            delivery.full_damage = true;
            return;
        };
        // A close while retaining the IOSurface must suppress the frame too.
        if !self.permitted() {
            drop(deliveries);
            self.end_all(StreamEndReason::Blocked);
            let _ = self.commands.send(Command::Wake);
            return;
        }
        if delivery.full_damage || delivery.size != Some(frame.size) {
            frame.damage = None;
        }
        delivery.full_damage = false;
        delivery.size = Some(frame.size);
        delivery.last = Some(Instant::now());
        delivery.sink.send(FrameEvent::Frame { stream: id, frame });
    }
}

fn screen_recording_granted() -> bool {
    permissions::state(Permission::ScreenRecording) == PermissionState::Granted
}

fn check_permission() -> Result<(), PlatformError> {
    if screen_recording_granted() {
        Ok(())
    } else {
        Err(PlatformError::PermissionDenied(Permission::ScreenRecording))
    }
}

type Reply = mpsc::Sender<Result<(), PlatformError>>;

enum Command {
    Start {
        id: StreamId,
        target: CaptureTarget,
        crop: Option<PixelRect>,
        fps: u32,
        sink: Arc<dyn EventSink<FrameEvent>>,
        deadline: Instant,
        reply: Reply,
    },
    Crop(StreamId, Option<PixelRect>, Instant, Reply),
    Stop(StreamId, Instant, Reply),
    Cancel(StreamId),
    Failed(StreamId),
    Wake,
    Shutdown,
}

/// A Send command handle. Call the synchronous trait methods off the main thread:
/// ScreenCaptureKit completion waits are bounded, and may not block the AppKit run loop.
pub struct MacFrameCapture {
    shared: Arc<Shared>,
    next: u64,
    cursor_worker: Option<std::thread::JoinHandle<()>>,
}

impl fmt::Debug for MacFrameCapture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MacFrameCapture").finish_non_exhaustive()
    }
}

impl MacFrameCapture {
    /// `PermissionDenied(ScreenRecording)` unless `CGPreflightScreenCaptureAccess()`.
    pub fn new(gate: Arc<IoGate>) -> Result<Self, PlatformError> {
        check_permission()?;
        let (commands, receiver) = mpsc::channel();
        let shared = Arc::new(Shared {
            gate,
            deliveries: Mutex::new(BTreeMap::new()),
            commands,
            cursor_wake: Condvar::new(),
            shutdown: AtomicBool::new(false),
        });
        let worker_shared = shared.clone();
        std::thread::Builder::new()
            .name("mac-frame-capture".into())
            .spawn(move || worker(worker_shared, receiver))
            .map_err(|e| PlatformError::Backend(format!("spawn frame capture worker: {e}")))?;
        let cursor_shared = shared.clone();
        let cursor_worker = std::thread::Builder::new()
            .name("mac-capture-cursor".into())
            .spawn(move || cursor::watch(cursor_shared))
            .map_err(|e| {
                let _ = shared.commands.send(Command::Shutdown);
                PlatformError::Backend(format!("spawn cursor watcher: {e}"))
            })?;
        Ok(Self {
            shared,
            next: 1,
            cursor_worker: Some(cursor_worker),
        })
    }

    fn call(&self, command: impl FnOnce(Instant, Reply) -> Command) -> Result<(), PlatformError> {
        if MainThreadMarker::new().is_some() {
            return Err(PlatformError::Unsupported(
                "call FrameCapture off the main thread",
            ));
        }
        let deadline = Instant::now() + TIMEOUT;
        let (reply, response) = mpsc::channel();
        self.shared
            .commands
            .send(command(deadline, reply))
            .map_err(|_| PlatformError::Backend("frame capture worker stopped".into()))?;
        native::receive(&response, deadline)?
    }
}

impl FrameCapture for MacFrameCapture {
    fn start(
        &mut self,
        target: CaptureTarget,
        crop: Option<PixelRect>,
        max_fps: u32,
        sink: Arc<dyn EventSink<FrameEvent>>,
    ) -> Result<StreamId, PlatformError> {
        check_permission()?;
        if !self.shared.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        if max_fps == 0 || max_fps > i32::MAX as u32 {
            return Err(PlatformError::Backend(
                "max_fps must be in 1..=i32::MAX".into(),
            ));
        }
        let id = StreamId(self.next);
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| PlatformError::Backend("stream IDs exhausted".into()))?;
        let result = self.call(|deadline, reply| Command::Start {
            id,
            target,
            crop,
            fps: max_fps,
            sink,
            deadline,
            reply,
        });
        if result.is_err() {
            // A start completing at the deadline must not leave an unaddressable live stream.
            self.shared.end(id, StreamEndReason::Requested);
            let _ = self.shared.commands.send(Command::Cancel(id));
        }
        result?;
        Ok(id)
    }

    fn set_crop(&mut self, stream: StreamId, crop: Option<PixelRect>) -> Result<(), PlatformError> {
        self.call(|deadline, reply| Command::Crop(stream, crop, deadline, reply))
    }

    fn stop(&mut self, stream: StreamId) -> Result<(), PlatformError> {
        self.call(|deadline, reply| Command::Stop(stream, deadline, reply))
    }
}

impl Drop for MacFrameCapture {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        self.shared.end_all(if self.shared.permitted() {
            StreamEndReason::Requested
        } else {
            StreamEndReason::Blocked
        });
        let _ = self.shared.commands.send(Command::Shutdown);
        if let Some(worker) = self.cursor_worker.take() {
            let _ = worker.join();
        }
    }
}

fn worker(shared: Arc<Shared>, commands: mpsc::Receiver<Command>) {
    let mut streams = BTreeMap::<StreamId, native::Stream>::new();
    let mut poll_at = Instant::now() + RESIZE_POLL;
    loop {
        // Native/autoreleased temporaries never accumulate over the worker's lifetime.
        let keep_running = objc2::rc::autoreleasepool(|_| {
            if !shared.permitted() {
                shared.end_all(StreamEndReason::Blocked);
            }
            // Reap after callbacks end streams, including gate closure and delivery failures.
            let ended: Vec<_> = {
                let deliveries = shared.deliveries.lock().unwrap_or_else(|e| e.into_inner());
                streams
                    .keys()
                    .filter(|id| !deliveries.contains_key(id))
                    .copied()
                    .collect()
            };
            stop_many(&shared, &mut streams, &ended);
            match commands.recv_timeout(POLL) {
                Ok(Command::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => return false,
                Ok(Command::Start {
                    id,
                    target,
                    crop,
                    fps,
                    sink,
                    deadline,
                    reply,
                }) => {
                    let wait = native::Wait::new(&shared, streams.values());
                    let result = (|| {
                        check_permission()?;
                        if !shared.gate.is_open() {
                            return Err(PlatformError::Locked);
                        }
                        let content = native::content(deadline, &wait)?;
                        let mut stream =
                            native::Stream::new(&content.0, target, crop, fps, id, &shared)?;
                        if !shared.permitted() || Instant::now() >= deadline {
                            return Err(if !shared.permitted() {
                                PlatformError::Locked
                            } else {
                                PlatformError::Timeout
                            });
                        }
                        shared
                            .deliveries
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(
                                id,
                                Delivery {
                                    sink,
                                    active: false,
                                    scale: stream.scale,
                                    crop: stream.cpu_crop(),
                                    interval: Duration::from_nanos(
                                        1_000_000_000_u64.div_ceil(u64::from(fps)),
                                    ),
                                    last: None,
                                    size: None,
                                    full_damage: true,
                                    cursor: cursor::StreamCursor::new(target, crop),
                                },
                            );
                        if let Err(error) = stream.start(deadline, &wait) {
                            shared.end(id, StreamEndReason::Failed);
                            let _ = stream.stop_async();
                            return Err(error);
                        }
                        let mut deliveries =
                            shared.deliveries.lock().unwrap_or_else(|e| e.into_inner());
                        if !shared.permitted() || !deliveries.contains_key(&id) {
                            drop(deliveries);
                            shared.end(id, StreamEndReason::Blocked);
                            let _ = stream.stop_async();
                            return Err(PlatformError::Locked);
                        }
                        if let Some(delivery) = deliveries.get_mut(&id) {
                            delivery.active = true;
                        }
                        shared.cursor_wake.notify_all();
                        streams.insert(id, stream);
                        Ok(())
                    })();
                    if reply.send(result).is_err() {
                        shared.end(id, StreamEndReason::Requested);
                        stop_many(&shared, &mut streams, &[id]);
                    }
                }
                Ok(Command::Crop(id, crop, deadline, reply)) => {
                    let wait = native::Wait::new(&shared, streams.values());
                    let result = if !shared.permitted() {
                        Err(PlatformError::Locked)
                    } else if let Some(stream) = streams.get_mut(&id) {
                        stream.set_crop(crop, deadline, &wait)
                    } else {
                        Err(PlatformError::NotFound)
                    };
                    if result.is_ok() {
                        let mut deliveries =
                            shared.deliveries.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(delivery) = deliveries.get_mut(&id)
                            && let Some(stream) = streams.get(&id)
                        {
                            delivery.full_damage = true;
                            delivery.scale = stream.scale;
                            delivery.crop = stream.cpu_crop();
                            delivery.cursor.crop = stream.cursor_crop();
                        }
                    } else if matches!(result, Err(PlatformError::Timeout)) {
                        // The OS may apply a timed-out configuration later. End the stream rather
                        // than deliver frames with geometry the command handle cannot confirm.
                        shared.end(id, StreamEndReason::Failed);
                        stop_many(&shared, &mut streams, &[id]);
                    }
                    let _ = reply.send(result);
                }
                Ok(Command::Stop(id, deadline, reply)) => {
                    let wait = native::Wait::new(&shared, streams.values());
                    let result = if let Some(stream) = streams.remove(&id) {
                        shared.end(id, StreamEndReason::Requested);
                        wait.receive(&stream.stop_async(), deadline, None)
                            .and_then(|result| result)
                    } else {
                        Err(PlatformError::NotFound)
                    };
                    let _ = reply.send(result);
                }
                Ok(Command::Cancel(id)) => {
                    shared.end(id, StreamEndReason::Requested);
                    stop_many(&shared, &mut streams, &[id]);
                }
                Ok(Command::Failed(id)) => {
                    let wait = native::Wait::new(&shared, streams.values());
                    let reason = if !shared.permitted() {
                        StreamEndReason::Blocked
                    } else if let Some(stream) = streams.get(&id) {
                        match native::content(Instant::now() + TIMEOUT, &wait) {
                            Ok(content) if !native::target_exists(&content.0, stream.target) => {
                                StreamEndReason::TargetGone
                            }
                            Err(PlatformError::Locked | PlatformError::PermissionDenied(_)) => {
                                StreamEndReason::Blocked
                            }
                            _ => StreamEndReason::Failed,
                        }
                    } else {
                        StreamEndReason::Failed
                    };
                    shared.end(id, reason);
                    stop_many(&shared, &mut streams, &[id]);
                }
                Ok(Command::Wake) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if !streams.is_empty() && Instant::now() >= poll_at && shared.permitted() {
                poll_at = Instant::now() + RESIZE_POLL;
                let deadline = Instant::now() + TIMEOUT;
                let wait = native::Wait::new(&shared, streams.values());
                if let Ok(content) = native::content(deadline, &wait) {
                    for (&id, stream) in &mut streams {
                        if !shared.permitted() {
                            break;
                        }
                        if !native::target_exists(&content.0, stream.target) {
                            shared.end(id, StreamEndReason::TargetGone);
                        } else if stream.resize(&content.0, deadline, &wait).is_err() {
                            shared.end(id, StreamEndReason::Failed);
                        } else {
                            let mut deliveries =
                                shared.deliveries.lock().unwrap_or_else(|e| e.into_inner());
                            if let Some(delivery) = deliveries.get_mut(&id) {
                                if delivery.scale != stream.scale
                                    || delivery.crop != stream.cpu_crop()
                                {
                                    delivery.full_damage = true;
                                }
                                delivery.scale = stream.scale;
                                delivery.crop = stream.cpu_crop();
                                delivery.cursor.crop = stream.cursor_crop();
                            }
                        }
                    }
                }
            }
            true
        });
        if !keep_running {
            break;
        }
    }
    shared.end_all(StreamEndReason::Requested);
    let ids: Vec<_> = streams.keys().copied().collect();
    stop_many(&shared, &mut streams, &ids);
}

fn stop_many(shared: &Shared, streams: &mut BTreeMap<StreamId, native::Stream>, ids: &[StreamId]) {
    if ids.is_empty() {
        return;
    }
    let wait = native::Wait::new(shared, streams.values());
    let deadline = Instant::now() + TIMEOUT;
    // Submit every stop before waiting for any of them.
    let stopping: Vec<_> = ids
        .iter()
        .filter_map(|id| streams.remove(id))
        .map(|stream| {
            let reply = stream.stop_async();
            (stream, reply)
        })
        .collect();
    for (_stream, reply) in stopping {
        let _ = wait.receive(&reply, deadline, None);
    }
}
