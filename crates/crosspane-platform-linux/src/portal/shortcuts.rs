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
//!   30 s). A dialog the user denied or dismissed is not retried automatically (it would nag); the
//!   session stays open and the chord is asked for again on the next `set_chord` with a different
//!   chord.
//! - **Stored bindings.** The desktop keeps a bound shortcut across runs (GNOME stores it in
//!   dconf per app id) and refuses to bind it again. So the session's first request lists the
//!   app's shortcuts (`ListShortcuts`); if `crosspane-release` is there it counts as bound and
//!   `BindShortcuts` is not called. The chord is bound only when the shortcut is absent (first
//!   run, or the desktop forgot it) or when `set_chord` changes it during the run. A failed
//!   rebind never tears down a binding that already works. Every bind failure is logged at `warn`
//!   with the portal's error text.
//! - **Triggers are hints.** The desktop owns the final binding: `preferred_trigger` is only used
//!   when the shortcut is first bound, and the user may rebind it in the desktop's settings. The
//!   portal reports activations for whatever trigger the user ended up with. The portal's
//!   trigger format cannot tell left and right modifiers apart, so both fold into one name (a
//!   chord naming both hands of one modifier is `Unsupported`); right Alt is `Unsupported`
//!   because on many layouts it is AltGr, which the portal's `ALT` does not match.
//! - **Lifetime.** Dropping the handle first emits `Released` if the chord was pressed (and
//!   delivers nothing afterwards), whatever state the worker is in. Then it stops the worker: the
//!   wait is bounded to 2 s, the worker closes the session with a 1.5 s bound, and a worker stuck
//!   in some other portal call is detached and exits when the call returns. A worker that died is
//!   reported by `set_chord` and `subscribe` as an error rather than leaving the chord silently
//!   unwatched.
//! - **Reporting.** A bind failure is logged at `warn` once per chord (a session retried by the
//!   backoff repeats it at `debug`), with the portal's error text and no key contents.
//! - No GlobalShortcuts portal: `new` returns `PlatformError::Unsupported` (`Timeout` if the
//!   portal doesn't answer the probe within 2 s) and `set_chord` never succeeds for a chord it
//!   can't spell (never a silent weakening). The tray and `crosspanectl` keep release and panic
//!   available.
//! - **App id.** The portal identifies a non-sandboxed client by an app id registered for the
//!   process's bus connection. That is process-wide: the agent calls
//!   `portal::register_host_app` once at startup, before any portal call; this module does not.

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
    Activated, BindShortcutsOptions, Deactivated, GlobalShortcuts, ListShortcutsOptions,
    NewShortcut, Shortcut,
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
/// Closing the session on the way out waits this long for the portal (less than `JOIN_BOUND`).
const CLOSE_BOUND: Duration = Duration::from_millis(1500);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A session that stayed bound this long was healthy: the next loss starts the backoff over.
const HEALTHY_AFTER: Duration = Duration::from_secs(30);

/// GlobalHotkeys through the GlobalShortcuts portal.
pub struct PortalHotkeys {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    /// How long `drop` waits for the worker (`JOIN_BOUND`; tests shorten it).
    join_bound: Duration,
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
                join_bound: JOIN_BOUND,
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
        // A pressed chord is released here, whether or not the worker (possibly stuck in a portal
        // call) ever gets to it.
        self.shared.hub.close();
        if let Some(worker) = self.worker.take() {
            let deadline = Instant::now() + self.join_bound;
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BindFailure {
    /// The desktop said no (the user dismissed the dialog, or its policy refused, or it considers
    /// the shortcut already bound): don't ask again until the chord changes.
    Denied,
    /// Anything else (portal restarting, bus error): end the session and retry with backoff.
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

/// What the desktop reports for our shortcut (from `ListShortcuts` or the `BindShortcuts` answer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShortcutState {
    /// Not in the list: never bound for this app, or the desktop forgot it.
    Absent,
    /// In the list. `has_trigger` is false if the user cleared its trigger in the desktop's
    /// settings (the shortcut exists but nothing can activate it).
    Present { has_trigger: bool },
}

/// The state of our shortcut among `(id, trigger_description)` pairs.
fn shortcut_state<'a>(entries: impl IntoIterator<Item = (&'a str, &'a str)>) -> ShortcutState {
    entries
        .into_iter()
        .find(|(id, _)| *id == SHORTCUT_ID)
        .map_or(ShortcutState::Absent, |(_, trigger)| {
            ShortcutState::Present {
                has_trigger: !trigger.is_empty(),
            }
        })
}

/// Does the shortcut still have to be bound? Only if the desktop doesn't already hold it: a
/// stored shortcut, even one whose trigger the user cleared, is not bound again.
fn needs_bind(state: ShortcutState) -> bool {
    state == ShortcutState::Absent
}

/// What the session does next after a bind request ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BindNext {
    /// Nothing more to ask; keep serving signals.
    Idle,
    /// The chord changed while the request was in flight: ask again for the newest chord.
    Rebind,
    /// Nothing is bound and the failure is not the desktop's refusal: end the session and retry.
    EndSession,
}

/// Bookkeeping for the session's bind requests: at most one in flight, the newest chord wins, and
/// a binding the desktop already holds is never torn down by a later failure.
#[derive(Debug, Default)]
struct Binder {
    /// The desktop holds a binding for our shortcut: found by `ListShortcuts` or bound.
    has_binding: bool,
    /// The chord generation of the request in flight.
    in_flight: Option<u64>,
    /// The newest chord generation seen.
    wanted: u64,
    /// A request has been started in this session.
    started: bool,
    /// The chord generation of the request that finished last.
    last_finished: Option<u64>,
}

/// Remembers the chord generation a bind failure was last reported for, so a failure that repeats
/// for the same chord (a session retried by the backoff) is reported at `warn` only once.
#[derive(Debug, Default)]
struct WarnedChord(Option<u64>);

impl WarnedChord {
    /// True the first time `generation` is reported.
    fn first(&mut self, generation: u64) -> bool {
        self.0.replace(generation) != Some(generation)
    }
}

impl Binder {
    /// A request for `generation` starts now. Returns whether it should look the shortcut up
    /// first: only the session's first request does, because a shortcut the desktop stored from
    /// an earlier run is already bound and must not be bound again (GNOME refuses that).
    fn begin(&mut self, generation: u64) -> bool {
        let first = !self.started;
        self.started = true;
        self.in_flight = Some(generation);
        self.wanted = generation;
        first
    }

    /// The chord changed to `generation`. Returns whether to start a request now; if one is in
    /// flight (it may be waiting on the user) the newer chord is asked for when it ends.
    fn changed(&mut self, generation: u64) -> bool {
        self.wanted = generation;
        self.in_flight.is_none()
    }

    fn has_binding(&self) -> bool {
        self.has_binding
    }

    /// The chord generation of the request that finished last.
    fn last_finished(&self) -> Option<u64> {
        self.last_finished
    }

    /// The request ended. `Ok` means the desktop holds the binding (found or bound).
    fn finished(&mut self, result: Result<(), BindFailure>) -> BindNext {
        let done = self.in_flight.take();
        self.last_finished = done;
        let retry_needed = done.is_some_and(|done| done != self.wanted);
        match result {
            Ok(()) => self.has_binding = true,
            // A failed rebind keeps the binding that already works. Without one, only a
            // refusal keeps the session: signals for a binding the desktop stored itself still
            // arrive, and the session is the place the next chord is bound.
            Err(BindFailure::Denied) => {}
            Err(BindFailure::Retry) if self.has_binding => {}
            Err(BindFailure::Retry) => return BindNext::EndSession,
        }
        if retry_needed {
            BindNext::Rebind
        } else {
            BindNext::Idle
        }
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
    /// `close` ran: no further `Pressed`.
    closed: bool,
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
        if state.closed {
            return;
        }
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

    /// The handle is going away: release a pressed chord now (not whenever a stuck worker gets
    /// around to it) and deliver nothing afterwards, so no `Pressed` can follow that `Released`.
    /// Idempotent.
    fn close(&self) {
        let mut state = lock(&self.state);
        let event = state.pairing.lost(self.clock.now());
        state.closed = true;
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

/// A one-shot clock for async code that has no timer of its own: a helper thread waits out the
/// limit and wakes the task.
#[derive(Default)]
struct Timer {
    state: Mutex<TimerState>,
    condvar: Condvar,
}

#[derive(Default)]
struct TimerState {
    fired: bool,
    cancelled: bool,
    waker: Option<Waker>,
}

impl Timer {
    /// The helper thread's body: wait `limit` (or until cancelled), then fire.
    fn run(&self, limit: Duration) {
        let mut state = self
            .condvar
            .wait_timeout_while(lock(&self.state), limit, |s| !s.cancelled)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
        if state.cancelled {
            return;
        }
        state.fired = true;
        let waker = state.waker.take();
        drop(state);
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn poll_fired(&self, cx: &mut Context<'_>) -> bool {
        let mut state = lock(&self.state);
        if state.fired {
            return true;
        }
        state.waker = Some(cx.waker().clone());
        false
    }

    /// Let the helper thread go without firing.
    fn cancel(&self) {
        lock(&self.state).cancelled = true;
        self.condvar.notify_all();
    }
}

/// Run `fut` for at most `limit`; `None` if it didn't finish. If the clock can't be started the
/// future is not run at all, so the bound holds either way.
async fn within<F: Future>(limit: Duration, fut: F) -> Option<F::Output> {
    let timer = Arc::new(Timer::default());
    let clock = Arc::clone(&timer);
    thread::Builder::new()
        .name("crosspane-portal-timer".into())
        .spawn(move || clock.run(limit))
        .ok()?;
    let mut fut = pin!(fut);
    let output = poll_fn(|cx| {
        if let Poll::Ready(output) = fut.as_mut().poll(cx) {
            return Poll::Ready(Some(output));
        }
        if timer.poll_fired(cx) {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    })
    .await;
    timer.cancel();
    output
}

/// Why a session ended.
#[derive(Debug)]
enum SessionEnd {
    Shutdown,
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
    let mut warned = WarnedChord::default();
    loop {
        if !shared.wait_for_trigger() {
            return;
        }
        let mut bound_at = None;
        let end = zbus::block_on(run_session(shared, &mut bound_at, &mut warned));
        // Whatever ended the session, a press in flight will never see its release.
        shared.hub.lost();
        match end {
            SessionEnd::Shutdown => return,
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
/// when the first bind succeeded; `warned` remembers which chord's bind failure was reported.
async fn run_session(
    shared: &Shared,
    bound_at: &mut Option<Instant>,
    warned: &mut WarnedChord,
) -> SessionEnd {
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

    let Err(end) = serve(shared, &portal, &session, signals, owner, bound_at, warned).await;
    match &end {
        // Close for good so no dialog or shortcut registration outlives us. Bounded: a portal
        // that has stopped answering must not hold the worker (and with it the handle's drop).
        SessionEnd::Shutdown => {
            let _ = within(CLOSE_BOUND, session.close()).await;
        }
        SessionEnd::Lost {
            session_gone: false,
            ..
        } => {
            let _ = interruptible(shared, within(CLOSE_BOUND, session.close())).await;
        }
        SessionEnd::Lost { .. } => {}
    }
    end
}

/// What a bind request found out.
struct BindOutcome {
    /// The shortcuts the desktop reported (all of this app's, from the list or the bind answer).
    shortcuts: Vec<Shortcut>,
    /// The shortcut was already bound (stored by the desktop from an earlier run), so
    /// `BindShortcuts` was not called.
    existing: bool,
}

type BindFuture<'a> = Pin<Box<dyn Future<Output = Result<BindOutcome, AshpdError>> + 'a>>;

/// Make sure the desktop holds the shortcut. With `lookup_first` (the session's first request) the
/// app's shortcuts are listed and, if ours is among them, it counts as bound and `BindShortcuts`
/// is not called: the desktop stores a bound shortcut across runs and refuses to bind it again.
/// Otherwise (first run, a changed chord, or a failed lookup) the chord is bound.
fn start_bind<'a>(
    portal: &'a GlobalShortcuts,
    session: &'a Session<GlobalShortcuts>,
    trigger: String,
    lookup_first: bool,
) -> BindFuture<'a> {
    Box::pin(async move {
        if lookup_first {
            let listed = match portal
                .list_shortcuts(session, ListShortcutsOptions::default())
                .await
            {
                Ok(request) => request.response(),
                Err(error) => Err(error),
            };
            match listed {
                Ok(listed) => {
                    let shortcuts = listed.shortcuts().to_vec();
                    let state = shortcut_state(shortcuts.iter().map(entry));
                    if !needs_bind(state) {
                        return Ok(BindOutcome {
                            shortcuts,
                            existing: true,
                        });
                    }
                }
                // Not fatal: bind as if nothing were stored.
                Err(error) => {
                    tracing::warn!(%error, "listing the desktop's shortcuts failed; binding the release chord instead");
                }
            }
        }
        let shortcuts =
            [NewShortcut::new(SHORTCUT_ID, SHORTCUT_DESCRIPTION)
                .preferred_trigger(trigger.as_str())];
        let request = portal
            .bind_shortcuts(session, &shortcuts, None, BindShortcutsOptions::default())
            .await?;
        let bound = request.response()?;
        Ok(BindOutcome {
            shortcuts: bound.shortcuts().to_vec(),
            existing: false,
        })
    })
}

/// A shortcut as `(id, trigger_description)`.
fn entry(shortcut: &Shortcut) -> (&str, &str) {
    (shortcut.id(), shortcut.trigger_description())
}

/// Start the next bind request for the newest chord.
fn begin_bind<'a>(
    shared: &Shared,
    portal: &'a GlobalShortcuts,
    session: &'a Session<GlobalShortcuts>,
    binder: &mut Binder,
    seen: &mut u64,
) -> Option<BindFuture<'a>> {
    let (trigger, generation) = shared.snapshot();
    let trigger = trigger?;
    *seen = generation;
    let lookup_first = binder.begin(generation);
    Some(start_bind(portal, session, trigger, lookup_first))
}

enum Event {
    Shutdown,
    Changed,
    SessionClosed,
    OwnerChanged,
    Signal(Option<zbus::Message>),
    Bind(Result<BindOutcome, AshpdError>),
}

/// Bind the chord and serve the session's signals. Never returns normally: every exit is an end.
async fn serve(
    shared: &Shared,
    portal: &GlobalShortcuts,
    session: &Session<GlobalShortcuts>,
    signals: impl Stream<Item = zbus::Message>,
    owner: impl Stream,
    bound_at: &mut Option<Instant>,
    warned: &mut WarnedChord,
) -> Result<Infallible, SessionEnd> {
    let closed = step(shared, "watch the session", false, session.receive_closed()).await?;
    let mut closed = pin!(closed);
    let mut signals = pin!(signals);
    let mut owner = pin!(owner);

    // `seen` is the newest chord generation acted on; `binder` tracks the request in flight.
    let mut seen = 0;
    let mut binder = Binder::default();
    let mut bind = begin_bind(shared, portal, session, &mut binder, &mut seen);
    if bind.is_none() {
        return Err(SessionEnd::Lost {
            reason: "no release chord set".into(),
            session_gone: false,
        });
    }

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
                let (_, generation) = shared.snapshot();
                seen = generation;
                // The replaced chord's release may never arrive.
                shared.hub.lost();
                // A request in flight finishes first (it may be waiting on the user); the newer
                // chord is asked for right after it.
                if binder.changed(generation) {
                    bind = begin_bind(shared, portal, session, &mut binder, &mut seen);
                }
            }
            Event::Bind(result) => {
                bind = None;
                let (next, failure) = match result {
                    Ok(outcome) => {
                        bound_at.get_or_insert_with(Instant::now);
                        log_outcome(&outcome);
                        (binder.finished(Ok(())), None)
                    }
                    Err(error) => {
                        let class = classify_bind_error(&error);
                        let next = binder.finished(Err(class));
                        // Once per chord at `warn`; a retried session repeats at `debug`.
                        let first = binder.last_finished().is_some_and(|g| warned.first(g));
                        if first {
                            tracing::warn!(
                                %error,
                                ?class,
                                has_binding = binder.has_binding(),
                                "binding the release chord failed"
                            );
                            if !binder.has_binding() && next != BindNext::EndSession {
                                tracing::warn!(
                                    "the Crosspane release chord is not bound; the session stays open in case the desktop holds a binding for it, and changing the chord asks again"
                                );
                            }
                        } else {
                            tracing::debug!(
                                %error,
                                ?class,
                                has_binding = binder.has_binding(),
                                "binding the release chord failed again"
                            );
                        }
                        (next, Some(error))
                    }
                };
                match next {
                    BindNext::Idle => {}
                    BindNext::Rebind => {
                        bind = begin_bind(shared, portal, session, &mut binder, &mut seen);
                    }
                    BindNext::EndSession => {
                        let reason = failure.map_or_else(
                            || "bind the release chord failed".to_string(),
                            |error| format!("bind the release chord: {error}"),
                        );
                        return Err(SessionEnd::Lost {
                            reason,
                            session_gone: false,
                        });
                    }
                }
            }
        }
    }
}

/// Say what the desktop holds for the shortcut. A missing or trigger-less shortcut leaves the
/// emergency control unreachable from the keyboard, so that is a warning. The trigger text is the
/// chord's configuration, never typed input.
fn log_outcome(outcome: &BindOutcome) {
    let state = shortcut_state(outcome.shortcuts.iter().map(entry));
    match state {
        ShortcutState::Present { has_trigger: true } => {
            let trigger = outcome
                .shortcuts
                .iter()
                .map(entry)
                .find(|(id, _)| *id == SHORTCUT_ID)
                .map_or("", |(_, trigger)| trigger);
            tracing::debug!(
                trigger,
                existing = outcome.existing,
                "release chord is bound by the desktop"
            );
        }
        ShortcutState::Present { has_trigger: false } => tracing::warn!(
            existing = outcome.existing,
            "the desktop holds the Crosspane release shortcut without a trigger; assign one in its shortcut settings"
        ),
        ShortcutState::Absent => tracing::warn!(
            existing = outcome.existing,
            "the desktop did not report the Crosspane release shortcut as bound"
        ),
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

    // Stored bindings and the bind requests.

    #[test]
    fn a_stored_shortcut_is_found_among_the_apps_shortcuts() {
        assert_eq!(shortcut_state([]), ShortcutState::Absent);
        assert_eq!(shortcut_state([("other", "Ctrl+A")]), ShortcutState::Absent);
        assert_eq!(
            shortcut_state([("crosspane-release", "Shift+Ctrl+Alt+Esc")]),
            ShortcutState::Present { has_trigger: true }
        );
        assert_eq!(
            shortcut_state([("other", ""), ("crosspane-release", "Ctrl+B"), ("x", "")]),
            ShortcutState::Present { has_trigger: true }
        );
        // The user cleared the trigger: still stored.
        assert_eq!(
            shortcut_state([("crosspane-release", "")]),
            ShortcutState::Present { has_trigger: false }
        );
    }

    #[test]
    fn only_an_absent_shortcut_is_bound() {
        assert!(needs_bind(ShortcutState::Absent));
        assert!(!needs_bind(ShortcutState::Present { has_trigger: true }));
        assert!(!needs_bind(ShortcutState::Present { has_trigger: false }));
    }

    #[test]
    fn only_the_sessions_first_request_looks_the_shortcut_up() {
        let mut binder = Binder::default();
        assert!(binder.begin(1));
        assert_eq!(binder.finished(Ok(())), BindNext::Idle);
        assert!(binder.changed(2));
        assert!(!binder.begin(2));
        assert_eq!(binder.finished(Ok(())), BindNext::Idle);
    }

    #[test]
    fn a_shortcut_stored_by_an_earlier_run_counts_as_bound_without_a_rebind() {
        // The first request found it listed: an Ok result, no chord change pending.
        let mut binder = Binder::default();
        assert!(binder.begin(1));
        assert!(!binder.has_binding());
        assert_eq!(binder.finished(Ok(())), BindNext::Idle);
        assert!(binder.has_binding());
    }

    #[test]
    fn a_chord_change_during_a_request_is_asked_after_it_ends() {
        let mut binder = Binder::default();
        binder.begin(1);
        // Never two requests at once: the dialog may be waiting on the user.
        assert!(!binder.changed(2));
        assert!(!binder.changed(3));
        assert_eq!(binder.finished(Ok(())), BindNext::Rebind);
        binder.begin(3);
        assert_eq!(binder.finished(Ok(())), BindNext::Idle);
    }

    #[test]
    fn a_chord_change_while_idle_starts_a_request() {
        let mut binder = Binder::default();
        binder.begin(1);
        binder.finished(Ok(()));
        assert!(binder.changed(2));
        binder.begin(2);
        assert!(!binder.changed(3));
    }

    #[test]
    fn a_refusal_without_a_binding_keeps_the_session_for_the_next_chord() {
        let mut binder = Binder::default();
        binder.begin(1);
        assert_eq!(binder.finished(Err(BindFailure::Denied)), BindNext::Idle);
        assert!(!binder.has_binding());
        // Changing the chord asks again, in the same session.
        assert!(binder.changed(2));
        binder.begin(2);
        assert_eq!(binder.finished(Ok(())), BindNext::Idle);
        assert!(binder.has_binding());
    }

    #[test]
    fn a_refusal_after_the_chord_moved_on_asks_for_the_newer_chord() {
        let mut binder = Binder::default();
        binder.begin(1);
        binder.changed(2);
        assert_eq!(binder.finished(Err(BindFailure::Denied)), BindNext::Rebind);
    }

    #[test]
    fn a_bus_failure_without_a_binding_ends_the_session_for_a_retry() {
        let mut binder = Binder::default();
        binder.begin(1);
        assert_eq!(
            binder.finished(Err(BindFailure::Retry)),
            BindNext::EndSession
        );
    }

    #[test]
    fn a_failed_rebind_never_tears_down_a_working_binding() {
        for failure in [BindFailure::Denied, BindFailure::Retry] {
            let mut binder = Binder::default();
            binder.begin(1);
            binder.finished(Ok(()));
            assert!(binder.changed(2));
            binder.begin(2);
            assert_eq!(binder.finished(Err(failure)), BindNext::Idle, "{failure:?}");
            assert!(binder.has_binding());
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
                join_bound: JOIN_BOUND,
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
    fn dropping_while_pressed_releases_even_if_the_worker_is_stuck() {
        // A worker stuck in a portal call: it never looks at the shutdown flag.
        let (unstick, stuck) = mpsc::channel::<()>();
        let (mut hotkeys, shared) = handle_with_worker(move |_| {
            let _ = stuck.recv();
        });
        hotkeys.join_bound = Duration::from_millis(30);
        let sink = Arc::new(Collect::default());
        hotkeys.subscribe(sink.clone()).unwrap();
        shared.hub.activated();
        assert_eq!(sink.events(), [HotkeyEvent::Pressed { at: at(0) }]);

        drop(hotkeys);
        assert_eq!(
            sink.events(),
            [
                HotkeyEvent::Pressed { at: at(0) },
                HotkeyEvent::Released { at: at(0) },
            ]
        );
        // The stuck worker waking up later can't undo that: no further press is delivered.
        shared.hub.activated();
        shared.hub.deactivated();
        shared.hub.lost();
        assert_eq!(sink.events().len(), 2);
        unstick.send(()).unwrap();
    }

    #[test]
    fn closing_the_hub_releases_once_and_stops_further_presses() {
        let (clock, hub, sink) = hub();
        hub.subscribe(sink.clone()).unwrap();
        clock.set(1);
        hub.activated();
        clock.set(2);
        hub.close();
        clock.set(3);
        hub.close();
        hub.activated();
        assert_eq!(
            sink.events(),
            [
                HotkeyEvent::Pressed { at: at(1) },
                HotkeyEvent::Released { at: at(2) },
            ]
        );
    }

    #[test]
    fn closing_the_hub_with_nothing_pressed_sends_nothing() {
        let (_clock, hub, sink) = hub();
        hub.subscribe(sink.clone()).unwrap();
        hub.close();
        assert!(sink.events().is_empty());
    }

    #[test]
    fn within_passes_a_finished_future_and_cuts_off_a_pending_one() {
        assert_eq!(
            pollster::block_on(within(Duration::from_secs(5), std::future::ready(3))),
            Some(3)
        );
        let started = Instant::now();
        assert_eq!(
            pollster::block_on(within(
                Duration::from_millis(30),
                std::future::pending::<()>()
            )),
            None
        );
        assert!(started.elapsed() >= Duration::from_millis(30));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_bind_failure_is_reported_at_warn_once_per_chord() {
        let mut warned = WarnedChord::default();
        assert!(warned.first(1));
        // A retried session fails for the same chord again.
        assert!(!warned.first(1));
        assert!(!warned.first(1));
        // A new chord is reported afresh.
        assert!(warned.first(2));
        assert!(!warned.first(2));

        // The generation reported is the one of the request that failed, even if the chord
        // moved on while it was in flight.
        let mut binder = Binder::default();
        binder.begin(4);
        binder.changed(5);
        binder.finished(Err(BindFailure::Denied));
        assert_eq!(binder.last_finished(), Some(4));
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
