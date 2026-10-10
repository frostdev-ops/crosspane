//! The PipeWire side of the ScreenCast capture (WP-G2.2).
//!
//! **Thread and ownership.** One thread (`crosspane-pipewire-capture`) owns the PipeWire main loop,
//! its context, the core connected to the portal's restricted remote, and every capture stream.
//! Nothing PipeWire-related leaves it; the rest of the crate talks to it with [`Command`]s over a
//! channel and gets answers on one-shot channels, each bounded by the caller. The loop is iterated
//! with a short timeout (10 ms while streams run, since the [`IoGate`] has no notification and is
//! polled; 50 ms with none) and the commands are drained after every iteration.
//!
//! **Epochs.** The session worker hands over the portal's PipeWire fd with `Attach { epoch }` and
//! takes it back with `Detach { epoch }`. A `Start` carries the epoch it resolved its node in and
//! is refused when that epoch is not the attached one, so a stream is never made on a session the
//! user has already stopped. A detach (or a lost connection) ends every stream with the reason it
//! was given, before the core is dropped.
//!
//! **Streams.** `Start` makes one `pw_stream` on the core, targeted at the display's node, with
//! the formats of [`format`]. It is answered as soon as the stream is connected; format
//! negotiation and frames follow asynchronously. Every stream has one [`Capture`] behind an
//! `Rc<RefCell<..>>` shared by its callbacks and the loop; callbacks use `try_borrow_mut` and catch
//! panics (a panic must never unwind into PipeWire's C code). A stream that must end (gate closed,
//! error, node gone, no usable format within 10 s) is only marked by its callbacks; the loop ends
//! it after the iteration, so a stream is never destroyed inside its own callback. A node that
//! disappears is noticed through the registry; the stream then ends `TargetGone` when its display
//! is gone from the displays snapshot too, `Failed` otherwise.
//!
//! **Frames.** Each `process` callback takes the newest buffer, returns the older ones at once
//! (keeping their damage), converts the whole buffer to tight BGRA (row stride and chunk offset
//! honoured, alpha forced opaque), and keeps that image as the stream's `cache`. The frame is the
//! cache cut to the crop (clamped to the buffer). It is held in a single slot (newest wins) until
//! the stream's pacer allows a delivery: at most `max_fps` frames per second, on a grid. The cache
//! lets `set_crop` produce a frame for the new crop without waiting for the compositor to repaint
//! (compositors only send buffers on damage). Nothing is read, converted or delivered while the
//! gate is closed; the streams then end with `Blocked`.
//!
//! **Order of destruction.** A stream's listener is dropped before the stream, the streams before
//! the session (registry listener, registry, core listener, core), the session before the context,
//! the context before the main loop. A stream is destroyed before its `Ended` event is sent.
//!
//! Event sinks are called on this thread, from callbacks or from the loop: a sink must not block
//! and must not call back into the capture handle (it would wait for this thread).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crosspane_platform::{
    EventSink, Frame, FrameEvent, IoGate, PlatformError, StreamEndReason, StreamId,
};
use crosspane_types::geom::{PixelRect, PixelSize, euclid::point2};
use crosspane_types::id::DisplayId;
use crosspane_types::time::MonoTime;
use pipewire as pw;
use pw::spa;
use pw::stream::{StreamFlags, StreamState};
use spa::buffer::meta::{MetaHeader, MetaHeaderFlags, MetaRegion, MetaVideoDamage};
use spa::buffer::{ChunkFlags, DataType};
use spa::param::ParamType;
use spa::pod::Pod;

use super::format::{self, Negotiated};
use super::frames::{
    DamageAcc, Pacer, clamp_crop, clip_damage, frame_time, mono_now, translate_damage,
    validate_crop,
};
use super::pixels::{PixelError, crop_bgra, to_bgra_shared};
use super::worker::Shared;
use crate::portal::eis::DisplaysFn;

/// How long the loop waits while streams run and nothing is due: the gate is polled at this rate.
const TICK: Duration = Duration::from_millis(10);
/// How long it waits with no stream: commands and shutdown are noticed at this rate.
const IDLE_TICK: Duration = Duration::from_millis(50);
/// The shortest wait (a zero timeout would spin).
const MIN_WAIT: Duration = Duration::from_millis(1);
/// How long `spawn` waits for the loop to come up.
const READY_TIMEOUT: Duration = Duration::from_millis(1500);
/// How long dropping waits for the thread before detaching it.
const STOP_TIMEOUT: Duration = Duration::from_secs(2);
/// Commands handled per loop iteration.
const COMMANDS_PER_TICK: usize = 16;
/// Consecutive unusable buffers after which a stream is given up.
const MAX_BAD_BUFFERS: u32 = 30;
/// Damage regions read from one buffer.
const MAX_DAMAGE_REGIONS: usize = 64;
/// How long a stream may go without a negotiated format (since it started, or since its format
/// was taken away) before it is given up: the producer cannot give us memory we can read, or the
/// link never came.
const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(10);
const STREAM_NAME: &str = "crosspane-screencast";

fn backend(error: impl std::fmt::Display) -> PlatformError {
    PlatformError::Backend(format!("screencast: {error}"))
}

/// What the rest of the crate asks of the PipeWire thread.
pub(super) enum Command {
    /// A new portal session: connect to its restricted PipeWire remote.
    Attach {
        epoch: u64,
        fd: OwnedFd,
    },
    /// The session of `epoch` is over: end its streams with `reason`, drop the connection.
    Detach {
        epoch: u64,
        reason: StreamEndReason,
    },
    Start(Box<StartRequest>),
    SetCrop {
        stream: StreamId,
        crop: Option<PixelRect>,
        reply: SyncSender<Result<(), PlatformError>>,
    },
    /// End a stream with `Requested`. Unknown streams are fine (idempotent).
    Stop {
        stream: StreamId,
        reply: SyncSender<Result<(), PlatformError>>,
    },
}

/// A request to start a capture stream.
pub(super) struct StartRequest {
    pub id: StreamId,
    /// The epoch the node was resolved in.
    pub epoch: u64,
    pub node_id: u32,
    pub display: DisplayId,
    /// The display's size in device pixels, which crops are expressed in.
    pub device_size: PixelSize,
    pub crop: Option<PixelRect>,
    pub max_fps: u32,
    pub sink: Arc<dyn EventSink<FrameEvent>>,
    /// Set by the caller when it stopped waiting; a request that finds it set is dropped.
    pub cancelled: Arc<AtomicBool>,
    pub reply: SyncSender<Result<(), PlatformError>>,
}

// ---- per-stream state --------------------------------------------------------------------------

/// Why a stream must end, decided in a callback and acted on by the loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::portal) enum Fault {
    Ended(StreamEndReason),
    /// The stream or its node went away on its own; the loop picks `TargetGone` or `Failed`.
    Lost,
}

/// The newest whole buffer as tight BGRA. A frame that shows all of it shares these bytes.
#[derive(Debug)]
struct Cached {
    size: PixelSize,
    pixels: Arc<[u8]>,
}

/// A frame waiting for its delivery slot, with the crop (buffer coordinates) it was cut with.
struct Held {
    frame: Frame,
    rect: PixelRect,
}

impl Cached {
    /// The cache cut to `crop` (clamped to the buffer); `None` when nothing of the crop is inside.
    fn cut(&self, crop: Option<PixelRect>, at: MonoTime) -> Result<Option<Held>, PixelError> {
        let Some(rect) = clamp_crop(self.size, crop) else {
            return Ok(None);
        };
        let (width, height) = (self.size.width, self.size.height);
        let whole = rect.min == point2(0, 0)
            && i64::from(rect.max.x) == i64::from(width)
            && i64::from(rect.max.y) == i64::from(height);
        if whole {
            return Ok(Some(Held {
                frame: Frame::cpu(
                    self.size,
                    width.saturating_mul(4),
                    Arc::clone(&self.pixels),
                    None,
                    at,
                ),
                rect,
            }));
        }
        let cropped = crop_bgra(&self.pixels, width, height, rect)?;
        let out = PixelSize::new(rect.width().unsigned_abs(), rect.height().unsigned_abs());
        Ok(Some(Held {
            frame: Frame::cpu(
                out,
                out.width.saturating_mul(4),
                Arc::<[u8]>::from(cropped),
                None,
                at,
            ),
            rect,
        }))
    }
}

/// Why a buffer gave no frame.
pub(in crate::portal) enum BufferFault {
    /// Nothing new in it (no data, or the producer marked it corrupt/empty): skipped silently.
    Empty,
    /// Not usable: counted, and the stream is given up after too many in a row.
    Bad(&'static str),
}

fn pixel_error_text(error: PixelError) -> &'static str {
    match error {
        PixelError::ZeroSize => "empty image",
        PixelError::TooLarge => "image too large",
        PixelError::BadStride => "bad row stride",
        PixelError::ShortBuffer => "buffer shorter than its image",
        PixelError::BadRect => "crop outside the image",
        PixelError::Alloc => "out of memory",
    }
}

/// What one buffer says it changed, independent of any capture (a stream shared by several captures
/// reads it once and hands it to each).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::portal) enum BufferDamage {
    /// Nothing usable: the whole image counts as changed.
    Unknown,
    /// These regions, clipped to the image and never empty.
    Regions(Vec<PixelRect>),
}

/// One capture stream's state. Touched by its callbacks and by the loop, never concurrently.
///
/// `portal::virtual_screen` also drives it: one PipeWire stream feeds several of these through
/// [`accept`](Capture::accept) and [`apply_damage`](Capture::apply_damage) and never uses the
/// format callbacks.
pub(in crate::portal) struct Capture {
    id: StreamId,
    sink: Arc<dyn EventSink<FrameEvent>>,
    gate: Arc<IoGate>,
    crop: Option<PixelRect>,
    /// The display's size in device pixels: what a crop is expressed in.
    device_size: PixelSize,
    /// The `Buffers` and `Meta` pods that answer a negotiated format.
    params: Vec<Vec<u8>>,
    negotiated: Option<Negotiated>,
    pacer: Pacer,
    damage: DamageAcc,
    cache: Option<Cached>,
    held: Option<Held>,
    fault: Option<Fault>,
    bad_buffers: u32,
    /// The stream has been connected to its node at some point.
    connected: bool,
    delivered: u64,
    /// Since when there is no negotiated format (`None` while there is one).
    waiting_since: Option<Instant>,
}

impl Capture {
    pub(in crate::portal) fn new(
        id: StreamId,
        sink: Arc<dyn EventSink<FrameEvent>>,
        gate: Arc<IoGate>,
        crop: Option<PixelRect>,
        device_size: PixelSize,
        max_fps: u32,
        params: Vec<Vec<u8>>,
    ) -> Capture {
        Capture {
            id,
            sink,
            gate,
            crop,
            device_size,
            params,
            negotiated: None,
            pacer: Pacer::new(max_fps),
            damage: DamageAcc::new(),
            cache: None,
            held: None,
            fault: None,
            bad_buffers: 0,
            connected: false,
            delivered: 0,
            waiting_since: Some(Instant::now()),
        }
    }

    pub(in crate::portal) fn mark(&mut self, fault: Fault) {
        self.fault.get_or_insert(fault);
    }

    /// The fault the capture has marked, if any (cleared).
    pub(in crate::portal) fn take_fault(&mut self) -> Option<Fault> {
        self.fault.take()
    }

    /// Where this capture's events go.
    pub(in crate::portal) fn sink(&self) -> &Arc<dyn EventSink<FrameEvent>> {
        &self.sink
    }

    /// Forget every pixel and what changed since: the next frame is whole.
    pub(in crate::portal) fn forget_pixels(&mut self) {
        self.cache = None;
        self.held = None;
        self.damage.add_unknown();
    }

    fn on_state(&mut self, new: &StreamState) {
        match new {
            StreamState::Error(message) => {
                tracing::warn!(stream = self.id.0, %message, "the PipeWire capture stream failed");
                self.mark(Fault::Ended(StreamEndReason::Failed));
            }
            StreamState::Unconnected if self.connected => self.mark(Fault::Lost),
            StreamState::Paused | StreamState::Streaming => self.connected = true,
            StreamState::Unconnected | StreamState::Connecting => {}
        }
    }

    /// The stream's format changed. Returns the parameters to answer with, if the format is usable.
    fn on_format(&mut self, pod: Option<&Pod>) -> Option<Vec<Vec<u8>>> {
        let Some(pod) = pod else {
            // The format was cleared (the stream is stopping or renegotiating).
            self.negotiated = None;
            self.waiting_since.get_or_insert_with(Instant::now);
            self.forget_pixels();
            return None;
        };
        match format::parse_negotiated(pod) {
            Ok(negotiated) => {
                if self.negotiated != Some(negotiated) {
                    self.forget_pixels();
                    tracing::debug!(
                        stream = self.id.0,
                        format = ?negotiated.format,
                        width = negotiated.size.width,
                        height = negotiated.size.height,
                        "the capture stream negotiated its format"
                    );
                    if negotiated.size != self.device_size {
                        // Crops are applied in stream pixels; a compositor that streams a scaled
                        // monitor at its logical size would make them land elsewhere.
                        tracing::warn!(
                            stream = self.id.0,
                            stream_width = negotiated.size.width,
                            stream_height = negotiated.size.height,
                            display_width = self.device_size.width,
                            display_height = self.device_size.height,
                            "the stream's pixel size differs from the display's"
                        );
                    }
                }
                self.negotiated = Some(negotiated);
                self.waiting_since = None;
                Some(self.params.clone())
            }
            Err(error) => {
                tracing::warn!(stream = self.id.0, ?error, "unusable ScreenCast format");
                self.negotiated = None;
                self.mark(Fault::Ended(StreamEndReason::Failed));
                None
            }
        }
    }

    /// Give the stream up when it has had no format for too long.
    fn check_negotiation(&mut self, now: Instant) {
        let Some(since) = self.waiting_since else {
            return;
        };
        if now.saturating_duration_since(since) > NEGOTIATION_TIMEOUT {
            tracing::warn!(
                stream = self.id.0,
                "no usable format was negotiated; the producer may offer only memory we cannot read"
            );
            self.waiting_since = None;
            self.mark(Fault::Ended(StreamEndReason::Failed));
        }
    }

    fn on_process(&mut self, stream: &pw::stream::Stream) {
        if !self.gate.is_open() {
            self.mark(Fault::Ended(StreamEndReason::Blocked));
        }
        let size = self.negotiated.map(|negotiated| negotiated.size);
        let reading = self.fault.is_none() && size.is_some();
        // The newest buffer wins; an older one goes back to the producer when it is replaced, but
        // what it changed still counts.
        let mut newest = None;
        while let Some(buffer) = stream.dequeue_buffer() {
            if reading && let Some(size) = size {
                self.note_damage(&buffer, size);
            }
            newest = Some(buffer);
        }
        if let (true, Some(mut buffer), Some(negotiated)) = (reading, newest, self.negotiated) {
            self.consume(&mut buffer, negotiated);
        }
    }

    /// Add what `buffer` says it changed to the damage the next frame reports.
    fn note_damage(&mut self, buffer: &pw::buffer::Buffer<'_>, size: PixelSize) {
        self.apply_damage(&buffer_damage(buffer, size));
    }

    /// Add `damage` to what the next frame reports.
    pub(in crate::portal) fn apply_damage(&mut self, damage: &BufferDamage) {
        match damage {
            BufferDamage::Unknown => self.damage.add_unknown(),
            BufferDamage::Regions(rects) => self.damage.add(rects),
        }
    }

    /// Turn the newest buffer into the held frame and deliver it if its slot has come.
    fn consume(&mut self, buffer: &mut pw::buffer::Buffer<'_>, negotiated: Negotiated) {
        let header = buffer
            .find_meta::<MetaHeader>()
            .map(|header| (header.pts(), header.flags()));
        if header.is_some_and(|(_, flags)| flags.contains(MetaHeaderFlags::CORRUPTED)) {
            return;
        }
        let full = match read_image(buffer, negotiated) {
            Ok(full) => full,
            Err(BufferFault::Empty) => return,
            Err(BufferFault::Bad(why)) => {
                self.bad_buffer(why);
                return;
            }
        };
        let at = frame_time(header.map(|(pts, _)| pts), mono_now());
        self.accept(full, negotiated.size, at, Instant::now());
    }

    /// `full` (tight BGRA of `size`) is the stream's newest image.
    pub(in crate::portal) fn accept(
        &mut self,
        full: impl Into<Arc<[u8]>>,
        size: PixelSize,
        at: MonoTime,
        now: Instant,
    ) {
        self.bad_buffers = 0;
        self.cache = Some(Cached {
            size,
            pixels: full.into(),
        });
        self.rebuild(at);
        self.deliver_due(now);
    }

    /// The held frame from the cache and the current crop.
    fn rebuild(&mut self, at: MonoTime) {
        let Some(cache) = self.cache.as_ref() else {
            return;
        };
        match cache.cut(self.crop, at) {
            Ok(held) => self.held = held,
            Err(error) => {
                self.cache = None;
                self.held = None;
                self.bad_buffer(pixel_error_text(error));
            }
        }
    }

    pub(in crate::portal) fn bad_buffer(&mut self, why: &'static str) {
        self.bad_buffers = self.bad_buffers.saturating_add(1);
        if self.bad_buffers == 1 || self.bad_buffers.is_multiple_of(100) {
            tracing::warn!(
                stream = self.id.0,
                why,
                count = self.bad_buffers,
                "unusable ScreenCast buffer"
            );
        }
        if self.bad_buffers >= MAX_BAD_BUFFERS {
            self.mark(Fault::Ended(StreamEndReason::Failed));
        }
    }

    /// Send the held frame if the gate is open and the pacer allows it. A closed gate drops it
    /// and ends the stream.
    pub(in crate::portal) fn deliver_due(&mut self, now: Instant) {
        if self.held.is_none() {
            return;
        }
        if !self.gate.is_open() {
            self.held = None;
            self.cache = None;
            self.mark(Fault::Ended(StreamEndReason::Blocked));
            return;
        }
        if !self.pacer.ready(now) {
            return;
        }
        let Some(held) = self.held.take() else {
            return;
        };
        let mut frame = held.frame;
        frame.damage = self
            .damage
            .take()
            .map(|damage| translate_damage(&damage, held.rect));
        self.pacer.delivered(now);
        self.delivered += 1;
        if self.delivered == 1 {
            tracing::debug!(
                stream = self.id.0,
                width = frame.size.width,
                height = frame.size.height,
                "first ScreenCast frame"
            );
        }
        emit(
            &self.sink,
            FrameEvent::Frame {
                stream: self.id,
                frame,
            },
        );
    }

    /// A new crop: the next frame is whole, and the cache gives one at once.
    pub(in crate::portal) fn set_crop(&mut self, crop: Option<PixelRect>, now: Instant) {
        self.crop = crop;
        self.held = None;
        self.damage.add_unknown();
        self.rebuild(mono_now());
        self.deliver_due(now);
    }

    /// When the loop should look at this stream again (a frame is waiting for its slot).
    pub(in crate::portal) fn wake_at(&self, now: Instant) -> Option<Instant> {
        self.held
            .as_ref()
            .map(|_| self.pacer.deadline().unwrap_or(now))
    }
}

/// What `buffer` says it changed within an image of `size`. A discontinuity, missing or unusable
/// metadata, or no region left after clipping is `Unknown`, never "unchanged".
pub(in crate::portal) fn buffer_damage(
    buffer: &pw::buffer::Buffer<'_>,
    size: PixelSize,
) -> BufferDamage {
    if let Some(header) = buffer.find_meta::<MetaHeader>()
        && header.flags().contains(MetaHeaderFlags::DISCONT)
    {
        return BufferDamage::Unknown;
    }
    let Some(meta) = buffer.find_meta::<MetaVideoDamage>() else {
        return BufferDamage::Unknown;
    };
    let (Ok(width), Ok(height)) = (i32::try_from(size.width), i32::try_from(size.height)) else {
        return BufferDamage::Unknown;
    };
    let rects: Vec<PixelRect> = meta
        .iter()
        .take(MAX_DAMAGE_REGIONS)
        .filter_map(region_rect)
        .collect();
    let bounds = PixelRect::new(point2(0, 0), point2(width, height));
    let clipped = clip_damage(&rects, bounds);
    if clipped.is_empty() {
        BufferDamage::Unknown
    } else {
        BufferDamage::Regions(clipped)
    }
}

/// A damage region as a rectangle; `None` for one that does not fit.
fn region_rect(region: &MetaRegion) -> Option<PixelRect> {
    let (position, size) = (region.position(), region.size());
    let width = i32::try_from(size.width).ok()?;
    let height = i32::try_from(size.height).ok()?;
    Some(PixelRect::new(
        point2(position.x, position.y),
        point2(
            position.x.checked_add(width)?,
            position.y.checked_add(height)?,
        ),
    ))
}

/// The buffer's pixels as tight BGRA, honouring the chunk's offset and the row stride.
pub(in crate::portal) fn read_image(
    buffer: &mut pw::buffer::Buffer<'_>,
    negotiated: Negotiated,
) -> Result<Arc<[u8]>, BufferFault> {
    let [data] = buffer.datas_mut() else {
        return Err(BufferFault::Bad("unexpected number of planes"));
    };
    let kind = data.type_();
    if kind != DataType::MemFd && kind != DataType::MemPtr {
        return Err(BufferFault::Bad("buffer is not CPU memory"));
    }
    let (offset, size, stride, corrupted) = {
        let chunk = data.chunk();
        (
            chunk.offset() as usize,
            chunk.size() as usize,
            chunk.stride(),
            chunk.flags().contains(ChunkFlags::CORRUPTED),
        )
    };
    if corrupted || size == 0 {
        return Err(BufferFault::Empty);
    }
    // A producer that leaves the stride at 0 means tightly packed rows.
    let stride = match stride {
        0 => negotiated.size.width as usize * 4,
        1.. => stride as usize,
        _ => return Err(BufferFault::Bad("bottom-up rows are not supported")),
    };
    let bytes = data
        .data()
        .ok_or(BufferFault::Bad("buffer is not mapped"))?;
    let end = offset
        .checked_add(size)
        .ok_or(BufferFault::Bad("chunk outside the buffer"))?
        .min(bytes.len());
    let plane = bytes
        .get(offset..end)
        .ok_or(BufferFault::Bad("chunk outside the buffer"))?;
    to_bgra_shared(
        negotiated.format,
        plane,
        stride,
        negotiated.size.width,
        negotiated.size.height,
    )
    .map_err(|error| BufferFault::Bad(pixel_error_text(error)))
}

/// Run `f` on a stream's state from a PipeWire callback: skipped if the state is busy, and a panic
/// is caught (it must not unwind into C) and fails the stream.
fn with<R>(capture: &Rc<RefCell<Capture>>, f: impl FnOnce(&mut Capture) -> R) -> Option<R> {
    let mut capture = capture.try_borrow_mut().ok()?;
    match catch_unwind(AssertUnwindSafe(|| f(&mut capture))) {
        Ok(value) => Some(value),
        Err(_) => {
            tracing::error!(stream = capture.id.0, "a PipeWire stream callback panicked");
            capture.mark(Fault::Ended(StreamEndReason::Failed));
            None
        }
    }
}

pub(in crate::portal) fn emit(sink: &Arc<dyn EventSink<FrameEvent>>, event: FrameEvent) {
    if catch_unwind(AssertUnwindSafe(|| sink.send(event))).is_err() {
        tracing::warn!("frame event sink panicked");
    }
}

/// Answer a negotiated format with the buffer and metadata parameters.
pub(in crate::portal) fn send_params(
    stream: &pw::stream::Stream,
    params: &[Vec<u8>],
) -> Result<(), ()> {
    let mut pods: Vec<&Pod> = Vec::with_capacity(params.len());
    for bytes in params {
        pods.push(Pod::from_bytes(bytes).ok_or(())?);
    }
    stream.update_params(&mut pods).map_err(|_| ())
}

// ---- the loop ----------------------------------------------------------------------------------

/// A stream on the loop. The listener goes before the stream.
struct Endpoint {
    _listener: pw::stream::StreamListener<Rc<RefCell<Capture>>>,
    _stream: pw::stream::StreamRc,
    capture: Rc<RefCell<Capture>>,
    display: DisplayId,
    node_id: u32,
}

/// The connection to one portal session. Listeners before what they listen to.
struct Session {
    epoch: u64,
    /// Node ids the registry reported removed.
    removed: Rc<RefCell<Vec<u32>>>,
    /// The core reported its connection broken.
    failed: Rc<Cell<bool>>,
    _registry_listener: Option<pw::registry::Listener>,
    _registry: Option<pw::registry::RegistryRc>,
    _core_listener: pw::core::Listener,
    core: pw::core::CoreRc,
}

/// Everything the thread owns besides the main loop.
struct Engine {
    streams: HashMap<StreamId, Endpoint>,
    session: Option<Session>,
    context: pw::context::ContextRc,
    gate: Arc<IoGate>,
    displays: DisplaysFn,
    shared: Arc<Shared>,
}

impl Engine {
    /// How long the loop may sleep: until the gate is next polled or a frame is due.
    fn wait(&self, now: Instant) -> Duration {
        if self.streams.is_empty() {
            // Nothing to watch: only a command (or shutdown) needs the loop's attention.
            return IDLE_TICK;
        }
        let mut wait = TICK;
        for endpoint in self.streams.values() {
            let due = endpoint
                .capture
                .try_borrow()
                .ok()
                .and_then(|capture| capture.wake_at(now));
            if let Some(due) = due {
                wait = wait.min(due.saturating_duration_since(now));
            }
        }
        wait.max(MIN_WAIT)
    }

    /// Handle commands and the state callbacks left behind. `false`: every sender is gone.
    fn pump(&mut self, receiver: &Receiver<Command>) -> bool {
        for _ in 0..COMMANDS_PER_TICK {
            match receiver.try_recv() {
                Ok(command) => self.command(command),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return false,
            }
        }
        self.check_session();
        self.check_streams(Instant::now());
        true
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Attach { epoch, fd } => self.attach(epoch, fd),
            Command::Detach { epoch, reason } => {
                if self.session.as_ref().is_some_and(|s| s.epoch == epoch) {
                    self.end_session(reason);
                }
            }
            Command::Start(request) => self.start(*request),
            Command::SetCrop {
                stream,
                crop,
                reply,
            } => {
                let _ = reply.send(self.set_crop(stream, crop));
            }
            Command::Stop { stream, reply } => {
                self.end(stream, StreamEndReason::Requested);
                let _ = reply.send(Ok(()));
            }
        }
    }

    fn attach(&mut self, epoch: u64, fd: OwnedFd) {
        // An earlier epoch that was never detached ends here.
        self.end_session(StreamEndReason::Failed);
        let core = match self.context.connect_fd_rc(fd, None) {
            Ok(core) => core,
            Err(error) => {
                tracing::warn!(%error, epoch, "cannot connect to the portal's PipeWire remote");
                self.shared.core_lost(epoch);
                return;
            }
        };
        let failed = Rc::new(Cell::new(false));
        let core_listener = {
            let failed = Rc::clone(&failed);
            core.add_listener_local()
                .error(move |id, _seq, res, message| {
                    // Errors on the core object itself mean the connection is broken; errors on a
                    // proxy (a node we may not touch) are the stream's business.
                    if id == pw::sys::PW_ID_CORE {
                        tracing::warn!(res, message, "the PipeWire connection broke");
                        failed.set(true);
                    } else {
                        tracing::debug!(id, res, message, "PipeWire object error");
                    }
                })
                .register()
        };
        let removed = Rc::new(RefCell::new(Vec::new()));
        let (registry, registry_listener) = match core.get_registry_rc() {
            Ok(registry) => {
                let removed = Rc::clone(&removed);
                let listener = registry
                    .add_listener_local()
                    .global_remove(move |id| removed.borrow_mut().push(id))
                    .register();
                (Some(registry), Some(listener))
            }
            Err(error) => {
                tracing::debug!(%error, "no PipeWire registry; node removal goes unnoticed");
                (None, None)
            }
        };
        tracing::info!(epoch, "connected to the portal's PipeWire remote");
        self.session = Some(Session {
            epoch,
            removed,
            failed,
            _registry_listener: registry_listener,
            _registry: registry,
            _core_listener: core_listener,
            core,
        });
    }

    fn start(&mut self, request: StartRequest) {
        if request.cancelled.load(Ordering::Acquire) {
            return;
        }
        if let Err(error) = self.open(&request) {
            let _ = request.reply.send(Err(error));
            return;
        }
        if request.reply.send(Ok(())).is_err() {
            // The caller gave up and nobody knows the stream id: drop it quietly.
            self.streams.remove(&request.id);
        }
    }

    /// Make the stream and connect it to its node. Answers before any format is negotiated.
    fn open(&mut self, request: &StartRequest) -> Result<(), PlatformError> {
        if !self.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        let session = self
            .session
            .as_ref()
            .filter(|session| session.epoch == request.epoch)
            .ok_or(PlatformError::NotFound)?;
        if session.failed.get() {
            return Err(backend("the PipeWire connection was lost"));
        }
        validate_crop(request.crop)?;
        let enum_format = format::enum_format(request.max_fps).map_err(backend_format)?;
        let mut params = vec![format::buffers_param().map_err(backend_format)?];
        params.extend(format::meta_params().map_err(backend_format)?);
        let capture = Rc::new(RefCell::new(Capture::new(
            request.id,
            Arc::clone(&request.sink),
            Arc::clone(&self.gate),
            request.crop,
            request.device_size,
            request.max_fps,
            params,
        )));
        let stream = pw::stream::StreamRc::new(
            session.core.clone(),
            STREAM_NAME,
            pw::properties::properties! {
                *pw::keys::NODE_NAME => STREAM_NAME,
                *pw::keys::MEDIA_TYPE => "Video",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Screen",
            },
        )
        .map_err(backend)?;
        let listener = stream
            .add_local_listener_with_user_data(Rc::clone(&capture))
            .state_changed(|_, data, _old, new| {
                with(data, |capture| capture.on_state(&new));
            })
            .param_changed(|stream, data, id, pod| {
                if id != ParamType::Format.as_raw() {
                    return;
                }
                let Some(params) = with(data, |capture| capture.on_format(pod)).flatten() else {
                    return;
                };
                // Outside the state's borrow: PipeWire may call back while the params are set.
                if send_params(stream, &params).is_err() {
                    with(data, |capture| {
                        capture.mark(Fault::Ended(StreamEndReason::Failed));
                    });
                }
            })
            .process(|stream, data| {
                with(data, |capture| capture.on_process(stream));
            })
            .register()
            .map_err(backend)?;
        let pod = Pod::from_bytes(&enum_format).ok_or_else(|| backend("format pod"))?;
        stream
            .connect(
                spa::utils::Direction::Input,
                Some(request.node_id),
                StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::DONT_RECONNECT,
                &mut [pod],
            )
            .map_err(backend)?;
        tracing::info!(
            stream = request.id.0,
            node = request.node_id,
            max_fps = request.max_fps,
            "capture stream started"
        );
        self.streams.insert(
            request.id,
            Endpoint {
                _listener: listener,
                _stream: stream,
                capture,
                display: request.display,
                node_id: request.node_id,
            },
        );
        Ok(())
    }

    fn set_crop(&mut self, stream: StreamId, crop: Option<PixelRect>) -> Result<(), PlatformError> {
        validate_crop(crop)?;
        let endpoint = self.streams.get(&stream).ok_or(PlatformError::NotFound)?;
        with(&endpoint.capture, |capture| {
            capture.set_crop(crop, Instant::now());
        });
        Ok(())
    }

    /// End one stream: destroy it, then tell its sink.
    fn end(&mut self, id: StreamId, reason: StreamEndReason) {
        let Some(endpoint) = self.streams.remove(&id) else {
            return;
        };
        let sink = Arc::clone(&endpoint.capture.borrow().sink);
        drop(endpoint);
        // A stream that dies while the gate is closed died of the gate, whatever else went wrong.
        let reason = if reason != StreamEndReason::Requested && !self.gate.is_open() {
            StreamEndReason::Blocked
        } else {
            reason
        };
        emit(&sink, FrameEvent::Ended { stream: id, reason });
    }

    fn end_all(&mut self, reason: StreamEndReason) {
        let ids: Vec<StreamId> = self.streams.keys().copied().collect();
        for id in ids {
            self.end(id, reason);
        }
    }

    /// End every stream with `reason` and drop the connection.
    fn end_session(&mut self, reason: StreamEndReason) {
        self.end_all(reason);
        if let Some(session) = self.session.take() {
            tracing::info!(
                epoch = session.epoch,
                "disconnected from the PipeWire remote"
            );
        }
    }

    /// A broken connection ends the epoch's streams and tells the session worker.
    fn check_session(&mut self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        if !session.failed.get() {
            return;
        }
        let epoch = session.epoch;
        self.end_session(StreamEndReason::Failed);
        self.shared.core_lost(epoch);
    }

    fn check_streams(&mut self, now: Instant) {
        if !self.gate.is_open() {
            self.end_all(StreamEndReason::Blocked);
            return;
        }
        if let Some(session) = self.session.as_ref() {
            let removed: Vec<u32> = session.removed.borrow_mut().drain(..).collect();
            for node in removed {
                for endpoint in self.streams.values().filter(|e| e.node_id == node) {
                    with(&endpoint.capture, |capture| capture.mark(Fault::Lost));
                }
            }
        }
        let mut ending = Vec::new();
        for (&id, endpoint) in &self.streams {
            let fault = with(&endpoint.capture, |capture| {
                capture.deliver_due(now);
                capture.check_negotiation(now);
                capture.fault.take()
            })
            .flatten();
            if let Some(fault) = fault {
                ending.push((id, endpoint.display, fault));
            }
        }
        for (id, display, fault) in ending {
            let reason = match fault {
                Fault::Ended(reason) => reason,
                Fault::Lost => self.loss_reason(display),
            };
            self.end(id, reason);
        }
    }

    /// A stream that went away on its own: the display is gone (`TargetGone`) or it broke.
    fn loss_reason(&self, display: DisplayId) -> StreamEndReason {
        if (self.displays)().iter().any(|d| d.id == display) {
            StreamEndReason::Failed
        } else {
            StreamEndReason::TargetGone
        }
    }
}

fn backend_format(error: format::FormatError) -> PlatformError {
    backend(format!("cannot build the stream parameters ({error:?})"))
}

// ---- the thread --------------------------------------------------------------------------------

/// The PipeWire thread. Dropping it stops the thread (bounded) and ends any stream still running.
pub(super) struct PipeWireThread {
    shutdown: Arc<AtomicBool>,
    done: Receiver<()>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for PipeWireThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipeWireThread").finish_non_exhaustive()
    }
}

/// Signals that the thread is over, even when it panicked.
struct DoneGuard(SyncSender<()>);

impl Drop for DoneGuard {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}

impl PipeWireThread {
    /// Start the thread and wait (bounded) for its main loop. Streams come with `Command::Start`.
    pub(super) fn spawn(
        receiver: Receiver<Command>,
        gate: Arc<IoGate>,
        displays: DisplaysFn,
        shared: Arc<Shared>,
    ) -> Result<PipeWireThread, PlatformError> {
        let (ready_tx, ready) = mpsc::sync_channel(1);
        let (done_tx, done) = mpsc::sync_channel(1);
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread = {
            let shutdown = Arc::clone(&shutdown);
            thread::Builder::new()
                .name("crosspane-pipewire-capture".to_owned())
                .spawn(move || {
                    let _done = DoneGuard(done_tx);
                    run(&receiver, &shutdown, gate, displays, shared, &ready_tx);
                })
                .map_err(backend)?
        };
        let mut this = PipeWireThread {
            shutdown,
            done,
            thread: Some(thread),
        };
        match ready.recv_timeout(READY_TIMEOUT) {
            Ok(Ok(())) => Ok(this),
            Ok(Err(error)) => {
                this.stop();
                Err(error)
            }
            Err(_) => {
                this.stop();
                Err(PlatformError::Timeout)
            }
        }
    }

    /// Stop the thread and wait for it, bounded. Idempotent.
    pub(super) fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let Some(thread) = self.thread.take() else {
            return;
        };
        if self.done.recv_timeout(STOP_TIMEOUT).is_ok() {
            if thread.join().is_err() {
                tracing::warn!("the PipeWire capture thread panicked");
            }
        } else {
            tracing::warn!("the PipeWire capture thread did not stop in time; detaching it");
        }
    }
}

impl Drop for PipeWireThread {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(
    receiver: &Receiver<Command>,
    shutdown: &AtomicBool,
    gate: Arc<IoGate>,
    displays: DisplaysFn,
    shared: Arc<Shared>,
    ready: &SyncSender<Result<(), PlatformError>>,
) {
    let mainloop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(mainloop) => mainloop,
        Err(error) => {
            let _ = ready.send(Err(backend(error)));
            return;
        }
    };
    let context = match pw::context::ContextRc::new(&mainloop, None) {
        Ok(context) => context,
        Err(error) => {
            let _ = ready.send(Err(backend(error)));
            return;
        }
    };
    let mut engine = Engine {
        streams: HashMap::new(),
        session: None,
        context,
        gate,
        displays,
        shared,
    };
    let _ = ready.send(Ok(()));
    while !shutdown.load(Ordering::Acquire) {
        let wait = engine.wait(Instant::now());
        mainloop.loop_().iterate(pw::loop_::Timeout::Finite(wait));
        if !engine.pump(receiver) {
            break;
        }
    }
    engine.end_session(StreamEndReason::Requested);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosspane_platform::FrameImage;
    use std::sync::Mutex;

    /// Collects what a stream sends.
    #[derive(Default)]
    struct Collector(Mutex<Vec<FrameEvent>>);

    impl EventSink<FrameEvent> for Collector {
        fn send(&self, event: FrameEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    impl Collector {
        fn frames(&self) -> Vec<Frame> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter_map(|event| match event {
                    FrameEvent::Frame { frame, .. } => Some(frame.clone()),
                    _ => None,
                })
                .collect()
        }
    }

    fn open_gate() -> Arc<IoGate> {
        let gate = IoGate::new();
        gate.set_session_permits(true);
        gate.set_engine_permits(true);
        gate
    }

    fn rect(x0: i32, y0: i32, x1: i32, y1: i32) -> PixelRect {
        PixelRect::new(point2(x0, y0), point2(x1, y1))
    }

    fn capture(max_fps: u32, crop: Option<PixelRect>) -> (Capture, Arc<Collector>, Arc<IoGate>) {
        let sink = Arc::new(Collector::default());
        let gate = open_gate();
        let capture = Capture::new(
            StreamId(7),
            sink.clone(),
            Arc::clone(&gate),
            crop,
            size(4, 2),
            max_fps,
            Vec::new(),
        );
        (capture, sink, gate)
    }

    /// A `width` x `height` BGRA image whose pixel (x, y) is [x, y, tag, 255].
    fn image(width: u32, height: u32, tag: u8) -> Vec<u8> {
        let mut pixels = Vec::new();
        for y in 0..height {
            for x in 0..width {
                pixels.extend_from_slice(&[x as u8, y as u8, tag, 255]);
            }
        }
        pixels
    }

    fn size(width: u32, height: u32) -> PixelSize {
        PixelSize::new(width, height)
    }

    fn pixels_of(frame: &Frame) -> Vec<u8> {
        frame.cpu_pixels().unwrap().0.to_vec()
    }

    #[test]
    fn the_first_frame_goes_out_whole_with_unknown_damage() {
        let (mut capture, sink, _gate) = capture(30, None);
        let t0 = Instant::now();
        capture.accept(image(4, 2, 1), size(4, 2), MonoTime::from_nanos(5), t0);
        let frames = sink.frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].size, size(4, 2));
        assert_eq!(frames[0].cpu_pixels().unwrap().1, 16);
        assert_eq!(pixels_of(&frames[0]), image(4, 2, 1));
        assert_eq!(frames[0].damage, None);
        assert_eq!(frames[0].at, MonoTime::from_nanos(5));
        assert!(capture.fault.is_none());
    }

    #[test]
    fn frames_are_cut_to_the_crop_and_damage_moves_into_crop_space() {
        let (mut capture, sink, _gate) = capture(30, Some(rect(1, 0, 3, 2)));
        let t0 = Instant::now();
        capture.accept(image(4, 2, 1), size(4, 2), MonoTime::ZERO, t0);
        let interval = Duration::from_millis(40);
        capture.damage.add(&[rect(0, 0, 2, 1), rect(3, 1, 4, 2)]);
        capture.accept(image(4, 2, 2), size(4, 2), MonoTime::ZERO, t0 + interval);
        let frames = sink.frames();
        assert_eq!(frames.len(), 2);
        // Columns 1 and 2 of both rows.
        let expected: Vec<u8> = [(1, 0), (2, 0), (1, 1), (2, 1)]
            .iter()
            .flat_map(|&(x, y)| [x, y, 2, 255])
            .collect();
        assert_eq!(frames[1].size, size(2, 2));
        assert_eq!(frames[1].cpu_pixels().unwrap().1, 8);
        assert_eq!(pixels_of(&frames[1]), expected);
        // The first rectangle overlaps column 1 of row 0; the second lies outside the crop.
        assert_eq!(frames[1].damage, Some(vec![rect(0, 0, 1, 1)]));
        assert_eq!(frames[0].damage, None);
    }

    #[test]
    fn the_newest_frame_waits_for_its_slot_and_older_ones_are_dropped() {
        let (mut capture, sink, _gate) = capture(10, None);
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        capture.accept(image(4, 2, 1), size(4, 2), MonoTime::ZERO, t0);
        capture.damage.add(&[rect(0, 0, 1, 1)]);
        capture.accept(image(4, 2, 2), size(4, 2), MonoTime::ZERO, t0 + ms(10));
        capture.damage.add(&[rect(2, 1, 4, 2)]);
        capture.accept(image(4, 2, 3), size(4, 2), MonoTime::ZERO, t0 + ms(20));
        assert_eq!(sink.frames().len(), 1, "inside the 100 ms slot");
        assert_eq!(capture.wake_at(t0), Some(t0 + ms(100)));
        capture.deliver_due(t0 + ms(99));
        assert_eq!(sink.frames().len(), 1);
        capture.deliver_due(t0 + ms(100));
        let frames = sink.frames();
        assert_eq!(frames.len(), 2);
        // Only the newest image, with what the dropped one changed too.
        assert_eq!(pixels_of(&frames[1]), image(4, 2, 3));
        assert_eq!(
            frames[1].damage,
            Some(vec![rect(0, 0, 1, 1), rect(2, 1, 4, 2)])
        );
        assert_eq!(capture.wake_at(t0 + ms(100)), None);
        // Nothing held: nothing more to send.
        capture.deliver_due(t0 + ms(500));
        assert_eq!(sink.frames().len(), 2);
    }

    #[test]
    fn a_fast_source_is_held_to_max_fps() {
        let (mut capture, sink, _gate) = capture(30, None);
        let t0 = Instant::now();
        // 100 Hz source for one second, the loop looking every millisecond.
        for step in 0..1000_u64 {
            let now = t0 + Duration::from_millis(step);
            if step % 10 == 0 {
                capture.damage.add(&[rect(0, 0, 4, 2)]);
                capture.accept(image(4, 2, 1), size(4, 2), MonoTime::ZERO, now);
            } else {
                capture.deliver_due(now);
            }
        }
        let count = sink.frames().len();
        assert!((29..=31).contains(&count), "{count} frames in one second");
    }

    #[test]
    fn set_crop_cuts_a_new_frame_from_the_cache() {
        let (mut capture, sink, _gate) = capture(30, None);
        let t0 = Instant::now();
        capture.accept(image(4, 2, 9), size(4, 2), MonoTime::ZERO, t0);
        // The screen is static: no new buffer comes, yet the new crop gets its frame at once.
        capture.set_crop(Some(rect(2, 1, 4, 2)), t0 + Duration::from_secs(1));
        let frames = sink.frames();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1].size, size(2, 1));
        assert_eq!(pixels_of(&frames[1]), vec![2, 1, 9, 255, 3, 1, 9, 255]);
        assert_eq!(
            frames[1].damage, None,
            "a crop change reports unknown damage"
        );
        // Back to the whole image, again without a new buffer.
        capture.set_crop(None, t0 + Duration::from_secs(2));
        let frames = sink.frames();
        assert_eq!(frames.len(), 3);
        assert_eq!(pixels_of(&frames[2]), image(4, 2, 9));
    }

    #[test]
    fn set_crop_inside_a_slot_waits_for_the_slot() {
        let (mut capture, sink, _gate) = capture(10, None);
        let t0 = Instant::now();
        capture.accept(image(4, 2, 9), size(4, 2), MonoTime::ZERO, t0);
        capture.set_crop(Some(rect(0, 0, 2, 2)), t0 + Duration::from_millis(5));
        assert_eq!(sink.frames().len(), 1);
        assert_eq!(capture.wake_at(t0), Some(t0 + Duration::from_millis(100)));
        capture.deliver_due(t0 + Duration::from_millis(100));
        let frames = sink.frames();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1].size, size(2, 2));
    }

    #[test]
    fn a_crop_outside_the_buffer_gives_no_frame_until_it_moves_back() {
        let (mut capture, sink, _gate) = capture(30, Some(rect(10, 10, 20, 20)));
        let t0 = Instant::now();
        capture.accept(image(4, 2, 1), size(4, 2), MonoTime::ZERO, t0);
        assert!(sink.frames().is_empty());
        assert!(capture.fault.is_none(), "a stale crop is never fatal");
        assert!(
            capture.cache.is_some(),
            "the image is kept for the next crop"
        );
        // A crop that sticks out of the buffer is clamped to it.
        capture.set_crop(Some(rect(-5, 1, 2, 9)), t0 + Duration::from_secs(1));
        let frames = sink.frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].size, size(2, 1));
        assert_eq!(pixels_of(&frames[0]), vec![0, 1, 1, 255, 1, 1, 1, 255]);
    }

    #[test]
    fn a_closed_gate_sends_nothing_and_ends_the_stream_blocked() {
        let (mut capture, sink, gate) = capture(30, None);
        let t0 = Instant::now();
        capture.accept(image(4, 2, 1), size(4, 2), MonoTime::ZERO, t0);
        // A frame is waiting for its slot when the gate closes.
        capture.accept(
            image(4, 2, 2),
            size(4, 2),
            MonoTime::ZERO,
            t0 + Duration::from_millis(1),
        );
        assert!(capture.held.is_some());
        gate.set_engine_permits(false);
        capture.deliver_due(t0 + Duration::from_secs(1));
        assert_eq!(sink.frames().len(), 1, "no frame after the gate closed");
        assert!(capture.held.is_none() && capture.cache.is_none());
        assert_eq!(capture.fault, Some(Fault::Ended(StreamEndReason::Blocked)));
        // A new image while the gate is still closed goes nowhere either.
        capture.accept(
            image(4, 2, 3),
            size(4, 2),
            MonoTime::ZERO,
            t0 + Duration::from_secs(2),
        );
        assert_eq!(sink.frames().len(), 1);
    }

    #[test]
    fn a_stream_that_never_gets_a_format_is_given_up() {
        let (mut stuck, _sink, _gate) = capture(30, None);
        let since = stuck.waiting_since.expect("waiting from the start");
        stuck.check_negotiation(since + NEGOTIATION_TIMEOUT);
        assert!(stuck.fault.is_none(), "not yet");
        stuck.check_negotiation(since + NEGOTIATION_TIMEOUT + Duration::from_millis(1));
        assert_eq!(stuck.fault, Some(Fault::Ended(StreamEndReason::Failed)));

        // A stream with a format is left alone however long it takes to get a frame.
        let (mut settled, _sink, _gate) = capture(30, None);
        let since = settled
            .waiting_since
            .take()
            .expect("waiting from the start");
        settled.check_negotiation(since + NEGOTIATION_TIMEOUT * 10);
        assert!(settled.fault.is_none());
    }

    #[test]
    fn a_run_of_unusable_buffers_gives_the_stream_up() {
        let (mut capture, _sink, _gate) = capture(30, None);
        for _ in 0..MAX_BAD_BUFFERS - 1 {
            capture.bad_buffer("test");
        }
        assert!(capture.fault.is_none());
        // One good buffer starts the count over.
        capture.accept(image(2, 2, 1), size(2, 2), MonoTime::ZERO, Instant::now());
        for _ in 0..MAX_BAD_BUFFERS - 1 {
            capture.bad_buffer("test");
        }
        assert!(capture.fault.is_none());
        capture.bad_buffer("test");
        assert_eq!(capture.fault, Some(Fault::Ended(StreamEndReason::Failed)));
    }

    #[test]
    fn buffer_damage_is_applied_as_regions_or_as_unknown() {
        let (mut capture, sink, _gate) = capture(30, None);
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        capture.accept(image(4, 2, 1), size(4, 2), MonoTime::ZERO, t0);
        // Regions add up between two delivered frames.
        capture.apply_damage(&BufferDamage::Regions(vec![rect(0, 0, 1, 1)]));
        capture.apply_damage(&BufferDamage::Regions(vec![rect(2, 1, 4, 2)]));
        capture.accept(image(4, 2, 2), size(4, 2), MonoTime::ZERO, t0 + ms(40));
        // One unknown makes the whole interval unknown, whatever else was reported.
        capture.apply_damage(&BufferDamage::Regions(vec![rect(0, 0, 1, 1)]));
        capture.apply_damage(&BufferDamage::Unknown);
        capture.apply_damage(&BufferDamage::Regions(vec![rect(1, 1, 2, 2)]));
        capture.accept(image(4, 2, 3), size(4, 2), MonoTime::ZERO, t0 + ms(80));
        let frames = sink.frames();
        assert_eq!(frames.len(), 3);
        assert_eq!(
            frames[1].damage,
            Some(vec![rect(0, 0, 1, 1), rect(2, 1, 4, 2)])
        );
        assert_eq!(frames[2].damage, None);
    }

    #[test]
    fn forgetting_pixels_makes_the_next_frame_whole() {
        let (mut capture, sink, _gate) = capture(30, None);
        let t0 = Instant::now();
        capture.accept(image(4, 2, 1), size(4, 2), MonoTime::ZERO, t0);
        capture.damage.add(&[rect(0, 0, 1, 1)]);
        capture.forget_pixels();
        assert!(capture.cache.is_none() && capture.held.is_none());
        capture.accept(
            image(2, 2, 2),
            size(2, 2),
            MonoTime::ZERO,
            t0 + Duration::from_secs(1),
        );
        let frames = sink.frames();
        assert_eq!(frames[1].size, size(2, 2));
        assert_eq!(frames[1].damage, None);
    }

    #[test]
    fn a_whole_image_is_shared_with_its_frame_and_a_cut_is_not() {
        let cache = Cached {
            size: size(4, 2),
            pixels: image(4, 2, 1).into(),
        };
        let at = MonoTime::ZERO;
        let whole = cache.cut(None, at).unwrap().unwrap();
        let again = cache.cut(Some(rect(0, 0, 4, 2)), at).unwrap().unwrap();
        assert!(
            Arc::ptr_eq(&arc_of(&whole.frame), &cache.pixels)
                && Arc::ptr_eq(&arc_of(&again.frame), &cache.pixels),
            "no second copy of the image"
        );
        let part = cache.cut(Some(rect(1, 1, 3, 2)), at).unwrap().unwrap();
        assert_eq!(part.rect, rect(1, 1, 3, 2));
        assert_eq!(pixels_of(&part.frame), vec![1, 1, 1, 255, 2, 1, 1, 255]);
        assert!(cache.cut(Some(rect(4, 0, 9, 9)), at).unwrap().is_none());
    }

    fn arc_of(frame: &Frame) -> Arc<[u8]> {
        match &frame.image {
            FrameImage::Cpu { pixels, .. } => Arc::clone(pixels),
            FrameImage::Native(_) => panic!("not a CPU frame"),
        }
    }

    #[test]
    fn a_sink_that_panics_does_not_take_the_stream_down() {
        struct Boom;
        impl EventSink<FrameEvent> for Boom {
            fn send(&self, _: FrameEvent) {
                panic!("sink panic (expected in this test)");
            }
        }
        let gate = open_gate();
        let mut capture = Capture::new(
            StreamId(1),
            Arc::new(Boom),
            gate,
            None,
            size(2, 2),
            30,
            Vec::new(),
        );
        capture.accept(image(2, 2, 1), size(2, 2), MonoTime::ZERO, Instant::now());
        assert!(capture.fault.is_none());
    }

    #[test]
    fn a_panicking_callback_fails_only_its_stream() {
        let (capture, _sink, _gate) = capture(30, None);
        let shared = Rc::new(RefCell::new(capture));
        let value = with(&shared, |_| -> u32 { panic!("expected in this test") });
        assert_eq!(value, None);
        assert_eq!(
            shared.borrow().fault,
            Some(Fault::Ended(StreamEndReason::Failed))
        );
        // A busy state is skipped, not panicked on.
        let guard = shared.borrow_mut();
        assert_eq!(with(&shared, |_| 1), None);
        drop(guard);
        assert_eq!(with(&shared, |_| 1), Some(1));
    }
}
