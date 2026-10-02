mod audio;
mod cleanup;
mod evidence;
mod fixture;
use evidence::*;

use super::messages::*;
use crate::agent_contract::*;
use crosspane_installer_core::{
    AttemptId, CounterId, CounterSample, EpochDependencies, EvidenceError, FlowEvent, JobIntent,
    JobStage, StepId, Verification, check_activity,
};
use crosspane_types::id::{NodeId, WindowId};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Preflight,
    Practice,
    Command,
    Projection,
    Terminal,
    Cleaning,
}
struct OwnedFixture {
    id: u64,
    window: WindowId,
    armed: bool,
    clicks: u64,
    clicked: bool,
    home_at: Option<u64>,
}
struct ProjectionCleanup {
    started: u64,
    returned: u64,
    failed: u64,
    terminal_at: Option<u64>,
}
struct Session {
    binding: TutorialBinding,
    context: TutorialContext,
    stage: Stage,
    start: Option<CounterSample>,
    end: Option<CounterSample>,
    fixture: Option<OwnedFixture>,
    tone: Option<u64>,
    audio_after: Option<u64>,
    tone_unknown: bool,
    projection: Option<ProjectionRef>,
    projection_observed: bool,
    projection_unknown: bool,
    projection_authorized: bool,
    projection_after: Option<u64>,
    projection_action_after: Option<u64>,
    unowned_projection: Option<ProjectionRef>,
    command_acked: bool,
    remote_windows: Vec<WindowId>,
    selected: Option<WindowId>,
    human: BTreeSet<HumanConfirmation>,
    sequence: u64,
    fixture_time: u64,
    retired: bool,
    release_sent: bool,
    return_sent: bool,
    stop_sent: bool,
    close_sent: bool,
    close_ack: bool,
    ownership_lost: bool,
    fixture_proved: bool,
    projection_cleanup: Option<ProjectionCleanup>,
    retired_at: u64,
    admitted_at: u64,
}
/// No native authority or persisted proof is created here. Feed effects to core in order.
pub struct Tutorial {
    session: Option<Session>,
    state: TutorialState,
    detail: TutorialDetail,
    last_attempt: u64,
    last_operation: u64,
    next_call: u64,
    now: u64,
    agent_calls: BTreeMap<u64, (InstallerRequest, u64)>,
    fixture_calls: BTreeMap<u64, TutorialFixtureAction>,
    fixture_sent: BTreeMap<u64, u64>,
    submitted: BTreeSet<u64>,
    tracked: BTreeMap<StepId, Option<NodeId>>,
    samples: Vec<CounterSample>,
    health: Option<Box<HealthSnapshot>>,
}
impl std::fmt::Debug for Tutorial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Tutorial")
    }
}
impl Default for Tutorial {
    fn default() -> Self {
        Self {
            session: None,
            state: TutorialState::Idle,
            detail: TutorialDetail::default(),
            last_attempt: 0,
            last_operation: 0,
            next_call: 1,
            now: 0,
            agent_calls: BTreeMap::new(),
            fixture_calls: BTreeMap::new(),
            fixture_sent: BTreeMap::new(),
            submitted: BTreeSet::new(),
            tracked: BTreeMap::new(),
            samples: Vec::new(),
            health: None,
        }
    }
}
impl Tutorial {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn state(&self) -> TutorialState {
        self.state
    }
    pub fn detail(&self) -> &TutorialDetail {
        &self.detail
    }
    pub fn current_samples(&self) -> &[CounterSample] {
        &self.samples
    }
    pub fn last_call_id(&self) -> u64 {
        self.next_call - 1
    }
    /// Mark a call only after ordered Core effects permit dispatch and the port accepts it.
    /// CoreRejected discards unsubmitted calls; submitted calls retain late cleanup authority.
    pub fn submitted(&mut self, id: u64) -> Result<(), TutorialError> {
        if (!self.agent_calls.contains_key(&id) && !self.fixture_calls.contains_key(&id))
            || !self.submitted.insert(id)
        {
            return Err(TutorialError::InvalidObservation);
        }
        Ok(())
    }
    /// Seed the shared call namespace when handing back from settings, never reusing an id.
    pub fn advance_call_ids(&mut self, last_call_id: u64) -> Result<(), TutorialError> {
        if !self.agent_calls.is_empty() || !self.fixture_calls.is_empty() {
            return Err(TutorialError::Busy);
        }
        self.next_call = self.next_call.max(
            last_call_id
                .checked_add(1)
                .ok_or(TutorialError::IdExhausted)?,
        );
        Ok(())
    }
    pub fn begin(
        &mut self,
        attempt: TutorialAttempt,
        context: TutorialContext,
        verify_job: JobIntent,
        view_revision: u64,
        now_ms: u64,
    ) -> Result<Vec<TutorialEffect>, TutorialError> {
        if self
            .session
            .as_ref()
            .is_some_and(|s| !s.retired && self.state != TutorialState::Verified)
            || !self.agent_calls.is_empty()
            || !self.fixture_calls.is_empty()
            || self.session.as_ref().is_some_and(|s| {
                s.fixture.is_some()
                    || s.tone.is_some()
                    || s.projection.is_some()
                    || s.projection_unknown
                    || s.tone_unknown
                    || s.projection_cleanup.is_some()
            })
        {
            return Err(TutorialError::Busy);
        }
        if attempt.attempt.0 <= self.last_attempt || now_ms < self.now {
            return Err(TutorialError::InvalidAttempt);
        }
        if attempt.attempt.0 == u64::MAX || attempt.operation.0 == u64::MAX {
            return Err(TutorialError::IdExhausted);
        }
        if verify_job.stage != JobStage::Verify
            || verify_job.step != attempt.step
            || verify_job.operation != attempt.operation
            || attempt.operation.0 <= self.last_operation
        {
            return Err(TutorialError::WrongOperation);
        }
        if (attempt.role == TutorialRole::Menu) != attempt.peer.is_none()
            || attempt.peer == Some(attempt.local)
        {
            return Err(TutorialError::WrongPeer);
        }
        if !bounded(&context.machine_label, 64)
            || context.machine_label.is_empty()
            || !matches!(
                (context.platform, context.source_policy),
                (AgentPlatform::Linux, TutorialSourcePolicy::Native)
                    | (
                        AgentPlatform::Macos,
                        TutorialSourcePolicy::MacMirror | TutorialSourcePolicy::MacPrivateDisplay
                    )
            )
        {
            return Err(TutorialError::InsufficientContract);
        }
        if attempt.role == TutorialRole::AudioSender
            && !context.speakers.as_ref().is_some_and(|s| {
                Some(s.peer) == attempt.peer
                    && !s.device_key.is_empty()
                    && bounded(&s.device_key, 160)
            })
        {
            return Err(TutorialError::WrongPeer);
        }
        if self.tracked.len() >= MAX_ITEMS && !self.tracked.contains_key(&attempt.step) {
            return Err(TutorialError::InsufficientContract);
        }
        self.last_attempt = attempt.attempt.0;
        self.last_operation = attempt.operation.0;
        self.now = now_ms;
        self.tracked.insert(attempt.step, attempt.peer);
        self.session = Some(Session {
            binding: TutorialBinding {
                attempt,
                view_revision,
                evidence: None,
            },
            context,
            stage: Stage::Preflight,
            start: None,
            end: None,
            fixture: None,
            tone: None,
            audio_after: None,
            tone_unknown: false,
            projection: None,
            projection_observed: false,
            projection_unknown: false,
            projection_authorized: false,
            projection_after: None,
            projection_action_after: None,
            unowned_projection: None,
            command_acked: false,
            remote_windows: Vec::new(),
            selected: None,
            human: BTreeSet::new(),
            sequence: 0,
            fixture_time: 0,
            retired: false,
            release_sent: false,
            return_sent: false,
            stop_sent: false,
            close_sent: false,
            close_ack: false,
            ownership_lost: false,
            fixture_proved: false,
            projection_cleanup: None,
            retired_at: 0,
            admitted_at: now_ms,
        });
        self.detail = TutorialDetail::default();
        self.state = TutorialState::WaitingHealth;
        let mut effects = self.observe_effects();
        self.call(InstallerRequest::Status, &mut effects)?;
        Ok(effects)
    }
    pub fn reduce(
        &mut self,
        event: TutorialEvent,
        now_ms: u64,
    ) -> Result<Vec<TutorialEffect>, TutorialError> {
        if now_ms < self.now {
            return Err(TutorialError::InvalidObservation);
        }
        self.now = now_ms;
        let mut effects = self.observe_effects();
        match event {
            TutorialEvent::Reply(reply) => self.reply(reply, &mut effects)?,
            event @ TutorialEvent::Fixture { .. } => {
                self.fixture(event, &mut effects)?;
            }
            TutorialEvent::FixtureSubmitFailed {
                attempt,
                call_id,
                error,
            } => {
                self.attempt(attempt)?;
                let action = self
                    .fixture_calls
                    .remove(&call_id)
                    .ok_or(TutorialError::InvalidObservation)?;
                self.submitted.remove(&call_id);
                self.fixture_sent.remove(&call_id);
                if matches!(action, TutorialFixtureAction::PlayTone { .. })
                    && error != TutorialFixtureError::TimedOut
                {
                    self.session_mut()?.tone_unknown = false;
                }
                self.fail(TutorialFailure::Fixture(error), &mut effects)?;
            }
            TutorialEvent::User {
                attempt,
                view_revision,
                action,
            } => {
                self.attempt(attempt)?;
                let s = self.session()?;
                if s.binding.view_revision != view_revision {
                    return Err(TutorialError::RetiredView);
                }
                if s.retired {
                    if action != TutorialUserAction::Confirm(HumanConfirmation::SourceRestored) {
                        return Err(TutorialError::RetiredView);
                    }
                    self.confirm_remote_restoration(&mut effects)?;
                } else if action == TutorialUserAction::Cancel {
                    self.retire(TutorialState::Cancelled, &mut effects)?;
                } else {
                    if !self.fresh() || self.session()?.stage == Stage::Preflight {
                        return Err(TutorialError::InvalidObservation);
                    }
                    self.user(action, &mut effects)?;
                }
            }
            TutorialEvent::CoreJob(job) => {
                let a = &self.session()?.binding.attempt;
                if job.stage != JobStage::Verify
                    || job.step != a.step
                    || job.operation != a.operation
                    || self.session()?.retired
                {
                    return Err(TutorialError::WrongOperation);
                }
            }
            TutorialEvent::CoreRejected(error) => {
                self.discard_unsubmitted()?;
                self.fail(TutorialFailure::Core(error), &mut effects)?;
                if !self
                    .agent_calls
                    .values()
                    .any(|(r, _)| *r == InstallerRequest::Status)
                {
                    self.call(InstallerRequest::Status, &mut effects)?;
                }
            }
            TutorialEvent::Tick => {
                if !self
                    .agent_calls
                    .values()
                    .any(|(r, _)| *r == InstallerRequest::Status)
                {
                    self.call(InstallerRequest::Status, &mut effects)?;
                }
            }
        }
        self.finish(&mut effects)?;
        Ok(effects)
    }
    fn session(&self) -> Result<&Session, TutorialError> {
        self.session.as_ref().ok_or(TutorialError::InvalidAttempt)
    }
    fn session_mut(&mut self) -> Result<&mut Session, TutorialError> {
        self.session.as_mut().ok_or(TutorialError::InvalidAttempt)
    }
    fn attempt(&self, attempt: AttemptId) -> Result<(), TutorialError> {
        if self.session()?.binding.attempt.attempt != attempt {
            Err(TutorialError::InvalidAttempt)
        } else {
            Ok(())
        }
    }
    fn effect(&self, kind: TutorialEffectKind) -> Result<TutorialEffect, TutorialError> {
        Ok(TutorialEffect {
            binding: self.session()?.binding.clone(),
            kind,
        })
    }
    fn observe_effects(&self) -> Vec<TutorialEffect> {
        self.effect(TutorialEffectKind::Core(FlowEvent::Observe {
            samples: self.samples.clone(),
        }))
        .into_iter()
        .collect()
    }
    fn id(&mut self) -> Result<u64, TutorialError> {
        let id = self.next_call;
        self.next_call = id.checked_add(1).ok_or(TutorialError::IdExhausted)?;
        Ok(id)
    }
    fn call(
        &mut self,
        request: InstallerRequest,
        effects: &mut Vec<TutorialEffect>,
    ) -> Result<(), TutorialError> {
        if self.agent_calls.len() >= MAX_QUEUE {
            return Err(TutorialError::Busy);
        }
        let id = self.id()?;
        if matches!(
            request,
            InstallerRequest::Project { .. } | InstallerRequest::Pull { .. }
        ) {
            self.prepare_projection_cleanup()?;
            self.session_mut()?.projection_unknown = true;
            self.session_mut()?.projection_authorized = true;
            // Lost acknowledgements retain cleanup authority after issuance, never proof.
            self.session_mut()?.projection_after = Some(self.now);
            self.session_mut()?.projection_action_after = Some(self.now);
        }
        self.agent_calls.insert(id, (request.clone(), self.now));
        effects.push(self.effect(TutorialEffectKind::Agent(AgentCall {
            id,
            request,
            timeout_ms: MAX_TIMEOUT_MS,
        }))?);
        Ok(())
    }
    fn fixture_call(
        &mut self,
        action: TutorialFixtureAction,
        effects: &mut Vec<TutorialEffect>,
    ) -> Result<(), TutorialError> {
        if self.fixture_calls.len() >= MAX_QUEUE {
            return Err(TutorialError::Busy);
        }
        let call_id = self.id()?;
        if matches!(action, TutorialFixtureAction::PlayTone { .. }) {
            self.session_mut()?.tone_unknown = true;
        }
        self.fixture_calls.insert(call_id, action.clone());
        self.fixture_sent.insert(call_id, self.now);
        effects.push(self.effect(TutorialEffectKind::Fixture { call_id, action })?);
        Ok(())
    }
    fn fresh(&self) -> bool {
        self.samples
            .first()
            .is_some_and(|s| s.observed_at_ms <= self.now && self.now - s.observed_at_ms <= 5000)
    }
    fn reply(
        &mut self,
        reply: AgentReply,
        effects: &mut Vec<TutorialEffect>,
    ) -> Result<(), TutorialError> {
        let (request, sent) = self
            .agent_calls
            .get(&reply.id)
            .ok_or(TutorialError::InvalidObservation)?;
        if reply.observed_at_ms < *sent
            || reply.observed_at_ms > self.now
            || reply.source != ObservationSource::Live
        {
            return Err(TutorialError::InvalidObservation);
        }
        let request = request.clone();
        self.agent_calls.remove(&reply.id);
        self.submitted.remove(&reply.id);
        match reply.result {
            Err(error) => {
                self.fail(TutorialFailure::Agent(error.clone()), effects)?;
                if error == CallFailure::TimeoutOutcomeUnknown
                    && request != InstallerRequest::Status
                {
                    effects.push(self.effect(TutorialEffectKind::DetectAfterUnknown)?);
                    self.detect(effects)?;
                }
            }
            Ok(DecodedReply::Status(StatusAdmission::Supported(health)))
                if request == InstallerRequest::Status =>
            {
                self.status(health, reply.source, reply.observed_at_ms, effects)?;
            }
            Ok(DecodedReply::Status(StatusAdmission::PendingHealthContract(_)))
                if request == InstallerRequest::Status =>
            {
                self.samples.clear();
                self.health = None;
                effects.clear();
                effects.extend(self.observe_effects());
                self.retire(TutorialState::PendingContract, effects)?;
                effects.push(self.effect(TutorialEffectKind::WaitForContract)?);
            }
            Ok(DecodedReply::Windows(windows)) if request == InstallerRequest::Windows => {
                let s = self.session()?;
                if s.retired {
                    return Ok(());
                }
                let window = s
                    .fixture
                    .as_ref()
                    .ok_or(TutorialError::InvalidObservation)?
                    .window;
                if !windows.iter().any(|w| w.id == window) {
                    self.fail(TutorialFailure::Isolation, effects)?;
                } else {
                    self.session_mut()?.stage = Stage::Command;
                    self.call(
                        InstallerRequest::Project {
                            window,
                            peer: self.peer()?,
                        },
                        effects,
                    )?;
                }
            }
            Ok(DecodedReply::WindowsFrom(windows))
                if matches!(request, InstallerRequest::WindowsFrom { .. }) =>
            {
                if self.session()?.retired {
                    return Ok(());
                }
                self.session_mut()?.remote_windows = windows.into_iter().map(|w| w.id).collect();
                self.state = TutorialState::WaitingUser;
                effects.push(self.effect(TutorialEffectKind::WaitForUser)?);
            }
            Ok(DecodedReply::Acknowledged)
                if matches!(
                    request,
                    InstallerRequest::Project { .. } | InstallerRequest::Pull { .. }
                ) =>
            {
                self.session_mut()?.projection_after = Some(reply.observed_at_ms);
                if self.session()?.retired {
                    self.session_mut()?.command_acked = true;
                    self.detect(effects)?;
                    return Ok(());
                }
                self.session_mut()?.command_acked = true;
                self.detect(effects)?;
            }
            Ok(DecodedReply::Acknowledged)
                if matches!(
                    request,
                    InstallerRequest::Release | InstallerRequest::Return { .. }
                ) =>
            {
                self.detect(effects)?;
            }
            _ => self.fail(
                TutorialFailure::Agent(CallFailure::InvalidResponse),
                effects,
            )?,
        }
        Ok(())
    }
    fn peer(&self) -> Result<NodeId, TutorialError> {
        self.session()?
            .binding
            .attempt
            .peer
            .ok_or(TutorialError::WrongPeer)
    }
    fn status(
        &mut self,
        health: Box<HealthSnapshot>,
        source: ObservationSource,
        at: u64,
        effects: &mut Vec<TutorialEffect>,
    ) -> Result<(), TutorialError> {
        if health.installer().node != self.session()?.binding.attempt.local
            || source != ObservationSource::Live
            || self.samples.first().is_some_and(|s| at < s.observed_at_ms)
        {
            return Err(TutorialError::InvalidObservation);
        }
        let mut scopes = BTreeSet::from([None]);
        scopes.extend(self.tracked.values().copied());
        scopes.insert(self.session()?.binding.attempt.peer);
        let mut samples = Vec::new();
        let mut absent = Vec::new();
        for peer in scopes {
            match counter_sample(&health, peer, source, at) {
                Ok(normalized) => samples.push(normalized.sample),
                Err(_) => absent.extend(
                    self.tracked
                        .iter()
                        .filter(|(_, p)| **p == peer)
                        .map(|(s, _)| *s),
                ),
            }
        }
        self.samples = samples;
        // Replace the old prefix: this receipt is observed before any invalidation or action.
        effects.clear();
        effects.extend(self.observe_effects());
        for step in absent {
            self.tracked.remove(&step);
            effects.push(self.effect(TutorialEffectKind::Core(FlowEvent::Invalidate { step }))?);
        }
        let current = self
            .samples
            .iter()
            .find(|s| s.binding.peer == self.session().ok().and_then(|s| s.binding.attempt.peer))
            .cloned();
        self.health = Some(health);
        if self.session()?.retired {
            if e2_role(self.session()?.binding.attempt.role)
                && let Some(current) = &current
            {
                self.projection_sample(current, effects)?;
            }
            self.cleanup(effects)?;
            self.settle_cleanup(effects)?;
            return Ok(());
        }
        let Some(current) = current else {
            return self.fail(TutorialFailure::BindingChanged, effects);
        };
        let role = self.session()?.binding.attempt.role;
        if self
            .session()?
            .binding
            .evidence
            .as_ref()
            .is_some_and(|binding| !same_binding(binding, &current.binding, dependencies(role)))
        {
            return self.fail(TutorialFailure::BindingChanged, effects);
        }
        if let Some(start) = &self.session()?.start {
            let (advance, forbidden) = requirements(self.session()?.binding.attempt.role);
            if self.state != TutorialState::Verified
                && let Err(error) = check_activity(
                    self.session()?.binding.attempt.attempt,
                    start,
                    &current,
                    dependencies(self.session()?.binding.attempt.role),
                    &advance,
                    &forbidden,
                )
                && !matches!(
                    error,
                    EvidenceError::CounterDidNotAdvance | EvidenceError::MissingCounter
                )
            {
                return self.fail(TutorialFailure::Evidence(error), effects);
            }
            if !same_binding(
                &start.binding,
                &current.binding,
                dependencies(self.session()?.binding.attempt.role),
            ) {
                return self.fail(TutorialFailure::BindingChanged, effects);
            }
        }
        if self.session()?.binding.evidence.is_none() {
            self.session_mut()?.binding.evidence = Some(current.binding.clone());
            self.session_mut()?.admitted_at = at;
        }
        let active_source = source_role(role)
            && self.session()?.start.as_ref().is_some_and(|start| {
                delta(start, &current, Metric::SourceStarted) == Some(1)
                    && delta(start, &current, Metric::SourceReturned) == Some(0)
            });
        if !healthy(
            self.health
                .as_deref()
                .ok_or(TutorialError::InsufficientContract)?,
            role,
            self.session()?.context.platform,
            active_source,
            self.session()?.binding.attempt.peer,
        ) {
            return self.fail(TutorialFailure::Health, effects);
        }
        if self.state == TutorialState::Verified {
            return Ok(());
        }
        if self.session()?.stage == Stage::Preflight {
            if matches!(
                role,
                TutorialRole::E2DestinationPush | TutorialRole::E2DestinationPull
            ) && !current
                .values
                .contains_key(&CounterId(Metric::FramesPresented as u16))
            {
                self.retire(TutorialState::PendingContract, effects)?;
                effects.push(self.effect(TutorialEffectKind::WaitForContract)?);
                return Ok(());
            }
            if !self.health.as_ref().is_some_and(|h| {
                h.terminal().projections.is_empty()
                    && h.terminal().controlling.is_none()
                    && h.terminal().controlled_by.is_none()
            }) {
                return self.fail(TutorialFailure::Isolation, effects);
            }
            if !audio_role(role) {
                self.session_mut()?.start = Some(current.clone());
            }
            self.session_mut()?.stage = Stage::Practice;
            if matches!(
                role,
                TutorialRole::E1Target
                    | TutorialRole::E2SourcePush
                    | TutorialRole::E2SourcePull
                    | TutorialRole::AudioSender
            ) {
                self.state = TutorialState::WaitingFixture;
                self.fixture_call(
                    TutorialFixtureAction::Open {
                        machine_label: self.session()?.context.machine_label.clone(),
                    },
                    effects,
                )?;
            } else if role == TutorialRole::E2DestinationPull {
                self.call(
                    InstallerRequest::WindowsFrom { peer: self.peer()? },
                    effects,
                )?;
            } else {
                self.state = TutorialState::WaitingUser;
                effects.push(self.effect(TutorialEffectKind::WaitForUser)?);
            }
            return Ok(());
        }
        if audio_role(role) {
            return self.audio_sample(current, effects);
        }
        if e2_role(role) {
            self.projection_sample(&current, effects)?;
        }
        if self.session()?.retired {
            return Ok(());
        }
        if let Some(start) = &self.session()?.start {
            if !isolated(role, start, &current) {
                return self.fail(TutorialFailure::Isolation, effects);
            }
            let (advance, forbidden) = requirements(role);
            if check_activity(
                self.session()?.binding.attempt.attempt,
                start,
                &current,
                dependencies(role),
                &advance,
                &forbidden,
            )
            .is_ok()
                && self.terminal()
                && (!e2_role(role) || self.session()?.projection_observed)
            {
                if self.session()?.end.is_none() {
                    self.session_mut()?.end = Some(current);
                    self.session_mut()?.stage = Stage::Terminal;
                    if source_role(role) {
                        let fixture = self
                            .session()?
                            .fixture
                            .as_ref()
                            .ok_or(TutorialError::InvalidObservation)?
                            .id;
                        self.fixture_call(
                            TutorialFixtureAction::ObserveWindow { fixture },
                            effects,
                        )?;
                    }
                }
                self.state = TutorialState::WaitingUser;
            }
        }
        Ok(())
    }
    fn terminal(&self) -> bool {
        self.health.as_ref().is_some_and(|h| {
            h.terminal().controlling.is_none()
                && h.terminal().controlled_by.is_none()
                && h.terminal().projections.is_empty()
                && h.installer().recovery_pending == 0
        })
    }
    fn projection_sample(
        &mut self,
        current: &CounterSample,
        effects: &mut Vec<TutorialEffect>,
    ) -> Result<(), TutorialError> {
        if self.cleanup_correlated() != Some(true) {
            return Ok(());
        }
        let projections = self
            .health
            .as_ref()
            .ok_or(TutorialError::InsufficientContract)?
            .terminal()
            .projections
            .clone();
        let s = self.session()?;
        let role = s.binding.attempt.role;
        let expected = if source_role(role) {
            s.binding.attempt.local
        } else {
            self.peer()?
        };
        if projections.len() > 1
            || projections.first().is_some_and(|p| p.source != expected)
            || projections
                .first()
                .is_some_and(|p| s.unowned_projection.as_ref() == Some(p))
            || s.projection
                .as_ref()
                .is_some_and(|p| !projections.is_empty() && projections.first() != Some(p))
        {
            return self.fail(TutorialFailure::Isolation, effects);
        }
        if let Some(projection) = projections.first() {
            if s.projection.is_none() {
                let authorized = s.projection_authorized
                    && (s.retired
                        || s.command_acked
                        || !matches!(
                            role,
                            TutorialRole::E2SourcePush | TutorialRole::E2DestinationPull
                        ));
                if !authorized
                    || s.projection_after
                        .is_none_or(|at| current.observed_at_ms <= at)
                {
                    if s.projection_action_after
                        .is_none_or(|at| current.observed_at_ms <= at)
                    {
                        self.session_mut()?.unowned_projection = Some(projection.clone());
                    }
                    return self.fail(TutorialFailure::Isolation, effects);
                }
                let metric = if source_role(role) {
                    Metric::SourceStarted
                } else {
                    Metric::DestStarted
                };
                let started = s
                    .projection_cleanup
                    .as_ref()
                    .map(|c| c.started)
                    .or_else(|| {
                        s.start
                            .as_ref()?
                            .values
                            .get(&CounterId(metric as u16))
                            .copied()
                    });
                if current
                    .values
                    .get(&CounterId(metric as u16))
                    .copied()
                    .and_then(|v| v.checked_sub(started?))
                    != Some(1)
                {
                    return self.fail(TutorialFailure::Isolation, effects);
                }
                self.prepare_projection_cleanup()?;
                self.session_mut()?.projection = Some(projection.clone());
                self.session_mut()?.projection_observed = true;
                self.session_mut()?.projection_unknown = false;
                if !self.session()?.retired {
                    self.session_mut()?.stage = Stage::Projection;
                    self.state = TutorialState::Running;
                }
            }
        } else if self.session()?.projection.is_some() {
            self.session_mut()?.projection = None;
        }
        if source_role(role) {
            let parking = self
                .health
                .as_ref()
                .and_then(|h| {
                    h.installer()
                        .peers
                        .iter()
                        .find(|p| Some(p.node) == current.binding.peer)
                })
                .and_then(|p| p.last_source_parking);
            self.detail.parking = match (self.session()?.context.platform, parking) {
                (_, None) => ParkingResult::Unknown,
                (AgentPlatform::Linux, Some(_)) => ParkingResult::Native,
                (_, Some(SourceParking::Twin)) => ParkingResult::Twin,
                (_, Some(SourceParking::Mirror)) => ParkingResult::MirrorFallback,
            };
        }
        Ok(())
    }
    fn user(
        &mut self,
        action: TutorialUserAction,
        effects: &mut Vec<TutorialEffect>,
    ) -> Result<(), TutorialError> {
        match action {
            TutorialUserAction::Confirm(confirmation) => {
                let s = self.session()?;
                let extra = (confirmation == HumanConfirmation::SourceMachineAndAttempt
                    && e2_role(s.binding.attempt.role))
                    || (confirmation == HumanConfirmation::PrivateDisplayObserved
                        && source_role(s.binding.attempt.role)
                        && s.context.source_policy == TutorialSourcePolicy::MacPrivateDisplay);
                if !confirmations(s.binding.attempt.role).contains(&confirmation) && !extra {
                    return Err(TutorialError::WrongRole);
                }
                self.session_mut()?.human.insert(confirmation);
                if confirmation == HumanConfirmation::SelectedSourceToneStarted {
                    let now = self.now;
                    self.session_mut()?.audio_after.get_or_insert(now);
                }
                if confirmation == HumanConfirmation::SourceMachineAndAttempt {
                    self.session_mut()?.projection_authorized = true;
                    let now = self.now;
                    self.session_mut()?.projection_after.get_or_insert(now);
                    self.prepare_projection_cleanup()?;
                    if matches!(
                        self.session()?.binding.attempt.role,
                        TutorialRole::E2SourcePull | TutorialRole::E2DestinationPush
                    ) {
                        self.session_mut()?.projection_unknown = true;
                        self.session_mut()?
                            .projection_action_after
                            .get_or_insert(now);
                    }
                }
                if confirmation == HumanConfirmation::DestinationPatternInteractionAndClose
                    && self.session()?.projection.is_some()
                {
                    self.return_owned(effects)?;
                }
                if confirmation == HumanConfirmation::SourceMachineAndAttempt {
                    self.try_pull(effects)?;
                }
            }
            TutorialUserAction::PlayTestSound => {
                let s = self.session()?;
                if s.binding.attempt.role != TutorialRole::AudioSender
                    || s.tone.is_some()
                    || s.stop_sent
                    || self
                        .fixture_calls
                        .values()
                        .any(|a| matches!(a, TutorialFixtureAction::PlayTone { .. }))
                {
                    return Err(TutorialError::WrongRole);
                }
                let fixture = s
                    .fixture
                    .as_ref()
                    .ok_or(TutorialError::InvalidObservation)?
                    .id;
                let speakers = s
                    .context
                    .speakers
                    .as_ref()
                    .ok_or(TutorialError::WrongPeer)?;
                self.fixture_call(
                    TutorialFixtureAction::PlayTone {
                        fixture,
                        peer: speakers.peer,
                        device_key: speakers.device_key.clone(),
                    },
                    effects,
                )?;
            }
            TutorialUserAction::SelectRemoteWindow { window } => {
                if self.session()?.binding.attempt.role != TutorialRole::E2DestinationPull
                    || !self.session()?.remote_windows.contains(&window)
                {
                    return Err(TutorialError::WrongRole);
                }
                self.session_mut()?.selected = Some(window);
                self.try_pull(effects)?;
            }
            TutorialUserAction::Cancel => return Err(TutorialError::RetiredView),
        }
        Ok(())
    }
    fn try_pull(&mut self, effects: &mut Vec<TutorialEffect>) -> Result<(), TutorialError> {
        let s = self.session()?;
        if s.binding.attempt.role == TutorialRole::E2DestinationPull
            && s.stage == Stage::Practice
            && s.human
                .contains(&HumanConfirmation::SourceMachineAndAttempt)
            && let Some(window) = s.selected
        {
            self.session_mut()?.stage = Stage::Command;
            self.call(
                InstallerRequest::Pull {
                    peer: self.peer()?,
                    window,
                },
                effects,
            )?;
        }
        Ok(())
    }
}
