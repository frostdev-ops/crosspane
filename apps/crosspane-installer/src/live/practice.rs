//! The nine practice roles: core job hand-off to the frozen sequencer, ordered effect reduction,
//! fixture launch and translation, and reply/receipt routing.

use std::collections::{BTreeSet, VecDeque};

use crosspane_installer_core::{
    AttemptId, FlowEvent, JobIntent, JobStage, ObservationSource, StepId, StepState,
};

use super::FixtureReadiness;
use super::controller::{LiveController, bounded};
use super::graph::steps;
use crate::agent_contract::{AgentCall, AgentReply, DecodedReply, RemoteWindow};
use crate::fixture::{
    FixtureCall, FixtureCommand, FixtureError, FixtureEvent, FixtureId, OwnToneState,
    OwnWindowFacts, PhaseId, SpeakersSelection, ToneId,
};
use crate::tutorial_flow::{
    HumanConfirmation, Tutorial, TutorialAttempt, TutorialContext, TutorialEffect,
    TutorialEffectKind, TutorialEvent, TutorialFixtureAction, TutorialFixtureError,
    TutorialFixtureObservation, TutorialRole, TutorialSourcePolicy, TutorialSpeakers,
    TutorialState, TutorialToneState, TutorialUserAction, TutorialWindowFacts,
};
use crate::view::HidingChoice;

const TICK_MS: u64 = 500;
const LAUNCH_MS: u64 = 15_000;
const CLEANUP_MS: u64 = 10_000;
const DEFER_MS: u64 = 5_000;
const MAX_REMOTE_WINDOWS: usize = 12;

pub const CONFIRMATIONS: [HumanConfirmation; 11] = [
    HumanConfirmation::RemotePracticeAndHud,
    HumanConfirmation::ControllerCrossingAndRelease,
    HumanConfirmation::DestinationPatternInteractionAndClose,
    HumanConfirmation::SourceRestored,
    HumanConfirmation::FarSpeakerHeard,
    HumanConfirmation::LocalSpeakerHeard,
    HumanConfirmation::TrayAndSettingsVisible,
    HumanConfirmation::ExclusiveAudioInterval,
    HumanConfirmation::SourceMachineAndAttempt,
    HumanConfirmation::PrivateDisplayObserved,
    HumanConfirmation::SelectedSourceToneStarted,
];

pub fn confirmation_index(c: HumanConfirmation) -> usize {
    CONFIRMATIONS.iter().position(|x| *x == c).unwrap_or(0)
}

/// The confirmations offered for a role. The sequencer alone decides which ones count.
pub fn confirmations(role: TutorialRole, private: bool) -> Vec<HumanConfirmation> {
    use HumanConfirmation::*;
    let mut list = match role {
        TutorialRole::E1Controller => vec![RemotePracticeAndHud],
        TutorialRole::E1Target => vec![ControllerCrossingAndRelease],
        TutorialRole::E2SourcePush => vec![DestinationPatternInteractionAndClose],
        // The person's match of the source machine and fixture label authorizes these two.
        TutorialRole::E2SourcePull => {
            vec![
                SourceMachineAndAttempt,
                DestinationPatternInteractionAndClose,
            ]
        }
        TutorialRole::E2DestinationPush | TutorialRole::E2DestinationPull => vec![
            SourceMachineAndAttempt,
            DestinationPatternInteractionAndClose,
            SourceRestored,
        ],
        TutorialRole::AudioSender => vec![FarSpeakerHeard, ExclusiveAudioInterval],
        TutorialRole::AudioReceiver => vec![
            SelectedSourceToneStarted,
            LocalSpeakerHeard,
            ExclusiveAudioInterval,
        ],
        TutorialRole::Menu => vec![TrayAndSettingsVisible],
    };
    if private
        && matches!(
            role,
            TutorialRole::E2SourcePush | TutorialRole::E2SourcePull
        )
    {
        list.push(PrivateDisplayObserved);
    }
    list
}

pub fn confirmation_label(c: HumanConfirmation) -> &'static str {
    match c {
        HumanConfirmation::RemotePracticeAndHud => {
            "I moved onto the other computer and saw the Crosspane indicator"
        }
        HumanConfirmation::ControllerCrossingAndRelease => {
            "The other computer moved this pointer, clicked the target, and gave control back"
        }
        HumanConfirmation::DestinationPatternInteractionAndClose => {
            "The practice window showed its moving pattern, responded to a click, and closed"
        }
        HumanConfirmation::SourceRestored => "The practice window is back where it started",
        HumanConfirmation::FarSpeakerHeard => "I heard the test sound on the other computer",
        HumanConfirmation::LocalSpeakerHeard => "I heard the test sound on this computer",
        HumanConfirmation::TrayAndSettingsVisible => {
            "I found the Crosspane menu and opened its settings"
        }
        HumanConfirmation::ExclusiveAudioInterval => {
            "Nothing else was playing sound during the test"
        }
        HumanConfirmation::SourceMachineAndAttempt => {
            "The window came from the other computer and shows this attempt's name"
        }
        HumanConfirmation::PrivateDisplayObserved => {
            "The window left this computer's screens while it was sent"
        }
        HumanConfirmation::SelectedSourceToneStarted => {
            "I started the test sound on the other computer"
        }
    }
}

pub(super) struct Run {
    pub role: TutorialRole,
    pub step: StepId,
    pub attempt: AttemptId,
    pub view_revision: u64,
    pub held_open: Option<(u64, FixtureCommand)>,
    pub launch_at: u64,
    pub confirm_closed: BTreeSet<u64>,
    pub remote_windows: Vec<RemoteWindow>,
    pub confirmed: BTreeSet<HumanConfirmation>,
    pub last_tick: u64,
    pub ended_at: Option<u64>,
    pub fixture_done: bool,
    pub fixture_retired: bool,
    pub note: Option<String>,
}

#[derive(Default)]
pub(super) struct PracticeState {
    pub tutorial: Tutorial,
    pub next_attempt: u64,
    pub run: Option<Run>,
    pub requested: Option<StepId>,
    /// A practice whose start waits for the previous attempt's last status reply.
    pub deferred: Option<(JobIntent, TutorialRole, u64)>,
}

impl PracticeState {
    pub fn active(&self) -> bool {
        self.run.as_ref().is_some_and(|r| r.ended_at.is_none()) && !terminal(self.tutorial.state())
    }

    /// The sequencer still owns the agent call-id namespace and polls for itself: from `begin`
    /// until its attempt is terminal and its cleanup has settled (or the cleanup window ends).
    /// The controller issues no agent call of its own while this holds, so ids never collide.
    pub fn engaged(&self, now: u64) -> bool {
        if self.deferred.is_some() {
            return true;
        }
        let Some(run) = &self.run else {
            return false;
        };
        match run.ended_at {
            None => true,
            Some(ended) => {
                !(self.tutorial.detail().cleanup_settled || now.saturating_sub(ended) > CLEANUP_MS)
            }
        }
    }
}

fn terminal(state: TutorialState) -> bool {
    matches!(
        state,
        TutorialState::Verified
            | TutorialState::Failed
            | TutorialState::Cancelled
            | TutorialState::PendingContract
    )
}

type Batches = VecDeque<(Vec<TutorialEffect>, bool)>;

impl LiveController {
    pub(super) fn source_policy(&self) -> TutorialSourcePolicy {
        if self.desc.hiding_choice
            && self.satisfied(steps::HIDING)
            && self.connect.hiding == Some(HidingChoice::Hide)
        {
            TutorialSourcePolicy::MacPrivateDisplay
        } else {
            self.desc.source_policy
        }
    }

    pub(super) fn start_practice(&mut self, role: TutorialRole) {
        if self.practice.engaged(self.now) {
            self.notice =
                Some("The previous practice is still cleaning up. Try again in a moment.".into());
            return;
        }
        let step = steps::practice(role);
        self.practice.requested = Some(step);
        self.details.remove(&step);
        match self.step_state(step) {
            StepState::Checking => {
                if let Some(job) = self.job(step, JobStage::Detect) {
                    self.intents.push_back(job);
                }
            }
            StepState::Planning
            | StepState::NeedsAction
            | StepState::Running
            | StepState::Verifying => {
                let _ = self.reduce(FlowEvent::Cancel { step });
                self.begin(step);
            }
            _ => self.begin(step),
        }
        self.dispatch_intents();
    }

    pub(super) fn dispatch_practice(&mut self, job: JobIntent, role: TutorialRole) {
        if self.practice.requested != Some(job.step) {
            return;
        }
        match job.stage {
            // The sequencer's own preflight is the detection; nothing here mutates.
            JobStage::Detect => {
                let _ = self.reduce(FlowEvent::Detected {
                    step: job.step,
                    operation: job.operation,
                    needs_action: false,
                });
            }
            JobStage::Verify => {
                self.practice.requested = None;
                self.begin_attempt(job, role);
            }
            JobStage::Plan | JobStage::Apply => {}
        }
    }

    fn begin_attempt(&mut self, job: JobIntent, role: TutorialRole) {
        let step = job.step;
        let wait = |c: &mut Self, kind, text: &str| {
            c.details.insert(step, text.into());
            let _ = c.reduce(FlowEvent::Waiting {
                step,
                operation: job.operation,
                kind,
            });
        };
        let Some(local) = self.health.as_ref().map(|h| h.snapshot.installer().node) else {
            wait(
                self,
                crosspane_installer_core::WaitKind::Contract,
                "Waiting for this computer's Crosspane agent to report.",
            );
            return;
        };
        let peer = if role == TutorialRole::Menu {
            None
        } else if let Some(peer) = self.peer {
            Some(peer)
        } else {
            wait(
                self,
                crosspane_installer_core::WaitKind::Peer,
                "Pair with the other computer first.",
            );
            return;
        };
        // The platform names the output per selected peer, e.g. `crosspane.{peer}.speaker`.
        let speakers = match (peer, &self.desc.speakers_device) {
            (Some(peer), Some(template)) => Some(TutorialSpeakers {
                peer,
                device_key: template.replace("{peer}", &peer.to_string()),
            }),
            _ => None,
        };
        let context = TutorialContext {
            machine_label: self.desc.machine_label.clone(),
            platform: self.desc.platform,
            source_policy: self.source_policy(),
            speakers,
        };
        if self
            .practice
            .tutorial
            .advance_call_ids(self.next_call.saturating_sub(1))
            .is_err()
        {
            // The last attempt's final status call is still in flight. Its reply normally arrives
            // within a moment, so wait for it a few seconds instead of dropping the click.
            let since = self
                .practice
                .deferred
                .as_ref()
                .map_or(self.now, |(_, _, since)| *since);
            if self.now.saturating_sub(since) > DEFER_MS {
                self.practice.deferred = None;
                self.details.insert(
                    step,
                    "The previous practice is still cleaning up. Try again in a moment.".into(),
                );
                let _ = self.reduce(FlowEvent::Cancel { step });
            } else {
                self.details
                    .insert(step, "Finishing the previous practice…".into());
                self.practice.deferred = Some((job, role, since));
            }
            return;
        }
        self.practice.deferred = None;
        self.practice.next_attempt += 1;
        let attempt = AttemptId(self.practice.next_attempt);
        let view_revision = self.view.revision;
        let begun = self.practice.tutorial.begin(
            TutorialAttempt {
                step,
                operation: job.operation,
                attempt,
                local,
                peer,
                role,
            },
            context,
            job.clone(),
            view_revision,
            self.now,
        );
        match begun {
            Ok(effects) => {
                self.practice.run = Some(Run {
                    role,
                    step,
                    attempt,
                    view_revision,
                    held_open: None,
                    launch_at: self.now,
                    confirm_closed: BTreeSet::new(),
                    remote_windows: Vec::new(),
                    confirmed: BTreeSet::new(),
                    last_tick: self.now,
                    ended_at: None,
                    fixture_done: false,
                    fixture_retired: false,
                    note: None,
                });
                self.apply_effects(effects);
            }
            Err(error) => {
                self.details
                    .insert(step, bounded(format!("Practice couldn't start ({error}).")));
                let _ = self.reduce(FlowEvent::Failed {
                    step,
                    operation: job.operation,
                });
            }
        }
    }

    /// Reduce effects in order. A core error drops the rest of its batch and is fed back; errors
    /// inside that recovery batch are not fed back again.
    pub(super) fn apply_effects(&mut self, effects: Vec<TutorialEffect>) {
        let mut batches: Batches = VecDeque::from([(effects, false)]);
        let mut budget = 64;
        while let Some((batch, recovery)) = batches.pop_front() {
            budget -= 1;
            if budget == 0 {
                break;
            }
            for effect in batch {
                match effect.kind {
                    TutorialEffectKind::Core(event) => {
                        if let Err(error) = self.reduce(event) {
                            if recovery {
                                continue;
                            }
                            if let Ok(more) = self
                                .practice
                                .tutorial
                                .reduce(TutorialEvent::CoreRejected(error), self.now)
                            {
                                batches.push_back((more, true));
                            }
                            break;
                        }
                    }
                    TutorialEffectKind::Agent(call) => {
                        let more = self.tutorial_call(call);
                        batches.push_back((more, false));
                    }
                    TutorialEffectKind::Fixture { call_id, action } => {
                        let more = self.fixture_effect(call_id, action);
                        batches.push_back((more, false));
                    }
                    TutorialEffectKind::WaitForUser => self.note(None),
                    TutorialEffectKind::WaitForPeer => {
                        self.note(Some("Waiting for the other computer."))
                    }
                    TutorialEffectKind::WaitForContract => self.note(Some(
                        "Waiting for the agent to report what this practice needs.",
                    )),
                    TutorialEffectKind::DetectAfterUnknown => self.note(Some(
                        "The last action's result is unknown. Start this practice again to re-check.",
                    )),
                }
            }
        }
        self.dispatch_intents();
        self.settle_run();
    }

    fn note(&mut self, text: Option<&str>) {
        if let Some(run) = self.practice.run.as_mut() {
            run.note = text.map(str::to_owned);
        }
    }

    fn tutorial_call(&mut self, call: AgentCall) -> Vec<TutorialEffect> {
        let id = call.id;
        self.next_call = self.next_call.max(id.saturating_add(1));
        match self.platform.agent().submit(call) {
            Ok(()) => {
                let _ = self.practice.tutorial.submitted(id);
                Vec::new()
            }
            // A local refusal can only fail the attempt.
            Err(failure) => self
                .practice
                .tutorial
                .reduce(
                    TutorialEvent::Reply(AgentReply {
                        id,
                        observed_at_ms: self.now,
                        source: ObservationSource::Live,
                        result: Err(failure),
                    }),
                    self.now,
                )
                .unwrap_or_default(),
        }
    }

    fn fixture_effect(
        &mut self,
        call_id: u64,
        action: TutorialFixtureAction,
    ) -> Vec<TutorialEffect> {
        let Some(attempt) = self.practice.run.as_ref().map(|r| r.attempt) else {
            return Vec::new();
        };
        let command = command(action);
        if matches!(command, FixtureCommand::Open { .. }) {
            return match self.platform.fixtures().launch(attempt) {
                Ok(()) => {
                    if let Some(run) = self.practice.run.as_mut() {
                        run.held_open = Some((call_id, command));
                        run.launch_at = self.now;
                    }
                    Vec::new()
                }
                Err(error) => self.fixture_failed(attempt, call_id, error),
            };
        }
        self.submit_fixture(attempt, call_id, command)
    }

    fn submit_fixture(
        &mut self,
        attempt: AttemptId,
        call_id: u64,
        command: FixtureCommand,
    ) -> Vec<TutorialEffect> {
        let closing = match &command {
            FixtureCommand::Close { fixture } => Some(*fixture),
            _ => None,
        };
        match self.platform.fixtures().submit(FixtureCall {
            id: call_id,
            attempt,
            command,
        }) {
            Ok(()) => {
                let _ = self.practice.tutorial.submitted(call_id);
                if let Some(fixture) = closing {
                    self.confirm_closed(attempt, fixture);
                }
                Vec::new()
            }
            Err(error) => self.fixture_failed(attempt, call_id, error),
        }
    }

    fn fixture_failed(
        &mut self,
        attempt: AttemptId,
        call_id: u64,
        error: FixtureError,
    ) -> Vec<TutorialEffect> {
        self.practice
            .tutorial
            .reduce(
                TutorialEvent::FixtureSubmitFailed {
                    attempt,
                    call_id,
                    error: fixture_error(error),
                },
                self.now,
            )
            .unwrap_or_default()
    }

    /// Queue native cleanup confirmation once per fixture, after Close or CloseRequested.
    fn confirm_closed(&mut self, attempt: AttemptId, fixture: FixtureId) {
        let Some(run) = self.practice.run.as_mut() else {
            return;
        };
        if run.attempt != attempt || !run.confirm_closed.insert(fixture.0) {
            return;
        }
        let _ = self.platform.fixtures().complete_closed(attempt, fixture);
    }

    pub(super) fn drain_fixtures(&mut self) {
        let receipts = self.platform.fixtures().poll();
        if receipts.is_empty() {
            return;
        }
        self.stamp();
        for receipt in receipts {
            self.now = self.now.max(receipt.received_at_ms);
            let Some(attempt) = self.practice.run.as_ref().map(|r| r.attempt) else {
                continue;
            };
            let message = receipt.message;
            if message.attempt != attempt {
                continue;
            }
            match &message.result {
                Ok(FixtureEvent::CloseRequested { fixture }) => {
                    self.confirm_closed(attempt, *fixture);
                }
                Ok(FixtureEvent::Closed { .. } | FixtureEvent::Lost { .. }) => {
                    if let Some(run) = self.practice.run.as_mut() {
                        run.fixture_done = true;
                    }
                }
                _ => {}
            }
            let event = TutorialEvent::Fixture {
                attempt,
                call_id: message.call_id,
                sequence: message.sequence,
                observed_at_ms: receipt.received_at_ms,
                result: observation(message.result),
            };
            if let Ok(effects) = self.practice.tutorial.reduce(event, self.now) {
                self.apply_effects(effects);
            }
        }
    }

    pub(super) fn practice_reply(&mut self, reply: AgentReply) {
        if self.practice.run.is_none() {
            return;
        }
        if matches!(reply.result, Ok(DecodedReply::Status(_))) {
            self.on_status(&reply, false);
        }
        if let (Ok(DecodedReply::WindowsFrom(list)), Some(run)) =
            (&reply.result, self.practice.run.as_mut())
        {
            run.remote_windows = list.iter().take(MAX_REMOTE_WINDOWS).cloned().collect();
        }
        if let Ok(effects) = self
            .practice
            .tutorial
            .reduce(TutorialEvent::Reply(reply), self.now)
        {
            self.apply_effects(effects);
        }
    }

    pub(super) fn practice_tick(&mut self) {
        if let Some((job, role, _)) = self.practice.deferred.clone() {
            if self.jobs.get(&job.step) == Some(&job) {
                self.begin_attempt(job, role);
            } else {
                self.practice.deferred = None;
            }
        }
        let Some(run) = self.practice.run.as_ref() else {
            return;
        };
        let attempt = run.attempt;
        if let Some((call_id, command)) = run.held_open.clone() {
            let launch_at = run.launch_at;
            let more = match self.platform.fixtures().readiness() {
                FixtureReadiness::Ready => {
                    self.clear_held();
                    self.submit_fixture(attempt, call_id, command)
                }
                FixtureReadiness::Failed(error) => {
                    self.clear_held();
                    self.fixture_failed(attempt, call_id, error)
                }
                FixtureReadiness::Launching | FixtureReadiness::Idle => {
                    if self.now.saturating_sub(launch_at) > LAUNCH_MS {
                        self.clear_held();
                        self.platform.fixtures().retire();
                        self.fixture_failed(attempt, call_id, FixtureError::TimedOut)
                    } else {
                        Vec::new()
                    }
                }
            };
            self.apply_effects(more);
        }
        let Some(run) = self.practice.run.as_ref() else {
            return;
        };
        if !self.practice.engaged(self.now) {
            return;
        }
        if self.now.saturating_sub(run.last_tick) >= TICK_MS {
            if let Some(run) = self.practice.run.as_mut() {
                run.last_tick = self.now;
            }
            if let Ok(effects) = self.practice.tutorial.reduce(TutorialEvent::Tick, self.now) {
                self.apply_effects(effects);
            }
        }
    }

    fn clear_held(&mut self) {
        if let Some(run) = self.practice.run.as_mut() {
            run.held_open = None;
        }
    }

    /// Record the end of an attempt and retire its fixture once cleanup settles or times out.
    fn settle_run(&mut self) {
        let state = self.practice.tutorial.state();
        let now = self.now;
        let Some(run) = self.practice.run.as_mut() else {
            return;
        };
        if !terminal(state) {
            return;
        }
        let step = run.step;
        if run.ended_at.is_none() {
            run.ended_at = Some(now);
            let failure = self.practice.tutorial.detail().failure.clone();
            let text = match state {
                TutorialState::Verified => "Done.".to_owned(),
                TutorialState::Cancelled => "Cancelled.".to_owned(),
                TutorialState::PendingContract => {
                    "This agent doesn't report what this practice needs yet.".to_owned()
                }
                _ => match failure {
                    Some(f) => format!("This practice didn't pass ({f:?})."),
                    None => "This practice didn't pass.".to_owned(),
                },
            };
            self.details.insert(step, bounded(text));
        }
        let Some(run) = self.practice.run.as_mut() else {
            return;
        };
        let expired = run
            .ended_at
            .is_some_and(|ended| now.saturating_sub(ended) > CLEANUP_MS);
        if (run.fixture_done || expired) && !run.fixture_retired {
            run.fixture_retired = true;
            self.platform.fixtures().retire();
        }
    }

    pub(super) fn practice_user(&mut self, action: TutorialUserAction) {
        let Some(run) = self.practice.run.as_ref() else {
            return;
        };
        let event = TutorialEvent::User {
            attempt: run.attempt,
            view_revision: run.view_revision,
            action: action.clone(),
        };
        match self.practice.tutorial.reduce(event, self.now) {
            Ok(effects) => {
                if let (TutorialUserAction::Confirm(c), Some(run)) =
                    (&action, self.practice.run.as_mut())
                {
                    run.confirmed.insert(*c);
                }
                self.apply_effects(effects);
            }
            Err(_) => {
                self.notice = Some("That isn't expected at this point of the practice.".into());
            }
        }
    }

    pub(super) fn cancel_practice(&mut self) {
        if self.practice.active() {
            self.practice_user(TutorialUserAction::Cancel);
        }
    }
}

fn command(action: TutorialFixtureAction) -> FixtureCommand {
    match action {
        TutorialFixtureAction::Open { machine_label } => FixtureCommand::Open { machine_label },
        TutorialFixtureAction::ArmTarget { fixture, phase } => FixtureCommand::ArmTarget {
            fixture: FixtureId(fixture),
            phase: PhaseId(phase),
        },
        TutorialFixtureAction::ObserveWindow { fixture } => FixtureCommand::ObserveWindow {
            fixture: FixtureId(fixture),
        },
        TutorialFixtureAction::PlayTone {
            fixture,
            peer,
            device_key,
        } => FixtureCommand::PlayTone {
            fixture: FixtureId(fixture),
            output: SpeakersSelection { peer, device_key },
        },
        TutorialFixtureAction::StopTone { fixture, tone } => FixtureCommand::StopTone {
            fixture: FixtureId(fixture),
            tone: ToneId(tone),
        },
        TutorialFixtureAction::Close { fixture } => FixtureCommand::Close {
            fixture: FixtureId(fixture),
        },
    }
}

fn fixture_error(error: FixtureError) -> TutorialFixtureError {
    match error {
        FixtureError::BadCall => TutorialFixtureError::BadCall,
        FixtureError::Busy => TutorialFixtureError::Busy,
        FixtureError::NotOwned => TutorialFixtureError::NotOwned,
        FixtureError::Unavailable => TutorialFixtureError::Unavailable,
        FixtureError::UnknownWindow => TutorialFixtureError::UnknownWindow,
        FixtureError::AmbiguousWindow => TutorialFixtureError::AmbiguousWindow,
        FixtureError::OutputUnavailable => TutorialFixtureError::OutputUnavailable,
        FixtureError::OutputChanged => TutorialFixtureError::OutputChanged,
        FixtureError::UnsupportedFormat => TutorialFixtureError::UnsupportedFormat,
        FixtureError::TimedOut => TutorialFixtureError::TimedOut,
        FixtureError::ChildExited => TutorialFixtureError::ChildExited,
        FixtureError::CounterExhausted => TutorialFixtureError::CounterExhausted,
        FixtureError::ChannelClosed => TutorialFixtureError::ChannelClosed,
        FixtureError::InvalidMessage => TutorialFixtureError::InvalidMessage,
        FixtureError::Refused => TutorialFixtureError::Refused,
        FixtureError::CleanupFailed => TutorialFixtureError::CleanupFailed,
    }
}

fn window_facts(facts: OwnWindowFacts) -> TutorialWindowFacts {
    match facts {
        OwnWindowFacts::Unknown => TutorialWindowFacts::Unknown,
        OwnWindowFacts::Present {
            visible_on_user_workspace,
            on_initial_display,
        } => TutorialWindowFacts::Present {
            visible_on_user_workspace,
            on_initial_display,
        },
        OwnWindowFacts::Missing => TutorialWindowFacts::Missing,
    }
}

fn tone_state(tone: OwnToneState) -> TutorialToneState {
    match tone {
        OwnToneState::Stopped => TutorialToneState::Stopped,
        OwnToneState::Running { tone } => TutorialToneState::Running { tone: tone.0 },
        OwnToneState::StopUnconfirmed { tone } => {
            TutorialToneState::StopUnconfirmed { tone: tone.0 }
        }
    }
}

fn observation(
    result: Result<FixtureEvent, FixtureError>,
) -> Result<TutorialFixtureObservation, TutorialFixtureError> {
    let event = result.map_err(fixture_error)?;
    Ok(match event {
        FixtureEvent::Opened {
            fixture,
            pid,
            window,
            label,
        } => TutorialFixtureObservation::Opened {
            fixture: fixture.0,
            pid,
            window,
            label,
        },
        FixtureEvent::TargetArmed { fixture, phase } => TutorialFixtureObservation::TargetArmed {
            fixture: fixture.0,
            phase: phase.0,
        },
        FixtureEvent::Snapshot { snapshot } => TutorialFixtureObservation::Snapshot {
            fixture: snapshot.fixture.0,
            window: snapshot.window,
            phase: snapshot.phase.map(|p| p.0),
            pattern_ticks: snapshot.pattern_ticks,
            target_clicks: snapshot.target_clicks,
            window_facts: window_facts(snapshot.window_facts),
            tone: tone_state(snapshot.tone),
        },
        FixtureEvent::ToneStarted { fixture, tone } => TutorialFixtureObservation::ToneStarted {
            fixture: fixture.0,
            tone: tone.0,
        },
        FixtureEvent::ToneStopped { fixture, tone } => TutorialFixtureObservation::ToneStopped {
            fixture: fixture.0,
            tone: tone.0,
        },
        FixtureEvent::CloseRequested { fixture } => {
            TutorialFixtureObservation::CloseRequested { fixture: fixture.0 }
        }
        FixtureEvent::Closed { fixture } => {
            TutorialFixtureObservation::Closed { fixture: fixture.0 }
        }
        FixtureEvent::Lost { fixture, reason } => TutorialFixtureObservation::Lost {
            fixture: fixture.0,
            reason: fixture_error(reason),
        },
    })
}
