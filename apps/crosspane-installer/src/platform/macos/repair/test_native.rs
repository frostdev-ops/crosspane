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
