//! View building and action handling. The revision changes only when meaning changes: screen,
//! buttons, fields, consent previews, the SAS or layout geometry and busy state.

use crosspane_installer_core::{JobStage, Milestone, StepId, StepState};

use super::controller::{LiveController, bounded};
use super::graph::{self, StepKind, steps};
use super::practice::{CONFIRMATIONS, confirmation_label, confirmations};
use super::shared::{CAPABILITIES, PairMode, capability_label};
use super::{
    Availability, CheckState, Consent, MaintenanceId, MaintenanceOutcome, MaintenanceReport,
    MaintenanceRequest, NativeJob, RemovalChoice, RepairOutcome, StatusEvidence, ids,
};
use crate::agent_contract::{InstallerRequest, PairPhase};
use crate::tutorial_flow::{TutorialRole, TutorialState, TutorialUserAction};
use crate::view::{
    ButtonKind, ButtonRole, ButtonView, EscapeMapping, FieldView, IllustrationView, LayoutPreview,
    PracticeIllustration, ProgressGroup, ProgressView, RowState, RowView, ScreenId, SummaryView,
    ToggleRole, WizardView, check_row_id,
};

const MAX_PROGRESS_LINES: usize = 24;

/// How long a repair click waits for a Status issued after it before it goes ahead without one:
/// the platform then plans, or refuses, with what it can read itself.
const REPAIR_STATUS_WAIT_MS: u64 = 12_000;
/// A repair that has started the new agent is asked again with each fresh Status. The platform
/// ends the wait itself well before this; this is only the backstop for a silent platform.
const REPAIR_WAIT_BACKSTOP_MS: u64 = 150_000;
/// A Status older than this isn't offered as evidence.
const REPAIR_STATUS_FRESH_MS: u64 = 3_000;
/// While a repair waits and no fresh Status comes (the old agent is stopped, or the new one
/// isn't answering yet), the platform is still asked this often, without one. Its own bounded
/// wait then ends the repair with its typed result instead of the backstop's.
const REPAIR_BLIND_LOOK_MS: u64 = 2_000;

/// The R9.4 attribution limit, shown with both audio rows.
const AUDIO_LIMIT: &str = "Audio counters are computer-wide, not per computer, and are only \
sampled: other sound between checks might not be noticed. Crosspane relies on you hearing the \
test sound.";

#[derive(Default)]
pub(super) struct MaintenanceState {
    pub next_id: u64,
    pub current: Option<MaintenanceId>,
    pub uninstall: Option<Availability>,
    pub repair: Option<Availability>,
    pub choices: Vec<RemovalChoice>,
    pub preview: Option<String>,
    pub planning: bool,
    pub confirmed: bool,
    pub progress: Vec<String>,
    pub follow_ups: Vec<(u16, String, String)>,
    pub finished: Option<(MaintenanceOutcome, Vec<String>)>,
    pub refused: Option<String>,
    pub return_to: Option<ScreenId>,
    pub repair_state: RepairState,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum RepairPhase {
    #[default]
    Idle,
    /// A review was asked for; the plan is being prepared.
    Planning,
    /// The preview is on screen and waits for the person's consent.
    Previewed,
    /// The platform is changing files and the service, or waits for a stage that will (the old
    /// agent's clean exit). Closing the window would cut it short.
    Running,
    /// Nothing is changing: the new agent was started and its health is being watched, and the
    /// platform said an interrupted repair is offered for resume next time. The window may close.
    Waiting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AwaitKind {
    Plan,
    Discard,
    Confirm { plan: u64, revision: u64 },
    Resume,
}

/// A repair click that waits for a Status issued after it, so the platform works from what is
/// running now and not from an older reading.
#[derive(Clone, Copy, Debug)]
struct AwaitStatus {
    kind: AwaitKind,
    /// The first call id that may serve as evidence.
    from_call: u64,
    started_at: u64,
}

#[derive(Default)]
pub(super) struct RepairState {
    pub phase: RepairPhase,
    pub discardable: bool,
    discarding: bool,
    /// The number of the preview on screen.
    pub plan: Option<u64>,
    pub preview: Option<String>,
    /// What an earlier, interrupted repair left, offered for resume.
    pub resumable: Option<Vec<String>>,
    pub finished: Option<RepairEnd>,
    /// What the platform last said while waiting for the new agent.
    pub detail: Option<String>,
    awaiting: Option<AwaitStatus>,
    /// When the platform last said anything about the running repair.
    since: u64,
    from_call: u64,
    last_call: u64,
    /// When the last `VerifyRepair` was sent.
    last_look: u64,
    verify_outstanding: bool,
    /// The platform answered the confirmation (or a look) with `RepairWaiting`: the repair is
    /// under way and is driven with `VerifyRepair` until it ends.
    driving: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct RepairEnd {
    pub outcome: RepairOutcome,
    pub lines: Vec<String>,
    pub resumable: bool,
}

impl RepairState {
    /// A repair is somewhere past the start of its review.
    fn engaged(&self) -> bool {
        self.phase != RepairPhase::Idle || self.finished.is_some()
    }

    fn resume_offered(&self) -> bool {
        self.phase == RepairPhase::Idle
            && (self.resumable.is_some() || self.finished.as_ref().is_some_and(|f| f.resumable))
    }
}

impl MaintenanceState {
    pub fn running(&self) -> bool {
        (self.confirmed && self.finished.is_none() && self.refused.is_none())
            || (self.repair_state.phase == RepairPhase::Running && self.refused.is_none())
    }

    pub fn repair_waiting(&self) -> bool {
        self.repair_state.phase == RepairPhase::Waiting && self.refused.is_none()
    }

    /// A confirmed repair the platform is still working through.
    fn repair_driving(&self) -> bool {
        self.repair_state.driving
            && matches!(
                self.repair_state.phase,
                RepairPhase::Running | RepairPhase::Waiting
            )
            && self.refused.is_none()
    }
}

fn row_state(state: StepState) -> RowState {
    match state {
        StepState::NotChecked | StepState::Stale => RowState::Unchecked,
        StepState::Checking | StepState::Planning | StepState::Running | StepState::Verifying => {
            RowState::Working
        }
        StepState::NeedsAction => RowState::NeedsAction,
        StepState::WaitingForUser | StepState::WaitingForPeer | StepState::PendingContract => {
            RowState::Waiting
        }
        StepState::Satisfied => RowState::Verified,
        StepState::Failed => RowState::Failed,
        StepState::Unsupported => RowState::Unsupported,
    }
}

fn button(id: u16, role: ButtonRole, label: &str, enabled: bool, kind: ButtonKind) -> ButtonView {
    ButtonView {
        id,
        role,
        label: label.into(),
        enabled,
        kind,
    }
}

fn group_of(screen: ScreenId) -> Option<ProgressGroup> {
    Some(match screen {
        ScreenId::Welcome
        | ScreenId::Compatibility
        | ScreenId::InstallPlan
        | ScreenId::Installing => ProgressGroup::Install,
        ScreenId::Permissions
        | ScreenId::AudioComponent
        | ScreenId::Network
        | ScreenId::HidingChoice => ProgressGroup::PermissionsNetwork,
        ScreenId::Connect | ScreenId::MatchNumbers => ProgressGroup::Connect,
        ScreenId::Grants | ScreenId::Layout => ProgressGroup::Arrange,
        ScreenId::Practice => ProgressGroup::Practice,
        ScreenId::Summary => ProgressGroup::Ready,
        ScreenId::RepairRemove => return None,
    })
}

fn retryable(state: StepState) -> bool {
    matches!(
        state,
        StepState::Failed
            | StepState::WaitingForUser
            | StepState::WaitingForPeer
            | StepState::PendingContract
            | StepState::Stale
    )
}

impl LiveController {
    fn pairing_phase(&self) -> Option<PairPhase> {
        self.job(steps::PAIR, JobStage::Apply)?;
        self.connect.pairing.as_ref().map(|p| p.phase)
    }

    fn display_screen(&self) -> ScreenId {
        if self.screen == ScreenId::Connect
            && matches!(
                self.pairing_phase(),
                Some(PairPhase::Confirm | PairPhase::Pick)
            )
        {
            ScreenId::MatchNumbers
        } else {
            self.screen
        }
    }

    pub(super) fn row(&self, step: StepId) -> RowView {
        let state = self.step_state(step);
        let meta = self.graph.meta(step);
        let detail = self
            .details
            .get(&step)
            .cloned()
            .unwrap_or_else(|| match state {
                StepState::NeedsAction => self.previews.get(&step).cloned().unwrap_or_default(),
                StepState::Stale => "Needs checking again.".into(),
                StepState::NotChecked => "Not checked yet.".into(),
                StepState::Satisfied => "Done.".into(),
                _ => String::new(),
            });
        let detail = if matches!(
            graph::role_of(step),
            Some(TutorialRole::AudioSender | TutorialRole::AudioReceiver)
        ) {
            bounded(format!("{detail} {AUDIO_LIMIT}").trim().to_owned())
        } else {
            detail
        };
        let detail = match self.support_timing(step) {
            Some(timing) if detail.is_empty() => timing,
            Some(timing) => bounded(format!("{detail} {timing}")),
            None => detail,
        };
        RowView {
            id: step.0,
            label: meta.map_or_else(String::new, |m| m.label.clone()),
            detail,
            state: row_state(state),
            human_confirmed: graph::role_of(step).is_some() && state == StepState::Satisfied,
        }
    }

    /// The support checklist belongs to `step`, and its pass is running now.
    fn support_pass_running(&self, step: StepId) -> bool {
        self.support_checks
            .as_ref()
            .is_some_and(|(checks, _)| checks.step == step)
            && matches!(
                self.step_state(step),
                StepState::Checking
                    | StepState::Planning
                    | StepState::Running
                    | StepState::Verifying
            )
    }

    /// "Checking now…" while the support pass runs, then when it last finished.
    fn support_timing(&self, step: StepId) -> Option<String> {
        let (checks, seen_at) = self
            .support_checks
            .as_ref()
            .filter(|(c, _)| c.step == step)?;
        if self.support_pass_running(step) {
            return Some("Checking now…".into());
        }
        if checks.pass == 0 {
            return None;
        }
        let age_s = self.now.saturating_sub(*seen_at) / 1_000;
        Some(match age_s {
            0 => "Last checked just now.".into(),
            1..=59 => format!("Last checked {age_s} s ago."),
            60..=3_599 => format!("Last checked {} min ago.", age_s / 60),
            _ => format!("Last checked {} h ago.", age_s / 3_600),
        })
    }

    /// The checklist rows under `step`'s card: each check of the last finished pass, or every
    /// one of them as "checking" while a new pass runs. Empty before the first pass.
    pub(super) fn support_check_rows(&self, step: StepId) -> Vec<RowView> {
        let Some((checks, _)) = self.support_checks.as_ref().filter(|(c, _)| c.step == step) else {
            return Vec::new();
        };
        let running = self.support_pass_running(step);
        checks
            .checks
            .iter()
            .enumerate()
            .map(|(index, check)| {
                let (state, detail) = if running {
                    (RowState::Working, String::new())
                } else {
                    match &check.state {
                        CheckState::Checking => (RowState::Working, String::new()),
                        CheckState::Passed(value) => {
                            (RowState::Verified, value.clone().unwrap_or_default())
                        }
                        CheckState::Failed(reason) => (RowState::Failed, reason.clone()),
                        CheckState::Unconfirmed(issue) => (RowState::Waiting, issue.clone()),
                        CheckState::Note(issue) => (RowState::Note, issue.clone()),
                    }
                };
                RowView {
                    id: check_row_id(index),
                    label: bounded(check.label.clone()),
                    detail: bounded(detail),
                    state,
                    human_confirmed: false,
                }
            })
            .collect()
    }

    fn screen_rows(&self, screen: ScreenId) -> Vec<RowView> {
        let screen = if screen == ScreenId::MatchNumbers {
            ScreenId::Connect
        } else {
            screen
        };
        match screen {
            ScreenId::Summary => self.rows(),
            ScreenId::Welcome | ScreenId::RepairRemove => Vec::new(),
            other => self
                .graph
                .on_screen(other)
                .flat_map(|m| {
                    let mut rows = vec![self.row(m.id)];
                    rows.extend(self.support_check_rows(m.id));
                    rows
                })
                .collect(),
        }
    }

    fn progress(&self, screen: ScreenId) -> ProgressView {
        let mut completed = Vec::new();
        for group in [
            ProgressGroup::Install,
            ProgressGroup::PermissionsNetwork,
            ProgressGroup::Connect,
            ProgressGroup::Arrange,
            ProgressGroup::Practice,
            ProgressGroup::Ready,
        ] {
            let mut members = self
                .graph
                .metas
                .iter()
                .filter(|m| m.group == group)
                .peekable();
            if members.peek().is_some() && members.all(|m| self.satisfied(m.id)) {
                completed.push(group);
            }
        }
        ProgressView {
            current: group_of(screen),
            completed,
        }
    }

    fn title_and_message(&self, screen: ScreenId) -> (String, String) {
        let consent_preview = self
            .graph
            .on_screen(screen)
            .filter(|m| self.step_state(m.id) == StepState::NeedsAction)
            .filter_map(|m| self.previews.get(&m.id).cloned())
            .collect::<Vec<_>>()
            .join("\n\n");
        let (title, message) = match screen {
            ScreenId::Welcome => ("Set up Crosspane", {
                let intro = "Crosspane lets this computer and another one share a keyboard, \
                                 mouse, windows and sound. Setup checks this computer, installs \
                                 Crosspane for your account, pairs the two computers and walks \
                                 you through each feature once.";
                match &self.desc.resume_note {
                    Some(note) => format!("{intro}\n\n{note}"),
                    None => intro.to_owned(),
                }
            }),
            ScreenId::Compatibility => (
                "Checking this computer",
                "Nothing changes on this computer during these checks.".to_owned(),
            ),
            ScreenId::InstallPlan => ("What will be installed", consent_preview.clone()),
            ScreenId::Installing => ("Installing Crosspane", consent_preview.clone()),
            ScreenId::Permissions => ("Permissions", consent_preview.clone()),
            ScreenId::AudioComponent => ("Sound", consent_preview.clone()),
            ScreenId::Network => ("Network access", consent_preview.clone()),
            ScreenId::HidingChoice => ("Windows you send", consent_preview.clone()),
            ScreenId::Connect => ("Pair with the other computer", consent_preview.clone()),
            ScreenId::MatchNumbers => (
                "Check the numbers",
                match self.pairing_phase() {
                    Some(PairPhase::Pick) => {
                        "Choose the number shown on the other computer.".to_owned()
                    }
                    _ => "Make sure the other computer shows the same numbers.".to_owned(),
                },
            ),
            ScreenId::Grants => ("What the other computer may do", consent_preview.clone()),
            ScreenId::Layout => ("Arrange your screens", consent_preview.clone()),
            ScreenId::Practice => ("Try each feature once", self.practice_message()),
            ScreenId::Summary => (
                match self.summary.milestone {
                    Milestone::WorkspaceReady => "Your workspace is ready",
                    Milestone::InstalledWaiting => "Crosspane is installed",
                    Milestone::NotInstalled => "Crosspane isn't installed yet",
                },
                match self.summary.milestone {
                    Milestone::WorkspaceReady => {
                        "Every step was checked just now on this computer.".to_owned()
                    }
                    Milestone::InstalledWaiting => {
                        "Some steps still need to be finished before the workspace is ready."
                            .to_owned()
                    }
                    Milestone::NotInstalled => {
                        "Finish the installation steps to start using Crosspane.".to_owned()
                    }
                },
            ),
            ScreenId::RepairRemove => ("Remove or repair Crosspane", self.maintenance_message()),
        };
        let mut message = message;
        if let Some(notice) = &self.notice {
            message = format!("{notice}\n\n{message}");
        }
        (title.to_owned(), bounded(message.trim().to_owned()))
    }

    fn practice_message(&self) -> String {
        let Some(run) = self
            .practice
            .run
            .as_ref()
            .filter(|_| self.practice.active())
        else {
            return "Each practice uses a small Crosspane practice window and checks that it \
                    really worked. Choose one to start."
                .into();
        };
        let mut text = format!("Practising: {}.", graph::role_label(run.role));
        if let Some(note) = &run.note {
            text.push(' ');
            text.push_str(note);
        }
        if self.practice.tutorial.state() == TutorialState::WaitingUser {
            text.push_str(" Follow the practice window, then confirm what you saw.");
        }
        text
    }

    fn maintenance_message(&self) -> String {
        let m = &self.maintenance;
        if let Some((outcome, lines)) = &m.finished {
            let head = match outcome {
                MaintenanceOutcome::Removed => "Crosspane was removed.",
                MaintenanceOutcome::Partial => "Some parts of Crosspane are still here.",
                MaintenanceOutcome::Refused => "Removal didn't start.",
                MaintenanceOutcome::Failed => "Removal didn't finish.",
            };
            return format!("{head}\n{}", lines.join("\n"));
        }
        if let Some(reason) = &m.refused {
            return reason.clone();
        }
        let repair = &m.repair_state;
        if let Some(end) = &repair.finished {
            let head = match end.outcome {
                RepairOutcome::Retired => "The earlier repair record was discarded.",
                RepairOutcome::Verified => {
                    "Crosspane was repaired. The new instance reported healthy."
                }
                RepairOutcome::HealthVerifiedCleanupIncomplete => {
                    "Repaired: health verified, cleanup incomplete."
                }
                RepairOutcome::OutcomeUnknown if end.resumable => {
                    "Outcome unknown, resume required."
                }
                // No Resume can help here (nothing left to reassess): don't ask for one.
                RepairOutcome::OutcomeUnknown => {
                    "Outcome unknown: what the repair did can't be proved."
                }
                RepairOutcome::RecoveryRetained => {
                    "The repair didn't finish. Backups and recovery files were kept."
                }
                RepairOutcome::CheckedAfterEarlierRepair => {
                    "An earlier repair didn't report back. Crosspane is now verified and healthy."
                }
            };
            let mut text = vec![head.to_owned()];
            text.extend(end.lines.iter().cloned());
            return text.join("\n");
        }
        let mut text = Vec::new();
        let mut unavailable = Vec::new();
        match &m.uninstall {
            None => text.push("Checking what can be removed…".to_owned()),
            Some(Availability::Unavailable(reason) | Availability::NotAvailableYet(reason)) => {
                unavailable.push(reason.clone());
            }
            Some(Availability::Available) => {}
        }
        // The reason repair isn't offered is shown too, unless it is the very text already shown.
        if let Some(Availability::Unavailable(reason) | Availability::NotAvailableYet(reason)) =
            &m.repair
            && !unavailable.contains(reason)
            && repair.phase == RepairPhase::Idle
        {
            unavailable.push(reason.clone());
        }
        text.extend(unavailable);
        if let Some(lines) = &repair.resumable
            && repair.phase == RepairPhase::Idle
        {
            let mut resume = vec!["An earlier repair didn't finish. Resume checks what is really on this computer and carries on without repeating anything uncertain.".to_owned()];
            resume.extend(lines.iter().cloned());
            text.push(resume.join("\n"));
        }
        if let Some(preview) = &m.preview {
            text.push(preview.clone());
        }
        if let Some(preview) = &repair.preview
            && repair.phase == RepairPhase::Previewed
        {
            text.push(preview.clone());
        }
        match repair.phase {
            RepairPhase::Planning => text.push("Preparing the repair preview…".to_owned()),
            RepairPhase::Running => text.push(
                repair
                    .detail
                    .clone()
                    .unwrap_or_else(|| "Repairing Crosspane…".to_owned()),
            ),
            RepairPhase::Waiting => text.push(repair.detail.clone().unwrap_or_else(|| {
                "Waiting for Crosspane to start again and report healthy…".to_owned()
            })),
            RepairPhase::Idle | RepairPhase::Previewed => {}
        }
        for (_, label, preview) in &m.follow_ups {
            text.push(format!("{label}: {preview}"));
        }
        text.extend(m.progress.iter().cloned());
        text.join("\n\n")
    }

    fn buttons_and_fields(&self, screen: ScreenId) -> (Vec<ButtonView>, Vec<FieldView>) {
        let mut buttons = Vec::new();
        let mut fields = Vec::new();
        let busy = self.mutation_in_flight();
        let secondary = ButtonKind::Secondary;
        // Step-level consent and retry buttons for native steps on this screen.
        for meta in self.graph.on_screen(screen) {
            let state = self.step_state(meta.id);
            if meta.kind == StepKind::Native
                && state == StepState::NeedsAction
                && self.previews.contains_key(&meta.id)
                && self.job(meta.id, JobStage::Plan).is_some()
            {
                buttons.push(button(
                    ids::consent(meta.id),
                    ButtonRole::Confirm,
                    &meta.action_label,
                    true,
                    ButtonKind::Primary,
                ));
            }
        }
        // One "Check again" for the whole screen, however many of its steps are waiting.
        let waiting = self.retryable_on(screen);
        if !waiting.is_empty() {
            let id = match waiting.as_slice() {
                [only] => ids::retry(*only),
                _ => ids::RETRY_ALL,
            };
            buttons.push(button(
                id,
                ButtonRole::Retry,
                "Check again",
                true,
                secondary,
            ));
        }
        match screen {
            ScreenId::Welcome => {
                buttons.push(button(
                    ids::REMOVE_OR_REPAIR,
                    ButtonRole::Ordinary,
                    "Remove or repair Crosspane…",
                    true,
                    secondary,
                ));
                buttons.push(button(
                    ids::CLOSE,
                    ButtonRole::Cancel,
                    "Close",
                    true,
                    secondary,
                ));
            }
            ScreenId::Connect => self.connect_controls(&mut buttons, &mut fields),
            ScreenId::MatchNumbers => match self.pairing_phase() {
                Some(PairPhase::Pick) => {
                    let candidates = self
                        .connect
                        .pairing
                        .as_ref()
                        .map(|p| p.candidates.clone())
                        .unwrap_or_default();
                    for (i, number) in candidates.iter().take(8).enumerate() {
                        buttons.push(button(
                            ids::pair_pick(i),
                            ButtonRole::Confirm,
                            &bounded(number.clone()),
                            true,
                            ButtonKind::Primary,
                        ));
                    }
                }
                _ => {
                    buttons.push(button(
                        ids::PAIR_CONFIRM,
                        ButtonRole::Confirm,
                        "The numbers match",
                        true,
                        ButtonKind::Primary,
                    ));
                    buttons.push(button(
                        ids::PAIR_REJECT,
                        ButtonRole::Cancel,
                        "They don't match",
                        true,
                        ButtonKind::Destructive,
                    ));
                }
            },
            ScreenId::Grants => {
                if self.step_state(steps::GRANTS) == StepState::NeedsAction {
                    for (i, c) in CAPABILITIES.iter().enumerate() {
                        fields.push(FieldView::Toggle {
                            id: ids::grant_field(i),
                            role: ToggleRole::Grant,
                            label: capability_label(*c).into(),
                            checked: self.connect.grants[i],
                            enabled: true,
                        });
                    }
                    buttons.push(button(
                        ids::GRANTS_APPLY,
                        ButtonRole::Confirm,
                        "Apply",
                        true,
                        ButtonKind::Primary,
                    ));
                }
            }
            ScreenId::Layout => {
                if self.step_state(steps::LAYOUT) == StepState::NeedsAction
                    && !self.connect.layout_busy
                {
                    buttons.push(button(
                        ids::LAYOUT_ACCEPT,
                        ButtonRole::Confirm,
                        "Use this layout",
                        true,
                        ButtonKind::Primary,
                    ));
                }
            }
            ScreenId::HidingChoice => {
                if self.step_state(steps::HIDING) == StepState::NeedsAction {
                    buttons.push(button(
                        ids::HIDING_APPLY,
                        ButtonRole::Confirm,
                        "Apply and continue",
                        self.connect.hiding.is_some(),
                        ButtonKind::Primary,
                    ));
                }
                if self.hiding_restart_pending() {
                    buttons.push(button(
                        ids::HIDING_RESTART,
                        ButtonRole::Confirm,
                        "Restart Crosspane now",
                        true,
                        ButtonKind::Primary,
                    ));
                }
            }
            ScreenId::Practice => self.practice_controls(&mut buttons),
            ScreenId::Summary => {
                buttons.push(button(
                    ids::FINAL_CHECK,
                    ButtonRole::Retry,
                    "Check again",
                    self.graph
                        .practice_steps()
                        .iter()
                        .all(|s| self.satisfied(*s)),
                    secondary,
                ));
                buttons.push(button(
                    ids::REMOVE_OR_REPAIR,
                    ButtonRole::Ordinary,
                    "Remove or repair Crosspane…",
                    !busy,
                    secondary,
                ));
                buttons.push(button(
                    ids::CLOSE,
                    ButtonRole::Cancel,
                    "Close",
                    true,
                    secondary,
                ));
            }
            ScreenId::RepairRemove => self.maintenance_controls(&mut buttons, &mut fields),
            _ => {}
        }
        if !matches!(
            screen,
            ScreenId::Welcome | ScreenId::Summary | ScreenId::RepairRemove | ScreenId::MatchNumbers
        ) {
            if self.previous_screen().is_some() {
                buttons.push(button(
                    ids::BACK,
                    ButtonRole::Back,
                    "Back",
                    !busy,
                    secondary,
                ));
            }
            if self.next_screen().is_some() {
                let enabled = !self.practice.engaged(self.now)
                    && (screen == ScreenId::Practice || self.screen_complete(screen));
                let label = if screen == ScreenId::Practice {
                    "Continue"
                } else {
                    "Next"
                };
                buttons.push(button(
                    ids::NEXT,
                    ButtonRole::Next,
                    label,
                    enabled,
                    ButtonKind::Primary,
                ));
            }
        }
        if screen == ScreenId::Welcome {
            buttons.push(button(
                ids::NEXT,
                ButtonRole::Next,
                "Get started",
                true,
                ButtonKind::Primary,
            ));
        }
        (buttons, fields)
    }

    fn connect_controls(&self, buttons: &mut Vec<ButtonView>, fields: &mut Vec<FieldView>) {
        if self.step_state(steps::PAIR) != StepState::NeedsAction {
            return;
        }
        let address = self.parsed_address().is_some();
        fields.push(FieldView::PeerAddress {
            id: ids::PEER_ADDRESS,
            value: self.connect.address.clone(),
            enabled: true,
        });
        buttons.push(button(
            ids::PAIR_LISTEN,
            ButtonRole::Ordinary,
            "Let the other computer join",
            true,
            ButtonKind::Primary,
        ));
        buttons.push(button(
            ids::PAIR_JOIN,
            ButtonRole::Ordinary,
            "Join the address above",
            address,
            ButtonKind::Secondary,
        ));
        buttons.push(button(
            ids::PAIR_DIAL,
            ButtonRole::Ordinary,
            "Reconnect a computer paired before",
            address,
            ButtonKind::Secondary,
        ));
        buttons.push(button(
            ids::PAIR_SCAN,
            ButtonRole::Ordinary,
            "Look for computers nearby",
            true,
            ButtonKind::Secondary,
        ));
        for (i, candidate) in self.connect.candidates.iter().enumerate() {
            buttons.push(button(
                ids::pair_candidate(i),
                ButtonRole::Ordinary,
                &bounded(format!("Pair with {}", candidate.name)),
                true,
                ButtonKind::Secondary,
            ));
        }
        let connected = self.connected_peers();
        if connected.len() > 1 {
            for (i, (_, name)) in connected.iter().take(8).enumerate() {
                buttons.push(button(
                    ids::select_peer(i),
                    ButtonRole::Ordinary,
                    &bounded(format!("Use {name}")),
                    true,
                    ButtonKind::Secondary,
                ));
            }
        }
    }

    fn practice_controls(&self, buttons: &mut Vec<ButtonView>) {
        if let Some(run) = self
            .practice
            .run
            .as_ref()
            .filter(|_| self.practice.active())
        {
            let waiting = self.practice.tutorial.state() == TutorialState::WaitingUser
                || self.practice.tutorial.state() == TutorialState::Running;
            let private = self.source_policy()
                == crate::tutorial_flow::TutorialSourcePolicy::MacPrivateDisplay;
            for c in confirmations(run.role, private) {
                if run.confirmed.contains(&c) {
                    continue;
                }
                buttons.push(button(
                    ids::confirm(c),
                    ButtonRole::Confirm,
                    confirmation_label(c),
                    waiting,
                    ButtonKind::Primary,
                ));
            }
            if run.role == TutorialRole::AudioSender {
                buttons.push(button(
                    ids::PLAY_TONE,
                    ButtonRole::Ordinary,
                    "Play the test sound",
                    waiting,
                    ButtonKind::Secondary,
                ));
            }
            if run.role == TutorialRole::E2DestinationPull {
                for (i, w) in run.remote_windows.iter().enumerate() {
                    buttons.push(button(
                        ids::remote_window(i),
                        ButtonRole::Ordinary,
                        &bounded(format!("Take “{}” ({})", w.title, w.app)),
                        waiting,
                        ButtonKind::Secondary,
                    ));
                }
            }
            buttons.push(button(
                ids::PRACTICE_CANCEL,
                ButtonRole::Stop,
                "Stop this practice",
                true,
                ButtonKind::Secondary,
            ));
            return;
        }
        for role in graph::ROLES {
            let step = steps::practice(role);
            if self.satisfied(step) {
                continue;
            }
            buttons.push(button(
                ids::practice_start(role),
                ButtonRole::Ordinary,
                &format!("Start: {}", graph::role_label(role)),
                self.prerequisites_valid(step) && !self.practice.engaged(self.now),
                ButtonKind::Secondary,
            ));
        }
    }

    fn maintenance_controls(&self, buttons: &mut Vec<ButtonView>, fields: &mut Vec<FieldView>) {
        let m = &self.maintenance;
        let repair = &m.repair_state;
        let available = m.uninstall == Some(Availability::Available);
        // Once a repair is under way (or ended) removal isn't offered on this visit: the person
        // leaves and comes back to see a fresh reading. An earlier repair that can only be
        // resumed doesn't hide it: when a resume can't settle that record, removing Crosspane
        // and installing it again is the way out every refusal names.
        let open = available
            && !m.confirmed
            && m.finished.is_none()
            && m.refused.is_none()
            && !repair.engaged();
        for choice in &m.choices {
            fields.push(FieldView::Toggle {
                id: ids::removal_field(choice.id),
                role: choice.role,
                label: choice.label.clone(),
                checked: choice.checked,
                enabled: open && !m.planning && choice.enabled,
            });
        }
        if open && m.preview.is_none() {
            buttons.push(button(
                ids::REMOVE_REVIEW,
                ButtonRole::Ordinary,
                "Review what will be removed",
                !m.planning,
                ButtonKind::Secondary,
            ));
        }
        if open && m.preview.is_some() {
            buttons.push(button(
                ids::REMOVE_CONFIRM,
                ButtonRole::Confirm,
                "Remove Crosspane",
                true,
                ButtonKind::Destructive,
            ));
        }
        if m.confirmed && m.finished.is_none() {
            for (id, label, _) in &m.follow_ups {
                buttons.push(button(
                    ids::follow_up_confirm(*id),
                    ButtonRole::Confirm,
                    label,
                    true,
                    ButtonKind::Destructive,
                ));
                buttons.push(button(
                    ids::follow_up_decline(*id),
                    ButtonRole::Ordinary,
                    "Keep it",
                    true,
                    ButtonKind::Secondary,
                ));
            }
        }
        let repair_open = m.repair == Some(Availability::Available)
            && !m.confirmed
            && !m.planning
            && m.preview.is_none()
            && m.finished.is_none()
            && m.refused.is_none()
            && !repair.engaged()
            && repair.resumable.is_none();
        buttons.push(button(
            ids::REPAIR,
            ButtonRole::Ordinary,
            "Review repair",
            repair_open,
            ButtonKind::Secondary,
        ));
        if repair.phase == RepairPhase::Previewed {
            buttons.push(button(
                ids::REPAIR_CONFIRM,
                ButtonRole::Confirm,
                "Repair Crosspane",
                true,
                ButtonKind::Primary,
            ));
        }
        if repair.discardable && m.refused.is_none() {
            buttons.push(button(
                ids::REPAIR_DISCARD,
                ButtonRole::Confirm,
                "Discard",
                !repair.engaged() && !m.planning && !m.confirmed && m.preview.is_none(),
                ButtonKind::Secondary,
            ));
        }
        if repair.resume_offered() && m.refused.is_none() {
            // Not while a removal is being reviewed or runs: one change at a time.
            buttons.push(button(
                ids::REPAIR_RESUME,
                ButtonRole::Confirm,
                "Resume repair",
                !m.planning && !m.confirmed && m.preview.is_none(),
                ButtonKind::Primary,
            ));
        }
        if !m.running() {
            if m.finished.is_none() {
                buttons.push(button(
                    ids::BACK,
                    ButtonRole::Back,
                    "Back",
                    !m.repair_waiting(),
                    ButtonKind::Secondary,
                ));
            }
            buttons.push(button(
                ids::CLOSE,
                ButtonRole::Cancel,
                "Close",
                true,
                ButtonKind::Secondary,
            ));
        }
    }

    pub(super) fn rebuild_view(&mut self) {
        let screen = self.display_screen();
        let (title, message) = self.title_and_message(screen);
        let (buttons, fields) = self.buttons_and_fields(screen);
        let layout = (screen == ScreenId::Layout).then(|| {
            let (confirmed, local_node, peer_order) = self.layout_rects();
            LayoutPreview {
                confirmed,
                local_node,
                peer_order,
                busy: self.connect.layout_busy,
            }
        });
        let practice = self
            .practice
            .run
            .as_ref()
            .filter(|_| self.practice.active())
            .map(|run| match run.role {
                TutorialRole::E1Controller | TutorialRole::E1Target => {
                    PracticeIllustration::Pointer
                }
                TutorialRole::AudioSender | TutorialRole::AudioReceiver => {
                    PracticeIllustration::Tone
                }
                _ => PracticeIllustration::Window,
            });
        let sas = (screen == ScreenId::MatchNumbers)
            .then(|| self.connect.pairing.as_ref().and_then(|p| p.sas.clone()))
            .flatten()
            .map(bounded);
        let escape = if self.mutation_in_flight() {
            EscapeMapping::None
        } else {
            match screen {
                ScreenId::Welcome | ScreenId::Summary => EscapeMapping::Close,
                ScreenId::MatchNumbers => EscapeMapping::None,
                _ => EscapeMapping::Back,
            }
        };
        let mut view = WizardView {
            revision: self.view.revision,
            escape,
            fields,
            screen,
            title,
            message,
            machine: Some(bounded(self.desc.machine_label.clone())),
            peer: self.peer_name(),
            rows: self.screen_rows(screen),
            buttons,
            summary: match self.summary.milestone {
                Milestone::NotInstalled => SummaryView::NotInstalled,
                Milestone::InstalledWaiting => SummaryView::InstalledWaiting,
                Milestone::WorkspaceReady => SummaryView::WorkspaceReady,
            },
            hiding_choice: (screen == ScreenId::HidingChoice)
                .then_some(self.connect.hiding)
                .flatten(),
            motion: self.motion,
            system_reduced_motion: None,
            layout,
            progress: self.progress(screen),
            illustration: IllustrationView {
                permission_row: None,
                traffic_observed: false,
                sas,
                practice,
            },
            demo: false,
        };
        let previews: Vec<&String> = self
            .graph
            .on_screen(screen)
            .filter(|m| self.step_state(m.id) == StepState::NeedsAction)
            .filter_map(|m| self.previews.get(&m.id))
            .collect();
        let signature = format!(
            "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
            view.screen,
            view.buttons
                .iter()
                .map(|b| (b.id, b.role, b.enabled, b.kind, &b.label))
                .collect::<Vec<_>>(),
            view.fields
                .iter()
                .map(|f| match f {
                    FieldView::PeerAddress { id, enabled, .. } => (*id, *enabled, String::new()),
                    FieldView::Toggle {
                        id, enabled, label, ..
                    } => (*id, *enabled, label.clone()),
                })
                .collect::<Vec<_>>(),
            previews,
            view.illustration.sas,
            view.layout.as_ref().map(|l| (&l.confirmed, l.busy)),
            view.hiding_choice,
            self.maintenance.preview,
            (
                self.maintenance.repair_state.plan,
                &self.maintenance.repair_state.preview,
            ),
        );
        if signature != self.signature {
            view.revision = self.view.revision.saturating_add(1);
            self.signature = signature;
        }
        self.view = view;
    }

    pub(super) fn back(&mut self) {
        if self.mutation_in_flight() {
            return;
        }
        if self.screen == ScreenId::RepairRemove {
            let to = self
                .maintenance
                .return_to
                .take()
                .unwrap_or(ScreenId::Welcome);
            self.go(to);
        } else if let Some(previous) = self.previous_screen() {
            self.go(previous);
        }
    }

    /// Returns true when the window should close.
    pub(super) fn button(&mut self, id: u16) -> bool {
        match id {
            ids::NEXT => {
                if let Some(next) = self.next_screen() {
                    self.go(next);
                }
            }
            ids::BACK => self.back(),
            ids::CLOSE => return self.try_close(),
            ids::REMOVE_OR_REPAIR => {
                self.maintenance.return_to = Some(self.screen);
                self.go(ScreenId::RepairRemove);
            }
            ids::PAIR_LISTEN => self.pair_action(PairMode::Listen),
            ids::PAIR_JOIN => {
                if let Some(addr) = self.parsed_address() {
                    self.pair_action(PairMode::Join(addr));
                }
            }
            ids::PAIR_DIAL => {
                if let Some(addr) = self.parsed_address() {
                    self.pair_action(PairMode::Dial(addr));
                }
            }
            ids::PAIR_SCAN => self.scan(),
            ids::PAIR_CONFIRM => self.pair_answer(InstallerRequest::PairConfirm { accept: true }),
            ids::PAIR_REJECT => self.pair_answer(InstallerRequest::PairConfirm { accept: false }),
            ids::GRANTS_APPLY => self.request_apply(steps::GRANTS),
            ids::LAYOUT_ACCEPT => self.accept_layout(),
            ids::HIDING_APPLY => self.request_apply(steps::HIDING),
            ids::HIDING_RESTART => self.hiding_restart(),
            ids::PLAY_TONE => self.practice_user(TutorialUserAction::PlayTestSound),
            ids::PRACTICE_CANCEL => self.practice_user(TutorialUserAction::Cancel),
            ids::FINAL_CHECK => self.begin(steps::FINAL),
            ids::REMOVE_REVIEW => self.plan_uninstall(),
            ids::REMOVE_CONFIRM => self.confirm_uninstall(),
            ids::REPAIR => self.review_repair(),
            ids::REPAIR_CONFIRM => self.confirm_repair(),
            ids::REPAIR_RESUME => self.resume_repair(),
            ids::REPAIR_DISCARD => self.discard_repair(),
            ids::RETRY_ALL => {
                for step in self.retryable_on(self.display_screen()) {
                    self.begin(step);
                }
            }
            other => self.ranged_button(other),
        }
        false
    }

    /// The steps on `screen` that "Check again" re-checks, in screen order.
    fn retryable_on(&self, screen: ScreenId) -> Vec<StepId> {
        self.graph
            .on_screen(screen)
            .filter(|m| !matches!(m.kind, StepKind::Practice(_) | StepKind::Final))
            .filter(|m| retryable(self.step_state(m.id)))
            .map(|m| m.id)
            .collect()
    }

    fn ranged_button(&mut self, id: u16) {
        match id {
            110..=190 => self.begin(StepId(id - 100)),
            1010..=1090 => self.consent_click(StepId(id - 1000)),
            2010..=2025 => {
                if let Some(c) = self.connect.candidates.get(usize::from(id - 2010)) {
                    let addr = c.addr;
                    self.pair_action(PairMode::Join(addr));
                }
            }
            2030..=2045 => {
                if let Some((node, _)) = self.connected_peers().get(usize::from(id - 2030)) {
                    let node = *node;
                    self.pair_action(PairMode::Existing(node));
                }
            }
            2110..=2125 => self.pair_answer(InstallerRequest::PairPick {
                index: usize::from(id - 2110),
            }),
            3000..=3008 => {
                if let Some(role) = graph::ROLES.get(usize::from(id - 3000)) {
                    self.start_practice(*role);
                }
            }
            3100..=3110 => {
                if let Some(c) = CONFIRMATIONS.get(usize::from(id - 3100)) {
                    self.practice_user(TutorialUserAction::Confirm(*c));
                }
            }
            3300..=3315 => {
                let window = self
                    .practice
                    .run
                    .as_ref()
                    .and_then(|r| r.remote_windows.get(usize::from(id - 3300)))
                    .map(|w| w.id);
                if let Some(window) = window {
                    self.practice_user(TutorialUserAction::SelectRemoteWindow { window });
                }
            }
            5100..=5190 => self.follow_up(id - 5100, true),
            5200..=5290 => self.follow_up(id - 5200, false),
            _ => {}
        }
    }

    fn consent_click(&mut self, step: StepId) {
        if self.graph.kind(step) != Some(StepKind::Native)
            || self.step_state(step) != StepState::NeedsAction
            || !self.previews.contains_key(&step)
        {
            return;
        }
        let Some(plan) = self.job(step, JobStage::Plan) else {
            return;
        };
        self.consents.insert(
            step,
            Consent {
                plan: plan.operation,
                operation: plan.operation,
                revision: self.view.revision,
            },
        );
        if self
            .reduce(crosspane_installer_core::FlowEvent::ApplyRequested {
                step,
                operation: plan.operation,
            })
            .is_err()
        {
            self.consents.remove(&step);
        }
    }

    pub(super) fn toggle(&mut self, field: u16, checked: bool) {
        if let Some(i) = (0..5).find(|i| ids::grant_field(*i) == field) {
            self.grant_toggle(i, checked);
            return;
        }
        let m = &mut self.maintenance;
        // While a plan is being prepared the choices are frozen, so the preview that comes back
        // always describes exactly the choices that will be confirmed.
        if m.confirmed || m.planning || m.finished.is_some() || m.repair_state.engaged() {
            return;
        }
        if let Some(choice) = m
            .choices
            .iter_mut()
            .find(|c| ids::removal_field(c.id) == field && c.enabled)
        {
            choice.checked = checked;
            // A changed selection retires the reviewed plan.
            m.preview = None;
        }
    }

    pub(super) fn inspect_maintenance(&mut self) {
        let return_to = self.maintenance.return_to;
        let next_id = self.maintenance.next_id.saturating_add(1);
        self.maintenance = MaintenanceState {
            next_id,
            current: Some(MaintenanceId(next_id)),
            return_to,
            ..MaintenanceState::default()
        };
        self.submit_maintenance(MaintenanceRequest::Inspect {
            id: MaintenanceId(next_id),
        });
    }

    fn submit_maintenance(&mut self, request: MaintenanceRequest) {
        if let Err(refusal) = self.platform.submit(NativeJob::Maintenance(request)) {
            self.maintenance.refused = Some(bounded(refusal.to_string()));
            // A request the platform never took can't leave a repair "working".
            let r = &mut self.maintenance.repair_state;
            r.phase = RepairPhase::Idle;
            r.awaiting = None;
            r.driving = false;
            r.verify_outstanding = false;
        }
    }

    // ---- repair ------------------------------------------------------------------------------

    /// Ask for the repair preview. The platform plans against a Status issued after this click.
    fn review_repair(&mut self) {
        let m = &self.maintenance;
        if m.current.is_none()
            || m.repair != Some(Availability::Available)
            || m.confirmed
            || m.planning
            || m.preview.is_some()
            || m.finished.is_some()
            || m.refused.is_some()
            || m.repair_state.engaged()
            || m.repair_state.resumable.is_some()
        {
            return;
        }
        self.await_repair_status(AwaitKind::Plan, RepairPhase::Planning);
    }

    /// Confirm the preview on screen: the click carries its plan number and the view revision it
    /// was given on, and the platform observes again with a Status issued after this click.
    fn confirm_repair(&mut self) {
        let r = &self.maintenance.repair_state;
        let (RepairPhase::Previewed, Some(plan)) = (r.phase, r.plan) else {
            return;
        };
        let revision = self.view.revision;
        self.await_repair_status(AwaitKind::Confirm { plan, revision }, RepairPhase::Running);
    }

    fn discard_repair(&mut self) {
        let m = &self.maintenance;
        if m.current.is_none()
            || !m.repair_state.discardable
            || m.repair_state.engaged()
            || m.planning
            || m.confirmed
            || m.preview.is_some()
            || m.refused.is_some()
        {
            return;
        }
        self.maintenance.repair_state.discarding = true;
        self.await_repair_status(AwaitKind::Discard, RepairPhase::Running);
    }

    fn resume_repair(&mut self) {
        let m = &self.maintenance;
        if m.current.is_none()
            || !m.repair_state.resume_offered()
            || m.planning
            || m.confirmed
            || m.preview.is_some()
            || m.refused.is_some()
        {
            return;
        }
        self.await_repair_status(AwaitKind::Resume, RepairPhase::Running);
    }

    fn await_repair_status(&mut self, kind: AwaitKind, phase: RepairPhase) {
        let r = &mut self.maintenance.repair_state;
        r.phase = phase;
        r.since = self.now;
        r.detail = None;
        if kind == AwaitKind::Resume {
            // A resume starts a new attempt: the earlier verdict and offer are replaced by its own.
            r.finished = None;
            r.resumable = None;
        }
        r.awaiting = Some(AwaitStatus {
            kind,
            from_call: self.next_call,
            started_at: self.now,
        });
        self.request_status_now();
    }

    fn submit_repair(&mut self, kind: AwaitKind, status: Option<StatusEvidence>) {
        let Some(id) = self.maintenance.current else {
            return;
        };
        self.submit_maintenance(match kind {
            AwaitKind::Plan => MaintenanceRequest::PlanRepair { id, status },
            AwaitKind::Discard => MaintenanceRequest::DiscardRepair { id, status },
            AwaitKind::Confirm { plan, revision } => MaintenanceRequest::ConfirmRepair {
                id,
                plan,
                revision,
                status,
            },
            AwaitKind::Resume => MaintenanceRequest::ResumeRepair { id, status },
        });
    }

    /// A Status reply arrived: release the repair click that waits for one issued after it.
    pub(super) fn release_repair_wait(&mut self, reply: &crate::agent_contract::AgentReply) {
        let Some(wait) = self.maintenance.repair_state.awaiting else {
            return;
        };
        if reply.id < wait.from_call {
            return;
        }
        self.maintenance.repair_state.awaiting = None;
        self.submit_repair(wait.kind, Some(StatusEvidence(reply.clone())));
    }

    /// Per-tick repair driving: a click still waiting for its Status gives up after a while and
    /// goes ahead without one, and a repair that started the new agent keeps offering it fresh
    /// Status replies until the platform ends the wait.
    pub(super) fn repair_tick(&mut self) {
        let now = self.now;
        if let Some(wait) = self.maintenance.repair_state.awaiting {
            if now.saturating_sub(wait.started_at) > REPAIR_STATUS_WAIT_MS {
                self.maintenance.repair_state.awaiting = None;
                self.submit_repair(wait.kind, None);
            } else {
                self.poll_for_repair();
            }
        }
        if !self.maintenance.repair_driving() {
            return;
        }
        let r = &mut self.maintenance.repair_state;
        if now.saturating_sub(r.since) > REPAIR_WAIT_BACKSTOP_MS {
            // The platform went silent: say so rather than keep the view working forever.
            r.phase = RepairPhase::Idle;
            r.driving = false;
            r.verify_outstanding = false;
            r.finished = Some(RepairEnd {
                outcome: RepairOutcome::OutcomeUnknown,
                lines: vec![
                    "Setup stopped hearing from the repair, so what it did can't be proved. Nothing was retried."
                        .to_owned(),
                ],
                resumable: true,
            });
            return;
        }
        if r.verify_outstanding {
            return;
        }
        self.poll_for_repair();
        let fresh = self.health.as_ref().filter(|h| {
            h.call >= self.maintenance.repair_state.from_call
                && h.call > self.maintenance.repair_state.last_call
                && now.saturating_sub(h.observed_at_ms) <= REPAIR_STATUS_FRESH_MS
        });
        let status = match fresh {
            Some(h) => {
                self.maintenance.repair_state.last_call = h.call;
                Some(StatusEvidence(h.reply.clone()))
            }
            // No fresh Status: the stopped old agent can't answer, nor can a new one that isn't
            // up yet. The platform is asked anyway, now and then, so its own stages and bounded
            // wait move on.
            None if now.saturating_sub(self.maintenance.repair_state.last_look)
                >= REPAIR_BLIND_LOOK_MS =>
            {
                None
            }
            None => return,
        };
        let Some(id) = self.maintenance.current else {
            return;
        };
        let request = MaintenanceRequest::VerifyRepair { id, status };
        let r = &mut self.maintenance.repair_state;
        r.last_look = now;
        r.verify_outstanding = true;
        // A look the platform can't take now (it is busy) is simply tried again; it never ends
        // or abandons the repair that is under way.
        if self
            .platform
            .submit(NativeJob::Maintenance(request))
            .is_err()
        {
            self.maintenance.repair_state.verify_outstanding = false;
        }
    }

    /// Ask for a Status now and then; one outstanding call at a time.
    fn poll_for_repair(&mut self) {
        if self
            .status_sent_at
            .is_none_or(|sent| self.now >= sent.saturating_add(super::controller::STATUS_ACTIVE_MS))
        {
            self.request_status_now();
        }
    }

    fn plan_uninstall(&mut self) {
        let Some(id) = self.maintenance.current else {
            return;
        };
        if self.maintenance.uninstall != Some(Availability::Available) || self.maintenance.confirmed
        {
            return;
        }
        self.maintenance.planning = true;
        let choices = self
            .maintenance
            .choices
            .iter()
            .map(|c| (c.id, c.checked))
            .collect();
        let status = self.fresh_status();
        self.submit_maintenance(MaintenanceRequest::PlanUninstall {
            id,
            choices,
            status,
        });
    }

    /// The latest Status, handed over only while it is fresh enough to plan against.
    fn fresh_status(&self) -> Option<super::StatusEvidence> {
        self.health
            .as_ref()
            .filter(|h| self.now.saturating_sub(h.observed_at_ms) <= 3_000)
            .map(|h| super::StatusEvidence(h.reply.clone()))
    }

    fn confirm_uninstall(&mut self) {
        let Some(id) = self.maintenance.current else {
            return;
        };
        if self.maintenance.preview.is_none() || self.maintenance.confirmed {
            return;
        }
        self.maintenance.confirmed = true;
        let revision = self.view.revision;
        let status = self.fresh_status();
        self.submit_maintenance(MaintenanceRequest::ConfirmUninstall {
            id,
            revision,
            status,
        });
    }

    fn follow_up(&mut self, follow_up: u16, confirm: bool) {
        let Some(id) = self.maintenance.current else {
            return;
        };
        if !self
            .maintenance
            .follow_ups
            .iter()
            .any(|(f, ..)| *f == follow_up)
        {
            return;
        }
        self.maintenance
            .follow_ups
            .retain(|(f, ..)| *f != follow_up);
        let revision = self.view.revision;
        self.submit_maintenance(if confirm {
            MaintenanceRequest::ConfirmFollowUp {
                id,
                follow_up,
                revision,
            }
        } else {
            MaintenanceRequest::DeclineFollowUp { id, follow_up }
        });
    }

    pub(super) fn maintenance_report(&mut self, report: MaintenanceReport) {
        let id = match &report {
            MaintenanceReport::Inspected { id, .. }
            | MaintenanceReport::Planned { id, .. }
            | MaintenanceReport::Progress { id, .. }
            | MaintenanceReport::FollowUp { id, .. }
            | MaintenanceReport::Finished { id, .. }
            | MaintenanceReport::Refused { id, .. }
            | MaintenanceReport::RepairDiscardable { id, .. }
            | MaintenanceReport::RepairDiscarded { id, .. }
            | MaintenanceReport::RepairPlanned { id, .. }
            | MaintenanceReport::RepairResumable { id, .. }
            | MaintenanceReport::RepairWaiting { id, .. }
            | MaintenanceReport::RepairFinished { id, .. } => *id,
        };
        if self.maintenance.current != Some(id) {
            return;
        }
        if let MaintenanceReport::RepairDiscarded { lines, .. } = &report {
            let r = &self.maintenance.repair_state;
            if r.discarding && r.phase == RepairPhase::Running && r.awaiting.is_none() {
                let lines = lines
                    .iter()
                    .take(MAX_PROGRESS_LINES)
                    .cloned()
                    .map(bounded)
                    .collect();
                self.inspect_maintenance();
                self.maintenance.progress = lines;
            }
            return;
        }
        let (now, next_call) = (self.now, self.next_call);
        let m = &mut self.maintenance;
        match report {
            MaintenanceReport::RepairDiscardable { .. }
                if m.repair_state.phase == RepairPhase::Idle =>
            {
                m.repair_state.discardable = true;
            }
            MaintenanceReport::RepairDiscardable { .. }
            | MaintenanceReport::RepairDiscarded { .. } => {}
            MaintenanceReport::Inspected {
                uninstall,
                repair,
                choices,
                ..
            } => {
                m.uninstall = Some(uninstall);
                m.repair = Some(repair);
                m.choices = choices.into_iter().take(8).collect();
            }
            // Only the answer to the outstanding review request is shown as the plan.
            MaintenanceReport::Planned { preview, .. } if m.planning && !m.confirmed => {
                m.planning = false;
                m.preview = Some(bounded(preview));
            }
            MaintenanceReport::Planned { .. } => {}
            MaintenanceReport::Progress { detail, .. } => {
                if m.progress.len() < MAX_PROGRESS_LINES {
                    m.progress.push(bounded(detail));
                }
            }
            MaintenanceReport::FollowUp {
                follow_up,
                label,
                preview,
                ..
            } => {
                if !m.follow_ups.iter().any(|(f, ..)| *f == follow_up) {
                    m.follow_ups
                        .push((follow_up, bounded(label), bounded(preview)));
                }
            }
            MaintenanceReport::Finished { outcome, lines, .. } => {
                m.follow_ups.clear();
                m.finished = Some((
                    outcome,
                    lines
                        .into_iter()
                        .take(MAX_PROGRESS_LINES)
                        .map(bounded)
                        .collect(),
                ));
            }
            // A confirmed repair under way is only ever asked to look again, which is never
            // refused: a refusal now answers some replayed or stale request, and must not hide the
            // repair that is still running (nor let the window close over it).
            MaintenanceReport::Refused { .. } if m.repair_driving() => {}
            MaintenanceReport::Refused { reason, .. } => {
                m.planning = false;
                m.refused = Some(bounded(reason));
                // A refusal ends whatever repair step it answered: nothing stays "working".
                let r = &mut m.repair_state;
                r.phase = RepairPhase::Idle;
                r.awaiting = None;
                r.verify_outstanding = false;
            }
            // Only the answer to the outstanding review request is shown as the repair plan.
            MaintenanceReport::RepairPlanned { plan, preview, .. }
                if m.repair_state.phase == RepairPhase::Planning =>
            {
                let r = &mut m.repair_state;
                r.phase = RepairPhase::Previewed;
                r.plan = Some(plan);
                r.preview = Some(bounded(preview));
            }
            MaintenanceReport::RepairPlanned { .. } => {}
            MaintenanceReport::RepairResumable { lines, .. }
                if m.repair_state.phase == RepairPhase::Idle =>
            {
                m.repair_state.resumable = Some(
                    lines
                        .into_iter()
                        .take(MAX_PROGRESS_LINES)
                        .map(bounded)
                        .collect(),
                );
            }
            MaintenanceReport::RepairResumable { .. } => {}
            MaintenanceReport::RepairWaiting {
                detail, closeable, ..
            } if matches!(
                m.repair_state.phase,
                RepairPhase::Running | RepairPhase::Waiting
            ) && m.repair_state.awaiting.is_none() =>
            {
                let r = &mut m.repair_state;
                if !r.driving {
                    // Health is judged only from Statuses issued after the repair got this far.
                    r.driving = true;
                    r.from_call = next_call;
                }
                // Only a wait that cuts no change short and leaves a resumable record may close.
                r.phase = if closeable {
                    RepairPhase::Waiting
                } else {
                    RepairPhase::Running
                };
                r.since = now;
                r.last_look = now;
                r.detail = Some(bounded(detail));
                r.verify_outstanding = false;
            }
            MaintenanceReport::RepairWaiting { .. } => {}
            MaintenanceReport::RepairFinished {
                outcome,
                lines,
                resumable,
                ..
            } if matches!(
                m.repair_state.phase,
                RepairPhase::Running | RepairPhase::Waiting
            ) && m.repair_state.awaiting.is_none() =>
            {
                let r = &mut m.repair_state;
                r.phase = RepairPhase::Idle;
                r.driving = false;
                r.verify_outstanding = false;
                r.awaiting = None;
                r.preview = None;
                r.plan = None;
                r.resumable = None;
                r.finished = Some(RepairEnd {
                    outcome,
                    lines: lines
                        .into_iter()
                        .take(MAX_PROGRESS_LINES)
                        .map(bounded)
                        .collect(),
                    resumable,
                });
            }
            MaintenanceReport::RepairFinished { .. } => {}
        }
    }
}
