//! The release chord through the GlobalShortcuts portal (WP-G1.5).
//!
//! A worker thread owns one GlobalShortcuts session (`ashpd`, async-io, driven with
//! `zbus::block_on` on that thread only) with one shortcut, id `crosspane-release`, whose
//! preferred trigger is the chord spelled for the portal (e.g. `CTRL+ALT+SHIFT+Escape`).
//! `Activated` becomes `HotkeyEvent::Pressed`, `Deactivated` becomes `Released`, timed with the
//! injected [`Clock`] when the signal arrives.
//!
//! - **Consent.** Binding may show the desktop's shortcut dialog once; it runs on the worker,
//!   never inside a trait call. `set_chord` only records the chord and wakes the worker, then
//!   returns. The worker does nothing (no session, no dialog) until the first `set_chord`.
//! - **Pairing.** Exactly one `Released` per `Pressed`: a `Deactivated` without a preceding
//!   `Activated` is dropped, as is a second `Activated` while pressed; a lost session (closed,
//!   portal gone) or a replaced chord while pressed emits `Released`.
//! - **Ordering.** `Activated` and `Deactivated` are read from one signal stream (the portal
//!   proxy's all-signals stream), not from two separate streams, so a quick second press can
//!   never be processed before the first one's release.
//! - **Pre-held chord.** The portal can't report a chord already held at subscribe, so the first
//!   event is never synthesized. That is the safe direction: the engine re-arms only on a fresh
//!   press. The one state `subscribe` does report is a chord this module already saw pressed.
//! - **Injected input.** The engine never injects the chord (it consumes it during capture), so
//!   portal activations come from local input.
//! - **Session loss.** If the session closes or the portal's bus name changes owner, the worker
//!   emits `Released` if pressed, then retries creating a session and rebinding the last chord
//!   with a backoff of 1 s doubling to 30 s (the backoff restarts once a session stayed bound for
//!   30 s). A dialog the user denied or dismissed is not retried automatically (it would nag); it
//!   is asked again on the next `set_chord` with a different chord.
//! - **Triggers are hints.** The desktop owns the final binding: `preferred_trigger` is only used
//!   when the shortcut is first bound, and the user may rebind it in the desktop's settings. The
//!   portal reports activations for whatever trigger the user ended up with. The portal's
//!   trigger format cannot tell left and right modifiers apart, so both fold into one name (a
//!   chord naming both hands of one modifier is `Unsupported`); right Alt is `Unsupported`
//!   because on many layouts it is AltGr, which the portal's `ALT` does not match.
//! - **Lifetime.** Dropping the handle stops the worker (the wait is bounded to 2 s; a worker stuck
//!   in a portal call is detached and exits when the call returns), closes the session, and
//!   emits `Released` if the chord was pressed. A worker that died is reported by `set_chord` and
//!   `subscribe` as an error rather than leaving the chord silently unwatched.
//! - No GlobalShortcuts portal: `new` returns `PlatformError::Unsupported` (`Timeout` if the
//!   portal doesn't answer the probe within 2 s) and `set_chord` never succeeds for a chord it
//!   can't spell (never a silent weakening). The tray and `crosspanectl` keep release and panic
//!   available.
//! - **App id.** The portal identifies a non-sandboxed client by an app id registered for the
//!   process's bus connection (`ashpd::register_host_app`). That is process-wide, so the agent
//!   registers it once at startup, before any portal call; this module does not.

use std::convert::Infallible;
use std::fmt::{self, Display};
use std::future::{Future, poll_fn};
use std::pin::{Pin, pin};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ashpd::desktop::global_shortcuts::{
    Activated, BindShortcuts, BindShortcutsOptions, Deactivated, GlobalShortcuts, NewShortcut,
};
use ashpd::desktop::{CreateSessionOptions, ResponseError, Session};
use ashpd::{Error as AshpdError, PortalError};
use crosspane_platform::{Chord, EventSink, GlobalHotkeys, HotkeyEvent, PlatformError};
use crosspane_types::hid::HidUsage;
use crosspane_types::time::{Clock, MonoTime};
use zbus::export::futures_core::Stream;

/// The one shortcut this module registers.
const SHORTCUT_ID: &str = "crosspane-release";
const SHORTCUT_DESCRIPTION: &str = "Crosspane: release control / panic (hold 1 s)";
/// `new` waits this long for the portal to answer the probe.
const PROBE_BOUND: Duration = Duration::from_secs(2);
/// Dropping the handle waits this long for the worker before detaching it.
const JOIN_BOUND: Duration = Duration::from_secs(2);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A session that stayed bound this long was healthy: the next loss starts the backoff over.
const HEALTHY_AFTER: Duration = Duration::from_secs(30);

/// GlobalHotkeys through the GlobalShortcuts portal.
pub struct PortalHotkeys {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl fmt::Debug for PortalHotkeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PortalHotkeys").finish_non_exhaustive()
    }
}

impl PortalHotkeys {
    /// Probe the portal (bounded: 2 s) and start the worker. No portal, or one without the
    /// GlobalShortcuts interface, is `Unsupported`; a portal that doesn't answer within the bound
    /// is `Timeout`. The worker idles until `set_chord`.
    pub fn new(clock: Arc<dyn Clock>) -> Result<PortalHotkeys, PlatformError> {
        let shared = Arc::new(Shared::new(clock));
        let (probe_tx, probe_rx) = mpsc::channel();
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("crosspane-portal-shortcuts".into())
            .spawn(move || worker_main(&worker_shared, &probe_tx))
            .map_err(|e| PlatformError::Backend(format!("start GlobalShortcuts worker: {e}")))?;
        match probe_rx.recv_timeout(PROBE_BOUND) {
            Ok(true) => Ok(PortalHotkeys {
                shared,
                worker: Some(worker),
            }),
            Ok(false) => {
                // The worker returns right after reporting; the join is immediate.
                let _ = worker.join();
                Err(PlatformError::Unsupported("no GlobalShortcuts portal"))
            }
            Err(RecvTimeoutError::Timeout) => {
                // The worker may still be inside the probe; it sees the flag and exits when the
                // portal finally answers. It is detached, never joined.
                shared.shutdown();
                drop(worker);
                Err(PlatformError::Timeout)
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = worker.join();
                Err(PlatformError::Backend(
                    "GlobalShortcuts worker exited during the probe".into(),
                ))
            }
        }
    }

    /// The worker only ends on drop; if it died (a panic), the chord is no longer watched, and
    /// saying so beats a silent weakening.
    fn check_worker(&self) -> Result<(), PlatformError> {
        match &self.worker {
            Some(worker) if !worker.is_finished() => Ok(()),
            _ => Err(PlatformError::Backend(
                "GlobalShortcuts worker is not running".into(),
            )),
        }
    }
}

impl Drop for PortalHotkeys {
    fn drop(&mut self) {
        self.shared.shutdown();
        if let Some(worker) = self.worker.take() {
            let deadline = Instant::now() + JOIN_BOUND;
            while !worker.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            if worker.is_finished() {
                let _ = worker.join();
            }
            // Otherwise the worker is stuck in a portal call: detach it. It exits as soon as the
            // call returns, because the shutdown flag is already set.
        }
    }
}

impl GlobalHotkeys for PortalHotkeys {
    fn set_chord(&mut self, chord: &Chord) -> Result<(), PlatformError> {
        let trigger = spell_trigger(chord)?;
        self.check_worker()?;
        tracing::debug!(%trigger, "release chord for the GlobalShortcuts portal");
        self.shared.set_trigger(trigger);
        Ok(())
    }

    fn subscribe(&mut self, sink: Arc<dyn EventSink<HotkeyEvent>>) -> Result<(), PlatformError> {
        self.check_worker()?;
        self.shared.hub.subscribe(sink)
    }
}

// ---------------------------------------------------------------------------------------------
// Pure parts: trigger spelling, pairing, backoff, signal routing, error classification.
// ---------------------------------------------------------------------------------------------

/// A modifier as the portal's trigger format names it. The derived order is the canonical order of
/// a spelled trigger.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Modifier {
    Ctrl,
    Alt,
    Shift,
    Logo,
}

impl Modifier {
    fn name(self) -> &'static str {
        match self {
            Modifier::Ctrl => "CTRL",
            Modifier::Alt => "ALT",
            Modifier::Shift => "SHIFT",
            Modifier::Logo => "LOGO",
        }
    }
}

/// The portal modifier for a HID modifier usage. Left and right fold together (the format has no
/// handedness). Right Alt is not spelled: on many layouts it is AltGr, not Alt.
fn modifier_of(usage: HidUsage) -> Option<Modifier> {
    if usage.page != HidUsage::PAGE_KEYBOARD {
        return None;
    }
    match usage.id {
        0xE0 | 0xE4 => Some(Modifier::Ctrl),
        0xE1 | 0xE5 => Some(Modifier::Shift),
        0xE2 => Some(Modifier::Alt),
        0xE3 | 0xE7 => Some(Modifier::Logo),
        _ => None,
    }
}

const LETTERS: [&str; 26] = [
    "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r", "s",
    "t", "u", "v", "w", "x", "y", "z",
];
const DIGITS: [&str; 10] = ["1", "2", "3", "4", "5", "6", "7", "8", "9", "0"];
const FUNCTION_KEYS: [&str; 24] = [
    "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13", "F14", "F15",
    "F16", "F17", "F18", "F19", "F20", "F21", "F22", "F23", "F24",
];

/// The xkb keysym name of an unambiguous key. Lock keys (their press/release is not an ordinary
/// chord part), the keypad (its keysyms depend on Num Lock) and layout-specific keys are `None`.
fn keysym_name(usage: HidUsage) -> Option<&'static str> {
    if usage.page != HidUsage::PAGE_KEYBOARD {
        return None;
    }
    let id = usage.id;
    Some(match id {
        0x04..=0x1D => LETTERS[usize::from(id - 0x04)],
        0x1E..=0x27 => DIGITS[usize::from(id - 0x1E)],
        0x28 => "Return",
        0x29 => "Escape",
        0x2A => "BackSpace",
        0x2B => "Tab",
        0x2C => "space",
        0x2D => "minus",
        0x2E => "equal",
        0x2F => "bracketleft",
        0x30 => "bracketright",
        0x31 => "backslash",
        0x33 => "semicolon",
        0x34 => "apostrophe",
        0x35 => "grave",
        0x36 => "comma",
        0x37 => "period",
        0x38 => "slash",
        0x3A..=0x45 => FUNCTION_KEYS[usize::from(id - 0x3A)],
        0x46 => "Print",
        0x47 => "Scroll_Lock",
        0x48 => "Pause",
        0x49 => "Insert",
        0x4A => "Home",
        0x4B => "Page_Up",
        0x4C => "Delete",
        0x4D => "End",
        0x4E => "Page_Down",
        0x4F => "Right",
        0x50 => "Left",
        0x51 => "Down",
        0x52 => "Up",
        0x65 => "Menu",
        0x68..=0x73 => FUNCTION_KEYS[usize::from(id - 0x68) + 12],
        _ => return None,
    })
}

/// The chord in the portal's shortcut-trigger format: modifiers `CTRL`, `ALT`, `SHIFT`, `LOGO`
/// (in that order) and then the key's keysym name, all joined with `+`.
fn spell_trigger(chord: &Chord) -> Result<String, PlatformError> {
    let key = keysym_name(chord.key).ok_or(PlatformError::Unsupported(
        "this release chord key can't be spelled for the GlobalShortcuts portal",
    ))?;
    let mut modifiers: Vec<Modifier> = Vec::with_capacity(chord.modifiers.len());
    for usage in &chord.modifiers {
        let modifier = modifier_of(*usage).ok_or(PlatformError::Unsupported(
            "this release chord modifier can't be spelled for the GlobalShortcuts portal",
        ))?;
        if modifiers.contains(&modifier) {
            return Err(PlatformError::Unsupported(
                "the GlobalShortcuts portal can't tell the release chord's repeated modifier apart",
            ));
        }
        modifiers.push(modifier);
    }
    modifiers.sort_unstable();
    let mut trigger = String::new();
    for modifier in modifiers {
        trigger.push_str(modifier.name());
        trigger.push('+');
    }
    trigger.push_str(key);
    Ok(trigger)
}

/// Press/release pairing: exactly one `Released` per `Pressed`.
#[derive(Debug, Default)]
struct Pairing {
    pressed: Option<MonoTime>,
}

impl Pairing {
    /// `Activated`. A repeat while already pressed is dropped.
    fn activated(&mut self, at: MonoTime) -> Option<HotkeyEvent> {
        if self.pressed.is_some() {
            return None;
        }
        self.pressed = Some(at);
        Some(HotkeyEvent::Pressed { at })
    }

    /// `Deactivated`. One without a preceding `Activated` is dropped.
    fn deactivated(&mut self, at: MonoTime) -> Option<HotkeyEvent> {
        self.pressed.take().map(|_| HotkeyEvent::Released { at })
    }

    /// The session was lost or the chord replaced: if pressed, the release will never be seen, so
    /// synthesize it. Same transition as `deactivated`.
    fn lost(&mut self, at: MonoTime) -> Option<HotkeyEvent> {
        self.deactivated(at)
    }

    /// The press this pairing is waiting to see released, with its original time.
    fn current(&self) -> Option<HotkeyEvent> {
        self.pressed.map(|at| HotkeyEvent::Pressed { at })
    }
}

/// Which of the portal's shortcut signals a message is, for our shortcut only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShortcutSignal {
    Activated,
    Deactivated,
}

fn route(member: &str, shortcut_id: &str) -> Option<ShortcutSignal> {
    if shortcut_id != SHORTCUT_ID {
        return None;
    }
    match member {
        "Activated" => Some(ShortcutSignal::Activated),
        "Deactivated" => Some(ShortcutSignal::Deactivated),
        _ => None,
    }
}

/// Reconnect delays: 1 s doubling to 30 s.
#[derive(Debug)]
struct Backoff {
    next: Duration,
}

impl Backoff {
    fn new() -> Self {
        Backoff { next: BACKOFF_MIN }
    }

    /// The delay before the next attempt, given how long the ended session stayed bound (`None`
    /// if it never bound). A session that was healthy for long enough starts the sequence over.
    fn delay(&mut self, bound_for: Option<Duration>) -> Duration {
        if bound_for.is_some_and(|d| d >= HEALTHY_AFTER) {
            self.next = BACKOFF_MIN;
        }
        let delay = self.next;
        self.next = self.next.saturating_mul(2).min(BACKOFF_MAX);
        delay
    }
}

/// What a failed bind means for the retry policy.
#[derive(Debug, PartialEq, Eq)]
enum BindFailure {
    /// The user (or the desktop's policy) said no: don't ask again until the chord changes.
    Denied,
    /// Anything else (portal restarting, bus error): retry with backoff.
    Retry,
}

fn classify_bind_error(error: &AshpdError) -> BindFailure {
    match error {
        AshpdError::Response(ResponseError::Cancelled | ResponseError::Other)
        | AshpdError::Portal(PortalError::Cancelled(_) | PortalError::NotAllowed(_)) => {
            BindFailure::Denied
        }
        _ => BindFailure::Retry,
    }
}

// ---------------------------------------------------------------------------------------------
// Shared state: event delivery and the worker's control channel.
// ---------------------------------------------------------------------------------------------

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Delivers hotkey events to the subscriber, in order, keeping the pairing state under one lock.
struct Hub {
    clock: Arc<dyn Clock>,
    state: Mutex<HubState>,
}

#[derive(Default)]
struct HubState {
    pairing: Pairing,
    sink: Option<Arc<dyn EventSink<HotkeyEvent>>>,
}

impl HubState {
    fn deliver(&self, event: Option<HotkeyEvent>) {
        if let (Some(event), Some(sink)) = (event, &self.sink) {
            sink.send(event);
        }
    }
}

impl Hub {
    fn new(clock: Arc<dyn Clock>) -> Self {
        Hub {
            clock,
            state: Mutex::new(HubState::default()),
        }
    }

    fn activated(&self) {
        let mut state = lock(&self.state);
        let event = state.pairing.activated(self.clock.now());
        state.deliver(event);
    }

    fn deactivated(&self) {
        let mut state = lock(&self.state);
        let event = state.pairing.deactivated(self.clock.now());
        state.deliver(event);
    }

    fn lost(&self) {
        let mut state = lock(&self.state);
        let event = state.pairing.lost(self.clock.now());
        state.deliver(event);
    }

    /// Attach the sink, current state first: a chord already seen pressed is reported before
    /// anything later. Events from before this call were not buffered.
    fn subscribe(&self, sink: Arc<dyn EventSink<HotkeyEvent>>) -> Result<(), PlatformError> {
        let mut state = lock(&self.state);
        if state.sink.is_some() {
            return Err(PlatformError::Backend(
                "release chord events are already subscribed".into(),
            ));
        }
        if let Some(pressed) = state.pairing.current() {
            sink.send(pressed);
        }
        state.sink = Some(sink);
        Ok(())
    }
}

#[derive(Debug, Default)]
struct Control {
    /// The spelled chord; `None` until the first `set_chord`.
    trigger: Option<String>,
    /// Bumped on every change of `trigger`.
    generation: u64,
    shutdown: bool,
    /// The async side's waker, woken on every change.
    waker: Option<Waker>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ControlChange {
    Shutdown,
    Changed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wake {
    Shutdown,
    Changed,
    Elapsed,
}

struct Shared {
    control: Mutex<Control>,
    condvar: Condvar,
    hub: Hub,
}

impl Shared {
    fn new(clock: Arc<dyn Clock>) -> Self {
        Shared {
            control: Mutex::new(Control::default()),
            condvar: Condvar::new(),
            hub: Hub::new(clock),
        }
    }

    /// Wake both sides: threads parked on the condvar and the async task's waker.
    fn notify(&self, mut control: MutexGuard<'_, Control>) {
        let waker = control.waker.take();
        drop(control);
        self.condvar.notify_all();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Record the chord. An unchanged chord is not a change: it neither wakes the worker nor asks
    /// the desktop again.
    fn set_trigger(&self, trigger: String) {
        let mut control = lock(&self.control);
        if control.trigger.as_deref() == Some(trigger.as_str()) {
            return;
        }
        control.trigger = Some(trigger);
        control.generation += 1;
        self.notify(control);
    }

    fn shutdown(&self) {
        let mut control = lock(&self.control);
        control.shutdown = true;
        self.notify(control);
    }

    fn snapshot(&self) -> (Option<String>, u64) {
        let control = lock(&self.control);
        (control.trigger.clone(), control.generation)
    }

    fn generation(&self) -> u64 {
        lock(&self.control).generation
    }

    /// Async side: ready when shutdown is requested or the chord changed since generation
    /// `seen`; otherwise registers the waker.
    fn poll_control(&self, cx: &mut Context<'_>, seen: u64) -> Poll<ControlChange> {
        let mut control = lock(&self.control);
        if control.shutdown {
            return Poll::Ready(ControlChange::Shutdown);
        }
        if control.generation != seen {
            return Poll::Ready(ControlChange::Changed);
        }
        control.waker = Some(cx.waker().clone());
        Poll::Pending
    }

    /// Async side: true once shutdown is requested; otherwise registers the waker.
    fn poll_shutdown(&self, cx: &mut Context<'_>) -> bool {
        let mut control = lock(&self.control);
        if control.shutdown {
            return true;
        }
        control.waker = Some(cx.waker().clone());
        false
    }

    /// Sync side: wait for the first chord. `false` on shutdown.
    fn wait_for_trigger(&self) -> bool {
        let control = self
            .condvar
            .wait_while(lock(&self.control), |c| !c.shutdown && c.trigger.is_none())
            .unwrap_or_else(PoisonError::into_inner);
        !control.shutdown
    }

    /// Sync side: wait until shutdown, a chord change since generation `seen`, or `timeout`
    /// (forever if `None`).
    fn wait_for_change(&self, seen: u64, timeout: Option<Duration>) -> Wake {
        let control = lock(&self.control);
        let control = match timeout {
            Some(timeout) => {
                self.condvar
                    .wait_timeout_while(control, timeout, |c| !c.shutdown && c.generation == seen)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0
            }
            None => self
                .condvar
                .wait_while(control, |c| !c.shutdown && c.generation == seen)
                .unwrap_or_else(PoisonError::into_inner),
        };
        if control.shutdown {
            Wake::Shutdown
        } else if control.generation != seen {
            Wake::Changed
        } else {
            Wake::Elapsed
        }
    }
}

/// Emits `Released` if the chord is pressed when the worker leaves, however it leaves.
struct ReleaseOnExit<'a>(&'a Shared);

impl Drop for ReleaseOnExit<'_> {
    fn drop(&mut self) {
        self.0.hub.lost();
    }
}

// ---------------------------------------------------------------------------------------------
// The worker.
// ---------------------------------------------------------------------------------------------

/// Run `fut` until it finishes or shutdown is requested (`None`).
async fn interruptible<F: Future>(shared: &Shared, fut: F) -> Option<F::Output> {
    let mut fut = pin!(fut);
    poll_fn(|cx| {
        if let Poll::Ready(output) = fut.as_mut().poll(cx) {
            return Poll::Ready(Some(output));
        }
        if shared.poll_shutdown(cx) {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    })
    .await
}

/// Why a session ended.
#[derive(Debug)]
enum SessionEnd {
    Shutdown,
    /// The desktop refused the binding (or the user dismissed its dialog) for the chord of this
    /// generation.
    Denied {
        generation: u64,
    },
    Lost {
        reason: String,
        /// The session object is already gone (closed by the portal, or the portal itself is),
        /// so closing it would be pointless.
        session_gone: bool,
    },
}

/// One interruptible portal step; a failure ends the session as `Lost`.
async fn step<T, E: Display>(
    shared: &Shared,
    what: &'static str,
    session_gone: bool,
    fut: impl Future<Output = Result<T, E>>,
) -> Result<T, SessionEnd> {
    match interruptible(shared, fut).await {
        None => Err(SessionEnd::Shutdown),
        Some(Ok(value)) => Ok(value),
        Some(Err(error)) => Err(SessionEnd::Lost {
            reason: format!("{what}: {error}"),
            session_gone,
        }),
    }
}

/// Is the GlobalShortcuts interface there? `GlobalShortcuts::new` alone isn't proof (it reports
/// version 1 for most errors), so the version property is read explicitly.
async fn probe_portal(shared: &Shared) -> Result<(), String> {
    let probe = async {
        let portal = GlobalShortcuts::new().await.map_err(|e| e.to_string())?;
        portal
            .get_property::<u32>("version")
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    };
    interruptible(shared, probe)
        .await
        .unwrap_or_else(|| Err("probe cancelled".into()))
}

fn worker_main(shared: &Shared, probe_tx: &mpsc::Sender<bool>) {
    let _release = ReleaseOnExit(shared);
    let probe = zbus::block_on(probe_portal(shared));
    if let Err(reason) = &probe {
        tracing::debug!(%reason, "no GlobalShortcuts portal");
    }
    // `new` may have given up waiting already; nobody is listening then, which is fine.
    let _ = probe_tx.send(probe.is_ok());
    if probe.is_ok() {
        run_worker(shared);
    }
}

fn run_worker(shared: &Shared) {
    let mut backoff = Backoff::new();
    let mut failures = 0u32;
    loop {
        if !shared.wait_for_trigger() {
            return;
        }
        let mut bound_at = None;
        let end = zbus::block_on(run_session(shared, &mut bound_at));
        // Whatever ended the session, a press in flight will never see its release.
        shared.hub.lost();
        match end {
            SessionEnd::Shutdown => return,
            SessionEnd::Denied { generation } => {
                tracing::warn!(
                    "the desktop did not bind the Crosspane release chord; change the chord to ask again"
                );
                if shared.wait_for_change(generation, None) == Wake::Shutdown {
                    return;
                }
            }
            SessionEnd::Lost { reason, .. } => {
                failures = if bound_at.is_some() {
                    1
                } else {
                    failures.saturating_add(1)
                };
                let delay = backoff.delay(bound_at.map(|at: Instant| at.elapsed()));
                if failures == 1 {
                    tracing::warn!(%reason, ?delay, "GlobalShortcuts session lost; retrying");
                } else {
                    tracing::debug!(%reason, ?delay, "GlobalShortcuts session unavailable; retrying");
                }
                if shared.wait_for_change(shared.generation(), Some(delay)) == Wake::Shutdown {
                    return;
                }
            }
        }
    }
}

/// One session: create it, bind the chord, then serve signals until it ends. `bound_at` records
/// when the first bind succeeded.
async fn run_session(shared: &Shared, bound_at: &mut Option<Instant>) -> SessionEnd {
    let portal = match step(shared, "open the portal", true, GlobalShortcuts::new()).await {
        Ok(portal) => portal,
        Err(end) => return end,
    };
    // Subscribe before binding so no activation can slip in between. One stream for every signal
    // of the interface keeps `Activated` and `Deactivated` in the order the portal sent them.
    let signals = match step(
        shared,
        "subscribe to signals",
        true,
        portal.receive_all_signals(),
    )
    .await
    {
        Ok(signals) => signals,
        Err(end) => return end,
    };
    let owner = match step(
        shared,
        "watch the portal's bus name",
        true,
        portal.receive_owner_changed(),
    )
    .await
    {
        Ok(owner) => owner,
        Err(end) => return end,
    };
    let session = match step(
        shared,
        "create the session",
        true,
        portal.create_session(CreateSessionOptions::default()),
    )
    .await
    {
        Ok(session) => session,
        Err(end) => return end,
    };

    let Err(end) = serve(shared, &portal, &session, signals, owner, bound_at).await;
    match &end {
        // Close for good so no dialog or shortcut registration outlives us.
        SessionEnd::Shutdown => {
            let _ = session.close().await;
        }
        SessionEnd::Denied { .. }
        | SessionEnd::Lost {
            session_gone: false,
            ..
        } => {
            let _ = interruptible(shared, session.close()).await;
        }
        SessionEnd::Lost { .. } => {}
    }
    end
}

type BindFuture<'a> = Pin<Box<dyn Future<Output = Result<BindShortcuts, AshpdError>> + 'a>>;

fn start_bind<'a>(
    portal: &'a GlobalShortcuts,
    session: &'a Session<GlobalShortcuts>,
    trigger: String,
) -> BindFuture<'a> {
    Box::pin(async move {
        let shortcuts =
            [NewShortcut::new(SHORTCUT_ID, SHORTCUT_DESCRIPTION)
                .preferred_trigger(trigger.as_str())];
        let request = portal
            .bind_shortcuts(session, &shortcuts, None, BindShortcutsOptions::default())
            .await?;
        request.response()
    })
}

enum Event {
    Shutdown,
    Changed,
    SessionClosed,
    OwnerChanged,
    Signal(Option<zbus::Message>),
    Bind(Result<BindShortcuts, AshpdError>),
}

/// Bind the chord and serve the session's signals. Never returns normally: every exit is an end.
async fn serve(
    shared: &Shared,
    portal: &GlobalShortcuts,
    session: &Session<GlobalShortcuts>,
    signals: impl Stream<Item = zbus::Message>,
    owner: impl Stream,
    bound_at: &mut Option<Instant>,
) -> Result<Infallible, SessionEnd> {
    let closed = step(shared, "watch the session", false, session.receive_closed()).await?;
    let mut closed = pin!(closed);
    let mut signals = pin!(signals);
    let mut owner = pin!(owner);

    let (trigger, generation) = shared.snapshot();
    let Some(trigger) = trigger else {
        return Err(SessionEnd::Lost {
            reason: "no release chord set".into(),
            session_gone: false,
        });
    };
    // The generation the in-flight bind was started for, and the newest one acted on.
    let mut bound_generation = generation;
    let mut seen = generation;
    let mut bind: Option<BindFuture<'_>> = Some(start_bind(portal, session, trigger));

    loop {
        let event = poll_fn(|cx| {
            match shared.poll_control(cx, seen) {
                Poll::Ready(ControlChange::Shutdown) => return Poll::Ready(Event::Shutdown),
                Poll::Ready(ControlChange::Changed) => return Poll::Ready(Event::Changed),
                Poll::Pending => {}
            }
            if closed.as_mut().poll_next(cx).is_ready() {
                return Poll::Ready(Event::SessionClosed);
            }
            if owner.as_mut().poll_next(cx).is_ready() {
                return Poll::Ready(Event::OwnerChanged);
            }
            if let Poll::Ready(message) = signals.as_mut().poll_next(cx) {
                return Poll::Ready(Event::Signal(message));
            }
            if let Some(pending) = bind.as_mut()
                && let Poll::Ready(result) = pending.as_mut().poll(cx)
            {
                return Poll::Ready(Event::Bind(result));
            }
            Poll::Pending
        })
        .await;

        match event {
            Event::Shutdown => return Err(SessionEnd::Shutdown),
            Event::SessionClosed => {
                return Err(SessionEnd::Lost {
                    reason: "the session was closed".into(),
                    session_gone: true,
                });
            }
            Event::OwnerChanged => {
                return Err(SessionEnd::Lost {
                    reason: "the portal's bus name changed owner".into(),
                    session_gone: true,
                });
            }
            Event::Signal(None) => {
                return Err(SessionEnd::Lost {
                    reason: "the signal stream ended".into(),
                    session_gone: true,
                });
            }
            Event::Signal(Some(message)) => handle_signal(&shared.hub, &message),
            Event::Changed => {
                let (trigger, generation) = shared.snapshot();
                seen = generation;
                // The replaced chord's release may never arrive.
                shared.hub.lost();
                // A bind in flight finishes first (it may be waiting on the user); the newer
                // chord is bound right after it.
                if bind.is_none()
                    && let Some(trigger) = trigger
                {
                    bound_generation = generation;
                    bind = Some(start_bind(portal, session, trigger));
                }
            }
            Event::Bind(Ok(bound)) => {
                bind = None;
                bound_at.get_or_insert_with(Instant::now);
                log_bound(&bound);
                if seen != bound_generation {
                    let (trigger, generation) = shared.snapshot();
                    seen = generation;
                    if let Some(trigger) = trigger {
                        bound_generation = generation;
                        bind = Some(start_bind(portal, session, trigger));
                    }
                }
            }
            Event::Bind(Err(error)) => {
                return Err(match classify_bind_error(&error) {
                    BindFailure::Denied => {
                        tracing::debug!(%error, "binding the release chord was refused");
                        SessionEnd::Denied {
                            generation: bound_generation,
                        }
                    }
                    BindFailure::Retry => SessionEnd::Lost {
                        reason: format!("bind the release chord: {error}"),
                        session_gone: false,
                    },
                });
            }
        }
    }
}

/// Say what the desktop actually bound. A missing or trigger-less shortcut leaves the emergency
/// control unreachable from the keyboard, so that is a warning.
fn log_bound(bound: &BindShortcuts) {
    match bound.shortcuts().iter().find(|s| s.id() == SHORTCUT_ID) {
        Some(shortcut) if !shortcut.trigger_description().is_empty() => {
            tracing::debug!(
                trigger = shortcut.trigger_description(),
                "release chord bound by the desktop"
            );
        }
        Some(_) => tracing::warn!(
            "the desktop bound the Crosspane release shortcut without a trigger; assign one in its shortcut settings"
        ),
        None => {
            tracing::warn!("the desktop did not report the Crosspane release shortcut as bound")
        }
    }
}

/// Route one signal of the interface into the hub. Other shortcuts' signals and other members are
/// ignored.
fn handle_signal(hub: &Hub, message: &zbus::Message) {
    let header = message.header();
    let Some(member) = header.member() else {
        return;
    };
    let body = message.body();
    let routed = match member.as_str() {
        "Activated" => body
            .deserialize::<Activated>()
            .map(|a| route("Activated", a.shortcut_id())),
        "Deactivated" => body
            .deserialize::<Deactivated>()
            .map(|d| route("Deactivated", d.shortcut_id())),
        _ => return,
    };
    match routed {
        Ok(Some(ShortcutSignal::Activated)) => hub.activated(),
        Ok(Some(ShortcutSignal::Deactivated)) => hub.deactivated(),
        Ok(None) => {}
        Err(error) => tracing::debug!(%error, "ignoring an unreadable shortcut signal"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::task::Wake as TaskWake;

    use super::*;

    fn at(n: u64) -> MonoTime {
        MonoTime::from_nanos(n)
    }

    fn key(id: u16) -> HidUsage {
        HidUsage::keyboard(id)
    }

    fn chord(modifiers: &[u16], key_id: u16) -> Chord {
        Chord {
            modifiers: modifiers.iter().map(|id| key(*id)).collect(),
            key: key(key_id),
        }
    }

    fn unsupported(chord: &Chord) -> bool {
        matches!(spell_trigger(chord), Err(PlatformError::Unsupported(_)))
    }

    // Trigger spelling.

    #[test]
    fn default_chord_is_spelled_in_canonical_order() {
        assert_eq!(
            spell_trigger(&chord(&[0xE0, 0xE2, 0xE1], 0x29)).unwrap(),
            "CTRL+ALT+SHIFT+Escape"
        );
        // Input order doesn't matter.
        assert_eq!(
            spell_trigger(&chord(&[0xE1, 0xE0, 0xE2], 0x29)).unwrap(),
            "CTRL+ALT+SHIFT+Escape"
        );
    }

    #[test]
    fn right_hand_modifiers_fold_into_the_same_names_except_alt() {
        assert_eq!(
            spell_trigger(&chord(&[0xE4, 0xE5, 0xE7], 0x2C)).unwrap(),
            "CTRL+SHIFT+LOGO+space"
        );
        assert_eq!(spell_trigger(&chord(&[0xE3], 0x04)).unwrap(), "LOGO+a");
        // Right Alt is AltGr on many layouts.
        assert!(unsupported(&chord(&[0xE6], 0x29)));
    }

    #[test]
    fn both_hands_of_one_modifier_and_repeats_are_unsupported() {
        assert!(unsupported(&chord(&[0xE0, 0xE4], 0x29)));
        assert!(unsupported(&chord(&[0xE0, 0xE0], 0x29)));
        assert!(unsupported(&chord(&[0xE1, 0xE5, 0xE0], 0x29)));
    }

    #[test]
    fn a_modifier_or_foreign_page_usage_cannot_be_the_key_or_a_modifier() {
        for modifier_as_key in 0xE0..=0xE7 {
            assert!(unsupported(&chord(&[0xE0], modifier_as_key)));
        }
        assert!(unsupported(&chord(&[0x29], 0x29)));
        assert!(unsupported(&Chord {
            modifiers: vec![HidUsage {
                page: 0x0C,
                id: 0xE0
            }],
            key: key(0x29),
        }));
        assert!(unsupported(&Chord {
            modifiers: vec![key(0xE0)],
            key: HidUsage {
                page: 0x0C,
                id: 0x29
            },
        }));
    }

    #[test]
    fn keys_the_portal_cannot_name_unambiguously_are_unsupported() {
        for id in [
            0x00, 0x01, 0x32, 0x39, 0x53, 0x54, 0x58, 0x59, 0x62, 0x63, 0x64, 0x66, 0x74, 0xFF,
        ] {
            assert!(unsupported(&chord(&[0xE0], id)), "usage {id:#x}");
        }
    }

    #[test]
    fn key_names_follow_the_xkb_keysym_table() {
        let cases: &[(u16, &str)] = &[
            (0x04, "a"),
            (0x1D, "z"),
            (0x1E, "1"),
            (0x26, "9"),
            (0x27, "0"),
            (0x28, "Return"),
            (0x29, "Escape"),
            (0x2A, "BackSpace"),
            (0x2B, "Tab"),
            (0x2C, "space"),
            (0x2D, "minus"),
            (0x2E, "equal"),
            (0x2F, "bracketleft"),
            (0x30, "bracketright"),
            (0x31, "backslash"),
            (0x33, "semicolon"),
            (0x34, "apostrophe"),
            (0x35, "grave"),
            (0x36, "comma"),
            (0x37, "period"),
            (0x38, "slash"),
            (0x3A, "F1"),
            (0x45, "F12"),
            (0x46, "Print"),
            (0x47, "Scroll_Lock"),
            (0x48, "Pause"),
            (0x49, "Insert"),
            (0x4A, "Home"),
            (0x4B, "Page_Up"),
            (0x4C, "Delete"),
            (0x4D, "End"),
            (0x4E, "Page_Down"),
            (0x4F, "Right"),
            (0x50, "Left"),
            (0x51, "Down"),
            (0x52, "Up"),
            (0x65, "Menu"),
            (0x68, "F13"),
            (0x73, "F24"),
        ];
        for (id, name) in cases {
            assert_eq!(keysym_name(key(*id)), Some(*name), "usage {id:#x}");
        }
    }

    #[test]
    fn every_function_key_and_letter_has_a_distinct_name() {
        let mut names = std::collections::HashSet::new();
        for id in (0x04..=0x45).chain(0x68..=0x73) {
            if let Some(name) = keysym_name(key(id)) {
                assert!(names.insert(name), "duplicate name {name}");
            }
        }
        assert!(names.contains("F17"));
        assert!(names.contains("q"));
    }

    #[test]
    fn a_chord_without_modifiers_is_just_the_key() {
        assert_eq!(spell_trigger(&chord(&[], 0x3F)).unwrap(), "F6");
    }

    // Pairing.

    #[test]
    fn pairing_reports_one_released_per_pressed() {
        let mut p = Pairing::default();
        assert_eq!(p.activated(at(1)), Some(HotkeyEvent::Pressed { at: at(1) }));
        assert_eq!(
            p.deactivated(at(2)),
            Some(HotkeyEvent::Released { at: at(2) })
        );
        assert_eq!(p.activated(at(3)), Some(HotkeyEvent::Pressed { at: at(3) }));
        assert_eq!(
            p.deactivated(at(4)),
            Some(HotkeyEvent::Released { at: at(4) })
        );
    }

    #[test]
    fn pairing_drops_an_unpaired_deactivated_and_a_repeated_activated() {
        let mut p = Pairing::default();
        assert_eq!(p.deactivated(at(1)), None);
        assert_eq!(p.lost(at(2)), None);
        assert_eq!(p.activated(at(3)), Some(HotkeyEvent::Pressed { at: at(3) }));
        assert_eq!(p.activated(at(4)), None);
        assert_eq!(p.current(), Some(HotkeyEvent::Pressed { at: at(3) }));
        assert_eq!(
            p.deactivated(at(5)),
            Some(HotkeyEvent::Released { at: at(5) })
        );
        assert_eq!(p.deactivated(at(6)), None);
        assert_eq!(p.current(), None);
    }

    #[test]
    fn pairing_synthesizes_one_released_when_lost_while_pressed() {
        let mut p = Pairing::default();
        p.activated(at(1));
        assert_eq!(p.lost(at(2)), Some(HotkeyEvent::Released { at: at(2) }));
        // The old session's Deactivated arriving late is unpaired now.
        assert_eq!(p.deactivated(at(3)), None);
        assert_eq!(p.lost(at(4)), None);
    }

    // Signal routing.

    #[test]
    fn only_our_shortcut_is_routed() {
        assert_eq!(
            route("Activated", "crosspane-release"),
            Some(ShortcutSignal::Activated)
        );
        assert_eq!(
            route("Deactivated", "crosspane-release"),
            Some(ShortcutSignal::Deactivated)
        );
        assert_eq!(route("Activated", "other"), None);
        assert_eq!(route("Deactivated", ""), None);
        assert_eq!(route("ShortcutsChanged", "crosspane-release"), None);
    }

    // Backoff.

    #[test]
    fn backoff_doubles_from_one_second_to_thirty() {
        let mut b = Backoff::new();
        let secs: Vec<u64> = (0..8).map(|_| b.delay(None).as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 16, 30, 30, 30]);
    }

    #[test]
    fn backoff_restarts_only_after_a_healthy_session() {
        let mut b = Backoff::new();
        for _ in 0..4 {
            b.delay(None);
        }
        // A session that bound but flapped quickly does not reset the sequence.
        assert_eq!(b.delay(Some(Duration::from_secs(5))).as_secs(), 16);
        // One that stayed bound for 30 s does.
        assert_eq!(b.delay(Some(HEALTHY_AFTER)).as_secs(), 1);
        assert_eq!(b.delay(None).as_secs(), 2);
    }

    // Bind failures.

    #[test]
    fn dismissed_and_refused_dialogs_are_denied_not_retried() {
        for error in [
            AshpdError::Response(ResponseError::Cancelled),
            AshpdError::Response(ResponseError::Other),
            AshpdError::Portal(PortalError::Cancelled(String::new())),
            AshpdError::Portal(PortalError::NotAllowed(String::new())),
        ] {
            assert_eq!(classify_bind_error(&error), BindFailure::Denied, "{error}");
        }
    }

    #[test]
    fn bus_and_portal_failures_are_retried() {
        for error in [
            AshpdError::NoResponse,
            AshpdError::Portal(PortalError::Failed(String::new())),
            AshpdError::Portal(PortalError::NotFound(String::new())),
            AshpdError::Zbus(zbus::Error::Failure("gone".into())),
        ] {
            assert_eq!(classify_bind_error(&error), BindFailure::Retry, "{error}");
        }
    }

    // Event delivery.

    struct FakeClock(AtomicU64);

    impl FakeClock {
        fn set(&self, nanos: u64) {
            self.0.store(nanos, Ordering::SeqCst);
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> MonoTime {
            MonoTime::from_nanos(self.0.load(Ordering::SeqCst))
        }
    }

    #[derive(Default)]
    struct Collect(Mutex<Vec<HotkeyEvent>>);

    impl Collect {
        fn events(&self) -> Vec<HotkeyEvent> {
            self.0.lock().unwrap().clone()
        }
    }

    impl EventSink<HotkeyEvent> for Collect {
        fn send(&self, event: HotkeyEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn hub() -> (Arc<FakeClock>, Hub, Arc<Collect>) {
        let clock = Arc::new(FakeClock(AtomicU64::new(0)));
        let hub = Hub::new(clock.clone());
        let sink = Arc::new(Collect::default());
        (clock, hub, sink)
    }

    #[test]
    fn hub_times_events_with_the_clock_and_pairs_them() {
        let (clock, hub, sink) = hub();
        hub.subscribe(sink.clone()).unwrap();
        clock.set(10);
        hub.activated();
        clock.set(20);
        hub.activated();
        clock.set(30);
        hub.deactivated();
        clock.set(40);
        hub.deactivated();
        assert_eq!(
            sink.events(),
            [
                HotkeyEvent::Pressed { at: at(10) },
                HotkeyEvent::Released { at: at(30) },
            ]
        );
    }

    #[test]
    fn hub_does_not_buffer_before_subscribe_except_the_pressed_state() {
        let (clock, hub, sink) = hub();
        // A complete press before subscribe is simply not reported.
        clock.set(1);
        hub.activated();
        clock.set(2);
        hub.deactivated();
        // A press in progress at subscribe is reported first, with its original time.
        clock.set(3);
        hub.activated();
        clock.set(4);
        hub.subscribe(sink.clone()).unwrap();
        assert_eq!(sink.events(), [HotkeyEvent::Pressed { at: at(3) }]);
        clock.set(5);
        hub.deactivated();
        assert_eq!(
            sink.events(),
            [
                HotkeyEvent::Pressed { at: at(3) },
                HotkeyEvent::Released { at: at(5) },
            ]
        );
    }

    #[test]
    fn hub_subscribe_with_nothing_pressed_sends_nothing() {
        let (_clock, hub, sink) = hub();
        hub.subscribe(sink.clone()).unwrap();
        assert!(sink.events().is_empty());
    }

    #[test]
    fn hub_subscribes_once() {
        let (_clock, hub, sink) = hub();
        hub.subscribe(sink.clone()).unwrap();
        assert!(matches!(
            hub.subscribe(sink.clone()),
            Err(PlatformError::Backend(_))
        ));
    }

    #[test]
    fn hub_releases_a_press_whose_session_was_lost() {
        let (clock, hub, sink) = hub();
        hub.subscribe(sink.clone()).unwrap();
        clock.set(1);
        hub.activated();
        clock.set(2);
        hub.lost();
        clock.set(3);
        hub.deactivated();
        hub.lost();
        assert_eq!(
            sink.events(),
            [
                HotkeyEvent::Pressed { at: at(1) },
                HotkeyEvent::Released { at: at(2) },
            ]
        );
    }

    #[test]
    fn leaving_the_worker_releases_a_pressed_chord() {
        let clock = Arc::new(FakeClock(AtomicU64::new(0)));
        let shared = Shared::new(clock.clone());
        let sink = Arc::new(Collect::default());
        shared.hub.subscribe(sink.clone()).unwrap();
        clock.set(1);
        shared.hub.activated();
        clock.set(2);
        drop(ReleaseOnExit(&shared));
        assert_eq!(
            sink.events(),
            [
                HotkeyEvent::Pressed { at: at(1) },
                HotkeyEvent::Released { at: at(2) },
            ]
        );
    }

    // The control channel.

    struct CountingWake(AtomicUsize);

    impl TaskWake for CountingWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn shared() -> Shared {
        Shared::new(Arc::new(FakeClock(AtomicU64::new(0))))
    }

    #[test]
    fn an_unchanged_chord_is_not_a_change() {
        let shared = shared();
        shared.set_trigger("CTRL+a".into());
        shared.set_trigger("CTRL+a".into());
        assert_eq!(shared.snapshot(), (Some("CTRL+a".into()), 1));
        shared.set_trigger("CTRL+b".into());
        assert_eq!(shared.snapshot(), (Some("CTRL+b".into()), 2));
    }

    #[test]
    fn control_changes_wake_the_registered_waker() {
        let shared = shared();
        let counter = Arc::new(CountingWake(AtomicUsize::new(0)));
        let waker = Waker::from(counter.clone());
        let mut cx = Context::from_waker(&waker);

        assert!(shared.poll_control(&mut cx, 0).is_pending());
        shared.set_trigger("CTRL+a".into());
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        assert_eq!(
            shared.poll_control(&mut cx, 0),
            Poll::Ready(ControlChange::Changed)
        );

        assert!(shared.poll_control(&mut cx, 1).is_pending());
        shared.shutdown();
        assert_eq!(counter.0.load(Ordering::SeqCst), 2);
        // Shutdown wins over a pending change.
        assert_eq!(
            shared.poll_control(&mut cx, 0),
            Poll::Ready(ControlChange::Shutdown)
        );
        assert!(shared.poll_shutdown(&mut cx));
    }

    #[test]
    fn sync_waits_return_on_the_right_cause() {
        let shared = shared();
        assert_eq!(
            shared.wait_for_change(0, Some(Duration::from_millis(5))),
            Wake::Elapsed
        );
        shared.set_trigger("CTRL+a".into());
        assert!(shared.wait_for_trigger());
        assert_eq!(shared.wait_for_change(0, None), Wake::Changed);
        assert_eq!(
            shared.wait_for_change(1, Some(Duration::from_millis(5))),
            Wake::Elapsed
        );
        shared.shutdown();
        assert!(!shared.wait_for_trigger());
        assert_eq!(shared.wait_for_change(1, None), Wake::Shutdown);
    }

    #[test]
    fn a_waiting_worker_is_woken_by_set_chord_and_by_shutdown() {
        let shared = Arc::new(shared());
        let waiter = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || shared.wait_for_trigger())
        };
        thread::sleep(Duration::from_millis(20));
        shared.set_trigger("CTRL+a".into());
        assert!(waiter.join().unwrap());

        let waiter = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || shared.wait_for_change(1, None))
        };
        thread::sleep(Duration::from_millis(20));
        shared.shutdown();
        assert_eq!(waiter.join().unwrap(), Wake::Shutdown);
    }

    #[test]
    fn interruptible_stops_a_pending_future_on_shutdown() {
        let shared = Arc::new(shared());
        // A future that finishes is passed through.
        assert_eq!(
            pollster::block_on(interruptible(&shared, std::future::ready(7))),
            Some(7)
        );
        let stopper = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(20));
                shared.shutdown();
            })
        };
        assert_eq!(
            pollster::block_on(interruptible(&shared, std::future::pending::<()>())),
            None
        );
        stopper.join().unwrap();
        // After shutdown a pending future is cut off at once.
        assert_eq!(
            pollster::block_on(interruptible(&shared, std::future::pending::<()>())),
            None
        );
    }

    // The handle.

    /// A handle whose worker is `body`, instead of the portal worker.
    fn handle_with_worker(
        body: impl FnOnce(Arc<Shared>) + Send + 'static,
    ) -> (PortalHotkeys, Arc<Shared>) {
        let shared = Arc::new(shared());
        let worker_shared = Arc::clone(&shared);
        let worker = thread::spawn(move || body(worker_shared));
        (
            PortalHotkeys {
                shared: Arc::clone(&shared),
                worker: Some(worker),
            },
            shared,
        )
    }

    fn idle_worker(shared: Arc<Shared>) {
        while shared.wait_for_change(shared.generation(), None) != Wake::Shutdown {}
    }

    #[test]
    fn set_chord_records_the_spelling_and_returns_without_waiting() {
        let (mut hotkeys, shared) = handle_with_worker(idle_worker);
        let started = Instant::now();
        hotkeys
            .set_chord(&chord(&[0xE0, 0xE2, 0xE1], 0x29))
            .unwrap();
        assert!(started.elapsed() < Duration::from_millis(50));
        assert_eq!(shared.snapshot(), (Some("CTRL+ALT+SHIFT+Escape".into()), 1));
        // A chord that can't be spelled is refused and leaves the watched one alone.
        assert!(matches!(
            hotkeys.set_chord(&chord(&[0xE6], 0x29)),
            Err(PlatformError::Unsupported(_))
        ));
        assert_eq!(shared.snapshot(), (Some("CTRL+ALT+SHIFT+Escape".into()), 1));
        // Replacing it is a new generation.
        hotkeys.set_chord(&chord(&[0xE0], 0x3A)).unwrap();
        assert_eq!(shared.snapshot(), (Some("CTRL+F1".into()), 2));
    }

    #[test]
    fn subscribe_goes_through_the_hub_and_is_once() {
        let (mut hotkeys, shared) = handle_with_worker(idle_worker);
        let sink = Arc::new(Collect::default());
        hotkeys.subscribe(sink.clone()).unwrap();
        assert!(hotkeys.subscribe(sink.clone()).is_err());
        shared.hub.activated();
        assert_eq!(sink.events().len(), 1);
    }

    #[test]
    fn dropping_the_handle_stops_the_worker_promptly() {
        let (hotkeys, _shared) = handle_with_worker(idle_worker);
        let started = Instant::now();
        drop(hotkeys);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_dead_worker_is_reported_instead_of_silently_ignoring_the_chord() {
        let (mut hotkeys, _shared) = handle_with_worker(|_| {});
        while !hotkeys.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            thread::yield_now();
        }
        assert!(matches!(
            hotkeys.set_chord(&chord(&[0xE0], 0x29)),
            Err(PlatformError::Backend(_))
        ));
        assert!(matches!(
            hotkeys.subscribe(Arc::new(Collect::default())),
            Err(PlatformError::Backend(_))
        ));
    }

    #[test]
    fn no_portal_on_a_dead_session_bus_is_unsupported() {
        // `new` reaches for the session bus. Run it only where that is the wrapper's dead socket;
        // anywhere else it would probe a live portal.
        let dead_bus = std::env::var("DBUS_SESSION_BUS_ADDRESS")
            .is_ok_and(|address| address.starts_with("unix:path=/nonexistent/"));
        if !dead_bus {
            eprintln!("skipped: the session bus is not the dead test socket");
            return;
        }
        let started = Instant::now();
        let result = PortalHotkeys::new(Arc::new(FakeClock(AtomicU64::new(0))));
        assert!(
            matches!(
                result,
                Err(PlatformError::Unsupported("no GlobalShortcuts portal"))
            ),
            "{result:?}"
        );
        assert!(started.elapsed() < PROBE_BOUND);
    }
}
