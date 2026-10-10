//! Controller capture on GNOME and KDE through the InputCapture portal (WP-G1.7).
//!
//! # The model
//!
//! The compositor activates a sticky pointer barrier on its own and from then on holds the pointer
//! and consumes every key, button, scroll and motion until the activation is released (mutter
//! `meta-input-capture-session.c`). That does not fit `InputCapture`'s "the pointer presses, the
//! engine decides, `begin` captures" directly, so this backend is a **compositor-activated pending
//! capture** adapter, under the amendments A1-A7 of `docs/wp/WP-G1.7.md`:
//!
//! - **A1 pressure** (`pressure.rs`). An activation is reported as `EdgePressed` (repeated once per
//!   EIS frame that moved the pointer) and routed nowhere: every event until `begin` is consumed
//!   and only counted. The activation is released to its origin (moved inward) when the virtual
//!   pointer moves back inward (more than 32 logical px) or off the portal's stretch (more than
//!   32 logical px past either end), when the I/O gate closes, when the portal is removed, when
//!   `end` is called, and in any case after 3 s without a `begin`. A crossing the engine refuses
//!   sends no `end` (nothing started), so its activation stays pending until one of those
//!   happens: a documented limitation, nothing is routed meanwhile.
//! - **A2 synchronous begin.** `begin` adopts the pending activation: `Ok` means capture is
//!   effective, `Started` precedes every capture event, and any failure releases the activation.
//!   `end(warp)` is `Release(activation_id, cursor_position)`.
//! - **A3 held keys (amended).** Keys held before the activation are unknown to the compositor;
//!   `held_keys` are the keys pressed since the activation, and releases of earlier ones are
//!   delivered as `down: false` for the router to drop. The compositor does not deliver those
//!   releases to its local clients either, which would keep the keys held (and repeating), so
//!   they are also **replayed locally**: [`InputCaptureConfig::local_release`] is called once per
//!   such key or button, right after the activation ended (after our `Release` returned, or when
//!   the compositor ended it on its own, or after an abort), never while the compositor still
//!   holds the activation (the injected release would be captured too), and never for a key
//!   pressed after the activation. The hook injects releases only.
//! - **A4 indicator.** GNOME shows its screen-sharing indicator at activation; the engine's HUD
//!   still precedes `begin`.
//! - **A5 cursor.** The compositor leaves the local cursor visible and frozen at the edge. The
//!   `cursor` hook (the Shell extension's `InhibitCursor`) hides it while captured, best effort,
//!   on a thread of its own.
//! - **A6 warp.** The release point is a suggestion to the compositor (mutter and KWin honour it).
//! - **A7 portal set.** `set_portals` returns `Ok` once the set is validated (the pure port of
//!   mutter's barrier rules) and installed, or handed over to be installed when the portal answers
//!   or when the current activation is over. A replacement never ends a capture whose own portal
//!   is unchanged; removing or changing the active portal ends it `Lost` first. Zone changes end
//!   the capture `Lost` and the set is installed again.
//!
//! # Threads
//!
//! | thread | owns | never |
//! |---|---|---|
//! | `portal-capture` (`session.rs`) | the portal session, its one signal stream, installs | releases |
//! | `portal-capture-eis` | the `reis` receiver; feeds the machine | calls the portal |
//! | `portal-capture-shutdown` | `Release`, the abort path, on a clone of the same zbus connection | waits for the worker |
//! | `portal-capture-cursor` (only with a `cursor` hook) | the cursor hook | |
//!
//! All state transitions happen in the pure machine of `pressure.rs` behind **one** mutex
//! (`Core`); the machine returns what to emit and what to do, and `Shared::apply` does it while
//! the lock is held, so events reach the subscriber in the order the machine decided them. The
//! subscriber's sink must not block (the agent's is a channel send).
//!
//! # Abort
//!
//! [`CaptureAbort::abort`] is an atomic epoch bump and an eventfd write, nothing else. The shutdown
//! thread wakes, reads the live activation from atomics (no lock), issues `Release` within a few
//! milliseconds, then emits `Ended { Aborted }` through the machine when it can take the lock. It
//! never drops the EIS fd: mutter keeps consuming input after an EIS-only disconnect.
//!
//! # Thread death
//!
//! Every backend thread body runs under `catch_unwind`. A panic must never leave the compositor
//! holding input for a backend that no longer serves it (the EIS thread's death even closes the
//! fd, which does not release anything): the panicking thread logs at error (the thread name, no
//! data), bumps the abort epoch (the shutdown thread releases and reports `Ended { Aborted }`; if
//! it is the one that died, the handler does that itself) and waits for that for up to 40 ms,
//! sets `closing`, closes the portal session within about 30 ms (which ends any activation on
//! the compositor's side, whatever the release did), reports `Closed`, and stops the EIS
//! thread. The backend is not restarted.
//!
//! # Logging
//!
//! Counts only. Key codes, button codes and typed data are never logged, and `REIS_DEBUG` (which
//! makes `reis` print them) makes [`PortalInputCapture::new`] and the receiver refuse.

mod barriers;
#[cfg(test)]
mod fake_eis;
#[cfg(test)]
mod fake_portal;
mod pressure;
mod receiver;
mod session;
mod sleep;
mod token;

use std::collections::VecDeque;
use std::fmt;
use std::os::fd::OwnedFd;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, TryLockError};
use std::task::Waker;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ashpd::desktop::Session;
use ashpd::desktop::input_capture::{InputCapture as PortalProxy, ReleaseOptions};
use crosspane_platform::{
    CaptureAbort, CaptureEvent, CaptureId, CapturePortal, CaptureStart, EventSink, InputCapture,
    IoGate, PlatformError, PortalId,
};
use crosspane_types::geom::PointDevice;
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::id::DisplayId;
use crosspane_types::input::LockKeys;
use crosspane_types::time::MonoTime;
use rustix::event::{EventfdFlags, PollFd, PollFlags, Timespec, poll};

use self::barriers::{Plan, Zone};
use self::pressure::{Machine, Out};
use self::receiver::{Devices, Receiver, Tagged};
use super::eis::DisplaysFn;
use crate::hyprland::capture::PORTALS_REJECTED;

/// `begin` and `end` give up waiting for the release thread after this (the input-path budget is
/// 50 ms).
const CALL_BUDGET: Duration = Duration::from_millis(40);
/// `set_portals` waits this long for the worker to install the set; after that it returns `Ok` and
/// the install continues.
const APPLY_WAIT: Duration = Duration::from_millis(40);
/// How often the pending or active activation is checked against the gate.
const TICK: Duration = Duration::from_millis(10);
/// How long the receiver's handshake may take.
const HANDSHAKE_BOUND: Duration = Duration::from_secs(2);
/// How long the worker waits for the receiver to attach and bind.
const ATTACH_BOUND: Duration = Duration::from_millis(2500);
/// One `Release` call may take this long.
const RELEASE_BOUND: Duration = Duration::from_millis(300);
/// The abort path's `Release` may take this long (the total budget is 50 ms).
const ABORT_RELEASE_BOUND: Duration = Duration::from_millis(30);
/// The abort path waits this long for the machine's lock to report `Ended`.
const ABORT_FENCE: Duration = Duration::from_millis(12);
/// The backend's thread names (also what a panic report says).
const WORKER_THREAD: &str = "portal-capture";
const ABORT_THREAD: &str = "portal-capture-shutdown";
const CALLER_THREAD: &str = "portal-capture-release";
const CURSOR_THREAD: &str = "portal-capture-cursor";
const EIS_THREAD: &str = "portal-capture-eis";
/// A thread that died waits at most this long for the shutdown thread to release the activation
/// before it closes the portal session.
const DEATH_ABORT_WAIT: Duration = Duration::from_millis(40);
/// A thread that died closes the portal session within this long.
const DEATH_CLOSE_BOUND: Duration = Duration::from_millis(30);
/// Dropping the handle waits this long for the abort to be acted on before closing the session.
const ABORT_SETTLE: Duration = Duration::from_millis(150);
/// Dropping the handle waits this long for the threads.
const JOIN_BOUND: Duration = Duration::from_secs(2);

/// The release of a key or button that was already down when a capture activation began, to be
/// replayed locally ([`InputCaptureConfig::local_release`]). Releases only: nothing here ever
/// presses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalUp {
    Key(HidUsage),
    Button(MouseButton),
}

/// What the backend needs from its host.
pub struct InputCaptureConfig {
    /// The displays snapshot (device-pixel to logical-pixel mapping, barrier planning).
    pub displays: DisplaysFn,
    /// The RemoteDesktop EIS sender's lock-key state (`EisSource::lock_keys`); `None` = unknown.
    pub lock_keys: Arc<dyn Fn() -> Option<LockKeys> + Send + Sync>,
    pub gate: Arc<IoGate>,
    /// Hide (true) / show (false) the local cursor during capture (Shell bridge v2 `InhibitCursor`).
    #[allow(clippy::type_complexity)]
    pub cursor: Option<Arc<dyn Fn(bool) -> Result<(), PlatformError> + Send + Sync>>,
    /// Replays the release of a key or button that was down before an activation and went up
    /// during it (the compositor swallowed that up), through the RemoteDesktop EIS sender
    /// (`EisSource::release_local`). Called on the backend's release thread, once per key or
    /// button, only after the compositor no longer holds the activation; it must not block for
    /// long and must not press anything. `None`: those keys stay held for the local clients.
    pub local_release: Option<Arc<dyn Fn(LocalUp) + Send + Sync>>,
    /// `<state_dir>/portal-input-capture.token`, used only when the portal is version >= 2.
    pub token_path: PathBuf,
    /// Desktop quirks (KDE: xdp-kde `Disable` bug, research §5).
    pub quirks: Quirks,
}

impl fmt::Debug for InputCaptureConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InputCaptureConfig")
            .field("token_path", &self.token_path)
            .field("quirks", &self.quirks)
            .field("cursor", &self.cursor.is_some())
            .field("local_release", &self.local_release.is_some())
            .finish_non_exhaustive()
    }
}

/// Deviations from GNOME's sequence for portals that differ (KDE sets both).
///
/// xdp-kde's `Disable` re-enables the session (KDE-v0 §4.3), and KWin disables the session when
/// barriers are set or the zones change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Quirks {
    /// Do not call `Disable` before `SetPointerBarriers`: the portal's `SetPointerBarriers`
    /// suspends the session itself, and `Disable` would re-enable it.
    pub disable_before_barriers: bool,
    /// Replace the barriers, or drop them, by closing the session and creating a new one (silent
    /// with a restore token) instead of `Disable`.
    pub close_to_replace: bool,
}

/// How the backend stands, reported to the status callback on every change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureStatus {
    /// No session yet (nothing wants capture), or between sessions.
    Idle,
    /// The session is being created; the desktop's consent dialog may be open.
    Pending,
    /// The session exists and its EIS connection is attached.
    Ready,
    /// The user said no. Not asked again until the agent restarts.
    Denied,
    /// The session was closed (the user stopped it in the desktop's indicator, or the portal went
    /// away). Not restarted on its own.
    Closed,
    /// No InputCapture portal, or it failed in a way another try would not mend.
    Unavailable,
}

// ---- plumbing --------------------------------------------------------------------------------

/// An eventfd a thread polls on, so it can be woken by anyone without a lock.
struct Wake(OwnedFd);

impl Wake {
    fn new() -> Result<Wake, PlatformError> {
        rustix::event::eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)
            .map(Wake)
            .map_err(|error| PlatformError::Backend(format!("cannot create a wake-up: {error}")))
    }

    fn wake(&self) {
        // A full counter or a closed fd leaves the thread to its next tick.
        let _ = rustix::io::write(&self.0, &1u64.to_ne_bytes());
    }

    fn drain(&self) {
        let mut counter = [0u8; 8];
        let _ = rustix::io::read(&self.0, &mut counter);
    }
}

/// A queue and the wake-up of the thread that serves it.
struct Mailbox<T> {
    queue: Mutex<VecDeque<T>>,
    wake: Wake,
}

impl<T> Mailbox<T> {
    fn new() -> Result<Mailbox<T>, PlatformError> {
        Ok(Mailbox {
            queue: Mutex::new(VecDeque::new()),
            wake: Wake::new()?,
        })
    }

    fn push(&self, item: T) {
        lock(&self.queue).push_back(item);
        self.wake.wake();
    }

    fn pop(&self) -> Option<T> {
        lock(&self.queue).pop_front()
    }

    fn is_empty(&self) -> bool {
        lock(&self.queue).is_empty()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `CLOCK_MONOTONIC` now: the clock every backend stamps events with.
fn mono_now() -> MonoTime {
    let time = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    MonoTime::from_nanos(
        u64::try_from(time.tv_sec)
            .unwrap_or(0)
            .saturating_mul(1_000_000_000)
            .saturating_add(u64::try_from(time.tv_nsec).unwrap_or(0)),
    )
}

fn timespec(duration: Duration) -> Timespec {
    Timespec {
        tv_sec: i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: i64::from(duration.subsec_nanos()),
    }
}

/// A `set_portals` refusal that leaves the previous set and any capture intact.
fn rejected(reason: impl fmt::Display) -> PlatformError {
    PlatformError::Backend(format!("{PORTALS_REJECTED}{reason}"))
}

/// The cursor hook of [`InputCaptureConfig::cursor`].
type CursorHook = Arc<dyn Fn(bool) -> Result<(), PlatformError> + Send + Sync>;

/// The portal session and the proxy it was made through, shared with the release thread.
struct PortalHandle {
    input_capture: Arc<PortalProxy>,
    session: Session<PortalProxy>,
}

/// Why a set could not be installed.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ApplyError {
    /// The set itself is unusable (the previous set is intact).
    Rejected(String),
    /// A portal call failed, and the previous set may be gone.
    Failed(String),
}

impl ApplyError {
    fn into_platform(self) -> PlatformError {
        match self {
            ApplyError::Rejected(reason) => rejected(reason),
            ApplyError::Failed(reason) => {
                PlatformError::Backend(format!("input capture: {reason}"))
            }
        }
    }
}

/// What the worker should look at.
struct CtlPoll {
    changed: bool,
    closing: bool,
    wanted_len: usize,
}

/// What a control change asks for.
struct Flags {
    closing: bool,
    restart: bool,
    rearm: bool,
    generation: u64,
}

#[derive(Default)]
struct CtlState {
    wanted: Vec<CapturePortal>,
    /// Bumped by every `set_portals`.
    generation: u64,
    /// Bumped by every change the worker should look at.
    version: u64,
    rearm: bool,
    restart: bool,
    closing: bool,
    applied: u64,
    applied_result: Option<Result<(), ApplyError>>,
    waker: Option<Waker>,
}

/// The requests the handles make of the worker.
struct Control {
    state: Mutex<CtlState>,
    applied: Condvar,
}

impl Control {
    fn new() -> Control {
        Control {
            state: Mutex::new(CtlState::default()),
            applied: Condvar::new(),
        }
    }

    fn bump(state: &mut CtlState) -> Option<Waker> {
        state.version += 1;
        state.waker.take()
    }

    /// The worker's poll: has anything changed since `seen`? Always remembers `waker`, so the next
    /// change wakes the worker whatever it does with this answer.
    fn poll(&self, seen: &std::cell::Cell<u64>, waker: &Waker) -> CtlPoll {
        let mut state = lock(&self.state);
        let changed = state.version != seen.get();
        seen.set(state.version);
        if !matches!(&state.waker, Some(old) if old.will_wake(waker)) {
            state.waker = Some(waker.clone());
        }
        CtlPoll {
            changed,
            closing: state.closing,
            wanted_len: state.wanted.len(),
        }
    }

    fn take_flags(&self) -> Flags {
        let mut state = lock(&self.state);
        Flags {
            closing: state.closing,
            restart: std::mem::take(&mut state.restart),
            rearm: std::mem::take(&mut state.rearm),
            generation: state.generation,
        }
    }

    fn is_closing(&self) -> bool {
        lock(&self.state).closing
    }

    fn wanted(&self) -> (u64, Vec<CapturePortal>) {
        let state = lock(&self.state);
        (state.generation, state.wanted.clone())
    }

    fn current_wanted(&self) -> Vec<CapturePortal> {
        lock(&self.state).wanted.clone()
    }

    /// A new wanted set. Returns its generation.
    fn submit(&self, portals: Vec<CapturePortal>) -> u64 {
        let (generation, waker) = {
            let mut state = lock(&self.state);
            state.wanted = portals;
            state.generation += 1;
            (state.generation, Self::bump(&mut state))
        };
        if let Some(waker) = waker {
            waker.wake();
        }
        generation
    }

    fn signal(&self, change: impl FnOnce(&mut CtlState)) {
        let waker = {
            let mut state = lock(&self.state);
            change(&mut state);
            Self::bump(&mut state)
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Something happened the worker may be waiting on (an activation ended).
    fn poke(&self) {
        self.signal(|_| {});
    }

    fn request_rearm(&self) {
        self.signal(|state| state.rearm = true);
    }

    fn request_restart(&self) {
        self.signal(|state| state.restart = true);
    }

    fn close(&self) {
        self.signal(|state| state.closing = true);
        self.applied.notify_all();
    }

    /// The worker's answer for `generation`.
    fn finish(&self, generation: u64, result: Result<(), ApplyError>) {
        let mut state = lock(&self.state);
        if generation >= state.applied {
            state.applied = generation;
            state.applied_result = Some(result);
        }
        drop(state);
        self.applied.notify_all();
    }

    /// Wait for the answer for `generation`, at most `budget`.
    fn wait_applied(&self, generation: u64, budget: Duration) -> Option<Result<(), ApplyError>> {
        let deadline = Instant::now() + budget;
        let mut state = lock(&self.state);
        loop {
            if state.applied >= generation {
                return state.applied_result.clone();
            }
            if state.closing {
                return None;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            state = self
                .applied
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// Delivers status changes in order.
struct Reporter {
    current: Mutex<CaptureStatus>,
    callback: Arc<dyn Fn(CaptureStatus) + Send + Sync>,
}

impl Reporter {
    fn set(&self, status: CaptureStatus) {
        let mut current = lock(&self.current);
        if *current == status {
            return;
        }
        *current = status;
        tracing::info!(?status, "input capture status");
        if catch_unwind(AssertUnwindSafe(|| (self.callback)(status))).is_err() {
            tracing::warn!("the input capture status callback panicked");
        }
    }

    fn get(&self) -> CaptureStatus {
        *lock(&self.current)
    }
}

/// The pending or active activation, readable without the machine's lock (the abort path).
#[derive(Default)]
struct Live {
    /// `1 << 32 | activation` while there is one, else 0.
    activation: AtomicU64,
    x: AtomicU64,
    y: AtomicU64,
}

impl Live {
    const PRESENT: u64 = 1 << 32;

    fn set(&self, live: Option<(u32, (f64, f64))>) {
        match live {
            Some((activation, (x, y))) => {
                self.x.store(x.to_bits(), Ordering::Release);
                self.y.store(y.to_bits(), Ordering::Release);
                self.activation
                    .store(Self::PRESENT | u64::from(activation), Ordering::Release);
            }
            None => self.activation.store(0, Ordering::Release),
        }
    }

    fn get(&self) -> Option<(u32, (f64, f64))> {
        let word = self.activation.load(Ordering::Acquire);
        (word & Self::PRESENT != 0).then(|| {
            (
                u32::try_from(word & u64::from(u32::MAX)).unwrap_or(0),
                (
                    f64::from_bits(self.x.load(Ordering::Acquire)),
                    f64::from_bits(self.y.load(Ordering::Acquire)),
                ),
            )
        })
    }
}

enum Request {
    Release {
        activation: u32,
        at: (f64, f64),
        /// Replayed locally once the portal answered the release (and not before).
        ups: Vec<LocalUp>,
        reply: Option<SyncSender<Result<(), PlatformError>>>,
    },
    /// The compositor ended the activation on its own: replay these at once.
    LocalUps(Vec<LocalUp>),
    Stop,
}

/// The `Release` the machine asked for that the abort path issues itself.
struct SkippedRelease {
    activation: u32,
    at: (f64, f64),
    ups: Vec<LocalUp>,
}

enum EisCmd {
    /// Handshake and bind on the portal's EIS socket; the answer is sent when the seat is bound.
    Attach {
        fd: OwnedFd,
        reply: SyncSender<Result<(), PlatformError>>,
    },
    Detach,
    Stop,
}

/// Everything the machine's owner needs, behind [`Shared::core`]'s mutex.
struct Core {
    machine: Machine,
    sink: Option<Arc<dyn EventSink<CaptureEvent>>>,
    /// The installed plan (activation mapping).
    plan: Option<Plan>,
    /// The zones of the last `GetZones` (empty before the first).
    zones: Vec<Zone>,
    zone_set: u32,
    /// The newest abort epoch the machine has acted on.
    handled_abort: u64,
}

struct Shared {
    displays: DisplaysFn,
    lock_keys: Arc<dyn Fn() -> Option<LockKeys> + Send + Sync>,
    gate: Arc<IoGate>,
    cursor: Option<CursorHook>,
    local_release: Option<Arc<dyn Fn(LocalUp) + Send + Sync>>,
    token_path: PathBuf,
    quirks: Quirks,
    /// Test only: the thread that panics at its next pass (see [`Shared::panic_point`]).
    #[cfg(test)]
    panic_in: Mutex<Option<&'static str>>,
    /// `None`: the session bus. Tests give a private bus.
    bus_address: Option<String>,
    core: Mutex<Core>,
    control: Control,
    status: Reporter,
    handle: Mutex<Option<Arc<PortalHandle>>>,
    caller: Mailbox<Request>,
    eis: Mailbox<EisCmd>,
    live: Live,
    /// Bumped by `CaptureAbort::abort`; the shutdown thread's wake-up.
    abort_epoch: AtomicU64,
    /// The newest abort epoch the shutdown thread has finished acting on.
    abort_done: AtomicU64,
    abort_wake: Wake,
    /// Whether the cursor should be hidden, and the cursor thread's wake-up.
    cursor_hidden: AtomicBool,
    cursor_wake: Wake,
    /// Which devices the compositor announced (bit 0 pointer, bit 1 keyboard).
    devices: AtomicU8,
    /// The session is enabled with barriers installed.
    enabled: AtomicBool,
    ready: AtomicBool,
    closing: AtomicBool,
    worker_exited: Mutex<bool>,
    worker_exited_cv: Condvar,
}

/// Marks the worker as exited, even by a panic.
struct WorkerExit(Arc<Shared>);

impl Drop for WorkerExit {
    fn drop(&mut self) {
        *lock(&self.0.worker_exited) = true;
        self.0.worker_exited_cv.notify_all();
    }
}

impl Shared {
    fn core(&self) -> MutexGuard<'_, Core> {
        lock(&self.core)
    }

    /// Do what the machine asked, in order, with the machine's lock held.
    fn apply(&self, core: &mut Core, outs: Vec<Out>, release: bool) {
        let mut poke = false;
        for out in outs {
            match out {
                Out::Emit(event) => {
                    match &event {
                        CaptureEvent::Started { .. } => self.want_cursor_hidden(true),
                        CaptureEvent::Ended { .. } => {
                            self.want_cursor_hidden(false);
                            poke = true;
                        }
                        _ => {}
                    }
                    if let Some(sink) = &core.sink {
                        sink.send(event);
                    }
                }
                Out::Release {
                    activation,
                    at,
                    ups,
                } => {
                    // Without `release` the caller issues the `Release` itself and takes the ups
                    // along (the abort path).
                    if release {
                        self.caller.push(Request::Release {
                            activation,
                            at,
                            ups,
                            reply: None,
                        });
                    }
                    poke = true;
                }
                Out::LocalUps(ups) => {
                    if release {
                        self.caller.push(Request::LocalUps(ups));
                    }
                }
                Out::ReleaseUnknown => {
                    if release {
                        self.release_unknown();
                    }
                    poke = true;
                }
                Out::Rearm => self.control.request_rearm(),
            }
        }
        self.live.set(core.machine.live());
        if poke {
            self.control.poke();
        }
    }

    fn want_cursor_hidden(&self, hidden: bool) {
        if self.cursor.is_some() && self.cursor_hidden.swap(hidden, Ordering::AcqRel) != hidden {
            self.cursor_wake.wake();
        }
    }

    /// Whether the gate permits capture and somebody listens.
    fn accepting(&self, core: &Core) -> bool {
        self.gate.is_open() && core.sink.is_some()
    }

    fn machine_idle(&self) -> bool {
        self.core().machine.is_idle()
    }

    fn busy(&self) -> bool {
        self.core().machine.busy()
    }

    // ---- the portal's signals (worker thread) --------------------------------------------

    fn activated(&self, activation: Option<u32>, barrier: Option<u32>, cursor: Option<(f32, f32)>) {
        let now = mono_now();
        let cursor = cursor
            .map(|(x, y)| (f64::from(x), f64::from(y)))
            .filter(|(x, y)| x.is_finite() && y.is_finite())
            .unwrap_or((f64::NAN, f64::NAN));
        let mut core = self.core();
        let accept = self.accepting(&core);
        let found = core
            .plan
            .as_ref()
            .and_then(|plan| plan.activation(barrier, cursor));
        let outs = core
            .machine
            .activated(now, activation.unwrap_or(0), found, cursor, accept);
        self.apply(&mut core, outs, true);
        drop(core);
        // The EIS thread looks at the gate and the timeout every few milliseconds from now on.
        self.eis.wake.wake();
    }

    fn deactivated(&self, activation: Option<u32>) {
        let now = mono_now();
        let mut core = self.core();
        let activation = activation
            .or_else(|| core.machine.live().map(|(id, _)| id))
            .unwrap_or(0);
        let outs = core.machine.deactivated(now, activation);
        self.apply(&mut core, outs, true);
    }

    /// The compositor disabled the session (zones changed, `Disabled`): nothing is held, and the
    /// barriers are gone from its side.
    fn session_disabled(&self) {
        let now = mono_now();
        let mut core = self.core();
        let outs = core.machine.lost(now);
        core.plan = None;
        self.apply(&mut core, outs, true);
        drop(core);
        self.enabled.store(false, Ordering::Release);
        self.recompute_ready();
    }

    /// The session is over: everything about it is forgotten.
    fn session_ended(&self) {
        let now = mono_now();
        let mut core = self.core();
        let outs = core.machine.lost(now);
        tracing::debug!(
            dropped = core.machine.dropped(),
            "the input capture session ended"
        );
        core.machine.reset();
        core.plan = None;
        core.zones.clear();
        self.apply(&mut core, outs, true);
        drop(core);
        self.enabled.store(false, Ordering::Release);
        self.recompute_ready();
    }

    fn set_zones(&self, zones: Vec<Zone>, zone_set: u32) {
        let mut core = self.core();
        core.zones = zones;
        core.zone_set = zone_set;
    }

    fn set_installed(&self, plan: Option<Plan>, enabled: bool) {
        self.core().plan = plan;
        self.enabled.store(enabled, Ordering::Release);
        self.recompute_ready();
    }

    fn set_handle(&self, handle: Option<Arc<PortalHandle>>) {
        *lock(&self.handle) = handle;
    }

    // ---- the EIS thread ---------------------------------------------------------------------

    fn feed(&self, inputs: Vec<Tagged>) {
        let now = mono_now();
        let mut core = self.core();
        if !self.accepting(&core) {
            // The gate is closed: nothing captured may be routed, and what is held is released.
            let outs = core.machine.tick(now, false);
            if !outs.is_empty() {
                self.apply(&mut core, outs, true);
            }
            return;
        }
        for Tagged { seq, input } in inputs {
            let outs = core.machine.input(now, seq, input);
            if !outs.is_empty() {
                self.apply(&mut core, outs, true);
            }
        }
    }

    /// Gate and timeout checks, and the abort fence. Called by the EIS thread every few
    /// milliseconds while something is held, and on every wake-up.
    fn tick(&self) {
        let mut core = self.core();
        let _ = self.abort_locked(&mut core, true);
        if core.machine.busy() {
            let accept = self.accepting(&core);
            let outs = core.machine.tick(mono_now(), accept);
            if !outs.is_empty() {
                self.apply(&mut core, outs, true);
            }
        }
    }

    fn set_devices(&self, devices: Devices) {
        let bits = u8::from(devices.pointer) | (u8::from(devices.keyboard) << 1);
        if self.devices.swap(bits, Ordering::AcqRel) != bits {
            self.recompute_ready();
        }
    }

    fn recompute_ready(&self) {
        let devices = self.devices.load(Ordering::Acquire);
        self.ready.store(
            self.enabled.load(Ordering::Acquire) && devices == 0b11,
            Ordering::Release,
        );
    }

    /// The EIS connection died: nothing can be received, and an activation would stay held
    /// (mutter keeps consuming input after an EIS-only disconnect), so release it.
    fn eis_lost(&self) {
        tracing::warn!("the capture EIS connection was lost");
        self.set_devices(Devices::default());
        let now = mono_now();
        let mut core = self.core();
        let outs = core.machine.portal_gone(now, |_| false);
        self.apply(&mut core, outs, true);
        drop(core);
        self.control.request_restart();
    }

    // ---- aborting -------------------------------------------------------------------------

    /// Act on an abort the machine has not seen yet. `release`: also queue the `Release` (the
    /// shutdown thread issues it itself and passes `false`; the `Release` the machine asked for
    /// is then returned, so one for an activation that appeared after the shutdown thread looked
    /// is not lost).
    fn abort_locked(&self, core: &mut Core, release: bool) -> Option<SkippedRelease> {
        let epoch = self.abort_epoch.load(Ordering::Acquire);
        if core.handled_abort >= epoch {
            return None;
        }
        core.handled_abort = epoch;
        let outs = core.machine.abort(mono_now());
        let skipped = outs.iter().find_map(|out| match out {
            Out::Release {
                activation,
                at,
                ups,
            } if !release => Some(SkippedRelease {
                activation: *activation,
                at: *at,
                ups: ups.clone(),
            }),
            _ => None,
        });
        self.apply(core, outs, release);
        skipped
    }

    /// The abort path's second step: tell the machine, if its lock can be had in time. The EIS
    /// thread does the same on its next pass when it cannot. `None`: the lock was busy.
    fn fence_abort(&self, wait: Duration) -> Option<Option<SkippedRelease>> {
        let deadline = Instant::now() + wait;
        loop {
            match self.core.try_lock() {
                Ok(mut core) => return Some(self.abort_locked(&mut core, false)),
                Err(TryLockError::Poisoned(poisoned)) => {
                    return Some(self.abort_locked(&mut poisoned.into_inner(), false));
                }
                Err(TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    thread::sleep(Duration::from_micros(500));
                }
            }
        }
    }

    fn run_abort(&self) {
        // Straight from the atomics: no lock a stuck thread could hold.
        let mut releases: Vec<(u32, (f64, f64))> = self.live.get().into_iter().collect();
        // The local ups of the activation(s) released here, replayed after every release was
        // answered and not before (the compositor would capture them).
        let mut ups: Vec<LocalUp> = Vec::new();
        let mut released_all = true;
        // Tell the machine first when its lock is free (it usually is): it then knows the
        // `Deactivated` that our `Release` causes is expected, not a loss.
        let fence = self.fence_abort(Duration::from_millis(3));
        let fenced = fence.is_some();
        let note = |skipped: Option<SkippedRelease>,
                    releases: &mut Vec<(u32, (f64, f64))>,
                    ups: &mut Vec<LocalUp>| {
            if let Some(skipped) = skipped {
                if !releases.iter().any(|(id, _)| *id == skipped.activation) {
                    releases.push((skipped.activation, skipped.at));
                }
                ups.extend(skipped.ups);
            }
        };
        note(fence.flatten(), &mut releases, &mut ups);
        for (activation, at) in std::mem::take(&mut releases) {
            if let Err(error) = self.portal_release(activation, at, ABORT_RELEASE_BOUND) {
                released_all = false;
                tracing::warn!(%error, "releasing the capture on abort failed");
            }
        }
        let mut fenced_late = false;
        if !fenced {
            let late = self.fence_abort(ABORT_FENCE);
            fenced_late = late.is_some();
            note(late.flatten(), &mut releases, &mut ups);
            for (activation, at) in releases {
                if self
                    .portal_release(activation, at, ABORT_RELEASE_BOUND)
                    .is_err()
                {
                    released_all = false;
                }
            }
        }
        if !fenced && !fenced_late {
            tracing::warn!("the capture state is busy; the EIS thread will report the abort");
        }
        if released_all {
            self.replay_ups(ups);
        } else if !ups.is_empty() {
            tracing::warn!(
                count = ups.len(),
                "the release failed; not replaying the held keys' releases"
            );
        }
    }

    /// Replay the releases of keys and buttons that were down before an activation (see
    /// [`InputCaptureConfig::local_release`]). Only counts are logged.
    fn replay_ups(&self, ups: Vec<LocalUp>) {
        if ups.is_empty() {
            return;
        }
        let Some(hook) = &self.local_release else {
            tracing::debug!(count = ups.len(), "no hook to replay the held releases");
            return;
        };
        tracing::info!(
            count = ups.len(),
            "replaying releases of keys and buttons held before the capture"
        );
        for up in ups {
            if catch_unwind(AssertUnwindSafe(|| hook(up))).is_err() {
                tracing::warn!("the local release hook panicked");
            }
        }
    }

    // ---- the portal ------------------------------------------------------------------------

    /// `Release(activation, cursor_position)`, bounded. A portal that answers with an error means
    /// the activation is not held (any more); only a missing answer is a failure.
    fn portal_release(
        &self,
        activation: u32,
        at: (f64, f64),
        bound: Duration,
    ) -> Result<(), PlatformError> {
        let Some(handle) = lock(&self.handle).clone() else {
            // No session: nothing is held.
            return Ok(());
        };
        let mut options = ReleaseOptions::default();
        if activation != 0 {
            options = options.set_activation_id(activation);
        }
        if at.0.is_finite() && at.1.is_finite() {
            options = options.set_cursor_position(at);
        }
        let call = handle.input_capture.release(&handle.session, options);
        match zbus::block_on(sleep::timeout(call, bound)) {
            Some(Ok(())) => Ok(()),
            Some(Err(error)) if answered(&error) => {
                tracing::debug!(%error, "Release was answered with an error");
                Ok(())
            }
            Some(Err(error)) => Err(PlatformError::Backend(format!("Release: {error}"))),
            None => Err(PlatformError::Timeout),
        }
    }

    /// An `Activated` signal that cannot be read: the compositor holds the pointer and input for
    /// an activation we know nothing about, so release whatever is held (no id, no position).
    fn release_unknown(&self) {
        self.caller.push(Request::Release {
            activation: 0,
            at: (f64::NAN, f64::NAN),
            ups: Vec::new(),
            reply: None,
        });
    }

    /// Test only: panic here when a test asked this thread to (the thread-death tests).
    #[cfg(test)]
    fn panic_point(&self, thread: &'static str) {
        let mut slot = lock(&self.panic_in);
        if *slot == Some(thread) {
            *slot = None;
            drop(slot);
            panic!("test: {thread} is made to panic");
        }
    }

    #[cfg(not(test))]
    #[inline(always)]
    fn panic_point(&self, _thread: &'static str) {}

    /// A backend thread panicked. The compositor may be holding input for a backend that no
    /// longer serves it, and the death of the EIS thread even closes the fd (which releases
    /// nothing), so: have the activation released (the shutdown thread; this thread itself when
    /// it was the shutdown thread), stop everything, close the portal session (which ends any
    /// activation on the compositor's side) within [`DEATH_CLOSE_BOUND`], and report `Closed`.
    /// Never logs anything about the input.
    fn thread_died(&self, thread: &'static str) {
        tracing::error!(
            thread,
            "an input capture thread panicked; giving the input back and closing the session"
        );
        let epoch = self.abort_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        self.abort_wake.wake();
        if thread == ABORT_THREAD {
            if catch_unwind(AssertUnwindSafe(|| self.run_abort())).is_err() {
                tracing::error!("the emergency release panicked as well");
            }
        } else {
            // The release goes first (the worker would drop the portal handle it uses as soon as
            // it sees `closing`), for as long as it takes the shutdown thread, which is a few
            // milliseconds.
            let wait = Instant::now() + DEATH_ABORT_WAIT;
            while self.abort_done.load(Ordering::Acquire) < epoch && Instant::now() < wait {
                thread::sleep(Duration::from_micros(500));
            }
        }
        self.closing.store(true, Ordering::Release);
        self.control.close();
        // The activation's end on the compositor's side does not depend on anything else.
        if let Some(handle) = lock(&self.handle).clone() {
            match zbus::block_on(sleep::timeout(handle.session.close(), DEATH_CLOSE_BOUND)) {
                Some(Ok(())) => {}
                Some(Err(error)) => tracing::debug!(%error, "closing the capture session failed"),
                None => tracing::warn!("closing the capture session timed out"),
            }
        }
        self.enabled.store(false, Ordering::Release);
        self.recompute_ready();
        self.status.set(CaptureStatus::Closed);
        self.cursor_wake.wake();
        self.caller.push(Request::Stop);
        self.eis.push(EisCmd::Stop);
        // The cursor thread shows the cursor again on its way out; when it is the one that died
        // nobody would, and the cursor would stay hidden (best effort).
        if thread == CURSOR_THREAD
            && let Some(hook) = &self.cursor
            && catch_unwind(AssertUnwindSafe(|| hook(false))).is_err()
        {
            tracing::warn!("the cursor hook panicked");
        }
    }

    fn release_failed(&self) {
        tracing::warn!("a capture activation could not be released; resetting the session state");
        self.control.request_rearm();
    }
}

/// Whether the portal answered a call with an error (so the request was processed) rather than
/// not answering at all.
fn answered(error: &ashpd::Error) -> bool {
    use ashpd::PortalError;
    match error {
        ashpd::Error::Portal(PortalError::ZBus(error)) | ashpd::Error::Zbus(error) => match error {
            zbus::Error::MethodError(..) => true,
            zbus::Error::FDO(error) => !matches!(
                **error,
                zbus::fdo::Error::ServiceUnknown(_)
                    | zbus::fdo::Error::NameHasNoOwner(_)
                    | zbus::fdo::Error::NoReply(_)
                    | zbus::fdo::Error::Disconnected(_)
                    | zbus::fdo::Error::TimedOut(_)
                    | zbus::fdo::Error::IOError(_)
            ),
            _ => false,
        },
        ashpd::Error::Portal(_) => true,
        _ => false,
    }
}

// ---- the threads -----------------------------------------------------------------------------

/// The release thread: the queue of `Release` requests (a slow portal delays only this queue; the
/// abort path has a thread of its own).
fn caller_main(shared: &Shared) {
    loop {
        shared.panic_point(CALLER_THREAD);
        let mut fds = [PollFd::new(&shared.caller.wake.0, PollFlags::IN)];
        let _ = poll(&mut fds, Some(&timespec(Duration::from_millis(250))));
        shared.caller.wake.drain();
        while let Some(request) = shared.caller.pop() {
            match request {
                Request::Release {
                    activation,
                    at,
                    ups,
                    reply,
                } => {
                    let result = shared.portal_release(activation, at, RELEASE_BOUND);
                    if result.is_err() {
                        shared.release_failed();
                    }
                    if let Some(reply) = reply {
                        let report = match &result {
                            Ok(()) => Ok(()),
                            Err(PlatformError::Timeout) => Err(PlatformError::Timeout),
                            Err(error) => Err(PlatformError::Backend(error.to_string())),
                        };
                        let _ = reply.try_send(report);
                    }
                    // After the answer (the caller of `end` does not wait for the replay), and
                    // only when the portal let go: an up injected while the compositor still
                    // holds the activation would be captured too.
                    if result.is_ok() {
                        shared.replay_ups(ups);
                    } else if !ups.is_empty() {
                        tracing::warn!(
                            count = ups.len(),
                            "the release failed; not replaying the held keys' releases"
                        );
                    }
                }
                Request::LocalUps(ups) => shared.replay_ups(ups),
                Request::Stop => return,
            }
        }
        if shared.closing.load(Ordering::Acquire) && shared.caller.is_empty() {
            return;
        }
    }
}

/// The shutdown thread: on an abort it releases the activation through the same bus connection
/// within a few milliseconds and tells the machine. It waits for nothing else, so a slow release
/// queue or a stuck EIS thread cannot delay it.
fn abort_main(shared: &Shared) {
    let mut handled = 0u64;
    loop {
        let mut fds = [PollFd::new(&shared.abort_wake.0, PollFlags::IN)];
        let _ = poll(&mut fds, Some(&timespec(Duration::from_millis(250))));
        shared.abort_wake.drain();
        shared.panic_point(ABORT_THREAD);
        let epoch = shared.abort_epoch.load(Ordering::Acquire);
        if epoch != handled {
            handled = epoch;
            shared.run_abort();
            shared.abort_done.fetch_max(epoch, Ordering::AcqRel);
        }
        if shared.closing.load(Ordering::Acquire) {
            return;
        }
    }
}

/// The EIS thread: the receiver, the machine's input, the gate.
fn eis_main(shared: &Shared) {
    let mut receiver: Option<Receiver> = None;
    loop {
        shared.eis.wake.drain();
        shared.panic_point(EIS_THREAD);
        while let Some(command) = shared.eis.pop() {
            match command {
                EisCmd::Attach { fd, reply } => {
                    receiver = None;
                    shared.set_devices(Devices::default());
                    let attached = Receiver::connect(fd, HANDSHAKE_BOUND).and_then(|mut r| {
                        r.wait_bound(HANDSHAKE_BOUND)?;
                        Ok(r)
                    });
                    let _ = reply.try_send(match attached {
                        Ok(r) => {
                            receiver = Some(r);
                            Ok(())
                        }
                        Err(error) => Err(error),
                    });
                }
                EisCmd::Detach => {
                    receiver = None;
                    shared.set_devices(Devices::default());
                }
                EisCmd::Stop => return,
            }
        }
        if let Some(r) = receiver.as_mut() {
            let inputs = r.pump();
            if !inputs.is_empty() {
                shared.feed(inputs);
            }
            shared.set_devices(r.devices());
            if r.is_dead() {
                receiver = None;
                shared.eis_lost();
            }
        }
        shared.tick();

        let timeout = if shared.busy() {
            TICK
        } else {
            Duration::from_millis(250)
        };
        let timeout = timespec(timeout);
        match receiver.as_ref() {
            Some(r) => {
                let mut fds = [
                    PollFd::new(&shared.eis.wake.0, PollFlags::IN),
                    PollFd::new(r.context(), PollFlags::IN),
                ];
                let _ = poll(&mut fds, Some(&timeout));
            }
            None => {
                let mut fds = [PollFd::new(&shared.eis.wake.0, PollFlags::IN)];
                let _ = poll(&mut fds, Some(&timeout));
            }
        }
    }
}

/// How often a failing cursor hook is tried again for one wish (every 500 ms).
const CURSOR_ATTEMPTS: u32 = 8;

/// The cursor thread: applies the latest wish of the hook (A5), best effort. A call that fails is
/// tried again a few times, so a missed "show" does not leave the cursor hidden.
fn cursor_main(shared: &Shared) {
    let Some(hook) = shared.cursor.clone() else {
        return;
    };
    let mut applied = false;
    let mut wished = false;
    let mut attempts = 0u32;
    loop {
        let mut fds = [PollFd::new(&shared.cursor_wake.0, PollFlags::IN)];
        let _ = poll(&mut fds, Some(&timespec(Duration::from_millis(500))));
        shared.cursor_wake.drain();
        shared.panic_point(CURSOR_THREAD);
        let closing = shared.closing.load(Ordering::Acquire);
        let want = shared.cursor_hidden.load(Ordering::Acquire) && !closing;
        if want != wished {
            wished = want;
            attempts = 0;
        }
        if want != applied && (attempts < CURSOR_ATTEMPTS || closing) {
            attempts += 1;
            match catch_unwind(AssertUnwindSafe(|| hook(want))) {
                Ok(Ok(())) => applied = want,
                Ok(Err(error)) => {
                    if attempts == 1 {
                        tracing::debug!(%error, "the cursor hook failed (best effort)");
                    }
                }
                Err(_) => tracing::warn!("the cursor hook panicked"),
            }
        }
        if closing {
            return;
        }
    }
}

// ---- the public handle -----------------------------------------------------------------------

/// Controller capture through the InputCapture portal.
pub struct PortalInputCapture {
    shared: Arc<Shared>,
    threads: Vec<(&'static str, JoinHandle<()>)>,
    subscribed: bool,
}

impl fmt::Debug for PortalInputCapture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PortalInputCapture")
            .field("status", &self.shared.status.get())
            .finish_non_exhaustive()
    }
}

impl PortalInputCapture {
    /// Spawns the worker threads; never blocks on consent (the session is created on the first
    /// non-empty `set_portals`). `status` is called on worker threads, must not block.
    pub fn new(
        config: InputCaptureConfig,
        status: Arc<dyn Fn(CaptureStatus) + Send + Sync>,
    ) -> Result<PortalInputCapture, PlatformError> {
        Self::spawn_on(config, status, None)
    }

    /// [`new`](Self::new) on a given bus address instead of the session bus (tests).
    fn spawn_on(
        config: InputCaptureConfig,
        status: Arc<dyn Fn(CaptureStatus) + Send + Sync>,
        bus_address: Option<String>,
    ) -> Result<PortalInputCapture, PlatformError> {
        // reis prints every message, key codes included, when this is set.
        if std::env::var_os("REIS_DEBUG").is_some_and(|value| !value.is_empty()) {
            return Err(PlatformError::Backend(
                "REIS_DEBUG is set; refusing to capture input (it would log key codes)".into(),
            ));
        }
        let shared = Arc::new(Shared {
            displays: config.displays,
            lock_keys: config.lock_keys,
            gate: config.gate,
            cursor: config.cursor,
            local_release: config.local_release,
            token_path: config.token_path,
            quirks: config.quirks,
            #[cfg(test)]
            panic_in: Mutex::new(None),
            bus_address,
            core: Mutex::new(Core {
                machine: Machine::new(),
                sink: None,
                plan: None,
                zones: Vec::new(),
                zone_set: 0,
                handled_abort: 0,
            }),
            control: Control::new(),
            status: Reporter {
                current: Mutex::new(CaptureStatus::Idle),
                callback: status,
            },
            handle: Mutex::new(None),
            caller: Mailbox::new()?,
            eis: Mailbox::new()?,
            live: Live::default(),
            abort_epoch: AtomicU64::new(0),
            abort_done: AtomicU64::new(0),
            abort_wake: Wake::new()?,
            cursor_hidden: AtomicBool::new(false),
            cursor_wake: Wake::new()?,
            devices: AtomicU8::new(0),
            enabled: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            worker_exited: Mutex::new(false),
            worker_exited_cv: Condvar::new(),
        });
        let mut capture = PortalInputCapture {
            shared: Arc::clone(&shared),
            threads: Vec::new(),
            subscribed: false,
        };
        // Every body runs under `catch_unwind`: a thread that dies must give the input back.
        let spawn = |name: &'static str, body: Box<dyn FnOnce() + Send>| {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name(name.to_owned())
                .spawn(move || {
                    if catch_unwind(AssertUnwindSafe(body)).is_err() {
                        shared.thread_died(name);
                    }
                })
                .map(|handle| (name, handle))
                .map_err(|error| PlatformError::Backend(format!("cannot start {name}: {error}")))
        };
        let worker = session::Worker::new(Arc::clone(&shared));
        capture
            .threads
            .push(spawn(WORKER_THREAD, Box::new(move || worker.run()))?);
        let s = Arc::clone(&shared);
        capture
            .threads
            .push(spawn(ABORT_THREAD, Box::new(move || abort_main(&s)))?);
        let s = Arc::clone(&shared);
        capture
            .threads
            .push(spawn(CALLER_THREAD, Box::new(move || caller_main(&s)))?);
        if shared.cursor.is_some() {
            let s = Arc::clone(&shared);
            capture
                .threads
                .push(spawn(CURSOR_THREAD, Box::new(move || cursor_main(&s)))?);
        }
        // Last: it is the last to stop (see `Drop`).
        let s = Arc::clone(&shared);
        capture
            .threads
            .push(spawn(EIS_THREAD, Box::new(move || eis_main(&s)))?);
        Ok(capture)
    }

    /// Whether the session is enabled with EIS devices present (controller readiness).
    pub fn is_ready(&self) -> bool {
        self.shared.ready.load(Ordering::Acquire)
    }

    /// The current status.
    #[cfg(test)]
    fn status(&self) -> CaptureStatus {
        self.shared.status.get()
    }
}

/// Zones as the displays say them, for validating a set before the portal has told the real ones.
fn zones_from_displays(displays: &[crosspane_types::display::DisplayInfo]) -> Vec<Zone> {
    displays
        .iter()
        .filter_map(|display| {
            let size = display.geometry.logical_size();
            let origin = display.geometry.logical_origin;
            let to_i32 = |value: f64| (value.is_finite()).then(|| value.round() as i32);
            Some(Zone {
                x: to_i32(origin.x)?,
                y: to_i32(origin.y)?,
                width: u32::try_from(to_i32(size.width)?).ok()?,
                height: u32::try_from(to_i32(size.height)?).ok()?,
            })
        })
        .collect()
}

impl InputCapture for PortalInputCapture {
    fn set_portals(&mut self, portals: &[CapturePortal]) -> Result<(), PlatformError> {
        let shared = &self.shared;
        match shared.status.get() {
            CaptureStatus::Denied if !portals.is_empty() => {
                return Err(rejected("input capture was not allowed on this desktop"));
            }
            CaptureStatus::Closed if !portals.is_empty() => {
                return Err(rejected("the input capture session was closed"));
            }
            CaptureStatus::Unavailable if !portals.is_empty() => {
                return Err(rejected("input capture is not available on this desktop"));
            }
            _ => {}
        }
        // Validate with the same rules the compositor applies, against its zones once known and
        // the displays' own rectangles before that. The previous set stays when this fails.
        if !portals.is_empty() {
            let displays = (shared.displays)();
            let (zones, zone_set) = {
                let core = shared.core();
                (core.zones.clone(), core.zone_set)
            };
            let zones = if zones.is_empty() {
                zones_from_displays(&displays)
            } else {
                zones
            };
            barriers::plan(portals, &displays, &zones, zone_set).map_err(rejected)?;
        }
        let previous = shared.control.current_wanted();
        if previous == portals && (portals.is_empty() || shared.enabled.load(Ordering::Acquire)) {
            // The engine offers an identical set again whenever its portal mapping changes: it is
            // installed already, and installing it again would take the barriers down meanwhile.
            return Ok(());
        }
        // A7: an activation on a portal that is gone or different is over before the new set.
        {
            let now = mono_now();
            let mut core = shared.core();
            let outs = core.machine.portal_gone(now, |id| {
                portals.iter().any(|p| p.id == id && previous.contains(p))
            });
            if !outs.is_empty() {
                shared.apply(&mut core, outs, true);
            }
        }
        let generation = shared.control.submit(portals.to_vec());
        if shared.status.get() != CaptureStatus::Ready {
            // The session is not there yet (or the consent dialog is open): the set is installed
            // as soon as it is.
            return Ok(());
        }
        match shared.control.wait_applied(generation, APPLY_WAIT) {
            Some(Err(error)) => Err(error.into_platform()),
            Some(Ok(())) | None => Ok(()),
        }
    }

    fn subscribe(&mut self, sink: Arc<dyn EventSink<CaptureEvent>>) -> Result<(), PlatformError> {
        if self.subscribed {
            return Err(PlatformError::Backend("already subscribed".into()));
        }
        self.subscribed = true;
        let locks = (self.shared.lock_keys)();
        let mut core = self.shared.core();
        if let Some(locks) = locks
            && (locks.caps_lock.is_some()
                || locks.num_lock.is_some()
                || locks.scroll_lock.is_some())
        {
            sink.send(CaptureEvent::LockKeys(locks));
        }
        sink.send(CaptureEvent::KeyboardBlinded(false));
        core.sink = Some(sink);
        Ok(())
    }

    fn begin(&mut self, id: CaptureId, portal: PortalId) -> Result<CaptureStart, PlatformError> {
        let shared = &self.shared;
        let locks = (shared.lock_keys)().unwrap_or_default();
        let now = mono_now();
        let mut core = shared.core();
        if core.sink.is_none() {
            return Err(PlatformError::Backend("not subscribed".into()));
        }
        let begin = core
            .machine
            .begin(now, id, portal, shared.gate.is_open(), locks);
        shared.apply(&mut core, begin.outs, true);
        begin.result
    }

    fn end(&mut self, warp: Option<(DisplayId, PointDevice)>) -> Result<(), PlatformError> {
        let shared = &self.shared;
        let displays = (shared.displays)();
        let now = mono_now();
        let (release, rest) = {
            let mut core = shared.core();
            // A capture, or an activation still pending (a crossing the engine refused leaves
            // one): both are given back.
            let Some((_, origin)) = core.machine.live() else {
                // Nothing is held (already ended, lost or aborted): nothing to give back.
                return Ok(());
            };
            let at = barriers::release_point(warp, &displays, origin);
            let outs = core.machine.end(now, at);
            let mut release = None;
            let mut rest = Vec::new();
            for out in outs {
                match out {
                    Out::Release {
                        activation,
                        at,
                        ups,
                    } => release = Some((activation, at, ups)),
                    other => rest.push(other),
                }
            }
            shared.live.set(core.machine.live());
            (release, rest)
        };
        // The release is made, and answered, before `Ended` tells the engine the input is back.
        let result = match release {
            Some((activation, at, ups)) => {
                let (reply, answer) = mpsc::sync_channel(1);
                shared.caller.push(Request::Release {
                    activation,
                    at,
                    ups,
                    reply: Some(reply),
                });
                answer
                    .recv_timeout(CALL_BUDGET)
                    .unwrap_or(Err(PlatformError::Timeout))
            }
            None => Ok(()),
        };
        {
            let mut core = shared.core();
            shared.apply(&mut core, rest, true);
        }
        result
    }

    fn abort_handle(&self) -> Arc<dyn CaptureAbort> {
        Arc::new(AbortHandle(Arc::clone(&self.shared)))
    }

    fn set_monitor_local_activity(&mut self, _on: bool) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported(
            "local activity monitoring on the InputCapture portal",
        ))
    }
}

/// `CaptureAbort`: an epoch bump and a wake-up, no locks.
struct AbortHandle(Arc<Shared>);

impl CaptureAbort for AbortHandle {
    fn abort(&self) {
        self.0.abort_epoch.fetch_add(1, Ordering::AcqRel);
        self.0.abort_wake.wake();
    }
}

impl Drop for PortalInputCapture {
    fn drop(&mut self) {
        let shared = &self.shared;
        // End any capture first, within the budget, and let the shutdown thread finish with the
        // portal session before the worker closes it.
        let epoch = shared.abort_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        shared.abort_wake.wake();
        // A backend a dead thread already shut down has nothing to settle.
        let settle = Instant::now() + ABORT_SETTLE;
        while !shared.closing.load(Ordering::Acquire)
            && shared.abort_done.load(Ordering::Acquire) < epoch
            && Instant::now() < settle
        {
            thread::sleep(Duration::from_micros(500));
        }
        shared.closing.store(true, Ordering::Release);
        shared.abort_wake.wake();
        shared.control.close();
        shared.caller.push(Request::Stop);
        shared.cursor_wake.wake();
        let deadline = Instant::now() + JOIN_BOUND;
        for (name, handle) in self.threads.drain(..) {
            if name == EIS_THREAD {
                // The EIS connection closes last: input is given back (the release thread) and
                // the session closed (the worker) first, never by dropping the fd.
                shared.eis.push(EisCmd::Stop);
            }
            if handle.thread().id() == thread::current().id() {
                continue;
            }
            // The worker reports when it exits; the others are simple loops.
            while !handle.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(2));
            }
            if handle.is_finished() {
                if handle.join().is_err() {
                    tracing::warn!(thread = name, "an input capture thread panicked");
                }
            } else {
                tracing::warn!(
                    thread = name,
                    "an input capture thread did not stop in time; detaching it"
                );
            }
        }
    }
}

// The handle is shared between the agent's threads.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<PortalInputCapture>();
    fn assert_sync<T: Send + Sync>() {}
    assert_sync::<Shared>();
};
