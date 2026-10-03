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

// A2 compiles the actual adapters with their existing PRIVATE cfg(test) /Library mapping.
// Every root is a newly exclusive Scratch. No SystemCommandRunner or native probe is constructed.
use crosspane_installer::agent_contract;
#[path = "../src/platform/macos/audio_package.rs"]
#[allow(dead_code, unused_imports)]
mod audio_package;
#[path = "../src/platform/macos/launch_agent.rs"]
#[allow(dead_code, unused_imports)]
mod launch_agent;
#[path = "../src/platform/macos/native_io.rs"]
#[allow(dead_code, unused_imports)]
mod native_io;
#[path = "../src/platform/macos/payload.rs"]
#[allow(dead_code, unused_imports)]
mod payload;
#[path = "../src/platform/macos/removal.rs"]
#[allow(dead_code, unused_imports)]
mod removal;
#[path = "../src/platform/macos/transport.rs"]
#[allow(dead_code, unused_imports)]
mod transport;

mod a2_tests {
    use super::{
        Scratch, agent_contract::*, audio_package::*, launch_agent::*, native_io::*, payload::*,
        removal::*, transport::SelectedAgent,
    };
    use crosspane_installer_core::{
        InstallReceipt, MutationOutcome, OperationId, ResourceObservation, ResourceOwnership,
        ResourceReceipt, StepId,
    };
    use serde_json::{Value, json};
    use std::{
        collections::BTreeMap,
        path::{Path, PathBuf},
        sync::{
            Arc, Condvar, Mutex,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        time::{Duration, Instant},
    };
    const STATUS: &[u8] = br#"{"ok":true,"result":{"controlling":null,"controlled_by":null,"projections":[],
"displays":[],"peers":[],"layout":[],"installer":{"schema_version":1,
"build":{"version":"test-1","features":["private-vdisplay","video"]},"instance":{"id":1,"pid":4242,"uid":1,"exe":"fixture","runtime_dir":"fixture","started_unix_ms":0},
"config_revision":"1111111111111111","node":"1111111111111111111111111111111111111111111111111111111111111111",
"recovery_pending":0,"startup_recovery":"restored","gate":{"open":true,"session":"unlocked","active":true,"armed":true,"panic":false},
"epochs":{"gate":1,"grants":1,"layout":1,"backends":1},"keystore":"os_store",
"permissions":[{"name":"screen_recording","state":"granted"},{"name":"accessibility","state":"granted"},{"name":"input_monitoring","state":"granted"},{"name":"microphone","state":"granted"}],
"backends":[{"name":"capture","state":"ready","reason":null},{"name":"keys","state":"ready","reason":null},{"name":"pointer","state":"ready","reason":null},
{"name":"overlay","state":"ready","reason":null},{"name":"hotkeys","state":"ready","reason":null},{"name":"keystore","state":"ready","reason":null},
{"name":"windows","state":"ready","reason":null},{"name":"parking","state":"ready","reason":null},{"name":"frames","state":"ready","reason":null},
{"name":"tray","state":"ready","reason":null},{"name":"links","state":"ready","reason":null},{"name":"gpu","state":"ready","reason":null},
{"name":"home","state":"ready","reason":null},{"name":"audio","state":"ready","reason":null},{"name":"discovery","state":"ready","reason":null}],
"discovery":{"enabled":true,"running":true,"candidates":0,"error":null},"tray":{"created":true},
"audio":{"enabled":true,"active_peers":[],"frames_sent":0,"frames_played":0},"settings_opened":0,"peers":[]}}}"#;

    type Hook = Arc<dyn Fn(&str, &Path) -> NativeResult<()> + Send + Sync>;
    #[derive(Default)]
    struct FakeClock(AtomicU64);
    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::Acquire)
        }
    }
    impl AudioClock for FakeClock {
        fn unix_ms(&self) -> NativeResult<u64> {
            Ok(1_790_950_100_000 + self.now_ms())
        }
    }
    struct Support {
        facts: Mutex<SupportObservation>,
    }
    impl SupportProbe for Support {
        fn observe(&self, d: &Deadline) -> NativeResult<SupportObservation> {
            d.check()?;
            Ok(self.facts.lock().unwrap().clone())
        }
    }
    struct Signatures {
        bad: AtomicBool,
    }
    impl SignatureProbe for Signatures {
        fn observe(
            &self,
            _: &Path,
            r: &SigningRequirement,
            d: &Deadline,
        ) -> NativeResult<SignatureObservation> {
            d.check()?;
            Ok(SignatureObservation {
                strict_verified: !self.bad.load(Ordering::Acquire),
                team_identifier: "ABCDE12345".into(),
                identifier: r.identifier.clone(),
                designated_requirement: r.designated_requirement.clone(),
                entitlements: r.entitlements.clone(),
                apple_development: true,
                hardened_runtime: true,
                ad_hoc: false,
            })
        }
    }
    struct Runner {
        uid: u32,
        exe: PathBuf,
        pid: AtomicU64,
        print: Mutex<NativeResult<CommandOutput>>,
        disabled: Mutex<NativeResult<CommandOutput>>,
        calls: Mutex<Vec<(PathBuf, Vec<String>)>>,
        hook: Mutex<Option<Hook>>,
    }
    impl CommandRunner for Runner {
        fn run(&self, s: &CommandSpec, d: &Deadline) -> NativeResult<CommandOutput> {
            d.check()?;
            assert!(
                !s.is_mutation(),
                "observation must never dispatch a mutation"
            );
            self.calls
                .lock()
                .unwrap()
                .push((s.program().to_owned(), s.args().to_vec()));
            if let Some(h) = self.hook.lock().unwrap().clone() {
                h("command", s.program())?;
            }
            let output = if s.program() == Path::new("/bin/launchctl") {
                match s.args()[0].as_str() {
                    "print" => self.print.lock().unwrap().clone()?,
                    "print-disabled" => self.disabled.lock().unwrap().clone()?,
                    _ => panic!("forbidden launchctl argv {:?}", s.args()),
                }
            } else if s.program() == Path::new("/bin/ps") {
                if s.args()[3] != self.pid.load(Ordering::Acquire).to_string() {
                    return Ok(out(1, "", ""));
                }
                let bytes = match s.args()[1].as_str() {
                    "uid=" => format!("{}\n", self.uid),
                    "comm=" => format!("{}\n", self.exe.display()),
                    "lstart=" => "Thu Jan  1 00:00:00 1970\n".into(),
                    _ => panic!("forbidden ps argv"),
                };
                out(0, &bytes, "")
            } else {
                assert_eq!(s.program(), Path::new("/usr/bin/codesign"));
                assert_eq!(&s.args()[..2], ["--verify", "--strict"]);
                out(0, "", "")
            };
            d.check()?;
            Ok(output)
        }
    }
    fn out(code: i32, stdout: &str, stderr: &str) -> CommandOutput {
        CommandOutput {
            code: Some(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }
    fn sha(bytes: &[u8]) -> [u8; 32] {
        aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes)
            .as_ref()
            .try_into()
            .unwrap()
    }
    fn hex(bytes: &[u8]) -> String {
        sha(bytes).iter().map(|b| format!("{b:02x}")).collect()
    }
    fn macho() -> Vec<u8> {
        let mut b = vec![0; 33];
        for (at, v) in [(0, 0xfeedfacfu32), (4, 0x0100000c), (12, 2)] {
            b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        }
        b[32] = 1;
        b
    }
    fn packages() -> Vec<u8> {
        format!("{{\"schema_version\":1,\"version\":\"0.1.0\",\"packages\":[{{\"kind\":\"install\",\"file\":\"CrosspaneAudio-install-0.1.0.pkg\",\"sha256\":\"{}\"}},{{\"kind\":\"remove\",\"file\":\"CrosspaneAudio-remove-0.1.0.pkg\",\"sha256\":\"{}\"}}]}}\n",hex(b"inert-install"),hex(b"inert-remove")).into_bytes()
    }
    fn inventory() -> ApprovedInventory {
        let entries = [
            (
                "Crosspane.app/Contents/MacOS/Crosspane",
                Some(PayloadRole::Agent),
            ),
            (
                "Crosspane.app/Contents/MacOS/crosspane-ui",
                Some(PayloadRole::Settings),
            ),
            (
                "Crosspane.app/Contents/MacOS/crosspane-tutorial",
                Some(PayloadRole::Tutorial),
            ),
            ("crosspanectl", Some(PayloadRole::Ctl)),
            ("crosspane-installer", Some(PayloadRole::Installer)),
            ("Crosspane.app/Contents/Info.plist", None),
            ("Crosspane.app/Contents/Resources/audio/packages.json", None),
            (
                "Crosspane.app/Contents/Resources/audio/CrosspaneAudio-install-0.1.0.pkg",
                None,
            ),
            (
                "Crosspane.app/Contents/Resources/audio/CrosspaneAudio-remove-0.1.0.pkg",
                None,
            ),
        ];
        ApprovedInventory {
            product_version: "test-1".into(),
            features: vec!["private-vdisplay".into(), "video".into()],
            files: entries
                .into_iter()
                .map(|(path, role)| {
                    let b = data(path);
                    PayloadFile {
                        path: path.into(),
                        size: b.len() as u64,
                        sha256: sha(&b),
                        mode: if role.is_some() { 0o755 } else { 0o644 },
                        signing: role.map(|role| SigningRule {
                            role,
                            identifier: if role == PayloadRole::Agent {
                                AGENT_LABEL.into()
                            } else {
                                format!("test.approved.{role:?}")
                            },
                            designated_requirement: "trusted-test-development-requirement".into(),
                            entitlements: BTreeMap::new(),
                        }),
                    }
                })
                .collect(),
        }
    }
    fn producer_inventory() -> ApprovedInventory {
        let mut approved = inventory();
        approved.files.sort_by(|a, b| a.path.cmp(&b.path));
        approved
    }
    fn data(path: &str) -> Vec<u8> {
        if path.ends_with("packages.json") {
            packages()
        } else if path.ends_with(".pkg") {
            if path.contains("-install-") {
                b"inert-install".to_vec()
            } else {
                b"inert-remove".to_vec()
            }
        } else if path.ends_with("Info.plist") {
            b"<plist><dict><key>CFBundleIdentifier</key><string>io.frostdev.crosspane.agent</string></dict></plist>".to_vec()
        } else {
            macho()
        }
    }
    struct Rig {
        scratch: Arc<Scratch>,
        io: Arc<MacNativeIo>,
        clock: Arc<FakeClock>,
        support: Arc<Support>,
        signatures: Arc<Signatures>,
        runner: Arc<Runner>,
        hook: Arc<Mutex<Option<Hook>>>,
        _listener: std::os::unix::net::UnixListener,
    }
    impl Rig {
        fn new() -> Self {
            let scratch = Scratch::new();
            let home = scratch.path.join("home");
            let tmp = scratch.path.join("temporary");
            let source = home.join("payload");
            let library = scratch.path.join("Library");
            for p in [&home, &tmp, &source, &library] {
                scratch.directory(p);
            }
            for p in [
                "Audio/Plug-Ins/HAL",
                "Application Support/Crosspane/Installer",
            ] {
                scratch.directory(&library.join(p));
            }
            let uid = rustix::process::geteuid().as_raw();
            let mut target = MacTarget::scratch(TargetPaths {
                uid,
                home: home.clone(),
                gui_tmpdir: tmp.clone(),
                runtime_override: None,
                payload_root: source.clone(),
            })
            .unwrap();
            let mapped = library.clone();
            target.test_path = Some(Arc::new(move |p| {
                p.strip_prefix("/Library")
                    .map(|s| mapped.join(s))
                    .unwrap_or_else(|_| p.to_owned())
            }));
            let hook: Arc<Mutex<Option<Hook>>> = Arc::new(Mutex::default());
            let injected = hook.clone();
            let held = scratch.clone();
            target.test_hook = Some(Arc::new(move |stage, p, mut id| {
                let _retain_scratch = &held;
                if stage.starts_with("audio-") {
                    assert!(p.starts_with("/Library"));
                    if let Some(id) = &mut id {
                        id.uid = 0;
                    }
                }
                let h = injected.lock().unwrap().clone();
                if let Some(h) = h {
                    h(stage, p)?;
                }
                Ok(id)
            }));
            let support = Arc::new(Support {
                facts: Mutex::new(SupportObservation {
                    macos_major: 26,
                    apple_silicon: true,
                    gui: GuiObservation {
                        console_uid: Some(uid),
                        interactive_uid: Some(uid),
                        console_session: "fixture-Aqua".into(),
                        interactive_session: "fixture-Aqua".into(),
                        active: true,
                    },
                    gui_tmpdir: tmp,
                }),
            });
            let signatures = Arc::new(Signatures {
                bad: AtomicBool::new(false),
            });
            let plist = home.join("Library/LaunchAgents/io.frostdev.crosspane.agent.plist");
            let print = format!(
                "gui/{uid}/{AGENT_LABEL} = {{\n path = {}\n program = {}\n pid = 4242\n arguments = {{\n {}\n run\n }}\n environment = {{\n RUST_LOG => info\n }}\n}}\n",
                plist.display(),
                target.agent_path().display(),
                target.agent_path().display()
            );
            let runner = Arc::new(Runner {
                uid,
                exe: target.agent_path(),
                pid: AtomicU64::new(4242),
                print: Mutex::new(Ok(out(0, &print, ""))),
                disabled: Mutex::new(Ok(out(
                    0,
                    "disabled services = {\n \"io.frostdev.crosspane.agent\" => false\n}\n",
                    "",
                ))),
                calls: Mutex::default(),
                hook: Mutex::default(),
            });
            let clock = Arc::new(FakeClock::default());
            for f in inventory().files {
                scratch.put(&source.join(&f.path), &data(&f.path), f.mode);
                let path = if f.path == "crosspanectl" {
                    home.join(".local/bin/crosspanectl")
                } else {
                    home.join("Applications").join(&f.path)
                };
                if f.path != "crosspane-installer" {
                    scratch.put(&path, &data(&f.path), f.mode);
                }
            }
            scratch.directory(target.runtime_dir());
            let listener = std::os::unix::net::UnixListener::bind(target.socket_path()).unwrap();
            rustix::fs::chmodat(
                scratch.directory(target.runtime_dir()),
                "agent.sock",
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
                rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
            )
            .unwrap();
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
            let r = Self {
                scratch,
                io,
                clock,
                support,
                signatures,
                runner,
                hook,
                _listener: listener,
            };
            r.put(&r.io.target().runtime_dir().join("bootstrap.json"),&serde_json::to_vec(&json!({"schema_version":1,"instance_id":1,"pid":4242,"started_unix_ms":0,"phase":"ready","phase_seq":1,"keystore":"os_store","reason":null,"runtime_dir":r.io.target().runtime_dir()})).unwrap(),0o600);
            r.put(&plist, &render_plist(r.io.target()).unwrap(), 0o644);
            r.hints(ResourceOwnership::Created);
            r
        }
        fn put(&self, p: &Path, b: &[u8], mode: u32) {
            self.scratch.put(p, b, mode);
        }
        fn d(&self) -> Deadline {
            Deadline::new(5000, self.clock.clone(), Cancellation::default()).unwrap()
        }
        fn audio(&self) -> PathBuf {
            self.io
                .target()
                .paths()
                .payload_root
                .join("Crosspane.app/Contents/Resources/audio")
        }
        fn observer(&self) -> MacRemovalObserver {
            MacRemovalObserver::admit(
                self.io.clone(),
                inventory(),
                self.audio(),
                self.clock.clone(),
                &self.d(),
            )
            .unwrap()
        }
        fn row<'a>(&self, o: &'a RemovalObservation, id: &str) -> &'a UserResource {
            o.inventory().resources.iter().find(|r| r.id == id).unwrap()
        }
        fn observe(&self, o: &MacRemovalObserver) -> RemovalObservation {
            o.observe(Some(self.current()), 1, OperationId(1), &self.d())
                .unwrap()
        }
        fn selected(&self) -> SelectedAgent {
            let rule = inventory().files[0].signing.clone().unwrap();
            let s = SigningRequirement {
                role: ArtifactRole::Agent,
                identifier: rule.identifier,
                designated_requirement: rule.designated_requirement,
                entitlements: rule.entitlements,
            };
            let main = self
                .io
                .admit_main_signature(&self.io.target().agent_path(), &s, &self.d())
                .unwrap();
            let support = self.io.admit_support(&main, &self.d()).unwrap();
            let instance = Arc::new(self.io.admit_instance(&support, &main, &self.d()).unwrap());
            SelectedAgent {
                io: self.io.clone(),
                support,
                instance,
                link: None,
            }
        }
        fn current(&self) -> (SelectedAgent, AgentReply) {
            let s = self.selected();
            let mut v: Value = serde_json::from_slice(STATUS).unwrap();
            v["result"]["installer"]["instance"] = json!({"id":1,"pid":4242,"uid":self.runner.uid,"exe":self.runner.exe,"runtime_dir":self.io.target().runtime_dir(),"started_unix_ms":0});
            let status =
                parse_status(&serde_json::to_vec(&v).unwrap(), AgentPlatform::Macos).unwrap();
            (
                s,
                AgentReply {
                    id: 1,
                    observed_at_ms: self.clock.now_ms(),
                    source: self.io.target().source(),
                    result: Ok(DecodedReply::Status(status)),
                },
            )
        }
        fn hints(&self, own: ResourceOwnership) {
            let p = MacPayload::admit(self.io.clone(), inventory(), &self.d()).unwrap();
            let mk = |id: &str, path: PathBuf| ResourceReceipt {
                resource_id: id.into(),
                resolved_path: path.to_string_lossy().into_owned(),
                ownership: own,
                before: if own == ResourceOwnership::Created {
                    ResourceObservation::Absent
                } else {
                    ResourceObservation::Different
                },
                after: ResourceObservation::Matching,
                outcome: MutationOutcome::Verified,
            };
            let receipt = InstallReceipt {
                schema_version: 1,
                operation_id: OperationId(1),
                product_version: "test-1".into(),
                manifest_sha256: p.manifest_sha256(),
                payload_sha256: producer_inventory().payload_digest(),
                resources: vec![
                    mk("mac.app", self.io.target().app_path()),
                    mk(
                        "mac.ctl",
                        self.io
                            .target()
                            .paths()
                            .home
                            .join(".local/bin/crosspanectl"),
                    ),
                ],
                unfinished: vec![],
            };
            self.put(
                &self.io.target().installer_dir().join("payload.json"),
                &serde_json::to_vec(&json!({"phase":"Verified","receipt":receipt})).unwrap(),
                0o600,
            );
            let plist = self
                .io
                .target()
                .paths()
                .home
                .join("Library/LaunchAgents/io.frostdev.crosspane.agent.plist");
            let mut launch = receipt;
            launch.payload_sha256 = sha(&render_plist(self.io.target()).unwrap());
            launch.resources = vec![mk("mac.launch-agent", plist)];
            launch.resources[0].outcome = MutationOutcome::Unknown;
            launch.unfinished = vec![StepId(12)];
            self.put(
                &self.io.target().installer_dir().join("launch-agent.json"),
                &serde_json::to_vec(&json!({"phase":"Observed","receipt":launch})).unwrap(),
                0o600,
            );
        }
        fn edit_launch(&self, f: impl FnOnce(&mut Value)) {
            let p = self.io.target().installer_dir().join("launch-agent.json");
            let mut v: Value =
                serde_json::from_slice(&self.io.read(&p, 512 * 1024, true, &self.d()).unwrap())
                    .unwrap();
            f(&mut v);
            self.put(&p, &serde_json::to_vec(&v).unwrap(), 0o600);
        }
        fn edit_payload(&self, f: impl FnOnce(&mut Value)) {
            let p = self.io.target().installer_dir().join("payload.json");
            let mut v: Value =
                serde_json::from_slice(&self.io.read(&p, 512 * 1024, true, &self.d()).unwrap())
                    .unwrap();
            f(&mut v);
            self.put(&p, &serde_json::to_vec(&v).unwrap(), 0o600);
        }
        fn no_current(&self, o: &MacRemovalObserver) -> RemovalObservation {
            o.observe(None, 1, OperationId(1), &self.d()).unwrap()
        }
    }

    fn assert_payload_retained(r: &Rig, o: &MacRemovalObserver) {
        let observed = r.no_current(o);
        for row in observed.inventory().resources.iter().filter(|row| {
            row.path.starts_with(r.io.target().app_path())
                || row.path == r.io.target().paths().home.join(".local/bin/crosspanectl")
        }) {
            assert!(
                !matches!(row.state, ResourceState::Owned | ResourceState::Adopted),
                "invalid complete payload envelope must retain {}",
                row.path.display()
            );
        }
        assert!(observed.tracked_original().is_none());
    }
    #[test]
    fn verify_genuine_sorted_producer_digest_from_unsorted_input_is_owned() {
        let original = inventory();
        assert!(original.files.windows(2).any(|p| p[0].path > p[1].path));
        assert_ne!(
            original.payload_digest(),
            producer_inventory().payload_digest()
        );
        for (ownership, state) in [
            (ResourceOwnership::Created, ResourceState::Owned),
            (ResourceOwnership::Adopted, ResourceState::Adopted),
        ] {
            let r = Rig::new();
            r.hints(ownership);
            let o = r.observer();
            let observed = r.no_current(&o);
            for id in ["Crosspane.app/Contents/MacOS/Crosspane", "crosspanectl"] {
                assert_eq!(r.row(&observed, id).state, state);
            }
        }
    }
    #[test]
    fn verify_unsorted_order_digest_is_retained() {
        let original = inventory();
        assert_ne!(
            original.payload_digest(),
            producer_inventory().payload_digest()
        );
        let r = Rig::new();
        let o = r.observer();
        r.edit_payload(|v| {
            v["receipt"]["payload_sha256"] = json!(original.payload_digest());
        });
        assert_payload_retained(&r, &o);
    }
    #[test]
    fn r1_payload_contradictory_before_semantics_are_retained() {
        for row in [0, 1] {
            for (ownership, before) in [
                (ResourceOwnership::Created, ResourceObservation::Different),
                (ResourceOwnership::Adopted, ResourceObservation::Absent),
            ] {
                let r = Rig::new();
                let o = r.observer();
                r.edit_payload(|v| {
                    v["receipt"]["resources"][row]["ownership"] = json!(ownership);
                    v["receipt"]["resources"][row]["before"] = json!(before);
                });
                assert_payload_retained(&r, &o);
            }
        }
    }
    #[test]
    fn r1_payload_wrong_digest_is_retained() {
        let r = Rig::new();
        let o = r.observer();
        r.edit_payload(|v| {
            let mut digest = producer_inventory().payload_digest();
            digest[0] ^= 1;
            v["receipt"]["payload_sha256"] = json!(digest);
        });
        assert_payload_retained(&r, &o);
    }
    #[test]
    fn r1_payload_companion_rows_are_exact_and_complete() {
        for invalid in 0..7 {
            let r = Rig::new();
            let o = r.observer();
            r.edit_payload(|v| {
                let rows = v["receipt"]["resources"].as_array_mut().unwrap();
                match invalid {
                    0 => rows[1]["resource_id"] = json!("unrelated"),
                    1 => rows[1]["resolved_path"] = json!("/foreign/crosspanectl"),
                    2 => rows[1] = rows[0].clone(),
                    3 => rows[1]["after"] = json!(ResourceObservation::Different),
                    4 => rows[1]["outcome"] = json!(MutationOutcome::Unknown),
                    5 => rows[1]["ownership"] = json!(ResourceOwnership::Foreign),
                    _ => rows.swap(0, 1),
                }
            });
            assert_payload_retained(&r, &o);
        }
    }
    #[test]
    fn r1_payload_missing_companion_is_retained() {
        for missing in [0, 1] {
            let r = Rig::new();
            let o = r.observer();
            r.edit_payload(|v| {
                v["receipt"]["resources"]
                    .as_array_mut()
                    .unwrap()
                    .remove(missing);
            });
            assert_payload_retained(&r, &o);
        }
    }
    #[test]
    fn r1_unrelated_disabled_duplicate_refuses_tracking() {
        for repeated in ["true", "false"] {
            let r = Rig::new();
            let o = r.observer();
            let map = format!(
                "disabled services = {{\n \"unrelated.job\" => true\n \"unrelated.job\" => {repeated}\n}}\n"
            );
            *r.runner.disabled.lock().unwrap() = Ok(out(0, &map, ""));
            assert_eq!(r.no_current(&o).inventory().disabled, None);
            assert!(
                o.observe(Some(r.current()), 1, OperationId(1), &r.d())
                    .is_err()
            );
        }
    }
    #[test]
    fn r1_nested_print_duplicate_keys_refuse_tracking() {
        for nested in [
            " detail = {\n nested = {\n key = first\n key = second\n }\n }\n",
            " detail = {\n nested = {\n key => first\n key => second\n }\n }\n",
            " runs = 1\n runs = 2\n",
            " detail = {\n nested = {\n }\n nested = {\n }\n }\n",
        ] {
            let r = Rig::new();
            let o = r.observer();
            let old = String::from_utf8(
                r.runner
                    .print
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .stdout
                    .clone(),
            )
            .unwrap();
            let mut text = old.strip_suffix("}\n").unwrap().to_owned();
            text.push_str(nested);
            text.push_str("}\n");
            *r.runner.print.lock().unwrap() = Ok(out(0, &text, ""));
            assert_eq!(r.no_current(&o).inventory().service, ServiceState::Unknown);
            assert!(
                o.observe(Some(r.current()), 1, OperationId(1), &r.d())
                    .is_err()
            );
        }
    }
    #[test]
    fn r1_print_same_key_in_distinct_scopes_is_admitted() {
        let r = Rig::new();
        let o = r.observer();
        let old = String::from_utf8(
            r.runner
                .print
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .stdout
                .clone(),
        )
        .unwrap();
        let mut text = old.strip_suffix("}\n").unwrap().to_owned();
        text.push_str(" one = {\n key = first\n }\n two = {\n key = second\n }\n}\n");
        *r.runner.print.lock().unwrap() = Ok(out(0, &text, ""));
        assert!(r.observe(&o).tracked_original().is_some());
    }

    #[test]
    fn a2_owned_inventory_and_original_are_correlated() {
        let r = Rig::new();
        let o = r.observer();
        let f = r.observe(&o);
        assert_eq!(r.row(&f, "mac.launch-agent").state, ResourceState::Owned);
        assert_eq!(r.row(&f, "crosspanectl").state, ResourceState::Owned);
        assert_eq!(f.inventory().service, ServiceState::Running(4242));
        assert_eq!(f.inventory().disabled, Some(false));
        assert!(f.tracked_original().is_some());
        assert_eq!(f.inventory().activity.as_ref().unwrap().instance, 1);
        f.check(&o, &r.d()).unwrap();
        assert!(
            r.runner
                .calls
                .lock()
                .unwrap()
                .iter()
                .all(|(p, a)| p != Path::new("/usr/bin/open")
                    && !a.iter().any(|s| s == "erase-identity"))
        );
    }
    #[test]
    fn a2_exact_app_directories_and_audio_leaves_are_listed() {
        let r = Rig::new();
        let f = r.no_current(&r.observer());
        let dirs: Vec<_> = f
            .inventory()
            .resources
            .iter()
            .filter(|r| r.id == "mac.app-directory")
            .collect();
        assert!(
            dirs.iter().any(
                |row| row.path == r.io.target().app_path() && row.state == ResourceState::Owned
            )
        );
        assert!(
            dirs.iter()
                .any(|row| row.path.ends_with("Contents/Resources/audio"))
        );
        assert_eq!(
            r.row(&f, "Crosspane.app/Contents/Resources/audio/packages.json")
                .state,
            ResourceState::Owned
        );
    }
    #[test]
    fn a2_retains_recovery_previous_installer_logs_and_legacy() {
        let r = Rig::new();
        let home = &r.io.target().paths().home;
        for (p,b) in [
 (home.join("Applications/.Crosspane.app.crosspane-previous/Contents/MacOS/Crosspane"),b"previous".as_slice()),
 (home.join(".local/bin/.crosspanectl.crosspane-previous"),b"previous"),
 (home.join(".cargo/bin/crosspanectl"),b"legacy"),
 (home.join("Library/Logs/Crosspane/agent.log"),b"unowned log"),
 (r.io.target().installer_dir().join("launch-agent-prior-7.plist"),b"prior"),
 (r.io.target().installer_dir().join("unknown.keep"),b"unknown"),
 (home.join("Library/LaunchAgents/.io.frostdev.crosspane.agent.plist.crosspane-temp-00000000000000000000000000000001"),b"temp"),
 ] { r.put(&p,b,0o600); }
        let f = r.no_current(&r.observer());
        for id in [
            "keep.installer",
            "keep.app-previous",
            "keep.ctl-previous",
            "keep.logs",
            "keep.legacy-ctl",
            "keep.installer-executable",
            "keep.agent-log",
        ] {
            assert_ne!(r.row(&f, id).state, ResourceState::Owned);
        }
        assert!(
            f.inventory()
                .resources
                .iter()
                .any(|x| x.path.ends_with("launch-agent-prior-7.plist"))
        );
        assert!(
            f.inventory()
                .resources
                .iter()
                .any(|x| x.path.ends_with("unknown.keep"))
        );
        assert_eq!(
            std::fs::read(home.join(".cargo/bin/crosspanectl")).unwrap(),
            b"legacy"
        );
    }
    #[test]
    fn a2_state_identity_and_journals_are_metadata_only() {
        let r = Rig::new();
        let state = r.io.target().state_dir();
        for name in [
            "device-key.pk8",
            "trust.json",
            "revocations.json",
            "input.journal",
            "projection-input.journal",
            "last_exit.json",
            "identity.lock",
        ] {
            r.put(&state.join(name), b"must not read", 0o600);
        }
        let state2 = state.clone();
        *r.hook.lock().unwrap() = Some(Arc::new(move |s, p| {
            if s == "open-before"
                && p.starts_with(&state2)
                && p.file_name().unwrap() != "payload.json"
                && p.file_name().unwrap() != "launch-agent.json"
            {
                return Err(NativeError::Foreign);
            }
            Ok(())
        }));
        let f = r.no_current(&r.observer());
        for name in [
            "device-key.pk8",
            "trust.json",
            "revocations.json",
            "input.journal",
            "projection-input.journal",
            "last_exit.json",
            "identity.lock",
        ] {
            let row = r.row(&f, &format!("keep.{name}"));
            assert_eq!(row.state, ResourceState::Unknown);
            assert!(row.sha256.is_none());
        }
    }
    #[test]
    fn a2_adopted_payload_and_plist_are_never_owned() {
        let r = Rig::new();
        r.hints(ResourceOwnership::Adopted);
        let f = r.no_current(&r.observer());
        for id in ["crosspanectl", "mac.launch-agent"] {
            assert_eq!(r.row(&f, id).state, ResourceState::Adopted);
        }
        assert!(f.tracked_original().is_none());
    }
    #[test]
    fn a2_absent_changed_and_unknown_receipt_states() {
        let r = Rig::new();
        r.scratch
            .remove(&r.io.target().paths().home.join(".local/bin/crosspanectl"));
        let path = r.io.target().app_path().join("Contents/Info.plist");
        r.put(&path, b"hand edit", 0o644);
        let f = r.no_current(&r.observer());
        assert_eq!(r.row(&f, "crosspanectl").state, ResourceState::Absent);
        assert_eq!(
            r.row(&f, "Crosspane.app/Contents/Info.plist").state,
            ResourceState::Changed
        );
        r.scratch
            .remove(&r.io.target().installer_dir().join("payload.json"));
        let f = r.no_current(&r.observer());
        assert_eq!(
            r.row(&f, "Crosspane.app/Contents/MacOS/Crosspane").state,
            ResourceState::Foreign
        );
    }
    #[test]
    fn a2_extra_app_file_directory_and_symlink_are_retained() {
        let r = Rig::new();
        let extra = r.io.target().app_path().join("Contents/unrelated/foreign");
        r.put(&extra, b"foreign", 0o600);
        let link = r.io.target().app_path().join("Contents/unknown-link");
        rustix::fs::symlinkat(
            "Info.plist",
            r.scratch.directory(link.parent().unwrap()),
            link.file_name().unwrap(),
        )
        .unwrap();
        let f = r.no_current(&r.observer());
        assert!(
            f.inventory()
                .resources
                .iter()
                .filter(|x| x.id == "keep.extra-app-resource")
                .count()
                >= 3
        );
        assert_eq!(std::fs::read(extra).unwrap(), b"foreign");
    }
    #[test]
    fn a2_plist_hand_edit_or_symlink_never_grants_tracking() {
        let r = Rig::new();
        let o = r.observer();
        let p =
            r.io.target()
                .paths()
                .home
                .join("Library/LaunchAgents/io.frostdev.crosspane.agent.plist");
        r.put(&p, b"hand edit", 0o644);
        assert_eq!(
            r.row(&r.no_current(&o), "mac.launch-agent").state,
            ResourceState::Changed
        );
        assert!(
            o.observe(Some(r.current()), 1, OperationId(1), &r.d())
                .is_err()
        );
        r.scratch.remove(&p);
        rustix::fs::symlinkat(
            "other.plist",
            r.scratch.directory(p.parent().unwrap()),
            p.file_name().unwrap(),
        )
        .unwrap();
        assert_eq!(
            r.row(&r.no_current(&o), "mac.launch-agent").state,
            ResourceState::Foreign
        );
    }
    #[test]
    fn a2_launch_hint_accepts_only_exact_observed_unfinished_envelope() {
        for mutator in [0, 1, 2, 3, 4, 5, 6, 7, 8, 9] {
            let r = Rig::new();
            r.edit_launch(|v| match mutator {
                0 => v["phase"] = json!("Published"),
                1 => v["receipt"]["unfinished"] = json!([]),
                2 => v["receipt"]["resources"][0]["outcome"] = json!("Verified"),
                3 => v["receipt"]["product_version"] = json!("other"),
                4 => v["receipt"]["manifest_sha256"] = json!(vec![0u8; 32]),
                5 => v["receipt"]["payload_sha256"] = json!(vec![0u8; 32]),
                6 => v["receipt"]["resources"][0]["resolved_path"] = json!("/foreign"),
                7 => v["receipt"]["resources"][0]["before"] = json!("Matching"),
                8 => v["receipt"]["operation_id"] = json!(0),
                _ => v["receipt"]["resources"][0]["after"] = json!("Unknown"),
            });
            let f = r.no_current(&r.observer());
            assert_eq!(
                r.row(&f, "mac.launch-agent").state,
                ResourceState::Foreign,
                "mutation {mutator}"
            );
            assert!(f.tracked_original().is_none());
        }
    }
    #[test]
    fn a2_launch_hint_duplicate_resource_and_wrong_schema_refuse() {
        let r = Rig::new();
        r.edit_launch(|v| {
            let extra = v["receipt"]["resources"][0].clone();
            v["receipt"]["resources"]
                .as_array_mut()
                .unwrap()
                .push(extra);
        });
        assert_eq!(
            r.row(&r.no_current(&r.observer()), "mac.launch-agent")
                .state,
            ResourceState::Foreign
        );
        let r = Rig::new();
        r.edit_launch(|v| v["receipt"]["schema_version"] = json!(2));
        assert_eq!(
            r.row(&r.no_current(&r.observer()), "mac.launch-agent")
                .state,
            ResourceState::Foreign
        );
    }
    #[test]
    fn a2_current_service_pid_mismatch_refuses_original() {
        let r = Rig::new();
        let o = r.observer();
        let text = String::from_utf8(
            r.runner
                .print
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .stdout
                .clone(),
        )
        .unwrap()
        .replace("pid = 4242", "pid = 4243");
        *r.runner.print.lock().unwrap() = Ok(out(0, &text, ""));
        assert_eq!(
            o.observe(Some(r.current()), 1, OperationId(1), &r.d())
                .unwrap_err(),
            NativeError::Foreign
        );
    }
    #[test]
    fn a2_failed_service_queries_never_expose_tracking() {
        for bad in [
            out(1, "", "failure"),
            out(0, "garbage", ""),
            out(0, "", "diagnostic"),
        ] {
            let r = Rig::new();
            let o = r.observer();
            *r.runner.print.lock().unwrap() = Ok(bad);
            let f = r.no_current(&o);
            assert_eq!(f.inventory().service, ServiceState::Unknown);
            assert!(f.tracked_original().is_none());
            assert!(
                o.observe(Some(r.current()), 1, OperationId(1), &r.d())
                    .is_err()
            );
        }
        let r = Rig::new();
        let o = r.observer();
        *r.runner.print.lock().unwrap() = Err(NativeError::OutcomeUnknown);
        assert_eq!(
            o.observe(Some(r.current()), 1, OperationId(1), &r.d())
                .unwrap_err(),
            NativeError::OutcomeUnknown
        );
    }
    #[test]
    fn a2_exact_service_absence_is_not_clean_authority() {
        let r = Rig::new();
        let o = r.observer();
        *r.runner.print.lock().unwrap() = Ok(out(
            113,
            "",
            &format!(
                "Could not find service \"{AGENT_LABEL}\" in domain for user gui: {}\n",
                r.runner.uid
            ),
        ));
        let f = r.no_current(&o);
        assert_eq!(f.inventory().service, ServiceState::Absent);
        assert!(f.tracked_original().is_none());
        *r.runner.print.lock().unwrap() = Ok(out(113, "", "not found"));
        assert_eq!(r.no_current(&o).inventory().service, ServiceState::Unknown);
    }
    #[test]
    fn a2_nested_print_duplicate_depth_path_and_program_refuse() {
        for mutation in [0, 1, 2, 3, 4] {
            let r = Rig::new();
            let o = r.observer();
            let old = String::from_utf8(
                r.runner
                    .print
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .stdout
                    .clone(),
            )
            .unwrap();
            let text = match mutation {
                0 => old.replace(" pid = 4242", " pid = 4242\n pid = 4242"),
                1 => old.replace("path = ", "path = /foreign"),
                2 => old.replace("program = ", "program = /foreign"),
                3 => format!(
                    "gui/{}/{AGENT_LABEL} = {{\n{}\n{}\n}}\n",
                    r.runner.uid,
                    " nested = {\n".repeat(33),
                    "}\n".repeat(33)
                ),
                _ => old.replace("arguments = {", "arguments = {\n nested = {"),
            };
            *r.runner.print.lock().unwrap() = Ok(out(0, &text, ""));
            assert_eq!(
                r.no_current(&o).inventory().service,
                ServiceState::Unknown,
                "mutation {mutation}"
            );
        }
    }
    #[test]
    fn a2_disabled_map_unknowns_and_true_are_explicit() {
        let r = Rig::new();
        let o = r.observer();
        for (text, expect) in [
            (
                "disabled services = {\n \"io.frostdev.crosspane.agent\" => true\n}\n",
                Some(true),
            ),
            ("disabled services = {\n}\n", Some(false)),
            (
                "disabled services = {\n \"io.frostdev.crosspane.agent\" => true\n \"io.frostdev.crosspane.agent\" => false\n}\n",
                None,
            ),
            (
                "disabled services = {\n \"io.frostdev.crosspane.agent\" => maybe\n}\n",
                None,
            ),
        ] {
            *r.runner.disabled.lock().unwrap() = Ok(out(0, text, ""));
            assert_eq!(r.no_current(&o).inventory().disabled, expect);
        }
    }
    #[test]
    fn a2_parser_output_limits_fail_closed() {
        let r = Rig::new();
        let o = r.observer();
        *r.runner.print.lock().unwrap() = Ok(out(0, &" ".repeat(65537), ""));
        *r.runner.disabled.lock().unwrap() = Ok(out(0, &" ".repeat(65537), ""));
        assert_eq!(
            o.observe(None, 1, OperationId(1), &r.d()).unwrap_err(),
            NativeError::Oversize
        );
    }
    #[test]
    fn a2_foreign_target_source_instance_and_stale_status_refuse() {
        let r = Rig::new();
        let o = r.observer();
        let (s, mut reply) = r.current();
        reply.observed_at_ms = 1;
        assert_eq!(
            o.observe(Some((s, reply)), 1, OperationId(1), &r.d())
                .unwrap_err(),
            NativeError::Foreign
        );
        let (s, mut reply) = r.current();
        reply.id = 0;
        assert_eq!(
            o.observe(Some((s, reply)), 1, OperationId(1), &r.d())
                .unwrap_err(),
            NativeError::Foreign
        );
        let other = Rig::new();
        assert_eq!(
            o.observe(Some(other.current()), 1, OperationId(1), &r.d())
                .unwrap_err(),
            NativeError::Foreign
        );
        let current = r.current();
        r.clock.0.store(5000, Ordering::Release);
        assert_eq!(
            o.observe(Some(current), 1, OperationId(1), &r.d())
                .unwrap_err(),
            NativeError::Foreign
        );
    }
    #[test]
    fn a2_fresh_support_renewal_and_changed_session_refusal() {
        let r = Rig::new();
        let o = r.observer();
        r.clock.0.store(6000, Ordering::Release);
        r.observe(&o);
        r.support.facts.lock().unwrap().gui.console_session = "other".into();
        assert_eq!(
            o.observe(None, 1, OperationId(1), &r.d()).unwrap_err(),
            NativeError::Unsupported
        );
    }
    #[test]
    fn a2_changed_installed_signature_refuses() {
        let r = Rig::new();
        let o = r.observer();
        let current = r.current();
        r.signatures.bad.store(true, Ordering::Release);
        assert!(o.observe(Some(current), 1, OperationId(1), &r.d()).is_err());
    }
    #[test]
    fn a2_activity_expiry_is_checked_after_io() {
        let r = Rig::new();
        let o = r.observer();
        let current = r.current();
        r.clock.0.store(4900, Ordering::Release);
        let c = r.clock.clone();
        let once = Arc::new(AtomicBool::new(false));
        *r.runner.hook.lock().unwrap() = Some(Arc::new(move |_, p| {
            if p == Path::new("/bin/launchctl") && !once.swap(true, Ordering::AcqRel) {
                c.0.store(5101, Ordering::Release);
            }
            Ok(())
        }));
        assert_eq!(
            o.observe(Some(current), 1, OperationId(1), &r.d())
                .unwrap_err(),
            NativeError::Refused
        );
    }
    #[test]
    fn a2_observation_freshness_and_cross_observer_binding() {
        let r = Rig::new();
        let o = r.observer();
        let f = r.observe(&o);
        let other = r.observer();
        assert_eq!(f.check(&other, &r.d()).unwrap_err(), NativeError::Foreign);
        r.clock.0.store(5001, Ordering::Release);
        assert_eq!(f.check(&o, &r.d()).unwrap_err(), NativeError::Refused);
    }
    #[test]
    fn a2_package_identity_change_is_detected_without_admin_open() {
        let r = Rig::new();
        let o = r.observer();
        let f = r.no_current(&o);
        assert_eq!(f.inventory().package.version, "0.1.0");
        assert_eq!(f.inventory().package.removal_sha256, sha(b"inert-remove"));
        r.put(
            &r.audio().join("CrosspaneAudio-remove-0.1.0.pkg"),
            b"changed",
            0o644,
        );
        assert!(o.observe(None, 1, OperationId(1), &r.d()).is_err());
    }
    #[test]
    fn a2_foreign_audio_source_is_refused_before_any_query() {
        let r = Rig::new();
        r.runner.calls.lock().unwrap().clear();
        assert_eq!(
            MacRemovalObserver::admit(
                r.io.clone(),
                inventory(),
                r.io.target().paths().home.join("other"),
                r.clock.clone(),
                &r.d()
            )
            .unwrap_err(),
            NativeError::Foreign
        );
        assert!(r.runner.calls.lock().unwrap().is_empty());
    }
    #[test]
    fn a2_symlink_expected_leaf_never_reads_destination() {
        let r = Rig::new();
        let p = r.io.target().paths().home.join(".local/bin/crosspanectl");
        r.scratch.remove(&p);
        rustix::fs::symlinkat(
            "elsewhere",
            r.scratch.directory(p.parent().unwrap()),
            p.file_name().unwrap(),
        )
        .unwrap();
        let f = r.no_current(&r.observer());
        assert_eq!(r.row(&f, "crosspanectl").state, ResourceState::Foreign);
    }
    #[test]
    fn a2_directory_replacement_during_listing_refuses() {
        let r = Rig::new();
        let o = r.observer();
        let app = r.io.target().app_path();
        let touched = Arc::new(AtomicBool::new(false));
        let changed = app.clone();
        let once = touched.clone();
        *r.hook.lock().unwrap() = Some(Arc::new(move |stage, p| {
            if stage == "metadata" && p == changed && !once.swap(true, Ordering::AcqRel) {
                return Err(NativeError::Foreign);
            }
            Ok(())
        }));
        assert_eq!(
            o.observe(None, 1, OperationId(1), &r.d()).unwrap_err(),
            NativeError::Foreign
        );
        assert!(touched.load(Ordering::Acquire));
    }
    #[test]
    fn a2_traversal_depth_and_entry_caps_refuse() {
        let r = Rig::new();
        let o = r.observer();
        let mut p = r.io.target().app_path().join("extra");
        for _ in 0..34 {
            p = p.join("d");
        }
        r.scratch.directory(&p);
        assert_eq!(
            o.observe(None, 1, OperationId(1), &r.d()).unwrap_err(),
            NativeError::Oversize
        );
        let r = Rig::new();
        let o = r.observer();
        for i in 0..4097 {
            r.put(
                &r.io.target().app_path().join(format!("extra-{i}")),
                b"x",
                0o600,
            );
        }
        assert_eq!(
            o.observe(None, 1, OperationId(1), &r.d()).unwrap_err(),
            NativeError::Oversize
        );
    }
    #[test]
    fn a2_cancellation_and_deadline_bound_noncooperative_reads() {
        for cancel in [false, true] {
            let r = Rig::new();
            let o = Arc::new(r.observer());
            let entered = Arc::new((Mutex::new(false), Condvar::new()));
            let release = Arc::new((Mutex::new(false), Condvar::new()));
            let a = entered.clone();
            let b = release.clone();
            let once = Arc::new(AtomicBool::new(false));
            *r.runner.hook.lock().unwrap() = Some(Arc::new(move |_, p| {
                if p == Path::new("/bin/launchctl") && !once.swap(true, Ordering::AcqRel) {
                    let (l, c) = &*a;
                    *l.lock().unwrap() = true;
                    c.notify_all();
                    let (l, c) = &*b;
                    let v = l.lock().unwrap();
                    let v = c
                        .wait_timeout_while(v, Duration::from_secs(3), |v| !*v)
                        .unwrap()
                        .0;
                    assert!(*v, "owned fake reader release must arrive");
                }
                Ok(())
            }));
            let token = Cancellation::default();
            let deadline = Deadline::new(5000, r.clock.clone(), token.clone()).unwrap();
            let worker = o.clone();
            let start = Instant::now();
            let j = std::thread::spawn(move || worker.observe(None, 1, OperationId(1), &deadline));
            let (l, c) = &*entered;
            let mut v = l.lock().unwrap();
            v = c
                .wait_timeout_while(v, Duration::from_secs(2), |v| !*v)
                .unwrap()
                .0;
            assert!(*v, "fake reader must be entered before expiry/cancellation");
            drop(v);
            if cancel {
                token.cancel();
            } else {
                r.clock.0.store(5001, Ordering::Release);
            }
            assert_eq!(
                j.join().unwrap().unwrap_err(),
                if cancel {
                    NativeError::Cancelled
                } else {
                    NativeError::Timeout
                }
            );
            assert!(start.elapsed() < Duration::from_secs(2));
            let (l, c) = &*release;
            *l.lock().unwrap() = true;
            c.notify_all();
        }
    }
    #[test]
    fn a2_debug_opaque_observation_never_prints_private_state() {
        let r = Rig::new();
        let o = r.observer();
        let f = r.observe(&o);
        assert_eq!(format!("{o:?}"), "MacRemovalObserver");
        assert_eq!(format!("{f:?}"), "RemovalObservation");
    }
    #[test]
    fn a2_real_scratch_directory_change_between_observations_refuses() {
        let r = Rig::new();
        let o = r.observer();
        let app = r.io.target().app_path();
        let scratch = r.scratch.clone();
        let counter = Arc::new(AtomicU64::new(0));
        let hits = counter.clone();
        *r.hook.lock().unwrap() = Some(Arc::new(move |stage, p| {
            if stage == "metadata" && p == app && hits.fetch_add(1, Ordering::AcqRel) == 3 {
                scratch.put(&app.join("created-during-observation"), b"foreign", 0o600);
            }
            Ok(())
        }));
        assert_eq!(
            o.observe(None, 1, OperationId(1), &r.d()).unwrap_err(),
            NativeError::Foreign
        );
        assert!(counter.load(Ordering::Acquire) >= 4);
    }
    #[test]
    fn a2_noncooperative_workers_keep_four_admission_slots_after_timeout() {
        struct Release(Arc<(Mutex<bool>, Condvar)>);
        impl Drop for Release {
            fn drop(&mut self) {
                let (l, c) = &*self.0;
                *l.lock().unwrap() = true;
                c.notify_all();
            }
        }
        let r = Rig::new();
        let o = Arc::new(r.observer());
        let entered = Arc::new((Mutex::new(0usize), Condvar::new()));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let guard = Release(release.clone());
        let a = entered.clone();
        let b = release.clone();
        *r.hook.lock().unwrap() = Some(Arc::new(move |stage, _| {
            if stage == "metadata" {
                let (l, c) = &*a;
                *l.lock().unwrap() += 1;
                c.notify_all();
                let (l, c) = &*b;
                let v = l.lock().unwrap();
                let v = c
                    .wait_timeout_while(v, Duration::from_secs(3), |v| !*v)
                    .unwrap()
                    .0;
                assert!(*v, "fixture must release actual worker");
            }
            Ok(())
        }));
        let mut joins = Vec::new();
        for _ in 0..4 {
            let o = o.clone();
            let d = r.d();
            joins.push(std::thread::spawn(move || {
                o.observe(None, 1, OperationId(1), &d)
            }));
        }
        let (l, c) = &*entered;
        let v = l.lock().unwrap();
        let v = c
            .wait_timeout_while(v, Duration::from_secs(2), |v| *v < 4)
            .unwrap()
            .0;
        assert_eq!(*v, 4);
        drop(v);
        r.clock.0.store(5001, Ordering::Release);
        for j in joins {
            assert_eq!(j.join().unwrap().unwrap_err(), NativeError::Timeout);
        }
        assert_eq!(
            o.observe(None, 1, OperationId(1), &r.d()).unwrap_err(),
            NativeError::Busy
        );
        drop(guard);
    }
    #[test]
    fn a2_disabled_query_failure_never_exposes_original() {
        let r = Rig::new();
        let o = r.observer();
        *r.runner.disabled.lock().unwrap() = Ok(out(0, "malformed", ""));
        assert!(
            o.observe(Some(r.current()), 1, OperationId(1), &r.d())
                .is_err()
        );
    }
    #[test]
    fn a2_revoked_selected_support_never_exposes_original() {
        let r = Rig::new();
        let o = r.observer();
        let current = r.current();
        current.0.support.revoke();
        assert!(o.observe(Some(current), 1, OperationId(1), &r.d()).is_err());
    }
    #[test]
    fn a2_active_input_projection_audio_and_epochs_are_typed() {
        let r = Rig::new();
        let o = r.observer();
        let selected = r.selected();
        let mut value: Value = serde_json::from_slice(STATUS).unwrap();
        let peer = "22".repeat(32);
        value["result"]["controlling"] = json!(peer);
        value["result"]["projections"] = json!([{"source":"2222222222222222","projection":7,"text":"synthetic test projection","received":null}]);
        let mut counters = serde_json::Map::new();
        for key in [
            "e1_controller_started",
            "e1_controller_ended",
            "e1_target_started",
            "e1_target_ended",
            "e1_injections_ok",
            "e1_hud_shows",
            "e1_chord_releases",
            "e1_command_releases",
            "e2_source_started",
            "e2_source_returned",
            "e2_dest_started",
            "e2_dest_returned",
            "e2_returns_failed",
        ] {
            counters.insert(key.into(), json!(0));
        }
        counters.insert("e2_frames_presented".into(), Value::Null);
        value["result"]["installer"]["peers"] = json!([{"node":peer,"name":"fixture peer","connected":true,"link_generation":1,"features":["video"],"grants_given":[],"last_source_parking":null,"counters":counters}]);
        value["result"]["installer"]["audio"]["active_peers"] = json!([peer]);
        value["result"]["installer"]["epochs"]["gate"] = json!(9);
        value["result"]["installer"]["instance"] = json!({"id":1,"pid":4242,"uid":r.runner.uid,"exe":r.runner.exe,"runtime_dir":r.io.target().runtime_dir(),"started_unix_ms":0});
        let reply = AgentReply {
            id: 2,
            observed_at_ms: 0,
            source: r.io.target().source(),
            result: Ok(DecodedReply::Status(
                parse_status(&serde_json::to_vec(&value).unwrap(), AgentPlatform::Macos).unwrap(),
            )),
        };
        let f = o
            .observe(Some((selected, reply)), 1, OperationId(1), &r.d())
            .unwrap();
        let activity = f.inventory().activity.as_ref().unwrap();
        assert!(activity.input && activity.audio);
        assert_eq!(activity.projections, 1);
        assert_eq!(activity.epochs, [9, 1, 1, 1]);
    }

    mod a2b_tests {
        use super::*;

        #[derive(Default)]
        struct ProbeCounts {
            filesystem: AtomicU64,
            support: AtomicU64,
            signature: AtomicU64,
        }
        struct CountSupport(Arc<Support>, Arc<ProbeCounts>);
        impl SupportProbe for CountSupport {
            fn observe(&self, d: &Deadline) -> NativeResult<SupportObservation> {
                self.1.support.fetch_add(1, Ordering::AcqRel);
                self.0.observe(d)
            }
        }
        struct CountSignature(Arc<Signatures>, Arc<ProbeCounts>);
        impl SignatureProbe for CountSignature {
            fn observe(
                &self,
                p: &Path,
                s: &SigningRequirement,
                d: &Deadline,
            ) -> NativeResult<SignatureObservation> {
                self.1.signature.fetch_add(1, Ordering::AcqRel);
                self.0.observe(p, s, d)
            }
        }
        fn counted_rig() -> (Rig, Arc<ProbeCounts>) {
            let mut r = Rig::new();
            let counts = Arc::new(ProbeCounts::default());
            let mut target = r.io.target().clone();
            let previous = target.test_hook.take().unwrap();
            let events = counts.clone();
            target.test_hook = Some(Arc::new(move |stage, path, identity| {
                events.filesystem.fetch_add(1, Ordering::AcqRel);
                previous(stage, path, identity)
            }));
            r.io = Arc::new(
                MacNativeIo::new(
                    target,
                    r.runner.clone(),
                    Arc::new(CountSupport(r.support.clone(), counts.clone())),
                    Arc::new(CountSignature(r.signatures.clone(), counts.clone())),
                    r.clock.clone(),
                )
                .unwrap(),
            );
            (r, counts)
        }
        fn io_counts(r: &Rig, c: &ProbeCounts) -> (usize, u64, u64, u64) {
            (
                r.runner.calls.lock().unwrap().len(),
                c.filesystem.load(Ordering::Acquire),
                c.support.load(Ordering::Acquire),
                c.signature.load(Ordering::Acquire),
            )
        }

        fn choices(delete_identity: bool, remove_driver: bool) -> RemovalChoices {
            RemovalChoices {
                delete_identity,
                remove_driver,
            }
        }
        fn plan(r: &Rig, m: &mut MacRemoval, revision: u64, c: RemovalChoices) -> RemovalPlan {
            m.plan(
                revision,
                OperationId(revision),
                c,
                Some(current_reply(r, revision, |_| {})),
                &r.d(),
            )
            .unwrap()
        }
        fn consent(
            r: &Rig,
            m: &mut MacRemoval,
            p: &RemovalPlan,
            c: RemovalChoices,
        ) -> RemovalConsent {
            m.consent(
                p,
                p.preview().revision,
                p.preview().operation,
                c,
                true,
                &r.d(),
            )
            .unwrap()
        }
        fn effect<'a>(p: &'a RemovalPlan, id: &str) -> &'a RemovalDelta {
            p.preview()
                .deltas
                .iter()
                .find(|d| d.resource == id)
                .unwrap()
        }
        fn assert_read_only(r: &Rig) {
            assert!(r.runner.calls.lock().unwrap().iter().all(|c| {
                !c.1.iter()
                    .any(|a| matches!(a.as_str(), "disable" | "bootout" | "erase-identity"))
            }));
        }
        fn current_reply(
            r: &Rig,
            id: u64,
            edit: impl FnOnce(&mut Value),
        ) -> (SelectedAgent, AgentReply) {
            let selected = r.selected();
            let mut v: Value = serde_json::from_slice(STATUS).unwrap();
            v["result"]["installer"]["instance"] = json!({"id":1,"pid":4242,"uid":r.runner.uid,
                "exe":r.runner.exe,"runtime_dir":r.io.target().runtime_dir(),"started_unix_ms":0});
            edit(&mut v);
            let reply = AgentReply {
                id,
                observed_at_ms: r.clock.now_ms(),
                source: r.io.target().source(),
                result: Ok(DecodedReply::Status(
                    parse_status(&serde_json::to_vec(&v).unwrap(), AgentPlatform::Macos).unwrap(),
                )),
            };
            (selected, reply)
        }

        #[test]
        fn a2b_constructor_consumes_observer_without_second_acquisition() {
            let r = Rig::new();
            let observer = r.observer();
            let before = r.runner.calls.lock().unwrap().clone();
            let _m = MacRemoval::new(observer);
            assert_eq!(*r.runner.calls.lock().unwrap(), before);
        }
        #[test]
        fn a2b_defaults_preview_exact_conditional_effects_and_paths() {
            let r = Rig::new();
            let mut m = MacRemoval::new(r.observer());
            let c = RemovalChoices::default();
            assert_eq!(c, choices(false, true));
            let p = plan(&r, &mut m, 1, c);
            assert_eq!(
                effect(&p, "mac.identity-pairings").effect,
                RemovalEffect::KeepIdentity
            );
            assert_eq!(
                effect(&p, "mac.agent").effect,
                RemovalEffect::StopTrackedAgent
            );
            assert_eq!(
                effect(&p, "mac.autostart").effect,
                RemovalEffect::DisableOwnedAutostart
            );
            assert_eq!(
                effect(&p, "mac.agent").path.as_ref().unwrap(),
                &r.io.target().agent_path()
            );
            assert_eq!(
                effect(&p, "mac.shared-audio").path.as_ref().unwrap(),
                Path::new("/Library/Audio/Plug-Ins/HAL/CrosspaneAudio.driver")
            );
            assert_eq!(
                effect(&p, "mac.shared-audio-previous")
                    .path
                    .as_ref()
                    .unwrap(),
                Path::new("/Library/Application Support/Crosspane/Installer/previous")
            );
            assert_eq!(
                effect(&p, "mac.shared-audio").effect,
                RemovalEffect::RemoveSharedDriverAfterPackageVerification
            );
            assert_eq!(
                effect(&p, "mac.shared-audio-previous").effect,
                RemovalEffect::RemovePreviousAfterPackageVerification
            );
            assert_eq!(
                p.preview().driver_label,
                "Remove the Crosspane audio driver (affects every user on this Mac)"
            );
            assert!(p.preview().identity_explanation.contains("re-pairing"));
            assert!(
                p.preview()
                    .identity_explanation
                    .contains("not remote revocation")
            );
            assert!(p.tracked_original().is_some());
            assert_read_only(&r);
        }
        #[test]
        fn a2b_explicit_delete_is_only_a_clean_exit_condition() {
            let r = Rig::new();
            let mut m = MacRemoval::new(r.observer());
            let p = plan(&r, &mut m, 1, choices(true, false));
            assert_eq!(
                effect(&p, "mac.identity-pairings").effect,
                RemovalEffect::EraseOnlyAfterCleanExit
            );
            assert_eq!(
                effect(&p, "mac.identity-pairings").path.as_ref().unwrap(),
                &r.io.target().state_dir()
            );
            assert_eq!(
                effect(&p, "mac.shared-audio").effect,
                RemovalEffect::KeepRecovery
            );
            assert_eq!(
                effect(&p, "mac.shared-audio-previous").effect,
                RemovalEffect::KeepRecovery
            );
            assert_read_only(&r);
        }
        #[test]
        fn a2b_already_stopped_uncorrelated_preserves_identity_and_owned_tools() {
            let r = Rig::new();
            *r.runner.print.lock().unwrap() = Ok(out(113, "", ""));
            let mut m = MacRemoval::new(r.observer());
            let p = m
                .plan(1, OperationId(1), choices(true, false), None, &r.d())
                .unwrap();
            assert!(p.tracked_original().is_none());
            assert_eq!(
                effect(&p, "mac.identity-pairings").effect,
                RemovalEffect::KeepIdentity
            );
            assert_eq!(
                effect(&p, "Crosspane.app/Contents/MacOS/Crosspane").effect,
                RemovalEffect::KeepRecovery
            );
            assert_eq!(
                effect(&p, "keep.revocations.json").effect,
                RemovalEffect::Absent
            );
            assert_read_only(&r);
        }
        #[test]
        fn a2b_owned_files_are_conditional_and_directories_are_empty_only() {
            let r = Rig::new();
            let mut m = MacRemoval::new(r.observer());
            let p = plan(&r, &mut m, 1, RemovalChoices::default());
            for row in p
                .inventory()
                .resources
                .iter()
                .filter(|row| row.state == ResourceState::Owned)
            {
                let d = p
                    .preview()
                    .deltas
                    .iter()
                    .find(|d| d.resource == row.id && d.path.as_ref() == Some(&row.path))
                    .unwrap();
                assert_eq!(
                    d.effect,
                    if row.id == "mac.app-directory" {
                        RemovalEffect::PruneEmptyOwnedAfterVerification
                    } else {
                        RemovalEffect::RemoveOwnedAfterVerification
                    }
                );
            }
            assert_read_only(&r);
        }
        #[test]
        fn a2b_adopted_changed_foreign_and_legacy_material_are_kept() {
            let r = Rig::new();
            *r.runner.print.lock().unwrap() = Ok(out(113, "", ""));
            r.hints(ResourceOwnership::Adopted);
            let foreign = r.io.target().app_path().join("unrelated.txt");
            r.put(&foreign, b"fixture foreign", 0o600);
            let legacy = r.io.target().paths().home.join(".cargo/bin/crosspanectl");
            r.put(&legacy, b"inert legacy", 0o755);
            let mut m = MacRemoval::new(r.observer());
            let p = m
                .plan(1, OperationId(1), RemovalChoices::default(), None, &r.d())
                .unwrap();
            assert_eq!(
                effect(&p, "crosspanectl").effect,
                RemovalEffect::KeepForeign
            );
            assert_eq!(
                effect(&p, "keep.extra-app-resource").effect,
                RemovalEffect::KeepForeign
            );
            assert_eq!(
                effect(&p, "keep.legacy-ctl").effect,
                RemovalEffect::KeepRecovery
            );
            assert_eq!(std::fs::read(foreign).unwrap(), b"fixture foreign");
            assert_eq!(std::fs::read(legacy).unwrap(), b"inert legacy");
            assert_read_only(&r);
        }
        #[test]
        fn a2b_recovery_previous_installer_logs_and_identity_rows_stay_present() {
            let r = Rig::new();
            for name in [
                "trust.json",
                "revocations.json",
                "device-key.pk8",
                "config.toml",
                "input.journal",
                "last_exit.json",
            ] {
                r.put(
                    &r.io.target().state_dir().join(name),
                    b"inert metadata",
                    0o600,
                );
            }
            for relative in [
                "Applications/.Crosspane.app.crosspane-previous/inert",
                ".local/bin/.crosspanectl.crosspane-previous",
                "Library/Logs/Crosspane/agent.log",
            ] {
                r.put(
                    &r.io.target().paths().home.join(relative),
                    b"inert recovery",
                    0o600,
                );
            }
            let mut m = MacRemoval::new(r.observer());
            let p = plan(&r, &mut m, 1, RemovalChoices::default());
            for id in [
                "keep.installer",
                "keep.installer-executable",
                "keep.app-previous",
                "keep.ctl-previous",
                "keep.logs",
                "keep.agent-log",
                "keep.config.toml",
                "keep.input.journal",
                "keep.last_exit.json",
            ] {
                assert_eq!(effect(&p, id).effect, RemovalEffect::KeepRecovery, "{id}");
            }
            for id in [
                "keep.trust.json",
                "keep.revocations.json",
                "keep.device-key.pk8",
            ] {
                assert_eq!(effect(&p, id).effect, RemovalEffect::KeepIdentity);
            }
            assert_read_only(&r);
        }
        #[test]
        fn a2b_consent_requires_exact_view_operation_choices_and_interruption() {
            let (r, counts) = counted_rig();
            let mut m = MacRemoval::new(r.observer());
            let c = RemovalChoices::default();
            let p = plan(&r, &mut m, 1, c);
            let before = io_counts(&r, &counts);
            assert!(before.0 > 0 && before.1 > 0 && before.2 > 0 && before.3 > 0);
            for (rev, op, choices, interruption) in [
                (2, 1, c, true),
                (1, 2, c, true),
                (1, 1, choices(true, true), true),
                (1, 1, choices(false, false), true),
                (1, 1, c, false),
            ] {
                assert_eq!(
                    m.consent(&p, rev, OperationId(op), choices, interruption, &r.d())
                        .unwrap_err(),
                    NativeError::Refused
                );
            }
            assert_eq!(io_counts(&r, &counts), before);
            let _yes = consent(&r, &mut m, &p, c);
            assert_read_only(&r);
        }
        #[test]
        fn a2b_cached_a_never_revives_after_b_or_retirement() {
            let (r, counts) = counted_rig();
            let mut m = MacRemoval::new(r.observer());
            let c = RemovalChoices::default();
            let a = plan(&r, &mut m, 1, c);
            let yes_a = consent(&r, &mut m, &a, c);
            let b = plan(&r, &mut m, 2, c);
            let yes_b = consent(&r, &mut m, &b, c);
            let stale_current = current_reply(&r, 2, |_| {});
            let b_current = current_reply(&r, 2, |_| {});
            let before = io_counts(&r, &counts);
            assert_eq!(
                m.consent(&a, 1, OperationId(1), c, true, &r.d())
                    .unwrap_err(),
                NativeError::Refused
            );
            assert_eq!(
                m.revalidate(&a, &yes_a, Some(stale_current), &r.d())
                    .unwrap_err(),
                NativeError::Refused
            );
            assert_eq!(
                m.revalidate(&b, &yes_b, Some(b_current), &r.d())
                    .unwrap_err(),
                NativeError::Refused
            );
            assert_eq!(io_counts(&r, &counts), before);
            let c_plan = plan(&r, &mut m, 3, c);
            let _yes_c = consent(&r, &mut m, &c_plan, c);
            let before = io_counts(&r, &counts);
            m.retire();
            assert!(
                m.consent(&c_plan, 3, OperationId(3), c, true, &r.d())
                    .is_err()
            );
            assert_eq!(io_counts(&r, &counts), before);
            assert_read_only(&r);
        }
        #[test]
        fn a2b_failed_new_observation_retires_a_and_consumes_identifiers() {
            let r = Rig::new();
            let mut m = MacRemoval::new(r.observer());
            let c = RemovalChoices::default();
            let a = plan(&r, &mut m, 1, c);
            let yes = consent(&r, &mut m, &a, c);
            let current = current_reply(&r, 2, |_| {});
            r.signatures.bad.store(true, Ordering::Release);
            assert!(m.plan(2, OperationId(2), c, Some(current), &r.d()).is_err());
            r.signatures.bad.store(false, Ordering::Release);
            assert_eq!(
                m.plan(
                    2,
                    OperationId(2),
                    c,
                    Some(current_reply(&r, 2, |_| {})),
                    &r.d()
                )
                .unwrap_err(),
                NativeError::Invalid
            );
            assert!(
                m.revalidate(&a, &yes, Some(current_reply(&r, 2, |_| {})), &r.d())
                    .is_err()
            );
            let _fresh = plan(&r, &mut m, 3, c);
        }
        #[test]
        fn a2b_nonmonotonic_revision_or_operation_is_refused() {
            let r = Rig::new();
            let mut m = MacRemoval::new(r.observer());
            let c = RemovalChoices::default();
            let _a = plan(&r, &mut m, 1, c);
            for (rev, op) in [(1, 2), (2, 1), (0, 2), (2, 0)] {
                assert_eq!(
                    m.plan(
                        rev,
                        OperationId(op),
                        c,
                        Some(current_reply(&r, 2, |_| {})),
                        &r.d()
                    )
                    .unwrap_err(),
                    NativeError::Invalid
                );
            }
        }
        #[test]
        fn a2b_foreign_controller_and_consent_cannot_authorize_plan() {
            let (r, counts) = counted_rig();
            let mut m = MacRemoval::new(r.observer());
            let mut other = MacRemoval::new(r.observer());
            let c = RemovalChoices::default();
            let a = plan(&r, &mut m, 1, c);
            let b = plan(&r, &mut other, 1, c);
            let yes_b = consent(&r, &mut other, &b, c);
            let current = current_reply(&r, 2, |_| {});
            let before = io_counts(&r, &counts);
            assert_eq!(
                m.revalidate(&a, &yes_b, Some(current), &r.d()).unwrap_err(),
                NativeError::Refused
            );
            assert_eq!(
                other
                    .consent(&a, 1, OperationId(1), c, true, &r.d())
                    .unwrap_err(),
                NativeError::Refused
            );
            assert_eq!(io_counts(&r, &counts), before);
            assert_read_only(&r);
        }
        #[test]
        fn a2b_fresh_receipts_with_identical_semantic_activity_revalidate() {
            let r = Rig::new();
            let mut m = MacRemoval::new(r.observer());
            let c = RemovalChoices::default();
            let p = plan(&r, &mut m, 1, c);
            let yes = consent(&r, &mut m, &p, c);
            r.clock.0.store(1, Ordering::Release);
            assert!(
                m.revalidate(&p, &yes, Some(current_reply(&r, 2, |_| {})), &r.d())
                    .is_ok()
            );
            assert_read_only(&r);
        }
        #[test]
        fn a2b_replayed_reply_or_backwards_receipt_refuses_new_generation() {
            let r = Rig::new();
            let mut m = MacRemoval::new(r.observer());
            r.clock.0.store(10, Ordering::Release);
            let _p = m
                .plan(
                    1,
                    OperationId(1),
                    RemovalChoices::default(),
                    Some(current_reply(&r, 10, |_| {})),
                    &r.d(),
                )
                .unwrap();
            assert_eq!(
                m.plan(
                    2,
                    OperationId(2),
                    RemovalChoices::default(),
                    Some(current_reply(&r, 9, |_| {})),
                    &r.d()
                )
                .unwrap_err(),
                NativeError::Foreign
            );
            r.clock.0.store(9, Ordering::Release);
            assert_eq!(
                m.plan(
                    3,
                    OperationId(3),
                    RemovalChoices::default(),
                    Some(current_reply(&r, 11, |_| {})),
                    &r.d()
                )
                .unwrap_err(),
                NativeError::Foreign
            );
        }
        #[test]
        fn a2b_activity_epoch_change_retires_consent() {
            let r = Rig::new();
            let mut m = MacRemoval::new(r.observer());
            let c = RemovalChoices::default();
            let p = plan(&r, &mut m, 1, c);
            let yes = consent(&r, &mut m, &p, c);
            let changed = current_reply(&r, 2, |v| {
                v["result"]["installer"]["epochs"]["gate"] = json!(2)
            });
            assert_eq!(
                m.revalidate(&p, &yes, Some(changed), &r.d()).unwrap_err(),
                NativeError::Foreign
            );
            assert_eq!(
                m.revalidate(&p, &yes, Some(current_reply(&r, 2, |_| {})), &r.d())
                    .unwrap_err(),
                NativeError::Refused
            );
        }
        #[test]
        fn a2b_lost_current_agent_retires_plan_preserving_original() {
            let r = Rig::new();
            let mut m = MacRemoval::new(r.observer());
            let c = RemovalChoices::default();
            let p = plan(&r, &mut m, 1, c);
            let original = p.tracked_original().unwrap();
            let yes = consent(&r, &mut m, &p, c);
            assert!(m.revalidate(&p, &yes, None, &r.d()).is_err());
            assert_eq!(original.instance_id(), 1);
            assert_eq!(original.process().pid, 4242);
            assert_eq!(std::fs::read(r.io.target().agent_path()).unwrap(), macho());
            assert_read_only(&r);
        }
        #[test]
        fn a2b_changed_resource_bytes_refuse_and_are_preserved() {
            let r = Rig::new();
            let mut m = MacRemoval::new(r.observer());
            let c = RemovalChoices::default();
            let p = plan(&r, &mut m, 1, c);
            let yes = consent(&r, &mut m, &p, c);
            let cli = r.io.target().paths().home.join(".local/bin/crosspanectl");
            r.put(&cli, b"inert hand edit", 0o755);
            assert_eq!(
                m.revalidate(&p, &yes, Some(current_reply(&r, 2, |_| {})), &r.d())
                    .unwrap_err(),
                NativeError::Foreign
            );
            assert_eq!(std::fs::read(cli).unwrap(), b"inert hand edit");
            assert_read_only(&r);
        }
        #[test]
        fn a2b_changed_package_identity_refuses_and_retains_tools() {
            let r = Rig::new();
            let mut m = MacRemoval::new(r.observer());
            let c = RemovalChoices::default();
            let p = plan(&r, &mut m, 1, c);
            let yes = consent(&r, &mut m, &p, c);
            r.put(
                &r.audio().join("CrosspaneAudio-remove-0.1.0.pkg"),
                b"inert different package",
                0o644,
            );
            assert!(
                m.revalidate(&p, &yes, Some(current_reply(&r, 2, |_| {})), &r.d())
                    .is_err()
            );
            assert!(r.io.target().agent_path().exists());
            assert!(
                r.io.target()
                    .paths()
                    .payload_root
                    .join("crosspane-installer")
                    .exists()
            );
            assert_read_only(&r);
        }
        #[test]
        fn a2b_changed_session_or_signature_refuses() {
            for change_session in [true, false] {
                let r = Rig::new();
                let mut m = MacRemoval::new(r.observer());
                let c = RemovalChoices::default();
                let p = plan(&r, &mut m, 1, c);
                let yes = consent(&r, &mut m, &p, c);
                let current = current_reply(&r, 2, |_| {});
                if change_session {
                    r.support.facts.lock().unwrap().gui.console_session =
                        "different fixture".into();
                } else {
                    r.signatures.bad.store(true, Ordering::Release);
                }
                assert!(m.revalidate(&p, &yes, Some(current), &r.d()).is_err());
                assert_read_only(&r);
            }
        }
        #[test]
        fn a2b_expired_or_cancelled_consent_never_survives_revalidation() {
            for cancel in [true, false] {
                let r = Rig::new();
                let mut m = MacRemoval::new(r.observer());
                let c = RemovalChoices::default();
                let p = plan(&r, &mut m, 1, c);
                let yes = consent(&r, &mut m, &p, c);
                let current = current_reply(&r, 2, |_| {});
                let cancelled = Cancellation::default();
                let d = Deadline::new(5000, r.clock.clone(), cancelled.clone()).unwrap();
                if cancel {
                    cancelled.cancel();
                } else {
                    r.clock.0.store(5000, Ordering::Release);
                }
                assert!(m.consent(&p, 1, OperationId(1), c, true, &d).is_err());
                assert!(m.revalidate(&p, &yes, Some(current), &d).is_err());
                assert_read_only(&r);
            }
        }
        #[test]
        fn a2b_unknown_foreign_or_explicit_retirement_is_type_only_debug() {
            let r = Rig::new();
            let mut m = MacRemoval::new(r.observer());
            let c = RemovalChoices::default();
            let p = plan(&r, &mut m, 1, c);
            let yes = consent(&r, &mut m, &p, c);
            for text in [format!("{p:?}"), format!("{yes:?}"), format!("{m:?}")] {
                assert!(!text.contains(&r.io.target().paths().home.to_string_lossy().to_string()));
                assert!(!text.contains("trust"));
                assert!(!text.contains("4242"));
            }
        }

        #[test]
        fn r1_identical_reply_cannot_plan_again_after_retirement() {
            let r = Rig::new();
            let mut m = MacRemoval::new(r.observer());
            let c = RemovalChoices::default();
            let _p = plan(&r, &mut m, 1, c);
            m.retire();
            assert_eq!(
                m.plan(
                    2,
                    OperationId(2),
                    c,
                    Some(current_reply(&r, 1, |_| {})),
                    &r.d()
                )
                .unwrap_err(),
                NativeError::Foreign
            );
            let _fresh = m
                .plan(
                    3,
                    OperationId(3),
                    c,
                    Some(current_reply(&r, 2, |_| {})),
                    &r.d(),
                )
                .unwrap();
        }
        #[test]
        fn r1_identical_reply_cannot_revalidate_and_retires_plan() {
            let r = Rig::new();
            let mut m = MacRemoval::new(r.observer());
            let c = RemovalChoices::default();
            let p = plan(&r, &mut m, 1, c);
            let yes = consent(&r, &mut m, &p, c);
            assert_eq!(
                m.revalidate(&p, &yes, Some(current_reply(&r, 1, |_| {})), &r.d())
                    .unwrap_err(),
                NativeError::Foreign
            );
            assert_eq!(
                m.revalidate(&p, &yes, Some(current_reply(&r, 2, |_| {})), &r.d())
                    .unwrap_err(),
                NativeError::Refused
            );
        }
        #[test]
        fn r1_newer_reply_with_same_receipt_time_revalidates() {
            let r = Rig::new();
            let mut m = MacRemoval::new(r.observer());
            let c = RemovalChoices::default();
            let p = plan(&r, &mut m, 1, c);
            let yes = consent(&r, &mut m, &p, c);
            assert!(
                m.revalidate(&p, &yes, Some(current_reply(&r, 2, |_| {})), &r.d())
                    .is_ok()
            );
            assert_read_only(&r);
        }
    }
}
