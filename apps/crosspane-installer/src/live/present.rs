//! View building and action handling. The revision changes only when meaning changes: screen,
//! buttons, fields, consent previews, the SAS or layout geometry and busy state.

use crosspane_installer_core::{JobStage, Milestone, StepId, StepState};

pub(super) mod permissions;

use super::controller::{LiveController, automatic_screen, bounded, bounded_lines};
use super::graph::{StepKind, steps};
use super::shared::{CAPABILITIES, PairMode, capability_label};
use super::{
    Availability, CheckState, Consent, MaintenanceId, MaintenanceOutcome, MaintenanceReport,
    MaintenanceRequest, NativeJob, RemovalChoice, RepairOutcome, StatusEvidence, ids,
};
use crate::agent_contract::{InstallerRequest, PairPhase};
use crate::view::{
    ButtonKind, ButtonRole, ButtonView, EscapeMapping, FieldView, IllustrationView, LayoutPreview,
    ProgressGroup, ProgressView, RowState, RowView, ScreenId, SummaryView, ToggleRole, WizardView,
    check_row_id,
};

const MAX_PROGRESS_LINES: usize = 24;

/// Rows the pairing screen draws in place of the pairing step: what it is doing right now, and
/// the computer it found. Outside both the step and the checklist id ranges.
const LOOKING_ROW: u16 = 950;
const FOUND_ROW: u16 = 951;

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
        StepState::Skipped => RowState::Skipped,
        StepState::Failed => RowState::Failed,
        StepState::Unsupported => RowState::Unsupported,
    }
}

/// A step's title names the action, so it reads right whatever its state: the mark and the line
/// under it say how far it got ("Install Crosspane" with "Installing…", never "Crosspane is
/// installed" while it isn't). The platforms describe steps by outcome; unknown ones pass through.
fn step_title(outcome: &str) -> &str {
    match outcome {
        "This computer can run Crosspane" => "Check this computer",
        "This Mac can run Crosspane" => "Check this Mac",
        "Crosspane is installed for your account"
        | "Crosspane is installed and starts when you sign in" => "Install Crosspane",
        "Crosspane starts when you sign in" => "Start Crosspane when you sign in",
        "A restart brings up a new, healthy instance" => "Restart Crosspane",
        "Crosspane is running with its key in the system keyring"
        | "Crosspane is running with its key in the Keychain" => "Check that Crosspane is running",
        "This computer can reach the other one on your network" => {
            "Allow Crosspane on your network"
        }
        "Crosspane has the Mac permissions it needs" => "Allow Mac permissions",
        "The Crosspane sound driver is installed" => "Install the sound driver",
        "Paired and connected to the other computer" => "Pair with the other computer",
        "What the other computer may do here" => "Choose what the other computer may do",
        "Where the other computer's screens sit" => "Arrange the screens",
        "How windows you send are hidden here" => "Choose how sent windows are hidden",
        "Everything is working right now" => "Check that everything works",
        other => other,
    }
}

/// A support check as the person reads it. Checks that need nothing from the person are notes,
/// folded away with the passed ones, and internal wording is replaced by plain language.
fn plain_check(state: RowState, detail: String) -> (RowState, String) {
    const PLAIN: [(&str, RowState, &str); 3] = [
        (
            "Runtime state is active or couldn't be proved safe to recover",
            RowState::Note,
            "Existing Crosspane files are left in place while setup checks them",
        ),
        (
            "The owned dead runtime will be cleaned",
            RowState::Note,
            "Leftovers from an earlier Crosspane are cleaned up during install",
        ),
        (
            "A system font couldn't be confirmed",
            RowState::Note,
            "The system font couldn't be confirmed; setup carries on",
        ),
    ];
    for (raw, plain_state, plain) in PLAIN {
        if detail.starts_with(raw) {
            return (plain_state, plain.to_owned());
        }
    }
    (state, detail)
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

    /// The screen shown: the pairing screen becomes the numbers comparison while one runs.
    pub(super) fn display_screen(&self) -> ScreenId {
        if self.screen == ScreenId::Connect
            && matches!(
                self.pairing_phase(),
                Some(PairPhase::Confirm | PairPhase::Pick | PairPhase::Waiting)
            )
        {
            ScreenId::MatchNumbers
        } else {
            self.screen
        }
    }

    /// The steps shown together: the three install screens are one page with one checklist.
    fn page_steps(&self, screen: ScreenId) -> Vec<StepId> {
        self.graph
            .metas
            .iter()
            .filter(|m| {
                if automatic_screen(screen) {
                    automatic_screen(m.screen)
                } else {
                    m.screen == screen
                }
            })
            .map(|m| m.id)
            .collect()
    }

    /// The other computer's name as the pairing knows it, if it does yet.
    fn pairing_peer(&self) -> Option<String> {
        self.connect
            .pairing
            .as_ref()
            .and_then(|p| p.peer.clone())
            .map(bounded)
            .or_else(|| match &self.connect.mode {
                Some(PairMode::Join(addr)) => self
                    .pair_candidates()
                    .iter()
                    .find(|c| c.addr == *addr)
                    .map(|c| bounded(c.name.clone())),
                _ => None,
            })
    }

    pub(super) fn row(&self, step: StepId) -> RowView {
        let state = self.step_state(step);
        let meta = self.graph.meta(step);
        // While a change runs, what it is doing is the preview it was started from.
        let running = (state == StepState::Running)
            .then(|| self.previews.get(&step).cloned())
            .flatten();
        let detail = running
            .or_else(|| self.details.get(&step).cloned())
            .unwrap_or_else(|| match state {
                StepState::NeedsAction => self.previews.get(&step).cloned().unwrap_or_default(),
                StepState::Stale => "Needs checking again.".into(),
                StepState::NotChecked => "Not checked yet.".into(),
                StepState::Satisfied => "Done.".into(),
                StepState::Skipped => super::skipped::LATER.into(),
                _ => String::new(),
            });
        let detail = match self.support_timing(step) {
            Some(timing) if detail.is_empty() => timing,
            Some(timing) => bounded(format!("{detail} {timing}")),
            None => detail,
        };
        RowView {
            id: step.0,
            label: meta.map_or_else(String::new, |m| step_title(&m.label).to_owned()),
            detail,
            state: row_state(state),
            human_confirmed: false,
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
                let (state, detail) = plain_check(state, detail);
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
        match screen {
            ScreenId::Summary => self.summary_rows(),
            ScreenId::Welcome | ScreenId::RepairRemove => Vec::new(),
            ScreenId::Connect | ScreenId::MatchNumbers => self.connect_rows(),
            // One row per permission, each with its own Allow (WP-4.33).
            ScreenId::Permissions if self.permission_rows_active() => self
                .page_steps(screen)
                .into_iter()
                .flat_map(|step| {
                    if self.graph.meta(step).and_then(|m| m.agent_apply)
                        == Some(super::AgentApply::AskPermissions)
                    {
                        self.permission_step_rows(step)
                    } else {
                        vec![self.row(step)]
                    }
                })
                .collect(),
            // These screens ask their question in their content; the step itself is shown only
            // while it isn't asking (checking, saving, or stopped with a reason).
            ScreenId::Grants | ScreenId::Layout | ScreenId::HidingChoice => self
                .page_steps(screen)
                .into_iter()
                .filter(|step| self.step_state(*step) != StepState::NeedsAction)
                .map(|step| self.row(step))
                .collect(),
            other => self
                .page_steps(other)
                .into_iter()
                .flat_map(|step| {
                    let mut rows = vec![self.row(step)];
                    rows.extend(self.support_check_rows(step));
                    rows
                })
                .collect(),
        }
    }

    /// What the pairing screen says is happening, in place of the pairing step's own row.
    fn connect_rows(&self) -> Vec<RowView> {
        let line = |id: u16, label: String, detail: &str, state: RowState| RowView {
            id,
            label,
            detail: detail.to_owned(),
            state,
            human_confirmed: false,
        };
        let looking = || {
            line(
                LOOKING_ROW,
                "Looking for the other computer…".into(),
                "Open Crosspane setup on the other computer too. The two find each other on \
                 your network.",
                RowState::Working,
            )
        };
        let state = self.step_state(steps::PAIR);
        let peer = self
            .pairing_peer()
            .unwrap_or_else(|| "the other computer".into());
        match state {
            StepState::Satisfied | StepState::Skipped => vec![self.row(steps::PAIR)],
            StepState::NotChecked
            | StepState::Stale
            | StepState::Checking
            | StepState::Planning => {
                vec![looking()]
            }
            StepState::NeedsAction => {
                let candidates = self.pair_candidates();
                if self.connect.manual || candidates.len() > 1 || self.connected_peers().len() > 1 {
                    Vec::new()
                } else if let [only] = candidates {
                    vec![line(
                        FOUND_ROW,
                        bounded(format!("Found {} on your network", only.name)),
                        "Pair with it to continue. You'll compare a number on both screens.",
                        RowState::Note,
                    )]
                } else if self.connect.auto_windows >= super::shared::MAX_AUTO_WINDOWS {
                    vec![line(
                        LOOKING_ROW,
                        "Still no sign of the other computer".into(),
                        "Check that Crosspane setup is open on it and that both computers are on \
                         the same network, then look again.",
                        RowState::Waiting,
                    )]
                } else {
                    vec![looking()]
                }
            }
            StepState::Running => {
                let phase = self.connect.pairing.as_ref().map(|p| p.phase);
                let row = match (&self.connect.mode, phase) {
                    (_, Some(PairPhase::Waiting)) => line(
                        LOOKING_ROW,
                        bounded(format!("Waiting for you to confirm on {peer}")),
                        "",
                        RowState::Working,
                    ),
                    (Some(PairMode::Listen), _) if self.connect.collision => line(
                        LOOKING_ROW,
                        "The other computer is waiting too".into(),
                        "Both opened a pairing window at the same moment. This one steps back \
                         when its window closes, then pairs with the other.",
                        RowState::Waiting,
                    ),
                    (Some(PairMode::Listen), _) => line(
                        LOOKING_ROW,
                        "Ready to be found".into(),
                        "On the other computer, choose this one when Crosspane setup finds it. \
                         Then compare the number on both screens.",
                        RowState::Working,
                    ),
                    (Some(PairMode::Join(_)), _) => line(
                        LOOKING_ROW,
                        bounded(format!("Connecting to {peer}…")),
                        "",
                        RowState::Working,
                    ),
                    (Some(PairMode::Dial(_)), _) => line(
                        LOOKING_ROW,
                        "Reconnecting to a computer paired before…".into(),
                        "",
                        RowState::Working,
                    ),
                    _ => line(LOOKING_ROW, "Pairing…".into(), "", RowState::Working),
                };
                vec![row]
            }
            StepState::Verifying => vec![line(
                LOOKING_ROW,
                "Paired. Waiting for the two computers to connect…".into(),
                "",
                RowState::Working,
            )],
            // Setup's own window closed with nobody joining: it is about to look again.
            _ if self.connect.relook_at.is_some() => vec![looking()],
            _ => {
                let mut row = self.row(steps::PAIR);
                row.label = "Pairing didn't finish".into();
                vec![row]
            }
        }
    }

    fn progress(&self, screen: ScreenId) -> ProgressView {
        let mut completed = Vec::new();
        for group in [
            ProgressGroup::Install,
            ProgressGroup::PermissionsNetwork,
            ProgressGroup::Connect,
            ProgressGroup::Arrange,
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

    /// The previews of the steps on this page that are asking for consent right now.
    fn consent_preview(&self, screen: ScreenId) -> String {
        self.page_steps(screen)
            .into_iter()
            .filter(|step| self.step_state(*step) == StepState::NeedsAction)
            .filter_map(|step| self.previews.get(&step).cloned())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Why an install step that normally goes ahead by itself is asking instead.
    fn why_install_asks(&self, screen: ScreenId) -> &'static str {
        let repeated = self.page_steps(screen).into_iter().any(|step| {
            self.step_state(step) == StepState::NeedsAction && self.auto_consented.contains(&step)
        });
        if !self.agent_quiet() {
            "Crosspane is in use, so this waits for you. Continuing interrupts what is shared \
             right now."
        } else if repeated {
            "Setup already did this once and it is needed again, so it waits for you."
        } else {
            "This waits for you."
        }
    }

    /// Whether any step on this page stopped with a problem the person has to fix.
    fn page_failed(&self, screen: ScreenId) -> bool {
        self.page_steps(screen).into_iter().any(|step| {
            matches!(
                self.step_state(step),
                StepState::Failed | StepState::Unsupported
            )
        })
    }

    fn title_and_message(&self, screen: ScreenId) -> (String, String) {
        let preview = self.consent_preview(screen);
        let asking = !preview.is_empty();
        let peer = self.peer_name();
        let (title, message): (String, String) = match screen {
            ScreenId::Welcome => ("Set up Crosspane".into(), {
                let intro = "Use one keyboard and mouse across two computers, move windows \
                             between them and share sound. Setup asks only when it needs you.";
                match &self.desc.resume_note {
                    Some(note) => format!("{intro}\n\n{note}"),
                    None => intro.to_owned(),
                }
            }),
            ScreenId::Compatibility | ScreenId::InstallPlan | ScreenId::Installing => {
                let unsupported = self
                    .page_steps(screen)
                    .into_iter()
                    .any(|step| self.step_state(step) == StepState::Unsupported);
                let done = self
                    .page_steps(screen)
                    .into_iter()
                    .all(|step| self.satisfied(step));
                if unsupported {
                    (
                        "Crosspane can't run here yet".into(),
                        "Nothing was changed. The checks below say why.".into(),
                    )
                } else if self.page_failed(screen) {
                    (
                        "Setup stopped".into(),
                        "Fix the step marked below, then try again.".into(),
                    )
                } else if asking {
                    (
                        "Ready for the next step".into(),
                        format!("{preview}\n\n{}", self.why_install_asks(screen)),
                    )
                } else if done {
                    ("Crosspane is installed".into(), String::new())
                } else {
                    (
                        "Installing Crosspane".into(),
                        "This carries on by itself.".into(),
                    )
                }
            }
            ScreenId::Permissions if self.permission_rows_active() => (
                "Allow Crosspane on this Mac".into(),
                self.permission_message(),
            ),
            ScreenId::Permissions => (
                "Allow Crosspane on this Mac".into(),
                if asking {
                    preview
                } else {
                    "Allow each one when macOS asks. Setup notices as you go.".into()
                },
            ),
            ScreenId::AudioComponent => (
                "Bring sound across".into(),
                if asking {
                    preview
                } else {
                    "Adds the speakers the other computer plays to.".into()
                },
            ),
            ScreenId::Network => (
                if asking {
                    "Let Crosspane through the firewall".into()
                } else {
                    "Checking the network".into()
                },
                if asking {
                    preview
                } else {
                    "Checking that the other computer can reach this one.".into()
                },
            ),
            ScreenId::HidingChoice => (
                "Windows you send from this Mac".into(),
                if asking {
                    preview
                } else if self.satisfied(steps::HIDING) && self.connect.hiding_saved {
                    "Windows you send from this Mac are already hidden on a private virtual \
                     display while they are shown on the other computer."
                        .into()
                } else if self.hiding_restart_pending() {
                    "Your choice is saved. Restarting Crosspane ends what is shared right now \
                     (input, windows or sound)."
                        .into()
                } else {
                    "Setup applies your choice and restarts Crosspane.".into()
                },
            ),
            ScreenId::Connect => (
                "Pair with the other computer".into(),
                if self.connect.manual {
                    "Enter the other computer's address. The port is 47811 unless it was changed."
                        .into()
                } else {
                    "Both computers will show a number to compare.".into()
                },
            ),
            ScreenId::MatchNumbers => {
                let peer = self
                    .pairing_peer()
                    .unwrap_or_else(|| "the other computer".into());
                match self.pairing_phase() {
                    Some(PairPhase::Pick) => (
                        "Which number do you see on the other computer?".into(),
                        format!("Pick the number shown on {peer}."),
                    ),
                    Some(PairPhase::Waiting) => (
                        "Confirm on the other computer".into(),
                        format!("Confirm the number on {peer}. This screen moves on by itself."),
                    ),
                    _ => (
                        "Do the numbers match?".into(),
                        format!(
                            "Confirm only if {peer} shows the same number. If it doesn't, stop: \
                             something else may be trying to pair."
                        ),
                    ),
                }
            }
            ScreenId::Grants => (
                match &peer {
                    Some(peer) => format!("What may {peer} do here?"),
                    None => "What the other computer may do here".into(),
                },
                "Choose what to allow here. You can change this later in Settings.".into(),
            ),
            ScreenId::Layout => (
                "Arrange your screens".into(),
                "Drag the screens to match your desk.".into(),
            ),
            ScreenId::Summary => (
                match self.summary.milestone {
                    Milestone::WorkspaceReady => "Your workspace is ready",
                    Milestone::InstalledWaiting => "Crosspane is installed",
                    Milestone::NotInstalled => "Crosspane isn't installed yet",
                }
                .into(),
                match self.summary.milestone {
                    Milestone::WorkspaceReady if !self.summary.skipped.is_empty() => {
                        "Crosspane is ready. The steps you skipped are listed below.".to_owned()
                    }
                    Milestone::WorkspaceReady => "Everything was checked just now.".to_owned(),
                    Milestone::InstalledWaiting => "A few steps are left.".to_owned(),
                    Milestone::NotInstalled => {
                        "Finish installing to start using Crosspane.".to_owned()
                    }
                },
            ),
            ScreenId::RepairRemove => (
                "Remove or repair Crosspane".into(),
                self.maintenance_message(),
            ),
        };
        let mut message = message;
        if screen == ScreenId::Connect {
            message.push_str("\n\nSkipping Connect also skips Arrange.");
        }
        if self.skipped_on(screen)
            && !self.reopened.contains(&screen)
            && screen != ScreenId::Summary
        {
            message = "You skipped this step. Set it up when you're ready.".into();
        }
        if let Some(notice) = &self.notice {
            message = format!("{notice}\n\n{message}");
        }
        (bounded(title), bounded_lines(message.trim().to_owned()))
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

    /// The screen's actions. Each screen asks one question at a time: at most one primary button
    /// (its answer), outlined buttons beside it, quiet links for other ways, and tiles when the
    /// answer is picked from several.
    fn buttons_and_fields(&self, screen: ScreenId) -> (Vec<ButtonView>, Vec<FieldView>) {
        let mut buttons = Vec::new();
        let mut fields = Vec::new();
        if self.pending_skip.is_some() {
            return (
                vec![button(
                    ids::BACK,
                    ButtonRole::Back,
                    "Back",
                    false,
                    ButtonKind::Link,
                )],
                fields,
            );
        }
        if self.skipped_on(screen)
            && !self.reopened.contains(&screen)
            && screen != ScreenId::Summary
        {
            return (
                vec![
                    button(ids::BACK, ButtonRole::Back, "Back", true, ButtonKind::Link),
                    button(
                        ids::SET_UP_NOW,
                        ButtonRole::Ordinary,
                        "Set up now",
                        true,
                        ButtonKind::Primary,
                    ),
                ],
                fields,
            );
        }
        // Waiting in setup's own pairing window is not work the person must sit through.
        let busy = self.mutation_in_flight() && !self.following_auto_window();
        // Consent for a step that asks first: its own action, with its preview on screen.
        for step in self.page_steps(screen) {
            let Some(meta) = self.graph.meta(step) else {
                continue;
            };
            // The permission rows replace the permissions step's one ask-for-everything button.
            let rows_instead = screen == ScreenId::Permissions
                && meta.agent_apply == Some(super::AgentApply::AskPermissions)
                && self.permission_rows_active();
            if meta.kind == StepKind::Native
                && !rows_instead
                && self.step_state(step) == StepState::NeedsAction
                && self.previews.contains_key(&step)
                && self.job(step, JobStage::Plan).is_some()
            {
                buttons.push(button(
                    ids::consent(step),
                    ButtonRole::Confirm,
                    &meta.action_label,
                    true,
                    ButtonKind::Primary,
                ));
            }
        }
        // One retry for the whole screen, however many of its steps stopped. A problem to fix is
        // the screen's question; a wait that re-checks by itself only gets a quiet link.
        let waiting = self.retryable_on(screen);
        if !waiting.is_empty() && !self.pairing_relooks(screen) {
            let id = match waiting.as_slice() {
                [only] => ids::retry(*only),
                _ => ids::RETRY_ALL,
            };
            let failed = waiting
                .iter()
                .any(|step| self.step_state(*step) == StepState::Failed);
            let primary_taken = buttons.iter().any(|b| b.kind == ButtonKind::Primary);
            let (label, kind) = if failed && !primary_taken {
                ("Try again", ButtonKind::Primary)
            } else {
                ("Check again", ButtonKind::Link)
            };
            buttons.push(button(id, ButtonRole::Retry, label, true, kind));
        }
        match screen {
            ScreenId::Welcome => {
                // Closing is the window's own button (or Escape) here.
                buttons.push(button(
                    ids::REMOVE_OR_REPAIR,
                    ButtonRole::Ordinary,
                    "Remove or repair Crosspane…",
                    true,
                    ButtonKind::Link,
                ));
                buttons.push(button(
                    ids::NEXT,
                    ButtonRole::Next,
                    "Start setup",
                    true,
                    ButtonKind::Primary,
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
                            ButtonKind::Choice,
                        ));
                    }
                }
                Some(PairPhase::Waiting) => {}
                _ => {
                    buttons.push(button(
                        ids::PAIR_REJECT,
                        ButtonRole::Cancel,
                        "They don't match",
                        true,
                        ButtonKind::Destructive,
                    ));
                    buttons.push(button(
                        ids::PAIR_CONFIRM,
                        ButtonRole::Confirm,
                        "The numbers match",
                        true,
                        ButtonKind::Primary,
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
                    let on = self.connect.grants.iter().filter(|g| **g).count();
                    if on == CAPABILITIES.len() {
                        buttons.push(button(
                            ids::GRANTS_APPLY,
                            ButtonRole::Confirm,
                            "Allow and continue",
                            true,
                            ButtonKind::Primary,
                        ));
                    } else {
                        buttons.push(button(
                            ids::GRANTS_ALL,
                            ButtonRole::Confirm,
                            "Allow all and continue",
                            true,
                            ButtonKind::Primary,
                        ));
                        if on > 0 {
                            buttons.push(button(
                                ids::GRANTS_APPLY,
                                ButtonRole::Confirm,
                                "Save only the ones turned on",
                                true,
                                ButtonKind::Link,
                            ));
                        }
                    }
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
                        "Apply and restart Crosspane",
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
                if self.satisfied(steps::HIDING) {
                    buttons.push(button(
                        ids::HIDING_CHANGE,
                        ButtonRole::Ordinary,
                        "Change",
                        true,
                        ButtonKind::Link,
                    ));
                }
            }
            ScreenId::Network => {
                // The firewall rule is optional: the other computer may still get through.
                if self
                    .page_steps(screen)
                    .into_iter()
                    .any(|step| self.step_state(step) == StepState::NeedsAction)
                    && self.screen_complete(screen)
                    && self.next_screen().is_some()
                {
                    buttons.push(button(
                        ids::NEXT,
                        ButtonRole::Next,
                        "Not now",
                        !busy,
                        ButtonKind::Link,
                    ));
                }
            }
            ScreenId::Permissions if self.permission_rows_active() => {
                self.permission_buttons(&mut buttons);
            }
            ScreenId::Summary => {
                let ready = self.summary.milestone == Milestone::WorkspaceReady;
                if ready {
                    buttons.push(button(
                        ids::CLOSE,
                        ButtonRole::Cancel,
                        "Done",
                        true,
                        ButtonKind::Primary,
                    ));
                } else if self.first_unfinished_screen().is_some() {
                    buttons.push(button(
                        ids::CONTINUE_SETUP,
                        ButtonRole::Next,
                        "Finish setup",
                        !busy,
                        ButtonKind::Primary,
                    ));
                }
                buttons.push(button(
                    ids::FINAL_CHECK,
                    ButtonRole::Retry,
                    "Check again",
                    self.graph
                        .on_screen(ScreenId::Summary)
                        .all(|m| m.prerequisites.iter().all(|s| self.settled(*s))),
                    ButtonKind::Link,
                ));
                buttons.push(button(
                    ids::REMOVE_OR_REPAIR,
                    ButtonRole::Ordinary,
                    "Remove or repair Crosspane…",
                    !busy,
                    ButtonKind::Link,
                ));
                if !ready {
                    buttons.push(button(
                        ids::CLOSE,
                        ButtonRole::Cancel,
                        "Close",
                        true,
                        ButtonKind::Link,
                    ));
                }
                for (screen, id) in [
                    (ScreenId::Connect, ids::REOPEN_CONNECT),
                    (ScreenId::Layout, ids::REOPEN_ARRANGE),
                ] {
                    if self.skipped_on(screen)
                        || (screen == ScreenId::Layout && self.skipped_on(ScreenId::Grants))
                    {
                        buttons.push(button(
                            id,
                            ButtonRole::Ordinary,
                            "Set up now",
                            !busy,
                            ButtonKind::Link,
                        ));
                    }
                }
            }
            ScreenId::RepairRemove => self.maintenance_controls(&mut buttons, &mut fields),
            _ => {}
        }
        if matches!(screen, ScreenId::Connect | ScreenId::Layout) {
            buttons.push(button(
                ids::SKIP,
                ButtonRole::Ordinary,
                "Skip for now",
                self.pending_skip.is_none() && !self.native_change_running(),
                ButtonKind::Link,
            ));
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
                    ButtonKind::Link,
                ));
            }
            // A finished screen moves on by itself. Continue is offered only where it doesn't:
            // after the person came back to it.
            if !self.auto_advance
                && self.screen_complete(screen)
                && !buttons.iter().any(|b| b.id == ids::NEXT)
                && self.next_screen().is_some()
            {
                buttons.push(button(
                    ids::NEXT,
                    ButtonRole::Next,
                    "Continue",
                    !busy,
                    ButtonKind::Primary,
                ));
            }
        }
        (buttons, fields)
    }

    /// The pairing screen hides its retry while setup is about to look again by itself.
    fn pairing_relooks(&self, screen: ScreenId) -> bool {
        screen == ScreenId::Connect && self.connect.relook_at.is_some()
    }

    /// The first screen, in order, that still has a step to finish.
    pub(super) fn first_unfinished_screen(&self) -> Option<ScreenId> {
        super::controller::ORDER.iter().copied().find(|screen| {
            *screen != ScreenId::Summary
                && self
                    .graph
                    .on_screen(*screen)
                    .any(|m| m.kind != StepKind::Final && !self.satisfied(m.id))
        })
    }

    /// Pairing is searched for automatically. The one computer found is the screen's answer;
    /// typing an address, opening this computer's own window and reconnecting are other ways.
    fn connect_controls(&self, buttons: &mut Vec<ButtonView>, fields: &mut Vec<FieldView>) {
        if self.following_auto_window() {
            // Waiting to be found never takes the other ways away.
            buttons.push(button(
                ids::PAIR_MANUAL,
                ButtonRole::Ordinary,
                "Enter its address",
                true,
                ButtonKind::Link,
            ));
            return;
        }
        if self.step_state(steps::PAIR) != StepState::NeedsAction {
            return;
        }
        let address = self.parsed_address().is_some();
        if self.connect.manual {
            fields.push(FieldView::PeerAddress {
                id: ids::PEER_ADDRESS,
                value: self.connect.address.clone(),
                enabled: true,
            });
            buttons.push(button(
                ids::PAIR_JOIN,
                ButtonRole::Ordinary,
                "Join",
                address,
                ButtonKind::Primary,
            ));
            buttons.push(button(
                ids::PAIR_DIAL,
                ButtonRole::Ordinary,
                "Reconnect a computer paired before",
                address,
                ButtonKind::Link,
            ));
            buttons.push(button(
                ids::PAIR_MANUAL,
                ButtonRole::Ordinary,
                "Search automatically instead",
                true,
                ButtonKind::Link,
            ));
            return;
        }
        let connected = self.connected_peers();
        if connected.len() > 1 {
            // Several paired computers are connected: the person says which one this is for.
            for (i, (_, name)) in connected.iter().take(8).enumerate() {
                buttons.push(button(
                    ids::select_peer(i),
                    ButtonRole::Ordinary,
                    &bounded(format!("Use {name}")),
                    true,
                    ButtonKind::Choice,
                ));
            }
        }
        match self.connect.candidates.as_slice() {
            [] => {}
            [only] => buttons.push(button(
                ids::pair_candidate(0),
                ButtonRole::Ordinary,
                &bounded(format!("Pair with {}", only.name)),
                true,
                ButtonKind::Primary,
            )),
            several => {
                for (i, candidate) in several.iter().enumerate() {
                    buttons.push(button(
                        ids::pair_candidate(i),
                        ButtonRole::Ordinary,
                        &bounded(format!("Pair with {}", candidate.name)),
                        true,
                        ButtonKind::Choice,
                    ));
                }
            }
        }
        if self.connect.auto_windows >= super::shared::MAX_AUTO_WINDOWS
            && self.connect.candidates.is_empty()
        {
            buttons.push(button(
                ids::PAIR_SCAN,
                ButtonRole::Ordinary,
                "Look again",
                true,
                ButtonKind::Primary,
            ));
        }
        buttons.push(button(
            ids::PAIR_MANUAL,
            ButtonRole::Ordinary,
            "Enter its address",
            true,
            ButtonKind::Link,
        ));
        if self.connect.candidates.is_empty() {
            buttons.push(button(
                ids::PAIR_LISTEN,
                ButtonRole::Ordinary,
                "Let the other computer find this one",
                true,
                ButtonKind::Link,
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
                ButtonKind::Link,
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
        let layout = (screen == ScreenId::Layout
            && (!self.skipped_on(screen) || self.reopened.contains(&screen)))
        .then(|| {
            let (confirmed, local_node, peer_order) = self.layout_rects();
            LayoutPreview {
                confirmed,
                local_node,
                peer_order,
                busy: self.connect.layout_busy,
            }
        });
        let sas = (screen == ScreenId::MatchNumbers)
            .then(|| self.connect.pairing.as_ref().and_then(|p| p.sas.clone()))
            .flatten()
            .map(bounded);
        let escape = if self.mutation_in_flight() && !self.following_auto_window() {
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
            },
            demo: false,
            link_caption: (screen == ScreenId::Connect
                && (self.step_state(steps::PAIR) == StepState::NeedsAction
                    || self.following_auto_window())
                && !self.connect.manual)
                .then(|| "Other ways to connect".to_owned()),
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
        // Setup's own pairing window doesn't hold the person on this screen.
        self.abandon_auto_window();
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
            // A screen the person went back to stays until they move on.
            self.auto_advance = false;
        }
    }

    /// Returns true when the window should close.
    pub(super) fn button(&mut self, id: u16) -> bool {
        match id {
            ids::SKIP => self.skip_current(),
            ids::SET_UP_NOW => self.reopen(self.screen),
            ids::REOPEN_CONNECT => self.reopen(ScreenId::Connect),
            ids::REOPEN_ARRANGE => self.reopen(ScreenId::Layout),
            ids::NEXT => {
                if self.screen == ScreenId::Welcome {
                    // The go-ahead for the install steps that stay inside this account.
                    self.install_started = true;
                }
                if let Some(next) = self.next_screen() {
                    self.go(next);
                }
            }
            ids::CONTINUE_SETUP => {
                if let Some(screen) = self.first_unfinished_screen() {
                    self.install_started = true;
                    self.go(screen);
                }
            }
            ids::PAIR_MANUAL => {
                self.abandon_auto_window();
                self.connect.manual = !self.connect.manual;
                self.connect.searching_since = None;
            }
            ids::GRANTS_ALL => {
                if self.step_state(steps::GRANTS) == StepState::NeedsAction {
                    self.connect.grants = [true; 5];
                    self.request_apply(steps::GRANTS);
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
            ids::PAIR_SCAN => {
                // Look again: a fresh automatic search, windows included.
                self.connect.auto_windows = 0;
                self.connect.searching_since = None;
                self.scan();
            }
            ids::PAIR_CONFIRM => self.pair_answer(InstallerRequest::PairConfirm { accept: true }),
            ids::PAIR_REJECT => self.pair_answer(InstallerRequest::PairConfirm { accept: false }),
            ids::GRANTS_APPLY => self.request_apply(steps::GRANTS),
            ids::LAYOUT_ACCEPT => self.accept_layout(),
            ids::HIDING_APPLY => self.request_apply(steps::HIDING),
            ids::HIDING_RESTART => self.hiding_restart(),
            ids::HIDING_CHANGE => self.change_hiding(),
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
            .filter(|m| m.kind != StepKind::Final)
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
            2600..=2639 => self.permission_button(id),
            5100..=5190 => self.follow_up(id - 5100, true),
            5200..=5290 => self.follow_up(id - 5200, false),
            _ => {}
        }
    }

    pub(super) fn consent_click(&mut self, step: StepId) {
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
                m.preview = Some(bounded_lines(preview));
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
                        .push((follow_up, bounded(label), bounded_lines(preview)));
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
                r.preview = Some(bounded_lines(preview));
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

#[cfg(test)]
mod copy_tests {
    use super::{plain_check, step_title};
    use crate::view::RowState;

    #[test]
    fn step_titles_name_the_action_so_no_state_contradicts_them() {
        for (outcome, action) in [
            ("This computer can run Crosspane", "Check this computer"),
            ("This Mac can run Crosspane", "Check this Mac"),
            (
                "Crosspane is installed for your account",
                "Install Crosspane",
            ),
            (
                "Crosspane is installed and starts when you sign in",
                "Install Crosspane",
            ),
            (
                "Crosspane is running with its key in the system keyring",
                "Check that Crosspane is running",
            ),
            (
                "Paired and connected to the other computer",
                "Pair with the other computer",
            ),
        ] {
            assert_eq!(step_title(outcome), action);
        }
        // An unknown step keeps the platform's own words.
        assert_eq!(step_title("Something new"), "Something new");
    }

    #[test]
    fn internal_check_reasons_never_reach_the_person() {
        let (state, text) = plain_check(
            RowState::Waiting,
            "Runtime state is active or couldn't be proved safe to recover; it will be retained"
                .into(),
        );
        assert_eq!(state, RowState::Note);
        assert!(
            !text.contains("proved") && !text.contains("retained"),
            "{text}"
        );
        let (state, text) = plain_check(
            RowState::Note,
            "The owned dead runtime will be cleaned under the install lock when you install".into(),
        );
        assert_eq!(state, RowState::Note);
        assert!(!text.contains("lock"), "{text}");
        // Plain failures pass through untouched.
        assert_eq!(
            plain_check(RowState::Failed, "uwsm isn't installed".into()),
            (RowState::Failed, "uwsm isn't installed".to_owned())
        );
    }
}
