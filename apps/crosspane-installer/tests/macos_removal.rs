#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Private scratch, injected process/support/signature answers. No command is ever spawned.
use crosspane_installer::platform::macos::native_io::*;
use rustix::{fd::OwnedFd, fs as rfs};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs::File,
    io::Write,
    os::unix::net::UnixListener,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
const DIR: rfs::OFlags = rfs::OFlags::RDONLY
    .union(rfs::OFlags::DIRECTORY)
    .union(rfs::OFlags::NOFOLLOW)
    .union(rfs::OFlags::CLOEXEC);
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Scratch {
    path: PathBuf,
    parent: OwnedFd,
    fd: OwnedFd,
    name: String,
}
impl Scratch {
    fn new() -> Arc<Self> {
        let root = rfs::open("/", DIR, rfs::Mode::empty()).unwrap();
        let private = rfs::openat(root, "private", DIR, rfs::Mode::empty()).unwrap();
        let parent = rfs::openat(private, "tmp", DIR, rfs::Mode::empty()).unwrap();
        let name = format!(
            "cp-remove-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        rfs::mkdirat(&parent, name.as_str(), rfs::Mode::RWXU).unwrap();
        let fd = rfs::openat(&parent, name.as_str(), DIR, rfs::Mode::empty()).unwrap();
        let stat = rfs::fstat(&fd).unwrap();
        assert_eq!(stat.st_uid, rustix::process::geteuid().as_raw());
        assert_eq!(stat.st_mode & 0o777, 0o700);
        Arc::new(Self {
            path: PathBuf::from("/private/tmp").join(&name),
            parent,
            fd,
            name,
        })
    }
    fn directory(&self, path: &Path) -> OwnedFd {
        let mut fd = rustix::io::dup(&self.fd).unwrap();
        for component in path.strip_prefix(&self.path).unwrap().components() {
            let Component::Normal(name) = component else {
                panic!("unconfined scratch");
            };
            match rfs::mkdirat(&fd, name, rfs::Mode::RWXU) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(e) => panic!("{e}"),
            }
            fd = rfs::openat(fd, name, DIR, rfs::Mode::empty()).unwrap();
            let stat = rfs::fstat(&fd).unwrap();
            assert_eq!(stat.st_uid, rustix::process::geteuid().as_raw());
            assert_eq!(stat.st_mode & 0o777, 0o700);
        }
        fd
    }
    fn put(&self, path: &Path, bytes: &[u8], mode: u32) {
        let parent = self.directory(path.parent().unwrap());
        let fd = rfs::openat(
            &parent,
            path.file_name().unwrap(),
            rfs::OFlags::WRONLY
                | rfs::OFlags::CREATE
                | rfs::OFlags::NOFOLLOW
                | rfs::OFlags::CLOEXEC,
            rfs::Mode::RUSR | rfs::Mode::WUSR,
        )
        .unwrap();
        let stat = rfs::fstat(&fd).unwrap();
        assert_eq!(stat.st_uid, rustix::process::geteuid().as_raw());
        assert_eq!(stat.st_mode & 0o170000, 0o100000);
        assert_eq!(stat.st_nlink, 1);
        rfs::fchmod(&fd, rfs::Mode::from_bits_truncate(mode.try_into().unwrap())).unwrap();
        let mut file = File::from(fd);
        file.set_len(0).unwrap();
        file.write_all(bytes).unwrap();
    }
    fn remove(&self, path: &Path) {
        let parent = self.directory(path.parent().unwrap());
        rfs::unlinkat(parent, path.file_name().unwrap(), rfs::AtFlags::empty()).unwrap();
    }
}
fn same(a: &rfs::Stat, b: &rfs::Stat) -> bool {
    a.st_dev == b.st_dev && a.st_ino == b.st_ino
}
fn clear(fd: &OwnedFd) {
    for e in rfs::Dir::read_from(fd).unwrap() {
        let e = e.unwrap();
        let name = e.file_name();
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        let before = rfs::statat(fd, name, rfs::AtFlags::SYMLINK_NOFOLLOW).unwrap();
        let directory = before.st_mode & 0o170000 == 0o040000;
        if directory {
            let child = rfs::openat(fd, name, DIR, rfs::Mode::empty()).unwrap();
            assert!(same(&before, &rfs::fstat(&child).unwrap()));
            clear(&child);
        }
        assert!(same(
            &before,
            &rfs::statat(fd, name, rfs::AtFlags::SYMLINK_NOFOLLOW).unwrap()
        ));
        rfs::unlinkat(
            fd,
            name,
            if directory {
                rfs::AtFlags::REMOVEDIR
            } else {
                rfs::AtFlags::empty()
            },
        )
        .unwrap();
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        assert!(same(
            &rfs::fstat(&self.fd).unwrap(),
            &rfs::statat(
                &self.parent,
                self.name.as_str(),
                rfs::AtFlags::SYMLINK_NOFOLLOW
            )
            .unwrap()
        ));
        clear(&self.fd);
        rfs::unlinkat(&self.parent, self.name.as_str(), rfs::AtFlags::REMOVEDIR).unwrap();
    }
}
#[derive(Default)]
struct ClockFake(AtomicU64);
impl Clock for ClockFake {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}
type Hook = Arc<dyn Fn() + Send + Sync>;
struct Support {
    facts: Mutex<SupportObservation>,
    hook: Mutex<Option<Hook>>,
}
impl SupportProbe for Support {
    fn observe(&self, _: &Deadline) -> NativeResult<SupportObservation> {
        let hook = self.hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook();
        }
        Ok(self.facts.lock().unwrap().clone())
    }
}
struct Signatures;
impl SignatureProbe for Signatures {
    fn observe(
        &self,
        _: &Path,
        expected: &SigningRequirement,
        _: &Deadline,
    ) -> NativeResult<SignatureObservation> {
        Ok(SignatureObservation {
            strict_verified: true,
            team_identifier: "ABCDE12345".into(),
            identifier: expected.identifier.clone(),
            designated_requirement: expected.designated_requirement.clone(),
            entitlements: expected.entitlements.clone(),
            apple_development: true,
            hardened_runtime: true,
            ad_hoc: false,
        })
    }
}
#[derive(Clone, Debug)]
struct Seen {
    program: PathBuf,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    limit: usize,
}
struct Runner {
    uid: u32,
    exe: PathBuf,
    state: AtomicU64,
    calls: Mutex<Vec<Seen>>,
    reply: Mutex<NativeResult<CommandOutput>>,
    hook: Mutex<Option<Hook>>,
}
impl CommandRunner for Runner {
    fn run(&self, spec: &CommandSpec, _: &Deadline) -> NativeResult<CommandOutput> {
        self.calls.lock().unwrap().push(Seen {
            program: spec.program().into(),
            args: spec.args().into(),
            env: spec.environment().clone(),
            limit: spec.max_output(),
        });
        let hook = self.hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook();
        }
        let text = if spec.program() == Path::new("/bin/ps") {
            match self.state.load(Ordering::Acquire) {
                1 => {
                    return Ok(CommandOutput {
                        code: Some(1),
                        stdout: vec![],
                        stderr: vec![],
                    });
                }
                3 => {
                    return Ok(CommandOutput {
                        code: Some(1),
                        stdout: vec![],
                        stderr: b"not an absence".to_vec(),
                    });
                }
                _ => {}
            }
            match spec.args()[1].as_str() {
                "uid=" => format!("{}\n", self.uid),
                "comm=" => format!("{}\n", self.exe.display()),
                "lstart=" => {
                    if self.state.load(Ordering::Acquire) == 2 {
                        "Thu Jan  1 00:00:01 1970\n".into()
                    } else {
                        "Thu Jan  1 00:00:00 1970\n".into()
                    }
                }
                _ => panic!("unexpected ps"),
            }
        } else if spec.program() == self.exe {
            assert_eq!(spec.args(), ["erase-identity"]);
            return self.reply.lock().unwrap().clone();
        } else {
            panic!("no real/native command admitted by fake");
        };
        Ok(CommandOutput {
            code: Some(0),
            stdout: text.into_bytes(),
            stderr: vec![],
        })
    }
}
struct Fixture {
    root: Arc<Scratch>,
    io: Arc<MacNativeIo>,
    clock: Arc<ClockFake>,
    support_probe: Arc<Support>,
    proof: SupportProof,
    signature: SignatureProof,
    instance: Arc<AdmittedInstance>,
    runner: Arc<Runner>,
    _listener: UnixListener,
}
impl Fixture {
    fn new() -> Self {
        let root = Scratch::new();
        let home = root.path.join("h");
        let tmp = root.path.join("t");
        let payload = tmp.join("payload");
        let runtime = tmp.join("crosspane");
        for path in [&home, &tmp, &payload, &runtime] {
            root.directory(path);
        }
        let uid = rustix::process::geteuid().as_raw();
        let target = MacTarget::scratch(TargetPaths {
            uid,
            home,
            gui_tmpdir: tmp.clone(),
            runtime_override: None,
            payload_root: payload,
        })
        .unwrap();
        root.put(&target.agent_path(), b"inert signed-fixture agent", 0o755);
        root.directory(&target.state_dir());
        root.put(&runtime.join("bootstrap.json"), &serde_json::to_vec(&json!({
            "schema_version":1,"instance_id":71,"pid":4242,"started_unix_ms":0,
            "phase":"ready","phase_seq":1,"keystore":"os_store","reason":null,"runtime_dir":runtime
        })).unwrap(), 0o600);
        let listener = UnixListener::bind(target.socket_path()).unwrap();
        // The sole pathname bind is inside this exclusive newly-created 0700 scratch root.
        let socket_parent = root.directory(target.socket_path().parent().unwrap());
        rfs::chmodat(
            socket_parent,
            target.socket_path().file_name().unwrap(),
            rfs::Mode::RUSR | rfs::Mode::WUSR,
            rfs::AtFlags::SYMLINK_NOFOLLOW,
        )
        .unwrap();
        let support_probe = Arc::new(Support {
            facts: Mutex::new(SupportObservation {
                macos_major: 26,
                apple_silicon: true,
                gui_tmpdir: tmp,
                gui: GuiObservation {
                    console_uid: Some(uid),
                    interactive_uid: Some(uid),
                    console_session: "fake-Aqua".into(),
                    interactive_session: "fake-Aqua".into(),
                    active: true,
                },
            }),
            hook: Mutex::new(None),
        });
        let runner = Arc::new(Runner { uid, exe: target.agent_path(), state: AtomicU64::new(0),
            calls: Mutex::default(), hook: Mutex::new(None),
            reply: Mutex::new(Ok(CommandOutput { code: Some(0), stdout: serde_json::to_vec(&json!({
                "schema_version":1,"result":"removed","reason":null,"key":"removed","trust":"removed"
            })).unwrap(), stderr: vec![] })) });
        let clock = Arc::new(ClockFake::default());
        let io = Arc::new(
            MacNativeIo::new(
                target,
                runner.clone(),
                support_probe.clone(),
                Arc::new(Signatures),
                clock.clone(),
            )
            .unwrap(),
        );
        let deadline = Deadline::new(5000, clock.clone(), Cancellation::default()).unwrap();
        let requirement = SigningRequirement {
            role: ArtifactRole::Agent,
            identifier: AGENT_LABEL.into(),
            designated_requirement: "fixture-designated-requirement".into(),
            entitlements: BTreeMap::new(),
        };
        let signature = io
            .admit_main_signature(&io.target().agent_path(), &requirement, &deadline)
            .unwrap();
        let proof = io.admit_support(&signature, &deadline).unwrap();
        let instance = Arc::new(io.admit_instance(&proof, &signature, &deadline).unwrap());
        runner.calls.lock().unwrap().clear();
        Self {
            root,
            io,
            clock,
            support_probe,
            proof,
            signature,
            instance,
            runner,
            _listener: listener,
        }
    }
    fn deadline(&self) -> Deadline {
        Deadline::new(5000, self.clock.clone(), Cancellation::default()).unwrap()
    }
    fn track(&self) -> Arc<TrackedAgent> {
        self.io
            .track_original(
                &self.proof,
                &self.signature,
                self.instance.clone(),
                &self.deadline(),
            )
            .unwrap()
    }
    fn receipt(&self, instance: u64, parking: &str, journals: bool, audio: bool) {
        self.root.put(
            &self.io.target().state_dir().join("last_exit.json"),
            &serde_json::to_vec(&json!({
                "schema_version":1,"instance_id":instance,"stopped_unix_ms":1,
                "clean":parking != "failed" && journals && audio,"parking":parking,
                "input_journals_empty":journals,"audio_stopped":audio
            }))
            .unwrap(),
            0o600,
        );
    }
    fn clean(&self) -> CleanAgentExit {
        let tracked = self.track();
        self.runner.state.store(1, Ordering::Release);
        self.receipt(71, "restored", true, true);
        self.io
            .observe_clean_exit(tracked, &self.proof, &self.deadline())
            .unwrap()
            .unwrap()
    }
    fn dispatches(&self) -> usize {
        self.runner
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.program == self.runner.exe)
            .count()
    }
}
#[test]
fn a1_red_capture_preserves_exact_original() {
    let f = Fixture::new();
    let original = f.track();
    assert_eq!(original.instance_id(), 71);
    assert_eq!(original.process(), f.instance.process());
    assert!(!f.runner.calls.lock().unwrap().is_empty());
}
#[test]
fn a1_red_clean_receipt_requires_actual_original_exit() {
    let f = Fixture::new();
    let original = f.track();
    f.receipt(71, "restored", true, true);
    assert!(
        f.io.observe_clean_exit(original.clone(), &f.proof, &f.deadline())
            .unwrap()
            .is_none()
    );
    f.runner.state.store(1, Ordering::Release);
    let proof =
        f.io.observe_clean_exit(original, &f.proof, &f.deadline())
            .unwrap()
            .unwrap();
    assert_eq!(proof.receipt().instance_id, 71);
}
#[test]
fn a1_red_exact_installed_erase_argv_environment_and_semantics() {
    let f = Fixture::new();
    let receipt =
        f.io.erase_installed_identity(f.clean(), &f.proof, &f.deadline())
            .unwrap();
    assert!(receipt.identity_and_pairings_removed());
    let calls = f.runner.calls.lock().unwrap();
    let call = calls.iter().find(|c| c.program == f.runner.exe).unwrap();
    assert_eq!(call.args, ["erase-identity"]);
    assert_eq!(call.limit, 4096);
    assert_eq!(
        call.env,
        BTreeMap::from([
            (
                "HOME".into(),
                f.io.target().paths().home.to_str().unwrap().into()
            ),
            (
                "TMPDIR".into(),
                f.io.target().paths().gui_tmpdir.to_str().unwrap().into()
            ),
            (
                "CROSSPANE_RUNTIME_DIR".into(),
                f.io.target().runtime_dir().to_str().unwrap().into()
            ),
            ("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into()),
            ("LC_ALL".into(), "C".into()),
            ("TZ".into(), "UTC".into())
        ])
    );
}
#[test]
fn a1_red_one_attempt_survives_new_exit_observation() {
    let f = Fixture::new();
    let original = f.track();
    f.runner.state.store(1, Ordering::Release);
    f.receipt(71, "restored", true, true);
    let clean =
        f.io.observe_clean_exit(original.clone(), &f.proof, &f.deadline())
            .unwrap()
            .unwrap();
    f.io.erase_installed_identity(clean, &f.proof, &f.deadline())
        .unwrap();
    let another =
        f.io.observe_clean_exit(original, &f.proof, &f.deadline())
            .unwrap()
            .unwrap();
    assert_eq!(
        f.io.erase_installed_identity(another, &f.proof, &f.deadline()),
        Err(NativeError::Refused)
    );
    assert_eq!(f.dispatches(), 1);
}

#[test]
fn stale_future_and_malformed_exit_receipts_refuse_clean_proof() {
    for mode in 0..3 {
        let f = Fixture::new();
        let original = if mode == 0 {
            f.runner.state.store(2, Ordering::Release);
            let instance = Arc::new(
                f.io.admit_instance(&f.proof, &f.signature, &f.deadline())
                    .unwrap(),
            );
            f.io.track_original(&f.proof, &f.signature, instance, &f.deadline())
                .unwrap()
        } else {
            f.track()
        };
        f.runner.state.store(1, Ordering::Release);
        f.receipt(71, "restored", true, true);
        let path = f.io.target().state_dir().join("last_exit.json");
        if mode == 1 {
            let mut value: serde_json::Value =
                serde_json::from_slice(&f.io.read(&path, 4096, true, &f.deadline()).unwrap())
                    .unwrap();
            value["stopped_unix_ms"] = json!(u64::MAX);
            f.root
                .put(&path, &serde_json::to_vec(&value).unwrap(), 0o600);
        } else if mode == 2 {
            f.root.put(&path, b"{\"clean\":true}", 0o600);
        }
        assert!(
            f.io.observe_clean_exit(original, &f.proof, &f.deadline())
                .is_err()
        );
        assert_eq!(f.dispatches(), 0);
    }
}
#[test]
fn bootstrap_change_between_exit_observations_refuses_cleanup() {
    let f = Fixture::new();
    let original = f.track();
    f.runner.state.store(1, Ordering::Release);
    f.receipt(71, "restored", true, true);
    let root = f.root.clone();
    let path = f.io.target().runtime_dir().join("bootstrap.json");
    let runtime = f.io.target().runtime_dir().to_owned();
    let once = AtomicBool::new(false);
    *f.runner.hook.lock().unwrap() = Some(Arc::new(move || {
        if !once.swap(true, Ordering::AcqRel) {
            root.put(
                &path,
                &serde_json::to_vec(&json!({"schema_version":1,"instance_id":72,
                "pid":4243,"started_unix_ms":1000,"phase":"ready","phase_seq":1,
                "keystore":"os_store","reason":null,"runtime_dir":runtime}))
                .unwrap(),
                0o600,
            );
        }
    }));
    assert_eq!(
        f.io.observe_clean_exit(original, &f.proof, &f.deadline())
            .unwrap_err(),
        NativeError::Foreign
    );
    assert_eq!(f.dispatches(), 0);
}
#[test]
fn symlink_receipt_never_authorizes_cleanup() {
    let f = Fixture::new();
    let original = f.track();
    f.runner.state.store(1, Ordering::Release);
    f.receipt(71, "restored", true, true);
    let receipt = f.io.target().state_dir().join("last_exit.json");
    let other = f.root.path.join("foreign-looking-receipt");
    f.root.put(
        &other,
        &f.io.read(&receipt, 4096, true, &f.deadline()).unwrap(),
        0o600,
    );
    f.root.remove(&receipt);
    let parent = f.root.directory(receipt.parent().unwrap());
    rfs::symlinkat(&other, parent, receipt.file_name().unwrap()).unwrap();
    assert!(
        f.io.observe_clean_exit(original, &f.proof, &f.deadline())
            .is_err()
    );
    assert_eq!(f.dispatches(), 0);
}
#[test]
fn proof_expiring_during_final_erase_preflight_dispatches_nothing() {
    let f = Fixture::new();
    let clean = f.clean();
    let clock = f.clock.clone();
    let count = AtomicU64::new(0);
    *f.support_probe.hook.lock().unwrap() = Some(Arc::new(move || {
        if count.fetch_add(1, Ordering::AcqRel) == 1 {
            clock.0.store(5001, Ordering::Release);
        }
    }));
    let deadline = Deadline::new(10000, f.clock.clone(), Cancellation::default()).unwrap();
    assert_eq!(
        f.io.erase_installed_identity(clean, &f.proof, &deadline),
        Err(NativeError::Unsupported)
    );
    assert_eq!(f.dispatches(), 0);
}
#[test]
fn post_dispatch_revocation_preserves_unknown_outcome() {
    let f = Fixture::new();
    let clean = f.clean();
    let proof = f.proof.clone();
    let runner = f.runner.clone();
    *f.runner.hook.lock().unwrap() = Some(Arc::new(move || {
        if runner
            .calls
            .lock()
            .unwrap()
            .last()
            .is_some_and(|c| c.program == runner.exe)
        {
            proof.revoke();
        }
    }));
    assert_eq!(
        f.io.erase_installed_identity(clean, &f.proof, &f.deadline()),
        Err(NativeError::OutcomeUnknown)
    );
    assert_eq!(f.dispatches(), 1);
}
#[test]
fn fixed_disable_and_generic_agent_refusal_stay_separate() {
    let f = Fixture::new();
    let disable = CommandSpec::disable_agent(f.io.target()).unwrap();
    assert_eq!(disable.program(), Path::new("/bin/launchctl"));
    assert_eq!(
        disable.args(),
        ["disable", &format!("gui/{}/{AGENT_LABEL}", f.runner.uid)]
    );
    assert!(disable.is_mutation());
    for args in [
        vec![],
        vec!["erase-identity".into()],
        vec!["erase-identity".into(), "--keep-trust".into()],
    ] {
        assert_eq!(
            CommandSpec::new(
                f.io.target(),
                NativeOperation::ControlledChild {
                    signature: Box::new(f.signature.clone()),
                    args,
                }
            )
            .unwrap_err(),
            NativeError::Invalid
        );
    }
}
#[test]
fn stopped_before_capture_cannot_mint_original_authority() {
    let f = Fixture::new();
    f.runner.state.store(1, Ordering::Release);
    f.receipt(71, "restored", true, true);
    assert!(
        f.io.track_original(&f.proof, &f.signature, f.instance.clone(), &f.deadline())
            .is_err()
    );
    assert_eq!(f.dispatches(), 0);
}
#[test]
fn reused_pid_and_error_absence_keep_identity() {
    for state in [2, 3] {
        let f = Fixture::new();
        let original = f.track();
        f.runner.state.store(state, Ordering::Release);
        f.receipt(71, "restored", true, true);
        assert!(
            f.io.observe_clean_exit(original, &f.proof, &f.deadline())
                .is_err()
        );
        assert_eq!(f.dispatches(), 0);
    }
}
#[test]
fn missing_unclean_and_wrong_instance_never_authorize_erase() {
    for (instance, parking, journals, audio) in [
        (71, "failed", true, true),
        (71, "restored", false, true),
        (71, "restored", true, false),
        (70, "restored", true, true),
    ] {
        let f = Fixture::new();
        let original = f.track();
        f.runner.state.store(1, Ordering::Release);
        assert!(
            f.io.observe_clean_exit(original.clone(), &f.proof, &f.deadline())
                .unwrap()
                .is_none()
        );
        f.receipt(instance, parking, journals, audio);
        assert!(!matches!(
            f.io.observe_clean_exit(original, &f.proof, &f.deadline()),
            Ok(Some(_))
        ));
        assert_eq!(f.dispatches(), 0);
    }
}
#[test]
fn newer_bootstrap_and_replaced_executable_refuse_clean_proof() {
    for replace in [false, true] {
        let f = Fixture::new();
        let original = f.track();
        f.runner.state.store(1, Ordering::Release);
        f.receipt(71, "restored", true, true);
        if replace {
            f.root.put(&f.runner.exe, b"changed agent", 0o755);
        } else {
            f.root.put(
                &f.io.target().runtime_dir().join("bootstrap.json"),
                &serde_json::to_vec(&json!({
                    "schema_version":1,"instance_id":72,"pid":4243,"started_unix_ms":1000,
                    "phase":"starting","phase_seq":1,"keystore":null,"reason":null,
                    "runtime_dir":f.io.target().runtime_dir()
                }))
                .unwrap(),
                0o600,
            );
        }
        assert!(
            f.io.observe_clean_exit(original, &f.proof, &f.deadline())
                .is_err()
        );
        assert_eq!(f.dispatches(), 0);
    }
}
#[test]
fn absent_bootstrap_after_tracked_exit_still_requires_literal_receipt() {
    let f = Fixture::new();
    let original = f.track();
    f.runner.state.store(1, Ordering::Release);
    f.root
        .remove(&f.io.target().runtime_dir().join("bootstrap.json"));
    assert!(
        f.io.observe_clean_exit(original.clone(), &f.proof, &f.deadline())
            .unwrap()
            .is_none()
    );
    f.receipt(71, "nothing_parked", true, true);
    assert!(
        f.io.observe_clean_exit(original, &f.proof, &f.deadline())
            .unwrap()
            .is_some()
    );
}
#[test]
fn erase_exit_zero_does_not_override_semantic_refusal_waiting_or_failure() {
    for (result, reason, key, trust) in [
        ("refused", "agent_running", "kept", "kept"),
        ("waiting", "keystore_locked", "kept", "kept"),
        ("failed", "keystore_error", "failed", "kept"),
        ("removed", "io", "removed", "failed"),
    ] {
        let f = Fixture::new();
        *f.runner.reply.lock().unwrap() = Ok(CommandOutput { code: Some(0),
            stdout: serde_json::to_vec(&json!({"schema_version":1,"result":result,"reason":reason,"key":key,"trust":trust})).unwrap(),
            stderr: vec![] });
        let result =
            f.io.erase_installed_identity(f.clean(), &f.proof, &f.deadline());
        assert!(!result.is_ok_and(|r| r.identity_and_pairings_removed()));
        assert_eq!(f.dispatches(), 1);
    }
}
#[test]
fn malformed_nonzero_and_oversize_erase_are_unknown_no_resend() {
    for output in [
        CommandOutput {
            code: Some(0),
            stdout: b"not json".to_vec(),
            stderr: vec![],
        },
        CommandOutput {
            code: Some(1),
            stdout: vec![],
            stderr: vec![],
        },
        CommandOutput {
            code: Some(0),
            stdout: vec![b' '; 4097],
            stderr: vec![],
        },
        CommandOutput {
            code: Some(0),
            stdout: vec![],
            stderr: b"diagnostic".to_vec(),
        },
    ] {
        let f = Fixture::new();
        let original = f.track();
        f.runner.state.store(1, Ordering::Release);
        f.receipt(71, "restored", true, true);
        *f.runner.reply.lock().unwrap() = Ok(output);
        let clean =
            f.io.observe_clean_exit(original.clone(), &f.proof, &f.deadline())
                .unwrap()
                .unwrap();
        assert_eq!(
            f.io.erase_installed_identity(clean, &f.proof, &f.deadline()),
            Err(NativeError::OutcomeUnknown)
        );
        let retry =
            f.io.observe_clean_exit(original, &f.proof, &f.deadline())
                .unwrap()
                .unwrap();
        assert_eq!(
            f.io.erase_installed_identity(retry, &f.proof, &f.deadline()),
            Err(NativeError::Refused)
        );
        assert_eq!(f.dispatches(), 1);
    }
}
#[test]
fn revoked_expired_and_foreign_proofs_dispatch_nothing() {
    for mode in 0..3 {
        let f = Fixture::new();
        let clean = f.clean();
        match mode {
            0 => f.proof.revoke(),
            1 => {
                f.clock.0.store(5001, Ordering::Release);
            }
            _ => {}
        }
        let target = if mode == 2 {
            Fixture::new().io
        } else {
            f.io.clone()
        };
        assert!(
            target
                .erase_installed_identity(clean, &f.proof, &f.deadline())
                .is_err()
        );
        assert_eq!(f.dispatches(), 0);
    }
}
#[derive(Default)]
struct Stall {
    entered: AtomicBool,
    released: Mutex<bool>,
    changed: Condvar,
    completed: AtomicBool,
}
impl Stall {
    fn block(&self) {
        self.entered.store(true, Ordering::Release);
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.changed.wait(released).unwrap();
        }
        self.completed.store(true, Ordering::Release);
    }
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
#[test]
fn capture_noncooperative_support_is_bounded_and_cancelled() {
    for cancel in [false, true] {
        let f = Fixture::new();
        let stall = Arc::new(Stall::default());
        let callback = stall.clone();
        let cancellation = Cancellation::default();
        let cancel_worker = cancellation.clone();
        *f.support_probe.hook.lock().unwrap() = Some(Arc::new(move || {
            if cancel {
                cancel_worker.cancel();
            }
            callback.block();
        }));
        let deadline = Deadline::new(40, f.clock.clone(), cancellation.clone()).unwrap();
        let now = Instant::now();
        let result =
            f.io.track_original(&f.proof, &f.signature, f.instance.clone(), &deadline);
        assert!(matches!(
            result,
            Err(NativeError::Timeout | NativeError::Cancelled)
        ));
        assert!(now.elapsed() < Duration::from_secs(1));
        stall.release();
        assert_eq!(f.dispatches(), 0);
    }
}
#[test]
fn expiry_during_support_observation_never_returns_authority_or_dispatches() {
    let f = Fixture::new();
    let clock = f.clock.clone();
    *f.support_probe.hook.lock().unwrap() = Some(Arc::new(move || {
        clock.0.store(5001, Ordering::Release);
    }));
    let deadline = Deadline::new(10000, f.clock.clone(), Cancellation::default()).unwrap();
    assert_eq!(
        f.io.track_original(&f.proof, &f.signature, f.instance.clone(), &deadline)
            .unwrap_err(),
        NativeError::Unsupported
    );
    assert_eq!(f.dispatches(), 0);
}
#[test]
fn erase_noncooperative_runner_returns_unknown_and_never_retries() {
    let f = Fixture::new();
    let original = f.track();
    f.runner.state.store(1, Ordering::Release);
    f.receipt(71, "restored", true, true);
    let clean =
        f.io.observe_clean_exit(original.clone(), &f.proof, &f.deadline())
            .unwrap()
            .unwrap();
    let stall = Arc::new(Stall::default());
    let blocked = stall.clone();
    let first = Arc::new(AtomicBool::new(false));
    let latch = first.clone();
    // Block only the one-shot (after read-only ps preflight), never a real child.
    let runner = f.runner.clone();
    let clock = f.clock.clone();
    *f.runner.hook.lock().unwrap() = Some(Arc::new(move || {
        if runner
            .calls
            .lock()
            .unwrap()
            .last()
            .is_some_and(|c| c.program == runner.exe)
            && !latch.swap(true, Ordering::AcqRel)
        {
            clock.0.store(1001, Ordering::Release);
            blocked.block();
        }
    }));
    let deadline = Deadline::new(1000, f.clock.clone(), Cancellation::default()).unwrap();
    let now = Instant::now();
    assert_eq!(
        f.io.erase_installed_identity(clean, &f.proof, &deadline),
        Err(NativeError::OutcomeUnknown)
    );
    assert!(now.elapsed() < Duration::from_secs(1));
    assert!(stall.entered.load(Ordering::Acquire));
    stall.release();
    // No capacity retry: clear the hook and require explicit read-only re-observation.
    *f.runner.hook.lock().unwrap() = None;
    let retry =
        f.io.observe_clean_exit(original, &f.proof, &f.deadline())
            .unwrap()
            .unwrap();
    assert_eq!(
        f.io.erase_installed_identity(retry, &f.proof, &f.deadline()),
        Err(NativeError::Refused)
    );
    assert_eq!(f.dispatches(), 1);
}

#[test]
fn r1_duplicate_captures_share_unknown_erase_latch() {
    let f = Fixture::new();
    let first = f.track();
    let second = f.track();
    f.runner.state.store(1, Ordering::Release);
    f.receipt(71, "restored", true, true);
    let first =
        f.io.observe_clean_exit(first, &f.proof, &f.deadline())
            .unwrap()
            .unwrap();
    let second =
        f.io.observe_clean_exit(second.clone(), &f.proof, &f.deadline())
            .unwrap()
            .unwrap();
    *f.runner.reply.lock().unwrap() = Err(NativeError::OutcomeUnknown);
    assert_eq!(
        f.io.erase_installed_identity(first, &f.proof, &f.deadline()),
        Err(NativeError::OutcomeUnknown)
    );
    let result =
        f.io.erase_installed_identity(second, &f.proof, &f.deadline());
    assert_eq!(
        f.dispatches(),
        1,
        "separate captures must share the one-shot authority"
    );
    assert_eq!(result, Err(NativeError::Refused));
}

fn receipt_drift_during_final_absence(mode: u8) {
    let f = Fixture::new();
    let original = f.track();
    f.runner.state.store(1, Ordering::Release);
    f.receipt(71, "restored", true, true);
    let path = f.io.target().state_dir().join("last_exit.json");
    let bootstrap = f.io.target().runtime_dir().join("bootstrap.json");
    let before_bootstrap = f.io.read(&bootstrap, 4096, true, &f.deadline()).unwrap();
    let before_receipt = f.io.read(&path, 4096, true, &f.deadline()).unwrap();
    let root = f.root.clone();
    let observed = AtomicU64::new(0);
    *f.runner.hook.lock().unwrap() = Some(Arc::new(move || {
        if observed.fetch_add(1, Ordering::AcqRel) == 2 {
            match mode {
                0 => root.remove(&path),
                1 => root.put(&path, &serde_json::to_vec(&json!({
                    "schema_version":1,"instance_id":71,"stopped_unix_ms":2,
                    "clean":true,"parking":"restored","input_journals_empty":true,"audio_stopped":true
                })).unwrap(), 0o600),
                2 => {
                    let parent = root.directory(path.parent().unwrap());
                    rfs::renameat(&parent, path.file_name().unwrap(), &parent, "retained-exit.json").unwrap();
                    root.put(&path, &before_receipt, 0o600);
                }
                _ => unreachable!(),
            }
        }
    }));
    let result = f.io.observe_clean_exit(original, &f.proof, &f.deadline());
    assert_eq!(
        f.io.read(&bootstrap, 4096, true, &f.deadline()).unwrap(),
        before_bootstrap
    );
    assert!(
        result.is_err(),
        "receipt drift after its read must never mint clean authority"
    );
    assert_eq!(f.dispatches(), 0);
}
#[test]
fn r1_deleted_receipt_after_final_absence_refuses_clean_authority() {
    receipt_drift_during_final_absence(0);
}
#[test]
fn r1_changed_receipt_after_final_absence_refuses_clean_authority() {
    receipt_drift_during_final_absence(1);
}
#[test]
fn r1_replaced_receipt_identity_after_final_absence_refuses_clean_authority() {
    receipt_drift_during_final_absence(2);
}

#[test]
fn r1_renewed_support_after_six_second_exit_preserves_original() {
    let f = Fixture::new();
    let original = f.track();
    f.clock.0.store(6000, Ordering::Release);
    let renewed = f.io.admit_support(&f.signature, &f.deadline()).unwrap();
    f.runner.state.store(1, Ordering::Release);
    f.receipt(71, "restored", true, true);
    let clean =
        f.io.observe_clean_exit(original.clone(), &renewed, &f.deadline())
            .unwrap()
            .unwrap();
    assert_eq!(clean.receipt().instance_id, original.instance_id());
    assert!(
        f.io.erase_installed_identity(clean, &renewed, &f.deadline())
            .unwrap()
            .identity_and_pairings_removed()
    );
    assert_eq!(f.dispatches(), 1);
}
#[test]
fn r1_support_renews_between_clean_observation_and_erase() {
    let f = Fixture::new();
    let clean = f.clean();
    f.clock.0.store(6000, Ordering::Release);
    let renewed = f.io.admit_support(&f.signature, &f.deadline()).unwrap();
    assert!(
        f.io.erase_installed_identity(clean, &renewed, &f.deadline())
            .unwrap()
            .identity_and_pairings_removed()
    );
    assert_eq!(f.dispatches(), 1);
}
#[test]
fn r1_foreign_fresh_support_cannot_authorize_original() {
    let f = Fixture::new();
    let foreign = Fixture::new();
    let original = f.track();
    f.runner.state.store(1, Ordering::Release);
    f.receipt(71, "restored", true, true);
    let clean =
        f.io.observe_clean_exit(original.clone(), &f.proof, &f.deadline())
            .unwrap()
            .unwrap();
    assert!(
        f.io.observe_clean_exit(original, &foreign.proof, &f.deadline())
            .is_err()
    );
    assert!(
        f.io.erase_installed_identity(clean, &foreign.proof, &f.deadline())
            .is_err()
    );

    assert_eq!(f.dispatches(), 0);
}
#[test]
fn r1_revoked_and_changed_session_support_refuse_original() {
    for mode in 0..3 {
        let f = Fixture::new();
        let original = f.track();
        let clean_original = original.clone();
        f.runner.state.store(1, Ordering::Release);
        f.receipt(71, "restored", true, true);
        let clean =
            f.io.observe_clean_exit(clean_original, &f.proof, &f.deadline())
                .unwrap()
                .unwrap();
        if mode == 2 {
            let mut facts = f.support_probe.facts.lock().unwrap();
            facts.gui.console_session = "different-Aqua".into();
            facts.gui.interactive_session = "different-Aqua".into();
        }
        let renewed = f.io.admit_support(&f.signature, &f.deadline()).unwrap();
        match mode {
            0 => renewed.revoke(),
            1 => f.proof.revoke(),
            _ => {}
        }
        assert!(
            f.io.observe_clean_exit(original, &renewed, &f.deadline())
                .is_err()
        );
        assert!(
            f.io.erase_installed_identity(clean, &renewed, &f.deadline())
                .is_err()
        );
        assert_eq!(f.dispatches(), 0);
    }
}
#[test]
fn r1_revocation_during_fresh_support_observation_dispatches_nothing() {
    for revoke_original in [false, true] {
        let f = Fixture::new();
        let clean = f.clean();
        let renewed = f.io.admit_support(&f.signature, &f.deadline()).unwrap();
        let revoke = if revoke_original {
            f.proof.clone()
        } else {
            renewed.clone()
        };
        *f.support_probe.hook.lock().unwrap() = Some(Arc::new(move || revoke.revoke()));
        assert!(
            f.io.erase_installed_identity(clean, &renewed, &f.deadline())
                .is_err()
        );
        assert_eq!(f.dispatches(), 0);
    }
}

#[test]
fn r1_target_clones_and_support_renewal_keep_shared_latch() {
    let f = Fixture::new();
    let other_io = Arc::new(
        MacNativeIo::new(
            f.io.target().clone(),
            f.runner.clone(),
            f.support_probe.clone(),
            Arc::new(Signatures),
            f.clock.clone(),
        )
        .unwrap(),
    );
    let first = f.track();
    let second = other_io
        .track_original(&f.proof, &f.signature, f.instance.clone(), &f.deadline())
        .unwrap();
    f.runner.state.store(1, Ordering::Release);
    f.receipt(71, "restored", true, true);
    let clean =
        f.io.observe_clean_exit(first, &f.proof, &f.deadline())
            .unwrap()
            .unwrap();
    *f.runner.reply.lock().unwrap() = Err(NativeError::OutcomeUnknown);
    assert_eq!(
        f.io.erase_installed_identity(clean, &f.proof, &f.deadline()),
        Err(NativeError::OutcomeUnknown)
    );
    f.clock.0.store(6000, Ordering::Release);
    let renewed = other_io.admit_support(&f.signature, &f.deadline()).unwrap();
    let clean = other_io
        .observe_clean_exit(second, &renewed, &f.deadline())
        .unwrap()
        .unwrap();
    assert_eq!(
        other_io.erase_installed_identity(clean, &renewed, &f.deadline()),
        Err(NativeError::Refused)
    );
    assert_eq!(f.dispatches(), 1);
}
#[test]
fn r1_target_lifetime_latch_admission_is_bounded_without_eviction() {
    let f = Fixture::new();
    let mut handles = Vec::new();
    for id in 100..100 + MAX_NATIVE_CALLS as u64 {
        f.root.put(
            &f.io.target().runtime_dir().join("bootstrap.json"),
            &serde_json::to_vec(&json!({
                "schema_version":1,"instance_id":id,"pid":4242,"started_unix_ms":0,
                "phase":"ready","phase_seq":1,"keystore":"os_store","reason":null,
                "runtime_dir":f.io.target().runtime_dir()
            }))
            .unwrap(),
            0o600,
        );
        let instance = Arc::new(
            f.io.admit_instance(&f.proof, &f.signature, &f.deadline())
                .unwrap(),
        );
        handles.push(
            f.io.track_original(&f.proof, &f.signature, instance, &f.deadline())
                .unwrap(),
        );
    }
    drop(handles);
    f.root.put(
        &f.io.target().runtime_dir().join("bootstrap.json"),
        &serde_json::to_vec(&json!({
            "schema_version":1,"instance_id":999,"pid":4242,"started_unix_ms":0,
            "phase":"ready","phase_seq":1,"keystore":"os_store","reason":null,
            "runtime_dir":f.io.target().runtime_dir()
        }))
        .unwrap(),
        0o600,
    );
    let instance = Arc::new(
        f.io.admit_instance(&f.proof, &f.signature, &f.deadline())
            .unwrap(),
    );
    assert_eq!(
        f.io.track_original(&f.proof, &f.signature, instance, &f.deadline())
            .unwrap_err(),
        NativeError::Busy
    );
    assert_eq!(f.dispatches(), 0);
}
