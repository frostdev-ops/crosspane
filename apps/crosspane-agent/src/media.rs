//! The E2 data plane (docs/wp/E2-v0.md). Pixels never pass through the engine:
//!
//! - **Source:** capture frames → one encoder thread per node → `Transport::send_media`. A frame
//!   the transport refuses (`Congested`) is dropped and the next one becomes a key frame, because a
//!   lost delta would corrupt the receiver's canvas.
//!   - Hybrid encoding (03 §7.1, WP-2.14): still content goes as lossless tiles; while a large
//!     part of the window keeps changing the projection switches to H.264 (when this node has an
//!     encoder and the peer advertised `h264`), and one lossless key frame follows when the motion
//!     stops, so the destination is bit-exact again. Captures arrive only on damage, so the thread
//!     also wakes on a timer to send that last key frame when frames simply stop.
//!   - Region video (WP-2.32): when the peer advertised `h264roi`, only the moving rectangle of
//!     the window goes as video and everything else stays lossless; its tiles are refreshed
//!     losslessly as soon as motion leaves them.
//!   - GPU paths (docs/wp/GPU-v0.md): a frame that is in GPU memory (DMA-BUF capture on Linux, an
//!     SCK IOSurface on the Mac) is hashed on the source GPU. Linux gathers only the tiles that go
//!     out and writes NV12 straight into NVENC's memory; the Mac reads changed tiles in place and
//!     hands the captured buffer to VideoToolbox. Any GPU failure falls back to the CPU path.
//!   - Cursor shapes (03 §4.6, WP-2.16) go as codec-2 frames numbered on their own, when the peer
//!     advertised `cursor`; the last one is sent again with every key frame request.
//!   - Captures (WP-2.46e2): each has an identity of its own and a gate, and everything it
//!     delivers is keyed by that identity, never by the platform's stream number, which a backend
//!     may hand out again. Once its gate is closed nothing of it is queued or sent. Pictures and
//!     cursor shapes are numbered per projection, so a replacement capture continues where the
//!     one it replaces stopped; the numbers never wrap, and go when this node's projection ends.
//! - **Destination:** media frames → a decoder thread → the proxy host. Frames are applied in
//!   `seq` order (streams can complete out of order); a gap that doesn't fill within 300 ms, or a
//!   frame that fails to apply, asks the source for a key frame through the engine.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::rc::Rc;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crosspane_engine::{Input, ProjectionKey};
use crosspane_media::codec::{EncodedVideo, VideoCodecs, VideoDecoder, VideoEncoder};
use crosspane_media::hybrid::{
    FramePlan, HybridConfig, HybridScheduler, RegionConfig, RegionPlan, RegionScheduler, TileRect,
};
use crosspane_media::picture::{Decoded, NativePicture, Nv12, nv12_to_bgra};
use crosspane_media::tiles::{EncodeStats, TileDecoder, TileEncoder, TilePixels, TileScan};
use crosspane_media::wire::{
    Codec, FrameHeader, MediaError, TILE, VideoRegion, read_codec, read_cursor, read_header,
    read_video_region, write_cursor, write_default_cursor, write_video, write_video_region,
};
use crosspane_platform::{CursorImage, Frame};
use crosspane_protocol::link::LinkError;
use crosspane_render::proxy::{HostCommand, HostHandle};
use crosspane_render::source::{FrameRegion, SourceGpu, TileChanges};
use crosspane_transport::Transport;
use crosspane_types::geom::{PixelRect, PixelSize, euclid::point2};
use crosspane_types::id::{NodeId, ProjectionId};

use crate::agent::Event;

/// Proxy ids for the window host, shared by the engine loop (which opens and closes proxies) and
/// the decoder thread (which feeds them frames).
#[derive(Clone, Debug, Default)]
pub struct ProxyIds {
    inner: Arc<Mutex<ProxyMap>>,
}

#[derive(Debug, Default)]
struct ProxyMap {
    next: u64,
    by_key: HashMap<ProjectionKey, u64>,
    by_id: HashMap<u64, ProjectionKey>,
    stats: HashMap<ProjectionKey, FrameStats>,
    /// Each peer's clock minus this node's (ns), from the agent's ping exchange.
    offsets: HashMap<NodeId, i64>,
    /// Frames the renderer reported presented, per source peer, for projections that have since
    /// closed: a closed projection's own counter goes with its stats, the peer's total doesn't
    /// (WP-4.5).
    presented_closed: HashMap<NodeId, u64>,
    /// Whether the renderer has ever reported a presented frame. Until it does, "no frames" is
    /// not known to be zero, only unreported (WP-4.5: the value stays `null`, never 0).
    presented_reported: bool,
}

/// What the decoder has shown for one projection (for `crosspanectl status`).
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameStats {
    pub frames: u64,
    pub bytes: u64,
    pub last: Option<Instant>,
    /// Capture-to-decoded time, smoothed, once the peer's clock offset is known.
    pub latency_ms: Option<f64>,
    /// Frames the renderer submitted for presentation (WP-4.5; the call site is WP-4.5a's).
    pub presented: u64,
}

impl ProxyIds {
    pub fn open(&self, key: ProjectionKey) -> u64 {
        let Ok(mut map) = self.inner.lock() else {
            return 0;
        };
        if let Some(id) = map.by_key.get(&key) {
            return *id;
        }
        map.next += 1;
        let id = map.next;
        map.by_key.insert(key, id);
        map.by_id.insert(id, key);
        id
    }

    pub fn close(&self, key: ProjectionKey) -> Option<u64> {
        let mut map = self.inner.lock().ok()?;
        let id = map.by_key.remove(&key)?;
        map.by_id.remove(&id);
        // What this projection presented stays in its peer's total.
        if let Some(stats) = map.stats.remove(&key)
            && stats.presented > 0
        {
            let closed = map.presented_closed.entry(key.source).or_default();
            *closed = closed.saturating_add(stats.presented);
        }
        Some(id)
    }

    /// The renderer reported `frames` presented frames of `key`. Counts per projection; a report
    /// for a projection that has closed isn't counted, as with the decoder's own stats.
    pub fn presented(&self, key: ProjectionKey, frames: u32) {
        if let Ok(mut map) = self.inner.lock()
            && map.by_key.contains_key(&key)
        {
            map.presented_reported = true;
            let stats = map.stats.entry(key).or_default();
            stats.presented = stats.presented.saturating_add(u64::from(frames));
        }
    }

    /// Frames of projections from `source` that the renderer reported presented, in this
    /// instance: the sum over its open projections and the ones that have closed. `None` while the
    /// renderer has reported nothing at all (unknown, which is not the same as 0).
    pub fn presented_from(&self, source: NodeId) -> Option<u64> {
        let map = self.inner.lock().ok()?;
        if !map.presented_reported {
            return None;
        }
        let open: u64 = map
            .stats
            .iter()
            .filter(|(key, _)| key.source == source)
            .map(|(_, stats)| stats.presented)
            .fold(0, u64::saturating_add);
        Some(
            map.presented_closed
                .get(&source)
                .copied()
                .unwrap_or(0)
                .saturating_add(open),
        )
    }

    fn shown(&self, key: ProjectionKey, bytes: usize, captured_ns: u64) {
        let now = crate::platform::now().as_nanos();
        if let Ok(mut map) = self.inner.lock() {
            let age_ms = map.offsets.get(&key.source).and_then(|&offset| {
                // The capture time on this node's clock.
                let local = i128::from(captured_ns) - i128::from(offset);
                let age = i128::from(now) - local;
                (0..10_000_000_000).contains(&age).then(|| age as f64 / 1e6)
            });
            // A frame decoded just after its proxy closed doesn't bring its stats back.
            if !map.by_key.contains_key(&key) {
                return;
            }
            let stats = map.stats.entry(key).or_default();
            stats.frames += 1;
            stats.bytes += bytes as u64;
            stats.last = Some(Instant::now());
            if let Some(age) = age_ms {
                stats.latency_ms = Some(stats.latency_ms.map_or(age, |l| l * 0.9 + age * 0.1));
            }
        }
    }

    /// `peer`'s clock minus this node's, in nanoseconds (for frame ages).
    pub fn set_offset(&self, peer: NodeId, offset_ns: i64) {
        if let Ok(mut map) = self.inner.lock() {
            map.offsets.insert(peer, offset_ns);
        }
    }

    pub fn stats(&self, key: ProjectionKey) -> Option<FrameStats> {
        self.inner.lock().ok()?.stats.get(&key).copied()
    }

    pub fn id(&self, key: ProjectionKey) -> Option<u64> {
        self.inner.lock().ok()?.by_key.get(&key).copied()
    }

    pub fn key(&self, id: u64) -> Option<ProjectionKey> {
        self.inner.lock().ok()?.by_id.get(&id).copied()
    }
}

// ---------------------------------------------------------------------------------------------
// Source side
// ---------------------------------------------------------------------------------------------

/// The cursor a capture reported (`FrameEvent::Cursor` / `CursorDefault`).
#[derive(Clone)]
pub enum Shape {
    Image(CursorImage),
    Hidden,
    Default,
}

/// One capture as the media layer knows it: an identity that no other capture of this process
/// ever has, and a gate that retiring the capture closes. The agent opens one before the
/// platform can call back, so everything the capture delivers, including what comes before its
/// `Start`, carries it. A platform `StreamId` keys nothing here: a backend may number a new
/// stream like an old one whose callbacks and commands are still on their way.
#[derive(Clone, Debug)]
pub struct Capture {
    id: CaptureId,
    open: Arc<Mutex<bool>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CaptureId(u64);

impl Capture {
    /// Run `f` unless the capture has been retired, holding its gate: retiring waits for `f`.
    fn while_open<T>(&self, f: impl FnOnce() -> T) -> Option<T> {
        let open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        (*open).then(f)
    }

    fn is_open(&self) -> bool {
        *self.open.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Once this returns, nothing of the capture is queued or sent any more.
    fn close(&self) {
        *self.open.lock().unwrap_or_else(PoisonError::into_inner) = false;
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn id(&self) -> CaptureId {
        self.id
    }
}

pub enum SourceCmd {
    /// Payload-free wake: pixels stay in the capture's bounded mailbox.
    FramesReady {
        capture: Capture,
    },
    /// `capture` started for `projection`, to be sent to `peer`. It replaces any other capture
    /// of the projection, whose numbering it continues.
    Start {
        capture: Capture,
        projection: ProjectionId,
        peer: NodeId,
        /// The peer can decode H.264 (it advertised `h264`).
        video: bool,
        /// The peer shows region video (it advertised `h264roi`, WP-2.32).
        region: bool,
        /// The peer shows cursor shapes (it advertised `cursor`).
        cursor: bool,
        /// Video bitrate for this stream (from the path's link class, 03 §7.4).
        bits_per_second: u32,
    },
    Frame {
        capture: Capture,
        frame: Frame,
    },
    Cursor {
        capture: Capture,
        cursor: Shape,
    },
    /// The capture was retired ([`SourceSender::stop`]): forget it.
    Stop {
        capture: CaptureId,
    },
    /// This node's projection ended: its numbers go, with any capture still feeding it.
    Retire {
        projection: ProjectionId,
    },
    RequestKey {
        projection: ProjectionId,
    },
    /// The peer's features changed (a replacement connection): applies to every stream to it.
    PeerFeatures {
        peer: NodeId,
        video: bool,
        region: bool,
        cursor: bool,
    },
    /// Answered once everything queued before it has been handled and sent (tests).
    #[cfg(test)]
    Barrier(Sender<()>),
    /// Stops the encoder until released, so that what is queued meanwhile is one batch (tests).
    #[cfg(test)]
    Hold(Receiver<()>),
    /// Set a projection's last numbers (tests).
    #[cfg(test)]
    Seed {
        projection: ProjectionId,
        numbers: Sequences,
    },
    /// A projection's last numbers, while it has any (tests).
    #[cfg(test)]
    Probe {
        projection: ProjectionId,
        reply: Sender<Option<Sequences>>,
    },
}

/// A projection's last picture and cursor-shape numbers. They belong to the projection, not to a
/// capture: every capture that feeds it continues them, so the destination, which numbers per
/// projection, takes a replacement's frames as newer. They never wrap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sequences {
    pub picture: u64,
    pub cursor: u64,
}

/// The numbers a projection's encodings share (all on the encoder thread).
type Numbers = Rc<Cell<Sequences>>;

/// Where encoded media goes: the transport, or a recorder in tests.
trait SourceOutput {
    fn send_media(&self, peer: NodeId, data: Arc<[u8]>) -> Result<(), LinkError>;
}

impl SourceOutput for Transport {
    fn send_media(&self, peer: NodeId, data: Arc<[u8]>) -> Result<(), LinkError> {
        Transport::send_media(self, peer, data)
    }
}

#[derive(Default)]
struct FrameMailbox {
    queues: HashMap<CaptureId, VecDeque<Frame>>,
    closed: bool,
    stopping: Arc<std::sync::atomic::AtomicBool>,
}
type SourceFrames = Arc<Mutex<FrameMailbox>>;

/// Non-waiting frame enqueue across backend swaps: two queued images per capture.
#[derive(Clone)]
pub struct SourceSender {
    commands: Sender<SourceCmd>,
    frames: Option<SourceFrames>,
    /// The next capture identity, shared by every clone.
    captures: Arc<AtomicU64>,
}

impl SourceSender {
    /// A new, open capture with an identity no capture has had before; `None` once all of them
    /// have been used.
    pub fn open_capture(&self) -> Option<Capture> {
        let id = self
            .captures
            .fetch_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |id| id.checked_add(1),
            )
            .ok()?;
        Some(Capture {
            id: CaptureId(id),
            open: Arc::new(Mutex::new(true)),
        })
    }

    /// Retire `capture`. Once this returns, none of its payloads is queued, encoded or sent any
    /// more, and the images it had queued are released.
    pub fn stop(&self, capture: &Capture) {
        capture.close();
        if let Some(frames) = &self.frames {
            let retired = frames
                .lock()
                .ok()
                .and_then(|mut frames| frames.queues.remove(&capture.id));
            drop(retired);
        }
        let _ = self.commands.send(SourceCmd::Stop {
            capture: capture.id,
        });
    }

    /// Queue `cmd`. A frame or cursor of a retired capture is refused (and handed back).
    pub fn send(&self, cmd: SourceCmd) -> Result<(), mpsc::SendError<SourceCmd>> {
        match cmd {
            SourceCmd::Frame { capture, frame } => self.send_frame(capture, frame),
            SourceCmd::Cursor { capture, cursor } => {
                // Queued under the gate, so it can't follow the capture's retirement.
                let gate = Arc::clone(&capture.open);
                let open = gate.lock().unwrap_or_else(PoisonError::into_inner);
                if !*open {
                    return Err(mpsc::SendError(SourceCmd::Cursor { capture, cursor }));
                }
                let result = self.commands.send(SourceCmd::Cursor { capture, cursor });
                drop(open);
                result
            }
            other => self.commands.send(other),
        }
    }

    fn send_frame(
        &self,
        capture: Capture,
        mut frame: Frame,
    ) -> Result<(), mpsc::SendError<SourceCmd>> {
        // Queued under the gate, so it can't follow the capture's retirement.
        let gate = Arc::clone(&capture.open);
        let open = gate.lock().unwrap_or_else(PoisonError::into_inner);
        if !*open {
            drop(open);
            return Err(mpsc::SendError(SourceCmd::Frame { capture, frame }));
        }
        let Some(frames) = &self.frames else {
            let result = self.commands.send(SourceCmd::Frame { capture, frame });
            drop(open);
            return result;
        };
        let Ok(mut mailbox) = frames.lock() else {
            drop(open);
            return Err(mpsc::SendError(SourceCmd::Frame { capture, frame }));
        };
        if mailbox.closed || mailbox.stopping.load(std::sync::atomic::Ordering::Acquire) {
            drop(mailbox);
            drop(open);
            return Err(mpsc::SendError(SourceCmd::Frame { capture, frame }));
        }
        let id = capture.id;
        let queue = mailbox.queues.entry(id).or_default();
        let wake = queue.is_empty();
        let replaced = (queue.len() == 2).then(|| queue.pop_back()).flatten();
        if let Some(old) = &replaced {
            merge_frame_damage(&mut frame, old);
        }
        queue.push_back(frame);
        drop(mailbox);
        let result = if wake {
            self.commands.send(SourceCmd::FramesReady { capture })
        } else {
            Ok(())
        };
        if result.is_err() {
            let retired = frames
                .lock()
                .ok()
                .and_then(|mut frames| frames.queues.remove(&id));
            drop(retired);
        }
        drop(open);
        drop(replaced); // Driver/lease release runs under neither the mailbox lock nor the gate.
        result
    }

    #[cfg(test)]
    pub(crate) fn take_frame(&self, capture: CaptureId) -> Option<Frame> {
        take_source_frame(self.frames.as_ref()?, capture)
    }
}

#[cfg(test)]
impl From<Sender<SourceCmd>> for SourceSender {
    fn from(commands: Sender<SourceCmd>) -> Self {
        Self {
            commands,
            frames: None,
            captures: Arc::default(),
        }
    }
}

pub(crate) fn source_channel() -> (SourceSender, Receiver<SourceCmd>) {
    let (commands, receiver) = mpsc::channel();
    (
        SourceSender {
            commands,
            frames: Some(Arc::new(Mutex::new(FrameMailbox::default()))),
            captures: Arc::default(),
        },
        receiver,
    )
}

fn take_source_frame(frames: &SourceFrames, capture: CaptureId) -> Option<Frame> {
    let mut queue = frames.lock().ok()?.queues.remove(&capture)?;
    let mut newest = queue.pop_back()?;
    if let Some(older) = queue.pop_front() {
        merge_frame_damage(&mut newest, &older);
    }
    Some(newest)
}

/// Serialize closing with enqueue; release every queued native image on the encoder, unlocked.
fn close_source_frames(frames: &SourceFrames) {
    let retired = {
        let mut mailbox = frames.lock().unwrap_or_else(|error| error.into_inner());
        mailbox.closed = true;
        std::mem::take(&mut mailbox.queues)
    };
    drop(retired);
}

/// Bounding union is conservative. Unknown/changed-size/invalid damage means FULL frame.
fn merge_frame_damage(new: &mut Frame, old: &Frame) {
    let damage = new.damage.take();
    let Some((old_damage, new_damage)) = old.damage.as_ref().zip(damage) else {
        return;
    };
    let Some(size) = new.size.try_cast() else {
        return;
    };
    if new.size != old.size {
        return;
    }
    let bounds = PixelRect::from_size(size);
    let mut union: Option<PixelRect> = None;
    for rect in old_damage.iter().chain(&new_damage) {
        if bounds.intersection(rect) != Some(*rect) {
            return;
        }
        union = Some(union.map_or(*rect, |union| union.union(rect)));
    }
    new.damage = Some(union.into_iter().collect());
}

struct Encoding {
    capture: Capture,
    projection: ProjectionId,
    peer: NodeId,
    encoder: TileEncoder,
    /// The projection's numbers, shared with any capture that replaces this one.
    numbers: Numbers,
    scheduler: HybridScheduler,
    /// The peer can decode H.264.
    peer_video: bool,
    /// The peer shows region video: only the moving rectangle goes as video (WP-2.32).
    peer_region: bool,
    regions: RegionScheduler,
    /// GPU change detection and NV12 for frames on the GPU (GPU-v0); `None` once it failed.
    gpu: Option<SourceGpu>,
    video: Option<Box<dyn VideoEncoder>>,
    /// The next video frame must be an IDR (a frame was dropped, or the receiver asked).
    video_key: bool,
    /// The last captured frame, for the lossless refresh when frames stop during video.
    last: Option<Frame>,
    last_at: Instant,
    /// The peer shows cursor shapes.
    peer_cursor: bool,
    bits_per_second: u32,
    /// The newest cursor the capture reported, and whether the peer still needs it.
    cursor: Option<Shape>,
    cursor_dirty: bool,
    /// A frame was lost or the receiver asked for a key frame: if no new capture comes (captures
    /// arrive only on damage), the last frame goes again as a key frame.
    refresh_due: bool,
    /// When a key-frame request was last honoured (requests closer than `KEY_REQUEST_GAP` are
    /// ignored: a large key frame can take longer than the receiver's gap timeout to arrive).
    last_key_request: Option<Instant>,
}

const KEY_REQUEST_GAP: Duration = Duration::from_secs(1);
/// How long a cursor or frame that came before its capture's Start is kept.
const EARLY_TTL: Duration = Duration::from_secs(5);
/// How long a stream must be idle before a due refresh is sent from the last frame.
const REFRESH_IDLE: Duration = Duration::from_millis(100);

/// What the encoder thread needs for video.
#[derive(Clone)]
pub struct VideoSetup {
    pub codecs: Option<Arc<dyn VideoCodecs>>,
    /// The source GPU (GPU-v0): frames on it are hashed and converted there.
    pub gpu: Option<crate::platform::GpuDevice>,
}

/// Kept by Linux's main orchestration until the owner's GPU destructors have finished.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct Worker {
    name: &'static str,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    joining: bool,
    joined: Arc<std::sync::atomic::AtomicBool>,
}

impl Worker {
    #[cfg(target_os = "linux")]
    pub fn completion(&self) -> (&'static str, Arc<std::sync::atomic::AtomicBool>) {
        (self.name, self.joined.clone())
    }

    fn spawn(
        name: &'static str,
        run: impl FnOnce(&std::sync::atomic::AtomicBool) + Send + 'static,
    ) -> Self {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || run(&stopping))
            .map_err(|error| tracing::error!(%error, worker = name, "could not start media worker"))
            .ok();
        Self {
            name,
            stop,
            thread,
            joining: false,
            joined: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
pub fn fake_worker(
    name: &'static str,
    run: impl FnOnce(&std::sync::atomic::AtomicBool) + Send + 'static,
) -> Worker {
    Worker::spawn(name, run)
}

/// Stop both owners together and spend one absolute budget, including their destructors.
#[cfg(target_os = "linux")]
pub fn stop_workers(workers: &mut [Worker], deadline: Instant) -> Vec<&'static str> {
    use std::sync::atomic::Ordering;
    for worker in workers.iter() {
        worker.stop.store(true, Ordering::Release);
    }
    for worker in workers.iter_mut().filter(|worker| !worker.joining) {
        worker.joining = true;
        let Some(owner) = worker.thread.take() else {
            worker.joined.store(true, Ordering::Release);
            continue;
        };
        let completed = worker.joined.clone();
        let name = worker.name;
        // is_finished() can precede a blocked OS TLS destructor. Only a completed real join
        // proves the GPU owner is gone. These two shutdown-only waiters never own GPU objects.
        worker.thread = std::thread::Builder::new()
            .name(format!("{name}-join"))
            .spawn(move || {
                if owner.join().is_err() {
                    tracing::error!(worker = name, "media worker panicked during shutdown");
                }
                completed.store(true, Ordering::Release);
            })
            .map_err(
                |error| tracing::error!(%error, worker = name, "could not start media join waiter"),
            )
            .ok();
    }
    loop {
        let mut remaining = Vec::new();
        for worker in workers.iter_mut() {
            if worker.joined.load(Ordering::Acquire) {
                // The GPU owner has been joined; the CPU waiter's remaining return/TLS work
                // cannot destroy its objects. Do not introduce another blocking join here.
                drop(worker.thread.take());
            } else {
                remaining.push(worker.name);
            }
        }
        if remaining.is_empty() || Instant::now() >= deadline {
            return remaining;
        }
        std::thread::sleep(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(10)),
        );
    }
}

/// Start the encoder thread; capture sinks and the engine loop send it `SourceCmd`s.
pub fn start_source(transport: Arc<Transport>, video: VideoSetup) -> (SourceSender, Worker) {
    let (tx, rx) = source_channel();
    let frames = tx.frames.clone();
    let worker = Worker::spawn("media-encode", move |stop| {
        crate::exit_on_panic("media encoder", || {
            encode_loop(&rx, frames.as_ref(), transport.as_ref(), &video, stop);
            if let Some(frames) = &frames {
                close_source_frames(frames);
            }
        })
    });
    if let Some(frames) = &tx.frames {
        frames
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .stopping = worker.stop.clone();
    }
    (tx, worker)
}

fn encode_loop(
    rx: &Receiver<SourceCmd>,
    frames: Option<&SourceFrames>,
    transport: &dyn SourceOutput,
    video: &VideoSetup,
    stop: &std::sync::atomic::AtomicBool,
) {
    // At most one per projection: a capture's Start retires the projection's other one.
    let mut streams: HashMap<CaptureId, Encoding> = HashMap::new();
    // Each projection's numbers, from its first Start until this node's projection ends.
    let mut numbering: HashMap<ProjectionId, Numbers> = HashMap::new();
    // Cursors reported before the capture's Start arrived (the capture thread may be first).
    let mut early_cursors: HashMap<CaptureId, (Capture, Shape, Instant)> = HashMap::new();
    // Frames that came before their capture's Start (the capture thread may be first): a still
    // window's first frame may be its only one. Only that capture's Start takes them.
    let mut early_frames: HashMap<CaptureId, (Capture, Frame, Instant)> = HashMap::new();
    let mut out = Vec::new();
    let epoch = Instant::now();
    while !stop.load(std::sync::atomic::Ordering::Acquire) {
        let first = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(cmd) => Some(cmd),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        // Only the newest frame of each capture matters: drain what queued up meanwhile.
        let mut latest: BTreeMap<CaptureId, Frame> = BTreeMap::new();
        #[cfg(test)]
        let mut barriers = Vec::new();
        let mut handle = |cmd: SourceCmd, streams: &mut HashMap<CaptureId, Encoding>| {
            let cmd = if let SourceCmd::FramesReady { capture } = cmd {
                let Some(frame) = frames.and_then(|frames| take_source_frame(frames, capture.id))
                else {
                    return;
                };
                // A stale wake must not erase the idle-refinement image. A real replacement
                // retires it before any encoding work, leaving one consumer-held image.
                if let Some(enc) = streams.get_mut(&capture.id) {
                    enc.last = None;
                }
                SourceCmd::Frame { capture, frame }
            } else {
                cmd
            };
            match cmd {
                SourceCmd::FramesReady { .. } => (),
                SourceCmd::Start {
                    capture,
                    projection,
                    peer,
                    video: peer_video,
                    region: peer_region,
                    cursor: peer_cursor,
                    bits_per_second,
                } => {
                    let cursor = early_cursors.remove(&capture.id);
                    let frame = early_frames.remove(&capture.id);
                    if !capture.is_open() {
                        return;
                    }
                    let replaced = retire_where(streams, frames, |e| {
                        e.projection == projection && e.capture.id != capture.id
                    });
                    for id in replaced {
                        latest.remove(&id);
                    }
                    if let Some((_, frame, _)) = frame {
                        latest.entry(capture.id).or_insert(frame);
                    }
                    let cursor = cursor.map(|(_, shape, _)| shape);
                    streams.insert(
                        capture.id,
                        Encoding {
                            numbers: Rc::clone(numbering.entry(projection).or_default()),
                            capture,
                            projection,
                            peer,
                            encoder: TileEncoder::new(),
                            scheduler: HybridScheduler::new(HybridConfig::default()),
                            peer_video,
                            peer_region,
                            regions: RegionScheduler::new(RegionConfig::default()),
                            gpu: None,
                            video: None,
                            video_key: true,
                            last: None,
                            last_at: Instant::now(),
                            peer_cursor,
                            bits_per_second,
                            cursor_dirty: cursor.is_some(),
                            cursor,
                            refresh_due: false,
                            last_key_request: None,
                        },
                    );
                }
                SourceCmd::Frame { capture, frame } => {
                    if streams.contains_key(&capture.id) {
                        let mut frame = frame;
                        if let Some(old) = latest.remove(&capture.id) {
                            merge_frame_damage(&mut frame, &old);
                        }
                        latest.insert(capture.id, frame);
                    } else if capture.is_open() {
                        early_frames
                            .retain(|_, (c, _, at)| c.is_open() && at.elapsed() < EARLY_TTL);
                        if early_frames.contains_key(&capture.id) || early_frames.len() < 8 {
                            let mut frame = frame;
                            if let Some((_, old, _)) = early_frames.remove(&capture.id) {
                                merge_frame_damage(&mut frame, &old);
                            }
                            early_frames.insert(capture.id, (capture, frame, Instant::now()));
                        }
                    }
                }
                SourceCmd::Cursor { capture, cursor } => match streams.get_mut(&capture.id) {
                    Some(e) => {
                        e.cursor = Some(cursor);
                        e.cursor_dirty = true;
                    }
                    None if capture.is_open() => {
                        early_cursors
                            .retain(|_, (c, _, at)| c.is_open() && at.elapsed() < EARLY_TTL);
                        if early_cursors.contains_key(&capture.id) || early_cursors.len() < 64 {
                            early_cursors.insert(capture.id, (capture, cursor, Instant::now()));
                        }
                    }
                    None => {}
                },
                SourceCmd::Stop { capture } => {
                    if let Some(enc) = streams.remove(&capture) {
                        enc.capture.close();
                    }
                    latest.remove(&capture);
                    early_cursors.remove(&capture);
                    early_frames.remove(&capture);
                }
                SourceCmd::Retire { projection } => {
                    numbering.remove(&projection);
                    for id in retire_where(streams, frames, |e| e.projection == projection) {
                        latest.remove(&id);
                    }
                }
                SourceCmd::RequestKey { projection } => {
                    for e in streams.values_mut().filter(|e| e.projection == projection) {
                        if e.last_key_request
                            .is_some_and(|at| at.elapsed() < KEY_REQUEST_GAP)
                        {
                            continue;
                        }
                        e.last_key_request = Some(Instant::now());
                        e.encoder.request_key();
                        e.video_key = true;
                        e.cursor_dirty = e.cursor.is_some();
                        e.refresh_due = true;
                    }
                }
                SourceCmd::PeerFeatures {
                    peer,
                    video,
                    region,
                    cursor,
                } => {
                    for e in streams.values_mut().filter(|e| e.peer == peer) {
                        let changed = e.peer_video != video || e.peer_region != region;
                        e.peer_video = video;
                        e.peer_region = region;
                        if changed {
                            // Start over in a known state: lossless key frame, fresh schedulers.
                            e.scheduler = HybridScheduler::new(HybridConfig::default());
                            e.regions = RegionScheduler::new(RegionConfig::default());
                            e.encoder.request_key();
                            e.video_key = true;
                            e.refresh_due = true;
                        }
                        if cursor && !e.peer_cursor {
                            e.cursor_dirty = e.cursor.is_some();
                        }
                        e.peer_cursor = cursor;
                    }
                }
                #[cfg(test)]
                SourceCmd::Barrier(reply) => barriers.push(reply),
                #[cfg(test)]
                SourceCmd::Hold(release) => {
                    let _ = release.recv();
                }
                #[cfg(test)]
                SourceCmd::Seed {
                    projection,
                    numbers,
                } => numbering.entry(projection).or_default().set(numbers),
                #[cfg(test)]
                SourceCmd::Probe { projection, reply } => {
                    let _ = reply.send(numbering.get(&projection).map(|numbers| numbers.get()));
                }
            }
        };
        if let Some(first) = first {
            handle(first, &mut streams);
        }
        while let Ok(cmd) = rx.try_recv() {
            handle(cmd, &mut streams);
        }
        // A capture retired from outside (its stop, or numbers used up) does no more work here;
        // what it still had waiting for its Start goes with it.
        retire_where(&mut streams, frames, |e| !e.capture.is_open());
        early_frames.retain(|_, (c, _, at)| c.is_open() && at.elapsed() < EARLY_TTL);
        early_cursors.retain(|_, (c, _, at)| c.is_open() && at.elapsed() < EARLY_TTL);
        for enc in streams.values_mut().filter(|e| e.cursor_dirty) {
            send_cursor(enc, transport, &mut out);
        }
        let now = epoch.elapsed();
        for (capture, frame) in latest {
            let Some(enc) = streams.get_mut(&capture) else {
                continue;
            };
            enc.refresh_due = false;
            encode_frame(enc, &frame, now, false, video, transport, &mut out);
            enc.last_at = Instant::now();
            enc.last = Some(frame);
        }
        // A lost frame or a key-frame request on a window that isn't changing: resend the last
        // frame as a lossless key frame.
        for enc in streams.values_mut() {
            if enc.refresh_due
                && enc.last_at.elapsed() >= REFRESH_IDLE
                && let Some(frame) = enc.last.clone()
            {
                enc.refresh_due = false;
                enc.encoder.request_key();
                enc.video_key = true;
                send_tiles(enc, &frame, true, transport, &mut out);
                tracing::debug!(seq = enc.numbers.get().picture, "refresh of an idle stream");
            }
        }
        // Motion stopped during video and no new frame came: plan on the last frame so the
        // scheduler can leave video with its lossless key frame.
        for enc in streams.values_mut() {
            tracing::trace!(
                in_video = enc.scheduler.in_video(),
                idle_ms = enc.last_at.elapsed().as_millis() as u64,
                has_last = enc.last.is_some(),
                "idle check"
            );
            // Region video: re-plan the unchanged last frame; once the region ends its stale
            // tiles go out losslessly (no key frame needed).
            if enc.peer_region
                && enc.regions.region().is_some()
                && enc.last_at.elapsed() >= Duration::from_millis(200)
                && let Some(frame) = enc.last.clone()
            {
                encode_frame(enc, &frame, now, true, video, transport, &mut out);
                continue;
            }
            if enc.scheduler.in_video()
                && enc.last_at.elapsed() >= Duration::from_millis(200)
                && let Some(frame) = enc.last.clone()
            {
                let total = tile_count(&frame);
                let available = enc.peer_video && video.codecs.is_some();
                let plan = enc.scheduler.plan(0, total, now, available);
                tracing::debug!(?plan, "idle plan during video");
                if plan == FramePlan::TilesKey {
                    enc.encoder.request_key();
                    send_tiles(enc, &frame, true, transport, &mut out);
                    tracing::debug!(
                        seq = enc.numbers.get().picture,
                        "lossless refresh after motion"
                    );
                }
            }
        }
        #[cfg(test)]
        for reply in barriers {
            let _ = reply.send(());
        }
    }
}

/// Retire the encodings `which` picks: their captures close and the images they had queued are
/// released (outside the mailbox lock). Returns the captures retired.
fn retire_where(
    streams: &mut HashMap<CaptureId, Encoding>,
    frames: Option<&SourceFrames>,
    which: impl Fn(&Encoding) -> bool,
) -> Vec<CaptureId> {
    let ids: Vec<_> = streams
        .iter()
        .filter(|(_, e)| which(e))
        .map(|(id, _)| *id)
        .collect();
    for id in &ids {
        if let Some(enc) = streams.remove(id) {
            enc.capture.close();
            let queued = frames
                .and_then(|frames| frames.lock().ok())
                .and_then(|mut frames| frames.queues.remove(id));
            drop(queued);
        }
    }
    ids
}

/// Run a codec step over a frame's pixels wherever they are (a native image is mapped meanwhile).
fn with_pixels<T, E: std::fmt::Display>(
    frame: &Frame,
    f: impl FnOnce(&[u8], u32) -> Result<T, E>,
) -> Result<T, String> {
    frame
        .with_pixels(f)
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

fn tile_count(frame: &Frame) -> u32 {
    frame.size.width.div_ceil(TILE) * frame.size.height.div_ceil(TILE)
}

/// The header of the projection's next picture; `None` once its numbers are used up, which ends
/// the capture (numbers never wrap or start over).
fn header(enc: &Encoding, frame: &Frame) -> Option<FrameHeader> {
    let Some(seq) = enc.numbers.get().picture.checked_add(1) else {
        exhausted(enc, "picture");
        return None;
    };
    Some(FrameHeader {
        projection: enc.projection.0,
        seq,
        key: false,
        captured_ns: frame.at.as_nanos(),
        width: frame.size.width,
        height: frame.size.height,
    })
}

/// The projection has sent its last number of `kind`: this capture ends. A replacement would
/// find no numbers either; only the projection's end retires them.
fn exhausted(enc: &Encoding, kind: &str) {
    if enc.capture.is_open() {
        tracing::warn!(
            kind,
            "a projection used up its media numbers; its capture ends"
        );
        enc.capture.close();
    }
}

/// What the scheduler decided for one capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Plan {
    /// Lossless tiles; `key` makes it a tile key frame.
    Tiles { key: bool },
    /// Video of the whole window (`region: None`) or of a region of it, plus lossless tiles for
    /// the changes outside the region; `key` asks for an IDR.
    Video { region: Option<TileRect>, key: bool },
}

/// A frame's texture on the source GPU and the frame's top-left corner in it, when the frame is
/// in GPU memory there (DMA-BUF capture on Linux, an SCK IOSurface on the Mac).
fn frame_texture(frame: &Frame, video: &VideoSetup) -> Option<(wgpu::Texture, (u32, u32))> {
    let native = frame.native()?;
    let gpu = video.gpu.as_ref()?;
    #[cfg(target_os = "linux")]
    {
        let _ = gpu;
        crosspane_platform_linux::dmabuf::texture_of(native.as_ref())
            .map(|(texture, origin)| (texture.clone(), origin))
    }
    #[cfg(target_os = "macos")]
    {
        crosspane_platform_macos::gpu_import::wrap_capture(&gpu.device, native.as_ref())
    }
}

/// Device memory isn't readable by the CPU: tiles for the lossless codec are gathered on the GPU.
/// The Mac's memory is unified, so it reads the changed tiles in place instead.
const GATHER_ON_GPU: bool = cfg!(target_os = "linux");

/// One captured frame: change detection always runs (on the GPU when the frame is there, else
/// over its pixels), then the scheduler decides what goes out, and only tiles that go out are
/// compressed. `idle` re-plans the last frame when captures stopped: no video is sent then.
fn encode_frame(
    enc: &mut Encoding,
    frame: &Frame,
    now: Duration,
    idle: bool,
    video: &VideoSetup,
    transport: &dyn SourceOutput,
    out: &mut Vec<u8>,
) {
    out.clear();
    let texture = frame_texture(frame, video);
    if texture.is_some()
        && enc.gpu.is_none()
        && let Some(gpu) = &video.gpu
    {
        enc.gpu = SourceGpu::new(gpu.device.clone(), gpu.queue.clone())
            .map_err(|e| tracing::info!(error = %e, "GPU tile hashing unavailable"))
            .ok();
    }
    let (scan, on_gpu) = match scan_frame(enc, frame, texture.as_ref()) {
        Ok(scanned) => scanned,
        Err(e) => {
            tracing::warn!(error = %e, "encode failed");
            return;
        }
    };
    let available = enc.peer_video && video.codecs.is_some();
    let plan = plan(enc, &scan, frame.size, now, available);
    tracing::trace!(
        changed = scan.changed(),
        total = scan.total(),
        ?plan,
        "frame plan"
    );
    match plan {
        Plan::Tiles { key } => {
            emit_and_send(
                enc,
                frame,
                texture.as_ref(),
                scan,
                on_gpu,
                None,
                key,
                transport,
                out,
            );
        }
        Plan::Video { region, key } => {
            match region {
                // The tile hashes follow the picture; the next tile frame is a key frame.
                None => match enc.encoder.commit(scan) {
                    Ok(()) => gpu_committed(enc, on_gpu),
                    Err(e) => tracing::warn!(error = %e, "tile commit failed"),
                },
                // The changes outside the region go losslessly first; the region's tiles become
                // stale and are refreshed once motion leaves them.
                Some(rect) => emit_and_send(
                    enc,
                    frame,
                    texture.as_ref(),
                    scan,
                    on_gpu,
                    Some(rect),
                    false,
                    transport,
                    out,
                ),
            }
            if idle {
                return;
            }
            if let Err(e) = send_video(
                enc,
                frame,
                texture.as_ref(),
                region,
                key,
                video,
                transport,
                out,
            ) {
                tracing::info!(error = %e, "video failed: lossless tiles for a while");
                enc.video = None;
                enc.scheduler.video_failed(now);
                enc.regions.video_failed(now);
                enc.encoder.request_key();
                send_tiles(enc, frame, true, transport, out);
            }
        }
    }
}

/// Change detection for one frame. The scan is on the GPU (`true`) when the frame is a texture
/// there and the GPU works; a GPU failure falls back to the CPU for good.
fn scan_frame(
    enc: &mut Encoding,
    frame: &Frame,
    texture: Option<&(wgpu::Texture, (u32, u32))>,
) -> Result<(TileScan, bool), String> {
    if let (Some((texture, origin)), Some(gpu)) = (texture, enc.gpu.as_mut()) {
        let region = FrameRegion {
            texture,
            origin: *origin,
            size: frame.size,
        };
        match gpu.scan(region, None, false) {
            Ok(changes) => {
                let scan = enc
                    .encoder
                    .scan_external(frame.size, &changes.changed_bits)
                    .map_err(|e| e.to_string())?;
                return Ok((scan, true));
            }
            Err(e) => {
                tracing::warn!(error = %e, "GPU tile hashing failed: CPU from now on");
                enc.gpu = None;
            }
        }
    }
    let scan = with_pixels(frame, |pixels, stride| {
        enc.encoder.scan(frame.size, pixels, stride)
    })?;
    Ok((scan, false))
}

/// Keep the GPU's committed hashes in step with the tile encoder's after it consumed a scan: a
/// GPU scan becomes the reference; after a CPU scan the GPU starts over (every tile changed).
fn gpu_committed(enc: &mut Encoding, on_gpu: bool) {
    if let Some(gpu) = enc.gpu.as_mut() {
        if on_gpu {
            gpu.commit();
        } else {
            gpu.reset();
        }
    }
}

fn plan(
    enc: &mut Encoding,
    scan: &TileScan,
    size: PixelSize,
    now: Duration,
    available: bool,
) -> Plan {
    if enc.peer_region {
        let (tiles_x, tiles_y) = (size.width.div_ceil(TILE), size.height.div_ceil(TILE));
        match enc
            .regions
            .plan(tiles_x, tiles_y, &scan.changed_bits(), now, available)
        {
            RegionPlan::Tiles => Plan::Tiles { key: false },
            RegionPlan::Video { region, key } => Plan::Video {
                region: Some(region),
                key,
            },
        }
    } else {
        match enc
            .scheduler
            .plan(scan.changed(), scan.total(), now, available)
        {
            FramePlan::Tiles => Plan::Tiles { key: false },
            FramePlan::TilesKey => Plan::Tiles { key: true },
            FramePlan::Video { key } => Plan::Video { region: None, key },
        }
    }
}

/// Encode the tiles a scan calls for (outside `video`, if given) and send them.
#[allow(clippy::too_many_arguments)]
fn emit_and_send(
    enc: &mut Encoding,
    frame: &Frame,
    texture: Option<&(wgpu::Texture, (u32, u32))>,
    scan: TileScan,
    on_gpu: bool,
    video: Option<TileRect>,
    key: bool,
    transport: &dyn SourceOutput,
    out: &mut Vec<u8>,
) {
    match emit_tiles(enc, frame, texture, scan, video, key, out) {
        Ok(stats) => {
            gpu_committed(enc, on_gpu);
            if stats.is_some() {
                send(enc, out, transport);
            }
        }
        Err(e) => tracing::warn!(error = %e, "encode failed"),
    }
}

fn emit_tiles(
    enc: &mut Encoding,
    frame: &Frame,
    texture: Option<&(wgpu::Texture, (u32, u32))>,
    scan: TileScan,
    video: Option<TileRect>,
    key: bool,
    out: &mut Vec<u8>,
) -> Result<Option<EncodeStats>, String> {
    let Some(header) = header(enc, frame) else {
        return Ok(None);
    };
    if GATHER_ON_GPU && let (Some((texture, origin)), Some(gpu)) = (texture, enc.gpu.as_mut()) {
        let bits = enc.encoder.tiles_to_send(&scan, video, key);
        let changes = TileChanges {
            tiles_x: frame.size.width.div_ceil(TILE),
            tiles_y: frame.size.height.div_ceil(TILE),
            changed: bits.iter().map(|word| word.count_ones()).sum(),
            changed_bits: bits,
        };
        let region = FrameRegion {
            texture,
            origin: *origin,
            size: frame.size,
        };
        let failure = match gpu.gather(region, &changes, false) {
            Ok(tiles) => {
                return enc
                    .encoder
                    .emit_region(scan, header, TilePixels::Packed(&tiles), video, key, out)
                    .map_err(|e| e.to_string());
            }
            Err(e) => e.to_string(),
        };
        // This frame is lost; the next one is scanned on the CPU, every tile changed.
        enc.gpu = None;
        return Err(format!(
            "GPU tile gather failed, CPU from now on: {failure}"
        ));
    }
    with_pixels(frame, |pixels, stride| {
        enc.encoder.emit_region(
            scan,
            header,
            TilePixels::Strided { pixels, stride },
            video,
            key,
            out,
        )
    })
}

/// Re-encode `frame` as tiles (a key frame when the tile encoder has one pending) and send it.
fn send_tiles(
    enc: &mut Encoding,
    frame: &Frame,
    key: bool,
    transport: &dyn SourceOutput,
    out: &mut Vec<u8>,
) {
    out.clear();
    let Some(header) = header(enc, frame) else {
        return;
    };
    match with_pixels(frame, |pixels, stride| {
        enc.encoder.encode(header, pixels, stride, key, out)
    }) {
        Ok(stats) => {
            // A CPU scan was committed: the GPU's hashes no longer describe the reference.
            gpu_committed(enc, false);
            if stats.is_some() {
                send(enc, out, transport);
            }
        }
        Err(e) => tracing::warn!(error = %e, "encode failed"),
    }
}

#[allow(clippy::too_many_arguments)]
fn send_video(
    enc: &mut Encoding,
    frame: &Frame,
    texture: Option<&(wgpu::Texture, (u32, u32))>,
    region: Option<TileRect>,
    key: bool,
    video: &VideoSetup,
    transport: &dyn SourceOutput,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let Some(mut header) = header(enc, frame) else {
        return Ok(());
    };
    let area = region.map(|rect| rect.region(frame.size));
    let (origin, size) = area.map_or(((0, 0), frame.size), |r| {
        ((r.x, r.y), PixelSize::new(r.width, r.height))
    });
    if enc.video.is_none() {
        let codecs = video.codecs.as_ref().ok_or("no video codecs")?;
        let encoder = codecs
            .encoder(size, enc.bits_per_second, 60)
            .map_err(|e| e.to_string())?;
        tracing::info!(encoder = encoder.name(), "video on");
        enc.video = Some(encoder);
        enc.video_key = true;
    }
    let force_key = key || enc.video_key;
    let mut access_unit = Vec::new();
    let encoder = enc.video.as_deref_mut().ok_or("no encoder")?;
    let encoded = encode_picture(
        encoder,
        enc.gpu.as_mut(),
        frame,
        texture,
        origin,
        size,
        force_key,
        &mut access_unit,
    )?;
    enc.video_key = false;
    header.key = encoded.key;
    match area {
        Some(area) => write_video_region(header, area, &access_unit, out),
        None => write_video(header, &access_unit, out),
    }
    .map_err(|e| e.to_string())?;
    send(enc, out, transport);
    Ok(())
}

/// One picture (`size` pixels at `origin` of the frame) through the encoder, from wherever the
/// frame is: NV12 written by the GPU into NVENC's own memory (Linux), the captured buffer itself
/// for VideoToolbox (Mac, whole frames), or CPU rows. A GPU path that fails falls back to rows.
#[allow(clippy::too_many_arguments, unused_variables)]
fn encode_picture(
    encoder: &mut dyn VideoEncoder,
    gpu: Option<&mut SourceGpu>,
    frame: &Frame,
    texture: Option<&(wgpu::Texture, (u32, u32))>,
    origin: (u32, u32),
    size: PixelSize,
    force_key: bool,
    out: &mut Vec<u8>,
) -> Result<EncodedVideo, String> {
    #[cfg(all(target_os = "linux", feature = "video"))]
    if let (Some((texture, at)), Some(gpu)) = (texture, gpu) {
        let native = (|| -> Result<Option<EncodedVideo>, String> {
            let Some(pool) = encoder.input_pool(size).map_err(|e| e.to_string())? else {
                return Ok(None);
            };
            let input = pool.acquire().map_err(|e| e.to_string())?;
            let Some((buffer, layout)) =
                crosspane_platform_linux::video::nv12_buffer(input.as_ref())
            else {
                return Ok(None);
            };
            let region = FrameRegion {
                texture,
                origin: (at.0 + origin.0, at.1 + origin.1),
                size,
            };
            let target = crosspane_render::source::Nv12Output {
                target: crosspane_render::source::Nv12Target::Buffer {
                    buffer,
                    y_offset: layout.y_offset,
                    y_pitch: layout.y_pitch,
                    uv_offset: layout.uv_offset,
                    uv_pitch: layout.uv_pitch,
                },
                colour: input.colour(),
            };
            gpu.write_nv12(region, target).map_err(|e| e.to_string())?;
            encoder
                .encode_native(input.as_ref(), size, force_key, out)
                .map(Some)
                .map_err(|e| e.to_string())
        })();
        match native {
            Ok(Some(encoded)) => return Ok(encoded),
            Ok(None) => {}
            Err(e) => tracing::debug!(error = %e, "GPU video input failed; encoding rows"),
        }
    }
    #[cfg(target_os = "macos")]
    if origin == (0, 0)
        && size == frame.size
        && let Some(native) = frame.native()
        && let Some(input) = crosspane_platform_macos::frame_capture::capture_input(native)
    {
        match encoder.encode_native(input.as_ref(), size, force_key, out) {
            Ok(encoded) => return Ok(encoded),
            Err(e) => tracing::debug!(error = %e, "native video input failed; encoding rows"),
        }
    }
    with_pixels(frame, |pixels, stride| {
        let offset = origin.1 as usize * stride as usize + origin.0 as usize * 4;
        encoder.encode(
            pixels.get(offset..).unwrap_or_default(),
            stride,
            size,
            force_key,
            out,
        )
    })
}

/// Send the newest cursor shape; a refused one stays due and goes again on the next pass.
fn send_cursor(enc: &mut Encoding, transport: &dyn SourceOutput, out: &mut Vec<u8>) {
    enc.cursor_dirty = false;
    let Some(cursor) = &enc.cursor else { return };
    if !enc.peer_cursor {
        return;
    }
    const HIDDEN: [u8; 4] = [0; 4];
    let (size, hotspot, pixels) = match cursor {
        Shape::Image(c) => (c.size, c.hotspot, &c.pixels[..]),
        Shape::Hidden | Shape::Default => (PixelSize::new(1, 1), (0, 0), &HIDDEN[..]),
    };
    let Some(seq) = enc.numbers.get().cursor.checked_add(1) else {
        exhausted(enc, "cursor");
        return;
    };
    let header = FrameHeader {
        projection: enc.projection.0,
        seq,
        key: false,
        captured_ns: 0,
        width: size.width,
        height: size.height,
    };
    let written = if matches!(cursor, Shape::Default) {
        write_default_cursor(header, out)
    } else {
        write_cursor(header, hotspot, pixels, out)
    };
    if let Err(e) = written {
        tracing::debug!(error = %e, "cursor image not sendable");
        return;
    }
    // Numbered and handed over under the gate: once the capture is retired, nothing more of it
    // goes out and it uses no more numbers.
    let sent = enc.capture.while_open(|| {
        enc.numbers.set(Sequences {
            cursor: seq,
            ..enc.numbers.get()
        });
        transport.send_media(enc.peer, Arc::from(&out[..]))
    });
    match sent {
        None | Some(Ok(())) => {}
        Some(Err(LinkError::Congested)) => enc.cursor_dirty = true,
        Some(Err(e)) => tracing::debug!(error = ?e, "cursor send failed"),
    }
}

/// Send one encoded frame; a refused one is dropped and the next of either kind becomes a key.
fn send(enc: &mut Encoding, frame: &[u8], transport: &dyn SourceOutput) {
    let Ok(header) = read_header(frame) else {
        return;
    };
    // Numbered and handed over under the gate: once the capture is retired, nothing more of it
    // goes out and it uses no more numbers.
    let sent = enc.capture.while_open(|| {
        let numbers = enc.numbers.get();
        enc.numbers.set(Sequences {
            picture: numbers.picture.max(header.seq),
            ..numbers
        });
        transport.send_media(enc.peer, Arc::from(frame))
    });
    match sent {
        None | Some(Ok(())) => {}
        Some(Err(LinkError::Congested)) => {
            enc.encoder.request_key();
            enc.video_key = true;
            enc.refresh_due = true;
        }
        Some(Err(e)) => tracing::debug!(error = ?e, "media send failed"),
    }
}

// ---------------------------------------------------------------------------------------------
// Destination side
// ---------------------------------------------------------------------------------------------

pub enum DestCmd {
    Media {
        peer: NodeId,
        data: Arc<[u8]>,
    },
    /// The proxy for `key` closed: forget its decoder.
    Forget(ProjectionKey),
    /// The picture last shown for `key` (BGRA rows), for `crosspanectl snapshot`.
    Snapshot {
        key: ProjectionKey,
        reply: Sender<Option<Shown>>,
    },
}

/// A decoded picture: its size and BGRA rows of `width * 4` bytes.
pub type Shown = (PixelSize, Arc<[u8]>);

struct Decoding {
    decoder: TileDecoder,
    /// Created at the first H.264 frame.
    video: Option<Box<dyn VideoDecoder>>,
    last: u64,
    /// The newest cursor frame applied (cursor frames are numbered on their own).
    cursor_seq: u64,
    pending: BTreeMap<u64, Arc<[u8]>>,
    gap_since: Option<Instant>,
    last_error: Option<Instant>,
    /// Which picture the proxy shows, for snapshots (made only when asked).
    showing: Showing,
    /// The newest decoded video picture when the decoder works in CPU memory. Its buffers are
    /// reused once the proxy has let go of it.
    picture: Arc<Nv12>,
    /// The newest decoded picture when it stayed in native memory (zero-copy).
    native: Option<Arc<dyn NativePicture>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Showing {
    Nothing,
    /// The tile decoder's canvas.
    Canvas,
    /// The picture's top-left `rect.size()` at `rect` in content of `size`, over the canvas.
    Video {
        size: PixelSize,
        rect: PixelRect,
    },
}

impl Decoding {
    /// The picture last handed to the proxy, as BGRA rows.
    fn snapshot(&self) -> Option<Shown> {
        match self.showing {
            Showing::Nothing => None,
            Showing::Canvas => {
                let (pixels, size) = self.decoder.canvas();
                Some((size, Arc::from(pixels)))
            }
            Showing::Video { size, rect } => {
                let copy;
                let picture = match &self.native {
                    Some(native) => {
                        let mut nv12 = Nv12::default();
                        native.to_nv12(&mut nv12).ok()?;
                        copy = nv12;
                        &copy
                    }
                    None => &*self.picture,
                };
                let area = PixelSize::new(rect.width() as u32, rect.height() as u32);
                let mut video = Vec::new();
                nv12_to_bgra(picture, area, &mut video).ok()?;
                // The canvas under a region (as the proxy shows it); black where there's none.
                let (canvas, canvas_size) = self.decoder.canvas();
                let mut pixels = if canvas_size == size {
                    canvas.to_vec()
                } else {
                    vec![0; size.width as usize * size.height as usize * 4]
                };
                let row = area.width as usize * 4;
                for (y, source) in video.chunks_exact(row).enumerate() {
                    let at =
                        ((rect.min.y as usize + y) * size.width as usize + rect.min.x as usize) * 4;
                    pixels.get_mut(at..at + row)?.copy_from_slice(source);
                }
                Some((size, Arc::from(pixels)))
            }
        }
    }
}

const GAP_TIMEOUT: Duration = Duration::from_millis(300);
const MAX_PENDING: usize = 8;

pub fn start_destination(
    host: Option<HostHandle>,
    ids: ProxyIds,
    engine: Sender<Event>,
    video: VideoSetup,
) -> (Sender<DestCmd>, Worker) {
    let (tx, rx) = mpsc::channel::<DestCmd>();
    let worker = Worker::spawn("media-decode", move |stop| {
        crate::exit_on_panic("media decoder", || {
            decode_loop(&rx, host.as_ref(), &ids, &engine, &video, stop)
        })
    });
    (tx, worker)
}

fn decode_loop(
    rx: &Receiver<DestCmd>,
    host: Option<&HostHandle>,
    ids: &ProxyIds,
    engine: &Sender<Event>,
    video: &VideoSetup,
    stop: &std::sync::atomic::AtomicBool,
) {
    let mut decoders: HashMap<ProjectionKey, Decoding> = HashMap::new();
    while !stop.load(std::sync::atomic::Ordering::Acquire) {
        let cmd = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(cmd) => Some(cmd),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        match cmd {
            Some(DestCmd::Forget(key)) => {
                decoders.remove(&key);
            }
            Some(DestCmd::Snapshot { key, reply }) => {
                let _ = reply.send(decoders.get(&key).and_then(Decoding::snapshot));
            }
            Some(DestCmd::Media { peer, data }) => {
                let Ok(header) = read_header(&data) else {
                    tracing::debug!("dropping a malformed media frame");
                    continue;
                };
                let key = ProjectionKey {
                    source: peer,
                    projection: ProjectionId(header.projection),
                };
                let Some(id) = ids.id(key) else { continue };
                let d = decoders.entry(key).or_insert_with(|| Decoding {
                    decoder: TileDecoder::new(),
                    video: None,
                    last: 0,
                    cursor_seq: 0,
                    showing: Showing::Nothing,
                    picture: Arc::default(),
                    native: None,
                    pending: BTreeMap::new(),
                    gap_since: None,
                    last_error: None,
                });
                if read_codec(&data) == Ok(Codec::Cursor) {
                    apply_cursor(d, id, &data, host);
                    continue;
                }
                if header.key && header.seq > d.last {
                    // A key frame supersedes everything older.
                    d.pending.retain(|seq, _| *seq > header.seq);
                    apply(d, key, id, &data, header.seq, host, engine, ids, video);
                } else if header.seq > d.last && d.pending.len() < MAX_PENDING {
                    d.pending.insert(header.seq, data);
                }
                // Apply whatever is now consecutive (nothing follows the last number).
                while let Some(seq) = d.last.checked_add(1)
                    && let Some(data) = d.pending.remove(&seq)
                {
                    apply(d, key, id, &data, seq, host, engine, ids, video);
                }
                d.gap_since = if d.pending.is_empty() {
                    None
                } else {
                    d.gap_since.or(Some(Instant::now()))
                };
            }
            None => {}
        }
        // Gaps that didn't fill: ask for a key frame (at most every 200 ms per projection).
        for (key, d) in &mut decoders {
            if d.gap_since.is_some_and(|t| t.elapsed() > GAP_TIMEOUT)
                && d.last_error
                    .is_none_or(|t| t.elapsed() > Duration::from_millis(200))
            {
                d.last_error = Some(Instant::now());
                d.pending.clear();
                d.gap_since = None;
                let _ = engine.send(Event::Input(Input::MediaError { key: *key }));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn apply(
    d: &mut Decoding,
    key: ProjectionKey,
    id: u64,
    data: &[u8],
    seq: u64,
    host: Option<&HostHandle>,
    engine: &Sender<Event>,
    ids: &ProxyIds,
    video: &VideoSetup,
) {
    let result = match read_codec(data) {
        Ok(Codec::H264) => apply_video(d, data, video)
            .map(|(header, size, rect)| (header, Some((size, rect)), None)),
        Ok(Codec::Tiles) => d
            .decoder
            .apply(data)
            .map_err(|e| e.to_string())
            .map(|(header, dirty)| (header, None, Some(dirty))),
        Ok(Codec::Cursor) => Err("a cursor frame in the picture sequence".to_owned()),
        Err(e) => Err(e.to_string()),
    };
    match result {
        Ok((header, video_size, dirty)) => {
            d.last = seq.max(header.seq);
            ids.shown(key, data.len(), header.captured_ns);
            let command = if let Some((size, rect)) = video_size {
                d.showing = Showing::Video { size, rect };
                match &d.native {
                    Some(picture) => HostCommand::VideoNative {
                        id,
                        size,
                        rect,
                        picture: Arc::clone(picture),
                    },
                    None => HostCommand::Video {
                        id,
                        size,
                        rect,
                        picture: Arc::clone(&d.picture),
                    },
                }
            } else {
                d.showing = Showing::Canvas;
                // Shared, not copied: the decoder copies the canvas only if the proxy still holds
                // it when the next tile frame arrives.
                let (pixels, size) = d.decoder.shared_canvas();
                HostCommand::Frame {
                    id,
                    size,
                    pixels,
                    dirty: dirty.unwrap_or_default(),
                }
            };
            if let Some(host) = host {
                let _ = host.send(command);
            }
        }
        Err(e) => {
            tracing::debug!(error = %e, "frame didn't apply; requesting a key frame");
            d.last = seq;
            if d.last_error
                .is_none_or(|t| t.elapsed() > Duration::from_millis(200))
            {
                d.last_error = Some(Instant::now());
                let _ = engine.send(Event::Input(Input::MediaError { key }));
            }
        }
    }
}

/// Show a cursor frame on the proxy unless a newer one was already shown.
fn apply_cursor(d: &mut Decoding, id: u64, data: &[u8], host: Option<&HostHandle>) {
    let frame = match read_cursor(data) {
        Ok(frame) => frame,
        Err(e) => {
            tracing::debug!(error = %e, "dropping a malformed cursor frame");
            return;
        }
    };
    if frame.header.seq <= d.cursor_seq {
        return;
    }
    d.cursor_seq = frame.header.seq;
    if let Some(host) = host {
        if frame.default {
            let _ = host.send(HostCommand::DefaultCursor { id });
            return;
        }
        let _ = host.send(HostCommand::SetCursor {
            id,
            size: PixelSize::new(frame.header.width, frame.header.height),
            hotspot: frame.hotspot,
            pixels: Arc::from(frame.pixels),
        });
    }
}

/// Decode an H.264 frame into `d.picture` (or keep it native); returns its header, the content
/// size and where the picture goes in it (the whole content, or a region for region video).
fn apply_video(
    d: &mut Decoding,
    data: &[u8],
    video: &VideoSetup,
) -> Result<(FrameHeader, PixelSize, PixelRect), String> {
    let (header, region, access_unit) =
        read_video_region(data).map_err(|e: MediaError| e.to_string())?;
    if d.video.is_none() {
        let codecs = video
            .codecs
            .as_ref()
            .ok_or("this node can't decode video")?;
        let decoder = codecs.decoder().map_err(|e| e.to_string())?;
        tracing::info!(decoder = decoder.name(), "video decoding on");
        d.video = Some(decoder);
    }
    let decoder = d.video.as_mut().ok_or("no decoder")?;
    // A CPU picture reuses the last one's buffers unless the proxy hasn't uploaded it yet; a
    // native one stays where the decoder put it.
    let coded = match decoder
        .decode_native(access_unit, &mut d.picture)
        .map_err(|e| e.to_string())?
    {
        Decoded::Nv12(picture) => {
            d.native = None;
            picture.size
        }
        Decoded::Native(picture) => {
            let size = picture.size();
            d.native = Some(picture);
            size
        }
    };
    let size = PixelSize::new(header.width, header.height);
    let area = region.unwrap_or(VideoRegion {
        x: 0,
        y: 0,
        width: size.width,
        height: size.height,
    });
    if area.width == 0 || area.height == 0 || coded.width < area.width || coded.height < area.height
    {
        return Err("decoded picture smaller than its region".into());
    }
    let rect = PixelRect::new(
        point2(area.x as i32, area.y as i32),
        point2((area.x + area.width) as i32, (area.y + area.height) as i32),
    );
    Ok((header, size, rect))
}

#[cfg(all(test, target_os = "linux"))]
mod worker_exit_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    struct OwnerDrop(Sender<std::thread::ThreadId>);
    impl Drop for OwnerDrop {
        fn drop(&mut self) {
            assert!(self.0.send(std::thread::current().id()).is_ok());
        }
    }
    thread_local! {
        static OWNER_TLS: std::cell::RefCell<Option<OwnerDrop>> = const { std::cell::RefCell::new(None) };
    }

    #[test]
    fn stop_joins_owner_resources_and_tls_before_return() {
        let main = std::thread::current().id();
        let (dropped, drops) = mpsc::channel();
        let (ready, started) = mpsc::channel();
        let mut workers = ["media-encode", "media-decode"].map(|name| {
            let dropped = dropped.clone();
            let ready = ready.clone();
            Worker::spawn(name, move |stop| {
                let _gpu = OwnerDrop(dropped.clone());
                OWNER_TLS.with(|tls| *tls.borrow_mut() = Some(OwnerDrop(dropped)));
                ready.send(()).unwrap();
                while !stop.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
        });
        for _ in 0..2 {
            started.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        assert!(stop_workers(&mut workers, Instant::now() + Duration::from_secs(2)).is_empty());
        assert!(workers.iter().all(|worker| worker.thread.is_none()));
        let owners: Vec<_> = drops.try_iter().collect();
        assert_eq!(
            owners.len(),
            4,
            "both worker-local and TLS destructors finished before join returned"
        );
        assert!(owners.iter().all(|owner| *owner != main));
    }

    #[test]
    fn hung_destructors_share_one_deadline_and_keep_handles_until_cleanup() {
        struct HungDrop {
            entered: Sender<()>,
            release: Receiver<()>,
        }
        impl Drop for HungDrop {
            fn drop(&mut self) {
                self.entered.send(()).unwrap();
                self.release.recv().unwrap();
            }
        }
        let (entered, destructors) = mpsc::channel();
        let (ready, started) = mpsc::channel();
        let mut releases = Vec::new();
        let mut workers = ["media-encode", "media-decode"].map(|name| {
            let (release, receiving) = mpsc::channel();
            releases.push(release);
            let entered = entered.clone();
            let ready = ready.clone();
            Worker::spawn(name, move |stop| {
                let _gpu = HungDrop {
                    entered,
                    release: receiving,
                };
                ready.send(()).unwrap();
                while !stop.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
            })
        });
        for _ in 0..2 {
            started.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        let deadline = Instant::now() + Duration::from_millis(250);
        assert_eq!(
            stop_workers(&mut workers, deadline),
            ["media-encode", "media-decode"]
        );
        assert!(
            Instant::now() < deadline + Duration::from_millis(100),
            "two per-owner budgets must not fit this tolerance"
        );
        for _ in 0..2 {
            destructors.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        assert!(workers.iter().all(|worker| worker.thread.is_some()));
        for release in releases {
            release.send(()).unwrap();
        }
        assert!(stop_workers(&mut workers, Instant::now() + Duration::from_secs(2)).is_empty());
    }

    #[test]
    fn hung_tls_destructor_cannot_extend_the_join_budget() {
        struct HungTls {
            entered: Sender<()>,
            release: Receiver<()>,
        }
        impl Drop for HungTls {
            fn drop(&mut self) {
                assert!(self.entered.send(()).is_ok());
                assert!(self.release.recv().is_ok());
            }
        }
        thread_local! {
            static HUNG_TLS: std::cell::RefCell<Option<HungTls>> = const { std::cell::RefCell::new(None) };
        }
        let (entered, destructor) = mpsc::channel();
        let (release, receiving) = mpsc::channel();
        let worker = Worker::spawn("media-encode", move |_| {
            HUNG_TLS.with(|tls| {
                *tls.borrow_mut() = Some(HungTls {
                    entered,
                    release: receiving,
                })
            });
        });
        destructor.recv_timeout(Duration::from_secs(2)).unwrap();
        let (done, completed) = mpsc::channel();
        let stopper = std::thread::spawn(move || {
            let mut workers = [worker];
            let remaining = stop_workers(&mut workers, Instant::now() + Duration::from_millis(80));
            done.send((workers, remaining)).unwrap();
        });
        let result = completed.recv_timeout(Duration::from_millis(250));
        // Always unblock/join our own fake threads, including a regression that exceeded budget.
        release.send(()).unwrap();
        let in_budget = result.is_ok();
        let (mut workers, remaining) =
            result.unwrap_or_else(|_| completed.recv_timeout(Duration::from_secs(2)).unwrap());
        stopper.join().unwrap();
        assert!(stop_workers(&mut workers, Instant::now() + Duration::from_secs(2)).is_empty());
        assert!(
            in_budget,
            "OS TLS destruction escaped the absolute join deadline"
        );
        assert_eq!(remaining, ["media-encode"]);
    }

    #[test]
    fn decoder_stops_even_while_a_command_sender_remains_alive() {
        let (events, _receiving) = mpsc::channel();
        let (sender, worker) = start_destination(
            None,
            ProxyIds::default(),
            events,
            VideoSetup {
                codecs: None,
                gpu: None,
            },
        );
        let mut workers = [worker];
        assert!(stop_workers(&mut workers, Instant::now() + Duration::from_secs(2)).is_empty());
        assert!(
            sender
                .send(DestCmd::Forget(ProjectionKey {
                    source: NodeId([0; 32]),
                    projection: ProjectionId(0)
                }))
                .is_err()
        );
    }

    #[test]
    fn retained_sender_mailbox_closes_at_stop_and_native_frames_drop_on_encoder() {
        use super::source_queue_tests::{RetainedImages, counted_frame};
        let retained = Arc::new(RetainedImages::default());
        let (sender, receiver) = source_channel();
        let frames = sender.frames.as_ref().unwrap().clone();
        let capture = sender.open_capture().unwrap();
        for id in 0..2 {
            sender
                .send(SourceCmd::Frame {
                    capture: capture.clone(),
                    frame: counted_frame(&retained, id, PixelSize::new(8, 8), None),
                })
                .unwrap();
        }
        let (ready, started) = mpsc::channel();
        let (close, closing) = mpsc::channel();
        let worker = Worker::spawn("media-encode", move |stop| {
            let _receiver = receiver;
            ready.send(std::thread::current().id()).unwrap();
            while !stop.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            closing.recv().unwrap();
            close_source_frames(&frames);
        });
        sender.frames.as_ref().unwrap().lock().unwrap().stopping = worker.stop.clone();
        let encoder = started.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.stop.store(true, Ordering::Release);
        let rejected = sender
            .send(SourceCmd::Frame {
                capture,
                frame: counted_frame(&retained, 2, PixelSize::new(8, 8), None),
            })
            .unwrap_err();
        assert_eq!(
            retained.live(),
            3,
            "the rejected new image stays in the caller's error"
        );
        drop(rejected);
        assert_eq!(
            retained.live(),
            2,
            "both queued images await owner-thread drainage"
        );
        close.send(()).unwrap();
        let mut workers = [worker];
        assert!(stop_workers(&mut workers, Instant::now() + Duration::from_secs(2)).is_empty());
        assert_eq!(
            retained.live(),
            0,
            "a retained sender holds no queued native images after join"
        );
        let drops = retained.dropped_on.lock().unwrap();
        assert_eq!(drops.iter().filter(|thread| **thread == encoder).count(), 2);
        assert!(sender.frames.as_ref().unwrap().lock().unwrap().closed);
        assert!(
            sender
                .frames
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .queues
                .is_empty()
        );
    }
}

#[cfg(test)]
mod presented_tests {
    //! The `presented` hook (WP-4.5): a per-projection counter summed per source peer, `None`
    //! until the renderer has reported at all, and monotonic across a projection's close.

    use super::*;

    fn key(source: u8, projection: u64) -> ProjectionKey {
        ProjectionKey {
            source: NodeId([source; 32]),
            projection: ProjectionId(projection),
        }
    }

    #[test]
    fn nothing_is_known_until_the_renderer_reports() {
        let ids = ProxyIds::default();
        ids.open(key(1, 1));
        assert_eq!(ids.presented_from(NodeId([1; 32])), None);
        // A frame the decoder showed is not a presented frame.
        ids.shown(key(1, 1), 10, 0);
        assert_eq!(ids.presented_from(NodeId([1; 32])), None);
        ids.presented(key(1, 1), 1);
        assert_eq!(ids.presented_from(NodeId([1; 32])), Some(1));
        // Once the renderer has reported, a peer it hasn't shown anything of has 0.
        assert_eq!(ids.presented_from(NodeId([2; 32])), Some(0));
    }

    #[test]
    fn counts_are_summed_per_source_peer_over_its_projections() {
        let ids = ProxyIds::default();
        for k in [key(1, 1), key(1, 2), key(2, 1)] {
            ids.open(k);
        }
        ids.presented(key(1, 1), 3);
        ids.presented(key(1, 2), 1);
        ids.presented(key(2, 1), 1);
        assert_eq!(ids.presented_from(NodeId([1; 32])), Some(4));
        assert_eq!(ids.presented_from(NodeId([2; 32])), Some(1));
    }

    #[test]
    fn closing_a_projection_never_lowers_its_peers_total() {
        let ids = ProxyIds::default();
        ids.open(key(1, 1));
        ids.presented(key(1, 1), 1);
        ids.presented(key(1, 1), 1);
        assert_eq!(ids.presented_from(NodeId([1; 32])), Some(2));
        ids.close(key(1, 1));
        assert_eq!(ids.presented_from(NodeId([1; 32])), Some(2));
        // A later projection from the same peer adds to it.
        ids.open(key(1, 2));
        ids.presented(key(1, 2), 1);
        assert_eq!(ids.presented_from(NodeId([1; 32])), Some(3));
        // A frame of a projection that is gone isn't counted.
        ids.close(key(1, 2));
        ids.presented(key(1, 2), 1);
        assert_eq!(ids.presented_from(NodeId([1; 32])), Some(3));
    }

    #[test]
    fn one_projections_presented_count_saturates() {
        let ids = ProxyIds::default();
        let key = key(1, 1);
        ids.open(key);
        ids.inner
            .lock()
            .unwrap()
            .stats
            .entry(key)
            .or_default()
            .presented = u64::MAX - 1;
        ids.presented(key, 2);
        assert_eq!(ids.stats(key).unwrap().presented, u64::MAX);
        assert_eq!(ids.presented_from(key.source), Some(u64::MAX));
        ids.presented(key, u32::MAX);
        assert_eq!(ids.stats(key).unwrap().presented, u64::MAX);
        assert_eq!(ids.presented_from(key.source), Some(u64::MAX));
    }

    #[test]
    fn several_projections_presented_sum_saturates() {
        let ids = ProxyIds::default();
        let first = key(1, 1);
        let second = key(1, 2);
        ids.open(first);
        ids.open(second);
        ids.inner
            .lock()
            .unwrap()
            .stats
            .entry(first)
            .or_default()
            .presented = u64::MAX - 1;
        ids.presented(second, 2);
        assert_eq!(ids.presented_from(first.source), Some(u64::MAX));
        ids.presented(second, 3);
        assert_eq!(ids.presented_from(first.source), Some(u64::MAX));
    }

    #[test]
    fn closed_plus_open_presented_totals_and_close_accumulation_saturate() {
        let ids = ProxyIds::default();
        let first = key(1, 1);
        let second = key(1, 2);
        let third = key(1, 3);
        ids.open(first);
        ids.inner
            .lock()
            .unwrap()
            .stats
            .entry(first)
            .or_default()
            .presented = u64::MAX - 1;
        ids.close(first);
        ids.open(second);
        ids.presented(second, 2);
        assert_eq!(ids.presented_from(first.source), Some(u64::MAX));
        ids.close(second);
        assert_eq!(ids.presented_from(first.source), Some(u64::MAX));
        ids.open(third);
        ids.presented(third, u32::MAX);
        assert_eq!(ids.presented_from(first.source), Some(u64::MAX));
        ids.close(third);
        assert_eq!(ids.presented_from(first.source), Some(u64::MAX));
    }
}

#[cfg(test)]
pub(crate) mod source_queue_tests {
    use super::*;
    use crosspane_platform::{FrameImage, NativeImage, PlatformError};
    use crosspane_types::time::MonoTime;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug, Default)]
    pub(crate) struct RetainedImages {
        live: AtomicUsize,
        peak: AtomicUsize,
        dropped: Mutex<Vec<u64>>,
        pub(crate) dropped_on: Mutex<Vec<std::thread::ThreadId>>,
    }
    impl RetainedImages {
        pub(crate) fn live(&self) -> usize {
            self.live.load(Ordering::SeqCst)
        }
        pub(crate) fn peak(&self) -> usize {
            self.peak.load(Ordering::SeqCst)
        }
    }
    #[derive(Debug)]
    struct CountedImage {
        id: u64,
        size: PixelSize,
        retained: Arc<RetainedImages>,
    }
    impl Drop for CountedImage {
        fn drop(&mut self) {
            self.retained.live.fetch_sub(1, Ordering::SeqCst);
            self.retained.dropped.lock().unwrap().push(self.id);
            self.retained
                .dropped_on
                .lock()
                .unwrap()
                .push(std::thread::current().id());
        }
    }
    impl NativeImage for CountedImage {
        fn size(&self) -> PixelSize {
            self.size
        }
        fn read(&self, _: &mut dyn FnMut(&[u8], u32)) -> Result<(), PlatformError> {
            Err(PlatformError::Unsupported("counted test image"))
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }
    pub(crate) fn counted_frame(
        retained: &Arc<RetainedImages>,
        id: u64,
        size: PixelSize,
        damage: Option<Vec<PixelRect>>,
    ) -> Frame {
        let live = retained.live.fetch_add(1, Ordering::SeqCst) + 1;
        retained.peak.fetch_max(live, Ordering::SeqCst);
        Frame {
            size,
            image: FrameImage::Native(Arc::new(CountedImage {
                id,
                size,
                retained: retained.clone(),
            })),
            damage,
            at: MonoTime::ZERO,
        }
    }
    fn rect(x: i32, y: i32) -> PixelRect {
        PixelRect::new(point2(x, y), point2(x + 1, y + 1))
    }
    fn id(frame: &Frame) -> u64 {
        frame
            .native()
            .unwrap()
            .as_any()
            .downcast_ref::<CountedImage>()
            .unwrap()
            .id
    }
    fn enqueue(
        sender: &SourceSender,
        capture: &Capture,
        retained: &Arc<RetainedImages>,
        id: u64,
        damage: Option<Vec<PixelRect>>,
    ) {
        sender
            .send(SourceCmd::Frame {
                capture: capture.clone(),
                frame: counted_frame(retained, id, PixelSize::new(8, 8), damage),
            })
            .unwrap();
    }

    #[test]
    fn stalled_encoder_keeps_two_waiting_images_and_drops_replaced_newest_immediately() {
        let (sender, receiver) = source_channel();
        let capture = sender.open_capture().unwrap();
        let retained = Arc::new(RetainedImages::default());
        enqueue(&sender, &capture, &retained, 1, Some(vec![rect(0, 0)]));
        assert!(matches!(
            receiver.try_recv().unwrap(),
            SourceCmd::FramesReady { capture: woken } if woken.id == capture.id
        ));
        let encoding = sender.take_frame(capture.id).unwrap();
        let producer = sender.clone();
        let producing = capture.clone();
        let counted = retained.clone();
        let (finished, done) = mpsc::sync_channel(1);
        let join = std::thread::spawn(move || {
            for frame in 2..=22 {
                enqueue(
                    &producer,
                    &producing,
                    &counted,
                    frame,
                    Some(vec![rect((frame % 8) as i32, 1)]),
                );
                assert!(
                    counted.live() <= 3,
                    "two queued plus the stalled encoding image"
                );
            }
            finished.send(()).unwrap();
        });
        done.recv_timeout(Duration::from_secs(2)).unwrap();
        join.join().unwrap();
        assert_eq!(
            receiver.try_iter().count(),
            1,
            "one wake for the whole queued burst"
        );
        let queued = sender.frames.as_ref().unwrap().lock().unwrap();
        let queue = &queued.queues[&capture.id];
        assert_eq!(queue.iter().map(id).collect::<Vec<_>>(), vec![2, 22]);
        drop(queued);
        assert_eq!(
            *retained.dropped.lock().unwrap(),
            (3..22).collect::<Vec<_>>()
        );
        let newest = sender.take_frame(capture.id).unwrap();
        assert_eq!(id(&newest), 22);
        assert_eq!(
            newest.damage,
            Some(vec![PixelRect::new(point2(0, 1), point2(8, 2))])
        );
        assert_eq!(retained.live(), 2);
        assert_eq!(
            retained.peak(),
            4,
            "one transient incoming image is immediately replaced"
        );
        drop((encoding, newest));
        assert_eq!(retained.live(), 0);
    }

    #[test]
    fn replaced_damage_is_unioned_or_full_when_unknown_invalid_or_size_changes() {
        let retained = Arc::new(RetainedImages::default());
        for (first, second, expected) in [
            (
                Some(vec![rect(1, 2)]),
                Some(vec![rect(5, 6)]),
                Some(vec![PixelRect::new(point2(1, 2), point2(6, 7))]),
            ),
            (None, Some(vec![rect(5, 6)]), None),
            (Some(vec![rect(1, 2)]), None, None),
            (Some(vec![rect(-1, 2)]), Some(vec![rect(5, 6)]), None),
            (Some(vec![]), Some(vec![]), Some(vec![])),
        ] {
            let (sender, _receiver) = source_channel();
            let capture = sender.open_capture().unwrap();
            enqueue(&sender, &capture, &retained, 1, first);
            enqueue(&sender, &capture, &retained, 2, second);
            assert_eq!(sender.take_frame(capture.id).unwrap().damage, expected);
        }
        let mut smaller = counted_frame(&retained, 1, PixelSize::new(8, 8), Some(vec![rect(1, 1)]));
        let larger = counted_frame(&retained, 2, PixelSize::new(9, 8), Some(vec![rect(2, 2)]));
        merge_frame_damage(&mut smaller, &larger);
        assert_eq!(smaller.damage, None);
        drop((smaller, larger));
        assert_eq!(retained.live(), 0);
    }

    #[test]
    fn normal_flow_preserves_frame_and_control_order_and_stop_releases_queue() {
        let (sender, receiver) = source_channel();
        let capture = sender.open_capture().unwrap();
        let retained = Arc::new(RetainedImages::default());
        sender
            .send(SourceCmd::Start {
                capture: capture.clone(),
                projection: ProjectionId(3),
                peer: NodeId([1; 32]),
                video: false,
                region: false,
                cursor: false,
                bits_per_second: 1,
            })
            .unwrap();
        enqueue(&sender, &capture, &retained, 1, Some(vec![rect(1, 2)]));
        assert!(matches!(
            receiver.try_recv().unwrap(),
            SourceCmd::Start { capture: started, .. } if started.id == capture.id
        ));
        assert!(matches!(
            receiver.try_recv().unwrap(),
            SourceCmd::FramesReady { capture: woken } if woken.id == capture.id
        ));
        let frame = sender.take_frame(capture.id).unwrap();
        assert_eq!(id(&frame), 1);
        assert_eq!(frame.damage, Some(vec![rect(1, 2)]));
        assert!(sender.take_frame(capture.id).is_none());
        drop(frame);
        enqueue(&sender, &capture, &retained, 2, None);
        sender
            .send(SourceCmd::Cursor {
                capture: capture.clone(),
                cursor: Shape::Default,
            })
            .unwrap();
        sender.stop(&capture);
        assert_eq!(retained.live(), 0);
        assert!(matches!(
            receiver.try_recv().unwrap(),
            SourceCmd::FramesReady { .. }
        ));
        assert!(matches!(
            receiver.try_recv().unwrap(),
            SourceCmd::Cursor { .. }
        ));
        assert!(matches!(
            receiver.try_recv().unwrap(),
            SourceCmd::Stop { .. }
        ));
        assert!(sender.take_frame(capture.id).is_none());
        assert!(
            sender
                .send(SourceCmd::Frame {
                    capture: capture.clone(),
                    frame: counted_frame(&retained, 3, PixelSize::new(8, 8), None)
                })
                .is_err(),
            "a stopped capture queues nothing"
        );
        assert_eq!(retained.live(), 0);
        drop(receiver);
        let fresh = sender.open_capture().unwrap();
        assert!(
            sender
                .send(SourceCmd::Frame {
                    capture: fresh.clone(),
                    frame: counted_frame(&retained, 4, PixelSize::new(8, 8), None)
                })
                .is_err()
        );
        assert!(sender.take_frame(fresh.id).is_none());
        assert_eq!(retained.live(), 0);
    }
}

/// WP-2.46e2: what a capture's identity and gate guarantee in the encoder, followed to the
/// packets it sends.
#[cfg(test)]
mod capture_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crosspane_types::time::MonoTime;

    const P: ProjectionId = ProjectionId(20);
    const Q: ProjectionId = ProjectionId(21);
    const PEER: NodeId = NodeId([7; 32]);

    fn start(sender: &SourceSender, capture: &Capture, projection: ProjectionId) {
        sender
            .send(SourceCmd::Start {
                capture: capture.clone(),
                projection,
                peer: PEER,
                video: false,
                region: false,
                cursor: true,
                bits_per_second: 1,
            })
            .unwrap();
    }

    /// Queue an 8×8 picture of one colour; whether the capture took it.
    fn show(sender: &SourceSender, capture: &Capture, color: u8) -> bool {
        let pixels: Vec<u8> = (0..64).flat_map(|_| [color, 0, 0, 255]).collect();
        let frame = Frame::cpu(
            PixelSize::new(8, 8),
            32,
            pixels.into(),
            None,
            MonoTime::ZERO,
        );
        sender
            .send(SourceCmd::Frame {
                capture: capture.clone(),
                frame,
            })
            .is_ok()
    }

    /// Queue a one-pixel cursor of one colour; whether the capture took it.
    fn point(sender: &SourceSender, capture: &Capture, color: u8) -> bool {
        sender
            .send(SourceCmd::Cursor {
                capture: capture.clone(),
                cursor: Shape::Image(CursorImage {
                    size: PixelSize::new(1, 1),
                    hotspot: (0, 0),
                    pixels: Arc::from([color, 0, 0, 255]),
                }),
            })
            .is_ok()
    }

    /// `projection`'s pictures as sent: each one's number and the colour it shows.
    fn pictures(sent: &[Arc<[u8]>], projection: ProjectionId) -> Vec<(u64, u8)> {
        let mut decoder = TileDecoder::new();
        sent.iter()
            .filter(|data| {
                read_codec(data) == Ok(Codec::Tiles)
                    && read_header(data).unwrap().projection == projection.0
            })
            .map(|data| {
                let (header, _) = decoder.apply(data).unwrap();
                (header.seq, decoder.canvas().0[0])
            })
            .collect()
    }

    /// `projection`'s cursor shapes as sent: each one's number and colour.
    fn cursors(sent: &[Arc<[u8]>], projection: ProjectionId) -> Vec<(u64, u8)> {
        sent.iter()
            .filter(|data| read_codec(data) == Ok(Codec::Cursor))
            .map(|data| read_cursor(data).unwrap())
            .filter(|cursor| cursor.header.projection == projection.0)
            .map(|cursor| (cursor.header.seq, cursor.pixels[0]))
            .collect()
    }

    /// Class 6: a capture's stop, queued behind everything a newer capture has delivered and
    /// its start, retires that capture alone.
    #[test]
    fn a_queued_stop_retires_its_own_capture_and_nothing_newer() {
        let (sender, queued) = recorded::queued();
        let encoder = queued.run();
        let old = sender.open_capture().unwrap();
        start(&sender, &old, P);
        assert!(show(&sender, &old, 31));
        encoder.settle();
        let release = encoder.hold();
        let fresh = sender.open_capture().unwrap();
        assert!(show(&sender, &fresh, 99));
        assert!(point(&sender, &fresh, 199));
        start(&sender, &fresh, Q);
        sender.stop(&old);
        assert!(!show(&sender, &old, 63), "a stopped capture queues nothing");
        drop(release);
        encoder.settle();
        assert!(show(&sender, &fresh, 98));
        encoder.settle();
        let sent = encoder.sent();
        assert_eq!(pictures(&sent, P), [(1, 31)]);
        assert_eq!(pictures(&sent, Q), [(1, 99), (2, 98)]);
        assert_eq!(cursors(&sent, Q), [(1, 199)]);
    }

    /// Fencing: a stop doesn't return while one of the capture's packets is being handed to the
    /// network, and once it has returned nothing more of the capture goes out.
    #[test]
    fn a_stop_waits_for_its_captures_send_in_flight_and_nothing_follows() {
        let (sender, queued) = recorded::queued();
        let (encoder, paused) = queued.run_paused();
        let capture = sender.open_capture().unwrap();
        start(&sender, &capture, P);
        assert!(show(&sender, &capture, 31));
        paused.entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let (stopped, stop_returned) = mpsc::channel();
        let stopper = {
            let sender = sender.clone();
            let capture = capture.clone();
            std::thread::spawn(move || {
                sender.stop(&capture);
                stopped.send(()).unwrap();
            })
        };
        assert!(
            stop_returned
                .recv_timeout(Duration::from_millis(300))
                .is_err(),
            "the stop waits for the send in flight"
        );
        paused.release.send(()).unwrap();
        stop_returned.recv_timeout(Duration::from_secs(5)).unwrap();
        stopper.join().unwrap();
        assert!(
            !show(&sender, &capture, 32),
            "nothing queues after the stop"
        );
        assert!(
            !point(&sender, &capture, 132),
            "nothing queues after the stop"
        );
        encoder.settle();
        let sent = encoder.sent();
        assert_eq!(pictures(&sent, P), [(1, 31)]);
        assert!(cursors(&sent, P).is_empty());
    }

    /// Checked exhaustion: a projection's last picture and cursor numbers go out, and then its
    /// capture ends. Nothing wraps or starts over, not even for a replacement capture.
    #[test]
    fn a_projections_last_numbers_end_its_capture_and_never_wrap() {
        let (sender, queued) = recorded::queued();
        let encoder = queued.run();
        let last = u64::MAX;
        encoder.seed(
            P,
            Sequences {
                picture: last - 1,
                cursor: last - 1,
            },
        );
        let capture = sender.open_capture().unwrap();
        start(&sender, &capture, P);
        assert!(show(&sender, &capture, 31));
        assert!(point(&sender, &capture, 131));
        encoder.settle();
        assert!(show(&sender, &capture, 32));
        assert!(point(&sender, &capture, 132));
        encoder.settle();
        assert!(!show(&sender, &capture, 33), "the capture has ended");
        let sent = encoder.sent();
        assert_eq!(pictures(&sent, P), [(last, 31)]);
        assert_eq!(cursors(&sent, P), [(last, 131)]);
        assert_eq!(
            encoder.numbers(P),
            Some(Sequences {
                picture: last,
                cursor: last
            })
        );
        let replacement = sender.open_capture().unwrap();
        start(&sender, &replacement, P);
        assert!(show(&sender, &replacement, 34));
        assert!(point(&sender, &replacement, 134));
        encoder.settle();
        assert!(encoder.sent().is_empty(), "no numbers are left for it");
        let shown = recorded::shown(PEER, P, &sent);
        assert_eq!(shown.map(|(_, pixels)| pixels[0]), Some(31));
    }

    /// Cleanup: the end of this node's projection retires its numbers and whatever capture still
    /// fed it, and nothing of another projection's.
    #[test]
    fn a_projections_end_retires_its_numbers_and_capture_only() {
        let (sender, queued) = recorded::queued();
        let encoder = queued.run();
        let p = sender.open_capture().unwrap();
        start(&sender, &p, P);
        assert!(show(&sender, &p, 31));
        let q = sender.open_capture().unwrap();
        start(&sender, &q, Q);
        assert!(show(&sender, &q, 41));
        encoder.settle();
        sender.send(SourceCmd::Retire { projection: P }).unwrap();
        encoder.settle();
        assert_eq!(encoder.numbers(P), None);
        assert_eq!(
            encoder.numbers(Q),
            Some(Sequences {
                picture: 1,
                cursor: 0
            })
        );
        assert!(
            !show(&sender, &p, 32),
            "the ended projection's capture ended"
        );
        assert!(show(&sender, &q, 42));
        let next = sender.open_capture().unwrap();
        start(&sender, &next, P);
        assert!(show(&sender, &next, 33));
        encoder.settle();
        let sent = encoder.sent();
        assert_eq!(pictures(&sent, P), [(1, 31), (1, 33)]);
        assert_eq!(pictures(&sent, Q), [(1, 41), (2, 42)]);
    }
}

/// WP-2.46e2: the destination's numbering at the end of the range (no overflow at `u64::MAX`).
#[cfg(test)]
mod destination_numbering_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_destination_shows_a_projections_last_number_without_overflowing() {
        let projection = ProjectionId(30);
        let mut encoder = TileEncoder::new();
        let mut packet = |seq: u64, color: u8, key: bool| {
            let header = FrameHeader {
                projection: projection.0,
                seq,
                key: false,
                captured_ns: 0,
                width: 8,
                height: 8,
            };
            let pixels: Vec<u8> = (0..64).flat_map(|_| [color, 0, 0, 255]).collect();
            let mut out = Vec::new();
            encoder.encode(header, &pixels, 32, key, &mut out).unwrap();
            Arc::<[u8]>::from(out)
        };
        let first = packet(u64::MAX - 1, 31, true);
        let last = packet(u64::MAX, 32, false);
        assert!(read_header(&first).unwrap().key);
        assert!(!read_header(&last).unwrap().key, "applied in number order");
        let shown = recorded::shown(NodeId([8; 32]), projection, &[first, last]);
        assert_eq!(shown.map(|(_, pixels)| pixels[0]), Some(32));
    }
}

/// The encoder thread with the network replaced by a recorder, and the destination's own
/// numbering, for tests that follow media from capture callbacks to what a peer would show.
#[cfg(test)]
pub(crate) mod recorded {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::atomic::Ordering;

    struct Recorder {
        sent: Mutex<Sender<Arc<[u8]>>>,
        pause: Mutex<Option<(Sender<()>, Receiver<()>)>>,
    }

    impl SourceOutput for Recorder {
        fn send_media(&self, _: NodeId, data: Arc<[u8]>) -> Result<(), LinkError> {
            let pause = self.pause.lock().unwrap().take();
            if let Some((entered, release)) = pause {
                let _ = entered.send(());
                let _ = release.recv();
            }
            self.sent.lock().unwrap().send(data).unwrap();
            Ok(())
        }
    }

    /// The encoder's first send stops inside the network call, as a slow one would, until
    /// released.
    pub(crate) struct Paused {
        pub(crate) entered: Receiver<()>,
        pub(crate) release: Sender<()>,
    }

    /// Commands queue up until [`Queued::run`] starts the encoder.
    pub(crate) struct Queued {
        sender: SourceSender,
        commands: Receiver<SourceCmd>,
    }

    pub(crate) fn queued() -> (SourceSender, Queued) {
        let (sender, commands) = source_channel();
        (sender.clone(), Queued { sender, commands })
    }

    impl Queued {
        pub(crate) fn run(self) -> Encoder {
            self.run_with(None)
        }

        pub(crate) fn run_paused(self) -> (Encoder, Paused) {
            let (entered_tx, entered) = mpsc::channel();
            let (release, release_rx) = mpsc::channel();
            let encoder = self.run_with(Some((entered_tx, release_rx)));
            (encoder, Paused { entered, release })
        }

        fn run_with(self, pause: Option<(Sender<()>, Receiver<()>)>) -> Encoder {
            let (sent, packets) = mpsc::channel();
            let recorder = Recorder {
                sent: Mutex::new(sent),
                pause: Mutex::new(pause),
            };
            let frames = self.sender.frames.clone();
            let commands = self.commands;
            let worker = Worker::spawn("media-encode", move |stop| {
                let video = VideoSetup {
                    codecs: None,
                    gpu: None,
                };
                encode_loop(&commands, frames.as_ref(), &recorder, &video, stop);
                if let Some(frames) = &frames {
                    close_source_frames(frames);
                }
            });
            if let Some(frames) = &self.sender.frames {
                frames.lock().unwrap().stopping = worker.stop.clone();
            }
            Encoder {
                sender: self.sender,
                packets,
                worker,
            }
        }
    }

    pub(crate) struct Encoder {
        sender: SourceSender,
        packets: Receiver<Arc<[u8]>>,
        worker: Worker,
    }

    impl Encoder {
        /// Wait until everything queued so far has been handled and sent.
        pub(crate) fn settle(&self) {
            let (reply, done) = mpsc::channel();
            self.sender.send(SourceCmd::Barrier(reply)).unwrap();
            done.recv_timeout(Duration::from_secs(5)).unwrap();
        }

        /// Stop the encoder at its next command; what is queued until the returned sender is
        /// dropped is handled as one batch.
        pub(crate) fn hold(&self) -> Sender<()> {
            let (release, held) = mpsc::channel();
            self.sender.send(SourceCmd::Hold(held)).unwrap();
            release
        }

        /// Everything sent since the last call, in order.
        pub(crate) fn sent(&self) -> Vec<Arc<[u8]>> {
            self.packets.try_iter().collect()
        }

        /// The projection's last numbers, while the encoder keeps any.
        pub(crate) fn numbers(&self, projection: ProjectionId) -> Option<Sequences> {
            let (reply, numbers) = mpsc::channel();
            self.sender
                .send(SourceCmd::Probe { projection, reply })
                .unwrap();
            numbers.recv_timeout(Duration::from_secs(5)).unwrap()
        }

        /// Give the projection these last numbers.
        pub(crate) fn seed(&self, projection: ProjectionId, numbers: Sequences) {
            self.sender
                .send(SourceCmd::Seed {
                    projection,
                    numbers,
                })
                .unwrap();
        }
    }

    impl Drop for Encoder {
        fn drop(&mut self) {
            self.worker.stop.store(true, Ordering::Release);
            if let Some(thread) = self.worker.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// What the destination shows for `projection` of `source` after receiving `packets` in
    /// this order (its decoder thread, numbering included).
    pub(crate) fn shown(
        source: NodeId,
        projection: ProjectionId,
        packets: &[Arc<[u8]>],
    ) -> Option<Shown> {
        let ids = ProxyIds::default();
        let key = ProjectionKey { source, projection };
        ids.open(key);
        let (events, _events) = mpsc::channel();
        let video = VideoSetup {
            codecs: None,
            gpu: None,
        };
        let (destination, mut worker) = start_destination(None, ids, events, video);
        for data in packets {
            destination
                .send(DestCmd::Media {
                    peer: source,
                    data: data.clone(),
                })
                .unwrap();
        }
        let (reply, snapshot) = mpsc::channel();
        destination.send(DestCmd::Snapshot { key, reply }).unwrap();
        let shown = snapshot.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.stop.store(true, Ordering::Release);
        if let Some(thread) = worker.thread.take() {
            thread.join().unwrap();
        }
        shown
    }

    /// Whether the destination shows each of one projection's cursor `packets`, in this order.
    pub(crate) fn cursors_taken(packets: &[Arc<[u8]>]) -> Vec<bool> {
        let mut d = Decoding {
            decoder: TileDecoder::new(),
            video: None,
            last: 0,
            cursor_seq: 0,
            pending: BTreeMap::new(),
            gap_since: None,
            last_error: None,
            showing: Showing::Nothing,
            picture: Arc::default(),
            native: None,
        };
        packets
            .iter()
            .map(|data| {
                let before = d.cursor_seq;
                apply_cursor(&mut d, 1, data, None);
                d.cursor_seq != before
            })
            .collect()
    }
}
