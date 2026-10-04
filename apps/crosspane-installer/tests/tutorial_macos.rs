#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Detached fakes only: no AppKit/Quartz query, command, GUI, audio, signing or TCC operation.
use crosspane_installer::{
    agent_contract::ObservationSource,
    fixture::*,
    platform::macos::{fonts::SystemFont, native_io::*, tutorial::*},
    tutorial_window::{Practice, TutorialNative},
};
use crosspane_installer_core::AttemptId;
use crosspane_types::id::{NodeId, WindowId};
use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
const PID: u32 = 505;
const WINDOW: WindowId = WindowId(9001);
fn wait(mut condition: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(Instant::now() < until, "fake-only watchdog");
        thread::sleep(Duration::from_millis(1));
    }
}
fn title() -> String {
    practice_title("Synthetic Mac", AttemptId(7), FixtureId(1)).unwrap()
}
fn row(pid: u32, number: i64, name: &str, primary: bool) -> OwnWindow {
    OwnWindow {
        pid,
        number,
        title: name.into(),
        primary,
    }
}
type Rows = Option<Result<Vec<OwnWindow>, FixtureError>>;
struct Windows(VecDeque<Rows>);
impl OwnWindowObserver for Windows {
    fn windows(&mut self) -> Rows {
        self.0.pop_front().flatten()
    }
}
fn tutorial(rows: Vec<Rows>) -> MacTutorial {
    MacTutorial::with_observer(PID, Box::new(Windows(rows.into())))
}
#[test]
fn own_numeric_quartz_id_is_widened_and_workspace_display_remain_unknown() {
    let mut native = tutorial(vec![Some(Ok(vec![row(
        PID,
        i64::from(u32::MAX),
        &title(),
        true,
    )]))]);
    assert_eq!(
        native.observe_window(1, &title()),
        Some(Ok((WindowId(u64::from(u32::MAX)), OwnWindowFacts::Unknown)))
    );
}
#[test]
fn absent_ambiguous_foreign_auxiliary_and_invalid_numbers_are_refused() {
    for number in [-1, 0, i64::from(u32::MAX) + 1] {
        assert_eq!(
            tutorial(vec![Some(Ok(vec![row(PID, number, &title(), true)]))])
                .observe_window(1, &title()),
            Some(Err(FixtureError::UnknownWindow))
        );
    }
    for rows in [
        vec![],
        vec![row(PID + 1, 3, &title(), true)],
        vec![row(PID, 3, &title(), false)],
        vec![row(PID, 3, "unrelated own title", true)],
    ] {
        assert_eq!(
            tutorial(vec![Some(Ok(rows))]).observe_window(1, &title()),
            Some(Err(FixtureError::UnknownWindow))
        );
    }
    assert_eq!(
        tutorial(vec![Some(Ok(vec![
            row(PID, 3, &title(), true),
            row(PID, 4, &title(), true)
        ]))])
        .observe_window(1, &title()),
        Some(Err(FixtureError::AmbiguousWindow))
    );
}
#[test]
fn observations_are_fresh_pending_and_pin_the_original_window() {
    let mut native = tutorial(vec![
        None,
        Some(Ok(vec![row(PID, 3, &title(), true)])),
        Some(Ok(vec![row(PID, 4, &title(), true)])),
    ]);
    assert_eq!(native.observe_window(1, &title()), None);
    assert_eq!(
        native.observe_window(1, &title()),
        Some(Ok((WindowId(3), OwnWindowFacts::Unknown)))
    );
    assert_eq!(
        native.observe_window(2, &title()),
        Some(Err(FixtureError::UnknownWindow))
    );
    assert_eq!(
        native.observe_window(1, &title()),
        Some(Err(FixtureError::BadCall))
    );
    assert_eq!(
        native.observe_window(3, "foreign uncontrolled title"),
        Some(Err(FixtureError::BadCall))
    );
}
#[test]
fn stage_a_tone_is_unavailable_and_common_attempt_phase_checks_remain() {
    let mut native = tutorial(vec![Some(Ok(vec![row(PID, 9001, &title(), true)]))]);
    let output = SpeakersSelection {
        peer: NodeId([8; 32]),
        device_key: format!("crosspane.{}.speaker", NodeId([8; 32])),
    };
    assert_eq!(
        native.play_tone(ToneId(2), &output),
        Some(Err(FixtureError::Unavailable))
    );
    assert_eq!(
        native.stop_tone(ToneId(2)),
        Some(Err(FixtureError::Unavailable))
    );
    assert_eq!(native.tone_state(), OwnToneState::Stopped);
    let observed = native.observe_window(1, &title()).unwrap();
    let mut practice = Practice::with_native(Box::new(native));
    assert!(
        practice
            .handle(
                call(
                    1,
                    FixtureCommand::Open {
                        machine_label: "Synthetic Mac".into()
                    }
                ),
                observed.clone()
            )
            .unwrap()
            .result
            .is_ok()
    );
    assert!(
        practice
            .handle(
                call(
                    2,
                    FixtureCommand::ArmTarget {
                        fixture: FixtureId(1),
                        phase: PhaseId(2)
                    }
                ),
                observed.clone()
            )
            .unwrap()
            .result
            .is_ok()
    );
    assert_eq!(
        practice.handle(
            call(
                3,
                FixtureCommand::ArmTarget {
                    fixture: FixtureId(1),
                    phase: PhaseId(1)
                }
            ),
            observed.clone()
        ),
        Err(FixtureError::NotOwned)
    );
    let mut foreign = call(
        4,
        FixtureCommand::ObserveWindow {
            fixture: FixtureId(1),
        },
    );
    foreign.attempt = AttemptId(8);
    assert_eq!(
        practice.handle(foreign, observed),
        Err(FixtureError::InvalidMessage)
    );
}

#[derive(Default)]
struct FakeClock(AtomicU64);
impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}
type ObservationAction = (usize, Box<dyn FnOnce() + Send>);
struct Support {
    facts: Mutex<SupportObservation>,
    calls: AtomicUsize,
    action: Mutex<Option<ObservationAction>>,
}
impl SupportProbe for Support {
    fn observe(&self, deadline: &Deadline) -> NativeResult<SupportObservation> {
        deadline.check()?;
        let n = self.calls.fetch_add(1, Ordering::AcqRel) + 1;
        let action = {
            let mut slot = self.action.lock().unwrap();
            if slot.as_ref().is_some_and(|(at, _)| *at == n) {
                slot.take().map(|(_, action)| action)
            } else {
                None
            }
        };
        if let Some(action) = action {
            action();
        }
        Ok(self.facts.lock().unwrap().clone())
    }
}
struct Signatures;
impl SignatureProbe for Signatures {
    fn observe(
        &self,
        _: &Path,
        approved: &SigningRequirement,
        deadline: &Deadline,
    ) -> NativeResult<SignatureObservation> {
        deadline.check()?;
        Ok(SignatureObservation {
            strict_verified: true,
            team_identifier: "ABCDE12345".into(),
            identifier: approved.identifier.clone(),
            designated_requirement: approved.designated_requirement.clone(),
            entitlements: approved.entitlements.clone(),
            apple_development: true,
            hardened_runtime: true,
            ad_hoc: false,
        })
    }
}
struct Runner {
    uid: u32,
    exe: PathBuf,
    mode: AtomicUsize,
    calls: AtomicUsize,
}
impl CommandRunner for Runner {
    fn run(&self, command: &CommandSpec, deadline: &Deadline) -> NativeResult<CommandOutput> {
        deadline.check()?;
        assert_eq!(command.program(), Path::new("/bin/ps"));
        assert_eq!(&command.args()[2..], &["-p", "505"]);
        let n = self.calls.fetch_add(1, Ordering::AcqRel);
        let mode = self.mode.load(Ordering::Acquire);
        let stdout = match command.args()[1].as_str() {
            "uid=" => format!("{}\n", self.uid + u32::from(mode == 1)).into_bytes(),
            "comm=" => {
                if mode == 2 {
                    b"/foreign/executable\n".to_vec()
                } else {
                    format!("{}\n", self.exe.display()).into_bytes()
                }
            }
            "lstart=" => {
                if mode == 3 && n >= 3 {
                    b"Tue Jan 2 00:00:00 2024\n".to_vec()
                } else {
                    b"Mon Jan 1 00:00:00 2024\n".to_vec()
                }
            }
            _ => panic!("unexpected ps query"),
        };
        Ok(CommandOutput {
            code: Some(0),
            stdout,
            stderr: vec![],
        })
    }
}
#[derive(Default)]
struct ProcessState {
    input: Vec<u8>,
    output: VecDeque<u8>,
    sequence: u64,
    eof: bool,
    reaped: bool,
    auto_close: bool,
    ignore_retire: bool,
    preparation_failure: bool,
    prepared: bool,
    retired: usize,
    reads: usize,
    eof_reads: usize,
    first_eof_at: Option<Instant>,
    reap_delay: Duration,
    dropped: bool,
}
struct Process(Arc<Mutex<ProcessState>>);
impl TutorialProcess for Process {
    fn pid(&self) -> u32 {
        PID
    }
    fn prepare_pipes(&mut self) -> NativeResult<()> {
        let mut s = self.0.lock().unwrap();
        s.prepared = true;
        if s.preparation_failure {
            Err(NativeError::Unavailable)
        } else {
            Ok(())
        }
    }
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let mut s = self.0.lock().unwrap();
        s.reads += 1;
        if s.output.is_empty() {
            if s.eof {
                s.eof_reads += 1;
                s.first_eof_at.get_or_insert_with(Instant::now);
                return Ok(0);
            }
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let n = bytes.len().min(s.output.len());
        for b in &mut bytes[..n] {
            *b = s.output.pop_front().unwrap();
        }
        Ok(n)
    }
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut s = self.0.lock().unwrap();
        s.input.extend_from_slice(bytes);
        if s.input.ends_with(b"\n") {
            let packet = decode_control(&s.input).unwrap();
            s.input.clear();
            s.sequence += 1;
            let event = match packet.call.command {
                FixtureCommand::Open { machine_label } => FixtureEvent::Opened {
                    fixture: FixtureId(packet.call.id),
                    pid: PID,
                    window: WINDOW,
                    label: machine_label,
                },
                FixtureCommand::Close { fixture } => {
                    if s.auto_close {
                        s.reaped = true;
                        s.eof = true;
                    }
                    FixtureEvent::CloseRequested { fixture }
                }
                _ => panic!("fake protocol case"),
            };
            let sequence = s.sequence;
            s.output.extend(
                encode_event(&FixtureEventPacket {
                    schema_version: 1,
                    message: FixtureMessage {
                        call_id: Some(packet.call.id),
                        attempt: packet.call.attempt,
                        sequence,
                        result: Ok(event),
                    },
                })
                .unwrap(),
            );
        }
        Ok(bytes.len())
    }
    fn reaped(&mut self) -> NativeResult<bool> {
        let (reaped, delay) = {
            let mut s = self.0.lock().unwrap();
            (s.reaped, std::mem::take(&mut s.reap_delay))
        };
        // Test-only scheduling stall between actual EOF and absence-query admission.
        thread::sleep(delay);
        Ok(reaped)
    }
    fn retire(&mut self) {
        let mut s = self.0.lock().unwrap();
        s.retired += 1;
        if !s.ignore_retire {
            s.reaped = true;
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.0.lock().unwrap().dropped = true;
    }
}
struct Spawner {
    state: Arc<Mutex<ProcessState>>,
    commands: Mutex<Vec<CommandSpec>>,
    blocked: AtomicBool,
    worker_finished: Arc<AtomicBool>,
}
struct WorkerFinished(Arc<AtomicBool>);
impl Drop for WorkerFinished {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
thread_local! {
    // TLS destructors run after the launch closure and its native worker-slot guard drop.
    static WORKER_FINISHED: RefCell<Option<WorkerFinished>> = const { RefCell::new(None) };
}
impl TutorialSpawner for Spawner {
    fn spawn(&self, command: &CommandSpec) -> NativeResult<Box<dyn TutorialProcess>> {
        WORKER_FINISHED.with(|slot| {
            slot.replace(Some(WorkerFinished(self.worker_finished.clone())));
        });
        self.commands.lock().unwrap().push(command.clone());
        while self.blocked.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(1));
        }
        Ok(Box::new(Process(self.state.clone())))
    }
}
static NEXT: AtomicU64 = AtomicU64::new(1);
struct Rig {
    root: PathBuf,
    io: Arc<MacNativeIo>,
    clock: Arc<FakeClock>,
    proof: SupportProof,
    support: Arc<Support>,
    signature: SignatureProof,
    runner: Arc<Runner>,
    spawner: Arc<Spawner>,
}
fn dir(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
fn file(path: &Path) {
    dir(path.parent().unwrap());
    fs::write(path, b"INERT SIGNED-BYTE FIXTURE; NEVER EXECUTED").unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}
fn requirement(role: ArtifactRole) -> SigningRequirement {
    SigningRequirement {
        role,
        identifier: if role == ArtifactRole::Agent {
            AGENT_LABEL.into()
        } else {
            "test.fixture.tutorial".into()
        },
        designated_requirement: "approved-fake-development-requirement".into(),
        entitlements: BTreeMap::new(),
    }
}
impl Rig {
    fn new() -> Self {
        let root = PathBuf::from(format!(
            "/private/tmp/cpt-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let home = root.join("h");
        let tmp = root.join("t");
        let payload = tmp.join("payload");
        for path in [&home, &tmp, &payload, &tmp.join("crosspane")] {
            dir(path);
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
        let exe = target.app_path().join("Contents/MacOS/crosspane-tutorial");
        file(&exe);
        file(&target.agent_path());
        let clock = Arc::new(FakeClock::default());
        let support = Arc::new(Support {
            facts: Mutex::new(SupportObservation {
                macos_major: 26,
                apple_silicon: true,
                gui: GuiObservation {
                    console_uid: Some(uid),
                    interactive_uid: Some(uid),
                    console_session: "fake-aqua".into(),
                    interactive_session: "fake-aqua".into(),
                    active: true,
                },
                gui_tmpdir: tmp,
            }),
            calls: AtomicUsize::new(0),
            action: Mutex::new(None),
        });
        let runner = Arc::new(Runner {
            uid,
            exe: exe.clone(),
            mode: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
        });
        let io = Arc::new(
            MacNativeIo::new(
                target,
                runner.clone(),
                support.clone(),
                Arc::new(Signatures),
                clock.clone(),
            )
            .unwrap(),
        );
        let deadline = Deadline::new(2000, clock.clone(), Cancellation::default()).unwrap();
        let main = io
            .admit_main_signature(
                &io.target().agent_path(),
                &requirement(ArtifactRole::Agent),
                &deadline,
            )
            .unwrap();
        let proof = io.admit_support(&main, &deadline).unwrap();
        let signature = io
            .admit_artifact_signature(&exe, &requirement(ArtifactRole::Tutorial), &main, &deadline)
            .unwrap();
        let spawner = Arc::new(Spawner {
            state: Arc::new(Mutex::new(ProcessState::default())),
            commands: Mutex::new(vec![]),
            blocked: AtomicBool::new(false),
            worker_finished: Arc::new(AtomicBool::new(false)),
        });
        Self {
            root,
            io,
            clock,
            proof,
            support,
            signature,
            runner,
            spawner,
        }
    }
    fn deadline(&self, ms: u64) -> Deadline {
        Deadline::new(ms, self.clock.clone(), Cancellation::default()).unwrap()
    }
    fn launch(&self) -> NativeResult<AdmittedTutorialChild> {
        self.io.launch_tutorial_with(
            &self.proof,
            &self.signature,
            SystemFont::Sfns.path(),
            &self.deadline(2000),
            self.spawner.clone(),
        )
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        self.spawner.blocked.store(false, Ordering::Release);
        self.spawner.state.lock().unwrap().reaped = true;
        fs::remove_dir_all(&self.root).unwrap();
    }
}
#[test]
fn admitted_launch_has_exact_cli_environment_prepared_private_pipes_and_tutorial_identity() {
    let rig = Rig::new();
    let child = rig.launch().unwrap();
    assert_eq!(child.identity().pid, PID);
    assert_eq!(child.identity().executable, rig.runner.exe);
    assert_eq!(child.identity().uid, rig.runner.uid);
    assert!(child.identity().started_unix_ms > 0);
    assert_eq!(child.source(), ObservationSource::Demo);
    assert!(rig.spawner.state.lock().unwrap().prepared);
    let commands = rig.spawner.commands.lock().unwrap();
    assert_eq!(commands.len(), 1);
    let c = &commands[0];
    assert_eq!(c.program(), rig.runner.exe);
    assert_eq!(
        c.args(),
        &["--controlled", "--font", "/System/Library/Fonts/SFNS.ttf"]
    );
    assert_eq!(
        c.environment(),
        &BTreeMap::from([
            (
                "HOME".into(),
                rig.io.target().paths().home.to_string_lossy().into_owned()
            ),
            (
                "TMPDIR".into(),
                rig.io
                    .target()
                    .paths()
                    .gui_tmpdir
                    .to_string_lossy()
                    .into_owned()
            ),
            (
                "CROSSPANE_RUNTIME_DIR".into(),
                rig.io.target().runtime_dir().to_string_lossy().into_owned()
            ),
            ("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into()),
            ("LC_ALL".into(), "C".into()),
            ("TZ".into(), "UTC".into())
        ])
    );
    drop(commands);
    drop(child);
    wait(|| rig.spawner.state.lock().unwrap().dropped);
}
#[test]
fn expired_revoked_foreign_proofs_and_unlisted_font_refuse_before_spawn() {
    let rig = Rig::new();
    let other = Rig::new();
    assert_eq!(
        rig.io
            .launch_tutorial_with(
                &other.proof,
                &rig.signature,
                SystemFont::Sfns.path(),
                &rig.deadline(2000),
                rig.spawner.clone()
            )
            .err(),
        Some(NativeError::Unsupported)
    );
    assert_eq!(
        rig.io
            .launch_tutorial_with(
                &rig.proof,
                &rig.signature,
                Path::new("/private/tmp/user-font.ttf"),
                &rig.deadline(2000),
                rig.spawner.clone()
            )
            .err(),
        Some(NativeError::Foreign)
    );
    rig.clock
        .0
        .store(SUPPORT_LIFETIME_MS + 1, Ordering::Release);
    assert_eq!(rig.launch().err(), Some(NativeError::Unsupported));
    rig.clock.0.store(0, Ordering::Release);
    rig.proof.revoke();
    assert_eq!(rig.launch().err(), Some(NativeError::Unsupported));
    assert!(rig.spawner.commands.lock().unwrap().is_empty());
}
fn invalidated_during_observation(observation: usize, revoke: bool) {
    let rig = Rig::new();
    rig.support.calls.store(0, Ordering::Release);
    let proof = rig.proof.clone();
    let clock = rig.clock.clone();
    *rig.support.action.lock().unwrap() = Some((
        observation,
        Box::new(move || {
            if revoke {
                proof.revoke();
            } else {
                clock.0.store(SUPPORT_LIFETIME_MS + 1, Ordering::Release);
            }
        }),
    ));
    let error = rig
        .io
        .launch_tutorial_with(
            &rig.proof,
            &rig.signature,
            SystemFont::Sfns.path(),
            &rig.deadline(20_000),
            rig.spawner.clone(),
        )
        .err();
    let spawned = rig.spawner.commands.lock().unwrap().len();
    if spawned != 0 {
        wait(|| rig.spawner.state.lock().unwrap().dropped);
        wait(|| rig.spawner.worker_finished.load(Ordering::Acquire));
    }
    assert_eq!(error, Some(NativeError::Unsupported));
    assert_eq!(spawned, usize::from(observation == 3));
    if spawned != 0 {
        assert_eq!(rig.spawner.state.lock().unwrap().retired, 1);
    }
}
#[test]
fn round1_expiry_during_last_pre_spawn_observation_refuses_without_spawn() {
    invalidated_during_observation(2, false);
}
#[test]
fn round1_revocation_during_last_pre_spawn_observation_refuses_without_spawn() {
    invalidated_during_observation(2, true);
}
#[test]
fn round1_expiry_during_post_identity_observation_retires_without_admission() {
    invalidated_during_observation(3, false);
}
#[test]
fn round1_revocation_during_post_identity_observation_retires_without_admission() {
    invalidated_during_observation(3, true);
}
#[test]
fn changed_executable_refuses_before_spawn() {
    let rig = Rig::new();
    fs::write(rig.signature.path(), b"different inert bytes").unwrap();
    assert_eq!(rig.launch().err(), Some(NativeError::Foreign));
    assert!(rig.spawner.commands.lock().unwrap().is_empty());
}
#[test]
fn foreign_uid_executable_and_start_generation_retire_only_owned_child() {
    for mode in 1..=3 {
        let rig = Rig::new();
        rig.runner.mode.store(mode, Ordering::Release);
        assert_eq!(rig.launch().err(), Some(NativeError::Foreign));
        wait(|| rig.spawner.state.lock().unwrap().dropped);
        assert_eq!(rig.spawner.state.lock().unwrap().retired, 1);
    }
}
#[test]
fn failed_pipe_preparation_is_reaped_without_identity_success() {
    let rig = Rig::new();
    rig.spawner.state.lock().unwrap().preparation_failure = true;
    assert_eq!(rig.launch().err(), Some(NativeError::Unavailable));
    wait(|| rig.spawner.state.lock().unwrap().dropped);
    assert_eq!(rig.runner.calls.load(Ordering::Acquire), 0);
}
#[test]
fn noncooperative_launch_deadline_and_cancellation_retain_bounded_slots_until_finish() {
    let rigs: Vec<_> = (0..4).map(|_| Rig::new()).collect();
    for (n, rig) in rigs.iter().enumerate() {
        rig.spawner.blocked.store(true, Ordering::Release);
        let cancellation = Cancellation::default();
        let deadline = Deadline::new(20, rig.clock.clone(), cancellation.clone()).unwrap();
        if n == 0 {
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(10));
                cancellation.cancel();
            });
        }
        let before = Instant::now();
        let result = rig.io.launch_tutorial_with(
            &rig.proof,
            &rig.signature,
            SystemFont::Sfns.path(),
            &deadline,
            rig.spawner.clone(),
        );
        assert!(matches!(
            result,
            Err(NativeError::Timeout | NativeError::Cancelled)
        ));
        assert!(before.elapsed() < Duration::from_millis(200));
        // The 20 ms deadline can fire before the blocked spawner thread records its command.
        wait(|| !rig.spawner.commands.lock().unwrap().is_empty());
    }
    let extra = Rig::new();
    assert_eq!(extra.launch().err(), Some(NativeError::Busy));
    for rig in &rigs {
        rig.spawner.blocked.store(false, Ordering::Release);
    }
    for rig in &rigs {
        wait(|| rig.spawner.state.lock().unwrap().dropped);
        wait(|| rig.spawner.worker_finished.load(Ordering::Acquire));
    }
    let child = extra.launch().unwrap();
    drop(child);
    wait(|| extra.spawner.state.lock().unwrap().dropped);
}
#[test]
fn drop_retirement_drains_until_reap_once_and_releases_child_capacity_afterward() {
    let rigs: Vec<_> = (0..4).map(|_| Rig::new()).collect();
    for rig in &rigs {
        rig.spawner.state.lock().unwrap().ignore_retire = true;
        let mut child = rig.launch().unwrap();
        child.retire();
        child.retire();
        drop(child);
    }
    for rig in &rigs {
        wait(|| rig.spawner.state.lock().unwrap().reads > 0);
        assert_eq!(rig.spawner.state.lock().unwrap().retired, 1);
    }
    let extra = Rig::new();
    assert_eq!(extra.launch().err(), Some(NativeError::Busy));
    for rig in &rigs {
        rig.spawner.state.lock().unwrap().reaped = true;
        wait(|| rig.spawner.state.lock().unwrap().dropped);
    }
    drop(extra.launch().unwrap());
    wait(|| extra.spawner.state.lock().unwrap().dropped);
}

struct Probe {
    child: Arc<Mutex<ProcessState>>,
    calls: AtomicUsize,
    blocked: AtomicBool,
    answer: Mutex<Result<bool, FixtureError>>,
    finished: AtomicBool,
}
impl ChildWindowProbe for Probe {
    fn absent(&self, pid: u32) -> Result<bool, FixtureError> {
        assert_eq!(pid, PID);
        {
            let s = self.child.lock().unwrap();
            assert!(s.reaped);
            assert!(s.eof_reads > 0);
        }
        self.calls.fetch_add(1, Ordering::AcqRel);
        while self.blocked.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(1));
        }
        let result = *self.answer.lock().unwrap();
        self.finished.store(true, Ordering::Release);
        result
    }
}
fn call(id: u64, command: FixtureCommand) -> FixtureCall {
    FixtureCall {
        id,
        attempt: AttemptId(7),
        command,
    }
}
fn messages(port: &mut PipeFixturePort, count: usize) -> Vec<FixtureMessage> {
    let mut result = Vec::new();
    wait(|| {
        result.extend(port.poll());
        result.len() >= count
    });
    result
}
fn opened(
    rig: &Rig,
    blocked: bool,
    answer: Result<bool, FixtureError>,
) -> (PipeFixturePort, Arc<Probe>) {
    let probe = Arc::new(Probe {
        child: rig.spawner.state.clone(),
        calls: AtomicUsize::new(0),
        blocked: AtomicBool::new(blocked),
        answer: Mutex::new(answer),
        finished: AtomicBool::new(false),
    });
    let clock = rig.clock.clone();
    let mut port = fixture_port_with(
        rig.launch().unwrap(),
        probe.clone(),
        Arc::new(move || clock.now_ms()),
    )
    .unwrap();
    port.submit(call(
        1,
        FixtureCommand::Open {
            machine_label: "Synthetic Mac".into(),
        },
    ))
    .unwrap();
    assert!(matches!(
        messages(&mut port, 1)[0].result,
        Ok(FixtureEvent::Opened { .. })
    ));
    (port, probe)
}
fn close(port: &mut PipeFixturePort) {
    port.submit(call(
        2,
        FixtureCommand::Close {
            fixture: FixtureId(1),
        },
    ))
    .unwrap();
    port.complete_closed(AttemptId(7), FixtureId(1)).unwrap();
}
fn terminal(port: &mut PipeFixturePort) -> Vec<FixtureMessage> {
    let mut result = vec![];
    wait(|| {
        result.extend(port.poll());
        result
            .iter()
            .any(|m| m.result.is_err() || matches!(m.result, Ok(FixtureEvent::Closed { .. })))
    });
    result
}
#[test]
fn fresh_absence_finishes_before_eof_exposure_and_confirms_closed() {
    let rig = Rig::new();
    rig.spawner.state.lock().unwrap().auto_close = true;
    let (mut port, probe) = opened(&rig, false, Ok(true));
    close(&mut port);
    assert!(
        terminal(&mut port)
            .iter()
            .any(|m| matches!(m.result, Ok(FixtureEvent::Closed { .. })))
    );
    assert_eq!(probe.calls.load(Ordering::Acquire), 1);
    wait(|| rig.spawner.state.lock().unwrap().dropped);
}
#[test]
fn absence_after_actual_eof_is_pending_then_confirms_within_fixed_hold() {
    let rig = Rig::new();
    rig.spawner.state.lock().unwrap().auto_close = true;
    let (mut port, probe) = opened(&rig, true, Ok(true));
    close(&mut port);
    wait(|| probe.calls.load(Ordering::Acquire) == 1);
    assert!(
        !port
            .poll()
            .iter()
            .any(|m| matches!(m.result, Ok(FixtureEvent::Closed { .. })))
    );
    probe.blocked.store(false, Ordering::Release);
    assert!(
        terminal(&mut port)
            .iter()
            .any(|m| matches!(m.result, Ok(FixtureEvent::Closed { .. })))
    );
    wait(|| rig.spawner.state.lock().unwrap().dropped);
}
#[test]
fn query_error_or_present_window_exposes_unconfirmed_eof_without_restart() {
    for answer in [Err(FixtureError::Unavailable), Ok(false)] {
        let rig = Rig::new();
        rig.spawner.state.lock().unwrap().auto_close = true;
        let (mut port, probe) = opened(&rig, false, answer);
        close(&mut port);
        let result = terminal(&mut port);
        assert!(
            !result
                .iter()
                .any(|m| matches!(m.result, Ok(FixtureEvent::Closed { .. })))
        );
        assert!(
            result
                .iter()
                .any(|m| m.result == Err(FixtureError::ChildExited))
        );
        assert_eq!(probe.calls.load(Ordering::Acquire), 1);
        wait(|| rig.spawner.state.lock().unwrap().dropped);
    }
}
#[test]
fn pending_absence_at_500ms_exposes_eof_unconfirmed_never_closed() {
    let rig = Rig::new();
    rig.spawner.state.lock().unwrap().auto_close = true;
    let (mut port, probe) = opened(&rig, true, Ok(true));
    close(&mut port);
    wait(|| probe.calls.load(Ordering::Acquire) == 1);
    let start = rig.spawner.state.lock().unwrap().first_eof_at.unwrap();
    let result = terminal(&mut port);
    assert!(start.elapsed() >= Duration::from_millis(450));
    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(
        result
            .iter()
            .any(|m| m.result == Err(FixtureError::ChildExited))
    );
    assert!(
        !result
            .iter()
            .any(|m| matches!(m.result, Ok(FixtureEvent::Closed { .. })))
    );
    probe.blocked.store(false, Ordering::Release);
    wait(|| probe.finished.load(Ordering::Acquire));
    assert_eq!(probe.calls.load(Ordering::Acquire), 1);
}
#[test]
fn round1_delayed_query_start_keeps_hold_anchored_to_actual_pipe_eof() {
    let rig = Rig::new();
    rig.spawner.state.lock().unwrap().auto_close = true;
    let (mut port, probe) = opened(&rig, true, Ok(true));
    rig.spawner.state.lock().unwrap().reap_delay = Duration::from_millis(200);
    close(&mut port);
    wait(|| probe.calls.load(Ordering::Acquire) == 1);
    let eof = rig.spawner.state.lock().unwrap().first_eof_at.unwrap();
    assert!(eof.elapsed() >= Duration::from_millis(200));
    thread::sleep(Duration::from_millis(520).saturating_sub(eof.elapsed()));
    let result = port.poll();
    probe.blocked.store(false, Ordering::Release);
    wait(|| probe.finished.load(Ordering::Acquire));
    assert!(
        result
            .iter()
            .any(|m| m.result == Err(FixtureError::ChildExited))
    );
    assert!(
        !result
            .iter()
            .any(|m| matches!(m.result, Ok(FixtureEvent::Closed { .. })))
    );
    assert_eq!(probe.calls.load(Ordering::Acquire), 1);
}
#[test]
fn original_close_deadline_expires_during_hold_never_becomes_closed() {
    let rig = Rig::new();
    rig.spawner.state.lock().unwrap().auto_close = true;
    let (mut port, probe) = opened(&rig, true, Ok(true));
    close(&mut port);
    wait(|| probe.calls.load(Ordering::Acquire) == 1);
    rig.clock.0.store(RESPONSE_MS, Ordering::Release);
    let result = terminal(&mut port);
    assert!(
        result
            .iter()
            .any(|m| m.result == Err(FixtureError::TimedOut))
    );
    assert!(
        !result
            .iter()
            .any(|m| matches!(m.result, Ok(FixtureEvent::Closed { .. })))
    );
    probe.blocked.store(false, Ordering::Release);
    wait(|| probe.finished.load(Ordering::Acquire));
    assert_eq!(probe.calls.load(Ordering::Acquire), 1);
}
#[test]
fn eof_without_reap_never_queries_or_confirms_cleanup() {
    let rig = Rig::new();
    let (mut port, probe) = opened(&rig, false, Ok(true));
    rig.spawner.state.lock().unwrap().eof = true;
    let result = terminal(&mut port);
    assert!(
        result
            .iter()
            .any(|m| m.result == Err(FixtureError::ChildExited))
    );
    assert_eq!(probe.calls.load(Ordering::Acquire), 0);
}

#[derive(Clone)]
struct FakeAudio(Arc<Mutex<AudioState>>);
struct AudioState {
    facts: OutputDevice,
    inspected: usize,
    created: usize,
    started: usize,
    stopped: usize,
    destroyed: usize,
    sessions_dropped: usize,
    buffer: Option<Arc<ToneBuffer>>,
    block_inspect: bool,
    block_after_start: bool,
    block_start: bool,
    block_stop: bool,
    create_error: Option<FixtureError>,
    start_error: Option<FixtureError>,
    stop_error: Option<FixtureError>,
    destroy_error: Option<FixtureError>,
    replace_on_create: bool,
    finished: Arc<AtomicBool>,
}
fn output_facts() -> OutputDevice {
    OutputDevice {
        id: 300,
        uid: SPEAKERS_APP_UID.into(),
        alive: true,
        hidden: false,
        class: u32::from_be_bytes(*b"adev"),
        transport: u32::from_be_bytes(*b"virt"),
        nominal_rate: 48_000.0,
        inputs: vec![],
        outputs: vec![(
            301,
            OutputFormat {
                mSampleRate: 48_000.0,
                mFormatID: u32::from_be_bytes(*b"lpcm"),
                mFormatFlags: 9,
                mBytesPerPacket: 8,
                mFramesPerPacket: 1,
                mBytesPerFrame: 8,
                mChannelsPerFrame: 2,
                mBitsPerChannel: 32,
                mReserved: 0,
            },
        )],
    }
}
impl FakeAudio {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(AudioState {
            facts: output_facts(),
            inspected: 0,
            created: 0,
            started: 0,
            stopped: 0,
            destroyed: 0,
            sessions_dropped: 0,
            buffer: None,
            block_inspect: false,
            block_after_start: false,
            block_start: false,
            block_stop: false,
            create_error: None,
            start_error: None,
            stop_error: None,
            destroy_error: None,
            replace_on_create: false,
            finished: Arc::new(AtomicBool::new(false)),
        })))
    }
    fn buffer(&self) -> Arc<ToneBuffer> {
        self.0.lock().unwrap().buffer.clone().unwrap()
    }
    fn finished(&self) -> bool {
        self.0.lock().unwrap().finished.load(Ordering::Acquire)
    }
}
impl TutorialHal for FakeAudio {
    fn inspect(&self) -> Result<OutputDevice, FixtureError> {
        {
            let mut s = self.0.lock().unwrap();
            let done = s.finished.clone();
            WORKER_FINISHED.with(|slot| {
                if slot.borrow().is_none() {
                    slot.replace(Some(WorkerFinished(done)));
                }
            });
            s.inspected += 1;
        }
        wait(|| {
            let s = self.0.lock().unwrap();
            !s.block_inspect && !(s.block_after_start && s.started > 0)
        });
        Ok(self.0.lock().unwrap().facts.clone())
    }
    fn create(
        &self,
        device: u32,
        buffer: Arc<ToneBuffer>,
    ) -> Result<Box<dyn ToneSession>, FixtureError> {
        assert_eq!(device, 300);
        let mut s = self.0.lock().unwrap();
        s.created += 1;
        if let Some(error) = s.create_error {
            return Err(error);
        }
        s.buffer = Some(buffer.clone());
        if s.replace_on_create {
            s.facts.id = 400;
        }
        Ok(Box::new(FakeTone {
            hal: self.clone(),
            _buffer: buffer,
        }))
    }
}
struct FakeTone {
    hal: FakeAudio,
    _buffer: Arc<ToneBuffer>,
}
impl ToneSession for FakeTone {
    fn start(&mut self) -> Result<(), FixtureError> {
        self.hal.0.lock().unwrap().started += 1;
        wait(|| !self.hal.0.lock().unwrap().block_start);
        self.hal.0.lock().unwrap().start_error.map_or(Ok(()), Err)
    }
    fn stop(&mut self) -> Result<(), FixtureError> {
        self.hal.0.lock().unwrap().stopped += 1;
        wait(|| !self.hal.0.lock().unwrap().block_stop);
        self.hal.0.lock().unwrap().stop_error.map_or(Ok(()), Err)
    }
    fn destroy(&mut self) -> Result<(), FixtureError> {
        self.hal.0.lock().unwrap().destroyed += 1;
        self.hal.0.lock().unwrap().destroy_error.map_or(Ok(()), Err)
    }
}
impl Drop for FakeTone {
    fn drop(&mut self) {
        self.hal.0.lock().unwrap().sessions_dropped += 1;
    }
}
fn audio_fixture(hal: &FakeAudio) -> MacTutorial {
    MacTutorial::with_output(
        PID,
        Box::new(Windows(VecDeque::new())),
        Arc::new(hal.clone()),
    )
}
fn speakers(peer: u8) -> SpeakersSelection {
    SpeakersSelection {
        peer: NodeId([peer; 32]),
        device_key: format!("crosspane.{}.speaker", NodeId([peer; 32])),
    }
}
fn play_result(native: &mut MacTutorial, id: u64) -> Result<(), FixtureError> {
    let mut result = None;
    wait(|| {
        result = native.play_tone(ToneId(id), &speakers(8));
        result.is_some()
    });
    result.unwrap()
}
fn stop_result(native: &mut MacTutorial, id: u64) -> Result<(), FixtureError> {
    let mut result = None;
    wait(|| {
        result = native.stop_tone(ToneId(id));
        result.is_some()
    });
    result.unwrap()
}
fn start_audio(native: &mut MacTutorial, id: u64) {
    assert_eq!(native.play_tone(ToneId(id), &speakers(8)), None);
    assert_eq!(play_result(native, id), Ok(()));
}
#[test]
fn stage_b_valid_selection_uses_only_injected_fixed_uid_output() {
    let hal = FakeAudio::new();
    let mut native = audio_fixture(&hal);
    let mut wrong = speakers(8);
    wrong.device_key = SPEAKERS_APP_UID.into();
    assert_eq!(
        native.play_tone(ToneId(1), &wrong),
        Some(Err(FixtureError::Unavailable))
    );
    assert_eq!(hal.0.lock().unwrap().inspected, 0);
    start_audio(&mut native, 1);
    assert_eq!(
        native.tone_state(),
        OwnToneState::Running { tone: ToneId(1) }
    );
    assert_eq!(stop_result(&mut native, 1), Ok(()));
    wait(|| hal.finished());
    let s = hal.0.lock().unwrap();
    assert_eq!((s.created, s.started, s.stopped, s.destroyed), (1, 1, 1, 1));
}
#[test]
fn stage_b_peer_pin_survives_stop_and_failed_open_without_queries_for_another_peer() {
    for fail in [false, true] {
        let hal = FakeAudio::new();
        if fail {
            hal.0.lock().unwrap().create_error = Some(FixtureError::OutputUnavailable);
        }
        let mut native = audio_fixture(&hal);
        assert_eq!(native.play_tone(ToneId(1), &speakers(8)), None);
        assert_eq!(
            play_result(&mut native, 1),
            if fail {
                Err(FixtureError::OutputUnavailable)
            } else {
                Ok(())
            }
        );
        if !fail {
            assert_eq!(stop_result(&mut native, 1), Ok(()));
        }
        wait(|| hal.finished());
        let inspected = hal.0.lock().unwrap().inspected;
        assert_eq!(
            native.play_tone(ToneId(2), &speakers(9)),
            Some(Err(FixtureError::Unavailable))
        );
        assert_eq!(hal.0.lock().unwrap().inspected, inspected);
        if !fail {
            hal.0
                .lock()
                .unwrap()
                .finished
                .store(false, Ordering::Release);
            start_audio(&mut native, 2);
            assert_eq!(stop_result(&mut native, 2), Ok(()));
            wait(|| hal.finished());
        }
    }
}
#[test]
fn stage_b_unsupported_format_and_every_topology_field_refuse_before_create() {
    for case in 0..19 {
        let hal = FakeAudio::new();
        {
            let mut s = hal.0.lock().unwrap();
            let f = &mut s.facts;
            match case {
                0 => f.id = 0,
                1 => f.uid = "physical-output".into(),
                2 => f.alive = false,
                3 => f.hidden = true,
                4 => f.class = 0,
                5 => f.transport = 0,
                6 => f.inputs.push(400),
                7 => f.outputs.clear(),
                8 => f.outputs.push(f.outputs[0]),
                9 => f.outputs[0].0 = 0,
                10 => f.nominal_rate = 44_100.0,
                11 => f.outputs[0].1.mSampleRate = 44_100.0,
                12 => f.outputs[0].1.mFormatID = 0,
                13 => f.outputs[0].1.mFormatFlags = 41,
                14 => f.outputs[0].1.mBytesPerPacket = 4,
                15 => f.outputs[0].1.mFramesPerPacket = 0,
                16 => f.outputs[0].1.mBytesPerFrame = 4,
                17 => f.outputs[0].1.mChannelsPerFrame = 1,
                18 => f.outputs[0].1.mBitsPerChannel = 16,
                _ => unreachable!(),
            }
        }
        let mut native = audio_fixture(&hal);
        assert_eq!(native.play_tone(ToneId(1), &speakers(8)), None);
        assert_eq!(
            play_result(&mut native, 1),
            Err(if case < 10 {
                FixtureError::OutputUnavailable
            } else {
                FixtureError::UnsupportedFormat
            })
        );
        wait(|| hal.finished());
        assert_eq!(hal.0.lock().unwrap().created, 0);
    }
    let hal = FakeAudio::new();
    hal.0.lock().unwrap().facts.outputs[0].1.mReserved = 1;
    let mut native = audio_fixture(&hal);
    assert_eq!(native.play_tone(ToneId(1), &speakers(8)), None);
    assert_eq!(
        play_result(&mut native, 1),
        Err(FixtureError::UnsupportedFormat)
    );
    wait(|| hal.finished());
}
#[test]
fn stage_b_output_replacement_before_start_and_during_operation_never_rebinds() {
    for before_start in [true, false] {
        let hal = FakeAudio::new();
        hal.0.lock().unwrap().replace_on_create = before_start;
        let mut native = audio_fixture(&hal);
        assert_eq!(native.play_tone(ToneId(1), &speakers(8)), None);
        if !before_start {
            assert_eq!(play_result(&mut native, 1), Ok(()));
            hal.0.lock().unwrap().facts.id = 400;
        }
        wait(|| hal.finished());
        assert_eq!(
            native.play_tone(ToneId(1), &speakers(8)),
            Some(Err(FixtureError::OutputChanged))
        );
        assert!(matches!(
            native.tone_state(),
            OwnToneState::StopUnconfirmed { .. }
        ));
        assert_eq!(
            native.play_tone(ToneId(2), &speakers(8)),
            Some(Err(FixtureError::Busy))
        );
        let s = hal.0.lock().unwrap();
        assert_eq!(s.started, usize::from(!before_start));
        assert_eq!((s.created, s.stopped, s.destroyed), (1, 1, 1));
    }
}
#[test]
fn stage_b_bounded_ramped_tone_is_stereo_and_natural_stop_requires_actual_destroy() {
    let hal = FakeAudio::new();
    let mut native = audio_fixture(&hal);
    start_audio(&mut native, 1);
    let buffer = hal.buffer();
    let mut all = Vec::new();
    while all.len() < 96_000 * 2 {
        let frames = (96_000 - all.len() / 2).min(4096);
        let mut samples = vec![99.0; frames * 2];
        buffer.render(2, &mut samples);
        assert!(samples.as_chunks::<2>().0.iter().all(|p| p[0] == p[1]));
        all.extend(samples);
    }
    assert_eq!(all[0], 0.0);
    assert_eq!(*all.last().unwrap(), 0.0);
    assert!(all.iter().all(|s| s.abs() <= 0.0316228 + 1e-8));
    assert!((all[2 * 12] - 0.0316228 * 12.0 / 960.0).abs() < 1e-8);
    assert!((all[2 * 972] - 0.0316228).abs() < 1e-7);
    let mut extra = [99.0; 32];
    buffer.render(2, &mut extra);
    assert_eq!(extra, [0.0; 32]);
    wait(|| hal.finished());
    assert_eq!(native.tone_state(), OwnToneState::Stopped);
    assert_eq!(hal.0.lock().unwrap().destroyed, 1);
    assert_eq!(native.stop_tone(ToneId(1)), Some(Ok(())));
}
#[test]
fn stage_b_stop_ownership_single_start_and_identical_polling_never_resend() {
    let hal = FakeAudio::new();
    hal.0.lock().unwrap().block_inspect = true;
    let mut native = audio_fixture(&hal);
    assert_eq!(native.play_tone(ToneId(1), &speakers(8)), None);
    wait(|| hal.0.lock().unwrap().inspected == 1);
    for _ in 0..100 {
        assert_eq!(native.play_tone(ToneId(1), &speakers(8)), None);
    }
    assert_eq!(
        native.stop_tone(ToneId(2)),
        Some(Err(FixtureError::NotOwned))
    );
    assert_eq!(
        native.play_tone(ToneId(2), &speakers(8)),
        Some(Err(FixtureError::Busy))
    );
    hal.0.lock().unwrap().block_inspect = false;
    assert_eq!(play_result(&mut native, 1), Ok(()));
    assert_eq!(stop_result(&mut native, 1), Ok(()));
    wait(|| hal.finished());
    assert_eq!(hal.0.lock().unwrap().created, 1);
    assert_eq!(hal.0.lock().unwrap().started, 1);
    assert_eq!(native.play_tone(ToneId(1), &speakers(8)), Some(Ok(())));
}
#[test]
fn stage_b_drop_pending_start_disables_immediately_and_cleans_only_owned_registration() {
    let hal = FakeAudio::new();
    hal.0.lock().unwrap().block_start = true;
    let mut native = audio_fixture(&hal);
    assert_eq!(native.play_tone(ToneId(1), &speakers(8)), None);
    wait(|| hal.0.lock().unwrap().started == 1);
    drop(native);
    let mut output = [99.0; 32];
    hal.buffer().render(2, &mut output);
    assert_eq!(output, [0.0; 32]);
    hal.0.lock().unwrap().block_start = false;
    wait(|| hal.finished());
    let s = hal.0.lock().unwrap();
    assert_eq!(
        (s.started, s.stopped, s.destroyed, s.sessions_dropped),
        (1, 1, 1, 1)
    );
}
#[test]
fn stage_b_uncertain_teardown_retains_state_and_never_reports_tone_stopped() {
    for stop_failure in [true, false] {
        let hal = FakeAudio::new();
        {
            let mut s = hal.0.lock().unwrap();
            if stop_failure {
                s.stop_error = Some(FixtureError::CleanupFailed);
            } else {
                s.destroy_error = Some(FixtureError::CleanupFailed);
            }
        }
        let mut native = audio_fixture(&hal);
        start_audio(&mut native, 1);
        assert_eq!(
            stop_result(&mut native, 1),
            Err(FixtureError::CleanupFailed)
        );
        wait(|| hal.finished());
        assert_eq!(
            native.tone_state(),
            OwnToneState::StopUnconfirmed { tone: ToneId(1) }
        );
        assert_eq!(
            native.play_tone(ToneId(2), &speakers(8)),
            Some(Err(FixtureError::Busy))
        );
        let mut output = [99.0; 32];
        hal.buffer().render(2, &mut output);
        assert_eq!(output, [0.0; 32]);
        let s = hal.0.lock().unwrap();
        assert_eq!((s.stopped, s.destroyed, s.sessions_dropped), (1, 1, 0));
        assert!(Arc::strong_count(s.buffer.as_ref().unwrap()) >= 3);
    }
}
#[test]
fn stage_b_stop_deadline_is_50ms_with_silence_and_no_late_success() {
    let hal = FakeAudio::new();
    hal.0.lock().unwrap().block_stop = true;
    let mut native = audio_fixture(&hal);
    start_audio(&mut native, 1);
    let at = Instant::now();
    let result = stop_result(&mut native, 1);
    let elapsed = at.elapsed();
    let mut output = [99.0; 32];
    hal.buffer().render(2, &mut output);
    hal.0.lock().unwrap().block_stop = false;
    wait(|| hal.finished());
    assert_eq!(result, Err(FixtureError::CleanupFailed));
    assert!(elapsed >= Duration::from_millis(50) && elapsed < Duration::from_millis(200));
    assert_eq!(output, [0.0; 32]);
    assert_eq!(
        native.tone_state(),
        OwnToneState::StopUnconfirmed { tone: ToneId(1) }
    );
    assert_eq!(
        native.stop_tone(ToneId(1)),
        Some(Err(FixtureError::CleanupFailed))
    );
}
#[test]
fn stage_b_open_deadline_is_two_seconds_and_late_start_cannot_reenable_generation() {
    let hal = FakeAudio::new();
    hal.0.lock().unwrap().block_start = true;
    let mut native = audio_fixture(&hal);
    assert_eq!(native.play_tone(ToneId(1), &speakers(8)), None);
    wait(|| hal.0.lock().unwrap().started == 1);
    let result = play_result(&mut native, 1);
    let mut output = [99.0; 32];
    hal.buffer().render(2, &mut output);
    hal.0.lock().unwrap().block_start = false;
    wait(|| hal.finished());
    assert_eq!(result, Err(FixtureError::TimedOut));
    assert_eq!(output, [0.0; 32]);
    assert_eq!(
        native.tone_state(),
        OwnToneState::StopUnconfirmed { tone: ToneId(1) }
    );
    assert_eq!(
        native.play_tone(ToneId(2), &speakers(8)),
        Some(Err(FixtureError::Busy))
    );
}
#[test]
fn stage_b_worker_capacity_remains_busy_until_noncooperative_inspection_really_finishes() {
    let fixtures: Vec<_> = (0..4)
        .map(|_| {
            let hal = FakeAudio::new();
            hal.0.lock().unwrap().block_inspect = true;
            let mut native = audio_fixture(&hal);
            assert_eq!(native.play_tone(ToneId(1), &speakers(8)), None);
            wait(|| hal.0.lock().unwrap().inspected == 1);
            (hal, native)
        })
        .collect();
    let extra = FakeAudio::new();
    let mut native = audio_fixture(&extra);
    assert_eq!(
        native.play_tone(ToneId(1), &speakers(8)),
        Some(Err(FixtureError::Busy))
    );
    assert_eq!(extra.0.lock().unwrap().inspected, 0);
    for (hal, output) in fixtures {
        drop(output);
        hal.0.lock().unwrap().block_inspect = false;
        wait(|| hal.finished());
        assert_eq!(hal.0.lock().unwrap().created, 0);
    }
    start_audio(&mut native, 1);
    assert_eq!(stop_result(&mut native, 1), Ok(()));
    wait(|| extra.finished());
}

#[test]
fn round1_open_completion_after_deadline_without_polling_remains_failed() {
    let hal = FakeAudio::new();
    hal.0.lock().unwrap().block_start = true;
    let mut native = audio_fixture(&hal);
    assert_eq!(native.play_tone(ToneId(1), &speakers(8)), None);
    wait(|| hal.0.lock().unwrap().started == 1);
    // No play/stop polls run while the injected native start exceeds the original deadline.
    thread::sleep(Duration::from_millis(2100));
    hal.0.lock().unwrap().block_start = false;
    wait(|| hal.finished() || matches!(native.tone_state(), OwnToneState::Running { .. }));
    let result = native.play_tone(ToneId(1), &speakers(8));
    let state = native.tone_state();
    drop(native);
    wait(|| hal.finished());
    assert_eq!(result, Some(Err(FixtureError::CleanupFailed)));
    assert_eq!(state, OwnToneState::StopUnconfirmed { tone: ToneId(1) });
    let mut output = [99.0; 32];
    hal.buffer().render(2, &mut output);
    assert_eq!(output, [0.0; 32]);
}

#[test]
fn round1_stop_completion_after_deadline_without_polling_remains_failed() {
    let hal = FakeAudio::new();
    hal.0.lock().unwrap().block_stop = true;
    let mut native = audio_fixture(&hal);
    start_audio(&mut native, 1);
    assert_eq!(native.stop_tone(ToneId(1)), None);
    wait(|| hal.0.lock().unwrap().stopped == 1);
    // Suspend completion polls until the stop and destruction have actually completed late.
    thread::sleep(Duration::from_millis(80));
    hal.0.lock().unwrap().block_stop = false;
    wait(|| hal.finished());
    assert_eq!(
        native.stop_tone(ToneId(1)),
        Some(Err(FixtureError::CleanupFailed))
    );
    assert_eq!(
        native.tone_state(),
        OwnToneState::StopUnconfirmed { tone: ToneId(1) }
    );
    assert_eq!(
        native.play_tone(ToneId(2), &speakers(8)),
        Some(Err(FixtureError::Busy))
    );
}

#[test]
fn round1_final_valid_callback_then_failure_cannot_publish_natural_stop() {
    let hal = FakeAudio::new();
    hal.0.lock().unwrap().block_after_start = true;
    let mut native = audio_fixture(&hal);
    start_audio(&mut native, 1);
    // The worker has entered its post-start inspection before any final callback facts change.
    wait(|| hal.0.lock().unwrap().inspected >= 3);
    let buffer = hal.buffer();
    let mut frames = [99.0; 8192];
    for _ in 0..24 {
        buffer.render(2, &mut frames);
    }
    let mut invalid = [99.0; 16];
    buffer.render(1, &mut invalid);
    assert_eq!(invalid, [0.0; 16]);
    hal.0.lock().unwrap().block_after_start = false;
    wait(|| hal.finished());
    assert_eq!(
        native.play_tone(ToneId(1), &speakers(8)),
        Some(Err(FixtureError::OutputChanged))
    );
    assert_eq!(
        native.tone_state(),
        OwnToneState::StopUnconfirmed { tone: ToneId(1) }
    );
    assert_eq!(
        native.play_tone(ToneId(2), &speakers(8)),
        Some(Err(FixtureError::Busy))
    );
    assert_eq!(hal.0.lock().unwrap().destroyed, 1);
}

#[test]
fn verify2_natural_finish_teardown_after_deadline_without_polling_remains_failed() {
    let hal = FakeAudio::new();
    hal.0.lock().unwrap().block_stop = true;
    let mut native = audio_fixture(&hal);
    start_audio(&mut native, 1);
    let buffer = hal.buffer();
    let mut frames = [99.0; 8192];
    for _ in 0..24 {
        buffer.render(2, &mut frames);
    }
    wait(|| hal.0.lock().unwrap().stopped == 1);
    // No explicit stop supplies a deadline, and no completion polls run during late teardown.
    thread::sleep(Duration::from_millis(80));
    hal.0.lock().unwrap().block_stop = false;
    wait(|| hal.finished());
    assert_eq!(
        native.tone_state(),
        OwnToneState::StopUnconfirmed { tone: ToneId(1) }
    );
    assert_eq!(
        native.play_tone(ToneId(1), &speakers(8)),
        Some(Err(FixtureError::CleanupFailed))
    );
    assert_eq!(
        native.play_tone(ToneId(2), &speakers(8)),
        Some(Err(FixtureError::Busy))
    );
}
