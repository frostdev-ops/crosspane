#![allow(dead_code, unused_imports, clippy::unwrap_used, clippy::expect_used)]
//! Fake process/instance/job/receipt observations. Never launches a Windows process.
use crosspane_installer::agent_contract;
#[path = "../src/platform/windows/detect.rs"]
mod detect;
#[path = "../src/platform/windows/native_io.rs"]
mod native_io;
#[path = "../src/platform/windows/service.rs"]
mod service;
#[path = "../src/platform/windows/transport.rs"]
mod transport;
use agent_contract::{CallFailure, DecodedReply, InstallerRequest, ObservationSource};
use native_io::NativeError;
use native_io::{Cancellation, Clock, Deadline, NativeResult};
use service::journal::{Journal, Phase};
use service::supervisor::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

fn generation() -> Generation {
    Generation {
        pid: 101,
        creation: 1001,
        instance: 5001,
    }
}
fn next() -> Generation {
    Generation {
        pid: 102,
        creation: 1002,
        instance: 5002,
    }
}
fn supervisor(now: u64) -> Supervisor {
    Supervisor::new(generation(), now, Vec::new()).unwrap()
}
fn exit(code: u32, clean: bool) -> Original {
    Original::Exited {
        code,
        receipt: Some(Receipt {
            instance: generation().instance,
            clean,
        }),
    }
}

#[test]
fn original_running_never_means_completed_stop() {
    assert_eq!(
        supervisor(0).observe(1, Original::Running, Tree::Empty),
        Decision::Observe
    );
}
#[test]
fn normal_clean_quit_with_empty_own_job_finishes_without_restart() {
    assert_eq!(
        supervisor(0).observe(1, exit(0, true), Tree::Empty),
        Decision::Finished
    );
}
#[test]
fn missing_receipt_is_retained_not_a_clean_quit_or_guessed_crash() {
    assert_eq!(
        supervisor(0).observe(
            1,
            Original::Exited {
                code: 0,
                receipt: None
            },
            Tree::Empty
        ),
        Decision::RecoveryRetained
    );
}
#[test]
fn unclean_zero_exit_never_finishes_cleanly() {
    assert_eq!(
        supervisor(0).observe(1, exit(0, false), Tree::Empty),
        Decision::RecoveryRetained
    );
}
#[test]
fn stale_instance_receipt_never_completes_stop() {
    let old = Original::Exited {
        code: 0,
        receipt: Some(Receipt {
            instance: 9999,
            clean: true,
        }),
    };
    assert_eq!(
        supervisor(0).observe(1, old, Tree::Empty),
        Decision::RecoveryRetained
    );
}
#[test]
fn matching_stop_latches_before_rpc_submission() {
    let mut s = supervisor(0);
    assert_eq!(
        s.stop(generation().instance),
        Ok(Decision::SubmitStop {
            expected_instance: generation().instance
        })
    );
    assert!(s.stop_latched());
}
#[test]
fn stale_stop_refuses_without_latching_or_dispatching() {
    let mut s = supervisor(0);
    assert_eq!(s.stop(9999), Err(NativeError::Foreign));
    assert!(!s.stop_latched());
}
#[test]
fn repeated_stop_does_not_submit_twice() {
    let mut s = supervisor(0);
    s.stop(generation().instance).unwrap();
    assert_eq!(s.stop(generation().instance), Ok(Decision::Observe));
}
#[test]
fn lost_ack_and_nonzero_exit_do_not_clear_stop_latch() {
    let mut s = supervisor(0);
    s.stop(generation().instance).unwrap();
    assert_eq!(
        s.observe(1, exit(1, false), Tree::Empty),
        Decision::RecoveryRetained
    );
    assert!(s.stop_latched());
    assert_eq!(s.start_once(), Decision::RecoveryRetained);
}
#[test]
fn stopped_clean_original_requires_job_empty_not_just_ack() {
    let mut s = supervisor(0);
    s.stop(generation().instance).unwrap();
    assert_eq!(
        s.observe(1, exit(0, true), Tree::Uncertain),
        Decision::RecoveryRetained
    );
}
#[test]
fn self_restart_follows_only_new_authenticated_owned_child() {
    let child = next();
    let mut s = supervisor(0);
    assert_eq!(
        s.observe(
            1,
            exit(0, true),
            Tree::Replacement {
                child,
                admitted: true
            }
        ),
        Decision::Follow(child)
    );
    assert!(s.restart_times().is_empty());
    assert_eq!(
        s.stop(child.instance),
        Ok(Decision::SubmitStop {
            expected_instance: child.instance
        })
    );
}
#[test]
fn unadmitted_descendant_never_becomes_replacement_authority() {
    assert_eq!(
        supervisor(0).observe(
            1,
            exit(0, true),
            Tree::Replacement {
                child: next(),
                admitted: false
            }
        ),
        Decision::RecoveryRetained
    );
}
#[test]
fn replacement_cannot_reuse_instance_or_creation_identity() {
    for child in [
        generation(),
        Generation {
            instance: generation().instance,
            ..next()
        },
        Generation {
            creation: generation().creation,
            ..next()
        },
    ] {
        assert_eq!(
            supervisor(0).observe(
                1,
                exit(0, true),
                Tree::Replacement {
                    child,
                    admitted: true
                }
            ),
            Decision::RecoveryRetained
        );
    }
}
#[test]
fn a_job_notification_without_accounting_never_proves_empty() {
    assert_eq!(
        supervisor(0).observe(1, exit(1, false), Tree::Uncertain),
        Decision::RecoveryRetained
    );
}
#[test]
fn known_crash_waits_five_seconds_before_one_restart() {
    let mut s = supervisor(0);
    assert_eq!(
        s.observe(10, exit(1, false), Tree::Empty),
        Decision::Backoff { until_ms: 5010 }
    );
    assert_eq!(
        s.observe(5009, exit(1, false), Tree::Empty),
        Decision::Backoff { until_ms: 5010 }
    );
    assert_eq!(
        s.observe(5010, exit(1, false), Tree::Empty),
        Decision::Start
    );
    assert_eq!(s.restart_times(), [5010]);
    assert_eq!(
        s.observe(5011, exit(1, false), Tree::Empty),
        Decision::Observe
    );
}
#[test]
fn fourth_restart_in_ten_minutes_is_refused_after_reopen() {
    let mut s = Supervisor::new(generation(), 3000, vec![1, 1000, 2000]).unwrap();
    assert_eq!(
        s.observe(3000, exit(1, false), Tree::Empty),
        Decision::RecoveryRetained
    );
}
#[test]
fn old_restart_entries_expire_only_after_full_window() {
    let mut s = Supervisor::new(generation(), WINDOW_MS, vec![1, 1000, 2000]).unwrap();
    assert_eq!(
        s.observe(WINDOW_MS, exit(1, false), Tree::Empty),
        Decision::RecoveryRetained
    );
    let mut s = Supervisor::new(generation(), WINDOW_MS + 1, vec![1, 1000, 2000]).unwrap();
    assert_eq!(
        s.observe(WINDOW_MS + 1, exit(1, false), Tree::Empty),
        Decision::Backoff {
            until_ms: WINDOW_MS + 5001
        }
    );
}
#[test]
fn stop_arriving_during_backoff_cancels_restart_permanently() {
    let mut s = supervisor(0);
    s.observe(10, exit(1, false), Tree::Empty);
    s.stop(generation().instance).unwrap();
    assert_eq!(
        s.observe(5010, exit(1, false), Tree::Empty),
        Decision::RecoveryRetained
    );
    assert!(s.restart_times().is_empty());
}
#[test]
fn one_start_per_operation_never_replays() {
    let mut s = supervisor(0);
    assert_eq!(s.start_once(), Decision::Start);
    assert_eq!(s.start_once(), Decision::Observe);
}
#[test]
fn uncertain_dispatch_retires_before_later_start() {
    let mut s = supervisor(0);
    assert_eq!(s.start_once(), Decision::Start);
    s.ambiguous();
    assert_eq!(s.start_once(), Decision::RecoveryRetained);
}
#[test]
fn monotonic_regression_retains_instead_of_resetting_restart_budget() {
    let mut s = supervisor(1000);
    assert_eq!(
        s.observe(999, exit(1, false), Tree::Empty),
        Decision::RecoveryRetained
    );
    assert_eq!(s.start_once(), Decision::RecoveryRetained);
}
#[test]
fn malformed_retained_restart_history_refuses() {
    assert!(matches!(
        Supervisor::new(generation(), 100, vec![101]),
        Err(NativeError::Invalid)
    ));
    assert!(matches!(
        Supervisor::new(generation(), 100, vec![20, 10]),
        Err(NativeError::Invalid)
    ));
    assert!(matches!(
        Supervisor::new(generation(), 100, vec![1, 2, 3, 4]),
        Err(NativeError::Invalid)
    ));
}

#[derive(Default)]
struct FakeClock(AtomicU64);
impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}
struct StopEndpoint {
    calls: Mutex<Vec<&'static str>>,
    current: u64,
    admit: bool,
    caller: bool,
    started: Option<mpsc::SyncSender<()>>,
    blocked: Mutex<Option<mpsc::Receiver<()>>>,
}
impl StopEndpoint {
    fn new() -> Self {
        Self {
            calls: Mutex::default(),
            current: 5001,
            admit: true,
            caller: true,
            started: None,
            blocked: Mutex::default(),
        }
    }
    fn event(&self, event: &'static str) {
        self.calls.lock().unwrap().push(event);
    }
}
impl transport::Endpoint for StopEndpoint {
    fn admit_caller(&self) -> NativeResult<()> {
        self.event("caller");
        if self.caller {
            Ok(())
        } else {
            Err(NativeError::Foreign)
        }
    }
    fn check(&self, deadline: &Deadline) -> NativeResult<()> {
        self.event("before");
        deadline.check()
    }
    fn frame(&self, request: &InstallerRequest, _: &Deadline) -> Result<DecodedReply, CallFailure> {
        assert!(
            matches!(request, InstallerRequest::Status),
            "terminal stop must not send ordinary actions"
        );
        self.event("status");
        Ok(DecodedReply::Acknowledged)
    }
    fn admit_status(&self, _: &DecodedReply) -> Result<(), CallFailure> {
        self.event("admit");
        if self.admit {
            Ok(())
        } else {
            Err(CallFailure::Unavailable)
        }
    }
    fn source(&self) -> ObservationSource {
        ObservationSource::Live
    }
    fn selected_instance(&self) -> NativeResult<u64> {
        Ok(self.current)
    }
    fn stop_frame(&self, expected: u64, deadline: &Deadline) -> Result<(), CallFailure> {
        assert_eq!(expected, self.current);
        self.event("stop");
        if let Some(started) = &self.started {
            started.send(()).unwrap();
        }
        if let Some(blocked) = self.blocked.lock().unwrap().take() {
            blocked.recv().unwrap();
        }
        deadline.check().map_err(transport::failure)?;
        Ok(())
    }
    fn check_after_stop(&self, deadline: &Deadline) -> NativeResult<()> {
        self.event("terminal-origin");
        deadline.check()
    }
}
fn stop_deadline() -> Deadline {
    Deadline::new(
        1000,
        Arc::new(FakeClock::default()),
        Cancellation::default(),
    )
    .unwrap()
}

#[test]
fn stop_codec_encodes_only_frozen_expected_instance_request() {
    assert_eq!(
        agent_contract::encode_installer_stop(5001).unwrap(),
        b"{\"cmd\":\"installer_stop\",\"expected_instance\":5001}\n"
    );
    assert!(agent_contract::encode_installer_stop(0).is_err());
}
#[test]
fn stop_codec_requires_actual_stopping_result_not_a_generic_ok() {
    assert!(
        agent_contract::decode_installer_stop(b"{\"ok\":true,\"result\":\"stopping\"}\n").is_ok()
    );
    for other in [
        b"{\"ok\":true}".as_slice(),
        b"{\"ok\":true,\"result\":\"restarting\"}",
        b"{\"ok\":true,\"result\":null}",
        b"{\"ok\":true,\"result\":\"stopping\",\"error\":null}",
    ] {
        assert!(matches!(
            agent_contract::decode_installer_stop(other),
            Err(CallFailure::InvalidResponse)
        ));
    }
}
#[test]
fn stop_codec_retains_bounded_duplicate_key_and_stale_refusal_rules() {
    assert!(matches!(
        agent_contract::decode_installer_stop(b"{\"ok\":true,\"ok\":true,\"result\":\"stopping\"}"),
        Err(CallFailure::InvalidResponse)
    ));
    assert!(matches!(
        agent_contract::decode_installer_stop(b"{\"ok\":false,\"error\":\"stale_instance\"}"),
        Err(CallFailure::Refused(_))
    ));
}
#[test]
fn terminal_stop_ack_is_not_followed_by_a_status_health_query() {
    let endpoint = StopEndpoint::new();
    assert_eq!(
        transport::run_stop(&endpoint, 5001, &stop_deadline()),
        Ok(transport::StopAcknowledgement::Stopping)
    );
    let calls = endpoint.calls.lock().unwrap();
    assert_eq!(calls.iter().filter(|call| **call == "status").count(), 1);
    assert_eq!(calls.iter().filter(|call| **call == "stop").count(), 1);
    assert_eq!(calls.last(), Some(&"terminal-origin"));
}
#[test]
fn terminal_stop_stale_instance_and_failed_admission_submit_nothing() {
    let endpoint = StopEndpoint::new();
    assert!(transport::run_stop(&endpoint, 9999, &stop_deadline()).is_err());
    assert!(!endpoint.calls.lock().unwrap().contains(&"stop"));
    let mut endpoint = StopEndpoint::new();
    endpoint.admit = false;
    assert!(transport::run_stop(&endpoint, 5001, &stop_deadline()).is_err());
    assert!(!endpoint.calls.lock().unwrap().contains(&"stop"));
}

// Only cases constructing the shared A2 worker are serialized. No counter is reset or evicted.
static STOP_WORKER: Mutex<()> = Mutex::new(());
#[test]
fn consuming_stop_checks_actual_caller_and_uses_existing_worker() {
    let _serial = STOP_WORKER
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let endpoint = Arc::new(StopEndpoint::new());
    let port = transport::WindowsAgentPort::fake(endpoint.clone(), Arc::new(FakeClock::default()))
        .unwrap();
    assert_eq!(
        port.installer_stop(5001, 1000),
        Ok(transport::StopAcknowledgement::Stopping)
    );
    assert_eq!(endpoint.calls.lock().unwrap().first(), Some(&"caller"));
    let mut denied = StopEndpoint::new();
    denied.caller = false;
    let denied = Arc::new(denied);
    let port =
        transport::WindowsAgentPort::fake(denied.clone(), Arc::new(FakeClock::default())).unwrap();
    assert!(port.installer_stop(5001, 1000).is_err());
    assert!(!denied.calls.lock().unwrap().contains(&"stop"));
}
#[test]
fn stop_caller_timeout_keeps_blocked_endpoint_and_same_worker_owned() {
    let _serial = STOP_WORKER
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let (started_send, started_receive) = mpsc::sync_channel(1);
    let (release, wait) = mpsc::sync_channel(1);
    let mut endpoint = StopEndpoint::new();
    endpoint.started = Some(started_send);
    *endpoint.blocked.lock().unwrap() = Some(wait);
    let endpoint = Arc::new(endpoint);
    let weak = Arc::downgrade(&endpoint);
    let port = transport::WindowsAgentPort::fake(endpoint.clone(), Arc::new(FakeClock::default()))
        .unwrap();
    drop(endpoint);
    let (done, completed) = mpsc::sync_channel(1);
    let caller = std::thread::spawn(move || {
        let result = port.installer_stop(5001, 100);
        let _ = done.send(result);
    });
    let entered = started_receive.recv_timeout(Duration::from_secs(1)).is_ok();
    let result = completed.recv_timeout(Duration::from_secs(2));
    let retained_before_release = weak.upgrade().is_some();
    let _ = release.send(());
    caller.join().unwrap();
    assert!(
        entered,
        "the production worker must enter the actual stop sequence"
    );
    assert!(retained_before_release);
    assert_eq!(result.unwrap(), Err(CallFailure::TimeoutOutcomeUnknown));
    let limit = Instant::now() + Duration::from_secs(1);
    while weak.upgrade().is_some() && Instant::now() < limit {
        std::thread::yield_now();
    }
    assert!(weak.upgrade().is_none());
}

fn journal() -> Journal {
    Journal {
        schema_version: 1,
        registration: [1; 16],
        operation: [2; 16],
        user: "S-1-5-21-101".into(),
        phase: Phase::Running,
        current: Some(generation()),
        stop_instance: None,
        original_xml: None,
        restart_times: vec![1, 2],
        last_tick_ms: 100,
        clock_epoch: 7001,
    }
}
#[test]
fn fixed_supervisor_record_roundtrips_without_path_or_executable_authority() {
    let expected = journal();
    let bytes = expected.encode().unwrap();
    assert_eq!(Journal::decode(&bytes).unwrap(), expected);
    assert_eq!(
        native_io::records::RecordName::Supervisor
            .file_name()
            .unwrap()
            .as_str(),
        "supervisor.json"
    );
}
#[test]
fn record_context_or_clock_domain_mismatch_refuses_before_resume() {
    let record = journal();
    assert_eq!(record.bind([1; 16], [2; 16], "S-1-5-21-101", 7001), Ok(()));
    for (registration, operation, user, clock) in [
        ([3; 16], [2; 16], "S-1-5-21-101", 7001),
        ([1; 16], [3; 16], "S-1-5-21-101", 7001),
        ([1; 16], [2; 16], "S-1-5-21-202", 7001),
        ([1; 16], [2; 16], "S-1-5-21-101", 7002),
    ] {
        assert_eq!(
            record.bind(registration, operation, user, clock),
            Err(NativeError::Foreign)
        );
    }
}
#[test]
fn malformed_unknown_and_duplicate_supervisor_fields_refuse() {
    assert!(Journal::decode(b"{").is_err());
    let mut value = serde_json::to_value(journal()).unwrap();
    value["executable"] = serde_json::json!("C:\\foreign.exe");
    let bytes =
        native_io::records::encode_record(&native_io::records::RecordName::Supervisor, value)
            .unwrap();
    assert!(Journal::decode(&bytes).is_err());
    let bytes = journal().encode().unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let duplicate = text.replace(
        "\"schema_version\":1",
        "\"schema_version\":1,\"schema_version\":1",
    );
    assert!(Journal::decode(duplicate.as_bytes()).is_err());
    let duplicate_body = text.replace(
        "\"clock_epoch\":7001",
        "\"clock_epoch\":7001,\"clock_epoch\":7001",
    );
    assert!(Journal::decode(duplicate_body.as_bytes()).is_err());
}
#[test]
fn prior_start_requested_never_replays_in_a_fresh_window() {
    let mut record = journal();
    record.phase = Phase::StartRequested;
    let mut restored = Journal::decode(&record.encode().unwrap())
        .unwrap()
        .restore_model(generation(), 100)
        .unwrap();
    assert_eq!(restored.start_once(), Decision::Observe);
}
#[test]
fn persisted_stop_intent_reopens_as_inhibition_not_another_rpc() {
    let mut record = journal();
    record.phase = Phase::StopIntent;
    record.stop_instance = Some(generation().instance);
    let mut restored = Journal::decode(&record.encode().unwrap())
        .unwrap()
        .restore_model(generation(), 100)
        .unwrap();
    assert!(restored.stop_latched());
    assert_eq!(restored.stop(generation().instance), Ok(Decision::Observe));
    assert_eq!(restored.start_once(), Decision::RecoveryRetained);
}
#[test]
fn recorded_restart_budget_survives_reopen_without_eviction() {
    let mut record = journal();
    record.restart_times = vec![1, 2, 3];
    let mut restored = Journal::decode(&record.encode().unwrap())
        .unwrap()
        .restore_model(generation(), 100)
        .unwrap();
    assert_eq!(
        restored.observe(100, exit(1, false), Tree::Empty),
        Decision::RecoveryRetained
    );
}
#[test]
fn fresh_replacement_or_regressed_clock_cannot_reuse_old_record_admission() {
    let record = journal();
    assert!(record.restore_model(next(), 100).is_err());
    assert!(record.restore_model(generation(), 99).is_err());
}
#[test]
fn unknown_record_stays_retired_even_after_fresh_process_admission() {
    let mut record = journal();
    record.phase = Phase::Unknown;
    let mut restored = Journal::decode(&record.encode().unwrap())
        .unwrap()
        .restore_model(generation(), 100)
        .unwrap();
    assert_eq!(restored.start_once(), Decision::RecoveryRetained);
}
#[test]
fn empty_binding_and_inconsistent_stop_phase_refuse_record() {
    let mut record = journal();
    record.registration = [0; 16];
    assert!(Journal::decode(&record.encode().unwrap()).is_err());
    let mut record = journal();
    record.phase = Phase::StopIntent;
    record.stop_instance = None;
    assert!(Journal::decode(&record.encode().unwrap()).is_err());
}
#[test]
fn original_xml_overflow_refuses_instead_of_truncating_backup() {
    let mut record = journal();
    record.original_xml = Some("x".repeat(service::task::MAX_XML_BYTES + 1));
    assert!(Journal::decode(&record.encode().unwrap()).is_err());
}

#[derive(Default)]
struct Startup {
    events: Vec<&'static str>,
    fail: Option<(&'static str, NativeError)>,
    cleanup_failure: bool,
}
impl Startup {
    fn event(&mut self, event: &'static str) -> NativeResult<()> {
        self.events.push(event);
        match self.fail {
            Some((stage, error)) if stage == event => Err(error),
            _ => Ok(()),
        }
    }
}
impl StartupPort for Startup {
    type Child = u64;
    fn create_suspended(&mut self) -> NativeResult<u64> {
        self.event("suspended")?;
        Ok(71)
    }
    fn assign(&mut self, child: &u64) -> NativeResult<()> {
        assert_eq!(*child, 71);
        self.event("assigned")
    }
    fn resume(&mut self, child: &u64) -> NativeResult<()> {
        assert_eq!(*child, 71);
        self.event("resumed")
    }
    fn cleanup_never_resumed(&mut self, child: u64) -> NativeResult<()> {
        assert_eq!(child, 71);
        self.events.push("cleanup-exact-never-resumed");
        if self.cleanup_failure {
            Err(NativeError::Unavailable)
        } else {
            Ok(())
        }
    }
}
#[test]
fn startup_assigns_owned_suspended_child_before_resume() {
    let mut port = Startup::default();
    assert_eq!(startup(&mut port), Ok(()));
    assert_eq!(port.events, ["suspended", "assigned", "resumed"]);
}
#[test]
fn failed_assignment_never_resumes_and_cleans_only_exact_owned_child() {
    let mut port = Startup {
        fail: Some(("assigned", NativeError::Unavailable)),
        ..Default::default()
    };
    assert_eq!(startup(&mut port), Err(NativeError::Unavailable));
    assert_eq!(
        port.events,
        ["suspended", "assigned", "cleanup-exact-never-resumed"]
    );
}
#[test]
fn definite_resume_failure_cleans_only_never_resumed_child() {
    let mut port = Startup {
        fail: Some(("resumed", NativeError::Unavailable)),
        ..Default::default()
    };
    assert_eq!(startup(&mut port), Err(NativeError::Unavailable));
    assert_eq!(
        port.events,
        [
            "suspended",
            "assigned",
            "resumed",
            "cleanup-exact-never-resumed"
        ]
    );
}
#[test]
fn ambiguous_resume_never_uses_never_resumed_cleanup_or_force() {
    let mut port = Startup {
        fail: Some(("resumed", NativeError::OutcomeUnknown)),
        ..Default::default()
    };
    assert_eq!(startup(&mut port), Err(NativeError::OutcomeUnknown));
    assert_eq!(port.events, ["suspended", "assigned", "resumed"]);
}
#[test]
fn unverified_startup_cleanup_remains_unknown() {
    let mut port = Startup {
        fail: Some(("assigned", NativeError::Unavailable)),
        cleanup_failure: true,
        ..Default::default()
    };
    assert_eq!(startup(&mut port), Err(NativeError::OutcomeUnknown));
}
#[test]
fn supervisor_entry_mode_is_sole_fixed_argv_without_gui_or_path_options() {
    use std::ffi::OsString;
    assert_eq!(
        service::supervisor_mode(&[OsString::from("--windows-supervisor")]),
        Ok(true)
    );
    assert_eq!(service::supervisor_mode(&[]), Ok(false));
    for extra in [
        "--diagnose",
        "--payload",
        "C:\\foreign.exe",
        "--windows-supervisor",
    ] {
        assert_eq!(
            service::supervisor_mode(&[
                OsString::from("--windows-supervisor"),
                OsString::from(extra)
            ]),
            Err(NativeError::Invalid)
        );
    }
}

struct StopOrder {
    events: Vec<&'static str>,
    failure: Option<&'static str>,
}
impl StopPort for StopOrder {
    fn persist_stop(&mut self, _: u64) -> NativeResult<()> {
        self.events.push("durable-stop-intent");
        if self.failure == Some("persist") {
            Err(NativeError::OutcomeUnknown)
        } else {
            Ok(())
        }
    }
    fn submit_stop(&mut self, _: u64) -> NativeResult<()> {
        self.events.push("terminal-stop-submission");
        if self.failure == Some("submit") {
            Err(NativeError::OutcomeUnknown)
        } else {
            Ok(())
        }
    }
}
#[test]
fn durable_stop_intent_precedes_one_rpc_and_latch_survives_lost_ack() {
    let mut model = supervisor(0);
    let mut port = StopOrder {
        events: vec![],
        failure: Some("submit"),
    };
    assert_eq!(
        request_stop(&mut model, &mut port, 5001),
        Err(NativeError::OutcomeUnknown)
    );
    assert!(model.stop_latched());
    assert_eq!(
        port.events,
        ["durable-stop-intent", "terminal-stop-submission"]
    );
    assert_eq!(request_stop(&mut model, &mut port, 5001), Ok(()));
    assert_eq!(port.events.len(), 2);
}
#[test]
fn uncertain_stop_publication_submits_nothing_and_never_clears_latch() {
    let mut model = supervisor(0);
    let mut port = StopOrder {
        events: vec![],
        failure: Some("persist"),
    };
    assert_eq!(
        request_stop(&mut model, &mut port, 5001),
        Err(NativeError::OutcomeUnknown)
    );
    assert_eq!(port.events, ["durable-stop-intent"]);
    assert!(model.stop_latched());
    assert_eq!(model.start_once(), Decision::RecoveryRetained);
}
#[test]
fn stale_stop_persists_and_submits_nothing() {
    let mut model = supervisor(0);
    let mut port = StopOrder {
        events: vec![],
        failure: None,
    };
    assert_eq!(
        request_stop(&mut model, &mut port, 9999),
        Err(NativeError::Foreign)
    );
    assert!(port.events.is_empty());
    assert!(!model.stop_latched());
}

#[test]
fn running_record_reopen_prevents_initial_start_but_keeps_crash_backoff() {
    let record = journal();
    let mut model = Journal::decode(&record.encode().unwrap())
        .unwrap()
        .restore_model(generation(), 100)
        .unwrap();
    assert_eq!(model.start_once(), Decision::Observe);
    assert_eq!(model.restart_times(), [1, 2]);
    assert_eq!(
        model.observe(100, exit(1, false), Tree::Empty),
        Decision::Backoff { until_ms: 5100 }
    );
    assert_eq!(
        model.observe(5100, exit(1, false), Tree::Empty),
        Decision::Start
    );
    assert_eq!(model.start_once(), Decision::Observe);
    assert_eq!(
        model.observe(5101, exit(1, false), Tree::Empty),
        Decision::Observe
    );
    assert_eq!(model.restart_times(), [1, 2, 5100]);
}
