//! A blocking client for the Crosspane Shell extension bridge, `io.frostdev.Crosspane.Shell1`
//! (WP-G1.2; v2 additions WP-G2.4 task C). Calls use a 2 s timeout (the frozen bound for non-input
//! methods; overlay calls 45 ms). An absent extension or a `Version` outside
//! [`MIN_VERSION`]`..=`[`VERSION`] is [`PlatformError::Unsupported`] at [`ShellBridge::connect`]; a
//! bus name that vanishes later makes calls fail with `Backend` and subscribers get
//! [`ShellEvent::Lost`].
//!
//! Version 2 adds the pointer fence, the layout snapshot and cursor hiding. A version-1 extension
//! (an old copy the Shell has not reloaded yet) is accepted, because windows and overlays work
//! with it; each v2 call on it fails with [`PlatformError::Unsupported`] without touching the bus.
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
/// The newest bridge version this client speaks (the one the packaged extension exports).
pub const VERSION: u32 = 2;
/// The oldest bridge version [`ShellBridge::connect`] accepts.
pub const MIN_VERSION: u32 = 1;

const DBUS: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const NAME_HAS_NO_OWNER: &str = "org.freedesktop.DBus.Error.NameHasNoOwner";
const INVALID_ARGS: &str = "org.freedesktop.DBus.Error.InvalidArgs";
const CALL_TIMEOUT: Duration = Duration::from_secs(2);
const OVERLAY_TIMEOUT: Duration = Duration::from_millis(45);
/// Bounds all of `connect` and `subscribe`, connection setup included (zbus's method timeout does
/// not cover authentication).
const SETUP_TIMEOUT: Duration = Duration::from_secs(2);
/// The largest width or height `MoveResize` and `SetPointerFence` accept.
const MAX_SIZE: i32 = 32_768;
/// `SetPointerFence` coordinates are within plus or minus this (2^20), as in the interface.
const MAX_COORD: i32 = 1 << 20;
/// The most window ids one `RestoreLayout` call may skip. The interface does not bound the array;
/// both ends refuse more than this.
pub const MAX_SKIP: usize = 4096;
const NOT_RUNNING: &str = "the Crosspane Shell extension is not running";
const NOT_EXPORTED: &str = "the Crosspane Shell extension does not export the expected interface";
const OTHER_VERSION: &str = "the Crosspane Shell extension speaks another bridge version";
const NEEDS_V2: &str =
    "the Crosspane Shell extension is version 1; log out and back in to load version 2";

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

/// Which bus the bridge lives on: the user's session bus, or (tests) a private daemon's address.
#[derive(Clone, Debug)]
enum Bus {
    Session,
    #[cfg(test)]
    Address(String),
}

struct Inner {
    /// Every call except the overlay ones (2 s method timeout).
    calls: Connection,
    /// `ShowOverlay`, `HideOverlay` and `InhibitCursor` only (45 ms method timeout).
    overlay: Connection,
    /// The bus `calls` and `overlay` are on; the signal connection opens on the same one.
    bus: Bus,
    /// The bridge version read at connect, in [`MIN_VERSION`]`..=`[`VERSION`].
    version: u32,
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
            .field("version", &self.version)
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
    /// Connect to the session bus, check the name has an owner and `Version` is in
    /// [`MIN_VERSION`]`..=`[`VERSION`], read `ShellEpoch`.
    pub fn connect() -> Result<ShellBridge, PlatformError> {
        Self::connect_on(Bus::Session)
    }

    /// [`ShellBridge::connect`] on a private bus (tests).
    #[cfg(test)]
    fn connect_at(address: &str) -> Result<ShellBridge, PlatformError> {
        Self::connect_on(Bus::Address(address.to_owned()))
    }

    fn connect_on(bus: Bus) -> Result<ShellBridge, PlatformError> {
        let inner = bounded(move || open(bus))?;
        Ok(ShellBridge {
            inner: Arc::new(inner),
        })
    }

    /// The epoch read at connect (or the latest one after `Lost` handling by the caller).
    pub fn epoch(&self) -> u64 {
        self.inner.epoch
    }

    /// The bridge version read at connect: 1 (no fence, layout snapshot or cursor hiding) or 2.
    pub fn version(&self) -> u32 {
        self.inner.version
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

    /// `SetPointerFence` (v2): fence the local pointer out of the rectangle (`x`, `y`, `width`,
    /// `height`, global logical coordinates) with four barriers that only let motion pass
    /// outward, replacing any earlier fence. `width` and `height` are `1..=32768` and `x` and `y`
    /// within plus or minus 2^20, checked before the call. The extension is stricter: Mutter's
    /// barriers take coordinates `0..=32767` only, so a rectangle with a negative `x` or `y`, or
    /// whose right or bottom edge (`x + width`, `y + height`) is past 32767, is refused with
    /// `Backend` (the old fence stays). The barriers stop injected motion too (relative, and
    /// absolute moves such as an EIS warp), so the caller clears the fence while it injects into
    /// the rectangle. A pointer stopped by the fence rests on the pixel just outside the
    /// rectangle. The fence belongs to this bridge's connection: the extension removes it when the
    /// connection goes away.
    pub fn set_pointer_fence(
        &self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<(), PlatformError> {
        self.require_v2()?;
        check_fence(x, y, width, height)?;
        self.call(&self.inner.calls, "SetPointerFence", &(x, y, width, height))?;
        Ok(())
    }

    /// `ClearPointerFence` (v2). Clearing no fence succeeds.
    pub fn clear_pointer_fence(&self) -> Result<(), PlatformError> {
        self.require_v2()?;
        self.call(&self.inner.calls, "ClearPointerFence", &())?;
        Ok(())
    }

    /// `SaveLayout` (v2): snapshot every eligible window's frame rect, maximize flags and
    /// fullscreen state. The extension keeps at most 4 snapshots (a fifth drops the oldest) and
    /// returns a nonzero token.
    pub fn save_layout(&self) -> Result<u32, PlatformError> {
        self.require_v2()?;
        let reply = self.call(&self.inner.calls, "SaveLayout", &())?;
        let token: u32 = reply.body().deserialize().map_err(|_| malformed_reply())?;
        if token == 0 {
            // Tokens are never zero: a zero would read as "no snapshot" to every caller.
            return Err(malformed_reply());
        }
        Ok(token)
    }

    /// `RestoreLayout` (v2): put back the windows of snapshot `token` that still exist and whose
    /// state differs, except the window ids in `skip` (the windows being parked). Returns how many
    /// windows changed and drops the snapshot. A token the extension does not know (never issued,
    /// already used, dropped as the oldest of five, or lost with a Shell restart) is
    /// [`PlatformError::NotFound`].
    pub fn restore_layout(&self, token: u32, skip: &[u64]) -> Result<u32, PlatformError> {
        self.require_v2()?;
        check_skip(skip)?;
        if token == 0 {
            // The extension never issues token 0.
            return Err(PlatformError::NotFound);
        }
        if self.inner.lost.load(Ordering::SeqCst) {
            return Err(PlatformError::Backend("Shell bridge lost".into()));
        }
        let reply = match self.inner.calls.call_method(
            Some(BUS_NAME),
            OBJECT_PATH,
            Some(INTERFACE),
            "RestoreLayout",
            &(token, skip),
        ) {
            Ok(reply) => reply,
            // The arguments were checked above, so InvalidArgs is the extension's answer for an
            // unknown token.
            Err(error) if reply_error_name(&error).as_deref() == Some(INVALID_ARGS) => {
                return Err(PlatformError::NotFound);
            }
            Err(error) => return Err(dbus_error(error)),
        };
        reply.body().deserialize().map_err(|_| malformed_reply())
    }

    /// `InhibitCursor` (v2): hide (`true`) or show (`false`) the local pointer while the agent
    /// captures input. Idempotent per state on the extension's side. Uses the 45 ms overlay
    /// connection, so it never stalls the input path for long. The hidden state belongs to the
    /// overlay connection of the latest `inhibit_cursor(true)` caller: if that connection goes away
    /// (the agent died) the extension shows the cursor again. A `Timeout` leaves the state unknown;
    /// the call is idempotent, so retry it.
    pub fn inhibit_cursor(&self, inhibit: bool) -> Result<(), PlatformError> {
        self.require_v2()?;
        self.call(&self.inner.overlay, "InhibitCursor", &(inhibit,))?;
        Ok(())
    }

    /// The v2 methods exist only on a version-2 extension.
    fn require_v2(&self) -> Result<(), PlatformError> {
        if self.inner.version >= 2 {
            Ok(())
        } else {
            Err(PlatformError::Unsupported(NEEDS_V2))
        }
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
fn open(bus: Bus) -> Result<Inner, PlatformError> {
    let calls = bus_connection(&bus, CALL_TIMEOUT)?;
    let overlay = bus_connection(&bus, OVERLAY_TIMEOUT)?;
    let Some(owner) = owner_of(&calls, BUS_NAME)? else {
        return Err(PlatformError::Unsupported(NOT_RUNNING));
    };
    let properties = get_all(&calls)?;
    let version = properties
        .get("Version")
        .and_then(|v| u32::try_from(v).ok())
        .filter(|v| (MIN_VERSION..=VERSION).contains(v))
        .ok_or(PlatformError::Unsupported(OTHER_VERSION))?;
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
        bus,
        version,
        epoch,
        owner,
        lost: Arc::new(AtomicBool::new(false)),
        stopping: Arc::new(AtomicBool::new(false)),
        callbacks: Arc::new(Mutex::new(Vec::new())),
        signals: Mutex::new(None),
    })
}

/// The signal connection, the match rules and the owner check of [`ShellBridge::subscribe`].
fn open_signals(bus: &Bus, owner: &str) -> Result<(Connection, MessageIterator), PlatformError> {
    let connection = bus_connection(bus, CALL_TIMEOUT)?;
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
    let (bus, owner) = (inner.bus.clone(), inner.owner.clone());
    let (connection, messages) = bounded(move || open_signals(&bus, &owner))?;
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

/// The fence rectangle the bridge accepts: `x` and `y` within plus or minus 2^20, `width` and
/// `height` in `1..=32768`. Rejected before any call.
fn check_fence(x: i32, y: i32, width: i32, height: i32) -> Result<(), PlatformError> {
    let coords = -MAX_COORD..=MAX_COORD;
    if coords.contains(&x) && coords.contains(&y) {
        check_size(width, height)
    } else {
        Err(PlatformError::Backend(
            "Shell bridge: invalid fence position".into(),
        ))
    }
}

/// The `RestoreLayout` skip list: at most [`MAX_SKIP`] ids, each exactly representable as a JS
/// number (the extension unpacks `t` values as numbers). Rejected before any call.
fn check_skip(skip: &[u64]) -> Result<(), PlatformError> {
    const MAX_SAFE_ID: u64 = (1 << 53) - 1;
    if skip.len() <= MAX_SKIP && skip.iter().all(|id| *id <= MAX_SAFE_ID) {
        Ok(())
    } else {
        Err(PlatformError::Backend(
            "Shell bridge: invalid skip list".into(),
        ))
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

fn bus_connection(bus: &Bus, timeout: Duration) -> Result<Connection, PlatformError> {
    let builder = match bus {
        Bus::Session => Builder::session(),
        #[cfg(test)]
        Bus::Address(address) => Builder::address(address.as_str()),
    }
    .map_err(session_bus)?;
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
    use std::fs;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::time::Instant;

    use zbus::message::Header;

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

    // ---- a private bus and a fake extension on it -------------------------------------------------

    /// How long a fake's private bus may take to come up.
    const WAIT: Duration = Duration::from_secs(5);
    /// The epoch the fake reports.
    const FAKE_EPOCH: u64 = 7;
    /// The fence coordinate limit of the interface (2^20), pinned here independently of the client.
    const COORD: i32 = 1 << 20;
    /// The frozen interface, as the packaging XML states it.
    const FROZEN_XML: &str = include_str!(
        "../../../../packaging/gnome-shell-extension/crosspane@frostdev.io/io.frostdev.Crosspane.Shell1.xml"
    );

    /// A `dbus-daemon` child with a private socket, killed on drop.
    struct Daemon {
        child: Child,
        dir: PathBuf,
        address: String,
    }

    impl Daemon {
        /// A bus with no names on it, or `None` when there is no `dbus-daemon`.
        fn start() -> Option<Daemon> {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "crosspane-fake-shell-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let socket = dir.join("bus");
            let config = dir.join("bus.conf");
            fs::write(
                &config,
                format!(
                    "<!DOCTYPE busconfig PUBLIC \"-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN\" \
                     \"http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd\">\n\
                     <busconfig>\n\
                     <type>session</type>\n\
                     <listen>unix:path={}</listen>\n\
                     <auth>EXTERNAL</auth>\n\
                     <policy context=\"default\">\n\
                     <allow send_destination=\"*\" eavesdrop=\"true\"/>\n\
                     <allow eavesdrop=\"true\"/>\n\
                     <allow own=\"*\"/>\n\
                     </policy>\n\
                     </busconfig>\n",
                    socket.display()
                ),
            )
            .unwrap();
            let child = match Command::new("dbus-daemon")
                .arg("--config-file")
                .arg(&config)
                .arg("--nofork")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => child,
                Err(error) => {
                    eprintln!("skipping: cannot run dbus-daemon: {error}");
                    let _ = fs::remove_dir_all(&dir);
                    return None;
                }
            };
            let mut daemon = Daemon {
                child,
                address: format!("unix:path={}", socket.display()),
                dir,
            };
            let deadline = Instant::now() + WAIT;
            // Ready once it accepts a connection: the socket file can appear a moment before that.
            while UnixStream::connect(&socket).is_err() {
                assert!(Instant::now() < deadline, "dbus-daemon did not come up");
                if let Ok(Some(status)) = daemon.child.try_wait() {
                    panic!("dbus-daemon exited early: {status}");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Some(daemon)
        }
    }

    impl Drop for Daemon {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// One method call the fake received: the caller's unique bus name and the call as text.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Call {
        sender: String,
        line: String,
    }

    type Log = Arc<Mutex<Vec<Call>>>;

    #[derive(Default)]
    struct FakeState {
        /// The last token `SaveLayout` issued.
        last_token: u32,
        /// The tokens issued and not restored yet.
        live: Vec<u32>,
    }

    /// The fake extension: every method of the frozen interface, and `Version` as configured.
    struct FakeExtension {
        version: u32,
        log: Log,
        state: Mutex<FakeState>,
    }

    impl FakeExtension {
        /// Logs one call with the caller's unique bus name, so a test can tell connections apart.
        fn record(&self, header: &Header<'_>, line: String) {
            let sender = header
                .sender()
                .map(|name| name.as_str().to_owned())
                .unwrap_or_default();
            self.log.lock().unwrap().push(Call { sender, line });
        }
    }

    #[zbus::interface(name = "io.frostdev.Crosspane.Shell1")]
    impl FakeExtension {
        #[zbus(property)]
        fn version(&self) -> u32 {
            self.version
        }

        #[zbus(property)]
        fn shell_epoch(&self) -> u64 {
            FAKE_EPOCH
        }

        fn list_windows(&self, #[zbus(header)] header: Header<'_>) -> (u64, Vec<WindowRow>) {
            self.record(&header, "ListWindows".to_owned());
            (FAKE_EPOCH, Vec::new())
        }

        fn activate(&self, id: u64, #[zbus(header)] header: Header<'_>) -> bool {
            self.record(&header, format!("Activate {id}"));
            true
        }

        fn move_resize(
            &self,
            id: u64,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            #[zbus(header)] header: Header<'_>,
        ) -> bool {
            self.record(&header, format!("MoveResize {id} {x} {y} {width} {height}"));
            true
        }

        fn set_minimized(
            &self,
            id: u64,
            minimized: bool,
            #[zbus(header)] header: Header<'_>,
        ) -> bool {
            self.record(&header, format!("SetMinimized {id} {minimized}"));
            true
        }

        fn close(&self, id: u64, #[zbus(header)] header: Header<'_>) -> bool {
            self.record(&header, format!("Close {id}"));
            true
        }

        #[allow(clippy::too_many_arguments)]
        fn show_overlay(
            &self,
            id: u32,
            x: i32,
            y: i32,
            anchor: u32,
            text: String,
            accent: u32,
            #[zbus(header)] header: Header<'_>,
        ) {
            self.record(
                &header,
                format!("ShowOverlay {id} {x} {y} {anchor} {text:?} {accent}"),
            );
        }

        fn hide_overlay(&self, id: u32, #[zbus(header)] header: Header<'_>) {
            self.record(&header, format!("HideOverlay {id}"));
        }

        fn set_pointer_fence(
            &self,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            #[zbus(header)] header: Header<'_>,
        ) {
            self.record(&header, format!("SetPointerFence {x} {y} {width} {height}"));
        }

        fn clear_pointer_fence(&self, #[zbus(header)] header: Header<'_>) {
            self.record(&header, "ClearPointerFence".to_owned());
        }

        fn save_layout(&self, #[zbus(header)] header: Header<'_>) -> u32 {
            self.record(&header, "SaveLayout".to_owned());
            let mut state = self.state.lock().unwrap();
            state.last_token += 1;
            let token = state.last_token;
            state.live.push(token);
            token
        }

        fn restore_layout(
            &self,
            token: u32,
            skip: Vec<u64>,
            #[zbus(header)] header: Header<'_>,
        ) -> zbus::fdo::Result<u32> {
            self.record(&header, format!("RestoreLayout {token} {skip:?}"));
            let mut state = self.state.lock().unwrap();
            let Some(index) = state.live.iter().position(|live| *live == token) else {
                return Err(zbus::fdo::Error::InvalidArgs(format!(
                    "no layout snapshot {token}"
                )));
            };
            state.live.remove(index);
            Ok(3)
        }

        fn inhibit_cursor(&self, inhibit: bool, #[zbus(header)] header: Header<'_>) {
            self.record(&header, format!("InhibitCursor {inhibit}"));
        }
    }

    /// A fake extension owning the bridge name on its own private bus. Fields drop in order: the
    /// connection before the daemon.
    struct FakeShell {
        connection: Connection,
        log: Log,
        daemon: Daemon,
    }

    impl FakeShell {
        /// An extension reporting `Version` `version`, or `None` when there is no `dbus-daemon`.
        fn start(version: u32) -> Option<FakeShell> {
            let daemon = Daemon::start()?;
            let log = Log::default();
            let extension = FakeExtension {
                version,
                log: log.clone(),
                state: Mutex::new(FakeState::default()),
            };
            let connection = Builder::address(daemon.address.as_str())
                .unwrap()
                .name(BUS_NAME)
                .unwrap()
                .serve_at(OBJECT_PATH, extension)
                .unwrap()
                .build()
                .unwrap();
            Some(FakeShell {
                connection,
                log,
                daemon,
            })
        }

        fn calls(&self) -> Vec<Call> {
            self.log.lock().unwrap().clone()
        }

        fn lines(&self) -> Vec<String> {
            self.calls().into_iter().map(|call| call.line).collect()
        }
    }

    /// A bridge on the fake's private bus.
    fn bridge_on(shell: &FakeShell) -> ShellBridge {
        ShellBridge::connect_at(&shell.daemon.address).unwrap()
    }

    /// The message of an `Unsupported` result; any other result fails the test.
    fn unsupported<T: std::fmt::Debug>(result: Result<T, PlatformError>) -> &'static str {
        match result {
            Err(PlatformError::Unsupported(message)) => message,
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    /// The sender of the first call whose line starts with `prefix`.
    fn sender_of(calls: &[Call], prefix: &str) -> String {
        calls
            .iter()
            .find(|call| call.line.starts_with(prefix))
            .unwrap_or_else(|| panic!("no call starting with {prefix}"))
            .sender
            .clone()
    }

    /// What an introspection document says about the bridge interface: each method with its
    /// arguments as (direction, type) pairs in order, and each property with its type. Both lists
    /// are sorted by name, so the order of elements in a document doesn't matter.
    #[derive(Debug, PartialEq, Eq)]
    struct Surface {
        methods: Vec<(String, Vec<(String, String)>)>,
        properties: Vec<(String, String)>,
    }

    /// The opening tag at the start of `text`, up to and including its `>`.
    fn tag_at(text: &str) -> &str {
        let end = text.find('>').map_or(text.len(), |end| end + 1);
        &text[..end]
    }

    /// The value of attribute `name` in `tag`, matched as a whole attribute name.
    fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
        let key = format!(" {name}=\"");
        let start = tag.find(&key)? + key.len();
        let len = tag[start..].find('"')?;
        Some(&tag[start..start + len])
    }

    /// The [`Surface`] of [`INTERFACE`] in an introspection document. Only the interface's own
    /// element is read, so the Introspectable and Properties interfaces a served object carries are
    /// not mixed in.
    fn surface(xml: &str) -> Surface {
        let start = xml
            .find(&format!("<interface name=\"{INTERFACE}\""))
            .unwrap();
        let block = &xml[start..];
        let block = &block[..block.find("</interface>").unwrap()];
        let mut methods: Vec<(String, Vec<(String, String)>)> = Vec::new();
        for (start, _) in block.match_indices("<method ") {
            let text = &block[start..];
            let tag = tag_at(text);
            let body = if tag.ends_with("/>") {
                tag
            } else {
                &text[..text.find("</method>").unwrap()]
            };
            let args: Vec<(String, String)> = body
                .match_indices("<arg ")
                .map(|(start, _)| {
                    let arg = tag_at(&body[start..]);
                    // The D-Bus default direction is `in`.
                    let direction = attribute(arg, "direction").unwrap_or("in");
                    let kind = attribute(arg, "type").unwrap();
                    (direction.to_owned(), kind.to_owned())
                })
                .collect();
            methods.push((attribute(tag, "name").unwrap().to_owned(), args));
        }
        let mut properties: Vec<(String, String)> = Vec::new();
        for (start, _) in block.match_indices("<property ") {
            let tag = tag_at(&block[start..]);
            properties.push((
                attribute(tag, "name").unwrap().to_owned(),
                attribute(tag, "type").unwrap().to_owned(),
            ));
        }
        methods.sort();
        properties.sort();
        Surface {
            methods,
            properties,
        }
    }

    // ---- tests -----------------------------------------------------------------------------------

    #[test]
    fn connect_accepts_versions_1_and_2_and_refuses_others() {
        for version in [1, 2] {
            let Some(shell) = FakeShell::start(version) else {
                return;
            };
            let bridge = bridge_on(&shell);
            assert_eq!(bridge.version(), version);
            assert_eq!(bridge.epoch(), FAKE_EPOCH);
        }
        for version in [0, 3] {
            let Some(shell) = FakeShell::start(version) else {
                return;
            };
            let error = ShellBridge::connect_at(&shell.daemon.address).unwrap_err();
            assert!(
                matches!(
                    error,
                    PlatformError::Unsupported(message)
                        if message.contains("speaks another bridge version")
                ),
                "version {version}: {error:?}"
            );
        }
        // A bus with no extension on it: the name has no owner.
        let Some(daemon) = Daemon::start() else {
            return;
        };
        let error = ShellBridge::connect_at(&daemon.address).unwrap_err();
        assert!(
            matches!(
                error,
                PlatformError::Unsupported(message) if message.contains("is not running")
            ),
            "{error:?}"
        );
    }

    #[test]
    fn version_1_extension_refuses_every_v2_call_without_calling() {
        let Some(shell) = FakeShell::start(1) else {
            return;
        };
        let bridge = bridge_on(&shell);
        assert_eq!(bridge.version(), 1);
        const NEEDS_V2: &str =
            "the Crosspane Shell extension is version 1; log out and back in to load version 2";
        let refused = [
            unsupported(bridge.set_pointer_fence(0, 0, 10, 10)),
            unsupported(bridge.clear_pointer_fence()),
            unsupported(bridge.save_layout()),
            unsupported(bridge.restore_layout(1, &[5])),
            unsupported(bridge.inhibit_cursor(true)),
        ];
        for message in refused {
            assert_eq!(message, NEEDS_V2);
        }
        assert!(shell.calls().is_empty(), "a v2 call reached the bus");

        // The version 1 methods still work on the same bridge.
        assert!(bridge.list_windows().unwrap().is_empty());
        bridge.activate(1).unwrap();
        assert_eq!(shell.lines(), vec!["ListWindows", "Activate 1"]);
    }

    #[test]
    fn v2_calls_reach_the_extension_with_the_frozen_arguments() {
        let Some(shell) = FakeShell::start(2) else {
            return;
        };
        let bridge = bridge_on(&shell);
        assert_eq!(bridge.version(), 2);

        bridge.set_pointer_fence(10, -20, 300, 400).unwrap();
        bridge.clear_pointer_fence().unwrap();
        assert_eq!(bridge.save_layout().unwrap(), 1);
        assert_eq!(bridge.save_layout().unwrap(), 2);
        assert_eq!(bridge.restore_layout(1, &[5, 6]).unwrap(), 3);
        // Token 1 is used up: the extension rejects it with InvalidArgs, which is NotFound here.
        assert!(matches!(
            bridge.restore_layout(1, &[]),
            Err(PlatformError::NotFound)
        ));
        // A token the extension never issued.
        assert!(matches!(
            bridge.restore_layout(999, &[]),
            Err(PlatformError::NotFound)
        ));
        // Token 0 is refused without a call.
        assert!(matches!(
            bridge.restore_layout(0, &[]),
            Err(PlatformError::NotFound)
        ));
        bridge.inhibit_cursor(true).unwrap();
        bridge.inhibit_cursor(false).unwrap();

        assert_eq!(
            shell.lines(),
            vec![
                "SetPointerFence 10 -20 300 400",
                "ClearPointerFence",
                "SaveLayout",
                "SaveLayout",
                "RestoreLayout 1 [5, 6]",
                "RestoreLayout 1 []",
                "RestoreLayout 999 []",
                "InhibitCursor true",
                "InhibitCursor false",
            ]
        );
    }

    #[test]
    fn inhibit_cursor_uses_the_overlay_connection() {
        let Some(shell) = FakeShell::start(2) else {
            return;
        };
        let bridge = bridge_on(&shell);
        bridge.save_layout().unwrap();
        bridge.set_pointer_fence(0, 0, 1, 1).unwrap();
        bridge.inhibit_cursor(true).unwrap();

        let calls = shell.calls();
        let save = sender_of(&calls, "SaveLayout");
        let fence = sender_of(&calls, "SetPointerFence");
        let inhibit = sender_of(&calls, "InhibitCursor");
        assert!(save.starts_with(':'), "unique name: {save}");
        // The 2 s connection carries both calls; the 45 ms one carries only InhibitCursor.
        assert_eq!(save, fence);
        assert_ne!(inhibit, save);
    }

    #[test]
    fn client_side_bounds_are_checked_before_any_call() {
        let Some(shell) = FakeShell::start(2) else {
            return;
        };
        let bridge = bridge_on(&shell);
        let refused = [
            (0, 0, 0, 1),
            (0, 0, 1, 0),
            (0, 0, 32_769, 1),
            (0, 0, 1, -1),
            (COORD + 1, 0, 1, 1),
            (0, -COORD - 1, 1, 1),
        ];
        for (x, y, width, height) in refused {
            assert!(
                matches!(
                    bridge.set_pointer_fence(x, y, width, height),
                    Err(PlatformError::Backend(_))
                ),
                "fence {x} {y} {width} {height}"
            );
        }
        assert!(matches!(
            bridge.restore_layout(1, &[0; 4097]),
            Err(PlatformError::Backend(_))
        ));
        assert!(matches!(
            bridge.restore_layout(1, &[1u64 << 53]),
            Err(PlatformError::Backend(_))
        ));
        assert!(shell.calls().is_empty(), "a refused argument made a call");

        // The extremes are accepted and reach the extension.
        bridge.set_pointer_fence(-COORD, COORD, 32_768, 1).unwrap();
        let token = bridge.save_layout().unwrap();
        let ids = [(1u64 << 53) - 1; 4096];
        assert_eq!(bridge.restore_layout(token, &ids).unwrap(), 3);
        let lines = shell.lines();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "SetPointerFence -1048576 1048576 32768 1");
        assert_eq!(lines[1], "SaveLayout");
        assert!(lines[2].starts_with(&format!("RestoreLayout {token} [9007199254740991, ")));
    }

    #[test]
    fn fence_and_skip_bounds_are_exact() {
        assert_eq!(MAX_SKIP, 4096);
        assert_eq!(MAX_SIZE, 32_768);
        assert!(check_fence(-COORD, COORD, 1, 32_768).is_ok());
        assert!(check_fence(COORD + 1, 0, 1, 1).is_err());
        assert!(check_fence(0, -COORD - 1, 1, 1).is_err());
        assert!(check_fence(0, 0, 0, 1).is_err());
        assert!(check_fence(0, 0, 1, 0).is_err());
        assert!(check_fence(0, 0, 32_769, 1).is_err());

        let max_id = (1u64 << 53) - 1;
        assert!(check_skip(&[]).is_ok());
        assert!(check_skip(&[max_id; 4096]).is_ok());
        assert!(check_skip(&[0; 4097]).is_err());
        assert!(check_skip(&[1u64 << 53]).is_err());
    }

    #[test]
    fn fake_introspection_matches_the_frozen_xml() {
        let Some(shell) = FakeShell::start(2) else {
            return;
        };
        let client = Builder::address(shell.daemon.address.as_str())
            .unwrap()
            .build()
            .unwrap();
        let reply = client
            .call_method(
                Some(BUS_NAME),
                OBJECT_PATH,
                Some("org.freedesktop.DBus.Introspectable"),
                "Introspect",
                &(),
            )
            .unwrap();
        let served: String = reply.body().deserialize().unwrap();

        let frozen = surface(FROZEN_XML);
        // Twelve methods (v1 and v2) and the two properties.
        assert_eq!(frozen.methods.len(), 12);
        assert_eq!(frozen.properties.len(), 2);
        assert_eq!(surface(&served), frozen);
    }

    #[test]
    fn subscribe_follows_the_private_bus() {
        let Some(shell) = FakeShell::start(2) else {
            return;
        };
        let bridge = bridge_on(&shell);
        let (tx, rx) = mpsc::channel();
        let callback: ShellCallback = Arc::new(move |event| {
            let _ = tx.send(event);
        });
        bridge.subscribe(callback).unwrap();

        // A signal from the fake reaches the callback, so the signal connection is on this bus.
        shell
            .connection
            .emit_signal(
                None::<&str>,
                OBJECT_PATH,
                INTERFACE,
                "WindowsChanged",
                &(FAKE_EPOCH,),
            )
            .unwrap();
        assert_eq!(
            rx.recv_timeout(WAIT).unwrap(),
            ShellEvent::WindowsChanged { epoch: FAKE_EPOCH }
        );

        let start = Instant::now();
        drop(bridge);
        assert!(start.elapsed() < WAIT, "dropping the bridge hangs");
    }
}
