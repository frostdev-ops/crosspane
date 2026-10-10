#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crosspane_installer::{
    agent_contract::*,
    platform::linux::{native_io::*, transport::*},
};
use crosspane_types::id::NodeId;
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::SocketAddr,
    os::unix::{
        fs::{PermissionsExt, symlink},
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

static ROOT_ID: AtomicU64 = AtomicU64::new(0);
const START: &[u8] = b"Fri Oct  2 12:00:00 2026\n";
// Literal producer fields. Only the selected scratch instance's native coordinates vary.
const HEALTH: &str = r#"{"ok":true,"result":{"controlling":null,"controlled_by":null,"projections":[],
"displays":[],"peers":[],"layout":[],"installer":{"schema_version":1,
"build":{"version":"0.0.0","features":["video"]},"instance":{"id":9,"pid":4242,"uid":1000,
"exe":"/home/u/.local/bin/crosspane-agent","runtime_dir":"/run/user/1000/crosspane","started_unix_ms":1790942400000},
"config_revision":"9f86d081884c7d65","node":"1111111111111111111111111111111111111111111111111111111111111111",
"recovery_pending":0,"startup_recovery":"nothing_parked","gate":{"open":true,"session":"unlocked","active":true,"armed":true,"panic":false},
"epochs":{"gate":1,"grants":1,"layout":1,"backends":1},"backends":[
{"name":"capture","state":"ready","reason":null},{"name":"keys","state":"ready","reason":null},
{"name":"pointer","state":"ready","reason":null},{"name":"overlay","state":"ready","reason":null},
{"name":"hotkeys","state":"ready","reason":null},{"name":"keystore","state":"ready","reason":null},
{"name":"windows","state":"ready","reason":null},{"name":"parking","state":"ready","reason":null},
{"name":"frames","state":"ready","reason":null},{"name":"tray","state":"ready","reason":null},
{"name":"links","state":"ready","reason":null},{"name":"gpu","state":"ready","reason":null},
{"name":"home","state":"ready","reason":null},{"name":"audio","state":"ready","reason":null},
{"name":"discovery","state":"ready","reason":null}],"keystore":"os_store","permissions":[],
"discovery":{"enabled":true,"running":true,"candidates":0,"error":null},"tray":{"created":true},
"audio":{"enabled":false,"active_peers":[],"frames_sent":0,"frames_played":0},"settings_opened":0,"peers":[]}}}"#;
#[derive(Default)]
struct Runner {
    calls: AtomicU64,
    block: AtomicBool,
    entered: AtomicBool,
    hook: Mutex<Option<QueryHook>>,
}
type QueryHook = (u64, Box<dyn FnOnce() + Send>);
impl CommandRunner for Runner {
    fn run(
        &self,
        command: &CommandSpec,
        deadline: &Deadline,
    ) -> Result<CommandOutput, NativeError> {
        let count = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
        let hook = { self.hook.lock().unwrap().take() };
        if let Some((at, hook)) = hook {
            if at == count {
                hook();
            } else {
                *self.hook.lock().unwrap() = Some((at, hook));
            }
        }
        self.entered.store(true, Ordering::Release);
        while self.block.load(Ordering::Acquire) {
            deadline.check()?;
            thread::sleep(Duration::from_millis(1));
        }
        deadline.check()?;
        assert_eq!(command.executable(), std::path::Path::new("/bin/ps"));
        let stdout = if command.argv()[1] == "lstart=" {
            START.to_vec()
        } else {
            b"crosspane-agent\n".to_vec()
        };
        Ok(CommandOutput {
            code: Some(0),
            stdout,
            stderr: Vec::new(),
        })
    }
}
struct Probe {
    root: PathBuf,
    generation: AtomicU64,
    foreign_uid: AtomicBool,
}
impl ProcessProbe for Probe {
    fn snapshot(&self, _: u32, deadline: &Deadline) -> Result<ProcessFacts, NativeError> {
        deadline.check()?;
        Ok(ProcessFacts {
            uid: rustix::process::geteuid().as_raw()
                + u32::from(self.foreign_uid.load(Ordering::Acquire)),
            executable: self.root.join(".local/bin/crosspane-agent"),
            generation: self.generation.load(Ordering::Acquire),
        })
    }
}
struct Fixture {
    root: PathBuf,
    io: Arc<LinuxNativeIo>,
    runner: Arc<Runner>,
    probe: Arc<Probe>,
}
impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(format!(
            "/tmp/cp47t-{}-{}",
            std::process::id(),
            ROOT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let runner = Arc::new(Runner::default());
        let probe = Arc::new(Probe {
            root: root.clone(),
            generation: AtomicU64::new(77),
            foreign_uid: AtomicBool::new(false),
        });
        let io = Arc::new(LinuxNativeIo::scratch(&root, runner.clone(), probe.clone()).unwrap());
        let fixture = Self {
            root,
            io,
            runner,
            probe,
        };
        let proof = fixture.proof();
        fixture
            .io
            .create_private_dir(&proof, fixture.io.target().runtime_dir())
            .unwrap();
        fixture
            .io
            .create_private_dir(&proof, &fixture.root.join(".local/bin"))
            .unwrap();
        fs::write(fixture.io.target().agent_path(), b"inert test fixture").unwrap();
        fs::set_permissions(
            fixture.io.target().agent_path(),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fixture.write_bootstrap(9, std::process::id());
        fixture
    }
    fn proof(&self) -> SupportProof {
        self.io
            .scratch_support(SupportObservations {
                uid: self.io.target().paths().uid,
                desktop: crosspane_installer::platform::linux::detect::Desktop::Hyprland,
                architecture: std::env::consts::ARCH.into(),
                arch_based: true,
                compositor_version: [0, 56, 0],
                protocols_ready: true,
                runtime_libraries_ready: true,
                compositor_managed: true,
                graphical_target_active: true,
                graphical_sessions: 1,
                session_id: "scratch".into(),
                session_type: "wayland".into(),
                seat: "seat0".into(),
                active: true,
            })
            .unwrap()
    }
    fn write_bootstrap(&self, id: u64, pid: u32) {
        let bytes = serde_json::to_vec(&json!({"schema_version":1,"instance_id":id,"pid":pid,"started_unix_ms":parse_ps_start(START).unwrap(),
            "phase":"ready","phase_seq":2,"keystore":"os_store","reason":null,"runtime_dir":self.io.target().runtime_dir()})).unwrap();
        self.io
            .atomic_write(
                &self.proof(),
                &self.io.target().runtime_dir().join("bootstrap.json"),
                &bytes,
            )
            .unwrap();
    }
    fn health(&self) -> Vec<u8> {
        let mut value: Value = serde_json::from_str(HEALTH).unwrap();
        value["result"]["installer"]["instance"] = json!({"id":9,"pid":std::process::id(),"uid":self.io.target().paths().uid,
            "exe":self.io.target().agent_path(),"runtime_dir":self.io.target().runtime_dir(),"started_unix_ms":parse_ps_start(START).unwrap()});
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        bytes
    }
    fn listener(&self) -> UnixListener {
        let listener = UnixListener::bind(self.io.target().socket_path()).unwrap();
        fs::set_permissions(
            self.io.target().socket_path(),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        listener
    }
    fn port(&self, proof: bool, clock: CallerClock) -> LinuxAgentPort {
        LinuxAgentPort::new(self.io.clone(), proof.then(|| self.proof()), clock).unwrap()
    }
    fn server(&self, actions: Vec<Action>) -> thread::JoinHandle<Vec<Vec<u8>>> {
        let listener = self.listener();
        thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let mut requests = Vec::new();
            for action in actions {
                let until = Instant::now() + Duration::from_secs(3);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < until);
                            thread::sleep(Duration::from_millis(1));
                        }
                        Err(e) => panic!("private accept failed: {e}"),
                    }
                };
                if let Action::NoRead(ms) = action {
                    thread::sleep(Duration::from_millis(ms));
                    requests.push(Vec::new());
                    continue;
                }
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut request = Vec::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_until(b'\n', &mut request)
                    .unwrap();
                requests.push(request);
                match action {
                    Action::Bytes(bytes) => {
                        let _ = stream.write_all(&bytes);
                    }
                    Action::Stall(ms) => thread::sleep(Duration::from_millis(ms)),
                    Action::Trailing => {
                        stream
                            .write_all(b"{\"ok\":true,\"result\":null}\n")
                            .unwrap();
                        thread::sleep(Duration::from_millis(10));
                        let _ = stream.write_all(b"{}");
                    }
                    Action::Reuse(probe, bytes) => {
                        let _ = stream.write_all(&bytes);
                        probe.generation.store(78, Ordering::Release);
                    }
                    Action::NoRead(_) => unreachable!(),
                }
            }
            requests
        })
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}
enum Action {
    Bytes(Vec<u8>),
    Stall(u64),
    NoRead(u64),
    Trailing,
    Reuse(Arc<Probe>, Vec<u8>),
}
fn clock() -> CallerClock {
    Arc::new(|| 123)
}
fn call(id: u64, request: InstallerRequest, timeout_ms: u64) -> AgentCall {
    AgentCall {
        id,
        request,
        timeout_ms,
    }
}
fn reply(port: &mut LinuxAgentPort) -> AgentReply {
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        let mut values = port.poll();
        if !values.is_empty() {
            assert_eq!(values.len(), 1);
            return values.remove(0);
        }
        assert!(Instant::now() < until, "bounded port did not reply");
        thread::sleep(Duration::from_millis(1));
    }
}
fn accept_bounded(listener: &UnixListener) -> UnixStream {
    listener.set_nonblocking(true).unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < until, "private bounded accept expired");
                thread::sleep(Duration::from_millis(1));
            }
            Err(e) => panic!("private accept failed: {e}"),
        }
    }
}
fn read_request(stream: &UnixStream) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut bytes = Vec::new();
    BufReader::new(stream.try_clone().unwrap())
        .read_until(b'\n', &mut bytes)
        .unwrap();
    bytes
}
fn revoke(proof: &SupportProof, io: &LinuxNativeIo) {
    assert_eq!(
        proof.revalidate(
            io,
            &SupportObservations {
                uid: io.target().paths().uid,
                desktop: crosspane_installer::platform::linux::detect::Desktop::Hyprland,
                architecture: std::env::consts::ARCH.into(),
                arch_based: true,
                compositor_version: [0, 56, 0],
                protocols_ready: true,
                runtime_libraries_ready: true,
                compositor_managed: true,
                graphical_target_active: true,
                graphical_sessions: 1,
                session_id: "scratch".into(),
                session_type: "wayland".into(),
                seat: "seat0".into(),
                active: false
            }
        ),
        Err(NativeError::Unsupported)
    );
}

#[test]
fn exact_codec_round_trip_receipt_clock_source_and_reply_correlation() {
    let fixture = Fixture::new();
    let status = fixture.health();
    assert!(matches!(
        parse_status(&status, AgentPlatform::Linux),
        Ok(StatusAdmission::Supported(_))
    ));
    let server = fixture.server(vec![Action::Bytes(status)]);
    let stamp = Arc::new(AtomicU64::new(0));
    let captured = stamp.clone();
    let (received, receipt) = mpsc::channel();
    let mut port = fixture.port(
        false,
        Arc::new(move || {
            let n = captured.fetch_add(1, Ordering::Relaxed);
            if n == 1 {
                received.send(()).unwrap();
            }
            n
        }),
    );
    port.submit(call(17, InstallerRequest::Status, 1000))
        .unwrap();
    server.join().unwrap();
    receipt.recv_timeout(Duration::from_secs(1)).unwrap();
    stamp.store(999, Ordering::Release);
    let response = reply(&mut port);
    assert_eq!(response.id, 17);
    assert_eq!(response.observed_at_ms, 1);
    assert_eq!(response.source, ObservationSource::Demo);
    assert!(matches!(
        response.result,
        Ok(DecodedReply::Status(StatusAdmission::Supported(_)))
    ));
}
#[test]
fn refusals_keep_exact_payload_and_mutations_are_never_resent() {
    let fixture = Fixture::new();
    let server = fixture.server(vec![
        Action::Bytes(fixture.health()),
        Action::Bytes(b"{\"ok\":false,\"result\":null,\"error\":\"revision_conflict\"}\n".to_vec()),
    ]);
    let mut port = fixture.port(true, clock());
    let request = InstallerRequest::SettingsUpdate {
        expected_revision: "9f86d081884c7d65".into(),
        mac_virtual_display: false,
    };
    port.submit(call(1, request.clone(), 1000)).unwrap();
    let result = reply(&mut port);
    assert_eq!(
        result.result,
        Err(CallFailure::Refused(AgentRefusal::RevisionConflict))
    );
    let requests = server.join().unwrap();
    assert_eq!(
        requests,
        [
            encode_request(&InstallerRequest::Status).unwrap(),
            encode_request(&request).unwrap()
        ]
    );
    assert!(port.poll().is_empty());
}
#[test]
fn malformed_response_bounds_trailing_bytes_and_incomplete_lines_are_refused() {
    let mut huge = vec![b' '; MAX_RESPONSE_BYTES + 1];
    huge.push(b'\n');
    for action in [
        Action::Bytes(huge),
        Action::Bytes(b"{}".to_vec()),
        Action::Trailing,
        Action::Bytes(b"{\"ok\":true,\"result\":{}}\n{}\n".to_vec()),
    ] {
        let fixture = Fixture::new();
        let server = fixture.server(vec![action]);
        let mut port = fixture.port(false, clock());
        port.submit(call(1, InstallerRequest::Status, 1000))
            .unwrap();
        assert_eq!(reply(&mut port).result, Err(CallFailure::InvalidResponse));
        server.join().unwrap();
        port.shutdown();
    }
}
#[test]
fn absent_socket_foreign_identity_directory_modes_and_symlinks_are_unavailable() {
    let fixture = Fixture::new();
    let mut port = fixture.port(false, clock());
    port.submit(call(1, InstallerRequest::Status, 1000))
        .unwrap();
    assert_eq!(reply(&mut port).result, Err(CallFailure::Unavailable));
    let listener = fixture.listener();
    fs::set_permissions(
        fixture.io.target().socket_path(),
        fs::Permissions::from_mode(0o666),
    )
    .unwrap();
    port.submit(call(2, InstallerRequest::Status, 1000))
        .unwrap();
    assert_eq!(reply(&mut port).result, Err(CallFailure::Unavailable));
    fs::set_permissions(
        fixture.io.target().socket_path(),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    fs::set_permissions(
        fixture.io.target().runtime_dir(),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    port.submit(call(3, InstallerRequest::Status, 1000))
        .unwrap();
    assert_eq!(reply(&mut port).result, Err(CallFailure::Unavailable));
    fs::set_permissions(
        fixture.io.target().runtime_dir(),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    fixture.probe.foreign_uid.store(true, Ordering::Release);
    port.submit(call(4, InstallerRequest::Status, 1000))
        .unwrap();
    assert_eq!(reply(&mut port).result, Err(CallFailure::Unavailable));
    fixture.probe.foreign_uid.store(false, Ordering::Release);
    drop(listener);
    fs::remove_file(fixture.io.target().socket_path()).unwrap();
    let foreign = UnixListener::bind(fixture.root.join("foreign.sock")).unwrap();
    foreign.set_nonblocking(true).unwrap();
    symlink(
        fixture.root.join("foreign.sock"),
        fixture.io.target().socket_path(),
    )
    .unwrap();
    port.submit(call(5, InstallerRequest::Status, 1000))
        .unwrap();
    assert_eq!(reply(&mut port).result, Err(CallFailure::Unavailable));
    assert!(matches!(foreign.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
}
#[test]
fn connected_peer_pid_and_wire_executable_runtime_and_instance_are_revalidated() {
    let fixture = Fixture::new();
    fixture.write_bootstrap(9, std::process::id() + 1);
    let listener = fixture.listener();
    let mut port = fixture.port(false, clock());
    port.submit(call(1, InstallerRequest::Status, 1000))
        .unwrap();
    assert_eq!(reply(&mut port).result, Err(CallFailure::Unavailable));
    drop(listener);
    port.shutdown();
    for field in ["exe", "uid", "runtime_dir", "id", "started_unix_ms"] {
        let fixture = Fixture::new();
        let mut v: Value = serde_json::from_slice(&fixture.health()).unwrap();
        v["result"]["installer"]["instance"][field] = if matches!(field, "exe" | "runtime_dir") {
            json!("/foreign")
        } else {
            json!(7)
        };
        let mut bytes = serde_json::to_vec(&v).unwrap();
        bytes.push(b'\n');
        let server = fixture.server(vec![Action::Bytes(bytes)]);
        let mut port = fixture.port(false, clock());
        port.submit(call(1, InstallerRequest::Status, 1000))
            .unwrap();
        assert_eq!(reply(&mut port).result, Err(CallFailure::Unavailable));
        server.join().unwrap();
    }
}
#[test]
fn bounded_connect_backlog_read_write_stalls_and_mutation_timeouts() {
    let fixture = Fixture::new();
    let listener = fixture.listener();
    rustix::net::listen(&listener, 0).unwrap();
    let _occupied = UnixStream::connect(fixture.io.target().socket_path()).unwrap();
    let mut port = fixture.port(false, clock());
    port.submit(call(1, InstallerRequest::Status, 40)).unwrap();
    assert_eq!(
        reply(&mut port).result,
        Err(CallFailure::TimeoutOutcomeUnknown)
    );
    port.shutdown();
    drop(listener);
    let fixture = Fixture::new();
    let server = fixture.server(vec![Action::Stall(100)]);
    let mut port = fixture.port(false, clock());
    port.submit(call(1, InstallerRequest::Status, 40)).unwrap();
    assert_eq!(
        reply(&mut port).result,
        Err(CallFailure::TimeoutOutcomeUnknown)
    );
    assert_eq!(server.join().unwrap().len(), 1);
    let fixture = Fixture::new();
    let server = fixture.server(vec![Action::Bytes(fixture.health()), Action::NoRead(150)]);
    let mut port = fixture.port(true, clock());
    let placements = (0..128)
        .map(|n| Placement {
            node: NodeId([n; 32]),
            display: u32::from(n),
            origin_mm: [f64::MAX, -f64::MAX],
        })
        .collect();
    port.submit(call(1, InstallerRequest::Place { placements }, 70))
        .unwrap();
    assert_eq!(
        reply(&mut port).result,
        Err(CallFailure::TimeoutOutcomeUnknown)
    );
    assert_eq!(server.join().unwrap().len(), 2);
    thread::sleep(Duration::from_millis(10));
    assert!(port.poll().is_empty());
}
#[test]
fn queue_full_invalid_call_and_shutdown_do_not_start_additional_io() {
    let fixture = Fixture::new();
    fixture.runner.block.store(true, Ordering::Release);
    let mut port = fixture.port(false, clock());
    for id in 1..=32 {
        port.submit(call(id, InstallerRequest::Status, 1000))
            .unwrap();
    }
    let until = Instant::now() + Duration::from_secs(1);
    while !fixture.runner.entered.load(Ordering::Acquire) {
        assert!(Instant::now() < until);
        thread::yield_now();
    }
    assert_eq!(
        port.submit(call(33, InstallerRequest::Status, 1000)),
        Err(CallFailure::QueueFull)
    );
    assert_eq!(
        port.submit(call(33, InstallerRequest::Status, 5001)),
        Err(CallFailure::InvalidCall(ContractError::InvalidDeadline))
    );
    assert_eq!(
        port.submit(call(32, InstallerRequest::Status, 1000)),
        Err(CallFailure::InvalidCall(ContractError::InvalidValue))
    );
    assert_eq!(fixture.runner.calls.load(Ordering::Acquire), 1);
    let now = Instant::now();
    assert!(port.poll().is_empty());
    assert!(now.elapsed() < Duration::from_millis(50));
    port.shutdown();
    fixture.runner.block.store(false, Ordering::Release);
    assert_eq!(
        port.submit(call(33, InstallerRequest::Status, 1000)),
        Err(CallFailure::Unavailable)
    );
    let until = Instant::now() + Duration::from_secs(1);
    let mut replies = Vec::new();
    while replies.len() < 32 {
        replies.extend(port.poll());
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
    replies.sort_by_key(|r| r.id);
    assert_eq!(
        replies.iter().map(|r| r.id).collect::<Vec<_>>(),
        (1..=32).collect::<Vec<_>>()
    );
    assert!(
        replies
            .iter()
            .all(|r| r.result == Err(CallFailure::TimeoutOutcomeUnknown))
    );
}
#[test]
fn missing_support_stale_process_and_disconnect_never_become_acknowledgements() {
    let fixture = Fixture::new();
    let listener = fixture.listener();
    let mut port = fixture.port(false, clock());
    port.submit(call(1, InstallerRequest::Release, 1000))
        .unwrap();
    assert_eq!(reply(&mut port).result, Err(CallFailure::Unavailable));
    assert_eq!(fixture.runner.calls.load(Ordering::Acquire), 0);
    drop(listener);
    port.shutdown();
    let fixture = Fixture::new();
    let server = fixture.server(vec![Action::Reuse(fixture.probe.clone(), fixture.health())]);
    let mut port = fixture.port(false, clock());
    port.submit(call(1, InstallerRequest::Status, 1000))
        .unwrap();
    assert_eq!(reply(&mut port).result, Err(CallFailure::Unavailable));
    server.join().unwrap();
    port.refresh_support(fixture.proof()).unwrap();
}

#[test]
fn proof_expiring_during_status_preflight_refuses_all_mutation_io() {
    let fixture = Fixture::new();
    let listener = fixture.listener();
    listener.set_nonblocking(true).unwrap();
    let health = fixture.health();
    let (entered, preflight) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let server = thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < until);
                    thread::sleep(Duration::from_millis(1));
                }
                Err(e) => panic!("private accept failed: {e}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut request = Vec::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_until(b'\n', &mut request)
            .unwrap();
        assert_eq!(request, encode_request(&InstallerRequest::Status).unwrap());
        entered.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(3)).unwrap();
        stream.write_all(&health).unwrap();
        (listener, request)
    });
    let proof = fixture.proof();
    let mut port = LinuxAgentPort::new(fixture.io.clone(), Some(proof.clone()), clock()).unwrap();
    thread::sleep(SUPPORT_LIFETIME - Duration::from_secs(2));
    port.submit(call(1, InstallerRequest::Release, 5000))
        .unwrap();
    preflight.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(proof.check(&fixture.io), Ok(()));
    let until = Instant::now() + Duration::from_secs(3);
    while proof.check(&fixture.io).is_ok() {
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
    release.send(()).unwrap();
    assert_eq!(reply(&mut port).result, Err(CallFailure::Unavailable));
    let (listener, _) = server.join().unwrap();
    assert_eq!(fixture.runner.calls.load(Ordering::Acquire), 12);
    assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    assert!(port.poll().is_empty());
}

#[test]
fn exec_restart_between_preflight_inspection_and_connect_sends_zero_bytes() {
    for at in [16, 17] {
        let fixture = Fixture::new();
        let listener = fixture.listener();
        let health = fixture.health();
        let path = fixture.io.target().socket_path();
        let bootstrap = fixture.io.target().runtime_dir().join("bootstrap.json");
        let (entered, inspect) = mpsc::channel();
        let (release, released) = mpsc::channel();
        *fixture.runner.hook.lock().unwrap() = Some((
            at,
            Box::new(move || {
                entered.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(2)).unwrap();
            }),
        ));
        let server = thread::spawn(move || {
            let mut first = accept_bounded(&listener);
            assert_eq!(
                read_request(&first),
                encode_request(&InstallerRequest::Status).unwrap()
            );
            first.write_all(&health).unwrap();
            drop(first);
            inspect.recv_timeout(Duration::from_secs(2)).unwrap();
            let old_connection = if at == 17 {
                Some(accept_bounded(&listener))
            } else {
                None
            };
            fs::remove_file(&path).unwrap();
            let replacement = UnixListener::bind(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            let mut value: Value = serde_json::from_slice(&fs::read(&bootstrap).unwrap()).unwrap();
            value["instance_id"] = json!(10);
            fs::write(bootstrap, serde_json::to_vec(&value).unwrap()).unwrap();
            release.send(()).unwrap();
            if let Some(stream) = old_connection {
                assert!(read_request(&stream).is_empty());
            }
            replacement
        });
        let mut port = fixture.port(true, clock());
        port.submit(call(1, InstallerRequest::Release, 2000))
            .unwrap();
        assert_eq!(reply(&mut port).result, Err(CallFailure::Unavailable));
        let replacement = server.join().unwrap();
        replacement.set_nonblocking(true).unwrap();
        assert!(
            matches!(replacement.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock)
        );
    }
}

#[test]
fn proof_expiry_or_revocation_after_connect_closes_before_mutation_bytes() {
    for expire in [false, true] {
        let fixture = Fixture::new();
        let proof = fixture.proof();
        let observed = proof.clone();
        let io = fixture.io.clone();
        *fixture.runner.hook.lock().unwrap() = Some((
            17,
            Box::new(move || {
                assert_eq!(observed.check(&io), Ok(()));
                if expire {
                    let until = Instant::now() + Duration::from_secs(3);
                    while observed.check(&io).is_ok() {
                        assert!(Instant::now() < until);
                        thread::sleep(Duration::from_millis(1));
                    }
                } else {
                    revoke(&observed, &io);
                }
            }),
        ));
        let listener = fixture.listener();
        let health = fixture.health();
        let server = thread::spawn(move || {
            let mut stream = accept_bounded(&listener);
            assert_eq!(
                read_request(&stream),
                encode_request(&InstallerRequest::Status).unwrap()
            );
            stream.write_all(&health).unwrap();
            drop(stream);
            let stream = accept_bounded(&listener);
            assert!(read_request(&stream).is_empty());
            listener
        });
        let mut port = LinuxAgentPort::new(fixture.io.clone(), Some(proof), clock()).unwrap();
        if expire {
            thread::sleep(SUPPORT_LIFETIME - Duration::from_secs(2));
        }
        port.submit(call(1, InstallerRequest::Release, 5000))
            .unwrap();
        assert_eq!(reply(&mut port).result, Err(CallFailure::Unavailable));
        let listener = server.join().unwrap();
        listener.set_nonblocking(true).unwrap();
        assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    }
}

#[test]
fn partial_mutation_write_revocation_or_cancellation_is_unknown_without_resend() {
    for cancel in [false, true] {
        let fixture = Fixture::new();
        let proof = fixture.proof();
        let listener = fixture.listener();
        let health = fixture.health();
        let (entered, writing) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut stream = accept_bounded(&listener);
            read_request(&stream);
            stream.write_all(&health).unwrap();
            drop(stream);
            let mut stream = accept_bounded(&listener);
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut first = [0];
            stream.read_exact(&mut first).unwrap();
            entered.send(()).unwrap();
            resumed.recv_timeout(Duration::from_secs(2)).unwrap();
            let mut bytes = first.to_vec();
            stream.read_to_end(&mut bytes).unwrap();
            (listener, bytes)
        });
        let placements = (0..128)
            .map(|display| Placement {
                node: NodeId([1; 32]),
                display,
                origin_mm: [f64::MAX, -f64::MAX],
            })
            .collect();
        let request = InstallerRequest::Place { placements };
        let expected = encode_request(&request).unwrap();
        assert!(expected.len() > 16384);
        let mut port =
            LinuxAgentPort::new(fixture.io.clone(), Some(proof.clone()), clock()).unwrap();
        port.submit(call(1, request, 2000)).unwrap();
        writing.recv_timeout(Duration::from_secs(1)).unwrap();
        if cancel {
            port.shutdown();
        } else {
            revoke(&proof, &fixture.io);
        }
        assert_eq!(
            reply(&mut port).result,
            Err(CallFailure::TimeoutOutcomeUnknown)
        );
        resume.send(()).unwrap();
        let (listener, bytes) = server.join().unwrap();
        assert!(!bytes.is_empty());
        assert!(bytes.len() < expected.len());
        assert_eq!(bytes, expected[..bytes.len()]);
        listener.set_nonblocking(true).unwrap();
        assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    }
}

#[test]
fn valid_framing_exact_bound_and_oversize_stream_without_eof_are_checked() {
    for kind in 0..5 {
        let fixture = Fixture::new();
        let listener = fixture.listener();
        let mut bytes = fixture.health();
        bytes.pop();
        if kind == 3 {
            bytes.resize(MAX_RESPONSE_BYTES - 1, b' ');
        }
        if kind == 4 {
            // Valid JSON followed only by whitespace: no framing error can mask the bound.
            bytes.resize(MAX_RESPONSE_BYTES + 1, b' ');
        }
        if kind != 0 && kind != 4 {
            bytes.push(b'\n');
        }
        if kind == 1 {
            bytes.extend_from_slice(b"{}");
        }
        let (release, released) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut stream = accept_bounded(&listener);
            read_request(&stream);
            let _ = stream.write_all(&bytes);
            if kind == 2 {
                thread::sleep(Duration::from_millis(20));
                let _ = stream.write_all(b"{}");
            }
            if kind == 4 {
                released.recv_timeout(Duration::from_secs(2)).unwrap();
            }
        });
        let mut port = fixture.port(false, clock());
        let start = Instant::now();
        port.submit(call(1, InstallerRequest::Status, 1000))
            .unwrap();
        let result = reply(&mut port).result;
        if kind == 3 {
            assert!(matches!(
                result,
                Ok(DecodedReply::Status(StatusAdmission::Supported(_)))
            ));
        } else {
            assert_eq!(result, Err(CallFailure::InvalidResponse));
        }
        if kind == 4 {
            assert!(start.elapsed() < Duration::from_millis(800));
            release.send(()).unwrap();
        }
        server.join().unwrap();
    }
}

#[test]
fn restart_admits_only_the_matched_new_instance_and_disconnect_stays_unavailable() {
    let fixture = Fixture::new();
    let mut updated: Value = serde_json::from_slice(&fixture.health()).unwrap();
    updated["result"]["installer"]["instance"]["id"] = json!(10);
    let mut bytes = serde_json::to_vec(&updated).unwrap();
    bytes.push(b'\n');
    let server = fixture.server(vec![Action::Bytes(fixture.health()), Action::Bytes(bytes)]);
    let mut port = fixture.port(false, clock());
    port.submit(call(1, InstallerRequest::Status, 1000))
        .unwrap();
    assert!(matches!(
        reply(&mut port).result,
        Ok(DecodedReply::Status(StatusAdmission::Supported(_)))
    ));
    fixture.write_bootstrap(10, std::process::id());
    fixture.probe.generation.store(78, Ordering::Release);
    port.submit(call(2, InstallerRequest::Status, 1000))
        .unwrap();
    let Ok(DecodedReply::Status(StatusAdmission::Supported(health))) = reply(&mut port).result
    else {
        panic!("new matched instance was refused");
    };
    assert_eq!(health.installer().instance.id, 10);
    server.join().unwrap();
    port.submit(call(3, InstallerRequest::Status, 1000))
        .unwrap();
    assert_eq!(reply(&mut port).result, Err(CallFailure::Unavailable));
}

#[test]
fn successful_ack_settings_read_and_resolved_requests_keep_final_receipt() {
    let lookup = Arc::new(Lookup {
        answers: vec!["192.0.2.1:47811".parse().unwrap()],
        calls: AtomicU64::new(0),
        stall: false,
    });
    let mut resolver = BoundedResolver::injected(lookup.clone());
    resolver.submit("example.test:47811", 1000).unwrap();
    let addr = resolved(&mut resolver).unwrap();
    assert_eq!(lookup.calls.load(Ordering::Acquire), 1);
    for (request, result, kind) in [
        (InstallerRequest::Release, json!(null), 0),
        (
            InstallerRequest::SettingsUpdate {
                expected_revision: "9f86d081884c7d65".into(),
                mac_virtual_display: false,
            },
            json!({"revision":"aaaaaaaaaaaaaaaa", "restart_required":true}),
            1,
        ),
        (InstallerRequest::Windows, json!([]), 2),
        (InstallerRequest::Dial { addr }, json!(null), 0),
        (
            InstallerRequest::PairJoin {
                addr,
                allow_input: false,
            },
            json!(null),
            0,
        ),
    ] {
        let fixture = Fixture::new();
        let mut bytes = serde_json::to_vec(&json!({"ok":true,"result":result})).unwrap();
        bytes.push(b'\n');
        let server = fixture.server(vec![Action::Bytes(fixture.health()), Action::Bytes(bytes)]);
        let times = Arc::new(AtomicU64::new(0));
        let worker = times.clone();
        let mut port = fixture.port(
            true,
            Arc::new(move || worker.fetch_add(1, Ordering::AcqRel)),
        );
        port.submit(call(42, request.clone(), 1000)).unwrap();
        let value = reply(&mut port);
        assert_eq!(value.id, 42);
        assert_eq!(value.observed_at_ms, 2);
        assert_eq!(value.source, ObservationSource::Demo);
        match (kind, value.result) {
            (0, Ok(DecodedReply::Acknowledged)) => {}
            (1, Ok(DecodedReply::SettingsUpdated(value))) => {
                assert_eq!(value.revision, "aaaaaaaaaaaaaaaa");
                assert!(value.restart_required);
            }
            (2, Ok(DecodedReply::Windows(values))) => assert!(values.is_empty()),
            other => panic!("unexpected success reply: {other:?}"),
        }
        assert_eq!(
            server.join().unwrap(),
            [
                encode_request(&InstallerRequest::Status).unwrap(),
                encode_request(&request).unwrap()
            ]
        );
    }
}

#[test]
fn completed_unpolled_queue_stays_full_until_correlated_results_are_drained() {
    let fixture = Fixture::new();
    let server = fixture.server((0..33).map(|_| Action::Bytes(fixture.health())).collect());
    let mut port = fixture.port(false, clock());
    for id in 1..=32 {
        port.submit(call(id, InstallerRequest::Status, 5000))
            .unwrap();
    }
    let until = Instant::now() + Duration::from_secs(2);
    while fixture.runner.calls.load(Ordering::Acquire) < 32 * 12 {
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
    thread::sleep(Duration::from_millis(10));
    let calls = fixture.runner.calls.load(Ordering::Acquire);
    assert_eq!(
        port.submit(call(33, InstallerRequest::Status, 5000)),
        Err(CallFailure::QueueFull)
    );
    assert_eq!(fixture.runner.calls.load(Ordering::Acquire), calls);
    let values = port.poll();
    assert_eq!(values.len(), 32);
    assert_eq!(
        values.iter().map(|v| v.id).collect::<Vec<_>>(),
        (1..=32).collect::<Vec<_>>()
    );
    assert!(
        values
            .iter()
            .all(|v| matches!(v.result, Ok(DecodedReply::Status(_))))
    );
    port.submit(call(33, InstallerRequest::Status, 1000))
        .unwrap();
    assert_eq!(reply(&mut port).id, 33);
    assert_eq!(server.join().unwrap().len(), 33);
}

#[test]
fn cumulative_stage_deadline_cannot_reset_and_timeout_opens_no_retry_connection() {
    let fixture = Fixture::new();
    let listener = fixture.listener();
    let health = fixture.health();
    *fixture.runner.hook.lock().unwrap() =
        Some((1, Box::new(|| thread::sleep(Duration::from_millis(20)))));
    let server = thread::spawn(move || {
        let mut stream = accept_bounded(&listener);
        read_request(&stream);
        thread::sleep(Duration::from_millis(35));
        stream.write_all(&health).unwrap();
        drop(stream);
        let mut stream = accept_bounded(&listener);
        assert_eq!(
            read_request(&stream),
            encode_request(&InstallerRequest::Release).unwrap()
        );
        thread::sleep(Duration::from_millis(55));
        let _ = stream.write_all(b"{\"ok\":true,\"result\":null}\n");
        listener
    });
    let mut port = fixture.port(true, clock());
    let start = Instant::now();
    port.submit(call(1, InstallerRequest::Release, 90)).unwrap();
    assert_eq!(
        reply(&mut port).result,
        Err(CallFailure::TimeoutOutcomeUnknown)
    );
    assert!(start.elapsed() < Duration::from_millis(300));
    let listener = server.join().unwrap();
    listener.set_nonblocking(true).unwrap();
    thread::sleep(Duration::from_millis(20));
    assert!(matches!(listener.accept(),Err(e) if e.kind()==std::io::ErrorKind::WouldBlock));
}

#[test]
fn cancellation_during_connect_and_response_read_is_bounded() {
    let fixture = Fixture::new();
    let listener = fixture.listener();
    rustix::net::listen(&listener, 0).unwrap();
    let occupied = UnixStream::connect(fixture.io.target().socket_path()).unwrap();
    let mut port = fixture.port(false, clock());
    port.submit(call(1, InstallerRequest::Status, 1000))
        .unwrap();
    thread::sleep(Duration::from_millis(20));
    let start = Instant::now();
    port.shutdown();
    assert_eq!(
        reply(&mut port).result,
        Err(CallFailure::TimeoutOutcomeUnknown)
    );
    assert!(start.elapsed() < Duration::from_millis(200));
    drop(occupied);
    drop(listener);
    drop(port);
    let fixture = Fixture::new();
    let listener = fixture.listener();
    let (entered, reading) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let server = thread::spawn(move || {
        let stream = accept_bounded(&listener);
        read_request(&stream);
        entered.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(1)).unwrap();
        listener
    });
    let mut port = fixture.port(false, clock());
    port.submit(call(1, InstallerRequest::Status, 1000))
        .unwrap();
    reading.recv_timeout(Duration::from_secs(1)).unwrap();
    let start = Instant::now();
    port.shutdown();
    assert_eq!(
        reply(&mut port).result,
        Err(CallFailure::TimeoutOutcomeUnknown)
    );
    assert!(start.elapsed() < Duration::from_millis(200));
    release.send(()).unwrap();
    let listener = server.join().unwrap();
    listener.set_nonblocking(true).unwrap();
    assert!(matches!(listener.accept(),Err(e) if e.kind()==std::io::ErrorKind::WouldBlock));
}

struct Lookup {
    answers: Vec<SocketAddr>,
    calls: AtomicU64,
    stall: bool,
}
impl HostLookup for Lookup {
    fn lookup(
        &self,
        host: &str,
        port: u16,
        deadline: &Deadline,
    ) -> Result<Vec<SocketAddr>, NativeError> {
        assert_eq!(host, "example.test");
        assert_eq!(port, 47811);
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.stall {
            loop {
                deadline.check()?;
                thread::sleep(Duration::from_millis(1));
            }
        }
        Ok(self.answers.clone())
    }
}
fn resolved(resolver: &mut BoundedResolver) -> Result<SocketAddr, NativeError> {
    let until = Instant::now() + Duration::from_secs(1);
    loop {
        if let Some(result) = resolver.poll() {
            return result;
        }
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(1));
    }
}
#[test]
fn resolver_grammar_literals_injected_answers_bounds_deadline_and_cancellation() {
    let lookup = Arc::new(Lookup {
        answers: vec![
            "192.0.2.2:47811".parse().unwrap(),
            "192.0.2.1:47811".parse().unwrap(),
        ],
        calls: AtomicU64::new(0),
        stall: false,
    });
    let mut resolver = BoundedResolver::injected(lookup.clone());
    for input in ["192.0.2.1:47811", "[2001:db8::1]:47811"] {
        resolver.submit(input, 1000).unwrap();
        assert_eq!(resolved(&mut resolver).unwrap(), input.parse().unwrap());
    }
    assert_eq!(lookup.calls.load(Ordering::Acquire), 0);
    resolver.submit("EXAMPLE.test:47811", 1000).unwrap();
    assert_eq!(
        resolved(&mut resolver).unwrap(),
        "192.0.2.1:47811".parse().unwrap()
    );
    for input in [
        "example.test",
        "example.test:0",
        "a..b:47811",
        "-bad.test:1",
        "example.test:65536",
        "a/b:1",
        "example.test:1\n",
    ] {
        assert_eq!(resolver.submit(input, 1000), Err(NativeError::Invalid));
    }
    let lookup = Arc::new(Lookup {
        answers: vec!["192.0.2.1:47811".parse().unwrap(); 17],
        calls: AtomicU64::new(0),
        stall: false,
    });
    let mut resolver = BoundedResolver::injected(lookup);
    resolver.submit("example.test:47811", 1000).unwrap();
    assert_eq!(resolved(&mut resolver), Err(NativeError::Oversize));
    let lookup = Arc::new(Lookup {
        answers: vec![],
        calls: AtomicU64::new(0),
        stall: true,
    });
    let mut resolver = BoundedResolver::injected(lookup);
    resolver.submit("example.test:47811", 20).unwrap();
    assert_eq!(
        resolver.submit("example.test:47811", 20),
        Err(NativeError::Busy)
    );
    assert_eq!(resolved(&mut resolver), Err(NativeError::Timeout));
    resolver.submit("example.test:47811", 1000).unwrap();
    resolver.cancel();
    assert_eq!(resolved(&mut resolver), Err(NativeError::Cancelled));
}
struct BlockingLookup {
    entered: AtomicU64,
    release: AtomicBool,
}
#[test]
fn resolver_cancel_drop_retains_slots_and_late_results_cannot_replace_requests() {
    let lookup = Arc::new(BlockingLookup {
        entered: AtomicU64::new(0),
        release: AtomicBool::new(false),
    });
    let mut jobs = Vec::new();
    for _ in 0..4 {
        let mut resolver = BoundedResolver::injected(lookup.clone());
        resolver.submit("example.test:47811", 1000).unwrap();
        jobs.push(resolver);
    }
    let until = Instant::now() + Duration::from_secs(1);
    while lookup.entered.load(Ordering::Acquire) != 4 {
        assert!(Instant::now() < until);
        thread::yield_now();
    }
    for job in &mut jobs {
        job.cancel();
        assert_eq!(resolved(job), Err(NativeError::Cancelled));
    }
    let mut survivor = jobs.pop().unwrap();
    drop(jobs);
    assert_eq!(
        survivor.submit("example.test:47811", 1000),
        Err(NativeError::Busy)
    );
    survivor.submit("192.0.2.99:47811", 1000).unwrap();
    assert_eq!(
        resolved(&mut survivor).unwrap(),
        "192.0.2.99:47811".parse().unwrap()
    );
    lookup.release.store(true, Ordering::Release);
    thread::sleep(Duration::from_millis(20));
    assert!(survivor.poll().is_none());
    survivor.submit("example.test:47811", 1000).unwrap();
    assert_eq!(
        resolved(&mut survivor).unwrap(),
        "192.0.2.1:47811".parse().unwrap()
    );
}

#[test]
fn resolver_hostname_deadline_empty_wrong_port_and_sixteen_answer_boundaries() {
    struct Answers(Vec<SocketAddr>);
    impl HostLookup for Answers {
        fn lookup(&self, _: &str, _: u16, _: &Deadline) -> Result<Vec<SocketAddr>, NativeError> {
            Ok(self.0.clone())
        }
    }
    let mut resolver = BoundedResolver::injected(Arc::new(Answers(
        (1..=16)
            .map(|n| SocketAddr::from(([192, 0, 2, n], 47811)))
            .collect(),
    )));
    for timeout in [0, 5001] {
        assert_eq!(
            resolver.submit("example.test:47811", timeout),
            Err(NativeError::Invalid)
        );
    }
    let host = [
        "a".repeat(63),
        "b".repeat(63),
        "c".repeat(63),
        "d".repeat(61),
    ]
    .join(".");
    assert_eq!(host.len(), 253);
    resolver.submit(&format!("{host}:47811"), 5000).unwrap();
    assert_eq!(
        resolved(&mut resolver).unwrap(),
        "192.0.2.1:47811".parse().unwrap()
    );
    assert_eq!(
        resolver.submit(&format!("{host}x:47811"), 1000),
        Err(NativeError::Invalid)
    );
    assert_eq!(
        resolver.submit(&format!("{}.test:47811", "x".repeat(64)), 1000),
        Err(NativeError::Invalid)
    );
    for (answers, error) in [
        (vec![], NativeError::Unavailable),
        (vec!["192.0.2.1:1".parse().unwrap()], NativeError::Oversize),
    ] {
        let mut resolver = BoundedResolver::injected(Arc::new(Answers(answers)));
        resolver.submit("example.test:47811", 1000).unwrap();
        assert_eq!(resolved(&mut resolver), Err(error));
    }
}
impl HostLookup for BlockingLookup {
    fn lookup(&self, _: &str, port: u16, _: &Deadline) -> Result<Vec<SocketAddr>, NativeError> {
        self.entered.fetch_add(1, Ordering::Release);
        while !self.release.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(1));
        }
        Ok(vec![SocketAddr::from(([192, 0, 2, 1], port))])
    }
}
#[test]
fn timed_out_blocking_resolution_retains_fixed_worker_slots_until_actual_completion() {
    let lookup = Arc::new(BlockingLookup {
        entered: AtomicU64::new(0),
        release: AtomicBool::new(false),
    });
    let mut resolvers = Vec::new();
    for index in 1..=4 {
        let mut resolver = BoundedResolver::injected(lookup.clone());
        resolver.submit("example.test:47811", 25).unwrap();
        let until = Instant::now() + Duration::from_secs(1);
        while lookup.entered.load(Ordering::Acquire) < index {
            assert!(Instant::now() < until);
            thread::yield_now();
        }
        assert_eq!(resolved(&mut resolver), Err(NativeError::Timeout));
        resolvers.push(resolver);
    }
    let mut refused = BoundedResolver::injected(lookup.clone());
    assert_eq!(
        refused.submit("example.test:47811", 1000),
        Err(NativeError::Busy)
    );
    assert_eq!(lookup.entered.load(Ordering::Acquire), 4);
    lookup.release.store(true, Ordering::Release);
    let until = Instant::now() + Duration::from_secs(1);
    loop {
        match refused.submit("example.test:47811", 1000) {
            Ok(()) => break,
            Err(NativeError::Busy) => {
                assert!(Instant::now() < until);
                thread::yield_now();
            }
            other => panic!("unexpected lookup admission: {other:?}"),
        }
    }
    assert!(resolved(&mut refused).is_ok());
}
