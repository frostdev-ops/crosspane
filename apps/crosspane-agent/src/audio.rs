//! The agent's audio data plane for speakers (speaker v0, WP-3.6c; `docs/wp/WP-3.6c.md`).
//!
//! The worker executes the engine's audio [`Output`]s off the engine loop: it encodes a virtual
//! speaker's PCM to Opus and sends it, and receives, jitter-buffers and decodes a peer's packets
//! into a physical playback. Microphones are refused outright: no capture is ever opened and no
//! virtual microphone is ever written.
//!
//! # Threads
//!
//! - **Host thread** (`host.rs`): the only thread that calls the [`AudioHost`] (`subscribe`,
//!   `add_peer`, `remove_peer`, `open_playback`; each bounded to 2 s by the trait). It blocks on
//!   its own inbox and nothing else ever waits for it.
//! - **Data thread** (`data.rs`): owns every session, codec, jitter buffer and PCM ring, and runs
//!   a pass at least every [`TICK`] (4 ms) of real time: commands, host replies, received packets,
//!   playout, then the send side. It never calls the host and never waits on a lock for longer than
//!   a few moves.
//! - **Callers** (`submit`, `packet`, `cancel_peer`) only take a short lock to append to a queue
//!   (and, for `packet`, to read the clock), then return.
//!
//! # Queues and their bounds
//!
//! | Queue | Direction | Bound |
//! |---|---|---|
//! | `commands` | callers to data thread | drained every pass; [`COMMAND_HARD_CAP`] |
//! | `host.queue` | data to host thread | at most 4 peer operations (one per peer) plus 8 opens |
//! | `replies` | host to data thread | at most as many as the host queue |
//! | `rx` slot | `packet` to data thread | [`MAX_QUEUED_PACKETS`] per stream, then drop |
//!
//! The data thread admits a host request only against its own capacity tables (4 peers, 8
//! sessions, 8 outstanding opens), so the two host-side queues are bounded by construction. The
//! command path must also be lossless while the worker lives: dropping a stop or a close would
//! leave audio running. So [`COMMAND_SOFT_CAP`] is only a high-water mark, counted and logged, and
//! no command is ever refused. The data thread empties the queue every pass and never waits for a
//! host call, so a sane engine cannot get near it. If the queue nevertheless reaches
//! [`COMMAND_HARD_CAP`], the data thread is stuck (in a callback, say) and the worker dies; see
//! "Death" below.
//!
//! # Stale keys
//!
//! Every session, request and reply carries the complete [`AudioKey`] (peer, stream and admission
//! generation, which is never reused). The data thread is the only mutator of the session table, so
//! a stop, close, cancel or late host reply is resolved by an exact key lookup: a key that is no
//! longer in the table is stale by definition, whatever stream ID a newer key reuses. A late
//! successful open for such a key has its handle dropped and is never reported.
//!
//! A received [`AudioPacket`] carries no generation, only a stream ID, so a packet cannot be told
//! apart from an older key's once a newer key reuses its stream ID. That is safe because AUDIO-v0
//! §8 forbids the reuse (since WP-3.0b a stream ID is never reused for a peer for the life of the
//! agent process), and, as a second line, the jitter buffer rejects a packet whose sequence number
//! and sample clock do not match the stream it is playing.
//!
//! # The caller-side shadow and immediate invalidation
//!
//! The data thread applies a stop, close, remove or cancel in order, which can be a few
//! milliseconds after the call. So the caller keeps a [`Shadow`] of what the engine believes is
//! going on: the keys it started and has not stopped (`live`) and the opens it asked for and has
//! not been told about (`opens`). `submit` and `cancel_peer` update it before queueing anything,
//! and the data thread consults it immediately before every packet sent, every playback frame
//! written and every report, between pipeline stages and before each pull of a multi-frame playout
//! batch. A key that is not `live` delivers nothing, and a report is made only by whoever removes
//! its entry from the shadow, which also makes each report happen exactly once. `cancel_peer` and
//! `shutdown` are therefore effective the moment they return, apart from a delivery already past
//! its last check.
//!
//! The shadow's lock is also what orders a request against a death (below): admitting a request
//! (checking that the worker lives, updating the shadow and queueing the command) is one step
//! under that lock, and so is the death's own transition (marking the worker dead and taking
//! everything the shadow holds). A request is therefore either in the shadow when the death takes
//! it, and reported by the death, or it finds the worker dead and is rejected outright, which
//! reports it as failed at once (an open as `DeviceOpened{Err(Failure::Other)}`, a start as
//! `StreamFailed`; a stop, close, cancel, remove or add reports nothing). Nothing can slip
//! between the two and be left with no report.
//!
//! # Death
//!
//! A worker that dies must never do so silently. The worker dies when the command queue reaches
//! [`COMMAND_HARD_CAP`], when either of its threads panics or ends for any reason other than a
//! caller's `shutdown`, or when the platform-event callback panics. The hard cap and the 2.5 s
//! shutdown bound are lead decisions recorded in `docs/wp/WP-3.6c.md` under "Lead decisions". The
//! context that detects the death (the caller in `submit` for the cap, the dying thread's unwind
//! guard, the backend thread for the callback; never a stuck thread) then:
//!
//! 1. marks the worker dead and takes everything the shadow holds, in one step (see above);
//! 2. stops all delivery and wakes the other thread;
//! 3. reports `WorkerEvent::StreamFailed` for every live key and `WorkerEvent::DeviceOpened` with
//!    `Err(Failure::Other)` for every pending open, each exactly once;
//! 4. only then drops everything queued: every command not yet applied is discarded (it is already
//!    reflected in the shadow), and every completed open waiting in the reply queue has its handle
//!    stopped.
//!
//! The reports come before anything that could block. Each thread's unwind guard lives inside the
//! function that owns its resources (the host in `host::run`, the sessions and playback handles in
//! `Core::run`) and is declared before them, so it runs, and reports, before a slow destructor of
//! the host or of a playback handle can delay it.
//!
//! In the cap case the `events` callback is called from within `submit`. A caller's `shutdown` is
//! not a death: it reports nothing, and later calls are ignored.
//!
//! # Shutdown
//!
//! `shutdown` stops all delivery at once, then waits for each thread by polling
//! `JoinHandle::is_finished` until [`SHUTDOWN_BUDGET`] (2.5 s: a compliant host call can take up to
//! 2 s) after it was entered; that includes the time spent dropping the handles that were still
//! queued. It never calls `join`, because a join also waits for thread-local destructors, which is
//! an unbounded wait. A thread that has not finished by the deadline is detached with a warning.
//!
//! # Callbacks
//!
//! The `events` callback runs on the data thread (for `DeviceOpened` and `StreamFailed`), on a
//! backend thread (for `Platform`) or, at a death, on the context that detected it, never with a
//! worker lock held, so it may call `submit` or `cancel_peer`. It must not block, because the data
//! thread waits for it.
//!
//! Logs and counters never contain samples or packet payloads.

mod data;
mod host;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::mem;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant};

use crosspane_engine::io::{AudioEndpoint, AudioKey};
use crosspane_engine::{Failure, Output};
use crosspane_platform::{AudioEvent, AudioHost, EventSink};
use crosspane_protocol::audio::AudioPacket;
use crosspane_protocol::link::LinkError;
use crosspane_types::audio::AudioKind;
use crosspane_types::id::NodeId;
use crosspane_types::time::MonoTime;

/// The data thread wakes at least this often (the spec asks for 5 ms at most).
const TICK: Duration = Duration::from_millis(4);
/// Peers with virtual devices, an open or a stream at once.
const MAX_PEERS: usize = 4;
/// Audio sessions (physical playbacks and virtual-speaker senders) at once.
const MAX_SESSIONS: usize = 8;
/// Packets queued between `packet()` and the data thread, per stream.
const MAX_QUEUED_PACKETS: usize = 8;
/// A pass encodes at most this many 10 ms frames per sender, so one stream cannot starve the rest.
const MAX_TX_FRAMES_PER_PASS: usize = 16;
/// A pass pulls at most this many 10 ms frames per receiver (the jitter buffer rebases beyond 8
/// late slots anyway).
const MAX_PULLS_PER_PASS: usize = 8;
/// High-water mark of the command queue: counted and logged, never a drop threshold (see the
/// module docs).
const COMMAND_SOFT_CAP: usize = 1024;
/// If the command queue ever reaches this, the data thread is not draining it (it is stuck in a
/// callback, say): the worker dies instead of growing without bound or losing a command.
const COMMAND_HARD_CAP: usize = 4096;
/// `shutdown` waits this long for each thread. Host calls are bounded to 2 s by the trait, so
/// 2.5 s joins any compliant one.
const SHUTDOWN_BUDGET: Duration = Duration::from_millis(2500);
/// `start` waits this long for the host thread's `subscribe` (bounded to 2 s by the trait).
const HOST_START_BUDGET: Duration = Duration::from_millis(2500);

/// What the worker reports to the agent loop, which feeds the engine.
#[derive(Debug)]
pub enum WorkerEvent {
    /// Feed `Input::AudioDeviceOpened`. Exactly one per `OpenAudioPlayback`/`OpenAudioCapture`
    /// that wasn't cancelled first; a late success for a cancelled key is closed, never reported.
    DeviceOpened {
        key: AudioKey,
        kind: AudioKind,
        result: Result<(), Failure>,
    },
    /// Feed `Input::AudioStreamFailed`: a started stream failed (encoder/decoder error, endpoint
    /// gone, playback closed by the backend).
    StreamFailed { key: AudioKey },
    /// Feed `Input::Audio` (virtual-device activity and device errors from the host).
    Platform(AudioEvent),
}

/// Sends one audio datagram to a peer (the agent passes the transport's link; tests pass a fake).
pub type AudioSend = Arc<dyn Fn(NodeId, &AudioPacket) -> Result<(), LinkError> + Send + Sync>;
/// The node's monotonic clock (`platform::now` in the agent; a fake clock in tests).
pub type AudioClock = Arc<dyn Fn() -> MonoTime + Send + Sync>;

/// The agent-facing handle of the worker's threads.
pub struct AudioWorker {
    shared: Arc<Shared>,
    clock: AudioClock,
    threads: Mutex<Option<Threads>>,
}

impl fmt::Debug for AudioWorker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AudioWorker")
            .field("shutdown", &self.shared.is_shutdown())
            .finish_non_exhaustive()
    }
}

impl AudioWorker {
    /// Nonblocking latest-set delivery, ordered against death like other caller submissions.
    pub(crate) fn set_peer_sources(&self, peer: NodeId, pids: &[u32]) {
        let shadow = lock(&self.shared.shadow);
        if self.shared.closing.load(Ordering::SeqCst) || shadow.dead {
            return;
        }
        self.shared.host.push_sources(peer, pids);
    }
    /// Start the worker's threads and subscribe to `host`'s events.
    pub fn start(
        host: Box<dyn AudioHost>,
        send: AudioSend,
        clock: AudioClock,
        events: Box<dyn Fn(WorkerEvent) + Send + Sync>,
    ) -> Result<AudioWorker, String> {
        let shared = Arc::new(Shared::new(Arc::from(events)));
        let sink: Arc<dyn EventSink<AudioEvent>> = {
            let shared = Arc::clone(&shared);
            Arc::new(move |event: AudioEvent| shared.platform_event(event))
        };

        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let host_thread = spawn_thread("cp-audio-host", {
            let shared = Arc::clone(&shared);
            move || host::run(host, shared, sink, ready_tx)
        })?;
        match ready_rx.recv_timeout(HOST_START_BUDGET) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                // The host thread returns right after reporting; reap it.
                wait_finished(host_thread, Instant::now() + SHUTDOWN_BUDGET);
                return Err(error);
            }
            Err(_) => {
                shared.begin_shutdown();
                return Err("the audio host did not answer subscribe in time".into());
            }
        }

        let data_thread = spawn_thread("cp-audio-data", {
            let shared = Arc::clone(&shared);
            let clock = Arc::clone(&clock);
            move || data::Core::new(shared, send, clock).run()
        });
        let data_thread = match data_thread {
            Ok(thread) => thread,
            Err(error) => {
                shared.begin_shutdown();
                wait_finished(host_thread, Instant::now() + SHUTDOWN_BUDGET);
                return Err(error);
            }
        };
        // Wake-ups are an optimisation (the data thread polls every `TICK` regardless).
        let _ = shared.data_thread.set(data_thread.thread().clone());
        Ok(AudioWorker {
            shared,
            clock,
            threads: Mutex::new(Some(Threads {
                data: data_thread,
                host: host_thread,
            })),
        })
    }

    /// Execute one engine output. Audio variants above only; anything else is ignored. Never
    /// blocks the caller (the engine loop); commands are never dropped, even under load.
    pub fn submit(&self, output: Output) {
        let command = match output {
            Output::AddAudioPeer { peer, name } => Command::AddPeer { peer, name },
            Output::RemoveAudioPeer { peer } => Command::RemovePeer { peer },
            Output::OpenAudioCapture { key } => Command::OpenCapture { key },
            Output::OpenAudioPlayback { key } => Command::OpenPlayback { key },
            Output::CloseAudioPlayback { key } => Command::ClosePlayback { key },
            Output::StartAudioStream {
                key,
                kind,
                endpoint,
            } => Command::Start {
                key,
                kind,
                endpoint,
            },
            Output::StopAudioStream { key } => Command::Stop { key },
            // No capture device is ever open (microphones are refused), so there is nothing to
            // close; every other output is not ours.
            _ => return,
        };
        self.shared.admit(command);
    }

    /// An incoming audio datagram from `peer`. Dropped unless `(peer, packet.stream)` is a started
    /// `LocalPlayback` stream; never creates a codec, buffer or session. Never blocks.
    pub fn packet(&self, peer: NodeId, packet: AudioPacket) {
        if self.shared.is_shutdown() {
            return;
        }
        let arrival = (self.clock)();
        let mut rx = lock(&self.shared.rx);
        let counters = &self.shared.counters;
        match rx
            .slots
            .iter_mut()
            .find(|slot| slot.key.peer == peer && slot.key.stream == packet.stream)
        {
            // The queue was allocated when the stream started: this push never allocates.
            Some(slot) if slot.queue.len() < MAX_QUEUED_PACKETS => {
                slot.queue.push_back((arrival, packet));
            }
            Some(_) => bump(&counters.rx_overflow),
            None => bump(&counters.rx_unknown),
        }
    }

    /// Stop every stream with `peer` at once (link closed or connection replaced); late results
    /// for its keys are dropped and nothing further is reported for them.
    pub fn cancel_peer(&self, peer: NodeId) {
        self.shared.admit(Command::CancelPeer { peer });
    }

    /// Stop all streams and join all threads, within 2.5 s.
    pub fn shutdown(self) {
        self.stop();
    }

    /// Idempotent teardown shared by `shutdown` and `Drop`.
    fn stop(&self) {
        // The budget runs from here, not from after the queues are emptied.
        let deadline = Instant::now() + SHUTDOWN_BUDGET;
        let threads = lock(&self.threads).take();
        // From here on nothing is reported, and nothing more is delivered.
        self.shared.closing.store(true, Ordering::SeqCst);
        self.shared.begin_shutdown();
        let Some(threads) = threads else {
            return;
        };
        wait_finished(threads.data, deadline);
        wait_finished(threads.host, deadline);
    }

    /// Counters, for tests and diagnostics.
    pub(crate) fn stats(&self) -> WorkerStats {
        self.shared.counters.snapshot()
    }
}

/// Data-only catalogue of genuine source-local projection/window bindings. PIDs are refreshed
/// from current WindowInfo; this mitigates but never authenticates process reuse \[U\].
#[derive(Debug, Default)]
pub(crate) struct SourceCatalogue {
    projections:
        BTreeMap<crosspane_engine::io::ProjectionKey, (NodeId, crosspane_types::id::WindowId)>,
    granted: BTreeSet<NodeId>,
    sent: BTreeMap<NodeId, Vec<u32>>,
}
impl SourceCatalogue {
    /// The SAME validated Input::Link path as the outgoing engine: wire bounded decode, Hello
    /// first, then transport hub current-connection filtering before on_link/feed. Not local grants.
    pub(crate) fn observe(&mut self, input: &crosspane_engine::Input) {
        use crosspane_engine::Input;
        use crosspane_protocol::{
            link::LinkEvent,
            msg::{Capability, ControlMessage},
        };
        match input {
            Input::Link(LinkEvent::Control {
                peer,
                msg: ControlMessage::Grants(grants),
            }) => {
                if grants.contains(&Capability::AudioSpeaker) {
                    self.granted.insert(*peer);
                } else {
                    self.granted.remove(peer);
                }
            }
            Input::Link(LinkEvent::Closed { peer, .. })
            | Input::AudioConnectionReplaced { peer } => {
                self.granted.remove(peer);
            }
            _ => {}
        }
    }
    /// Initial Parked input and ProjectionStarted share one handle batch, even for Twin capture.
    pub(crate) fn outputs(
        &mut self,
        node: NodeId,
        window: Option<crosspane_types::id::WindowId>,
        outputs: &[Output],
    ) {
        for output in outputs {
            match output {
                Output::Notice(crosspane_engine::Notice::ProjectionStarted {
                    key, peer, ..
                }) if key.source == node => {
                    if let Some(window) = window {
                        self.projections.insert(*key, (*peer, window));
                    }
                }
                Output::Notice(crosspane_engine::Notice::ProjectionEnded { key, .. }) => {
                    self.projections.remove(key);
                }
                _ => {}
            }
        }
    }
    pub(crate) fn changes(
        &mut self,
        node: NodeId,
        pid: impl Fn(crosspane_types::id::WindowId) -> Option<u32>,
    ) -> Vec<(NodeId, Vec<u32>)> {
        let mut desired: BTreeMap<NodeId, BTreeSet<u32>> = BTreeMap::new();
        for (key, (peer, window)) in &self.projections {
            if key.source == node
                && self.granted.contains(peer)
                && let Some(pid) = pid(*window).filter(|pid| *pid != 0)
            {
                desired.entry(*peer).or_default().insert(pid);
            }
        }
        let peers: BTreeSet<_> = desired.keys().chain(self.sent.keys()).copied().collect();
        let mut changed = Vec::new();
        for peer in peers {
            let pids: Vec<_> = desired
                .remove(&peer)
                .unwrap_or_default()
                .into_iter()
                .collect();
            if self.sent.get(&peer).map_or(&[][..], Vec::as_slice) == pids {
                continue;
            }
            if pids.is_empty() {
                self.sent.remove(&peer);
            } else {
                self.sent.insert(peer, pids.clone());
            }
            changed.push((peer, pids));
        }
        changed
    }
}

impl Drop for AudioWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What a caller asked of the data thread. Applied there in submission order.
#[derive(Debug)]
enum Command {
    AddPeer {
        peer: NodeId,
        name: String,
    },
    RemovePeer {
        peer: NodeId,
    },
    OpenCapture {
        key: AudioKey,
    },
    OpenPlayback {
        key: AudioKey,
    },
    ClosePlayback {
        key: AudioKey,
    },
    Start {
        key: AudioKey,
        kind: AudioKind,
        endpoint: AudioEndpoint,
    },
    Stop {
        key: AudioKey,
    },
    CancelPeer {
        peer: NodeId,
    },
}

/// Where the data thread is, for tests that need to act between two stages of a pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// A frame was encoded and is about to be sent.
    Encoded,
    /// A frame was pulled from a jitter buffer and is about to be written to a playback.
    Pulled,
    /// A frame was written to a playback, and the next pull is next.
    Written,
    /// A request for a key is being admitted, between the check that the worker lives and the
    /// shadow update, with the shadow's lock held (tests try to kill the worker right here).
    #[cfg(test)]
    Admitting,
}

#[cfg(test)]
type Hook = Arc<dyn Fn(Stage, AudioKey, &[f32]) + Send + Sync>;

/// State shared by the caller-facing handle, the data thread and the host thread. Every lock is
/// held only for a few moves: never across a codec call, a host call or a callback.
struct Shared {
    events: Arc<dyn Fn(WorkerEvent) + Send + Sync>,
    /// The worker is over, by a caller's `shutdown` or by a death: threads leave their loops and
    /// nothing is delivered.
    shutdown: AtomicBool,
    /// A caller asked for the shutdown: from then on nothing at all is reported.
    closing: AtomicBool,
    /// Callers to the data thread.
    commands: Mutex<VecDeque<Command>>,
    /// Host thread to the data thread.
    replies: Mutex<ReplyQueue>,
    /// Data thread to the host thread.
    host: host::Inbox,
    /// Started receive streams, so `packet()` can drop what is not ours without allocating.
    rx: Mutex<RxTable>,
    /// What the engine believes: the keys that may deliver and the opens still owed a report.
    shadow: Mutex<Shadow>,
    data_thread: OnceLock<Thread>,
    counters: Counters,
    /// Completed data-thread passes.
    passes: AtomicU64,
    /// Commands queued, and commands the data thread has finished applying (tests wait for the
    /// two to meet).
    submitted: AtomicU64,
    applied: AtomicU64,
    /// Host replies published, and replies the data thread has finished applying.
    replies_published: AtomicU64,
    replies_applied: AtomicU64,
    #[cfg(test)]
    hook: Mutex<Option<Hook>>,
}

impl Shared {
    fn new(events: Arc<dyn Fn(WorkerEvent) + Send + Sync>) -> Self {
        Shared {
            events,
            shutdown: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            commands: Mutex::new(VecDeque::with_capacity(64)),
            replies: Mutex::new(ReplyQueue {
                closed: false,
                items: VecDeque::with_capacity(16),
            }),
            host: host::Inbox::new(),
            rx: Mutex::new(RxTable::new()),
            shadow: Mutex::new(Shadow::default()),
            data_thread: OnceLock::new(),
            counters: Counters::default(),
            passes: AtomicU64::new(0),
            submitted: AtomicU64::new(0),
            applied: AtomicU64::new(0),
            replies_published: AtomicU64::new(0),
            replies_applied: AtomicU64::new(0),
            #[cfg(test)]
            hook: Mutex::new(None),
        }
    }

    fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Leave a mark for tests, with the frame being worked on; free in production.
    fn stage(&self, stage: Stage, key: AudioKey, pcm: &[f32]) {
        #[cfg(test)]
        {
            let hook = lock(&self.hook).clone();
            if let Some(hook) = hook {
                hook(stage, key, pcm);
            }
        }
        #[cfg(not(test))]
        {
            let _ = (stage, key, pcm);
        }
    }

    /// Stop all delivery and wake both threads.
    fn signal_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.wake_data();
        self.host.wake();
    }

    /// Empty every queue and drop what was in them, outside the locks (a completed open's handle
    /// stops here), and close the reply queue so that nothing can be published into it after.
    fn discard_queues(&self) {
        let replies = {
            let mut queue = lock(&self.replies);
            queue.closed = true;
            mem::take(&mut queue.items)
        };
        let commands = mem::take(&mut *lock(&self.commands));
        let slots = mem::take(&mut lock(&self.rx).slots);
        drop((replies, commands, slots));
    }

    /// Stop all delivery at once, wake both threads and discard everything queued.
    fn begin_shutdown(&self) {
        self.signal_shutdown();
        self.discard_queues();
    }

    /// The worker has died: report everything the engine is still waiting on, exactly once each,
    /// from the context that noticed. Does nothing beyond the shutdown if a caller asked for it.
    fn die(&self, why: &str) {
        if self.closing.load(Ordering::SeqCst) {
            self.begin_shutdown();
            return;
        }
        let taken = lock(&self.shadow).mark_dead();
        if let Some(taken) = taken {
            self.finish_death(why, taken);
        }
    }

    /// The part of a death that follows the transition under the shadow's lock. The reports come
    /// first: nothing that can block (a queued handle's stop) is allowed to delay them.
    fn finish_death(&self, why: &str, owed: Owed) {
        let Owed { live, opens } = owed;
        tracing::error!(
            why,
            "the audio worker died: reporting its opens and streams as failed"
        );
        self.signal_shutdown();
        for (key, kind) in opens {
            self.emit_guarded(WorkerEvent::DeviceOpened {
                key,
                kind,
                result: Err(Failure::Other),
            });
        }
        for key in live {
            self.emit_guarded(WorkerEvent::StreamFailed { key });
        }
        self.discard_queues();
    }

    /// A request after a death: it is not run, and what the engine is waiting on is reported as
    /// failed at once.
    fn reject(&self, command: Command) {
        let event = match command {
            Command::OpenPlayback { key } => WorkerEvent::DeviceOpened {
                key,
                kind: AudioKind::Speaker,
                result: Err(Failure::Other),
            },
            Command::OpenCapture { key } => WorkerEvent::DeviceOpened {
                key,
                kind: AudioKind::Microphone,
                result: Err(Failure::Other),
            },
            Command::Start { key, .. } => WorkerEvent::StreamFailed { key },
            // Nothing is waiting on these.
            Command::AddPeer { .. }
            | Command::RemovePeer { .. }
            | Command::ClosePlayback { .. }
            | Command::Stop { .. }
            | Command::CancelPeer { .. } => return,
        };
        self.emit_guarded(event);
    }

    /// Report to the agent unless a caller has shut the worker down.
    fn emit(&self, event: WorkerEvent) {
        if !self.closing.load(Ordering::SeqCst) {
            (self.events)(event);
        }
    }

    /// As [`Shared::emit`], for a context that is already failing: a callback that panics must not
    /// take that context down with it (a panic while unwinding aborts the process).
    fn emit_guarded(&self, event: WorkerEvent) {
        if self.closing.load(Ordering::SeqCst) {
            return;
        }
        if catch_unwind(AssertUnwindSafe(|| (self.events)(event))).is_err() {
            tracing::error!("the audio events callback panicked while reporting a failure");
        }
    }

    /// A platform event from a backend thread. A panic in the callback must not leave the worker
    /// running without anyone knowing: it is a death, reported before the panic goes on.
    fn platform_event(&self, event: AudioEvent) {
        if self.is_shutdown() {
            return;
        }
        let delivered = catch_unwind(AssertUnwindSafe(|| {
            (self.events)(WorkerEvent::Platform(event));
        }));
        if let Err(payload) = delivered {
            self.die("the platform event callback panicked");
            resume_unwind(payload);
        }
    }

    /// Whether `key` may deliver right now: the worker lives and the engine has not stopped it.
    fn deliverable(&self, key: AudioKey) -> bool {
        !self.is_shutdown() && lock(&self.shadow).live.contains(&key)
    }

    /// Take the right to report the open of `key`; `false` if it was cancelled, already reported
    /// or taken by a death. Ordered against the death's transition by the shadow's lock.
    fn claim_open(&self, key: AudioKey) -> bool {
        let mut shadow = lock(&self.shadow);
        if shadow.dead {
            return false;
        }
        match shadow.opens.iter().position(|(k, _)| *k == key) {
            Some(index) => {
                shadow.opens.swap_remove(index);
                true
            }
            None => false,
        }
    }

    /// Whether an open of `key` is still owed a report.
    fn open_pending(&self, key: AudioKey) -> bool {
        lock(&self.shadow).opens.iter().any(|(k, _)| *k == key)
    }

    /// Take the right to report the failure of `key`; `false` if the engine already stopped it, it
    /// was reported, or a death took it.
    fn claim_fail(&self, key: AudioKey) -> bool {
        let mut shadow = lock(&self.shadow);
        if shadow.dead {
            return false;
        }
        match shadow.live.iter().position(|k| *k == key) {
            Some(index) => {
                shadow.live.swap_remove(index);
                true
            }
            None => false,
        }
    }

    fn wake_data(&self) {
        if let Some(thread) = self.data_thread.get() {
            thread.unpark();
        }
    }

    /// Admit one request: check that the worker lives, update the shadow and queue the command, as
    /// one step under the shadow's lock (see the module docs). A request that finds the worker
    /// dead is rejected and reported failed; one that finds it shut down by a caller is ignored.
    fn admit(&self, command: Command) {
        let mut shadow = lock(&self.shadow);
        if self.closing.load(Ordering::SeqCst) {
            return;
        }
        if shadow.dead {
            drop(shadow);
            self.reject(command);
            return;
        }
        #[cfg(test)]
        if let Command::Start { key, .. }
        | Command::OpenPlayback { key }
        | Command::OpenCapture { key } = &command
        {
            self.stage(Stage::Admitting, *key, &[]);
        }
        match &command {
            Command::OpenCapture { key } => shadow.add_open(*key, AudioKind::Microphone),
            Command::OpenPlayback { key } => shadow.add_open(*key, AudioKind::Speaker),
            Command::Start { key, .. } => shadow.add_live(*key),
            Command::Stop { key } => shadow.end_stream(*key),
            Command::ClosePlayback { key } => shadow.cancel_open(*key),
            Command::RemovePeer { peer } | Command::CancelPeer { peer } => shadow.end_peer(*peer),
            Command::AddPeer { .. } => {}
        }
        let queued = {
            let mut queue = lock(&self.commands);
            if queue.len() >= COMMAND_HARD_CAP {
                false
            } else {
                if queue.len() >= COMMAND_SOFT_CAP
                    && self
                        .counters
                        .commands_over_cap
                        .fetch_add(1, Ordering::Relaxed)
                        == 0
                {
                    // Never dropped below the hard cap: a lost stop or close would leave audio
                    // running.
                    tracing::error!(
                        cap = COMMAND_SOFT_CAP,
                        "audio command queue is over its high-water mark"
                    );
                }
                queue.push_back(command);
                self.submitted.fetch_add(1, Ordering::SeqCst);
                true
            }
        };
        if queued {
            drop(shadow);
            self.wake_data();
        } else {
            // The data thread is not draining: the worker dies, here, in the caller's context. This
            // request is already in the shadow, so the death reports it too.
            let taken = shadow.mark_dead();
            drop(shadow);
            if let Some(taken) = taken {
                self.finish_death("the audio data thread is not draining commands", taken);
            }
        }
    }

    /// Hand a host reply to the data thread. It is handed back when the worker is over, so the
    /// caller (the host thread) drops it, which stops a playback that opened late.
    fn publish_reply(&self, reply: host::Reply) -> Result<(), host::Reply> {
        {
            let mut queue = lock(&self.replies);
            if queue.closed {
                return Err(reply);
            }
            queue.items.push_back(reply);
            self.replies_published.fetch_add(1, Ordering::SeqCst);
        }
        self.wake_data();
        Ok(())
    }
}

/// Host replies waiting for the data thread. Closed for good when the worker is over, so the host
/// thread cannot publish into a queue that has already been cleaned up.
struct ReplyQueue {
    closed: bool,
    items: VecDeque<host::Reply>,
}

/// What the engine believes is going on, kept on the caller's side of the worker (see the module
/// docs). Every list is bounded by the command queue, since each entry has a queued command.
#[derive(Default)]
struct Shadow {
    /// The worker died: set, with `take_all`, in one step under the shadow's lock, so every later
    /// request is rejected and every earlier one was taken.
    dead: bool,
    /// Started and not stopped, cancelled, closed or failed: the keys that may deliver.
    live: Vec<AudioKey>,
    /// Opens asked for and not yet reported (or cancelled).
    opens: Vec<(AudioKey, AudioKind)>,
}

/// What the engine was still owed a report for when the worker died.
struct Owed {
    live: Vec<AudioKey>,
    opens: Vec<(AudioKey, AudioKind)>,
}

impl Shadow {
    fn add_open(&mut self, key: AudioKey, kind: AudioKind) {
        if !self.opens.iter().any(|(k, _)| *k == key) {
            self.opens.push((key, kind));
        }
    }

    fn add_live(&mut self, key: AudioKey) {
        if !self.live.contains(&key) {
            self.live.push(key);
        }
    }

    /// `StopAudioStream`: the stream ends; an open still running is not affected.
    fn end_stream(&mut self, key: AudioKey) {
        self.live.retain(|k| *k != key);
    }

    /// `CloseAudioPlayback`: an open still running is cancelled. It says nothing about streams
    /// (the engine stops a stream with `StopAudioStream` before it closes the device, and a close
    /// never ends a sender, which has no device).
    fn cancel_open(&mut self, key: AudioKey) {
        self.opens.retain(|(k, _)| *k != key);
    }

    /// `cancel_peer` and `RemoveAudioPeer`: every key of the peer is over.
    fn end_peer(&mut self, peer: NodeId) {
        self.live.retain(|k| k.peer != peer);
        self.opens.retain(|(k, _)| k.peer != peer);
    }

    /// The death transition: `None` if the worker was already dead, otherwise everything still
    /// owed, taken so that it is reported exactly once.
    fn mark_dead(&mut self) -> Option<Owed> {
        if self.dead {
            return None;
        }
        self.dead = true;
        Some(Owed {
            live: mem::take(&mut self.live),
            opens: mem::take(&mut self.opens),
        })
    }
}

/// Started `LocalPlayback` streams and the packets waiting for the data thread.
struct RxTable {
    slots: Vec<RxSlot>,
}

struct RxSlot {
    key: AudioKey,
    queue: VecDeque<(MonoTime, AudioPacket)>,
}

impl RxTable {
    fn new() -> Self {
        RxTable {
            slots: Vec::with_capacity(MAX_SESSIONS),
        }
    }
}

/// Counters only: nothing here can carry samples or payloads.
#[derive(Default)]
struct Counters {
    sent: AtomicU64,
    congested: AtomicU64,
    rx_unknown: AtomicU64,
    rx_overflow: AtomicU64,
    rx_rejected: AtomicU64,
    played: AtomicU64,
    playback_overflow: AtomicU64,
    discarded_samples: AtomicU64,
    sanitized_frames: AtomicU64,
    stale_replies: AtomicU64,
    commands_over_cap: AtomicU64,
    sessions_created: AtomicU64,
    encoders_created: AtomicU64,
    jitters_created: AtomicU64,
}

/// A snapshot of [`Counters`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WorkerStats {
    /// Packets handed to `send` successfully.
    pub sent: u64,
    /// Packets `send` dropped for congestion.
    pub congested: u64,
    /// Packets for a stream that is not a started `LocalPlayback`.
    pub rx_unknown: u64,
    /// Packets dropped because their stream already had `MAX_QUEUED_PACKETS` waiting.
    pub rx_overflow: u64,
    /// Packets the jitter buffer rejected as inconsistent.
    pub rx_rejected: u64,
    /// Whole frames written to a playback ring.
    pub played: u64,
    /// Whole frames dropped because the playback ring was full.
    pub playback_overflow: u64,
    /// Virtual-speaker samples discarded while no stream was started (or at a start).
    pub discarded_samples: u64,
    /// Sent frames in which an out-of-range or non-finite sample was repaired.
    pub sanitized_frames: u64,
    /// Host replies for a key (or peer) that was cancelled meanwhile.
    pub stale_replies: u64,
    /// Times a command was queued beyond `COMMAND_SOFT_CAP`.
    pub commands_over_cap: u64,
    /// Sessions ever created (an entry in the session table).
    pub sessions_created: u64,
    /// Opus encoders ever created.
    pub encoders_created: u64,
    /// Jitter buffers (and so decoders) ever created.
    pub jitters_created: u64,
}

impl Counters {
    fn snapshot(&self) -> WorkerStats {
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        WorkerStats {
            sent: get(&self.sent),
            congested: get(&self.congested),
            rx_unknown: get(&self.rx_unknown),
            rx_overflow: get(&self.rx_overflow),
            rx_rejected: get(&self.rx_rejected),
            played: get(&self.played),
            playback_overflow: get(&self.playback_overflow),
            discarded_samples: get(&self.discarded_samples),
            sanitized_frames: get(&self.sanitized_frames),
            stale_replies: get(&self.stale_replies),
            commands_over_cap: get(&self.commands_over_cap),
            sessions_created: get(&self.sessions_created),
            encoders_created: get(&self.encoders_created),
            jitters_created: get(&self.jitters_created),
        }
    }
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Lock a mutex, ignoring poisoning: every critical section here is a handful of moves that
/// cannot leave the data half-updated.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs when a worker thread leaves the function that owns its resources, however it leaves. A
/// thread ending for any reason but a caller's `shutdown` is a death.
///
/// It is declared first in that function, so it is dropped before the resources (a local is
/// dropped before the function's parameters, and in reverse order of declaration), and the death is
/// reported before a slow destructor of the host or of a playback handle can delay it.
struct ExitGuard(Arc<Shared>);

impl Drop for ExitGuard {
    fn drop(&mut self) {
        let why = if thread::panicking() {
            "an audio worker thread panicked"
        } else {
            "an audio worker thread ended"
        };
        self.0.die(why);
    }
}

struct Threads {
    data: JoinHandle<()>,
    host: JoinHandle<()>,
}

fn spawn_thread(
    name: &str,
    body: impl FnOnce() + Send + 'static,
) -> Result<JoinHandle<()>, String> {
    thread::Builder::new()
        .name(name.to_string())
        .spawn(body)
        .map_err(|error| format!("cannot start the {name} thread: {error}"))
}

/// Wait for `handle`'s thread to finish its closure, by polling, until `deadline`. The closure
/// owns everything that matters (the host, the sessions, the playback handles), so once it has
/// finished they are all dropped.
///
/// This never calls `join`: a join also waits for the thread's thread-local destructors, which no
/// deadline bounds. A finished thread's handle is just dropped; one still busy at the deadline is
/// detached, with a warning.
fn wait_finished(handle: JoinHandle<()>, deadline: Instant) {
    loop {
        if handle.is_finished() {
            return;
        }
        if Instant::now() >= deadline {
            tracing::warn!(
                thread = handle.thread().name().unwrap_or("audio"),
                "audio thread still busy at shutdown: detaching it"
            );
            return;
        }
        thread::sleep(Duration::from_millis(1));
    }
}
