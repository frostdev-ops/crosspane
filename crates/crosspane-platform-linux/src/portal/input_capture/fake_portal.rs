//! Test-only: the whole backend against a fake InputCapture portal on a private `dbus-daemon`.
//!
//! The fake speaks the portal's real protocol (Request objects and `Response` signals, Session
//! objects, `ConnectToEIS` returning a socket served by [`fake_eis`], the `Activated`,
//! `Deactivated`, `ZonesChanged` and `Disabled` signals) and enforces the state rules mutter
//! enforces and the live probe of 2026-10-10 found: `Enable` before `ConnectToEIS` fails ("Not
//! connected to EIS"), barriers change only while disabled, `Disable` of a disabled session and
//! `Release` of an inactive one fail, activation starts the devices' emulation with the activation
//! id as the sequence, and a release stops it again and emits `Deactivated`. A KDE flavour makes
//! `SetPointerBarriers` disable the session itself and `Disable` re-enable it, as xdp-kde does.
//!
//! Every backend here runs on the daemon's own address (`spawn_on`), never the session bus, and
//! the daemon's config has no service directories, so nothing can be D-Bus-activated. Tests skip
//! with a printed reason when there is no `dbus-daemon` binary.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use crosspane_platform::{EndReason, MotionKind};
use crosspane_types::color::ColorSpace;
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{DisplayGeometry, PixelSize, PointLogical, SizeMm};
use crosspane_types::hid::HidUsage;
use zbus::message::Header;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, ObjectServer, fdo, interface};

use super::fake_eis::{self, Cmd, Target};
use super::*;

const PORTAL_NAME: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const WAIT: Duration = Duration::from_secs(5);

// ---- the private bus -------------------------------------------------------------------------

/// A `dbus-daemon` child with a private socket, killed on drop.
struct Daemon {
    child: Child,
    dir: PathBuf,
    address: String,
}

impl Daemon {
    fn start() -> Option<Daemon> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "crosspane-fake-capture-portal-{}-{}",
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
        while !socket.exists() {
            assert!(Instant::now() < deadline, "dbus-daemon did not come up");
            if let Ok(Some(status)) = daemon.child.try_wait() {
                panic!("dbus-daemon exited early: {status}");
            }
            thread::sleep(Duration::from_millis(5));
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

// ---- the fake portal -------------------------------------------------------------------------

/// What `CreateSession` (or `Start`) does next.
#[derive(Clone, Copy, Debug)]
enum Create {
    Grant,
    /// Respond with this non-success code (1 cancelled, 2 other).
    Respond(u32),
}

/// A portal call the fake saw.
#[derive(Clone, Debug, PartialEq)]
enum Call {
    CreateSession,
    CreateSession2,
    Start {
        persist_mode: Option<u32>,
        restore_token: Option<String>,
    },
    GetZones,
    SetBarriers {
        barriers: Vec<(u32, (i32, i32, i32, i32))>,
        zone_set: u32,
    },
    ConnectToEis,
    Enable,
    Disable,
    Release {
        activation: Option<u32>,
        cursor: Option<(f64, f64)>,
    },
    Close,
}

struct Inner {
    version: u32,
    /// xdp-kde: `SetPointerBarriers` disables, `Disable` enables.
    kde: bool,
    create: VecDeque<Create>,
    /// (width, height, x, y), as `GetZones` answers.
    zones: Vec<(u32, u32, i32, i32)>,
    zone_set: u32,
    calls: Vec<(Instant, Call)>,
    sessions: Vec<String>,
    connected: bool,
    enabled: bool,
    activated: Option<u32>,
    activation_counter: u32,
    eis: Option<fake_eis::Fake>,
    tokens: u32,
    /// `Release` answers this late (to see what the backend does when the portal is slow).
    release_delay: Option<Duration>,
}

struct Fake {
    inner: Mutex<Inner>,
}

impl Fake {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap()
    }

    fn record(&self, call: Call) {
        self.lock().calls.push((Instant::now(), call));
    }
}

fn sender_segment(header: &Header<'_>) -> String {
    header
        .sender()
        .map(|name| name.as_str().trim_start_matches(':').replace('.', "_"))
        .unwrap_or_default()
}

fn option_str(options: &HashMap<String, OwnedValue>, key: &str) -> fdo::Result<String> {
    options
        .get(key)
        .and_then(|value| value.downcast_ref::<&str>().ok())
        .map(str::to_owned)
        .ok_or_else(|| fdo::Error::InvalidArgs(format!("missing option {key}")))
}

fn option_u32(options: &HashMap<String, OwnedValue>, key: &str) -> Option<u32> {
    options
        .get(key)
        .and_then(|value| value.downcast_ref::<u32>().ok())
}

fn object_path(path: String) -> fdo::Result<OwnedObjectPath> {
    OwnedObjectPath::try_from(path).map_err(|error| fdo::Error::Failed(error.to_string()))
}

/// The InputCapture portal answers `session_handle` as an object path.
fn session_handle(path: &str) -> fdo::Result<Value<'static>> {
    ObjectPath::try_from(path.to_owned())
        .map(Value::from)
        .map_err(|error| fdo::Error::Failed(error.to_string()))
}

fn request_path(header: &Header<'_>, options: &HashMap<String, OwnedValue>) -> fdo::Result<String> {
    Ok(format!(
        "/org/freedesktop/portal/desktop/request/{}/{}",
        sender_segment(header),
        option_str(options, "handle_token")?
    ))
}

async fn respond(
    connection: &Connection,
    request: &str,
    code: u32,
    results: HashMap<String, Value<'_>>,
) -> fdo::Result<()> {
    connection
        .emit_signal(
            None::<&str>,
            request,
            "org.freedesktop.portal.Request",
            "Response",
            &(code, results),
        )
        .await?;
    Ok(())
}

struct FakeInputCapture(Arc<Fake>);

#[interface(name = "org.freedesktop.portal.InputCapture")]
impl FakeInputCapture {
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        self.0.lock().version
    }

    #[zbus(property, name = "SupportedCapabilities")]
    fn supported_capabilities(&self) -> u32 {
        3
    }

    /// Version 1: the dialog is the response.
    async fn create_session(
        &self,
        _parent_window: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> fdo::Result<OwnedObjectPath> {
        self.0.record(Call::CreateSession);
        let request = request_path(&header, &options)?;
        let session = format!(
            "/org/freedesktop/portal/desktop/session/{}/{}",
            sender_segment(&header),
            option_str(&options, "session_handle_token")?
        );
        let script = self.0.lock().create.pop_front().unwrap_or(Create::Grant);
        match script {
            Create::Grant => {
                new_session(&self.0, server, &session).await?;
                let mut results = HashMap::new();
                results.insert("session_handle".to_owned(), session_handle(&session)?);
                results.insert("capabilities".to_owned(), Value::U32(3));
                respond(connection, &request, 0, results).await?;
            }
            Create::Respond(code) => {
                respond(connection, &request, code, HashMap::new()).await?;
            }
        }
        object_path(request)
    }

    /// Version 2: the session first, then `Start` (the dialog).
    #[zbus(name = "CreateSession2")]
    async fn create_session2(
        &self,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> fdo::Result<HashMap<String, OwnedValue>> {
        self.0.record(Call::CreateSession2);
        let session = format!(
            "/org/freedesktop/portal/desktop/session/{}/{}",
            sender_segment(&header),
            option_str(&options, "session_handle_token")?
        );
        new_session(&self.0, server, &session).await?;
        let mut results = HashMap::new();
        results.insert(
            "session_handle".to_owned(),
            session_handle(&session)?
                .try_into()
                .map_err(|e: zbus::zvariant::Error| fdo::Error::Failed(e.to_string()))?,
        );
        Ok(results)
    }

    async fn start(
        &self,
        _session: ObjectPath<'_>,
        _parent_window: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<OwnedObjectPath> {
        self.0.record(Call::Start {
            persist_mode: option_u32(&options, "persist_mode"),
            restore_token: option_str(&options, "restore_token").ok(),
        });
        let request = request_path(&header, &options)?;
        let (script, token) = {
            let mut inner = self.0.lock();
            inner.tokens += 1;
            (
                inner.create.pop_front().unwrap_or(Create::Grant),
                format!("tok-{}", inner.tokens),
            )
        };
        match script {
            Create::Grant => {
                let mut results = HashMap::new();
                results.insert("capabilities".to_owned(), Value::U32(3));
                results.insert("restore_token".to_owned(), Value::from(token));
                respond(connection, &request, 0, results).await?;
            }
            Create::Respond(code) => respond(connection, &request, code, HashMap::new()).await?,
        }
        object_path(request)
    }

    async fn get_zones(
        &self,
        _session: ObjectPath<'_>,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<OwnedObjectPath> {
        self.0.record(Call::GetZones);
        let request = request_path(&header, &options)?;
        let (zones, zone_set) = {
            let inner = self.0.lock();
            (inner.zones.clone(), inner.zone_set)
        };
        let mut results = HashMap::new();
        results.insert("zones".to_owned(), Value::from(zones));
        results.insert("zone_set".to_owned(), Value::U32(zone_set));
        respond(connection, &request, 0, results).await?;
        object_path(request)
    }

    async fn set_pointer_barriers(
        &self,
        _session: ObjectPath<'_>,
        options: HashMap<String, OwnedValue>,
        barriers: Vec<HashMap<String, OwnedValue>>,
        zone_set: u32,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<OwnedObjectPath> {
        let request = request_path(&header, &options)?;
        let mut parsed = Vec::new();
        for barrier in &barriers {
            let id = option_u32(barrier, "barrier_id")
                .ok_or_else(|| fdo::Error::InvalidArgs("barrier_id".into()))?;
            let position = barrier
                .get("position")
                .and_then(|value| value.try_clone().ok())
                .and_then(|value| <(i32, i32, i32, i32)>::try_from(value).ok())
                .ok_or_else(|| fdo::Error::InvalidArgs("position".into()))?;
            parsed.push((id, position));
        }
        self.0.record(Call::SetBarriers {
            barriers: parsed,
            zone_set,
        });
        {
            let mut inner = self.0.lock();
            if inner.enabled {
                if inner.kde {
                    inner.enabled = false;
                } else {
                    return Err(fdo::Error::AccessDenied("Session already enabled".into()));
                }
            }
        }
        let mut results = HashMap::new();
        results.insert("failed_barriers".to_owned(), Value::from(Vec::<u32>::new()));
        respond(connection, &request, 0, results).await?;
        object_path(request)
    }

    async fn enable(
        &self,
        _session: ObjectPath<'_>,
        _options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<()> {
        self.0.record(Call::Enable);
        let mut inner = self.0.lock();
        if !inner.connected {
            return Err(fdo::Error::Failed("Not connected to EIS".into()));
        }
        inner.enabled = true;
        Ok(())
    }

    async fn disable(
        &self,
        _session: ObjectPath<'_>,
        _options: HashMap<String, OwnedValue>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        self.0.record(Call::Disable);
        let deactivated = {
            let mut inner = self.0.lock();
            if inner.kde {
                // xdp-kde's bug.
                inner.enabled = true;
                return Ok(());
            }
            if !inner.enabled {
                return Err(fdo::Error::Failed("Session not enabled".into()));
            }
            inner.enabled = false;
            // Mutter deactivates an activated session first: devices stop, `Deactivated` follows.
            let id = inner.activated.take();
            if id.is_some()
                && let Some(eis) = &inner.eis
            {
                eis.send(Cmd::Stop(Target::Pointer));
                eis.send(Cmd::Stop(Target::Keyboard));
            }
            id
        };
        if let Some(id) = deactivated {
            emit_session_signal(
                connection,
                "Deactivated",
                activation_options(id, None, None),
            )
            .await;
        }
        Ok(())
    }

    async fn release(
        &self,
        _session: ObjectPath<'_>,
        options: HashMap<String, OwnedValue>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        let cursor = options
            .get("cursor_position")
            .and_then(|value| value.try_clone().ok())
            .and_then(|value| <(f64, f64)>::try_from(value).ok());
        self.0.record(Call::Release {
            activation: option_u32(&options, "activation_id"),
            cursor,
        });
        let delay = self.0.lock().release_delay;
        if let Some(delay) = delay {
            super::sleep::Sleep::new(delay).await;
        }
        let id = {
            let mut inner = self.0.lock();
            let Some(id) = inner.activated.take() else {
                return Err(fdo::Error::AccessDenied("Capture not active".into()));
            };
            if let Some(eis) = &inner.eis {
                eis.send(Cmd::Stop(Target::Pointer));
                eis.send(Cmd::Stop(Target::Keyboard));
            }
            id
        };
        emit_session_signal(
            connection,
            "Deactivated",
            activation_options(id, None, None),
        )
        .await;
        Ok(())
    }

    #[zbus(name = "ConnectToEIS")]
    async fn connect_to_eis(
        &self,
        _session: ObjectPath<'_>,
        _options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<zbus::zvariant::OwnedFd> {
        self.0.record(Call::ConnectToEis);
        let mut inner = self.0.lock();
        if inner.connected {
            return Err(fdo::Error::Failed("Already connected to EIS".into()));
        }
        let (eis, fd) = fake_eis::Fake::start(true);
        inner.eis = Some(eis);
        inner.connected = true;
        Ok(fd.into())
    }
}

async fn new_session(fake: &Arc<Fake>, server: &ObjectServer, path: &str) -> fdo::Result<()> {
    server.at(path, FakeSession(fake.clone())).await?;
    let mut inner = fake.lock();
    inner.sessions.push(path.to_owned());
    // A new session: not connected, not enabled, nothing activated.
    inner.connected = false;
    inner.enabled = false;
    inner.activated = None;
    inner.eis = None;
    Ok(())
}

struct FakeSession(Arc<Fake>);

#[interface(name = "org.freedesktop.portal.Session")]
impl FakeSession {
    async fn close(&self) -> fdo::Result<()> {
        self.0.record(Call::Close);
        let mut inner = self.0.lock();
        inner.enabled = false;
        inner.activated = None;
        inner.connected = false;
        inner.eis = None;
        Ok(())
    }
}

/// The options of an `Activated` or `Deactivated` signal.
fn activation_options(
    id: u32,
    cursor: Option<(f64, f64)>,
    barrier: Option<u32>,
) -> HashMap<String, Value<'static>> {
    let mut options = HashMap::new();
    options.insert("activation_id".to_owned(), Value::U32(id));
    if let Some(cursor) = cursor {
        options.insert("cursor_position".to_owned(), Value::from(cursor));
    }
    if let Some(barrier) = barrier {
        options.insert("barrier_id".to_owned(), Value::U32(barrier));
    }
    options
}

/// Emit a signal of the InputCapture interface; the session handle is the first argument. The
/// backend watches one session, so the handle is not checked.
async fn emit_session_signal(
    connection: &Connection,
    name: &str,
    options: HashMap<String, Value<'_>>,
) {
    let session = ObjectPath::try_from("/org/freedesktop/portal/desktop/session/x/y").unwrap();
    connection
        .emit_signal(
            None::<&str>,
            PORTAL_PATH,
            "org.freedesktop.portal.InputCapture",
            name,
            &(session, options),
        )
        .await
        .unwrap();
}

/// The fake portal, its bus and its connection.
struct Portal {
    daemon: Daemon,
    connection: Connection,
    fake: Arc<Fake>,
}

impl Portal {
    fn start(version: u32, kde: bool, zones: Vec<(u32, u32, i32, i32)>) -> Option<Portal> {
        let daemon = Daemon::start()?;
        let fake = Arc::new(Fake {
            inner: Mutex::new(Inner {
                version,
                kde,
                create: VecDeque::new(),
                zones,
                zone_set: 0,
                calls: Vec::new(),
                sessions: Vec::new(),
                connected: false,
                enabled: false,
                activated: None,
                activation_counter: 0,
                eis: None,
                tokens: 0,
                release_delay: None,
            }),
        });
        let connection = zbus::block_on(async {
            zbus::connection::Builder::address(daemon.address.as_str())?
                .serve_at(PORTAL_PATH, FakeInputCapture(fake.clone()))?
                .name(PORTAL_NAME)?
                .build()
                .await
        })
        .unwrap();
        Some(Portal {
            daemon,
            connection,
            fake,
        })
    }

    fn script(&self, create: Create) {
        self.fake.lock().create.push_back(create);
    }

    fn calls(&self) -> Vec<Call> {
        self.fake
            .lock()
            .calls
            .iter()
            .map(|(_, call)| call.clone())
            .collect()
    }

    fn timed_calls(&self) -> Vec<(Instant, Call)> {
        self.fake.lock().calls.clone()
    }

    fn wait_calls(&self, what: &str, done: impl Fn(&[Call]) -> bool) -> Vec<Call> {
        let deadline = Instant::now() + WAIT;
        loop {
            let calls = self.calls();
            if done(&calls) {
                return calls;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {calls:?}"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn eis(&self, cmd: Cmd) {
        let inner = self.fake.lock();
        inner.eis.as_ref().expect("no EIS connection").send(cmd);
    }

    /// The compositor activates a barrier: the signal, then both devices start emulating with the
    /// activation id as the sequence (the order mutter uses). Returns the activation id.
    fn activate(&self, barrier: u32, cursor: (f64, f64)) -> u32 {
        let id = {
            let mut inner = self.fake.lock();
            assert!(inner.enabled, "an activation needs an enabled session");
            inner.activation_counter += 1;
            inner.activated = Some(inner.activation_counter);
            inner.activation_counter
        };
        zbus::block_on(emit_session_signal(
            &self.connection,
            "Activated",
            activation_options(id, Some(cursor), Some(barrier)),
        ));
        self.eis(Cmd::Start(Target::Pointer, id));
        self.eis(Cmd::Start(Target::Keyboard, id));
        id
    }

    /// The compositor activates a barrier with a signal body this backend cannot read.
    fn activate_unreadable(&self) {
        {
            let mut inner = self.fake.lock();
            assert!(inner.enabled, "an activation needs an enabled session");
            inner.activation_counter += 1;
            inner.activated = Some(inner.activation_counter);
        }
        let mut options = HashMap::new();
        options.insert("activation_id".to_owned(), Value::from("not a number"));
        options.insert("barrier_id".to_owned(), Value::from("x"));
        zbus::block_on(emit_session_signal(&self.connection, "Activated", options));
    }

    /// The compositor ends the activation on its own (the escape chord): the session is disabled.
    fn cancel_externally(&self) {
        let id = {
            let mut inner = self.fake.lock();
            inner.enabled = false;
            inner.activated.take().expect("nothing is activated")
        };
        self.eis(Cmd::Stop(Target::Pointer));
        self.eis(Cmd::Stop(Target::Keyboard));
        zbus::block_on(emit_session_signal(
            &self.connection,
            "Deactivated",
            activation_options(id, None, None),
        ));
    }

    fn emit_zones_changed(&self) {
        {
            let mut inner = self.fake.lock();
            inner.enabled = false;
            inner.zone_set += 1;
        }
        let mut options = HashMap::new();
        options.insert("zone_set".to_owned(), Value::U32(self.fake.lock().zone_set));
        zbus::block_on(emit_session_signal(
            &self.connection,
            "ZonesChanged",
            options,
        ));
    }

    fn emit_closed(&self) {
        let session = self.fake.lock().sessions.last().cloned().unwrap();
        zbus::block_on(self.connection.emit_signal(
            None::<&str>,
            session.as_str(),
            "org.freedesktop.portal.Session",
            "Closed",
            &HashMap::<String, Value<'_>>::new(),
        ))
        .unwrap();
    }

    fn set_zones(&self, zones: Vec<(u32, u32, i32, i32)>) {
        self.fake.lock().zones = zones;
    }
}

// ---- helpers for the tests -------------------------------------------------------------------

use crosspane_platform::CaptureEvent as Ev;

/// The events the subscriber got, in order.
#[derive(Default)]
struct Events {
    list: Mutex<Vec<CaptureEvent>>,
    changed: Condvar,
}

impl Events {
    fn sink(self: &Arc<Self>) -> Arc<dyn EventSink<CaptureEvent>> {
        let events = Arc::clone(self);
        Arc::new(move |event: CaptureEvent| {
            events.list.lock().unwrap().push(event);
            events.changed.notify_all();
        })
    }

    fn all(&self) -> Vec<CaptureEvent> {
        self.list.lock().unwrap().clone()
    }

    fn wait_for(&self, what: &str, done: impl Fn(&[CaptureEvent]) -> bool) -> Vec<CaptureEvent> {
        let deadline = Instant::now() + WAIT;
        let mut list = self.list.lock().unwrap();
        while !done(&list) {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "timed out waiting for {what}: {:?}",
                summary(&list)
            );
            list = self.changed.wait_timeout(list, left).unwrap().0;
        }
        list.clone()
    }
}

/// The events without their (many) repeated presses and motions, for failure messages.
fn summary(events: &[CaptureEvent]) -> Vec<String> {
    events
        .iter()
        .map(|event| match event {
            Ev::EdgePressed { position, .. } => format!("EdgePressed({position:.2})"),
            Ev::Motion { dx, dy, .. } => format!("Motion({dx},{dy})"),
            other => format!("{other:?}"),
        })
        .collect()
}

fn pressed(events: &[CaptureEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, Ev::EdgePressed { .. }))
        .count()
}

fn ended(events: &[CaptureEvent]) -> Vec<(CaptureId, EndReason)> {
    events
        .iter()
        .filter_map(|event| match event {
            Ev::Ended { id, reason } => Some((*id, *reason)),
            _ => None,
        })
        .collect()
}

#[derive(Default)]
struct Statuses {
    list: Mutex<Vec<CaptureStatus>>,
    changed: Condvar,
}

impl Statuses {
    fn callback(self: &Arc<Self>) -> Arc<dyn Fn(CaptureStatus) + Send + Sync> {
        let statuses = Arc::clone(self);
        Arc::new(move |status| {
            statuses.list.lock().unwrap().push(status);
            statuses.changed.notify_all();
        })
    }

    fn all(&self) -> Vec<CaptureStatus> {
        self.list.lock().unwrap().clone()
    }

    fn wait_for(&self, status: CaptureStatus) {
        let deadline = Instant::now() + WAIT;
        let mut list = self.list.lock().unwrap();
        while !list.contains(&status) {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "timed out waiting for {status:?}: {list:?}"
            );
            list = self.changed.wait_timeout(list, left).unwrap().0;
        }
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        TempDir(std::env::temp_dir().join(format!(
            "crosspane-capture-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )))
    }

    fn token_path(&self) -> PathBuf {
        self.0.join("portal-input-capture.token")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn display(id: u32, pixels: (u32, u32), origin: (f64, f64)) -> DisplayInfo {
    DisplayInfo {
        id: DisplayId(id),
        name: format!("display-{id}"),
        geometry: DisplayGeometry {
            physical_size: SizeMm::new(600.0, 340.0),
            pixel_size: PixelSize::new(pixels.0, pixels.1),
            scale: 1.0,
            logical_origin: PointLogical::new(origin.0, origin.1),
        },
        refresh_millihz: 60_000,
        color_space: ColorSpace::Srgb,
        hdr: false,
    }
}

/// The developer's layout: DP-3 (1), DP-2 (2) and the portrait HDMI-1 (3) at `hdmi_y`.
fn layout(hdmi_y: f64) -> Vec<DisplayInfo> {
    vec![
        display(1, (3440, 1440), (1080.0, 1080.0)),
        display(2, (1920, 1080), (1817.0, 0.0)),
        display(3, (1080, 1920), (0.0, hdmi_y)),
    ]
}

fn zones(hdmi_y: i32) -> Vec<(u32, u32, i32, i32)> {
    vec![
        (3440, 1440, 1080, 1080),
        (1920, 1080, 1817, 0),
        (1080, 1920, 0, hdmi_y),
    ]
}

/// HDMI-1's outer right stretch: barrier line (1080,600)-(1080,1079) on the dev layout.
fn hdmi_right() -> CapturePortal {
    CapturePortal {
        id: PortalId(1),
        display: DisplayId(3),
        edge: crosspane_platform::Edge::Right,
        from: 0.0,
        to: 480.0,
    }
}

/// DP-3's right edge: barrier line (4520,1080)-(4520,2519).
fn dp3_right() -> CapturePortal {
    CapturePortal {
        id: PortalId(2),
        display: DisplayId(1),
        edge: crosspane_platform::Edge::Right,
        from: 0.0,
        to: 1440.0,
    }
}

struct Rig {
    portal: Portal,
    capture: Option<PortalInputCapture>,
    events: Arc<Events>,
    statuses: Arc<Statuses>,
    gate: Arc<IoGate>,
    displays: Arc<Mutex<Vec<DisplayInfo>>>,
    cursor: Arc<Mutex<Vec<bool>>>,
    /// How many of the next "show" calls of the cursor hook fail.
    cursor_failures: Arc<AtomicUsize>,
    /// The local releases the backend replayed, and whether the fake compositor still held the
    /// activation at that moment (an injected up would be captured then).
    ups: Arc<Mutex<Vec<(LocalUp, bool)>>>,
    dir: TempDir,
}

impl Rig {
    fn new(version: u32, kde: bool) -> Option<Rig> {
        let portal = Portal::start(version, kde, zones(600))?;
        let mut rig = Rig {
            portal,
            capture: None,
            events: Arc::new(Events::default()),
            statuses: Arc::new(Statuses::default()),
            gate: IoGate::new(),
            displays: Arc::new(Mutex::new(layout(600.0))),
            cursor: Arc::new(Mutex::new(Vec::new())),
            cursor_failures: Arc::new(AtomicUsize::new(0)),
            ups: Arc::new(Mutex::new(Vec::new())),
            dir: TempDir::new(),
        };
        rig.gate.set_session_permits(true);
        rig.gate.set_engine_permits(true);
        rig.start(Quirks {
            disable_before_barriers: kde,
            close_to_replace: false,
        });
        Some(rig)
    }

    /// (Re)start the backend on the portal's bus.
    fn start(&mut self, quirks: Quirks) {
        self.capture = None;
        let displays = Arc::clone(&self.displays);
        let cursor = Arc::clone(&self.cursor);
        let failures = Arc::clone(&self.cursor_failures);
        let ups = Arc::clone(&self.ups);
        let fake = Arc::clone(&self.portal.fake);
        let config = InputCaptureConfig {
            displays: Arc::new(move || displays.lock().unwrap().clone()),
            lock_keys: Arc::new(|| {
                Some(LockKeys {
                    caps_lock: Some(false),
                    num_lock: Some(true),
                    scroll_lock: None,
                })
            }),
            gate: Arc::clone(&self.gate),
            cursor: Some(Arc::new(move |hidden| {
                if !hidden && failures.load(Ordering::SeqCst) > 0 {
                    failures.fetch_sub(1, Ordering::SeqCst);
                    return Err(PlatformError::Timeout);
                }
                cursor.lock().unwrap().push(hidden);
                Ok(())
            })),
            local_release: Some(Arc::new(move |up| {
                let held = fake.lock().activated.is_some();
                ups.lock().unwrap().push((up, held));
            })),
            token_path: self.dir.token_path(),
            quirks,
        };
        let mut capture = PortalInputCapture::spawn_on(
            config,
            self.statuses.callback(),
            Some(self.portal.daemon.address.clone()),
        )
        .unwrap();
        capture.subscribe(self.events.sink()).unwrap();
        self.capture = Some(capture);
    }

    fn capture(&mut self) -> &mut PortalInputCapture {
        self.capture.as_mut().unwrap()
    }

    /// Set the portals and wait until the barriers are enabled with devices present.
    fn arm(&mut self, portals: &[CapturePortal]) {
        self.capture().set_portals(portals).unwrap();
        self.statuses.wait_for(CaptureStatus::Ready);
        self.portal
            .wait_calls("Enable", |calls| calls.contains(&Call::Enable));
        let deadline = Instant::now() + WAIT;
        while !self.capture().is_ready() {
            assert!(Instant::now() < deadline, "the backend never became ready");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Activate the first barrier at HDMI-1's outer right stretch and wait for the first press.
    fn press(&mut self) -> u32 {
        let before = pressed(&self.events.all());
        let id = self.portal.activate(1, (1080.0, 840.0));
        self.events
            .wait_for("an EdgePressed", |events| pressed(events) > before);
        id
    }

    fn eis_motion(&self, dx: f32, dy: f32) {
        self.portal.eis(Cmd::Motion(dx, dy));
        self.portal.eis(Cmd::Frame(Target::Pointer));
    }
}

// ---- tests -----------------------------------------------------------------------------------

macro_rules! rig {
    ($version:expr, $kde:expr) => {
        match Rig::new($version, $kde) {
            Some(rig) => rig,
            None => return,
        }
    };
}

#[test]
fn no_session_is_created_until_a_portal_is_wanted_then_it_installs_in_the_order_mutter_needs() {
    let mut rig = rig!(1, false);
    thread::sleep(Duration::from_millis(150));
    assert!(
        rig.portal.calls().is_empty(),
        "nothing before the first set"
    );
    assert_eq!(rig.capture().status(), CaptureStatus::Idle);
    // The subscription's initial events.
    assert_eq!(
        rig.events.all(),
        vec![
            Ev::LockKeys(LockKeys {
                caps_lock: Some(false),
                num_lock: Some(true),
                scroll_lock: None
            }),
            Ev::KeyboardBlinded(false)
        ]
    );
    // Setting an empty set wants nothing either.
    rig.capture().set_portals(&[]).unwrap();
    thread::sleep(Duration::from_millis(100));
    assert!(rig.portal.calls().is_empty());

    rig.arm(&[hdmi_right()]);
    assert_eq!(
        rig.portal.calls(),
        vec![
            Call::CreateSession,
            Call::GetZones,
            Call::SetBarriers {
                barriers: vec![(1, (1080, 600, 1080, 1079))],
                zone_set: 0
            },
            Call::ConnectToEis,
            Call::Enable,
        ]
    );
    assert_eq!(
        rig.statuses.all(),
        vec![CaptureStatus::Pending, CaptureStatus::Ready]
    );
}

#[test]
fn replacing_barriers_is_disable_then_set_then_enable_and_an_empty_set_disables() {
    let mut rig = rig!(1, false);
    rig.arm(&[hdmi_right()]);
    rig.portal.fake.lock().calls.clear();
    rig.capture()
        .set_portals(&[hdmi_right(), dp3_right()])
        .unwrap();
    let calls = rig
        .portal
        .wait_calls("the replacement", |calls| calls.contains(&Call::Enable));
    assert_eq!(
        calls,
        vec![
            Call::GetZones,
            Call::Disable,
            Call::SetBarriers {
                barriers: vec![(1, (1080, 600, 1080, 1079)), (2, (4520, 1080, 4520, 2519))],
                zone_set: 0
            },
            Call::Enable,
        ]
    );
    // No new session, no second EIS connection.
    rig.portal.fake.lock().calls.clear();
    rig.capture().set_portals(&[]).unwrap();
    let calls = rig
        .portal
        .wait_calls("the disable", |calls| !calls.is_empty());
    assert_eq!(calls, vec![Call::Disable]);
    let deadline = Instant::now() + WAIT;
    while rig.capture().is_ready() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn the_kde_flavour_skips_disable_and_close_to_replace_recreates_the_session() {
    let mut rig = rig!(1, true);
    rig.arm(&[hdmi_right()]);
    rig.portal.fake.lock().calls.clear();
    rig.capture()
        .set_portals(&[hdmi_right(), dp3_right()])
        .unwrap();
    let calls = rig
        .portal
        .wait_calls("the replacement", |calls| calls.contains(&Call::Enable));
    assert!(!calls.contains(&Call::Disable), "{calls:?}");
    assert_eq!(calls.first(), Some(&Call::GetZones));
    assert!(matches!(calls.get(1), Some(Call::SetBarriers { .. })));

    // close_to_replace: the session is closed and a new one made.
    rig.portal.fake.lock().calls.clear();
    rig.start(Quirks {
        disable_before_barriers: true,
        close_to_replace: true,
    });
    rig.arm(&[hdmi_right()]);
    rig.portal.fake.lock().calls.clear();
    rig.capture().set_portals(&[dp3_right()]).unwrap();
    let calls = rig
        .portal
        .wait_calls("the new session", |calls| calls.contains(&Call::Enable));
    assert_eq!(calls.first(), Some(&Call::Close), "{calls:?}");
    assert!(calls.contains(&Call::CreateSession));
}

#[test]
fn a_pending_activation_presses_swallows_input_and_is_adopted_by_begin() {
    let mut rig = rig!(1, false);
    rig.arm(&[hdmi_right()]);
    let activation = rig.press();
    assert_eq!(activation, 1);

    // Keys and a button pressed after the activation, and motion along the stretch.
    rig.portal.eis(Cmd::Key(30, true));
    rig.portal.eis(Cmd::Frame(Target::Keyboard));
    rig.eis_motion(6.0, 24.0);
    let events = rig
        .events
        .wait_for("a second press", |events| pressed(events) >= 2);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Ev::Key { .. } | Ev::Motion { .. } | Ev::Started { .. })),
        "nothing is routed before begin: {:?}",
        summary(&events)
    );
    assert!(
        events.iter().any(
            |e| matches!(e, Ev::EdgePressed { position, .. } if (*position - 0.55).abs() < 1e-9)
        )
    );

    let start = rig.capture().begin(CaptureId(7), PortalId(1)).unwrap();
    assert_eq!(start.held_keys, vec![HidUsage::keyboard(0x04)]);
    assert_eq!(
        start.lock_keys,
        LockKeys {
            caps_lock: Some(false),
            num_lock: Some(true),
            scroll_lock: None
        }
    );
    // Started precedes every capture event, and events are routed now.
    rig.eis_motion(3.0, -1.0);
    rig.portal.eis(Cmd::Key(30, false));
    rig.portal.eis(Cmd::Frame(Target::Keyboard));
    let events = rig.events.wait_for("the routed events", |events| {
        events
            .iter()
            .any(|e| matches!(e, Ev::Key { down: false, .. }))
    });
    let at = events
        .iter()
        .position(|e| matches!(e, Ev::Started { id: CaptureId(7) }))
        .expect("Started");
    assert!(
        events[..at]
            .iter()
            .all(|e| !matches!(e, Ev::Motion { .. } | Ev::Key { .. }))
    );
    assert!(events[at..].iter().any(|e| matches!(
        e,
        Ev::Motion { dx, dy, kind: MotionKind::Accelerated { display: DisplayId(3) }, .. }
            if *dx == 3.0 && *dy == -1.0
    )));
    // The local cursor is hidden while captured (best effort, on its own thread).
    let deadline = Instant::now() + WAIT;
    while rig.cursor.lock().unwrap().last() != Some(&true) {
        assert!(Instant::now() < deadline, "the cursor was never hidden");
        thread::sleep(Duration::from_millis(5));
    }

    // End: Release(activation, the warp point), answered before Ended is reported.
    rig.capture()
        .end(Some((DisplayId(3), PointDevice::new(500.0, 100.0))))
        .unwrap();
    let release = rig
        .portal
        .calls()
        .into_iter()
        .find_map(|call| match call {
            Call::Release { activation, cursor } => Some((activation, cursor)),
            _ => None,
        })
        .expect("a Release");
    assert_eq!(release, (Some(1), Some((500.0, 700.0))));
    assert_eq!(
        ended(&rig.events.all()),
        vec![(CaptureId(7), EndReason::Requested)]
    );
    // The pointer is off the portal again: EdgeReleased follows Ended so the engine can re-arm it.
    let events = rig.events.all();
    let at = events
        .iter()
        .position(|e| matches!(e, Ev::Ended { .. }))
        .unwrap();
    assert!(matches!(
        events.get(at + 1),
        Some(Ev::EdgeReleased {
            portal: PortalId(1),
            ..
        })
    ));
    let deadline = Instant::now() + WAIT;
    while rig.cursor.lock().unwrap().last() != Some(&false) {
        assert!(
            Instant::now() < deadline,
            "the cursor was never shown again"
        );
        thread::sleep(Duration::from_millis(5));
    }
    // Our own release is not a loss, and no re-arm follows it: barriers stay armed.
    thread::sleep(Duration::from_millis(100));
    assert_eq!(ended(&rig.events.all()).len(), 1);
    assert!(!rig.portal.calls().contains(&Call::Disable));
    // A second `end` has nothing to give back.
    rig.capture().end(None).unwrap();
    // The next push activates again without re-enabling.
    let before = rig
        .portal
        .calls()
        .iter()
        .filter(|c| **c == Call::Enable)
        .count();
    assert_eq!(rig.portal.activate(1, (1080.0, 700.0)), 2);
    rig.events
        .wait_for("the next press", |events| pressed(events) >= 3);
    assert_eq!(
        rig.portal
            .calls()
            .iter()
            .filter(|c| **c == Call::Enable)
            .count(),
        before
    );
}

#[test]
fn a_pending_activation_the_pointer_leaves_is_released_to_its_origin() {
    let mut rig = rig!(1, false);
    rig.arm(&[hdmi_right()]);
    rig.press();
    // An ordinary push-back (30 px inward, accelerated EIS motion) keeps the crossing pending.
    rig.eis_motion(20.0, 0.0);
    rig.eis_motion(-30.0, 0.0);
    rig.events
        .wait_for("the press", |events| pressed(events) >= 3);
    assert!(
        !rig.portal
            .calls()
            .iter()
            .any(|c| matches!(c, Call::Release { .. }))
    );
    // Push outwards again, then 40 px back in: net inward beyond the 32 px box.
    rig.eis_motion(40.0, 0.0);
    rig.eis_motion(-40.0, 0.0);
    let calls = rig.portal.wait_calls("the release", |calls| {
        calls.iter().any(|c| matches!(c, Call::Release { .. }))
    });
    let release = calls
        .iter()
        .find_map(|c| match c {
            Call::Release { activation, cursor } => Some((*activation, *cursor)),
            _ => None,
        })
        .unwrap();
    assert_eq!(release, (Some(1), Some((1078.0, 840.0))));
    rig.events.wait_for("EdgeReleased", |events| {
        events.iter().any(|e| {
            matches!(
                e,
                Ev::EdgeReleased {
                    portal: PortalId(1),
                    ..
                }
            )
        })
    });
    assert!(matches!(
        rig.capture().begin(CaptureId(1), PortalId(1)),
        Err(PlatformError::NotFound)
    ));
}

#[test]
fn a_closed_gate_releases_a_pending_activation_and_refuses_begin() {
    let mut rig = rig!(1, false);
    rig.arm(&[hdmi_right()]);
    rig.press();
    rig.gate.set_session_permits(false);
    rig.portal.wait_calls("the release", |calls| {
        calls.iter().any(|c| matches!(c, Call::Release { .. }))
    });
    assert!(matches!(
        rig.capture().begin(CaptureId(1), PortalId(1)),
        Err(PlatformError::NotFound | PlatformError::Locked)
    ));
    // An activation while locked is released at once and never reported.
    let presses = pressed(&rig.events.all());
    rig.portal.activate(1, (1080.0, 840.0));
    let releases = |rig: &Rig| {
        rig.portal
            .calls()
            .iter()
            .filter(|c| matches!(c, Call::Release { .. }))
            .count()
    };
    let deadline = Instant::now() + WAIT;
    while releases(&rig) < 2 {
        assert!(
            Instant::now() < deadline,
            "the locked activation was never released"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(pressed(&rig.events.all()), presses);
}

#[test]
fn a_closed_gate_ends_an_active_capture_lost() {
    let mut rig = rig!(1, false);
    rig.arm(&[hdmi_right()]);
    rig.press();
    rig.capture().begin(CaptureId(3), PortalId(1)).unwrap();
    rig.gate.set_engine_permits(false);
    rig.events.wait_for("Ended Lost", |events| {
        ended(events) == vec![(CaptureId(3), EndReason::Lost)]
    });
    rig.portal.wait_calls("the release", |calls| {
        calls.iter().any(|c| matches!(c, Call::Release { .. }))
    });
}

#[test]
fn a_deactivation_without_our_release_is_lost_and_the_session_is_armed_again() {
    let mut rig = rig!(1, false);
    rig.arm(&[hdmi_right()]);
    rig.press();
    rig.capture().begin(CaptureId(5), PortalId(1)).unwrap();
    rig.portal.fake.lock().calls.clear();
    rig.portal.cancel_externally();
    rig.events.wait_for("Ended Lost", |events| {
        ended(events) == vec![(CaptureId(5), EndReason::Lost)]
    });
    // Disable (a disabled session says no, which is fine), the zones, the barriers, Enable.
    let calls = rig
        .portal
        .wait_calls("the re-arm", |calls| calls.contains(&Call::Enable));
    assert_eq!(calls.first(), Some(&Call::GetZones), "{calls:?}");
    assert!(calls.contains(&Call::Disable));
    assert!(calls.iter().any(|c| matches!(c, Call::SetBarriers { .. })));
    assert!(!calls.iter().any(|c| matches!(c, Call::Release { .. })));
    // And the next push works.
    let deadline = Instant::now() + WAIT;
    while !rig.capture().is_ready() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    rig.portal.activate(1, (1080.0, 800.0));
    rig.events
        .wait_for("a press", |events| pressed(events) >= 2);
}

#[test]
fn zones_changing_ends_the_capture_and_installs_the_new_layout() {
    let mut rig = rig!(1, false);
    rig.arm(&[hdmi_right()]);
    rig.press();
    rig.capture().begin(CaptureId(2), PortalId(1)).unwrap();
    rig.portal.fake.lock().calls.clear();
    // HDMI-1 moves up 100 px: the compositor deactivates (signal first), then says zones changed.
    *rig.displays.lock().unwrap() = layout(500.0);
    rig.portal.set_zones(zones(500));
    rig.portal.cancel_externally();
    rig.portal.emit_zones_changed();
    rig.events.wait_for("Ended Lost", |events| {
        ended(events) == vec![(CaptureId(2), EndReason::Lost)]
    });
    let calls = rig.portal.wait_calls("the new layout", |calls| {
        calls
            .iter()
            .any(|c| matches!(c, Call::SetBarriers { barriers, .. } if barriers == &[(1, (1080, 500, 1080, 979))]))
            && calls.contains(&Call::Enable)
    });
    // Lost exactly once, and no release was needed.
    assert_eq!(ended(&rig.events.all()).len(), 1);
    assert!(!calls.iter().any(|c| matches!(c, Call::Release { .. })));
}

#[test]
fn abort_releases_an_active_capture_within_the_budget() {
    let mut rig = rig!(1, false);
    rig.arm(&[hdmi_right()]);
    rig.press();
    rig.capture().begin(CaptureId(4), PortalId(1)).unwrap();
    let abort = rig.capture().abort_handle();
    let before = Instant::now();
    abort.abort();
    let release_at = loop {
        if let Some((at, _)) = rig
            .portal
            .timed_calls()
            .into_iter()
            .find(|(_, call)| matches!(call, Call::Release { .. }))
        {
            break at;
        }
        assert!(before.elapsed() < WAIT, "no Release after the abort");
        thread::sleep(Duration::from_micros(200));
    };
    let latency = release_at.saturating_duration_since(before);
    assert!(
        latency < Duration::from_millis(50),
        "Release took {latency:?}"
    );
    rig.events.wait_for("Ended Aborted", |events| {
        ended(events) == vec![(CaptureId(4), EndReason::Aborted)]
    });
    // Idempotent.
    abort.abort();
    thread::sleep(Duration::from_millis(100));
    assert_eq!(ended(&rig.events.all()).len(), 1);
}

#[test]
fn abort_releases_a_pending_activation_too() {
    let mut rig = rig!(1, false);
    rig.arm(&[hdmi_right()]);
    rig.press();
    rig.capture().abort_handle().abort();
    rig.portal.wait_calls("the release", |calls| {
        calls.iter().any(|c| matches!(c, Call::Release { .. }))
    });
    assert!(matches!(
        rig.capture().begin(CaptureId(1), PortalId(1)),
        Err(PlatformError::NotFound)
    ));
}

#[test]
fn removing_the_active_portal_ends_the_capture_lost_but_keeping_it_does_not() {
    let mut rig = rig!(1, false);
    rig.arm(&[hdmi_right(), dp3_right()]);
    rig.press();
    rig.capture().begin(CaptureId(6), PortalId(1)).unwrap();
    // The active portal is unchanged: the replacement is deferred and the capture goes on.
    rig.portal.fake.lock().calls.clear();
    rig.capture().set_portals(&[hdmi_right()]).unwrap();
    thread::sleep(Duration::from_millis(200));
    assert!(ended(&rig.events.all()).is_empty());
    assert!(
        rig.portal.calls().is_empty(),
        "no portal call while the capture is active"
    );
    // Removing it ends the capture Lost, releases, and installs the (deferred) set afterwards.
    rig.capture().set_portals(&[dp3_right()]).unwrap();
    rig.events.wait_for("Ended Lost", |events| {
        ended(events) == vec![(CaptureId(6), EndReason::Lost)]
    });
    let calls = rig
        .portal
        .wait_calls("the install", |calls| calls.contains(&Call::Enable));
    assert!(calls.iter().any(|c| matches!(c, Call::Release { .. })));
    assert!(calls.iter().any(
        |c| matches!(c, Call::SetBarriers { barriers, .. } if barriers == &[(1, (4520, 1080, 4520, 2519))])
    ));
}

#[test]
fn invalid_sets_are_rejected_with_the_marker_and_change_nothing() {
    let mut rig = rig!(1, false);
    rig.arm(&[hdmi_right()]);
    rig.portal.fake.lock().calls.clear();
    // Touching DP-3's left edge as well: adjacent to two monitor edges.
    let shared = CapturePortal {
        id: PortalId(9),
        display: DisplayId(1),
        edge: crosspane_platform::Edge::Left,
        from: 0.0,
        to: 1440.0,
    };
    let unknown = CapturePortal {
        display: DisplayId(99),
        ..hdmi_right()
    };
    for bad in [shared, unknown] {
        let error = rig.capture().set_portals(&[bad]).unwrap_err();
        assert!(
            matches!(&error, PlatformError::Backend(m) if m.starts_with(PORTALS_REJECTED)),
            "{error:?}"
        );
        assert_eq!(
            crate::hyprland::capture::set_portals_failure(&error),
            crate::hyprland::capture::SetPortalsFailure::Rejected
        );
    }
    thread::sleep(Duration::from_millis(100));
    assert!(
        rig.portal.calls().is_empty(),
        "the previous set stays installed"
    );
}

#[test]
fn a_denied_dialog_is_reported_and_never_asked_again() {
    let mut rig = rig!(1, false);
    rig.portal.script(Create::Respond(1));
    rig.capture().set_portals(&[hdmi_right()]).unwrap();
    rig.statuses.wait_for(CaptureStatus::Denied);
    assert!(matches!(
        rig.capture().set_portals(&[hdmi_right()]),
        Err(PlatformError::Backend(m)) if m.starts_with(PORTALS_REJECTED)
    ));
    // Disarming is always fine.
    rig.capture().set_portals(&[]).unwrap();
    thread::sleep(Duration::from_millis(200));
    assert_eq!(rig.portal.calls(), vec![Call::CreateSession]);
    assert!(!rig.capture().is_ready());
}

#[test]
fn the_user_stopping_the_session_closes_it_and_ends_a_capture_lost() {
    let mut rig = rig!(1, false);
    rig.arm(&[hdmi_right()]);
    rig.press();
    rig.capture().begin(CaptureId(8), PortalId(1)).unwrap();
    rig.portal.emit_closed();
    rig.statuses.wait_for(CaptureStatus::Closed);
    rig.events.wait_for("Ended Lost", |events| {
        ended(events) == vec![(CaptureId(8), EndReason::Lost)]
    });
    assert!(!rig.capture().is_ready());
    assert!(matches!(
        rig.capture().set_portals(&[hdmi_right()]),
        Err(PlatformError::Backend(_))
    ));
}

#[test]
fn a_version_two_portal_persists_with_a_rotated_private_token() {
    let mut rig = rig!(2, false);
    rig.arm(&[hdmi_right()]);
    let calls = rig.portal.calls();
    assert_eq!(
        &calls[..2],
        &[
            Call::CreateSession2,
            Call::Start {
                persist_mode: Some(2),
                restore_token: None
            },
        ]
    );
    assert_eq!(token::read(&rig.dir.token_path()).as_deref(), Some("tok-1"));
    assert_eq!(mode(&rig.dir.token_path()), 0o600);
    // The next start presents it and stores the rotated one.
    rig.capture = None;
    rig.portal.fake.lock().calls.clear();
    rig.start(Quirks::default());
    rig.statuses.list.lock().unwrap().clear();
    rig.arm(&[hdmi_right()]);
    let calls = rig.portal.calls();
    assert!(
        calls.contains(&Call::Start {
            persist_mode: Some(2),
            restore_token: Some("tok-1".into())
        }),
        "{calls:?}"
    );
    assert_eq!(token::read(&rig.dir.token_path()).as_deref(), Some("tok-2"));
}

#[test]
fn dropping_the_backend_gives_input_back_and_closes_the_session() {
    let mut rig = rig!(1, false);
    rig.arm(&[hdmi_right()]);
    rig.press();
    rig.capture().begin(CaptureId(9), PortalId(1)).unwrap();
    let deadline = Instant::now() + WAIT;
    while rig.cursor.lock().unwrap().last() != Some(&true) {
        assert!(Instant::now() < deadline, "the cursor was never hidden");
        thread::sleep(Duration::from_millis(5));
    }
    let started = Instant::now();
    rig.capture = None;
    assert!(started.elapsed() < Duration::from_secs(3));
    let calls = rig.portal.calls();
    assert!(
        calls.iter().any(|c| matches!(c, Call::Release { .. })),
        "{calls:?}"
    );
    assert!(calls.contains(&Call::Close), "{calls:?}");
    assert_eq!(
        ended(&rig.events.all()),
        vec![(CaptureId(9), EndReason::Aborted)]
    );
    // The cursor is shown again.
    assert_eq!(rig.cursor.lock().unwrap().last(), Some(&false));
}

#[test]
fn an_identical_set_is_a_no_op_and_a_different_one_reinstalls() {
    let mut rig = rig!(1, false);
    rig.arm(&[hdmi_right()]);
    rig.portal.fake.lock().calls.clear();
    // The engine offers the same set again whenever its portal mapping changes.
    rig.capture().set_portals(&[hdmi_right()]).unwrap();
    thread::sleep(Duration::from_millis(150));
    assert!(rig.portal.calls().is_empty(), "{:?}", rig.portal.calls());
    assert!(
        rig.capture().is_ready(),
        "the barriers were never taken down"
    );
    rig.capture()
        .set_portals(&[hdmi_right(), dp3_right()])
        .unwrap();
    rig.portal
        .wait_calls("the new set", |calls| calls.contains(&Call::Enable));
}

#[test]
fn a_cursor_hook_that_fails_to_show_is_tried_again() {
    let mut rig = rig!(1, false);
    rig.arm(&[hdmi_right()]);
    rig.press();
    rig.capture().begin(CaptureId(2), PortalId(1)).unwrap();
    let deadline = Instant::now() + WAIT;
    while rig.cursor.lock().unwrap().last() != Some(&true) {
        assert!(Instant::now() < deadline, "the cursor was never hidden");
        thread::sleep(Duration::from_millis(5));
    }
    rig.cursor_failures.store(2, Ordering::SeqCst);
    rig.capture().end(None).unwrap();
    let deadline = Instant::now() + WAIT;
    while rig.cursor.lock().unwrap().last() != Some(&false) {
        assert!(
            Instant::now() < deadline,
            "the cursor was never shown again"
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(rig.cursor_failures.load(Ordering::SeqCst), 0);
}

#[test]
fn an_unreadable_activation_is_released_anyway() {
    let mut rig = rig!(1, false);
    rig.arm(&[hdmi_right()]);
    rig.portal.fake.lock().calls.clear();
    rig.portal.activate_unreadable();
    let calls = rig.portal.wait_calls("the release", |calls| {
        calls.iter().any(|c| matches!(c, Call::Release { .. }))
    });
    // No id and no position to give: the portal is asked to release whatever is held.
    assert!(
        calls.contains(&Call::Release {
            activation: None,
            cursor: None
        }),
        "{calls:?}"
    );
    assert!(pressed(&rig.events.all()) == 0);
}

// ---- review round 1: local ups, stale early input, thread death -----------------------------

impl Rig {
    fn eis_key(&self, code: u32, down: bool) {
        self.portal.eis(Cmd::Key(code, down));
        self.portal.eis(Cmd::Frame(Target::Keyboard));
    }

    fn eis_button(&self, code: u32, down: bool) {
        self.portal.eis(Cmd::Button(code, down));
        self.portal.eis(Cmd::Frame(Target::Pointer));
    }

    fn wait_ups(&self, count: usize) -> Vec<(LocalUp, bool)> {
        let deadline = Instant::now() + WAIT;
        loop {
            let ups = self.ups.lock().unwrap().clone();
            if ups.len() >= count {
                return ups;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {count} local releases: {ups:?}"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn releases(&self) -> usize {
        self.portal
            .calls()
            .iter()
            .filter(|c| matches!(c, Call::Release { .. }))
            .count()
    }
}

const UP_CTRL: LocalUp = LocalUp::Key(HidUsage::keyboard(0xE0));
const UP_A: LocalUp = LocalUp::Key(HidUsage::keyboard(0x04));
const UP_SHIFT: LocalUp = LocalUp::Key(HidUsage::keyboard(0xE1));
const UP_LEFT: LocalUp = LocalUp::Button(crosspane_types::hid::MouseButton::PRIMARY);

#[test]
fn the_releases_of_keys_held_before_the_activation_are_replayed_after_the_capture_ends() {
    let mut rig = rig!(1, false);
    rig.arm(&[hdmi_right()]);
    rig.press();
    // A and the left button were down before the activation; only their ups arrive. A key
    // pressed (and released) after the activation is not owed.
    rig.eis_key(30, false);
    rig.eis_button(0x110, false);
    rig.eis_key(31, true);
    rig.eis_key(31, false);
    rig.eis_motion(0.0, 24.0);
    rig.events
        .wait_for("a press after the input", |events| pressed(events) >= 2);
    rig.capture().begin(CaptureId(7), PortalId(1)).unwrap();
    // Ctrl and Shift were held too; their ups arrive while captured and are forwarded.
    rig.eis_key(29, false);
    rig.eis_key(42, false);
    rig.events.wait_for("the forwarded ups", |events| {
        events
            .iter()
            .filter(|e| matches!(e, Ev::Key { down: false, .. }))
            .count()
            >= 2
    });
    // While the capture lasts nothing is injected (the compositor would capture it).
    thread::sleep(Duration::from_millis(100));
    assert!(rig.ups.lock().unwrap().is_empty());
    rig.capture().end(None).unwrap();
    let ups = rig.wait_ups(4);
    // Once each, keys in code order then buttons; the activation was already released.
    assert_eq!(
        ups.iter().map(|(up, _)| *up).collect::<Vec<_>>(),
        vec![UP_CTRL, UP_A, UP_SHIFT, UP_LEFT]
    );
    assert!(ups.iter().all(|(_, held)| !held), "{ups:?}");
    thread::sleep(Duration::from_millis(100));
    assert_eq!(rig.ups.lock().unwrap().len(), 4);
}

#[test]
fn the_ups_of_an_abandoned_pending_activation_are_replayed_after_its_release() {
    let mut rig = rig!(1, false);
    rig.arm(&[hdmi_right()]);
    rig.press();
    rig.eis_key(42, false);
    // A press after the key shows the key was processed (a closed gate drops later input).
    rig.eis_motion(0.0, 10.0);
    rig.events
        .wait_for("a press after the input", |events| pressed(events) >= 2);
    rig.gate.set_session_permits(false);
    let ups = rig.wait_ups(1);
    assert_eq!(ups, vec![(UP_SHIFT, false)]);
    assert_eq!(rig.releases(), 1);
}

#[test]
fn the_ups_of_an_activation_the_compositor_ended_are_replayed() {
    let mut rig = rig!(1, false);
    rig.arm(&[hdmi_right()]);
    rig.press();
    rig.capture().begin(CaptureId(5), PortalId(1)).unwrap();
    rig.eis_key(29, false);
    rig.events.wait_for("the forwarded up", |events| {
        events
            .iter()
            .any(|e| matches!(e, Ev::Key { down: false, .. }))
    });
    rig.portal.cancel_externally();
    rig.events.wait_for("Ended Lost", |events| {
        ended(events) == vec![(CaptureId(5), EndReason::Lost)]
    });
    assert_eq!(rig.wait_ups(1), vec![(UP_CTRL, false)]);
    assert_eq!(rig.releases(), 0, "the compositor let go by itself");
}

#[test]
fn the_ups_are_replayed_after_an_abort_released_the_activation() {
    let mut rig = rig!(1, false);
    rig.arm(&[hdmi_right()]);
    rig.press();
    rig.eis_key(30, false);
    rig.eis_motion(0.0, 10.0);
    rig.events
        .wait_for("a press after the input", |events| pressed(events) >= 2);
    rig.capture().begin(CaptureId(4), PortalId(1)).unwrap();
    rig.capture().abort_handle().abort();
    assert_eq!(rig.wait_ups(1), vec![(UP_A, false)]);
    rig.events.wait_for("Ended Aborted", |events| {
        ended(events) == vec![(CaptureId(4), EndReason::Aborted)]
    });
    thread::sleep(Duration::from_millis(100));
    assert_eq!(rig.ups.lock().unwrap().len(), 1, "replayed once");
}

#[test]
fn nothing_is_replayed_when_the_release_failed() {
    let mut rig = rig!(1, false);
    rig.arm(&[hdmi_right()]);
    rig.press();
    rig.eis_key(30, false);
    rig.eis_motion(0.0, 10.0);
    rig.events
        .wait_for("a press after the input", |events| pressed(events) >= 2);
    // The portal answers `Release` far too late: the activation may still be held when the
    // call gives up, and an injected up would be captured, so none is replayed.
    rig.portal.fake.lock().release_delay = Some(Duration::from_secs(1));
    rig.capture().abort_handle().abort();
    rig.portal.wait_calls("the release", |calls| {
        calls.iter().any(|c| matches!(c, Call::Release { .. }))
    });
    thread::sleep(Duration::from_millis(400));
    assert!(rig.ups.lock().unwrap().is_empty());
    // The same for the release thread's own releases (here: the gate closing).
    let mut rig = rig!(1, false);
    rig.arm(&[hdmi_right()]);
    rig.press();
    rig.eis_key(30, false);
    rig.eis_motion(0.0, 10.0);
    rig.events
        .wait_for("a press after the input", |events| pressed(events) >= 2);
    rig.portal.fake.lock().release_delay = Some(Duration::from_secs(1));
    rig.gate.set_session_permits(false);
    rig.portal.wait_calls("the release", |calls| {
        calls.iter().any(|c| matches!(c, Call::Release { .. }))
    });
    thread::sleep(Duration::from_millis(600));
    assert!(rig.ups.lock().unwrap().is_empty());
}

#[test]
fn end_gives_back_a_pending_activation() {
    let mut rig = rig!(1, false);
    rig.arm(&[hdmi_right()]);
    rig.press();
    // The engine refused the crossing and later ends it: the activation is released, at the
    // warp point when there is one.
    rig.capture()
        .end(Some((DisplayId(3), PointDevice::new(500.0, 100.0))))
        .unwrap();
    let calls = rig.portal.calls();
    assert!(
        calls.contains(&Call::Release {
            activation: Some(1),
            cursor: Some((500.0, 700.0))
        }),
        "{calls:?}"
    );
    rig.events.wait_for("EdgeReleased", |events| {
        events.iter().any(|e| {
            matches!(
                e,
                Ev::EdgeReleased {
                    portal: PortalId(1),
                    ..
                }
            )
        })
    });
    assert!(matches!(
        rig.capture().begin(CaptureId(1), PortalId(1)),
        Err(PlatformError::NotFound)
    ));
    // Nothing is held any more.
    rig.capture().end(None).unwrap();
    assert_eq!(rig.releases(), 1);
}

#[test]
fn input_for_an_activation_that_is_never_announced_is_released_after_250_ms() {
    let mut rig = rig!(1, false);
    rig.arm(&[hdmi_right()]);
    // The compositor holds an activation whose `Activated` signal never reaches us: only its
    // devices start emulating.
    rig.portal.fake.lock().activated = Some(7);
    let started = Instant::now();
    rig.portal.eis(Cmd::Start(Target::Pointer, 7));
    rig.eis_motion(3.0, 1.0);
    let calls = rig.portal.wait_calls("the release", |calls| {
        calls.iter().any(|c| matches!(c, Call::Release { .. }))
    });
    let waited = started.elapsed();
    assert!(
        calls.contains(&Call::Release {
            activation: None,
            cursor: None
        }),
        "{calls:?}"
    );
    assert!(
        waited >= Duration::from_millis(240),
        "released after {waited:?}"
    );
    assert!(waited < Duration::from_secs(2), "released after {waited:?}");
    assert_eq!(pressed(&rig.events.all()), 0);
}

/// Make `thread` panic at its next pass and wake it.
fn kill(rig: &mut Rig, thread: &'static str) {
    let shared = Arc::clone(&rig.capture().shared);
    *shared.panic_in.lock().unwrap() = Some(thread);
    match thread {
        super::EIS_THREAD => shared.eis.wake.wake(),
        super::ABORT_THREAD => shared.abort_wake.wake(),
        super::CALLER_THREAD => shared.caller.wake.wake(),
        super::CURSOR_THREAD => shared.cursor_wake.wake(),
        _ => shared.control.poke(),
    }
}

#[test]
fn a_thread_that_dies_never_leaves_the_compositor_activated() {
    for thread in [
        super::EIS_THREAD,
        super::ABORT_THREAD,
        super::CALLER_THREAD,
        super::CURSOR_THREAD,
        super::WORKER_THREAD,
    ] {
        let mut rig = rig!(1, false);
        rig.arm(&[hdmi_right()]);
        rig.press();
        rig.eis_key(30, false);
        rig.eis_motion(0.0, 10.0);
        rig.events
            .wait_for("a press after the input", |events| pressed(events) >= 2);
        rig.capture().begin(CaptureId(3), PortalId(1)).unwrap();
        rig.portal.fake.lock().calls.clear();
        let before = Instant::now();
        kill(&mut rig, thread);
        // The activation is released and the session closed, whatever thread died.
        let calls = rig.portal.wait_calls(thread, |calls| {
            calls.iter().any(|c| matches!(c, Call::Release { .. })) && calls.contains(&Call::Close)
        });
        let close = rig
            .portal
            .timed_calls()
            .into_iter()
            .find(|(_, call)| *call == Call::Close)
            .map(|(at, _)| at.saturating_duration_since(before))
            .unwrap();
        assert!(
            close < Duration::from_millis(500),
            "{thread}: Close took {close:?} ({calls:?})"
        );
        assert!(
            rig.portal.fake.lock().activated.is_none(),
            "{thread}: still activated"
        );
        rig.events.wait_for("Ended Aborted", |events| {
            ended(events) == vec![(CaptureId(3), EndReason::Aborted)]
        });
        rig.statuses.wait_for(CaptureStatus::Closed);
        assert!(!rig.capture().is_ready(), "{thread}");
        // The local cursor is not left hidden, even when the thread that shows it is the one that
        // died (it may never have been hidden at all: the hide is asynchronous).
        let deadline = Instant::now() + WAIT;
        while rig.cursor.lock().unwrap().last() == Some(&true) {
            assert!(
                Instant::now() < deadline,
                "{thread}: the cursor stayed hidden"
            );
            thread::sleep(Duration::from_millis(5));
        }
        // The backend is closed for good: no capture, and the portals are refused.
        assert!(matches!(
            rig.capture().begin(CaptureId(4), PortalId(1)),
            Err(PlatformError::NotFound | PlatformError::Locked)
        ));
        assert!(matches!(
            rig.capture().set_portals(&[hdmi_right()]),
            Err(PlatformError::Backend(_))
        ));
        // The held key's release was still replayed once the activation was gone.
        assert_eq!(rig.wait_ups(1), vec![(UP_A, false)], "{thread}");
        // Dropping a backend that died is prompt and clean.
        let dropped = Instant::now();
        rig.capture = None;
        assert!(dropped.elapsed() < Duration::from_secs(2), "{thread}");
    }
}
