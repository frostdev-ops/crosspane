//! Included only by the scratch repair suite, sharing its fake native types and owned socket.
#[path = "../integration/domains.rs"]
mod domains;
#[path = "../integration/native.rs"]
mod native;
use super::*;
use domains::{Installs, Repairer};
use native::{MacProbes, NativeEnv, NativeInstalls, NativeRepairer};
use std::io::BufRead;
use std::time::{Duration, Instant};

fn repairer(f: &Fixture) -> NativeRepairer {
    NativeRepairer::new(Arc::new(NativeEnv::new(
        f.io.target().clone(),
        inventory(),
        MacProbes {
            support: f.support.clone(),
            signatures: f.signatures.clone(),
            approval: Arc::new(ApprovalFixture(Approval::Allowed)),
            runner: f.runner.clone(),
        },
        f.clock.clone(),
    )))
}

fn one_status(f: &Fixture) -> std::thread::JoinHandle<u64> {
    let listener = f._listener.try_clone().unwrap();
    listener.set_nonblocking(true).unwrap();
    let bootstrap = parse_bootstrap(&read_owned(&f.runtime.join("bootstrap.json"))).unwrap();
    let mut reply = serde_json::to_vec(&f.status(bootstrap.instance_id)).unwrap();
    reply.push(b'\n');
    let record_path = f.io.target().installer_dir().join("repair.json");
    std::thread::spawn(move || {
        let end = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < end =>
                {
                    std::thread::sleep(Duration::from_millis(1))
                }
                Err(error) => panic!("owned fake accept: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = Vec::new();
        std::io::BufReader::new(stream.try_clone().unwrap())
            .read_until(b'\n', &mut request)
            .unwrap();
        assert_eq!(request, encode_request(&InstallerRequest::Status).unwrap());
        let reserved = if record_path.exists() {
            let value: Value = serde_json::from_slice(&read_owned(&record_path)).unwrap();
            value["status_watermark"].as_u64().unwrap()
        } else {
            0 // The original confirm's read precedes its first persisted repair intent.
        };
        stream.write_all(&reply).unwrap();
        reserved
    })
}

#[test]
fn fresh_adapter_inspection_offers_saved_reassessment_without_status_or_mutation() {
    let (f, saved) = completed_with_saved_hint();
    let path = f.io.target().installer_dir().join("repair.json");
    bytes(&path, &saved, 0o600);
    let identity = f.io.metadata(&path).unwrap();
    let mutations = (f.runner.count("bootout"), f.runner.count("bootstrap"));
    let offer = repairer(&f).inspect(&f.deadline());
    assert!(offer.resumable.is_some());
    assert!(matches!(offer.repair, live::Availability::Unavailable(_)));
    assert_eq!(read_owned(&path), saved);
    assert_eq!(f.io.metadata(&path).unwrap(), identity);
    assert_eq!(
        (f.runner.count("bootout"), f.runner.count("bootstrap")),
        mutations
    );
    f._listener.set_nonblocking(true).unwrap();
    assert!(
        matches!(f._listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
}

#[test]
fn new_adapter_resume_reads_reserved_status_and_reports_only_checked_current_health() {
    let (f, saved) = completed_with_saved_hint();
    let path = f.io.target().installer_dir().join("repair.json");
    bytes(&path, &saved, 0o600);
    let mutations = (f.runner.count("bootout"), f.runner.count("bootstrap"));
    let server = one_status(&f);
    let finish = repairer(&f).resume(None, &f.deadline()).unwrap();
    assert!(server.join().unwrap() > 101);
    assert_eq!(
        finish.outcome,
        live::RepairOutcome::CheckedAfterEarlierRepair
    );
    assert!(!finish.resumable);
    assert!(f.io.metadata(&path).unwrap().is_none());
    assert_eq!(
        (f.runner.count("bootout"), f.runner.count("bootstrap")),
        mutations
    );
}

#[test]
fn adapter_status_ids_increase_across_two_new_windows_on_a_retained_repair() {
    let f = producer_fixture();
    let mut original = repair_adapter(&f);
    let plan = repair_plan(&f, &mut original, 1, 2, 100);
    let pending = apply_repair(&f, &mut original, plan, 101);
    drop(pending);
    drop(original);
    // A partial payload keeps the reassessment unverifiable, so the record survives each window.
    remove_owned(&f.home.join(".local/bin/crosspanectl"));
    let mut last = 101;
    for _ in 0..2 {
        let server = one_status(&f);
        let finish = repairer(&f).resume(None, &f.deadline()).unwrap();
        let next = server.join().unwrap();
        assert!(next > last);
        last = next;
        assert_eq!(finish.outcome, live::RepairOutcome::RecoveryRetained);
        assert!(finish.resumable);
    }
    assert_eq!(f.runner.count("bootout"), 1);
    assert_eq!(f.runner.count("bootstrap"), 2);
}

#[test]
fn corrupt_record_is_not_offered_or_resumed_by_a_fresh_adapter() {
    let f = producer_fixture();
    let path = f.io.target().installer_dir().join("repair.json");
    bytes(&path, b"corrupt", 0o600);
    let mut fresh = repairer(&f);
    assert!(fresh.inspect(&f.deadline()).resumable.is_none());
    assert!(fresh.resume(None, &f.deadline()).is_err());
    assert_eq!(read_owned(&path), b"corrupt");
    assert_eq!(f.runner.count("bootout"), 0);
}

/// One whole native repair runs inside this stage's deadline; the 5 s fixture default is too
/// tight on a loaded shared Mac running the suite in parallel (it ends as "ran out of time").
fn stage_deadline(f: &Fixture) -> Deadline {
    Deadline::new(30_000, f.clock.clone(), Cancellation::default()).unwrap()
}

fn confirmed_health_wait(f: &Fixture, repair: &mut NativeRepairer) -> domains::RepairStep {
    let (_, plan_status) = current(f, 100);
    repair
        .plan(
            Some(&plan_status),
            crosspane_installer_core::OperationId(2),
            &stage_deadline(f),
        )
        .unwrap();
    let (_, consent_status) = current(f, 101);
    let server = one_status(f);
    let step = repair
        .confirm(
            Some(&consent_status),
            crosspane_installer_core::OperationId(2),
            crosspane_installer_core::OperationId(3),
            &stage_deadline(f),
        )
        .unwrap();
    assert_eq!(server.join().unwrap(), 0);
    step
}

#[test]
fn same_window_health_wait_is_durable_but_not_closeable_and_keeps_genuine_continuation() {
    let f = producer_fixture();
    let mut repair = repairer(&f);
    assert!(matches!(
        confirmed_health_wait(&f, &mut repair),
        domains::RepairStep::Waiting {
            closeable: false,
            ..
        }
    ));
    let path = f.io.target().installer_dir().join("repair.json");
    assert_eq!(record(&f, "repair.json")["step"], "health_wait");
    let server = one_status(&f);
    let finish = repair.resume(None, &stage_deadline(&f)).unwrap();
    assert!(server.join().unwrap() > (1 << 41));
    assert_eq!(
        finish.outcome,
        live::RepairOutcome::Verified,
        "same-window genuine pending tokens still verify the new instance"
    );
    assert!(f.io.metadata(&path).unwrap().is_none());
    assert_eq!(f.runner.count("bootout"), 1);
    assert_eq!(f.runner.count("bootstrap"), 2);
}

#[test]
fn corrupt_wait_record_is_never_replaced_and_keeps_the_same_window_waiting() {
    let f = producer_fixture();
    let mut repair = repairer(&f);
    assert!(matches!(
        confirmed_health_wait(&f, &mut repair),
        domains::RepairStep::Waiting {
            closeable: false,
            ..
        }
    ));
    let path = f.io.target().installer_dir().join("repair.json");
    bytes(&path, b"corrupt", 0o600);
    assert!(matches!(
        repair.verify(None, &stage_deadline(&f)),
        domains::RepairStep::Waiting {
            closeable: false,
            ..
        }
    ));
    assert_eq!(read_owned(&path), b"corrupt");
    assert_eq!(f.runner.count("bootout"), 1);
}

#[test]
fn stale_plan_and_confirm_in_a_fresh_window_never_overwrite_a_saved_record() {
    let (f, saved) = completed_with_saved_hint();
    let path = f.io.target().installer_dir().join("repair.json");
    let mutations = (f.runner.count("bootout"), f.runner.count("bootstrap"));
    let mut fresh = repairer(&f);
    let (_, status) = current(&f, 103);
    // A preview kept before the record appeared (another window) must not reach apply.
    fresh
        .plan(
            Some(&status),
            crosspane_installer_core::OperationId(4),
            &f.deadline(),
        )
        .unwrap();
    bytes(&path, &saved, 0o600);
    let (_, status) = current(&f, 104);
    assert!(
        fresh
            .confirm(
                Some(&status),
                crosspane_installer_core::OperationId(4),
                crosspane_installer_core::OperationId(5),
                &f.deadline()
            )
            .is_err()
    );
    let (_, status) = current(&f, 105);
    assert!(
        fresh
            .plan(
                Some(&status),
                crosspane_installer_core::OperationId(6),
                &f.deadline()
            )
            .is_err()
    );
    assert_eq!(read_owned(&path), saved);
    assert_eq!(
        (f.runner.count("bootout"), f.runner.count("bootstrap")),
        mutations
    );
}

#[test]
fn install_keeps_publication_across_managed_restart_admission_failure() {
    let f = Fixture::new(false);
    let mut installs = NativeInstalls::new(Arc::new(NativeEnv::new(
        f.io.target().clone(),
        inventory(),
        MacProbes {
            support: f.support.clone(),
            signatures: f.signatures.clone(),
            approval: Arc::new(ApprovalFixture(Approval::Allowed)),
            runner: f.runner.clone(),
        },
        f.clock.clone(),
    )));
    let op = crosspane_installer_core::OperationId(1);
    assert!(installs.plan(op, None, &f.deadline()).unwrap().is_some());
    assert_eq!(
        installs.apply(op, &f.deadline()).unwrap(),
        domains::InstallApplied::Requested
    );
    f.clock.0.store(100, Ordering::Release);
    let (_, old) = current(&f, 100);
    // Simulate the gap between AppKit termination and launchd's next managed process.
    f.runner.stopped.store(true, Ordering::Release);
    assert!(installs.verify(Some(&old), &f.deadline()).is_err());
    assert_eq!(record(&f, "payload.json")["phase"], "Published");
    f.runner.stopped.store(false, Ordering::Release);
    f.runner.pid.store(4244, Ordering::Release);
    f.runner.behavior.lock().unwrap().job_pid = 4244;
    f.bootstrap(3, 4244, 1000, "ready");
    let (_, fresh) = current(&f, 101);
    assert_eq!(
        installs.verify(Some(&fresh), &f.deadline()).unwrap(),
        ObservationSource::Demo
    );
    assert_eq!(record(&f, "payload.json")["phase"], "Verified");
    assert_eq!(f.runner.count("bootstrap"), 1);
    assert_eq!(f.runner.count("bootout"), 0);
}

/// WP-4.31b, the Mac loop: after its own apply, the install step's first Verify finds the new
/// agent not answering yet, the step is checked again from detection, and detection used to
/// report the fresh, still unconfirmed publication as an interrupted install to resume. Setup
/// then stopped the agent it had just started and published everything again, every few seconds.
/// The publication this run made is done; only its Verify finishes it. Another run (a fresh
/// adapter) still sees it as interrupted, as WP-4.30 wants.
#[test]
fn an_install_this_run_applied_is_detected_as_done_until_its_new_agent_confirms_it() {
    let f = Fixture::new(false);
    let env = || {
        Arc::new(NativeEnv::new(
            f.io.target().clone(),
            inventory(),
            MacProbes {
                support: f.support.clone(),
                signatures: f.signatures.clone(),
                approval: Arc::new(ApprovalFixture(Approval::Allowed)),
                runner: f.runner.clone(),
            },
            f.clock.clone(),
        ))
    };
    let mut installs = NativeInstalls::new(env());
    let op = crosspane_installer_core::OperationId(1);
    assert!(installs.plan(op, None, &f.deadline()).unwrap().is_some());
    assert_eq!(
        installs.apply(op, &f.deadline()).unwrap(),
        domains::InstallApplied::Requested
    );
    // The first Status after the bootstrap reaches no agent yet.
    assert!(installs.verify(None, &f.deadline()).is_err());
    // The new agent comes up.
    f.runner.pid.store(4244, Ordering::Release);
    f.runner.behavior.lock().unwrap().job_pid = 4244;
    f.bootstrap(3, 4244, 1000, "ready");
    f.clock.0.store(100, Ordering::Release);
    assert_eq!(record(&f, "payload.json")["phase"], "Published");
    // Checked again from detection: this run's own publication needs nothing more.
    let (_, status) = current(&f, 101);
    assert_eq!(
        installs.detect(Some(&status), &f.deadline()).unwrap(),
        domains::InstallState::Current
    );
    // And a plan asked for now has nothing to change.
    let (_, status) = current(&f, 102);
    assert!(
        installs
            .plan(
                crosspane_installer_core::OperationId(2),
                Some(&status),
                &f.deadline()
            )
            .unwrap()
            .is_none()
    );
    // Another installer run didn't make this publication: to it, it is an interrupted install.
    let (_, status) = current(&f, 103);
    assert_eq!(
        NativeInstalls::new(env())
            .detect(Some(&status), &f.deadline())
            .unwrap(),
        domains::InstallState::Needed
    );
    // This run's Verify confirms it against the new agent.
    let (_, status) = current(&f, 104);
    assert_eq!(
        installs.verify(Some(&status), &f.deadline()).unwrap(),
        ObservationSource::Demo
    );
    assert_eq!(record(&f, "payload.json")["phase"], "Verified");
    let (_, status) = current(&f, 105);
    assert_eq!(
        installs.detect(Some(&status), &f.deadline()).unwrap(),
        domains::InstallState::Current
    );
    // One start, and the agent it started was never stopped.
    assert_eq!(f.runner.count("bootstrap"), 1);
    assert_eq!(f.runner.count("bootout"), 0);
}

// ---- WP-4.32: setup owns its install paths ------------------------------------------------------

/// The Mac installer state captured at the owner's fifth stop (2026-10-05): a run killed while
/// waiting for the old agent's clean stop (launch-agent.json WaitingForCleanStop, op 45), the
/// earlier build's publication (payload.json Published, op 41), its inventory companion and
/// eight retained prior plists. Paths are rewritten from /Users/jame to the scratch home.
const CAPTURED_PAYLOAD: &str = r##"{"phase":"Published","receipt":{"schema_version":1,"operation_id":41,"product_version":"0.0.0","manifest_sha256":[242,107,59,40,71,190,73,25,1,51,30,44,7,170,108,132,50,180,58,90,233,247,168,189,133,227,47,56,250,85,21,172],"payload_sha256":[176,61,210,119,107,41,112,229,239,112,122,197,254,171,215,172,100,243,129,190,121,87,69,105,254,52,82,239,180,53,210,235],"resources":[{"resource_id":"mac.app","resolved_path":"/Users/jame/Applications/Crosspane.app","ownership":"Created","before":"Absent","after":"Unknown","outcome":"Unknown"},{"resource_id":"mac.ctl","resolved_path":"/Users/jame/.local/bin/crosspanectl","ownership":"Created","before":"Absent","after":"Unknown","outcome":"Unknown"}],"unfinished":[12]}}"##;
const CAPTURED_LAUNCH: &str = r##"{"phase":"WaitingForCleanStop","stop_attempted":true,"session":"501:jame","baseline":13878150831861781465,"prior":null,"receipt":{"schema_version":1,"operation_id":45,"product_version":"0.0.0","manifest_sha256":[242,107,59,40,71,190,73,25,1,51,30,44,7,170,108,132,50,180,58,90,233,247,168,189,133,227,47,56,250,85,21,172],"payload_sha256":[125,42,84,22,54,46,84,131,12,143,220,33,175,174,22,78,242,35,88,242,4,130,119,32,100,127,43,117,14,26,55,153],"resources":[{"resource_id":"mac.launch-agent","resolved_path":"/Users/jame/Library/LaunchAgents/io.frostdev.crosspane.agent.plist","ownership":"Created","before":"Absent","after":"Unknown","outcome":"Unknown"}],"unfinished":[12]}}"##;
const CAPTURED_INVENTORY: &str = r##"{"inventory":{"product_version":"0.0.0","features":["private-vdisplay","video"],"files":[{"path":"Crosspane.app/Contents/Frameworks/libopus.0.dylib","size":373920,"sha256":[27,240,151,95,196,47,147,97,171,169,243,101,62,64,183,86,147,94,170,38,55,195,164,118,174,149,136,109,239,26,207,245],"mode":493,"signing":{"role":"EmbeddedCode","identifier":"libopus.0","designated_requirement":"identifier \"libopus.0\" and anchor apple generic and certificate leaf[subject.CN] = \"Apple Development: Created via API (UMB4CJ832G)\" and certificate 1[field.1.2.840.113635.100.6.2.1] /* exists */","entitlements":{}}},{"path":"Crosspane.app/Contents/Info.plist","size":994,"sha256":[244,62,126,190,119,202,32,210,69,123,165,152,44,218,15,98,98,209,166,10,242,189,8,185,70,222,17,137,79,41,244,114],"mode":420,"signing":null},{"path":"Crosspane.app/Contents/MacOS/Crosspane","size":24180384,"sha256":[151,57,137,93,93,141,33,42,61,164,117,129,105,249,75,96,5,98,117,115,48,113,95,174,81,189,181,68,1,255,229,63],"mode":493,"signing":{"role":"Agent","identifier":"io.frostdev.crosspane.agent","designated_requirement":"identifier \"io.frostdev.crosspane.agent\" and anchor apple generic and certificate leaf[subject.CN] = \"Apple Development: Created via API (UMB4CJ832G)\" and certificate 1[field.1.2.840.113635.100.6.2.1] /* exists */","entitlements":{"com.apple.security.device.audio-input":true}}},{"path":"Crosspane.app/Contents/MacOS/crosspane-ui","size":14632656,"sha256":[43,124,153,227,147,169,14,177,240,165,134,7,143,255,177,103,27,35,160,152,108,122,53,13,254,121,166,171,167,163,217,233],"mode":493,"signing":{"role":"Settings","identifier":"crosspane-ui","designated_requirement":"identifier \"crosspane-ui\" and anchor apple generic and certificate leaf[subject.CN] = \"Apple Development: Created via API (UMB4CJ832G)\" and certificate 1[field.1.2.840.113635.100.6.2.1] /* exists */","entitlements":{}}},{"path":"Crosspane.app/Contents/Resources/audio/CrosspaneAudio-install-0.1.0.pkg","size":19954,"sha256":[47,216,103,102,79,172,156,115,129,193,186,250,248,225,181,127,14,254,12,207,8,236,9,162,190,14,85,207,44,102,246,110],"mode":420,"signing":null},{"path":"Crosspane.app/Contents/Resources/audio/CrosspaneAudio-remove-0.1.0.pkg","size":4000,"sha256":[135,29,70,19,244,172,245,66,17,159,39,195,210,95,101,190,20,164,70,199,30,90,61,60,206,10,174,110,109,1,220,38],"mode":420,"signing":null},{"path":"Crosspane.app/Contents/Resources/audio/packages.json","size":324,"sha256":[3,140,37,19,231,229,53,136,124,13,135,210,225,70,123,197,202,16,168,45,84,208,34,185,186,112,173,10,163,160,246,200],"mode":420,"signing":null},{"path":"Crosspane.app/Contents/_CodeSignature/CodeResources","size":4149,"sha256":[247,64,182,180,117,175,27,103,174,4,72,196,81,62,183,46,61,3,255,189,149,242,107,97,46,226,79,54,122,115,110,194],"mode":420,"signing":null},{"path":"crosspane-installer","size":17314432,"sha256":[207,41,72,155,152,85,213,142,162,195,68,111,182,180,250,124,71,140,65,245,214,196,155,247,248,147,223,53,95,114,123,63],"mode":493,"signing":{"role":"Installer","identifier":"io.frostdev.crosspane.installer","designated_requirement":"identifier \"io.frostdev.crosspane.installer\" and anchor apple generic and certificate leaf[subject.CN] = \"Apple Development: Created via API (UMB4CJ832G)\" and certificate 1[field.1.2.840.113635.100.6.2.1] /* exists */","entitlements":{}}},{"path":"crosspanectl","size":1630544,"sha256":[241,82,46,159,139,9,255,53,60,54,250,171,86,207,242,202,248,56,88,157,124,2,186,175,225,154,128,116,112,89,123,57],"mode":493,"signing":{"role":"Ctl","identifier":"io.frostdev.crosspane.ctl","designated_requirement":"identifier \"io.frostdev.crosspane.ctl\" and anchor apple generic and certificate leaf[subject.CN] = \"Apple Development: Created via API (UMB4CJ832G)\" and certificate 1[field.1.2.840.113635.100.6.2.1] /* exists */","entitlements":{}}}]}}"##;
const CAPTURED_INVENTORY_NAME: &str =
    "payload-inventory-f26b3b2847be491901331e2c07aa6c8432b43a5ae9f7a8bd85e32f38fa5515ac.json";
const CAPTURED_PRIORS: [u64; 8] = [8, 12, 17, 22, 26, 31, 36, 41];

fn fresh_installs(f: &Fixture) -> NativeInstalls {
    NativeInstalls::new(env_for(f, f.io.target().clone()))
}

fn env_for(f: &Fixture, target: MacTarget) -> Arc<NativeEnv> {
    Arc::new(NativeEnv::new(
        target,
        inventory(),
        MacProbes {
            support: f.support.clone(),
            signatures: f.signatures.clone(),
            approval: Arc::new(ApprovalFixture(Approval::Allowed)),
            runner: f.runner.clone(),
        },
        f.clock.clone(),
    ))
}

fn backups(f: &Fixture) -> Vec<PathBuf> {
    let root = f.io.target().backups_dir();
    let mut found: Vec<_> = fs::read_dir(&root)
        .map(|entries| entries.map(|e| e.unwrap().path()).collect())
        .unwrap_or_default();
    found.sort();
    found
}

/// The user's own data next to the installer's: never install state.
fn user_data(f: &Fixture) -> PathBuf {
    let path = f.io.target().state_dir().join("identity-fixture.json");
    bytes(&path, b"{\"keep\":true}", 0o600);
    path
}

/// Install with one fresh adapter (one installer run): detect, plan, apply.
fn install_run(f: &Fixture, status: Option<&AgentReply>) -> (NativeInstalls, bool) {
    let mut installs = fresh_installs(f);
    assert_eq!(
        installs.detect(status, &f.deadline()).unwrap(),
        domains::InstallState::Needed
    );
    let op = crosspane_installer_core::OperationId(1);
    let preview = installs.plan(op, status, &f.deadline()).unwrap().unwrap();
    assert_eq!(
        installs.apply(op, &f.deadline()).unwrap(),
        domains::InstallApplied::Requested
    );
    (installs, preview.replacing)
}

/// The new agent comes up and confirms the install; a later run has nothing to do.
fn confirm(f: &Fixture, installs: &mut NativeInstalls) {
    // Before the new agent answers, this run's own install is a wait, never a fresh start.
    assert_eq!(
        installs.detect(None, &f.deadline()),
        Err(domains::InstallError::Unavailable)
    );
    assert!(
        installs
            .plan(
                crosspane_installer_core::OperationId(7),
                None,
                &f.deadline()
            )
            .is_err()
    );
    f.runner.pid.store(4244, Ordering::Release);
    f.runner.stopped.store(false, Ordering::Release);
    f.runner.behavior.lock().unwrap().job_pid = 4244;
    f.bootstrap(3, 4244, 1000, "ready");
    f.clock.0.store(100, Ordering::Release);
    let (_, status) = current(f, 101);
    assert_eq!(
        installs.verify(Some(&status), &f.deadline()).unwrap(),
        ObservationSource::Demo
    );
    assert_eq!(record(f, "payload.json")["phase"], "Verified");
    let (_, status) = current(f, 102);
    assert_eq!(
        fresh_installs(f)
            .detect(Some(&status), &f.deadline())
            .unwrap(),
        domains::InstallState::Current
    );
    for file in inventory()
        .files
        .iter()
        .filter(|file| file.path != "crosspane-installer")
    {
        let path = if file.path == "crosspanectl" {
            f.home.join(".local/bin/crosspanectl")
        } else {
            f.home.join("Applications").join(&file.path)
        };
        assert_eq!(sha(&read_owned(&path)), file.sha256, "{}", file.path);
    }
    assert_eq!(
        read_owned(&launch_plist(f)),
        expected_plist(f.home.to_str().unwrap())
    );
}

fn captured_state(f: &Fixture) {
    let home = f.home.to_str().unwrap();
    let installer = f.io.target().installer_dir();
    bytes(
        &installer.join("payload.json"),
        CAPTURED_PAYLOAD.replace("/Users/jame", home).as_bytes(),
        0o600,
    );
    bytes(
        &installer.join("launch-agent.json"),
        CAPTURED_LAUNCH.replace("/Users/jame", home).as_bytes(),
        0o600,
    );
    bytes(
        &installer.join(CAPTURED_INVENTORY_NAME),
        CAPTURED_INVENTORY.as_bytes(),
        0o600,
    );
    for op in CAPTURED_PRIORS {
        bytes(
            &installer.join(format!("launch-agent-prior-{op}.plist")),
            &expected_plist(home),
            0o600,
        );
    }
    bytes(&installer.join("lock"), b"", 0o600);
    bytes(&launch_plist(f), &expected_plist(home), 0o644);
}

/// The owner's fifth stop: the captured state, with the old agent already booted out (as the
/// killed run left it) or still running and not answering. One run installs it, by itself.
#[test]
fn the_captured_stuck_mac_state_installs_by_itself() {
    for running in [false, true] {
        let f = Fixture::new(true);
        captured_state(&f);
        let keep = user_data(&f);
        if !running {
            f.runner.behavior.lock().unwrap().job_pid = 0;
            f.runner.stopped.store(true, Ordering::Release);
        }
        let old_ctl = read_owned(&f.home.join(".local/bin/crosspanectl"));
        let (mut installs, replacing) = install_run(&f, None);
        assert!(replacing);
        assert_eq!(f.runner.count("bootout"), usize::from(running));
        assert_eq!(f.runner.count("bootstrap"), 1);
        let saved = backups(&f);
        assert_eq!(saved.len(), 1);
        let folder = &saved[0];
        assert!(folder.join("Crosspane.app/Contents/Info.plist").exists());
        assert_eq!(read_owned(&folder.join("crosspanectl")), old_ctl);
        assert!(folder.join("io.frostdev.crosspane.agent.plist").exists());
        for name in ["payload.json", "launch-agent.json", CAPTURED_INVENTORY_NAME] {
            assert!(folder.join("Installer").join(name).exists(), "{name}");
        }
        for op in CAPTURED_PRIORS {
            assert!(
                folder
                    .join(format!("Installer/launch-agent-prior-{op}.plist"))
                    .exists()
            );
        }
        assert_eq!(
            installs.backup().as_deref(),
            Some(folder.as_path()),
            "the person is told where"
        );
        // The lock, the user's data and the payload source are untouched.
        assert!(f.io.target().installer_dir().join("lock").exists());
        assert_eq!(read_owned(&keep), b"{\"keep\":true}");
        assert!(f.source.join("Crosspane.app/Contents/Info.plist").exists());
        confirm(&f, &mut installs);
    }
}

/// Bytes setup never recorded at every install path, with the old agent running and
/// answering: saved, its sign-in item stopped, then replaced.
#[test]
fn unrecorded_bytes_at_every_install_path_are_saved_and_replaced() {
    let f = Fixture::new(true);
    bytes(&launch_plist(&f), b"<plist>hand-made</plist>", 0o644);
    let keep = user_data(&f);
    let old_agent = read_owned(&f.io.target().agent_path());
    let (_, status) = current(&f, 100);
    let (mut installs, _) = install_run(&f, Some(&status));
    assert_eq!(f.runner.count("bootout"), 1);
    let folder = &backups(&f)[0];
    assert_eq!(
        read_owned(&folder.join("Crosspane.app/Contents/MacOS/Crosspane")),
        old_agent
    );
    assert_eq!(
        read_owned(&folder.join("io.frostdev.crosspane.agent.plist")),
        b"<plist>hand-made</plist>"
    );
    assert_eq!(read_owned(&keep), b"{\"keep\":true}");
    confirm(&f, &mut installs);
}

/// A link at an install path is moved aside, never followed: what it points to stays as it is.
#[test]
fn links_at_install_paths_are_moved_aside_never_followed() {
    let f = Fixture::new(false);
    let outside = f.root.join("outside");
    bytes(
        &outside.join("Crosspane.app/Contents/Info.plist"),
        b"elsewhere",
        0o644,
    );
    bytes(&outside.join("crosspanectl"), b"elsewhere-ctl", 0o755);
    directory(&f.home.join("Applications"));
    directory(&f.home.join(".local/bin"));
    symlink_owned(&outside.join("Crosspane.app"), &f.io.target().app_path());
    symlink_owned(
        &outside.join("crosspanectl"),
        &f.home.join(".local/bin/crosspanectl"),
    );
    let (mut installs, _) = install_run(&f, None);
    let folder = &backups(&f)[0];
    for name in ["Crosspane.app", "crosspanectl"] {
        let stat = owned_stat(&folder.join(name));
        assert_eq!(stat.st_mode & 0o170000, 0o120000, "{name}");
    }
    assert_eq!(
        read_owned(&outside.join("Crosspane.app/Contents/Info.plist")),
        b"elsewhere"
    );
    assert_eq!(read_owned(&outside.join("crosspanectl")), b"elsewhere-ctl");
    confirm(&f, &mut installs);
}

/// A run killed at points throughout publication: the next run ends installed.
#[test]
fn a_run_killed_anywhere_in_publication_converges() {
    for kill_at in [1usize, 4, 12, 30, 60, 120, 200] {
        let f = Fixture::new(false);
        let calls = Arc::new(AtomicU64::new(0));
        let dead = Arc::new(AtomicBool::new(false));
        let mut target = f.io.target().clone();
        let (counter, stop) = (calls.clone(), dead.clone());
        let previous = target.test_hook.clone();
        target.test_hook = Some(Arc::new(move |stage, path, identity| {
            if stop.load(Ordering::Acquire) {
                return Err(NativeError::OutcomeUnknown);
            }
            if !matches!(stage, "walk" | "metadata" | "open-before" | "fd-stat")
                && counter.fetch_add(1, Ordering::AcqRel) + 1 == kill_at as u64
            {
                stop.store(true, Ordering::Release);
                return Err(NativeError::OutcomeUnknown);
            }
            match &previous {
                Some(hook) => hook(stage, path, identity),
                None => Ok(identity),
            }
        }));
        let mut killed = NativeInstalls::new(env_for(&f, target));
        let op = crosspane_installer_core::OperationId(1);
        if killed.plan(op, None, &f.deadline()).is_ok() {
            let _ = killed.apply(op, &f.deadline());
        }
        drop(killed);
        let killed_mid_run = dead.load(Ordering::Acquire);
        dead.store(false, Ordering::Release);
        // The next run: no agent is assumed to answer.
        let mut installs = fresh_installs(&f);
        if installs.detect(None, &f.deadline()).unwrap() == domains::InstallState::Needed {
            let op = crosspane_installer_core::OperationId(2);
            installs.plan(op, None, &f.deadline()).unwrap().unwrap();
            assert_eq!(
                installs.apply(op, &f.deadline()).unwrap(),
                domains::InstallApplied::Requested,
                "kill at {kill_at}"
            );
        } else {
            assert!(!killed_mid_run, "kill at {kill_at}");
            continue;
        }
        confirm(&f, &mut installs);
    }
}

/// An ordinary update whose old agent never reports a clean exit: its sign-in item is booted
/// out once more and the install continues fresh, within the same apply.
#[test]
fn an_update_without_a_clean_exit_report_continues_fresh() {
    let f = Fixture::new(true);
    f.runner.behavior.lock().unwrap().bootout = 2;
    let (_, status) = current(&f, 100);
    let (mut installs, _) = install_run(&f, Some(&status));
    assert!(installs.backup().is_some());
    confirm(&f, &mut installs);
}

/// Twice in a row converges; at most three backup folders stay.
#[test]
fn repeated_runs_converge_and_keep_three_backups() {
    let f = Fixture::new(true);
    let root = f.io.target().backups_dir();
    for old in ["20200101-000000", "20210101-000000", "20220101-000000"] {
        bytes(&root.join(old).join("note"), b"old", 0o600);
    }
    let (_, status) = current(&f, 100);
    let (mut installs, _) = install_run(&f, Some(&status));
    confirm(&f, &mut installs);
    let kept = backups(&f);
    assert_eq!(kept.len(), KEEP_BACKUPS);
    assert!(!root.join("20200101-000000").exists());
    assert!(root.join("20220101-000000/note").exists());
}
