//! A blocking client for the Crosspane Shell extension bridge, `io.frostdev.Crosspane.Shell1`
//! (WP-G1.2). Calls use a 2 s timeout (the frozen bound for non-input methods; overlay calls 45 ms).
//! An absent extension or a wrong `Version` is [`PlatformError::Unsupported`] at
//! [`ShellBridge::connect`]; a bus name that vanishes later makes calls fail with `Backend` and
//! subscribers get [`ShellEvent::Lost`].
//!
//! A bridge holds three session-bus connections: `calls` (2 s method timeout), `overlay` (45 ms, so
//! overlay updates never wait long) and a signal connection that only the signal thread iterates.
//! The signal thread holds no reference to the bridge, so dropping the last [`ShellBridge`] closes
//! that connection and joins the thread.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crosspane_platform::PlatformError;
use zbus::blocking::connection::Builder;
use zbus::blocking::{Connection, MessageIterator};
use zbus::zvariant::OwnedValue;
use zbus::{DBusError, MatchRule, Message};

pub const BUS_NAME: &str = "io.frostdev.Crosspane.Shell";
pub const OBJECT_PATH: &str = "/io/frostdev/Crosspane/Shell";
pub const INTERFACE: &str = "io.frostdev.Crosspane.Shell1";
pub const VERSION: u32 = 1;

const DBUS: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const NAME_HAS_NO_OWNER: &str = "org.freedesktop.DBus.Error.NameHasNoOwner";
const CALL_TIMEOUT: Duration = Duration::from_secs(2);
const OVERLAY_TIMEOUT: Duration = Duration::from_millis(45);
/// Bounds all of `connect` and `subscribe`, connection setup included (zbus's method timeout does
/// not cover authentication).
const SETUP_TIMEOUT: Duration = Duration::from_secs(2);
/// The largest width or height `MoveResize` accepts.
const MAX_SIZE: i32 = 32_768;
const NOT_RUNNING: &str = "the Crosspane Shell extension is not running";
const NOT_EXPORTED: &str = "the Crosspane Shell extension does not export the expected interface";
const OTHER_VERSION: &str = "the Crosspane Shell extension speaks another bridge version";

/// One window as the bridge reports it (`ListWindows`, frame rect in global logical coordinates).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellWindow {
    pub id: u64,
    pub app_id: String,
    pub title: String,
    pub pid: u32,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub focused: bool,
    pub minimized: bool,
    pub fullscreen: bool,
}

/// Events from the bridge, delivered on its signal thread in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellEvent {
    /// `WindowsChanged` for this epoch.
    WindowsChanged { epoch: u64 },
    /// `OverlayState`.
    OverlayState { id: u32, visible: bool },
    /// The bus name lost its owner or got a new one (Shell restart, extension disabled or
    /// re-enabled). Every window id and overlay from before is gone.
    Lost,
}

/// Called on the bridge's signal thread. Must not block.
pub type ShellCallback = Arc<dyn Fn(ShellEvent) + Send + Sync>;

/// One row of `ListWindows`, in wire order: `(tssuiiiibbb)`.
type WindowRow = (
    u64,
    String,
    String,
    u32,
    i32,
    i32,
    i32,
    i32,
    bool,
    bool,
    bool,
);

/// The signal connection and its thread, held by the bridge so that dropping it stops both.
#[derive(Debug)]
struct SignalHandle {
    connection: Connection,
    thread: JoinHandle<()>,
}

struct Inner {
    /// Every call except the overlay ones (2 s method timeout).
    calls: Connection,
    /// `ShowOverlay` and `HideOverlay` only (45 ms method timeout).
    overlay: Connection,
    epoch: u64,
    /// The unique name that owned [`BUS_NAME`] at connect.
    owner: String,
    /// Set by the signal thread when the bridge is lost; calls then fail fast.
    lost: Arc<AtomicBool>,
    /// Set when the bridge is dropped, so the signal thread ends without reporting `Lost`.
    stopping: Arc<AtomicBool>,
    /// Every subscriber's callback, in registration order. Shared with the signal thread.
    callbacks: Arc<Mutex<Vec<ShellCallback>>>,
    /// The signal connection and thread, built by the first subscribe. Held under this lock while
    /// subscribing, so only one subscriber sets the signal machinery up.
    signals: Mutex<Option<SignalHandle>>,
}

impl fmt::Debug for Inner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Inner")
            .field("epoch", &self.epoch)
            .field("lost", &self.lost.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        let handle = self
            .signals
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(SignalHandle { connection, thread }) = handle {
            // Closing the dedicated connection wakes the blocking signal iteration.
            let _ = connection.close();
            // The callback may hold the last clone of the bridge, so this can run on the signal
            // thread itself. Joining that thread from itself would never return.
            if thread.thread().id() != thread::current().id() {
                let _ = thread.join();
            }
        }
    }
}

/// A connection to the bridge on the session bus. Cloning shares the connection.
#[derive(Clone, Debug)]
pub struct ShellBridge {
    inner: Arc<Inner>,
}

impl ShellBridge {
    /// Connect to the session bus, check the name has an owner and `Version == VERSION`, read
    /// `ShellEpoch`.
    pub fn connect() -> Result<ShellBridge, PlatformError> {
        let inner = bounded(open)?;
        Ok(ShellBridge {
            inner: Arc::new(inner),
        })
    }

    /// The epoch read at connect (or the latest one after `Lost` handling by the caller).
    pub fn epoch(&self) -> u64 {
        self.inner.epoch
    }

    /// Subscribe to `WindowsChanged`, `OverlayState` and name-owner changes. Called once.
    pub fn subscribe(&self, callback: ShellCallback) -> Result<(), PlatformError> {
        let inner = &self.inner;
        // Held for the whole call: one subscriber at a time builds the signal machinery.
        let mut signals = inner.signals.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.lost.load(Ordering::SeqCst) {
            return Err(PlatformError::Backend("Shell bridge lost".into()));
        }
        // Registered before the signal thread can run, so the first subscriber misses no event.
        lock(&inner.callbacks).push(callback);
        if signals.is_some() {
            return Ok(());
        }
        match start_signals(inner) {
            Ok(handle) => {
                *signals = Some(handle);
                Ok(())
            }
            Err(error) => {
                // Nobody else can push while `signals` is held, so the last entry is ours.
                lock(&inner.callbacks).pop();
                Err(error)
            }
        }
    }

    /// `ListWindows`. An epoch different from [`ShellBridge::epoch`] is an error (`Backend`).
    pub fn list_windows(&self) -> Result<Vec<ShellWindow>, PlatformError> {
        let reply = self.call(&self.inner.calls, "ListWindows", &())?;
        let (epoch, rows): (u64, Vec<WindowRow>) =
            reply.body().deserialize().map_err(|_| malformed_reply())?;
        if epoch != self.epoch() {
            return Err(PlatformError::Backend(
                "Shell bridge: window list from another epoch".into(),
            ));
        }
        Ok(rows.into_iter().map(window).collect())
    }

    /// `Activate`; `false` from the bridge is [`PlatformError::NotFound`].
    pub fn activate(&self, id: u64) -> Result<(), PlatformError> {
        let reply = self.call(&self.inner.calls, "Activate", &(id,))?;
        found(&reply)
    }

    /// `MoveResize`; `false` is [`PlatformError::NotFound`].
    pub fn move_resize(
        &self,
        id: u64,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<(), PlatformError> {
        check_size(width, height)?;
        let reply = self.call(&self.inner.calls, "MoveResize", &(id, x, y, width, height))?;
        found(&reply)
    }

    /// `SetMinimized`; `false` is [`PlatformError::NotFound`].
    pub fn set_minimized(&self, id: u64, minimized: bool) -> Result<(), PlatformError> {
        let reply = self.call(&self.inner.calls, "SetMinimized", &(id, minimized))?;
        found(&reply)
    }

    /// `Close`; `false` is [`PlatformError::NotFound`].
    pub fn close(&self, id: u64) -> Result<(), PlatformError> {
        let reply = self.call(&self.inner.calls, "Close", &(id,))?;
        found(&reply)
    }

    /// `ShowOverlay` (45 ms timeout).
    pub fn show_overlay(
        &self,
        id: u32,
        x: i32,
        y: i32,
        anchor: u32,
        text: &str,
        accent: u32,
    ) -> Result<(), PlatformError> {
        check_overlay(anchor, accent)?;
        self.call(
            &self.inner.overlay,
            "ShowOverlay",
            &(id, x, y, anchor, text, accent),
        )?;
        Ok(())
    }

    /// `HideOverlay` (45 ms timeout).
    pub fn hide_overlay(&self, id: u32) -> Result<(), PlatformError> {
        self.call(&self.inner.overlay, "HideOverlay", &(id,))?;
        Ok(())
    }

    /// One method call on `connection`. Fails fast once the bridge is lost.
    fn call<B>(
        &self,
        connection: &Connection,
        method: &str,
        body: &B,
    ) -> Result<Message, PlatformError>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        if self.inner.lost.load(Ordering::SeqCst) {
            return Err(PlatformError::Backend("Shell bridge lost".into()));
        }
        bridge_call(connection, method, body)
    }
}

/// One method call on the bridge's interface. Reply errors are mapped by [`dbus_error`].
fn bridge_call<B>(connection: &Connection, method: &str, body: &B) -> Result<Message, PlatformError>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    connection
        .call_method(Some(BUS_NAME), OBJECT_PATH, Some(INTERFACE), method, body)
        .map_err(dbus_error)
}

/// The whole of [`ShellBridge::connect`], run on a worker by [`bounded`].
fn open() -> Result<Inner, PlatformError> {
    let calls = session_connection(CALL_TIMEOUT)?;
    let overlay = session_connection(OVERLAY_TIMEOUT)?;
    let Some(owner) = owner_of(&calls, BUS_NAME)? else {
        return Err(PlatformError::Unsupported(NOT_RUNNING));
    };
    let properties = get_all(&calls)?;
    if properties
        .get("Version")
        .and_then(|v| u32::try_from(v).ok())
        != Some(VERSION)
    {
        return Err(PlatformError::Unsupported(OTHER_VERSION));
    }
    let epoch = properties
        .get("ShellEpoch")
        .and_then(|v| u64::try_from(v).ok())
        .ok_or_else(malformed_properties)?;
    // The same owner before and after the read: the properties came from one instance.
    if owner_of(&calls, BUS_NAME)?.as_deref() != Some(owner.as_str()) {
        return Err(PlatformError::Backend(
            "Shell bridge: the extension restarted while connecting".into(),
        ));
    }
    Ok(Inner {
        calls,
        overlay,
        epoch,
        owner,
        lost: Arc::new(AtomicBool::new(false)),
        stopping: Arc::new(AtomicBool::new(false)),
        callbacks: Arc::new(Mutex::new(Vec::new())),
        signals: Mutex::new(None),
    })
}

/// The signal connection, the match rules and the owner check of [`ShellBridge::subscribe`].
fn open_signals(owner: &str) -> Result<(Connection, MessageIterator), PlatformError> {
    let connection = session_connection(CALL_TIMEOUT)?;
    // The iterator exists before AddMatch, so no signal sent once the rules are installed is lost.
    let messages = MessageIterator::from(&connection);
    let rules = [
        MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(owner)
            .map_err(dbus_error)?
            .path(OBJECT_PATH)
            .map_err(dbus_error)?
            .interface(INTERFACE)
            .map_err(dbus_error)?
            .build(),
        MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(DBUS)
            .map_err(dbus_error)?
            .interface(DBUS)
            .map_err(dbus_error)?
            .member("NameOwnerChanged")
            .map_err(dbus_error)?
            .add_arg(BUS_NAME)
            .map_err(dbus_error)?
            .build(),
    ];
    for rule in rules {
        connection
            .call_method(
                Some(DBUS),
                DBUS_PATH,
                Some(DBUS),
                "AddMatch",
                &(rule.to_string(),),
            )
            .map_err(dbus_error)?;
    }
    if owner_of(&connection, BUS_NAME)?.as_deref() != Some(owner) {
        return Err(PlatformError::Backend(
            "Shell bridge: the extension restarted before subscribing".into(),
        ));
    }
    Ok((connection, messages))
}

/// The signal thread: decodes bridge signals in order until the bridge is lost. It holds only what
/// it needs, never the bridge itself, so no reference cycle keeps the bridge alive.
fn run_signals(
    messages: MessageIterator,
    owner: String,
    epoch: u64,
    callbacks: Arc<Mutex<Vec<ShellCallback>>>,
    lost: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
) {
    for message in messages {
        if stopping.load(Ordering::SeqCst) {
            return;
        }
        match message.and_then(|message| parse_signal(&message, &owner, epoch)) {
            Ok(Some(event)) => {
                let ends = matches!(event, ShellEvent::Lost);
                tracing::debug!(event = event_name(&event), "Shell bridge signal");
                if ends {
                    lost.store(true, Ordering::SeqCst);
                }
                dispatch(&callbacks, event);
                if ends {
                    return;
                }
            }
            Ok(None) => {}
            Err(_) => {
                tracing::debug!("Shell bridge signal could not be decoded");
                break;
            }
        }
    }
    // The stream ended or a signal could not be decoded: the bridge is gone, unless it is being
    // dropped right now (then nobody is left to hear about it).
    if !stopping.load(Ordering::SeqCst) {
        lost.store(true, Ordering::SeqCst);
        dispatch(&callbacks, ShellEvent::Lost);
    }
}

/// Delivers `event` to every registered callback, in registration order. The list is copied out
/// before any callback runs, so a callback may subscribe again without deadlocking.
fn dispatch(callbacks: &Mutex<Vec<ShellCallback>>, event: ShellEvent) {
    let snapshot = lock(callbacks).clone();
    for callback in &snapshot {
        callback(event.clone());
    }
}

/// Locks `mutex`, recovering the data from a poisoned lock: the callback list is always whole.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Builds the signal connection and starts the signal thread. Only the first subscriber calls it.
fn start_signals(inner: &Inner) -> Result<SignalHandle, PlatformError> {
    let owner = inner.owner.clone();
    let (connection, messages) = bounded(move || open_signals(&owner))?;
    let (owner, epoch) = (inner.owner.clone(), inner.epoch);
    let (callbacks, lost, stopping) = (
        inner.callbacks.clone(),
        inner.lost.clone(),
        inner.stopping.clone(),
    );
    let thread = thread::Builder::new()
        .name("crosspane-shell-signals".into())
        .spawn(move || run_signals(messages, owner, epoch, callbacks, lost, stopping))
        .map_err(|e| PlatformError::Backend(format!("Shell bridge: signal thread: {e}")))?;
    Ok(SignalHandle { connection, thread })
}

/// Decodes one message from the bridge's connection. Only a signal from the owner this bridge
/// connected to, on the bridge's object and interface, counts. The bus's `NameOwnerChanged` counts
/// when it concerns [`BUS_NAME`]. A malformed body is an error, which the signal thread treats as
/// a loss.
fn parse_signal(message: &Message, owner: &str, epoch: u64) -> zbus::Result<Option<ShellEvent>> {
    let header = message.header();
    if header.message_type() != zbus::message::Type::Signal {
        return Ok(None);
    }
    let interface = header.interface().map(|v| v.as_str());
    let member = header.member().map(|v| v.as_str());
    let path = header.path().map(|v| v.as_str());
    let sender = header.sender().map(|v| v.as_str());
    if sender == Some(DBUS) && interface == Some(DBUS) && member == Some("NameOwnerChanged") {
        let (name, _old, _new): (String, String, String) = message.body().deserialize()?;
        return Ok((name == BUS_NAME).then_some(ShellEvent::Lost));
    }
    if sender != Some(owner) || path != Some(OBJECT_PATH) || interface != Some(INTERFACE) {
        return Ok(None);
    }
    match member {
        Some("WindowsChanged") => {
            let changed: u64 = message.body().deserialize()?;
            Ok(Some(if changed == epoch {
                ShellEvent::WindowsChanged { epoch }
            } else {
                // The extension was re-enabled or restarted: its window ids are from another epoch.
                ShellEvent::Lost
            }))
        }
        Some("OverlayState") => {
            let (id, visible): (u32, bool) = message.body().deserialize()?;
            Ok(Some(ShellEvent::OverlayState { id, visible }))
        }
        _ => Ok(None),
    }
}

fn event_name(event: &ShellEvent) -> &'static str {
    match event {
        ShellEvent::WindowsChanged { .. } => "WindowsChanged",
        ShellEvent::OverlayState { .. } => "OverlayState",
        ShellEvent::Lost => "Lost",
    }
}

fn window(row: WindowRow) -> ShellWindow {
    let (id, app_id, title, pid, x, y, width, height, focused, minimized, fullscreen) = row;
    ShellWindow {
        id,
        app_id,
        title,
        pid,
        x,
        y,
        width,
        height,
        focused,
        minimized,
        fullscreen,
    }
}

/// A `bool` reply: `true` is success and `false` (the id is unknown) is [`PlatformError::NotFound`].
fn found(reply: &Message) -> Result<(), PlatformError> {
    let found: bool = reply.body().deserialize().map_err(|_| malformed_reply())?;
    if found {
        Ok(())
    } else {
        Err(PlatformError::NotFound)
    }
}

/// Window sizes the bridge accepts. Rejected before any call.
fn check_size(width: i32, height: i32) -> Result<(), PlatformError> {
    if (1..=MAX_SIZE).contains(&width) && (1..=MAX_SIZE).contains(&height) {
        Ok(())
    } else {
        Err(PlatformError::Backend("Shell bridge: invalid size".into()))
    }
}

/// Overlay anchors are 0 to 3 and accents are `0xRRGGBB`. Rejected before any call.
fn check_overlay(anchor: u32, accent: u32) -> Result<(), PlatformError> {
    if anchor <= 3 && accent <= 0x00FF_FFFF {
        Ok(())
    } else {
        Err(PlatformError::Backend(
            "Shell bridge: invalid overlay argument".into(),
        ))
    }
}

/// Runs `operation` on a worker thread and waits at most [`SETUP_TIMEOUT`] for its result. A worker
/// that outlives the wait finishes on its own and drops what it made.
fn bounded<T, F>(operation: F) -> Result<T, PlatformError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, PlatformError> + Send + 'static,
{
    let (tx, rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("crosspane-shell-setup".into())
        .spawn(move || {
            let _ = tx.send(operation());
        })
        .map_err(|e| PlatformError::Backend(format!("Shell bridge: setup thread: {e}")))?;
    match rx.recv_timeout(SETUP_TIMEOUT) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(PlatformError::Timeout),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(PlatformError::Backend(
            "Shell bridge: setup thread stopped".into(),
        )),
    }
}

fn session_connection(timeout: Duration) -> Result<Connection, PlatformError> {
    let builder = Builder::session().map_err(session_bus)?;
    builder.method_timeout(timeout).build().map_err(session_bus)
}

fn session_bus(error: zbus::Error) -> PlatformError {
    PlatformError::Backend(format!("Shell bridge: session bus: {error}"))
}

/// The unique name that owns `name`, or `None` when nothing owns it.
fn owner_of(connection: &Connection, name: &str) -> Result<Option<String>, PlatformError> {
    match connection.call_method(Some(DBUS), DBUS_PATH, Some(DBUS), "GetNameOwner", &(name,)) {
        Ok(reply) => {
            let owner: String = reply.body().deserialize().map_err(|_| malformed_reply())?;
            Ok(Some(owner))
        }
        Err(error) if reply_error_name(&error).as_deref() == Some(NAME_HAS_NO_OWNER) => Ok(None),
        Err(error) => Err(dbus_error(error)),
    }
}

/// `org.freedesktop.DBus.Properties.GetAll` on the bridge, as a map.
fn get_all(calls: &Connection) -> Result<HashMap<String, OwnedValue>, PlatformError> {
    let reply = match calls.call_method(
        Some(BUS_NAME),
        OBJECT_PATH,
        Some(PROPERTIES),
        "GetAll",
        &(INTERFACE,),
    ) {
        Ok(reply) => reply,
        Err(error) if is_unknown_export(&error) => {
            return Err(PlatformError::Unsupported(NOT_EXPORTED));
        }
        Err(error) => return Err(dbus_error(error)),
    };
    let properties: HashMap<String, OwnedValue> = reply
        .body()
        .deserialize()
        .map_err(|_| malformed_properties())?;
    Ok(properties)
}

/// Errors that mean the object or interface at the bridge's name is not the one this client was
/// built for.
fn is_unknown_export(error: &zbus::Error) -> bool {
    matches!(
        reply_error_name(error).as_deref(),
        Some(
            "org.freedesktop.DBus.Error.UnknownObject"
                | "org.freedesktop.DBus.Error.UnknownInterface"
                | "org.freedesktop.DBus.Error.UnknownMethod"
                | "org.freedesktop.DBus.Error.InvalidArgs"
        )
    )
}

/// The D-Bus error name of an error reply, whichever way zbus reports it.
fn reply_error_name(error: &zbus::Error) -> Option<String> {
    match error {
        zbus::Error::MethodError(name, _, _) => Some(name.as_str().to_owned()),
        zbus::Error::FDO(error) => Some(error.name().as_str().to_owned()),
        _ => None,
    }
}

/// Maps a D-Bus failure to a [`PlatformError`]. Only the error name is kept: a reply's description
/// and body are never copied into an error.
fn dbus_error(error: zbus::Error) -> PlatformError {
    if let Some(name) = reply_error_name(&error) {
        return match name.as_str() {
            "org.freedesktop.DBus.Error.Timeout"
            | "org.freedesktop.DBus.Error.TimedOut"
            | "org.freedesktop.DBus.Error.NoReply" => PlatformError::Timeout,
            "org.freedesktop.DBus.Error.ServiceUnknown" | NAME_HAS_NO_OWNER => {
                PlatformError::Backend("Shell bridge: the extension is not running".into())
            }
            _ => PlatformError::Backend(format!("Shell bridge: {name}")),
        };
    }
    match &error {
        zbus::Error::InputOutput(e) | zbus::Error::Connection(e, _)
            if e.kind() == std::io::ErrorKind::TimedOut =>
        {
            PlatformError::Timeout
        }
        _ => PlatformError::Backend(format!("Shell bridge: {error}")),
    }
}

fn malformed_reply() -> PlatformError {
    PlatformError::Backend("Shell bridge: malformed reply".into())
}

fn malformed_properties() -> PlatformError {
    PlatformError::Backend("Shell bridge: malformed properties".into())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Instant;

    const OWNER: &str = ":1.5";

    fn signal(
        sender: &str,
        path: &str,
        interface: &str,
        member: &str,
        body: &(impl serde::Serialize + zbus::zvariant::DynamicType),
    ) -> Message {
        Message::signal(path, interface, member)
            .unwrap()
            .sender(sender)
            .unwrap()
            .build(body)
            .unwrap()
    }

    fn decode(message: &Message) -> Option<ShellEvent> {
        parse_signal(message, OWNER, 7).unwrap()
    }

    /// A D-Bus error reply of the given name, converted by zbus as for a real call.
    fn reply(name: &str) -> zbus::Error {
        let call = Message::method_call(OBJECT_PATH, "ListWindows")
            .unwrap()
            .build(&())
            .unwrap();
        zbus::Error::from(
            Message::error(&call.header(), name)
                .unwrap()
                .build(&"detail")
                .unwrap(),
        )
    }

    #[test]
    fn parse_signal_decodes_the_bridge_signals() {
        assert_eq!(
            decode(&signal(
                OWNER,
                OBJECT_PATH,
                INTERFACE,
                "WindowsChanged",
                &(7u64,)
            )),
            Some(ShellEvent::WindowsChanged { epoch: 7 })
        );
        // Another epoch means the extension was re-enabled: the bridge is lost.
        assert_eq!(
            decode(&signal(
                OWNER,
                OBJECT_PATH,
                INTERFACE,
                "WindowsChanged",
                &(8u64,)
            )),
            Some(ShellEvent::Lost)
        );
        assert_eq!(
            decode(&signal(
                OWNER,
                OBJECT_PATH,
                INTERFACE,
                "OverlayState",
                &(9001u32, true)
            )),
            Some(ShellEvent::OverlayState {
                id: 9001,
                visible: true
            })
        );
        assert_eq!(
            decode(&signal(
                OWNER,
                OBJECT_PATH,
                INTERFACE,
                "OverlayState",
                &(9001u32, false)
            )),
            Some(ShellEvent::OverlayState {
                id: 9001,
                visible: false
            })
        );
        assert_eq!(
            decode(&signal(OWNER, OBJECT_PATH, INTERFACE, "Unknown", &())),
            None
        );
    }

    #[test]
    fn parse_signal_ignores_other_senders_paths_and_interfaces() {
        assert_eq!(
            decode(&signal(
                ":1.99",
                OBJECT_PATH,
                INTERFACE,
                "WindowsChanged",
                &(7u64,)
            )),
            None
        );
        assert_eq!(
            decode(&signal(
                OWNER,
                "/other/path",
                INTERFACE,
                "WindowsChanged",
                &(7u64,)
            )),
            None
        );
        assert_eq!(
            decode(&signal(
                OWNER,
                OBJECT_PATH,
                "io.frostdev.Other",
                "WindowsChanged",
                &(7u64,)
            )),
            None
        );
    }

    #[test]
    fn name_owner_changes_of_the_bridge_name_are_lost() {
        let name_owner_changed = |name: &str| {
            signal(
                DBUS,
                DBUS_PATH,
                DBUS,
                "NameOwnerChanged",
                &(name, OWNER, ""),
            )
        };
        assert_eq!(
            decode(&name_owner_changed(BUS_NAME)),
            Some(ShellEvent::Lost)
        );
        assert_eq!(decode(&name_owner_changed("org.example.Other")), None);
    }

    #[test]
    fn dbus_errors_map_to_platform_errors() {
        let timed_out = zbus::Error::InputOutput(Arc::new(std::io::ErrorKind::TimedOut.into()));
        assert!(matches!(dbus_error(timed_out), PlatformError::Timeout));
        assert!(matches!(
            dbus_error(reply("org.freedesktop.DBus.Error.NoReply")),
            PlatformError::Timeout
        ));
        assert!(matches!(
            dbus_error(zbus::Error::FDO(Box::new(zbus::fdo::Error::Timeout(
                "slow".into()
            )))),
            PlatformError::Timeout
        ));
        let not_running = dbus_error(zbus::Error::FDO(Box::new(
            zbus::fdo::Error::NameHasNoOwner("gone".into()),
        )));
        assert_eq!(
            not_running.to_string(),
            "Shell bridge: the extension is not running"
        );
        let named = dbus_error(reply("org.freedesktop.DBus.Error.AccessDenied"));
        assert_eq!(
            named.to_string(),
            "Shell bridge: org.freedesktop.DBus.Error.AccessDenied"
        );
        assert!(!named.to_string().contains("detail"));
    }

    #[test]
    fn unknown_export_errors_are_recognised() {
        assert!(is_unknown_export(&reply(
            "org.freedesktop.DBus.Error.UnknownObject"
        )));
        assert!(is_unknown_export(&reply(
            "org.freedesktop.DBus.Error.UnknownInterface"
        )));
        assert!(!is_unknown_export(&reply(
            "org.freedesktop.DBus.Error.AccessDenied"
        )));
    }

    #[test]
    fn size_and_overlay_arguments_are_checked() {
        assert!(check_size(1, 1).is_ok());
        assert!(check_size(MAX_SIZE, MAX_SIZE).is_ok());
        assert!(check_size(0, 1).is_err());
        assert!(check_size(1, MAX_SIZE + 1).is_err());
        assert!(check_size(-5, 10).is_err());
        assert!(check_overlay(3, 0x00FF_FFFF).is_ok());
        assert!(check_overlay(4, 0).is_err());
        assert!(check_overlay(0, 0x0100_0000).is_err());
    }

    #[test]
    fn dispatch_reaches_every_callback_in_registration_order() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let callbacks = Mutex::new(Vec::new());
        for label in ["first", "second"] {
            let seen = seen.clone();
            let callback: ShellCallback = Arc::new(move |event| {
                seen.lock().unwrap().push((label, event));
            });
            callbacks.lock().unwrap().push(callback);
        }
        dispatch(&callbacks, ShellEvent::Lost);
        dispatch(
            &callbacks,
            ShellEvent::OverlayState {
                id: 1,
                visible: true,
            },
        );
        let overlay = ShellEvent::OverlayState {
            id: 1,
            visible: true,
        };
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                ("first", ShellEvent::Lost),
                ("second", ShellEvent::Lost),
                ("first", overlay.clone()),
                ("second", overlay),
            ]
        );
    }

    fn wait_for(events: &mpsc::Receiver<ShellEvent>, wanted: &ShellEvent) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let event = events.recv_timeout(left).unwrap();
            if &event == wanted {
                return;
            }
        }
    }

    /// Runs only against a live Crosspane Shell extension (`CROSSPANE_GNOME_SHELL_LIVE=1`). It draws
    /// one labelled overlay and hides it again. It never touches windows.
    #[test]
    fn live_smoke() {
        if std::env::var("CROSSPANE_GNOME_SHELL_LIVE").as_deref() != Ok("1") {
            return;
        }
        let bridge = ShellBridge::connect().unwrap();
        let windows = bridge.list_windows().unwrap();
        assert!(windows.iter().all(|w| w.width > 0 && w.height > 0));
        println!("Shell bridge windows: {}", windows.len());
        let (tx, rx) = mpsc::channel();
        let callback: ShellCallback = Arc::new(move |event| {
            let _ = tx.send(event);
        });
        bridge.subscribe(callback).unwrap();
        bridge
            .show_overlay(9001, 200, 200, 3, "Crosspane test", 0x3584e4)
            .unwrap();
        wait_for(
            &rx,
            &ShellEvent::OverlayState {
                id: 9001,
                visible: true,
            },
        );
        bridge.hide_overlay(9001).unwrap();
        wait_for(
            &rx,
            &ShellEvent::OverlayState {
                id: 9001,
                visible: false,
            },
        );
    }
}
