//! Test-only: the capture against a fake ScreenCast portal on a private `dbus-daemon`.
//!
//! The fake speaks the portal's real protocol (Request objects and `Response` signals, Session
//! objects and `Closed`, `OpenPipeWireRemote` returning a socket), so these tests exercise the
//! worker's `ashpd` use (the options it sends, the streams it parses), its token handling, its
//! races and its teardown without touching the desktop's own portal. Every capture here runs on
//! the daemon's own address (`new_on`), never the session bus, and the daemon's config has no
//! service directories, so nothing can be D-Bus-activated. The PipeWire fd the fake returns is one
//! end of a socket pair that nobody serves: the PipeWire core connects to it and stays silent, so
//! streams can be made, ended and lost, but no format is ever negotiated and no frame arrives
//! (that part needs a real compositor). The PipeWire library is only ever given that fd: nothing
//! here connects to a PipeWire server. Tests skip with a printed reason when there is no
//! `dbus-daemon` binary.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crosspane_platform::{EventSink, FrameEvent, StreamEndReason};
use crosspane_types::color::ColorSpace;
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::euclid::point2;
use crosspane_types::geom::{DisplayGeometry, PixelSize, PointLogical, SizeMm};
use crosspane_types::id::{DisplayId, WindowId};
use serde::Serialize;
use zbus::message::Header;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Type, Value, as_value};
use zbus::{Connection, ObjectServer, fdo, interface};

use super::*;

const PORTAL_NAME: &str = "org.freedesktop.portal.Desktop";
const WAIT: Duration = Duration::from_secs(10);

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
            "crosspane-fake-screencast-{}-{}",
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
        let child = match Process::new("dbus-daemon")
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

// ---- the fake portal -------------------------------------------------------------------------

/// One stream a `Start` grants.
#[derive(Clone, Copy, Debug)]
struct Granted {
    node: u32,
    position: (i32, i32),
    size: (i32, i32),
}

const MONITOR: Granted = Granted {
    node: 42,
    position: (0, 0),
    size: (1920, 1080),
};

/// What the next `Start` does.
#[derive(Clone, Debug)]
enum StartScript {
    /// Grant these streams and a fresh restore token.
    Grant(Vec<Granted>),
    /// Respond with this non-success response code (1 cancelled, 2 other).
    Respond(u32),
}

/// What a `SelectSources` call carried.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Select {
    types: Option<u32>,
    multiple: Option<bool>,
    cursor_mode: Option<u32>,
    persist_mode: Option<u32>,
    restore_token: Option<String>,
}

#[derive(Default)]
struct Inner {
    version: u32,
    scripts: VecDeque<StartScript>,
    selects: Vec<Select>,
    sessions: Vec<String>,
    closed_by_client: Vec<String>,
    /// The portal's end of each `OpenPipeWireRemote` socket.
    peers: Vec<UnixStream>,
    tokens: u32,
}

struct Fake {
    inner: Mutex<Inner>,
}

impl Fake {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap()
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

fn option_bool(options: &HashMap<String, OwnedValue>, key: &str) -> Option<bool> {
    options
        .get(key)
        .and_then(|value| value.downcast_ref::<bool>().ok())
}

fn object_path(path: String) -> fdo::Result<OwnedObjectPath> {
    OwnedObjectPath::try_from(path).map_err(|error| fdo::Error::Failed(error.to_string()))
}

async fn respond<R: Serialize + Type>(
    connection: &Connection,
    request: &str,
    code: u32,
    results: R,
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

/// The results of a granted `Start`, shaped as the portal's `a{sv}` (the same shape `ashpd`
/// reads).
#[derive(Serialize, Type)]
#[zvariant(signature = "dict")]
struct GrantedResults {
    #[serde(with = "as_value")]
    streams: Vec<(u32, StreamProps)>,
    #[serde(with = "as_value")]
    restore_token: String,
}

#[derive(Serialize, Type)]
#[zvariant(signature = "dict")]
struct StreamProps {
    #[serde(with = "as_value")]
    position: (i32, i32),
    #[serde(with = "as_value")]
    size: (i32, i32),
    #[serde(with = "as_value")]
    source_type: u32,
}

struct FakeScreenCast(Arc<Fake>);

#[interface(name = "org.freedesktop.portal.ScreenCast")]
impl FakeScreenCast {
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        self.0.lock().version
    }

    async fn create_session(
        &self,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
        #[zbus(object_server)] server: &ObjectServer,
    ) -> fdo::Result<OwnedObjectPath> {
        let sender = sender_segment(&header);
        let request = format!(
            "/org/freedesktop/portal/desktop/request/{sender}/{}",
            option_str(&options, "handle_token")?
        );
        let session = format!(
            "/org/freedesktop/portal/desktop/session/{sender}/{}",
            option_str(&options, "session_handle_token")?
        );
        server
            .at(
                session.as_str(),
                FakeSession(self.0.clone(), session.clone()),
            )
            .await?;
        self.0.lock().sessions.push(session.clone());
        let mut results = HashMap::new();
        results.insert("session_handle".to_owned(), Value::from(session));
        respond(connection, &request, 0, results).await?;
        object_path(request)
    }

    async fn select_sources(
        &self,
        _session: ObjectPath<'_>,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<OwnedObjectPath> {
        let request = format!(
            "/org/freedesktop/portal/desktop/request/{}/{}",
            sender_segment(&header),
            option_str(&options, "handle_token")?
        );
        self.0.lock().selects.push(Select {
            types: option_u32(&options, "types"),
            multiple: option_bool(&options, "multiple"),
            cursor_mode: option_u32(&options, "cursor_mode"),
            persist_mode: option_u32(&options, "persist_mode"),
            restore_token: option_str(&options, "restore_token").ok(),
        });
        respond(connection, &request, 0, HashMap::<String, Value<'_>>::new()).await?;
        object_path(request)
    }

    async fn start(
        &self,
        _session: ObjectPath<'_>,
        _parent_window: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<OwnedObjectPath> {
        let request = format!(
            "/org/freedesktop/portal/desktop/request/{}/{}",
            sender_segment(&header),
            option_str(&options, "handle_token")?
        );
        let (script, token) = {
            let mut inner = self.0.lock();
            inner.tokens += 1;
            (
                inner
                    .scripts
                    .pop_front()
                    .unwrap_or(StartScript::Grant(vec![MONITOR])),
                format!("tok-{}", inner.tokens),
            )
        };
        match script {
            StartScript::Grant(streams) => {
                let results = GrantedResults {
                    streams: streams
                        .iter()
                        .map(|g| {
                            (
                                g.node,
                                StreamProps {
                                    position: g.position,
                                    size: g.size,
                                    source_type: 1,
                                },
                            )
                        })
                        .collect(),
                    restore_token: token,
                };
                respond(connection, &request, 0, results).await?;
            }
            StartScript::Respond(code) => {
                respond(
                    connection,
                    &request,
                    code,
                    HashMap::<String, Value<'_>>::new(),
                )
                .await?;
            }
        }
        object_path(request)
    }

    #[zbus(name = "OpenPipeWireRemote")]
    async fn open_pipe_wire_remote(
        &self,
        _session: ObjectPath<'_>,
        _options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<zbus::zvariant::OwnedFd> {
        let (ours, theirs) = UnixStream::pair().map_err(|e| fdo::Error::Failed(e.to_string()))?;
        self.0.lock().peers.push(ours);
        Ok(std::os::fd::OwnedFd::from(theirs).into())
    }
}

struct FakeSession(Arc<Fake>, String);

#[interface(name = "org.freedesktop.portal.Session")]
impl FakeSession {
    async fn close(&self) -> fdo::Result<()> {
        self.0.lock().closed_by_client.push(self.1.clone());
        Ok(())
    }
}

/// The fake portal, its bus and its connection.
struct Portal {
    daemon: Daemon,
    connection: Connection,
    fake: Arc<Fake>,
}

impl Portal {
    /// A portal that owns the portal name, or `None` when there is no `dbus-daemon`.
    fn start(version: u32) -> Option<Portal> {
        let daemon = Daemon::start()?;
        let fake = Arc::new(Fake {
            inner: Mutex::new(Inner {
                version,
                ..Inner::default()
            }),
        });
        let connection = zbus::block_on(async {
            zbus::connection::Builder::address(daemon.address.as_str())?
                .serve_at(
                    "/org/freedesktop/portal/desktop",
                    FakeScreenCast(fake.clone()),
                )?
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

    fn script(&self, scripts: &[StartScript]) {
        self.fake.lock().scripts.extend(scripts.iter().cloned());
    }

    /// The portal revokes the last session (the user pressed Stop).
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

    fn selects(&self) -> Vec<Select> {
        self.fake.lock().selects.clone()
    }

    fn sessions(&self) -> usize {
        self.fake.lock().sessions.len()
    }
}

// ---- helpers for the tests -------------------------------------------------------------------

/// Records a stream's events.
#[derive(Default)]
struct Events(Mutex<Vec<FrameEvent>>);

impl EventSink<FrameEvent> for Events {
    fn send(&self, event: FrameEvent) {
        self.0.lock().unwrap().push(event);
    }
}

impl Events {
    /// The end reasons seen so far, in order.
    fn ended(&self) -> Vec<(StreamId, StreamEndReason)> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                FrameEvent::Ended { stream, reason } => Some((*stream, *reason)),
                _ => None,
            })
            .collect()
    }
}

struct TokenDir(PathBuf);

impl TokenDir {
    fn new() -> TokenDir {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        TokenDir(std::env::temp_dir().join(format!(
            "crosspane-fake-screencast-token-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )))
    }

    fn token_path(&self) -> PathBuf {
        self.0.join("portal-screencast.token")
    }
}

impl Drop for TokenDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn gate() -> Arc<IoGate> {
    let gate = IoGate::new();
    gate.set_session_permits(true);
    gate.set_engine_permits(true);
    gate
}

/// One 1920x1080 display at the origin with id 1: the geometry of `MONITOR`.
fn displays() -> DisplaysFn {
    Arc::new(|| {
        vec![DisplayInfo {
            id: DisplayId(1),
            name: "DP-1".to_owned(),
            geometry: DisplayGeometry {
                physical_size: SizeMm::new(600.0, 340.0),
                pixel_size: PixelSize::new(1920, 1080),
                scale: 1.0,
                logical_origin: PointLogical::new(0.0, 0.0),
            },
            refresh_millihz: 60_000,
            color_space: ColorSpace::Srgb,
            hdr: false,
        }]
    })
}

fn open(portal: &Portal, tokens: &TokenDir, gate: &Arc<IoGate>) -> PortalScreenCast {
    PortalScreenCast::new_on(
        Arc::clone(gate),
        ScreenCastConfig {
            token_path: tokens.token_path(),
        },
        displays(),
        Some(portal.daemon.address.clone()),
    )
    .unwrap()
}

/// Poll `condition` until it holds; panics with `what` after `WAIT`.
fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn start_display(
    capture: &mut PortalScreenCast,
    events: &Arc<Events>,
) -> Result<StreamId, PlatformError> {
    capture.start(
        CaptureTarget::Display(DisplayId(1)),
        None,
        30,
        Arc::clone(events) as Arc<dyn EventSink<FrameEvent>>,
    )
}

// ---- tests -----------------------------------------------------------------------------------

#[test]
fn asks_for_every_monitor_hidden_cursor_and_a_persistent_token() {
    let Some(portal) = Portal::start(4) else {
        return;
    };
    let tokens = TokenDir::new();
    let started = Instant::now();
    let capture = open(&portal, &tokens, &gate());
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "construction does not wait for consent"
    );
    wait_for("an active session", || capture.is_live());
    // SourceType::Monitor = 1, CursorMode::Hidden = 1, PersistMode::ExplicitlyRevoked = 2.
    assert_eq!(
        portal.selects(),
        vec![Select {
            types: Some(1),
            multiple: Some(true),
            cursor_mode: Some(1),
            persist_mode: Some(2),
            restore_token: None,
        }]
    );
    // The rotated token is stored for the next start, private to the user.
    let stored = tokens.token_path();
    assert_eq!(fs::read_to_string(&stored).unwrap(), "tok-1");
    assert_eq!(mode(&stored), 0o600);
    // One PipeWire remote was opened for the session.
    assert_eq!(portal.fake.lock().peers.len(), 1);
}

#[test]
fn start_refuses_what_it_cannot_capture() {
    let Some(portal) = Portal::start(4) else {
        return;
    };
    let tokens = TokenDir::new();
    let gate = gate();
    let mut capture = open(&portal, &tokens, &gate);
    let events = Arc::new(Events::default());
    wait_for("an active session", || capture.is_live());

    let sink = || Arc::clone(&events) as Arc<dyn EventSink<FrameEvent>>;
    assert!(matches!(
        capture.start(CaptureTarget::Window(WindowId(1)), None, 30, sink()),
        Err(PlatformError::Unsupported(_))
    ));
    assert!(matches!(
        capture.start(CaptureTarget::Display(DisplayId(2)), None, 30, sink()),
        Err(PlatformError::NotFound)
    ));
    assert!(matches!(
        capture.start(CaptureTarget::Display(DisplayId(1)), None, 0, sink()),
        Err(PlatformError::Backend(_))
    ));
    let empty = PixelRect::new(point2(5, 5), point2(5, 9));
    assert!(matches!(
        capture.start(
            CaptureTarget::Display(DisplayId(1)),
            Some(empty),
            30,
            sink()
        ),
        Err(PlatformError::Backend(_))
    ));
    gate.set_engine_permits(false);
    assert!(matches!(
        start_display(&mut capture, &events),
        Err(PlatformError::Locked)
    ));
    assert!(events.ended().is_empty());
}

#[test]
fn a_stream_starts_stops_and_stops_again() {
    let Some(portal) = Portal::start(4) else {
        return;
    };
    let tokens = TokenDir::new();
    let mut capture = open(&portal, &tokens, &gate());
    let events = Arc::new(Events::default());
    wait_for("an active session", || capture.is_live());

    let first = start_display(&mut capture, &events).unwrap();
    let second = start_display(&mut capture, &events).unwrap();
    assert_ne!(first, second);
    capture.stop(first).unwrap();
    assert_eq!(events.ended(), vec![(first, StreamEndReason::Requested)]);
    // Stopping again, or an unknown stream, is fine and says nothing more.
    capture.stop(first).unwrap();
    capture.stop(StreamId(99)).unwrap();
    assert_eq!(events.ended().len(), 1);
    assert!(matches!(
        capture.set_crop(first, None),
        Err(PlatformError::NotFound)
    ));
    capture.set_crop(second, None).unwrap();
    // Dropping the capture ends the stream that is left, as requested.
    drop(capture);
    assert_eq!(
        events.ended(),
        vec![
            (first, StreamEndReason::Requested),
            (second, StreamEndReason::Requested)
        ]
    );
    // And closed the portal session.
    assert_eq!(portal.fake.lock().closed_by_client.len(), 1);
}

#[test]
fn stopping_the_share_ends_the_streams_blocked_and_stays_closed() {
    let Some(portal) = Portal::start(4) else {
        return;
    };
    let tokens = TokenDir::new();
    let mut capture = open(&portal, &tokens, &gate());
    let events = Arc::new(Events::default());
    wait_for("an active session", || capture.is_live());
    let stream = start_display(&mut capture, &events).unwrap();

    portal.emit_closed();
    wait_for("the stream to end", || !events.ended().is_empty());
    assert_eq!(events.ended(), vec![(stream, StreamEndReason::Blocked)]);
    assert!(!capture.is_live());
    assert!(matches!(
        start_display(&mut capture, &events),
        Err(PlatformError::InteractionRequired)
    ));
    // The user's stop is not undone behind their back: no second session.
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(portal.sessions(), 1);
    assert!(!capture.is_live());

    // Asking again uses the stored token, silently, and starts a new epoch.
    capture.restart().unwrap();
    wait_for("a second session", || capture.is_live());
    let selects = portal.selects();
    assert_eq!(selects.len(), 2);
    assert_eq!(selects[1].restore_token.as_deref(), Some("tok-1"));
    assert_eq!(fs::read_to_string(tokens.token_path()).unwrap(), "tok-2");
    start_display(&mut capture, &events).unwrap();
}

#[test]
fn a_no_is_not_retried_until_restart() {
    let Some(portal) = Portal::start(4) else {
        return;
    };
    portal.script(&[StartScript::Respond(1)]);
    let tokens = TokenDir::new();
    let mut capture = open(&portal, &tokens, &gate());
    let events = Arc::new(Events::default());
    wait_for("the refusal", || {
        matches!(
            start_display(&mut capture, &events),
            Err(PlatformError::PermissionDenied(_))
        )
    });
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(portal.sessions(), 1, "a refusal is final until restart");
    assert!(!tokens.token_path().exists());
    capture.restart().unwrap();
    wait_for("a granted session", || capture.is_live());
    assert_eq!(portal.sessions(), 2);
}

#[test]
fn monitors_the_user_did_not_share_are_unmapped() {
    let Some(portal) = Portal::start(4) else {
        return;
    };
    // The desktop grants a monitor that is not the one we ask for.
    portal.script(&[StartScript::Grant(vec![Granted {
        node: 7,
        position: (1920, 0),
        size: (1920, 1080),
    }])]);
    let tokens = TokenDir::new();
    let mut capture = open(&portal, &tokens, &gate());
    let events = Arc::new(Events::default());
    wait_for("a session", || {
        !matches!(
            start_display(&mut capture, &events),
            Err(PlatformError::InteractionRequired)
        )
    });
    assert!(matches!(
        start_display(&mut capture, &events),
        Err(PlatformError::NotFound)
    ));
    assert!(!capture.is_live(), "no mapped stream, not live");
}

#[test]
fn a_lost_pipewire_connection_ends_the_streams_and_retries_once() {
    let Some(portal) = Portal::start(4) else {
        return;
    };
    let tokens = TokenDir::new();
    let mut capture = open(&portal, &tokens, &gate());
    let events = Arc::new(Events::default());
    wait_for("an active session", || capture.is_live());
    let stream = start_display(&mut capture, &events).unwrap();

    // The server end of the PipeWire socket goes away.
    portal.fake.lock().peers.clear();
    wait_for("the stream to end", || !events.ended().is_empty());
    assert_eq!(events.ended(), vec![(stream, StreamEndReason::Failed)]);
    // One silent retry, with the stored token: a second session and a second remote.
    wait_for("the retried session", || {
        portal.sessions() == 2 && capture.is_live()
    });
    assert_eq!(portal.selects()[1].restore_token.as_deref(), Some("tok-1"));
    start_display(&mut capture, &events).unwrap();
}

#[test]
fn a_gate_that_closes_ends_the_streams_blocked() {
    let Some(portal) = Portal::start(4) else {
        return;
    };
    let tokens = TokenDir::new();
    let gate = gate();
    let mut capture = open(&portal, &tokens, &gate);
    let events = Arc::new(Events::default());
    wait_for("an active session", || capture.is_live());
    let stream = start_display(&mut capture, &events).unwrap();
    gate.set_engine_permits(false);
    wait_for("the stream to end", || !events.ended().is_empty());
    assert_eq!(events.ended(), vec![(stream, StreamEndReason::Blocked)]);
    // The session itself is untouched: reopening the gate allows new streams.
    gate.set_engine_permits(true);
    start_display(&mut capture, &events).unwrap();
}
