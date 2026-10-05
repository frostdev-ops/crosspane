#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The shared live controller against fake platform ports. Fakes are test-only construction;
//! there is no production fake mode.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crosspane_installer::agent_contract::*;
use crosspane_installer::fixture::{
    FixtureCall, FixtureCommand, FixtureError, FixtureEvent, FixtureId, FixtureMessage,
    FixtureReceipt, FixtureSnapshot, OwnToneState, OwnWindowFacts, PhaseId, ToneId,
};
use crosspane_installer::gui::{InstallerController, ShellEffect};
use crosspane_installer::live::{
    self, Clock, FixtureReadiness, LiveController, MaintenanceId, MaintenanceReport,
    MaintenanceRequest, NativeJob, NativeOutcome, NativeRefusal, NativeReport, NativeStep,
    Platform, PlatformDescription, PracticeFixtures, RepairOutcome, StepReport, ids,
};
use crosspane_installer::live::{CheckState, SupportCheck, SupportChecklist, SupportChecksSlot};
use crosspane_installer::tutorial_flow::{HumanConfirmation, TutorialRole, TutorialSourcePolicy};
use crosspane_installer::view::*;
use crosspane_installer_core::*;
use crosspane_types::id::{NodeId, WindowId};
use crosspane_ui_kit::layout::{LayoutAction, PlacementIntent};
use serde_json::{Value, json};

const STATUS: &str = r#"{"ok":true,"result":{"controlling":null,"controlled_by":null,"projections":[],
"displays":[{"id":1,"name":"local panel","pixels":[2560,1440],"scale":1.0,"mm":[600.0,340.0],"origin":[0.0,0.0]}],
"peers":[],"layout":[],"installer":{"schema_version":1,
"build":{"version":"0.0.0","features":[]},"instance":{"id":99,"pid":1,"uid":1000,"exe":"fixture","runtime_dir":"fixture","started_unix_ms":1},
"config_revision":"1111111111111111","node":"1111111111111111111111111111111111111111111111111111111111111111",
"recovery_pending":0,"startup_recovery":"restored","gate":{"open":true,"session":"unlocked","active":true,"armed":true,"panic":false},
"epochs":{"gate":1,"grants":1,"layout":1,"backends":1},"keystore":"os_store","permissions":[],
"backends":[{"name":"capture","state":"ready","reason":null},{"name":"keys","state":"ready","reason":null},{"name":"pointer","state":"ready","reason":null},
{"name":"overlay","state":"ready","reason":null},{"name":"hotkeys","state":"ready","reason":null},{"name":"keystore","state":"ready","reason":null},
{"name":"windows","state":"ready","reason":null},{"name":"parking","state":"ready","reason":null},{"name":"frames","state":"ready","reason":null},
{"name":"tray","state":"ready","reason":null},{"name":"links","state":"ready","reason":null},{"name":"gpu","state":"ready","reason":null},
{"name":"home","state":"ready","reason":null},{"name":"audio","state":"ready","reason":null},{"name":"discovery","state":"ready","reason":null}],
"discovery":{"enabled":true,"running":true,"candidates":0,"error":null},"tray":{"created":true},
"audio":{"enabled":true,"active_peers":[],"frames_sent":0,"frames_played":0},"settings_opened":0,
"peers":[]}}}"#;

fn local() -> NodeId {
    NodeId([0x11; 32])
}
fn peer() -> NodeId {
    NodeId([0x22; 32])
}
fn peer_json(grants: &[&str]) -> Value {
    json!({"node": peer().to_string(), "name": "sensitive peer", "connected": true,
        "link_generation": 2, "features": [], "grants_given": grants, "last_source_parking": null,
        "counters": {"e1_controller_started":0,"e1_controller_ended":0,"e1_target_started":0,
        "e1_target_ended":0,"e1_injections_ok":0,"e1_hud_shows":0,"e1_chord_releases":0,
        "e1_command_releases":0,"e2_source_started":0,"e2_source_returned":0,"e2_dest_started":0,
        "e2_dest_returned":0,"e2_frames_presented":0,"e2_returns_failed":0}})
}

const SUPPORT: StepId = StepId(10);
const PAYLOAD: StepId = StepId(20);
const AGENT: StepId = StepId(22);

fn native(id: StepId, prerequisites: &[StepId], screen: ScreenId, installed: bool) -> NativeStep {
    NativeStep {
        id,
        prerequisites: prerequisites.to_vec(),
        required_for_installed: installed,
        required_for_ready: true,
        screen,
        group: ProgressGroup::Install,
        label: format!("native {}", id.0),
        action_label: format!("Do {}", id.0),
        uses_status: false,
        settles_with_peer: false,
        agent_apply: None,
    }
}

fn description() -> PlatformDescription {
    PlatformDescription {
        platform: AgentPlatform::Linux,
        machine_label: "own fixture A".into(),
        steps: vec![
            native(SUPPORT, &[], ScreenId::Compatibility, true),
            native(PAYLOAD, &[SUPPORT], ScreenId::InstallPlan, true),
            native(AGENT, &[PAYLOAD], ScreenId::Installing, true),
        ],
        connect_after: vec![AGENT],
        practice_after: vec![AGENT],
        hiding_choice: false,
        source_policy: TutorialSourcePolicy::Native,
        speakers_device: Some("validated virtual output".into()),
        resume_note: None,
    }
}

#[derive(Default)]
struct Native {
    jobs: Vec<NativeJob>,
    reports: Vec<NativeReport>,
    refuse: bool,
    shutdown: bool,
}

struct SharedAgent(Rc<RefCell<AgentQueue>>);
impl AgentPort for SharedAgent {
    fn submit(&mut self, call: AgentCall) -> Result<(), CallFailure> {
        self.0.borrow_mut().submit(call)
    }
    fn poll(&mut self) -> Vec<AgentReply> {
        self.0.borrow_mut().poll()
    }
}

#[derive(Default)]
struct FixtureState {
    launches: Vec<AttemptId>,
    readiness: Option<FixtureReadiness>,
    calls: Vec<FixtureCall>,
    receipts: Vec<FixtureReceipt>,
    closed: Vec<(AttemptId, FixtureId)>,
    retired: u32,
}
struct SharedFixtures(Rc<RefCell<FixtureState>>);
impl PracticeFixtures for SharedFixtures {
    fn launch(&mut self, attempt: AttemptId) -> Result<(), FixtureError> {
        let mut s = self.0.borrow_mut();
        s.launches.push(attempt);
        s.readiness = Some(FixtureReadiness::Launching);
        Ok(())
    }
    fn readiness(&mut self) -> FixtureReadiness {
        self.0.borrow().readiness.unwrap_or(FixtureReadiness::Idle)
    }
    fn submit(&mut self, call: FixtureCall) -> Result<(), FixtureError> {
        self.0.borrow_mut().calls.push(call);
        Ok(())
    }
    fn poll(&mut self) -> Vec<FixtureReceipt> {
        std::mem::take(&mut self.0.borrow_mut().receipts)
    }
    fn complete_closed(
        &mut self,
        attempt: AttemptId,
        fixture: FixtureId,
    ) -> Result<(), FixtureError> {
        self.0.borrow_mut().closed.push((attempt, fixture));
        Ok(())
    }
    fn retire(&mut self) {
        let mut s = self.0.borrow_mut();
        s.retired += 1;
        s.readiness = None;
    }
}

struct Fake {
    description: PlatformDescription,
    native: Rc<RefCell<Native>>,
    agent: SharedAgent,
    fixtures: SharedFixtures,
    checks: Option<SupportChecksSlot>,
}
impl Platform for Fake {
    fn describe(&self) -> PlatformDescription {
        self.description.clone()
    }
    fn submit(&mut self, job: NativeJob) -> Result<(), NativeRefusal> {
        let mut n = self.native.borrow_mut();
        if n.refuse {
            return Err(NativeRefusal::Busy);
        }
        n.jobs.push(job);
        Ok(())
    }
    fn poll(&mut self) -> Vec<NativeReport> {
        std::mem::take(&mut self.native.borrow_mut().reports)
    }
    fn agent(&mut self) -> &mut dyn AgentPort {
        &mut self.agent
    }
    fn fixtures(&mut self) -> &mut dyn PracticeFixtures {
        &mut self.fixtures
    }
    fn shutdown(&mut self) {
        self.native.borrow_mut().shutdown = true;
    }
    fn support_checks(&mut self) -> Option<SupportChecklist> {
        self.checks.as_ref().and_then(SupportChecksSlot::latest)
    }
}

struct H {
    c: LiveController,
    native: Rc<RefCell<Native>>,
    agent: Rc<RefCell<AgentQueue>>,
    fixtures: Rc<RefCell<FixtureState>>,
    clock: Arc<AtomicU64>,
    status: Value,
    calls: Vec<AgentCall>,
    effects: Vec<ShellEffect>,
    fixture_sequence: u64,
    /// Where the last `next` (or `install`) left the flow.
    last: ScreenId,
}

impl H {
    fn new() -> Self {
        Self::with(description())
    }
    fn with(description: PlatformDescription) -> Self {
        Self::with_checks(description, None)
    }
    fn with_checks(description: PlatformDescription, checks: Option<SupportChecksSlot>) -> Self {
        let native = Rc::new(RefCell::new(Native::default()));
        let agent = Rc::new(RefCell::new(AgentQueue::default()));
        let fixtures = Rc::new(RefCell::new(FixtureState::default()));
        let clock = Arc::new(AtomicU64::new(1_000));
        let time = clock.clone();
        let clock_fn: Clock = Arc::new(move || time.load(Ordering::SeqCst));
        let c = LiveController::new(
            Box::new(Fake {
                description,
                native: native.clone(),
                agent: SharedAgent(agent.clone()),
                fixtures: SharedFixtures(fixtures.clone()),
                checks,
            }),
            clock_fn,
        )
        .unwrap();
        Self {
            c,
            native,
            agent,
            fixtures,
            clock,
            status: serde_json::from_str(STATUS).unwrap(),
            calls: Vec::new(),
            effects: Vec::new(),
            fixture_sequence: 0,
            last: ScreenId::Welcome,
        }
    }
    fn advance(&mut self, ms: u64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
    }
    fn now(&self) -> u64 {
        self.clock.load(Ordering::SeqCst)
    }
    fn tick(&mut self) {
        self.advance(1);
        let tick = self.c.tick();
        self.effects.extend(tick.effects);
        self.calls.extend(self.agent.borrow_mut().take_calls());
    }
    fn view(&self) -> &WizardView {
        self.c.view()
    }
    fn button(&self, id: u16) -> Option<&ButtonView> {
        self.view().buttons.iter().find(|b| b.id == id)
    }
    fn click(&mut self, id: u16) {
        let b = self
            .button(id)
            .unwrap_or_else(|| panic!("button {id} missing on {:?}", self.view().screen));
        assert!(
            b.enabled,
            "button {id} disabled on {:?}",
            self.view().screen
        );
        let revision = self.view().revision;
        self.act(revision, WizardIntent::Button(id));
    }
    fn act(&mut self, revision: u64, intent: WizardIntent) -> bool {
        let closed = self.c.accept(WizardAction { revision, intent });
        self.tick();
        closed
    }
    /// Move to the next screen: with Next where the screen offers it (the welcome screen, or a
    /// screen the person came back to), otherwise by letting a finished screen move on by itself.
    fn next(&mut self) {
        let from = self.view().screen;
        if from != self.last {
            // It already moved on by itself while the test was answering the agent.
            self.last = from;
            return;
        }
        if self.button(ids::NEXT).is_some_and(|b| b.enabled) {
            self.click(ids::NEXT);
            self.last = self.view().screen;
            return;
        }
        for _ in 0..20 {
            self.advance(100);
            self.tick();
            if self.view().screen != from {
                self.last = self.view().screen;
                return;
            }
        }
        panic!("{from:?} neither offers Next nor moves on by itself");
    }
    fn row(&self, step: StepId) -> RowView {
        let wanted = step.0;
        let mut seen = self.view().rows.clone();
        if let Some(r) = seen.iter().find(|r| r.id == wanted) {
            return r.clone();
        }
        seen.clear();
        self.c
            .rows()
            .into_iter()
            .find(|r| r.id == wanted)
            .unwrap_or_else(|| panic!("row {wanted} missing"))
    }
    fn take_jobs(&mut self) -> Vec<NativeJob> {
        std::mem::take(&mut self.native.borrow_mut().jobs)
    }
    fn job(&mut self, step: StepId, stage: JobStage) -> (JobIntent, Option<live::Consent>) {
        let jobs = self.take_jobs();
        jobs.into_iter()
            .find_map(|j| match j {
                NativeJob::Step { job, consent, .. } if job.step == step && job.stage == stage => {
                    Some((job, consent))
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("no {stage:?} job for step {}", step.0))
    }
    fn report(&mut self, job: &JobIntent, outcome: NativeOutcome) {
        self.native
            .borrow_mut()
            .reports
            .push(NativeReport::Step(StepReport {
                job: job.clone(),
                outcome,
                detail: "fake detail".into(),
            }));
        self.tick();
    }
    fn live(&self) -> NativeOutcome {
        NativeOutcome::Verified {
            source: ObservationSource::Live,
            observed_at_ms: self.now(),
        }
    }
    /// Detection finds nothing to do; verification is Live.
    fn pass(&mut self, step: StepId) {
        let (detect, _) = self.job(step, JobStage::Detect);
        self.report(
            &detect,
            NativeOutcome::Detected {
                needs_action: false,
            },
        );
        let (verify, _) = self.job(step, JobStage::Verify);
        let live = self.live();
        self.report(&verify, live);
        assert_eq!(self.row(step).state, RowState::Verified);
    }
    /// Start setup, then let the install page run: each finished screen of it moves on by itself.
    fn install(&mut self) {
        self.tick();
        self.next();
        assert_eq!(self.view().screen, ScreenId::Compatibility);
        self.pass(SUPPORT);
        assert_eq!(self.view().screen, ScreenId::InstallPlan);
        self.pass(PAYLOAD);
        assert_eq!(self.view().screen, ScreenId::Installing);
        self.pass(AGENT);
        self.last = ScreenId::Installing;
    }
    fn call(&mut self, matches: impl Fn(&InstallerRequest) -> bool) -> AgentCall {
        self.calls.extend(self.agent.borrow_mut().take_calls());
        let i = self
            .calls
            .iter()
            .position(|c| matches(&c.request))
            .unwrap_or_else(|| panic!("missing call; have {}", self.calls.len()));
        self.calls.remove(i)
    }
    fn has_call(&mut self, matches: impl Fn(&InstallerRequest) -> bool) -> bool {
        self.calls.extend(self.agent.borrow_mut().take_calls());
        self.calls.iter().any(|c| matches(&c.request))
    }
    fn reply(&mut self, call: &AgentCall, result: Result<DecodedReply, CallFailure>) {
        self.reply_from(call, result, ObservationSource::Live);
    }
    fn reply_from(
        &mut self,
        call: &AgentCall,
        result: Result<DecodedReply, CallFailure>,
        source: ObservationSource,
    ) {
        self.advance(1);
        self.agent
            .borrow_mut()
            .push_reply(AgentReply {
                id: call.id,
                observed_at_ms: self.now(),
                source,
                result,
            })
            .unwrap();
        self.tick();
    }
    fn health(&self) -> Box<HealthSnapshot> {
        match parse_status(
            &serde_json::to_vec(&self.status).unwrap(),
            AgentPlatform::Linux,
        )
        .unwrap()
        {
            StatusAdmission::Supported(h) => h,
            _ => panic!("fixture status not admitted"),
        }
    }
    /// Answer every outstanding Status call, ticking until one is issued if none is pending.
    fn status_reply(&mut self) {
        for _ in 0..40 {
            if self.has_call(|r| *r == InstallerRequest::Status) {
                break;
            }
            self.advance(100);
            self.tick();
        }
        let call = self.call(|r| *r == InstallerRequest::Status);
        let h = self.health();
        self.reply(
            &call,
            Ok(DecodedReply::Status(StatusAdmission::Supported(h))),
        );
    }
    fn ack(&mut self, call: &AgentCall) {
        let ack = decode_reply(
            &call.request,
            br#"{"ok":true,"result":"arbitrary producer prose"}"#,
            AgentPlatform::Linux,
        )
        .unwrap();
        self.reply(call, Ok(ack));
    }
    fn pair_status(&mut self, call: &AgentCall, body: &str) {
        let decoded = decode_reply(&call.request, body.as_bytes(), AgentPlatform::Linux).unwrap();
        self.reply(call, Ok(decoded));
    }
    fn paired_peer(&mut self, grants: &[&str]) {
        self.status["result"]["installer"]["peers"] = json!([peer_json(grants)]);
        self.status["result"]["peers"] = json!([{"node": peer().to_string(),
            "displays": [{"id":1,"name":"peer panel","pixels":[1920,1080],"scale":1.0,"mm":[530.0,300.0],"origin":[0.0,0.0]}]}]);
    }
    fn placements(&mut self) {
        self.status["result"]["layout"] = json!([
            {"node": local().short(), "display": 1, "origin_mm": [0.0, 0.0], "version": 1},
            {"node": peer().short(), "display": 1, "origin_mm": [600.0, 0.0], "version": 1}
        ]);
    }
    fn summary(&self) -> SummaryView {
        self.view().summary
    }
}

const ALL_GRANTS: [&str; 5] = ["browse", "input", "present", "share", "speaker"];

#[test]
fn graph_rejects_platform_steps_outside_the_reserved_range_or_shared_anchors() {
    let mut bad = description();
    bad.steps
        .push(native(StepId(60), &[], ScreenId::Network, false));
    let err = LiveController::new(
        Box::new(Fake {
            description: bad,
            native: Rc::default(),
            agent: SharedAgent(Rc::default()),
            fixtures: SharedFixtures(Rc::default()),
            checks: None,
        }),
        Arc::new(|| 1),
    );
    assert!(err.is_err());
    let mut unknown = description();
    unknown.connect_after = vec![StepId(55)];
    assert!(
        LiveController::new(
            Box::new(Fake {
                description: unknown,
                native: Rc::default(),
                agent: SharedAgent(Rc::default()),
                fixtures: SharedFixtures(Rc::default()),
                checks: None,
            }),
            Arc::new(|| 1),
        )
        .is_err()
    );
}

#[test]
fn welcome_starts_nothing_and_detection_begins_only_on_its_screen() {
    let mut h = H::new();
    h.tick();
    assert_eq!(h.view().screen, ScreenId::Welcome);
    assert!(!h.view().demo);
    assert!(
        h.take_jobs().is_empty(),
        "no native work before the person starts"
    );
    assert!(
        h.calls.is_empty(),
        "no agent traffic before the agent is installed"
    );
    assert_eq!(h.summary(), SummaryView::NotInstalled);
    h.next();
    assert_eq!(h.view().screen, ScreenId::Compatibility);
    let (detect, consent) = h.job(SUPPORT, JobStage::Detect);
    assert!(consent.is_none(), "detection never carries consent");
    assert_eq!(detect.step, SUPPORT);
    assert!(h.take_jobs().is_empty(), "PAYLOAD waits for SUPPORT");
}

#[test]
fn a_screen_moves_on_by_itself_only_once_every_step_on_it_is_verified() {
    let mut h = H::new();
    h.tick();
    h.next();
    // No Next to click: the install page carries on by itself.
    assert!(h.button(ids::NEXT).is_none());
    let (detect, _) = h.job(SUPPORT, JobStage::Detect);
    h.report(
        &detect,
        NativeOutcome::Detected {
            needs_action: false,
        },
    );
    for _ in 0..5 {
        h.advance(500);
        h.tick();
    }
    assert_eq!(
        h.view().screen,
        ScreenId::Compatibility,
        "detection alone never moves on"
    );
    let (verify, _) = h.job(SUPPORT, JobStage::Verify);
    let live = h.live();
    h.report(&verify, live);
    assert_eq!(h.view().screen, ScreenId::InstallPlan);
    // The next screen's step starts being looked at in the same pass.
    let _ = h.job(PAYLOAD, JobStage::Detect);
}

#[test]
fn going_back_to_a_finished_screen_keeps_it_until_the_person_continues() {
    let mut h = H::new();
    h.install();
    h.next();
    assert_eq!(h.view().screen, ScreenId::Connect);
    h.click(ids::BACK);
    assert_eq!(h.view().screen, ScreenId::Installing);
    for _ in 0..10 {
        h.advance(500);
        h.tick();
    }
    assert_eq!(
        h.view().screen,
        ScreenId::Installing,
        "a screen the person went back to stays"
    );
    let next = h.button(ids::NEXT).expect("Continue is offered");
    assert_eq!(next.kind, ButtonKind::Primary);
    h.click(ids::NEXT);
    assert_eq!(h.view().screen, ScreenId::Connect);
}

#[test]
fn demo_or_scratch_verification_reaches_core_and_never_verifies() {
    let mut h = H::new();
    h.tick();
    h.next();
    let (detect, _) = h.job(SUPPORT, JobStage::Detect);
    h.report(
        &detect,
        NativeOutcome::Detected {
            needs_action: false,
        },
    );
    let (verify, _) = h.job(SUPPORT, JobStage::Verify);
    let now = h.now();
    h.report(
        &verify,
        NativeOutcome::Verified {
            source: ObservationSource::Demo,
            observed_at_ms: now,
        },
    );
    assert_ne!(h.row(SUPPORT).state, RowState::Verified);
    assert_eq!(
        h.view().screen,
        ScreenId::Compatibility,
        "an unverified screen never moves on"
    );
}

#[test]
fn stale_wrong_stage_and_retired_reports_are_dropped() {
    let mut h = H::new();
    h.tick();
    h.next();
    let (detect, _) = h.job(SUPPORT, JobStage::Detect);
    // Wrong operation.
    let forged = JobIntent {
        operation: OperationId(detect.operation.0 + 50),
        ..detect.clone()
    };
    h.report(
        &forged,
        NativeOutcome::Detected {
            needs_action: false,
        },
    );
    assert!(h.take_jobs().is_empty());
    // Wrong stage for the live operation.
    let wrong_stage = JobIntent {
        stage: JobStage::Verify,
        ..detect.clone()
    };
    let live = h.live();
    h.report(&wrong_stage, live);
    assert_ne!(h.row(SUPPORT).state, RowState::Verified);
    // The genuine report still works, exactly once.
    h.report(
        &detect,
        NativeOutcome::Detected {
            needs_action: false,
        },
    );
    let (verify, _) = h.job(SUPPORT, JobStage::Verify);
    h.report(&detect, NativeOutcome::Detected { needs_action: true });
    assert!(
        h.take_jobs().is_empty(),
        "a retired detect report schedules nothing"
    );
    let live = h.live();
    h.report(&verify, live);
    assert_eq!(h.row(SUPPORT).state, RowState::Verified);
}

#[test]
fn consent_is_bound_to_the_plan_preview_revision_and_operation() {
    // A step outside the user-scope install (here a network change) always asks.
    const NETWORK: StepId = StepId(30);
    let mut h = with_extra_step(native(NETWORK, &[], ScreenId::Network, false));
    h.install();
    h.next();
    assert_eq!(h.view().screen, ScreenId::Network);
    let (detect, _) = h.job(NETWORK, JobStage::Detect);
    h.report(&detect, NativeOutcome::Detected { needs_action: true });
    let (plan, _) = h.job(NETWORK, JobStage::Plan);
    assert!(
        h.button(ids::consent(NETWORK)).is_none(),
        "no consent before the preview exists"
    );
    h.report(
        &plan,
        NativeOutcome::Planned {
            preview: "pkexec /usr/bin/ufw allow from 192.0.2.0/24".into(),
        },
    );
    assert_eq!(h.row(NETWORK).state, RowState::NeedsAction);
    assert!(h.view().message.contains("pkexec /usr/bin/ufw allow"));
    let consent_button = h.button(ids::consent(NETWORK)).unwrap();
    assert_eq!(consent_button.kind, ButtonKind::Primary);
    for _ in 0..10 {
        h.advance(500);
        h.tick();
    }
    assert!(
        h.take_jobs().is_empty(),
        "a step outside the install page never goes ahead by itself"
    );
    assert_eq!(
        h.view().screen,
        ScreenId::Network,
        "an open question holds the screen"
    );
    let old = h.view().revision;
    // A stale revision is ignored.
    h.act(old - 1, WizardIntent::Button(ids::consent(NETWORK)));
    assert!(h.take_jobs().is_empty());
    h.click(ids::consent(NETWORK));
    let (apply, consent) = h.job(NETWORK, JobStage::Apply);
    let consent = consent.expect("apply carries consent");
    assert_eq!(consent.operation, apply.operation);
    assert_eq!(consent.revision, old);
    // A double click cannot start a second mutation.
    let revision = h.view().revision;
    h.act(revision, WizardIntent::Button(ids::consent(NETWORK)));
    assert!(h.take_jobs().is_empty());
    h.report(&apply, NativeOutcome::Applied(ApplyOutcome::Applied));
    let (verify, _) = h.job(NETWORK, JobStage::Verify);
    let live = h.live();
    h.report(&verify, live);
    assert_eq!(h.row(NETWORK).state, RowState::Verified);
}

#[test]
fn install_steps_go_ahead_with_the_start_click_bound_to_their_own_preview() {
    let mut h = H::new();
    h.tick();
    h.next();
    h.pass(SUPPORT);
    let (detect, _) = h.job(PAYLOAD, JobStage::Detect);
    h.report(&detect, NativeOutcome::Detected { needs_action: true });
    let (plan, _) = h.job(PAYLOAD, JobStage::Plan);
    h.report(
        &plan,
        NativeOutcome::Planned {
            preview: "Copy 4 files into ~/.local".into(),
        },
    );
    // No further click: the start of setup is the consent, recorded against exactly this plan.
    let (apply, consent) = h.job(PAYLOAD, JobStage::Apply);
    let consent = consent.expect("apply carries consent");
    assert_eq!(consent.plan, plan.operation);
    assert_eq!(consent.operation, apply.operation);
    // While it runs, the step says what it is doing: the preview it was started from.
    assert_eq!(h.row(PAYLOAD).state, RowState::Working);
    assert!(h.row(PAYLOAD).detail.contains("Copy 4 files"));
    assert!(h.button(ids::consent(PAYLOAD)).is_none());
    h.report(&apply, NativeOutcome::Applied(ApplyOutcome::Applied));
    let (verify, _) = h.job(PAYLOAD, JobStage::Verify);
    let live = h.live();
    h.report(&verify, live);
    assert_eq!(h.row(PAYLOAD).state, RowState::Verified);
    assert_eq!(h.view().screen, ScreenId::Installing);
}

#[test]
fn an_install_step_waits_for_the_person_while_crosspane_is_in_use() {
    const RESTART: StepId = StepId(30);
    let mut h = with_extra_step(native(RESTART, &[], ScreenId::Installing, false));
    // Something is playing across right now: a restart would cut it short.
    h.status["result"]["installer"]["audio"]["active_peers"] = json!([peer().to_string()]);
    h.install();
    h.status_reply();
    let (detect, _) = h.job(RESTART, JobStage::Detect);
    h.report(&detect, NativeOutcome::Detected { needs_action: true });
    let (plan, _) = h.job(RESTART, JobStage::Plan);
    h.report(
        &plan,
        NativeOutcome::Planned {
            preview: "Restart Crosspane once".into(),
        },
    );
    for _ in 0..4 {
        h.advance(300);
        h.tick();
    }
    assert!(
        h.take_jobs().is_empty(),
        "nothing in use is interrupted without the person"
    );
    assert_eq!(h.row(RESTART).state, RowState::NeedsAction);
    assert!(h.view().message.contains("Restart Crosspane once"));
    assert!(h.view().message.contains("in use"));
    assert_eq!(
        h.button(ids::consent(RESTART)).map(|b| b.kind),
        Some(ButtonKind::Primary)
    );
    // Once nothing is shared any more, setup carries on by itself.
    h.status["result"]["installer"]["audio"]["active_peers"] = json!([]);
    h.status_reply();
    let (apply, consent) = h.job(RESTART, JobStage::Apply);
    assert_eq!(consent.map(|c| c.plan), Some(plan.operation));
    assert_eq!(apply.step, RESTART);
}

#[test]
fn refused_and_unknown_mutations_wait_or_redetect_before_any_retry() {
    let mut h = H::new();
    h.tick();
    h.next();
    h.pass(SUPPORT);
    let (detect, _) = h.job(PAYLOAD, JobStage::Detect);
    h.report(&detect, NativeOutcome::Detected { needs_action: true });
    let (plan, _) = h.job(PAYLOAD, JobStage::Plan);
    h.report(
        &plan,
        NativeOutcome::Planned {
            preview: "p".into(),
        },
    );
    // The install step goes ahead with the start click as its consent.
    let (apply, _) = h.job(PAYLOAD, JobStage::Apply);
    h.report(&apply, NativeOutcome::Applied(ApplyOutcome::Unknown));
    // Unknown always re-detects; nothing is retried blindly.
    let (redetect, consent) = h.job(PAYLOAD, JobStage::Detect);
    assert!(consent.is_none());
    h.report(&redetect, NativeOutcome::Detected { needs_action: true });
    let (plan, _) = h.job(PAYLOAD, JobStage::Plan);
    h.report(
        &plan,
        NativeOutcome::Planned {
            preview: "p2".into(),
        },
    );
    // A new plan is a new preview: the go-ahead is recorded against it, never against "p".
    let (apply, consent) = h.job(PAYLOAD, JobStage::Apply);
    assert_eq!(consent.map(|c| c.plan), Some(plan.operation));
    h.report(&apply, NativeOutcome::Applied(ApplyOutcome::Refused));
    assert_eq!(h.row(PAYLOAD).state, RowState::Waiting);
    for _ in 0..5 {
        h.advance(500);
        h.tick();
    }
    assert!(
        h.take_jobs().is_empty(),
        "a refusal is never retried by itself"
    );
    h.click(ids::retry(PAYLOAD));
    let _ = h.job(PAYLOAD, JobStage::Detect);
}

#[test]
fn a_platform_refusal_fails_the_job_instead_of_hanging() {
    let mut h = H::new();
    h.tick();
    h.native.borrow_mut().refuse = true;
    h.next();
    h.tick();
    assert_eq!(h.row(SUPPORT).state, RowState::Failed);
    h.native.borrow_mut().refuse = false;
    h.click(ids::retry(SUPPORT));
    let _ = h.job(SUPPORT, JobStage::Detect);
}

#[test]
fn installed_milestone_comes_only_from_core_and_status_polling_follows_the_agent() {
    let mut h = H::new();
    h.install();
    assert_eq!(h.summary(), SummaryView::InstalledWaiting);
    // The agent step is verified, so status polling starts.
    h.status_reply();
    assert_eq!(h.summary(), SummaryView::InstalledWaiting);
}

#[test]
fn passive_status_updates_keep_the_view_revision() {
    let mut h = H::new();
    h.install();
    h.next();
    assert_eq!(h.view().screen, ScreenId::Connect);
    h.status_reply();
    h.tick();
    let revision = h.view().revision;
    h.status_reply();
    h.advance(600);
    h.tick();
    h.status_reply();
    assert_eq!(h.view().revision, revision);
}

fn to_connect(h: &mut H) {
    h.install();
    h.next();
    assert_eq!(h.view().screen, ScreenId::Connect);
    h.status_reply();
}

#[test]
fn pairing_never_offers_input_and_the_sas_is_display_only_until_explicit_confirmation() {
    let mut h = H::new();
    to_connect(&mut h);
    assert_eq!(h.row(live::steps::PAIR).state, RowState::NeedsAction);
    // Typing an address is one of the other ways to connect; the field appears only then.
    assert!(
        !h.view()
            .fields
            .iter()
            .any(|f| matches!(f, FieldView::PeerAddress { .. }))
    );
    h.click(ids::PAIR_MANUAL);
    let revision = h.view().revision;
    h.act(
        revision,
        WizardIntent::EditPeerAddress {
            field: ids::PEER_ADDRESS,
            value: "192.0.2.7:7878".into(),
        },
    );
    h.click(ids::PAIR_JOIN);
    let join = h.call(|r| matches!(r, InstallerRequest::PairJoin { .. }));
    let InstallerRequest::PairJoin { addr, allow_input } = &join.request else {
        unreachable!()
    };
    assert_eq!(addr.to_string(), "192.0.2.7:7878");
    assert!(!allow_input, "pairing never grants input implicitly");
    h.ack(&join);
    h.advance(600);
    h.tick();
    let poll = h.call(|r| *r == InstallerRequest::PairStatus);
    h.pair_status(
        &poll,
        r#"{"ok":true,"result":{"phase":"confirm","sas":"482 913","candidates":[],"peer":"sensitive peer","error":null}}"#,
    );
    assert_eq!(h.view().screen, ScreenId::MatchNumbers);
    assert_eq!(h.view().illustration.sas.as_deref(), Some("482 913"));
    assert!(!h.has_call(|r| matches!(r, InstallerRequest::PairConfirm { .. })));
    h.click(ids::PAIR_CONFIRM);
    let confirm = h.call(|r| matches!(r, InstallerRequest::PairConfirm { .. }));
    assert_eq!(
        confirm.request,
        InstallerRequest::PairConfirm { accept: true }
    );
    h.ack(&confirm);
    h.advance(600);
    h.tick();
    let poll = h.call(|r| *r == InstallerRequest::PairStatus);
    h.pair_status(
        &poll,
        r#"{"ok":true,"result":{"phase":"paired","sas":null,"candidates":[],"peer":"sensitive peer","error":null}}"#,
    );
    assert_ne!(
        h.row(live::steps::PAIR).state,
        RowState::Verified,
        "an acknowledgement or pair phase never verifies"
    );
    h.paired_peer(&[]);
    // A Status call issued before verification began cannot verify it, even if it carries the
    // new peer.
    h.status_reply();
    assert_ne!(
        h.row(live::steps::PAIR).state,
        RowState::Verified,
        "a status issued before the verify job began is not evidence"
    );
    h.status_reply();
    assert_eq!(h.row(live::steps::PAIR).state, RowState::Verified);
    assert_eq!(h.view().peer.as_deref(), Some("sensitive peer"));
}

#[test]
fn listener_picks_the_matching_number_explicitly() {
    let mut h = H::new();
    to_connect(&mut h);
    h.click(ids::PAIR_LISTEN);
    let listen = h.call(|r| matches!(r, InstallerRequest::PairListen { .. }));
    assert_eq!(
        listen.request,
        InstallerRequest::PairListen { allow_input: false }
    );
    h.ack(&listen);
    h.advance(600);
    h.tick();
    let poll = h.call(|r| *r == InstallerRequest::PairStatus);
    h.pair_status(
        &poll,
        r#"{"ok":true,"result":{"phase":"pick","sas":null,"candidates":["11","42","97"],"peer":null,"error":null}}"#,
    );
    assert_eq!(h.view().screen, ScreenId::MatchNumbers);
    h.click(ids::pair_pick(1));
    let pick = h.call(|r| matches!(r, InstallerRequest::PairPick { .. }));
    assert_eq!(pick.request, InstallerRequest::PairPick { index: 1 });
}

#[test]
fn a_refused_pairing_waits_for_the_person_and_an_unknown_one_redetects() {
    let mut h = H::new();
    to_connect(&mut h);
    h.click(ids::PAIR_LISTEN);
    let listen = h.call(|r| matches!(r, InstallerRequest::PairListen { .. }));
    h.reply(&listen, Err(CallFailure::Refused(AgentRefusal::Other)));
    assert_eq!(h.row(live::steps::PAIR).state, RowState::Waiting);
    h.click(ids::retry(live::steps::PAIR));
    h.status_reply();
    assert_eq!(h.row(live::steps::PAIR).state, RowState::NeedsAction);
    h.click(ids::PAIR_LISTEN);
    let listen = h.call(|r| matches!(r, InstallerRequest::PairListen { .. }));
    h.reply(&listen, Err(CallFailure::TimeoutOutcomeUnknown));
    assert_eq!(h.row(live::steps::PAIR).state, RowState::Working);
    // Re-detection reads status before anything else is offered.
    h.paired_peer(&[]);
    h.status_reply();
    h.status_reply();
    assert_eq!(h.row(live::steps::PAIR).state, RowState::Verified);
}

// ---- automatic pairing ----------------------------------------------------------------------

impl H {
    /// Answer the outstanding discovery request with `found` (a JSON list of name/addr pairs).
    fn scan_reply(&mut self, found: &str) {
        for _ in 0..20 {
            if self.has_call(|r| *r == InstallerRequest::PairScan) {
                break;
            }
            self.advance(200);
            self.tick();
        }
        let call = self.call(|r| *r == InstallerRequest::PairScan);
        let body = format!(r#"{{"ok":true,"result":{found}}}"#);
        let decoded = decode_reply(&call.request, body.as_bytes(), AgentPlatform::Linux).unwrap();
        self.reply(&call, Ok(decoded));
    }
    /// Let time pass on the pairing screen, answering discovery with nobody, until setup asks
    /// the agent to open a pairing window.
    fn until_listen(&mut self) -> AgentCall {
        for _ in 0..40 {
            if self.has_call(|r| matches!(r, InstallerRequest::PairListen { .. })) {
                return self.call(|r| matches!(r, InstallerRequest::PairListen { .. }));
            }
            if self.has_call(|r| *r == InstallerRequest::PairScan) {
                self.scan_reply("[]");
            }
            self.advance(250);
            self.tick();
        }
        panic!("setup never opened a pairing window");
    }
    fn pair_poll(&mut self, body: &str) {
        for _ in 0..10 {
            if self.has_call(|r| *r == InstallerRequest::PairStatus) {
                break;
            }
            self.advance(300);
            self.tick();
        }
        let poll = self.call(|r| *r == InstallerRequest::PairStatus);
        self.pair_status(&poll, body);
    }
}

#[test]
fn discovery_starts_on_the_pairing_screen_and_one_found_computer_is_the_one_primary_action() {
    let mut h = H::new();
    to_connect(&mut h);
    assert!(
        h.has_call(|r| *r == InstallerRequest::PairScan),
        "looking starts when the screen opens"
    );
    h.scan_reply(r#"[{"name":"mac-studio","addr":"192.0.2.9:47811"}]"#);
    let primary: Vec<_> = h
        .view()
        .buttons
        .iter()
        .filter(|b| b.kind == ButtonKind::Primary)
        .cloned()
        .collect();
    assert_eq!(primary.len(), 1, "one primary action: {primary:?}");
    assert_eq!(primary[0].id, ids::pair_candidate(0));
    assert_eq!(primary[0].label, "Pair with mac-studio");
    // The other ways are quiet links under their own heading; no address field yet.
    assert_eq!(
        h.view().link_caption.as_deref(),
        Some("Other ways to connect")
    );
    assert_eq!(
        h.button(ids::PAIR_MANUAL).map(|b| b.kind),
        Some(ButtonKind::Link)
    );
    assert!(h.view().fields.is_empty());
    assert!(
        h.view()
            .rows
            .iter()
            .any(|r| r.label == "Found mac-studio on your network")
    );
    // While someone is there to choose, this computer opens no window of its own.
    for _ in 0..12 {
        h.advance(500);
        h.tick();
        if h.has_call(|r| *r == InstallerRequest::PairScan) {
            h.scan_reply(r#"[{"name":"mac-studio","addr":"192.0.2.9:47811"}]"#);
        }
    }
    assert!(!h.has_call(|r| matches!(r, InstallerRequest::PairListen { .. })));
    h.click(ids::pair_candidate(0));
    let join = h.call(|r| matches!(r, InstallerRequest::PairJoin { .. }));
    assert_eq!(
        join.request,
        InstallerRequest::PairJoin {
            addr: "192.0.2.9:47811".parse().unwrap(),
            allow_input: false
        }
    );
}

#[test]
fn with_nobody_found_setup_opens_this_computers_window_and_the_numbers_still_need_confirming() {
    let mut h = H::new();
    to_connect(&mut h);
    let listen = h.until_listen();
    assert_eq!(
        listen.request,
        InstallerRequest::PairListen { allow_input: false },
        "an automatic window never offers input"
    );
    h.ack(&listen);
    h.pair_poll(
        r#"{"ok":true,"result":{"phase":"listening","sas":null,"candidates":[],"peer":null,"error":null}}"#,
    );
    assert_eq!(h.view().screen, ScreenId::Connect);
    assert!(
        h.view()
            .rows
            .iter()
            .any(|r| r.label == "Ready to be found" && r.state == RowState::Working)
    );
    h.pair_poll(
        r#"{"ok":true,"result":{"phase":"confirm","sas":"482 913","candidates":[],"peer":"mac-studio","error":null}}"#,
    );
    assert_eq!(h.view().screen, ScreenId::MatchNumbers);
    assert_eq!(h.view().illustration.sas.as_deref(), Some("482 913"));
    for _ in 0..5 {
        h.advance(400);
        h.tick();
    }
    assert!(
        !h.has_call(|r| matches!(r, InstallerRequest::PairConfirm { .. })),
        "the numbers are never confirmed for the person"
    );
    assert_eq!(
        h.button(ids::PAIR_CONFIRM).map(|b| b.kind),
        Some(ButtonKind::Primary)
    );
    assert_eq!(
        h.button(ids::PAIR_REJECT).map(|b| b.kind),
        Some(ButtonKind::Destructive)
    );
    h.click(ids::PAIR_CONFIRM);
    let confirm = h.call(|r| matches!(r, InstallerRequest::PairConfirm { .. }));
    assert_eq!(
        confirm.request,
        InstallerRequest::PairConfirm { accept: true }
    );
}

#[test]
fn an_automatic_window_that_closes_unanswered_is_reopened_quietly() {
    let mut h = H::new();
    to_connect(&mut h);
    let listen = h.until_listen();
    h.ack(&listen);
    h.pair_poll(
        r#"{"ok":true,"result":{"phase":"failed","sas":null,"candidates":[],"peer":null,"error":"the pairing window closed without a connection"}}"#,
    );
    // Nobody joined: not a failure to show, and nothing to click.
    assert!(
        !h.view()
            .rows
            .iter()
            .any(|r| r.label == "Pairing didn't finish")
    );
    assert!(h.button(ids::retry(live::steps::PAIR)).is_none());
    assert!(
        h.view()
            .rows
            .iter()
            .any(|r| r.label == "Looking for the other computer…")
    );
    h.status_reply();
    let again = h.until_listen();
    assert_eq!(
        again.request,
        InstallerRequest::PairListen { allow_input: false }
    );
}

#[test]
fn a_window_the_person_opened_that_fails_says_why_and_waits() {
    let mut h = H::new();
    to_connect(&mut h);
    h.click(ids::PAIR_LISTEN);
    let listen = h.call(|r| matches!(r, InstallerRequest::PairListen { .. }));
    h.ack(&listen);
    h.pair_poll(
        r#"{"ok":true,"result":{"phase":"failed","sas":null,"candidates":[],"peer":null,"error":"the pairing window closed without a connection"}}"#,
    );
    let row = h.row(live::steps::PAIR);
    assert_eq!(row.state, RowState::Failed);
    assert_eq!(row.label, "Pairing didn't finish");
    assert!(row.detail.contains("closed without a connection"));
    assert_eq!(
        h.button(ids::retry(live::steps::PAIR)).map(|b| b.kind),
        Some(ButtonKind::Primary)
    );
}

#[test]
fn a_refused_automatic_window_follows_the_pairing_the_agent_already_runs() {
    let mut h = H::new();
    to_connect(&mut h);
    let listen = h.until_listen();
    h.reply(&listen, Err(CallFailure::Refused(AgentRefusal::Other)));
    // The agent already had a window open (an earlier visit): setup follows it.
    h.pair_poll(
        r#"{"ok":true,"result":{"phase":"listening","sas":null,"candidates":[],"peer":null,"error":null}}"#,
    );
    assert_ne!(h.row(live::steps::PAIR).state, RowState::Failed);
    h.pair_poll(
        r#"{"ok":true,"result":{"phase":"confirm","sas":"111 222","candidates":[],"peer":"mac-studio","error":null}}"#,
    );
    assert_eq!(h.view().screen, ScreenId::MatchNumbers);
}

#[test]
fn while_waiting_to_be_found_the_person_can_still_type_an_address_or_go_back() {
    let mut h = H::new();
    to_connect(&mut h);
    let listen = h.until_listen();
    h.ack(&listen);
    h.pair_poll(
        r#"{"ok":true,"result":{"phase":"listening","sas":null,"candidates":[],"peer":null,"error":null}}"#,
    );
    assert_eq!(h.button(ids::BACK).map(|b| b.enabled), Some(true));
    assert_eq!(
        h.button(ids::PAIR_MANUAL).map(|b| b.kind),
        Some(ButtonKind::Link)
    );
    h.click(ids::PAIR_MANUAL);
    h.status_reply();
    assert_eq!(h.row(live::steps::PAIR).state, RowState::NeedsAction);
    assert!(
        h.view()
            .fields
            .iter()
            .any(|f| matches!(f, FieldView::PeerAddress { .. }))
    );
    for _ in 0..10 {
        h.advance(500);
        h.tick();
    }
    assert!(
        !h.has_call(|r| *r == InstallerRequest::PairStatus),
        "the abandoned window is no longer followed"
    );
    assert!(!h.has_call(|r| matches!(r, InstallerRequest::PairListen { .. })));

    let mut h = H::new();
    to_connect(&mut h);
    let listen = h.until_listen();
    h.ack(&listen);
    h.click(ids::BACK);
    assert_ne!(h.view().screen, ScreenId::Connect);
}

fn hiding_description() -> PlatformDescription {
    let mut d = description();
    d.hiding_choice = true;
    d.source_policy = TutorialSourcePolicy::MacMirror;
    d
}

#[test]
fn the_hiding_choice_is_never_preselected_and_apply_restarts_unless_something_is_shared() {
    for busy in [false, true] {
        let mut h = H::with(hiding_description());
        if busy {
            h.status["result"]["installer"]["audio"]["active_peers"] = json!([peer().to_string()]);
        }
        h.install();
        h.status_reply();
        h.next();
        assert_eq!(h.view().screen, ScreenId::HidingChoice);
        h.status_reply();
        assert_eq!(h.view().hiding_choice, None, "D7: nothing is preselected");
        let apply = h.button(ids::HIDING_APPLY).cloned().expect("apply");
        assert!(!apply.enabled, "D7: no choice, no apply");
        assert_eq!(apply.kind, ButtonKind::Primary);
        let revision = h.view().revision;
        h.act(revision, WizardIntent::ChooseHiding(HidingChoice::Hide));
        h.click(ids::HIDING_APPLY);
        let update = h.call(|r| matches!(r, InstallerRequest::SettingsUpdate { .. }));
        let InstallerRequest::SettingsUpdate {
            mac_virtual_display,
            ..
        } = update.request
        else {
            unreachable!()
        };
        assert!(mac_virtual_display);
        let saved = decode_reply(
            &update.request,
            br#"{"ok":true,"result":{"revision":"aaaaaaaaaaaaaaaa","restart_required":true}}"#,
            AgentPlatform::Linux,
        )
        .unwrap();
        h.reply(&update, Ok(saved));
        if busy {
            // Something is shared right now: the restart asks, explained, instead of cutting
            // it short.
            assert!(!h.has_call(|r| *r == InstallerRequest::Restart));
            assert_eq!(
                h.button(ids::HIDING_RESTART).map(|b| b.kind),
                Some(ButtonKind::Primary)
            );
            assert!(h.view().message.contains("ends what is shared"));
            h.click(ids::HIDING_RESTART);
        }
        // Otherwise the Apply click, which said it restarts Crosspane, was the consent.
        let _ = h.call(|r| *r == InstallerRequest::Restart);
    }
}

#[test]
fn typing_an_address_stops_the_automatic_window() {
    let mut h = H::new();
    to_connect(&mut h);
    h.click(ids::PAIR_MANUAL);
    assert!(
        h.view()
            .fields
            .iter()
            .any(|f| matches!(f, FieldView::PeerAddress { .. }))
    );
    assert_eq!(h.button(ids::PAIR_JOIN).map(|b| b.enabled), Some(false));
    for _ in 0..20 {
        h.advance(500);
        h.tick();
    }
    assert!(!h.has_call(|r| matches!(r, InstallerRequest::PairListen { .. })));
    // Back to searching: the automatic window comes back.
    h.click(ids::PAIR_MANUAL);
    let _ = h.until_listen();
}

fn to_grants(h: &mut H) {
    to_connect(h);
    h.paired_peer(&[]);
    h.status_reply();
    h.status_reply();
    assert_eq!(h.row(live::steps::PAIR).state, RowState::Verified);
    h.next();
    assert_eq!(h.view().screen, ScreenId::Grants);
    h.status_reply();
}

#[test]
fn grants_are_explicit_toggles_and_verify_only_from_a_later_status() {
    let mut h = H::new();
    to_grants(&mut h);
    assert_eq!(h.row(live::steps::GRANTS).state, RowState::NeedsAction);
    let toggles: Vec<_> = h
        .view()
        .fields
        .iter()
        .filter_map(|f| match f {
            FieldView::Toggle {
                id,
                role: ToggleRole::Grant,
                checked,
                ..
            } => Some((*id, *checked)),
            _ => None,
        })
        .collect();
    assert_eq!(toggles.len(), 5);
    assert!(
        toggles.iter().all(|(_, checked)| !checked),
        "never pre-checked"
    );
    for (id, _) in &toggles {
        let revision = h.view().revision;
        h.act(
            revision,
            WizardIntent::SetToggle {
                field: *id,
                checked: true,
            },
        );
    }
    h.click(ids::GRANTS_APPLY);
    let mut allowed = BTreeSet::new();
    for _ in 0..5 {
        let call = h.call(|r| matches!(r, InstallerRequest::Allow { .. }));
        let InstallerRequest::Allow {
            peer: p,
            capability,
            allow,
        } = call.request.clone()
        else {
            unreachable!()
        };
        assert_eq!(p, peer());
        assert!(allow);
        allowed.insert(format!("{capability:?}"));
        h.ack(&call);
    }
    assert_eq!(allowed.len(), 5);
    assert_ne!(h.row(live::steps::GRANTS).state, RowState::Verified);
    h.paired_peer(&ALL_GRANTS);
    h.status_reply();
    assert_eq!(h.row(live::steps::GRANTS).state, RowState::Verified);
}

#[test]
fn layout_apply_is_busy_until_committed_status_and_failure_reverts() {
    let mut h = H::new();
    to_grants(&mut h);
    h.paired_peer(&ALL_GRANTS);
    h.status_reply();
    h.status_reply();
    assert_eq!(h.row(live::steps::GRANTS).state, RowState::Verified);
    h.next();
    assert_eq!(h.view().screen, ScreenId::Layout);
    h.placements();
    h.status_reply();
    let layout = h.view().layout.clone().expect("layout preview");
    assert_eq!(layout.confirmed.len(), 2);
    assert!(!layout.busy);
    let peer_key = layout.peer_order[0].clone();
    let revision = h.view().revision;
    h.act(
        revision,
        WizardIntent::Layout(LayoutAction::Apply(vec![PlacementIntent {
            node: peer_key.clone(),
            display: 1,
            origin_mm: [0.0, 340.0],
        }])),
    );
    let place = h.call(|r| matches!(r, InstallerRequest::Place { .. }));
    let InstallerRequest::Place { placements } = &place.request else {
        unreachable!()
    };
    assert_eq!(placements.len(), 1);
    assert_eq!(placements[0].node, peer());
    assert!(h.view().layout.as_ref().unwrap().busy);
    h.reply(&place, Err(CallFailure::Unavailable));
    assert!(h.effects.contains(&ShellEffect::RevertLayout));
    assert!(!h.view().layout.as_ref().unwrap().busy);
    h.effects.clear();
    h.click(ids::retry(live::steps::LAYOUT));
    h.status_reply();
    let revision = h.view().revision;
    h.act(
        revision,
        WizardIntent::Layout(LayoutAction::Apply(vec![PlacementIntent {
            node: peer_key,
            display: 1,
            origin_mm: [0.0, 340.0],
        }])),
    );
    let place = h.call(|r| matches!(r, InstallerRequest::Place { .. }));
    h.ack(&place);
    assert!(
        h.view().layout.as_ref().unwrap().busy,
        "ack is not commitment"
    );
    h.status["result"]["layout"][1]["origin_mm"] = json!([0.0, 340.0]);
    h.status["result"]["layout"][1]["version"] = json!(2);
    h.status["result"]["installer"]["epochs"]["layout"] = json!(2);
    h.status_reply();
    assert_eq!(h.row(live::steps::LAYOUT).state, RowState::Verified);
    assert!(!h.view().layout.as_ref().unwrap().busy);
    assert!(h.effects.contains(&ShellEffect::FollowLayout));
}

#[test]
fn unknown_layout_keys_are_refused_without_a_place_call() {
    let mut h = H::new();
    to_grants(&mut h);
    h.paired_peer(&ALL_GRANTS);
    h.status_reply();
    h.status_reply();
    h.next();
    h.placements();
    h.status_reply();
    let revision = h.view().revision;
    h.act(
        revision,
        WizardIntent::Layout(LayoutAction::Apply(vec![PlacementIntent {
            node: "forged".into(),
            display: 1,
            origin_mm: [0.0, 0.0],
        }])),
    );
    assert!(!h.has_call(|r| matches!(r, InstallerRequest::Place { .. })));
    assert!(h.effects.contains(&ShellEffect::RevertLayout));
}

#[test]
fn removal_screen_inspects_and_never_mutates_without_a_previewed_confirmation() {
    let mut h = H::new();
    h.tick();
    h.click(ids::REMOVE_OR_REPAIR);
    assert_eq!(h.view().screen, ScreenId::RepairRemove);
    let jobs = h.take_jobs();
    let inspect = jobs
        .iter()
        .find_map(|j| match j {
            NativeJob::Maintenance(MaintenanceRequest::Inspect { id }) => Some(*id),
            _ => None,
        })
        .expect("inspect");
    h.native
        .borrow_mut()
        .reports
        .push(NativeReport::Maintenance(MaintenanceReport::Inspected {
            id: inspect,
            uninstall: live::Availability::Available,
            repair: live::Availability::NotAvailableYet(
                "Repair isn't available in this build yet.".into(),
            ),
            choices: vec![live::RemovalChoice {
                id: 1,
                role: ToggleRole::DeleteIdentity,
                label: "Also forget this computer's pairings".into(),
                checked: false,
                enabled: true,
            }],
        }));
    h.tick();
    assert!(!h.button(ids::REPAIR).unwrap().enabled);
    assert!(h.button(ids::REMOVE_CONFIRM).is_none());
    h.click(ids::REMOVE_REVIEW);
    let plan = h
        .take_jobs()
        .into_iter()
        .find_map(|j| match j {
            NativeJob::Maintenance(MaintenanceRequest::PlanUninstall { id, choices, .. }) => {
                Some((id, choices))
            }
            _ => None,
        })
        .expect("plan");
    assert_eq!(plan.1, vec![(1, false)]);
    h.native
        .borrow_mut()
        .reports
        .push(NativeReport::Maintenance(MaintenanceReport::Planned {
            id: plan.0,
            preview: "Stop and remove the agent; keep pairings".into(),
        }));
    h.tick();
    assert!(h.view().message.contains("keep pairings"));
    let revision = h.view().revision;
    h.click(ids::REMOVE_CONFIRM);
    let confirm = h
        .take_jobs()
        .into_iter()
        .find_map(|j| match j {
            NativeJob::Maintenance(MaintenanceRequest::ConfirmUninstall {
                id, revision, ..
            }) => Some((id, revision)),
            _ => None,
        })
        .expect("confirm");
    assert_eq!(confirm, (plan.0, revision));
}

#[test]
fn close_shuts_the_platform_down() {
    let mut h = H::new();
    h.tick();
    let revision = h.view().revision;
    assert!(h.act(revision, WizardIntent::Close));
    assert!(h.native.borrow().shutdown);
}

// ---- practice -------------------------------------------------------------------------------

impl H {
    fn counter(&mut self, name: &str, value: u64) {
        self.status["result"]["installer"]["peers"][0]["counters"][name] = json!(value);
    }
    fn bump(&mut self, name: &str, by: u64) {
        let now = self.status["result"]["installer"]["peers"][0]["counters"][name]
            .as_u64()
            .unwrap_or(0);
        self.counter(name, now + by);
    }
    fn practice_row(&self, role: TutorialRole) -> RowView {
        self.row(live::steps::practice(role))
    }
    fn confirm(&mut self, c: HumanConfirmation) {
        self.click(ids::confirm(c));
    }
    fn begin_practice(&mut self, role: TutorialRole) {
        self.click(ids::practice_start(role));
        // The first answer may belong to a poll that was already in flight; the second is the
        // sequencer's own baseline.
        self.status_reply();
        self.status_reply();
    }
    /// The fixture command the controller submitted, if any.
    fn fixture_command(
        &mut self,
        matches: impl Fn(&FixtureCommand) -> bool,
    ) -> Option<FixtureCall> {
        let mut state = self.fixtures.borrow_mut();
        let i = state.calls.iter().position(|c| matches(&c.command))?;
        Some(state.calls.remove(i))
    }
    /// Let the fixture child "start" and deliver its receipt for `call`.
    fn fixture_reply(&mut self, call: &FixtureCall, result: Result<FixtureEvent, FixtureError>) {
        self.fixture_sequence += 1;
        let receipt = FixtureReceipt {
            received_at_ms: self.now() + 1,
            message: FixtureMessage {
                call_id: Some(call.id),
                attempt: call.attempt,
                sequence: self.fixture_sequence,
                result,
            },
        };
        self.fixtures.borrow_mut().receipts.push(receipt);
        self.advance(2);
        self.tick();
    }
    fn fixture_event(&mut self, attempt: AttemptId, event: FixtureEvent) {
        self.fixture_sequence += 1;
        let receipt = FixtureReceipt {
            received_at_ms: self.now() + 1,
            message: FixtureMessage {
                call_id: None,
                attempt,
                sequence: self.fixture_sequence,
                result: Ok(event),
            },
        };
        self.fixtures.borrow_mut().receipts.push(receipt);
        self.advance(2);
        self.tick();
    }
    /// Make the fixture ready and run until the sequencer submits the Open command.
    fn fixture_open(&mut self) -> FixtureCall {
        self.fixtures.borrow_mut().readiness = Some(FixtureReadiness::Ready);
        for _ in 0..8 {
            self.advance(100);
            self.tick();
            if let Some(call) = self.fixture_command(|c| matches!(c, FixtureCommand::Open { .. })) {
                self.fixture_reply(
                    &call,
                    Ok(FixtureEvent::Opened {
                        fixture: FixtureId(10),
                        pid: 123,
                        window: WindowId(100),
                        label: "own fixture A".into(),
                    }),
                );
                return call;
            }
        }
        panic!("the sequencer never asked the fixture to open");
    }
}

fn to_practice(h: &mut H) {
    to_grants(h);
    h.paired_peer(&ALL_GRANTS);
    h.status_reply();
    h.status_reply();
    assert_eq!(h.row(live::steps::GRANTS).state, RowState::Verified);
    h.next();
    assert_eq!(h.view().screen, ScreenId::Layout);
    h.placements();
    h.status_reply();
    h.click(ids::LAYOUT_ACCEPT);
    h.status_reply();
    h.status_reply();
    assert_eq!(h.row(live::steps::LAYOUT).state, RowState::Verified);
    h.next();
    assert_eq!(h.view().screen, ScreenId::Practice);
}

#[test]
fn practice_starts_only_on_an_explicit_click_and_never_before_prerequisites() {
    let mut h = H::new();
    to_grants(&mut h);
    h.paired_peer(&ALL_GRANTS);
    h.status_reply();
    h.status_reply();
    h.next();
    h.placements();
    h.status_reply();
    // Layout is not committed yet, so the screen stays and no practice can start.
    for _ in 0..5 {
        h.advance(500);
        h.tick();
    }
    assert_eq!(h.view().screen, ScreenId::Layout);
    assert!(h.button(ids::NEXT).is_none());
    let mut h = H::new();
    to_practice(&mut h);
    for _ in 0..6 {
        h.advance(600);
        h.tick();
    }
    assert!(h.fixtures.borrow().launches.is_empty());
    assert!(
        !h.has_call(|r| matches!(
            r,
            InstallerRequest::Project { .. }
                | InstallerRequest::Pull { .. }
                | InstallerRequest::Release
        )),
        "nothing is projected or released until the person starts a practice"
    );
    assert_eq!(h.summary(), SummaryView::InstalledWaiting);
}

#[test]
fn menu_practice_needs_the_spawn_counter_the_tray_and_the_human() {
    let mut h = H::new();
    to_practice(&mut h);
    h.begin_practice(TutorialRole::Menu);
    h.confirm(HumanConfirmation::TrayAndSettingsVisible);
    assert_ne!(h.practice_row(TutorialRole::Menu).state, RowState::Verified);
    h.status["result"]["installer"]["settings_opened"] = json!(1);
    h.status_reply();
    h.status_reply();
    assert_eq!(h.practice_row(TutorialRole::Menu).state, RowState::Verified);
    assert!(h.practice_row(TutorialRole::Menu).human_confirmed);
}

#[test]
fn e1_controller_passes_with_the_chord_release_and_not_with_a_command_release() {
    let mut command = H::new();
    to_practice(&mut command);
    command.begin_practice(TutorialRole::E1Controller);
    command.bump("e1_controller_started", 1);
    command.bump("e1_controller_ended", 1);
    command.bump("e1_command_releases", 1);
    command.status_reply();
    let _ = command
        .view()
        .buttons
        .iter()
        .find(|b| b.id == ids::confirm(HumanConfirmation::RemotePracticeAndHud));
    if command
        .button(ids::confirm(HumanConfirmation::RemotePracticeAndHud))
        .is_some_and(|b| b.enabled)
    {
        command.confirm(HumanConfirmation::RemotePracticeAndHud);
    }
    assert_ne!(
        command.practice_row(TutorialRole::E1Controller).state,
        RowState::Verified,
        "a command release is not the chord"
    );

    let mut h = H::new();
    to_practice(&mut h);
    h.begin_practice(TutorialRole::E1Controller);
    h.bump("e1_controller_started", 1);
    h.bump("e1_controller_ended", 1);
    h.bump("e1_chord_releases", 1);
    h.status_reply();
    h.confirm(HumanConfirmation::RemotePracticeAndHud);
    assert_eq!(
        h.practice_row(TutorialRole::E1Controller).state,
        RowState::Verified
    );
}

fn snapshot(phase: Option<u64>, clicks: u64, facts: OwnWindowFacts) -> FixtureEvent {
    FixtureEvent::Snapshot {
        snapshot: FixtureSnapshot {
            fixture: FixtureId(10),
            window: WindowId(100),
            phase: phase.map(PhaseId),
            pattern_ticks: 10,
            target_clicks: clicks,
            window_facts: facts,
            tone: OwnToneState::Stopped,
        },
    }
}

fn home() -> OwnWindowFacts {
    OwnWindowFacts::Present {
        visible_on_user_workspace: Some(true),
        on_initial_display: Some(true),
    }
}

impl H {
    fn attempt(&self) -> AttemptId {
        *self
            .fixtures
            .borrow()
            .launches
            .last()
            .expect("a launched attempt")
    }
    fn windows_reply(&mut self, request: InstallerRequest, body: &str) {
        let call = self.call(|r| *r == request);
        let decoded = decode_reply(&request, body.as_bytes(), AgentPlatform::Linux).unwrap();
        self.reply(&call, Ok(decoded));
    }
    fn projection(&mut self, source: NodeId, on: bool) {
        self.status["result"]["projections"] = if on {
            json!([{"source": source.short(), "projection": 55, "text": "sensitive title",
                "received": {"frames": 999999, "bytes": 999999}}])
        } else {
            json!([])
        };
    }
    fn set(&mut self, path: &[&str], value: Value) {
        let mut at = &mut self.status["result"];
        for key in path {
            at = &mut at[*key];
        }
        *at = value;
    }
    fn fixture_close(&mut self) {
        let call = self
            .fixture_command(|c| matches!(c, FixtureCommand::Close { .. }))
            .expect("the owned fixture is closed");
        self.fixture_reply(
            &call,
            Ok(FixtureEvent::Closed {
                fixture: FixtureId(10),
            }),
        );
    }
    fn fixture_home(&mut self) {
        let call = self
            .fixture_command(|c| matches!(c, FixtureCommand::ObserveWindow { .. }))
            .expect("the controller observes the fixture's home window");
        self.fixture_reply(&call, Ok(snapshot(None, 0, home())));
    }
}

fn run_e1_target(h: &mut H) {
    h.begin_practice(TutorialRole::E1Target);
    h.fixture_open();
    let arm = h
        .fixture_command(|c| matches!(c, FixtureCommand::ArmTarget { .. }))
        .expect("the target is armed");
    let FixtureCommand::ArmTarget { phase, .. } = arm.command.clone() else {
        unreachable!()
    };
    h.fixture_reply(
        &arm,
        Ok(FixtureEvent::TargetArmed {
            fixture: FixtureId(10),
            phase,
        }),
    );
    let attempt = h.attempt();
    h.fixture_event(attempt, snapshot(Some(phase.0), 1, OwnWindowFacts::Unknown));
    for name in [
        "e1_target_started",
        "e1_target_ended",
        "e1_injections_ok",
        "e1_hud_shows",
    ] {
        h.bump(name, 1);
    }
    h.status_reply();
    h.confirm(HumanConfirmation::ControllerCrossingAndRelease);
    h.fixture_close();
}

fn e2_start(h: &mut H, role: TutorialRole) {
    let source = matches!(
        role,
        TutorialRole::E2SourcePush | TutorialRole::E2SourcePull
    );
    h.begin_practice(role);
    match role {
        TutorialRole::E2SourcePush => {
            h.fixture_open();
            h.windows_reply(
                InstallerRequest::Windows,
                r#"{"ok":true,"result":[{"id":100,"app":"fixture","title":"sensitive","display":null,"size":[20.5,30.5]}]}"#,
            );
            let project = h.call(|r| matches!(r, InstallerRequest::Project { .. }));
            assert_eq!(
                project.request,
                InstallerRequest::Project {
                    window: WindowId(100),
                    peer: peer()
                },
                "only the owned fixture window is ever projected"
            );
            h.ack(&project);
        }
        TutorialRole::E2SourcePull => {
            h.fixture_open();
            h.confirm(HumanConfirmation::SourceMachineAndAttempt);
        }
        TutorialRole::E2DestinationPull => {
            h.windows_reply(
                InstallerRequest::WindowsFrom { peer: peer() },
                r#"{"ok":true,"result":[{"id":100,"app":"fixture","title":"advisory","size":[20,30]}]}"#,
            );
            h.click(ids::remote_window(0));
            h.confirm(HumanConfirmation::SourceMachineAndAttempt);
            let pull = h.call(|r| matches!(r, InstallerRequest::Pull { .. }));
            h.ack(&pull);
        }
        TutorialRole::E2DestinationPush => h.confirm(HumanConfirmation::SourceMachineAndAttempt),
        _ => unreachable!(),
    }
    h.projection(if source { local() } else { peer() }, true);
    let metric = if source {
        "e2_source_started"
    } else {
        "e2_dest_started"
    };
    h.bump(metric, 1);
    if source {
        h.set(&["installer", "recovery_pending"], json!(1));
        h.set(&["installer", "peers"], {
            let mut peers = h.status["result"]["installer"]["peers"].clone();
            peers[0]["last_source_parking"] = json!("twin");
            peers
        });
    }
    h.status_reply();
}

fn e2_end(h: &mut H, role: TutorialRole) {
    let source = matches!(
        role,
        TutorialRole::E2SourcePush | TutorialRole::E2SourcePull
    );
    h.confirm(HumanConfirmation::DestinationPatternInteractionAndClose);
    let ret = h.call(|r| matches!(r, InstallerRequest::Return { .. }));
    assert_eq!(
        ret.request,
        InstallerRequest::Return {
            projection: 55,
            source: (!source).then_some(peer()),
        }
    );
    h.ack(&ret);
    assert_ne!(
        h.practice_row(role).state,
        RowState::Verified,
        "the return acknowledgement is not completion"
    );
    h.projection(local(), false);
    h.bump(
        if source {
            "e2_source_returned"
        } else {
            "e2_dest_returned"
        },
        1,
    );
    h.set(&["installer", "recovery_pending"], json!(0));
    if !source {
        h.bump("e2_frames_presented", 10);
    }
    h.status_reply();
    if source {
        h.fixture_home();
        if h.fixtures
            .borrow()
            .calls
            .iter()
            .any(|c| matches!(c.command, FixtureCommand::Close { .. }))
        {
            h.fixture_close();
        }
    } else {
        h.confirm(HumanConfirmation::SourceRestored);
    }
}

fn run_e2(h: &mut H, role: TutorialRole) {
    e2_start(h, role);
    e2_end(h, role);
}

fn run_audio_sender(h: &mut H) {
    h.begin_practice(TutorialRole::AudioSender);
    h.fixture_open();
    h.click(ids::PLAY_TONE);
    let play = h
        .fixture_command(|c| matches!(c, FixtureCommand::PlayTone { .. }))
        .expect("the owned fixture plays its own tone");
    let FixtureCommand::PlayTone { output, .. } = &play.command else {
        unreachable!()
    };
    assert_eq!(output.peer, peer());
    h.fixture_reply(
        &play,
        Ok(FixtureEvent::ToneStarted {
            fixture: FixtureId(10),
            tone: ToneId(33),
        }),
    );
    h.set(&["installer", "audio", "active_peers"], json!([peer()]));
    h.status_reply();
    h.set(&["installer", "audio", "frames_sent"], json!(10));
    h.status_reply();
    let stop = h
        .fixture_command(|c| matches!(c, FixtureCommand::StopTone { .. }))
        .expect("the tone is stopped");
    h.fixture_reply(
        &stop,
        Ok(FixtureEvent::ToneStopped {
            fixture: FixtureId(10),
            tone: ToneId(33),
        }),
    );
    h.confirm(HumanConfirmation::FarSpeakerHeard);
    h.confirm(HumanConfirmation::ExclusiveAudioInterval);
    h.fixture_close();
}

fn run_audio_receiver(h: &mut H) {
    h.begin_practice(TutorialRole::AudioReceiver);
    h.confirm(HumanConfirmation::SelectedSourceToneStarted);
    h.set(&["installer", "audio", "active_peers"], json!([peer()]));
    h.status_reply();
    h.set(&["installer", "audio", "frames_played"], json!(10));
    h.status_reply();
    h.confirm(HumanConfirmation::LocalSpeakerHeard);
    h.confirm(HumanConfirmation::ExclusiveAudioInterval);
}

fn run_menu(h: &mut H) {
    h.begin_practice(TutorialRole::Menu);
    h.set(&["installer", "settings_opened"], json!(1));
    h.status_reply();
    h.confirm(HumanConfirmation::TrayAndSettingsVisible);
}

fn run_e1_controller(h: &mut H) {
    h.begin_practice(TutorialRole::E1Controller);
    h.bump("e1_controller_started", 1);
    h.bump("e1_controller_ended", 1);
    h.bump("e1_chord_releases", 1);
    h.status_reply();
    h.confirm(HumanConfirmation::RemotePracticeAndHud);
}

fn with_extra_step(mut extra: NativeStep) -> H {
    let mut d = description();
    extra.prerequisites = vec![AGENT];
    d.steps.push(extra);
    d.connect_after.push(StepId(30));
    d.practice_after.push(StepId(30));
    H::with(d)
}

#[test]
fn a_status_step_holds_its_native_job_until_a_status_issued_after_it_arrives() {
    let mut extra = native(StepId(30), &[], ScreenId::Permissions, false);
    extra.uses_status = true;
    let mut h = with_extra_step(extra);
    h.install();
    h.status_reply();
    h.next();
    assert_eq!(h.view().screen, ScreenId::Permissions);
    // The Detect job is not handed to the platform until a fresh Status rides with it.
    h.tick();
    assert!(
        !h.native.borrow().jobs.iter().any(|j| matches!(
            j,
            NativeJob::Step { job, .. } if job.step == StepId(30)
        )),
        "held for status"
    );
    h.status_reply();
    let jobs = h.take_jobs();
    let (job, status) = jobs
        .into_iter()
        .find_map(|j| match j {
            NativeJob::Step { job, status, .. } if job.step == StepId(30) => Some((job, status)),
            _ => None,
        })
        .expect("the held job is released");
    assert_eq!(job.stage, JobStage::Detect);
    assert!(status.is_some(), "the Status that released it rides along");
    // A status that was already in flight when the job began is not evidence for it.
    h.report(
        &job,
        NativeOutcome::Detected {
            needs_action: false,
        },
    );
    h.tick();
    let before = h.take_jobs();
    assert!(before.iter().all(|j| !matches!(j, NativeJob::Step { status: Some(_), job, .. } if job.step == StepId(30) && job.stage == JobStage::Verify)));
}

#[test]
fn an_agent_applied_step_sends_its_request_after_consent_and_is_verified_only_from_status() {
    let mut extra = native(StepId(30), &[], ScreenId::Permissions, false);
    extra.uses_status = true;
    extra.agent_apply = Some(live::AgentApply::AskPermissions);
    let mut h = with_extra_step(extra);
    h.install();
    h.status_reply();
    h.next();
    h.status_reply();
    let (detect, status) = loop_for_job(&mut h, StepId(30), JobStage::Detect);
    assert!(status.is_some());
    h.report(&detect, NativeOutcome::Detected { needs_action: true });
    // Planning against a running agent also waits for a Status issued after it began.
    let (plan, plan_status) = loop_for_job(&mut h, StepId(30), JobStage::Plan);
    assert!(plan_status.is_some());
    h.report(
        &plan,
        NativeOutcome::Planned {
            preview: "Crosspane asks macOS for permissions".into(),
        },
    );
    assert!(
        !h.has_call(|r| *r == InstallerRequest::AskPermissions),
        "nothing is asked before consent"
    );
    h.click(ids::consent(StepId(30)));
    let ask = h.call(|r| *r == InstallerRequest::AskPermissions);
    assert!(
        h.take_jobs().iter().all(|j| !matches!(
            j,
            NativeJob::Step { job, .. } if job.step == StepId(30) && job.stage == JobStage::Apply
        )),
        "the platform never sees an agent-applied Apply"
    );
    h.ack(&ask);
    assert_ne!(
        h.row(StepId(30)).state,
        RowState::Verified,
        "the acknowledgement is not completion"
    );
    // Only a later status verifies, and only through the platform's own Verify.
    h.status_reply();
    let (verify, evidence) = loop_for_job(&mut h, StepId(30), JobStage::Verify);
    assert!(evidence.is_some());
    let live = h.live();
    h.report(&verify, live);
    assert_eq!(h.row(StepId(30)).state, RowState::Verified);
}

#[test]
fn a_refused_agent_request_waits_for_the_person_and_an_unknown_one_is_detected_again() {
    for (failure, wait) in [
        (CallFailure::Refused(AgentRefusal::Other), true),
        (CallFailure::TimeoutOutcomeUnknown, false),
    ] {
        let mut extra = native(StepId(30), &[], ScreenId::Permissions, false);
        extra.agent_apply = Some(live::AgentApply::Restart);
        let mut h = with_extra_step(extra);
        h.install();
        h.status_reply();
        h.next();
        let (detect, _) = loop_for_job(&mut h, StepId(30), JobStage::Detect);
        h.report(&detect, NativeOutcome::Detected { needs_action: true });
        let (plan, _) = h.job(StepId(30), JobStage::Plan);
        h.report(
            &plan,
            NativeOutcome::Planned {
                preview: "restart".into(),
            },
        );
        h.click(ids::consent(StepId(30)));
        let call = h.call(|r| *r == InstallerRequest::Restart);
        h.reply(&call, Err(failure));
        if wait {
            assert_eq!(h.row(StepId(30)).state, RowState::Waiting);
        } else {
            // Unknown outcome re-detects before anything is offered again.
            let _ = loop_for_job(&mut h, StepId(30), JobStage::Detect);
        }
    }
}

/// Tick until the platform has been handed `step`'s job for `stage`; return it with its status.
fn loop_for_job(
    h: &mut H,
    step: StepId,
    stage: JobStage,
) -> (JobIntent, Option<live::StatusEvidence>) {
    for _ in 0..40 {
        let found = {
            let mut native = h.native.borrow_mut();
            let at = native.jobs.iter().position(|j| {
                matches!(j, NativeJob::Step { job, .. } if job.step == step && job.stage == stage)
            });
            at.map(|i| native.jobs.remove(i))
        };
        if let Some(NativeJob::Step { job, status, .. }) = found {
            return (job, status);
        }
        h.advance(100);
        h.tick();
        if h.has_call(|r| *r == InstallerRequest::Status) {
            h.status_reply();
        }
    }
    panic!("no {stage:?} job for step {}", step.0);
}

#[test]
fn e1_target_needs_its_armed_click_counters_and_human_confirmation() {
    let mut h = H::new();
    to_practice(&mut h);
    run_e1_target(&mut h);
    h.status_reply();
    assert_eq!(
        h.practice_row(TutorialRole::E1Target).state,
        RowState::Verified
    );
    assert!(
        h.fixtures.borrow().retired >= 1,
        "the owned fixture is retired"
    );
}

#[test]
fn all_four_e2_roles_pass_with_distinct_owned_attempts_and_verified_returns() {
    for role in [
        TutorialRole::E2SourcePush,
        TutorialRole::E2DestinationPush,
        TutorialRole::E2SourcePull,
        TutorialRole::E2DestinationPull,
    ] {
        let mut h = H::new();
        to_practice(&mut h);
        run_e2(&mut h, role);
        h.status_reply();
        assert_eq!(
            h.practice_row(role).state,
            RowState::Verified,
            "{role:?}: {}",
            h.practice_row(role).detail
        );
    }
}

#[test]
fn audio_sender_and_receiver_pass_with_hearing_and_the_global_counter_limit_is_shown() {
    let mut h = H::new();
    to_practice(&mut h);
    run_audio_sender(&mut h);
    h.status_reply();
    assert_eq!(
        h.practice_row(TutorialRole::AudioSender).state,
        RowState::Verified,
        "{}",
        h.practice_row(TutorialRole::AudioSender).detail
    );
    let mut h = H::new();
    to_practice(&mut h);
    run_audio_receiver(&mut h);
    h.status_reply();
    assert_eq!(
        h.practice_row(TutorialRole::AudioReceiver).state,
        RowState::Verified
    );
    // Audio counters are global and sampled; both audio rows say so even before practice.
    for role in [TutorialRole::AudioSender, TutorialRole::AudioReceiver] {
        let detail = h.practice_row(role).detail;
        assert!(
            detail.contains("computer-wide") && detail.contains("hear"),
            "audio rows carry the attribution limit: {detail}"
        );
    }
}

#[test]
fn all_nine_roles_then_fresh_final_health_reach_ready_and_readiness_lapses_after_five_seconds() {
    let mut h = H::new();
    to_practice(&mut h);
    run_e1_controller(&mut h);
    run_e1_target(&mut h);
    run_e2(&mut h, TutorialRole::E2SourcePush);
    run_e2(&mut h, TutorialRole::E2DestinationPush);
    run_e2(&mut h, TutorialRole::E2SourcePull);
    run_e2(&mut h, TutorialRole::E2DestinationPull);
    run_audio_sender(&mut h);
    // Each role is verified before the next starts; the summary is still waiting until the last.
    assert_eq!(h.summary(), SummaryView::InstalledWaiting);
    run_audio_receiver(&mut h);
    assert_eq!(h.summary(), SummaryView::InstalledWaiting);
    run_menu(&mut h);
    for role in live::PRACTICE_ROLES {
        assert_eq!(h.practice_row(role).state, RowState::Verified, "{role:?}");
    }
    h.next();
    assert_eq!(h.view().screen, ScreenId::Summary);
    // The final fresh-health step is checked only by a status issued after every role passed.
    h.status_reply();
    h.status_reply();
    assert_eq!(h.summary(), SummaryView::WorkspaceReady);
    assert_eq!(h.row(live::steps::FINAL).state, RowState::Verified);
    // Without a fresh observation, readiness lapses honestly after five seconds.
    h.advance(5_500);
    h.c.tick();
    assert_ne!(
        h.summary(),
        SummaryView::WorkspaceReady,
        "stale health is never green"
    );
    // Renewing it needs a new status, not a button.
    h.status_reply();
    h.status_reply();
    assert_eq!(h.summary(), SummaryView::WorkspaceReady);
}

// ---- review (Opus) regressions ---------------------------------------------------------------

fn other_peer() -> NodeId {
    NodeId([0x33; 32])
}

fn other_peer_json(connected: bool) -> Value {
    let mut p = peer_json(&[]);
    p["node"] = json!(other_peer().to_string());
    p["name"] = json!("older peer");
    p["connected"] = json!(connected);
    p
}

#[test]
fn a_new_pairing_never_selects_an_older_peer_that_happens_to_reconnect() {
    let mut h = H::new();
    // An older, already-paired computer is known but offline when pairing starts.
    h.status["result"]["installer"]["peers"] = json!([other_peer_json(false)]);
    to_connect(&mut h);
    assert_eq!(h.row(live::steps::PAIR).state, RowState::NeedsAction);
    h.click(ids::PAIR_LISTEN);
    let listen = h.call(|r| matches!(r, InstallerRequest::PairListen { .. }));
    h.ack(&listen);
    h.advance(600);
    h.tick();
    let poll = h.call(|r| *r == InstallerRequest::PairStatus);
    h.pair_status(
        &poll,
        r#"{"ok":true,"result":{"phase":"paired","sas":null,"candidates":[],"peer":"sensitive peer","error":null}}"#,
    );
    // The older peer reconnects before the newly paired one does.
    h.status["result"]["installer"]["peers"] = json!([other_peer_json(true)]);
    h.status_reply();
    h.status_reply();
    assert_ne!(
        h.row(live::steps::PAIR).state,
        RowState::Verified,
        "the older peer is not the computer that was just paired"
    );
    // The newly paired computer connects: it, and only it, is selected.
    let mut fresh = peer_json(&[]);
    fresh["connected"] = json!(true);
    h.status["result"]["installer"]["peers"] = json!([other_peer_json(true), fresh]);
    h.status_reply();
    h.status_reply();
    assert_eq!(h.row(live::steps::PAIR).state, RowState::Verified);
    assert_eq!(h.view().peer.as_deref(), Some("sensitive peer"));
}

#[test]
fn removal_choices_are_frozen_while_their_plan_is_prepared() {
    let mut h = H::new();
    h.tick();
    h.click(ids::REMOVE_OR_REPAIR);
    let inspect = h
        .take_jobs()
        .iter()
        .find_map(|j| match j {
            NativeJob::Maintenance(MaintenanceRequest::Inspect { id }) => Some(*id),
            _ => None,
        })
        .expect("inspect");
    h.native
        .borrow_mut()
        .reports
        .push(NativeReport::Maintenance(MaintenanceReport::Inspected {
            id: inspect,
            uninstall: live::Availability::Available,
            repair: live::Availability::NotAvailableYet("not yet".into()),
            choices: vec![live::RemovalChoice {
                id: 1,
                role: ToggleRole::DeleteIdentity,
                label: "Also forget this computer's pairings".into(),
                checked: false,
                enabled: true,
            }],
        }));
    h.tick();
    h.click(ids::REMOVE_REVIEW);
    let plan = h
        .take_jobs()
        .into_iter()
        .find_map(|j| match j {
            NativeJob::Maintenance(MaintenanceRequest::PlanUninstall { id, .. }) => Some(id),
            _ => None,
        })
        .expect("plan");
    // The person flips a choice while the plan for the old choices is still being prepared.
    let field = ids::removal_field(1);
    let toggle_enabled = h
        .view()
        .fields
        .iter()
        .any(|f| matches!(f, FieldView::Toggle { id, enabled: true, .. } if *id == field));
    assert!(!toggle_enabled, "choices are frozen while planning");
    let revision = h.view().revision;
    h.act(
        revision,
        WizardIntent::SetToggle {
            field,
            checked: true,
        },
    );
    h.native
        .borrow_mut()
        .reports
        .push(NativeReport::Maintenance(MaintenanceReport::Planned {
            id: plan,
            preview: "Remove the agent; keep pairings".into(),
        }));
    h.tick();
    let checked = h
        .view()
        .fields
        .iter()
        .any(|f| matches!(f, FieldView::Toggle { id, checked: true, .. } if *id == field));
    assert!(
        !checked,
        "the shown plan always describes the choices that will be confirmed"
    );
    assert!(h.button(ids::REMOVE_CONFIRM).is_some());
    // A second, unrequested plan report never replaces the reviewed one.
    h.native
        .borrow_mut()
        .reports
        .push(NativeReport::Maintenance(MaintenanceReport::Planned {
            id: plan,
            preview: "something else entirely".into(),
        }));
    h.tick();
    assert!(!h.view().message.contains("something else"));
}

#[test]
fn a_window_close_is_refused_while_a_native_change_runs_and_allowed_after() {
    let mut h = H::new();
    h.tick();
    h.next();
    h.pass(SUPPORT);
    h.next();
    let (detect, _) = h.job(PAYLOAD, JobStage::Detect);
    h.report(&detect, NativeOutcome::Detected { needs_action: true });
    let (plan, _) = h.job(PAYLOAD, JobStage::Plan);
    h.report(
        &plan,
        NativeOutcome::Planned {
            preview: "Install".into(),
        },
    );
    let (apply, _) = h.job(PAYLOAD, JobStage::Apply);
    assert!(!h.c.request_close(), "closing would cut the install short");
    assert!(!h.native.borrow().shutdown);
    assert!(h.view().message.contains("Wait for the current change"));
    // Progress keeps the guard; a platform that goes silent past every deadline can be closed.
    h.advance(100_000);
    h.report(&apply, NativeOutcome::Progress);
    h.advance(100_000);
    h.tick();
    assert!(!h.c.request_close(), "progress was reported recently");
    h.report(&apply, NativeOutcome::Applied(ApplyOutcome::Applied));
    assert!(h.c.request_close(), "nothing is running any more");
    assert!(h.native.borrow().shutdown);
}

#[test]
fn a_silent_platform_never_makes_the_window_unclosable() {
    let mut h = H::new();
    h.tick();
    h.next();
    h.pass(SUPPORT);
    h.next();
    let (detect, _) = h.job(PAYLOAD, JobStage::Detect);
    h.report(&detect, NativeOutcome::Detected { needs_action: true });
    let (plan, _) = h.job(PAYLOAD, JobStage::Plan);
    h.report(
        &plan,
        NativeOutcome::Planned {
            preview: "Install".into(),
        },
    );
    let _ = h.job(PAYLOAD, JobStage::Apply);
    assert!(!h.c.request_close());
    h.advance(151_000);
    h.tick();
    assert!(h.c.request_close());
}

#[test]
fn an_answer_that_does_not_belong_to_the_jobs_stage_fails_it_instead_of_hanging() {
    let mut h = H::new();
    h.tick();
    h.next();
    h.pass(SUPPORT);
    h.next();
    let (detect, _) = h.job(PAYLOAD, JobStage::Detect);
    h.report(&detect, NativeOutcome::Detected { needs_action: true });
    let (plan, _) = h.job(PAYLOAD, JobStage::Plan);
    // A platform fault: a detection answer for a Plan job.
    h.report(
        &plan,
        NativeOutcome::Detected {
            needs_action: false,
        },
    );
    assert_eq!(h.row(PAYLOAD).state, RowState::Failed);
    assert!(h.button(ids::retry(PAYLOAD)).is_some_and(|b| b.enabled));
}

// ---- compatible repair ------------------------------------------------------------------------

fn maintenance_jobs(h: &mut H) -> Vec<MaintenanceRequest> {
    h.take_jobs()
        .into_iter()
        .filter_map(|j| match j {
            NativeJob::Maintenance(m) => Some(m),
            _ => None,
        })
        .collect()
}

fn platform_says(h: &mut H, report: MaintenanceReport) {
    h.native
        .borrow_mut()
        .reports
        .push(NativeReport::Maintenance(report));
    h.tick();
}

/// Open "Remove or repair" and answer its inspection with `repair`.
fn open_repair(h: &mut H, repair: live::Availability) -> MaintenanceId {
    h.tick();
    h.click(ids::REMOVE_OR_REPAIR);
    let id = maintenance_jobs(h)
        .into_iter()
        .find_map(|m| match m {
            MaintenanceRequest::Inspect { id } => Some(id),
            _ => None,
        })
        .expect("inspect");
    platform_says(
        h,
        MaintenanceReport::Inspected {
            id,
            uninstall: live::Availability::Available,
            repair,
            choices: Vec::new(),
        },
    );
    id
}

/// Click Review, answer the Status it waits for, and show the platform's preview.
fn review_repair(h: &mut H, id: MaintenanceId, plan: u64) {
    h.click(ids::REPAIR);
    assert!(
        maintenance_jobs(h).is_empty(),
        "the review waits for a Status issued after the click"
    );
    h.status_reply();
    let requests = maintenance_jobs(h);
    assert!(
        requests.iter().any(
            |m| matches!(m, MaintenanceRequest::PlanRepair { id: i, status: Some(_) } if *i == id)
        ),
        "{requests:?}"
    );
    platform_says(
        h,
        MaintenanceReport::RepairPlanned {
            id,
            plan,
            preview: "Repair puts back bin/crosspane-agent (different). Crosspane is stopped, then started again.".into(),
        },
    );
}

#[test]
fn repair_is_not_callable_unless_the_platform_says_it_is_available() {
    for repair in [
        live::Availability::NotAvailableYet("Not in this build.".into()),
        live::Availability::Unavailable(
            "Some files weren't put there by Crosspane. Remove Crosspane and install it again."
                .into(),
        ),
    ] {
        let mut h = H::new();
        let reason = match &repair {
            live::Availability::NotAvailableYet(r) | live::Availability::Unavailable(r) => {
                r.clone()
            }
            live::Availability::Available => unreachable!(),
        };
        open_repair(&mut h, repair);
        assert!(!h.button(ids::REPAIR).unwrap().enabled);
        assert!(h.view().message.contains(&reason), "{}", h.view().message);
        // Even a forced click starts nothing.
        let revision = h.view().revision;
        h.act(revision, WizardIntent::Button(ids::REPAIR));
        h.advance(13_000);
        h.tick();
        assert!(
            maintenance_jobs(&mut h)
                .iter()
                .all(|m| !matches!(m, MaintenanceRequest::PlanRepair { .. }))
        );
        assert!(h.button(ids::REPAIR_CONFIRM).is_none());
        assert!(h.button(ids::REPAIR_RESUME).is_none());
    }
}

#[test]
fn repair_is_planned_previewed_and_confirmed_by_plan_and_view_revision() {
    let mut h = H::new();
    let id = open_repair(&mut h, live::Availability::Available);
    assert!(h.button(ids::REPAIR).unwrap().enabled);
    assert!(
        h.button(ids::REPAIR_CONFIRM).is_none(),
        "no consent without a preview"
    );
    review_repair(&mut h, id, 77);
    assert!(
        h.view()
            .message
            .contains("Repair puts back bin/crosspane-agent")
    );
    assert!(!h.button(ids::REPAIR).unwrap().enabled);
    assert!(
        h.button(ids::REMOVE_REVIEW).is_none(),
        "removal isn't offered over a repair"
    );
    let revision = h.view().revision;
    // A click on a view that is no longer current is dropped.
    h.act(revision - 1, WizardIntent::Button(ids::REPAIR_CONFIRM));
    h.advance(13_000);
    h.tick();
    assert!(
        maintenance_jobs(&mut h)
            .iter()
            .all(|m| !matches!(m, MaintenanceRequest::ConfirmRepair { .. }))
    );
    // The real click carries the plan number and the revision, and a Status issued after it.
    h.click(ids::REPAIR_CONFIRM);
    assert!(
        maintenance_jobs(&mut h).is_empty(),
        "the confirmation waits for a fresh Status"
    );
    h.status_reply();
    let requests = maintenance_jobs(&mut h);
    assert!(
        requests.iter().any(|m| matches!(
            m,
            MaintenanceRequest::ConfirmRepair { id: i, plan: 77, revision: r, status: Some(_) }
                if *i == id && *r == revision
        )),
        "{requests:?}"
    );
    // While the change runs, nothing can be repeated or abandoned.
    assert!(h.view().message.contains("Repairing Crosspane"));
    assert!(h.button(ids::REPAIR_CONFIRM).is_none());
    assert!(h.button(ids::CLOSE).is_none());
    assert!(h.button(ids::BACK).is_none());
    assert!(!h.c.request_close());
    assert!(h.view().message.contains("Wait for the current change"));
    assert!(!h.native.borrow().shutdown);
}

#[test]
fn a_repair_click_that_gets_no_status_goes_ahead_without_one() {
    let mut h = H::new();
    let id = open_repair(&mut h, live::Availability::Available);
    h.click(ids::REPAIR);
    h.advance(13_000);
    h.tick();
    let requests = maintenance_jobs(&mut h);
    assert!(
        requests.iter().any(
            |m| matches!(m, MaintenanceRequest::PlanRepair { id: i, status: None } if *i == id)
        ),
        "{requests:?}"
    );
}

#[test]
fn the_repair_watches_the_new_agent_with_fresh_statuses_and_a_window_can_close_meanwhile() {
    let mut h = H::new();
    let id = open_repair(&mut h, live::Availability::Available);
    review_repair(&mut h, id, 5);
    h.click(ids::REPAIR_CONFIRM);
    h.status_reply();
    maintenance_jobs(&mut h);
    platform_says(
        &mut h,
        MaintenanceReport::RepairWaiting {
            id,
            detail: "Crosspane was started again. Waiting for the new instance…".into(),
            closeable: true,
        },
    );
    assert!(h.view().message.contains("Waiting for the new instance"));
    // Nothing is changing now: the window can close, and Back waits for the verdict.
    assert!(h.button(ids::CLOSE).is_some_and(|b| b.enabled));
    assert!(!h.button(ids::BACK).unwrap().enabled);
    // Each look is a Status issued after the previous one, offered with the repair's id.
    let mut seen = Vec::new();
    for _ in 0..3 {
        h.advance(600);
        h.status_reply();
        let ask: Vec<_> = maintenance_jobs(&mut h)
            .into_iter()
            .filter_map(|m| match m {
                MaintenanceRequest::VerifyRepair {
                    id: i,
                    status: Some(s),
                } if i == id => Some(format!("{s:?}")),
                _ => None,
            })
            .collect();
        assert_eq!(ask.len(), 1, "one look per fresh Status");
        seen.push(ask);
        platform_says(
            &mut h,
            MaintenanceReport::RepairWaiting {
                id,
                detail: "Still waiting…".into(),
                closeable: true,
            },
        );
    }
    platform_says(
        &mut h,
        MaintenanceReport::RepairFinished {
            id,
            outcome: RepairOutcome::Verified,
            lines: vec!["1 file(s) were put back.".into()],
            resumable: false,
        },
    );
    assert!(h.view().message.contains("Crosspane was repaired"));
    assert!(h.view().message.contains("1 file(s) were put back."));
    // No more looks once it ended.
    h.advance(600);
    h.tick();
    assert!(!h.has_call(|r| *r == InstallerRequest::Status));
    assert!(
        maintenance_jobs(&mut h)
            .iter()
            .all(|m| !matches!(m, MaintenanceRequest::VerifyRepair { .. }))
    );
}

#[test]
fn closing_the_window_while_the_new_agent_is_watched_is_allowed_and_leaves_nothing_working() {
    let mut h = H::new();
    let id = open_repair(&mut h, live::Availability::Available);
    review_repair(&mut h, id, 5);
    h.click(ids::REPAIR_CONFIRM);
    h.status_reply();
    maintenance_jobs(&mut h);
    platform_says(
        &mut h,
        MaintenanceReport::RepairWaiting {
            id,
            detail: "Waiting…".into(),
            closeable: true,
        },
    );
    assert!(
        h.c.request_close(),
        "nothing is changing, so the window closes"
    );
    assert!(h.native.borrow().shutdown);
}

#[test]
fn each_typed_repair_outcome_reaches_the_view_and_offers_resume_only_when_it_can_help() {
    for (outcome, head, resume) in [
        (RepairOutcome::Verified, "Crosspane was repaired", false),
        (
            RepairOutcome::HealthVerifiedCleanupIncomplete,
            "Repaired: health verified, cleanup incomplete.",
            false,
        ),
        (
            RepairOutcome::OutcomeUnknown,
            "Outcome unknown, resume required.",
            true,
        ),
        (
            RepairOutcome::RecoveryRetained,
            "The repair didn't finish. Backups and recovery files were kept.",
            true,
        ),
    ] {
        let mut h = H::new();
        let id = open_repair(&mut h, live::Availability::Available);
        review_repair(&mut h, id, 5);
        h.click(ids::REPAIR_CONFIRM);
        h.status_reply();
        maintenance_jobs(&mut h);
        platform_says(
            &mut h,
            MaintenanceReport::RepairFinished {
                id,
                outcome,
                lines: vec!["Kept: the old backups.".into()],
                resumable: resume,
            },
        );
        let message = h.view().message.clone();
        assert!(message.contains(head), "{outcome:?}: {message}");
        assert!(message.contains("Kept: the old backups."), "{outcome:?}");
        assert_eq!(
            h.button(ids::REPAIR_RESUME).is_some_and(|b| b.enabled),
            resume,
            "{outcome:?}"
        );
        assert!(h.button(ids::REPAIR_CONFIRM).is_none());
        assert!(
            h.button(ids::CLOSE).is_some_and(|b| b.enabled),
            "nothing is left working"
        );
        if resume {
            // Resume asks for a Status issued after the click, then names the repair.
            h.click(ids::REPAIR_RESUME);
            h.status_reply();
            let requests = maintenance_jobs(&mut h);
            assert!(
                requests.iter().any(|m| matches!(
                    m,
                    MaintenanceRequest::ResumeRepair { id: i, status: Some(_) } if *i == id
                )),
                "{requests:?}"
            );
            assert!(h.view().message.contains("Repairing Crosspane"));
            platform_says(
                &mut h,
                MaintenanceReport::RepairFinished {
                    id,
                    outcome: RepairOutcome::Verified,
                    lines: Vec::new(),
                    resumable: false,
                },
            );
            assert!(h.view().message.contains("Crosspane was repaired"));
            assert!(h.button(ids::REPAIR_RESUME).is_none());
        }
    }
}

#[test]
fn an_interrupted_repair_found_on_inspection_offers_resume_and_never_a_new_repair() {
    let mut h = H::new();
    let id = open_repair(
        &mut h,
        live::Availability::Unavailable("An earlier repair didn't finish.".into()),
    );
    platform_says(
        &mut h,
        MaintenanceReport::RepairResumable {
            id,
            lines: vec!["It stopped while files were being replaced.".into()],
        },
    );
    assert!(h.button(ids::REPAIR_RESUME).is_some_and(|b| b.enabled));
    assert!(!h.button(ids::REPAIR).unwrap().enabled);
    assert!(
        h.button(ids::REMOVE_REVIEW).is_some_and(|b| b.enabled),
        "removal stays the way out when a resume can't settle the record"
    );
    let message = h.view().message.clone();
    assert!(
        message.contains("An earlier repair didn't finish"),
        "{message}"
    );
    // One change at a time: while a removal is reviewed, Resume waits.
    {
        let mut h = H::new();
        let id = open_repair(
            &mut h,
            live::Availability::Unavailable("An earlier repair didn't finish.".into()),
        );
        platform_says(
            &mut h,
            MaintenanceReport::RepairResumable {
                id,
                lines: vec!["It stopped while files were being replaced.".into()],
            },
        );
        h.click(ids::REMOVE_REVIEW);
        assert!(!h.button(ids::REPAIR_RESUME).unwrap().enabled);
    }
    assert!(message.contains("It stopped while files were being replaced."));
    h.click(ids::REPAIR_RESUME);
    h.status_reply();
    assert!(
        maintenance_jobs(&mut h)
            .iter()
            .any(|m| matches!(m, MaintenanceRequest::ResumeRepair { .. }))
    );
}

#[test]
fn a_refusal_or_a_busy_platform_never_leaves_a_repair_working() {
    // The platform refuses the confirmation (the install changed, say).
    let mut h = H::new();
    let id = open_repair(&mut h, live::Availability::Available);
    review_repair(&mut h, id, 5);
    h.click(ids::REPAIR_CONFIRM);
    h.status_reply();
    maintenance_jobs(&mut h);
    platform_says(
        &mut h,
        MaintenanceReport::Refused {
            id,
            reason: "Things changed since the preview. Nothing was changed.".into(),
        },
    );
    assert!(
        h.view()
            .message
            .contains("Things changed since the preview")
    );
    assert!(h.button(ids::REPAIR_CONFIRM).is_none());
    assert!(h.button(ids::CLOSE).is_some_and(|b| b.enabled));
    assert!(h.button(ids::BACK).is_some_and(|b| b.enabled));
    assert!(h.c.request_close(), "nothing is running");

    // The platform can't even take the request.
    let mut h = H::new();
    let id = open_repair(&mut h, live::Availability::Available);
    review_repair(&mut h, id, 5);
    h.native.borrow_mut().refuse = true;
    h.click(ids::REPAIR_CONFIRM);
    h.status_reply();
    assert!(
        h.button(ids::CLOSE).is_some_and(|b| b.enabled),
        "{:?}",
        h.view().buttons
    );
    assert!(h.view().message.contains("busy"), "{}", h.view().message);
}

#[test]
fn a_silent_platform_ends_the_wait_as_unknown_with_a_resume_instead_of_working_forever() {
    let mut h = H::new();
    let id = open_repair(&mut h, live::Availability::Available);
    review_repair(&mut h, id, 5);
    h.click(ids::REPAIR_CONFIRM);
    h.status_reply();
    maintenance_jobs(&mut h);
    platform_says(
        &mut h,
        MaintenanceReport::RepairWaiting {
            id,
            detail: "Waiting…".into(),
            closeable: true,
        },
    );
    h.advance(151_000);
    h.tick();
    let message = h.view().message.clone();
    assert!(
        message.contains("Outcome unknown, resume required."),
        "{message}"
    );
    assert!(h.button(ids::REPAIR_RESUME).is_some_and(|b| b.enabled));
    assert!(h.button(ids::BACK).is_some_and(|b| b.enabled));
}

#[test]
fn a_report_for_another_maintenance_visit_never_changes_the_repair_view() {
    let mut h = H::new();
    let id = open_repair(&mut h, live::Availability::Available);
    review_repair(&mut h, id, 5);
    let stale = MaintenanceId(id.0 + 5);
    platform_says(
        &mut h,
        MaintenanceReport::RepairFinished {
            id: stale,
            outcome: RepairOutcome::Verified,
            lines: Vec::new(),
            resumable: false,
        },
    );
    assert!(h.button(ids::REPAIR_CONFIRM).is_some());
    assert!(!h.view().message.contains("Crosspane was repaired"));
    // A finished report that no confirmation asked for changes nothing either.
    platform_says(
        &mut h,
        MaintenanceReport::RepairFinished {
            id,
            outcome: RepairOutcome::Verified,
            lines: Vec::new(),
            resumable: false,
        },
    );
    assert!(h.button(ids::REPAIR_CONFIRM).is_some());
}

/// Review and confirm a repair, then let the platform say it waits.
fn confirmed_and_waiting(h: &mut H, closeable: bool, detail: &str) -> MaintenanceId {
    let id = open_repair(h, live::Availability::Available);
    review_repair(h, id, 5);
    h.click(ids::REPAIR_CONFIRM);
    h.status_reply();
    maintenance_jobs(h);
    platform_says(
        h,
        MaintenanceReport::RepairWaiting {
            id,
            detail: detail.into(),
            closeable,
        },
    );
    id
}

#[test]
fn a_repair_whose_agent_cant_answer_is_still_asked_to_look_so_its_own_bound_can_end_it() {
    let mut h = H::new();
    let id = confirmed_and_waiting(
        &mut h,
        false,
        "Crosspane was stopped. Waiting for it to exit cleanly…",
    );
    // The old agent is stopped: no Status is ever answered.
    let mut looks = 0;
    for _ in 0..12 {
        h.advance(600);
        h.tick();
        for m in maintenance_jobs(&mut h) {
            match m {
                MaintenanceRequest::VerifyRepair {
                    id: i,
                    status: None,
                } if i == id => {
                    looks += 1;
                    platform_says(
                        &mut h,
                        MaintenanceReport::RepairWaiting {
                            id,
                            detail: "Still waiting for it to exit cleanly…".into(),
                            closeable: false,
                        },
                    );
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }
    assert!(
        (2..=4).contains(&looks),
        "looks every couple of seconds: {looks}"
    );
    // The platform's own bound ends it with its typed result.
    platform_says(
        &mut h,
        MaintenanceReport::RepairFinished {
            id,
            outcome: RepairOutcome::RecoveryRetained,
            lines: vec!["Crosspane may not be running.".into()],
            resumable: true,
        },
    );
    let message = h.view().message.clone();
    assert!(message.contains("The repair didn't finish"), "{message}");
    assert!(
        message.contains("Crosspane may not be running."),
        "{message}"
    );
}

#[test]
fn a_wait_that_isnt_closeable_keeps_the_window_open_and_shows_what_it_waits_for() {
    let mut h = H::new();
    let id = confirmed_and_waiting(
        &mut h,
        false,
        "Crosspane was stopped. Waiting for it to exit cleanly…",
    );
    assert!(h.view().message.contains("Waiting for it to exit cleanly"));
    assert!(h.button(ids::CLOSE).is_none());
    assert!(h.button(ids::BACK).is_none());
    assert!(!h.c.request_close());
    assert!(!h.native.borrow().shutdown);
    // The platform reaches the health wait, which leaves a resumable record: now it may close.
    platform_says(
        &mut h,
        MaintenanceReport::RepairWaiting {
            id,
            detail: "Crosspane was started again. Waiting for the new instance…".into(),
            closeable: true,
        },
    );
    assert!(h.button(ids::CLOSE).is_some_and(|b| b.enabled));
    assert!(h.c.request_close());
}

#[test]
fn a_refusal_of_some_replayed_request_never_hides_a_repair_that_is_under_way() {
    let mut h = H::new();
    let id = confirmed_and_waiting(&mut h, true, "Waiting for the new instance…");
    platform_says(
        &mut h,
        MaintenanceReport::Refused {
            id,
            reason: "Confirm the current repair preview before repairing Crosspane. Nothing was \
                     changed."
                .into(),
        },
    );
    let message = h.view().message.clone();
    assert!(
        message.contains("Waiting for the new instance"),
        "{message}"
    );
    assert!(!message.contains("Confirm the current"), "{message}");
    assert!(h.button(ids::REPAIR_RESUME).is_none());
    // It is still driven to its end.
    h.advance(600);
    h.status_reply();
    assert!(maintenance_jobs(&mut h).iter().any(|m| matches!(
        m,
        MaintenanceRequest::VerifyRepair { id: i, status: Some(_) } if *i == id
    )));
    platform_says(
        &mut h,
        MaintenanceReport::RepairFinished {
            id,
            outcome: RepairOutcome::Verified,
            lines: Vec::new(),
            resumable: false,
        },
    );
    assert!(h.view().message.contains("Crosspane was repaired"));
}

#[test]
fn a_busy_platform_that_cant_take_a_look_never_ends_the_repair() {
    let mut h = H::new();
    let id = confirmed_and_waiting(&mut h, true, "Waiting for the new instance…");
    h.native.borrow_mut().refuse = true;
    h.advance(600);
    h.status_reply();
    h.tick();
    let message = h.view().message.clone();
    assert!(
        message.contains("Waiting for the new instance"),
        "{message}"
    );
    assert!(!message.contains("busy"), "{message}");
    h.native.borrow_mut().refuse = false;
    h.advance(600);
    h.status_reply();
    assert!(maintenance_jobs(&mut h).iter().any(|m| matches!(
        m,
        MaintenanceRequest::VerifyRepair { id: i, status: Some(_) } if *i == id
    )));
}

#[test]
fn a_repair_review_cant_start_while_a_removal_plan_is_pending() {
    let mut h = H::new();
    let id = open_repair(&mut h, live::Availability::Available);
    h.click(ids::REMOVE_REVIEW);
    assert!(
        maintenance_jobs(&mut h)
            .iter()
            .any(|m| matches!(m, MaintenanceRequest::PlanUninstall { id: i, .. } if *i == id))
    );
    assert!(!h.button(ids::REPAIR).unwrap().enabled);
    let revision = h.view().revision;
    h.act(revision, WizardIntent::Button(ids::REPAIR));
    h.advance(13_000);
    h.tick();
    assert!(
        maintenance_jobs(&mut h)
            .iter()
            .all(|m| !matches!(m, MaintenanceRequest::PlanRepair { .. }))
    );
    assert!(!h.view().message.contains("Preparing the repair preview"));
}

#[test]
fn an_unknown_outcome_with_no_resume_doesnt_ask_for_one() {
    let mut h = H::new();
    let id = open_repair(&mut h, live::Availability::Available);
    review_repair(&mut h, id, 5);
    h.click(ids::REPAIR_CONFIRM);
    h.status_reply();
    maintenance_jobs(&mut h);
    platform_says(
        &mut h,
        MaintenanceReport::RepairFinished {
            id,
            outcome: RepairOutcome::OutcomeUnknown,
            lines: vec!["The repair was cut short.".into()],
            resumable: false,
        },
    );
    let message = h.view().message.clone();
    assert!(message.contains("Outcome unknown"), "{message}");
    assert!(!message.contains("resume required"), "{message}");
    assert!(h.button(ids::REPAIR_RESUME).is_none());
}

// ---- the support checklist (WP-4.28) ----------------------------------------------------------

fn sample_checks() -> Vec<SupportCheck> {
    vec![
        SupportCheck::new(
            "Operating system",
            CheckState::Passed(Some("Arch-based".into())),
        ),
        SupportCheck::new("Hyprland version", CheckState::Failed("too old".into())),
        SupportCheck::new(
            "This session is the signed-in one",
            CheckState::Unconfirmed("couldn't read the session environment".into()),
        ),
        SupportCheck::new("Processor", CheckState::Passed(None)),
    ]
}

/// The checklist rows of the current view, in order.
fn check_rows(h: &H) -> Vec<RowView> {
    h.view()
        .rows
        .iter()
        .filter(|r| r.is_check())
        .cloned()
        .collect()
}

#[test]
fn the_checklist_follows_the_support_pass_and_check_again_resets_it() {
    let slot = SupportChecksSlot::new(SUPPORT);
    let mut h = H::with_checks(description(), Some(slot.clone()));
    h.tick();
    h.next();
    assert_eq!(h.view().screen, ScreenId::Compatibility);
    // The first pass is running and nothing has finished yet: no rows, a running indicator.
    let (detect, _) = h.job(SUPPORT, JobStage::Detect);
    assert!(check_rows(&h).is_empty());
    assert!(
        h.row(SUPPORT).detail.ends_with("Checking now…"),
        "{:?}",
        h.row(SUPPORT)
    );
    // The pass finishes: its checklist is published before the report that ends the job.
    slot.publish(sample_checks());
    h.report(&detect, NativeOutcome::Waiting(WaitKind::User));
    assert_eq!(h.row(SUPPORT).state, RowState::Waiting);
    let rows = check_rows(&h);
    assert_eq!(
        rows.iter()
            .map(|r| (r.id, r.label.as_str(), r.state, r.detail.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (900, "Operating system", RowState::Verified, "Arch-based"),
            (901, "Hyprland version", RowState::Failed, "too old"),
            (
                902,
                "This session is the signed-in one",
                RowState::Waiting,
                "couldn't read the session environment"
            ),
            (903, "Processor", RowState::Verified, ""),
        ]
    );
    // The rows sit right under their card, before the page's later steps, and the summary
    // sentence is kept.
    let ids: Vec<u16> = h.view().rows.iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![SUPPORT.0, 900, 901, 902, 903, PAYLOAD.0, AGENT.0]);
    let card = h.row(SUPPORT);
    assert!(card.detail.starts_with("fake detail"), "{card:?}");
    assert!(card.detail.ends_with("Last checked just now."), "{card:?}");
    let revision = h.view().revision;
    h.advance(3_000);
    h.tick();
    assert!(h.row(SUPPORT).detail.ends_with("Last checked 3 s ago."));
    h.advance(120_000);
    h.tick();
    assert!(h.row(SUPPORT).detail.ends_with("Last checked 2 min ago."));
    // Time passing is not a change of meaning.
    assert_eq!(h.view().revision, revision);
    // Check again re-runs the pass and shows every row as checking until it finishes.
    h.click(ids::retry(SUPPORT));
    let (again, _) = h.job(SUPPORT, JobStage::Detect);
    let rows = check_rows(&h);
    assert_eq!(rows.len(), 4);
    assert!(
        rows.iter()
            .all(|r| r.state == RowState::Working && r.detail.is_empty())
    );
    assert!(h.row(SUPPORT).detail.ends_with("Checking now…"));
    slot.publish(vec![
        SupportCheck::new("Operating system", CheckState::Passed(None)),
        SupportCheck::new("Hyprland version", CheckState::Passed(None)),
    ]);
    h.report(
        &again,
        NativeOutcome::Detected {
            needs_action: false,
        },
    );
    let (verify, _) = h.job(SUPPORT, JobStage::Verify);
    // Verification is another pass: the rows read as checking again until it ends.
    assert!(check_rows(&h).iter().all(|r| r.state == RowState::Working));
    slot.publish(vec![
        SupportCheck::new("Operating system", CheckState::Passed(None)),
        SupportCheck::new("Hyprland version", CheckState::Passed(None)),
    ]);
    let live = h.live();
    h.report(&verify, live);
    assert_eq!(h.row(SUPPORT).state, RowState::Verified);
    let rows = check_rows(&h);
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.state == RowState::Verified));
    assert!(h.row(SUPPORT).detail.ends_with("Last checked just now."));
}

#[test]
fn the_checklist_stays_on_its_own_screen_and_platforms_without_one_are_unchanged() {
    // No slot: the card reads exactly as before and no checklist rows appear.
    let mut h = H::new();
    h.tick();
    h.next();
    let (detect, _) = h.job(SUPPORT, JobStage::Detect);
    assert!(check_rows(&h).is_empty());
    h.report(&detect, NativeOutcome::Waiting(WaitKind::User));
    assert_eq!(h.row(SUPPORT).detail, "fake detail");
    assert!(check_rows(&h).is_empty());

    // With a slot, the rows belong to the support card only, never to the next screen.
    let slot = SupportChecksSlot::new(SUPPORT);
    let mut h = H::with_checks(description(), Some(slot.clone()));
    h.tick();
    h.next();
    let (detect, _) = h.job(SUPPORT, JobStage::Detect);
    slot.publish(sample_checks());
    h.report(
        &detect,
        NativeOutcome::Detected {
            needs_action: false,
        },
    );
    let (verify, _) = h.job(SUPPORT, JobStage::Verify);
    slot.publish(vec![SupportCheck::new(
        "Processor",
        CheckState::Passed(None),
    )]);
    let live = h.live();
    h.report(&verify, live);
    // The three install screens are one page: the checklist stays under the support card while
    // the page carries on, and never moves under another card.
    assert_eq!(h.view().screen, ScreenId::InstallPlan);
    assert_eq!(check_rows(&h).len(), 1);
    let ids: Vec<u16> = h.view().rows.iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![SUPPORT.0, 900, PAYLOAD.0, AGENT.0]);
    h.pass(PAYLOAD);
    h.pass(AGENT);
    h.status_reply();
    h.last = h.view().screen;
    if h.view().screen != ScreenId::Connect {
        h.next();
    }
    assert_eq!(h.view().screen, ScreenId::Connect);
    assert!(check_rows(&h).is_empty());
    // A checklist naming a step that isn't a native step is ignored.
    let mut h = H::with_checks(description(), Some(SupportChecksSlot::new(StepId(59))));
    h.tick();
    h.next();
    assert!(check_rows(&h).is_empty());
    assert!(!h.row(SUPPORT).detail.contains("Checking now"));
}

#[test]
fn a_screen_with_several_waiting_steps_shows_one_check_again_that_rechecks_each() {
    let mut desc = description();
    for id in [24, 25] {
        desc.steps
            .push(native(StepId(id), &[PAYLOAD], ScreenId::Installing, true));
    }
    let mut h = H::with(desc);
    h.tick();
    h.next();
    h.pass(SUPPORT);
    h.next();
    h.pass(PAYLOAD);
    h.next();
    assert_eq!(h.view().screen, ScreenId::Installing);
    let installing = [AGENT, StepId(24), StepId(25)];
    let jobs = h.take_jobs();
    for step in installing {
        let job = jobs
            .iter()
            .find_map(|j| match j {
                NativeJob::Step { job, .. } if job.step == step => Some(job.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no detect for {}", step.0));
        h.report(&job, NativeOutcome::Waiting(WaitKind::User));
        assert_eq!(h.row(step).state, RowState::Waiting);
    }
    let retries = |h: &H| -> Vec<ButtonView> {
        h.view()
            .buttons
            .iter()
            .filter(|b| b.role == ButtonRole::Retry)
            .cloned()
            .collect()
    };
    let shown = retries(&h);
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert_eq!(shown[0].id, ids::RETRY_ALL);
    assert_eq!(shown[0].label, "Check again");
    assert!(shown[0].enabled);
    // One click re-checks every waiting step on the screen.
    h.click(ids::RETRY_ALL);
    let jobs = h.take_jobs();
    for step in installing {
        assert!(
            jobs.iter().any(|j| matches!(j, NativeJob::Step { job, .. }
                if job.step == step && job.stage == JobStage::Detect)),
            "{} not re-checked: {jobs:?}",
            step.0
        );
        assert_eq!(h.row(step).state, RowState::Working);
    }
    assert!(
        retries(&h).is_empty(),
        "nothing waits while all are checking"
    );
    // Two settle; the last one still waiting gets its own single "Check again".
    for j in &jobs {
        if let NativeJob::Step { job, .. } = j {
            if job.step == StepId(25) {
                h.report(job, NativeOutcome::Waiting(WaitKind::User));
            } else {
                h.report(
                    job,
                    NativeOutcome::Detected {
                        needs_action: false,
                    },
                );
                let (verify, _) = h.job(job.step, JobStage::Verify);
                let live = h.live();
                h.report(&verify, live);
            }
        }
    }
    let shown = retries(&h);
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert_eq!(shown[0].id, ids::retry(StepId(25)));
}

#[test]
fn advisory_check_is_a_visible_note_and_does_not_claim_verified_evidence() {
    let slot = SupportChecksSlot::new(SUPPORT);
    let mut h = H::with_checks(description(), Some(slot.clone()));
    h.tick();
    h.next();
    let (detect, _) = h.job(SUPPORT, JobStage::Detect);
    slot.publish(vec![SupportCheck::new(
        "Runtime evidence",
        CheckState::Note("Couldn't confirm an optional fact; setup can continue".into()),
    )]);
    h.report(
        &detect,
        NativeOutcome::Detected {
            needs_action: false,
        },
    );
    let (verify, _) = h.job(SUPPORT, JobStage::Verify);
    slot.publish(vec![SupportCheck::new(
        "Runtime evidence",
        CheckState::Note("Couldn't confirm an optional fact; setup can continue".into()),
    )]);
    let live = h.live();
    h.report(&verify, live);
    assert_eq!(h.row(SUPPORT).state, RowState::Verified);
    let rows = check_rows(&h);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, RowState::Note);
    assert!(rows[0].detail.contains("Couldn't confirm"));
}
