//! A client for Mutter's `org.gnome.Mutter.DisplayConfig` and a pure layout planner (WP-G2.4 B1).
//!
//! GNOME lays every new virtual monitor (the "twin" a projected window is parked on) out linearly
//! and drops the user's rotation and placement while it exists. The agent puts the user's layout
//! back with one `ApplyMonitorsConfig` right after the twin appears or changes mode.
//!
//! # What this module may call (owner ruling, 2026-10-10)
//!
//! Only `GetCurrentState`, `ApplyMonitorsConfig` with **method 1 (temporary)** and the
//! `MonitorsChanged` signal, all on `org.gnome.Mutter.DisplayConfig`. Method 1 is a constant of
//! this module and the only code that builds an `ApplyMonitorsConfig` body hard-codes it: method 2
//! (persistent, which would write `monitors.xml` and raise a "Keep changes?" dialog) and method 0
//! (verify) cannot be sent, and no other Mutter interface or method is reachable (the private
//! `Call` enum is the whole list).
//!
//! # Layout
//!
//! [`DisplayConfig`] is the blocking client (its own session-bus connection, 2 s call timeout; the
//! signal thread pattern follows `shell.rs`). [`physical`], [`plan`], [`pick_scale`] and
//! [`twin_rect`] are pure and live in `layout.rs`, with their derivation from Mutter's sources.
//!
//! # Behaviour a caller should know
//!
//! - **Replies are validated.** A reply that does not match the documented signature, or has a
//!   non-positive size, a non-finite scale, a transform above 7, a layout mode other than 1 or 2 or
//!   a repeated connector, is [`PlatformError::Backend`]; nothing is guessed.
//! - **Nothing but positions changes.** `ApplyMonitorsConfig` rebuilds each monitor's
//!   configuration from the call, so properties that [`MonitorState`] does not carry would silently
//!   revert (HDR `color-mode`, `rgb-range`, `underscanning`, and the physical layout mode). The
//!   client therefore remembers them from the last [`DisplayConfig::current_state`] and sends them
//!   back unchanged for the monitors it saw (and `layout-mode` only when that was the non-default
//!   physical mode). A twin, which was not in that state, gets none. Call `current_state` right
//!   before [`DisplayConfig::apply_temporary`], as the serial requires anyway.
//! - **Errors.** An error reply is `Backend("DisplayConfig: <error name>: <Mutter's message>")`,
//!   e.g. `AccessDenied: The requested configuration is based on stale information` for a serial
//!   that moved on and `InvalidArgs: Logical monitors not adjacent`. [`is_serial_race`] tells the
//!   first kind apart, for the caller's re-read and retry.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crosspane_platform::PlatformError;
use zbus::blocking::connection::Builder;
use zbus::blocking::{Connection, MessageIterator};
use zbus::zvariant::{OwnedValue, Value};
use zbus::{DBusError, MatchRule, Message};

mod layout;
#[cfg(test)]
mod tests;

pub use layout::{physical, pick_scale, plan, twin_rect};

/// Connectors of Mutter's virtual monitors start with this (`Meta-0`, `Meta-1`, ...).
pub const TWIN_PREFIX: &str = "Meta-";

const BUS_NAME: &str = "org.gnome.Mutter.DisplayConfig";
const OBJECT_PATH: &str = "/org/gnome/Mutter/DisplayConfig";
const INTERFACE: &str = "org.gnome.Mutter.DisplayConfig";
const MONITORS_CHANGED: &str = "MonitorsChanged";

const DBUS: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const NAME_HAS_NO_OWNER: &str = "org.freedesktop.DBus.Error.NameHasNoOwner";
const CALL_TIMEOUT: Duration = Duration::from_secs(2);
/// Bounds `connect` and the first `subscribe`, connection setup included (zbus's method timeout
/// does not cover authentication).
const SETUP_TIMEOUT: Duration = Duration::from_secs(2);
const NOT_RUNNING: &str = "the Mutter DisplayConfig service is not running";

/// `ApplyMonitorsConfig`'s `method` for a temporary configuration: applied, not saved, no
/// confirmation dialog. The only value this module sends. (0 verifies, 2 persists.)
const METHOD_TEMPORARY: u32 = 1;
/// `layout-mode` of the physical layout mode. Mutter's default, logical (1), needs no property.
const LAYOUT_MODE_PHYSICAL: u32 = 2;
/// The longest part of Mutter's message copied into an error.
const MAX_MESSAGE_CHARS: usize = 200;

/// One monitor as `GetCurrentState` reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct MonitorState {
    pub connector: String,
    pub vendor: String,
    pub product: String,
    pub serial: String,
    pub modes: Vec<ModeState>,
    pub is_builtin: bool,
}

/// One mode of a monitor.
#[derive(Clone, Debug, PartialEq)]
pub struct ModeState {
    /// Mutter's mode id (`1920x1080@60.000`), as `ApplyMonitorsConfig` wants it back.
    pub id: String,
    pub width: i32,
    pub height: i32,
    pub refresh: f64,
    pub preferred_scale: f64,
    pub supported_scales: Vec<f64>,
    /// `is-current`.
    pub current: bool,
    /// `is-preferred`.
    pub preferred: bool,
}

/// One logical monitor of the current layout.
#[derive(Clone, Debug, PartialEq)]
pub struct LogicalState {
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    pub transform: u32,
    pub primary: bool,
    pub connectors: Vec<String>,
}

/// The whole of `GetCurrentState` that the agent uses.
#[derive(Clone, Debug, PartialEq)]
pub struct DisplayState {
    /// The configuration serial `ApplyMonitorsConfig` must echo.
    pub serial: u32,
    pub monitors: Vec<MonitorState>,
    pub logical: Vec<LogicalState>,
    /// The `layout-mode` property: 1 logical, 2 physical; absent means logical.
    pub layout_mode: Option<u32>,
}

/// One logical monitor for `ApplyMonitorsConfig`:
/// `(x, y, scale, transform, primary, [(connector, mode id)])`.
#[derive(Clone, Debug, PartialEq)]
pub struct LogicalConfig {
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    pub transform: u32,
    pub primary: bool,
    pub monitors: Vec<(String, String)>,
}

// ---- the wire ----------------------------------------------------------------------------------

/// `(siiddada{sv})`: id, width, height, refresh, preferred scale, supported scales, properties.
type ModeWire = (
    String,
    i32,
    i32,
    f64,
    f64,
    Vec<f64>,
    HashMap<String, OwnedValue>,
);
/// `((ssss)a(siiddada{sv})a{sv})`: connector, vendor, product, serial; modes; properties.
type MonitorWire = (
    (String, String, String, String),
    Vec<ModeWire>,
    HashMap<String, OwnedValue>,
);
/// `(iiduba(ssss)a{sv})`: x, y, scale, transform, primary, monitors, properties.
type LogicalWire = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<(String, String, String, String)>,
    HashMap<String, OwnedValue>,
);
/// `GetCurrentState`'s four out arguments.
type StateWire = (
    u32,
    Vec<MonitorWire>,
    Vec<LogicalWire>,
    HashMap<String, OwnedValue>,
);

/// `(ssa{sv})`: connector, mode id, monitor properties.
type ApplyMonitor<'a> = (&'a str, &'a str, HashMap<&'static str, Value<'static>>);
/// `(iiduba(ssa{sv}))`.
type ApplyLogical<'a> = (i32, i32, f64, u32, bool, Vec<ApplyMonitor<'a>>);
/// `ApplyMonitorsConfig`'s in arguments: `uua(iiduba(ssa{sv}))a{sv}`.
type ApplyBody<'a> = (
    u32,
    u32,
    Vec<ApplyLogical<'a>>,
    HashMap<&'static str, Value<'static>>,
);

/// The calls this module can make. The whole list; nothing else is ever sent to Mutter.
#[derive(Clone, Copy, Debug)]
enum Call {
    GetCurrentState,
    ApplyMonitorsConfig,
}

impl Call {
    fn member(self) -> &'static str {
        match self {
            Call::GetCurrentState => "GetCurrentState",
            Call::ApplyMonitorsConfig => "ApplyMonitorsConfig",
        }
    }
}

// ---- what an apply must carry over -------------------------------------------------------------

/// Monitor properties that `GetCurrentState` reports and `ApplyMonitorsConfig` would reset to
/// their defaults if left out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MonitorExtras {
    /// `is-underscanning`, when the monitor supports it.
    underscanning: Option<bool>,
    /// `color-mode` (0 default, 1 BT.2100, 2 SDR native).
    color_mode: u32,
    /// `rgb-range` (0 unknown, 1 auto, 2 full, 3 limited).
    rgb_range: u32,
}

/// What the last `current_state` showed that `apply_temporary` has to repeat.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Carry {
    layout_mode: Option<u32>,
    extras: HashMap<String, MonitorExtras>,
}

// ---- the client --------------------------------------------------------------------------------

/// The signal connection and its thread, held by the client so that dropping it stops both.
#[derive(Debug)]
struct SignalHandle {
    connection: Connection,
    thread: JoinHandle<()>,
}

type Callback = Arc<dyn Fn() + Send + Sync>;

/// A blocking connection to `org.gnome.Mutter.DisplayConfig` on the session bus.
pub struct DisplayConfig {
    /// The bus address under test; `None` is the session bus.
    address: Option<String>,
    calls: Connection,
    carry: Mutex<Carry>,
    /// Every subscriber's callback, in registration order. Shared with the signal thread.
    callbacks: Arc<Mutex<Vec<Callback>>>,
    /// Set when the client is dropped, so the signal thread ends quietly.
    stopping: Arc<AtomicBool>,
    /// The signal connection and thread, built by the first subscribe (held while building).
    signals: Mutex<Option<SignalHandle>>,
}

impl fmt::Debug for DisplayConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DisplayConfig").finish_non_exhaustive()
    }
}

impl Drop for DisplayConfig {
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
            // A callback may hold the last reference to the client, so this can run on the signal
            // thread itself, which must not join itself.
            if thread.thread().id() != thread::current().id() {
                let _ = thread.join();
            }
        }
    }
}

impl DisplayConfig {
    /// Connect to the session bus (bounded: 2 s). `Unsupported` when nothing owns
    /// `org.gnome.Mutter.DisplayConfig` (not GNOME, or no Shell yet).
    pub fn connect() -> Result<DisplayConfig, PlatformError> {
        Self::open(None)
    }

    /// `connect` on the given bus address (`None`: the session bus). Tests use a private bus.
    fn open(address: Option<String>) -> Result<DisplayConfig, PlatformError> {
        let target = address.clone();
        let calls = bounded(move || {
            let calls = open_connection(target.as_deref(), CALL_TIMEOUT)?;
            if has_owner(&calls)? {
                Ok(calls)
            } else {
                Err(PlatformError::Unsupported(NOT_RUNNING))
            }
        })?;
        Ok(DisplayConfig {
            address,
            calls,
            carry: Mutex::new(Carry::default()),
            callbacks: Arc::new(Mutex::new(Vec::new())),
            stopping: Arc::new(AtomicBool::new(false)),
            signals: Mutex::new(None),
        })
    }

    /// `GetCurrentState`. A reply that does not fit the documented shape is `Backend`.
    pub fn current_state(&self) -> Result<DisplayState, PlatformError> {
        let reply = self.call(Call::GetCurrentState, &())?;
        let wire: StateWire = reply.body().deserialize().map_err(|_| malformed("shape"))?;
        let (state, carry) = parse_state(wire)?;
        *lock(&self.carry) = carry;
        Ok(state)
    }

    /// `ApplyMonitorsConfig(serial, method = 1 TEMPORARY, logical, {})`: the only method value this
    /// module can send (owner ruling). Errors map to `Backend` with Mutter's message
    /// (see the module docs); a call that outlives 2 s is `Timeout`.
    ///
    /// The properties `GetCurrentState` last showed for each monitor, and the physical layout mode,
    /// are repeated so that nothing but positions changes (see the module docs). The only property
    /// map this sends is empty, or `layout-mode = 2` after a physical-mode state.
    pub fn apply_temporary(
        &self,
        serial: u32,
        logical: &[LogicalConfig],
    ) -> Result<(), PlatformError> {
        check_config(logical)?;
        let carry = lock(&self.carry).clone();
        let body = apply_body(serial, logical, &carry);
        self.call(Call::ApplyMonitorsConfig, &body)?;
        Ok(())
    }

    /// `MonitorsChanged`, delivered on a signal thread of this client in order (never from inside
    /// `subscribe`). The callback must not block. The thread and its connection end when the
    /// client is dropped.
    pub fn subscribe(&self, callback: Arc<dyn Fn() + Send + Sync>) -> Result<(), PlatformError> {
        // Held for the whole call: one subscriber at a time builds the signal machinery.
        let mut signals = lock(&self.signals);
        // Registered before the signal thread can run, so the first subscriber misses nothing.
        lock(&self.callbacks).push(callback);
        if signals.is_some() {
            return Ok(());
        }
        match self.start_signals() {
            Ok(handle) => {
                *signals = Some(handle);
                Ok(())
            }
            Err(error) => {
                // Nobody else can push while `signals` is held, so the last entry is ours.
                lock(&self.callbacks).pop();
                Err(error)
            }
        }
    }

    /// Builds the signal connection and starts the signal thread.
    fn start_signals(&self) -> Result<SignalHandle, PlatformError> {
        let address = self.address.clone();
        let (connection, messages) = bounded(move || open_signals(address.as_deref()))?;
        let (callbacks, stopping) = (self.callbacks.clone(), self.stopping.clone());
        let thread = thread::Builder::new()
            .name("crosspane-displayconfig-signals".into())
            .spawn(move || run_signals(messages, &callbacks, &stopping))
            .map_err(|e| backend(format!("signal thread: {e}")))?;
        Ok(SignalHandle { connection, thread })
    }

    /// One method call on Mutter's interface. Error replies are mapped by [`dbus_error`].
    fn call<B>(&self, call: Call, body: &B) -> Result<Message, PlatformError>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        self.calls
            .call_method(
                Some(BUS_NAME),
                OBJECT_PATH,
                Some(INTERFACE),
                call.member(),
                body,
            )
            .map_err(dbus_error)
    }
}

/// Whether `error` is Mutter refusing an `ApplyMonitorsConfig` because the configuration moved on
/// since `GetCurrentState` (the serial is stale): the caller re-reads and tries again.
pub fn is_serial_race(error: &PlatformError) -> bool {
    let PlatformError::Backend(message) = error else {
        return false;
    };
    let named = |name: &str| message.starts_with(&format!("DisplayConfig: {name}: "));
    (named("org.freedesktop.DBus.Error.AccessDenied")
        || named("org.freedesktop.DBus.Error.InvalidArgs"))
        && (message.contains("stale information") || message.contains("serial"))
}

// ---- parsing and encoding ----------------------------------------------------------------------

/// Turns `GetCurrentState`'s reply into the state and what an apply must carry over.
fn parse_state(wire: StateWire) -> Result<(DisplayState, Carry), PlatformError> {
    let (serial, monitors, logical, properties) = wire;
    let mut carry = Carry::default();
    let mut parsed_monitors = Vec::with_capacity(monitors.len());
    for ((connector, vendor, product, monitor_serial), modes, props) in monitors {
        if connector.is_empty() || carry.extras.contains_key(&connector) {
            return Err(malformed("monitor connector"));
        }
        let extras = MonitorExtras {
            underscanning: optional_bool(&props, "is-underscanning")?,
            color_mode: optional_u32(&props, "color-mode")?.unwrap_or(0),
            rgb_range: optional_u32(&props, "rgb-range")?.unwrap_or(0),
        };
        carry.extras.insert(connector.clone(), extras);
        parsed_monitors.push(MonitorState {
            is_builtin: optional_bool(&props, "is-builtin")?.unwrap_or(false),
            modes: modes
                .into_iter()
                .map(parse_mode)
                .collect::<Result<Vec<_>, _>>()?,
            connector,
            vendor,
            product,
            serial: monitor_serial,
        });
    }
    let parsed_logical = logical
        .into_iter()
        .map(parse_logical)
        .collect::<Result<Vec<_>, _>>()?;
    let layout_mode = optional_u32(&properties, "layout-mode")?;
    if layout_mode.is_some_and(|mode| !(1..=2).contains(&mode)) {
        return Err(malformed("layout mode"));
    }
    carry.layout_mode = layout_mode;
    Ok((
        DisplayState {
            serial,
            monitors: parsed_monitors,
            logical: parsed_logical,
            layout_mode,
        },
        carry,
    ))
}

fn parse_mode(wire: ModeWire) -> Result<ModeState, PlatformError> {
    let (id, width, height, refresh, preferred_scale, supported_scales, props) = wire;
    let sane = |scale: f64| scale.is_finite() && scale > 0.0;
    if width <= 0
        || height <= 0
        || !(refresh.is_finite() && refresh >= 0.0)
        || !(preferred_scale.is_finite() && preferred_scale >= 0.0)
        || !supported_scales.iter().copied().all(sane)
    {
        return Err(malformed("mode"));
    }
    Ok(ModeState {
        id,
        width,
        height,
        refresh,
        preferred_scale,
        supported_scales,
        current: optional_bool(&props, "is-current")?.unwrap_or(false),
        preferred: optional_bool(&props, "is-preferred")?.unwrap_or(false),
    })
}

fn parse_logical(wire: LogicalWire) -> Result<LogicalState, PlatformError> {
    let (x, y, scale, transform, primary, monitors, _properties) = wire;
    if !(scale.is_finite() && scale > 0.0) || transform > 7 || monitors.is_empty() {
        return Err(malformed("logical monitor"));
    }
    Ok(LogicalState {
        x,
        y,
        scale,
        transform,
        primary,
        connectors: monitors
            .into_iter()
            .map(|(connector, _vendor, _product, _serial)| connector)
            .collect(),
    })
}

/// A boolean property that may be absent but, when present, must be a boolean.
fn optional_bool(
    props: &HashMap<String, OwnedValue>,
    key: &str,
) -> Result<Option<bool>, PlatformError> {
    props.get(key).map_or(Ok(None), |value| {
        value
            .downcast_ref::<bool>()
            .map(Some)
            .map_err(|_| malformed("property type"))
    })
}

/// A `u32` property that may be absent but, when present, must be a `u32`.
fn optional_u32(
    props: &HashMap<String, OwnedValue>,
    key: &str,
) -> Result<Option<u32>, PlatformError> {
    props.get(key).map_or(Ok(None), |value| {
        value
            .downcast_ref::<u32>()
            .map(Some)
            .map_err(|_| malformed("property type"))
    })
}

/// What `apply_temporary` refuses to send at all: nothing, a logical monitor without monitors, a
/// scale Mutter cannot use or a transform above 7. (Mutter would refuse them too, with a worse
/// message, after the layout had already been disturbed by whatever called us.)
fn check_config(logical: &[LogicalConfig]) -> Result<(), PlatformError> {
    let sane = !logical.is_empty()
        && logical.iter().all(|config| {
            config.scale.is_finite()
                && config.scale > 0.0
                && config.transform <= 7
                && !config.monitors.is_empty()
        });
    if sane {
        Ok(())
    } else {
        Err(backend("refusing to apply an invalid layout"))
    }
}

/// The in-arguments of the one `ApplyMonitorsConfig` this module sends. `method` is
/// [`METHOD_TEMPORARY`], not a parameter.
fn apply_body<'a>(serial: u32, logical: &'a [LogicalConfig], carry: &Carry) -> ApplyBody<'a> {
    let logical = logical
        .iter()
        .map(|config| {
            let monitors: Vec<ApplyMonitor<'_>> = config
                .monitors
                .iter()
                .map(|(connector, mode)| {
                    (
                        connector.as_str(),
                        mode.as_str(),
                        monitor_properties(carry.extras.get(connector)),
                    )
                })
                .collect();
            (
                config.x,
                config.y,
                config.scale,
                config.transform,
                config.primary,
                monitors,
            )
        })
        .collect();
    let mut properties = HashMap::new();
    if carry.layout_mode == Some(LAYOUT_MODE_PHYSICAL) {
        properties.insert("layout-mode", Value::U32(LAYOUT_MODE_PHYSICAL));
    }
    (serial, METHOD_TEMPORARY, logical, properties)
}

/// The per-monitor properties of an apply: only what differs from Mutter's defaults.
fn monitor_properties(extras: Option<&MonitorExtras>) -> HashMap<&'static str, Value<'static>> {
    let mut properties = HashMap::new();
    let Some(extras) = extras else {
        return properties;
    };
    if extras.underscanning == Some(true) {
        properties.insert("underscanning", Value::Bool(true));
    }
    if extras.color_mode != 0 {
        properties.insert("color-mode", Value::U32(extras.color_mode));
    }
    if extras.rgb_range != 0 {
        properties.insert("rgb-range", Value::U32(extras.rgb_range));
    }
    properties
}

// ---- signals -----------------------------------------------------------------------------------

/// The signal connection with its match rule installed. The iterator exists before `AddMatch`, so
/// no signal sent once the rule is installed is lost. The rule names the well-known name, which
/// the bus resolves to its current owner for every message: a restarted Shell keeps working.
fn open_signals(address: Option<&str>) -> Result<(Connection, MessageIterator), PlatformError> {
    let connection = open_connection(address, CALL_TIMEOUT)?;
    let messages = MessageIterator::from(&connection);
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(BUS_NAME)
        .map_err(dbus_error)?
        .path(OBJECT_PATH)
        .map_err(dbus_error)?
        .interface(INTERFACE)
        .map_err(dbus_error)?
        .member(MONITORS_CHANGED)
        .map_err(dbus_error)?
        .build();
    connection
        .call_method(
            Some(DBUS),
            DBUS_PATH,
            Some(DBUS),
            "AddMatch",
            &(rule.to_string(),),
        )
        .map_err(dbus_error)?;
    Ok((connection, messages))
}

/// The signal thread: calls every callback for each `MonitorsChanged`, in order, until the
/// connection closes. It holds no reference to the client.
fn run_signals(messages: MessageIterator, callbacks: &Mutex<Vec<Callback>>, stopping: &AtomicBool) {
    for message in messages {
        if stopping.load(Ordering::SeqCst) {
            return;
        }
        let Ok(message) = message else {
            tracing::debug!("DisplayConfig signal stream ended");
            return;
        };
        if is_monitors_changed(&message) {
            tracing::debug!("DisplayConfig MonitorsChanged");
            // Copied out first, so a callback may subscribe again without deadlocking.
            let snapshot = lock(callbacks).clone();
            for callback in &snapshot {
                callback();
            }
        }
    }
}

/// Whether `message` is Mutter's `MonitorsChanged` on its object and interface.
fn is_monitors_changed(message: &Message) -> bool {
    let header = message.header();
    header.message_type() == zbus::message::Type::Signal
        && header.path().map(|v| v.as_str()) == Some(OBJECT_PATH)
        && header.interface().map(|v| v.as_str()) == Some(INTERFACE)
        && header.member().map(|v| v.as_str()) == Some(MONITORS_CHANGED)
}

// ---- plumbing ----------------------------------------------------------------------------------

/// Locks `mutex`, recovering the data from a poisoned lock: everything behind these locks is
/// always whole.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
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
        .name("crosspane-displayconfig-setup".into())
        .spawn(move || {
            let _ = tx.send(operation());
        })
        .map_err(|e| backend(format!("setup thread: {e}")))?;
    match rx.recv_timeout(SETUP_TIMEOUT) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(PlatformError::Timeout),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(backend("setup thread stopped")),
    }
}

fn open_connection(address: Option<&str>, timeout: Duration) -> Result<Connection, PlatformError> {
    let builder = match address {
        Some(address) => Builder::address(address),
        None => Builder::session(),
    }
    .map_err(session_bus)?;
    builder.method_timeout(timeout).build().map_err(session_bus)
}

/// Whether anything owns [`BUS_NAME`].
fn has_owner(connection: &Connection) -> Result<bool, PlatformError> {
    let reply = connection
        .call_method(
            Some(DBUS),
            DBUS_PATH,
            Some(DBUS),
            "NameHasOwner",
            &(BUS_NAME,),
        )
        .map_err(dbus_error)?;
    reply.body().deserialize().map_err(|_| malformed("reply"))
}

fn session_bus(error: zbus::Error) -> PlatformError {
    backend(format!("session bus: {error}"))
}

fn backend(detail: impl fmt::Display) -> PlatformError {
    PlatformError::Backend(format!("DisplayConfig: {detail}"))
}

fn malformed(what: &str) -> PlatformError {
    backend(format!("malformed reply ({what})"))
}

/// The D-Bus error name and message of an error reply, whichever way zbus reports it.
fn reply_error(error: &zbus::Error) -> Option<(String, Option<String>)> {
    match error {
        zbus::Error::MethodError(name, detail, _) => {
            Some((name.as_str().to_owned(), detail.clone()))
        }
        zbus::Error::FDO(error) => Some((
            error.name().as_str().to_owned(),
            error.description().map(str::to_owned),
        )),
        _ => None,
    }
}

/// Maps a D-Bus failure to a [`PlatformError`]. An error reply keeps its name and (cut short)
/// message, which for Mutter says why a layout was refused and never holds user content.
fn dbus_error(error: zbus::Error) -> PlatformError {
    if let Some((name, detail)) = reply_error(&error) {
        return match name.as_str() {
            "org.freedesktop.DBus.Error.Timeout"
            | "org.freedesktop.DBus.Error.TimedOut"
            | "org.freedesktop.DBus.Error.NoReply" => PlatformError::Timeout,
            "org.freedesktop.DBus.Error.ServiceUnknown" | NAME_HAS_NO_OWNER => {
                backend("Mutter is not running")
            }
            _ => match detail {
                Some(detail) => backend(format!(
                    "{name}: {}",
                    detail.chars().take(MAX_MESSAGE_CHARS).collect::<String>()
                )),
                None => backend(name),
            },
        };
    }
    match &error {
        zbus::Error::InputOutput(e) | zbus::Error::Connection(e, _)
            if e.kind() == std::io::ErrorKind::TimedOut =>
        {
            PlatformError::Timeout
        }
        _ => backend(error),
    }
}
