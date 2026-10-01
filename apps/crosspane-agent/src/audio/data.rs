//! The data thread: sessions, codecs, jitter buffers and PCM rings.
//!
//! One thread owns all of it, so a stop, a close, a cancel and a late host reply are applied in a
//! single order with no locking between them. Each pass:
//!
//! 1. applies queued commands, in submission order;
//! 2. applies host replies (open completions, peer devices);
//! 3. moves received packets into their jitter buffers;
//! 4. pulls due 10 ms frames out of each jitter buffer into its playback ring;
//! 5. encodes and sends whole 10 ms frames from each started virtual speaker, and drains the PCM
//!    of every virtual speaker that has no started stream.
//!
//! It never calls the host and holds no lock across a codec call or an event callback. Before every
//! packet sent and every frame written to a playback it checks that the worker still lives and
//! that the engine has not stopped the key (`Shared::deliverable`), so a stop, a cancel or a
//! shutdown takes effect between any two steps of a pass, not only between passes.

use std::collections::VecDeque;
use std::mem;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;

use crosspane_engine::Failure;
use crosspane_engine::io::{AudioEndpoint, AudioKey};
use crosspane_media::audio::{AudioPull, Encoder, JitterBuffer};
use crosspane_platform::{
    AudioDeviceError, AudioEvent, AudioKind, AudioPlayback, PlatformError, VirtualPorts,
};
use crosspane_protocol::audio::AudioPacket;
use crosspane_protocol::link::LinkError;
use crosspane_types::audio::AUDIO_FRAME_SAMPLES;
use crosspane_types::id::NodeId;
use crosspane_types::time::MonoTime;
use rtrb::{Consumer, Producer};

use super::host::{Reply, Request};
use super::{
    AudioClock, AudioSend, Command, Counters, ExitGuard, MAX_PEERS, MAX_PULLS_PER_PASS,
    MAX_QUEUED_PACKETS, MAX_SESSIONS, MAX_TX_FRAMES_PER_PASS, RxSlot, Shared, Stage, TICK,
    WorkerEvent, bump, lock,
};

/// Interleaved samples in one 10 ms stereo frame: the unit of everything here.
const SPEAKER_FRAME: usize = AudioKind::Speaker.format().frame_samples();

/// One audio session, found by its complete key.
struct Session {
    key: AudioKey,
    state: State,
}

enum State {
    /// `open_playback` is queued or running on the host thread.
    Opening,
    /// The playback device is open; no stream runs on it.
    Opened(AudioPlayback),
    /// Packets for this key's stream are decoded into the playback.
    Receiving(Box<Receiving>),
    /// PCM from the peer's virtual speaker is encoded and sent.
    Sending(Box<Sending>),
    /// Failed (and reported once); inert until the engine stops, closes or cancels it.
    Failed,
}

struct Receiving {
    playback: AudioPlayback,
    jitter: JitterBuffer,
    frame: Vec<f32>,
}

struct Sending {
    encoder: Encoder,
    frame: Vec<f32>,
    seq: u32,
    sample_time: u64,
}

/// One peer's virtual speaker, as the engine asked for it and as the host has it.
struct PeerEntry {
    peer: NodeId,
    /// The name the engine last asked for; `None` once removal was asked for.
    desired: Option<String>,
    actual: PeerActual,
}

enum PeerActual {
    Absent,
    /// `add_peer` is queued or running, with the name it was asked with.
    Adding(String),
    Present {
        name: String,
        ports: VirtualPorts,
        /// Whether the next sample in the virtual speaker's ring is the right half of a pair whose
        /// left half was thrown away. That is a fact about the ring, which outlives any one stream,
        /// so it is kept here: across stops, restarts and cancellations. Every consumer of the
        /// ring (the idle drain, a start's discard, the sender) keeps it up to date.
        mid_pair: bool,
    },
    /// `remove_peer` is queued or running.
    Removing,
}

enum PeerAction {
    Add(String),
    Remove,
    Forget,
    Wait,
}

pub(super) struct Core {
    shared: Arc<Shared>,
    send: AudioSend,
    clock: AudioClock,
    /// At most `MAX_PEERS`.
    peers: Vec<PeerEntry>,
    /// At most `MAX_SESSIONS`.
    sessions: Vec<Session>,
    /// Keys whose `OpenPlayback` is queued or in flight on the host thread (at most
    /// `MAX_SESSIONS`: a cancelled open still counts until the host is done with it).
    open_requests: Vec<AudioKey>,
    // Scratch, allocated once.
    batch: Vec<Command>,
    /// Host replies taken from the shared queue and not yet applied. They stay owned by `Core`
    /// (popped one at a time, never moved into a local batch), so that if applying one panics, the
    /// handles in the rest are dropped with `Core`, after the exit guard has reported the death,
    /// and not while the stack unwinds past the loop that held them.
    replies: VecDeque<Reply>,
    ingest: Vec<(AudioKey, MonoTime, AudioPacket)>,
    failed: Vec<AudioKey>,
}

impl Core {
    pub(super) fn new(shared: Arc<Shared>, send: AudioSend, clock: AudioClock) -> Self {
        Core {
            shared,
            send,
            clock,
            peers: Vec::with_capacity(MAX_PEERS),
            sessions: Vec::with_capacity(MAX_SESSIONS),
            open_requests: Vec::with_capacity(MAX_SESSIONS),
            batch: Vec::with_capacity(16),
            replies: VecDeque::with_capacity(16),
            ingest: Vec::with_capacity(MAX_SESSIONS * MAX_QUEUED_PACKETS),
            failed: Vec::with_capacity(MAX_SESSIONS),
        }
    }

    pub(super) fn run(mut self) {
        // First, so that it is dropped before `self` (see `ExitGuard`): a death is reported before
        // the sessions' playback handles are stopped, which a backend may take time over.
        let _guard = ExitGuard(Arc::clone(&self.shared));
        while !self.shared.is_shutdown() {
            self.pass();
            self.shared.passes.fetch_add(1, Ordering::SeqCst);
            thread::park_timeout(TICK);
        }
        self.teardown();
    }

    fn pass(&mut self) {
        self.drain_commands();
        self.drain_replies();
        let now = (self.clock)();
        self.take_packets();
        self.playout(now);
        self.send_side();
    }

    /// Stop everything: dropping a session drops its codec state and stops its playback. (The
    /// queues were emptied, outside their locks, when the shutdown began.)
    fn teardown(&mut self) {
        self.sessions.clear();
        self.peers.clear();
        tracing::debug!(stats = ?self.shared.counters.snapshot(), "audio data thread stopped");
    }

    /// A report about an open, made only by whoever takes its entry out of the shadow: so it is
    /// made once, and never for an open the engine has cancelled.
    fn emit_open(&self, key: AudioKey, kind: AudioKind, result: Result<(), Failure>) {
        if self.shared.claim_open(key) {
            self.shared
                .emit(WorkerEvent::DeviceOpened { key, kind, result });
        }
    }

    /// A report that a stream failed or was refused, with the same rule: only for a key the engine
    /// still has started, and once.
    fn emit_failed(&self, key: AudioKey) {
        if self.shared.claim_fail(key) {
            tracing::warn!(
                peer = %key.peer.short(),
                stream = key.stream.0,
                generation = key.generation,
                "audio stream failed or refused"
            );
            self.shared.emit(WorkerEvent::StreamFailed { key });
        }
    }

    fn position(&self, key: AudioKey) -> Option<usize> {
        self.sessions.iter().position(|s| s.key == key)
    }

    // ---- Commands ----

    fn drain_commands(&mut self) {
        {
            let mut queue = lock(&self.shared.commands);
            self.batch.extend(queue.drain(..));
        }
        let mut batch = mem::take(&mut self.batch);
        for command in batch.drain(..) {
            if self.shared.is_shutdown() {
                break;
            }
            self.command(command);
            self.shared.applied.fetch_add(1, Ordering::SeqCst);
        }
        self.batch = batch;
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::AddPeer { peer, name } => self.add_peer(peer, name),
            Command::RemovePeer { peer } => self.remove_peer(peer),
            // Microphones are refused: no capture is opened and nothing is ever written to a
            // virtual microphone.
            Command::OpenCapture { key } => {
                self.emit_open(key, AudioKind::Microphone, Err(Failure::Other));
            }
            Command::OpenPlayback { key } => self.open_playback(key),
            Command::ClosePlayback { key } => self.close_playback(key),
            Command::Start {
                key,
                kind,
                endpoint,
            } => match (kind, endpoint) {
                (AudioKind::Speaker, AudioEndpoint::VirtualSpeaker) => self.start_sending(key),
                (AudioKind::Speaker, AudioEndpoint::LocalPlayback) => self.start_receiving(key),
                // Microphones (either endpoint) and any kind/endpoint mismatch.
                _ => self.emit_failed(key),
            },
            Command::Stop { key } => self.stop(key),
            Command::CancelPeer { peer } => self.cancel_peer(peer),
        }
    }

    fn open_playback(&mut self, key: AudioKey) {
        if self.position(key).is_some() {
            // The engine never repeats an open; the first one's reply stands.
            return;
        }
        if self.sessions.len() >= MAX_SESSIONS
            || self.open_requests.len() >= MAX_SESSIONS
            || !self.admits_peer(key.peer)
        {
            tracing::warn!("audio capacity reached: playback open refused");
            self.emit_open(key, AudioKind::Speaker, Err(Failure::Other));
            return;
        }
        self.push_session(Session {
            key,
            state: State::Opening,
        });
        self.open_requests.push(key);
        self.shared.host.push(Request::OpenPlayback { key });
    }

    /// Drops a device handle (stopping it). Never touches a sender, which has no device.
    fn close_playback(&mut self, key: AudioKey) {
        let Some(idx) = self.position(key) else {
            return;
        };
        if matches!(self.sessions[idx].state, State::Sending(_)) {
            return;
        }
        self.discard(idx);
    }

    fn start_receiving(&mut self, key: AudioKey) {
        // Packets are routed by (peer, stream): a second started key on the same pair would be
        // ambiguous. The engine stops the older key first, so this is a stale start.
        let ambiguous = self.sessions.iter().any(|s| {
            s.key != key
                && s.key.peer == key.peer
                && s.key.stream == key.stream
                && matches!(s.state, State::Receiving(_))
        });
        if ambiguous {
            return self.emit_failed(key);
        }
        let Some(idx) = self.position(key) else {
            // No device was opened for this key (or it was cancelled).
            return self.emit_failed(key);
        };
        match self.sessions[idx].state {
            // A repeated start, or one for a stream already reported as failed.
            State::Receiving(_) | State::Failed => return,
            State::Opened(_) => {}
            State::Opening | State::Sending(_) => return self.emit_failed(key),
        }
        let jitter = JitterBuffer::new(AudioKind::Speaker, key.stream);
        if jitter.is_ok() {
            bump(&self.shared.counters.jitters_created);
        }
        let State::Opened(playback) = mem::replace(&mut self.sessions[idx].state, State::Failed)
        else {
            return;
        };
        match jitter {
            Ok(jitter) => {
                self.sessions[idx].state = State::Receiving(Box::new(Receiving {
                    playback,
                    jitter,
                    frame: vec![0.0; SPEAKER_FRAME],
                }));
                self.rx_register(key);
                tracing::info!(
                    peer = %key.peer.short(),
                    stream = key.stream.0,
                    generation = key.generation,
                    "audio playback stream started"
                );
            }
            Err(error) => {
                // The state is already `Failed`; the handle is dropped (stopped) here.
                drop(playback);
                tracing::warn!(error = %error, "cannot create a jitter buffer");
                self.emit_failed(key);
            }
        }
    }

    fn start_sending(&mut self, key: AudioKey) {
        if let Some(idx) = self.position(key) {
            // A repeated start for the running or already failed stream is a no-op; anything
            // else on this key (a device handle) is a mismatch.
            if !matches!(self.sessions[idx].state, State::Sending(_) | State::Failed) {
                self.emit_failed(key);
            }
            return;
        }
        let busy = self.sessions.len() >= MAX_SESSIONS
            || self
                .sessions
                .iter()
                .any(|s| s.key.peer == key.peer && matches!(s.state, State::Sending(_)));
        let present = self
            .peers
            .iter()
            .any(|p| p.peer == key.peer && matches!(p.actual, PeerActual::Present { .. }));
        if busy || !present {
            return self.emit_failed(key);
        }
        let encoder = match Encoder::new(AudioKind::Speaker) {
            Ok(encoder) => encoder,
            Err(error) => {
                tracing::warn!(error = %error, "cannot create an Opus encoder");
                return self.emit_failed(key);
            }
        };
        bump(&self.shared.counters.encoders_created);
        let ports = self
            .peers
            .iter_mut()
            .find(|p| p.peer == key.peer)
            .and_then(|p| match &mut p.actual {
                PeerActual::Present {
                    ports, mid_pair, ..
                } => Some((ports, mid_pair)),
                _ => None,
            });
        let Some((ports, mid_pair)) = ports else {
            return;
        };
        // Whatever the apps played before this start is stale: never send it, down to an odd
        // sample (one half of a pair), whose other half is stale too and will be dropped when it
        // comes (`mid_pair`).
        discard_all_pcm(&mut ports.speaker_out, mid_pair, &self.shared.counters);
        self.push_session(Session {
            key,
            state: State::Sending(Box::new(Sending {
                encoder,
                frame: vec![0.0; SPEAKER_FRAME],
                seq: 0,
                sample_time: 0,
            })),
        });
        tracing::info!(
            peer = %key.peer.short(),
            stream = key.stream.0,
            generation = key.generation,
            "audio send stream started"
        );
    }

    /// Disable delivery for exactly this key. A device handle stays open until its close.
    fn stop(&mut self, key: AudioKey) {
        let Some(idx) = self.position(key) else {
            return;
        };
        match self.sessions[idx].state {
            State::Sending(_) | State::Failed => self.discard(idx),
            State::Receiving(_) => self.stop_receiving(idx),
            State::Opening | State::Opened(_) => {}
        }
    }

    fn stop_receiving(&mut self, idx: usize) {
        let key = self.sessions[idx].key;
        match mem::replace(&mut self.sessions[idx].state, State::Failed) {
            State::Receiving(receiving) => {
                let Receiving {
                    playback, jitter, ..
                } = *receiving;
                tracing::debug!(stats = ?jitter.stats(), "audio playback stream stopped");
                // The jitter buffer, its queued packets and the partial frame go here.
                self.sessions[idx].state = State::Opened(playback);
                self.rx_unregister(key);
            }
            other => self.sessions[idx].state = other,
        }
    }

    /// Remove a session and everything it owns.
    fn discard(&mut self, idx: usize) {
        let session = self.sessions.swap_remove(idx);
        match &session.state {
            State::Opening => {
                if self.shared.host.cancel_open(session.key) {
                    self.open_requests.retain(|k| *k != session.key);
                }
                // Otherwise the open is in flight: its reply will find no session and its handle
                // is dropped without a report.
            }
            State::Receiving(_) => self.rx_unregister(session.key),
            _ => {}
        }
        // `session` drops here: codec state, jitter buffer and the playback handle (stopping it).
    }

    fn push_session(&mut self, session: Session) {
        bump(&self.shared.counters.sessions_created);
        self.sessions.push(session);
    }

    /// Whether `peer` may have an open, a stream or devices: it already has one of those, or fewer
    /// than `MAX_PEERS` distinct peers do. Receive sessions count, not only virtual devices, and
    /// so do opens still outstanding at the host, even ones the engine has since cancelled.
    fn admits_peer(&self, peer: NodeId) -> bool {
        let mut seen = [NodeId([0; 32]); MAX_PEERS + 2 * MAX_SESSIONS];
        let mut count = 0;
        let known = self
            .peers
            .iter()
            .map(|p| p.peer)
            .chain(self.sessions.iter().map(|s| s.key.peer))
            // An open the engine has cancelled still occupies the host until it completes.
            .chain(self.open_requests.iter().map(|k| k.peer));
        for candidate in known {
            if candidate == peer {
                return true;
            }
            if !seen[..count].contains(&candidate) && count < seen.len() {
                seen[count] = candidate;
                count += 1;
            }
        }
        count < MAX_PEERS
    }

    fn cancel_peer(&mut self, peer: NodeId) {
        while let Some(idx) = self.sessions.iter().position(|s| s.key.peer == peer) {
            self.discard(idx);
        }
    }

    /// A started stream failed: drop what it owns and tell the engine, once.
    fn fail(&mut self, key: AudioKey) {
        let Some(idx) = self.position(key) else {
            return;
        };
        let was_receiving = match self.sessions[idx].state {
            State::Failed => return,
            State::Receiving(_) => true,
            _ => false,
        };
        // Drops the codec state, and a playback handle, which stops it.
        self.sessions[idx].state = State::Failed;
        if was_receiving {
            self.rx_unregister(key);
        }
        self.emit_failed(key);
    }

    fn rx_register(&self, key: AudioKey) {
        // Allocate before taking the lock; `packet()` then never allocates.
        let slot = RxSlot {
            key,
            queue: VecDeque::with_capacity(MAX_QUEUED_PACKETS),
        };
        lock(&self.shared.rx).slots.push(slot);
    }

    fn rx_unregister(&self, key: AudioKey) {
        let removed = {
            let mut rx = lock(&self.shared.rx);
            rx.slots
                .iter()
                .position(|slot| slot.key == key)
                .map(|idx| rx.slots.swap_remove(idx))
        };
        // Queued packets are freed outside the lock.
        drop(removed);
    }

    // ---- Peers (virtual devices) ----

    fn add_peer(&mut self, peer: NodeId, name: String) {
        let idx = match self.peers.iter().position(|p| p.peer == peer) {
            Some(idx) => {
                self.peers[idx].desired = Some(name);
                idx
            }
            None => {
                if self.peers.len() >= MAX_PEERS || !self.admits_peer(peer) {
                    tracing::warn!("audio peer capacity reached: virtual devices refused");
                    self.shared
                        .emit(WorkerEvent::Platform(AudioEvent::DeviceError {
                            peer: Some(peer),
                            kind: AudioKind::Speaker,
                            error: AudioDeviceError::Unavailable,
                        }));
                    return;
                }
                self.peers.push(PeerEntry {
                    peer,
                    desired: Some(name),
                    actual: PeerActual::Absent,
                });
                self.peers.len() - 1
            }
        };
        self.reconcile(idx);
    }

    fn remove_peer(&mut self, peer: NodeId) {
        // Its streams stop first, then the devices go.
        self.cancel_peer(peer);
        if let Some(idx) = self.peers.iter().position(|p| p.peer == peer) {
            self.peers[idx].desired = None;
            self.reconcile(idx);
        }
    }

    /// Bring one peer's devices to what the engine last asked for, one host operation at a time:
    /// at most one is outstanding per peer, so the host inbox stays bounded.
    fn reconcile(&mut self, idx: usize) {
        let entry = &mut self.peers[idx];
        let action = match (&entry.actual, &entry.desired) {
            (PeerActual::Absent, Some(name)) => PeerAction::Add(name.clone()),
            (PeerActual::Absent, None) => PeerAction::Forget,
            (PeerActual::Present { name, .. }, desired) if desired.as_ref() != Some(name) => {
                PeerAction::Remove
            }
            _ => PeerAction::Wait,
        };
        match action {
            PeerAction::Add(name) => {
                self.shared.host.push(Request::AddPeer {
                    peer: entry.peer,
                    name: name.clone(),
                });
                entry.actual = PeerActual::Adding(name);
            }
            PeerAction::Remove => {
                // The ports go before the devices do, and a stream still sending from them
                // can't go on (the engine hears about it and ends the session).
                entry.actual = PeerActual::Removing;
                let peer = entry.peer;
                self.shared.host.push(Request::RemovePeer { peer });
                while let Some(key) = self
                    .sessions
                    .iter()
                    .find(|s| s.key.peer == peer && matches!(s.state, State::Sending(_)))
                    .map(|s| s.key)
                {
                    self.fail(key);
                }
            }
            PeerAction::Forget => {
                self.peers.swap_remove(idx);
            }
            PeerAction::Wait => {}
        }
    }

    // ---- Host replies ----

    fn drain_replies(&mut self) {
        {
            let mut queue = lock(&self.shared.replies);
            self.replies.extend(queue.items.drain(..));
        }
        while let Some(reply) = self.replies.pop_front() {
            if self.shared.is_shutdown() {
                break;
            }
            self.reply(reply);
            self.shared.replies_applied.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn reply(&mut self, reply: Reply) {
        match reply {
            Reply::PlaybackOpened { key, result } => self.playback_opened(key, result),
            Reply::PeerAdded { peer, result } => self.peer_added(peer, result),
            Reply::PeerRemoved { peer, result } => self.peer_removed(peer, result),
        }
    }

    fn playback_opened(&mut self, key: AudioKey, result: Result<AudioPlayback, PlatformError>) {
        self.open_requests.retain(|k| *k != key);
        let idx = self
            .sessions
            .iter()
            .position(|s| s.key == key && matches!(s.state, State::Opening));
        // An open the caller has already cancelled counts as cancelled, whether or not the data
        // thread has applied the call: its session (if any) goes with that call.
        let idx = idx.filter(|_| self.shared.open_pending(key));
        let Some(idx) = idx else {
            // Cancelled or closed while the host was opening it: `result` drops here, which
            // stops a handle that opened late, and nothing is reported.
            bump(&self.shared.counters.stale_replies);
            return;
        };
        match result {
            Ok(playback) => {
                self.sessions[idx].state = State::Opened(playback);
                self.emit_open(key, AudioKind::Speaker, Ok(()));
            }
            Err(error) => {
                tracing::warn!(error = %error, "audio playback open failed");
                self.sessions.swap_remove(idx);
                self.emit_open(key, AudioKind::Speaker, Err(failure(&error)));
            }
        }
    }

    fn peer_added(&mut self, peer: NodeId, result: Result<VirtualPorts, PlatformError>) {
        let Some(idx) = self.peers.iter().position(|p| p.peer == peer) else {
            bump(&self.shared.counters.stale_replies);
            return;
        };
        match mem::replace(&mut self.peers[idx].actual, PeerActual::Absent) {
            PeerActual::Adding(name) => match result {
                Ok(ports) => {
                    self.peers[idx].actual = PeerActual::Present {
                        name,
                        ports,
                        mid_pair: false,
                    };
                    // The engine may have moved on while the host was busy.
                    self.reconcile(idx);
                }
                Err(error) => {
                    tracing::warn!(error = %error, "audio virtual devices could not be created");
                    self.peers.swap_remove(idx);
                    self.shared
                        .emit(WorkerEvent::Platform(AudioEvent::DeviceError {
                            peer: Some(peer),
                            kind: AudioKind::Speaker,
                            error: device_error(&error),
                        }));
                }
            },
            other => {
                self.peers[idx].actual = other;
                bump(&self.shared.counters.stale_replies);
            }
        }
    }

    fn peer_removed(&mut self, peer: NodeId, result: Result<(), PlatformError>) {
        let Some(idx) = self.peers.iter().position(|p| p.peer == peer) else {
            bump(&self.shared.counters.stale_replies);
            return;
        };
        match mem::replace(&mut self.peers[idx].actual, PeerActual::Absent) {
            PeerActual::Removing => {
                if let Err(error) = result {
                    // Treated as gone: there is nothing more this side can do about it.
                    tracing::warn!(error = %error, "audio virtual devices could not be removed");
                }
                self.reconcile(idx);
            }
            other => {
                self.peers[idx].actual = other;
                bump(&self.shared.counters.stale_replies);
            }
        }
    }

    // ---- Receive side ----

    fn take_packets(&mut self) {
        {
            let mut rx = lock(&self.shared.rx);
            for slot in rx.slots.iter_mut() {
                while let Some((arrival, packet)) = slot.queue.pop_front() {
                    self.ingest.push((slot.key, arrival, packet));
                }
            }
        }
        let mut ingest = mem::take(&mut self.ingest);
        for (key, arrival, packet) in ingest.drain(..) {
            let receiving = self
                .sessions
                .iter_mut()
                .find(|s| s.key == key)
                .and_then(|s| match &mut s.state {
                    State::Receiving(receiving) => Some(receiving),
                    _ => None,
                });
            // A slot exists only while its session is receiving, but never trust that blindly.
            let Some(receiving) = receiving else {
                continue;
            };
            if receiving.jitter.push(packet, arrival).is_err() {
                bump(&self.shared.counters.rx_rejected);
            }
        }
        self.ingest = ingest;
    }

    fn playout(&mut self, now: MonoTime) {
        let shared = &self.shared;
        let mut failed = mem::take(&mut self.failed);
        for session in self.sessions.iter_mut() {
            let State::Receiving(receiving) = &mut session.state else {
                continue;
            };
            let key = session.key;
            let Receiving {
                playback,
                jitter,
                frame,
            } = &mut **receiving;
            if !shared.deliverable(key) {
                // Stopped, cancelled or shut down, whether or not the call has been applied yet.
                continue;
            }
            if playback.pcm.is_abandoned() {
                // The backend closed the playback under us.
                failed.push(key);
                continue;
            }
            for _ in 0..MAX_PULLS_PER_PASS {
                // Before every pull: a multi-frame batch must notice a stop between its frames.
                if !shared.deliverable(key) {
                    break;
                }
                match jitter.pull(now, frame) {
                    Ok(AudioPull::Waiting) => break,
                    Ok(_) => {
                        shared.stage(Stage::Pulled, key, &frame[..]);
                        // And immediately before the write.
                        if !shared.deliverable(key) {
                            break;
                        }
                        if write_frame(&mut playback.pcm, &frame[..]) {
                            bump(&shared.counters.played);
                            shared.stage(Stage::Written, key, &frame[..]);
                        } else {
                            // Whole frames only: a full ring loses this frame, never half of it.
                            bump(&shared.counters.playback_overflow);
                        }
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "audio decoder failed");
                        failed.push(key);
                        break;
                    }
                }
            }
        }
        for key in failed.drain(..) {
            self.fail(key);
        }
        self.failed = failed;
    }

    // ---- Send side ----

    fn send_side(&mut self) {
        let mut failed = mem::take(&mut self.failed);
        {
            let Core {
                peers,
                sessions,
                send,
                shared,
                ..
            } = self;
            for entry in peers.iter_mut() {
                let PeerActual::Present {
                    ports, mid_pair, ..
                } = &mut entry.actual
                else {
                    continue;
                };
                let sender = sessions.iter_mut().find_map(|s| match &mut s.state {
                    State::Sending(sending) if s.key.peer == entry.peer => Some((s.key, sending)),
                    _ => None,
                });
                match sender {
                    // Nothing is sending: what apps play now must not be sent later.
                    None => discard_pcm(&mut ports.speaker_out, mid_pair, &shared.counters),
                    Some((key, sending)) => {
                        if !pump(ports, mid_pair, sending, key, send, shared) {
                            failed.push(key);
                        }
                    }
                }
            }
        }
        for key in failed.drain(..) {
            self.fail(key);
        }
        self.failed = failed;
    }
}

/// Encode and send every whole 10 ms frame the virtual speaker has produced (bounded per pass).
/// Returns `false` when the stream failed.
///
/// The key is checked (the worker lives and the engine has not stopped it) before each frame is
/// read, after it is read, and again immediately before it is sent, so nothing leaves once a stop,
/// a cancel or a shutdown has been requested, whichever stage the pass is in.
fn pump(
    ports: &mut VirtualPorts,
    mid_pair: &mut bool,
    sending: &mut Sending,
    key: AudioKey,
    send: &AudioSend,
    shared: &Shared,
) -> bool {
    let counters = &shared.counters;
    if !finish_stale_pair(&mut ports.speaker_out, mid_pair, counters) {
        // The stale half of the pair an earlier discard cut in two has not come yet: no audio
        // can be assembled before it does.
        return !endpoint_gone(ports);
    }
    for _ in 0..MAX_TX_FRAMES_PER_PASS {
        if !shared.deliverable(key) {
            // The session goes when the data thread applies the stop (or the death).
            return true;
        }
        // Fails when fewer than a whole frame is available: a partial frame is never encoded,
        // so left and right always stay paired.
        let Ok(chunk) = ports.speaker_out.read_chunk(SPEAKER_FRAME) else {
            break;
        };
        let (first, second) = chunk.as_slices();
        let mut repaired = false;
        for (dst, src) in sending.frame.iter_mut().zip(first.iter().chain(second)) {
            // The encoder rejects non-finite and out-of-range samples; an app clipping must not
            // kill the stream.
            *dst = if src.is_finite() {
                src.clamp(-1.0, 1.0)
            } else {
                0.0
            };
            repaired |= *dst != *src;
        }
        chunk.commit_all();
        if repaired {
            bump(&counters.sanitized_frames);
        }
        if !shared.deliverable(key) {
            return true;
        }
        let opus = match sending.encoder.encode(&sending.frame) {
            Ok(opus) => opus,
            Err(error) => {
                tracing::warn!(error = %error, "audio encoder failed");
                return false;
            }
        };
        shared.stage(Stage::Encoded, key, &sending.frame);
        // Immediately before the send.
        if !shared.deliverable(key) {
            return true;
        }
        let packet = AudioPacket {
            stream: key.stream,
            seq: sending.seq,
            sample_time: sending.sample_time,
            opus,
        };
        // A packet the link drops still used up its slot in the stream's timeline, so the
        // receiver sees the gap and conceals it.
        sending.seq = sending.seq.wrapping_add(1);
        sending.sample_time = sending
            .sample_time
            .saturating_add(AUDIO_FRAME_SAMPLES as u64);
        match send(key.peer, &packet) {
            Ok(()) => bump(&counters.sent),
            Err(LinkError::Congested) => bump(&counters.congested),
            Err(error) => {
                tracing::warn!(error = ?error, "audio send failed");
                return false;
            }
        }
    }
    !endpoint_gone(ports)
}

/// The backend dropped its end of the virtual speaker and nothing is left to send.
fn endpoint_gone(ports: &VirtualPorts) -> bool {
    ports.speaker_out.is_abandoned() && ports.speaker_out.slots() < SPEAKER_FRAME
}

/// If the ring's next sample is the right half of a pair whose left half was thrown away, throw
/// that away too. Returns `false` while that sample has not arrived yet.
fn finish_stale_pair(pcm: &mut Consumer<f32>, mid_pair: &mut bool, counters: &Counters) -> bool {
    if !*mid_pair {
        return true;
    }
    match pcm.read_chunk(1) {
        Ok(chunk) => {
            chunk.commit_all();
            *mid_pair = false;
            counters.discarded_samples.fetch_add(1, Ordering::Relaxed);
            true
        }
        Err(_) => false,
    }
}

/// Throw away the PCM waiting in an idle virtual speaker, in whole stereo frames so a sample the
/// backend is still writing keeps its channel: a lone left sample stays until its right half
/// comes, and a right half owed to an earlier discard goes first.
fn discard_pcm(pcm: &mut Consumer<f32>, mid_pair: &mut bool, counters: &Counters) {
    if !finish_stale_pair(pcm, mid_pair, counters) {
        return;
    }
    let samples = pcm.slots() & !1;
    if samples == 0 {
        return;
    }
    if let Ok(chunk) = pcm.read_chunk(samples) {
        chunk.commit_all();
        counters
            .discarded_samples
            .fetch_add(samples as u64, Ordering::Relaxed);
    }
}

/// Throw away everything in a virtual speaker at a stream's start. When that cuts a stereo pair in
/// two (an odd count), `mid_pair` flips: the pair's other half is stale too, and is dropped when it
/// comes (`finish_stale_pair`), before any audio is assembled.
fn discard_all_pcm(pcm: &mut Consumer<f32>, mid_pair: &mut bool, counters: &Counters) {
    let samples = pcm.slots();
    if samples == 0 {
        return;
    }
    if let Ok(chunk) = pcm.read_chunk(samples) {
        chunk.commit_all();
        counters
            .discarded_samples
            .fetch_add(samples as u64, Ordering::Relaxed);
        if samples % 2 == 1 {
            *mid_pair = !*mid_pair;
        }
    }
}

/// Write one whole frame, or nothing when the ring cannot take all of it.
fn write_frame(pcm: &mut Producer<f32>, frame: &[f32]) -> bool {
    match pcm.write_chunk_uninit(frame.len()) {
        Ok(chunk) => {
            let _written = chunk.fill_from_iter(frame.iter().copied());
            true
        }
        Err(_) => false,
    }
}

fn failure(error: &PlatformError) -> Failure {
    match error {
        PlatformError::Locked => Failure::Locked,
        PlatformError::SecureInput => Failure::SecureInput,
        PlatformError::PointerButtonHeld => Failure::PointerButtonHeld,
        PlatformError::PermissionDenied(_) => Failure::PermissionDenied,
        _ => Failure::Other,
    }
}

fn device_error(error: &PlatformError) -> AudioDeviceError {
    match error {
        PlatformError::Locked => AudioDeviceError::Locked,
        PlatformError::PermissionDenied(_) => AudioDeviceError::PermissionDenied,
        PlatformError::Unsupported(_) | PlatformError::NotFound => AudioDeviceError::Unavailable,
        _ => AudioDeviceError::Failed,
    }
}
