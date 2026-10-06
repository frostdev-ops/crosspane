//! WP-3.6c tests. Everything here is fake: a fake `AudioHost` with owned `rtrb` rings, a fake send
//! and a fake clock. No OS audio of any kind is opened.
//!
//! How the tests wait. The worker has threads, so a test never assumes how far they have got:
//!
//! - [`Harness::barrier`] waits for an *application barrier*: every command submitted so far has
//!   been applied by the data thread, and every host request has finished and its reply has been
//!   applied (the worker counts both).
//! - [`Harness::settle`] is the barrier plus two further full passes, for the periodic work (send,
//!   playout) that has no acknowledgement of its own.
//! - Tests that hold a thread on purpose use a [`Gate`]: the held code announces that it has
//!   entered, and the test releases it. Wall-clock time is used only where a time bound is itself
//!   what is being tested.

use std::collections::HashMap;
use std::f64::consts::PI;
use std::sync::atomic::AtomicUsize;
use std::sync::{Barrier, Condvar};
use std::thread::ThreadId;

use crosspane_media::audio::Encoder;
use crosspane_platform::{
    AudioCapture, AudioDeviceError, AudioFormat, AudioPlayback, AudioStop, PlatformError,
    VirtualPorts,
};
use crosspane_types::audio::{AUDIO_FRAME_SAMPLES, AudioStreamId};
use rtrb::{Consumer, Producer, RingBuffer};

use super::*;

// ---- Fakes ----

struct ReleaseSourceGate(Arc<Gate>);
impl Drop for ReleaseSourceGate {
    fn drop(&mut self) {
        self.0.release();
    }
}

fn receive_speaker_grant(sources: &mut SourceCatalogue, peer: NodeId, enabled: bool) {
    sources.observe(&crosspane_engine::Input::Link(
        crosspane_protocol::link::LinkEvent::Control {
            peer,
            msg: crosspane_protocol::msg::ControlMessage::Grants(if enabled {
                vec![crosspane_protocol::msg::Capability::AudioSpeaker]
            } else {
                vec![]
            }),
        },
    ));
}

fn source_notice(node: NodeId, projection: u64, peer: NodeId) -> Output {
    Output::Notice(crosspane_engine::Notice::ProjectionStarted {
        key: crosspane_engine::io::ProjectionKey {
            source: node,
            projection: crosspane_types::id::ProjectionId(projection),
        },
        peer,
        parking: crosspane_protocol::projection::ParkingKind::Twin,
    })
}

#[test]
fn source_projection_start_sends_current_pids_and_second_window_same_pid_is_unchanged() {
    let peer = node(46);
    let node = node(45);
    let mut sources = SourceCatalogue::default();
    receive_speaker_grant(&mut sources, peer, true);
    let first = crosspane_types::id::WindowId(1);
    sources.outputs(node, Some(first), &[source_notice(node, 1, peer)]);
    assert_eq!(sources.changes(node, |_| Some(10)), vec![(peer, vec![10])]);
    sources.outputs(
        node,
        Some(crosspane_types::id::WindowId(2)),
        &[source_notice(node, 2, peer)],
    );
    assert!(sources.changes(node, |_| Some(10)).is_empty());
    assert_eq!(
        sources.changes(node, |window| Some(if window == first { 20 } else { 10 })),
        vec![(peer, vec![10, 20])]
    );
}

#[test]
fn source_projection_end_sends_empty_and_destination_or_pidless_window_never_adds_a_source() {
    let peer = node(48);
    let node = node(47);
    let mut sources = SourceCatalogue::default();
    receive_speaker_grant(&mut sources, peer, true);
    sources.outputs(
        node,
        Some(crosspane_types::id::WindowId(1)),
        &[source_notice(peer, 1, node)],
    );
    assert!(sources.changes(node, |_| Some(99)).is_empty());
    sources.outputs(
        node,
        Some(crosspane_types::id::WindowId(2)),
        &[source_notice(node, 1, peer)],
    );
    assert!(sources.changes(node, |_| None).is_empty());
    assert_eq!(sources.changes(node, |_| Some(10)), vec![(peer, vec![10])]);
    sources.outputs(
        node,
        None,
        &[Output::Notice(crosspane_engine::Notice::ProjectionEnded {
            key: crosspane_engine::io::ProjectionKey {
                source: node,
                projection: crosspane_types::id::ProjectionId(1),
            },
            reason: crosspane_protocol::projection::ProjectionEndReason::Returned,
        })],
    );
    assert_eq!(sources.changes(node, |_| Some(10)), vec![(peer, vec![])]);
    assert!(sources.changes(node, |_| Some(10)).is_empty());
}

#[test]
fn source_projection_uses_received_grant_only_and_revoke_sends_empty() {
    let peer = node(50);
    let node = node(49);
    let mut sources = SourceCatalogue::default();
    sources.outputs(
        node,
        Some(crosspane_types::id::WindowId(1)),
        &[source_notice(node, 1, peer)],
    );
    sources.observe(&crosspane_engine::Input::Grants(
        [(
            peer,
            [crosspane_protocol::msg::Capability::AudioSpeaker].into(),
        )]
        .into(),
    ));
    assert!(sources.changes(node, |_| Some(10)).is_empty());
    receive_speaker_grant(&mut sources, peer, true);
    assert_eq!(sources.changes(node, |_| Some(10)), vec![(peer, vec![10])]);
    sources.observe(&crosspane_engine::Input::Grants(Default::default()));
    assert!(sources.changes(node, |_| Some(10)).is_empty());
    receive_speaker_grant(&mut sources, peer, false);
    assert_eq!(sources.changes(node, |_| Some(10)), vec![(peer, vec![])]);
}

#[test]
fn source_projection_connection_replacement_and_closed_window_clear_without_cached_pid_rebind() {
    let peer = node(52);
    let node = node(51);
    let mut sources = SourceCatalogue::default();
    receive_speaker_grant(&mut sources, peer, true);
    sources.outputs(
        node,
        Some(crosspane_types::id::WindowId(1)),
        &[source_notice(node, 1, peer)],
    );
    assert_eq!(sources.changes(node, |_| Some(10)), vec![(peer, vec![10])]);
    sources.observe(&crosspane_engine::Input::AudioConnectionReplaced { peer });
    assert_eq!(sources.changes(node, |_| Some(10)), vec![(peer, vec![])]);
    assert!(sources.changes(node, |_| Some(10)).is_empty());
    receive_speaker_grant(&mut sources, peer, true);
    assert_eq!(sources.changes(node, |_| Some(20)), vec![(peer, vec![20])]);
    sources.observe(&crosspane_engine::Input::Link(
        crosspane_protocol::link::LinkEvent::Closed {
            peer,
            error: LinkError::Closed,
        },
    ));
    assert_eq!(sources.changes(node, |_| Some(20)), vec![(peer, vec![])]);
    receive_speaker_grant(&mut sources, peer, true);
    assert_eq!(sources.changes(node, |_| Some(20)), vec![(peer, vec![20])]);
    assert_eq!(sources.changes(node, |_| None), vec![(peer, vec![])]);
}

#[test]
fn source_sets_wait_for_successful_add_and_coalesce_to_latest_before_dispatch() {
    let h = Harness::new();
    let peer = NodeId([41; 32]);
    let gate = h.probe.hold_adds();
    let _release = ReleaseSourceGate(gate.clone());
    h.submit(Output::AddAudioPeer {
        peer,
        name: "owned fake".into(),
    });
    gate.wait_entered(1);
    h.w().set_peer_sources(peer, &[10]);
    h.w().set_peer_sources(peer, &[20]);
    h.w().set_peer_sources(peer, &[]);
    assert!(lock(&h.probe.source_sets).is_empty());
    gate.release();
    h.barrier();
    assert_eq!(*lock(&h.probe.source_sets), vec![(peer, vec![])]);
}

#[test]
fn source_sets_pending_behind_a_call_keep_only_latest_empty_and_run_off_caller() {
    let h = Harness::new();
    let peer = NodeId([42; 32]);
    h.add_peer(peer);
    let gate = Gate::closed();
    let _release = ReleaseSourceGate(gate.clone());
    *lock(&h.probe.sources_gate) = Some(gate.clone());
    h.w().set_peer_sources(peer, &[10]);
    gate.wait_entered(1);
    for pid in 20..2000 {
        h.w().set_peer_sources(peer, &[pid]);
    }
    h.w().set_peer_sources(peer, &[]);
    assert_eq!(h.shared().host.queued_sources(), 1);
    gate.release();
    h.barrier();
    assert_eq!(
        *lock(&h.probe.source_sets),
        vec![(peer, vec![10]), (peer, vec![])]
    );
}

#[test]
fn source_sets_failed_add_or_pending_remove_never_dispatch_stale_capture() {
    let h = Harness::new();
    let peer = NodeId([43; 32]);
    let gate = h.probe.hold_adds();
    let _release = ReleaseSourceGate(gate.clone());
    h.probe.fail_add.store(true, Ordering::SeqCst);
    h.submit(Output::AddAudioPeer {
        peer,
        name: "owned fake".into(),
    });
    gate.wait_entered(1);
    h.w().set_peer_sources(peer, &[10]);
    gate.release();
    h.barrier();
    assert!(lock(&h.probe.source_sets).is_empty());
    assert_eq!(h.shared().host.queued_sources(), 0);
    h.probe.fail_add.store(false, Ordering::SeqCst);
    h.add_peer(peer);
    h.w().set_peer_sources(peer, &[1]);
    h.barrier();
    assert_eq!(*lock(&h.probe.source_sets), vec![(peer, vec![1])]);
    lock(&h.probe.source_sets).clear();
    let held = h.probe.hold_opens();
    let _release_open = ReleaseSourceGate(held.clone());
    h.submit(Output::OpenAudioPlayback {
        key: key(peer, 2, 1),
    });
    held.wait_entered(1);
    h.w().set_peer_sources(peer, &[20]);
    h.submit(Output::RemoveAudioPeer { peer });
    h.wait_applied();
    held.release();
    h.barrier();
    assert!(lock(&h.probe.source_sets).is_empty());
}

#[test]
fn source_sets_error_is_dropped_without_killing_worker_or_changing_data_replies() {
    let h = Harness::new();
    let peer = NodeId([44; 32]);
    h.add_peer(peer);
    h.probe.fail_sources.store(true, Ordering::SeqCst);
    h.w().set_peer_sources(peer, &[10]);
    h.barrier();
    assert!(!h.shared().is_shutdown());
    assert_eq!(*lock(&h.probe.source_sets), vec![(peer, vec![10])]);
    h.probe.fail_sources.store(false, Ordering::SeqCst);
    h.w().set_peer_sources(peer, &[]);
    h.barrier();
    assert_eq!(lock(&h.probe.source_sets).last(), Some(&(peer, vec![])));
}

const STEREO: AudioFormat = AudioFormat {
    rate: 48_000,
    channels: 2,
};

#[derive(Clone)]
struct FakeClock(Arc<AtomicU64>);

impl FakeClock {
    fn new() -> Self {
        FakeClock(Arc::new(AtomicU64::new(1_000_000_000)))
    }
    fn advance(&self, by: Duration) {
        self.0.fetch_add(by.as_nanos() as u64, Ordering::SeqCst);
    }
    fn as_clock(&self) -> AudioClock {
        let nanos = Arc::clone(&self.0);
        Arc::new(move || MonoTime::from_nanos(nanos.load(Ordering::SeqCst)))
    }
}

/// Holds whatever calls [`Gate::pass`] until the test releases it, and counts arrivals so the
/// test can wait for the held code to be there before it acts.
struct Gate {
    entered: AtomicUsize,
    state: Mutex<GateState>,
    changed: Condvar,
}

struct GateState {
    open: bool,
    /// Passers that may go through while the gate is closed (see [`Gate::allow`]).
    tokens: usize,
}

impl Gate {
    fn new(open: bool) -> Arc<Self> {
        Arc::new(Gate {
            entered: AtomicUsize::new(0),
            state: Mutex::new(GateState { open, tokens: 0 }),
            changed: Condvar::new(),
        })
    }

    fn closed() -> Arc<Self> {
        Self::new(false)
    }

    fn close(&self) {
        lock(&self.state).open = false;
    }

    fn release(&self) {
        lock(&self.state).open = true;
        self.changed.notify_all();
    }

    /// Let exactly the next `n` passers through; the gate stays closed to the ones after them.
    fn allow(&self, n: usize) {
        lock(&self.state).tokens += n;
        self.changed.notify_all();
    }

    /// Called by the held code: announce the arrival, then wait (boundedly) to be released.
    fn pass(&self) {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let give_up = Instant::now() + Duration::from_secs(20);
        let mut state = lock(&self.state);
        loop {
            if state.open {
                return;
            }
            if state.tokens > 0 {
                state.tokens -= 1;
                return;
            }
            if Instant::now() >= give_up {
                return;
            }
            state = self
                .changed
                .wait_timeout(state, Duration::from_millis(20))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn entered(&self) -> usize {
        self.entered.load(Ordering::SeqCst)
    }

    fn wait_entered(&self, n: usize) {
        wait_until("code to reach a gate", Duration::from_secs(10), || {
            self.entered() >= n
        });
    }
}

/// The app side of a peer's virtual devices.
struct PeerEnds {
    /// What apps play into "<peer> speakers".
    speaker_in: Producer<f32>,
    /// What the worker wrote to "<peer> microphone": it must stay empty.
    mic_out: Consumer<f32>,
}

/// The device side of an opened playback.
struct PlaybackEnds {
    /// What the worker wrote for the speakers to play.
    pcm: Consumer<f32>,
    /// Times the backend was told to stop.
    stopped: Arc<AtomicUsize>,
}

/// A backend's stop: it can be slow (a delay) or held (a gate) before it counts as done.
#[derive(Debug)]
struct CountStop {
    stopped: Arc<AtomicUsize>,
    gate: Option<Arc<Gate>>,
    delay_ms: u64,
}

impl fmt::Debug for Gate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Gate")
    }
}

impl AudioStop for CountStop {
    fn stop(&mut self) {
        if let Some(gate) = &self.gate {
            gate.pass();
        }
        sleep_ms(self.delay_ms);
        self.stopped.fetch_add(1, Ordering::SeqCst);
    }
}

/// A thread-local whose destructor is slow, to prove shutdown does not wait for destructors.
struct SlowThreadExit;

impl Drop for SlowThreadExit {
    fn drop(&mut self) {
        thread::sleep(Duration::from_secs(3));
    }
}

thread_local! {
    static SLOW_EXIT: SlowThreadExit = const { SlowThreadExit };
}

type RemoveHook = Arc<dyn Fn(&HostProbe) + Send + Sync>;

/// What a test sees of, and tells, the fake host.
struct HostProbe {
    add_calls: AtomicUsize,
    remove_calls: AtomicUsize,
    open_playback_calls: AtomicUsize,
    open_capture_calls: AtomicUsize,
    subscribe_calls: AtomicUsize,
    host_dropped: AtomicBool,
    /// How long `open_playback` blocks before answering. Read before the call is announced.
    open_delay: AtomicU64,
    /// How long `add_peer` blocks before answering. Read before the call is announced.
    add_delay: AtomicU64,
    /// Held by `open_playback` / `add_peer` while closed. Read before the call is announced.
    open_gate: Mutex<Option<Arc<Gate>>>,
    add_gate: Mutex<Option<Arc<Gate>>>,
    /// Called inside `remove_peer`, before the devices go.
    remove_hook: Mutex<Option<RemoveHook>>,
    /// Given to each playback opened from now on: its stop waits at the gate / takes this long.
    stop_gate: Mutex<Option<Arc<Gate>>>,
    stop_delay: AtomicU64,
    /// The host's destructor waits here.
    drop_gate: Mutex<Option<Arc<Gate>>>,
    fail_open: AtomicBool,
    fail_add: AtomicBool,
    fail_subscribe: AtomicBool,
    panic_on_add: AtomicBool,
    /// `open_playback` touches a thread-local with a slow destructor.
    slow_thread_exit: AtomicBool,
    /// Capacity of each playback ring, in samples.
    playback_samples: AtomicUsize,
    peers: Mutex<HashMap<NodeId, PeerEnds>>,
    playbacks: Mutex<Vec<PlaybackEnds>>,
    names: Mutex<Vec<String>>,
    formats: Mutex<Vec<AudioFormat>>,
    source_sets: Mutex<Vec<(NodeId, Vec<u32>)>>,
    sources_gate: Mutex<Option<Arc<Gate>>>,
    fail_sources: AtomicBool,
    sink: Mutex<Option<Arc<dyn EventSink<AudioEvent>>>>,
}

impl HostProbe {
    fn new() -> Arc<Self> {
        Arc::new(HostProbe {
            add_calls: AtomicUsize::new(0),
            remove_calls: AtomicUsize::new(0),
            open_playback_calls: AtomicUsize::new(0),
            open_capture_calls: AtomicUsize::new(0),
            subscribe_calls: AtomicUsize::new(0),
            host_dropped: AtomicBool::new(false),
            open_delay: AtomicU64::new(0),
            add_delay: AtomicU64::new(0),
            open_gate: Mutex::new(None),
            add_gate: Mutex::new(None),
            remove_hook: Mutex::new(None),
            stop_gate: Mutex::new(None),
            stop_delay: AtomicU64::new(0),
            drop_gate: Mutex::new(None),
            fail_open: AtomicBool::new(false),
            fail_add: AtomicBool::new(false),
            fail_subscribe: AtomicBool::new(false),
            panic_on_add: AtomicBool::new(false),
            slow_thread_exit: AtomicBool::new(false),
            playback_samples: AtomicUsize::new(96_000),
            peers: Mutex::new(HashMap::new()),
            playbacks: Mutex::new(Vec::new()),
            names: Mutex::new(Vec::new()),
            formats: Mutex::new(Vec::new()),
            source_sets: Mutex::new(Vec::new()),
            sources_gate: Mutex::new(None),
            fail_sources: AtomicBool::new(false),
            sink: Mutex::new(None),
        })
    }

    fn set_open_delay(&self, delay: Duration) {
        self.open_delay
            .store(delay.as_millis() as u64, Ordering::SeqCst);
    }

    /// From now on `open_playback` waits at a closed gate (which the test releases) after
    /// announcing that it was entered.
    fn hold_opens(&self) -> Arc<Gate> {
        let gate = Gate::closed();
        *lock(&self.open_gate) = Some(Arc::clone(&gate));
        gate
    }

    fn hold_adds(&self) -> Arc<Gate> {
        let gate = Gate::closed();
        *lock(&self.add_gate) = Some(Arc::clone(&gate));
        gate
    }

    /// Write interleaved stereo PCM as an app playing into the peer's virtual speaker would.
    fn speak(&self, peer: NodeId, samples: &[f32]) {
        let mut peers = lock(&self.peers);
        let ends = peers.get_mut(&peer).expect("peer has virtual devices");
        ends.speaker_in
            .push_entire_slice(samples)
            .expect("the virtual speaker ring has room");
    }

    /// Free room in the peer's virtual speaker ring, in samples.
    fn speaker_room(&self, peer: NodeId) -> usize {
        lock(&self.peers)
            .get(&peer)
            .map(|ends| ends.speaker_in.slots())
            .expect("peer has virtual devices")
    }

    /// Everything the worker has written to playback number `index` so far.
    fn played(&self, index: usize) -> Vec<f32> {
        let mut playbacks = lock(&self.playbacks);
        let mut out = Vec::new();
        while let Ok(sample) = playbacks[index].pcm.pop() {
            out.push(sample);
        }
        out
    }

    /// Samples waiting in playback `index`'s ring, and the ring's capacity.
    fn ring_fill(&self, index: usize) -> (usize, usize) {
        let playbacks = lock(&self.playbacks);
        let pcm = &playbacks[index].pcm;
        (pcm.slots(), pcm.buffer().capacity())
    }

    fn stopped(&self, index: usize) -> usize {
        lock(&self.playbacks)[index].stopped.load(Ordering::SeqCst)
    }

    /// Total stops over every playback opened so far.
    fn stops(&self) -> usize {
        lock(&self.playbacks)
            .iter()
            .map(|p| p.stopped.load(Ordering::SeqCst))
            .sum()
    }

    fn playbacks_opened(&self) -> usize {
        lock(&self.playbacks).len()
    }

    fn mic_samples_written(&self) -> usize {
        lock(&self.peers)
            .values()
            .map(|ends| ends.mic_out.slots())
            .sum()
    }
}

struct FakeHost(Arc<HostProbe>);

impl Drop for FakeHost {
    fn drop(&mut self) {
        let gate = lock(&self.0.drop_gate).clone();
        if let Some(gate) = gate {
            gate.pass();
        }
        self.0.host_dropped.store(true, Ordering::SeqCst);
    }
}

fn sleep_ms(ms: u64) {
    if ms > 0 {
        thread::sleep(Duration::from_millis(ms));
    }
}

impl AudioHost for FakeHost {
    fn set_peer_sources(&mut self, peer: NodeId, pids: &[u32]) -> Result<(), PlatformError> {
        assert!(
            lock(&self.0.peers).contains_key(&peer),
            "sources preceded real add_peer"
        );
        lock(&self.0.source_sets).push((peer, pids.to_vec()));
        if let Some(gate) = lock(&self.0.sources_gate).clone() {
            gate.pass();
        }
        if self.0.fail_sources.load(Ordering::SeqCst) {
            return Err(PlatformError::Backend("fake source failure".into()));
        }
        Ok(())
    }
    fn add_peer(&mut self, peer: NodeId, name: &str) -> Result<VirtualPorts, PlatformError> {
        let probe = &self.0;
        // Everything the test configures is read before the call is announced, so a test that
        // reacts to the announcement cannot race a later change.
        let delay = probe.add_delay.load(Ordering::SeqCst);
        let gate = lock(&probe.add_gate).clone();
        probe.add_calls.fetch_add(1, Ordering::SeqCst);
        assert!(
            !probe.panic_on_add.load(Ordering::SeqCst),
            "the fake host's thread dies (expected by its test)"
        );
        lock(&probe.names).push(name.to_string());
        if let Some(gate) = gate {
            gate.pass();
        }
        sleep_ms(delay);
        if probe.fail_add.load(Ordering::SeqCst) {
            return Err(PlatformError::Backend("fake add_peer failure".into()));
        }
        let (speaker_in, speaker_out) = RingBuffer::<f32>::new(96_000);
        let (mic_in, mic_out) = RingBuffer::<f32>::new(48_000);
        lock(&probe.peers).insert(
            peer,
            PeerEnds {
                speaker_in,
                mic_out,
            },
        );
        Ok(VirtualPorts {
            speaker_out,
            mic_in,
        })
    }

    fn remove_peer(&mut self, peer: NodeId) -> Result<(), PlatformError> {
        self.0.remove_calls.fetch_add(1, Ordering::SeqCst);
        let hook = lock(&self.0.remove_hook).clone();
        if let Some(hook) = hook {
            hook(&self.0);
        }
        lock(&self.0.peers).remove(&peer);
        Ok(())
    }

    fn subscribe(&mut self, sink: Arc<dyn EventSink<AudioEvent>>) -> Result<(), PlatformError> {
        self.0.subscribe_calls.fetch_add(1, Ordering::SeqCst);
        if self.0.fail_subscribe.load(Ordering::SeqCst) {
            return Err(PlatformError::Unsupported("fake: no events"));
        }
        *lock(&self.0.sink) = Some(sink);
        Ok(())
    }

    fn open_capture(&mut self, _format: AudioFormat) -> Result<AudioCapture, PlatformError> {
        self.0.open_capture_calls.fetch_add(1, Ordering::SeqCst);
        Err(PlatformError::Unsupported("fake: no capture"))
    }

    fn open_playback(&mut self, format: AudioFormat) -> Result<AudioPlayback, PlatformError> {
        let probe = &self.0;
        let delay = probe.open_delay.load(Ordering::SeqCst);
        let gate = lock(&probe.open_gate).clone();
        let fail = probe.fail_open.load(Ordering::SeqCst);
        let ring = probe.playback_samples.load(Ordering::SeqCst);
        let stop_gate = lock(&probe.stop_gate).clone();
        let stop_delay = probe.stop_delay.load(Ordering::SeqCst);
        probe.open_playback_calls.fetch_add(1, Ordering::SeqCst);
        lock(&probe.formats).push(format);
        if probe.slow_thread_exit.load(Ordering::SeqCst) {
            SLOW_EXIT.with(|_| {});
        }
        if let Some(gate) = gate {
            gate.pass();
        }
        sleep_ms(delay);
        if fail {
            return Err(PlatformError::PermissionDenied(
                crosspane_platform::Permission::Microphone,
            ));
        }
        let (pcm_in, pcm) = RingBuffer::<f32>::new(ring);
        let stopped = Arc::new(AtomicUsize::new(0));
        lock(&probe.playbacks).push(PlaybackEnds {
            pcm,
            stopped: Arc::clone(&stopped),
        });
        Ok(AudioPlayback::new(
            pcm_in,
            Box::new(CountStop {
                stopped,
                gate: stop_gate,
                delay_ms: stop_delay,
            }),
        ))
    }
}

#[derive(Clone, Copy, Debug)]
enum SendMode {
    Ok,
    /// Every `n`th send is dropped for congestion.
    CongestEvery(usize),
    Closed,
    /// Every send waits at the send gate (and is counted when it gets there).
    Gated,
}

struct Harness {
    worker: Option<AudioWorker>,
    clock: FakeClock,
    probe: Arc<HostProbe>,
    sent: Arc<Mutex<Vec<(NodeId, AudioPacket)>>>,
    send_mode: Arc<Mutex<SendMode>>,
    send_gate: Arc<Gate>,
    events: Arc<Mutex<Vec<WorkerEvent>>>,
    /// The thread each event was reported from, in step with `events`.
    event_threads: Arc<Mutex<Vec<ThreadId>>>,
    /// Every event passes this gate on its way to `events`; close it to park the reporting thread.
    event_gate: Arc<Gate>,
    /// The next non-platform event makes the events callback panic (once).
    panic_on_event: Arc<AtomicBool>,
    /// Every platform event makes the events callback panic.
    panic_on_platform: Arc<AtomicBool>,
    /// The report that an open of this key succeeded makes the events callback panic.
    panic_on_open_of: Arc<Mutex<Option<AudioKey>>>,
}

impl Harness {
    fn new() -> Self {
        Self::with_probe(HostProbe::new())
    }

    fn with_probe(probe: Arc<HostProbe>) -> Self {
        let clock = FakeClock::new();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let send_mode = Arc::new(Mutex::new(SendMode::Ok));
        let send_gate = Gate::new(true);
        let events = Arc::new(Mutex::new(Vec::new()));
        let event_threads = Arc::new(Mutex::new(Vec::new()));
        let event_gate = Gate::new(true);
        let panic_on_event = Arc::new(AtomicBool::new(false));
        let panic_on_platform = Arc::new(AtomicBool::new(false));
        let panic_on_open_of = Arc::new(Mutex::new(None));
        let calls = Arc::new(AtomicUsize::new(0));
        let send: AudioSend = {
            let (sent, mode, calls, gate) = (
                sent.clone(),
                send_mode.clone(),
                calls.clone(),
                send_gate.clone(),
            );
            Arc::new(move |peer, packet| {
                let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                let current = *lock(&mode);
                if matches!(current, SendMode::Gated) {
                    gate.pass();
                }
                match current {
                    SendMode::Ok | SendMode::Gated => {}
                    SendMode::CongestEvery(every) if n % every == 0 => {
                        return Err(LinkError::Congested);
                    }
                    SendMode::CongestEvery(_) => {}
                    SendMode::Closed => return Err(LinkError::Closed),
                }
                lock(&sent).push((peer, packet.clone()));
                Ok(())
            })
        };
        let worker = AudioWorker::start(
            Box::new(FakeHost(probe.clone())),
            send,
            clock.as_clock(),
            Box::new({
                let (events, threads, gate) =
                    (events.clone(), event_threads.clone(), event_gate.clone());
                let (panic_event, panic_platform, panic_open) = (
                    panic_on_event.clone(),
                    panic_on_platform.clone(),
                    panic_on_open_of.clone(),
                );
                move |event| {
                    // Only the data thread is ever parked here, so a test can report from its own
                    // thread (a death, say) while the data thread is held.
                    if thread::current().name() == Some("cp-audio-data") {
                        gate.pass();
                    }
                    let platform = matches!(event, WorkerEvent::Platform(_));
                    if platform && panic_platform.load(Ordering::SeqCst) {
                        panic!("the platform callback panics (expected by its test)");
                    }
                    if let WorkerEvent::DeviceOpened { key, .. } = &event
                        && *lock(&panic_open) == Some(*key)
                    {
                        *lock(&panic_open) = None;
                        panic!("the events callback panics on one open (expected by its test)");
                    }
                    if !platform && panic_event.swap(false, Ordering::SeqCst) {
                        panic!("the events callback panics (expected by its test)");
                    }
                    lock(&threads).push(thread::current().id());
                    lock(&events).push(event);
                }
            }),
        )
        .expect("the worker starts");
        Harness {
            worker: Some(worker),
            clock,
            probe,
            sent,
            send_mode,
            send_gate,
            events,
            event_threads,
            event_gate,
            panic_on_event,
            panic_on_platform,
            panic_on_open_of,
        }
    }

    fn w(&self) -> &AudioWorker {
        self.worker.as_ref().expect("worker is running")
    }

    fn shared(&self) -> &Arc<Shared> {
        &self.w().shared
    }

    fn submit(&self, output: Output) {
        self.w().submit(output);
    }

    /// Every command submitted so far has been applied (the host may still be busy).
    fn wait_applied(&self) {
        let shared = self.shared();
        wait_until("commands to be applied", Duration::from_secs(10), || {
            shared.applied.load(Ordering::SeqCst) == shared.submitted.load(Ordering::SeqCst)
        });
    }

    /// The application barrier: every command applied, every host request finished, every reply
    /// applied. A reply can start new host work, so the checks bracket the host's idleness.
    fn barrier(&self) {
        let shared = self.shared();
        let all_applied = || {
            shared.applied.load(Ordering::SeqCst) == shared.submitted.load(Ordering::SeqCst)
                && shared.replies_published.load(Ordering::SeqCst)
                    == shared.replies_applied.load(Ordering::SeqCst)
        };
        wait_until(
            "the worker to apply everything",
            Duration::from_secs(10),
            || all_applied() && shared.host.is_idle() && all_applied(),
        );
    }

    /// Wait for the data thread to complete `n` more full passes (does not wait for the host).
    fn run_passes(&self, n: u64) {
        let shared = self.shared();
        let before = shared.passes.load(Ordering::SeqCst);
        wait_until("data thread passes", Duration::from_secs(5), || {
            shared.passes.load(Ordering::SeqCst) > before + n
        });
    }

    /// The barrier, then two full passes over the state it established, for the periodic work.
    fn settle(&self) {
        self.barrier();
        self.run_passes(2);
    }

    /// Advance the clock one 10 ms step and let the data thread see it.
    fn step(&self) {
        self.clock.advance(Duration::from_millis(10));
        self.settle();
    }

    fn add_peer(&self, peer: NodeId) {
        self.submit(Output::AddAudioPeer {
            peer,
            name: format!("peer-{}", peer.short()),
        });
        self.settle();
    }

    /// Open playback for `key` and wait for the open to be reported.
    fn open_playback(&self, key: AudioKey) {
        self.submit(Output::OpenAudioPlayback { key });
        self.settle();
        assert_eq!(self.opened(key), vec![(AudioKind::Speaker, Ok(()))]);
    }

    fn start(&self, key: AudioKey, endpoint: AudioEndpoint) {
        self.submit(Output::StartAudioStream {
            key,
            kind: AudioKind::Speaker,
            endpoint,
        });
        self.settle();
    }

    fn stop(&self, key: AudioKey) {
        self.submit(Output::StopAudioStream { key });
        self.settle();
    }

    fn close(&self, key: AudioKey) {
        self.submit(Output::CloseAudioPlayback { key });
        self.settle();
    }

    /// Every `DeviceOpened` reported for `key`.
    fn opened(&self, key: AudioKey) -> Vec<(AudioKind, Result<(), Failure>)> {
        lock(&self.events)
            .iter()
            .filter_map(|event| match event {
                WorkerEvent::DeviceOpened {
                    key: k,
                    kind,
                    result,
                } if *k == key => Some((*kind, *result)),
                _ => None,
            })
            .collect()
    }

    fn failed(&self, key: AudioKey) -> usize {
        lock(&self.events)
            .iter()
            .filter(|event| matches!(event, WorkerEvent::StreamFailed { key: k } if *k == key))
            .count()
    }

    fn event_count(&self) -> usize {
        lock(&self.events).len()
    }

    fn platform_events(&self) -> Vec<AudioEvent> {
        lock(&self.events)
            .iter()
            .filter_map(|event| match event {
                WorkerEvent::Platform(event) => Some(event.clone()),
                _ => None,
            })
            .collect()
    }

    fn sent_count(&self) -> usize {
        lock(&self.sent).len()
    }

    fn sent_packets(&self) -> Vec<AudioPacket> {
        lock(&self.sent).iter().map(|(_, p)| p.clone()).collect()
    }

    fn stats(&self) -> WorkerStats {
        self.w().stats()
    }

    fn set_send_mode(&self, mode: SendMode) {
        *lock(&self.send_mode) = mode;
    }

    /// Hold every send at the gate until `release_sends`.
    fn hold_sends(&self) {
        self.send_gate.close();
        self.set_send_mode(SendMode::Gated);
    }

    fn release_sends(&self) {
        self.set_send_mode(SendMode::Ok);
        self.send_gate.release();
    }

    /// Run `f` on the data thread whenever it reaches a stage that has a mark.
    fn on_stage(&self, f: impl Fn(Stage, AudioKey, &[f32]) + Send + Sync + 'static) {
        *lock(&self.shared().hook) = Some(Arc::new(f));
    }

    fn shutdown(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.shutdown();
        }
    }

    /// An app plays into the virtual speaker of a worker that has died, and the data thread's
    /// usual period passes a few times over: nothing is sent.
    fn assert_nothing_sent_after_death(&self, peer: NodeId) {
        assert!(self.shared().is_shutdown(), "the worker has not died");
        let sent = self.sent_count();
        if lock(&self.probe.peers).contains_key(&peer) {
            self.probe.speak(peer, &tone_frame(0));
        }
        thread::sleep(TICK * 5);
        assert_eq!(self.sent_count(), sent, "a frame was sent after the death");
    }
}

fn wait_until(what: &str, timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(1));
    }
}

fn node(n: u8) -> NodeId {
    NodeId([n; 32])
}

fn key(peer: NodeId, stream: u16, generation: u64) -> AudioKey {
    AudioKey {
        peer,
        stream: AudioStreamId(stream),
        generation,
    }
}

/// A worker with no threads at all, for tests that look at the caller side alone.
fn bare_worker(events: Arc<dyn Fn(WorkerEvent) + Send + Sync>) -> AudioWorker {
    AudioWorker {
        shared: Arc::new(Shared::new(events)),
        clock: FakeClock::new().as_clock(),
        threads: Mutex::new(None),
    }
}

// ---- Signals ----

/// Frame number `index` of a 1 kHz stereo tone at amplitude 0.5 (interleaved, left == right).
fn tone_frame(index: usize) -> Vec<f32> {
    (0..AUDIO_FRAME_SAMPLES)
        .flat_map(|n| {
            let t = (index * AUDIO_FRAME_SAMPLES + n) as f64 / 48_000.0;
            let sample = (0.5 * (2.0 * PI * 1000.0 * t).sin()) as f32;
            [sample, sample]
        })
        .collect()
}

/// A frame whose left channel is the tone and whose right channel is silent.
fn left_only_frame(index: usize) -> Vec<f32> {
    tone_frame(index)
        .chunks(2)
        .flat_map(|pair| [pair[0], 0.0])
        .collect()
}

/// Encoded packets of the tone for `stream`, as a remote sender would produce them.
fn tone_packets(stream: u16, frames: usize) -> Vec<AudioPacket> {
    let mut encoder = Encoder::new(AudioKind::Speaker).expect("encoder");
    (0..frames)
        .map(|i| AudioPacket {
            stream: AudioStreamId(stream),
            seq: i as u32,
            sample_time: (i * AUDIO_FRAME_SAMPLES) as u64,
            opus: encoder.encode(&tone_frame(i)).expect("encode"),
        })
        .collect()
}

/// Amplitude of the `freq` Hz component of `samples` (Goertzel; exact when the window holds a
/// whole number of cycles).
fn amplitude_at(samples: &[f32], freq: f64) -> f64 {
    let w = 2.0 * PI * freq / 48_000.0;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &x in samples {
        let s = f64::from(x) + coeff * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    2.0 * power.max(0.0).sqrt() / samples.len() as f64
}

fn left(interleaved: &[f32]) -> Vec<f32> {
    interleaved.chunks(2).map(|c| c[0]).collect()
}

fn right(interleaved: &[f32]) -> Vec<f32> {
    interleaved.chunks(2).map(|c| c[1]).collect()
}

/// Feed `packets` for `peer` one per 10 ms of fake time.
fn play_in(h: &Harness, peer: NodeId, packets: &[AudioPacket]) {
    for packet in packets {
        h.w().packet(peer, packet.clone());
        h.step();
    }
}

// ---- 1. Tone through the send side and back through the receive side ----

#[test]
fn tone_round_trips_through_send_and_playout() {
    let h = Harness::new();
    let peer = node(1);
    h.add_peer(peer);

    // Send side: 500 ms of a 1 kHz tone played into the virtual speaker.
    let sender = key(peer, 1, 1);
    h.start(sender, AudioEndpoint::VirtualSpeaker);
    let pcm: Vec<f32> = (0..50).flat_map(tone_frame).collect();
    h.probe.speak(peer, &pcm);
    wait_until("50 packets", Duration::from_secs(5), || {
        h.sent_count() >= 50
    });
    h.settle();
    let packets = h.sent_packets();
    assert_eq!(packets.len(), 50);
    for (i, packet) in packets.iter().enumerate() {
        assert_eq!(packet.stream, AudioStreamId(1));
        assert_eq!(packet.seq, i as u32);
        assert_eq!(packet.sample_time, (i * AUDIO_FRAME_SAMPLES) as u64);
        assert!(!packet.opus.is_empty() && packet.opus.len() <= 400);
    }
    assert!(lock(&h.sent).iter().all(|(p, _)| *p == peer));
    assert_eq!(h.stats().sent, 50);

    // Receive side: the same packets, re-addressed to a playback stream, one per 10 ms of fake
    // time. Nothing is played before the initial 40 ms jitter delay has run out on the clock;
    // after it, exactly one whole frame arrives per 10 ms step.
    let receiver = key(peer, 2, 2);
    h.open_playback(receiver);
    assert_eq!(*lock(&h.probe.formats), vec![STEREO]);
    h.start(receiver, AudioEndpoint::LocalPlayback);
    let mut played = Vec::new();
    for (i, packet) in packets.iter().enumerate() {
        h.w().packet(
            peer,
            AudioPacket {
                stream: AudioStreamId(2),
                ..packet.clone()
            },
        );
        h.step();
        played.extend(h.probe.played(0));
        // The first packet arrived at step 0; the deadline is 40 ms later, which is reached by
        // the clock at the end of step 3.
        let due_frames = i.saturating_sub(2);
        assert_eq!(
            played.len(),
            due_frames * 960,
            "after {} ms of fake time",
            (i + 1) * 10
        );
    }

    // Skip the jitter delay and codec warm-up, then look at 100 ms (a whole number of cycles).
    let window = &played[200 * 96..300 * 96];
    let (l, r) = (left(window), right(window));
    for channel in [&l, &r] {
        let level = amplitude_at(channel, 1000.0);
        assert!(
            (0.45..=0.55).contains(&level),
            "1 kHz level {level}, expected about 0.5"
        );
        for other in [500.0, 1500.0, 2000.0, 3000.0] {
            assert!(
                amplitude_at(channel, other) < 0.02,
                "unexpected energy at {other} Hz"
            );
        }
    }
    assert_eq!(h.failed(sender) + h.failed(receiver), 0);
}

// ---- 2. A slow host call stalls nothing ----

#[test]
fn slow_open_does_not_stall_playout_stop_or_callers() {
    let h = Harness::new();
    let peer = node(1);
    let (a, b) = (key(peer, 1, 1), key(peer, 3, 2));
    h.open_playback(a);
    h.start(a, AudioEndpoint::LocalPlayback);

    // From now on the host thread is held inside B's open until the test says otherwise.
    let host = h.probe.hold_opens();
    h.submit(Output::OpenAudioPlayback { key: b });
    host.wait_entered(1);

    // Playout of A carries on, and no caller waits for the host.
    let packets = tone_packets(1, 12);
    let mut slowest = Duration::ZERO;
    for packet in packets.iter().take(8) {
        let started = Instant::now();
        h.w().packet(peer, packet.clone());
        h.submit(Output::StopAudioStream {
            key: key(peer, 9, 99),
        });
        slowest = slowest.max(started.elapsed());
        h.clock.advance(Duration::from_millis(10));
        h.wait_applied();
        h.run_passes(2);
    }
    assert!(
        slowest < Duration::from_millis(300),
        "a caller waited {slowest:?} behind a host call"
    );
    assert!(
        h.probe.played(0).len() >= 3 * 960,
        "A's playout was delayed"
    );

    // A stop takes effect while the host is still blocked: nothing more reaches A.
    h.submit(Output::StopAudioStream { key: a });
    h.wait_applied();
    for packet in packets.iter().skip(8) {
        h.w().packet(peer, packet.clone());
        h.clock.advance(Duration::from_millis(10));
        h.run_passes(2);
    }
    assert!(
        h.probe.played(0).is_empty(),
        "A kept playing after its stop"
    );
    assert_eq!(h.stats().rx_unknown, 4);

    // Cancel while B's open is still held: the late handle is dropped and nothing is reported.
    let begun = Instant::now();
    h.w().cancel_peer(peer);
    h.wait_applied();
    assert!(
        begun.elapsed() < Duration::from_secs(1),
        "cancel waited for the host"
    );
    assert_eq!(h.probe.stopped(0), 1, "cancel_peer stops A's playback");
    host.release();
    h.barrier();
    assert_eq!(h.probe.playbacks_opened(), 2);
    assert_eq!(h.probe.stopped(1), 1, "B's late handle was kept");
    assert!(h.opened(b).is_empty(), "a cancelled open was reported");
    assert_eq!(h.stats().stale_replies, 1);
}

#[test]
fn an_open_queued_behind_a_slow_one_is_withdrawn_when_closed() {
    let h = Harness::new();
    let peer = node(1);
    let (a, b) = (key(peer, 1, 1), key(peer, 3, 2));
    let host = h.probe.hold_opens();
    h.submit(Output::OpenAudioPlayback { key: a });
    host.wait_entered(1);
    h.submit(Output::OpenAudioPlayback { key: b });
    h.submit(Output::CloseAudioPlayback { key: b });
    h.wait_applied();
    host.release();
    h.barrier();
    assert_eq!(h.probe.open_playback_calls.load(Ordering::SeqCst), 1);
    assert_eq!(h.opened(a), vec![(AudioKind::Speaker, Ok(()))]);
    assert!(h.opened(b).is_empty());
    assert_eq!(h.probe.playbacks_opened(), 1);
}

#[test]
fn open_failures_are_reported_once_with_the_engine_failure() {
    let h = Harness::new();
    let peer = node(1);
    h.probe.fail_open.store(true, Ordering::SeqCst);
    let k = key(peer, 1, 1);
    h.submit(Output::OpenAudioPlayback { key: k });
    h.settle();
    assert_eq!(
        h.opened(k),
        vec![(AudioKind::Speaker, Err(Failure::PermissionDenied))]
    );
    // The failed session freed its slot, and a start on it fails.
    h.start(k, AudioEndpoint::LocalPlayback);
    assert_eq!(h.failed(k), 1);
}

// ---- 3. Unknown, misrouted, stopped and stale-generation packets and keys ----

#[test]
fn stray_packets_create_no_codec_buffer_or_session_and_leave_the_table_as_it_was() {
    let h = Harness::new();
    let peer = node(1);
    let k1 = key(peer, 5, 1);
    h.open_playback(k1);
    // Opened but not started: dropped.
    let packets = tone_packets(5, 6);
    h.w().packet(peer, packets[0].clone());
    assert_eq!(h.stats().rx_unknown, 1);

    h.start(k1, AudioEndpoint::LocalPlayback);
    let before = h.stats();
    assert_eq!(
        (
            before.sessions_created,
            before.jitters_created,
            before.encoders_created
        ),
        (1, 1, 0)
    );
    let capacities = {
        let rx = lock(&h.shared().rx);
        assert_eq!(rx.slots.len(), 1);
        (rx.slots.capacity(), rx.slots[0].queue.capacity())
    };
    // Wrong stream and wrong peer.
    h.w().packet(peer, tone_packets(6, 1).remove(0));
    h.w().packet(node(2), packets[0].clone());
    h.settle();
    let after = h.stats();
    assert_eq!(after.rx_unknown, 3);
    // Nothing was created for them: the counters count every session, encoder and jitter buffer
    // (decoder) the worker ever constructs.
    assert_eq!(
        (
            after.sessions_created,
            after.jitters_created,
            after.encoders_created
        ),
        (1, 1, 0)
    );
    {
        // And the receive table kept its capacity: it did not grow to hold them.
        let rx = lock(&h.shared().rx);
        assert_eq!(rx.slots.len(), 1, "a stray packet created a stream");
        assert_eq!(rx.slots[0].queue.len(), 0);
        assert_eq!(
            (rx.slots.capacity(), rx.slots[0].queue.capacity()),
            capacities,
            "the receive table's capacity changed"
        );
    }

    // A stopped key's packets are dropped, and its slot is gone.
    h.stop(k1);
    h.w().packet(peer, packets[1].clone());
    assert_eq!(h.stats().rx_unknown, 4);
    assert_eq!(lock(&h.shared().rx).slots.len(), 0);
    assert!(h.probe.played(0).is_empty());
}

#[test]
fn packets_queued_for_an_older_key_never_reach_a_newer_key_on_the_same_stream() {
    let h = Harness::new();
    let peer = node(1);
    let (old, new) = (key(peer, 5, 1), key(peer, 5, 2));
    h.open_playback(old);
    h.start(old, AudioEndpoint::LocalPlayback);

    // Hold the data thread (inside a report) so that nothing it could do consumes the packets.
    h.event_gate.close();
    h.submit(Output::OpenAudioCapture {
        key: key(peer, 8, 9),
    });
    h.event_gate.wait_entered(1);
    for packet in tone_packets(5, 4) {
        h.w().packet(peer, packet);
    }
    // The old key's packets are really still queued, waiting for a thread that has not looked.
    {
        let rx = lock(&h.shared().rx);
        assert_eq!(rx.slots.len(), 1);
        assert_eq!(rx.slots[0].key, old);
        assert_eq!(rx.slots[0].queue.len(), 4, "the packets were not queued");
    }
    // The old key is stopped and closed with those packets still queued...
    h.submit(Output::StopAudioStream { key: old });
    h.submit(Output::CloseAudioPlayback { key: old });
    h.event_gate.release();
    h.barrier();
    assert_eq!(h.probe.stopped(0), 1);
    assert!(lock(&h.shared().rx).slots.is_empty());
    assert!(h.probe.played(0).is_empty());

    // ...and the key that takes over its stream ID hears none of them.
    h.open_playback(new);
    h.start(new, AudioEndpoint::LocalPlayback);
    assert_eq!(lock(&h.shared().rx).slots[0].queue.len(), 0);
    // 300 ms with no packets for the new key: it must stay silent.
    for _ in 0..30 {
        h.step();
    }
    assert!(h.probe.played(1).is_empty());
    assert_eq!(h.stats().rx_rejected, 0);
}

#[test]
fn a_newer_key_on_a_reused_stream_is_isolated_from_a_held_older_open() {
    // AudioPacket carries no generation, and AUDIO-v0 §8 forbids reusing a stream ID, so the
    // engine never does this. The worker still keeps the two keys apart by their full keys.
    let h = Harness::new();
    let peer = node(1);
    let (old, new) = (key(peer, 5, 1), key(peer, 5, 2));

    // The old key's open is held inside the host; the engine cancels it and admits the new key.
    let host = h.probe.hold_opens();
    h.submit(Output::OpenAudioPlayback { key: old });
    host.wait_entered(1);
    h.submit(Output::CloseAudioPlayback { key: old });
    h.submit(Output::OpenAudioPlayback { key: new });
    h.wait_applied();
    // Releasing the old open lets it complete (stale), and then the new key's open proceeds.
    host.release();
    h.settle();

    // Handles: the old key's was dropped, the new key's was kept. Reports: only the new key's.
    assert_eq!(h.probe.playbacks_opened(), 2);
    assert_eq!(h.probe.stopped(0), 1, "the old key's late handle was kept");
    assert_eq!(h.probe.stopped(1), 0, "the new key's handle was dropped");
    assert!(h.opened(old).is_empty(), "the old key was reported");
    assert_eq!(h.opened(new), vec![(AudioKind::Speaker, Ok(()))]);
    assert_eq!(h.stats().stale_replies, 1);

    // The new key plays; its audio goes to its own handle only.
    h.start(new, AudioEndpoint::LocalPlayback);
    let packets = tone_packets(5, 12);
    play_in(&h, peer, &packets);
    assert!(!h.probe.played(1).is_empty());
    assert!(h.probe.played(0).is_empty());

    // A delayed packet of the old key's timeline arrives after the replacement. It cannot be told
    // apart by generation, but it does not match the stream's sample clock, so it is rejected and
    // adds nothing to the new key's audio.
    let rejected = h.stats().rx_rejected;
    h.w().packet(
        peer,
        AudioPacket {
            stream: AudioStreamId(5),
            seq: 2,
            sample_time: 7_000_000,
            opus: packets[2].opus.clone(),
        },
    );
    h.settle();
    assert_eq!(h.stats().rx_rejected, rejected + 1);
    assert_eq!(h.failed(new), 0);
}

#[test]
fn stop_and_close_for_an_older_key_leave_the_newer_one_running() {
    let h = Harness::new();
    let peer = node(1);
    let (old, new) = (key(peer, 5, 1), key(peer, 5, 2));
    h.open_playback(old);
    h.start(old, AudioEndpoint::LocalPlayback);
    h.stop(old);
    h.close(old);
    h.open_playback(new);
    h.start(new, AudioEndpoint::LocalPlayback);

    h.stop(old);
    h.close(old);
    h.submit(Output::StopAudioStream {
        key: key(peer, 5, 0),
    });
    h.submit(Output::CloseAudioPlayback {
        key: key(peer, 5, 99),
    });
    h.settle();
    assert_eq!(h.probe.stopped(1), 0, "the newer playback was closed");
    play_in(&h, peer, &tone_packets(5, 12));
    assert!(
        !h.probe.played(1).is_empty(),
        "the newer stream stopped playing"
    );
    assert_eq!(h.failed(new), 0);
}

#[test]
fn the_receive_queue_holds_eight_packets_and_never_grows() {
    // Built by hand so no data thread competes for the queue.
    let worker = bare_worker(Arc::new(|_| {}));
    let shared = Arc::clone(&worker.shared);
    let peer = node(1);
    let k = key(peer, 7, 1);
    lock(&shared.rx).slots.push(RxSlot {
        key: k,
        queue: VecDeque::with_capacity(MAX_QUEUED_PACKETS),
    });
    let slots_capacity = lock(&shared.rx).slots.capacity();
    let queue_capacity = lock(&shared.rx).slots[0].queue.capacity();
    for packet in tone_packets(7, 20) {
        worker.packet(peer, packet);
    }
    let stats = shared.counters.snapshot();
    assert_eq!(stats.rx_overflow, 12);
    let rx = lock(&shared.rx);
    assert_eq!(rx.slots[0].queue.len(), MAX_QUEUED_PACKETS);
    assert_eq!(rx.slots[0].queue.capacity(), queue_capacity);
    assert_eq!(rx.slots.capacity(), slots_capacity);
}

// ---- 4. Microphones are refused ----

#[test]
fn microphones_are_refused_without_touching_a_device() {
    let h = Harness::new();
    let peer = node(1);
    h.add_peer(peer);
    let capture = key(peer, 2, 1);
    h.submit(Output::OpenAudioCapture { key: capture });
    h.settle();
    assert_eq!(
        h.opened(capture),
        vec![(AudioKind::Microphone, Err(Failure::Other))]
    );
    assert_eq!(h.probe.open_capture_calls.load(Ordering::SeqCst), 0);

    for (k, kind, endpoint) in [
        (
            key(peer, 2, 2),
            AudioKind::Microphone,
            AudioEndpoint::VirtualMicrophone,
        ),
        (
            key(peer, 2, 3),
            AudioKind::Microphone,
            AudioEndpoint::LocalCapture,
        ),
        (
            key(peer, 2, 4),
            AudioKind::Speaker,
            AudioEndpoint::LocalCapture,
        ),
        (
            key(peer, 2, 5),
            AudioKind::Speaker,
            AudioEndpoint::VirtualMicrophone,
        ),
        (
            key(peer, 2, 6),
            AudioKind::Microphone,
            AudioEndpoint::VirtualSpeaker,
        ),
    ] {
        h.submit(Output::StartAudioStream {
            key: k,
            kind,
            endpoint,
        });
        h.settle();
        assert_eq!(h.failed(k), 1, "{kind:?}/{endpoint:?} was not refused");
    }
    h.submit(Output::CloseAudioCapture { key: capture });
    h.submit(Output::StopAudioStream { key: capture });
    h.settle();

    // Nothing started: PCM played into the virtual speaker is not sent, and the virtual
    // microphone was never written.
    h.probe.speak(peer, &tone_frame(0));
    h.settle();
    assert_eq!(h.sent_count(), 0);
    assert_eq!(h.probe.mic_samples_written(), 0);
    assert_eq!(h.probe.open_capture_calls.load(Ordering::SeqCst), 0);
    assert_eq!(h.probe.playbacks_opened(), 0);
    assert!(
        !lock(&h.probe.peers)[&peer].speaker_in.is_abandoned(),
        "the worker still holds the peer's ports"
    );
}

// ---- 5. Congestion and send errors ----

#[test]
fn congestion_drops_packets_without_failing_the_stream() {
    let h = Harness::new();
    let peer = node(1);
    h.add_peer(peer);
    let k = key(peer, 1, 1);
    h.start(k, AudioEndpoint::VirtualSpeaker);
    h.set_send_mode(SendMode::CongestEvery(3));
    let pcm: Vec<f32> = (0..12).flat_map(tone_frame).collect();
    h.probe.speak(peer, &pcm);
    wait_until("12 attempts", Duration::from_secs(5), || {
        let stats = h.stats();
        stats.sent + stats.congested >= 12
    });
    h.settle();
    let stats = h.stats();
    assert_eq!((stats.sent, stats.congested), (8, 4));
    assert_eq!(h.failed(k), 0);
    // A dropped packet still used up its slot in the timeline, so the receiver sees the gap.
    let seqs: Vec<u32> = h.sent_packets().iter().map(|p| p.seq).collect();
    assert_eq!(seqs, vec![0, 1, 3, 4, 6, 7, 9, 10]);
    let times: Vec<u64> = h.sent_packets().iter().map(|p| p.sample_time).collect();
    assert_eq!(
        times,
        seqs.iter().map(|s| u64::from(*s) * 480).collect::<Vec<_>>()
    );

    // Still running: the next frame goes out.
    h.set_send_mode(SendMode::Ok);
    h.probe.speak(peer, &tone_frame(12));
    wait_until("the next packet", Duration::from_secs(5), || {
        h.sent_count() == 9
    });
    assert_eq!(h.sent_packets()[8].seq, 12);
    assert_eq!(h.failed(k), 0);
}

#[test]
fn any_other_send_error_fails_the_stream_once() {
    let h = Harness::new();
    let peer = node(1);
    h.add_peer(peer);
    let k = key(peer, 1, 1);
    h.start(k, AudioEndpoint::VirtualSpeaker);
    h.set_send_mode(SendMode::Closed);
    h.probe
        .speak(peer, &(0..4).flat_map(tone_frame).collect::<Vec<_>>());
    wait_until("the failure", Duration::from_secs(5), || h.failed(k) >= 1);
    h.settle();
    assert_eq!(h.failed(k), 1);
    // The failed stream sends no more and reports nothing again.
    h.set_send_mode(SendMode::Ok);
    h.probe
        .speak(peer, &(0..4).flat_map(tone_frame).collect::<Vec<_>>());
    h.settle();
    assert_eq!(h.sent_count(), 0);
    assert_eq!(h.failed(k), 1);
    assert_eq!(
        h.probe.speaker_room(peer),
        96_000,
        "stale PCM was left in the ring"
    );

    // The engine stops the failed key; a fresh key works from sequence 0.
    h.stop(k);
    let k2 = key(peer, 3, 2);
    h.start(k2, AudioEndpoint::VirtualSpeaker);
    h.probe.speak(peer, &tone_frame(0));
    wait_until("a packet", Duration::from_secs(5), || h.sent_count() == 1);
    let packet = &h.sent_packets()[0];
    assert_eq!(
        (packet.stream, packet.seq, packet.sample_time),
        (AudioStreamId(3), 0, 0)
    );
}

#[test]
fn out_of_range_samples_are_repaired_not_fatal() {
    let h = Harness::new();
    let peer = node(1);
    h.add_peer(peer);
    let k = key(peer, 1, 1);
    h.start(k, AudioEndpoint::VirtualSpeaker);
    let mut frame = tone_frame(0);
    frame[0] = 3.0;
    frame[1] = f32::NAN;
    frame[2] = f32::NEG_INFINITY;
    h.probe.speak(peer, &frame);
    wait_until("the packet", Duration::from_secs(5), || h.sent_count() == 1);
    assert_eq!(h.failed(k), 0);
    assert_eq!(h.stats().sanitized_frames, 1);
}

#[test]
fn a_second_sender_for_the_same_peer_is_refused_and_the_first_keeps_running() {
    let h = Harness::new();
    let peer = node(1);
    h.add_peer(peer);
    let (first, second) = (key(peer, 1, 1), key(peer, 3, 2));
    h.start(first, AudioEndpoint::VirtualSpeaker);
    h.start(second, AudioEndpoint::VirtualSpeaker);
    assert_eq!(h.failed(second), 1);
    assert_eq!(h.failed(first), 0);
    h.probe.speak(peer, &tone_frame(0));
    wait_until("a packet", Duration::from_secs(5), || h.sent_count() == 1);
    assert_eq!(h.sent_packets()[0].stream, AudioStreamId(1));
}

#[test]
fn a_sender_needs_the_peers_virtual_devices() {
    let h = Harness::new();
    let peer = node(1);
    let k = key(peer, 1, 1);
    h.start(k, AudioEndpoint::VirtualSpeaker);
    assert_eq!(h.failed(k), 1);
    // And a playback stream needs its open device.
    let p = key(peer, 2, 2);
    h.start(p, AudioEndpoint::LocalPlayback);
    assert_eq!(h.failed(p), 1);
}

// ---- 6. Stale PCM and partial frames ----

#[test]
fn stale_pcm_is_never_sent_and_partial_frames_are_never_encoded() {
    let h = Harness::new();
    let peer = node(1);
    h.add_peer(peer);

    // Played while nothing was started: drained, never sent.
    h.probe
        .speak(peer, &(0..3).flat_map(tone_frame).collect::<Vec<_>>());
    h.settle();
    assert_eq!(
        h.probe.speaker_room(peer),
        96_000,
        "idle PCM was not drained"
    );
    // Played just before a start, possibly before any pass saw it: discarded at the start.
    h.probe
        .speak(peer, &(0..3).flat_map(tone_frame).collect::<Vec<_>>());
    let k = key(peer, 1, 1);
    h.start(k, AudioEndpoint::VirtualSpeaker);
    h.settle();
    assert_eq!(h.sent_count(), 0, "stale PCM was sent");

    // One and a half frames: one packet, and the half frame waits.
    let frames: Vec<f32> = (0..2).flat_map(tone_frame).collect();
    h.probe.speak(peer, &frames[..1440]);
    wait_until("one packet", Duration::from_secs(5), || h.sent_count() == 1);
    h.settle();
    assert_eq!(h.sent_count(), 1, "a partial frame was encoded");
    assert_eq!(h.probe.speaker_room(peer), 96_000 - 480);
    // The other half arrives: the second packet carries one whole, aligned frame.
    h.probe.speak(peer, &frames[1440..1920]);
    wait_until("two packets", Duration::from_secs(5), || {
        h.sent_count() == 2
    });
    h.settle();
    assert_eq!(h.sent_count(), 2);
    assert_eq!(h.probe.speaker_room(peer), 96_000);
    let seqs: Vec<u32> = h.sent_packets().iter().map(|p| p.seq).collect();
    assert_eq!(seqs, vec![0, 1]);

    // After the stop, new PCM is drained again and a restart does not send it.
    h.stop(k);
    h.probe.speak(peer, &tone_frame(0));
    h.settle();
    assert_eq!(h.probe.speaker_room(peer), 96_000);
    h.start(key(peer, 3, 2), AudioEndpoint::VirtualSpeaker);
    h.settle();
    assert_eq!(h.sent_count(), 2);
    assert!(h.stats().discarded_samples >= 3 * 960 + 3 * 960 + 960);
}

#[test]
fn an_odd_stale_sample_across_the_start_boundary_is_dropped_with_its_other_half() {
    let h = Harness::new();
    let peer = node(1);
    h.add_peer(peer);
    // The frames the sender is given, as the encoder is about to see them.
    let seen: Arc<Mutex<Vec<Vec<f32>>>> = Arc::new(Mutex::new(Vec::new()));
    h.on_stage({
        let seen = seen.clone();
        move |stage, _, pcm| {
            if stage == Stage::Encoded {
                lock(&seen).push(pcm.to_vec());
            }
        }
    });

    // Three stale samples while idle: the whole pair is drained, and the lone left sample of the
    // next pair waits for its right half.
    h.probe.speak(peer, &[0.9, 0.9, 0.9]);
    wait_until("the idle drain", Duration::from_secs(5), || {
        h.probe.speaker_room(peer) == 96_000 - 1
    });
    h.start(key(peer, 1, 1), AudioEndpoint::VirtualSpeaker);
    // The start dropped that lone sample. Its right half, still to come, is stale too; then comes
    // the first real frame, with a silent right channel so a one-sample shift would show.
    let mut audio = vec![0.9];
    audio.extend(left_only_frame(0));
    audio.extend(left_only_frame(1));
    h.probe.speak(peer, &audio);
    wait_until("two packets", Duration::from_secs(5), || {
        h.sent_count() == 2
    });
    h.settle();

    let seen = lock(&seen);
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[0],
        left_only_frame(0),
        "the first frame is not aligned"
    );
    assert_eq!(seen[1], left_only_frame(1));
    assert_eq!(h.probe.speaker_room(peer), 96_000);
}

#[test]
fn a_sample_the_backend_is_still_writing_keeps_its_channel() {
    let h = Harness::new();
    let peer = node(1);
    h.add_peer(peer);
    // Three samples while idle: one whole frame goes, the lone left sample waits for its right.
    h.probe.speak(peer, &[0.1, 0.2, 0.3]);
    h.settle();
    assert_eq!(h.probe.speaker_room(peer), 96_000 - 1);
    h.probe.speak(peer, &[0.4]);
    h.settle();
    assert_eq!(h.probe.speaker_room(peer), 96_000);
}

#[test]
fn closing_a_playback_never_touches_a_sender() {
    let h = Harness::new();
    let peer = node(1);
    h.add_peer(peer);
    let k = key(peer, 1, 1);
    h.start(k, AudioEndpoint::VirtualSpeaker);
    h.close(k);
    h.probe.speak(peer, &tone_frame(0));
    wait_until("a packet", Duration::from_secs(5), || h.sent_count() == 1);
    assert_eq!(h.failed(k), 0);
}

// ---- 7. Cancellation, removal and shutdown ----

#[test]
fn cancel_peer_stops_everything_for_that_peer_only() {
    let h = Harness::new();
    let (p, q) = (node(1), node(2));
    h.add_peer(p);
    h.add_peer(q);
    let (send_p, play_p, send_q) = (key(p, 1, 1), key(p, 2, 2), key(q, 1, 3));
    h.start(send_p, AudioEndpoint::VirtualSpeaker);
    h.open_playback(play_p);
    h.start(play_p, AudioEndpoint::LocalPlayback);
    h.start(send_q, AudioEndpoint::VirtualSpeaker);
    let events_before = h.event_count();

    h.w().cancel_peer(p);
    h.settle();
    assert_eq!(h.probe.stopped(0), 1, "P's playback was not stopped");
    assert_eq!(lock(&h.shared().rx).slots.len(), 0);
    // P's PCM is no longer sent; Q's still is.
    let sent_before = h.sent_count();
    h.probe.speak(p, &tone_frame(0));
    h.probe.speak(q, &tone_frame(0));
    wait_until("Q's packet", Duration::from_secs(5), || {
        h.sent_count() == sent_before + 1
    });
    h.settle();
    assert_eq!(h.sent_count(), sent_before + 1);
    assert!(lock(&h.sent).last().is_some_and(|(peer, _)| *peer == q));
    // P's packets are dropped, and nothing was reported for the cancelled keys.
    h.w().packet(p, tone_packets(2, 1).remove(0));
    assert_eq!(h.stats().rx_unknown, 1);
    assert_eq!(h.event_count(), events_before);
    // P's virtual devices survive a cancel (the connection may just have been replaced).
    h.start(key(p, 5, 4), AudioEndpoint::VirtualSpeaker);
    assert_eq!(h.failed(key(p, 5, 4)), 0);
}

/// What the fake host saw at the moment `remove_peer` was called.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AtRemoval {
    /// Playback handles the backend had been told to stop.
    playbacks_stopped: usize,
    /// Receive streams still registered for packets.
    receive_slots: usize,
    /// Whether the worker had already let go of the virtual speaker's ring.
    ports_released: bool,
    /// Packets sent so far.
    packets_sent: usize,
}

#[test]
fn removing_a_peer_stops_working_streams_before_its_devices_go() {
    let h = Harness::new();
    let peer = node(1);
    h.add_peer(peer);
    let (send_key, play_key) = (key(peer, 1, 1), key(peer, 2, 2));
    h.start(send_key, AudioEndpoint::VirtualSpeaker);
    h.open_playback(play_key);
    h.start(play_key, AudioEndpoint::LocalPlayback);

    // Both streams are working: audio is sent, and received audio is played.
    h.probe
        .speak(peer, &(0..3).flat_map(tone_frame).collect::<Vec<_>>());
    wait_until("packets sent", Duration::from_secs(5), || {
        h.sent_count() >= 3
    });
    play_in(&h, peer, &tone_packets(2, 12));
    assert!(!h.probe.played(0).is_empty(), "playback was not working");
    assert_eq!(h.probe.stopped(0), 0);
    assert_eq!(lock(&h.shared().rx).slots.len(), 1);

    // The host records what the worker had done by the time it was asked to remove the devices.
    let seen: Arc<Mutex<Option<AtRemoval>>> = Arc::new(Mutex::new(None));
    *lock(&h.probe.remove_hook) = Some(Arc::new({
        let (seen, shared, sent) = (seen.clone(), h.shared().clone(), h.sent.clone());
        move |probe: &HostProbe| {
            *lock(&seen) = Some(AtRemoval {
                playbacks_stopped: probe.stops(),
                receive_slots: lock(&shared.rx).slots.len(),
                ports_released: lock(&probe.peers)[&peer].speaker_in.is_abandoned(),
                packets_sent: lock(&sent).len(),
            });
        }
    }));

    // The sender is caught mid-send with two more frames waiting; the peer is removed meanwhile.
    let sent_before = h.sent_count();
    h.hold_sends();
    h.probe
        .speak(peer, &(0..3).flat_map(tone_frame).collect::<Vec<_>>());
    h.send_gate.wait_entered(1);
    let events_before = h.event_count();
    h.submit(Output::RemoveAudioPeer { peer });
    h.release_sends();
    h.settle();

    assert_eq!(h.probe.remove_calls.load(Ordering::SeqCst), 1);
    let at_removal = lock(&seen).expect("remove_peer was called");
    assert_eq!(
        at_removal.playbacks_stopped, 1,
        "the playback was still open when the devices were removed"
    );
    assert_eq!(
        at_removal.receive_slots, 0,
        "the receive stream was still registered when the devices were removed"
    );
    assert!(
        at_removal.ports_released,
        "the worker still held the virtual speaker when the devices were removed"
    );
    // Only the send that was already inside the link completed; the two queued frames never went.
    assert_eq!(at_removal.packets_sent, sent_before + 1);
    assert_eq!(h.sent_count(), sent_before + 1);
    // Delivery stays stopped: received packets are dropped and nothing plays.
    let unknown = h.stats().rx_unknown;
    h.w().packet(peer, tone_packets(2, 1).remove(0));
    h.clock.advance(Duration::from_millis(50));
    h.settle();
    assert_eq!(h.stats().rx_unknown, unknown + 1);
    assert!(h.probe.played(0).is_empty());
    // The engine ended these streams itself: no failure is reported for them.
    assert_eq!(h.event_count(), events_before);
    // The device is gone: a start for the old peer fails, and the peer can come back.
    h.start(key(peer, 3, 3), AudioEndpoint::VirtualSpeaker);
    assert_eq!(h.failed(key(peer, 3, 3)), 1);
    h.add_peer(peer);
    assert_eq!(h.probe.add_calls.load(Ordering::SeqCst), 2);
    h.start(key(peer, 5, 4), AudioEndpoint::VirtualSpeaker);
    assert_eq!(h.failed(key(peer, 5, 4)), 0);
}

#[test]
fn peer_add_and_remove_are_reconciled_one_host_call_at_a_time() {
    let h = Harness::new();
    let peer = node(1);
    let host = h.probe.hold_adds();
    // Add, remove, add while the first add is still running: one device set, no churn.
    h.submit(Output::AddAudioPeer {
        peer,
        name: "p".into(),
    });
    host.wait_entered(1);
    h.submit(Output::RemoveAudioPeer { peer });
    h.submit(Output::AddAudioPeer {
        peer,
        name: "p".into(),
    });
    h.wait_applied();
    host.release();
    h.settle();
    assert_eq!(h.probe.add_calls.load(Ordering::SeqCst), 1);
    assert_eq!(h.probe.remove_calls.load(Ordering::SeqCst), 0);
    h.start(key(peer, 1, 1), AudioEndpoint::VirtualSpeaker);
    assert_eq!(h.failed(key(peer, 1, 1)), 0);

    // A new name replaces the devices: remove, then add.
    h.submit(Output::AddAudioPeer {
        peer,
        name: "renamed".into(),
    });
    h.settle();
    assert_eq!(h.probe.remove_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        *lock(&h.probe.names),
        vec!["p".to_string(), "renamed".to_string()]
    );
    assert_eq!(h.probe.add_calls.load(Ordering::SeqCst), 2);
    // The sender that was using the replaced devices failed (once), rather than hanging on.
    assert_eq!(h.failed(key(peer, 1, 1)), 1);
}

#[test]
fn a_failed_peer_add_is_reported_and_not_retried() {
    let h = Harness::new();
    let peer = node(1);
    h.probe.fail_add.store(true, Ordering::SeqCst);
    h.add_peer(peer);
    assert_eq!(h.probe.add_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        h.platform_events(),
        vec![AudioEvent::DeviceError {
            peer: Some(peer),
            kind: AudioKind::Speaker,
            error: AudioDeviceError::Failed,
        }]
    );
    h.run_passes(8);
    assert_eq!(h.probe.add_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn shutdown_joins_a_compliant_host_call_and_stops_its_handle_before_it_returns() {
    let mut h = Harness::new();
    let peer = node(1);
    h.add_peer(peer);
    let sender = key(peer, 1, 1);
    let receiver = key(peer, 2, 2);
    let slow = key(peer, 4, 3);
    h.start(sender, AudioEndpoint::VirtualSpeaker);
    h.open_playback(receiver);
    h.start(receiver, AudioEndpoint::LocalPlayback);
    // A host call at the top of what the trait allows (it is bounded to 2 s).
    h.probe.set_open_delay(Duration::from_millis(1950));
    h.submit(Output::OpenAudioPlayback { key: slow });
    wait_until("the call", Duration::from_secs(5), || {
        h.probe.open_playback_calls.load(Ordering::SeqCst) == 2
    });

    let begun = Instant::now();
    h.shutdown();
    let took = begun.elapsed();
    assert!(took < SHUTDOWN_BUDGET, "shutdown took {took:?}");
    // Joined and stopped by the time shutdown returned: no waiting for any of it.
    assert!(
        h.probe.host_dropped.load(Ordering::SeqCst),
        "the host thread was left running"
    );
    assert_eq!(h.probe.playbacks_opened(), 2);
    assert_eq!(h.probe.stopped(0), 1, "the receiver's playback outlived it");
    assert_eq!(h.probe.stopped(1), 1, "the late handle outlived it");
    assert!(h.opened(slow).is_empty());
    assert_eq!(h.sent_count(), 0);
}

#[test]
fn shutdown_does_not_wait_for_a_thread_local_destructor() {
    let mut h = Harness::new();
    h.probe.slow_thread_exit.store(true, Ordering::SeqCst);
    let k = key(node(1), 1, 1);
    h.open_playback(k);
    let begun = Instant::now();
    h.shutdown();
    // The host thread's closure is done; its thread-local destructor takes three more seconds,
    // which a join would have waited for.
    assert!(
        begun.elapsed() < Duration::from_secs(1),
        "shutdown waited {:?}",
        begun.elapsed()
    );
    assert!(h.probe.host_dropped.load(Ordering::SeqCst));
    assert_eq!(h.probe.stopped(0), 1);
}

#[test]
fn shutdown_does_not_wait_for_a_host_call_past_its_budget() {
    let mut h = Harness::new();
    h.probe.set_open_delay(Duration::from_millis(3200));
    h.submit(Output::OpenAudioPlayback {
        key: key(node(1), 1, 1),
    });
    wait_until("the call", Duration::from_secs(5), || {
        h.probe.open_playback_calls.load(Ordering::SeqCst) == 1
    });
    let begun = Instant::now();
    h.shutdown();
    let took = begun.elapsed();
    assert!(
        took >= SHUTDOWN_BUDGET - Duration::from_millis(100) && took < Duration::from_secs(3),
        "shutdown took {took:?}"
    );
    // The detached host thread finishes its call later, and the handle it opens is dropped there.
    wait_until("the late handle", Duration::from_secs(5), || {
        h.probe.playbacks_opened() == 1 && h.probe.stopped(0) == 1
    });
}

#[test]
fn a_shut_down_worker_ignores_calls_and_reports_nothing_more() {
    let h = Harness::new();
    let peer = node(1);
    h.add_peer(peer);
    let k = key(peer, 2, 1);
    h.open_playback(k);
    h.start(k, AudioEndpoint::LocalPlayback);
    let sink = lock(&h.probe.sink).clone().expect("subscribed");
    sink.send(AudioEvent::VirtualActive {
        peer,
        kind: AudioKind::Speaker,
        active: true,
    });
    assert_eq!(h.platform_events().len(), 1);

    // `stop` is what `shutdown` runs; the handle stays usable so late calls can be observed.
    h.w().stop();
    assert!(h.shared().is_shutdown());
    assert!(h.probe.host_dropped.load(Ordering::SeqCst));
    assert_eq!(h.probe.stopped(0), 1, "the playback outlived the worker");
    let events = h.event_count();

    h.submit(Output::OpenAudioCapture {
        key: key(peer, 4, 2),
    });
    h.submit(Output::StopAudioStream { key: k });
    h.w().cancel_peer(peer);
    h.w().packet(peer, tone_packets(2, 1).remove(0));
    sink.send(AudioEvent::VirtualActive {
        peer,
        kind: AudioKind::Speaker,
        active: false,
    });
    assert_eq!(h.event_count(), events, "an event arrived after shutdown");
    assert!(lock(&h.shared().commands).is_empty(), "a call was queued");
    assert_eq!(h.stats().rx_unknown, 0, "a packet was looked at");
}

#[test]
fn dropping_the_worker_shuts_it_down() {
    let probe = HostProbe::new();
    let h = Harness::with_probe(probe.clone());
    h.add_peer(node(1));
    drop(h);
    assert!(
        probe.host_dropped.load(Ordering::SeqCst),
        "dropping the worker did not join its host thread"
    );
}

#[test]
fn platform_events_pass_through_in_order() {
    let h = Harness::new();
    let sink = lock(&h.probe.sink).clone().expect("subscribed");
    assert_eq!(h.probe.subscribe_calls.load(Ordering::SeqCst), 1);
    let events = [
        AudioEvent::VirtualActive {
            peer: node(1),
            kind: AudioKind::Speaker,
            active: true,
        },
        AudioEvent::DeviceError {
            peer: None,
            kind: AudioKind::Speaker,
            error: AudioDeviceError::Unavailable,
        },
    ];
    for event in &events {
        sink.send(event.clone());
    }
    assert_eq!(h.platform_events(), events.to_vec());
}

// ---- 8. Capacity ----

#[test]
fn capacity_limits_refuse_cleanly() {
    let h = Harness::new();
    // Peers: four, then refused without a host call.
    for n in 1..=4 {
        h.add_peer(node(n));
    }
    h.add_peer(node(5));
    assert_eq!(h.probe.add_calls.load(Ordering::SeqCst), 4);
    assert_eq!(
        h.platform_events(),
        vec![AudioEvent::DeviceError {
            peer: Some(node(5)),
            kind: AudioKind::Speaker,
            error: AudioDeviceError::Unavailable,
        }]
    );

    // Sessions: eight playbacks, then the ninth open and a sender are refused.
    let peer = node(1);
    for generation in 1..=8u64 {
        h.open_playback(key(peer, generation as u16 * 2, generation));
    }
    let ninth = key(peer, 18, 9);
    h.submit(Output::OpenAudioPlayback { key: ninth });
    h.settle();
    assert_eq!(
        h.opened(ninth),
        vec![(AudioKind::Speaker, Err(Failure::Other))]
    );
    assert_eq!(h.probe.open_playback_calls.load(Ordering::SeqCst), 8);
    let sender = key(node(2), 1, 10);
    h.start(sender, AudioEndpoint::VirtualSpeaker);
    assert_eq!(h.failed(sender), 1);

    // Closing one makes room again.
    h.close(key(peer, 2, 1));
    h.open_playback(key(peer, 20, 11));
    assert_eq!(h.probe.open_playback_calls.load(Ordering::SeqCst), 9);
}

#[test]
fn receive_sessions_count_against_the_four_peer_limit() {
    let h = Harness::new();
    let (a, b, c, d, e) = (node(1), node(2), node(3), node(4), node(5));
    // Two peers with devices, two with only a playback: four distinct peers.
    h.add_peer(a);
    h.add_peer(b);
    h.open_playback(key(c, 2, 1));
    h.open_playback(key(d, 2, 2));
    // A fifth peer is refused before anything is allocated for it, whichever way it asks.
    let before = h.stats();
    h.add_peer(e);
    assert_eq!(h.probe.add_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        h.platform_events(),
        vec![AudioEvent::DeviceError {
            peer: Some(e),
            kind: AudioKind::Speaker,
            error: AudioDeviceError::Unavailable,
        }]
    );
    let refused = key(e, 2, 3);
    h.submit(Output::OpenAudioPlayback { key: refused });
    h.settle();
    assert_eq!(
        h.opened(refused),
        vec![(AudioKind::Speaker, Err(Failure::Other))]
    );
    assert_eq!(h.probe.open_playback_calls.load(Ordering::SeqCst), 2);
    assert_eq!(h.stats().sessions_created, before.sessions_created);

    // Peers that are already known keep working, with more sessions.
    h.open_playback(key(c, 4, 4));
    h.open_playback(key(a, 2, 5));
    // And the reverse order: when a playback-only peer goes, the fifth fits.
    h.close(key(d, 2, 2));
    h.add_peer(e);
    assert_eq!(h.probe.add_calls.load(Ordering::SeqCst), 3);
    h.open_playback(key(e, 2, 6));
}

#[test]
fn playback_only_peers_are_refused_beyond_four() {
    let h = Harness::new();
    for n in 1..=4u8 {
        h.open_playback(key(node(n), 2, u64::from(n)));
    }
    let fifth = key(node(5), 2, 5);
    h.submit(Output::OpenAudioPlayback { key: fifth });
    h.settle();
    assert_eq!(
        h.opened(fifth),
        vec![(AudioKind::Speaker, Err(Failure::Other))]
    );
    assert_eq!(h.probe.open_playback_calls.load(Ordering::SeqCst), 4);
    h.add_peer(node(5));
    assert_eq!(h.probe.add_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn outstanding_opens_are_capped_even_when_cancelled_ones_are_still_running() {
    let h = Harness::new();
    let peer = node(1);
    let host = h.probe.hold_opens();
    h.submit(Output::OpenAudioPlayback {
        key: key(peer, 1, 1),
    });
    host.wait_entered(1);
    // Cancel it, then try to open many more while the cancelled one still occupies the host.
    h.submit(Output::CloseAudioPlayback {
        key: key(peer, 1, 1),
    });
    for generation in 2..=12u64 {
        h.submit(Output::OpenAudioPlayback {
            key: key(peer, generation as u16, generation),
        });
    }
    h.wait_applied();
    // One in flight (cancelled) plus seven queued fill the eight outstanding; the rest are
    // refused with an error and never queued.
    assert_eq!(h.shared().host.queued(), 7);
    host.release();
    h.barrier();
    let refused: Vec<u64> = (2..=12u64)
        .filter(|g| {
            h.opened(key(peer, *g as u16, *g)) == vec![(AudioKind::Speaker, Err(Failure::Other))]
        })
        .collect();
    assert_eq!(refused, vec![9, 10, 11, 12]);
    assert_eq!(h.probe.open_playback_calls.load(Ordering::SeqCst), 8);
}

// ---- Failure of a started stream ----

#[test]
fn a_playback_closed_by_the_backend_fails_its_stream_once() {
    let h = Harness::new();
    let peer = node(1);
    let k = key(peer, 1, 1);
    h.open_playback(k);
    h.start(k, AudioEndpoint::LocalPlayback);
    // The backend drops its end of the ring.
    let ends = lock(&h.probe.playbacks).remove(0);
    drop(ends.pcm);
    h.settle();
    assert_eq!(h.failed(k), 1);
    h.step();
    assert_eq!(h.failed(k), 1, "reported more than once");
    // The failed stream no longer takes packets, and the engine's stop clears it.
    h.w().packet(peer, tone_packets(1, 1).remove(0));
    assert_eq!(h.stats().rx_unknown, 1);
    h.stop(k);
    h.close(k);
    assert_eq!(h.failed(k), 1);
}

#[test]
fn a_virtual_speaker_that_goes_away_fails_its_sender() {
    let h = Harness::new();
    let peer = node(1);
    h.add_peer(peer);
    let k = key(peer, 1, 1);
    h.start(k, AudioEndpoint::VirtualSpeaker);
    let ends = lock(&h.probe.peers).remove(&peer).expect("ends");
    drop(ends);
    h.settle();
    assert_eq!(h.failed(k), 1);
    h.step();
    assert_eq!(h.failed(k), 1);
}

#[test]
fn a_full_playback_ring_loses_whole_frames_and_never_writes_half_of_one() {
    // Room for three whole frames and half of a fourth.
    let probe = HostProbe::new();
    probe
        .playback_samples
        .store(3 * 960 + 480, Ordering::SeqCst);
    let h = Harness::with_probe(probe);
    let peer = node(1);
    let k = key(peer, 1, 1);
    h.open_playback(k);
    h.start(k, AudioEndpoint::LocalPlayback);
    play_in(&h, peer, &tone_packets(1, 16));
    assert!(h.stats().playback_overflow > 0);
    assert_eq!(h.failed(k), 0);
    // The ring holds exactly its three whole frames; the half frame of room stayed unused.
    let (queued, capacity) = h.probe.ring_fill(0);
    assert_eq!((queued, capacity), (3 * 960, 3 * 960 + 480));
    assert_eq!(h.probe.played(0).len(), 3 * 960);
}

#[test]
fn the_worker_handle_can_be_shared_across_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AudioWorker>();
}

// ---- Delivery stops between any two steps ----

/// Start a sender for `peer` with `frames` frames of PCM waiting in its virtual speaker.
fn sender_with_pcm(h: &Harness, peer: NodeId, frames: usize) -> AudioKey {
    h.add_peer(peer);
    let k = key(peer, 1, 1);
    h.start(k, AudioEndpoint::VirtualSpeaker);
    h.hold_sends();
    h.probe
        .speak(peer, &(0..frames).flat_map(tone_frame).collect::<Vec<_>>());
    k
}

#[test]
fn a_send_held_across_shutdown_leaves_the_queued_frames_unsent() {
    let h = Harness::new();
    let peer = node(1);
    let k = sender_with_pcm(&h, peer, 4);
    // The data thread is inside the first send, with three more frames waiting.
    h.send_gate.wait_entered(1);
    thread::scope(|scope| {
        let stopper = scope.spawn(|| h.w().stop());
        wait_until("the shutdown to begin", Duration::from_secs(5), || {
            h.shared().is_shutdown()
        });
        h.release_sends();
        stopper.join().expect("shutdown does not panic");
    });
    // Only the send that was already inside the link completed.
    assert_eq!(h.sent_count(), 1);
    assert_eq!(h.failed(k), 0);
}

#[test]
fn a_send_held_across_a_stop_leaves_the_queued_frames_unsent() {
    let h = Harness::new();
    let peer = node(1);
    let k = sender_with_pcm(&h, peer, 4);
    h.send_gate.wait_entered(1);
    h.submit(Output::StopAudioStream { key: k });
    h.release_sends();
    h.settle();
    assert_eq!(h.sent_count(), 1, "frames were sent after the stop");
}

#[test]
fn a_send_held_across_cancel_peer_leaves_the_queued_frames_unsent() {
    let h = Harness::new();
    let peer = node(1);
    let k = sender_with_pcm(&h, peer, 4);
    h.send_gate.wait_entered(1);
    h.w().cancel_peer(peer);
    h.release_sends();
    h.settle();
    assert_eq!(h.sent_count(), 1, "frames were sent after cancel_peer");
    assert_eq!(h.failed(k), 0);
}

#[test]
fn a_cancel_between_encode_and_send_sends_nothing() {
    let h = Harness::new();
    let peer = node(1);
    h.add_peer(peer);
    let k = key(peer, 1, 1);
    h.start(k, AudioEndpoint::VirtualSpeaker);
    let encoded = Arc::new(AtomicUsize::new(0));
    h.on_stage({
        let (shared, encoded) = (h.shared().clone(), encoded.clone());
        move |stage, _, _| {
            if stage == Stage::Encoded && encoded.fetch_add(1, Ordering::SeqCst) == 0 {
                shared.admit(Command::CancelPeer { peer });
            }
        }
    });
    h.probe
        .speak(peer, &(0..3).flat_map(tone_frame).collect::<Vec<_>>());
    wait_until(
        "the first frame to be encoded",
        Duration::from_secs(5),
        || encoded.load(Ordering::SeqCst) >= 1,
    );
    h.settle();
    assert_eq!(
        encoded.load(Ordering::SeqCst),
        1,
        "a second frame was encoded"
    );
    assert_eq!(
        h.sent_count(),
        0,
        "an encoded frame was sent after the cancel"
    );
}

#[test]
fn a_shutdown_between_encode_and_send_sends_nothing() {
    let h = Harness::new();
    let peer = node(1);
    h.add_peer(peer);
    let k = key(peer, 1, 1);
    h.start(k, AudioEndpoint::VirtualSpeaker);
    let encoded = Arc::new(AtomicUsize::new(0));
    h.on_stage({
        let (shared, encoded) = (h.shared().clone(), encoded.clone());
        move |stage, _, _| {
            if stage == Stage::Encoded && encoded.fetch_add(1, Ordering::SeqCst) == 0 {
                // What a caller's `shutdown` does first.
                shared.closing.store(true, Ordering::SeqCst);
                shared.begin_shutdown();
            }
        }
    });
    h.probe
        .speak(peer, &(0..3).flat_map(tone_frame).collect::<Vec<_>>());
    wait_until(
        "the first frame to be encoded",
        Duration::from_secs(5),
        || encoded.load(Ordering::SeqCst) >= 1,
    );
    // Both threads are finished when `stop` returns.
    h.w().stop();
    assert_eq!(
        h.sent_count(),
        0,
        "an encoded frame was sent after the shutdown"
    );
    assert_eq!(encoded.load(Ordering::SeqCst), 1);
}

#[test]
fn a_cancel_during_a_multi_frame_playout_pass_writes_nothing_more() {
    let h = Harness::new();
    let peer = node(1);
    let k = key(peer, 2, 1);
    h.open_playback(k);
    h.start(k, AudioEndpoint::LocalPlayback);
    let packets = tone_packets(2, 16);
    // Get the stream playing, then queue more packets without moving the clock.
    play_in(&h, peer, &packets[..8]);
    for packet in &packets[8..] {
        h.w().packet(peer, packet.clone());
    }
    h.settle();
    h.probe.played(0);
    let played_before = h.stats().played;

    // Four frames come due in one pass. The cancel arrives as the second is pulled.
    let pulled = Arc::new(AtomicUsize::new(0));
    h.on_stage({
        let (shared, pulled) = (h.shared().clone(), pulled.clone());
        move |stage, _, _| {
            if stage == Stage::Pulled && pulled.fetch_add(1, Ordering::SeqCst) == 1 {
                shared.admit(Command::CancelPeer { peer });
            }
        }
    });
    h.clock.advance(Duration::from_millis(40));
    wait_until("the second pull", Duration::from_secs(5), || {
        pulled.load(Ordering::SeqCst) >= 2
    });
    h.settle();
    assert_eq!(pulled.load(Ordering::SeqCst), 2, "a third frame was pulled");
    assert_eq!(
        h.stats().played - played_before,
        1,
        "a frame was written after the cancel"
    );
    assert_eq!(h.probe.played(0).len(), 960);
}

#[test]
fn a_cancel_after_a_frame_is_written_pulls_no_further_frame() {
    let h = Harness::new();
    let peer = node(1);
    let k = key(peer, 2, 1);
    h.open_playback(k);
    h.start(k, AudioEndpoint::LocalPlayback);
    let packets = tone_packets(2, 16);
    play_in(&h, peer, &packets[..8]);
    for packet in &packets[8..] {
        h.w().packet(peer, packet.clone());
    }
    h.settle();
    h.probe.played(0);
    let played_before = h.stats().played;

    // Four frames come due in one pass. The cancel arrives right after the first is written, so
    // the next pull must not happen at all.
    let pulled = Arc::new(AtomicUsize::new(0));
    h.on_stage({
        let (shared, pulled) = (h.shared().clone(), pulled.clone());
        move |stage, _, _| match stage {
            Stage::Pulled => {
                pulled.fetch_add(1, Ordering::SeqCst);
            }
            Stage::Written => shared.admit(Command::CancelPeer { peer }),
            Stage::Encoded | Stage::Admitting => {}
        }
    });
    h.clock.advance(Duration::from_millis(40));
    wait_until("the first pull", Duration::from_secs(5), || {
        pulled.load(Ordering::SeqCst) >= 1
    });
    h.settle();
    assert_eq!(
        pulled.load(Ordering::SeqCst),
        1,
        "a frame was pulled after the cancel"
    );
    assert_eq!(h.stats().played - played_before, 1);
}

// ---- Immediate invalidation ----

#[test]
fn applied_calls_leave_the_shadow_matching_what_the_engine_believes() {
    let h = Harness::new();
    let peer = node(1);
    let (a, b) = (key(peer, 1, 1), key(peer, 3, 2));
    h.open_playback(a);
    h.start(a, AudioEndpoint::LocalPlayback);
    h.submit(Output::OpenAudioPlayback { key: b });
    h.settle();
    // Both opens were reported (and so are no longer owed); only `a` is started.
    assert!(lock(&h.shared().shadow).opens.is_empty());
    assert_eq!(lock(&h.shared().shadow).live, vec![a]);

    h.stop(a);
    assert!(lock(&h.shared().shadow).live.is_empty());
    h.start(a, AudioEndpoint::LocalPlayback);
    h.close(a);
    h.w().cancel_peer(peer);
    h.submit(Output::RemoveAudioPeer { peer });
    h.settle();
    let shadow = lock(&h.shared().shadow);
    assert!(shadow.live.is_empty() && shadow.opens.is_empty());
    drop(shadow);
    // A later key of the same peer is not suppressed by any of that.
    let later = key(peer, 5, 3);
    h.open_playback(later);
    h.start(later, AudioEndpoint::LocalPlayback);
    play_in(&h, peer, &tone_packets(5, 8));
    assert!(!h.probe.played(2).is_empty());
}

#[test]
fn nothing_is_reported_for_a_cancelled_open_even_while_the_data_thread_is_busy() {
    let h = Harness::new();
    let peer = node(1);
    let (first, second) = (key(peer, 2, 1), key(peer, 4, 2));
    // The first report parks the data thread inside the events callback.
    h.event_gate.close();
    h.submit(Output::OpenAudioCapture { key: first });
    h.event_gate.wait_entered(1);
    h.submit(Output::OpenAudioCapture { key: second });
    h.w().cancel_peer(peer);
    h.event_gate.release();
    h.barrier();
    assert_eq!(h.opened(first).len(), 1);
    assert!(h.opened(second).is_empty(), "a cancelled open was reported");
}

#[test]
fn a_stop_does_not_cancel_an_open_that_is_still_running() {
    let h = Harness::new();
    let peer = node(1);
    let k = key(peer, 1, 1);
    let host = h.probe.hold_opens();
    h.submit(Output::OpenAudioPlayback { key: k });
    host.wait_entered(1);
    // Park the data thread inside a report, so the open's reply is waiting when the stop arrives
    // and the stop has not been applied yet when the reply is.
    h.event_gate.close();
    h.submit(Output::OpenAudioCapture {
        key: key(peer, 2, 2),
    });
    h.event_gate.wait_entered(1);
    host.release();
    wait_until("the open's reply", Duration::from_secs(5), || {
        h.shared().replies_published.load(Ordering::SeqCst) == 1
    });
    h.submit(Output::StopAudioStream { key: k });
    h.event_gate.release();
    h.barrier();
    assert_eq!(h.opened(k), vec![(AudioKind::Speaker, Ok(()))]);
    assert_eq!(h.probe.stopped(0), 0);
}

// ---- Alignment across stops, and admission with outstanding opens ----

#[test]
fn stereo_alignment_survives_stops_and_restarts_with_an_odd_tail() {
    let h = Harness::new();
    let peer = node(1);
    h.add_peer(peer);
    let seen: Arc<Mutex<Vec<Vec<f32>>>> = Arc::new(Mutex::new(Vec::new()));
    h.on_stage({
        let seen = seen.clone();
        move |stage, _, pcm| {
            if stage == Stage::Encoded {
                lock(&seen).push(pcm.to_vec());
            }
        }
    });
    let first = key(peer, 1, 1);
    h.start(first, AudioEndpoint::VirtualSpeaker);
    h.stop(first);

    // Idle, an app is caught mid-write: a lone left sample waits in the ring.
    h.probe.speak(peer, &[0.9, 0.9, 0.9]);
    wait_until("the idle drain", Duration::from_secs(5), || {
        h.probe.speaker_room(peer) == 96_000 - 1
    });
    // A start throws that left sample away and now owes its right half. The stream is cancelled
    // before the right half arrives, so the debt must outlive the stream.
    let second = key(peer, 3, 2);
    h.start(second, AudioEndpoint::VirtualSpeaker);
    h.stop(second);

    // The right half arrives, with four more stale samples (two whole pairs) behind it. The idle
    // drain pays the debt first, so everything goes and nothing is left over to shift a channel.
    h.probe.speak(peer, &[0.9, 0.8, 0.7, 0.6, 0.5]);
    wait_until("the stale pairs to drain", Duration::from_secs(5), || {
        h.probe.speaker_room(peer) == 96_000
    });

    // A restart now hears distinct channels (tone on the left, silence on the right) in place.
    h.start(key(peer, 5, 3), AudioEndpoint::VirtualSpeaker);
    let mut audio = left_only_frame(0);
    audio.extend(left_only_frame(1));
    h.probe.speak(peer, &audio);
    wait_until("two packets", Duration::from_secs(5), || {
        h.sent_count() == 2
    });
    h.settle();
    let seen = lock(&seen);
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0], left_only_frame(0), "the channels are shifted");
    assert_eq!(seen[1], left_only_frame(1));
}

#[test]
fn a_cancelled_but_outstanding_open_still_counts_against_the_four_peer_limit() {
    let h = Harness::new();
    let (a, b, c, d, e) = (node(1), node(2), node(3), node(4), node(5));
    let host = h.probe.hold_opens();
    // An open for A is held inside the host, and the engine cancels it: it still occupies the host.
    h.submit(Output::OpenAudioPlayback { key: key(a, 2, 1) });
    host.wait_entered(1);
    h.submit(Output::CloseAudioPlayback { key: key(a, 2, 1) });
    // B, C and D are admitted; with A that is four peers, so E is refused without a host call.
    for (peer, generation) in [(b, 2), (c, 3), (d, 4)] {
        h.submit(Output::OpenAudioPlayback {
            key: key(peer, 2, generation),
        });
    }
    let fifth = key(e, 2, 5);
    h.submit(Output::OpenAudioPlayback { key: fifth });
    h.wait_applied();
    assert_eq!(
        h.opened(fifth),
        vec![(AudioKind::Speaker, Err(Failure::Other))],
        "a fifth distinct peer was admitted"
    );
    assert_eq!(h.shared().host.queued(), 3);
    // The host finishes A's open (stale: dropped, not reported), then the others.
    host.release();
    h.barrier();
    assert_eq!(h.probe.open_playback_calls.load(Ordering::SeqCst), 4);
    assert!(h.opened(key(a, 2, 1)).is_empty());
    for (peer, generation) in [(b, 2), (c, 3), (d, 4)] {
        assert_eq!(
            h.opened(key(peer, 2, generation)),
            vec![(AudioKind::Speaker, Ok(()))]
        );
    }
    // With A's open done it no longer counts, so E fits now.
    h.open_playback(key(e, 4, 6));
}

// ---- A request racing a death, and an open racing a death ----

#[test]
fn a_request_after_a_death_is_rejected_and_reported_failed_at_once() {
    let recorder = Recorder::default();
    let worker = bare_worker(recorder.callback());
    let peer = node(1);
    let live = key(peer, 2, 1);
    worker.submit(Output::StartAudioStream {
        key: live,
        kind: AudioKind::Speaker,
        endpoint: AudioEndpoint::VirtualSpeaker,
    });
    worker.shared.die("the test");
    assert_eq!(recorder.failed(live), 1);

    // Everything the engine could still be waiting on is reported failed as it is submitted.
    let (start, open, capture) = (key(peer, 4, 2), key(peer, 6, 3), key(peer, 8, 4));
    worker.submit(Output::StartAudioStream {
        key: start,
        kind: AudioKind::Speaker,
        endpoint: AudioEndpoint::LocalPlayback,
    });
    worker.submit(Output::OpenAudioPlayback { key: open });
    worker.submit(Output::OpenAudioCapture { key: capture });
    assert_eq!(recorder.failed(start), 1);
    assert_eq!(recorder.open_failed(open, AudioKind::Speaker), 1);
    assert_eq!(recorder.open_failed(capture, AudioKind::Microphone), 1);
    // Requests nobody waits on report nothing, and are not queued.
    worker.submit(Output::StopAudioStream { key: start });
    worker.submit(Output::CloseAudioPlayback { key: open });
    worker.submit(Output::AddAudioPeer {
        peer,
        name: "late".into(),
    });
    worker.submit(Output::RemoveAudioPeer { peer });
    worker.cancel_peer(peer);
    assert_eq!(recorder.count(), 4);
    assert!(lock(&worker.shared.commands).is_empty());
    // Nothing is left in the shadow to be orphaned, and a second death reports nothing more.
    {
        let shadow = lock(&worker.shared.shadow);
        assert!(shadow.live.is_empty() && shadow.opens.is_empty());
    }
    worker.shared.die("again");
    assert_eq!(recorder.count(), 4);
    assert!(
        recorder
            .threads()
            .iter()
            .all(|t| *t == thread::current().id())
    );
}

#[test]
fn a_death_during_admission_cannot_slip_between_the_check_and_the_update() {
    let recorder = Recorder::default();
    let worker = Arc::new(bare_worker(recorder.callback()));
    let k = key(node(1), 2, 1);
    let killer: Arc<Mutex<Option<thread::JoinHandle<()>>>> = Arc::new(Mutex::new(None));
    *lock(&worker.shared.hook) = Some(Arc::new({
        let (shared, killer) = (worker.shared.clone(), killer.clone());
        move |stage, _, _| {
            if stage == Stage::Admitting {
                // Another thread kills the worker now, with the request half admitted, and has
                // time to get as far as it can before the admission finishes.
                let shared = shared.clone();
                *lock(&killer) = Some(thread::spawn(move || shared.die("mid-admission")));
                thread::sleep(Duration::from_millis(100));
            }
        }
    }));
    worker.submit(Output::StartAudioStream {
        key: k,
        kind: AudioKind::Speaker,
        endpoint: AudioEndpoint::VirtualSpeaker,
    });
    let killer = lock(&killer).take().expect("the request was admitted");
    killer.join().expect("killer");
    // The request was either taken by the death or rejected by it: reported once, not orphaned.
    assert_eq!(recorder.failed(k), 1);
    assert_eq!(recorder.count(), 1);
    let shadow = lock(&worker.shared.shadow);
    assert!(shadow.live.is_empty() && shadow.opens.is_empty());
}

#[test]
fn a_submission_racing_a_death_is_reported_exactly_once_whichever_wins() {
    let peer = node(1);
    for round in 0..300 {
        let recorder = Recorder::default();
        let worker = Arc::new(bare_worker(recorder.callback()));
        let (start, open) = (key(peer, 2, 1), key(peer, 4, 2));
        // Both threads are released together, so the submission lands before, during or after.
        let barrier = Arc::new(Barrier::new(3));
        let submitter = thread::spawn({
            let (worker, barrier) = (worker.clone(), barrier.clone());
            move || {
                barrier.wait();
                worker.submit(Output::StartAudioStream {
                    key: start,
                    kind: AudioKind::Speaker,
                    endpoint: AudioEndpoint::VirtualSpeaker,
                });
                worker.submit(Output::OpenAudioPlayback { key: open });
            }
        });
        let killer = thread::spawn({
            let (worker, barrier) = (worker.clone(), barrier.clone());
            move || {
                barrier.wait();
                worker.shared.die("the race");
            }
        });
        barrier.wait();
        submitter.join().expect("submitter");
        killer.join().expect("killer");
        assert_eq!(recorder.failed(start), 1, "round {round}");
        assert_eq!(
            recorder.open_failed(open, AudioKind::Speaker),
            1,
            "round {round}"
        );
        assert_eq!(recorder.count(), 2, "round {round}");
        let shadow = lock(&worker.shared.shadow);
        assert!(shadow.live.is_empty() && shadow.opens.is_empty());
    }
}

#[test]
fn every_start_racing_a_host_panic_is_reported_exactly_once() {
    let h = Harness::new();
    let keys: Vec<AudioKey> = (1..=300u64)
        .map(|g| key(node(1), (g * 2) as u16, g))
        .collect();
    h.probe.panic_on_add.store(true, Ordering::SeqCst);
    thread::scope(|scope| {
        let submitter = scope.spawn(|| {
            for k in &keys {
                h.submit(Output::StartAudioStream {
                    key: *k,
                    kind: AudioKind::Speaker,
                    endpoint: AudioEndpoint::VirtualSpeaker,
                });
            }
        });
        // The host thread dies on this while the starts are still going in.
        h.submit(Output::AddAudioPeer {
            peer: node(2),
            name: "boom".into(),
        });
        submitter.join().expect("submitter");
    });
    // Each key is refused by the data thread, taken by the death, or rejected: once, whichever.
    wait_until("every key to be reported", Duration::from_secs(10), || {
        keys.iter().all(|k| h.failed(*k) >= 1)
    });
    assert!(h.shared().is_shutdown());
    for k in &keys {
        assert_eq!(h.failed(*k), 1, "{k:?}");
    }
    assert_eq!(h.event_count(), keys.len());
}

#[test]
fn an_open_reported_before_a_death_is_not_reported_again() {
    let h = Harness::new();
    let k = key(node(1), 2, 1);
    h.open_playback(k);
    h.shared().die("after the open");
    assert_eq!(h.opened(k), vec![(AudioKind::Speaker, Ok(()))]);
    assert_eq!(h.event_count(), 1);
}

#[test]
fn an_open_completing_after_a_death_has_begun_is_not_reported_as_success() {
    let h = Harness::new();
    let peer = node(1);
    let (failing, completing) = (key(peer, 2, 1), key(peer, 4, 2));
    // Park the data thread inside a report, with two host replies waiting for it.
    let host = h.probe.hold_opens();
    h.probe.fail_open.store(true, Ordering::SeqCst);
    h.submit(Output::OpenAudioPlayback { key: failing });
    host.wait_entered(1);
    h.probe.fail_open.store(false, Ordering::SeqCst);
    h.submit(Output::OpenAudioPlayback { key: completing });
    h.wait_applied();
    h.event_gate.close();
    h.submit(Output::OpenAudioCapture {
        key: key(peer, 6, 3),
    });
    h.event_gate.wait_entered(1);
    let published = h.shared().replies_published.load(Ordering::SeqCst);
    host.release();
    wait_until("both replies", Duration::from_secs(5), || {
        h.shared().replies_published.load(Ordering::SeqCst) >= published + 2
    });
    // The data thread takes both replies into its batch, and parks again while reporting the
    // first one (the failed open). The second, a success, is still waiting in its batch.
    h.event_gate.allow(1);
    h.event_gate.wait_entered(2);

    // The worker dies now, from another thread. The completing open is still owed a report, and
    // the death makes it: an error.
    h.shared().die("while an open was completing");
    assert_eq!(
        h.opened(completing),
        vec![(AudioKind::Speaker, Err(Failure::Other))]
    );
    // The data thread goes on with its batch: the completion finds the open taken by the death,
    // so it reports nothing, and its handle is dropped.
    h.event_gate.release();
    wait_until("the handle to be stopped", Duration::from_secs(5), || {
        h.probe.playbacks_opened() == 1 && h.probe.stopped(0) == 1
    });
    assert!(h.shared().is_shutdown());
    assert_eq!(
        h.opened(completing),
        vec![(AudioKind::Speaker, Err(Failure::Other))],
        "an open reported success after the death had begun"
    );
    assert_eq!(
        h.opened(failing).len(),
        1,
        "the report that was already claimed is made once"
    );
}

// ---- A death is reported before anything slow ----

#[test]
fn a_host_panic_is_reported_before_the_hosts_destructor_finishes() {
    let h = Harness::new();
    let peer = node(1);
    h.add_peer(peer);
    let sender = key(peer, 1, 1);
    h.start(sender, AudioEndpoint::VirtualSpeaker);
    let queued = key(peer, 2, 2);
    // The host's destructor blocks, once it is reached.
    let destructor = Gate::closed();
    *lock(&h.probe.drop_gate) = Some(destructor.clone());
    h.probe.panic_on_add.store(true, Ordering::SeqCst);
    h.submit(Output::AddAudioPeer {
        peer: node(2),
        name: "boom".into(),
    });
    h.submit(Output::OpenAudioPlayback { key: queued });

    // The reports arrive while the destructor is still held (or not even reached).
    wait_until("the reports", Duration::from_secs(5), || {
        h.event_count() == 2
    });
    assert!(
        !h.probe.host_dropped.load(Ordering::SeqCst),
        "the host's destructor finished before the death was reported"
    );
    assert_eq!(h.failed(sender), 1);
    assert_eq!(
        h.opened(queued),
        vec![(AudioKind::Speaker, Err(Failure::Other))]
    );
    // And delivery has stopped meanwhile: a frame played now is not sent.
    h.assert_nothing_sent_after_death(peer);
    destructor.release();
    wait_until("the destructor", Duration::from_secs(5), || {
        h.probe.host_dropped.load(Ordering::SeqCst)
    });
    assert_eq!(h.event_count(), 2);
}

#[test]
fn a_data_thread_panic_is_reported_before_a_playback_handles_stop_finishes() {
    let h = Harness::new();
    let peer = node(1);
    h.add_peer(peer);
    let (sender, receiver) = (key(peer, 1, 1), key(peer, 2, 2));
    // The playback's stop blocks, once it is reached.
    let stop = Gate::closed();
    *lock(&h.probe.stop_gate) = Some(stop.clone());
    h.open_playback(receiver);
    h.start(receiver, AudioEndpoint::LocalPlayback);
    h.start(sender, AudioEndpoint::VirtualSpeaker);
    let events_before = h.event_count();
    // The data thread panics inside a report.
    h.panic_on_event.store(true, Ordering::SeqCst);
    h.submit(Output::OpenAudioCapture {
        key: key(peer, 4, 3),
    });

    wait_until("the reports", Duration::from_secs(5), || {
        h.failed(sender) == 1 && h.failed(receiver) == 1
    });
    assert_eq!(
        h.probe.stopped(0),
        0,
        "the playback's stop finished before the death was reported"
    );
    // The data thread is unwinding into the blocked stop: nothing is delivered any more.
    h.assert_nothing_sent_after_death(peer);
    stop.release();
    wait_until("the stop", Duration::from_secs(5), || {
        h.probe.stopped(0) == 1
    });
    assert_eq!(h.event_count(), events_before + 2);
}

#[test]
fn a_panic_while_applying_one_reply_reports_the_death_before_the_next_replys_handle_stops() {
    let h = Harness::new();
    let peer = node(1);
    h.add_peer(peer);
    let sender = key(peer, 1, 1);
    h.start(sender, AudioEndpoint::VirtualSpeaker);
    let (first, second) = (key(peer, 2, 2), key(peer, 4, 3));

    // Two opens complete on the host. The second one's handle has a stop that blocks.
    let host = h.probe.hold_opens();
    h.submit(Output::OpenAudioPlayback { key: first });
    host.wait_entered(1);
    let stop = Gate::closed();
    *lock(&h.probe.stop_gate) = Some(stop.clone());
    h.submit(Output::OpenAudioPlayback { key: second });
    h.wait_applied();

    // The data thread is parked, so that both replies are waiting for it together, and the report
    // of the first open will panic.
    h.event_gate.close();
    h.submit(Output::OpenAudioCapture {
        key: key(peer, 6, 4),
    });
    h.event_gate.wait_entered(1);
    *lock(&h.panic_on_open_of) = Some(first);
    host.release();
    wait_until("both replies", Duration::from_secs(5), || {
        h.shared().replies_published.load(Ordering::SeqCst) == 3
    });
    h.event_gate.release();

    // The thread unwinds out of the first reply. The death must be reported before the second
    // reply's handle (which is still queued) is dropped, because that stop is held.
    wait_until("the death to be reported", Duration::from_secs(5), || {
        h.failed(sender) == 1 && h.opened(second) == vec![(AudioKind::Speaker, Err(Failure::Other))]
    });
    assert_eq!(
        h.probe.playbacks_opened(),
        2,
        "the second open had not completed"
    );
    assert_eq!(h.probe.stopped(1), 0, "the held stop was not held");
    // Only now is the held stop let go, and it then completes.
    stop.wait_entered(1);
    stop.release();
    wait_until("the stop", Duration::from_secs(5), || {
        h.probe.stopped(1) == 1
    });
    // Each report was made once: the panicking one was claimed, so it is not repeated.
    assert_eq!(h.failed(sender), 1);
    assert_eq!(h.opened(second).len(), 1);
    assert!(h.opened(first).is_empty());
}

#[test]
fn shutdown_bounds_the_whole_wait_including_dropping_queued_handles() {
    let mut h = Harness::new();
    let peer = node(1);
    // Two opens complete on the host and their replies wait, unapplied, for a data thread that is
    // parked; the backend takes 400 ms to stop each handle.
    h.probe.stop_delay.store(400, Ordering::SeqCst);
    let host = h.probe.hold_opens();
    for generation in 1..=2 {
        h.submit(Output::OpenAudioPlayback {
            key: key(peer, generation as u16 * 2, generation),
        });
    }
    host.wait_entered(1);
    h.wait_applied();
    h.event_gate.close();
    h.submit(Output::OpenAudioCapture {
        key: key(peer, 8, 3),
    });
    h.event_gate.wait_entered(1);
    host.release();
    wait_until("both replies", Duration::from_secs(5), || {
        h.shared().replies_published.load(Ordering::SeqCst) == 2
    });

    // Shutdown drops the two handles (0.8 s) and then waits for the parked data thread, which
    // never finishes. The 2.5 s budget runs from entry, so the whole thing takes 2.5 s, not 3.3.
    let begun = Instant::now();
    h.shutdown();
    let took = begun.elapsed();
    assert_eq!(h.probe.stopped(0) + h.probe.stopped(1), 2);
    assert!(
        took >= SHUTDOWN_BUDGET - Duration::from_millis(100)
            && took < SHUTDOWN_BUDGET + Duration::from_millis(250),
        "shutdown took {took:?}"
    );
    h.event_gate.release();
}

// ---- Death ----

/// Records what the events callback receives, and from which thread.
#[derive(Clone, Default)]
struct Recorder {
    events: Arc<Mutex<Vec<(WorkerEvent, ThreadId)>>>,
}

impl Recorder {
    fn callback(&self) -> Arc<dyn Fn(WorkerEvent) + Send + Sync> {
        let events = Arc::clone(&self.events);
        Arc::new(move |event| lock(&events).push((event, thread::current().id())))
    }

    fn failed(&self, key: AudioKey) -> usize {
        lock(&self.events)
            .iter()
            .filter(|(e, _)| matches!(e, WorkerEvent::StreamFailed { key: k } if *k == key))
            .count()
    }

    fn open_failed(&self, key: AudioKey, kind: AudioKind) -> usize {
        lock(&self.events)
            .iter()
            .filter(|(e, _)| {
                matches!(e, WorkerEvent::DeviceOpened { key: k, kind: kd, result: Err(Failure::Other) }
                    if *k == key && *kd == kind)
            })
            .count()
    }

    fn count(&self) -> usize {
        lock(&self.events).len()
    }

    fn threads(&self) -> Vec<ThreadId> {
        lock(&self.events).iter().map(|(_, t)| *t).collect()
    }
}

#[test]
fn command_saturation_kills_the_worker_and_reports_every_live_key_and_pending_open_once() {
    // No data thread: nobody drains the queue, so it fills.
    let recorder = Recorder::default();
    let worker = bare_worker(recorder.callback());
    let peer = node(1);
    let (open_playback, open_capture) = (key(peer, 2, 1), key(peer, 4, 2));
    let (live, stopped, cancelled) = (key(peer, 6, 3), key(peer, 8, 4), key(node(2), 2, 5));
    worker.submit(Output::OpenAudioPlayback { key: open_playback });
    worker.submit(Output::OpenAudioCapture { key: open_capture });
    for k in [live, stopped, cancelled] {
        worker.submit(Output::StartAudioStream {
            key: k,
            kind: AudioKind::Speaker,
            endpoint: AudioEndpoint::VirtualSpeaker,
        });
    }
    worker.submit(Output::StopAudioStream { key: stopped });
    worker.cancel_peer(node(2));
    // Up to the high-water mark nothing is dropped, only counted; below the hard cap nothing is
    // reported either.
    let filler = Output::StopAudioStream {
        key: key(node(9), 2, 99),
    };
    while lock(&worker.shared.commands).len() < COMMAND_SOFT_CAP + 1 {
        worker.submit(filler.clone());
    }
    assert_eq!(worker.stats().commands_over_cap, 1);
    assert!(!worker.shared.is_shutdown());
    assert_eq!(recorder.count(), 0);

    // At the hard cap the worker dies, in the caller's context.
    for _ in 0..COMMAND_HARD_CAP {
        worker.submit(filler.clone());
    }
    assert!(worker.shared.is_shutdown(), "the queue passed its hard cap");
    assert!(
        lock(&worker.shared.commands).is_empty(),
        "queued commands were kept"
    );
    assert_eq!(
        recorder.count(),
        3,
        "one report per live key and pending open"
    );
    assert_eq!(recorder.failed(live), 1);
    assert_eq!(recorder.failed(stopped), 0, "a stopped key was reported");
    assert_eq!(
        recorder.failed(cancelled),
        0,
        "a cancelled key was reported"
    );
    assert_eq!(recorder.open_failed(open_playback, AudioKind::Speaker), 1);
    assert_eq!(recorder.open_failed(open_capture, AudioKind::Microphone), 1);
    assert!(
        recorder
            .threads()
            .iter()
            .all(|t| *t == thread::current().id()),
        "the death was reported from a thread other than the one that detected it"
    );

    // Exactly once: a second death, or more calls, report nothing.
    worker.shared.die("again");
    worker.submit(filler.clone());
    worker.cancel_peer(peer);
    assert_eq!(recorder.count(), 3);
}

#[test]
fn a_data_thread_panic_stops_a_late_open_and_reports_every_live_key_and_pending_open_once() {
    let h = Harness::new();
    let peer = node(1);
    h.add_peer(peer);
    let sender = key(peer, 1, 1);
    h.start(sender, AudioEndpoint::VirtualSpeaker);
    // An open that has completed on the host but whose reply the data thread has not applied.
    let (pending, parked) = (key(peer, 2, 2), key(peer, 4, 3));
    let host = h.probe.hold_opens();
    h.submit(Output::OpenAudioPlayback { key: pending });
    host.wait_entered(1);
    // The data thread parks inside a report, and that report will panic.
    h.event_gate.close();
    h.panic_on_event.store(true, Ordering::SeqCst);
    h.submit(Output::OpenAudioCapture { key: parked });
    h.event_gate.wait_entered(1);
    let published = h.shared().replies_published.load(Ordering::SeqCst);
    host.release();
    wait_until("the open's reply", Duration::from_secs(5), || {
        h.shared().replies_published.load(Ordering::SeqCst) > published
    });
    assert_eq!(h.probe.playbacks_opened(), 1);
    assert_eq!(h.probe.stopped(0), 0);
    h.event_gate.release();

    // The thread unwinds: its guard dies the worker, which empties the reply queue and stops the
    // handle in it, and reports what the engine is still waiting on.
    wait_until("the handle to be stopped", Duration::from_secs(5), || {
        h.probe.stopped(0) == 1
    });
    wait_until(
        "the worker to report its death",
        Duration::from_secs(5),
        || h.failed(sender) == 1,
    );
    assert_eq!(
        h.opened(pending),
        vec![(AudioKind::Speaker, Err(Failure::Other))]
    );
    assert!(h.shared().is_shutdown());
    // The report that panicked was the parked capture's, which had been claimed: not repeated.
    assert!(h.opened(parked).is_empty());
    assert_eq!(h.failed(sender), 1);
    assert_eq!(h.event_count(), 2);
}

#[test]
fn a_host_thread_panic_reports_every_live_key_and_pending_open_once() {
    let h = Harness::new();
    let peer = node(1);
    h.add_peer(peer);
    let sender = key(peer, 1, 1);
    h.start(sender, AudioEndpoint::VirtualSpeaker);
    let queued = key(peer, 2, 2);
    // The host thread dies on the next add; the open behind it is never run.
    h.probe.panic_on_add.store(true, Ordering::SeqCst);
    h.submit(Output::AddAudioPeer {
        peer: node(2),
        name: "boom".into(),
    });
    h.submit(Output::OpenAudioPlayback { key: queued });
    wait_until("the worker to shut down", Duration::from_secs(5), || {
        h.shared().is_shutdown()
    });
    wait_until("the reports", Duration::from_secs(5), || {
        h.event_count() == 2
    });
    assert_eq!(h.failed(sender), 1);
    assert_eq!(
        h.opened(queued),
        vec![(AudioKind::Speaker, Err(Failure::Other))]
    );
    // Calls after that are ignored.
    h.submit(Output::AddAudioPeer {
        peer: node(3),
        name: "ignored".into(),
    });
    assert!(lock(&h.shared().commands).is_empty());
    assert_eq!(h.event_count(), 2);
}

#[test]
fn a_panic_in_the_platform_callback_on_a_backend_thread_reports_before_it_propagates() {
    let h = Harness::new();
    let peer = node(1);
    h.add_peer(peer);
    let sender = key(peer, 1, 1);
    h.start(sender, AudioEndpoint::VirtualSpeaker);
    let held = key(peer, 2, 2);
    let host = h.probe.hold_opens();
    h.submit(Output::OpenAudioPlayback { key: held });
    host.wait_entered(1);
    h.probe.speak(peer, &tone_frame(0));
    wait_until("the sender to work", Duration::from_secs(5), || {
        h.sent_count() == 1
    });

    // A backend thread delivers a platform event, and the agent's callback panics on it.
    h.panic_on_platform.store(true, Ordering::SeqCst);
    let sink = lock(&h.probe.sink).clone().expect("subscribed");
    let backend = thread::spawn(move || {
        sink.send(AudioEvent::VirtualActive {
            peer,
            kind: AudioKind::Speaker,
            active: true,
        });
    });
    let backend_id = backend.thread().id();
    assert!(backend.join().is_err(), "the panic was swallowed");

    // The worker is over, the engine was told about everything, and from the backend thread.
    assert!(h.shared().is_shutdown());
    assert_eq!(h.failed(sender), 1);
    assert_eq!(
        h.opened(held),
        vec![(AudioKind::Speaker, Err(Failure::Other))]
    );
    assert_eq!(h.event_count(), 2);
    assert!(
        lock(&h.event_threads).iter().all(|t| *t == backend_id),
        "the reports did not come from the context that detected the death"
    );
    // The held open finishes later; its handle is dropped by the host thread, not kept.
    host.release();
    wait_until("the late handle", Duration::from_secs(5), || {
        h.probe.playbacks_opened() == 1 && h.probe.stopped(0) == 1
    });
    assert_eq!(h.event_count(), 2);
}

#[test]
fn a_callers_shutdown_is_not_a_death_and_reports_no_failure() {
    let mut h = Harness::new();
    let peer = node(1);
    h.add_peer(peer);
    let (sender, pending) = (key(peer, 1, 1), key(peer, 2, 2));
    h.start(sender, AudioEndpoint::VirtualSpeaker);
    let host = h.probe.hold_opens();
    h.submit(Output::OpenAudioPlayback { key: pending });
    host.wait_entered(1);
    host.release();
    let shared = h.shared().clone();
    h.shutdown();
    // The open may well have completed (and been reported as a success) before the shutdown
    // got there, but nothing is reported as failed: that is what a death does, not a shutdown.
    assert_eq!(h.failed(sender), 0);
    assert!(h.opened(pending).iter().all(|(_, result)| result.is_ok()));
    // And a later call is ignored without a report.
    let events = h.event_count();
    shared.admit(Command::Start {
        key: key(peer, 4, 3),
        kind: AudioKind::Speaker,
        endpoint: AudioEndpoint::VirtualSpeaker,
    });
    assert_eq!(h.event_count(), events);
}
