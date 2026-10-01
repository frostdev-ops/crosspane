//! Read-only logind session observation, with a same-user `/proc` locker cross-check.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::os::unix::fs::MetadataExt;
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

impl LogindSession {
    /// Find this graphical session (`GetSessionByPID`, else `$XDG_SESSION_ID`) and read its state.
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
    connection
        .call_method(Some(destination), path, Some(interface), method, body)
        .map_err(bus_error)?
        .body()
        .deserialize()
        .map_err(bus_error)
}

fn properties(connection: &Connection, path: &str) -> Result<Properties, PlatformError> {
    call(connection, LOGIND, path, PROPERTIES, "GetAll", &(SESSION,))
}

fn boolean(properties: &Properties, name: &str) -> Option<bool> {
    properties.get(name).and_then(|v| bool::try_from(v).ok())
}

fn find_session(connection: &Connection, uid: u32) -> Result<OwnedObjectPath, PlatformError> {
    let own: Result<OwnedObjectPath, _> = call(
        connection,
        LOGIND,
        MANAGER_PATH,
        MANAGER,
        "GetSessionByPID",
        &(std::process::id(),),
    );
    if let Ok(path) = own
        && graphical_session(connection, &path, uid)?
    {
        return Ok(path);
    }
    let id = std::env::var("XDG_SESSION_ID").map_err(|_| PlatformError::NotFound)?;
    let path: OwnedObjectPath = call(
        connection,
        LOGIND,
        MANAGER_PATH,
        MANAGER,
        "GetSession",
        &(id,),
    )?;
    if graphical_session(connection, &path, uid)? {
        Ok(path)
    } else {
        Err(PlatformError::NotFound)
    }
}

fn graphical_session(connection: &Connection, path: &str, uid: u32) -> Result<bool, PlatformError> {
    let properties = properties(connection, path)?;
    let kind = properties
        .get("Type")
        .and_then(|v| <&str>::try_from(v).ok());
    let user = properties.get("User").and_then(|v| {
        v.try_clone()
            .ok()
            .and_then(|v| <(u32, OwnedObjectPath)>::try_from(v).ok())
    });
    Ok(matches!(kind, Some("wayland" | "x11")) && user.is_some_and(|(id, _)| id == uid))
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
