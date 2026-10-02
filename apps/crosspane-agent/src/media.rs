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
//! - **Destination:** media frames → a decoder thread → the proxy host. Frames are applied in
//!   `seq` order (streams can complete out of order); a gap that doesn't fill within 300 ms, or a
//!   frame that fails to apply, asks the source for a key frame through the engine.

use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
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
use crosspane_platform::{CursorImage, Frame, StreamId};
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
            *map.presented_closed.entry(key.source).or_default() += stats.presented;
        }
        Some(id)
    }

    /// The renderer submitted one frame of `key` for presentation (WP-4.5a calls this where it
    /// does; until it does, [`ProxyIds::presented_from`] says `None`). Counts per projection;
    /// a frame of a projection that has closed isn't counted, as with the decoder's own stats.
    #[allow(dead_code)]
    pub fn presented(&self, key: ProjectionKey) {
        if let Ok(mut map) = self.inner.lock()
            && map.by_key.contains_key(&key)
        {
            map.presented_reported = true;
            map.stats.entry(key).or_default().presented += 1;
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
            .sum();
        Some(map.presented_closed.get(&source).copied().unwrap_or(0) + open)
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

pub enum SourceCmd {
    /// A capture stream started for `projection`, to be sent to `peer`.
    Start {
        stream: StreamId,
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
        stream: StreamId,
        frame: Frame,
    },
    Cursor {
        stream: StreamId,
        cursor: Shape,
    },
    Stop {
        stream: StreamId,
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
}

struct Encoding {
    projection: ProjectionId,
    peer: NodeId,
    encoder: TileEncoder,
    seq: u64,
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
    cursor_seq: u64,
    /// A frame was lost or the receiver asked for a key frame: if no new capture comes (captures
    /// arrive only on damage), the last frame goes again as a key frame.
    refresh_due: bool,
    /// When a key-frame request was last honoured (requests closer than `KEY_REQUEST_GAP` are
    /// ignored: a large key frame can take longer than the receiver's gap timeout to arrive).
    last_key_request: Option<Instant>,
}

const KEY_REQUEST_GAP: Duration = Duration::from_secs(1);
/// How long a cursor or frame that came before its stream's Start is kept.
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

/// Start the encoder thread; capture sinks and the engine loop send it `SourceCmd`s.
pub fn start_source(transport: Arc<Transport>, video: VideoSetup) -> Sender<SourceCmd> {
    let (tx, rx) = mpsc::channel::<SourceCmd>();
    let spawned = std::thread::Builder::new()
        .name("media-encode".into())
        .spawn(move || {
            crate::exit_on_panic("media encoder", || encode_loop(&rx, &transport, &video))
        });
    if let Err(e) = spawned {
        tracing::error!(error = %e, "could not start the media encoder");
    }
    tx
}

fn encode_loop(rx: &Receiver<SourceCmd>, transport: &Transport, video: &VideoSetup) {
    let mut streams: HashMap<StreamId, Encoding> = HashMap::new();
    // Cursors reported before the stream's Start arrived (the capture thread may be first).
    let mut early_cursors: HashMap<StreamId, (Shape, Instant)> = HashMap::new();
    // Frames that came before their stream's Start (the capture thread may be first): a still
    // window's first frame may be its only one.
    let mut early_frames: HashMap<StreamId, (Frame, Instant)> = HashMap::new();
    let mut out = Vec::new();
    let epoch = Instant::now();
    loop {
        let first = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(cmd) => Some(cmd),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        // Only the newest frame of each stream matters: drain what queued up meanwhile.
        let mut latest: BTreeMap<StreamId, Frame> = BTreeMap::new();
        let mut handle = |cmd: SourceCmd, streams: &mut HashMap<StreamId, Encoding>| match cmd {
            SourceCmd::Start {
                stream,
                projection,
                peer,
                video: peer_video,
                region: peer_region,
                cursor: peer_cursor,
                bits_per_second,
            } => {
                let cursor = early_cursors.remove(&stream).map(|(shape, _)| shape);
                if let Some((frame, _)) = early_frames.remove(&stream) {
                    latest.entry(stream).or_insert(frame);
                }
                streams.insert(
                    stream,
                    Encoding {
                        projection,
                        peer,
                        encoder: TileEncoder::new(),
                        seq: 0,
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
                        cursor_seq: 0,
                        refresh_due: false,
                        last_key_request: None,
                    },
                );
            }
            SourceCmd::Frame { stream, frame } => {
                if streams.contains_key(&stream) {
                    latest.insert(stream, frame);
                } else {
                    early_frames.retain(|_, (_, at)| at.elapsed() < EARLY_TTL);
                    if early_frames.len() < 8 {
                        early_frames.insert(stream, (frame, Instant::now()));
                    }
                }
            }
            SourceCmd::Cursor { stream, cursor } => match streams.get_mut(&stream) {
                Some(e) => {
                    e.cursor = Some(cursor);
                    e.cursor_dirty = true;
                }
                None => {
                    early_cursors.retain(|_, (_, at)| at.elapsed() < EARLY_TTL);
                    if early_cursors.len() < 64 {
                        early_cursors.insert(stream, (cursor, Instant::now()));
                    }
                }
            },
            SourceCmd::Stop { stream } => {
                streams.remove(&stream);
                latest.remove(&stream);
                early_cursors.remove(&stream);
                early_frames.remove(&stream);
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
        };
        if let Some(first) = first {
            handle(first, &mut streams);
        }
        while let Ok(cmd) = rx.try_recv() {
            handle(cmd, &mut streams);
        }
        for enc in streams.values_mut().filter(|e| e.cursor_dirty) {
            send_cursor(enc, transport, &mut out);
        }
        let now = epoch.elapsed();
        for (stream, frame) in latest {
            let Some(enc) = streams.get_mut(&stream) else {
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
                tracing::debug!(seq = enc.seq, "refresh of an idle stream");
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
                    tracing::debug!(seq = enc.seq, "lossless refresh after motion");
                }
            }
        }
    }
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

fn header(enc: &Encoding, frame: &Frame) -> FrameHeader {
    FrameHeader {
        projection: enc.projection.0,
        seq: enc.seq + 1,
        key: false,
        captured_ns: frame.at.as_nanos(),
        width: frame.size.width,
        height: frame.size.height,
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
    transport: &Transport,
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
    transport: &Transport,
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
    let header = header(enc, frame);
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
    transport: &Transport,
    out: &mut Vec<u8>,
) {
    out.clear();
    let header = header(enc, frame);
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
    transport: &Transport,
    out: &mut Vec<u8>,
) -> Result<(), String> {
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
    let mut header = header(enc, frame);
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
fn send_cursor(enc: &mut Encoding, transport: &Transport, out: &mut Vec<u8>) {
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
    let header = FrameHeader {
        projection: enc.projection.0,
        seq: enc.cursor_seq + 1,
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
    enc.cursor_seq += 1;
    match transport.send_media(enc.peer, Arc::from(&out[..])) {
        Ok(()) => {}
        Err(LinkError::Congested) => enc.cursor_dirty = true,
        Err(e) => tracing::debug!(error = ?e, "cursor send failed"),
    }
}

/// Send one encoded frame; a refused one is dropped and the next of either kind becomes a key.
fn send(enc: &mut Encoding, frame: &[u8], transport: &Transport) {
    enc.seq += 1;
    match transport.send_media(enc.peer, Arc::from(frame)) {
        Ok(()) => {}
        Err(LinkError::Congested) => {
            enc.encoder.request_key();
            enc.video_key = true;
            enc.refresh_due = true;
        }
        Err(e) => tracing::debug!(error = ?e, "media send failed"),
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
) -> Sender<DestCmd> {
    let (tx, rx) = mpsc::channel::<DestCmd>();
    let spawned = std::thread::Builder::new()
        .name("media-decode".into())
        .spawn(move || {
            crate::exit_on_panic("media decoder", || {
                decode_loop(&rx, host.as_ref(), &ids, &engine, &video)
            })
        });
    if let Err(e) = spawned {
        tracing::error!(error = %e, "could not start the media decoder");
    }
    tx
}

fn decode_loop(
    rx: &Receiver<DestCmd>,
    host: Option<&HostHandle>,
    ids: &ProxyIds,
    engine: &Sender<Event>,
    video: &VideoSetup,
) {
    let mut decoders: HashMap<ProjectionKey, Decoding> = HashMap::new();
    loop {
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
                // Apply whatever is now consecutive.
                while let Some(data) = d.pending.remove(&(d.last + 1)) {
                    let seq = d.last + 1;
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
        ids.presented(key(1, 1));
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
        for _ in 0..3 {
            ids.presented(key(1, 1));
        }
        ids.presented(key(1, 2));
        ids.presented(key(2, 1));
        assert_eq!(ids.presented_from(NodeId([1; 32])), Some(4));
        assert_eq!(ids.presented_from(NodeId([2; 32])), Some(1));
    }

    #[test]
    fn closing_a_projection_never_lowers_its_peers_total() {
        let ids = ProxyIds::default();
        ids.open(key(1, 1));
        ids.presented(key(1, 1));
        ids.presented(key(1, 1));
        assert_eq!(ids.presented_from(NodeId([1; 32])), Some(2));
        ids.close(key(1, 1));
        assert_eq!(ids.presented_from(NodeId([1; 32])), Some(2));
        // A later projection from the same peer adds to it.
        ids.open(key(1, 2));
        ids.presented(key(1, 2));
        assert_eq!(ids.presented_from(NodeId([1; 32])), Some(3));
        // A frame of a projection that is gone isn't counted.
        ids.close(key(1, 2));
        ids.presented(key(1, 2));
        assert_eq!(ids.presented_from(NodeId([1; 32])), Some(3));
    }
}
