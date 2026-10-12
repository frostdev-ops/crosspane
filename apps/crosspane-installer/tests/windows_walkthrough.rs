#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The Windows setup flow, walked from Welcome to Ready and then through removal. The real worker
//! (`integration::run`) runs over a fake native domain and a fake agent port: no Windows backend,
//! task, agent, file, firewall, driver or window is touched, so this runs on every OS.
use crosspane_installer::{agent_contract, gui, live, view};
#[path = "../src/platform/windows/integration.rs"]
#[allow(dead_code)]
mod integration;
use agent_contract::{
    AgentCall, AgentPlatform, AgentPort, AgentQueue, AgentReply, CallFailure, DecodedReply,
    InstallerRequest, decode_reply,
};
use crosspane_installer_core::elevated::step::{ElevatedPlan, PREVIEW_HEADER};
use crosspane_installer_core::elevated::{AgentProgram, InstallId, RuleScope, Verb};
use crosspane_installer_core::{ObservationSource, StepId};
use gui::InstallerController;
use integration::domains::{
    Cold, Dispatch, Domains, ElevatedDetection, ElevatedPlanning, ElevatedRequest, Failure,
    Handoff, Operation, Snapshot, State,
};
use live::{Clock, LiveController, ids};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use view::{ButtonView, RowState, ScreenId, SummaryView, WizardAction, WizardIntent, WizardView};

/// The effects the worker asked the fake domain to make, in order.
type Log = Arc<Mutex<Vec<&'static str>>>;

/// The step ids of the Windows step table (`integration::description`).
const INSTALL: StepId = StepId(20);
const AGENT: StepId = StepId(30);
const ADMIN: StepId = StepId(55);

/// The most steps one wait may pump; each is 50 ms of fake time and a 1 ms real pause.
const SETTLE_LIMIT: usize = 3_000;
const STEP_MS: u64 = 50;
const REFUSAL_TITLE: &str = "Crosspane can't run here yet";
/// What the install reports when the person declined the administrator block of its preview.
const DECLINED: &str = "Approval was declined: no firewall rule or display driver was added.";

/// The Windows Status the agent answers with: the same producer-shaped fixture the transport
/// tests decode (`tests/windows_transport.rs`), with every backend ready.
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

/// A PC with nothing of Crosspane on it yet: the first install is eligible.
fn fresh() -> Snapshot {
    Snapshot {
        supported: true,
        inventory: true,
        sources: true,
        payload: State::Missing,
        task: State::Missing,
        agent: State::Missing,
        cold: Cold::Eligible,
        terminal_history: false,
        unsettled: false,
        correlation: vec![7],
        source: ObservationSource::Live,
    }
}

fn rule_scope() -> RuleScope {
    RuleScope {
        id: InstallId::parse("walk-1").unwrap(),
        program: AgentProgram::parse(
            r"C:\Users\walk\AppData\Local\Programs\Crosspane\crosspane-agent.exe",
        )
        .unwrap(),
    }
}

fn record(log: &Log, entry: &'static str) {
    log.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(entry);
}

/// The walk's native domain. An install, the administrator step and a removal change only what
/// they say; each effect is recorded in `log`.
struct Walk {
    observed: Snapshot,
    /// Observations left until the agent answers Healthy after the install; the third does.
    agent_in: u8,
    /// Set once the administrator step has added the firewall rule and display driver.
    admin_on: bool,
    /// Lines the install reports after the person declined the administrator block.
    report: Vec<String>,
    log: Log,
}

impl Walk {
    fn new(log: Log) -> Self {
        Self {
            observed: fresh(),
            agent_in: 0,
            admin_on: false,
            report: Vec::new(),
            log,
        }
    }
}

impl Domains for Walk {
    fn observe(&mut self) -> Result<Snapshot, Failure> {
        if self.agent_in > 0 {
            self.agent_in -= 1;
            if self.agent_in == 0 {
                self.observed.agent = State::Healthy;
            }
        }
        Ok(self.observed.clone())
    }

    fn settle(&mut self, _operation: Operation) -> Result<bool, Failure> {
        Ok(false)
    }

    fn apply(&mut self, operation: Operation) -> Result<Dispatch, Failure> {
        match operation {
            Operation::Install => {
                record(&self.log, "install");
                self.observed.payload = State::Healthy;
                self.observed.task = State::Healthy;
                self.observed.cold = Cold::Existing;
                self.agent_in = 3;
                Ok(Dispatch {
                    handoff: Handoff::NotCommitted,
                    complete: true,
                })
            }
            Operation::Removal { .. } => {
                record(&self.log, "removal");
                self.observed.payload = State::Missing;
                self.observed.task = State::Missing;
                self.observed.agent = State::Missing;
                Ok(Dispatch {
                    handoff: Handoff::Committed,
                    complete: false,
                })
            }
            // The walk never reaches another change; a refusal here would show as a stall.
            _ => Err(Failure::NotSubmitted),
        }
    }

    fn verify(&mut self, operation: Operation) -> Result<bool, Failure> {
        match operation {
            Operation::Elevated => Ok(self.admin_on),
            other => Ok(self.observed.verified(other)),
        }
    }

    fn elevated_plan(&mut self, request: ElevatedRequest) -> Result<ElevatedPlanning, Failure> {
        match request {
            ElevatedRequest::Setup => ElevatedPlan::new(Verb::Setup(rule_scope()))
                .map(ElevatedPlanning::Planned)
                .map_err(|_| Failure::NotSubmitted),
            ElevatedRequest::Teardown => Ok(ElevatedPlanning::NotNeeded),
        }
    }

    fn elevated_detect(&mut self) -> Result<ElevatedDetection, Failure> {
        let detail = if self.admin_on {
            "The firewall rule is added."
        } else {
            "The firewall rule isn't added yet."
        };
        Ok(ElevatedDetection {
            configured: self.admin_on,
            detail: detail.to_owned(),
        })
    }

    fn apply_elevated(
        &mut self,
        operation: Operation,
        elevated: Option<&ElevatedPlan>,
    ) -> Result<Dispatch, Failure> {
        match (operation, elevated) {
            (Operation::Install, Some(_)) => {
                // The person declined the administrator block of the install preview. The install
                // goes ahead without it, and the wizard is told so.
                self.report = vec![DECLINED.to_owned()];
                self.apply(Operation::Install)
            }
            (Operation::Elevated, Some(_)) => {
                record(&self.log, "elevated");
                self.admin_on = true;
                Ok(Dispatch {
                    handoff: Handoff::NotCommitted,
                    complete: true,
                })
            }
            (other, _) => self.apply(other),
        }
    }

    fn take_elevated_report(&mut self) -> Vec<String> {
        std::mem::take(&mut self.report)
    }
}

/// The agent port the controller talks to. `Harness::pump` answers its Status calls.
struct SharedAgent(Rc<RefCell<AgentQueue>>);

impl AgentPort for SharedAgent {
    fn submit(&mut self, call: AgentCall) -> Result<(), CallFailure> {
        self.0.borrow_mut().submit(call)
    }
    fn poll(&mut self) -> Vec<AgentReply> {
        self.0.borrow_mut().poll()
    }
}

/// The controller over the real worker, with a fake clock. Every other agent call (the pairing
/// scan, for one) is left unanswered, as the walk skips pairing.
struct Harness {
    c: LiveController,
    queue: Rc<RefCell<AgentQueue>>,
    time: Arc<AtomicU64>,
    status: DecodedReply,
}

impl Harness {
    fn new(log: Log) -> Self {
        let time = Arc::new(AtomicU64::new(1_000));
        let clock: Clock = {
            let time = time.clone();
            Arc::new(move || time.load(Ordering::SeqCst))
        };
        let queue = Rc::new(RefCell::new(AgentQueue::default()));
        let platform = integration::platform_fixture(
            Walk::new(log),
            clock.clone(),
            Box::new(SharedAgent(queue.clone())),
        );
        let c = LiveController::new(platform, clock).unwrap();
        let status =
            decode_reply(&InstallerRequest::Status, STATUS, AgentPlatform::Windows).unwrap();
        Self {
            c,
            queue,
            time,
            status,
        }
    }

    fn now(&self) -> u64 {
        self.time.load(Ordering::SeqCst)
    }

    fn view(&self) -> &WizardView {
        self.c.view()
    }

    fn screen(&self) -> ScreenId {
        self.view().screen
    }

    fn button(&self, id: u16) -> Option<&ButtonView> {
        self.view().buttons.iter().find(|b| b.id == id)
    }

    fn enabled(&self, id: u16) -> bool {
        self.button(id).is_some_and(|b| b.enabled)
    }

    fn label(&self, id: u16) -> Option<&str> {
        self.button(id).map(|b| b.label.as_str())
    }

    fn row(&self, step: StepId) -> RowState {
        self.c
            .rows()
            .into_iter()
            .find(|r| r.id == step.0)
            .unwrap_or_else(|| panic!("row {} is missing", step.0))
            .state
    }

    fn click(&mut self, id: u16) {
        let button = self
            .button(id)
            .unwrap_or_else(|| panic!("button {id} is missing on {:?}", self.screen()));
        assert!(
            button.enabled,
            "button {id} is disabled on {:?}",
            self.screen()
        );
        let revision = self.view().revision;
        let closed = self.c.accept(WizardAction {
            revision,
            intent: WizardIntent::Button(id),
        });
        assert!(!closed, "the walk never closes the window");
    }

    /// Answers every Status call issued so far with a live, healthy reply at this moment.
    fn answer_status(&mut self) {
        let now = self.now();
        let calls = self.queue.borrow_mut().take_calls();
        for call in calls
            .into_iter()
            .filter(|call| call.request == InstallerRequest::Status)
        {
            let reply = AgentReply {
                id: call.id,
                observed_at_ms: now,
                source: ObservationSource::Live,
                result: Ok(self.status.clone()),
            };
            self.queue.borrow_mut().push_reply(reply).unwrap();
        }
    }

    /// One pump: the clock moves on, the controller ticks, and the invariants must hold.
    fn pump(&mut self) {
        self.time.fetch_add(STEP_MS, Ordering::SeqCst);
        let _ = self.c.tick();
        self.answer_status();
        for row in self.c.rows() {
            assert_ne!(
                row.state,
                RowState::Unsupported,
                "{} is unsupported: {}",
                row.label,
                row.detail
            );
        }
        assert_ne!(self.view().title.as_str(), REFUSAL_TITLE);
    }

    /// Pumps until `done` holds, for at most `SETTLE_LIMIT` steps. `what` names the wait.
    fn settle(&mut self, what: &str, done: impl Fn(&Harness) -> bool) {
        for _ in 0..SETTLE_LIMIT {
            self.pump();
            if done(&*self) {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!(
            "{what} was not reached in {SETTLE_LIMIT} steps; screen {:?}, title {:?}, message {:?}",
            self.screen(),
            self.view().title,
            self.view().message
        );
    }
}

#[test]
fn windows_setup_walks_from_welcome_to_ready_then_removal_through_the_real_worker() {
    let log: Log = Arc::default();
    let mut h = Harness::new(log.clone());
    assert_eq!(h.screen(), ScreenId::Welcome);

    // Welcome: Start setup. Nothing is changed before this click.
    h.click(ids::NEXT);

    // The install preview asks first, with the administrator block in front of the person.
    let install = ids::consent(INSTALL);
    h.settle("the install preview", |w| {
        w.screen() == ScreenId::InstallPlan && w.enabled(install)
    });
    assert_eq!(h.label(install), Some("Install"));
    assert!(
        h.view().message.contains(PREVIEW_HEADER),
        "the install preview lacks the administrator block: {:?}",
        h.view().message
    );
    h.click(install);

    // INSTALL and AGENT verify, and the wizard reaches Network.
    h.settle("INSTALL and AGENT verified on Network", |w| {
        w.screen() == ScreenId::Network
            && w.row(INSTALL) == RowState::Verified
            && w.row(AGENT) == RowState::Verified
    });

    // The administrator step asks, and "Not now" stays on the screen.
    let admin = ids::consent(ADMIN);
    h.settle("the administrator consent", |w| {
        w.enabled(admin)
            && w.label(admin) == Some("Allow with administrator approval")
            && w.label(ids::NEXT) == Some("Not now")
    });
    h.click(admin);

    // The administrator step verifies; Connect is skipped (pairing, grants and layout).
    h.settle("ADMIN verified, and Connect offers Skip", |w| {
        w.row(ADMIN) == RowState::Verified
            && w.screen() == ScreenId::Connect
            && w.enabled(ids::SKIP)
    });
    h.click(ids::SKIP);

    // The summary reaches workspace ready.
    h.settle("the summary reaches workspace ready", |w| {
        w.view().summary == SummaryView::WorkspaceReady
    });

    // Removal: review, confirm, and the worker submits it.
    h.click(ids::REMOVE_OR_REPAIR);
    h.settle("the removal review", |w| w.enabled(ids::REMOVE_REVIEW));
    h.click(ids::REMOVE_REVIEW);
    h.settle("the removal confirmation", |w| {
        w.enabled(ids::REMOVE_CONFIRM)
    });
    h.click(ids::REMOVE_CONFIRM);
    h.settle("the submitted removal remains unconfirmed", |w| {
        w.view()
            .message
            .contains("Some parts of Crosspane are still here.")
    });
    assert!(!h.view().message.contains("submission:"));

    // Each effect ran exactly once, in order.
    let entries = log.lock().unwrap_or_else(PoisonError::into_inner).clone();
    assert_eq!(entries, ["install", "elevated", "removal"]);
}
