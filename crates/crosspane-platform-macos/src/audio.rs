//! CoreAudio host for speaker v0 (WP-3.5): [`CoreAudioHost`] implements the frozen
//! [`AudioHost`] over the fixed four-device loopback inventory of the Crosspane audio plug-in
//! (WP-3.0b/3.4) and the default physical output.
//!
//! # Public SDK paths
//!
//! Public CoreAudio only, declared in `audio/ffi.rs` and used from `audio/native.rs`:
//! `AudioObjectGetPropertyData[Size]`, `AudioObject{Add,Remove}PropertyListenerBlock` and
//! `AudioDeviceCreateIOProcID`/`Start`/`Stop`/`DestroyIOProcID`. No private API, no Mach service,
//! no custom IPC, no PID handshake, no HAL calls inside the driver, no HAL callbacks that touch
//! agent state beyond setting an atomic flag. Everything the policy code does goes through the
//! small read-only [`hal::Hal`] trait, which the tests implement with a fake: the trait has no
//! setter, so the host cannot change a default device, a hardware rate or a format.
//!
//! # Devices and demand
//!
//! The four devices are found only by UID (`kAudioHardwarePropertyTranslateUIDToDevice` with a
//! CFString qualifier): speakers app/loopback and microphone app/loopback, UIDs
//! [`SPEAKERS_APP_UID`] and friends. Each is validated before use: the read-back UID, object class
//! (`adev`), virtual transport, alive, hidden flag, exactly one stream in exactly the right scope,
//! the complete ASBD (48 kHz packed native float32 interleaved, 2 or 1 channels, every field), and
//! the nominal rate. Never enumeration by name or order; never a default input.
//!
//! Demand is `kAudioDevicePropertyDeviceIsRunningSomewhere` of the two **visible** devices only.
//! The host subscribes (adds listeners) first, then reads the initial snapshot; a notification
//! sets one atomic flag and wakes the owner thread, which re-reads the HAL outside any IOProc and
//! reconciles. Hidden-device state is never observed, so the host's own loopback IO can never look
//! like application demand. While the visible speakers are running the host runs one IOProc on the
//! hidden speakers loopback input and forwards its audio; when they stop, the IOProc is stopped and
//! destroyed, so with no active application there is no loopback IO at all. Microphone demand is
//! reported as a [`AudioEvent::VirtualActive`] level for `AudioKind::Microphone`, and nothing else
//! happens: no IOProc is ever created on either microphone device.
//!
//! # One peer
//!
//! The host serves exactly one peer. `add_peer` for the bound peer is idempotent (it re-arms the
//! rings, no event churn); `add_peer` for another peer while one is bound fails with
//! `PlatformError::Backend(PEER_BUSY)` (the frozen `PlatformError` has no `Unavailable` variant). A
//! missing plug-in is `Unsupported`. Every rebind after a detach (`remove_peer`, or a lost binding
//! re-added), the same peer included, is refused `PEER_BUSY` while either visible device is
//! active, judged on the listener-backed snapshot before the binding is published (the
//! provisional listeners are retired on refusal); only the very first bind accepts running
//! devices, so an agent restart mid-playback still binds. Nothing is ever rebound automatically. Installed
//! objects are static: `remove_peer` detaches the host's ports, listeners and IO, not the driver.
//! If the devices disappear (service restart, plug-in unload) the binding is marked lost, demand
//! drops, `DeviceError::Unavailable` is reported for both kinds, and only a deliberate
//! `remove_peer`/`add_peer` rebinds.
//!
//! # Format, clock, rings
//!
//! `VirtualPorts::speaker_out` carries 48 kHz interleaved stereo f32 through a preallocated 50 ms
//! SPSC ring (4800 samples). The IOProc is driven by the plug-in's software clock; the host
//! neither resamples nor reorders, it copies each cycle's frames. It accepts exactly one
//! interleaved 2-channel buffer of at most 4096 frames, zero frames being a no-op and a null
//! (disabled-stream) buffer being "no data"; anything else is a bad cycle: nothing is forwarded,
//! the forwarding session ends and `DeviceError::Failed` is reported. Samples that are not finite
//! become 0.0. On overrun whole frames are dropped (the ring's free space is rounded down to a
//! frame), so the channels can never rotate. `mic_in` is returned empty and is never written.
//! Opus's 480-frame packets are independent of HAL callback sizes.
//!
//! # Gate
//!
//! `open_playback` fails `Locked` while the [`IoGate`] is closed. Every registration that moves
//! samples (the physical playback IOProc and the hidden speaker forwarder alike) checks the gate
//! each cycle, before any sample is read or consumed, and an observed closure **latches** that
//! registration silent at once: it never moves a sample again, whether or not the gate reopens.
//! The owner thread also polls the gate every 20 ms and retires a registration whose gate closed
//! even between callbacks. A closed playback handle reports `DeviceError::Locked` and is not
//! restarted. The latch is carried through every retirement and through a same-peer re-add (even
//! one that runs before the owner has supervised a callback that already observed the closure),
//! and is cleared only when the owner reconciles the speakers inactive. A latched forwarder is
//! retired and **stays stopped across a reopened gate**; a new
//! one starts only on a fresh demand cycle after the gate is open (below). Every callback also
//! checks the host's shutdown flag first, so dropping the host silences all of them even if a
//! retirement is stuck; teardown disables every control before retiring any.
//!
//! **v0 limitation:** after a lock/unlock the speakers do not resume by themselves. The visible
//! speaker must go inactive, the owner must reconcile that, and it must go active again with the
//! gate open: in practice the user pauses and plays again.
//!
//! # Physical playback
//!
//! `open_playback` supports only the default output, and only when it is not a Crosspane device
//! and its client-facing format is exactly 48 kHz float32 stereo, one stream, interleaved or
//! planar. Otherwise `Unsupported`, with no IOProc created and nothing changed. Default-device
//! changes, device loss, nominal-rate or stream-format changes close the handle with
//! `DeviceError::Unavailable`; the caller reopens deliberately. Physical `open_capture` is always
//! `Unsupported`: it never reads a device, a default input, a permission state, and no microphone
//! is ever opened by this module.
//!
//! # Lifetimes and retirement
//!
//! One owner thread serializes every HAL call (bounded 2 s per operation; a late success after a
//! timeout is undone). IO callbacks are admitted through [`hal::CallbackControl`]: retirement
//! first disables it (effective immediately), then stops and destroys the IOProc, then drains
//! admitted callbacks (bounded 100 ms). In the native HAL the IOProc's client data (a small
//! registration holding two atomics) is **never freed**; a trampoline's first action is to count
//! itself in, its second to find the registration disabled and return silent, and only then may
//! it touch the callback state (rings, buffers), which retirement releases only after the proc is
//! destroyed and no trampoline is inside. If that cannot be proven the state is kept, disabled,
//! forever (and the session `mem::forget`ten), never freed while possibly
//! referenced. Listener blocks hold their own `Arc<Notifier>`. `AudioStop::stop` disables sample
//! flow at once and waits at most 30 ms for the owner thread's retirement. Dropping the host
//! retires everything and joins its thread (or leaks it if the HAL wedges it).
//!
//! # Known limits and unverified assumptions
//!
//! - Physical playback needs the default output to already run at 48 kHz float32 stereo; the host
//!   never changes a hardware rate, so a device running at 44.1 kHz (the dev Mac's built-in
//!   speakers do whenever the last client opened them at 44.1) is `Unsupported`. No resampler.
//! - The devices are required to report virtual transport (`virt`), as the P9 spike's driver does;
//!   WP-3.4's property table must agree.
//! - Whether starting IO on the hidden loopback *input* needs Microphone permission, what the OS
//!   returns when it is denied (the host maps `kAudioDevicePermissionsError` to
//!   `PermissionDenied`, anything else to `Failed`), and whether denied IO is delivered as silence
//!   rather than an error are all unknown until the owner-attended probe. The host never requests
//!   or works around a permission.
//!
//! # Owner-run probe (never run by the acceptance commands)
//!
//! `tests/audio.rs` ends with two `#[ignore]`d tests that also need
//! `CROSSPANE_AUDIO_OWNER_ATTENDED=1`. After the owner installs the signed plug-in (WP-3.4's
//! commands), in a Mac login session:
//!
//! ```sh
//! CROSSPANE_AUDIO_OWNER_ATTENDED=1 cargo test -p crosspane-platform-macos --test audio \
//!     -- --ignored --nocapture owner_attended
//! ```
//!
//! The speaker probe binds the real devices and for 20 s prints demand events and the level of
//! whatever is played to "Crosspane speakers". Expect `VirtualActive{Speaker, true}` and non-zero
//! PCM, no change when a second application starts, `VirtualActive{Speaker, false}` when the last
//! stops, a `DeviceError` (never silent success) if macOS denies the loopback input, and no
//! microphone IO anywhere. The tone probe plays one quiet second on the default output when that is
//! 48 kHz float32 stereo and prints why not otherwise. Neither changes any default device.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::thread::{self, JoinHandle, Thread, ThreadId};
use std::time::{Duration, Instant};

use crosspane_platform::{
    AudioCapture, AudioDeviceError, AudioEvent, AudioFormat, AudioHost, AudioKind, AudioPlayback,
    AudioStop, EventSink, IoGate, PlatformError, VirtualPorts,
};
use crosspane_types::id::NodeId;
use rtrb::{Consumer, Producer, RingBuffer};

mod callback;
mod devices;
mod ffi;
pub mod hal;
mod native;

use callback::{PlaybackRender, RingCell, SpeakerForward};
use devices::CONTRACTS;
pub use devices::{MIC_APP_UID, MIC_LOOPBACK_UID, SPEAKERS_APP_UID, SPEAKERS_LOOPBACK_UID};
use hal::{DeviceId, Hal, HalError, IoCallback, IoSession, ListenTarget, ListenerId, Notifier};

/// The message of the `PlatformError::Backend` that `add_peer` returns when the Crosspane devices
/// are already bound to another peer (v0 serves one peer; the frozen `PlatformError` has no
/// `Unavailable` variant).
pub const PEER_BUSY: &str = "Crosspane audio devices are bound to another peer";

const PLUGIN_MISSING: &str = "the Crosspane audio plug-in is not installed";
const DRIVER_MISMATCH: &str = "the installed Crosspane audio devices do not match the v0 contract";

/// Every host method is bounded by this (the trait's 2 s rule).
const REQUEST_TIME: Duration = Duration::from_secs(2);
/// How long `AudioStop::stop` waits for the owner thread (the trait's 50 ms rule, with margin).
const STOP_WAIT: Duration = Duration::from_millis(30);
/// How long retirement waits for admitted callbacks / notifications to finish.
const RETIRE_WAIT: Duration = Duration::from_millis(100);
/// The owner thread's cadence while IO or playback is active (gate polling, supervision).
const TICK: Duration = Duration::from_millis(20);
/// The owner thread's wait when idle; every command and notification wakes it earlier.
const IDLE: Duration = Duration::from_secs(1);
const MAX_PLAYBACKS: usize = 4;
/// Consecutive failed HAL reads after which a binding is declared lost.
const MAX_READ_FAILURES: u8 = 3;

fn speaker_ring() -> (Producer<f32>, Consumer<f32>) {
    // Fifty milliseconds: five 10 ms packets of interleaved stereo.
    RingBuffer::new(AudioKind::Speaker.format().frame_samples() * 5)
}

fn mic_ring() -> (Producer<f32>, Consumer<f32>) {
    RingBuffer::new(AudioKind::Microphone.format().frame_samples() * 5)
}

fn hal_error(error: HalError) -> PlatformError {
    match error {
        HalError::Gone => PlatformError::NotFound,
        HalError::Denied => PlatformError::Backend("CoreAudio refused the operation".into()),
        HalError::Status(code) => PlatformError::Backend(format!("CoreAudio status {code}")),
        HalError::Stuck => PlatformError::Timeout,
        HalError::Invalid => PlatformError::Backend("CoreAudio request invalid".into()),
    }
}

/// How a failed hidden-loopback start is reported: a permission refusal is `PermissionDenied`
/// (never worked around), a vanished device `Unavailable`, anything else `Failed`.
fn start_failure(error: HalError) -> AudioDeviceError {
    match error {
        HalError::Denied => AudioDeviceError::PermissionDenied,
        HalError::Gone => AudioDeviceError::Unavailable,
        _ => AudioDeviceError::Failed,
    }
}

/// Stop and destroy an IOProc registration and drain its callback. `true` when everything was
/// retired cleanly and the callback state could be released; otherwise the session and callback
/// are leaked on purpose (they may still be referenced by the HAL or a running callback).
fn retire_io(callback: Arc<dyn IoCallback>, mut session: Box<dyn IoSession>) -> bool {
    callback.control().disable();
    let stopped = matches!(session.stop(), Ok(()) | Err(HalError::Gone));
    let drained = callback.control().drain(RETIRE_WAIT);
    if stopped && drained {
        drop(session);
        true
    } else {
        tracing::warn!(
            stopped,
            drained,
            "audio IOProc retirement incomplete; state kept alive"
        );
        std::mem::forget((callback, session));
        false
    }
}

// ---------------------------------------------------------------------------------------------
// Public host
// ---------------------------------------------------------------------------------------------

enum Request {
    Add(NodeId, SyncSender<Result<VirtualPorts, PlatformError>>),
    Remove(NodeId, SyncSender<Result<(), PlatformError>>),
    Subscribe(
        Arc<dyn EventSink<AudioEvent>>,
        SyncSender<Result<(), PlatformError>>,
    ),
    Playback(SyncSender<Result<AudioPlayback, PlatformError>>),
    Barrier(SyncSender<()>),
}

struct Command {
    request: Request,
    cancelled: Arc<AtomicBool>,
}

/// The macOS [`AudioHost`]. See the module documentation for the full behaviour.
///
/// Constructing it spawns the owner thread and makes **no** CoreAudio call and creates no stream.
///
/// Events are delivered on the owner thread. The sink must only queue them (as the trait already
/// requires): it must not call back into the host, which would wait on the very thread it runs on.
/// Dropping an [`AudioPlayback`] from the sink is fine.
pub struct CoreAudioHost {
    commands: SyncSender<Command>,
    owner: Thread,
    shutdown: Arc<AtomicBool>,
    done: Receiver<()>,
    thread: Option<JoinHandle<()>>,
    gate: Arc<IoGate>,
    /// Commands sent and not yet taken by the owner thread (test synchronisation only).
    queued: Arc<AtomicUsize>,
}

impl std::fmt::Debug for CoreAudioHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreAudioHost").finish_non_exhaustive()
    }
}

impl CoreAudioHost {
    /// A host over the real CoreAudio HAL.
    pub fn new(gate: Arc<IoGate>) -> Result<Self, PlatformError> {
        Self::with_hal(gate, Arc::new(native::NativeHal::new()))
    }

    /// A host over any [`Hal`]. This is how the tests run the real policy against a fake HAL.
    #[doc(hidden)]
    pub fn with_hal(gate: Arc<IoGate>, hal: Arc<dyn Hal>) -> Result<Self, PlatformError> {
        let (commands, receiver) = mpsc::sync_channel(16);
        let (done_tx, done) = mpsc::sync_channel(1);
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = shutdown.clone();
        let worker_gate = gate.clone();
        let queued = Arc::new(AtomicUsize::new(0));
        let worker_queued = queued.clone();
        let thread = thread::Builder::new()
            .name("crosspane-coreaudio".into())
            .spawn(move || {
                Worker::new(hal, worker_gate, worker_shutdown.clone(), worker_queued)
                    .run(receiver, &worker_shutdown);
                let _ = done_tx.send(());
            })
            .map_err(|_| PlatformError::Backend("CoreAudio host thread failed to start".into()))?;
        Ok(Self {
            commands,
            owner: thread.thread().clone(),
            shutdown,
            done,
            thread: Some(thread),
            gate,
            queued,
        })
    }

    /// How many commands are queued behind the owner thread right now. Test synchronisation only:
    /// lets a test know a call is waiting without racing it.
    #[doc(hidden)]
    pub fn queue_probe(&self) -> Arc<AtomicUsize> {
        self.queued.clone()
    }

    /// Wait until the owner thread has handled everything posted before this call, including every
    /// pending change notification. Test synchronisation only.
    #[doc(hidden)]
    pub fn sync(&self) -> Result<(), PlatformError> {
        self.request(Request::Barrier)
    }

    fn request<T>(&self, build: impl FnOnce(SyncSender<T>) -> Request) -> Result<T, PlatformError> {
        let (reply, receive) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        self.queued.fetch_add(1, Ordering::SeqCst);
        self.commands
            .try_send(Command {
                request: build(reply),
                cancelled: cancelled.clone(),
            })
            .map_err(|error| {
                self.queued.fetch_sub(1, Ordering::SeqCst);
                match error {
                    mpsc::TrySendError::Full(_) => PlatformError::Timeout,
                    mpsc::TrySendError::Disconnected(_) => {
                        PlatformError::Backend("CoreAudio host thread is gone".into())
                    }
                }
            })?;
        self.owner.unpark();
        match receive.recv_timeout(REQUEST_TIME) {
            Ok(value) => Ok(value),
            Err(_) => {
                // The owner thread may still run it later; it checks this flag and undoes a late
                // success whose caller has gone.
                cancelled.store(true, Ordering::SeqCst);
                Err(PlatformError::Timeout)
            }
        }
    }
}

impl AudioHost for CoreAudioHost {
    fn add_peer(&mut self, peer: NodeId, name: &str) -> Result<VirtualPorts, PlatformError> {
        // The visible device names are fixed by the plug-in ("Crosspane speakers"/"Crosspane
        // microphone"); the peer's display name is not used.
        let _ = name;
        self.request(|reply| Request::Add(peer, reply))?
    }

    fn remove_peer(&mut self, peer: NodeId) -> Result<(), PlatformError> {
        self.request(|reply| Request::Remove(peer, reply))?
    }

    fn subscribe(&mut self, sink: Arc<dyn EventSink<AudioEvent>>) -> Result<(), PlatformError> {
        self.request(|reply| Request::Subscribe(sink, reply))?
    }

    fn open_capture(&mut self, _format: AudioFormat) -> Result<AudioCapture, PlatformError> {
        // Always refused, before anything else: no gate lookup, no device, no default input, no
        // permission request, no future-microphone flag. v0 never records.
        Err(PlatformError::Unsupported(
            "audio capture is not supported on macOS in v0",
        ))
    }

    fn open_playback(&mut self, format: AudioFormat) -> Result<AudioPlayback, PlatformError> {
        if !self.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        if format != AudioKind::Speaker.format() {
            return Err(PlatformError::Unsupported(
                "macOS playback is 48 kHz stereo only",
            ));
        }
        self.request(Request::Playback)?
    }
}

impl Drop for CoreAudioHost {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.owner.unpark();
        if self.done.recv_timeout(REQUEST_TIME).is_ok()
            && let Some(thread) = self.thread.take()
        {
            let _ = thread.join();
        }
        // If the HAL wedged the thread, it is detached: it owns all callback state and never frees
        // it while it is stuck.
    }
}

// ---------------------------------------------------------------------------------------------
// Owner thread
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Devices {
    speakers_app: DeviceId,
    speakers_loop: DeviceId,
    mic_app: DeviceId,
    mic_loop: DeviceId,
}

impl Devices {
    fn all(self) -> [DeviceId; 4] {
        [
            self.speakers_app,
            self.speakers_loop,
            self.mic_app,
            self.mic_loop,
        ]
    }
}

struct ForwardSession {
    callback: Arc<SpeakerForward>,
    io: Box<dyn IoSession>,
}

enum Forward {
    /// No loopback IO and none wanted: the speakers are inactive.
    Idle,
    /// A start is permitted (a fresh demand edge reconciled with the gate open, or a deliberate
    /// re-add); the owner starts it in this same service pass.
    Pending,
    Running(ForwardSession),
    /// Start or cycle failed; retried only on the next demand edge (or a deliberate re-add).
    Failed,
    /// A retirement did not finish: the old ring state may still be referenced, so a fresh ring is
    /// required (a deliberate re-add provides one).
    Poisoned,
}

struct Binding {
    peer: NodeId,
    devices: Devices,
    notifier: Arc<Notifier>,
    listeners: Vec<ListenerId>,
    speaker_demand: bool,
    mic_demand: bool,
    ring: Arc<RingCell<Producer<f32>>>,
    /// The consumer end of the (never written) microphone ring, kept so the agent's producer is
    /// not abandoned. Never read.
    _mic_end: Consumer<f32>,
    forward: Forward,
    /// An observed gate closure latched the forwarder: no forwarder runs or starts for this demand
    /// cycle, whatever the gate does afterwards. It is carried through every retirement and
    /// re-add and cleared only when the owner reconciles the speakers inactive, so the next
    /// forwarder starts on the following inactive-to-active cycle with the gate open.
    gate_latched: bool,
    lost: bool,
    read_failures: u8,
}

struct Playback {
    device: DeviceId,
    snapshot: hal::DeviceInfo,
    render: Arc<PlaybackRender>,
    io: Box<dyn IoSession>,
    notifier: Arc<Notifier>,
    listeners: Vec<ListenerId>,
    retired: Arc<AtomicBool>,
}

struct Worker {
    hal: Arc<dyn Hal>,
    gate: Arc<IoGate>,
    me: Thread,
    sink: Option<Arc<dyn EventSink<AudioEvent>>>,
    binding: Option<Binding>,
    /// True once any binding has been detached (removed, or lost and re-added): every later bind
    /// is a rebind and needs both visible devices inactive.
    detached_before: bool,
    playbacks: Vec<Playback>,
    /// The host's shutdown flag, shared with every callback.
    shutdown: Arc<AtomicBool>,
    queued: Arc<AtomicUsize>,
}

impl Worker {
    fn new(
        hal: Arc<dyn Hal>,
        gate: Arc<IoGate>,
        shutdown: Arc<AtomicBool>,
        queued: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            hal,
            gate,
            shutdown,
            queued,
            me: thread::current(),
            sink: None,
            binding: None,
            detached_before: false,
            playbacks: Vec::new(),
        }
    }

    fn run(mut self, receiver: Receiver<Command>, shutdown: &AtomicBool) {
        let mut barriers: Vec<SyncSender<()>> = Vec::new();
        while !shutdown.load(Ordering::SeqCst) {
            for _ in 0..16 {
                match receiver.try_recv() {
                    Ok(command) => {
                        self.queued.fetch_sub(1, Ordering::SeqCst);
                        self.handle(command, &mut barriers);
                    }
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                }
            }
            self.service();
            for barrier in barriers.drain(..) {
                let _ = barrier.send(());
            }
            thread::park_timeout(self.wait());
        }
        self.teardown();
    }

    fn wait(&self) -> Duration {
        // Only these states can change without a notification or a command: a running loopback
        // IOProc (gate and callback supervision), a permitted start, a failed HAL read.
        let binding_busy = self.binding.as_ref().is_some_and(|b| {
            !b.lost
                && (matches!(b.forward, Forward::Running(_) | Forward::Pending)
                    || b.read_failures > 0)
        });
        if binding_busy || !self.playbacks.is_empty() {
            TICK
        } else {
            IDLE
        }
    }

    fn emit(&self, event: AudioEvent) {
        if let Some(sink) = &self.sink {
            sink.send(event);
        }
    }

    fn handle(&mut self, command: Command, barriers: &mut Vec<SyncSender<()>>) {
        let cancelled = command.cancelled;
        match command.request {
            Request::Barrier(reply) => barriers.push(reply),
            Request::Subscribe(sink, reply) => {
                if cancelled.load(Ordering::SeqCst) {
                    return;
                }
                let result = self.subscribe(sink);
                let _ = reply.send(result);
            }
            Request::Add(peer, reply) => {
                if cancelled.load(Ordering::SeqCst) {
                    return;
                }
                let result = self.add_peer(peer, &cancelled);
                if let Err(mpsc::SendError(late)) = reply.send(result) {
                    // The caller gave up. A late success must not leave a binding nobody owns.
                    if late.is_ok() {
                        let _ = self.remove_peer(peer);
                    }
                }
            }
            Request::Remove(peer, reply) => {
                if cancelled.load(Ordering::SeqCst) {
                    return;
                }
                let result = self.remove_peer(peer);
                let _ = reply.send(result);
            }
            Request::Playback(reply) => {
                if cancelled.load(Ordering::SeqCst) {
                    return;
                }
                let result = self.open_playback(&cancelled);
                if let Err(mpsc::SendError(late)) = reply.send(result) {
                    // Dropping the handle disables it; retire it right away.
                    drop(late);
                    self.service_playbacks();
                }
            }
        }
    }

    /// Everything that happens between commands: reconcile notifications, supervise IO, poll the
    /// gate.
    fn service(&mut self) {
        self.refresh_if_dirty();
        self.supervise_forward();
        self.service_playbacks();
    }

    /// Host drop. Every callback is silenced before any retirement starts, so a slow or stuck
    /// retirement cannot leave the others moving samples.
    fn teardown(&mut self) {
        for playback in &self.playbacks {
            playback.render.control().disable();
        }
        if let Some(Forward::Running(session)) = self.binding.as_ref().map(|b| &b.forward) {
            session.callback.control().disable();
        }
        for playback in std::mem::take(&mut self.playbacks) {
            self.retire_playback(playback, None);
        }
        let _ = self.detach_binding();
    }

    // -- subscribe ------------------------------------------------------------------------

    fn subscribe(&mut self, sink: Arc<dyn EventSink<AudioEvent>>) -> Result<(), PlatformError> {
        if self.sink.is_some() {
            return Err(PlatformError::Unsupported(
                "audio subscription already installed",
            ));
        }
        self.sink = Some(sink);
        if let Some(b) = &self.binding {
            let (peer, speaker, mic) = (b.peer, b.speaker_demand, b.mic_demand);
            self.emit_active(peer, AudioKind::Speaker, speaker);
            self.emit_active(peer, AudioKind::Microphone, mic);
        }
        Ok(())
    }

    fn emit_active(&self, peer: NodeId, kind: AudioKind, active: bool) {
        self.emit(AudioEvent::VirtualActive { peer, kind, active });
    }

    fn emit_error(&self, peer: Option<NodeId>, kind: AudioKind, error: AudioDeviceError) {
        self.emit(AudioEvent::DeviceError { peer, kind, error });
    }

    // -- add / remove ---------------------------------------------------------------------

    fn busy() -> PlatformError {
        PlatformError::Backend(PEER_BUSY.into())
    }

    fn running(&self, device: DeviceId) -> Result<bool, PlatformError> {
        self.hal.is_running_somewhere(device).map_err(hal_error)
    }

    /// Find the four devices by exact UID and validate each against its contract.
    fn locate(&self) -> Result<Devices, PlatformError> {
        let mut ids = [DeviceId(0); 4];
        let mut missing = 0;
        for (slot, contract) in ids.iter_mut().zip(CONTRACTS) {
            match self.hal.translate_uid(contract.uid) {
                Ok(Some(id)) => *slot = id,
                Ok(None) | Err(HalError::Gone) => missing += 1,
                Err(error) => return Err(hal_error(error)),
            }
        }
        if missing == ids.len() {
            return Err(PlatformError::Unsupported(PLUGIN_MISSING));
        }
        if missing > 0 {
            return Err(PlatformError::Unsupported(DRIVER_MISMATCH));
        }
        for (index, id) in ids.iter().enumerate() {
            if ids[..index].contains(id) {
                return Err(PlatformError::Unsupported(DRIVER_MISMATCH));
            }
        }
        for (id, contract) in ids.iter().zip(CONTRACTS) {
            let info = match self.hal.device_info(*id) {
                Ok(info) => info,
                // The object vanished between translation and validation.
                Err(HalError::Gone) => return Err(PlatformError::Unsupported(DRIVER_MISMATCH)),
                Err(error) => return Err(hal_error(error)),
            };
            if !devices::matches_contract(&info, &contract) {
                return Err(PlatformError::Unsupported(DRIVER_MISMATCH));
            }
        }
        Ok(Devices {
            speakers_app: ids[0],
            speakers_loop: ids[1],
            mic_app: ids[2],
            mic_loop: ids[3],
        })
    }

    fn add_peer(
        &mut self,
        peer: NodeId,
        cancelled: &AtomicBool,
    ) -> Result<VirtualPorts, PlatformError> {
        if let Some(binding) = &self.binding {
            if binding.peer != peer {
                return Err(Self::busy());
            }
            if !binding.lost {
                return self.rearm();
            }
            // The same peer deliberately re-adds after the devices were lost: detach, then bind
            // from scratch like any other rebind.
            let _ = self.detach_binding();
        }
        let devices = self.locate()?;
        if cancelled.load(Ordering::SeqCst) {
            return Err(PlatformError::Timeout);
        }
        let notifier = Notifier::new(self.me.clone());
        // Subscribe before the snapshot: listeners first (running state of the visible devices,
        // liveness of all four, device-list and service changes), then read the initial state.
        let mut targets = vec![
            ListenTarget::DeviceRunning(devices.speakers_app),
            ListenTarget::DeviceRunning(devices.mic_app),
            ListenTarget::DeviceList,
            ListenTarget::ServiceRestarted,
        ];
        targets.extend(devices.all().map(ListenTarget::DeviceAlive));
        let mut listeners = Vec::with_capacity(targets.len());
        for target in targets {
            match self.hal.add_listener(target, notifier.clone()) {
                Ok(id) => listeners.push(id),
                Err(error) => {
                    self.release_listeners(&notifier, listeners);
                    return Err(hal_error(error));
                }
            }
        }
        let (speaker_now, mic_now) = match (
            self.running(devices.speakers_app),
            self.running(devices.mic_app),
        ) {
            (Ok(speaker), Ok(mic)) => (speaker, mic),
            (Err(error), _) | (_, Err(error)) => {
                self.release_listeners(&notifier, listeners);
                return Err(error);
            }
        };
        if self.detached_before && (speaker_now || mic_now) {
            // Any rebind (the same peer included) under an application that is using a visible
            // device is refused, judged on the listener-backed snapshot, before anything is
            // published. The provisional listeners are retired.
            self.release_listeners(&notifier, listeners);
            return Err(Self::busy());
        }
        let (speaker_producer, speaker_out) = speaker_ring();
        let (mic_in, mic_end) = mic_ring();
        self.binding = Some(Binding {
            peer,
            devices,
            notifier,
            listeners,
            speaker_demand: false,
            mic_demand: false,
            ring: Arc::new(RingCell::new(speaker_producer)),
            _mic_end: mic_end,
            forward: Forward::Idle,
            gate_latched: false,
            lost: false,
            read_failures: 0,
        });
        // Initial state: edges from "inactive", so a device that is already running at the very
        // first bind is reported and forwarded exactly like one that starts later.
        self.apply_demand(speaker_now, mic_now);
        Ok(VirtualPorts {
            speaker_out,
            mic_in,
        })
    }

    /// The bound peer calls `add_peer` again: fresh rings, same binding, no events. A re-add is
    /// not a demand cycle, so a gate latch survives it: one the owner already recorded, one a
    /// still-running callback has observed but the owner has not yet supervised, one observed
    /// while the old forwarder was being retired, and a gate that is closed right now. Anything
    /// else restarts on the new ring.
    fn rearm(&mut self) -> Result<VirtualPorts, PlatformError> {
        let closed_before = !self.gate.is_open();
        let observed = self.stop_forward(Forward::Idle);
        let closed_after = !self.gate.is_open();
        let Some(binding) = self.binding.as_mut() else {
            return Err(PlatformError::NotFound);
        };
        if binding.speaker_demand && (observed || closed_before || closed_after) {
            binding.gate_latched = true;
        }
        let (speaker_producer, speaker_out) = speaker_ring();
        let (mic_in, mic_end) = mic_ring();
        binding.ring = Arc::new(RingCell::new(speaker_producer));
        binding._mic_end = mic_end;
        // `supervise_forward` starts a permitted forwarder right after this command.
        binding.forward = if binding.speaker_demand && !binding.gate_latched {
            Forward::Pending
        } else {
            Forward::Idle
        };
        Ok(VirtualPorts {
            speaker_out,
            mic_in,
        })
    }

    fn release_listeners(&self, notifier: &Notifier, listeners: Vec<ListenerId>) -> bool {
        notifier.retire();
        let mut clean = true;
        for listener in listeners {
            clean &= self.hal.remove_listener(listener).is_ok();
        }
        clean & self.hal.flush_listeners(RETIRE_WAIT)
    }

    fn remove_peer(&mut self, peer: NodeId) -> Result<(), PlatformError> {
        if self.binding.as_ref().is_none_or(|b| b.peer != peer) {
            return Err(PlatformError::NotFound);
        }
        // Drain anything the HAL reported before the removal, so the sink sees it in order.
        self.refresh_if_dirty();
        let (speaker, mic, clean) = self.detach_binding();
        // Forwarding is already stopped, so these are the last events of the old binding.
        if speaker {
            self.emit_active(peer, AudioKind::Speaker, false);
        }
        if mic {
            self.emit_active(peer, AudioKind::Microphone, false);
        }
        if clean {
            Ok(())
        } else {
            Err(PlatformError::Timeout)
        }
    }

    /// Retire the binding's IO and listeners. Returns the demand it had and whether retirement was
    /// clean. After this nothing from the old binding can produce an event.
    fn detach_binding(&mut self) -> (bool, bool, bool) {
        let Some(mut binding) = self.binding.take() else {
            return (false, false, true);
        };
        self.detached_before = true;
        binding.notifier.retire();
        let mut clean = true;
        if let Forward::Running(session) = std::mem::replace(&mut binding.forward, Forward::Idle) {
            clean &= retire_io(session.callback, session.io);
        }
        for listener in binding.listeners.drain(..) {
            clean &= self.hal.remove_listener(listener).is_ok();
        }
        clean &= self.hal.flush_listeners(RETIRE_WAIT);
        (binding.speaker_demand, binding.mic_demand, clean)
    }

    // -- demand and forwarding ------------------------------------------------------------

    /// Apply a newly observed (speaker, microphone) running state, emitting only edges.
    ///
    /// A forwarder can start only here, on a rising speaker edge reconciled while the gate is
    /// open (`Pending`, started by `supervise_forward` in the same service pass). A rising edge
    /// while the gate is closed latches instead: the owner never starts a forwarder just because
    /// the gate reopened.
    fn apply_demand(&mut self, speaker: bool, mic: bool) {
        let gate_open = self.gate.is_open();
        let Some(binding) = self.binding.as_mut() else {
            return;
        };
        let peer = binding.peer;
        let speaker_edge = speaker != binding.speaker_demand;
        let mic_edge = mic != binding.mic_demand;
        binding.speaker_demand = speaker;
        binding.mic_demand = mic;
        if speaker_edge {
            if speaker {
                // A fresh demand cycle: the gate decides now. A closed gate latches this cycle.
                binding.gate_latched = !gate_open;
                if gate_open && !matches!(binding.forward, Forward::Poisoned) {
                    binding.forward = Forward::Pending;
                }
                self.emit_active(peer, AudioKind::Speaker, true);
            } else {
                // Stop forwarding first: by the time "inactive" is observed, no IO remains. This
                // reconciliation is also what ends a latch: the next rising edge is a new cycle.
                let _ = self.stop_forward(Forward::Idle);
                if let Some(binding) = self.binding.as_mut() {
                    binding.gate_latched = false;
                }
                self.emit_active(peer, AudioKind::Speaker, false);
            }
        }
        if mic_edge {
            // A level only: no microphone IO is ever created.
            self.emit_active(peer, AudioKind::Microphone, mic);
        }
    }

    /// Start the hidden speakers-loopback IOProc for a permitted (`Pending`) start. Returns the
    /// failure to report, if any; the caller emits it.
    fn start_forward(&mut self) -> Option<AudioDeviceError> {
        let gate_open = self.gate.is_open();
        let shutdown = self.shutdown.clone();
        let binding = self.binding.as_mut()?;
        if binding.lost || !binding.speaker_demand || !matches!(binding.forward, Forward::Pending) {
            return None;
        }
        if binding.gate_latched || !gate_open {
            // Closed between the edge and the start: latch, do not wait for the gate.
            binding.gate_latched = true;
            binding.forward = Forward::Idle;
            return None;
        }
        let callback = Arc::new(SpeakerForward::new(
            binding.ring.clone(),
            self.gate.clone(),
            shutdown,
        ));
        match self
            .hal
            .start_io(binding.devices.speakers_loop, callback.clone())
        {
            Ok(io) => {
                binding.forward = Forward::Running(ForwardSession { callback, io });
                None
            }
            Err(error) => {
                binding.forward = Forward::Failed;
                Some(start_failure(error))
            }
        }
    }

    fn start_forward_and_report(&mut self) {
        let peer = self.binding.as_ref().map(|b| b.peer);
        if let (Some(peer), Some(error)) = (peer, self.start_forward()) {
            self.emit_error(Some(peer), AudioKind::Speaker, error);
        }
    }

    /// Retire a running forwarder. A clean stop (or no forwarder at all) leaves `then`, except
    /// that a poisoned ring stays poisoned; an unclean stop poisons the ring until a deliberate
    /// re-add and reports `Failed`. Returns whether the retired forwarder observed a gate closure
    /// at any point up to the end of its retirement (a callback still inside when retirement
    /// began can observe one after it was disabled). While demand is active, an observed closure
    /// and any unclean retirement (which cannot rule out an unpublished one) are both recorded in
    /// `gate_latched`, which only an inactive reconciliation clears.
    fn stop_forward(&mut self, then: Forward) -> bool {
        let Some(binding) = self.binding.as_mut() else {
            return false;
        };
        let peer = binding.peer;
        let (observed, poisoned) = match std::mem::replace(&mut binding.forward, Forward::Idle) {
            Forward::Running(session) => {
                let observer = session.callback.clone();
                let clean = retire_io(session.callback, session.io);
                // Read after the drain: this includes a closure seen during retirement.
                let observed = observer.control().gate_closed_seen();
                binding.forward = if clean { then } else { Forward::Poisoned };
                (observed, !clean)
            }
            Forward::Poisoned => {
                binding.forward = Forward::Poisoned;
                (false, false)
            }
            _ => {
                binding.forward = then;
                (false, false)
            }
        };
        // An unclean retirement did not prove the old callbacks quiescent, so a callback that has
        // seen the closed gate but not yet published its latch may still do so into state nobody
        // reads again: while demand is active, treat the closure as observed.
        if (observed || poisoned) && binding.speaker_demand {
            binding.gate_latched = true;
        }
        if poisoned {
            self.emit_error(Some(peer), AudioKind::Speaker, AudioDeviceError::Failed);
        }
        observed
    }

    /// Per-tick supervision of the loopback IOProc.
    fn supervise_forward(&mut self) {
        let Some(binding) = self.binding.as_mut() else {
            return;
        };
        if binding.lost {
            return;
        }
        let peer = binding.peer;
        match &binding.forward {
            Forward::Running(session) => {
                let control = session.callback.control();
                let bad = control.bad_buffer_seen();
                let closed = control.gate_closed_seen() || !self.gate.is_open();
                if bad || closed {
                    if closed {
                        // Fail closed, and remember it before anything else can restart IO: the
                        // callback has already latched itself silent if it saw the closure.
                        binding.gate_latched = true;
                    }
                    let _ = self.stop_forward(if bad { Forward::Failed } else { Forward::Idle });
                    // An unclean stop already reported `Failed` and poisoned the ring.
                    if bad
                        && matches!(
                            self.binding.as_ref().map(|b| &b.forward),
                            Some(Forward::Failed)
                        )
                    {
                        self.emit_error(Some(peer), AudioKind::Speaker, AudioDeviceError::Failed);
                    }
                }
            }
            Forward::Pending => self.start_forward_and_report(),
            _ => {}
        }
    }

    // -- notifications --------------------------------------------------------------------

    fn refresh_if_dirty(&mut self) {
        let dirty = self
            .binding
            .as_ref()
            .is_some_and(|b| !b.lost && b.notifier.take_dirty());
        if dirty {
            self.refresh_binding();
        }
    }

    /// Re-read the HAL and reconcile. State, not an event log: a demand that already ended may
    /// never be reported, but forwarding cannot outlive an inactive reconciliation.
    fn refresh_binding(&mut self) {
        let Some(binding) = self.binding.as_ref() else {
            return;
        };
        let devices = binding.devices;
        let mut gone = false;
        for (device, contract) in devices.all().into_iter().zip(CONTRACTS) {
            match self.hal.is_alive(device) {
                Ok(true) => {}
                _ => gone = true,
            }
            // Object ids are stable for the life of the plug-in; a changed or missing UID mapping
            // means the plug-in was reloaded.
            if !matches!(self.hal.translate_uid(contract.uid), Ok(Some(id)) if id == device) {
                gone = true;
            }
        }
        if gone {
            self.mark_lost();
            return;
        }
        match (
            self.hal.is_running_somewhere(devices.speakers_app),
            self.hal.is_running_somewhere(devices.mic_app),
        ) {
            (Ok(speaker), Ok(mic)) => {
                if let Some(binding) = self.binding.as_mut() {
                    binding.read_failures = 0;
                }
                self.apply_demand(speaker, mic);
            }
            (Err(HalError::Gone), _) | (_, Err(HalError::Gone)) => self.mark_lost(),
            _ => {
                // A transient read failure: try again next tick, give up after a few.
                let Some(binding) = self.binding.as_mut() else {
                    return;
                };
                binding.read_failures += 1;
                if binding.read_failures >= MAX_READ_FAILURES {
                    self.mark_lost();
                } else {
                    binding.notifier.mark_dirty();
                }
            }
        }
    }

    /// The devices are gone: end demand and IO, report once, wait for a deliberate rebind.
    fn mark_lost(&mut self) {
        let _ = self.stop_forward(Forward::Idle);
        let Some(binding) = self.binding.as_mut() else {
            return;
        };
        let (peer, speaker, mic) = (binding.peer, binding.speaker_demand, binding.mic_demand);
        binding.lost = true;
        binding.speaker_demand = false;
        binding.mic_demand = false;
        binding.gate_latched = false;
        binding.forward = Forward::Idle;
        if speaker {
            self.emit_active(peer, AudioKind::Speaker, false);
        }
        if mic {
            self.emit_active(peer, AudioKind::Microphone, false);
        }
        self.emit_error(
            Some(peer),
            AudioKind::Speaker,
            AudioDeviceError::Unavailable,
        );
        self.emit_error(
            Some(peer),
            AudioKind::Microphone,
            AudioDeviceError::Unavailable,
        );
    }

    // -- physical playback ----------------------------------------------------------------

    fn open_playback(&mut self, cancelled: &AtomicBool) -> Result<AudioPlayback, PlatformError> {
        if !self.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        if self.playbacks.len() >= MAX_PLAYBACKS {
            return Err(PlatformError::Unsupported("audio stream limit"));
        }
        let device = self
            .hal
            .default_output_device()
            .map_err(hal_error)?
            .ok_or(PlatformError::NotFound)?;
        let info = self.hal.device_info(device).map_err(hal_error)?;
        let layout = devices::playback_layout(&info)
            .map_err(|reject| PlatformError::Unsupported(reject.reason()))?;
        if self
            .binding
            .as_ref()
            .is_some_and(|b| b.devices.all().contains(&device))
        {
            return Err(PlatformError::Unsupported(
                devices::Reject::CrosspaneDevice.reason(),
            ));
        }

        let notifier = Notifier::new(self.me.clone());
        let mut targets = vec![
            ListenTarget::DefaultOutput,
            ListenTarget::DeviceAlive(device),
            ListenTarget::DeviceNominalRate(device),
            ListenTarget::DeviceOutputConfig(device),
        ];
        targets.extend(
            info.output_streams
                .iter()
                .map(|stream| ListenTarget::StreamFormat(stream.id)),
        );
        let mut listeners = Vec::with_capacity(targets.len());
        for target in targets {
            match self.hal.add_listener(target, notifier.clone()) {
                Ok(id) => listeners.push(id),
                Err(error) => {
                    self.release_listeners(&notifier, listeners);
                    return Err(hal_error(error));
                }
            }
        }
        // Listeners are in place: re-read, so a change between validation and registration is
        // never missed.
        let unchanged = matches!(self.hal.default_output_device(), Ok(Some(d)) if d == device)
            && self.hal.device_info(device).as_ref() == Ok(&info);
        let permitted = self.gate.is_open();
        if !unchanged || !permitted || cancelled.load(Ordering::SeqCst) {
            self.release_listeners(&notifier, listeners);
            return Err(if !permitted {
                PlatformError::Locked
            } else if !unchanged {
                PlatformError::Backend("the default output changed while opening".into())
            } else {
                PlatformError::Timeout
            });
        }

        let (producer, consumer) = speaker_ring();
        let render = Arc::new(PlaybackRender::new(
            consumer,
            self.gate.clone(),
            layout,
            self.shutdown.clone(),
        ));
        let io = match self.hal.start_io(device, render.clone()) {
            Ok(io) => io,
            Err(error) => {
                self.release_listeners(&notifier, listeners);
                return Err(hal_error(error));
            }
        };
        let retired = Arc::new(AtomicBool::new(false));
        let stop = PlaybackStop {
            render: render.clone(),
            retired: retired.clone(),
            owner: self.me.id(),
            wake: self.me.clone(),
            stopped: false,
        };
        self.playbacks.push(Playback {
            device,
            snapshot: info,
            render,
            io,
            notifier,
            listeners,
            retired,
        });
        Ok(AudioPlayback::new(producer, Box::new(stop)))
    }

    /// `None`: keep. `Some(None)`: retire quietly (the handle was stopped). `Some(Some(error))`:
    /// retire and report.
    fn playback_verdict(&self, playback: &Playback) -> Option<Option<AudioDeviceError>> {
        if playback.render.stop_requested() {
            return Some(None);
        }
        let control = playback.render.control();
        if control.is_disabled() {
            return Some(Some(if control.gate_closed_seen() {
                AudioDeviceError::Locked
            } else {
                AudioDeviceError::Failed
            }));
        }
        if !self.gate.is_open() {
            return Some(Some(AudioDeviceError::Locked));
        }
        if playback.notifier.take_dirty() && self.playback_changed(playback) {
            return Some(Some(AudioDeviceError::Unavailable));
        }
        None
    }

    /// Did the default output, the device, or its format change since the handle opened?
    fn playback_changed(&self, playback: &Playback) -> bool {
        if !matches!(self.hal.default_output_device(), Ok(Some(d)) if d == playback.device) {
            return true;
        }
        self.hal.device_info(playback.device).as_ref() != Ok(&playback.snapshot)
    }

    fn service_playbacks(&mut self) {
        let mut index = 0;
        while index < self.playbacks.len() {
            match self.playback_verdict(&self.playbacks[index]) {
                None => index += 1,
                Some(error) => {
                    let playback = self.playbacks.remove(index);
                    self.retire_playback(playback, error);
                }
            }
        }
    }

    fn retire_playback(&mut self, playback: Playback, error: Option<AudioDeviceError>) {
        let Playback {
            render,
            io,
            notifier,
            listeners,
            retired,
            ..
        } = playback;
        render.control().disable();
        let _ = retire_io(render, io);
        let _ = self.release_listeners(&notifier, listeners);
        retired.store(true, Ordering::SeqCst);
        if let Some(error) = error {
            self.emit_error(None, AudioKind::Speaker, error);
        }
    }
}

/// The handle's stop: silence immediately, retirement on the owner thread.
struct PlaybackStop {
    render: Arc<PlaybackRender>,
    retired: Arc<AtomicBool>,
    owner: ThreadId,
    wake: Thread,
    stopped: bool,
}

impl std::fmt::Debug for PlaybackStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaybackStop")
            .field("stopped", &self.stopped)
            .finish_non_exhaustive()
    }
}

impl AudioStop for PlaybackStop {
    fn stop(&mut self) {
        if std::mem::replace(&mut self.stopped, true) {
            return;
        }
        // Effective immediately: the next callback writes silence and consumes nothing.
        self.render.request_stop();
        self.wake.unpark();
        if thread::current().id() == self.owner {
            // On the owner thread the retirement happens in its next service pass.
            return;
        }
        let deadline = Instant::now() + STOP_WAIT;
        while !self.retired.load(Ordering::SeqCst) && Instant::now() < deadline {
            thread::sleep(Duration::from_micros(200));
        }
    }
}

impl Drop for PlaybackStop {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Every layout fact and constant the FFI relies on, keyed by a C expression that evaluates to the
/// SDK's value. `tests/audio.rs` compiles those expressions with the SDK's headers and compares.
#[doc(hidden)]
pub fn sdk_abi_report() -> Vec<(&'static str, u64)> {
    ffi::abi_report()
}
