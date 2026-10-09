#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // Assertions may unwrap fixture results.
//! WP-4.12a native admission and WP-4.12b bounded transport, with injected observations.
use crosspane_installer::agent_contract::{
    self, BootstrapPhase, InstanceStatus, ObservationSource,
};
// Compile the implementation privately so hooks stay unavailable to application consumers.
#[path = "../src/platform/macos/launchd_observation.rs"]
#[allow(dead_code)]
mod launchd_observation;
#[path = "../src/platform/macos/native_io.rs"]
#[allow(dead_code, unused_imports)]
mod subject;
use subject as native_io;
#[path = "../src/platform/macos/transport.rs"]
#[allow(dead_code, unused_imports)]
mod wire;
use crate::agent_contract::{
    AgentCall, AgentPlatform, AgentPort, AgentRefusal, CallFailure, ContractError, DecodedReply,
    InstallerRequest, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, StatusAdmission,
};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::{
        fs::{PermissionsExt, symlink},
        net::UnixListener,
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use subject::*;
use wire::{BoundedResolver, HostLookup, MacAgentPort, SelectedAgent, SelectedLink};

#[derive(Default)]
struct FakeClock(AtomicU64);
impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}
impl FakeClock {
    fn set(&self, n: u64) {
        self.0.store(n, Ordering::Release);
    }
}
type SupportHook = Arc<dyn Fn(u64) + Send + Sync>;
type CommandHook = Arc<dyn Fn(&CommandSpec) + Send + Sync>;
struct Support(
    Mutex<SupportObservation>,
    AtomicU64,
    Mutex<Option<SupportHook>>,
);
impl SupportProbe for Support {
    fn observe(&self, deadline: &Deadline) -> NativeResult<SupportObservation> {
        deadline.check()?;
        let n = self.1.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some(hook) = self.2.lock().unwrap().clone() {
            hook(n);
        }
        Ok(self.0.lock().unwrap().clone())
    }
}
struct Signatures {
    overrides: Mutex<BTreeMap<PathBuf, SignatureObservation>>,
    calls: Mutex<Vec<PathBuf>>,
}
impl SignatureProbe for Signatures {
    fn observe(
        &self,
        path: &Path,
        approved: &SigningRequirement,
        deadline: &Deadline,
    ) -> NativeResult<SignatureObservation> {
        deadline.check()?;
        self.calls.lock().unwrap().push(path.to_owned());
        Ok(self
            .overrides
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .unwrap_or_else(|| signed(approved)))
    }
}
fn requirement(role: ArtifactRole) -> SigningRequirement {
    SigningRequirement {
        role,
        identifier: if role == ArtifactRole::Agent {
            AGENT_LABEL.into()
        } else {
            format!("test.fixture.{role:?}")
        },
        designated_requirement: "test-approved-development-requirement".into(),
        entitlements: if role == ArtifactRole::Agent {
            BTreeMap::from([("com.apple.security.device.audio-input".into(), true)])
        } else {
            BTreeMap::new()
        },
    }
}
fn signed(approved: &SigningRequirement) -> SignatureObservation {
    SignatureObservation {
        strict_verified: true,
        team_identifier: "ABCDE12345".into(),
        identifier: approved.identifier.clone(),
        designated_requirement: approved.designated_requirement.clone(),
        entitlements: approved.entitlements.clone(),
        apple_development: true,
        hardened_runtime: true,
        ad_hoc: false,
    }
}
struct Runner {
    uid: u32,
    exe: PathBuf,
    start: Mutex<Vec<u8>>,
    calls: Mutex<Vec<CommandSpec>>,
    absent: AtomicBool,
    blocked: AtomicBool,
    hook: Mutex<Option<CommandHook>>,
    result: Mutex<Option<NativeResult<CommandOutput>>>,
    reported_exe: Mutex<Option<PathBuf>>,
    reported_uid: Mutex<Option<u32>>,
}
impl CommandRunner for Runner {
    fn run(&self, spec: &CommandSpec, deadline: &Deadline) -> NativeResult<CommandOutput> {
        self.calls.lock().unwrap().push(spec.clone());
        if let Some(hook) = self.hook.lock().unwrap().clone() {
            hook(spec);
        }
        while self.blocked.load(Ordering::Acquire) {
            deadline.check()?;
            std::thread::sleep(Duration::from_millis(1));
        }
        deadline.check()?;
        if let Some(result) = self.result.lock().unwrap().clone() {
            return result;
        }
        let stdout = if spec.program() == Path::new("/bin/ps") {
            if self.absent.load(Ordering::Acquire) {
                return Ok(CommandOutput {
                    code: Some(1),
                    stdout: vec![],
                    stderr: vec![],
                });
            }
            match spec.args()[1].as_str() {
                "uid=" => format!(
                    " {}\n",
                    self.reported_uid.lock().unwrap().unwrap_or(self.uid)
                )
                .into_bytes(),
                "lstart=" => self.start.lock().unwrap().clone(),
                "comm=" => format!(
                    "{}\n",
                    self.reported_exe
                        .lock()
                        .unwrap()
                        .as_deref()
                        .unwrap_or(&self.exe)
                        .display()
                )
                .into_bytes(),
                _ => panic!("unexpected ps field"),
            }
        } else {
            vec![]
        };
        Ok(CommandOutput {
            code: Some(0),
            stdout,
            stderr: vec![],
        })
    }
}
static NEXT: AtomicU64 = AtomicU64::new(1);
struct Fixture {
    root: PathBuf,
    home: PathBuf,
    tmp: PathBuf,
    runtime: PathBuf,
    io: Arc<MacNativeIo>,
    clock: Arc<FakeClock>,
    support: Arc<Support>,
    signatures: Arc<Signatures>,
    runner: Arc<Runner>,
    _listener: UnixListener,
}
fn directory(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
fn bytes(path: &Path, data: &[u8], mode: u32) {
    directory(path.parent().unwrap());
    fs::write(path, data).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}
impl Fixture {
    fn new() -> Self {
        let uid = rustix::process::geteuid().as_raw();
        let root = PathBuf::from(format!(
            "/private/tmp/cp-a-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let (home, tmp) = (root.join("h"), root.join("t"));
        directory(&home);
        directory(&tmp);
        let runtime = tmp.join("crosspane");
        directory(&runtime);
        let payload_root = tmp.join("payload");
        directory(&payload_root);
        let target = MacTarget::scratch(TargetPaths {
            uid,
            home: home.clone(),
            gui_tmpdir: tmp.clone(),
            runtime_override: None,
            payload_root,
        })
        .unwrap();
        let exe = target.agent_path();
        bytes(&exe, b"owned signed executable fixture", 0o755);
        directory(&target.installer_dir());
        let clock = Arc::new(FakeClock::default());
        let support = Arc::new(Support(
            Mutex::new(SupportObservation {
                macos_major: 26,
                apple_silicon: true,
                gui: GuiObservation {
                    console_uid: Some(uid),
                    interactive_uid: Some(uid),
                    console_session: "selected-aqua-session".into(),
                    interactive_session: "selected-aqua-session".into(),
                    active: true,
                },
                gui_tmpdir: tmp.clone(),
            }),
            AtomicU64::new(0),
            Mutex::default(),
        ));
        let signatures = Arc::new(Signatures {
            overrides: Mutex::default(),
            calls: Mutex::default(),
        });
        let runner = Arc::new(Runner {
            uid,
            exe,
            start: Mutex::new(b"Thu Jan  1 00:00:00 1970\n".to_vec()),
            calls: Mutex::default(),
            absent: AtomicBool::new(false),
            blocked: AtomicBool::new(false),
            hook: Mutex::default(),
            result: Mutex::default(),
            reported_exe: Mutex::default(),
            reported_uid: Mutex::default(),
        });
        let listener = UnixListener::bind(target.socket_path()).unwrap();
        fs::set_permissions(target.socket_path(), fs::Permissions::from_mode(0o600)).unwrap();
        let io = Arc::new(
            MacNativeIo::new(
                target,
                runner.clone(),
                support.clone(),
                signatures.clone(),
                clock.clone(),
            )
            .unwrap(),
        );
        let fixture = Self {
            root,
            home,
            tmp,
            runtime,
            io,
            clock,
            support,
            signatures,
            runner,
            _listener: listener,
        };
        fixture.bootstrap(u64::MAX, 0, "ready", 1);
        fixture
    }
    fn deadline(&self) -> Deadline {
        Deadline::new(5000, self.clock.clone(), Cancellation::default()).unwrap()
    }
    fn main(&self) -> SignatureProof {
        self.io
            .admit_main_signature(
                &self.io.target().agent_path(),
                &requirement(ArtifactRole::Agent),
                &self.deadline(),
            )
            .unwrap()
    }
    fn proof(&self) -> SupportProof {
        self.io
            .admit_support(&self.main(), &self.deadline())
            .unwrap()
    }
    fn bootstrap(&self, instance: u64, started: u64, phase: &str, seq: u64) {
        let data = serde_json::json!({"schema_version":1,"instance_id":instance,"pid":4242,"started_unix_ms":started,"phase":phase,"phase_seq":seq,"keystore":null,"reason":null,"runtime_dir":self.runtime});
        bytes(
            &self.runtime.join("bootstrap.json"),
            &serde_json::to_vec(&data).unwrap(),
            0o600,
        );
    }
    fn admitted(&self) -> AdmittedInstance {
        self.io
            .admit_instance(&self.proof(), &self.main(), &self.deadline())
            .unwrap()
    }
    fn status(&self) -> InstanceStatus {
        InstanceStatus {
            id: u64::MAX,
            pid: 4242,
            uid: Some(self.runner.uid),
            exe: self.runner.exe.to_string_lossy().into_owned(),
            runtime_dir: self.runtime.to_string_lossy().into_owned(),
            started_unix_ms: 0,
        }
    }
    fn command(&self, action: LaunchctlAction) -> CommandSpec {
        CommandSpec::new(self.io.target(), NativeOperation::Launchctl(action)).unwrap()
    }
    fn hooked(&self, hook: TestHook) -> Arc<MacNativeIo> {
        let mut target = self.io.target().clone();
        target.test_hook = Some(hook);
        Arc::new(
            MacNativeIo::new(
                target,
                self.runner.clone(),
                self.support.clone(),
                self.signatures.clone(),
                self.clock.clone(),
            )
            .unwrap(),
        )
    }
    fn operated(&self, filesystem: Arc<dyn FilesystemOps>) -> Arc<MacNativeIo> {
        Arc::new(
            MacNativeIo::new(
                self.io.target().clone(),
                self.runner.clone(),
                self.support.clone(),
                self.signatures.clone(),
                self.clock.clone(),
            )
            .unwrap()
            .with_filesystem(filesystem),
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.runner.blocked.store(false, Ordering::Release);
        fs::remove_dir_all(&self.root).unwrap();
    }
}

#[test]
fn explicit_scratch_target_never_uses_owner_defaults_and_discovery_is_read_only() {
    let f = Fixture::new();
    assert_eq!(f.io.target().paths().home, f.home);
    assert_eq!(f.io.target().runtime_dir(), f.runtime);
    assert_eq!(f.io.target().source(), ObservationSource::Demo);
    assert!(!f.home.join(".local").exists());
    let calls = f.runner.calls.lock().unwrap();
    assert!(calls.is_empty());
    assert!(!f.home.join("Library/LaunchAgents").exists());
}

// Producer-shaped bytes. Only scratch identity/path fields are replaced for each selected target.
const MAC_STATUS: &[u8] = br#"{"ok":true,"result":{"controlling":null,"controlled_by":null,
"projections":[],"displays":[],"peers":[],"layout":[],"installer":{
"schema_version":1,"build":{"version":"0.0.0","features":["video"]},
"instance":{"id":18446744073709551615,"pid":4242,"uid":1,"exe":"/explicit/scratch",
"runtime_dir":"/explicit/scratch","started_unix_ms":0},"config_revision":"9f86d081884c7d65",
"node":"1111111111111111111111111111111111111111111111111111111111111111",
"recovery_pending":0,"startup_recovery":"nothing_parked",
"gate":{"open":true,"session":"unlocked","active":true,"armed":false,"panic":false},
"epochs":{"gate":1,"grants":2,"layout":3,"backends":4},
"backends":[{"name":"capture","state":"ready","reason":null},
{"name":"keys","state":"ready","reason":null},{"name":"pointer","state":"ready","reason":null},
{"name":"overlay","state":"ready","reason":null},{"name":"hotkeys","state":"ready","reason":null},
{"name":"keystore","state":"ready","reason":null},{"name":"windows","state":"ready","reason":null},
{"name":"parking","state":"ready","reason":null},{"name":"frames","state":"ready","reason":null},
{"name":"tray","state":"ready","reason":null},{"name":"links","state":"ready","reason":null},
{"name":"gpu","state":"ready","reason":null},{"name":"home","state":"ready","reason":null},
{"name":"audio","state":"ready","reason":null},{"name":"discovery","state":"ready","reason":null}],
"keystore":"os_store","permissions":[{"name":"screen_recording","state":"granted"},
{"name":"accessibility","state":"granted"},{"name":"input_monitoring","state":"granted"}],
"discovery":{"enabled":false,"running":false,"candidates":0,"error":null},"tray":{"created":true},
"audio":{"enabled":false,"active_peers":[],"frames_sent":0,"frames_played":0},"settings_opened":0,
"peers":[{"node":"2222222222222222222222222222222222222222222222222222222222222222",
"name":"owned peer","connected":true,"link_generation":3,"features":["e1"],"grants_given":[],
"last_source_parking":null,"counters":{"e1_controller_started":0,"e1_controller_ended":0,
"e1_target_started":0,"e1_target_ended":0,"e1_injections_ok":0,"e1_hud_shows":0,
"e1_chord_releases":0,"e1_command_releases":0,"e2_source_started":0,"e2_source_returned":0,
"e2_dest_started":0,"e2_dest_returned":0,"e2_frames_presented":null,"e2_returns_failed":0}}]}}}"#;
fn wire_status(f: &Fixture) -> serde_json::Value {
    let mut value: serde_json::Value = serde_json::from_slice(MAC_STATUS).unwrap();
    value["result"]["installer"]["instance"] = serde_json::to_value(f.status()).unwrap();
    let bytes = line(&value);
    assert!(matches!(
        agent_contract::decode_reply(&InstallerRequest::Status, &bytes, AgentPlatform::Macos),
        Ok(DecodedReply::Status(StatusAdmission::Supported(_)))
    ));
    value
}
fn line(value: &serde_json::Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(value).unwrap();
    bytes.push(b'\n');
    bytes
}
fn selected(f: &Fixture) -> SelectedAgent {
    SelectedAgent {
        io: f.io.clone(),
        support: f.proof(),
        instance: Arc::new(f.admitted()),
        link: Some(SelectedLink {
            node: crosspane_types::id::NodeId([0x22; 32]),
            generation: 3,
        }),
    }
}
fn call(id: u64, request: InstallerRequest, timeout_ms: u64) -> AgentCall {
    AgentCall {
        id,
        request,
        timeout_ms,
    }
}
fn drain(port: &mut MacAgentPort, count: usize) -> Vec<agent_contract::AgentReply> {
    let until = Instant::now() + Duration::from_secs(10);
    let mut replies = Vec::new();
    while replies.len() < count {
        replies.extend(port.poll());
        assert!(Instant::now() < until, "missing selected-agent reply");
        std::thread::sleep(Duration::from_millis(1));
    }
    replies
}
type SocketHandler = Arc<dyn Fn(&[u8], &mut std::os::unix::net::UnixStream) + Send + Sync>;
struct ReleaseOnDrop(Arc<AtomicBool>);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
fn wait_for(mut ready: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(3);
    while !ready() {
        assert!(Instant::now() < until, "owned test stage did not finish");
        std::thread::sleep(Duration::from_millis(1));
    }
}
struct Server {
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    stop: Arc<AtomicBool>,
    task: Option<std::thread::JoinHandle<()>>,
}
impl Server {
    fn new(f: &Fixture, handler: SocketHandler) -> Self {
        Self::guarded(f, handler, None)
    }
    fn guarded(
        f: &Fixture,
        handler: SocketHandler,
        before_read: Option<Arc<dyn Fn(usize) + Send + Sync>>,
    ) -> Self {
        let listener = f._listener.try_clone().unwrap();
        Self::owned(listener, handler, before_read)
    }
    fn owned(
        listener: UnixListener,
        handler: SocketHandler,
        before_read: Option<Arc<dyn Fn(usize) + Send + Sync>>,
    ) -> Self {
        Self::reader(listener, handler, before_read, false)
    }
    fn reader(
        listener: UnixListener,
        handler: SocketHandler,
        before_read: Option<Arc<dyn Fn(usize) + Send + Sync>>,
        prefix: bool,
    ) -> Self {
        use std::io::Read;
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let received = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let task = std::thread::spawn(move || {
            let mut accepted = 0;
            while !stopping.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        accepted += 1;
                        if let Some(before) = &before_read {
                            before(accepted);
                        }
                        // macOS refuses socket options (EINVAL) once the peer has already
                        // closed the connection: such a client sent nothing.
                        let configured = socket
                            .set_nonblocking(false)
                            .and_then(|()| socket.set_read_timeout(Some(Duration::from_secs(2))))
                            .and_then(|()| socket.set_write_timeout(Some(Duration::from_secs(2))));
                        if configured.is_err() {
                            received.lock().unwrap().push(Vec::new());
                            continue;
                        }
                        let mut bytes = Vec::new();
                        if prefix {
                            let mut buffer = [0; 8192];
                            let n = socket.read(&mut buffer).unwrap();
                            bytes.extend_from_slice(&buffer[..n]);
                        } else {
                            (&mut socket)
                                .take((MAX_REQUEST_BYTES + 1) as u64)
                                .read_to_end(&mut bytes)
                                .unwrap();
                        }
                        assert!(bytes.len() <= MAX_REQUEST_BYTES);
                        received.lock().unwrap().push(bytes.clone());
                        if !bytes.is_empty() {
                            handler(&bytes, &mut socket);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(e) => panic!("owned listener failed: {e}"),
                }
            }
        });
        Self {
            requests,
            stop,
            task: Some(task),
        }
    }
    fn commands(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|b| !b.is_empty())
            .map(|b| {
                serde_json::from_slice::<serde_json::Value>(b).unwrap()["cmd"]
                    .as_str()
                    .unwrap()
                    .into()
            })
            .collect()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let result = self.task.take().unwrap().join();
        if !std::thread::panicking() {
            result.unwrap();
        }
    }
}
fn command(bytes: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(bytes).unwrap()["cmd"]
        .as_str()
        .unwrap()
        .into()
}
fn send_owned(socket: &mut std::os::unix::net::UnixStream, bytes: &[u8]) {
    use std::io::Write;
    if let Err(error) = socket.write_all(bytes) {
        assert!(
            matches!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
            ),
            "unexpected owned socket error: {error}"
        );
    }
}
fn basic_server(f: &Fixture, answer: &[u8]) -> Server {
    let status = line(&wire_status(f));
    let answer = answer.to_vec();
    Server::new(
        f,
        Arc::new(move |bytes, socket| {
            send_owned(
                socket,
                if command(bytes) == "status" {
                    &status
                } else {
                    &answer
                },
            );
        }),
    )
}

fn bound_not_listening(f: &Fixture) -> Arc<rustix::fd::OwnedFd> {
    use rustix::net::{self, AddressFamily, SocketAddrUnix, SocketType};
    let path = f.io.target().socket_path();
    fs::remove_file(&path).unwrap();
    let socket = net::socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
    net::bind(&socket, &SocketAddrUnix::new(&path).unwrap()).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    Arc::new(socket)
}

#[test]
fn transport_reconnect_has_three_fresh_pre_byte_attempts_and_two_four_ms_backoff() {
    for ready_at in [None, Some(2), Some(3)] {
        let f = Fixture::new();
        let socket = bound_not_listening(&f);
        let selection = selected(&f);
        let checks = Arc::new(AtomicU64::new(0));
        let checked = checks.clone();
        let server = Arc::new(Mutex::new(None::<Server>));
        let started = server.clone();
        let status = line(&wire_status(&f));
        *f.support.2.lock().unwrap() = Some(Arc::new(move |_| {
            let attempt = checked.fetch_add(1, Ordering::AcqRel) + 1;
            if Some(attempt) == ready_at {
                rustix::net::listen(&socket, 16).unwrap();
                let listener = UnixListener::from(socket.try_clone().unwrap());
                let status = status.clone();
                *started.lock().unwrap() = Some(Server::owned(
                    listener,
                    Arc::new(move |bytes, stream| {
                        assert_eq!(command(bytes), "status");
                        send_owned(stream, &status);
                    }),
                    None,
                ));
            }
        }));
        let mut port = MacAgentPort::new(selection, Arc::new(|| 17)).unwrap();
        port.submit(call(1, InstallerRequest::Status, 5000))
            .unwrap();
        let reply = drain(&mut port, 1).pop().unwrap();
        assert_eq!(
            wire::BACKOFF
                .lock()
                .unwrap()
                .iter()
                .filter(|(home, _)| home == &f.home)
                .map(|(_, delay)| *delay)
                .collect::<Vec<_>>(),
            if ready_at == Some(2) {
                vec![2]
            } else {
                vec![2, 4]
            }
        );
        if ready_at.is_none() {
            assert_eq!(reply.result, Err(CallFailure::Unavailable));
            assert_eq!(checks.load(Ordering::Acquire), 3);
        } else {
            assert!(matches!(
                reply.result,
                Ok(DecodedReply::Status(StatusAdmission::Supported(_)))
            ));
            assert_eq!(
                server.lock().unwrap().as_ref().unwrap().commands(),
                ["status"]
            );
        }
        *f.support.2.lock().unwrap() = None;
    }
}

#[test]
fn transport_reconnect_cannot_escape_total_deadline_or_original_instance() {
    for expires in [false, true] {
        let f = Fixture::new();
        let _socket = bound_not_listening(&f);
        let selection = selected(&f);
        let checks = Arc::new(AtomicU64::new(0));
        let checked = checks.clone();
        let clock = f.clock.clone();
        let bootstrap = f.runtime.join("bootstrap.json");
        let mut replacement: serde_json::Value =
            serde_json::from_slice(&fs::read(&bootstrap).unwrap()).unwrap();
        replacement["instance_id"] = 7.into();
        *f.support.2.lock().unwrap() = Some(Arc::new(move |_| {
            if checked.fetch_add(1, Ordering::AcqRel) + 1 == 2 {
                if expires {
                    clock.set(5001);
                } else {
                    fs::write(&bootstrap, serde_json::to_vec(&replacement).unwrap()).unwrap();
                }
            }
        }));
        let mut port = MacAgentPort::new(selection, Arc::new(|| 19)).unwrap();
        port.submit(call(1, InstallerRequest::Status, 5000))
            .unwrap();
        assert_eq!(
            drain(&mut port, 1)[0].result,
            Err(if expires {
                CallFailure::TimeoutOutcomeUnknown
            } else {
                CallFailure::Unavailable
            })
        );
        assert_eq!(checks.load(Ordering::Acquire), 2);
        *f.support.2.lock().unwrap() = None;
    }
}

#[test]
fn transport_status_and_partial_reply_retain_complete_byte_clock_before_poll_and_post_status() {
    let f = Fixture::new();
    let status = line(&wire_status(&f));
    let ticks = Arc::new(AtomicU64::new(10));
    let body = Arc::new(AtomicBool::new(false));
    let newline = Arc::new(AtomicBool::new(false));
    let receipt = Arc::new(AtomicBool::new(false));
    let continue_receipt = Arc::new(AtomicBool::new(false));
    let validation = Arc::new(AtomicBool::new(false));
    let stages = Arc::new(AtomicU64::new(0));
    let home = f.home.clone();
    let received = receipt.clone();
    let resumed = continue_receipt.clone();
    let count = stages.clone();
    *wire::HOOK.lock().unwrap() = Some(Arc::new(move |selected, stage| {
        if selected == home && stage == "receipt" && count.fetch_add(1, Ordering::AcqRel) + 1 == 2 {
            received.store(true, Ordering::Release);
            wait_for(|| resumed.load(Ordering::Acquire));
        }
    }));
    let after = receipt.clone();
    let advanced = validation.clone();
    let during = ticks.clone();
    *f.support.2.lock().unwrap() = Some(Arc::new(move |_| {
        if after.load(Ordering::Acquire) && !advanced.swap(true, Ordering::AcqRel) {
            during.store(50, Ordering::Release);
        }
    }));
    let sent = body.clone();
    let send_newline = newline.clone();
    let before = ticks.clone();
    let server = Server::new(
        &f,
        Arc::new(move |bytes, socket| {
            if command(bytes) == "status" {
                send_owned(socket, &status);
                return;
            }
            assert_eq!(command(bytes), "settings_update");
            let answer=b"{\"ok\":true,\"result\":{\"revision\":\"9f86d081884c7d65\",\"restart_required\":true}}";
            before.store(20, Ordering::Release);
            for chunk in answer.chunks(13) {
                send_owned(socket, chunk);
            }
            sent.store(true, Ordering::Release);
            wait_for(|| send_newline.load(Ordering::Acquire));
            send_owned(socket, b"\n");
        }),
    );
    let _newline_unwind = ReleaseOnDrop(newline.clone());
    let _receipt_unwind = ReleaseOnDrop(continue_receipt.clone());
    let clock = ticks.clone();
    let mut port = MacAgentPort::new(
        selected(&f),
        Arc::new(move || clock.load(Ordering::Acquire)),
    )
    .unwrap();
    port.submit(call(
        1,
        InstallerRequest::SettingsUpdate {
            expected_revision: "9f86d081884c7d65".into(),
            mac_virtual_display: false,
        },
        5000,
    ))
    .unwrap();
    wait_for(|| body.load(Ordering::Acquire));
    assert_eq!(stages.load(Ordering::Acquire), 1); // Pre-status only; body without newline has no receipt.
    ticks.store(30, Ordering::Release);
    newline.store(true, Ordering::Release);
    wait_for(|| receipt.load(Ordering::Acquire)); // Complete bytes, before EOF/post-read admission.
    ticks.store(40, Ordering::Release);
    continue_receipt.store(true, Ordering::Release);
    wait_for(|| validation.load(Ordering::Acquire) && server.commands().len() == 3);
    assert_eq!(ticks.load(Ordering::Acquire), 50);
    ticks.store(60, Ordering::Release); // GUI delivery cannot refresh the completed receipt.
    let reply = drain(&mut port, 1).pop().unwrap();
    assert_eq!(reply.observed_at_ms, 30);
    assert_eq!(reply.source, ObservationSource::Demo);
    assert_eq!(
        reply.result,
        Ok(DecodedReply::SettingsUpdated(
            agent_contract::SettingsUpdated {
                revision: "9f86d081884c7d65".into(),
                restart_required: true
            }
        ))
    );
    assert_eq!(server.commands(), ["status", "settings_update", "status"]);
    *wire::HOOK.lock().unwrap() = None;
    *f.support.2.lock().unwrap() = None;
}

#[test]
fn transport_full_typed_place_request_uses_partial_writes_and_keeps_exact_bytes() {
    let f = Fixture::new();
    let server = basic_server(&f, b"{\"ok\":true}\n");
    let selection = selected(&f);
    let before = f.runner.calls.lock().unwrap().len();
    let mut port = MacAgentPort::new(selection, Arc::new(|| 29)).unwrap();
    let request = InstallerRequest::Place {
        placements: (0..128)
            .map(|display| agent_contract::Placement {
                node: crosspane_types::id::NodeId([0x22; 32]),
                display,
                origin_mm: [1.234567890123456e200, -1.234567890123456e200],
            })
            .collect(),
    };
    let expected = agent_contract::encode_request(&request).unwrap();
    assert!(expected.len() > 8192 && expected.len() < MAX_REQUEST_BYTES);
    port.submit(call(1, request, 5000)).unwrap();
    assert_eq!(
        drain(&mut port, 1)[0].result,
        Ok(DecodedReply::Acknowledged)
    );
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests[1], expected);
    assert!(
        f.runner.calls.lock().unwrap().len() - before > 120,
        "bounded send buffer must require another mutation write and original-instance recheck"
    );
}

#[test]
fn transport_refusal_payload_categories_and_sensitive_debug_are_preserved() {
    for (reason, refusal) in [
        ("revision_conflict", AgentRefusal::RevisionConflict),
        ("not_supported", AgentRefusal::NotSupported),
        ("other", AgentRefusal::Other),
    ] {
        let f = Fixture::new();
        let reason = if reason == "other" {
            "SENSITIVE-WIRE-SENTINEL"
        } else {
            reason
        };
        let answer = format!("{{\"ok\":false,\"error\":\"{reason}\",\"result\":null}}\n");
        let server = basic_server(&f, answer.as_bytes());
        let selection = selected(&f);
        assert!(!format!("{selection:?}").contains("SENSITIVE"));
        let mut port = MacAgentPort::new(selection, Arc::new(|| 31)).unwrap();
        port.submit(call(1, InstallerRequest::Release, 1000))
            .unwrap();
        let reply = drain(&mut port, 1).pop().unwrap();
        assert_eq!(reply.result, Err(CallFailure::Refused(refusal)));
        assert_eq!(reply.observed_at_ms, 31);
        assert_eq!(reply.source, ObservationSource::Demo);
        assert!(!format!("{reply:?} {port:?}").contains("SENSITIVE-WIRE-SENTINEL"));
        assert_eq!(server.commands(), ["status", "release", "status"]);
    }
}

#[test]
fn transport_response_limit_exact_boundary_missing_newline_and_trailing_objects() {
    for kind in [
        "exact",
        "oversize",
        "missing-newline",
        "trailing",
        "invalid-json",
    ] {
        let f = Fixture::new();
        let mut answer = b"{\"ok\":true}".to_vec();
        match kind {
            "exact" | "oversize" => {
                answer.resize(MAX_RESPONSE_BYTES - usize::from(kind == "exact"), b' ');
                answer.push(b'\n');
            }
            "missing-newline" => {}
            "trailing" => answer.extend_from_slice(b"\n{\"ok\":true}\n"),
            "invalid-json" => answer = b"not-json\n".to_vec(),
            _ => unreachable!(),
        }
        let server = basic_server(&f, &answer);
        let mut port = MacAgentPort::new(selected(&f), Arc::new(|| 47)).unwrap();
        port.submit(call(1, InstallerRequest::Release, 5000))
            .unwrap();
        assert_eq!(
            drain(&mut port, 1)[0].result,
            if kind == "exact" {
                Ok(DecodedReply::Acknowledged)
            } else {
                Err(CallFailure::InvalidResponse)
            }
        );
        assert_eq!(
            server.commands().iter().filter(|c| *c == "release").count(),
            1
        );
    }
}

#[test]
fn transport_queue_full_and_local_invalid_calls_start_no_io_and_poll_is_bounded() {
    let f = Fixture::new();
    let selection = selected(&f);
    let before = f.runner.calls.lock().unwrap().len();
    f._listener.set_nonblocking(true).unwrap();
    f.runner.blocked.store(true, Ordering::Release);
    let mut port = MacAgentPort::new(selection.clone(), Arc::new(|| 50)).unwrap();
    assert_eq!(f.runner.calls.lock().unwrap().len(), before);
    assert_eq!(
        port.submit(call(0, InstallerRequest::Status, 1000)),
        Err(CallFailure::InvalidCall(ContractError::InvalidValue))
    );
    assert_eq!(
        port.submit(call(1, InstallerRequest::Status, 0)),
        Err(CallFailure::InvalidCall(ContractError::InvalidDeadline))
    );
    assert_eq!(
        port.submit(call(1, InstallerRequest::Status, 5001)),
        Err(CallFailure::InvalidCall(ContractError::InvalidDeadline))
    );
    assert_eq!(
        port.submit(call(
            1,
            InstallerRequest::PairListen { allow_input: true },
            1000
        )),
        Err(CallFailure::InvalidCall(ContractError::InvalidValue))
    );
    let begin = Instant::now();
    for id in 1..=32 {
        port.submit(call(id, InstallerRequest::Status, 1000))
            .unwrap();
    }
    assert!(begin.elapsed() < Duration::from_millis(100));
    assert_eq!(
        port.submit(call(33, InstallerRequest::Status, 1000)),
        Err(CallFailure::QueueFull)
    );
    assert_eq!(port.redetect(selection), Err(NativeError::Busy));
    assert!(matches!(f._listener.accept(), Err(e) if e.kind()==std::io::ErrorKind::WouldBlock));
    f.clock.set(1001);
    let replies = drain(&mut port, 32);
    assert_eq!(replies.len(), 32);
    assert!(
        replies
            .iter()
            .all(|r| r.result == Err(CallFailure::TimeoutOutcomeUnknown)
                && r.source == ObservationSource::Demo)
    );
    assert_eq!(
        replies.iter().map(|r| r.id).collect::<Vec<_>>(),
        (1..=32).collect::<Vec<_>>()
    );
    f.runner.blocked.store(false, Ordering::Release);
    assert_eq!(
        port.submit(call(32, InstallerRequest::Status, 1000)),
        Err(CallFailure::InvalidCall(ContractError::InvalidValue))
    );
}

#[test]
fn transport_completed_undrained_replies_still_count_toward_thirty_two() {
    let f = Fixture::new();
    let server = basic_server(&f, b"{\"ok\":true}\n");
    let mut port = MacAgentPort::new(selected(&f), Arc::new(|| 61)).unwrap();
    for id in 1..=32 {
        port.submit(call(id, InstallerRequest::Status, 5000))
            .unwrap();
    }
    let until = Instant::now() + Duration::from_secs(5);
    while server.commands().len() < 32 {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(1));
    }
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(
        port.submit(call(33, InstallerRequest::Status, 1000)),
        Err(CallFailure::QueueFull)
    );
    assert_eq!(drain(&mut port, 32).len(), 32);
    port.submit(call(33, InstallerRequest::Status, 1000))
        .unwrap();
    assert!(matches!(
        drain(&mut port, 1)[0].result,
        Ok(DecodedReply::Status(StatusAdmission::Supported(_)))
    ));
}

#[test]
fn transport_mutation_timeout_detects_but_never_resends_or_unblocks_without_explicit_handoff() {
    let f = Fixture::new();
    let status = line(&wire_status(&f));
    let clock = f.clock.clone();
    let release = Arc::new(AtomicBool::new(false));
    let released = release.clone();
    let server = Server::new(
        &f,
        Arc::new(move |bytes, socket| match command(bytes).as_str() {
            "status" => send_owned(socket, &status),
            "release" => {
                clock.set(1001);
                while !released.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            "panic" => send_owned(socket, b"{\"ok\":true}\n"),
            other => panic!("unexpected command {other}"),
        }),
    );
    let mut port = MacAgentPort::new(selected(&f), Arc::new(|| 71)).unwrap();
    let _unwind_release = ReleaseOnDrop(release.clone());
    port.submit(call(1, InstallerRequest::Release, 1000))
        .unwrap();
    port.submit(call(2, InstallerRequest::Panic, 5000)).unwrap();
    let replies = drain(&mut port, 2);
    assert_eq!(replies[0].result, Err(CallFailure::TimeoutOutcomeUnknown));
    assert_eq!(replies[1].result, Err(CallFailure::Unavailable));
    release.store(true, Ordering::Release);
    port.submit(call(3, InstallerRequest::Status, 1000))
        .unwrap();
    assert!(matches!(
        drain(&mut port, 1)[0].result,
        Ok(DecodedReply::Status(StatusAdmission::Supported(_)))
    ));
    port.submit(call(4, InstallerRequest::Panic, 1000)).unwrap();
    assert_eq!(drain(&mut port, 1)[0].result, Err(CallFailure::Unavailable));
    port.redetect(selected(&f)).unwrap();
    port.submit(call(5, InstallerRequest::Panic, 1000)).unwrap();
    assert_eq!(
        drain(&mut port, 1)[0].result,
        Ok(DecodedReply::Acknowledged)
    );
    assert_eq!(
        server.commands().iter().filter(|c| *c == "release").count(),
        1
    );
    assert_eq!(
        server.commands().iter().filter(|c| *c == "panic").count(),
        1
    );
}

#[test]
fn transport_restart_or_link_change_requires_idle_explicit_re_detection_and_new_call_id() {
    for restart in [false, true] {
        let f = Fixture::new();
        let current = Arc::new(Mutex::new(wire_status(&f)));
        let replies = current.clone();
        let server = Server::new(
            &f,
            Arc::new(move |bytes, socket| {
                if command(bytes) == "status" {
                    send_owned(socket, &line(&replies.lock().unwrap()));
                } else {
                    send_owned(socket, b"{\"ok\":true}\n");
                }
            }),
        );
        let mut port = MacAgentPort::new(selected(&f), Arc::new(|| 83)).unwrap();
        port.submit(call(1, InstallerRequest::Status, 1000))
            .unwrap();
        drain(&mut port, 1);
        if restart {
            f.bootstrap(7, 0, "ready", 1);
            current.lock().unwrap()["result"]["installer"]["instance"]["id"] = 7.into();
        } else {
            current.lock().unwrap()["result"]["installer"]["peers"][0]["link_generation"] =
                8.into();
        }
        port.submit(call(2, InstallerRequest::Release, 1000))
            .unwrap();
        assert_eq!(drain(&mut port, 1)[0].result, Err(CallFailure::Unavailable));
        assert!(!server.commands().iter().any(|c| c == "release"));
        let mut renewed = selected(&f);
        if !restart {
            renewed.link.as_mut().unwrap().generation = 8;
        }
        port.redetect(renewed).unwrap();
        assert_eq!(
            port.submit(call(2, InstallerRequest::Release, 1000)),
            Err(CallFailure::InvalidCall(ContractError::InvalidValue))
        );
        port.submit(call(3, InstallerRequest::Release, 1000))
            .unwrap();
        assert_eq!(
            drain(&mut port, 1)[0].result,
            Ok(DecodedReply::Acknowledged)
        );
        assert_eq!(
            server.commands().iter().filter(|c| *c == "release").count(),
            1
        );
    }
}

/// WP-4.33b: an in-place exec restart keeps the PID (4242) and the process start, and changes
/// only the instance id. A Status on the old admission is refused before any byte reaches the
/// agent, and the reply says it was never admitted (not Live): the caller has to settle that call,
/// not wait on it. A fresh admission then admits the new instance under the same PID.
#[test]
fn transport_in_place_restart_refuses_the_old_admission_unadmitted_and_admits_the_new_instance() {
    let f = Fixture::new();
    let current = Arc::new(Mutex::new(wire_status(&f)));
    let replies = current.clone();
    let _server = Server::new(
        &f,
        Arc::new(move |bytes, socket| {
            if command(bytes) == "status" {
                send_owned(socket, &line(&replies.lock().unwrap()));
            }
        }),
    );
    let mut port = MacAgentPort::new(selected(&f), Arc::new(|| 83)).unwrap();
    port.emulate_live();
    port.submit(call(1, InstallerRequest::Status, 1000))
        .unwrap();
    let first = drain(&mut port, 1).remove(0);
    assert!(matches!(
        first.result,
        Ok(DecodedReply::Status(StatusAdmission::Supported(_)))
    ));
    assert_eq!(first.source, ObservationSource::Live);
    // The same process execs itself: same PID and start, a new instance id.
    f.bootstrap(7, 0, "ready", 1);
    current.lock().unwrap()["result"]["installer"]["instance"]["id"] = 7.into();
    port.submit(call(2, InstallerRequest::Status, 1000))
        .unwrap();
    let stale = drain(&mut port, 1).remove(0);
    assert_eq!(stale.result, Err(CallFailure::Unavailable));
    assert_ne!(stale.source, ObservationSource::Live);
    port.redetect(selected(&f)).unwrap();
    port.emulate_live();
    port.submit(call(3, InstallerRequest::Status, 1000))
        .unwrap();
    let fresh = drain(&mut port, 1).remove(0);
    let Ok(DecodedReply::Status(StatusAdmission::Supported(health))) = &fresh.result else {
        panic!("the new instance is admitted");
    };
    assert_eq!(health.installer().instance.id, 7);
    assert_eq!(health.installer().instance.pid, 4242);
    assert_eq!(fresh.source, ObservationSource::Live);
}

#[test]
fn transport_wrong_status_identity_and_wrong_requested_peer_never_transmit_mutation() {
    for field in [
        "id",
        "pid",
        "uid",
        "exe",
        "runtime_dir",
        "started_unix_ms",
        "requested-peer",
    ] {
        let f = Fixture::new();
        let mut status = wire_status(&f);
        let instance = &mut status["result"]["installer"]["instance"];
        match field {
            "id" => instance[field] = 9.into(),
            "pid" => instance[field] = 4243.into(),
            "uid" => instance[field] = (f.runner.uid + 1).into(),
            "exe" => instance[field] = "/explicit/other/Crosspane".into(),
            "runtime_dir" => instance[field] = "/private/tmp/explicit-other/crosspane".into(),
            "started_unix_ms" => instance[field] = 1000.into(),
            "requested-peer" => {}
            _ => unreachable!(),
        }
        let status = line(&status);
        let server = Server::new(
            &f,
            Arc::new(move |bytes, socket| {
                assert_eq!(command(bytes), "status");
                send_owned(socket, &status);
            }),
        );
        let mut port = MacAgentPort::new(selected(&f), Arc::new(|| 97)).unwrap();
        let request = if field == "requested-peer" {
            InstallerRequest::Project {
                window: crosspane_types::id::WindowId(1),
                peer: crosspane_types::id::NodeId([0x33; 32]),
            }
        } else {
            InstallerRequest::Release
        };
        port.submit(call(1, request, 1000)).unwrap();
        assert_eq!(drain(&mut port, 1)[0].result, Err(CallFailure::Unavailable));
        assert!(server.commands().iter().all(|c| c == "status"));
    }
}

#[test]
fn transport_incomplete_health_remains_pending_and_never_authorizes_a_mutation() {
    let f = Fixture::new();
    let server = Server::new(
        &f,
        Arc::new(|bytes, socket| {
            assert_eq!(command(bytes), "status");
            send_owned(socket, b"{\"ok\":true,\"result\":{}}\n");
        }),
    );
    let mut port = MacAgentPort::new(selected(&f), Arc::new(|| 103)).unwrap();
    port.submit(call(1, InstallerRequest::Status, 1000))
        .unwrap();
    assert_eq!(
        drain(&mut port, 1)[0].result,
        Ok(DecodedReply::Status(
            StatusAdmission::PendingHealthContract(agent_contract::PendingHealthReason::Absent)
        ))
    );
    port.submit(call(2, InstallerRequest::Release, 1000))
        .unwrap();
    assert_eq!(drain(&mut port, 1)[0].result, Err(CallFailure::Unavailable));
    assert_eq!(server.commands(), ["status", "status"]);
}

#[test]
fn transport_endpoint_replacement_after_connect_or_before_mutation_write_transmits_no_bytes() {
    for connection in [1, 2] {
        let f = Fixture::new();
        let path = f.io.target().socket_path();
        let home = f.home.clone();
        let sockets = Arc::new(Mutex::new(Vec::<UnixListener>::new()));
        let held = sockets.clone();
        let connected = Arc::new(AtomicU64::new(0));
        let count = connected.clone();
        *wire::HOOK.lock().unwrap() = Some(Arc::new(move |selected, stage| {
            if selected == home
                && stage == "connected"
                && count.fetch_add(1, Ordering::AcqRel) + 1 == connection
            {
                fs::remove_file(&path).unwrap();
                held.lock()
                    .unwrap()
                    .push(UnixListener::bind(&path).unwrap());
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            }
        }));
        let status = line(&wire_status(&f));
        let server = Server::new(
            &f,
            Arc::new(move |bytes, socket| {
                assert_eq!(command(bytes), "status");
                send_owned(socket, &status);
            }),
        );
        let mut port = MacAgentPort::new(selected(&f), Arc::new(|| 109)).unwrap();
        port.submit(call(1, InstallerRequest::Release, 5000))
            .unwrap();
        assert_eq!(drain(&mut port, 1)[0].result, Err(CallFailure::Unavailable));
        wait_for(|| server.requests.lock().unwrap().len() == connection as usize);
        assert!(server.requests.lock().unwrap().last().unwrap().is_empty());
        assert_eq!(
            server.commands(),
            if connection == 1 {
                vec![]
            } else {
                vec!["status"]
            }
        );
        *wire::HOOK.lock().unwrap() = None;
    }
}

#[test]
fn transport_unsafe_runtime_foreign_selection_and_stale_socket_are_refused_before_connect() {
    for kind in ["runtime", "selection", "socket"] {
        let f = Fixture::new();
        let other = Fixture::new();
        let mut selection = selected(&f);
        let replacement = match kind {
            "runtime" => {
                fs::set_permissions(&f.runtime, fs::Permissions::from_mode(0o777)).unwrap();
                None
            }
            "selection" => {
                selection.instance = Arc::new(other.admitted());
                None
            }
            "socket" => {
                let path = f.io.target().socket_path();
                fs::remove_file(&path).unwrap();
                let listener = UnixListener::bind(&path).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
                Some(listener)
            }
            _ => unreachable!(),
        };
        f._listener.set_nonblocking(true).unwrap();
        if let Some(listener) = &replacement {
            listener.set_nonblocking(true).unwrap();
        }
        let mut port = MacAgentPort::new(selection, Arc::new(|| 109)).unwrap();
        port.submit(call(1, InstallerRequest::Release, 1000))
            .unwrap();
        assert_eq!(drain(&mut port, 1)[0].result, Err(CallFailure::Unavailable));
        assert!(matches!(f._listener.accept(),Err(e)if e.kind()==std::io::ErrorKind::WouldBlock));
        if let Some(listener) = &replacement {
            assert!(matches!(listener.accept(),Err(e)if e.kind()==std::io::ErrorKind::WouldBlock));
        }
        fs::set_permissions(&f.runtime, fs::Permissions::from_mode(0o700)).unwrap();
    }
}

#[test]
fn transport_support_revocation_and_post_reply_restart_cannot_report_admitted_success() {
    for revoked in [true, false] {
        let f = Fixture::new();
        let selection = selected(&f);
        let status = line(&wire_status(&f));
        let bootstrap = f.runtime.join("bootstrap.json");
        let runtime = f.runtime.clone();
        let server = Server::new(
            &f,
            Arc::new(move |bytes_received, socket| {
                assert_eq!(command(bytes_received), "status");
                if !revoked {
                    let changed = serde_json::json!({"schema_version":1,"instance_id":7,"pid":4242,"started_unix_ms":0,
                    "phase":"ready","phase_seq":1,"keystore":null,"reason":null,"runtime_dir":runtime});
                    bytes(&bootstrap, &serde_json::to_vec(&changed).unwrap(), 0o600);
                }
                send_owned(socket, &status);
            }),
        );
        if revoked {
            selection.support.revoke();
        }
        let mut port = MacAgentPort::new(selection, Arc::new(|| 113)).unwrap();
        port.submit(call(1, InstallerRequest::Release, 1000))
            .unwrap();
        let reply = drain(&mut port, 1).pop().unwrap();
        assert_eq!(reply.result, Err(CallFailure::Unavailable));
        assert_eq!(reply.source, ObservationSource::Demo);
        assert!(!server.commands().iter().any(|c| c == "release"));
    }
}

#[test]
fn transport_shutdown_cancels_owned_pending_mutation_and_queued_calls_without_join_or_resend() {
    let f = Fixture::new();
    let status = line(&wire_status(&f));
    let entered = Arc::new(AtomicBool::new(false));
    let active = entered.clone();
    let release = Arc::new(AtomicBool::new(false));
    let released = release.clone();
    let server = Server::new(
        &f,
        Arc::new(move |bytes, socket| {
            if command(bytes) == "status" {
                send_owned(socket, &status);
            } else {
                assert_eq!(command(bytes), "release");
                active.store(true, Ordering::Release);
                while !released.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }),
    );
    let mut port = MacAgentPort::new(selected(&f), Arc::new(|| 127)).unwrap();
    let _unwind_release = ReleaseOnDrop(release.clone());
    port.submit(call(1, InstallerRequest::Release, 1000))
        .unwrap();
    port.submit(call(2, InstallerRequest::Status, 1000))
        .unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    while !entered.load(Ordering::Acquire) {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(1));
    }
    let begin = Instant::now();
    port.shutdown();
    assert!(begin.elapsed() < Duration::from_millis(100));
    assert!(
        drain(&mut port, 2)
            .iter()
            .all(|r| r.result == Err(CallFailure::TimeoutOutcomeUnknown))
    );
    release.store(true, Ordering::Release);
    assert_eq!(
        port.submit(call(3, InstallerRequest::Status, 1000)),
        Err(CallFailure::Unavailable)
    );
    assert_eq!(port.redetect(selected(&f)), Err(NativeError::Unavailable));
    assert_eq!(server.commands(), ["status", "release"]);
}

#[test]
fn transport_typed_dial_pairing_and_id_exhaustion_never_substitute_requests() {
    for request in [
        InstallerRequest::Dial {
            addr: "[::1]:47811".parse().unwrap(),
        },
        InstallerRequest::PairJoin {
            addr: "192.0.2.1:47811".parse().unwrap(),
            allow_input: false,
        },
    ] {
        let f = Fixture::new();
        let server = basic_server(&f, b"{\"ok\":true}\n");
        let mut port = MacAgentPort::new(selected(&f), Arc::new(|| 131)).unwrap();
        let expected = agent_contract::encode_request(&request).unwrap();
        port.submit(call(u64::MAX, request, 1000)).unwrap();
        assert_eq!(
            drain(&mut port, 1)[0].result,
            Ok(DecodedReply::Acknowledged)
        );
        assert_eq!(server.requests.lock().unwrap()[1], expected);
        assert_eq!(
            port.submit(call(u64::MAX, InstallerRequest::Status, 1000)),
            Err(CallFailure::InvalidCall(ContractError::IdExhausted))
        );
    }
}
#[test]
fn transport_live_source_requires_admission_and_scratch_never_acquires_live_authority() {
    assert_eq!(
        wire::reply_source(false, ObservationSource::Live),
        ObservationSource::Demo
    );
    assert_eq!(
        wire::reply_source(true, ObservationSource::Live),
        ObservationSource::Live
    );
    assert_eq!(
        wire::reply_source(false, ObservationSource::Demo),
        ObservationSource::Demo
    );
    assert_eq!(
        wire::reply_source(true, ObservationSource::Demo),
        ObservationSource::Demo
    );
}

struct Lookup {
    answers: Mutex<NativeResult<Vec<std::net::SocketAddr>>>,
    calls: Mutex<Vec<(String, u16)>>,
    blocked: AtomicBool,
    entered: AtomicBool,
    finished: AtomicBool,
}
impl HostLookup for Lookup {
    fn lookup(
        &self,
        host: &str,
        port: u16,
        _: &Deadline,
    ) -> NativeResult<Vec<std::net::SocketAddr>> {
        self.calls.lock().unwrap().push((host.into(), port));
        self.entered.store(true, Ordering::Release);
        let until = Instant::now() + Duration::from_secs(5);
        while self.blocked.load(Ordering::Acquire) {
            if Instant::now() >= until {
                self.finished.store(true, Ordering::Release);
                return Err(NativeError::Unavailable);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let answers = self.answers.lock().unwrap().clone();
        self.finished.store(true, Ordering::Release);
        answers
    }
}
fn lookup(answers: NativeResult<Vec<std::net::SocketAddr>>) -> Arc<Lookup> {
    Arc::new(Lookup {
        answers: Mutex::new(answers),
        calls: Mutex::default(),
        blocked: AtomicBool::new(false),
        entered: AtomicBool::new(false),
        finished: AtomicBool::new(false),
    })
}
fn resolve(resolver: &mut BoundedResolver) -> NativeResult<std::net::SocketAddr> {
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(result) = resolver.poll() {
            return result;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(1));
    }
}
#[test]
fn resolver_literals_never_lookup_and_hostnames_are_explicit_normalized_bounded_and_typed() {
    let answers = vec![
        "192.0.2.8:47811".parse().unwrap(),
        "192.0.2.1:47811".parse().unwrap(),
        "192.0.2.1:47811".parse().unwrap(),
    ];
    let lookup = lookup(Ok(answers));
    let clock = Arc::new(FakeClock::default());
    let mut resolver = BoundedResolver::injected(lookup.clone(), clock);
    for input in ["[::1]:47811", "192.0.2.9:47811"] {
        resolver.submit(input, 1000).unwrap();
        assert_eq!(resolve(&mut resolver).unwrap(), input.parse().unwrap());
    }
    assert!(lookup.calls.lock().unwrap().is_empty());
    resolver
        .submit("Selected-Peer.EXAMPLE:47811", 1000)
        .unwrap();
    assert_eq!(
        resolve(&mut resolver).unwrap(),
        "192.0.2.1:47811".parse().unwrap()
    );
    assert_eq!(
        lookup.calls.lock().unwrap().as_slice(),
        [("selected-peer.example".into(), 47811)]
    );
    assert!(!format!("{resolver:?}").contains("selected-peer"));
}
#[test]
fn resolver_bad_inputs_answers_and_deadlines_remain_explicit_errors() {
    let fake = lookup(Ok(vec![]));
    let clock = Arc::new(FakeClock::default());
    let mut resolver = BoundedResolver::injected(fake.clone(), clock.clone());
    for input in [
        "",
        "hostname",
        "bad name:47811",
        "-bad:47811",
        "bad-:47811",
        "bad..name:47811",
        "host:0",
        "host:65536",
        "host:\n47811",
        "[::1]:0",
    ] {
        assert_eq!(resolver.submit(input, 1000), Err(NativeError::Invalid));
    }
    assert_eq!(
        resolver.submit(&format!("{}:47811", "a".repeat(254)), 1000),
        Err(NativeError::Invalid)
    );
    assert_eq!(resolver.submit("host:47811", 0), Err(NativeError::Invalid));
    assert_eq!(
        resolver.submit("host:47811", 5001),
        Err(NativeError::Invalid)
    );
    assert!(fake.calls.lock().unwrap().is_empty());
    for (answers, error) in [
        (Ok(vec![]), NativeError::Unavailable),
        (Err(NativeError::Unavailable), NativeError::Unavailable),
        (
            Ok(vec!["192.0.2.1:1".parse().unwrap()]),
            NativeError::Oversize,
        ),
        (
            Ok(vec!["192.0.2.1:47811".parse().unwrap(); 17]),
            NativeError::Oversize,
        ),
    ] {
        let mut resolver = BoundedResolver::injected(lookup(answers), clock.clone());
        resolver.submit("host:47811", 1000).unwrap();
        assert_eq!(resolve(&mut resolver), Err(error));
    }
    resolver.submit("192.0.2.1:47811", 100).unwrap();
    clock.set(100);
    assert_eq!(resolver.poll(), Some(Err(NativeError::Timeout)));
}
#[test]
fn resolver_cancelled_noncooperative_lookup_retains_four_slots_and_never_delivers_late_answer() {
    let mut lookups = Vec::new();
    let mut resolvers = Vec::new();
    for _ in 0..4 {
        let fake = lookup(Ok(vec!["192.0.2.1:47811".parse().unwrap()]));
        fake.blocked.store(true, Ordering::Release);
        let mut resolver = BoundedResolver::injected(fake.clone(), Arc::new(FakeClock::default()));
        resolver
            .submit("explicit-owned-test.example:47811", 1000)
            .unwrap();
        wait_for(|| fake.entered.load(Ordering::Acquire));
        assert_eq!(resolver.submit("other:47811", 1000), Err(NativeError::Busy));
        resolver.cancel();
        assert_eq!(resolver.poll(), Some(Err(NativeError::Cancelled)));
        assert!(resolver.poll().is_none());
        lookups.push(fake);
        resolvers.push(resolver);
    }
    let fifth = lookup(Ok(vec![]));
    let mut fifth_resolver =
        BoundedResolver::injected(fifth.clone(), Arc::new(FakeClock::default()));
    assert_eq!(
        fifth_resolver.submit("fifth:47811", 1000),
        Err(NativeError::Busy)
    );
    assert!(fifth.calls.lock().unwrap().is_empty());
    for fake in &lookups {
        fake.blocked.store(false, Ordering::Release);
    }
    wait_for(|| {
        lookups
            .iter()
            .all(|fake| fake.finished.load(Ordering::Acquire))
    });
    // Wait for actual slot release, then occupy all four again with DISTINCT answers.
    for (i, (resolver, fake)) in resolvers.iter_mut().zip(&lookups).enumerate() {
        assert!(resolver.poll().is_none());
        fake.entered.store(false, Ordering::Release);
        fake.finished.store(false, Ordering::Release);
        fake.blocked.store(true, Ordering::Release);
        *fake.answers.lock().unwrap() =
            Ok(vec![format!("192.0.2.{}:47811", i + 2).parse().unwrap()]);
        wait_for(
            || match resolver.submit("second-owned-test.example:47811", 1000) {
                Ok(()) => true,
                Err(NativeError::Busy) => false,
                other => panic!("unexpected capacity recovery {other:?}"),
            },
        );
        wait_for(|| fake.entered.load(Ordering::Acquire));
    }
    assert_eq!(
        fifth_resolver.submit("still-fifth:47811", 1000),
        Err(NativeError::Busy)
    );
    for fake in &lookups {
        fake.blocked.store(false, Ordering::Release);
    }
    wait_for(|| {
        lookups
            .iter()
            .all(|fake| fake.finished.load(Ordering::Acquire))
    });
    for (i, (resolver, fake)) in resolvers.iter_mut().zip(&lookups).enumerate() {
        assert_eq!(
            resolve(resolver),
            Ok(format!("192.0.2.{}:47811", i + 2).parse().unwrap())
        );
        assert_eq!(fake.calls.lock().unwrap().len(), 2);
        assert!(resolver.poll().is_none()); // No cancelled old result contaminates this request.
    }
}
#[test]
fn transport_cancel_or_expiry_inside_final_endpoint_walk_sends_zero_mutation_bytes() {
    for cancel in [false, true] {
        let f = Fixture::new();
        let enabled = Arc::new(AtomicBool::new(false));
        let pausing = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(AtomicBool::new(false));
        let resume = Arc::new(AtomicBool::new(false));
        let walks = Arc::new(AtomicU64::new(0));
        let limit = Arc::new(AtomicU64::new(u64::MAX));
        let runtime = f.runtime.clone();
        let active = enabled.clone();
        let pause = pausing.clone();
        let count = walks.clone();
        let last = limit.clone();
        let reached = entered.clone();
        let resumed = resume.clone();
        let io = f.hooked(Arc::new(move |stage, path, value| {
            if stage == "walk" && path == runtime && active.load(Ordering::Acquire) {
                let n = count.fetch_add(1, Ordering::AcqRel) + 1;
                if pause.load(Ordering::Acquire) && n == last.load(Ordering::Acquire) {
                    reached.store(true, Ordering::Release);
                    wait_for(|| resumed.load(Ordering::Acquire));
                }
            }
            Ok(value)
        }));
        let mut selection = selected(&f);
        selection.io = io;
        enabled.store(true, Ordering::Release);
        selection
            .instance
            .endpoint()
            .revalidate(&selection.io)
            .unwrap();
        limit.store(walks.load(Ordering::Acquire), Ordering::Release);
        assert!(limit.load(Ordering::Acquire) > 0);
        enabled.store(false, Ordering::Release);
        walks.store(0, Ordering::Release);
        let home = f.home.clone();
        let validations = Arc::new(AtomicU64::new(0));
        let checks = validations.clone();
        *wire::HOOK.lock().unwrap() = Some(Arc::new(move |selected, stage| {
            if selected == home
                && stage == "write_validation"
                && checks.fetch_add(1, Ordering::AcqRel) + 1 == 2
            {
                pausing.store(true, Ordering::Release);
                enabled.store(true, Ordering::Release);
            }
        }));
        let server = basic_server(&f, b"{\"ok\":true}\n");
        let _unwind = ReleaseOnDrop(resume.clone());
        let mut port = MacAgentPort::new(selection, Arc::new(|| 151)).unwrap();
        port.emulate_live();
        port.submit(call(1, InstallerRequest::Release, 5000))
            .unwrap();
        wait_for(|| entered.load(Ordering::Acquire));
        if cancel {
            port.shutdown();
        } else {
            f.clock.set(5001);
        }
        resume.store(true, Ordering::Release);
        let reply = drain(&mut port, 1).pop().unwrap();
        assert_eq!(reply.result, Err(CallFailure::TimeoutOutcomeUnknown));
        assert_eq!(reply.source, ObservationSource::Live);
        wait_for(|| server.requests.lock().unwrap().len() == 2);
        assert!(server.requests.lock().unwrap()[1].is_empty());
        assert_eq!(server.commands(), ["status"]);
        *wire::HOOK.lock().unwrap() = None;
    }
}

#[test]
fn transport_partial_mutation_prefix_is_unknown_and_never_reconnected_or_resent() {
    let f = Fixture::new();
    let request = InstallerRequest::Place {
        placements: (0..128)
            .map(|display| agent_contract::Placement {
                node: crosspane_types::id::NodeId([0x22; 32]),
                display,
                origin_mm: [1.234567890123456e200, -1.234567890123456e200],
            })
            .collect(),
    };
    let expected = agent_contract::encode_request(&request).unwrap();
    let status = line(&wire_status(&f));
    let clock = f.clock.clone();
    let prefix = Arc::new(AtomicBool::new(false));
    let saw_prefix = prefix.clone();
    let server = Server::reader(
        f._listener.try_clone().unwrap(),
        Arc::new(move |bytes, socket| {
            if !bytes.ends_with(b"\n") {
                assert!(!saw_prefix.swap(true, Ordering::AcqRel));
                clock.set(5001); // The owned peer closes after a nonempty, incomplete mutation.
            } else if command(bytes) == "status" {
                send_owned(socket, &status);
            } else {
                assert_eq!(command(bytes), "panic");
                send_owned(socket, b"{\"ok\":true}\n");
            }
        }),
        None,
        true,
    );
    let home = f.home.clone();
    let writes = Arc::new(AtomicU64::new(0));
    let count = writes.clone();
    let received = prefix.clone();
    *wire::HOOK.lock().unwrap() = Some(Arc::new(move |selected, stage| {
        if selected == home && stage == "written" && count.fetch_add(1, Ordering::AcqRel) + 1 == 2 {
            wait_for(|| received.load(Ordering::Acquire));
        }
    }));
    let mut port = MacAgentPort::new(selected(&f), Arc::new(|| 157)).unwrap();
    port.emulate_live();
    port.submit(call(1, request, 5000)).unwrap();
    let reply = drain(&mut port, 1).pop().unwrap();
    assert_eq!(reply.result, Err(CallFailure::TimeoutOutcomeUnknown));
    assert_eq!(reply.source, ObservationSource::Live);
    let received = server.requests.lock().unwrap()[1].clone();
    assert!(!received.is_empty() && received.len() < expected.len());
    assert_eq!(received, expected[..received.len()]);
    port.submit(call(2, InstallerRequest::Panic, 1000)).unwrap();
    assert_eq!(drain(&mut port, 1)[0].result, Err(CallFailure::Unavailable));
    assert_eq!(server.requests.lock().unwrap().len(), 2);
    port.redetect(selected(&f)).unwrap();
    port.emulate_live();
    port.submit(call(3, InstallerRequest::Panic, 1000)).unwrap();
    assert_eq!(
        drain(&mut port, 1)[0].result,
        Ok(DecodedReply::Acknowledged)
    );
    assert_eq!(server.requests.lock().unwrap().len(), 5);
    *wire::HOOK.lock().unwrap() = None;
}

#[test]
fn transport_admitted_refusal_survives_followup_status_timeout_without_uncertainty_latch() {
    let f = Fixture::new();
    let status = line(&wire_status(&f));
    let clock = f.clock.clone();
    let queries = Arc::new(AtomicU64::new(0));
    let count = queries.clone();
    let server = Server::new(
        &f,
        Arc::new(move |bytes, socket| match command(bytes).as_str() {
            "status" => {
                if count.fetch_add(1, Ordering::AcqRel) + 1 == 2 {
                    clock.set(1001);
                } else {
                    send_owned(socket, &status);
                }
            }
            "release" => send_owned(
                socket,
                b"{\"ok\":false,\"result\":null,\"error\":\"revision_conflict\"}\n",
            ),
            "panic" => send_owned(socket, b"{\"ok\":true}\n"),
            other => panic!("unexpected owned command {other}"),
        }),
    );
    let mut port = MacAgentPort::new(selected(&f), Arc::new(|| 163)).unwrap();
    port.emulate_live();
    port.submit(call(1, InstallerRequest::Release, 1000))
        .unwrap();
    let reply = drain(&mut port, 1).pop().unwrap();
    assert_eq!(
        reply.result,
        Err(CallFailure::Refused(AgentRefusal::RevisionConflict))
    );
    assert_eq!(reply.source, ObservationSource::Live);
    port.submit(call(2, InstallerRequest::Panic, 1000)).unwrap();
    assert_eq!(
        drain(&mut port, 1)[0].result,
        Ok(DecodedReply::Acknowledged)
    );
    assert_eq!(
        server.commands(),
        ["status", "release", "status", "status", "panic", "status"]
    );
}

#[test]
fn admitted_unknown_outcomes_drive_settings_detection_without_replay() {
    use crosspane_installer::settings_transition::*;
    let f = Fixture::new();
    let status_value = wire_status(&f);
    let status = line(&status_value);
    let clock = f.clock.clone();
    let server = Server::new(
        &f,
        Arc::new(move |bytes, socket| match command(bytes).as_str() {
            "status" => send_owned(socket, &status),
            "settings_update" | "release" => clock.set(5001),
            other => panic!("unexpected owned command {other}"),
        }),
    );
    let mut port = MacAgentPort::new(selected(&f), Arc::new(|| 10)).unwrap();
    port.emulate_live(); // In-memory source seam only; all native facts and sockets remain explicit scratch.

    let StatusAdmission::Supported(health) =
        agent_contract::parse_status(&line(&status_value), AgentPlatform::Macos).unwrap()
    else {
        panic!("missing fixture health")
    };
    let mut transition = SettingsTransition::new(crosspane_types::id::NodeId([0x11; 32]), 17);
    transition
        .detected(&health, ObservationSource::Live, 1, 1, 17)
        .unwrap();
    port.submit(transition.consent_update(1, 17, true).unwrap())
        .unwrap();
    let reply = drain(&mut port, 1).pop().unwrap();
    assert_eq!(reply.source, ObservationSource::Live);
    assert_eq!(reply.result, Err(CallFailure::TimeoutOutcomeUnknown));
    let outcome = transition.reply(reply, 10).unwrap();
    assert!(outcome.detect_after_unknown);
    assert_eq!(transition.state(), &SettingsTransitionState::NeedsDetection);
    assert!(transition.consent_update(2, 17, true).is_err());
    transition
        .detected(&health, ObservationSource::Live, 11, 11, 18)
        .unwrap();
    assert!(transition.consent_update(2, 17, true).is_err());
    assert!(transition.consent_update(2, 18, true).is_ok());
    assert_eq!(server.commands(), ["status", "settings_update"]);
}

#[test]
fn owned_server_teardown_releases_waiting_handler_during_assertion_unwind() {
    let f = Fixture::new();
    let release = Arc::new(AtomicBool::new(false));
    let released = release.clone();
    let entered = Arc::new(AtomicBool::new(false));
    let active = entered.clone();
    let status = line(&wire_status(&f));
    let server = Server::new(
        &f,
        Arc::new(move |bytes, socket| {
            if command(bytes) == "status" {
                send_owned(socket, &status);
            } else {
                active.store(true, Ordering::Release);
                wait_for(|| released.load(Ordering::Acquire));
            }
        }),
    );
    let mut port = MacAgentPort::new(selected(&f), Arc::new(|| 173)).unwrap();
    port.submit(call(1, InstallerRequest::Release, 5000))
        .unwrap();
    wait_for(|| entered.load(Ordering::Acquire));
    let begin = Instant::now();
    let failure = std::panic::catch_unwind(|| {
        let _unwind = ReleaseOnDrop(release.clone());
        panic!("owned simulated assertion");
    });
    assert!(failure.is_err());
    drop(port);
    drop(server);
    assert!(begin.elapsed() < Duration::from_secs(3));
}

#[test]
fn finite_alias_table_accepts_only_var_and_tmp_and_rejects_dirty_spellings() {
    assert_eq!(
        admitted_spelling(Path::new("/var/folders/a/T")).unwrap(),
        Path::new("/private/var/folders/a/T")
    );
    assert_eq!(
        admitted_spelling(Path::new("/tmp/owned")).unwrap(),
        Path::new("/private/tmp/owned")
    );
    assert_eq!(
        admitted_spelling(Path::new("/various/a")).unwrap(),
        Path::new("/various/a")
    );
    for p in [
        "relative",
        "/tmp/../owned",
        "/tmp//owned",
        "/tmp/./owned",
        "/tmp/owned/",
        "/tmp/a\n",
    ] {
        assert!(admitted_spelling(Path::new(p)).is_err(), "{p:?}");
    }
    let f = Fixture::new();
    let aliases = TargetPaths {
        uid: f.runner.uid,
        home: PathBuf::from(f.home.to_string_lossy().replacen("/private/tmp", "/tmp", 1)),
        gui_tmpdir: PathBuf::from(f.tmp.to_string_lossy().replacen("/private/tmp", "/tmp", 1)),
        runtime_override: None,
        payload_root: f.io.target().paths().payload_root.clone(),
    };
    let io = MacNativeIo::new(
        MacTarget::scratch(aliases).unwrap(),
        f.runner.clone(),
        f.support.clone(),
        f.signatures.clone(),
        f.clock.clone(),
    )
    .unwrap();
    assert_eq!(
        io.socket_endpoint().unwrap().identity().inode,
        f.io.socket_endpoint().unwrap().identity().inode
    );
}
#[test]
fn runtime_override_is_explicit_confined_and_private() {
    let f = Fixture::new();
    let mut paths = f.io.target().paths().clone();
    paths.runtime_override = Some(f.home.join("private-runtime"));
    directory(paths.runtime_override.as_ref().unwrap());
    let target = MacTarget::scratch(paths.clone()).unwrap();
    assert_eq!(
        target.runtime_dir(),
        paths.runtime_override.as_deref().unwrap()
    );
    paths.runtime_override = Some("/private/tmp/unrelated-runtime".into());
    assert!(MacTarget::scratch(paths.clone()).is_err());
    paths.uid = 0;
    assert!(MacTarget::scratch(paths).is_err());
    fs::set_permissions(&f.runtime, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(f.io.validate_target().is_err());
}
#[test]
fn every_ancestor_must_be_owned_safe_and_no_follow() {
    let f = Fixture::new();
    fs::set_permissions(&f.root, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(f.io.validate_target().is_err());
    fs::set_permissions(&f.root, fs::Permissions::from_mode(0o700)).unwrap();
    fs::rename(&f.tmp, f.root.join("old-tmp")).unwrap();
    symlink(f.root.join("old-tmp"), &f.tmp).unwrap();
    assert!(f.io.validate_target().is_err());
}
#[test]
fn admitted_socket_rejects_parent_replacement_before_transmission() {
    let f = Fixture::new();
    let endpoint = f.io.socket_endpoint().unwrap();
    fs::rename(&f.runtime, f.tmp.join("old-runtime")).unwrap();
    directory(&f.runtime);
    let _unrelated = UnixListener::bind(f.runtime.join("agent.sock")).unwrap();
    fs::set_permissions(
        f.runtime.join("agent.sock"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert_eq!(endpoint.revalidate(&f.io), Err(NativeError::Foreign));
    assert!(f.runner.calls.lock().unwrap().is_empty());
}
#[test]
fn admitted_socket_rejects_substituted_inode_and_wrong_type() {
    let f = Fixture::new();
    let endpoint = f.io.socket_endpoint().unwrap();
    fs::remove_file(f.runtime.join("agent.sock")).unwrap();
    let _other = UnixListener::bind(f.runtime.join("agent.sock")).unwrap();
    fs::set_permissions(
        f.runtime.join("agent.sock"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert_eq!(endpoint.revalidate(&f.io), Err(NativeError::Foreign));
    fs::remove_file(f.runtime.join("agent.sock")).unwrap();
    bytes(&f.runtime.join("agent.sock"), b"not socket", 0o600);
    assert!(f.io.socket_endpoint().is_err());
}
#[test]
fn no_follow_reads_reject_symlink_hardlink_nonprivate_and_oversize() {
    let f = Fixture::new();
    let path = f.io.target().installer_dir().join("observation.json");
    bytes(&path, b"1234", 0o600);
    assert_eq!(f.io.read(&path, 4, true, &f.deadline()).unwrap(), b"1234");
    assert_eq!(
        f.io.read(&path, 3, true, &f.deadline()),
        Err(NativeError::Oversize)
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(f.io.read(&path, 4, true, &f.deadline()).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::hard_link(&path, f.io.target().installer_dir().join("linked")).unwrap();
    assert!(f.io.read(&path, 4, true, &f.deadline()).is_err());
    let link = f.io.target().installer_dir().join("symlink");
    symlink(&path, &link).unwrap();
    assert!(f.io.read(&link, 4, true, &f.deadline()).is_err());
    let mut identity = f.io.metadata(&path).unwrap().unwrap();
    identity.links = 1;
    identity.uid += 1;
    assert_eq!(
        identity.regular(f.runner.uid, true),
        Err(NativeError::Foreign)
    );
}
#[test]
fn compatibility_is_separate_and_noninteractive_facts_cannot_mutate() {
    let f = Fixture::new();
    let baseline = f.support.0.lock().unwrap().clone();
    let main = f.main();
    let mut changes = Vec::new();
    let mut x = baseline.clone();
    x.macos_major = 25;
    changes.push(x);
    let mut x = baseline.clone();
    x.apple_silicon = false;
    changes.push(x);
    let mut x = baseline.clone();
    x.gui.active = false;
    changes.push(x);
    let mut x = baseline.clone();
    x.gui.console_uid = None;
    changes.push(x);
    let mut x = baseline.clone();
    x.gui.interactive_uid = None;
    changes.push(x);
    let mut x = baseline.clone();
    x.gui.console_uid = Some(f.runner.uid + 1);
    changes.push(x);
    let mut x = baseline.clone();
    x.gui.interactive_session = "ssh-session".into();
    changes.push(x);
    let mut x = baseline.clone();
    x.gui.console_session.clear();
    changes.push(x);
    let mut x = baseline.clone();
    x.gui_tmpdir = f.home.clone();
    changes.push(x);
    for (index, changed) in changes.into_iter().enumerate() {
        *f.support.0.lock().unwrap() = changed;
        let admitted = f.io.admit_support(&main, &f.deadline());
        if index < 2 {
            let proof = admitted.unwrap();
            assert_eq!(
                proof.check_agent_compatibility(),
                Err(NativeError::Unsupported)
            );
            assert_eq!(
                f.io.execute(
                    &f.command(LaunchctlAction::Bootstrap),
                    Some(&proof),
                    &f.deadline()
                ),
                Err(NativeError::Unsupported)
            );
        } else {
            assert!(admitted.is_err());
        }
    }
    assert_eq!(
        f.io.execute(&f.command(LaunchctlAction::Bootstrap), None, &f.deadline()),
        Err(NativeError::Unsupported)
    );
    assert!(f.runner.calls.lock().unwrap().is_empty());
}
#[test]
fn support_revocation_expiry_and_changed_session_retire_mutation_authority() {
    let f = Fixture::new();
    let proof = f.proof();
    proof.revoke();
    assert!(
        f.io.execute(
            &f.command(LaunchctlAction::Bootout),
            Some(&proof),
            &f.deadline()
        )
        .is_err()
    );
    let proof = f.proof();
    f.clock.set(SUPPORT_LIFETIME_MS + 1);
    assert!(proof.check(&f.io, &f.deadline()).is_err());
    let proof = f.proof();
    f.support.0.lock().unwrap().gui.interactive_session = "new-session".into();
    assert!(proof.check(&f.io, &f.deadline()).is_err());
    assert!(f.runner.calls.lock().unwrap().is_empty());
}
#[test]
fn each_signature_fact_and_role_specific_entitlement_is_independently_required() {
    let f = Fixture::new();
    let expected = requirement(ArtifactRole::Agent);
    let baseline = signed(&expected);
    let mut cases = Vec::new();
    let mut x = baseline.clone();
    x.strict_verified = false;
    cases.push(x);
    let mut x = baseline.clone();
    x.ad_hoc = true;
    cases.push(x);
    let mut x = baseline.clone();
    x.apple_development = false;
    cases.push(x);
    let mut x = baseline.clone();
    x.hardened_runtime = false;
    cases.push(x);
    let mut x = baseline.clone();
    x.team_identifier.clear();
    cases.push(x);
    let mut x = baseline.clone();
    x.identifier = "other.identity".into();
    cases.push(x);
    let mut x = baseline.clone();
    x.designated_requirement = "different requirement".into();
    cases.push(x);
    let mut x = baseline.clone();
    x.entitlements.clear();
    cases.push(x);
    let mut x = baseline.clone();
    x.entitlements.insert("unexpected.entitlement".into(), true);
    cases.push(x);
    for observation in cases {
        f.signatures
            .overrides
            .lock()
            .unwrap()
            .insert(f.runner.exe.clone(), observation);
        assert!(
            f.io.admit_main_signature(&f.runner.exe, &expected, &f.deadline())
                .is_err()
        );
    }
    assert!(f.runner.calls.lock().unwrap().is_empty());
}
#[test]
fn artifact_team_is_compared_with_independent_main_not_manifest_or_helper() {
    let f = Fixture::new();
    let main = f.main();
    let helper =
        f.io.target()
            .app_path()
            .join("Contents/MacOS/fixture-helper");
    bytes(&helper, b"owned helper", 0o755);
    for role in [
        ArtifactRole::Settings,
        ArtifactRole::Installer,
        ArtifactRole::Ctl,
        ArtifactRole::Installer,
    ] {
        let approved = requirement(role);
        let accepted =
            f.io.admit_artifact_signature(&helper, &approved, &main, &f.deadline())
                .unwrap();
        assert_eq!(
            accepted.observation().team_identifier,
            main.observation().team_identifier
        );
        assert!(accepted.observation().entitlements.is_empty());
        let mut wrong = signed(&approved);
        wrong.team_identifier = "OTHER12345".into();
        f.signatures
            .overrides
            .lock()
            .unwrap()
            .insert(helper.clone(), wrong);
        assert!(
            f.io.admit_artifact_signature(&helper, &approved, &main, &f.deadline())
                .is_err()
        );
        f.signatures.overrides.lock().unwrap().remove(&helper);
    }
    fs::write(&f.runner.exe, b"replaced same path").unwrap();
    assert!(main.revalidate(&f.io).is_err());
}
#[test]
fn bounded_ps_uses_absolute_argv_c_locale_utc_and_selected_executable() {
    let f = Fixture::new();
    let admitted = f.admitted();
    assert_eq!(admitted.process().pid, 4242);
    assert_eq!(admitted.bootstrap().instance_id, u64::MAX);
    assert_eq!(admitted.source(), ObservationSource::Demo);
    for command in f.runner.calls.lock().unwrap().iter() {
        assert_eq!(command.program(), Path::new("/bin/ps"));
        assert_eq!(command.args()[0], "-o");
        assert_eq!(command.args()[2..], ["-p", "4242"]);
        assert_eq!(command.environment()["LC_ALL"], "C");
        assert_eq!(command.environment()["TZ"], "UTC");
        assert_eq!(command.environment()["HOME"], f.home.to_str().unwrap());
        assert_eq!(command.max_output(), 4096);
        assert!(!command.is_mutation());
    }
    admitted.admit_status(&f.status()).unwrap();
}
#[test]
fn ps_calendar_locale_and_scalars_are_strict() {
    assert_eq!(parse_ps_start(b"Thu Jan  1 00:00:00 1970\n").unwrap(), 0);
    assert_eq!(
        parse_ps_start(b"Fri Oct  2 00:00:00 2026\n").unwrap(),
        1_790_899_200_000
    );
    for bad in [
        "Wed Jan 1 00:00:00 1970",
        "Thu Jan 0 00:00:00 1970",
        "Thu Jan 1 24:00:00 1970",
        "Thu Jan 1 00:60:00 1970",
        "Thu Jan 1 00:00:60 1970",
        "Thu Jan 1 0:00:00 1970",
        "Thu Jan 1 00:00:00 1969",
        "Thu Jan 1 00:00:00 -1970",
        "Thu Jan 1 00:00:00 1970\nextra",
        "Thu\tJan 1 00:00:00 1970",
        "Do Jan 1 00:00:00 1970",
        "Thu Feb 30 00:00:00 2024",
    ] {
        assert!(parse_ps_start(bad.as_bytes()).is_err(), "{bad}");
    }
    assert_eq!(parse_ps_start(&[b' '; 129]), Err(NativeError::Oversize));
}
#[test]
fn ps_start_tolerance_is_exactly_two_seconds_and_name_alone_is_insufficient() {
    let f = Fixture::new();
    let main = f.main();
    for offset in [0, 1999, 2000] {
        f.bootstrap(u64::MAX, offset, "ready", 1);
        assert!(f.io.bootstrap(&main, &f.deadline()).is_ok());
    }
    f.bootstrap(u64::MAX, 2001, "ready", 1);
    assert!(f.io.bootstrap(&main, &f.deadline()).is_err());
    f.bootstrap(u64::MAX, 0, "ready", 1);
    *f.runner.reported_exe.lock().unwrap() = Some("Crosspane".into());
    assert!(f.io.process_identity(4242, &main, &f.deadline()).is_err());
    *f.runner.reported_exe.lock().unwrap() = Some("/other/Crosspane".into());
    assert!(f.io.process_identity(4242, &main, &f.deadline()).is_err());
    *f.runner.reported_exe.lock().unwrap() = None;
    *f.runner.reported_uid.lock().unwrap() = Some(f.runner.uid + 1);
    assert!(f.io.process_identity(4242, &main, &f.deadline()).is_err());
}
#[test]
fn waiting_keystore_is_lifecycle_truth_and_does_not_create_fallback_identity() {
    let f = Fixture::new();
    f.bootstrap(u64::MAX, 0, "waiting_for_keystore", 2);
    let admitted = f.admitted();
    assert_eq!(
        admitted.bootstrap().phase,
        BootstrapPhase::WaitingForKeystore
    );
    assert_eq!(admitted.bootstrap().keystore, None);
    assert!(!f.io.target().state_dir().join("keys").exists());
}
#[test]
fn bootstrap_replacement_mid_process_queries_cannot_rebind_instance() {
    let f = Fixture::new();
    let path = f.runtime.join("bootstrap.json");
    let runtime = f.runtime.clone();
    *f.runner.hook.lock().unwrap() = Some(Arc::new(move |_| {
        let data = serde_json::json!({"schema_version":1,"instance_id":7,"pid":4242,"started_unix_ms":0,"phase":"ready","phase_seq":2,"keystore":null,"reason":null,"runtime_dir":runtime});
        bytes(&path, &serde_json::to_vec(&data).unwrap(), 0o600);
    }));
    assert!(
        f.io.admit_instance(&f.proof(), &f.main(), &f.deadline())
            .is_err()
    );
}
#[test]
fn status_each_instance_binding_and_post_exchange_restart_must_match() {
    let f = Fixture::new();
    let admitted = f.admitted();
    let baseline = f.status();
    let mut cases = Vec::new();
    let mut x = baseline.clone();
    x.id = 1;
    cases.push(x);
    let mut x = baseline.clone();
    x.pid += 1;
    cases.push(x);
    let mut x = baseline.clone();
    x.uid = Some(x.uid.unwrap() + 1);
    cases.push(x);
    let mut x = baseline.clone();
    x.exe = "/other/Crosspane".into();
    cases.push(x);
    let mut x = baseline.clone();
    x.runtime_dir = f.tmp.to_string_lossy().into();
    cases.push(x);
    let mut x = baseline.clone();
    x.started_unix_ms += 1;
    cases.push(x);
    for status in cases {
        assert!(admitted.admit_status(&status).is_err());
    }
    admitted
        .revalidate(&f.io, &f.proof(), &f.deadline())
        .unwrap();
    f.bootstrap(8, 0, "ready", 2);
    assert!(
        admitted
            .revalidate(&f.io, &f.proof(), &f.deadline())
            .is_err()
    );
}
#[test]
fn clean_exit_requires_matching_receipt_and_actual_observed_exit_without_remote_claim() {
    let f = Fixture::new();
    let identity = f.admitted().process().clone();
    assert_eq!(
        f.io.exit_receipt(&identity, u64::MAX, &f.deadline())
            .unwrap(),
        None
    );
    let path = f.io.target().state_dir().join("last_exit.json");
    let write = |instance, clean, parking| {
        bytes(&path, &serde_json::to_vec(&serde_json::json!({"schema_version":1,"instance_id":instance,"stopped_unix_ms":1000,"clean":clean,"parking":parking,"input_journals_empty":true,"audio_stopped":true})).unwrap(), 0o600)
    };
    write(u64::MAX, true, "restored");
    assert!(
        f.io.exit_receipt(&identity, u64::MAX, &f.deadline())
            .is_err()
    );
    f.runner.absent.store(true, Ordering::Release);
    assert!(
        f.io.exit_receipt(&identity, u64::MAX, &f.deadline())
            .unwrap()
            .unwrap()
            .clean
    );
    write(9, true, "restored");
    assert!(
        f.io.exit_receipt(&identity, u64::MAX, &f.deadline())
            .is_err()
    );
    write(u64::MAX, false, "failed");
    assert!(
        !f.io
            .exit_receipt(&identity, u64::MAX, &f.deadline())
            .unwrap()
            .unwrap()
            .clean
    );
}
#[test]
fn confined_atomic_private_receipts_lock_and_bounded_inventory() {
    let f = Fixture::new();
    let proof = f.proof();
    let path = f.io.target().installer_dir().join("intent.json");
    let first =
        f.io.atomic_write(&proof, &path, b"intent-v1", None, &f.deadline())
            .unwrap();
    assert_eq!(first.mode & 0o777, 0o600);
    assert_eq!(
        f.io.read(&path, 100, true, &f.deadline()).unwrap(),
        b"intent-v1"
    );
    assert!(
        f.io.atomic_write(&proof, &path, b"overwrite", None, &f.deadline())
            .is_err()
    );
    let second =
        f.io.atomic_write(&proof, &path, b"intent-v2", Some(&first), &f.deadline())
            .unwrap();
    assert_ne!(second.inode, first.inode);
    let lock = f.io.lock(&proof, &f.deadline()).unwrap();
    assert!(matches!(
        f.io.lock(&proof, &f.deadline()),
        Err(NativeError::Busy)
    ));
    drop(lock);
    assert!(f.io.lock(&proof, &f.deadline()).is_ok());
    assert!(
        f.io.entries(&f.io.target().installer_dir(), 1, &f.deadline())
            .is_err()
    );
    assert!(
        f.io.entries(&f.io.target().installer_dir(), 4096, &f.deadline())
            .unwrap()
            .len()
            >= 2
    );
    for forbidden in [
        f.home.join(".ssh/authorized_keys"),
        f.io.target().state_dir().join("config.toml"),
        f.runtime.join("bootstrap.json"),
    ] {
        assert!(
            f.io.atomic_write(&proof, &forbidden, b"no", None, &f.deadline())
                .is_err()
        );
    }
    f.io.remove_owned_leaf(&proof, &path, &second, &f.deadline())
        .unwrap();
    assert!(!path.exists());
}
#[test]
fn stage_rename_preserves_existing_previous_and_refuses_unobserved_files() {
    let f = Fixture::new();
    let proof = f.proof();
    let stage = f.home.join("Applications/.Crosspane.app.crosspane-stage");
    directory(&stage);
    let previous = f
        .home
        .join("Applications/.Crosspane.app.crosspane-previous");
    directory(&previous);
    let observed = f.io.metadata(&stage).unwrap().unwrap();
    assert!(
        f.io.rename_owned(&proof, &stage, &previous, &observed, &f.deadline())
            .is_err()
    );
    assert!(stage.exists() && previous.exists());
    fs::remove_dir(&previous).unwrap();
    f.io.rename_owned(&proof, &stage, &previous, &observed, &f.deadline())
        .unwrap();
    assert!(previous.exists() && !stage.exists());
}
#[test]
fn native_queue_bounds_monotonic_ids_and_replies_without_caller_io() {
    let f = Fixture::new();
    f.runner.blocked.store(true, Ordering::Release);
    let mut queue = NativeCommandQueue::new(f.io.clone()).unwrap();
    let command = CommandSpec::new(
        f.io.target(),
        NativeOperation::Process {
            pid: 4242,
            field: PsField::Uid,
        },
    )
    .unwrap();
    for id in 1..=32 {
        queue
            .submit(NativeCall {
                id,
                command: command.clone(),
                timeout_ms: 5000,
                proof: None,
            })
            .unwrap();
    }
    assert_eq!(
        queue
            .submit(NativeCall {
                id: 33,
                command: command.clone(),
                timeout_ms: 5000,
                proof: None
            })
            .unwrap_err(),
        NativeError::Busy
    );
    f.clock.set(42);
    f.runner.blocked.store(false, Ordering::Release);
    let until = Instant::now() + Duration::from_secs(5);
    let mut replies = Vec::new();
    while replies.len() < 32 && Instant::now() < until {
        replies.extend(queue.poll());
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(replies.len(), 32);
    for r in replies {
        assert_eq!(r.source, ObservationSource::Demo);
        assert_eq!(r.observed_at_ms, 42);
        assert!(r.result.is_ok());
    }
    assert_eq!(f.runner.calls.lock().unwrap().len(), 32);
    assert_eq!(
        queue
            .submit(NativeCall {
                id: 32,
                command: command.clone(),
                timeout_ms: 5000,
                proof: None
            })
            .unwrap_err(),
        NativeError::Invalid
    );
    queue
        .submit(NativeCall {
            id: u64::MAX,
            command: command.clone(),
            timeout_ms: 5000,
            proof: None,
        })
        .unwrap();
    assert_eq!(
        queue
            .submit(NativeCall {
                id: 1,
                command,
                timeout_ms: 5000,
                proof: None
            })
            .unwrap_err(),
        NativeError::IdExhausted
    );
}
#[test]
fn mutation_timeout_is_unknown_and_never_automatically_resubmitted() {
    let f = Fixture::new();
    let proof = f.proof();
    let clock = f.clock.clone();
    *f.runner.hook.lock().unwrap() = Some(Arc::new(move |_| clock.set(5001)));
    assert_eq!(
        f.io.execute(
            &f.command(LaunchctlAction::Bootout),
            Some(&proof),
            &f.deadline()
        ),
        Err(NativeError::OutcomeUnknown)
    );
    assert_eq!(f.runner.calls.lock().unwrap().len(), 1);
}
#[test]
fn command_and_output_debug_never_prints_raw_responses_or_child_arguments() {
    assert!(
        !format!(
            "{:?}",
            SignatureQuery::VerifyRequirement("RAW-ARGUMENT-SENTINEL".into())
        )
        .contains("SENTINEL")
    );
    let f = Fixture::new();
    let output = CommandOutput {
        code: Some(0),
        stdout: b"KEY-CONTENTS-SENTINEL".to_vec(),
        stderr: b"RAW-RESPONSE-SENTINEL".to_vec(),
    };
    assert!(!format!("{output:?}").contains("SENTINEL"));
    let helper = f.io.target().app_path().join("Contents/MacOS/tutorial");
    bytes(&helper, b"owned fixture", 0o755);
    let signature =
        f.io.admit_artifact_signature(
            &helper,
            &requirement(ArtifactRole::Installer),
            &f.main(),
            &f.deadline(),
        )
        .unwrap();
    let command = CommandSpec::new(
        f.io.target(),
        NativeOperation::ControlledChild {
            signature: Box::new(signature),
            args: vec!["TYPED-CHARACTERS-SENTINEL".into()],
        },
    )
    .unwrap();
    assert!(!format!("{command:?}").contains("SENTINEL"));
    assert_eq!(
        SystemCommandRunner.run(&command, &f.deadline()),
        Err(NativeError::Unsupported)
    );
}

#[test]
fn endpoint_is_rechecked_after_owned_connect_and_before_any_mutation_byte() {
    use std::io::Read;
    let f = Fixture::new();
    let endpoint = f.io.socket_endpoint().unwrap();
    let _client = std::os::unix::net::UnixStream::connect(endpoint.path()).unwrap();
    let (mut accepted, _) = f._listener.accept().unwrap();
    accepted.set_nonblocking(true).unwrap();
    fs::rename(&f.runtime, f.tmp.join("old-runtime")).unwrap();
    directory(&f.runtime);
    let _replacement = UnixListener::bind(f.runtime.join("agent.sock")).unwrap();
    fs::set_permissions(
        f.runtime.join("agent.sock"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert_eq!(endpoint.revalidate(&f.io), Err(NativeError::Foreign));
    let mut buffer = [0; 1];
    assert_eq!(
        accepted.read(&mut buffer).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn socket_owner_and_writable_mode_are_independently_rejected() {
    let f = Fixture::new();
    let mut identity = f.io.socket_endpoint().unwrap().identity().clone();
    identity.uid += 1;
    assert_eq!(identity.socket(f.runner.uid), Err(NativeError::Foreign));
    fs::set_permissions(
        f.runtime.join("agent.sock"),
        fs::Permissions::from_mode(0o622),
    )
    .unwrap();
    assert!(f.io.socket_endpoint().is_err());
}

#[test]
fn explicit_readonly_payload_can_admit_fresh_support_before_installation() {
    let f = Fixture::new();
    let incoming =
        f.io.target()
            .paths()
            .payload_root
            .join("Crosspane.app/Contents/MacOS/Crosspane");
    bytes(&incoming, b"signed incoming main", 0o755);
    fs::remove_file(&f.runner.exe).unwrap();
    let signature =
        f.io.admit_main_signature(&incoming, &requirement(ArtifactRole::Agent), &f.deadline())
            .unwrap();
    let proof = f.io.admit_support(&signature, &f.deadline()).unwrap();
    let path = f.io.target().installer_dir().join("intent.json");
    f.io.atomic_write(&proof, &path, b"private-intent", None, &f.deadline())
        .unwrap();
    assert!(!f.runner.exe.exists());
    assert!(
        f.io.atomic_write(
            &proof,
            &incoming,
            b"must not mutate distribution source",
            Some(&f.io.metadata(&incoming).unwrap().unwrap()),
            &f.deadline()
        )
        .is_err()
    );
}

#[test]
fn interruption_after_private_write_keeps_previous_file_and_explicit_uncertainty() {
    let f = Fixture::new();
    let proof = f.proof();
    let path = f.io.target().installer_dir().join("intent.json");
    let before =
        f.io.atomic_write(&proof, &path, b"previous-usable", None, &f.deadline())
            .unwrap();
    let boundary = f.support.1.load(Ordering::Acquire) + 2;
    let clock = f.clock.clone();
    *f.support.2.lock().unwrap() = Some(Arc::new(move |count| {
        if count == boundary {
            clock.set(6000);
        }
    }));
    assert_eq!(
        f.io.atomic_write(&proof, &path, b"replacement", Some(&before), &f.deadline()),
        Err(NativeError::OutcomeUnknown)
    );
    assert_eq!(fs::read(&path).unwrap(), b"previous-usable");
    let leftovers: Vec<_> = fs::read_dir(f.io.target().installer_dir())
        .unwrap()
        .map(|p| p.unwrap())
        .filter(|p| p.file_name().to_string_lossy().contains(".crosspane-temp-"))
        .collect();
    assert_eq!(leftovers.len(), 1);
    assert_eq!(fs::read(leftovers[0].path()).unwrap(), b"replacement");
}

#[test]
fn staged_file_mode_directory_creation_and_owned_leaf_removal_are_confined() {
    let f = Fixture::new();
    let proof = f.proof();
    let stage = f.home.join("Applications/.Crosspane.app.crosspane-stage");
    f.io.create_directory(&proof, &stage, 0o700, &f.deadline())
        .unwrap();
    let path = stage.join("code");
    let written =
        f.io.atomic_write(&proof, &path, b"staged code", None, &f.deadline())
            .unwrap();
    let executable =
        f.io.finalize_staged_mode(&proof, &path, &written, 0o755, &f.deadline())
            .unwrap();
    assert_eq!(executable.mode & 0o777, 0o755);
    assert!(
        f.io.finalize_staged_mode(&proof, &path, &written, 0o755, &f.deadline())
            .is_err()
    );
    let receipt = f.io.target().installer_dir().join("receipt.json");
    let observed =
        f.io.atomic_write(&proof, &receipt, b"receipt", None, &f.deadline())
            .unwrap();
    assert!(
        f.io.finalize_staged_mode(&proof, &receipt, &observed, 0o644, &f.deadline())
            .is_err()
    );
    assert!(
        f.io.remove_owned_leaf(
            &proof,
            &stage,
            &f.io.metadata(&stage).unwrap().unwrap(),
            &f.deadline()
        )
        .is_err()
    );
    f.io.remove_owned_leaf(&proof, &path, &executable, &f.deadline())
        .unwrap();
    f.io.remove_owned_leaf(
        &proof,
        &stage,
        &f.io.metadata(&stage).unwrap().unwrap(),
        &f.deadline(),
    )
    .unwrap();
    assert!(!stage.exists());
    assert!(
        f.io.create_directory(&proof, &f.home.join(".unrelated"), 0o700, &f.deadline())
            .is_err()
    );
}

#[test]
fn native_output_limits_cancellation_and_command_factories_are_exact() {
    let f = Fixture::new();
    let read = CommandSpec::new(
        f.io.target(),
        NativeOperation::Process {
            pid: 4242,
            field: PsField::Uid,
        },
    )
    .unwrap();
    *f.runner.result.lock().unwrap() = Some(Ok(CommandOutput {
        code: Some(0),
        stdout: vec![0; 4097],
        stderr: vec![],
    }));
    assert_eq!(
        f.io.execute(&read, None, &f.deadline()),
        Err(NativeError::Oversize)
    );
    let cancellation = Cancellation::default();
    cancellation.cancel();
    let deadline = Deadline::new(5000, f.clock.clone(), cancellation).unwrap();
    let before = f.runner.calls.lock().unwrap().len();
    assert_eq!(
        f.io.execute(&read, None, &deadline),
        Err(NativeError::Cancelled)
    );
    assert_eq!(f.runner.calls.lock().unwrap().len(), before);
    for timeout in [0, MAX_NATIVE_TIMEOUT_MS + 1] {
        assert!(Deadline::new(timeout, f.clock.clone(), Cancellation::default()).is_err());
    }
    let subject = format!("gui/{}/{}", f.runner.uid, AGENT_LABEL);
    assert_eq!(
        f.command(LaunchctlAction::Bootout).args(),
        ["bootout", &subject]
    );
    assert_eq!(
        f.command(LaunchctlAction::Print).args(),
        ["print", &subject]
    );
    let verify = CommandSpec::new(
        f.io.target(),
        NativeOperation::Signature {
            path: f.runner.exe.clone(),
            query: SignatureQuery::VerifyRequirement("approved requirement".into()),
        },
    )
    .unwrap();
    assert_eq!(verify.program(), Path::new("/usr/bin/codesign"));
    assert_eq!(
        verify.args()[..3],
        ["--verify", "--strict", "-R=approved requirement"]
    );
    assert_eq!(verify.args()[3], f.runner.exe.to_str().unwrap());
    assert!(
        !format!(
            "{:?}",
            NativeOperation::ControlledChild {
                signature: Box::new(f.main()),
                args: vec!["SECRET-SENTINEL".into()]
            }
        )
        .contains("SENTINEL")
    );
}

#[test]
fn noncooperative_injected_commands_return_at_deadline_and_retain_four_slots() {
    let f = Fixture::new();
    let gate = Arc::new(AtomicBool::new(false));
    let waiting = gate.clone();
    *f.runner.hook.lock().unwrap() = Some(Arc::new(move |_| {
        while !waiting.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
        }
    }));
    let read = CommandSpec::new(
        f.io.target(),
        NativeOperation::Process {
            pid: 4242,
            field: PsField::Uid,
        },
    )
    .unwrap();
    for _ in 0..4 {
        let deadline = Deadline::new(20, f.clock.clone(), Cancellation::default()).unwrap();
        let started = Instant::now();
        assert_eq!(
            f.io.execute(&read, None, &deadline),
            Err(NativeError::Timeout)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }
    assert_eq!(
        f.io.execute(&read, None, &f.deadline()),
        Err(NativeError::Busy)
    );
    assert_eq!(f.runner.calls.lock().unwrap().len(), 4);
    gate.store(true, Ordering::Release);
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        match f.io.execute(&read, None, &f.deadline()) {
            Ok(_) => break,
            Err(NativeError::Busy) if Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(1))
            }
            result => panic!("slots did not recover: {result:?}"),
        }
    }
}

#[test]
fn incoming_payload_support_cannot_admit_agent_signed_by_different_team() {
    let f = Fixture::new();
    let main = f.main();
    let incoming =
        f.io.target()
            .paths()
            .payload_root
            .join("Crosspane.app/Contents/MacOS/Crosspane");
    bytes(&incoming, b"different owner development payload", 0o755);
    let expected = requirement(ArtifactRole::Agent);
    let mut observation = signed(&expected);
    observation.team_identifier = "OTHER12345".into();
    f.signatures
        .overrides
        .lock()
        .unwrap()
        .insert(incoming.clone(), observation);
    let candidate =
        f.io.admit_main_signature(&incoming, &expected, &f.deadline())
            .unwrap();
    let support = f.io.admit_support(&candidate, &f.deadline()).unwrap();
    assert!(matches!(
        f.io.admit_instance(&support, &main, &f.deadline()),
        Err(NativeError::Unsupported)
    ));
    assert!(f.runner.calls.lock().unwrap().is_empty());
}

#[test]
fn every_fd_walk_component_rejects_foreign_uid_unsafe_mode_and_symlink_facts() {
    let f = Fixture::new();
    for component in f
        .runtime
        .ancestors()
        .chain(f.home.ancestors())
        .chain(f.io.target().paths().payload_root.ancestors())
    {
        for fault in ["uid", "mode", "symlink"] {
            let component = component.to_owned();
            let mut target = f.io.target().clone();
            target.test_hook = Some(Arc::new(move |stage, path, value| {
                let mut value = value;
                if stage == "walk" && path == component {
                    let identity = value.as_mut().unwrap();
                    match fault {
                        "uid" => identity.uid = u32::MAX,
                        "mode" => identity.mode = 0o040777,
                        _ => identity.mode = 0o120700,
                    }
                }
                Ok(value)
            }));
            assert!(matches!(
                MacNativeIo::new(
                    target,
                    f.runner.clone(),
                    f.support.clone(),
                    f.signatures.clone(),
                    f.clock.clone()
                ),
                Err(NativeError::Foreign)
            ));
        }
    }
    for field in ["dev", "inode"] {
        let enabled = Arc::new(AtomicBool::new(false));
        let switch = enabled.clone();
        let selected = f.runtime.clone();
        let io = f.hooked(Arc::new(move |stage, path, value| {
            let mut value = value;
            if switch.load(Ordering::Acquire) && stage == "walk" && path == selected {
                let v = value.as_mut().unwrap();
                if field == "dev" {
                    v.device += 1;
                } else {
                    v.inode += 1;
                }
            }
            Ok(value)
        }));
        enabled.store(true, Ordering::Release);
        assert_eq!(io.validate_target(), Err(NativeError::Foreign));
    }
}

#[test]
fn both_aliases_correlate_bootstrap_status_dev_inode_and_refuse_substituted_target() {
    for alias in ["/tmp", "/var"] {
        let f = Fixture::new();
        let virtual_root = PathBuf::from(alias).join(f.root.file_name().unwrap());
        let mut target = MacTarget::scratch(TargetPaths {
            uid: f.runner.uid,
            home: virtual_root.join("h"),
            gui_tmpdir: virtual_root.join("t"),
            runtime_override: None,
            payload_root: virtual_root.join("t/payload"),
        })
        .unwrap();
        if alias == "/var" {
            // All fd operations still use this test's private scratch; no owner /var discovery.
            let physical = f.root.clone();
            let logical = admitted_spelling(&virtual_root).unwrap();
            target.test_path = Some(Arc::new(move |path| {
                path.strip_prefix(&logical)
                    .map(|s| physical.join(s))
                    .unwrap_or_else(|_| path.to_owned())
            }));
        }
        f.support.0.lock().unwrap().gui_tmpdir = virtual_root.join("t");
        *f.runner.reported_exe.lock().unwrap() =
            Some(virtual_root.join("h/Applications/Crosspane.app/Contents/MacOS/Crosspane"));
        let mut bootstrap: serde_json::Value =
            serde_json::from_slice(&fs::read(f.runtime.join("bootstrap.json")).unwrap()).unwrap();
        bootstrap["runtime_dir"] = virtual_root
            .join("t/crosspane")
            .to_string_lossy()
            .into_owned()
            .into();
        bytes(
            &f.runtime.join("bootstrap.json"),
            &serde_json::to_vec(&bootstrap).unwrap(),
            0o600,
        );
        let io = MacNativeIo::new(
            target,
            f.runner.clone(),
            f.support.clone(),
            f.signatures.clone(),
            f.clock.clone(),
        )
        .unwrap();
        let signature = io
            .admit_main_signature(
                &io.target().agent_path(),
                &requirement(ArtifactRole::Agent),
                &f.deadline(),
            )
            .unwrap();
        let support = io.admit_support(&signature, &f.deadline()).unwrap();
        let admitted = io
            .admit_instance(&support, &signature, &f.deadline())
            .unwrap();
        let real = f.io.socket_endpoint().unwrap();
        assert_eq!(
            (
                admitted.endpoint().identity().device,
                admitted.endpoint().identity().inode
            ),
            (real.identity().device, real.identity().inode)
        );
        let mut status = f.status();
        status.runtime_dir = io.target().runtime_dir().to_string_lossy().into_owned();
        status.exe = io.target().agent_path().to_string_lossy().into_owned();
        admitted.admit_status(&status).unwrap();
        status.runtime_dir = virtual_root
            .join("t/crosspane")
            .to_string_lossy()
            .into_owned();
        admitted.admit_status(&status).unwrap();
        fs::rename(&f.runtime, f.tmp.join("old-runtime")).unwrap();
        directory(&f.runtime);
        assert_eq!(
            admitted.endpoint().revalidate(&io),
            Err(NativeError::Foreign)
        );
    }
}

#[test]
fn socket_only_substitution_after_connect_refuses_without_transmission() {
    use std::io::Read;
    let f = Fixture::new();
    let endpoint = f.io.socket_endpoint().unwrap();
    let parent = fs::metadata(&f.runtime).unwrap();
    let _client = std::os::unix::net::UnixStream::connect(endpoint.path()).unwrap();
    let (mut accepted, _) = f._listener.accept().unwrap();
    fs::rename(endpoint.path(), f.runtime.join("old.sock")).unwrap();
    let _replacement = UnixListener::bind(endpoint.path()).unwrap();
    fs::set_permissions(endpoint.path(), fs::Permissions::from_mode(0o600)).unwrap();
    use std::os::unix::fs::MetadataExt;
    assert_eq!(
        (parent.dev(), parent.ino()),
        (
            fs::metadata(&f.runtime).unwrap().dev(),
            fs::metadata(&f.runtime).unwrap().ino()
        )
    );
    assert_eq!(endpoint.revalidate(&f.io), Err(NativeError::Foreign));
    accepted.set_nonblocking(true).unwrap();
    assert_eq!(
        accepted.read(&mut [0; 1]).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn process_reuse_exit_reappearance_and_phase_rollback_never_admit() {
    for revalidate in [false, true] {
        let f = Fixture::new();
        let admitted = revalidate.then(|| f.admitted());
        let starts = Arc::new(AtomicU64::new(0));
        let times = starts.clone();
        let runner = f.runner.clone();
        *f.runner.hook.lock().unwrap() = Some(Arc::new(move |spec| {
            if spec.args()[1] == "lstart=" && times.fetch_add(1, Ordering::AcqRel) == 1 {
                *runner.start.lock().unwrap() = b"Thu Jan  1 00:00:01 1970\n".to_vec();
            }
        }));
        let result = if let Some(admitted) = admitted {
            admitted.revalidate(&f.io, &f.proof(), &f.deadline())
        } else {
            f.io.admit_instance(&f.proof(), &f.main(), &f.deadline())
                .map(|_| ())
        };
        assert_eq!(result, Err(NativeError::Foreign));
    }
    let f = Fixture::new();
    let identity = f.admitted().process().clone();
    let calls = Arc::new(AtomicU64::new(0));
    let count = calls.clone();
    let runner = f.runner.clone();
    *f.runner.hook.lock().unwrap() = Some(Arc::new(move |_| {
        runner
            .absent
            .store(count.fetch_add(1, Ordering::AcqRel) == 0, Ordering::Release)
    }));
    assert!(!f.io.process_exited(&identity, &f.deadline()).unwrap());
    let f = Fixture::new();
    let path = f.runtime.join("bootstrap.json");
    let once = Arc::new(AtomicBool::new(false));
    *f.runner.hook.lock().unwrap() = Some(Arc::new(move |_| {
        if !once.swap(true, Ordering::AcqRel) {
            let mut value: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            value["phase_seq"] = 0.into();
            bytes(&path, &serde_json::to_vec(&value).unwrap(), 0o600);
        }
    }));
    assert!(matches!(
        f.io.admit_instance(&f.proof(), &f.main(), &f.deadline()),
        Err(NativeError::Foreign)
    ));
}

#[test]
fn every_same_team_helper_still_requires_its_own_identity_verification_and_entitlements() {
    let f = Fixture::new();
    let main = f.main();
    for role in [
        ArtifactRole::Settings,
        ArtifactRole::Installer,
        ArtifactRole::Ctl,
        ArtifactRole::Installer,
    ] {
        let path =
            f.io.target()
                .app_path()
                .join(format!("Contents/MacOS/{role:?}"));
        bytes(&path, b"owned helper", 0o755);
        let expected = requirement(role);
        for fault in 0..7 {
            let mut observation = signed(&expected);
            match fault {
                0 => observation.identifier = requirement(ArtifactRole::Agent).identifier,
                1 => observation.designated_requirement = "wrong requirement".into(),
                2 => observation.strict_verified = false,
                3 => observation.entitlements = requirement(ArtifactRole::Agent).entitlements,
                4 => observation.apple_development = false,
                5 => observation.hardened_runtime = false,
                _ => observation.ad_hoc = true,
            }
            f.signatures
                .overrides
                .lock()
                .unwrap()
                .insert(path.clone(), observation);
            assert!(matches!(
                f.io.admit_artifact_signature(&path, &expected, &main, &f.deadline()),
                Err(NativeError::Unsupported)
            ));
        }
        f.signatures.overrides.lock().unwrap().remove(&path);
        f.io.admit_artifact_signature(&path, &expected, &main, &f.deadline())
            .unwrap();
    }
}

#[test]
fn cancellation_between_dispatch_check_and_runner_is_unknown_and_cannot_invoke_late() {
    let f = Fixture::new();
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let (seen, gate) = (entered.clone(), release.clone());
    let io = f.hooked(Arc::new(move |stage, _, value| {
        if stage == "dispatch" {
            seen.store(true, Ordering::Release);
            while !gate.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
        }
        Ok(value)
    }));
    let cancellation = Cancellation::default();
    let limit = Deadline::new(1000, f.clock.clone(), cancellation.clone()).unwrap();
    let proof = f.proof();
    let command = f.command(LaunchctlAction::Bootout);
    let task = std::thread::spawn(move || io.execute(&command, Some(&proof), &limit));
    let wait = Instant::now();
    while !entered.load(Ordering::Acquire) {
        assert!(wait.elapsed() < Duration::from_secs(1));
        std::thread::yield_now();
    }
    cancellation.cancel();
    assert_eq!(task.join().unwrap(), Err(NativeError::OutcomeUnknown));
    release.store(true, Ordering::Release);
    std::thread::sleep(Duration::from_millis(10));
    assert!(f.runner.calls.lock().unwrap().is_empty());
}

#[test]
fn observed_leaf_replacement_is_never_chmodded_or_unlinked() {
    for action in ["open-before", "chmod", "quarantine"] {
        let f = Fixture::new();
        let path = f.home.join(".local/bin/.crosspanectl.crosspane-stage");
        bytes(&path, b"observed-original", 0o600);
        let original = f.io.metadata(&path).unwrap().unwrap();
        let selected = path.clone();
        let backup = path.with_file_name("original-held-by-repair");
        let once = Arc::new(AtomicBool::new(false));
        let io = f.hooked(Arc::new(move |stage, path, value| {
            if stage == action && path == selected && !once.swap(true, Ordering::AcqRel) {
                fs::rename(path, &backup).unwrap();
                bytes(path, b"unobserved-replacement", 0o600);
            }
            Ok(value)
        }));
        let result = if action != "quarantine" {
            io.finalize_staged_mode(&f.proof(), &path, &original, 0o755, &f.deadline())
                .map(|_| ())
        } else {
            io.remove_owned_leaf(&f.proof(), &path, &original, &f.deadline())
        };
        assert_eq!(
            result,
            Err(if action == "open-before" {
                NativeError::Foreign
            } else {
                NativeError::OutcomeUnknown
            })
        );
        assert_eq!(fs::read(&path).unwrap(), b"unobserved-replacement");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn destination_appearing_at_publication_cannot_be_replaced() {
    for rename in [false, true] {
        let f = Fixture::new();
        let destination = f.home.join(if rename {
            ".local/bin/.crosspanectl.crosspane-previous"
        } else {
            "Library/LaunchAgents/io.frostdev.crosspane.agent.plist"
        });
        directory(destination.parent().unwrap());
        let selected = destination.clone();
        let once = Arc::new(AtomicBool::new(false));
        let io = f.hooked(Arc::new(move |stage, path, value| {
            if stage == "publish" && path == selected && !once.swap(true, Ordering::AcqRel) {
                bytes(path, b"concurrent-preserved-file", 0o600);
            }
            Ok(value)
        }));
        let result = if rename {
            let stage = f.home.join(".local/bin/.crosspanectl.crosspane-stage");
            bytes(&stage, b"new-stage", 0o600);
            io.rename_owned(
                &f.proof(),
                &stage,
                &destination,
                &io.metadata(&stage).unwrap().unwrap(),
                &f.deadline(),
            )
        } else {
            io.atomic_write(&f.proof(), &destination, b"new-plist", None, &f.deadline())
                .map(|_| ())
        };
        assert_eq!(result, Err(NativeError::OutcomeUnknown));
        assert_eq!(
            fs::read(&destination).unwrap(),
            b"concurrent-preserved-file"
        );
    }
}

#[test]
fn filesystem_operations_execute_flush_order_and_failed_operations_preserve_receipt() {
    let expected_operations = [
        "write",
        "file-sync",
        "rename",
        "directory-sync",
        "directory-sync",
    ];
    for failure in 0..=expected_operations.len() {
        let f = Fixture::new();
        let path = f.io.target().installer_dir().join("intent.json");
        bytes(&path, b"previous", 0o600);
        let expected = f.io.metadata(&path).unwrap().unwrap();
        let operations = Arc::new(RecordingFilesystem {
            operations: Mutex::default(),
            failure,
            after: None,
        });
        let io = f.operated(operations.clone());
        let result = io.atomic_write(
            &f.proof(),
            &path,
            b"replacement",
            Some(&expected),
            &f.deadline(),
        );
        if failure == 0 {
            result.unwrap();
        } else {
            assert_eq!(result, Err(NativeError::OutcomeUnknown));
        }
        if (1..=3).contains(&failure) {
            assert_eq!(fs::read(&path).unwrap(), b"previous");
        } else {
            assert_eq!(fs::read(&path).unwrap(), b"replacement");
        }
        let log = operations.operations.lock().unwrap();
        if failure == 0 {
            assert_eq!(log.as_slice(), expected_operations);
        } else {
            assert_eq!(log.as_slice(), &expected_operations[..failure]);
        }
    }
}

type AfterFilesystemOperation = Arc<dyn Fn(usize) + Send + Sync>;
struct RecordingFilesystem {
    operations: Mutex<Vec<&'static str>>,
    failure: usize,
    after: Option<AfterFilesystemOperation>,
}
impl FilesystemOps for RecordingFilesystem {
    fn execute(&self, operation: FilesystemOperation<'_>) -> NativeResult<()> {
        let stage = match &operation {
            FilesystemOperation::Write(..) => "write",
            FilesystemOperation::FileSync(..) => "file-sync",
            FilesystemOperation::Rename { .. } => "rename",
            FilesystemOperation::DirectorySync(..) => "directory-sync",
        };
        let index = {
            let mut log = self.operations.lock().unwrap();
            log.push(stage);
            log.len()
        };
        if index == self.failure {
            return Err(NativeError::Unavailable);
        }
        SystemFilesystem.execute(operation)?;
        if let Some(after) = &self.after {
            after(index);
        }
        Ok(())
    }
}

#[test]
fn interruption_replacing_existing_receipt_observes_old_or_new_never_absent() {
    for interrupt_after in 1..=5 {
        let f = Fixture::new();
        let path = f.io.target().installer_dir().join("intent.json");
        bytes(&path, b"previous", 0o600);
        let expected = f.io.metadata(&path).unwrap().unwrap();
        let canonical = path.clone();
        let cancellation = Cancellation::default();
        let cancel = cancellation.clone();
        let operations = Arc::new(RecordingFilesystem {
            operations: Mutex::default(),
            failure: 0,
            after: Some(Arc::new(move |index| {
                let contents =
                    fs::read(&canonical).expect("canonical receipt must never disappear");
                assert!(contents == b"previous" || contents == b"replacement");
                if index == interrupt_after {
                    cancel.cancel();
                }
            })),
        });
        let io = f.operated(operations.clone());
        let deadline = Deadline::new(5000, f.clock.clone(), cancellation).unwrap();
        assert_eq!(
            io.atomic_write(
                &f.proof(),
                &path,
                b"replacement",
                Some(&expected),
                &deadline
            ),
            Err(NativeError::OutcomeUnknown)
        );
        let contents = fs::read(&path).expect("interrupted receipt must remain discoverable");
        assert_eq!(
            contents,
            if interrupt_after < 3 {
                b"previous".as_slice()
            } else {
                b"replacement".as_slice()
            }
        );
        assert_eq!(operations.operations.lock().unwrap().len(), interrupt_after);
    }
}

#[test]
fn atomic_exchange_retains_unobserved_displaced_object_instead_of_deleting_it() {
    let f = Fixture::new();
    let path = f.io.target().installer_dir().join("intent.json");
    bytes(&path, b"observed-previous", 0o600);
    let expected = f.io.metadata(&path).unwrap().unwrap();
    let canonical = path.clone();
    let once = AtomicBool::new(false);
    let io = f.hooked(Arc::new(move |stage, path, value| {
        if stage == "publish" && path == canonical && !once.swap(true, Ordering::AcqRel) {
            fs::rename(path, path.with_file_name("previous-held-by-repair")).unwrap();
            bytes(path, b"unobserved-displaced", 0o600);
        }
        Ok(value)
    }));
    assert_eq!(
        io.atomic_write(
            &f.proof(),
            &path,
            b"replacement",
            Some(&expected),
            &f.deadline()
        ),
        Err(NativeError::OutcomeUnknown)
    );
    assert_eq!(fs::read(&path).unwrap(), b"replacement");
    assert!(fs::read_dir(path.parent().unwrap()).unwrap().any(|entry| {
        let entry = entry.unwrap();
        entry
            .file_name()
            .to_string_lossy()
            .starts_with(".intent.json.crosspane-temp-")
            && fs::read(entry.path()).ok().as_deref() == Some(b"unobserved-displaced")
    }));
}

#[test]
fn each_filesystem_mutation_checks_deadline_and_postmutation_errors_are_unknown() {
    for action in ["mkdir", "chmod", "rename", "remove"] {
        for after in [false, true] {
            let f = Fixture::new();
            let path = f.home.join(".local/bin/.crosspanectl.crosspane-stage");
            bytes(&path, b"owned", 0o600);
            let expected = f.io.metadata(&path).unwrap().unwrap();
            let clock = f.clock.clone();
            let boundary = if after {
                "complete"
            } else {
                match action {
                    "mkdir" => "mkdir",
                    "chmod" => "chmod",
                    _ => "quarantine",
                }
            };
            let io = f.hooked(Arc::new(move |stage, _, value| {
                if stage == boundary {
                    clock.set(5001);
                }
                Ok(value)
            }));
            let deadline = f.deadline();
            let proof = f.proof();
            let result = match action {
                "mkdir" => io.create_directory(
                    &proof,
                    &f.home.join("Applications/.Crosspane.app.crosspane-stage"),
                    0o700,
                    &deadline,
                ),
                "chmod" => io
                    .finalize_staged_mode(&proof, &path, &expected, 0o755, &deadline)
                    .map(|_| ()),
                "rename" => io.rename_owned(
                    &proof,
                    &path,
                    &f.home.join(".local/bin/.crosspanectl.crosspane-previous"),
                    &expected,
                    &deadline,
                ),
                _ => io.remove_owned_leaf(&proof, &path, &expected, &deadline),
            };
            assert_eq!(
                result,
                Err(if after {
                    NativeError::OutcomeUnknown
                } else {
                    NativeError::Timeout
                })
            );
            if !after {
                assert_eq!(fs::read(&path).unwrap(), b"owned");
                assert_eq!(
                    fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
    }
    let f = Fixture::new();
    let path = f.home.join(".local/bin/.crosspanectl.crosspane-stage");
    bytes(&path, b"owned", 0o600);
    let observed = f.io.metadata(&path).unwrap().unwrap();
    let selected = path.clone();
    let armed = Arc::new(AtomicBool::new(false));
    let flag = armed.clone();
    let io = f.hooked(Arc::new(move |stage, path, value| {
        if stage == "chmod" {
            flag.store(true, Ordering::Release);
        }
        if stage == "metadata" && path == selected && flag.load(Ordering::Acquire) {
            return Err(NativeError::Unavailable);
        }
        Ok(value)
    }));
    assert_eq!(
        io.finalize_staged_mode(&f.proof(), &path, &observed, 0o755, &f.deadline()),
        Err(NativeError::OutcomeUnknown)
    );
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[test]
fn interrupted_ctl_and_plist_temporaries_are_bounded_unique_and_cleanup_requires_identity() {
    for resource in [
        ".local/bin/crosspanectl",
        "Library/LaunchAgents/io.frostdev.crosspane.agent.plist",
    ] {
        let f = Fixture::new();
        let path = f.home.join(resource);
        directory(path.parent().unwrap());
        let mut leftovers = Vec::new();
        for _restart in 0..8 {
            // Reconstruct the target/IO after each interruption, retaining only observed files.
            let io = f.hooked(Arc::new(|stage, _, value| {
                if stage == "file-sync" {
                    Err(NativeError::Unavailable)
                } else {
                    Ok(value)
                }
            }));
            assert_eq!(
                io.atomic_write(&f.proof(), &path, b"private-intent", None, &f.deadline()),
                Err(NativeError::OutcomeUnknown)
            );
        }
        for entry in fs::read_dir(path.parent().unwrap()).unwrap() {
            let path = entry.unwrap().path();
            if path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains(".crosspane-temp-")
            {
                leftovers.push(path);
            }
        }
        assert_eq!(leftovers.len(), 8);
        assert_eq!(
            f.io.atomic_write(&f.proof(), &path, b"no", None, &f.deadline()),
            Err(NativeError::Busy)
        );
        for temporary in leftovers {
            let expected = f.io.metadata(&temporary).unwrap().unwrap();
            let mut wrong = expected.clone();
            wrong.inode += 1;
            assert_eq!(
                f.io.remove_owned_leaf(&f.proof(), &temporary, &wrong, &f.deadline()),
                Err(NativeError::Foreign)
            );
            f.io.remove_owned_leaf(&f.proof(), &temporary, &expected, &f.deadline())
                .unwrap();
        }
        assert!(
            fs::read_dir(path.parent().unwrap())
                .unwrap()
                .next()
                .is_none()
        );
        f.io.atomic_write(&f.proof(), &path, b"renewed-intent", None, &f.deadline())
            .unwrap();
    }
}

struct FakePipe {
    remaining: usize,
    blocked: bool,
    entered: Arc<AtomicBool>,
    first: bool,
}
impl std::io::Read for FakePipe {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.entered.store(true, Ordering::Release);
        if self.first {
            self.first = false;
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        if self.blocked {
            return Err(std::io::ErrorKind::WouldBlock.into());
        }
        let n = self.remaining.min(buffer.len()).min(1024);
        buffer[..n].fill(b'x');
        self.remaining -= n;
        Ok(n)
    }
}
struct FakeSpawner {
    spawned: AtomicU64,
    reaping: Arc<AtomicU64>,
    reaped: Arc<AtomicU64>,
    release: Arc<AtomicBool>,
    entered: Arc<AtomicBool>,
    sizes: Mutex<(usize, usize)>,
    hang: AtomicBool,
    pipe_failure: AtomicBool,
}
impl FakeSpawner {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            spawned: AtomicU64::new(0),
            reaping: Arc::new(AtomicU64::new(0)),
            reaped: Arc::new(AtomicU64::new(0)),
            release: Arc::new(AtomicBool::new(true)),
            entered: Arc::new(AtomicBool::new(false)),
            sizes: Mutex::new((0, 0)),
            hang: AtomicBool::new(false),
            pipe_failure: AtomicBool::new(false),
        })
    }
}
struct FakeChild {
    output: (usize, usize),
    hang: bool,
    pipe_failure: bool,
    entered: Arc<AtomicBool>,
    reaping: Arc<AtomicU64>,
    reaped: Arc<AtomicU64>,
    release: Arc<AtomicBool>,
}
impl ChildSpawner for FakeSpawner {
    fn spawn(&self, _: &CommandSpec) -> NativeResult<Box<dyn OwnedChild>> {
        self.spawned.fetch_add(1, Ordering::AcqRel);
        Ok(Box::new(FakeChild {
            output: *self.sizes.lock().unwrap(),
            hang: self.hang.load(Ordering::Acquire),
            pipe_failure: self.pipe_failure.load(Ordering::Acquire),
            entered: self.entered.clone(),
            reaping: self.reaping.clone(),
            reaped: self.reaped.clone(),
            release: self.release.clone(),
        }))
    }
}
impl OwnedChild for FakeChild {
    fn pipes(
        &mut self,
    ) -> NativeResult<(Box<dyn std::io::Read + Send>, Box<dyn std::io::Read + Send>)> {
        if self.pipe_failure {
            return Err(NativeError::Unavailable);
        }
        Ok((
            Box::new(FakePipe {
                remaining: self.output.0,
                blocked: self.hang,
                entered: self.entered.clone(),
                first: true,
            }),
            Box::new(FakePipe {
                remaining: self.output.1,
                blocked: false,
                entered: self.entered.clone(),
                first: true,
            }),
        ))
    }
    fn status(&mut self) -> NativeResult<Option<Option<i32>>> {
        Ok((!self.hang).then_some(Some(0)))
    }
    fn reap(&mut self) {
        self.reaping.fetch_add(1, Ordering::AcqRel);
        while !self.release.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        self.reaped.fetch_add(1, Ordering::AcqRel);
    }
}
struct ChildRunner(Arc<FakeSpawner>);
impl CommandRunner for ChildRunner {
    fn run(&self, spec: &CommandSpec, deadline: &Deadline) -> NativeResult<CommandOutput> {
        run_owned_child(spec, deadline, &*self.0)
    }
}
fn child_io(f: &Fixture, spawner: Arc<FakeSpawner>) -> MacNativeIo {
    MacNativeIo::new(
        f.io.target().clone(),
        Arc::new(ChildRunner(spawner)),
        f.support.clone(),
        f.signatures.clone(),
        f.clock.clone(),
    )
    .unwrap()
}
fn until(mut predicate: impl FnMut() -> bool) {
    let started = Instant::now();
    while !predicate() {
        assert!(started.elapsed() < Duration::from_secs(2));
        std::thread::yield_now();
    }
}

#[test]
fn owned_child_streaming_combined_stderr_overflow_and_pipe_failure_reap_only_owned_child() {
    let f = Fixture::new();
    let command = CommandSpec::new(
        f.io.target(),
        NativeOperation::Process {
            pid: 4242,
            field: PsField::Uid,
        },
    )
    .unwrap();
    let max = command.max_output();
    for sizes in [(0, max + 1), (max / 4, max - max / 4 + 1)] {
        let spawner = FakeSpawner::new();
        *spawner.sizes.lock().unwrap() = sizes;
        let io = child_io(&f, spawner.clone());
        assert_eq!(
            io.execute(&command, None, &f.deadline()),
            Err(NativeError::Oversize)
        );
        until(|| spawner.reaped.load(Ordering::Acquire) == 1);
        assert_eq!(spawner.spawned.load(Ordering::Acquire), 1);
    }
    let spawner = FakeSpawner::new();
    spawner.pipe_failure.store(true, Ordering::Release);
    assert_eq!(
        child_io(&f, spawner.clone()).execute(&command, None, &f.deadline()),
        Err(NativeError::Unavailable)
    );
    until(|| spawner.reaped.load(Ordering::Acquire) == 1);
    let spawner = FakeSpawner::new();
    *spawner.sizes.lock().unwrap() = (max / 2, max / 2);
    let result = child_io(&f, spawner.clone())
        .execute(&command, None, &f.deadline())
        .unwrap();
    assert_eq!(result.stdout.len() + result.stderr.len(), max);
    assert_eq!(spawner.reaped.load(Ordering::Acquire), 0); // already observed exited; never signalled
}

#[test]
fn in_flight_owned_child_cancellation_is_unknown_and_reaper_retains_four_slots_until_exit() {
    let f = Fixture::new();
    let spawner = FakeSpawner::new();
    spawner.hang.store(true, Ordering::Release);
    let io = Arc::new(child_io(&f, spawner.clone()));
    let cancel = Cancellation::default();
    let deadline = Deadline::new(1000, f.clock.clone(), cancel.clone()).unwrap();
    let (worker, command, proof) = (io.clone(), f.command(LaunchctlAction::Bootout), f.proof());
    let task = std::thread::spawn(move || worker.execute(&command, Some(&proof), &deadline));
    until(|| spawner.entered.load(Ordering::Acquire));
    cancel.cancel();
    assert_eq!(task.join().unwrap(), Err(NativeError::OutcomeUnknown));
    until(|| spawner.reaped.load(Ordering::Acquire) == 1);
    let spawner = FakeSpawner::new();
    spawner.hang.store(true, Ordering::Release);
    spawner.release.store(false, Ordering::Release);
    let io = child_io(&f, spawner.clone());
    let read = CommandSpec::new(
        f.io.target(),
        NativeOperation::Process {
            pid: 4242,
            field: PsField::Uid,
        },
    )
    .unwrap();
    for _ in 0..4 {
        let deadline = Deadline::new(30, f.clock.clone(), Cancellation::default()).unwrap();
        assert_eq!(
            io.execute(&read, None, &deadline),
            Err(NativeError::Timeout)
        );
    }
    until(|| spawner.reaping.load(Ordering::Acquire) == 4);
    assert_eq!(
        io.execute(&read, None, &f.deadline()),
        Err(NativeError::Busy)
    );
    assert_eq!(spawner.spawned.load(Ordering::Acquire), 4);
    spawner.release.store(true, Ordering::Release);
    until(|| spawner.reaped.load(Ordering::Acquire) == 4);
    spawner.hang.store(false, Ordering::Release);
    until(|| io.execute(&read, None, &f.deadline()).is_ok());
    assert_eq!(spawner.spawned.load(Ordering::Acquire), 5);
}

#[test]
fn expired_queued_command_never_spawns_or_invokes_a_runner() {
    let f = Fixture::new();
    f.runner.blocked.store(true, Ordering::Release);
    let command = CommandSpec::new(
        f.io.target(),
        NativeOperation::Process {
            pid: 4242,
            field: PsField::Uid,
        },
    )
    .unwrap();
    let mut queue = NativeCommandQueue::new(f.io.clone()).unwrap();
    for id in 1..=5 {
        queue
            .submit(NativeCall {
                id,
                command: command.clone(),
                timeout_ms: if id == 5 { 10 } else { 5000 },
                proof: None,
            })
            .unwrap();
    }
    until(|| f.runner.calls.lock().unwrap().len() == 4);
    f.clock.set(11);
    f.runner.blocked.store(false, Ordering::Release);
    let mut replies = Vec::new();
    until(|| {
        replies.extend(queue.poll());
        replies.len() == 5
    });
    assert_eq!(
        replies.iter().find(|r| r.id == 5).unwrap().result,
        Err(NativeError::Timeout)
    );
    assert_eq!(f.runner.calls.lock().unwrap().len(), 4);
}

#[test]
fn actual_symlink_substitution_at_each_owned_ancestry_component_is_refused() {
    let f = Fixture::new();
    let components: std::collections::BTreeSet<_> = f
        .runtime
        .ancestors()
        .chain(f.home.ancestors())
        .chain(f.io.target().paths().payload_root.ancestors())
        .filter(|p| p.starts_with(&f.root))
        .map(Path::to_owned)
        .collect();
    for component in components {
        let backup = component.with_file_name(format!(
            "{}.held",
            component.file_name().unwrap().to_string_lossy()
        ));
        fs::rename(&component, &backup).unwrap();
        symlink(&backup, &component).unwrap();
        assert_eq!(f.io.validate_target(), Err(NativeError::Foreign));
        fs::remove_file(&component).unwrap();
        fs::rename(&backup, &component).unwrap();
    }
}

#[test]
fn filesystem_cancellation_after_each_mutation_and_new_lock_creation_is_unknown() {
    for action in ["mkdir", "chmod", "rename", "remove", "lock"] {
        let f = Fixture::new();
        let path = f.home.join(".local/bin/.crosspanectl.crosspane-stage");
        bytes(&path, b"owned", 0o600);
        let expected = f.io.metadata(&path).unwrap().unwrap();
        let cancellation = Cancellation::default();
        let cancel = cancellation.clone();
        let io = f.hooked(Arc::new(move |stage, _, value| {
            if stage == "complete" {
                cancel.cancel();
            }
            Ok(value)
        }));
        let deadline = Deadline::new(5000, f.clock.clone(), cancellation).unwrap();
        let proof = f.proof();
        let result = match action {
            "mkdir" => io.create_directory(
                &proof,
                &f.home.join("Applications/.Crosspane.app.crosspane-stage"),
                0o700,
                &deadline,
            ),
            "chmod" => io
                .finalize_staged_mode(&proof, &path, &expected, 0o755, &deadline)
                .map(|_| ()),
            "rename" => io.rename_owned(
                &proof,
                &path,
                &f.home.join(".local/bin/.crosspanectl.crosspane-previous"),
                &expected,
                &deadline,
            ),
            "remove" => io.remove_owned_leaf(&proof, &path, &expected, &deadline),
            _ => io.lock(&proof, &deadline).map(|_| ()),
        };
        assert_eq!(result, Err(NativeError::OutcomeUnknown));
    }
}

#[test]
fn post_chmod_fd_observation_failure_remains_unknown() {
    let f = Fixture::new();
    let path = f.home.join(".local/bin/.crosspanectl.crosspane-stage");
    bytes(&path, b"owned", 0o600);
    let observed = f.io.metadata(&path).unwrap().unwrap();
    let selected = path.clone();
    let armed = Arc::new(AtomicBool::new(false));
    let flag = armed.clone();
    let io = f.hooked(Arc::new(move |stage, path, value| {
        if stage == "chmod" {
            flag.store(true, Ordering::Release);
        }
        if stage == "fd-stat" && path == selected && flag.load(Ordering::Acquire) {
            return Err(NativeError::Unavailable);
        }
        Ok(value)
    }));
    assert_eq!(
        io.finalize_staged_mode(&f.proof(), &path, &observed, 0o755, &f.deadline()),
        Err(NativeError::OutcomeUnknown)
    );
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[test]
fn reconstructed_io_for_the_same_target_serializes_mutations_without_dispatch() {
    let f = Fixture::new();
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let (seen, gate) = (entered.clone(), release.clone());
    let io = f.hooked(Arc::new(move |stage, _, value| {
        if stage == "file-sync" {
            seen.store(true, Ordering::Release);
            while !gate.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
        }
        Ok(value)
    }));
    let (proof, deadline, path) = (
        f.proof(),
        f.deadline(),
        f.io.target().installer_dir().join("intent.json"),
    );
    let task =
        std::thread::spawn(move || io.atomic_write(&proof, &path, b"intent", None, &deadline));
    until(|| entered.load(Ordering::Acquire));
    let second = f.io.target().installer_dir().join("other.json");
    assert_eq!(
        f.io.atomic_write(&f.proof(), &second, b"no", None, &f.deadline()),
        Err(NativeError::Busy)
    );
    assert!(!second.exists());
    release.store(true, Ordering::Release);
    task.join().unwrap().unwrap();
    f.io.atomic_write(&f.proof(), &second, b"now", None, &f.deadline())
        .unwrap();
}
