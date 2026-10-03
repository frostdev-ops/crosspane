#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // AGENTS.md permits assertions in test fixtures.
//! WP-4.12a native admission tests. Transport/framing is supplied by the sequential WP-4.12b.
use crosspane_installer::agent_contract::{
    self, BootstrapPhase, InstanceStatus, ObservationSource,
};
// Compile the implementation privately so hooks stay unavailable to application consumers.
#[path = "../src/platform/macos/native_io.rs"]
#[allow(dead_code, unused_imports)]
mod subject;
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
            uid: self.runner.uid,
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
fn unsupported_or_noninteractive_facts_cannot_mutate() {
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
    for changed in changes {
        *f.support.0.lock().unwrap() = changed;
        assert!(f.io.admit_support(&main, &f.deadline()).is_err());
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
        ArtifactRole::Tutorial,
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
    x.uid += 1;
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
            &requirement(ArtifactRole::Tutorial),
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
        ArtifactRole::Tutorial,
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
