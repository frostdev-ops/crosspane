//! Controller state, the core reduce/dispatch loop, native-result correlation and agent routing.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crosspane_installer_core::{
    Flow, FlowError, FlowEvent, JobIntent, JobStage, ObservationSource, StepId, StepState, Summary,
    Verification,
};
use crosspane_types::id::NodeId;

use super::graph::{self, Graph, StepKind};
use super::ledger::Ledger;
use super::practice::PracticeState;
use super::present::MaintenanceState;
use super::present::permissions::PermissionAsks;
use super::shared::ConnectState;
use super::{
    Clock, Consent, LiveError, NativeJob, NativeOutcome, NativeReport, Platform,
    PlatformDescription, StatusEvidence, StepReport, SupportChecklist,
};
use crate::agent_contract::{
    AgentCall, AgentReply, CallFailure, DecodedReply, HealthSnapshot, InstallerRequest,
    StatusAdmission,
};
use crate::gui::{ControllerTick, InstallerController, ShellEffect};
use crate::view::{MotionPreference, ScreenId, WizardAction, WizardIntent, WizardView};

pub(super) const STATUS_ACTIVE_MS: u64 = 500;
pub(super) const STATUS_IDLE_MS: u64 = 2000;
pub(super) const CALL_TIMEOUT_MS: u64 = 4000;
const AUTO_BEGIN_MS: u64 = 2000;
pub(super) const MAX_TEXT: usize = 600;
const STATUS_WAIT_MS: u64 = 12_000;
/// A window-manager close is refused while a native change runs, unless the platform has said
/// nothing about it for this long (every native stage has its own shorter deadline), so a dead
/// worker can never make the window unclosable.
const CLOSE_GUARD_MS: u64 = 150_000;

pub(super) const ORDER: [ScreenId; 13] = [
    ScreenId::Welcome,
    ScreenId::Compatibility,
    ScreenId::InstallPlan,
    ScreenId::Installing,
    ScreenId::Permissions,
    ScreenId::AudioComponent,
    ScreenId::Network,
    ScreenId::HidingChoice,
    ScreenId::Connect,
    ScreenId::Grants,
    ScreenId::Layout,
    ScreenId::Practice,
    ScreenId::Summary,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OwnCall {
    Status,
    PairScan,
    PairListen,
    PairJoin,
    PairStatus,
    PairConfirm,
    PairPick,
    Dial,
    Allow,
    Place,
    Settings,
    StepApply,
    /// A permission row's ask, reset or restart (WP-4.33).
    Permission,
}

pub(super) struct Health {
    pub snapshot: Box<HealthSnapshot>,
    pub observed_at_ms: u64,
    pub call: u64,
    pub source: ObservationSource,
    /// The complete reply, kept so a native step can be handed exactly what the agent said.
    pub reply: AgentReply,
}

/// A native Verify job held back until a Status reply issued after it began can ride along.
pub(super) struct StatusWait {
    pub job: JobIntent,
    /// The first call id that may serve as evidence; older in-flight replies cannot.
    pub from_call: u64,
    pub started_at: u64,
}

/// The shared live controller. Construct it with a platform port; it owns that port.
pub struct LiveController {
    pub(super) platform: Box<dyn Platform>,
    pub(super) desc: PlatformDescription,
    pub(super) graph: Graph,
    pub(super) clock: Clock,
    pub(super) now: u64,
    pub(super) flow: Flow,
    pub(super) ledger: Ledger,
    pub(super) summary: Summary,
    pub(super) jobs: BTreeMap<StepId, JobIntent>,
    pub(super) intents: VecDeque<JobIntent>,
    pub(super) consents: BTreeMap<StepId, Consent>,
    pub(super) details: BTreeMap<StepId, String>,
    pub(super) previews: BTreeMap<StepId, String>,
    pub(super) auto_begun: BTreeMap<StepId, u64>,
    pub(super) next_call: u64,
    pub(super) own_calls: BTreeMap<u64, OwnCall>,
    pub(super) status_sent_at: Option<u64>,
    pub(super) status_waits: BTreeMap<StepId, StatusWait>,
    pub(super) health: Option<Health>,
    pub(super) peer: Option<NodeId>,
    pub(super) connect: ConnectState,
    pub(super) practice: PracticeState,
    pub(super) maintenance: MaintenanceState,
    pub(super) screen: ScreenId,
    pub(super) view: WizardView,
    pub(super) signature: String,
    pub(super) effects: Vec<ShellEffect>,
    pub(super) motion: MotionPreference,
    pub(super) notice: Option<String>,
    pub(super) closed: bool,
    /// When the running native change last started or reported progress.
    pub(super) change_since: Option<u64>,
    /// Agent-applied step requests in flight, by call id, with the Apply job each one serves.
    pub(super) step_apply_calls: BTreeMap<u64, JobIntent>,
    /// The latest finished support checklist the platform reported, and when this controller
    /// first saw that pass (its own clock, so "checked N s ago" never mixes clocks).
    pub(super) support_checks: Option<(SupportChecklist, u64)>,
    /// The person started setup from the welcome screen. That click is the go-ahead for the
    /// install steps that only touch their own account; nothing is changed before it.
    pub(super) install_started: bool,
    /// The install steps setup already went ahead with by itself in this run. Each step gets that
    /// once: if it asks again (it planned another change after the first one), the person
    /// answers, so a step that keeps re-planning can never repeat itself without end.
    pub(super) auto_consented: BTreeSet<StepId>,
    /// The current screen moves on by itself once everything on it is done. Off after the person
    /// went back to it, so a deliberate visit isn't cut short.
    pub(super) auto_advance: bool,
    /// When the current screen was first seen complete.
    pub(super) complete_since: Option<u64>,
    /// The last own Status came from an agent too old to report its health, so whether it is in
    /// use (and what a restart would interrupt) can't be known.
    pub(super) agent_health_pending: bool,
    /// The macOS permission rows' history and calls (WP-4.33).
    pub(super) permission_asks: PermissionAsks,
}

/// How long a finished screen stays up before the next one, so its last check is seen.
const ADVANCE_PAUSE_MS: u64 = 700;

/// The screens of the user-scope installation. Their changes stay inside the person's own
/// account (no administrator, no system permission, no firewall), so once setup is started
/// they run without asking again. Steps on every other screen ask first.
pub(super) fn automatic_screen(screen: ScreenId) -> bool {
    matches!(
        screen,
        ScreenId::Compatibility | ScreenId::InstallPlan | ScreenId::Installing
    )
}

/// Screens that move on by themselves once complete. The welcome, the numbers comparison, the
/// summary and maintenance always wait for the person.
fn advances_by_itself(screen: ScreenId) -> bool {
    !matches!(
        screen,
        ScreenId::Welcome | ScreenId::MatchNumbers | ScreenId::Summary | ScreenId::RepairRemove
    )
}

impl std::fmt::Debug for LiveController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LiveController")
    }
}

pub(super) fn bounded(text: impl Into<String>) -> String {
    bound(
        text.into()
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c }),
        MAX_TEXT,
    )
}

/// [`bounded`] for text shown as paragraphs (messages and consent previews): line breaks are
/// kept, so an exact command stays on a line of its own, and the bound is roomier, so a preview
/// is never cut short of the command it asks consent for.
pub(super) fn bounded_lines(text: impl Into<String>) -> String {
    bound(
        text.into().chars().map(|c| match c {
            '\n' => '\n',
            c if c.is_control() => ' ',
            c => c,
        }),
        MAX_MESSAGE,
    )
}

/// The longest message or preview shown.
const MAX_MESSAGE: usize = 2_400;

fn bound(chars: impl Iterator<Item = char>, max: usize) -> String {
    let mut text: String = chars.collect();
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

impl LiveController {
    pub fn new(platform: Box<dyn Platform>, clock: Clock) -> Result<Self, LiveError> {
        let desc = platform.describe();
        let graph = graph::build(&desc)?;
        let flow = Flow::new(graph.specs.clone())?;
        let now = clock();
        let summary = flow.summary(now, &[]);
        let mut controller = Self {
            platform,
            desc,
            graph,
            clock,
            now,
            flow,
            ledger: Ledger::default(),
            summary,
            jobs: BTreeMap::new(),
            intents: VecDeque::new(),
            consents: BTreeMap::new(),
            details: BTreeMap::new(),
            previews: BTreeMap::new(),
            auto_begun: BTreeMap::new(),
            next_call: 1,
            own_calls: BTreeMap::new(),
            status_sent_at: None,
            status_waits: BTreeMap::new(),
            health: None,
            peer: None,
            connect: ConnectState::default(),
            practice: PracticeState::default(),
            maintenance: MaintenanceState::default(),
            screen: ScreenId::Welcome,
            view: crate::demo::disconnected_view(),
            signature: String::new(),
            effects: Vec::new(),
            motion: MotionPreference::Auto,
            notice: None,
            closed: false,
            change_since: None,
            step_apply_calls: BTreeMap::new(),
            support_checks: None,
            install_started: false,
            auto_consented: BTreeSet::new(),
            auto_advance: true,
            complete_since: None,
            agent_health_pending: false,
            permission_asks: PermissionAsks::default(),
        };
        controller.view.demo = false;
        controller.rebuild_view();
        Ok(controller)
    }

    pub(super) fn stamp(&mut self) {
        self.now = self.now.max((self.clock)());
    }

    /// Every core event goes through here; Observe batches pass the ledger first.
    pub(super) fn reduce(&mut self, event: FlowEvent) -> Result<(), FlowError> {
        self.stamp();
        let (event, observed) = match event {
            FlowEvent::Observe { samples } => {
                let samples = self.ledger.prepare(samples);
                (
                    FlowEvent::Observe {
                        samples: samples.clone(),
                    },
                    Some(samples),
                )
            }
            other => (other, None),
        };
        let result = self.flow.reduce(event, self.now);
        if let (Ok(_), Some(samples)) = (&result, observed) {
            self.ledger.accepted(samples);
        }
        let result = result.map(|intents| {
            for intent in intents {
                self.jobs.insert(intent.step, intent.clone());
                self.intents.push_back(intent);
            }
        });
        self.refresh_summary();
        result
    }

    pub(super) fn refresh_summary(&mut self) {
        self.summary = self.flow.summary(self.now, self.ledger.latest());
        let summary = &self.summary;
        self.jobs.retain(|step, _| {
            summary.steps.iter().any(|s| {
                s.id == *step
                    && matches!(
                        s.state,
                        StepState::Checking
                            | StepState::Planning
                            | StepState::NeedsAction
                            | StepState::Running
                            | StepState::Verifying
                    )
            })
        });
    }

    pub(super) fn step_state(&self, step: StepId) -> StepState {
        self.summary
            .steps
            .iter()
            .find(|s| s.id == step)
            .map_or(StepState::NotChecked, |s| s.state)
    }

    pub(super) fn satisfied(&self, step: StepId) -> bool {
        self.step_state(step) == StepState::Satisfied
    }

    pub(super) fn prerequisites_valid(&self, step: StepId) -> bool {
        self.graph
            .meta(step)
            .is_some_and(|m| m.prerequisites.iter().all(|p| self.satisfied(*p)))
    }

    pub(super) fn job(&self, step: StepId, stage: JobStage) -> Option<JobIntent> {
        self.jobs.get(&step).filter(|j| j.stage == stage).cloned()
    }

    pub(super) fn verified(
        &mut self,
        job: &JobIntent,
        source: ObservationSource,
        observed_at_ms: u64,
    ) {
        let _ = self.reduce(FlowEvent::Verified {
            step: job.step,
            operation: job.operation,
            verification: Verification {
                source,
                observed_at_ms,
                binding: None,
                activity: None,
                human_attempt: None,
                fixture_attempt: None,
            },
        });
    }

    pub(super) fn dispatch_intents(&mut self) {
        let mut budget = 256;
        while let Some(job) = self.intents.pop_front() {
            budget -= 1;
            if budget == 0 {
                break;
            }
            if self.jobs.get(&job.step) != Some(&job) {
                continue;
            }
            match self.graph.kind(job.step) {
                Some(StepKind::Native) => self.dispatch_native(job),
                Some(StepKind::Practice(role)) => self.dispatch_practice(job, role),
                Some(StepKind::Final) => self.dispatch_final(job),
                Some(StepKind::Pair) => self.dispatch_pair(job),
                Some(StepKind::Grants) => self.dispatch_grants(job),
                Some(StepKind::Layout) => self.dispatch_layout(job),
                Some(StepKind::Hiding) => self.dispatch_hiding(job),
                None => {}
            }
        }
    }

    fn dispatch_native(&mut self, job: JobIntent) {
        if matches!(
            job.stage,
            JobStage::Detect | JobStage::Plan | JobStage::Verify
        ) && self.graph.meta(job.step).is_some_and(|m| m.uses_status)
        {
            // Hold the job; a Status issued from now on will ride along with it.
            self.status_waits.insert(
                job.step,
                StatusWait {
                    job,
                    from_call: self.next_call,
                    started_at: self.now,
                },
            );
            self.request_status_now();
            return;
        }
        self.submit_step(job, None);
    }

    fn submit_step(&mut self, job: JobIntent, status: Option<StatusEvidence>) {
        let consent = if job.stage == JobStage::Apply {
            match self.consents.remove(&job.step) {
                Some(given) => Some(Consent {
                    operation: job.operation,
                    ..given
                }),
                None => {
                    // Core only schedules Apply after an explicit consent click.
                    self.details
                        .insert(job.step, "No consent was recorded for this change.".into());
                    let _ = self.reduce(FlowEvent::Failed {
                        step: job.step,
                        operation: job.operation,
                    });
                    return;
                }
            }
        } else {
            None
        };
        let step = job.step;
        let operation = job.operation;
        // An agent-applied step is performed here, by the controller, through the agent port.
        if job.stage == JobStage::Apply
            && let Some(kind) = self.graph.meta(step).and_then(|m| m.agent_apply)
        {
            let request = match kind {
                super::AgentApply::AskPermissions => InstallerRequest::AskPermissions,
                super::AgentApply::Restart => InstallerRequest::Restart,
            };
            match self.call(OwnCall::StepApply, request) {
                Ok(id) => {
                    self.step_apply_calls.insert(id, job);
                }
                Err(failure) => {
                    self.details
                        .insert(step, bounded(super::shared::failure_text(&failure)));
                    let _ = self.reduce(FlowEvent::Applied {
                        step,
                        operation,
                        outcome: super::shared::outcome(&failure),
                    });
                }
            }
            return;
        }
        if let Err(refusal) = self.platform.submit(NativeJob::Step {
            job,
            consent,
            status,
        }) {
            self.details.insert(step, bounded(refusal.to_string()));
            let _ = self.reduce(FlowEvent::Failed { step, operation });
        }
    }

    /// Issue a Status call now unless one is already outstanding.
    pub(super) fn request_status_now(&mut self) {
        // While the sequencer polls, its own Status replies release held jobs.
        if self.practice.engaged(self.now) || self.own_calls.values().any(|k| *k == OwnCall::Status)
        {
            return;
        }
        self.status_sent_at = Some(self.now);
        let _ = self.call(OwnCall::Status, InstallerRequest::Status);
    }

    /// Release held Verify jobs whose Status was issued after they began. A failed or pending
    /// reply is delivered too: the platform maps it to its own typed waiting state.
    fn release_status_waits(&mut self, reply: &AgentReply) {
        let ready: Vec<StepId> = self
            .status_waits
            .iter()
            .filter(|(_, wait)| reply.id >= wait.from_call)
            .map(|(step, _)| *step)
            .collect();
        for step in ready {
            let Some(wait) = self.status_waits.remove(&step) else {
                continue;
            };
            if self.jobs.get(&step) != Some(&wait.job) {
                continue;
            }
            self.submit_step(wait.job, Some(StatusEvidence(reply.clone())));
        }
    }

    /// Keep asking while a held job waits; give up honestly when no admissible reply comes.
    fn status_wait_tick(&mut self) {
        if self.status_waits.is_empty() {
            return;
        }
        let expired: Vec<StepId> = self
            .status_waits
            .iter()
            .filter(|(_, wait)| self.now.saturating_sub(wait.started_at) > STATUS_WAIT_MS)
            .map(|(step, _)| *step)
            .collect();
        for step in expired {
            let Some(wait) = self.status_waits.remove(&step) else {
                continue;
            };
            if self.jobs.get(&step) == Some(&wait.job) {
                self.details
                    .insert(step, "The agent didn't answer in time. Check again.".into());
                let _ = self.reduce(FlowEvent::Waiting {
                    step,
                    operation: wait.job.operation,
                    kind: crosspane_installer_core::WaitKind::Contract,
                });
            }
        }
        self.status_waits
            .retain(|step, wait| self.jobs.get(step) == Some(&wait.job));
        if !self.status_waits.is_empty() {
            self.request_status_now();
        }
    }

    /// Take a newly finished support pass, if there is one. Display only: it never changes state.
    fn poll_support_checks(&mut self) {
        let Some(latest) = self.platform.support_checks() else {
            return;
        };
        let known = self
            .support_checks
            .as_ref()
            .is_some_and(|(seen, _)| seen.pass == latest.pass && seen.step == latest.step);
        if !known && self.graph.kind(latest.step) == Some(StepKind::Native) {
            self.support_checks = Some((latest, self.now));
        }
    }

    fn drain_platform(&mut self) {
        let reports = self.platform.poll();
        self.stamp();
        // Checks before reports: a pass is published before the report that ends its job.
        self.poll_support_checks();
        if !reports.is_empty() && self.change_since.is_some() {
            self.change_since = Some(self.now);
        }
        for report in reports {
            match report {
                NativeReport::Step(report) => self.native_report(report),
                NativeReport::Maintenance(report) => self.maintenance_report(report),
            }
            self.dispatch_intents();
        }
    }

    fn native_report(&mut self, report: StepReport) {
        let step = report.job.step;
        if self.graph.kind(step) != Some(StepKind::Native)
            || self.jobs.get(&step) != Some(&report.job)
        {
            return;
        }
        let operation = report.job.operation;
        let event = match (report.job.stage, report.outcome) {
            (_, NativeOutcome::Progress) => {
                self.details.insert(step, bounded(report.detail));
                return;
            }
            (JobStage::Detect, NativeOutcome::Detected { needs_action }) => FlowEvent::Detected {
                step,
                operation,
                needs_action,
            },
            (JobStage::Plan, NativeOutcome::Planned { preview }) => {
                self.previews.insert(step, bounded_lines(preview));
                FlowEvent::Planned { step, operation }
            }
            (JobStage::Apply, NativeOutcome::Applied(outcome)) => FlowEvent::Applied {
                step,
                operation,
                outcome,
            },
            (
                JobStage::Verify,
                NativeOutcome::Verified {
                    source,
                    observed_at_ms,
                },
            ) => {
                self.details.insert(step, bounded(report.detail));
                let job = report.job;
                self.verified(&job, source, observed_at_ms);
                return;
            }
            (_, NativeOutcome::Waiting(kind)) => FlowEvent::Waiting {
                step,
                operation,
                kind,
            },
            (_, NativeOutcome::Failed) => FlowEvent::Failed { step, operation },
            (_, NativeOutcome::Unsupported) => FlowEvent::Unsupported { step, operation },
            // An outcome that doesn't belong to the job's stage is a platform fault. It fails the
            // job honestly instead of leaving the step working forever.
            _ => {
                self.details.insert(
                    step,
                    "This step got an unexpected answer. Check again.".into(),
                );
                let _ = self.reduce(FlowEvent::Failed { step, operation });
                return;
            }
        };
        self.details.insert(step, bounded(report.detail));
        let _ = self.reduce(event);
    }

    /// Own calls, settings and the tutorial share one increasing id namespace.
    pub(super) fn alloc_call_id(&mut self) -> u64 {
        let id = self
            .next_call
            .max(self.practice.tutorial.last_call_id().saturating_add(1));
        self.next_call = id.saturating_add(1);
        id
    }

    pub(super) fn submit_call(
        &mut self,
        kind: OwnCall,
        call: AgentCall,
    ) -> Result<u64, CallFailure> {
        let id = call.id;
        self.next_call = self.next_call.max(id.saturating_add(1));
        self.platform.agent().submit(call)?;
        self.own_calls.insert(id, kind);
        Ok(id)
    }

    pub(super) fn call(
        &mut self,
        kind: OwnCall,
        request: InstallerRequest,
    ) -> Result<u64, CallFailure> {
        let id = self.alloc_call_id();
        self.submit_call(
            kind,
            AgentCall {
                id,
                request,
                timeout_ms: CALL_TIMEOUT_MS,
            },
        )
    }

    fn drain_agent(&mut self) {
        let replies = self.platform.agent().poll();
        self.stamp();
        for reply in replies {
            self.now = self.now.max(reply.observed_at_ms);
            match self.own_calls.remove(&reply.id) {
                Some(kind) => self.own_reply(kind, reply),
                None => self.practice_reply(reply),
            }
            self.dispatch_intents();
        }
    }

    fn own_reply(&mut self, kind: OwnCall, reply: AgentReply) {
        match kind {
            OwnCall::Status => self.on_status(&reply, true),
            OwnCall::PairScan => self.scan_reply(reply),
            OwnCall::PairListen | OwnCall::PairJoin | OwnCall::Dial => {
                self.pair_started(kind, reply)
            }
            OwnCall::PairStatus => self.pair_status_reply(reply),
            OwnCall::PairConfirm | OwnCall::PairPick => self.pair_answered(reply),
            OwnCall::Allow => self.allow_reply(reply),
            OwnCall::Place => self.place_reply(reply),
            OwnCall::Settings => self.settings_reply(reply),
            OwnCall::StepApply => self.step_apply_reply(reply),
            OwnCall::Permission => self.permission_reply(reply),
        }
    }

    /// Admit a Status reply's health; the controller's own replies also feed the ledger.
    pub(super) fn on_status(&mut self, reply: &AgentReply, own: bool) {
        match &reply.result {
            Ok(DecodedReply::Status(StatusAdmission::Supported(snapshot))) => {
                self.health = Some(Health {
                    snapshot: snapshot.clone(),
                    observed_at_ms: reply.observed_at_ms,
                    call: reply.id,
                    source: reply.source,
                    reply: reply.clone(),
                });
                if own {
                    self.agent_health_pending = false;
                    let samples = self.samples(snapshot, reply.source, reply.observed_at_ms);
                    let _ = self.reduce(FlowEvent::Observe { samples });
                }
            }
            _ => {
                if own {
                    self.agent_health_pending = matches!(
                        reply.result,
                        Ok(DecodedReply::Status(
                            StatusAdmission::PendingHealthContract(_)
                        ))
                    );
                    self.health = None;
                    let _ = self.reduce(FlowEvent::Observe {
                        samples: Vec::new(),
                    });
                }
            }
        }
        self.dispatch_intents();
        self.after_status(reply.id);
        self.release_status_waits(reply);
        self.release_repair_wait(reply);
    }

    /// The stable union: the local scope plus the selected peer while it is connected.
    fn samples(
        &self,
        health: &HealthSnapshot,
        source: ObservationSource,
        at: u64,
    ) -> Vec<crosspane_installer_core::CounterSample> {
        let mut samples = Vec::new();
        if let Ok(local) = crate::agent_contract::counter_sample(health, None, source, at) {
            samples.push(local.sample);
        }
        if let Some(peer) = self.peer
            && let Ok(scoped) =
                crate::agent_contract::counter_sample(health, Some(peer), source, at)
        {
            samples.push(scoped.sample);
        }
        samples
    }

    fn poll_status(&mut self) {
        if self.practice.engaged(self.now)
            || self.summary.milestone == crosspane_installer_core::Milestone::NotInstalled
            || self.own_calls.values().any(|k| *k == OwnCall::Status)
        {
            return;
        }
        let interval = if self.live_screen() {
            STATUS_ACTIVE_MS
        } else {
            STATUS_IDLE_MS
        };
        if self
            .status_sent_at
            .is_some_and(|sent| self.now < sent.saturating_add(interval))
        {
            return;
        }
        self.status_sent_at = Some(self.now);
        let _ = self.call(OwnCall::Status, InstallerRequest::Status);
    }

    pub(super) fn live_screen(&self) -> bool {
        // The permission rows follow each grant as macOS reports it (WP-4.33).
        if self.screen == ScreenId::Permissions && self.permission_rows_active() {
            return true;
        }
        matches!(
            self.screen,
            ScreenId::Connect
                | ScreenId::MatchNumbers
                | ScreenId::Grants
                | ScreenId::Layout
                | ScreenId::Practice
                | ScreenId::HidingChoice
                | ScreenId::Summary
        )
    }

    /// Detection only: Begin never mutates. Practice steps wait for the person.
    fn auto_begin(&mut self) {
        let candidates: Vec<StepId> = self
            .graph
            .on_screen(self.screen)
            .filter(|m| !matches!(m.kind, StepKind::Practice(_)))
            .map(|m| m.id)
            .collect();
        for step in candidates {
            // A step that reads the agent (it may still be starting, or waiting for the person to
            // unlock the key store) is looked at again on its own screen, quietly and rarely.
            let state = self.step_state(step);
            let recheck = self.graph.meta(step).is_some_and(|m| m.uses_status)
                && matches!(
                    state,
                    StepState::PendingContract
                        | StepState::WaitingForUser
                        | StepState::WaitingForPeer
                );
            if !(matches!(state, StepState::NotChecked | StepState::Stale) || recheck)
                || !self.prerequisites_valid(step)
                || self
                    .auto_begun
                    .get(&step)
                    .is_some_and(|at| self.now < at.saturating_add(AUTO_BEGIN_MS))
            {
                continue;
            }
            self.auto_begun.insert(step, self.now);
            let _ = self.reduce(FlowEvent::Begin { step });
            self.dispatch_intents();
        }
    }

    /// The install steps go ahead on their own once the person has started setup: each one is
    /// still planned and previewed, and the consent the platform requires is the person's start
    /// click, recorded against that exact preview. Never for steps that need an administrator, a
    /// system permission or a firewall change, and never while the agent is in use (a restart or
    /// replacement would cut that short): then the step asks, with its preview, like any other.
    /// Each step goes ahead by itself at most once per run; after that it asks (see
    /// [`Self::auto_consented`]).
    fn auto_consent(&mut self) {
        if !self.install_started || !automatic_screen(self.screen) || !self.agent_quiet() {
            return;
        }
        let ready: Vec<StepId> = self
            .graph
            .on_screen(self.screen)
            .filter(|m| m.kind == StepKind::Native && !self.auto_consented.contains(&m.id))
            .filter(|m| {
                self.step_state(m.id) == StepState::NeedsAction
                    && self.previews.contains_key(&m.id)
                    && self.job(m.id, JobStage::Plan).is_some()
            })
            .map(|m| m.id)
            .collect();
        for step in ready {
            // Recorded before the attempt: whatever happens to it, this was the step's one go.
            self.auto_consented.insert(step);
            self.consent_click(step);
        }
        self.dispatch_intents();
    }

    /// Whether nothing the person is doing right now would be interrupted by a restart: no input
    /// is shared, no window is projected and no sound is playing across. An agent that can't
    /// report this (too old) counts as in use.
    pub(super) fn agent_quiet(&self) -> bool {
        if self.agent_health_pending {
            return false;
        }
        let Some(health) = self.health.as_ref().map(|h| h.snapshot.as_ref()) else {
            return true;
        };
        let terminal = health.terminal();
        terminal.controlling.is_none()
            && terminal.controlled_by.is_none()
            && terminal.projections.is_empty()
            && health.installer().audio.active_peers.is_empty()
    }

    /// Whether everything on the current screen is done, so it can move on by itself. A step that
    /// is asking a question (even an optional one) holds the screen until it is answered.
    fn screen_settled(&self) -> bool {
        if self.screen == ScreenId::Practice {
            return self
                .graph
                .practice_steps()
                .iter()
                .all(|step| self.satisfied(*step));
        }
        self.screen_complete(self.screen)
            && !self
                .graph
                .on_screen(self.screen)
                .any(|m| self.step_state(m.id) == StepState::NeedsAction)
    }

    /// Move on from a finished screen without a click. Screens of one page follow each other at
    /// once; a new page comes after a short pause so the last check is seen.
    fn auto_advance_tick(&mut self) {
        let eligible = self.auto_advance
            && advances_by_itself(self.screen)
            && self.display_screen() == self.screen
            && !self.mutation_in_flight()
            && self.screen_settled();
        if !eligible {
            self.complete_since = None;
            return;
        }
        let Some(next) = self.next_screen() else {
            return;
        };
        let since = *self.complete_since.get_or_insert(self.now);
        let pause = if automatic_screen(self.screen) && automatic_screen(next) {
            0
        } else {
            ADVANCE_PAUSE_MS
        };
        if self.now >= since.saturating_add(pause) {
            self.go(next);
            // Start looking at the new screen's steps in this same pass.
            self.auto_begin();
            self.dispatch_intents();
        }
    }

    pub(super) fn begin(&mut self, step: StepId) {
        self.auto_begun.insert(step, self.now);
        self.details.remove(&step);
        self.previews.remove(&step);
        if let Err(error) = self.reduce(FlowEvent::Begin { step }) {
            self.details.insert(step, bounded(begin_error(error)));
        }
        self.dispatch_intents();
    }

    pub(super) fn screen_present(&self, screen: ScreenId) -> bool {
        matches!(
            screen,
            ScreenId::Welcome | ScreenId::Practice | ScreenId::Summary
        ) || self.graph.on_screen(screen).next().is_some()
    }

    pub(super) fn screen_complete(&self, screen: ScreenId) -> bool {
        self.graph.on_screen(screen).all(|m| {
            self.satisfied(m.id)
                || (m.settles_with_peer
                    && !matches!(
                        self.step_state(m.id),
                        StepState::NotChecked
                            | StepState::Checking
                            | StepState::Planning
                            | StepState::Running
                            | StepState::Verifying
                            | StepState::Unsupported
                    ))
        })
    }

    /// A step whose proof includes live traffic is re-checked as soon as a peer is connected.
    pub(super) fn recheck_traffic_steps(&mut self) {
        if self.connected_peers().is_empty() {
            return;
        }
        let steps: Vec<StepId> = self
            .graph
            .metas
            .iter()
            .filter(|m| m.settles_with_peer)
            .map(|m| m.id)
            .collect();
        for step in steps {
            if matches!(
                self.step_state(step),
                StepState::NotChecked
                    | StepState::Stale
                    | StepState::WaitingForUser
                    | StepState::WaitingForPeer
                    | StepState::PendingContract
                    | StepState::NeedsAction
            ) && self.prerequisites_valid(step)
                && !self
                    .auto_begun
                    .get(&step)
                    .is_some_and(|at| self.now < at.saturating_add(AUTO_BEGIN_MS))
            {
                self.begin(step);
            }
        }
    }

    pub(super) fn next_screen(&self) -> Option<ScreenId> {
        let at = ORDER.iter().position(|s| *s == self.screen)?;
        ORDER[at + 1..]
            .iter()
            .copied()
            .find(|s| self.screen_present(*s))
    }

    pub(super) fn previous_screen(&self) -> Option<ScreenId> {
        let at = ORDER.iter().position(|s| *s == self.screen)?;
        ORDER[..at]
            .iter()
            .rev()
            .copied()
            .find(|s| self.screen_present(*s))
    }

    pub(super) fn go(&mut self, screen: ScreenId) {
        if self.screen != screen {
            if self.screen == ScreenId::Layout {
                self.effects.push(ShellEffect::CancelLayoutDrag);
            }
            if self.screen == ScreenId::Connect {
                self.connect.leave();
            }
            self.screen = screen;
            self.notice = None;
            self.complete_since = None;
            // Going forward (or anywhere else) lets the new screen move on by itself; `back`
            // turns that off again for the screen it returns to.
            self.auto_advance = true;
            if screen == ScreenId::RepairRemove {
                self.inspect_maintenance();
            }
            if screen == ScreenId::Connect {
                self.scan();
            }
        }
    }

    pub(super) fn mutation_in_flight(&self) -> bool {
        self.jobs.values().any(|j| j.stage == JobStage::Apply)
            || self.maintenance.running()
            || self.maintenance.repair_waiting()
            || self.practice.engaged(self.now)
    }

    /// A native change (a platform Apply or a removal) that closing the window would cut short.
    pub(super) fn native_change_running(&self) -> bool {
        self.jobs.values().any(|j| {
            j.stage == JobStage::Apply && self.graph.kind(j.step) == Some(StepKind::Native)
        }) || self.maintenance.running()
    }

    pub(super) fn try_close(&mut self) -> bool {
        if self.native_change_running() {
            self.notice = Some("Wait for the current change to finish before closing.".into());
            return false;
        }
        InstallerController::close(self);
        true
    }

    /// All rows, including steps not on the current screen.
    pub fn rows(&self) -> Vec<crate::view::RowView> {
        self.graph.metas.iter().map(|m| self.row(m.id)).collect()
    }
}

pub(super) fn begin_error(error: FlowError) -> &'static str {
    match error {
        FlowError::PrerequisitePending => "An earlier step needs to be finished first.",
        FlowError::Busy => "This step is already being checked.",
        _ => "This step can't be checked right now.",
    }
}

impl InstallerController for LiveController {
    fn view(&self) -> &WizardView {
        &self.view
    }

    fn accept(&mut self, action: WizardAction) -> bool {
        if self.closed {
            return true;
        }
        self.stamp();
        let current = action.revision == self.view.revision;
        let close = match action.intent {
            WizardIntent::EditPeerAddress { field, value } => {
                self.edit_address(field, value);
                false
            }
            WizardIntent::SetMotion(motion) => {
                self.motion = motion;
                false
            }
            _ if !current => false,
            WizardIntent::Button(id) => {
                let enabled = self.view.buttons.iter().any(|b| b.id == id && b.enabled);
                enabled && self.button(id)
            }
            WizardIntent::Back => {
                self.back();
                false
            }
            WizardIntent::Close => self.try_close(),
            WizardIntent::SetToggle { field, checked } => {
                self.toggle(field, checked);
                false
            }
            WizardIntent::ChooseHiding(choice) => {
                self.choose_hiding(choice);
                false
            }
            WizardIntent::Layout(action) => {
                self.layout_action(action);
                false
            }
        };
        if close {
            return true;
        }
        self.dispatch_intents();
        self.rebuild_view();
        false
    }

    fn tick(&mut self) -> ControllerTick {
        if self.closed {
            return ControllerTick::default();
        }
        self.stamp();
        self.drain_platform();
        self.change_since = if self.native_change_running() {
            Some(self.change_since.unwrap_or(self.now))
        } else {
            None
        };
        self.drain_agent();
        self.drain_fixtures();
        self.practice_tick();
        self.poll_status();
        self.status_wait_tick();
        self.repair_tick();
        self.connect_tick();
        self.permissions_tick();
        self.auto_begin();
        self.dispatch_intents();
        self.auto_consent();
        // A change started in this pass is guarded from now, not from the next pass.
        if self.change_since.is_none() && self.native_change_running() {
            self.change_since = Some(self.now);
        }
        self.refresh_summary();
        self.auto_advance_tick();
        self.refresh_summary();
        self.rebuild_view();
        ControllerTick {
            effects: std::mem::take(&mut self.effects),
            wake_after_ms: Some(if self.mutation_in_flight() || self.live_screen() {
                100
            } else {
                250
            }),
        }
    }

    fn request_close(&mut self) -> bool {
        if self.closed {
            return true;
        }
        self.stamp();
        let quiet_for = self
            .change_since
            .map_or(0, |since| self.now.saturating_sub(since));
        if self.native_change_running() && quiet_for <= CLOSE_GUARD_MS {
            self.notice = Some("Wait for the current change to finish before closing.".into());
            self.rebuild_view();
            return false;
        }
        InstallerController::close(self);
        true
    }

    fn close(&mut self) {
        if self.closed {
            return;
        }
        self.practice.deferred = None;
        self.cancel_practice();
        self.platform.fixtures().retire();
        self.platform.shutdown();
        self.closed = true;
    }
}
