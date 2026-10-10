//! Test-only: the session worker against a fake RemoteDesktop portal on a private `dbus-daemon`.
//!
//! The fake speaks the portal's real protocol (Request objects and `Response` signals, Session
//! objects and `Closed`, `ConnectToEIS` returning a socket), so these tests exercise the worker's
//! `ashpd` use, its races and its teardown without touching the desktop's own portal. Every
//! session here runs on the daemon's own address (`spawn_on`), never the session bus, and the
//! daemon's config has no service directories, so nothing can be D-Bus-activated. Tests skip with
//! a printed reason when there is no `dbus-daemon` binary.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use zbus::message::Header;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
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
            "crosspane-fake-portal-{}-{}",
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

/// What the next `Start` does.
#[derive(Clone, Copy, Debug)]
enum StartScript {
    /// Grant these device bits and a fresh restore token.
    Grant(u32),
    /// Respond with this non-success response code (1 cancelled, 2 other).
    Respond(u32),
    /// Show the dialog forever: no response until the test sends one.
    Hang,
}

/// What a `SelectDevices` call carried.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Select {
    types: Option<u32>,
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
    /// The request and session of a `Hang` start.
    pending: Option<(String, String)>,
    /// The portal's end of each `ConnectToEIS` socket.
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

fn object_path(path: String) -> fdo::Result<OwnedObjectPath> {
    OwnedObjectPath::try_from(path).map_err(|error| fdo::Error::Failed(error.to_string()))
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

fn granted(devices: u32, token: Option<String>) -> HashMap<String, Value<'static>> {
    let mut results = HashMap::new();
    results.insert("devices".to_owned(), Value::U32(devices));
    if let Some(token) = token {
        results.insert("restore_token".to_owned(), Value::from(token));
    }
    results
}

struct FakeRemoteDesktop(Arc<Fake>);

#[interface(name = "org.freedesktop.portal.RemoteDesktop")]
impl FakeRemoteDesktop {
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

    async fn select_devices(
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
            persist_mode: option_u32(&options, "persist_mode"),
            restore_token: option_str(&options, "restore_token").ok(),
        });
        respond(connection, &request, 0, HashMap::new()).await?;
        object_path(request)
    }

    async fn start(
        &self,
        session: ObjectPath<'_>,
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
                inner.scripts.pop_front().unwrap_or(StartScript::Grant(3)),
                format!("tok-{}", inner.tokens),
            )
        };
        match script {
            StartScript::Grant(devices) => {
                respond(connection, &request, 0, granted(devices, Some(token))).await?;
            }
            StartScript::Respond(code) => {
                respond(connection, &request, code, HashMap::new()).await?;
            }
            StartScript::Hang => {
                self.0.lock().pending = Some((request.clone(), session.to_string()));
            }
        }
        object_path(request)
    }

    #[zbus(name = "ConnectToEIS")]
    async fn connect_to_eis(
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
                    FakeRemoteDesktop(fake.clone()),
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

    /// The bus without any portal on it.
    fn start_empty() -> Option<Daemon> {
        Daemon::start()
    }

    fn script(&self, scripts: &[StartScript]) {
        self.fake.lock().scripts.extend(scripts.iter().copied());
    }

    /// The portal revokes `session`.
    fn emit_closed(&self, session: &str) {
        zbus::block_on(self.connection.emit_signal(
            None::<&str>,
            session,
            "org.freedesktop.portal.Session",
            "Closed",
            &HashMap::<String, Value<'_>>::new(),
        ))
        .unwrap();
    }

    fn last_session(&self) -> String {
        self.fake.lock().sessions.last().cloned().unwrap()
    }

    /// The portal drops its name, as a restarting portal does.
    fn release_name(&self) {
        assert!(zbus::block_on(self.connection.release_name(PORTAL_NAME)).unwrap());
    }

    /// The portal drops its name and takes it again at once, as a portal restart does.
    fn restart_name(&self) {
        self.release_name();
        zbus::block_on(self.connection.request_name(PORTAL_NAME)).unwrap();
    }
}

// ---- helpers for the tests -------------------------------------------------------------------

/// Records the status callbacks, in order.
#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<SessionStatus>>,
    changed: Condvar,
}

impl Recorder {
    fn callback(self: &Arc<Self>) -> StatusCallback {
        let recorder = Arc::clone(self);
        Arc::new(move |status| {
            recorder.events.lock().unwrap().push(status);
            recorder.changed.notify_all();
        })
    }

    fn events(&self) -> Vec<SessionStatus> {
        self.events.lock().unwrap().clone()
    }

    /// Wait until the recorded statuses satisfy `done`; panics with them after `WAIT`.
    fn wait_for(&self, what: &str, done: impl Fn(&[SessionStatus]) -> bool) -> Vec<SessionStatus> {
        let deadline = Instant::now() + WAIT;
        let mut events = self.events.lock().unwrap();
        while !done(&events) {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "timed out waiting for {what}: {events:?}");
            events = self.changed.wait_timeout(events, left).unwrap().0;
        }
        events.clone()
    }

    fn wait_for_status(&self, status: SessionStatus) -> Vec<SessionStatus> {
        self.wait_for(&format!("{status:?}"), |events| events.contains(&status))
    }
}

struct TokenDir(PathBuf);

impl TokenDir {
    fn new() -> TokenDir {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        TokenDir(std::env::temp_dir().join(format!(
            "crosspane-fake-portal-token-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )))
    }

    fn token_path(&self) -> PathBuf {
        self.0.join("portal.token")
    }
}

impl Drop for TokenDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn spawn_on(address: &str, tokens: &TokenDir, recorder: &Arc<Recorder>) -> RemoteDesktopSession {
    RemoteDesktopSession::spawn_on(
        RemoteDesktopConfig {
            token_path: tokens.token_path(),
        },
        recorder.callback(),
        Some(address.to_owned()),
    )
    .unwrap()
}

use SessionStatus::{Active, Closed, Denied, Pending, Unavailable};

/// Take the socket of `epoch` and check it is the portal's `index`-th socket.
fn check_eis(session: &RemoteDesktopSession, portal: &Portal, epoch: u64, index: usize) {
    let (got, fd) = session.take_eis().expect("the epoch's socket");
    assert_eq!(got, epoch);
    assert!(
        session.take_eis().is_none(),
        "the socket is handed out once"
    );
    let mut ours = UnixStream::from(fd);
    ours.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut theirs = portal.fake.lock().peers[index].try_clone().unwrap();
    theirs.write_all(b"x").unwrap();
    let mut byte = [0u8; 1];
    ours.read_exact(&mut byte).unwrap();
    assert_eq!(&byte, b"x");
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

// ---- tests -----------------------------------------------------------------------------------

#[test]
fn grants_and_hands_over_the_eis_socket_once() {
    let Some(portal) = Portal::start(2) else {
        return;
    };
    let tokens = TokenDir::new();
    let recorder = Arc::new(Recorder::default());
    let session = spawn_on(&portal.daemon.address, &tokens, &recorder);

    recorder.wait_for_status(Active { epoch: 1 });
    assert_eq!(recorder.events(), vec![Pending, Active { epoch: 1 }]);
    assert_eq!(session.status(), Active { epoch: 1 });

    // Keyboard and pointer, persisted until revoked, no token the first time.
    assert_eq!(
        portal.fake.lock().selects,
        vec![Select {
            types: Some(3),
            persist_mode: Some(2),
            restore_token: None,
        }]
    );
    // The rotated token is stored privately.
    assert_eq!(token::read(&tokens.token_path()).as_deref(), Some("tok-1"));
    assert_eq!(mode(&tokens.token_path()), 0o600);
    check_eis(&session, &portal, 1, 0);

    // Dropping the handle closes the portal session and reports it.
    drop(session);
    assert_eq!(
        recorder.events(),
        vec![Pending, Active { epoch: 1 }, Closed { epoch: 1 }]
    );
    let first = portal.fake.lock().sessions[0].clone();
    assert_eq!(portal.fake.lock().closed_by_client, vec![first]);
}

#[test]
fn restart_presents_the_stored_token_and_begins_a_new_epoch() {
    let Some(portal) = Portal::start(2) else {
        return;
    };
    let tokens = TokenDir::new();
    let recorder = Arc::new(Recorder::default());
    let session = spawn_on(&portal.daemon.address, &tokens, &recorder);
    recorder.wait_for_status(Active { epoch: 1 });

    session.restart().unwrap();
    // The old epoch's socket is gone at once, whatever the worker is doing.
    assert!(session.take_eis().is_none());
    recorder.wait_for_status(Active { epoch: 2 });

    assert_eq!(
        recorder.events(),
        vec![
            Pending,
            Active { epoch: 1 },
            Closed { epoch: 1 },
            Pending,
            Active { epoch: 2 }
        ]
    );
    let selects = portal.fake.lock().selects.clone();
    assert_eq!(selects[1].restore_token.as_deref(), Some("tok-1"));
    assert_eq!(token::read(&tokens.token_path()).as_deref(), Some("tok-2"));
    check_eis(&session, &portal, 2, 1);
    // The first epoch's portal session was closed by the restart.
    let first = portal.fake.lock().sessions[0].clone();
    assert!(portal.fake.lock().closed_by_client.contains(&first));
}

#[test]
fn a_session_the_portal_closed_stays_closed_until_restart() {
    // The portal's `Closed` signal is the user (or the desktop) stopping remote control, for
    // instance with the Stop button of GNOME's indicator. It must not be re-armed on its own.
    let Some(portal) = Portal::start(2) else {
        return;
    };
    let tokens = TokenDir::new();
    let recorder = Arc::new(Recorder::default());
    let session = spawn_on(&portal.daemon.address, &tokens, &recorder);
    recorder.wait_for_status(Active { epoch: 1 });

    portal.emit_closed(&portal.last_session());
    recorder.wait_for_status(Closed { epoch: 1 });
    assert_eq!(session.status(), Closed { epoch: 1 });
    assert!(session.take_eis().is_none());
    // Well past the delay an automatic retry would have waited: nothing happens.
    std::thread::sleep(RETRY_DELAY + Duration::from_millis(700));
    assert_eq!(
        recorder.events(),
        vec![Pending, Active { epoch: 1 }, Closed { epoch: 1 }]
    );
    assert_eq!(session.status(), Closed { epoch: 1 });
    assert_eq!(portal.fake.lock().selects.len(), 1, "no second start");
    assert_eq!(portal.fake.lock().sessions.len(), 1, "no second session");
    // The portal reported the session closed, so the worker didn't call Close on it.
    assert!(portal.fake.lock().closed_by_client.is_empty());

    // An explicit restart is the way back, with the stored token.
    session.restart().unwrap();
    recorder.wait_for_status(Active { epoch: 2 });
    assert_eq!(
        portal.fake.lock().selects[1].restore_token.as_deref(),
        Some("tok-1")
    );
    check_eis(&session, &portal, 2, 1);

    // The same again for the second epoch: still no automatic restart.
    portal.emit_closed(&portal.last_session());
    recorder.wait_for_status(Closed { epoch: 2 });
    std::thread::sleep(RETRY_DELAY + Duration::from_millis(700));
    assert_eq!(session.status(), Closed { epoch: 2 });
    assert_eq!(portal.fake.lock().selects.len(), 2);
}

#[test]
fn a_restarted_portal_is_retried_once_with_the_stored_token() {
    let Some(portal) = Portal::start(2) else {
        return;
    };
    let tokens = TokenDir::new();
    let recorder = Arc::new(Recorder::default());
    let session = spawn_on(&portal.daemon.address, &tokens, &recorder);
    recorder.wait_for_status(Active { epoch: 1 });

    // The portal process goes away and comes back, as a restart does.
    portal.restart_name();
    recorder.wait_for_status(Closed { epoch: 1 });
    assert!(session.take_eis().is_none());
    // The silent retry is epoch 2 and carries the token the first start rotated in.
    recorder.wait_for_status(Active { epoch: 2 });
    assert_eq!(
        portal.fake.lock().selects[1].restore_token.as_deref(),
        Some("tok-1")
    );
    // The portal was gone, so the worker didn't call Close on the old session.
    assert!(portal.fake.lock().closed_by_client.is_empty());
    check_eis(&session, &portal, 2, 1);

    // A second restart stays closed: the retry budget is spent.
    portal.restart_name();
    recorder.wait_for_status(Closed { epoch: 2 });
    std::thread::sleep(RETRY_DELAY + Duration::from_millis(700));
    assert_eq!(
        recorder.events(),
        vec![
            Pending,
            Active { epoch: 1 },
            Closed { epoch: 1 },
            Active { epoch: 2 },
            Closed { epoch: 2 }
        ]
    );
    assert_eq!(session.status(), Closed { epoch: 2 });
    assert_eq!(portal.fake.lock().selects.len(), 2);

    // An explicit restart is the way back, and renews the budget.
    session.restart().unwrap();
    recorder.wait_for_status(Active { epoch: 3 });
    check_eis(&session, &portal, 3, 2);
}

#[test]
fn a_denied_dialog_is_not_retried_until_restart() {
    let Some(portal) = Portal::start(2) else {
        return;
    };
    portal.script(&[StartScript::Respond(1)]);
    let tokens = TokenDir::new();
    let recorder = Arc::new(Recorder::default());
    let session = spawn_on(&portal.daemon.address, &tokens, &recorder);

    recorder.wait_for_status(Denied);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(recorder.events(), vec![Pending, Denied]);
    assert_eq!(session.status(), Denied);
    assert!(session.take_eis().is_none());
    assert_eq!(token::read(&tokens.token_path()), None);
    // The refused session was closed rather than left behind.
    assert_eq!(portal.fake.lock().closed_by_client.len(), 1);

    session.restart().unwrap();
    recorder.wait_for_status(Active { epoch: 1 });
    assert_eq!(
        recorder.events(),
        vec![Pending, Denied, Pending, Active { epoch: 1 }]
    );
}

#[test]
fn granting_less_than_keyboard_and_pointer_is_denied() {
    let Some(portal) = Portal::start(2) else {
        return;
    };
    // Keyboard only.
    portal.script(&[StartScript::Grant(1)]);
    let tokens = TokenDir::new();
    let recorder = Arc::new(Recorder::default());
    let session = spawn_on(&portal.daemon.address, &tokens, &recorder);

    recorder.wait_for_status(Denied);
    assert!(session.take_eis().is_none());
    // The partial grant's token is not kept, and its session is closed.
    assert_eq!(token::read(&tokens.token_path()), None);
    assert_eq!(portal.fake.lock().closed_by_client.len(), 1);
    assert!(
        portal.fake.lock().peers.is_empty(),
        "no EIS for a partial grant"
    );
}

#[test]
fn close_while_the_dialog_is_up_drops_the_late_answer() {
    let Some(portal) = Portal::start(2) else {
        return;
    };
    portal.script(&[StartScript::Hang]);
    let tokens = TokenDir::new();
    let recorder = Arc::new(Recorder::default());
    let session = spawn_on(&portal.daemon.address, &tokens, &recorder);

    // Wait until the dialog is up.
    let deadline = Instant::now() + WAIT;
    while portal.fake.lock().pending.is_none() {
        assert!(
            Instant::now() < deadline,
            "the start never reached the portal"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(session.status(), Pending);

    session.close();
    // Closed at once for handles; no epoch ever began.
    assert_eq!(session.status(), Closed { epoch: 0 });
    assert!(session.take_eis().is_none());
    assert!(session.restart().is_err());
    recorder.wait_for_status(Closed { epoch: 0 });
    // The abandoned dialog's session was closed on the portal.
    let pending = portal.fake.lock().pending.clone().unwrap();
    assert_eq!(portal.fake.lock().closed_by_client, vec![pending.1.clone()]);

    // The user clicks Allow after all: the answer is stale.
    let results = granted(3, Some("late".to_owned()));
    zbus::block_on(respond(&portal.connection, &pending.0, 0, results)).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    drop(session);
    assert_eq!(recorder.events(), vec![Pending, Closed { epoch: 0 }]);
    assert_eq!(token::read(&tokens.token_path()), None);
    assert!(portal.fake.lock().peers.is_empty());
}

#[test]
fn restart_while_the_dialog_is_up_starts_over() {
    let Some(portal) = Portal::start(2) else {
        return;
    };
    portal.script(&[StartScript::Hang]);
    let tokens = TokenDir::new();
    let recorder = Arc::new(Recorder::default());
    let session = spawn_on(&portal.daemon.address, &tokens, &recorder);
    let deadline = Instant::now() + WAIT;
    while portal.fake.lock().pending.is_none() {
        assert!(
            Instant::now() < deadline,
            "the start never reached the portal"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    session.restart().unwrap();
    recorder.wait_for_status(Active { epoch: 1 });
    // Still one Pending: the restart of a pending start isn't a status change.
    assert_eq!(recorder.events(), vec![Pending, Active { epoch: 1 }]);
    let pending = portal.fake.lock().pending.clone().unwrap();
    assert!(portal.fake.lock().closed_by_client.contains(&pending.1));
    check_eis(&session, &portal, 1, 0);
}

#[test]
fn the_portal_vanishing_ends_the_epoch() {
    let Some(portal) = Portal::start(2) else {
        return;
    };
    let tokens = TokenDir::new();
    let recorder = Arc::new(Recorder::default());
    let session = spawn_on(&portal.daemon.address, &tokens, &recorder);
    recorder.wait_for_status(Active { epoch: 1 });

    portal.release_name();
    recorder.wait_for_status(Closed { epoch: 1 });
    assert!(session.take_eis().is_none());
    // The silent retry finds no portal and leaves the status closed.
    std::thread::sleep(RETRY_DELAY + Duration::from_millis(700));
    assert_eq!(
        recorder.events(),
        vec![Pending, Active { epoch: 1 }, Closed { epoch: 1 }]
    );
    assert_eq!(session.status(), Closed { epoch: 1 });
    // No Close call to a portal that is gone.
    assert!(portal.fake.lock().closed_by_client.is_empty());
}

#[test]
fn a_portal_without_connect_to_eis_is_unavailable() {
    let Some(portal) = Portal::start(1) else {
        return;
    };
    let tokens = TokenDir::new();
    let recorder = Arc::new(Recorder::default());
    let session = spawn_on(&portal.daemon.address, &tokens, &recorder);
    recorder.wait_for_status(Unavailable);
    assert_eq!(recorder.events(), vec![Pending, Unavailable]);
    // No session was created, so no dialog could have been shown.
    assert!(portal.fake.lock().sessions.is_empty());
    assert_eq!(session.status(), Unavailable);
}

#[test]
fn a_bus_without_a_portal_is_unavailable_and_restartable() {
    let Some(daemon) = Portal::start_empty() else {
        return;
    };
    let tokens = TokenDir::new();
    let recorder = Arc::new(Recorder::default());
    let session = spawn_on(&daemon.address, &tokens, &recorder);
    recorder.wait_for_status(Unavailable);
    session.restart().unwrap();
    recorder.wait_for("a second Unavailable", |events| {
        events == [Pending, Unavailable, Pending, Unavailable]
    });
}

#[test]
fn an_unreachable_bus_is_unavailable_and_the_handle_still_works() {
    let tokens = TokenDir::new();
    let recorder = Arc::new(Recorder::default());
    let session = spawn_on(
        "unix:path=/nonexistent/crosspane-test-bus",
        &tokens,
        &recorder,
    );
    recorder.wait_for_status(Unavailable);
    assert_eq!(session.status(), Unavailable);
    assert!(session.take_eis().is_none());

    session.restart().unwrap();
    recorder.wait_for("a second Unavailable", |events| {
        events == [Pending, Unavailable, Pending, Unavailable]
    });

    // Close is final and idempotent; the status of a refused session stays.
    session.close();
    session.close();
    assert!(session.restart().is_err());
    assert_eq!(session.status(), Unavailable);
    let start = Instant::now();
    drop(session);
    assert!(start.elapsed() < Duration::from_secs(2), "drop is bounded");
}

#[test]
fn a_callback_may_call_back_into_the_handle() {
    // close() from inside the status callback must not deadlock the worker.
    let tokens = TokenDir::new();
    let slot: Arc<Mutex<Option<Arc<RemoteDesktopSession>>>> = Arc::new(Mutex::new(None));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let callback: StatusCallback = {
        let slot = Arc::clone(&slot);
        let seen = Arc::clone(&seen);
        Arc::new(move |status| {
            seen.lock().unwrap().push(status);
            if status == Unavailable
                && let Some(session) = slot.lock().unwrap().as_ref()
            {
                session.close();
                let _ = session.restart();
                let _ = session.take_eis();
            }
        })
    };
    let session = Arc::new(
        RemoteDesktopSession::spawn_on(
            RemoteDesktopConfig {
                token_path: tokens.token_path(),
            },
            callback,
            Some("unix:path=/nonexistent/crosspane-test-bus".to_owned()),
        )
        .unwrap(),
    );
    *slot.lock().unwrap() = Some(Arc::clone(&session));
    let deadline = Instant::now() + WAIT;
    while !matches!(session.status(), Unavailable) {
        assert!(Instant::now() < deadline, "no Unavailable");
        std::thread::sleep(Duration::from_millis(5));
    }
    // Break the cycle through the callback's slot, then drop the last handle.
    slot.lock().unwrap().take();
    drop(session);
    assert_eq!(seen.lock().unwrap().first(), Some(&Pending));
}
