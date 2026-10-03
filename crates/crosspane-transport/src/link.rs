//! The [`PeerLink`] the engine sends through.
//!
//! A handle is bound to a *logical link*: the sequence of events the engine saw for one peer from
//! its `Hello` to its `Closed`. When a duplicate connection resolves (rule 7), the logical link
//! moves to the surviving connection and existing handles keep working. After the link closes, its
//! handles stay closed for good, even if the peer reconnects: the engine must fetch a new handle.
//!
//! Sends never block. Frames are queued per stream with a byte cap; a peer that stops reading
//! until the cap is hit loses its connection instead of growing memory without bound. Media frames
//! have a budget of their own and are refused as congested instead (see [`crate::media`]).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crosspane_protocol::audio::{AudioPacket, encode_audio, grants_for_features};
use crosspane_protocol::clip::{ClipDataHeader, grants_for_clip};
use crosspane_protocol::link::{LinkError, PeerLink};
use crosspane_protocol::msg::{ControlMessage, InputMessage, PointerMessage};
use crosspane_protocol::wire::{WireError, encode_control, encode_input, encode_pointer};
use crosspane_types::id::NodeId;
use quinn::{Connection, SendDatagramError, VarInt};
use tokio::runtime::Handle;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::clip;
use crate::hub::{CODE_NORMAL, CODE_OVERFLOW};
use crate::media::{self, SendBudget};

/// Queued input-stream bytes at which the connection is closed.
const INPUT_QUEUE_CAP: usize = 1024 * 1024;
/// Queued control-stream bytes at which the connection is closed.
const CONTROL_QUEUE_CAP: usize = 8 * 1024 * 1024;
/// How long a graceful close waits for queued data to be acknowledged.
pub(crate) const FLUSH_TIMEOUT: Duration = Duration::from_millis(250);

/// The `Hello.features` entry that negotiates audio (D8), and the static reason audio is refused
/// on a connection that did not negotiate it.
const AUDIO_FEATURE: &str = "audio";
pub(crate) const AUDIO_UNAVAILABLE: &str = "audio is unavailable";

/// The control messages that belong to the audio feature: only a connection whose Hello exchange
/// negotiated audio (both sides advertised it) may carry them, in either direction.
pub(crate) fn is_audio_control(msg: &ControlMessage) -> bool {
    matches!(
        msg,
        ControlMessage::AudioOpen { .. }
            | ControlMessage::AudioOpened { .. }
            | ControlMessage::AudioRefused { .. }
            | ControlMessage::AudioClose { .. }
    )
}

/// What a stream's writer task receives.
pub(crate) enum Out {
    /// Bytes to write: one or more complete frames.
    Data(Vec<u8>),
    /// Everything queued so far is written: finish the stream, wait for the peer to acknowledge
    /// it, then report back.
    Finish(oneshot::Sender<()>),
}

/// The sending side of one stream's queue.
#[derive(Clone)]
pub(crate) struct Queue {
    tx: UnboundedSender<Out>,
    queued: Arc<AtomicUsize>,
    cap: usize,
}

/// The writer task's side of a queue.
pub(crate) struct QueueRx {
    pub(crate) rx: UnboundedReceiver<Out>,
    queued: Arc<AtomicUsize>,
}

impl QueueRx {
    /// A frame of `len` bytes has been written out.
    pub(crate) fn written(&self, len: usize) {
        self.queued.fetch_sub(len, Ordering::Relaxed);
    }
}

pub(crate) enum PushError {
    /// The writer is gone.
    Closed,
    /// The queue would exceed its cap.
    Overflow,
}

impl Queue {
    pub(crate) fn new(cap: usize) -> (Queue, QueueRx) {
        let (tx, rx) = unbounded_channel();
        let queued = Arc::new(AtomicUsize::new(0));
        (
            Queue {
                tx,
                queued: queued.clone(),
                cap,
            },
            QueueRx { rx, queued },
        )
    }

    pub(crate) fn push(&self, frame: Vec<u8>) -> Result<(), PushError> {
        let len = frame.len();
        if self.queued.fetch_add(len, Ordering::Relaxed) + len > self.cap {
            self.queued.fetch_sub(len, Ordering::Relaxed);
            return Err(PushError::Overflow);
        }
        self.tx.send(Out::Data(frame)).map_err(|_| {
            self.queued.fetch_sub(len, Ordering::Relaxed);
            PushError::Closed
        })
    }
}

/// What the senders need from the connection currently carrying a logical link.
#[derive(Clone)]
pub(crate) struct ConnTx {
    pub(crate) conn: Connection,
    /// Frames for the control stream's writer task.
    pub(crate) control: Queue,
    /// Frames for the input stream's writer task.
    pub(crate) input: Queue,
    /// What the peer has not yet acknowledged of the media frames sent on this connection.
    media: Arc<SendBudget>,
    /// Admission belongs to this connection and starts disabled until its first Hello.
    pub(crate) audio_enabled: Arc<AtomicBool>,
    pub(crate) clip: Arc<clip::Plane>,
    /// Serializes the datagram space check and enqueue across all handles of this connection.
    datagram_send: Arc<Mutex<()>>,
    /// The runtime the connection's tasks run on, for the graceful close.
    rt: Handle,
}

// `Connection` and the queues have no useful Debug output; keep it short.
impl std::fmt::Debug for ConnTx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConnTx { .. }")
    }
}

impl ConnTx {
    /// Queues for a new connection, and the receiving ends for its writer tasks. Must be called
    /// inside the tokio runtime.
    pub(crate) fn new(conn: Connection) -> (ConnTx, QueueRx, QueueRx) {
        let (control, control_rx) = Queue::new(CONTROL_QUEUE_CAP);
        let (input, input_rx) = Queue::new(INPUT_QUEUE_CAP);
        let tx = ConnTx {
            conn,
            control,
            input,
            media: Arc::new(SendBudget::default()),
            audio_enabled: Arc::new(AtomicBool::new(false)),
            clip: clip::Plane::new(),
            datagram_send: Arc::new(Mutex::new(())),
            rt: Handle::current(),
        };
        (tx, control_rx, input_rx)
    }

    /// Best effort, without evicting previously queued motion or audio.
    fn send_datagram(&self, datagram: Vec<u8>) -> Result<(), LinkError> {
        // Never wait on another sender. All ConnTx clones share this connection's guard so
        // motion and audio cannot both reserve the same free space and evict queued datagrams.
        let _guard = self
            .datagram_send
            .try_lock()
            .map_err(|_| LinkError::Congested)?;
        match self.conn.max_datagram_size() {
            None => return Err(LinkError::Invalid("the peer does not accept datagrams")),
            Some(max) if datagram.len() > max => return Err(LinkError::Congested),
            Some(_) => {}
        }
        // quinn would silently drop older queued datagrams to make room. Report a full buffer
        // instead so the caller sees the congestion.
        if self.conn.datagram_send_buffer_space() < datagram.len() {
            return Err(LinkError::Congested);
        }
        self.conn
            .send_datagram(datagram.into())
            .map_err(|error| match error {
                SendDatagramError::TooLarge => LinkError::Congested,
                SendDatagramError::ConnectionLost(_) => LinkError::Closed,
                _ => LinkError::Invalid("datagrams are unavailable"),
            })
    }

    fn queue(&self, queue: &Queue, frame: Vec<u8>) -> Result<(), LinkError> {
        match queue.push(frame) {
            Ok(()) => Ok(()),
            Err(PushError::Closed) => Err(LinkError::Closed),
            Err(PushError::Overflow) => {
                // The peer isn't reading. Don't let the queue grow without bound.
                tracing::warn!("send queue overflow; closing the connection");
                self.conn
                    .close(VarInt::from_u32(CODE_OVERFLOW), b"send queue overflow");
                Err(LinkError::Closed)
            }
        }
    }

    pub(crate) fn queue_control(&self, frame: Vec<u8>) -> Result<(), LinkError> {
        self.queue(&self.control, frame)
    }

    /// Queue a control message the engine sent, applying what this connection negotiated. Audio
    /// control messages are refused unless audio is negotiated, and a `Grants` message loses its
    /// audio capabilities then: a peer without the feature may not know them, and its decoder
    /// rejects unknown capabilities. Decided here, at the actual enqueue on this connection, so
    /// a link handle that moved to a surviving connection (a silent supersession) is filtered by
    /// the survivor's own Hello and never by the connection it replaced.
    pub(crate) fn send_control(&self, msg: &ControlMessage) -> Result<(), LinkError> {
        let audio = self.audio_enabled.load(Ordering::Acquire);
        if is_audio_control(msg) && !audio {
            return Err(LinkError::Invalid(AUDIO_UNAVAILABLE));
        }
        let filtered;
        let msg = match msg {
            ControlMessage::Grants(grants) => {
                let negotiated: Vec<String> = audio
                    .then(|| AUDIO_FEATURE.to_owned())
                    .into_iter()
                    .collect();
                filtered = ControlMessage::Grants(grants_for_clip(
                    &grants_for_features(grants, &negotiated),
                    self.clip.available(),
                ));
                &filtered
            }
            _ => msg,
        };
        let mut frame = Vec::new();
        encode_control(msg, &mut frame).map_err(invalid)?;
        self.queue_control(frame)
    }

    pub(crate) fn queue_input(&self, frame: Vec<u8>) -> Result<(), LinkError> {
        self.queue(&self.input, frame)
    }

    /// Send `frame` on a stream of its own. `Congested` if the peer has too much unfinished media
    /// (the frame is not sent); `Invalid` if the peer would refuse a frame this large.
    pub(crate) fn send_media(&self, frame: Arc<[u8]>) -> Result<(), LinkError> {
        if frame.len() > media::MAX_FRAME {
            return Err(LinkError::Invalid("media frame too large"));
        }
        let (projection, key) = media::frame_info(&frame);
        let reservation = self.media.reserve(projection, key, frame.len())?;
        // The runtime handle, not `tokio::spawn`, so the engine may call from any thread.
        self.rt
            .spawn(media::write_frame(self.conn.clone(), frame, reservation));
        Ok(())
    }

    pub(crate) fn send_clip_data(
        &self,
        header: ClipDataHeader,
        data: Arc<[u8]>,
    ) -> Result<(), LinkError> {
        let work = self.clip.sending(header, data.len())?;
        self.rt
            .spawn(clip::write(self.conn.clone(), header, data, work));
        Ok(())
    }

    /// Finish both streams once their queued data is out, give the peer a moment to acknowledge it,
    /// then close the connection with `message` (application code 0). Everything queued before this
    /// call is delivered unless the peer is unresponsive.
    pub(crate) fn flush_and_close(self, message: String) -> JoinHandle<()> {
        let rt = self.rt.clone();
        rt.spawn(async move {
            let mut waits = Vec::new();
            for queue in [&self.control, &self.input] {
                let (done, wait) = oneshot::channel();
                if queue.tx.send(Out::Finish(done)).is_ok() {
                    waits.push(wait);
                }
            }
            let _ = timeout(FLUSH_TIMEOUT, async {
                for wait in waits {
                    let _ = wait.await;
                }
            })
            .await;
            self.conn
                .close(VarInt::from_u32(CODE_NORMAL), message.as_bytes());
        })
    }
}

/// Shared by the registry and every [`QuicLink`] of one logical link.
#[derive(Debug)]
pub(crate) struct LinkCell {
    /// `None` once the logical link has closed.
    current: Mutex<Option<ConnTx>>,
    /// The engine asked to close: further sends fail even while the flush is still in flight.
    closing: AtomicBool,
}

impl LinkCell {
    pub(crate) fn new(tx: ConnTx) -> Arc<Self> {
        Arc::new(Self {
            current: Mutex::new(Some(tx)),
            closing: AtomicBool::new(false),
        })
    }

    /// Route the link over another connection (duplicate resolution).
    pub(crate) fn set(&self, tx: ConnTx) {
        if self.closing.load(Ordering::Relaxed) {
            // The engine already closed this link; the survivor goes too.
            tx.conn
                .close(VarInt::from_u32(CODE_NORMAL), b"closed by the local engine");
        }
        if let Some(old) = self
            .current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(tx)
        {
            old.clip.retire();
        }
    }

    /// Close the logical link for good.
    pub(crate) fn clear(&self) {
        if let Some(old) = self
            .current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            old.clip.retire();
        }
    }

    /// Retire clipboard work even after QUIC closes, before registry/session cleanup runs.
    pub(crate) fn cancel_clip(&self) {
        if let Some(tx) = self
            .current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            tx.clip.cancel();
        }
    }

    /// Start a graceful close: nothing more can be sent, queued data is flushed, then the
    /// connection closes. `None` if the link was already closing or closed.
    pub(crate) fn begin_close(&self, message: &str) -> Option<JoinHandle<()>> {
        if self.closing.swap(true, Ordering::Relaxed) {
            return None;
        }
        let tx = self
            .current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()?;
        tx.clip.retire();
        Some(tx.flush_and_close(message.to_owned()))
    }

    /// Send a media frame over the live connection.
    pub(crate) fn send_media(&self, frame: Arc<[u8]>) -> Result<(), LinkError> {
        self.live()?.send_media(frame)
    }

    /// The live connection, if the link is open and the connection hasn't closed.
    pub(crate) fn live(&self) -> Result<ConnTx, LinkError> {
        if self.closing.load(Ordering::Relaxed) {
            return Err(LinkError::Closed);
        }
        let tx = self
            .current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or(LinkError::Closed)?;
        if tx.conn.close_reason().is_some() {
            return Err(LinkError::Closed);
        }
        Ok(tx)
    }
}

/// The engine's send handle for one peer.
#[derive(Debug)]
pub(crate) struct QuicLink {
    peer: NodeId,
    cell: Arc<LinkCell>,
}

impl QuicLink {
    pub(crate) fn new(peer: NodeId, cell: Arc<LinkCell>) -> Self {
        Self { peer, cell }
    }
}

fn invalid(error: WireError) -> LinkError {
    match error {
        WireError::TooLarge { .. } => LinkError::Invalid("message too large"),
        _ => LinkError::Invalid("message cannot be encoded"),
    }
}

impl PeerLink for QuicLink {
    fn peer(&self) -> NodeId {
        self.peer
    }

    fn send_input(&mut self, msg: &InputMessage) -> Result<(), LinkError> {
        let mut frame = Vec::new();
        encode_input(msg, &mut frame).map_err(invalid)?;
        self.cell.live()?.queue_input(frame)
    }

    fn send_motion(&mut self, msg: &PointerMessage) -> Result<(), LinkError> {
        let datagram = encode_pointer(msg).map_err(invalid)?;
        let tx = self.cell.live()?;
        tx.send_datagram(datagram)
    }

    fn send_audio(&mut self, packet: &AudioPacket) -> Result<(), LinkError> {
        let tx = self.cell.live()?;
        if !tx.audio_enabled.load(Ordering::Acquire) {
            return Err(LinkError::Invalid(AUDIO_UNAVAILABLE));
        }
        let datagram = encode_audio(packet).map_err(invalid)?;
        tx.send_datagram(datagram)
    }

    fn send_control(&mut self, msg: &ControlMessage) -> Result<(), LinkError> {
        self.cell.live()?.send_control(msg)
    }

    fn rtt(&self) -> Option<Duration> {
        // The handshake itself yields an RTT sample, so an established connection always has one.
        self.cell.live().ok().map(|tx| tx.conn.rtt())
    }

    fn remote_addr(&self) -> Option<std::net::SocketAddr> {
        self.cell.live().ok().map(|tx| tx.conn.remote_address())
    }

    fn close(&mut self, message: &str) {
        let _ = self.cell.begin_close(message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn contended_connection_guard_refuses_motion_and_audio_from_other_handles() {
        use crate::common::{Node, Raw, identity};
        use crosspane_protocol::audio::AudioStreamId;
        use crosspane_types::geom::PointDevice;
        use crosspane_types::id::{DisplayId, SessionId};

        let local = identity();
        let remote = identity();
        let node = Node::start("remote", remote.clone(), &[&local]);
        let raw = Raw::new();
        let conn = raw.connect(&local, &remote, node.addr()).await;
        let (tx, _control_rx, _input_rx) = ConnTx::new(conn);
        tx.audio_enabled.store(true, Ordering::Release);
        let cell = LinkCell::new(tx.clone());
        let mut motion_handle = QuicLink::new(node.id, cell.clone());
        let mut audio_handle = QuicLink::new(node.id, cell);
        let motion = PointerMessage {
            session: SessionId(1),
            seq: 3,
            display: DisplayId(1),
            position: PointDevice::new(2.0, 4.0),
        };
        let audio = AudioPacket {
            stream: AudioStreamId(7),
            seq: 19,
            sample_time: 48_000,
            opus: vec![0x5a; 400],
        };
        motion_handle.send_motion(&motion).unwrap();
        let space = tx.conn.datagram_send_buffer_space();
        let guard = tx.datagram_send.lock().unwrap();
        let (mut motion_handle, mut audio_handle) = std::thread::spawn({
            let audio = audio.clone();
            move || {
                assert_eq!(
                    motion_handle.send_motion(&motion),
                    Err(LinkError::Congested)
                );
                assert_eq!(audio_handle.send_audio(&audio), Err(LinkError::Congested));
                (motion_handle, audio_handle)
            }
        })
        .join()
        .unwrap();
        // The current-thread runtime has not drained the previously queued motion. Neither
        // contended send enqueued anything or evicted it, despite ample remaining space.
        assert_eq!(tx.conn.datagram_send_buffer_space(), space);
        drop(guard);
        motion_handle.send_motion(&motion).unwrap();
        audio_handle.send_audio(&audio).unwrap();
        // Quinn also budgets per-datagram bookkeeping, so assert the payload lower bound.
        assert!(
            tx.conn.datagram_send_buffer_space()
                <= space
                    - encode_pointer(&motion).unwrap().len()
                    - encode_audio(&audio).unwrap().len()
        );
    }

    #[test]
    fn a_queue_refuses_frames_past_its_cap_and_frees_room_as_they_are_written() {
        let (queue, mut rx) = Queue::new(100);
        assert!(queue.push(vec![0; 60]).is_ok());
        assert!(matches!(queue.push(vec![0; 60]), Err(PushError::Overflow)));
        assert!(queue.push(vec![0; 40]).is_ok());
        assert!(matches!(queue.push(vec![0; 1]), Err(PushError::Overflow)));

        let Some(Out::Data(first)) = rx.rx.try_recv().ok() else {
            panic!("expected data");
        };
        rx.written(first.len());
        assert!(queue.push(vec![0; 60]).is_ok());
    }

    #[test]
    fn a_queue_with_no_writer_reports_closed() {
        let (queue, rx) = Queue::new(100);
        drop(rx);
        assert!(matches!(queue.push(vec![0; 10]), Err(PushError::Closed)));
        // The failed push did not leak budget.
        let (queue, rx) = Queue::new(10);
        drop(rx);
        assert!(matches!(queue.push(vec![0; 10]), Err(PushError::Closed)));
        assert!(matches!(queue.push(vec![0; 10]), Err(PushError::Closed)));
    }
}
