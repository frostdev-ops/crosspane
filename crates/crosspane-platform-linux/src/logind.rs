//! Read-only logind session observation, with a same-user `/proc` locker cross-check.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crosspane_platform::{
    EventSink, IoGate, LockState, PlatformError, SessionEvent, SessionEvents, SessionState,
};
use zbus::blocking::{Connection, MessageIterator};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::{MatchRule, Message};

use crate::hyprland::ipc::HyprIpc;

const LOGIND: &str = "org.freedesktop.login1";
const MANAGER_PATH: &str = "/org/freedesktop/login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";
const SESSION: &str = "org.freedesktop.login1.Session";
const USER: &str = "org.freedesktop.login1.User";
const NO_SUCH_SESSION: &str = "org.freedesktop.login1.NoSuchSession";
const NO_SESSION_FOR_PID: &str = "org.freedesktop.login1.NoSessionForPID";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const DBUS: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const POLL_INTERVAL: Duration = Duration::from_millis(500);
const CALL_TIMEOUT: Duration = Duration::from_millis(100);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(200);
const UNKNOWN: SessionState = SessionState {
    lock: LockState::Unknown,
    active: None,
};

type Properties = HashMap<String, OwnedValue>;

/// Observes this graphical session without changing logind or compositor state.
///
/// Observation starts at construction, so the gate is maintained even before subscription.
/// Dropping the backend closes its side of the gate and joins its observation threads.
pub struct LogindSession {
    monitor: Arc<Monitor>,
    worker: Option<JoinHandle<()>>,
}

impl fmt::Debug for LogindSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LogindSession")
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

/// Which desktop screen-locker service supplies lock evidence next to logind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockerEvidence {
    /// Scan for external locker processes (the Hyprland path).
    ProcessScan,
    /// `org.gnome.ScreenSaver` `GetActive` / `ActiveChanged`.
    GnomeScreenSaver,
    /// `org.freedesktop.ScreenSaver` (KDE).
    FreedesktopScreenSaver,
}

impl LogindSession {
    /// Find this graphical session and read its state. The session is, in order, the one that owns
    /// this process (`GetSessionByPID`), the one `$XDG_SESSION_ID` names, or the user's logind
    /// `Display` session, only when that is the user's single graphical session on a seat (a
    /// user-manager service has neither of the first two, and `Display` cannot tell two graphical
    /// sessions apart). With none of them the gate stays closed.
    /// The required locker check uses `/proc` and works without a Hyprland IPC endpoint.
    pub fn new(gate: Arc<IoGate>, _ipc: Option<HyprIpc>) -> Result<LogindSession, PlatformError> {
        let monitor = Arc::new(Monitor::new(gate));
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let observed = monitor.clone();
        let worker = thread::Builder::new()
            .name("crosspane-logind".into())
            .spawn(move || observe(observed, ready_tx))
            .map_err(|e| PlatformError::Backend(format!("spawn logind observer: {e}")))?;
        let session = Self {
            monitor,
            worker: Some(worker),
        };
        // Reserve time for bounded worker shutdown on a failed startup.
        ready_rx
            .recv_timeout(Duration::from_millis(1500))
            .map_err(|e| match e {
                mpsc::RecvTimeoutError::Timeout => PlatformError::Timeout,
                mpsc::RecvTimeoutError::Disconnected => {
                    PlatformError::Backend("logind observer stopped during startup".into())
                }
            })?;
        Ok(session)
    }

    /// Like [`LogindSession::new`], with the desktop's own screen locker as additional lock
    /// evidence (GNOME ScreenSaver or freedesktop ScreenSaver, no Hyprland IPC).
    pub fn with_locker(
        gate: Arc<IoGate>,
        locker: LockerEvidence,
    ) -> Result<LogindSession, PlatformError> {
        // lead: replaced by the G1.3 lock-evidence implementation
        let _ = locker;
        LogindSession::new(gate, None)
    }
}

impl SessionEvents for LogindSession {
    fn state(&self) -> SessionState {
        self.monitor.lock().state
    }

    fn subscribe(&mut self, sink: Arc<dyn EventSink<SessionEvent>>) -> Result<(), PlatformError> {
        let mut inner = self.monitor.lock();
        if inner.sink.is_some() || inner.pending_subscription.is_some() {
            return Err(PlatformError::Backend(
                "SessionEvents::subscribe called twice".into(),
            ));
        }
        if inner.stopped {
            return Err(PlatformError::Backend("logind observer has stopped".into()));
        }
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        inner.pending_subscription = Some((sink, ready_tx));
        self.monitor.changed.notify_all();
        drop(inner);
        match ready_rx.recv_timeout(Duration::from_millis(1500)) {
            Ok(()) => Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.monitor.lock().pending_subscription = None;
                Err(PlatformError::Timeout)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(PlatformError::Backend("logind observer has stopped".into()))
            }
        }
    }
}

impl Drop for LogindSession {
    fn drop(&mut self) {
        let mut inner = self.monitor.lock();
        inner.stopped = true;
        self.monitor.gate.set_session_permits(false);
        self.monitor.changed.notify_all();
        drop(inner);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Monitor {
    gate: Arc<IoGate>,
    inner: Mutex<Observation>,
    changed: Condvar,
}

struct Observation {
    state: SessionState,
    sink: Option<Arc<dyn EventSink<SessionEvent>>>,
    pending_subscription: Option<(Arc<dyn EventSink<SessionEvent>>, mpsc::SyncSender<()>)>,
    revision: u64,
    connected: bool,
    stopped: bool,
    sleeping: bool,
    lock_requested: bool,
    lock_seen: bool,
    fresh_event: bool,
}

#[derive(Clone, Copy, Debug)]
enum Signal {
    Lock,
    Unlock,
    Properties {
        locked: Option<bool>,
        active: Option<bool>,
        invalidated: bool,
    },
    Sleep(bool),
    Lost,
}

impl Monitor {
    fn new(gate: Arc<IoGate>) -> Self {
        gate.set_session_permits(false);
        Self {
            gate,
            inner: Mutex::new(Observation {
                state: UNKNOWN,
                sink: None,
                pending_subscription: None,
                revision: 0,
                connected: false,
                stopped: false,
                sleeping: false,
                lock_requested: false,
                lock_seen: false,
                fresh_event: false,
            }),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Observation> {
        self.inner.lock().unwrap_or_else(|poisoned| {
            self.gate.set_session_permits(false);
            let mut inner = poisoned.into_inner();
            inner.state = UNKNOWN;
            inner.stopped = true;
            inner
        })
    }

    fn publish(&self, inner: &mut Observation, state: SessionState, force: bool) {
        self.gate
            .set_session_permits(!inner.stopped && !inner.sleeping && state.permits_io());
        if inner.state != state || force {
            inner.state = state;
            if !inner.stopped
                && let Some(sink) = &inner.sink
            {
                sink.send(SessionEvent::State(state));
            }
        }
    }

    fn signal(&self, signal: Signal) {
        let mut inner = self.lock();
        self.apply_signal(&mut inner, signal);
    }

    fn apply_signal(&self, inner: &mut Observation, signal: Signal) {
        if inner.stopped {
            return;
        }
        // Every relevant signal invalidates an in-flight read. Signals never open the gate.
        inner.revision = inner.revision.wrapping_add(1);
        self.gate.set_session_permits(false);
        let mut state = inner.state;
        match signal {
            Signal::Lock => {
                inner.lock_requested = true;
                state.lock = LockState::Locked;
            }
            Signal::Unlock => {
                inner.lock_requested = false;
                inner.lock_seen = false;
                state.lock = LockState::Unknown;
            }
            Signal::Properties {
                locked,
                active,
                invalidated,
            } => {
                if invalidated {
                    state = UNKNOWN;
                }
                if let Some(locked) = locked {
                    state.lock = if locked {
                        inner.lock_seen = true;
                        LockState::Locked
                    } else {
                        inner.lock_requested = false;
                        LockState::Unknown // A fresh locker scan is still required.
                    };
                }
                if let Some(active) = active {
                    state.active = Some(active);
                }
            }
            Signal::Sleep(sleeping) => {
                inner.sleeping = sleeping;
                inner.fresh_event = true;
                if let Some(sink) = &inner.sink {
                    sink.send(if sleeping {
                        SessionEvent::WillSleep
                    } else {
                        SessionEvent::Woke
                    });
                }
                if state.lock == LockState::Unlocked {
                    state.lock = LockState::Unknown;
                }
            }
            Signal::Lost => {
                inner.connected = false;
                state = UNKNOWN;
            }
        }
        // A permissive property notification must not reopen before revalidation.
        if state.permits_io() {
            state.lock = LockState::Unknown;
        }
        self.publish(inner, state, false);
        self.changed.notify_all();
    }
}

#[derive(Clone, Copy, Debug)]
struct Reading {
    locked_hint: Option<bool>,
    active: Option<bool>,
    locker: Option<bool>,
    sleeping: Option<bool>,
    bus_ok: bool,
}

impl Reading {
    const UNKNOWN: Self = Self {
        locked_hint: None,
        active: None,
        locker: None,
        sleeping: None,
        bus_ok: false,
    };
}

/// Private injection point: unit tests exercise the same read/commit path as logind.
trait StateSource {
    fn read(&mut self) -> Reading;
}

fn lock_state(reading: Reading, lock_requested: bool) -> LockState {
    if !reading.bus_ok || reading.locked_hint.is_none() || reading.active.is_none() {
        LockState::Unknown
    } else if lock_requested || reading.locked_hint == Some(true) || reading.locker == Some(true) {
        LockState::Locked
    } else if reading.locker == Some(false) {
        LockState::Unlocked
    } else {
        LockState::Unknown
    }
}

/// False means a signal superseded this read; the observer must read again immediately.
fn refresh(monitor: &Monitor, source: &mut dyn StateSource) -> bool {
    let revision = monitor.lock().revision;
    let reading = source.read();
    let mut inner = monitor.lock();
    if inner.stopped || inner.revision != revision {
        return false;
    }
    if !reading.bus_ok {
        inner.connected = false;
    }
    if reading.bus_ok
        && let Some(sleeping) = reading.sleeping
        && sleeping != inner.sleeping
    {
        // PreparingForSleep corrects a missed sleep/wake signal. In particular, discard the
        // read that discovers a wake: reopening still requires a read after delivering Woke.
        monitor.apply_signal(&mut inner, Signal::Sleep(sleeping));
        return false;
    }
    if reading.locked_hint == Some(true) || reading.locker == Some(true) {
        inner.lock_seen = true;
    } else if reading.bus_ok
        && reading.locked_hint == Some(false)
        && reading.locker == Some(false)
        && inner.lock_seen
    {
        // Correct a missed Unlock once a previously observed lock has gone away.
        inner.lock_requested = false;
        inner.lock_seen = false;
    }
    let mut state = SessionState {
        lock: if inner.connected {
            lock_state(reading, inner.lock_requested)
        } else {
            LockState::Unknown
        },
        active: reading.active,
    };
    if inner.sleeping && state.lock == LockState::Unlocked {
        state.lock = LockState::Unknown;
    }
    let force = inner.fresh_event;
    inner.fresh_event = false;
    monitor.publish(&mut inner, state, force);
    true
}

struct LiveSource {
    connection: Connection,
    session: OwnedObjectPath,
    uid: u32,
    signals: Option<JoinHandle<()>>,
}

impl LiveSource {
    fn connect(monitor: &Arc<Monitor>) -> Result<Self, PlatformError> {
        // The blocking builder's method timeout does not cover authentication. Bound that one
        // future explicitly, then use the blocking API for all D-Bus calls and signal iteration.
        let connection: Connection = bounded(
            zbus::connection::Builder::system()
                .map_err(bus_error)?
                .method_timeout(CALL_TIMEOUT)
                .build(),
            CONNECT_TIMEOUT,
        )?
        .map_err(bus_error)?
        .into();
        let uid = std::fs::metadata("/proc/self")
            .map_err(|e| PlatformError::Backend(format!("read process owner: {e}")))?
            .uid();
        let mut messages = MessageIterator::from(&connection);
        let owner: String = call(
            &connection,
            DBUS,
            DBUS_PATH,
            DBUS,
            "GetNameOwner",
            &(LOGIND,),
        )?;
        // The iterator is active before AddMatch, so setup cannot lose an early safety signal.
        for rule in [
            MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .sender(owner.as_str())
                .map_err(bus_error)?
                .path_namespace(MANAGER_PATH)
                .map_err(bus_error)?
                .build(),
            MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .sender(DBUS)
                .map_err(bus_error)?
                .interface(DBUS)
                .map_err(bus_error)?
                .member("NameOwnerChanged")
                .map_err(bus_error)?
                .add_arg(LOGIND)
                .map_err(bus_error)?
                .build(),
        ] {
            let _: () = call(
                &connection,
                DBUS,
                DBUS_PATH,
                DBUS,
                "AddMatch",
                &(rule.to_string(),),
            )?;
        }
        let current_owner: String = call(
            &connection,
            DBUS,
            DBUS_PATH,
            DBUS,
            "GetNameOwner",
            &(LOGIND,),
        )?;
        if owner != current_owner {
            return Err(PlatformError::Backend(
                "logind owner changed during setup".into(),
            ));
        }
        let session = find_session(&connection, uid)?;
        let path = session.clone();
        let observed = monitor.clone();
        monitor.lock().connected = true;
        let signals = thread::Builder::new()
            .name("crosspane-logind-signals".into())
            .spawn(move || {
                for message in &mut messages {
                    if observed.lock().stopped {
                        return;
                    }
                    match message.and_then(|message| parse_signal(&message, &path, &owner)) {
                        Ok(Some(signal)) => {
                            observed.signal(signal);
                            if matches!(signal, Signal::Lost) {
                                return;
                            }
                        }
                        Ok(None) => {}
                        Err(_) => {
                            observed.signal(Signal::Lost);
                            return;
                        }
                    }
                }
                observed.signal(Signal::Lost);
            })
            .map_err(|e| PlatformError::Backend(format!("spawn logind signals: {e}")))?;
        Ok(Self {
            connection,
            session,
            uid,
            signals: Some(signals),
        })
    }
}

impl StateSource for LiveSource {
    fn read(&mut self) -> Reading {
        let properties = properties(&self.connection, &self.session);
        match properties {
            Ok(properties) => {
                let locked_hint = boolean(&properties, "LockedHint");
                let active = boolean(&properties, "Active");
                let sleeping: Result<OwnedValue, _> = call(
                    &self.connection,
                    LOGIND,
                    MANAGER_PATH,
                    PROPERTIES,
                    "Get",
                    &(MANAGER, "PreparingForSleep"),
                );
                let sleeping = sleeping.ok().and_then(|v| bool::try_from(v).ok());
                Reading {
                    locked_hint,
                    active,
                    locker: locker_present(self.uid).ok(),
                    sleeping,
                    bus_ok: locked_hint.is_some() && active.is_some() && sleeping.is_some(),
                }
            }
            Err(_) => Reading::UNKNOWN,
        }
    }
}

impl Drop for LiveSource {
    fn drop(&mut self) {
        // Closing this dedicated connection wakes blocking signal iteration, including on drop.
        let _ = self.connection.clone().close();
        if let Some(signals) = self.signals.take() {
            let _ = signals.join();
        }
    }
}

fn observe(monitor: Arc<Monitor>, ready: mpsc::SyncSender<()>) {
    // Even an unexpected worker panic must close the gate.
    struct CloseGate(Arc<IoGate>);
    impl Drop for CloseGate {
        fn drop(&mut self) {
            self.0.set_session_permits(false);
        }
    }
    let _close = CloseGate(monitor.gate.clone());
    let mut source: Option<LiveSource> = None;
    let mut ready = Some(ready);
    let mut next_poll = Instant::now();
    loop {
        let mut inner = monitor.lock();
        if inner.stopped {
            return;
        }
        if let Some((sink, ready)) = inner.pending_subscription.take() {
            // All events, including the initial state, are delivered on backend threads. The
            // same mutex serializes this handoff with signal delivery and subsequent reads.
            sink.send(SessionEvent::State(inner.state));
            inner.sink = Some(sink);
            let _ = ready.send(());
        }
        let connected = inner.connected;
        drop(inner);
        if !connected {
            drop(source.take());
            source = LiveSource::connect(&monitor).ok();
            if source.is_none() {
                monitor.signal(Signal::Lost);
            }
        }
        let revision = monitor.lock().revision;
        let read_started = Instant::now();
        let fresh = if let Some(source) = &mut source {
            if source.connection.is_closed()
                || source.signals.as_ref().is_some_and(JoinHandle::is_finished)
            {
                monitor.signal(Signal::Lost);
                false
            } else {
                refresh(&monitor, source)
            }
        } else {
            true
        };
        if let Some(ready) = ready.take() {
            let _ = ready.send(());
        }
        if read_started >= next_poll {
            next_poll = read_started + POLL_INTERVAL;
        }
        let inner = monitor.lock();
        if inner.stopped {
            return;
        }
        if !fresh || inner.revision != revision {
            continue;
        }
        let _ = monitor
            .changed
            .wait_timeout(inner, next_poll.saturating_duration_since(Instant::now()));
    }
}

fn call<B, R>(
    connection: &Connection,
    destination: &str,
    path: &str,
    interface: &str,
    method: &str,
    body: &B,
) -> Result<R, PlatformError>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
    R: for<'de> serde::Deserialize<'de> + zbus::zvariant::Type,
{
    call_raw(connection, destination, path, interface, method, body).map_err(bus_error)
}

/// Like [`call`], but keeps the D-Bus error so a caller can tell "no such session" from a failure.
fn call_raw<B, R>(
    connection: &Connection,
    destination: &str,
    path: &str,
    interface: &str,
    method: &str,
    body: &B,
) -> zbus::Result<R>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
    R: for<'de> serde::Deserialize<'de> + zbus::zvariant::Type,
{
    connection
        .call_method(Some(destination), path, Some(interface), method, body)?
        .body()
        .deserialize()
}

/// True if `error` is the D-Bus error reply `name` (logind's `NoSessionForPID`, `NoSuchSession`).
fn is_error_reply(error: &zbus::Error, name: &str) -> bool {
    matches!(error, zbus::Error::MethodError(reply, _, _) if reply.as_str() == name)
}

/// The result of a session lookup. Only the error reply `absent` means that logind has no such
/// session; every other failure (timeout, access denied, a malformed reply) is an error. It must
/// not read as "absent": a later step could then pick a session this process is not part of.
fn lookup(
    reply: zbus::Result<OwnedObjectPath>,
    absent: &str,
) -> Result<Option<OwnedObjectPath>, PlatformError> {
    match reply {
        Ok(path) => Ok(Some(path)),
        Err(e) if is_error_reply(&e, absent) => Ok(None),
        Err(e) => Err(bus_error(e)),
    }
}

fn properties(connection: &Connection, path: &str) -> Result<Properties, PlatformError> {
    call(connection, LOGIND, path, PROPERTIES, "GetAll", &(SESSION,))
}

fn boolean(properties: &Properties, name: &str) -> Option<bool> {
    properties.get(name).and_then(|v| bool::try_from(v).ok())
}

/// Which lookup found the session. Logged, so an install that picks the wrong one can be told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// `GetSessionByPID` for this process.
    Pid,
    /// The session `$XDG_SESSION_ID` names.
    Env,
    /// The user's logind `Display` session.
    Display,
}

impl Step {
    fn name(self) -> &'static str {
        match self {
            Step::Pid => "pid",
            Step::Env => "env",
            Step::Display => "display",
        }
    }
}

/// What logind says about one session: the properties the selection rules read, nothing more.
/// A property that is missing or of an unexpected type is `None`. Such a session never qualifies,
/// and the `Display` election treats it as unreadable ([`Candidate::readable`]).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Candidate {
    id: String,
    path: OwnedObjectPath,
    /// `Type`: `wayland`, `x11`, `tty`, `unspecified`, ...
    kind: Option<String>,
    /// The uid in `User`.
    user: Option<u32>,
    /// The seat id in `Seat`; empty for a seatless session (ssh, remote).
    seat: Option<String>,
}

impl Candidate {
    fn from_properties(path: OwnedObjectPath, properties: &Properties) -> Self {
        let text = |name: &str| {
            properties
                .get(name)
                .and_then(|v| <&str>::try_from(v).ok())
                .map(str::to_owned)
        };
        // `User` is `(uo)` and `Seat` is `(so)`; only the leading id is needed from either.
        let user = properties
            .get("User")
            .and_then(|v| v.try_clone().ok())
            .and_then(|v| <(u32, OwnedObjectPath)>::try_from(v).ok())
            .map(|(uid, _)| uid);
        let seat = properties
            .get("Seat")
            .and_then(|v| v.try_clone().ok())
            .and_then(|v| <(String, OwnedObjectPath)>::try_from(v).ok())
            .map(|(seat, _)| seat);
        Self {
            id: text("Id").unwrap_or_else(|| path.as_str().to_owned()),
            kind: text("Type"),
            user,
            seat,
            path,
        }
    }

    /// Whether every property the selection rules read was present and decoded: `Type`, `User`
    /// and `Seat`. A session that fails this could be a graphical, seated one of ours, so it is not
    /// just "not counted" by the `Display` election.
    fn readable(&self) -> bool {
        self.kind.is_some() && self.user.is_some() && self.seat.is_some()
    }

    /// A graphical (`wayland` or `x11`) session owned by `uid`.
    fn graphical(&self, uid: u32) -> bool {
        matches!(self.kind.as_deref(), Some("wayland" | "x11")) && self.user == Some(uid)
    }

    /// Attached to a seat. A seatless session (ssh, remote) has an empty seat id.
    fn on_seat(&self) -> bool {
        self.seat.as_deref().is_some_and(|seat| !seat.is_empty())
    }
}

/// Picks the session, from the first step whose candidate qualifies:
///
/// 1. `own`, the session that owns this process: graphical and ours;
/// 2. `env`, the session `$XDG_SESSION_ID` names: graphical and ours;
/// 3. `display`, the user's logind `Display` session: graphical, ours and on a seat, and not
///    logind's "none" path `/`. The caller must also have established that no other graphical,
///    seated session of the user exists ([`elect_display`]), because `Display` says nothing about
///    which session this process's compositor belongs to.
///
/// `Active` is not a criterion: the observer tracks it and closes the gate while the session is
/// inactive. Each step is read only if the earlier ones did not qualify, and an error from a step
/// that is read ends the search. A `None` from a read means that step has no session.
fn choose<E>(
    own: impl FnOnce() -> Result<Option<Candidate>, E>,
    env: impl FnOnce() -> Result<Option<Candidate>, E>,
    display: impl FnOnce() -> Result<Option<Candidate>, E>,
    uid: u32,
) -> Result<Option<(Step, Candidate)>, E> {
    if let Some(candidate) = own()?
        && candidate.graphical(uid)
    {
        return Ok(Some((Step::Pid, candidate)));
    }
    if let Some(candidate) = env()?
        && candidate.graphical(uid)
    {
        return Ok(Some((Step::Env, candidate)));
    }
    if let Some(candidate) = display()?
        && candidate.path.as_str() != "/"
        && candidate.graphical(uid)
        && candidate.on_seat()
    {
        return Ok(Some((Step::Display, candidate)));
    }
    Ok(None)
}

fn find_session(connection: &Connection, uid: u32) -> Result<OwnedObjectPath, PlatformError> {
    let found = choose(
        || {
            // A user-manager service belongs to no session, which logind reports as
            // `NoSessionForPID`. Any other failure is an error, not a missing session.
            let own = lookup(
                call_raw(
                    connection,
                    LOGIND,
                    MANAGER_PATH,
                    MANAGER,
                    "GetSessionByPID",
                    &(std::process::id(),),
                ),
                NO_SESSION_FOR_PID,
            )?;
            own.map(|path| candidate(connection, path)).transpose()
        },
        || {
            let Some(id) = std::env::var("XDG_SESSION_ID")
                .ok()
                .filter(|id| !id.is_empty())
            else {
                return Ok(None);
            };
            // A stale id, as in a service started under a session that has since ended, is
            // `NoSuchSession`.
            let path = lookup(
                call_raw(
                    connection,
                    LOGIND,
                    MANAGER_PATH,
                    MANAGER,
                    "GetSession",
                    &(id,),
                ),
                NO_SUCH_SESSION,
            )?;
            path.map(|path| candidate(connection, path)).transpose()
        },
        || Ok(display_session(connection, uid)),
        uid,
    )?;
    let (step, found) = found.ok_or(PlatformError::NotFound)?;
    tracing::info!(step = step.name(), session = %found.id, "logind session found");
    Ok(found.path)
}

/// Whether the user's `Display` session is safe to take for this process's session.
#[derive(Debug, PartialEq, Eq)]
enum Election {
    /// `Display` is the user's only graphical, seated session.
    Chosen(Candidate),
    /// There is no usable `Display`, or it is not one of the user's graphical, seated sessions.
    None,
    /// The user has this many (two or more) graphical, seated sessions, so `Display` cannot say
    /// which one this process's compositor belongs to.
    Ambiguous(usize),
    /// A listed session's `Type`, `User` or `Seat` could not be read, so it cannot be ruled out as
    /// a rival.
    Unreadable,
}

impl Election {
    fn chosen(self) -> Option<Candidate> {
        match self {
            Election::Chosen(candidate) => Some(candidate),
            Election::None | Election::Ambiguous(_) | Election::Unreadable => None,
        }
    }
}

/// Elects the `Display` session from everything logind says about the user's sessions.
///
/// logind picks `Display` per user, not per compositor, so with two graphical, seated sessions a
/// service connected to one compositor could be given the other's state (say, an active, unlocked
/// session opening the gate for a compositor in an inactive one). The election therefore accepts
/// `Display` only when it is the one graphical, ours, seated session; two or more such sessions
/// (whichever is `Display`, and whichever is active) elect nothing.
///
/// A session that decodes fine but is not graphical, not ours or seatless is simply not counted.
/// One whose `Type`, `User` or `Seat` is missing or undecodable might be a rival, so any such
/// session in the list elects nothing, whatever the others say.
fn elect_display(display: &OwnedObjectPath, sessions: Vec<Candidate>, uid: u32) -> Election {
    if sessions.iter().any(|session| !session.readable()) {
        return Election::Unreadable;
    }
    let mut qualifying: Vec<Candidate> = sessions
        .into_iter()
        .filter(|session| session.graphical(uid) && session.on_seat())
        .collect();
    match qualifying.len() {
        0 => Election::None,
        1 if display.as_str() != "/" && qualifying[0].path == *display => {
            Election::Chosen(qualifying.remove(0))
        }
        1 => Election::None,
        count => Election::Ambiguous(count),
    }
}

/// `Display` (`(so)`, with path `/` for none) and `Sessions` (`a(so)`) from `GetAll` on
/// `org.freedesktop.login1.User`, as object paths. `None` if either is missing or malformed.
fn user_sessions(properties: &Properties) -> Option<(OwnedObjectPath, Vec<OwnedObjectPath>)> {
    let value = |name: &str| properties.get(name).and_then(|v| v.try_clone().ok());
    let (_, display) = <(String, OwnedObjectPath)>::try_from(value("Display")?).ok()?;
    let sessions = Vec::<(String, OwnedObjectPath)>::try_from(value("Sessions")?).ok()?;
    Some((
        display,
        sessions.into_iter().map(|(_, path)| path).collect(),
    ))
}

/// The count last reported by [`display_session`], so a retry loop reports an ambiguity once and
/// not on every attempt. 0 when the last attempt was not ambiguous.
static REPORTED_AMBIGUITY: AtomicUsize = AtomicUsize::new(0);

/// The user's `Display` session if it is the only graphical, seated one ([`elect_display`]).
/// Any failure to read the user or any of its sessions means no candidate, so the caller fails
/// closed.
fn display_session(connection: &Connection, uid: u32) -> Option<Candidate> {
    let user: OwnedObjectPath = call_raw(
        connection,
        LOGIND,
        MANAGER_PATH,
        MANAGER,
        "GetUser",
        &(uid,),
    )
    .ok()?;
    // One call, so `Display` and `Sessions` are a consistent snapshot.
    let properties: Properties = call(
        connection,
        LOGIND,
        user.as_str(),
        PROPERTIES,
        "GetAll",
        &(USER,),
    )
    .ok()?;
    let (display, paths) = user_sessions(&properties)?;
    if display.as_str() == "/" {
        return None;
    }
    let sessions = paths
        .into_iter()
        .map(|path| candidate(connection, path))
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let election = elect_display(&display, sessions, uid);
    let ambiguous = match &election {
        Election::Ambiguous(count) => *count,
        Election::Chosen(_) | Election::None | Election::Unreadable => 0,
    };
    if REPORTED_AMBIGUITY.swap(ambiguous, Ordering::Relaxed) != ambiguous && ambiguous > 0 {
        tracing::warn!(
            sessions = ambiguous,
            "several graphical seated logind sessions: not choosing one"
        );
    }
    election.chosen()
}

fn candidate(connection: &Connection, path: OwnedObjectPath) -> Result<Candidate, PlatformError> {
    let properties = properties(connection, path.as_str())?;
    Ok(Candidate::from_properties(path, &properties))
}

fn parse_signal(
    message: &Message,
    session: &OwnedObjectPath,
    owner: &str,
) -> zbus::Result<Option<Signal>> {
    let header = message.header();
    if header.message_type() != zbus::message::Type::Signal {
        return Ok(None);
    }
    let interface = header.interface().map(|v| v.as_str());
    let member = header.member().map(|v| v.as_str());
    let path = header.path().map(|v| v.as_str());
    let sender = header.sender().map(|v| v.as_str());
    if sender == Some(DBUS) && interface == Some(DBUS) && member == Some("NameOwnerChanged") {
        let (name, old, new): (String, String, String) = message.body().deserialize()?;
        return Ok((name == LOGIND && old == owner && new != owner).then_some(Signal::Lost));
    }
    if sender != Some(owner) {
        return Ok(None);
    }
    if path == Some(MANAGER_PATH) && interface == Some(MANAGER) && member == Some("PrepareForSleep")
    {
        let sleeping: bool = message.body().deserialize()?;
        return Ok(Some(Signal::Sleep(sleeping)));
    }
    if path != Some(session.as_str()) {
        return Ok(None);
    }
    match (interface, member) {
        (Some(SESSION), Some("Lock")) => Ok(Some(Signal::Lock)),
        (Some(SESSION), Some("Unlock")) => Ok(Some(Signal::Unlock)),
        (Some(PROPERTIES), Some("PropertiesChanged")) => {
            let (interface, changed, invalidated): (String, Properties, Vec<String>) =
                message.body().deserialize()?;
            if interface != SESSION {
                return Ok(None);
            }
            let invalidated = invalidated
                .iter()
                .any(|v| matches!(v.as_str(), "LockedHint" | "Active"));
            let locked = boolean(&changed, "LockedHint");
            let active = boolean(&changed, "Active");
            let malformed = (changed.contains_key("LockedHint") && locked.is_none())
                || (changed.contains_key("Active") && active.is_none());
            Ok(
                (locked.is_some() || active.is_some() || invalidated || malformed).then_some(
                    Signal::Properties {
                        locked,
                        active,
                        invalidated: invalidated || malformed,
                    },
                ),
            )
        }
        _ => Ok(None),
    }
}

fn locker_present(uid: u32) -> std::io::Result<bool> {
    let deadline = Instant::now() + CALL_TIMEOUT;
    for entry in std::fs::read_dir("/proc")? {
        if Instant::now() >= deadline {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        let entry = entry?;
        if !entry
            .file_name()
            .as_encoded_bytes()
            .iter()
            .all(u8::is_ascii_digit)
        {
            continue;
        }
        let path = entry.path();
        let result: std::io::Result<bool> = (|| {
            if std::fs::metadata(&path)?.uid() != uid {
                return Ok(false);
            }
            let name = std::fs::read(path.join("comm"))?;
            Ok(matches!(
                name.as_slice(),
                b"hyprlock\n" | b"swaylock\n" | b"gtklock\n"
            ))
        })();
        match result {
            Ok(true) => return Ok(true),
            Ok(false) => {}
            // Processes routinely exit between directory enumeration and reading `comm`.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(false)
}

fn bus_error(error: zbus::Error) -> PlatformError {
    match error {
        zbus::Error::InputOutput(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
            PlatformError::Timeout
        }
        _ => PlatformError::Backend(format!("logind D-Bus: {error}")),
    }
}

/// A small deadline executor for connection authentication, which zbus's method timeout excludes.
fn bounded<F: Future>(future: F, timeout: Duration) -> Result<F::Output, PlatformError> {
    struct Unpark(thread::Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }
    let deadline = Instant::now() + timeout;
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if Instant::now() >= deadline {
            return Err(PlatformError::Timeout);
        }
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return Ok(value),
            Poll::Pending => {
                thread::park_timeout(deadline.saturating_duration_since(Instant::now()))
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use zbus::zvariant::{ObjectPath, Structure, Value};

    const UNLOCKED: Reading = Reading {
        locked_hint: Some(false),
        active: Some(true),
        locker: Some(false),
        sleeping: Some(false),
        bus_ok: true,
    };

    struct FakeSource {
        reading: Reading,
        during_read: Option<Box<dyn FnOnce()>>,
    }

    impl StateSource for FakeSource {
        fn read(&mut self) -> Reading {
            if let Some(during_read) = self.during_read.take() {
                during_read();
            }
            self.reading
        }
    }

    fn read(monitor: &Monitor, reading: Reading) -> bool {
        refresh(
            monitor,
            &mut FakeSource {
                reading,
                during_read: None,
            },
        )
    }

    fn connected(gate: Arc<IoGate>) -> Arc<Monitor> {
        let monitor = Arc::new(Monitor::new(gate));
        monitor.lock().connected = true;
        monitor
    }

    #[test]
    fn lock_decision_fails_closed() {
        for bus_ok in [false, true] {
            for hint in [None, Some(false), Some(true)] {
                for locker in [None, Some(false), Some(true)] {
                    for requested in [false, true] {
                        let reading = Reading {
                            locked_hint: hint,
                            locker,
                            bus_ok,
                            ..UNLOCKED
                        };
                        let expected = if !bus_ok || hint.is_none() {
                            LockState::Unknown
                        } else if requested || hint == Some(true) || locker == Some(true) {
                            LockState::Locked
                        } else if locker == Some(false) {
                            LockState::Unlocked
                        } else {
                            LockState::Unknown
                        };
                        assert_eq!(lock_state(reading, requested), expected, "{reading:?}");
                    }
                }
            }
        }
        // Unreadable Active is also a D-Bus observation failure.
        assert_eq!(
            lock_state(
                Reading {
                    active: None,
                    ..UNLOCKED
                },
                false
            ),
            LockState::Unknown
        );
    }

    #[test]
    fn injected_reads_update_gate_before_events() {
        let gate = IoGate::new();
        gate.set_engine_permits(true);
        let monitor = connected(gate.clone());
        let events = Arc::new(Mutex::new(Vec::new()));
        let received = events.clone();
        let observed_gate = gate.clone();
        monitor.lock().sink = Some(Arc::new(move |event| {
            if let SessionEvent::State(state) = event {
                assert_eq!(observed_gate.is_open(), state.permits_io());
            }
            received.lock().unwrap().push(event);
        }));
        for reading in [
            UNLOCKED,
            Reading {
                locked_hint: Some(true),
                ..UNLOCKED
            },
            UNLOCKED,
            Reading {
                locker: Some(true),
                ..UNLOCKED
            },
            Reading {
                locker: None,
                ..UNLOCKED
            },
            Reading {
                active: Some(false),
                ..UNLOCKED
            },
            Reading {
                active: None,
                ..UNLOCKED
            },
            Reading {
                bus_ok: false,
                ..UNLOCKED
            },
            UNLOCKED,
        ] {
            monitor.lock().connected = true;
            assert!(read(&monitor, reading));
            assert_eq!(gate.is_open(), monitor.lock().state.permits_io());
            assert_eq!(monitor.lock().state.active, reading.active);
        }
        let count = events.lock().unwrap().len();
        read(&monitor, UNLOCKED);
        assert_eq!(
            events.lock().unwrap().len(),
            count,
            "unchanged poll is silent"
        );
        gate.set_engine_permits(false);
        read(&monitor, UNLOCKED);
        assert!(!gate.is_open(), "the session cannot override the engine");
    }

    #[test]
    fn safety_signals_invalidate_an_inflight_permissive_read() {
        let gate = IoGate::new();
        gate.set_engine_permits(true);
        let monitor = connected(gate.clone());
        for signal in [
            Signal::Lock,
            Signal::Properties {
                locked: Some(true),
                active: None,
                invalidated: false,
            },
            Signal::Properties {
                locked: None,
                active: Some(false),
                invalidated: false,
            },
            Signal::Properties {
                locked: None,
                active: None,
                invalidated: true,
            },
            Signal::Sleep(true),
            Signal::Sleep(false),
            Signal::Lost,
        ] {
            {
                let mut inner = monitor.lock();
                inner.connected = true;
                inner.sleeping = false;
                inner.lock_requested = false;
                inner.lock_seen = false;
            }
            read(&monitor, UNLOCKED);
            assert!(gate.is_open());
            let observed = monitor.clone();
            let gate_during = gate.clone();
            let mut source = FakeSource {
                reading: UNLOCKED,
                during_read: Some(Box::new(move || {
                    observed.signal(signal);
                    assert!(
                        !gate_during.is_open(),
                        "closed while the read is still running"
                    );
                })),
            };
            assert!(!refresh(&monitor, &mut source));
            assert!(!gate.is_open(), "stale result must not reopen: {signal:?}");
        }
    }

    #[test]
    fn sleep_wake_and_connection_loss_require_fresh_state() {
        let gate = IoGate::new();
        gate.set_engine_permits(true);
        let monitor = connected(gate.clone());
        read(&monitor, UNLOCKED);
        let events = Arc::new(Mutex::new(Vec::new()));
        let received = events.clone();
        let observed_gate = gate.clone();
        monitor.lock().sink = Some(Arc::new(move |event| {
            match event {
                SessionEvent::WillSleep | SessionEvent::Woke => assert!(!observed_gate.is_open()),
                SessionEvent::State(state) => {
                    assert_eq!(observed_gate.is_open(), state.permits_io())
                }
                _ => panic!("unexpected event"),
            }
            received.lock().unwrap().push(event);
        }));
        monitor.signal(Signal::Sleep(true));
        read(
            &monitor,
            Reading {
                sleeping: Some(true),
                ..UNLOCKED
            },
        );
        assert!(!gate.is_open(), "polls during sleep cannot reopen");
        assert_eq!(monitor.lock().state.active, Some(true));
        read(
            &monitor,
            Reading {
                sleeping: Some(true),
                locked_hint: Some(true),
                ..UNLOCKED
            },
        );
        assert_eq!(monitor.lock().state.lock, LockState::Locked);
        monitor.signal(Signal::Sleep(false));
        assert!(!gate.is_open());
        read(&monitor, UNLOCKED);
        assert!(gate.is_open());

        // Polling also corrects missed PrepareForSleep signals, without reopening on the read
        // that first discovers a wake.
        assert!(!read(
            &monitor,
            Reading {
                sleeping: Some(true),
                ..UNLOCKED
            }
        ));
        assert!(!gate.is_open());
        assert!(!read(&monitor, UNLOCKED));
        assert!(!gate.is_open());
        assert!(read(&monitor, UNLOCKED));
        assert!(gate.is_open());
        let events = events.lock().unwrap();
        assert_eq!(events[0], SessionEvent::WillSleep);
        let woke = events
            .iter()
            .position(|v| *v == SessionEvent::Woke)
            .unwrap();
        assert!(
            events[woke + 1..].contains(&SessionEvent::State(SessionState {
                lock: LockState::Unlocked,
                active: Some(true),
            }))
        );
        drop(events);
        monitor.signal(Signal::Lost);
        assert_eq!(monitor.lock().state, UNKNOWN);
        read(&monitor, UNLOCKED);
        assert!(
            !gate.is_open(),
            "reads cannot reopen without signal observation"
        );
        monitor.lock().connected = true;
        read(&monitor, UNLOCKED);
        assert!(gate.is_open());
    }

    const UID: u32 = 1000;

    fn session(id: &str, kind: &str, uid: u32, seat: &str) -> Candidate {
        Candidate {
            id: id.into(),
            path: OwnedObjectPath::try_from(
                format!("/org/freedesktop/login1/session/_3{id}").as_str(),
            )
            .unwrap(),
            kind: Some(kind.into()),
            user: Some(uid),
            seat: Some(seat.into()),
        }
    }

    /// A graphical session of ours on a seat: what every step accepts.
    fn good(id: &str) -> Candidate {
        session(id, "wayland", UID, "seat0")
    }

    fn pick(
        own: Option<Candidate>,
        env: Option<Candidate>,
        display: Option<Candidate>,
    ) -> Option<(Step, String)> {
        choose::<()>(|| Ok(own), || Ok(env), || Ok(display), UID)
            .unwrap()
            .map(|(step, found)| (step, found.id))
    }

    #[test]
    fn pid_session_wins_over_environment_and_display() {
        assert_eq!(
            pick(Some(good("1")), Some(good("2")), Some(good("3"))),
            Some((Step::Pid, "1".into()))
        );
        // x11 is graphical too.
        assert_eq!(
            pick(Some(session("1", "x11", UID, "seat0")), None, None),
            Some((Step::Pid, "1".into()))
        );
        // Later steps are not even read once one qualifies.
        let found = choose::<()>(
            || Ok(Some(good("1"))),
            || panic!("env read after the pid session qualified"),
            || panic!("display read after the pid session qualified"),
            UID,
        )
        .unwrap();
        assert_eq!(found.map(|(step, _)| step), Some(Step::Pid));
    }

    #[test]
    fn environment_session_is_used_without_a_graphical_pid_session() {
        let env = || Some(good("2"));
        for own in [
            None,
            Some(session("1", "tty", UID, "seat0")),
            Some(session("1", "wayland", UID + 1, "seat0")),
        ] {
            assert_eq!(
                pick(own, env(), Some(good("3"))),
                Some((Step::Env, "2".into()))
            );
        }
        let found = choose::<()>(
            || Ok(None),
            || Ok(env()),
            || panic!("display read after the env session qualified"),
            UID,
        )
        .unwrap();
        assert_eq!(found.map(|(step, _)| step), Some(Step::Env));
    }

    #[test]
    fn display_session_is_used_when_the_environment_has_none() {
        let display = || Some(good("3"));
        for env in [
            // `$XDG_SESSION_ID` unset, or naming a session that has ended.
            None,
            // A non-graphical session of ours.
            Some(session("2", "tty", UID, "seat0")),
            Some(session("2", "unspecified", UID, "")),
            // Another user's graphical session.
            Some(session("2", "wayland", UID + 1, "seat0")),
        ] {
            assert_eq!(
                pick(None, env, display()),
                Some((Step::Display, "3".into()))
            );
        }
        // The same when the pid session exists but is not usable.
        assert_eq!(
            pick(Some(session("1", "tty", UID, "seat0")), None, display()),
            Some((Step::Display, "3".into()))
        );
        assert_eq!(
            pick(None, None, Some(session("3", "x11", UID, "seat1"))),
            Some((Step::Display, "3".into()))
        );
    }

    #[test]
    fn display_session_is_refused_unless_graphical_ours_and_seated() {
        let refused = [
            // logind's "none".
            Candidate {
                path: OwnedObjectPath::try_from("/").unwrap(),
                ..good("3")
            },
            // Seatless (ssh, remote), whether the seat reads as empty or not at all.
            session("3", "wayland", UID, ""),
            Candidate {
                seat: None,
                ..good("3")
            },
            // Not a graphical type.
            session("3", "tty", UID, "seat0"),
            session("3", "unspecified", UID, "seat0"),
            session("3", "mir", UID, "seat0"),
            Candidate {
                kind: None,
                ..good("3")
            },
            // Another user's, or not readable as anyone's.
            session("3", "wayland", UID + 1, "seat0"),
            session("3", "wayland", 0, "seat0"),
            Candidate {
                user: None,
                ..good("3")
            },
        ];
        for display in refused {
            assert_eq!(pick(None, None, Some(display.clone())), None, "{display:?}");
        }
    }

    #[test]
    fn only_the_display_session_needs_a_seat() {
        // The pid and environment steps are as they were: graphical and ours is enough.
        let seatless = || session("1", "wayland", UID, "");
        assert_eq!(
            pick(Some(seatless()), None, None),
            Some((Step::Pid, "1".into()))
        );
        assert_eq!(
            pick(None, Some(seatless()), None),
            Some((Step::Env, "1".into()))
        );
    }

    #[test]
    fn nothing_qualifying_selects_nothing() {
        assert_eq!(pick(None, None, None), None);
        let unusable = || {
            [
                session("1", "tty", UID, "seat0"),
                session("1", "wayland", UID + 1, "seat0"),
            ]
        };
        for (own, env, display) in unusable()
            .into_iter()
            .flat_map(|own| unusable().into_iter().map(move |env| (own.clone(), env)))
            .flat_map(|(own, env)| {
                unusable()
                    .into_iter()
                    .map(move |display| (own.clone(), env.clone(), display))
            })
        {
            assert_eq!(pick(Some(own), Some(env), Some(display)), None);
        }
    }

    #[test]
    fn a_failed_step_ends_the_search() {
        // An error in a step that is read propagates, so the caller closes the gate and retries.
        let failed = choose(
            || Ok(None),
            || Err("bus failure"),
            || panic!("display read after an error"),
            UID,
        );
        assert_eq!(
            failed.map(|found| found.map(|(step, _)| step)),
            Err("bus failure")
        );
        let failed = choose(
            || Err("bus failure"),
            || panic!("env read after an error"),
            || panic!("display read after an error"),
            UID,
        );
        assert_eq!(
            failed.map(|found| found.map(|(step, _)| step)),
            Err("bus failure")
        );
        let failed = choose(|| Ok(None), || Ok(None), || Err("bus failure"), UID);
        assert_eq!(
            failed.map(|found| found.map(|(step, _)| step)),
            Err("bus failure")
        );
        // An earlier session that qualifies is chosen without ever reading the step that fails.
        let found = choose(
            || Ok(Some(good("1"))),
            || Err("bus failure"),
            || Err("bus failure"),
            UID,
        );
        assert_eq!(
            found.map(|found| found.map(|(step, _)| step)),
            Ok(Some(Step::Pid))
        );
    }

    fn owned(value: impl Into<Value<'static>>) -> OwnedValue {
        OwnedValue::try_from(value.into()).unwrap()
    }

    fn pair(
        first: impl Into<Value<'static>> + zbus::zvariant::Type,
        path: &'static str,
    ) -> OwnedValue {
        owned(Structure::from((
            first,
            ObjectPath::try_from(path).unwrap(),
        )))
    }

    #[test]
    fn candidate_reads_the_property_shapes_logind_sends() {
        let path = OwnedObjectPath::try_from("/org/freedesktop/login1/session/_31").unwrap();
        // `Id` s, `Type` s, `User` (uo), `Seat` (so): as in `busctl introspect`.
        let properties: Properties = [
            ("Id".to_owned(), owned("1")),
            ("Type".to_owned(), owned("wayland")),
            (
                "User".to_owned(),
                pair(1000u32, "/org/freedesktop/login1/user/_1000"),
            ),
            (
                "Seat".to_owned(),
                pair("seat0", "/org/freedesktop/login1/seat/seat0"),
            ),
        ]
        .into();
        let read = Candidate::from_properties(path.clone(), &properties);
        assert_eq!(read.id, "1");
        assert_eq!(read.path, path);
        assert_eq!(read.kind.as_deref(), Some("wayland"));
        assert_eq!(read.user, Some(1000));
        assert_eq!(read.seat.as_deref(), Some("seat0"));
        assert!(read.graphical(1000) && read.on_seat());
        assert!(!read.graphical(1001));

        // A seatless session reports an empty seat id and the "none" path.
        let mut seatless = properties.clone();
        seatless.insert("Seat".to_owned(), pair("", "/"));
        let read = Candidate::from_properties(path.clone(), &seatless);
        assert_eq!(read.seat.as_deref(), Some(""));
        assert!(!read.on_seat());

        // Anything missing or of another type never qualifies; the id falls back to the path.
        let wrong: Properties = [
            ("Type".to_owned(), owned(7u32)),
            ("User".to_owned(), owned("1000")),
            ("Seat".to_owned(), owned("seat0")),
        ]
        .into();
        let read = Candidate::from_properties(path.clone(), &wrong);
        assert_eq!(read.id, path.as_str());
        assert!(read.kind.is_none() && read.user.is_none() && read.seat.is_none());
        assert!(!read.graphical(1000) && !read.on_seat());
    }

    fn path(id: &str) -> OwnedObjectPath {
        session(id, "wayland", UID, "seat0").path
    }

    fn elect(display: &str, sessions: Vec<Candidate>) -> Election {
        elect_display(&path(display), sessions, UID)
    }

    #[test]
    fn display_is_elected_when_it_is_the_one_qualifying_session() {
        assert_eq!(elect("1", vec![good("1")]), Election::Chosen(good("1")));
        assert_eq!(
            elect("1", vec![session("1", "x11", UID, "seat0")]),
            Election::Chosen(session("1", "x11", UID, "seat0"))
        );
        // Sessions that read fine but are not graphical, not ours or seatless are not rivals, in
        // any order. (One that does not read fine is: see the unreadable tests.)
        let mut sessions = vec![
            good("1"),
            session("2", "tty", UID, "seat0"),
            session("3", "wayland", UID, ""),
            session("4", "wayland", UID + 1, "seat0"),
            session("5", "unspecified", UID, ""),
        ];
        for _ in 0..2 {
            assert_eq!(elect("1", sessions.clone()), Election::Chosen(good("1")));
            sessions.reverse();
        }
    }

    /// The properties logind sends for one session, as the production decoder reads them.
    fn session_properties(
        id: &str,
        kind: &'static str,
        uid: u32,
        seat: &'static str,
    ) -> Properties {
        let seat_path = if seat.is_empty() {
            "/"
        } else {
            "/org/freedesktop/login1/seat/seat0"
        };
        [
            ("Id".to_owned(), owned(id.to_owned())),
            ("Type".to_owned(), owned(kind)),
            (
                "User".to_owned(),
                pair(uid, "/org/freedesktop/login1/user/_1000"),
            ),
            ("Seat".to_owned(), pair(seat, seat_path)),
        ]
        .into()
    }

    /// A session as `candidate` reads it from logind: through `Candidate::from_properties`.
    fn decoded(id: &str, properties: &Properties) -> Candidate {
        Candidate::from_properties(path(id), properties)
    }

    #[test]
    fn decoding_logind_properties_matches_the_hand_built_candidates() {
        let properties = session_properties("1", "wayland", UID, "seat0");
        assert_eq!(decoded("1", &properties), good("1"));
        assert!(decoded("1", &properties).readable());
        // Fully readable but ineligible sessions stay readable.
        for (kind, uid, seat) in [
            ("tty", UID, "seat0"),
            ("unspecified", UID, ""),
            ("wayland", UID, ""),
            ("wayland", UID + 1, "seat0"),
        ] {
            assert!(decoded("2", &session_properties("2", kind, uid, seat)).readable());
        }
    }

    /// Ways a session's eligibility properties can be missing or undecodable.
    fn broken_properties() -> Vec<(&'static str, Properties)> {
        let base = || session_properties("2", "wayland", UID, "seat0");
        let without = |name: &str| {
            let mut properties = base();
            properties.remove(name);
            properties
        };
        let replaced = |name: &str, value: OwnedValue| {
            let mut properties = base();
            properties.insert(name.to_owned(), value);
            properties
        };
        let user_path = "/org/freedesktop/login1/user/_1000";
        let seat_path = "/org/freedesktop/login1/seat/seat0";
        vec![
            ("Type missing", without("Type")),
            ("User missing", without("User")),
            ("Seat missing", without("Seat")),
            ("no properties", Properties::new()),
            ("Type is a number", replaced("Type", owned(7u32))),
            ("User is a string", replaced("User", owned("1000"))),
            (
                "User has a text id",
                replaced("User", pair("1000", user_path)),
            ),
            ("Seat is a string", replaced("Seat", owned("seat0"))),
            (
                "Seat has a number id",
                replaced("Seat", pair(0u32, seat_path)),
            ),
        ]
    }

    #[test]
    fn an_unreadable_rival_rejects_the_whole_election() {
        // Session 1 is a perfectly good unique candidate, until session 2 cannot be read: it
        // could be a second graphical, seated session, and then 1's state must not authorize
        // 2's compositor.
        let a = decoded("1", &session_properties("1", "wayland", UID, "seat0"));
        let healthy_rival = decoded("2", &session_properties("2", "tty", UID, "seat0"));
        assert_eq!(
            elect("1", vec![a.clone(), healthy_rival]),
            Election::Chosen(a.clone()),
            "control: a rival that reads fine and is not graphical does not block"
        );
        for (what, properties) in broken_properties() {
            let b = decoded("2", &properties);
            assert!(!b.readable(), "{what}");
            for display in ["1", "2"] {
                for sessions in [vec![a.clone(), b.clone()], vec![b.clone(), a.clone()]] {
                    assert_eq!(
                        elect(display, sessions),
                        Election::Unreadable,
                        "{what}, Display {display}"
                    );
                }
            }
            // Nor can it be rescued by a third, fully readable session.
            assert_eq!(
                elect("1", vec![a.clone(), b.clone(), good("3")]),
                Election::Unreadable,
                "{what}"
            );
            // The election yields no candidate, so `choose` falls through to nothing.
            let found = choose::<()>(
                || Ok(None),
                || Ok(None),
                || Ok(elect("1", vec![a.clone(), b.clone()]).chosen()),
                UID,
            )
            .unwrap();
            assert_eq!(found, None, "{what}");
        }
    }

    #[test]
    fn an_unreadable_session_rejects_the_election_even_if_it_is_not_a_rival() {
        // The only listed session is unreadable, or it is the lone unreadable one among sessions
        // that are all ineligible: still nothing, not "none qualifies".
        for (what, properties) in broken_properties() {
            let broken = decoded("1", &properties);
            assert_eq!(
                elect("1", vec![broken.clone()]),
                Election::Unreadable,
                "{what}"
            );
            assert_eq!(
                elect("1", vec![session("2", "tty", UID, "seat0"), broken]),
                Election::Unreadable,
                "{what}"
            );
        }
    }

    #[test]
    fn two_qualifying_sessions_elect_nothing_whichever_is_display() {
        // Whichever is Display, and (by construction here) whichever is active.
        for display in ["1", "2"] {
            assert_eq!(
                elect(display, vec![good("1"), good("2")]),
                Election::Ambiguous(2)
            );
        }
        assert_eq!(
            elect(
                "1",
                vec![
                    good("1"),
                    session("2", "x11", UID, "seat1"),
                    session("3", "wayland", UID, "seat0"),
                    session("4", "tty", UID, "seat0"),
                ]
            ),
            Election::Ambiguous(3)
        );
        // Display is neither of them: still ambiguous, still nothing chosen.
        assert_eq!(
            elect("9", vec![good("1"), good("2")]),
            Election::Ambiguous(2)
        );
        // A rival has to be graphical, ours and seated to count.
        assert_eq!(
            elect(
                "1",
                vec![good("1"), session("2", "wayland", UID + 1, "seat0")]
            ),
            Election::Chosen(good("1"))
        );
    }

    #[test]
    fn display_must_be_the_qualifying_session() {
        // Display names a session that is not graphical, or seatless, or absent from the list.
        assert_eq!(
            elect("2", vec![good("1"), session("2", "tty", UID, "seat0")]),
            Election::None
        );
        assert_eq!(
            elect("2", vec![good("1"), session("2", "wayland", UID, "")]),
            Election::None
        );
        assert_eq!(elect("9", vec![good("1")]), Election::None);
        // logind's "none".
        assert_eq!(
            elect_display(
                &OwnedObjectPath::try_from("/").unwrap(),
                vec![good("1")],
                UID
            ),
            Election::None
        );
        // Nothing qualifies at all.
        assert_eq!(elect("1", vec![]), Election::None);
        assert_eq!(
            elect("1", vec![session("1", "tty", UID, "seat0")]),
            Election::None
        );
        assert_eq!(
            elect("1", vec![session("1", "wayland", UID + 1, "seat0")]),
            Election::None
        );
    }

    #[test]
    fn a_second_graphical_session_never_authorizes_the_first() {
        // Regression for the Display ambiguity: with sessions A and B both graphical and seated,
        // neither the pid nor the env step qualifying must not leave A or B selected, whichever
        // logind names as Display. Otherwise A's state could open the gate for a compositor in B.
        let (a, b) = (good("1"), good("2"));
        for display in ["1", "2"] {
            let found = choose::<()>(
                || Ok(None),
                || Ok(None),
                || Ok(elect(display, vec![a.clone(), b.clone()]).chosen()),
                UID,
            )
            .unwrap();
            assert_eq!(found, None, "Display {display}");
        }
        // Once B is the only graphical, seated session (A is, say, an ssh login), it is chosen.
        let found = choose::<()>(
            || Ok(None),
            || Ok(None),
            || Ok(elect("2", vec![session("1", "tty", UID, "seat0"), b.clone()]).chosen()),
            UID,
        )
        .unwrap();
        assert_eq!(found, Some((Step::Display, b)));
    }

    #[test]
    fn user_sessions_reads_the_display_and_sessions_properties() {
        let list = |paths: &[&'static str]| {
            let sessions: Vec<(String, ObjectPath<'static>)> = paths
                .iter()
                .enumerate()
                .map(|(i, path)| (i.to_string(), ObjectPath::try_from(*path).unwrap()))
                .collect();
            owned(sessions)
        };
        let (one, two) = (
            "/org/freedesktop/login1/session/_31",
            "/org/freedesktop/login1/session/_32",
        );
        // `Display` (so) and `Sessions` a(so): as in `busctl introspect`.
        let properties: Properties = [
            ("Display".to_owned(), pair("1", one)),
            ("Sessions".to_owned(), list(&[two, one])),
        ]
        .into();
        let (display, sessions) = user_sessions(&properties).unwrap();
        assert_eq!(display.as_str(), one);
        assert_eq!(
            sessions.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            [two, one]
        );
        // No display session is the path `/`; an empty session list is fine.
        let none: Properties = [
            ("Display".to_owned(), pair("", "/")),
            ("Sessions".to_owned(), list(&[])),
        ]
        .into();
        let (display, sessions) = user_sessions(&none).unwrap();
        assert_eq!(display.as_str(), "/");
        assert!(sessions.is_empty());
        // Either one missing or of another type is unreadable, never a partial answer.
        for broken in [
            [("Sessions".to_owned(), list(&[one]))].into(),
            [("Display".to_owned(), pair("1", one))].into(),
            [
                ("Display".to_owned(), owned("1")),
                ("Sessions".to_owned(), list(&[one])),
            ]
            .into(),
            [
                ("Display".to_owned(), pair("1", one)),
                ("Sessions".to_owned(), owned("1")),
            ]
            .into(),
        ] as [Properties; 4]
        {
            assert!(user_sessions(&broken).is_none());
        }
    }

    /// A D-Bus error reply of the given name, converted by zbus as for a real call.
    fn reply(name: &str) -> zbus::Error {
        let call = Message::method_call("/org/freedesktop/login1", "GetSessionByPID")
            .unwrap()
            .build(&(1u32,))
            .unwrap();
        zbus::Error::from(
            Message::error(&call.header(), name)
                .unwrap()
                .build(&"detail")
                .unwrap(),
        )
    }

    /// Fresh instances of failures that are not "no such session".
    fn failures() -> [zbus::Error; 5] {
        [
            reply("org.freedesktop.DBus.Error.AccessDenied"),
            reply("org.freedesktop.DBus.Error.NoReply"),
            reply("org.freedesktop.DBus.Error.ServiceUnknown"),
            zbus::Error::InputOutput(Arc::new(std::io::ErrorKind::TimedOut.into())),
            zbus::Error::InvalidReply,
        ]
    }

    #[test]
    fn only_the_named_error_reply_counts_as_absence() {
        let found = path("1");
        assert_eq!(
            lookup(Ok(found.clone()), NO_SESSION_FOR_PID).unwrap(),
            Some(found)
        );
        // The reply each step names is absence, and nothing else is.
        assert!(
            lookup(Err(reply(NO_SESSION_FOR_PID)), NO_SESSION_FOR_PID)
                .unwrap()
                .is_none()
        );
        assert!(
            lookup(Err(reply(NO_SUCH_SESSION)), NO_SUCH_SESSION)
                .unwrap()
                .is_none()
        );
        // The two replies are different things: one session being gone says nothing about a PID.
        assert!(lookup(Err(reply(NO_SUCH_SESSION)), NO_SESSION_FOR_PID).is_err());
        assert!(lookup(Err(reply(NO_SESSION_FOR_PID)), NO_SUCH_SESSION).is_err());
        for failure in failures() {
            assert!(!is_error_reply(&failure, NO_SESSION_FOR_PID), "{failure:?}");
            assert!(!is_error_reply(&failure, NO_SUCH_SESSION), "{failure:?}");
        }
        for absent in [NO_SESSION_FOR_PID, NO_SUCH_SESSION] {
            for failure in failures() {
                let error = lookup(Err(failure), absent).unwrap_err();
                assert!(
                    matches!(error, PlatformError::Backend(_) | PlatformError::Timeout),
                    "{error:?}"
                );
            }
        }
    }

    #[test]
    fn a_pid_lookup_failure_never_falls_through_to_the_other_steps() {
        // As `find_session` wires step 1: the real lookup conversion, then `choose`.
        let own = |reply: zbus::Result<OwnedObjectPath>| {
            lookup(reply, NO_SESSION_FOR_PID).map(|path| path.map(|_| good("1")))
        };
        for failure in failures() {
            let found = choose(
                || own(Err(failure)),
                || panic!("env read after a failed pid lookup"),
                || panic!("display read after a failed pid lookup"),
                UID,
            );
            assert!(found.is_err());
        }
        // `NoSessionForPID` is the one reply that lets the search go on.
        let found = choose(
            || own(Err(reply(NO_SESSION_FOR_PID))),
            || Ok(None),
            || Ok(Some(good("3"))),
            UID,
        )
        .unwrap();
        assert_eq!(found, Some((Step::Display, good("3"))));
        // A session that is found is used.
        let found = choose(
            || own(Ok(path("1"))),
            || panic!("env read after the pid session qualified"),
            || panic!("display read after the pid session qualified"),
            UID,
        )
        .unwrap();
        assert_eq!(found.map(|(step, _)| step), Some(Step::Pid));
    }

    #[test]
    fn lock_request_stays_closed_until_unlock_evidence() {
        let gate = IoGate::new();
        gate.set_engine_permits(true);
        let monitor = connected(gate.clone());
        read(&monitor, UNLOCKED);
        // Exercise real signal decoding without sending anything to logind.
        let path = OwnedObjectPath::try_from("/org/freedesktop/login1/session/test").unwrap();
        let owner = ":1.42";
        let signal = |member: &str| {
            let message = Message::signal(path.as_str(), SESSION, member)
                .unwrap()
                .sender(owner)
                .unwrap()
                .build(&())
                .unwrap();
            parse_signal(&message, &path, owner).unwrap().unwrap()
        };
        monitor.signal(signal("Lock"));
        read(&monitor, UNLOCKED);
        assert_eq!(monitor.lock().state.lock, LockState::Locked);
        assert!(!gate.is_open(), "a locker may not have launched yet");
        monitor.signal(signal("Unlock"));
        assert!(!gate.is_open());
        read(
            &monitor,
            Reading {
                locker: Some(true),
                ..UNLOCKED
            },
        );
        assert!(
            !gate.is_open(),
            "Unlock alone cannot override a running locker"
        );
        read(&monitor, UNLOCKED);
        assert!(gate.is_open());
        monitor.signal(signal("Lock"));
        read(
            &monitor,
            Reading {
                locked_hint: Some(true),
                ..UNLOCKED
            },
        );
        read(&monitor, UNLOCKED);
        assert!(
            gate.is_open(),
            "poll corrects a missed Unlock after a proven lock"
        );
    }
}
