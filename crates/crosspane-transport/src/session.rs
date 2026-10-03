//! One connection's tasks: a writer per outgoing stream and one reader that owns everything
//! arriving (both streams, datagrams, closure). A single reader keeps a connection's events
//! ordered and makes every protocol-error decision in one place.
//!
//! Media frames are the exception to "the reader reads": each media stream is read by a task of its
//! own, so a large frame never delays control or input. The reader only collects the finished
//! frames (a `JoinSet` that dies with the session) and delivers them.

use std::collections::VecDeque;
use std::future::pending;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use std::time::{Duration, Instant};

use crosspane_protocol::audio::decode_audio;

use crosspane_protocol::clip::CLIP_FEATURE;
use crosspane_protocol::link::{LinkError, LinkEvent};
use crosspane_protocol::msg::{ControlMessage, InputMessage};
use crosspane_protocol::wire::{
    FrameDecoder, HEADER_LEN, KIND_AUDIO, KIND_POINTER, MAX_CONTROL_PAYLOAD, MAX_INPUT_PAYLOAD,
    WireError, decode_control, decode_input, decode_pointer,
};
use crosspane_types::id::NodeId;
use quinn::{Chunk, Connection, ConnectionError, ReadError, ReadExactError, RecvStream, VarInt};
use tokio::task::{JoinError, JoinSet};
use tokio::time::{MissedTickBehavior, Sleep, interval, sleep};

use crate::clip;
use crate::hub::{
    Activity, CODE_NORMAL, CODE_PROTOCOL_ERROR, DUPLICATE_GRACE, Inner, MAX_HOLD, Role, SETTLE,
    is_duplicate_close,
};
use crate::link::{AUDIO_UNAVAILABLE, Out, QueueRx, is_audio_control};
use crate::media::{self, Received, STREAM_MEDIA};

/// First byte of a unidirectional stream: the control channel.
const STREAM_CONTROL: u8 = 0x01;
/// First byte of a unidirectional stream: the input channel.
const STREAM_INPUT: u8 = 0x02;
/// Send priorities (higher is sent first): input over control (03 §2).
const PRIORITY_INPUT: i32 = 100;
const PRIORITY_CONTROL: i32 = 50;

/// At most this many input-stream messages are delivered per second per connection (04 §3).
const INPUT_LIMIT: usize = 2_000;
const INPUT_WINDOW: Duration = Duration::from_secs(1);

/// A peer that doesn't send its `Hello` this soon after the handshake is broken.
const HELLO_DEADLINE: Duration = Duration::from_secs(5);
/// A peer that finishes a stream must close the connection within this long.
const FINISHED_STREAM_GRACE: Duration = Duration::from_secs(2);
/// How often a held connection re-checks for a competing handshake.
const HOLD_POLL: Duration = Duration::from_millis(25);
/// How often the reader notes whether the connection is still receiving.
const ACTIVITY_TICK: Duration = Duration::from_millis(250);

/// Largest read from a stream at once.
const READ_CHUNK: usize = 64 * 1024;
/// A writer coalesces already-queued frames up to about this many bytes per write.
const WRITE_BATCH: usize = 16 * 1024;

/// A stream the peer opened, with its type byte consumed.
pub(crate) type FirstStream = (u8, RecvStream);

pub(crate) enum AcceptError {
    /// The connection closed.
    Closed(ConnectionError),
    /// The peer misbehaved opening the stream.
    Fault(&'static str),
}

/// Accept the peer's next unidirectional stream and read its type byte.
///
/// Not cancel-safe once a stream has been accepted: callers keep the future alive until it
/// completes.
pub(crate) async fn accept_stream(conn: Connection) -> Result<FirstStream, AcceptError> {
    let mut recv = conn.accept_uni().await.map_err(AcceptError::Closed)?;
    let mut kind = [0u8; 1];
    match recv.read_exact(&mut kind).await {
        Ok(()) => Ok((kind[0], recv)),
        Err(ReadExactError::FinishedEarly(_)) => {
            Err(AcceptError::Fault("stream ended before its type byte"))
        }
        Err(ReadExactError::ReadError(ReadError::ConnectionLost(error))) => {
            Err(AcceptError::Closed(error))
        }
        Err(ReadExactError::ReadError(_)) => {
            Err(AcceptError::Fault("stream reset before its type byte"))
        }
    }
}

/// Everything a registered connection's tasks need.
pub(crate) struct Start {
    pub(crate) inner: Arc<Inner>,
    pub(crate) conn: Connection,
    pub(crate) conn_id: u64,
    pub(crate) peer: NodeId,
    pub(crate) control_rx: QueueRx,
    pub(crate) input_rx: QueueRx,
    pub(crate) first: Option<FirstStream>,
    /// The connection can still lose a duplicate race: don't read or announce anything until the
    /// hold ends.
    pub(crate) hold: bool,
    pub(crate) role: Role,
    pub(crate) activity: Arc<Activity>,
    pub(crate) audio_enabled: Arc<AtomicBool>,
    pub(crate) clip: Arc<clip::Plane>,
}

/// Start the tasks of a registered connection.
pub(crate) fn spawn(start: Start) {
    let Start {
        inner,
        conn,
        conn_id,
        peer,
        control_rx,
        input_rx,
        first,
        hold,
        role,
        activity,
        audio_enabled,
        clip,
    } = start;
    tokio::spawn(write_stream(
        conn.clone(),
        STREAM_CONTROL,
        PRIORITY_CONTROL,
        control_rx,
    ));
    tokio::spawn(write_stream(
        conn.clone(),
        STREAM_INPUT,
        PRIORITY_INPUT,
        input_rx,
    ));
    let session = Session {
        guard: Guard {
            inner: inner.clone(),
            conn: conn.clone(),
            peer,
            conn_id,
            armed: true,
        },
        inner,
        conn,
        conn_id,
        peer,
        streams: Streams::default(),
        hello_seen: false,
        audio_enabled,
        clip,
        settled: !hold,
        role,
        held_since: Instant::now(),
        limiter: RateLimiter::new(INPUT_LIMIT, INPUT_WINDOW),
        last_drop_warning: None,
        dropped: 0,
        activity,
        rx_datagrams: 0,
        finish_timer: None,
        media: JoinSet::new(),
        media_buffered: Arc::new(AtomicUsize::new(0)),
        clips: JoinSet::new(),
    };
    tokio::spawn(session.run(first));
}

/// Write one outgoing stream. If it fails while the connection is still open (the peer sent
/// STOP_SENDING, say) the link is half dead, so the connection is closed.
async fn write_stream(conn: Connection, kind: u8, priority: i32, mut queue: QueueRx) {
    if run_writer(&conn, kind, priority, &mut queue).await.is_err() && conn.close_reason().is_none()
    {
        tracing::debug!("a stream write failed; closing the connection");
        conn.close(VarInt::from_u32(CODE_PROTOCOL_ERROR), b"stream error");
    }
}

async fn run_writer(
    conn: &Connection,
    kind: u8,
    priority: i32,
    queue: &mut QueueRx,
) -> Result<(), ()> {
    let mut send = conn.open_uni().await.map_err(|_| ())?;
    send.set_priority(priority).map_err(|_| ())?;
    // The type byte goes out at once, so the peer sees both streams without waiting for traffic.
    send.write_all(&[kind]).await.map_err(|_| ())?;
    while let Some(first) = queue.rx.recv().await {
        let mut batch = Vec::new();
        let mut finish = None;
        match first {
            Out::Data(data) => batch = data,
            Out::Finish(done) => finish = Some(done),
        }
        // Coalesce what is already queued, stopping at a finish.
        while finish.is_none() && batch.len() < WRITE_BATCH {
            match queue.rx.try_recv() {
                Ok(Out::Data(data)) => batch.extend_from_slice(&data),
                Ok(Out::Finish(done)) => finish = Some(done),
                Err(_) => break,
            }
        }
        if !batch.is_empty() {
            send.write_all(&batch).await.map_err(|_| ())?;
            queue.written(batch.len());
        }
        if let Some(done) = finish {
            // Everything queued before the close is written: end the stream and wait for the peer
            // to acknowledge it, so the connection close that follows doesn't discard it.
            let _ = send.finish();
            let _ = send.stopped().await;
            let _ = done.send(());
            return Ok(());
        }
    }
    Ok(())
}

/// A peer's stream being decoded.
struct Incoming {
    recv: RecvStream,
    decoder: FrameDecoder,
    /// The peer finished the stream.
    finished: bool,
}

#[derive(Default)]
struct Streams {
    control: Option<Incoming>,
    input: Option<Incoming>,
}

impl Streams {
    /// Attach a newly accepted stream by its type. Anything but one control and one input stream
    /// is a protocol error.
    fn install(&mut self, kind: u8, recv: RecvStream) -> Result<(), &'static str> {
        let (slot, max_payload, duplicate) = match kind {
            STREAM_CONTROL => (
                &mut self.control,
                MAX_CONTROL_PAYLOAD,
                "second control stream",
            ),
            STREAM_INPUT => (&mut self.input, MAX_INPUT_PAYLOAD, "second input stream"),
            _ => return Err("unknown stream type"),
        };
        if slot.is_some() {
            return Err(duplicate);
        }
        *slot = Some(Incoming {
            recv,
            decoder: FrameDecoder::new(max_payload),
            finished: false,
        });
        Ok(())
    }
}

/// The next chunk of a stream, or never if there is no such stream yet (or it has finished).
async fn read_chunk(stream: &mut Option<Incoming>) -> Result<Option<Chunk>, ReadError> {
    match stream {
        Some(incoming) if !incoming.finished => incoming.recv.read_chunk(READ_CHUNK, true).await,
        _ => pending().await,
    }
}

/// Wait for an optional timer, or forever if there is none.
async fn wait_for(timer: &mut Option<Pin<Box<Sleep>>>) {
    match timer {
        Some(timer) => timer.as_mut().await,
        None => pending().await,
    }
}

/// How a connection's reader ended.
enum Outcome {
    /// The connection closed (locally, by the peer, or by timeout).
    Closed(ConnectionError),
    /// The peer broke the protocol; the reason is for the engine's `Closed` event.
    Fault(&'static str),
}

/// Makes sure a reader that dies without finishing (a panic in the event sink, say) still closes
/// its connection and ends the peer's link, so no half-dead connection is left behind.
struct Guard {
    inner: Arc<Inner>,
    conn: Connection,
    peer: NodeId,
    conn_id: u64,
    armed: bool,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.conn
            .close(VarInt::from_u32(CODE_NORMAL), b"internal error");
        // The sink may be what panicked; don't let it take this destructor down too.
        let _ = catch_unwind(AssertUnwindSafe(|| {
            self.inner
                .finish(self.peer, self.conn_id, LinkError::Closed);
        }));
    }
}

struct Session {
    inner: Arc<Inner>,
    conn: Connection,
    conn_id: u64,
    peer: NodeId,
    streams: Streams,
    /// This connection's `Hello` has arrived. Input and motion wait for it so the engine always
    /// sees the `Hello` first.
    hello_seen: bool,
    audio_enabled: Arc<AtomicBool>,
    clip: Arc<clip::Plane>,
    /// Whether we dialed this connection or the peer did.
    role: Role,
    /// When the hold began.
    held_since: Instant,
    /// The connection is past its hold: its link is visible and it is read. Until then nothing is
    /// read, so no event (not even a protocol error) comes from a connection that may be a
    /// duplicate about to close.
    settled: bool,
    limiter: RateLimiter,
    last_drop_warning: Option<Instant>,
    dropped: u64,
    activity: Arc<Activity>,
    /// UDP datagrams received when last sampled.
    rx_datagrams: u64,
    /// Set once the peer finishes a stream: the connection must close soon after.
    finish_timer: Option<Pin<Box<Sleep>>>,
    /// The tasks reading the peer's media streams. Dropping them (when the session ends) aborts
    /// whatever is still unfinished.
    media: JoinSet<Received>,
    /// Bytes of partly received media across `media`.
    media_buffered: Arc<AtomicUsize>,
    clips: JoinSet<Option<clip::Received>>,
    guard: Guard,
}

impl Session {
    async fn run(mut self, first: Option<FirstStream>) {
        let outcome = self.drive(first).await;
        match outcome {
            Outcome::Closed(error) => {
                tracing::debug!(peer = %self.peer.short(), %error, "connection closed");
                if is_duplicate_close(&error)
                    && self.inner.mark_duplicate_closed(self.peer, self.conn_id)
                {
                    // The peer kept another connection to us; let it take this link over before
                    // the engine hears anything.
                    sleep(DUPLICATE_GRACE).await;
                }
                self.inner
                    .finish(self.peer, self.conn_id, LinkError::Closed);
            }
            Outcome::Fault(reason) => {
                self.inner.log_fault(self.peer, reason);
                self.conn
                    .close(VarInt::from_u32(CODE_PROTOCOL_ERROR), b"protocol error");
                self.inner
                    .finish(self.peer, self.conn_id, LinkError::Invalid(reason));
            }
        }
        self.guard.armed = false;
        self.clip.retire();
    }

    async fn drive(&mut self, first: Option<FirstStream>) -> Outcome {
        if let Some((kind, recv)) = first
            && let Some(outcome) = self.on_stream(kind, recv)
        {
            return outcome;
        }
        // Futures that must survive across loop iterations (they hold a half-accepted stream or a
        // registration) are pinned outside the select; the reads inside it are cancel-safe.
        let mut accepting = Box::pin(accept_stream(self.conn.clone()));
        let closed_conn = self.conn.clone();
        let mut closed = Box::pin(async move { closed_conn.closed().await });
        let mut settle_timer = Box::pin(sleep(SETTLE));
        let mut hello_deadline = Box::pin(sleep(HELLO_DEADLINE));
        let mut tick = interval(ACTIVITY_TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            // Biased: data the peer sent before closing is delivered before the closure is noticed.
            tokio::select! {
                biased;
                chunk = read_chunk(&mut self.streams.control), if self.settled => {
                    if let Some(outcome) = self.on_control(chunk) {
                        return outcome;
                    }
                }
                chunk = read_chunk(&mut self.streams.input), if self.settled && self.hello_seen => {
                    if let Some(outcome) = self.on_input(chunk) {
                        return outcome;
                    }
                }
                datagram = self.conn.read_datagram(), if self.settled && self.hello_seen => {
                    match datagram {
                        Ok(datagram) => {
                            if let Some(outcome) = self.on_datagram(&datagram) {
                                return outcome;
                            }
                        }
                        Err(error) => return Outcome::Closed(error),
                    }
                }
                // Before `closed`, so a frame that finished before the connection closed is delivered.
                Some(done) = self.media.join_next(), if !self.media.is_empty() => {
                    if let Some(outcome) = self.on_media(done) {
                        return outcome;
                    }
                }
                Some(Ok(Some(received))) = self.clips.join_next(), if !self.clips.is_empty() => {
                    self.inner.deliver_clip(self.peer, self.conn_id, received);
                }
                error = &mut closed => return Outcome::Closed(error),
                accepted = &mut accepting, if self.settled => match accepted {
                    Ok((kind, recv)) => {
                        if let Some(outcome) = self.on_stream(kind, recv) {
                            return outcome;
                        }
                        accepting = Box::pin(accept_stream(self.conn.clone()));
                    }
                    Err(AcceptError::Closed(error)) => return Outcome::Closed(error),
                    Err(AcceptError::Fault(reason)) => return Outcome::Fault(reason),
                },
                _ = tick.tick() => self.sample_activity(),
                _ = &mut settle_timer, if !self.settled => {
                    // Keep holding while a competing handshake with this peer is still in
                    // progress; it may be about to win.
                    let competing = self.inner.competitor_in_flight(self.conn.remote_address(), self.role);
                    if competing && self.held_since.elapsed() < MAX_HOLD {
                        settle_timer = Box::pin(sleep(HOLD_POLL));
                    } else {
                        self.inner.settle(self.peer, self.conn_id);
                        self.settled = true;
                    }
                }
                _ = &mut hello_deadline, if !self.hello_seen => {
                    return Outcome::Fault("no hello within the deadline");
                }
                () = wait_for(&mut self.finish_timer) => {
                    return Outcome::Fault("stream finished without the connection closing");
                }
            }
        }
    }

    fn on_datagram(&self, datagram: &[u8]) -> Option<Outcome> {
        // Only inspect the kind once the bounded header is present. The frozen codecs
        // validate version, reserved bytes and exact lengths before any payload allocation.
        let event = match datagram.get(..HEADER_LEN).map(|header| header[1]) {
            Some(KIND_AUDIO) => {
                if !self.audio_enabled.load(Ordering::Acquire) {
                    return Some(Outcome::Fault(AUDIO_UNAVAILABLE));
                }
                let Ok(packet) = decode_audio(datagram) else {
                    return Some(Outcome::Fault("malformed audio datagram"));
                };
                LinkEvent::Audio {
                    peer: self.peer,
                    packet,
                }
            }
            Some(KIND_POINTER) => {
                let Ok(msg) = decode_pointer(datagram) else {
                    return Some(Outcome::Fault("malformed pointer datagram"));
                };
                LinkEvent::Motion {
                    peer: self.peer,
                    msg,
                }
            }
            // Keep the existing pointer protocol-error category for unknown/truncated data.
            _ => return Some(Outcome::Fault("malformed pointer datagram")),
        };
        self.inner.deliver(self.peer, self.conn_id, event);
        None
    }

    /// Note whether the connection received anything since the last sample, so a replacement
    /// decision can tell a live connection from a silent one.
    fn sample_activity(&mut self) {
        let received = self.conn.stats().udp_rx.datagrams;
        if received != self.rx_datagrams {
            self.rx_datagrams = received;
            self.activity.touch();
        }
    }

    /// The peer opened a stream. Control and input attach to their slot (anything else of those
    /// kinds is a protocol error); each media frame gets a reading task of its own.
    fn on_stream(&mut self, kind: u8, recv: RecvStream) -> Option<Outcome> {
        if kind == clip::STREAM_CLIP {
            if !self.hello_seen || !self.clip.available() || self.clips.len() >= clip::MAX_TASKS {
                let mut recv = recv;
                let _ = recv.stop(VarInt::from_u32(4));
            } else {
                self.clips
                    .spawn(clip::read(recv, self.clip.clone(), self.peer));
            }
            return None;
        }
        if kind != STREAM_MEDIA {
            return self.streams.install(kind, recv).err().map(Outcome::Fault);
        }
        // The engine hears of a peer's `Hello` before anything else it sends. A peer only has media
        // to send once the other side has accepted a projection, which takes control messages that
        // follow the `Hello`: media before it is not a peer we can serve.
        if !self.hello_seen {
            return Some(Outcome::Fault("media before hello"));
        }
        self.media
            .spawn(media::read_frame(recv, self.media_buffered.clone()));
        None
    }

    /// A media task ended: deliver its frame.
    fn on_media(&mut self, done: Result<Received, JoinError>) -> Option<Outcome> {
        match done {
            Ok(Received::Frame(data)) => {
                self.inner.deliver(
                    self.peer,
                    self.conn_id,
                    LinkEvent::Media {
                        peer: self.peer,
                        data,
                    },
                );
                None
            }
            Ok(Received::Dropped) => None,
            Ok(Received::Fault(reason)) => Some(Outcome::Fault(reason)),
            Err(error) => {
                tracing::debug!(peer = %self.peer.short(), %error, "a media task failed");
                None
            }
        }
    }

    /// The peer finished a stream. That is how a graceful close starts, so it is fine, but the
    /// connection must then actually close.
    fn note_finished(&mut self) {
        if self.finish_timer.is_none() {
            self.finish_timer = Some(Box::pin(sleep(FINISHED_STREAM_GRACE)));
        }
    }

    fn on_control(&mut self, chunk: Result<Option<Chunk>, ReadError>) -> Option<Outcome> {
        let chunk = match chunk {
            Ok(Some(chunk)) => chunk,
            Ok(None) => {
                if let Some(incoming) = self.streams.control.as_mut() {
                    incoming.finished = true;
                }
                self.note_finished();
                return None;
            }
            Err(error) => return Some(read_failure(error, "control stream reset")),
        };
        let incoming = self.streams.control.as_mut()?;
        incoming.decoder.push(&chunk.bytes);
        loop {
            let frame = match incoming.decoder.next_frame() {
                Ok(Some(frame)) => frame,
                Ok(None) => return None,
                Err(_) => return Some(Outcome::Fault("malformed control frame")),
            };
            match decode_control(&frame) {
                Ok(ControlMessage::Hello(hello)) if !self.hello_seen => {
                    self.audio_enabled.store(
                        self.inner.local_audio
                            && hello.features.iter().any(|feature| feature == "audio"),
                        Ordering::Release,
                    );
                    self.clip.enabled.store(
                        self.inner.local_clip && hello.features.iter().any(|f| f == CLIP_FEATURE),
                        Ordering::Release,
                    );
                    self.hello_seen = true;
                    self.inner.deliver_hello(self.peer, self.conn_id, hello);
                }
                // The engine is promised exactly one `Hello`, and before anything else.
                Ok(ControlMessage::Hello(_)) => return Some(Outcome::Fault("second hello")),
                Ok(_) if !self.hello_seen => {
                    return Some(Outcome::Fault("first control message was not a hello"));
                }
                // Audio control messages need audio negotiated on this very connection (both
                // Hellos advertised it). Anything else is a protocol error, and is not delivered.
                Ok(msg)
                    if is_audio_control(&msg) && !self.audio_enabled.load(Ordering::Acquire) =>
                {
                    return Some(Outcome::Fault(AUDIO_UNAVAILABLE));
                }
                Ok(msg) => self.inner.deliver(
                    self.peer,
                    self.conn_id,
                    LinkEvent::Control {
                        peer: self.peer,
                        msg,
                    },
                ),
                // A newer peer's message this version doesn't know: skip it.
                Err(WireError::UnknownControl) => {}
                Err(_) => return Some(Outcome::Fault("malformed control message")),
            }
        }
    }

    fn on_input(&mut self, chunk: Result<Option<Chunk>, ReadError>) -> Option<Outcome> {
        let chunk = match chunk {
            Ok(Some(chunk)) => chunk,
            Ok(None) => {
                if let Some(incoming) = self.streams.input.as_mut() {
                    incoming.finished = true;
                }
                self.note_finished();
                return None;
            }
            Err(error) => return Some(read_failure(error, "input stream reset")),
        };
        let incoming = self.streams.input.as_mut()?;
        incoming.decoder.push(&chunk.bytes);
        loop {
            let frame = match incoming.decoder.next_frame() {
                Ok(Some(frame)) => frame,
                Ok(None) => return None,
                Err(_) => return Some(Outcome::Fault("malformed input frame")),
            };
            let Ok(msg) = decode_input(&frame) else {
                return Some(Outcome::Fault("malformed input message"));
            };
            let now = Instant::now();
            if self.limiter.record(now, never_dropped(&msg)) {
                self.inner.deliver(
                    self.peer,
                    self.conn_id,
                    LinkEvent::Input {
                        peer: self.peer,
                        msg,
                    },
                );
            } else {
                self.dropped += 1;
                let due = self
                    .last_drop_warning
                    .is_none_or(|last| now.duration_since(last) >= INPUT_WINDOW);
                if due {
                    tracing::warn!(
                        peer = %self.peer.short(),
                        dropped = self.dropped,
                        limit = INPUT_LIMIT,
                        "input rate limit exceeded; dropping messages"
                    );
                    self.last_drop_warning = Some(now);
                    self.dropped = 0;
                }
            }
        }
    }
}

fn read_failure(error: ReadError, reason: &'static str) -> Outcome {
    match error {
        ReadError::ConnectionLost(error) => Outcome::Closed(error),
        _ => Outcome::Fault(reason),
    }
}

/// Messages the rate limit must never drop: a lost release leaves a key or button stuck down, and
/// a lost heartbeat or acknowledgement makes the peer think the link is dead (04 §8).
fn never_dropped(msg: &InputMessage) -> bool {
    matches!(
        msg,
        InputMessage::Key { down: false, .. }
            | InputMessage::Button { down: false, .. }
            | InputMessage::State { .. }
            | InputMessage::Ack { .. }
    )
}

/// Allows at most `limit` events in any window of length `window`, except that some events are
/// exempt: they are always allowed, and still count toward the limit.
#[derive(Debug)]
struct RateLimiter {
    limit: usize,
    window: Duration,
    /// Times of the `limit` most recent events, oldest first.
    recent: VecDeque<Instant>,
}

impl RateLimiter {
    fn new(limit: usize, window: Duration) -> Self {
        Self {
            limit,
            window,
            recent: VecDeque::with_capacity(limit),
        }
    }

    /// Record an event at `now`. `false` means the event is over the limit and must be dropped
    /// (never for an exempt event, which is recorded either way).
    fn record(&mut self, now: Instant, exempt: bool) -> bool {
        if self.recent.len() >= self.limit {
            // The oldest of the `limit` most recent events is the limit-th most recent: if it is
            // still inside the window, so are all of them.
            let full = self
                .recent
                .front()
                .is_some_and(|oldest| now.duration_since(*oldest) < self.window);
            if full && !exempt {
                return false;
            }
            self.recent.pop_front();
        }
        self.recent.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limiter_allows_the_limit_then_refuses() {
        let start = Instant::now();
        let mut limiter = RateLimiter::new(3, Duration::from_secs(1));
        assert!(limiter.record(start, false));
        assert!(limiter.record(start, false));
        assert!(limiter.record(start, false));
        assert!(!limiter.record(start, false));
        assert!(!limiter.record(start + Duration::from_millis(999), false));
    }

    #[test]
    fn limiter_recovers_as_the_window_slides() {
        let start = Instant::now();
        let mut limiter = RateLimiter::new(2, Duration::from_secs(1));
        assert!(limiter.record(start, false));
        assert!(limiter.record(start + Duration::from_millis(600), false));
        assert!(!limiter.record(start + Duration::from_millis(900), false));
        // The first event left the window; the second has not.
        assert!(limiter.record(start + Duration::from_millis(1000), false));
        assert!(!limiter.record(start + Duration::from_millis(1100), false));
        assert!(limiter.record(start + Duration::from_millis(1600), false));
    }

    #[test]
    fn limiter_never_exceeds_the_limit_in_any_window() {
        let start = Instant::now();
        let mut limiter = RateLimiter::new(5, Duration::from_secs(1));
        let mut allowed = Vec::new();
        for step in 0..300u64 {
            let at = start + Duration::from_millis(step * 10);
            if limiter.record(at, false) {
                allowed.push(at);
            }
        }
        for (index, from) in allowed.iter().enumerate() {
            let in_window = allowed[index..]
                .iter()
                .take_while(|at| at.duration_since(*from) < Duration::from_secs(1))
                .count();
            assert!(in_window <= 5, "{in_window} events within one window");
        }
        assert!(
            allowed.len() >= 5 * 2,
            "the limiter starved: {}",
            allowed.len()
        );
    }

    #[test]
    fn exempt_events_are_always_allowed_and_still_use_up_the_budget() {
        let start = Instant::now();
        let mut limiter = RateLimiter::new(3, Duration::from_secs(1));
        // Ordinary events fill the budget; exempt ones go through regardless.
        assert!(limiter.record(start, false));
        assert!(limiter.record(start, false));
        assert!(limiter.record(start, false));
        assert!(!limiter.record(start, false));
        for _ in 0..10 {
            assert!(limiter.record(start, true));
        }
        // Exempt events counted, so ordinary ones are still refused...
        assert!(!limiter.record(start + Duration::from_millis(500), false));
        // ...until the whole window has passed.
        assert!(limiter.record(start + Duration::from_millis(1000), false));
    }

    #[test]
    fn exempt_events_starve_ordinary_ones_but_never_themselves() {
        let start = Instant::now();
        let mut limiter = RateLimiter::new(2, Duration::from_secs(1));
        for step in 0..50u64 {
            assert!(limiter.record(start + Duration::from_millis(step), true));
        }
        assert!(!limiter.record(start + Duration::from_millis(60), false));
        assert!(limiter.record(start + Duration::from_millis(60), true));
    }

    #[test]
    fn releases_heartbeats_and_acks_are_exempt() {
        use crosspane_types::hid::{HidUsage, MouseButton};
        use crosspane_types::id::SessionId;
        let session = SessionId(1);
        let up = InputMessage::Key {
            session,
            seq: 1,
            usage: HidUsage::keyboard(4),
            down: false,
        };
        let down = InputMessage::Key {
            session,
            seq: 2,
            usage: HidUsage::keyboard(4),
            down: true,
        };
        let button_up = InputMessage::Button {
            session,
            seq: 3,
            button: MouseButton::PRIMARY,
            down: false,
        };
        let button_down = InputMessage::Button {
            session,
            seq: 4,
            button: MouseButton::PRIMARY,
            down: true,
        };
        let state = InputMessage::State {
            session,
            seq: 5,
            held_keys: Vec::new(),
            held_buttons: Vec::new(),
        };
        let ack = InputMessage::Ack { session, seq: 6 };
        assert!(never_dropped(&up));
        assert!(never_dropped(&button_up));
        assert!(never_dropped(&state));
        assert!(never_dropped(&ack));
        assert!(!never_dropped(&down));
        assert!(!never_dropped(&button_down));
    }
}
