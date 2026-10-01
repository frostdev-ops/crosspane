//! The E2 data plane (docs/wp/E2-v0.md). Pixels never pass through the engine:
//!
//! - **Source:** capture frames → one encoder thread per node → `Transport::send_media`. A frame
//!   the transport refuses (`Congested`) is dropped and the next one becomes a key frame, because a
//!   lost delta would corrupt the receiver's canvas.
//! - **Destination:** media frames → a decoder thread → the proxy host. Frames are applied in
//!   `seq` order (streams can complete out of order); a gap that doesn't fill within 300 ms, or a
//!   frame that fails to apply, asks the source for a key frame through the engine.

use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crosspane_engine::{Input, ProjectionKey};
use crosspane_media::tiles::{TileDecoder, TileEncoder};
use crosspane_media::wire::{FrameHeader, read_header};
use crosspane_platform::{Frame, StreamId};
use crosspane_protocol::link::LinkError;
use crosspane_render::proxy::{HostCommand, HostHandle};
use crosspane_transport::Transport;
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
}

/// What the decoder has shown for one projection (for `crosspanectl status`).
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameStats {
    pub frames: u64,
    pub bytes: u64,
    pub last: Option<Instant>,
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
        map.stats.remove(&key);
        Some(id)
    }

    fn shown(&self, key: ProjectionKey, bytes: usize) {
        if let Ok(mut map) = self.inner.lock() {
            let stats = map.stats.entry(key).or_default();
            stats.frames += 1;
            stats.bytes += bytes as u64;
            stats.last = Some(Instant::now());
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

pub enum SourceCmd {
    /// A capture stream started for `projection`, to be sent to `peer`.
    Start {
        stream: StreamId,
        projection: ProjectionId,
        peer: NodeId,
    },
    Frame {
        stream: StreamId,
        frame: Frame,
    },
    Stop {
        stream: StreamId,
    },
    RequestKey {
        projection: ProjectionId,
    },
}

struct Encoding {
    projection: ProjectionId,
    peer: NodeId,
    encoder: TileEncoder,
    seq: u64,
}

/// Start the encoder thread; capture sinks and the engine loop send it `SourceCmd`s.
pub fn start_source(transport: Arc<Transport>) -> Sender<SourceCmd> {
    let (tx, rx) = mpsc::channel::<SourceCmd>();
    let spawned = std::thread::Builder::new()
        .name("media-encode".into())
        .spawn(move || encode_loop(&rx, &transport));
    if let Err(e) = spawned {
        tracing::error!(error = %e, "could not start the media encoder");
    }
    tx
}

fn encode_loop(rx: &Receiver<SourceCmd>, transport: &Transport) {
    let mut streams: HashMap<StreamId, Encoding> = HashMap::new();
    let mut out = Vec::new();
    while let Ok(first) = rx.recv() {
        // Only the newest frame of each stream matters: drain what queued up meanwhile.
        let mut latest: BTreeMap<StreamId, Frame> = BTreeMap::new();
        let mut handle = |cmd: SourceCmd, streams: &mut HashMap<StreamId, Encoding>| match cmd {
            SourceCmd::Start {
                stream,
                projection,
                peer,
            } => {
                streams.insert(
                    stream,
                    Encoding {
                        projection,
                        peer,
                        encoder: TileEncoder::new(),
                        seq: 0,
                    },
                );
            }
            SourceCmd::Frame { stream, frame } => {
                latest.insert(stream, frame);
            }
            SourceCmd::Stop { stream } => {
                streams.remove(&stream);
                latest.remove(&stream);
            }
            SourceCmd::RequestKey { projection } => {
                for e in streams.values_mut().filter(|e| e.projection == projection) {
                    e.encoder.request_key();
                }
            }
        };
        handle(first, &mut streams);
        while let Ok(cmd) = rx.try_recv() {
            handle(cmd, &mut streams);
        }
        for (stream, frame) in latest {
            let Some(enc) = streams.get_mut(&stream) else {
                continue;
            };
            let header = FrameHeader {
                projection: enc.projection.0,
                seq: enc.seq + 1,
                key: false,
                captured_ns: frame.at.as_nanos(),
                width: frame.size.width,
                height: frame.size.height,
            };
            out.clear();
            match enc
                .encoder
                .encode(header, &frame.pixels, frame.stride, false, &mut out)
            {
                Ok(Some(_stats)) => {
                    enc.seq += 1;
                    match transport.send_media(enc.peer, Arc::from(out.as_slice())) {
                        Ok(()) => {}
                        Err(LinkError::Congested) => {
                            // Dropped: the receiver will see a gap, and the next frame is a key frame.
                            enc.encoder.request_key();
                        }
                        Err(e) => tracing::debug!(error = ?e, "media send failed"),
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, "encode failed"),
            }
        }
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
}

struct Decoding {
    decoder: TileDecoder,
    last: u64,
    pending: BTreeMap<u64, Arc<[u8]>>,
    gap_since: Option<Instant>,
    last_error: Option<Instant>,
}

const GAP_TIMEOUT: Duration = Duration::from_millis(300);
const MAX_PENDING: usize = 8;

pub fn start_destination(
    host: Option<HostHandle>,
    ids: ProxyIds,
    engine: Sender<Event>,
) -> Sender<DestCmd> {
    let (tx, rx) = mpsc::channel::<DestCmd>();
    let spawned = std::thread::Builder::new()
        .name("media-decode".into())
        .spawn(move || decode_loop(&rx, host.as_ref(), &ids, &engine));
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
                    last: 0,
                    pending: BTreeMap::new(),
                    gap_since: None,
                    last_error: None,
                });
                if header.key && header.seq > d.last {
                    // A key frame supersedes everything older.
                    d.pending.retain(|seq, _| *seq > header.seq);
                    apply(d, key, id, &data, header.seq, host, engine, ids);
                } else if header.seq > d.last && d.pending.len() < MAX_PENDING {
                    d.pending.insert(header.seq, data);
                }
                // Apply whatever is now consecutive.
                while let Some(data) = d.pending.remove(&(d.last + 1)) {
                    let seq = d.last + 1;
                    apply(d, key, id, &data, seq, host, engine, ids);
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
) {
    match d.decoder.apply(data) {
        Ok((header, dirty)) => {
            d.last = seq.max(header.seq);
            ids.shown(key, data.len());
            if let Some(host) = host {
                let (pixels, size) = d.decoder.canvas();
                let _ = host.send(HostCommand::Frame {
                    id,
                    size,
                    pixels: Arc::from(pixels),
                    dirty,
                });
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
