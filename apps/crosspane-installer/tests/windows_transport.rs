#![allow(dead_code, unused_imports, clippy::unwrap_used, clippy::expect_used)]
//! Owned in-memory endpoint tests. Native connection code is never entered here.
use crosspane_installer::agent_contract;
#[path = "../src/platform/windows/detect.rs"]
mod detect;
#[path = "../src/platform/windows/native_io.rs"]
mod native_io;
#[path = "../src/platform/windows/transport.rs"]
mod transport;
use agent_contract::*;
use native_io::{Cancellation, Clock, Deadline, NativeError, NativeResult};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use transport::{Endpoint, WindowsAgentPort, security::Response};
// The real worker budget is process-wide. Serialize only fixtures constructing ports; pure
// comparison/framing tests remain concurrent. Never force/reset the production slot counter.
static PORT_CASE: Mutex<()> = Mutex::new(());

#[derive(Default)]
struct FakeClock(AtomicU64);
impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}
struct Fake {
    pending: bool,
    admit: bool,
    frames: Mutex<Vec<InstallerRequest>>,
    result: Mutex<Option<CallFailure>>,
}
impl Fake {
    fn new(pending: bool, admit: bool) -> Self {
        Self {
            pending,
            admit,
            frames: Mutex::default(),
            result: Mutex::default(),
        }
    }
}
impl Endpoint for Fake {
    fn check(&self, deadline: &Deadline) -> NativeResult<()> {
        deadline.check()
    }
    fn frame(
        &self,
        request: &InstallerRequest,
        deadline: &Deadline,
    ) -> Result<DecodedReply, CallFailure> {
        deadline.check().map_err(transport::failure)?;
        self.frames.lock().unwrap().push(request.clone());
        if let Some(error) = self.result.lock().unwrap().take() {
            return Err(error);
        }
        if matches!(request, InstallerRequest::Status) && self.pending {
            Ok(DecodedReply::Status(
                StatusAdmission::PendingHealthContract(PendingHealthReason::Absent),
            ))
        } else {
            Ok(DecodedReply::Acknowledged)
        }
    }
    fn admit_status(&self, _: &DecodedReply) -> Result<(), CallFailure> {
        if self.admit && !self.pending {
            Ok(())
        } else {
            Err(CallFailure::Unavailable)
        }
    }
    fn source(&self) -> ObservationSource {
        ObservationSource::Live
    }
}
fn deadline(clock: Arc<FakeClock>) -> Deadline {
    Deadline::new(5000, clock, Cancellation::default()).unwrap()
}
#[test]
fn pending_status_is_demo_and_never_positive_native_evidence() {
    let fake = Fake::new(true, false);
    let (result, source) =
        transport::run(&fake, &InstallerRequest::Status, &deadline(Arc::default()));
    assert!(matches!(
        result,
        Ok(DecodedReply::Status(
            StatusAdmission::PendingHealthContract(_)
        ))
    ));
    assert_eq!(source, ObservationSource::Demo);
}
#[test]
fn status_admission_failure_is_refused_even_for_readonly_call() {
    let fake = Fake::new(false, false);
    let (result, source) =
        transport::run(&fake, &InstallerRequest::Status, &deadline(Arc::default()));
    assert!(matches!(result, Err(CallFailure::Unavailable)));
    assert_eq!(source, ObservationSource::Demo);
}
#[test]
fn pending_status_never_transmits_mutation() {
    let fake = Fake::new(true, false);
    assert!(
        transport::run(&fake, &InstallerRequest::Release, &deadline(Arc::default()))
            .0
            .is_err()
    );
    assert!(
        fake.frames
            .lock()
            .unwrap()
            .iter()
            .all(|r| matches!(r, InstallerRequest::Status))
    );
}
#[test]
fn response_finishes_at_newline_without_waiting_for_pipe_eof() {
    let mut response = Response::new();
    assert!(response.append(b"{\"ok\":").unwrap().is_none());
    assert_eq!(
        response.append(b"true}\n").unwrap().unwrap(),
        b"{\"ok\":true}\n"
    );
}
#[test]
fn response_rejects_extra_data_and_cap_before_allocation() {
    assert_eq!(
        Response::new().append(b"{}\n{}\n").err(),
        Some(NativeError::Invalid)
    );
    assert_eq!(
        Response::new()
            .append(&vec![b'x'; MAX_RESPONSE_BYTES + 1])
            .err(),
        Some(NativeError::Oversize)
    );
}
#[test]
fn expiration_before_transmission_posts_nothing() {
    let clock = Arc::new(FakeClock::default());
    let limit = deadline(clock.clone());
    clock.0.store(5000, Ordering::Release);
    let fake = Fake::new(false, true);
    assert!(
        transport::run(&fake, &InstallerRequest::Release, &limit)
            .0
            .is_err()
    );
    assert!(fake.frames.lock().unwrap().is_empty());
}
fn drain(port: &mut WindowsAgentPort, count: usize) -> Vec<AgentReply> {
    let end = Instant::now() + Duration::from_secs(2);
    let mut results = Vec::new();
    while results.len() < count && Instant::now() < end {
        results.extend(port.poll());
        std::thread::yield_now();
    }
    assert_eq!(results.len(), count);
    results
}
#[test]
fn queue_caps_calls_and_replies_and_preserves_ids_across_explicit_redetection() {
    let _case = PORT_CASE.lock().unwrap();
    let fake = Arc::new(Fake::new(true, false));
    let mut port = WindowsAgentPort::fake(fake.clone(), Arc::new(FakeClock::default())).unwrap();
    for id in 1..=32 {
        port.submit(AgentCall {
            id,
            request: InstallerRequest::Status,
            timeout_ms: 5000,
        })
        .unwrap();
    }
    assert!(matches!(
        port.submit(AgentCall {
            id: 33,
            request: InstallerRequest::Status,
            timeout_ms: 5000
        }),
        Err(CallFailure::QueueFull)
    ));
    assert_eq!(port.fake_redetect(fake.clone()), Err(NativeError::Busy));
    assert!(
        drain(&mut port, 32)
            .iter()
            .all(|r| r.source == ObservationSource::Demo)
    );
    port.fake_redetect(fake).unwrap();
    assert!(matches!(
        port.submit(AgentCall {
            id: 32,
            request: InstallerRequest::Status,
            timeout_ms: 5000
        }),
        Err(CallFailure::InvalidCall(_))
    ));
    port.submit(AgentCall {
        id: 33,
        request: InstallerRequest::Status,
        timeout_ms: 5000,
    })
    .unwrap();
    assert_eq!(drain(&mut port, 1)[0].id, 33);
}
#[test]
fn shutdown_refuses_new_calls_and_debug_never_logs_wire_text() {
    let _case = PORT_CASE.lock().unwrap();
    let mut port = WindowsAgentPort::fake(
        Arc::new(Fake::new(true, false)),
        Arc::new(FakeClock::default()),
    )
    .unwrap();
    assert_eq!(format!("{port:?}"), "WindowsAgentPort");
    port.shutdown();
    assert!(matches!(
        port.submit(AgentCall {
            id: 1,
            request: InstallerRequest::Status,
            timeout_ms: 5000
        }),
        Err(CallFailure::Unavailable)
    ));
}

struct Blocked {
    entered: Mutex<Option<mpsc::SyncSender<()>>>,
    complete: Mutex<mpsc::Receiver<()>>,
    frames: AtomicU64,
}
impl Endpoint for Blocked {
    fn check(&self, deadline: &Deadline) -> NativeResult<()> {
        deadline.check()
    }
    fn frame(&self, _: &InstallerRequest, _: &Deadline) -> Result<DecodedReply, CallFailure> {
        self.frames.fetch_add(1, Ordering::AcqRel);
        if let Some(entered) = self.entered.lock().unwrap().take() {
            entered.send(()).unwrap();
            self.complete.lock().unwrap().recv().unwrap();
        }
        Ok(DecodedReply::Status(
            StatusAdmission::PendingHealthContract(PendingHealthReason::Absent),
        ))
    }
    fn admit_status(&self, _: &DecodedReply) -> Result<(), CallFailure> {
        Err(CallFailure::Unavailable)
    }
    fn source(&self) -> ObservationSource {
        ObservationSource::Live
    }
}
fn blocked() -> (Arc<Blocked>, mpsc::Receiver<()>, mpsc::SyncSender<()>) {
    let (entered, observed) = mpsc::sync_channel(1);
    let (complete, release) = mpsc::sync_channel(1);
    (
        Arc::new(Blocked {
            entered: Mutex::new(Some(entered)),
            complete: Mutex::new(release),
            frames: AtomicU64::new(0),
        }),
        observed,
        complete,
    )
}
#[test]
fn timeout_settles_without_recycling_blocked_worker_or_admitting_late_result() {
    let _case = PORT_CASE.lock().unwrap();
    let (fake, entered, complete) = blocked();
    let clock = Arc::new(FakeClock::default());
    let mut port = WindowsAgentPort::fake(fake.clone(), clock.clone()).unwrap();
    port.submit(AgentCall {
        id: 1,
        request: InstallerRequest::Status,
        timeout_ms: 5000,
    })
    .unwrap();
    entered.recv_timeout(Duration::from_secs(1)).unwrap();
    clock.0.store(5000, Ordering::Release);
    let replies = port.poll();
    assert_eq!(replies.len(), 1);
    assert!(matches!(
        replies[0].result,
        Err(CallFailure::TimeoutOutcomeUnknown)
    ));
    assert_eq!(replies[0].source, ObservationSource::Demo);
    assert_eq!(
        port.fake_redetect(Arc::new(Fake::new(true, false))),
        Err(NativeError::Busy)
    );
    assert_eq!(fake.frames.load(Ordering::Acquire), 1);
    complete.send(()).unwrap();
    let end = Instant::now() + Duration::from_secs(1);
    loop {
        assert!(port.poll().is_empty());
        match port.fake_redetect(Arc::new(Fake::new(true, false))) {
            Ok(()) => break,
            Err(NativeError::Busy) if Instant::now() < end => std::thread::yield_now(),
            other => panic!("retained worker did not finish: {other:?}"),
        }
    }
    assert_eq!(fake.frames.load(Ordering::Acquire), 1);
}
#[test]
fn drop_retains_owned_endpoint_until_blocked_completion_without_replay() {
    let _case = PORT_CASE.lock().unwrap();
    let (fake, entered, complete) = blocked();
    let weak = Arc::downgrade(&fake);
    let mut port = WindowsAgentPort::fake(fake.clone(), Arc::new(FakeClock::default())).unwrap();
    port.submit(AgentCall {
        id: 1,
        request: InstallerRequest::Status,
        timeout_ms: 5000,
    })
    .unwrap();
    entered.recv_timeout(Duration::from_secs(1)).unwrap();
    drop(fake);
    drop(port);
    assert!(weak.upgrade().is_some());
    complete.send(()).unwrap();
    let end = Instant::now() + Duration::from_secs(1);
    while weak.upgrade().is_some() && Instant::now() < end {
        std::thread::yield_now();
    }
    assert!(weak.upgrade().is_none());
}

// Producer-shaped owned fixture, using the frozen Windows decoder rather than a fabricated
// Supported enum. No files, processes, named pipes, credentials or real agent are accessed.
const STATUS: &[u8] = br#"{"ok":true,"result":{
"controlling":null,"controlled_by":null,"projections":[],
"displays":[{"id":1,"name":"owned","pixels":[1920,1080],"scale":1,"mm":[500,300],"origin":[0,0]}],"peers":[],"layout":[],
"installer":{"schema_version":1,"build":{"version":"0.0.0","features":[]},
"instance":{"id":7,"pid":4242,"uid":null,"exe":"C:\\owned\\crosspane-agent.exe","runtime_dir":"C:\\owned\\runtime","started_unix_ms":99},
"config_revision":"9f86d081884c7d65","node":"1111111111111111111111111111111111111111111111111111111111111111",
"recovery_pending":0,"startup_recovery":"nothing_parked",
"gate":{"open":true,"session":"unlocked","active":true,"armed":false,"panic":false},
"epochs":{"gate":1,"grants":1,"layout":1,"backends":1},
"backends":[{"name":"capture","state":"ready","reason":null},{"name":"keys","state":"ready","reason":null},
{"name":"pointer","state":"ready","reason":null},{"name":"overlay","state":"ready","reason":null},
{"name":"hotkeys","state":"ready","reason":null},{"name":"keystore","state":"ready","reason":null},
{"name":"windows","state":"ready","reason":null},{"name":"parking","state":"ready","reason":null},
{"name":"frames","state":"ready","reason":null},{"name":"tray","state":"ready","reason":null},
{"name":"links","state":"ready","reason":null},{"name":"gpu","state":"ready","reason":null},
{"name":"home","state":"ready","reason":null},{"name":"audio","state":"ready","reason":null},
{"name":"discovery","state":"ready","reason":null}],"keystore":"os_store","permissions":[],
"discovery":{"enabled":false,"running":false,"candidates":0,"error":null},"tray":{"created":true},
"audio":{"enabled":false,"active_peers":[],"frames_sent":0,"frames_played":0},"settings_opened":0,"peers":[]}}}"#;
#[derive(Clone)]
struct State {
    process: transport::security::ProcessFacts,
    bootstrap: BootstrapV1,
    status: InstanceStatus,
}
fn state() -> State {
    use native_io::{
        files::FileIdentity,
        identity::{Sid, TokenFacts},
    };
    let status:InstanceStatus=serde_json::from_value(serde_json::from_slice::<serde_json::Value>(STATUS).unwrap()["result"]["installer"]["instance"].clone()).unwrap();
    State {
        process: transport::security::ProcessFacts {
            token: TokenFacts {
                user: Sid::from_bytes(vec![1, 1, 0, 0, 0, 0, 0, 5, 21, 0, 0, 0]).unwrap(),
                logon: Sid::from_bytes(vec![
                    1, 3, 0, 0, 0, 0, 0, 5, 5, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0,
                ])
                .unwrap(),
                session: 1,
                elevated: false,
                integrity: 0x2000,
                authentication_id: 7,
                impersonating: false,
            },
            pid: status.pid,
            created: 123,
            alive: true,
            image: status.exe.clone(),
            file: FileIdentity {
                volume: 11,
                file: [1; 16],
            },
        },
        bootstrap: BootstrapV1 {
            schema_version: 1,
            instance_id: status.id,
            pid: status.pid,
            started_unix_ms: status.started_unix_ms,
            phase: BootstrapPhase::Ready,
            phase_seq: 1,
            keystore: None,
            reason: None,
            runtime_dir: status.runtime_dir.clone(),
        },
        status,
    }
}
type Action = Arc<dyn Fn(&Mutex<State>) -> Result<DecodedReply, CallFailure> + Send + Sync>;
struct SelectedFake {
    original: State,
    current: Arc<Mutex<State>>,
    calls: Arc<Mutex<Vec<InstallerRequest>>>,
    action: Action,
}
impl Endpoint for SelectedFake {
    fn check(&self, deadline: &Deadline) -> NativeResult<()> {
        deadline.check()?;
        let current = self.current.lock().unwrap();
        transport::security::process_matches(&self.original.process, &current.process)?;
        transport::security::bootstrap_matches(&self.original.bootstrap, &current.bootstrap)
    }
    fn frame(
        &self,
        request: &InstallerRequest,
        deadline: &Deadline,
    ) -> Result<DecodedReply, CallFailure> {
        self.check(deadline).map_err(transport::failure)?;
        self.calls.lock().unwrap().push(request.clone());
        if matches!(request, InstallerRequest::Status) {
            let mut wire: serde_json::Value = serde_json::from_slice(STATUS).unwrap();
            wire["result"]["installer"]["instance"] =
                serde_json::to_value(&self.current.lock().unwrap().status).unwrap();
            decode_reply(
                request,
                &serde_json::to_vec(&wire).unwrap(),
                AgentPlatform::Windows,
            )
        } else {
            (self.action)(&self.current)
        }
    }
    fn admit_status(&self, reply: &DecodedReply) -> Result<(), CallFailure> {
        let DecodedReply::Status(StatusAdmission::Supported(health)) = reply else {
            return Err(CallFailure::Unavailable);
        };
        transport::security::status_matches(
            &self.original.bootstrap,
            &self.original.process.image,
            &health.installer().instance,
        )
        .map_err(transport::failure)
    }
    fn source(&self) -> ObservationSource {
        ObservationSource::Demo
    }
}
fn selected(
    current: Arc<Mutex<State>>,
    calls: Arc<Mutex<Vec<InstallerRequest>>>,
    action: Action,
) -> Arc<SelectedFake> {
    let original = current.lock().unwrap().clone();
    Arc::new(SelectedFake {
        original,
        current,
        calls,
        action,
    })
}
fn call(port: &mut WindowsAgentPort, id: u64, request: InstallerRequest) -> AgentReply {
    port.submit(AgentCall {
        id,
        request,
        timeout_ms: 5000,
    })
    .unwrap();
    drain(port, 1).remove(0)
}
#[test]
fn supported_status_then_mutation_uses_pre_and_post_status_and_never_fake_live() {
    let _case = PORT_CASE.lock().unwrap();
    let current = Arc::new(Mutex::new(state()));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let endpoint = selected(
        current,
        calls.clone(),
        Arc::new(|_| Ok(DecodedReply::Acknowledged)),
    );
    let mut port = WindowsAgentPort::fake(endpoint, Arc::new(FakeClock::default())).unwrap();
    let status = call(&mut port, 1, InstallerRequest::Status);
    assert!(matches!(
        status.result,
        Ok(DecodedReply::Status(StatusAdmission::Supported(_)))
    ));
    assert_eq!(status.source, ObservationSource::Demo);
    assert!(matches!(
        call(&mut port, 2, InstallerRequest::Release).result,
        Ok(DecodedReply::Acknowledged)
    ));
    assert!(matches!(
        calls.lock().unwrap().as_slice(),
        [
            InstallerRequest::Status,
            InstallerRequest::Status,
            InstallerRequest::Release,
            InstallerRequest::Status
        ]
    ));
}
#[test]
fn replacement_child_requires_explicit_fresh_selected_admission() {
    let _case = PORT_CASE.lock().unwrap();
    let current = Arc::new(Mutex::new(state()));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let action: Action = Arc::new(|_| Ok(DecodedReply::Acknowledged));
    let mut port = WindowsAgentPort::fake(
        selected(current.clone(), calls.clone(), action.clone()),
        Arc::new(FakeClock::default()),
    )
    .unwrap();
    assert!(call(&mut port, 1, InstallerRequest::Status).result.is_ok());
    {
        let mut child = current.lock().unwrap();
        child.process.pid += 1;
        child.process.created += 1;
        child.bootstrap.pid = child.process.pid;
        child.bootstrap.instance_id += 1;
        child.status.pid = child.process.pid;
        child.status.id = child.bootstrap.instance_id;
    }
    assert!(matches!(
        call(&mut port, 2, InstallerRequest::Release).result,
        Err(CallFailure::Unavailable)
    ));
    assert_eq!(calls.lock().unwrap().len(), 1);
    port.fake_redetect(selected(current, calls.clone(), action))
        .unwrap();
    assert!(matches!(
        call(&mut port, 3, InstallerRequest::Release).result,
        Ok(DecodedReply::Acknowledged)
    ));
    assert_eq!(calls.lock().unwrap().len(), 4);
}
#[test]
fn uncertain_mutation_retires_selection_until_explicit_redetection_without_replay() {
    let _case = PORT_CASE.lock().unwrap();
    let current = Arc::new(Mutex::new(state()));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let endpoint = selected(
        current.clone(),
        calls.clone(),
        Arc::new(|_| Err(CallFailure::TimeoutOutcomeUnknown)),
    );
    let mut port = WindowsAgentPort::fake(endpoint, Arc::new(FakeClock::default())).unwrap();
    assert!(matches!(
        call(&mut port, 1, InstallerRequest::Release).result,
        Err(CallFailure::TimeoutOutcomeUnknown)
    ));
    assert!(matches!(
        call(&mut port, 2, InstallerRequest::Panic).result,
        Err(CallFailure::Unavailable)
    ));
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| !transport::readonly(c))
            .count(),
        1
    );
    port.fake_redetect(selected(
        current,
        calls.clone(),
        Arc::new(|_| Ok(DecodedReply::Acknowledged)),
    ))
    .unwrap();
    assert!(matches!(
        call(&mut port, 3, InstallerRequest::Release).result,
        Ok(DecodedReply::Acknowledged)
    ));
}
#[test]
fn complete_authenticated_refusal_survives_without_replay_or_uncertainty() {
    let _case = PORT_CASE.lock().unwrap();
    let current = Arc::new(Mutex::new(state()));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let endpoint = selected(
        current,
        calls.clone(),
        Arc::new(|_| Err(CallFailure::Refused(AgentRefusal::Other))),
    );
    let mut port = WindowsAgentPort::fake(endpoint, Arc::new(FakeClock::default())).unwrap();
    for id in 1..=2 {
        assert!(matches!(
            call(&mut port, id, InstallerRequest::Release).result,
            Err(CallFailure::Refused(_))
        ));
    }
    assert_eq!(calls.lock().unwrap().len(), 4);
}
#[test]
fn replacement_during_mutation_cannot_turn_ack_into_verified_completion() {
    let _case = PORT_CASE.lock().unwrap();
    let current = Arc::new(Mutex::new(state()));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let endpoint = selected(
        current,
        calls.clone(),
        Arc::new(|current| {
            let mut current = current.lock().unwrap();
            current.status.id += 1;
            Ok(DecodedReply::Acknowledged)
        }),
    );
    let mut port = WindowsAgentPort::fake(endpoint, Arc::new(FakeClock::default())).unwrap();
    assert!(matches!(
        call(&mut port, 1, InstallerRequest::Release).result,
        Err(CallFailure::TimeoutOutcomeUnknown)
    ));
}

struct CallerGuard {
    fake: Fake,
    refusing: AtomicBool,
    observations: AtomicU64,
}
impl Endpoint for CallerGuard {
    fn admit_caller(&self) -> NativeResult<()> {
        self.observations.fetch_add(1, Ordering::AcqRel);
        if self.refusing.load(Ordering::Acquire) {
            Err(NativeError::Foreign)
        } else {
            Ok(())
        }
    }
    fn check(&self, deadline: &Deadline) -> NativeResult<()> {
        self.fake.check(deadline)
    }
    fn frame(
        &self,
        request: &InstallerRequest,
        deadline: &Deadline,
    ) -> Result<DecodedReply, CallFailure> {
        self.fake.frame(request, deadline)
    }
    fn admit_status(&self, reply: &DecodedReply) -> Result<(), CallFailure> {
        self.fake.admit_status(reply)
    }
    fn source(&self) -> ObservationSource {
        ObservationSource::Demo
    }
}
#[test]
fn caller_refusal_precedes_id_and_queue_acceptance_and_sends_nothing() {
    let _case = PORT_CASE.lock().unwrap();
    let endpoint = Arc::new(CallerGuard {
        fake: Fake::new(true, false),
        refusing: AtomicBool::new(true),
        observations: AtomicU64::new(0),
    });
    let mut port =
        WindowsAgentPort::fake(endpoint.clone(), Arc::new(FakeClock::default())).unwrap();
    let request = AgentCall {
        id: 1,
        request: InstallerRequest::Status,
        timeout_ms: 5000,
    };
    assert!(matches!(
        port.submit(request.clone()),
        Err(CallFailure::Unavailable)
    ));
    assert_eq!(endpoint.observations.load(Ordering::Acquire), 1);
    assert!(endpoint.fake.frames.lock().unwrap().is_empty());
    assert!(port.poll().is_empty());
    endpoint.refusing.store(false, Ordering::Release);
    port.submit(request).unwrap();
    assert_eq!(drain(&mut port, 1)[0].id, 1);
}

struct TerminalLoss {
    selected: SelectedFake,
    checks: AtomicU64,
}
impl Endpoint for TerminalLoss {
    fn check(&self, deadline: &Deadline) -> NativeResult<()> {
        if self.checks.fetch_add(1, Ordering::AcqRel) == 0 {
            self.selected.check(deadline)
        } else {
            Err(NativeError::Foreign)
        }
    }
    fn frame(
        &self,
        request: &InstallerRequest,
        deadline: &Deadline,
    ) -> Result<DecodedReply, CallFailure> {
        self.selected.frame(request, deadline)
    }
    fn admit_status(&self, reply: &DecodedReply) -> Result<(), CallFailure> {
        self.selected.admit_status(reply)
    }
    fn source(&self) -> ObservationSource {
        ObservationSource::Live
    }
}
#[test]
fn supported_status_terminal_context_failure_never_publishes_live_source() {
    let current = Arc::new(Mutex::new(state()));
    let original = current.lock().unwrap().clone();
    let endpoint = TerminalLoss {
        selected: SelectedFake {
            original,
            current,
            calls: Arc::default(),
            action: Arc::new(|_| Ok(DecodedReply::Acknowledged)),
        },
        checks: AtomicU64::new(0),
    };
    let (result, source) = transport::run(
        &endpoint,
        &InstallerRequest::Status,
        &deadline(Arc::default()),
    );
    assert!(matches!(result, Err(CallFailure::Unavailable)));
    assert_eq!(source, ObservationSource::Demo);
}
#[test]
fn already_queued_work_cannot_run_after_unknown_action_retirement() {
    let _case = PORT_CASE.lock().unwrap();
    let current = Arc::new(Mutex::new(state()));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut port = WindowsAgentPort::fake(
        selected(
            current,
            calls.clone(),
            Arc::new(|_| Err(CallFailure::TimeoutOutcomeUnknown)),
        ),
        Arc::new(FakeClock::default()),
    )
    .unwrap();
    for (id, request) in [(1, InstallerRequest::Release), (2, InstallerRequest::Panic)] {
        port.submit(AgentCall {
            id,
            request,
            timeout_ms: 5000,
        })
        .unwrap();
    }
    let replies = drain(&mut port, 2);
    assert!(matches!(
        replies[0].result,
        Err(CallFailure::TimeoutOutcomeUnknown)
    ));
    assert!(matches!(replies[1].result, Err(CallFailure::Unavailable)));
    assert!(matches!(
        calls.lock().unwrap().as_slice(),
        [InstallerRequest::Status, InstallerRequest::Release]
    ));
}
#[test]
fn deadline_overflow_does_not_consume_id_or_queue_capacity() {
    let _case = PORT_CASE.lock().unwrap();
    let clock = Arc::new(FakeClock(AtomicU64::new(u64::MAX)));
    let mut port = WindowsAgentPort::fake(Arc::new(Fake::new(true, false)), clock.clone()).unwrap();
    let request = AgentCall {
        id: 1,
        request: InstallerRequest::Status,
        timeout_ms: 5000,
    };
    assert!(matches!(
        port.submit(request.clone()),
        Err(CallFailure::InvalidCall(ContractError::InvalidDeadline))
    ));
    clock.0.store(0, Ordering::Release);
    port.submit(request).unwrap();
    assert_eq!(drain(&mut port, 1)[0].id, 1);
}
#[test]
fn exact_response_cap_invalid_utf8_and_typed_debug_remain_content_free() {
    let mut frame = Response::new();
    assert!(
        frame
            .append(&vec![b'x'; MAX_RESPONSE_BYTES - 1])
            .unwrap()
            .is_none()
    );
    assert_eq!(
        frame.append(b"\n").unwrap().unwrap().len(),
        MAX_RESPONSE_BYTES
    );
    assert!(matches!(
        decode_reply(&InstallerRequest::Status, b"\xff\n", AgentPlatform::Windows),
        Err(CallFailure::InvalidResponse)
    ));
    assert!(matches!(
        decode_reply(
            &InstallerRequest::Status,
            b"{}\n{}\n",
            AgentPlatform::Windows
        ),
        Err(CallFailure::InvalidResponse)
    ));
    let reply = AgentReply {
        id: 7,
        observed_at_ms: 0,
        source: ObservationSource::Demo,
        result: Ok(DecodedReply::Windows(Vec::new())),
    };
    assert_eq!(format!("{reply:?}"), "AgentReply { .. }");
}
#[test]
fn thirty_two_workers_refuse_new_port_without_eviction_and_release_owned_slots() {
    let _case = PORT_CASE.lock().unwrap();
    let end = Instant::now() + Duration::from_secs(2);
    while WindowsAgentPort::workers() != 0 && Instant::now() < end {
        std::thread::yield_now();
    }
    assert_eq!(WindowsAgentPort::workers(), 0);
    let endpoint = Arc::new(Fake::new(true, false));
    let clock = Arc::new(FakeClock::default());
    let mut ports: Vec<_> = (0..32)
        .map(|_| WindowsAgentPort::fake(endpoint.clone(), clock.clone()).unwrap())
        .collect();
    assert_eq!(WindowsAgentPort::workers(), 32);
    assert_eq!(
        WindowsAgentPort::fake(endpoint, clock).err(),
        Some(NativeError::Busy)
    );
    assert!(
        call(&mut ports[0], 1, InstallerRequest::Status)
            .result
            .is_ok()
    );
    assert_eq!(WindowsAgentPort::workers(), 32);
    drop(ports);
    let end = Instant::now() + Duration::from_secs(2);
    while WindowsAgentPort::workers() != 0 && Instant::now() < end {
        std::thread::yield_now();
    }
    assert_eq!(WindowsAgentPort::workers(), 0);
}
