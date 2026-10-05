//! The macOS permissions screen (WP-4.33): one row per permission Crosspane still needs, each
//! with one "Allow" button. A click sends the agent `AskPermission` for that permission only: the
//! grants belong to the agent's identity, so the agent asks, and it shows the macOS request or
//! opens System Settings at the right pane. Status polling turns each row into a check as macOS
//! reports the grant, and the screen moves on by itself once all are granted.
//!
//! When the person says a permission is on but macOS still reports it off after a few seconds,
//! the row offers to restart Crosspane (a running process can hold a stale answer) or to reset
//! Crosspane's own entry and ask again. Both happen only on that explicit click.

use std::collections::BTreeMap;

use crosspane_installer_core::{StepId, StepState};

use super::super::AgentApply;
use super::super::controller::{LiveController, OwnCall, bounded};
use super::super::shared::failure_text;
use crate::agent_contract::{
    AgentPlatform, AgentReply, InstallerRequest, PermissionName, PermissionState,
};
use crate::screens::{PERMISSION_BUTTON_IDS, PERMISSION_ROW_IDS};
use crate::view::{ButtonKind, ButtonRole, ButtonView, RowState, RowView, ScreenId};

/// How long after the person says a permission is on before setup offers a restart or a reset.
pub(in crate::live) const ON_GRACE_MS: u64 = 5_000;
/// The permissions step is checked again at most this often once every permission is granted.
const BEGIN_EVERY_MS: u64 = 2_000;

/// The rows, in the order they are asked in: Input Monitoring only after Accessibility.
const ORDER: [PermissionName; 4] = [
    PermissionName::Accessibility,
    PermissionName::InputMonitoring,
    PermissionName::ScreenRecording,
    PermissionName::Microphone,
];

/// A row's actions; their ids are `PERMISSION_BUTTON_IDS.start + 10 * row + action`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Allow = 0,
    ItsOn = 1,
    Reset = 2,
    Restart = 3,
}

const ACTIONS: [Action; 4] = [Action::Allow, Action::ItsOn, Action::Reset, Action::Restart];

fn button_id(index: usize, action: Action) -> u16 {
    PERMISSION_BUTTON_IDS.start + (index as u16) * 10 + action as u16
}

fn decode(id: u16) -> Option<(PermissionName, Action)> {
    if !PERMISSION_BUTTON_IDS.contains(&id) {
        return None;
    }
    let offset = id - PERMISSION_BUTTON_IDS.start;
    let name = ORDER.get(usize::from(offset / 10))?;
    let action = ACTIONS.get(usize::from(offset % 10))?;
    Some((*name, *action))
}

fn index_of(name: PermissionName) -> usize {
    ORDER.iter().position(|n| *n == name).unwrap_or(0)
}

/// The System Settings pane `name` is turned on in, by macOS version: macOS 27 renamed the
/// Accessibility pane "Device Control and Data Access".
pub(in crate::live) fn pane_name(name: PermissionName, macos_major: Option<u64>) -> &'static str {
    match name {
        PermissionName::Accessibility if macos_major.is_some_and(|m| m >= 27) => {
            "Device Control and Data Access"
        }
        PermissionName::Accessibility => "Accessibility",
        PermissionName::InputMonitoring => "Input Monitoring",
        PermissionName::ScreenRecording => "Screen & System Audio Recording",
        PermissionName::Microphone => "Microphone",
    }
}

fn reason(name: PermissionName) -> &'static str {
    match name {
        PermissionName::Accessibility => {
            "Lets the other computer type and click on this Mac, and move windows you send."
        }
        PermissionName::InputMonitoring => {
            "Lets this Mac's keyboard and mouse move over to the other computer."
        }
        PermissionName::ScreenRecording => "Lets Crosspane show this Mac's windows elsewhere.",
        PermissionName::Microphone => {
            "Lets Crosspane hear the Crosspane speakers device; your real microphone is never \
             opened."
        }
    }
}

#[cfg(target_os = "macos")]
fn macos_major() -> Option<u64> {
    crate::platform::macos::permissions::running_macos_major()
}

#[cfg(not(target_os = "macos"))]
fn macos_major() -> Option<u64> {
    None
}

/// What one in-flight call is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Call {
    Ask,
    Reset,
    Restart,
}

/// One row's history on this screen.
#[derive(Clone, Copy, Debug, Default)]
struct Row {
    /// The agent took an ask for it (the macOS request or its pane was shown).
    asked_at: Option<u64>,
    /// The person said it's on.
    on_since: Option<u64>,
    /// The last call for it failed, in words.
    failed: Option<&'static str>,
}

/// The permission screen's own state: per-row history and the calls in flight.
#[derive(Debug, Default)]
pub(in crate::live) struct PermissionAsks {
    rows: BTreeMap<PermissionName, Row>,
    calls: BTreeMap<u64, (PermissionName, Call)>,
}

impl LiveController {
    /// The step the agent completes by asking: the native step on the permissions screen.
    fn permissions_step(&self) -> Option<StepId> {
        self.graph
            .metas
            .iter()
            .find(|m| {
                m.screen == ScreenId::Permissions
                    && m.agent_apply == Some(AgentApply::AskPermissions)
            })
            .map(|m| m.id)
    }

    /// One row per permission applies on a Mac while its permissions step isn't done yet.
    pub(in crate::live) fn permission_rows_active(&self) -> bool {
        self.desc.platform == AgentPlatform::Macos
            && self.permissions_step().is_some_and(|s| !self.satisfied(s))
    }

    /// Each permission Crosspane needs on this Mac and what the agent reports for it, from the
    /// latest Status. Microphone only while audio is on.
    fn required_permissions(&self) -> Option<Vec<(PermissionName, PermissionState)>> {
        let health = self.health.as_ref()?;
        let installer = health.snapshot.installer();
        Some(
            ORDER
                .into_iter()
                .filter(|name| *name != PermissionName::Microphone || installer.audio.enabled)
                .map(|name| {
                    let state = installer
                        .permissions
                        .iter()
                        .find(|fact| fact.name == name)
                        .map_or(PermissionState::Unknown, |fact| fact.state);
                    (name, state)
                })
                .collect(),
        )
    }

    fn all_granted(&self) -> bool {
        self.required_permissions().is_some_and(|rows| {
            rows.iter()
                .all(|(_, state)| *state == PermissionState::Granted)
        })
    }

    fn permission_call_in_flight(&self) -> bool {
        !self.permission_asks.calls.is_empty()
    }

    fn row_call(&self, name: PermissionName) -> Option<Call> {
        self.permission_asks
            .calls
            .values()
            .find(|(n, _)| *n == name)
            .map(|(_, call)| *call)
    }

    /// Input Monitoring is asked for only once Accessibility is on.
    fn waits_for_accessibility(
        &self,
        name: PermissionName,
        rows: &[(PermissionName, PermissionState)],
    ) -> bool {
        name == PermissionName::InputMonitoring
            && rows.iter().any(|(n, state)| {
                *n == PermissionName::Accessibility && *state != PermissionState::Granted
            })
    }

    /// The person said it's on, macOS still says off, and the grace period is over.
    fn still_off(&self, row: &Row) -> bool {
        row.on_since
            .is_some_and(|since| self.now >= since.saturating_add(ON_GRACE_MS))
    }

    /// The step's own row on this screen: a short count instead of the long plan preview.
    pub(in crate::live) fn permission_step_rows(&self, step: StepId) -> Vec<RowView> {
        let mut row = self.row(step);
        if self.step_state(step) == StepState::NeedsAction
            && let Some(rows) = self.required_permissions()
        {
            let granted = rows
                .iter()
                .filter(|(_, s)| *s == PermissionState::Granted)
                .count();
            row.detail = bounded(format!(
                "{granted} of {} allowed. Allow each one below.",
                rows.len()
            ));
        }
        let mut rows = vec![row];
        rows.extend(self.permission_rows());
        rows
    }

    /// One row per permission: a check once granted, else what to do next.
    pub(in crate::live) fn permission_rows(&self) -> Vec<RowView> {
        let major = macos_major();
        let Some(required) = self.required_permissions() else {
            return Vec::new();
        };
        required
            .iter()
            .map(|(name, state)| {
                let row = self
                    .permission_asks
                    .rows
                    .get(name)
                    .copied()
                    .unwrap_or_default();
                let place = format!(
                    "System Settings > Privacy & Security > {}",
                    pane_name(*name, major)
                );
                let (state, detail) = match state {
                    PermissionState::Granted => (RowState::Verified, "Allowed.".to_owned()),
                    PermissionState::Unknown => (
                        RowState::Waiting,
                        "Crosspane can't read this permission right now.".to_owned(),
                    ),
                    PermissionState::NotGranted => match self.row_call(*name) {
                        Some(Call::Ask) => (RowState::Working, "Asking macOS…".to_owned()),
                        Some(Call::Reset) => (
                            RowState::Working,
                            "Resetting Crosspane's entry, then asking again…".to_owned(),
                        ),
                        Some(Call::Restart) => {
                            (RowState::Working, "Restarting Crosspane…".to_owned())
                        }
                        None if row.failed.is_some() => {
                            (RowState::Failed, row.failed.unwrap_or_default().to_owned())
                        }
                        None if self.waits_for_accessibility(*name, &required) => (
                            RowState::Waiting,
                            format!(
                                "Allow {} first.",
                                pane_name(PermissionName::Accessibility, major)
                            ),
                        ),
                        None if self.still_off(&row) => (
                            RowState::Waiting,
                            "macOS still reports this off for Crosspane. Restart Crosspane so it \
                             reads it again, or reset Crosspane's entry and ask again."
                                .to_owned(),
                        ),
                        None if row.on_since.is_some() => (
                            RowState::Working,
                            "Checking that macOS reports it…".to_owned(),
                        ),
                        None if row.asked_at.is_some() => (
                            RowState::Waiting,
                            format!(
                                "Answer the macOS request, or turn Crosspane on in {place}. \
                                 Setup notices it by itself."
                            ),
                        ),
                        None => (RowState::NeedsAction, reason(*name).to_owned()),
                    },
                };
                RowView {
                    id: PERMISSION_ROW_IDS.start + index_of(*name) as u16,
                    label: pane_name(*name, major).to_owned(),
                    detail: bounded(detail),
                    state,
                    human_confirmed: false,
                }
            })
            .collect()
    }

    /// Each missing permission's actions, drawn inside its row.
    pub(in crate::live) fn permission_buttons(&self, buttons: &mut Vec<ButtonView>) {
        let Some(required) = self.required_permissions() else {
            return;
        };
        let busy = self.permission_call_in_flight();
        for (name, state) in &required {
            if *state != PermissionState::NotGranted {
                continue;
            }
            let index = index_of(*name);
            let row = self
                .permission_asks
                .rows
                .get(name)
                .copied()
                .unwrap_or_default();
            let mut push = |action: Action, label: &str, enabled: bool, kind: ButtonKind| {
                buttons.push(ButtonView {
                    id: button_id(index, action),
                    role: ButtonRole::Confirm,
                    label: label.into(),
                    enabled,
                    kind,
                });
            };
            push(
                Action::Allow,
                "Allow",
                !busy && !self.waits_for_accessibility(*name, &required),
                ButtonKind::Choice,
            );
            if row.asked_at.is_some() && row.on_since.is_none() {
                push(Action::ItsOn, "It's on", !busy, ButtonKind::Link);
            }
            if self.still_off(&row) {
                push(
                    Action::Restart,
                    "Restart Crosspane and check again",
                    !busy && self.agent_quiet(),
                    ButtonKind::Link,
                );
                push(
                    Action::Reset,
                    "Reset Crosspane's entry and ask again",
                    !busy,
                    ButtonKind::Link,
                );
            }
        }
    }

    /// The screen's words while it shows one row per permission.
    pub(in crate::live) fn permission_message(&self) -> String {
        if self.required_permissions().is_none() {
            return "Checking which permissions Crosspane has…".into();
        }
        if self.all_granted() {
            return "Crosspane has every permission it needs.".into();
        }
        "Allow each permission below. macOS shows a request, or opens System Settings where you \
         turn Crosspane on. Setup notices each one by itself and moves on once all are allowed. \
         Crosspane may restart by itself to start using a new permission."
            .into()
    }

    /// A click on one of a permission row's actions.
    pub(in crate::live) fn permission_button(&mut self, id: u16) {
        let Some((name, action)) = decode(id) else {
            return;
        };
        if !self.permission_rows_active() || self.permission_call_in_flight() {
            return;
        }
        let Some(required) = self.required_permissions() else {
            return;
        };
        if !required
            .iter()
            .any(|(n, state)| *n == name && *state == PermissionState::NotGranted)
        {
            return;
        }
        let now = self.now;
        match action {
            Action::Allow => {
                if !self.waits_for_accessibility(name, &required) {
                    self.permission_call(name, Call::Ask);
                }
            }
            Action::ItsOn => {
                let row = self.permission_asks.rows.entry(name).or_default();
                if row.asked_at.is_some() {
                    row.on_since = Some(now);
                }
            }
            Action::Reset => {
                let row = self.permission_asks.rows.get(&name).copied();
                if row.is_some_and(|r| self.still_off(&r)) {
                    self.permission_call(name, Call::Reset);
                }
            }
            Action::Restart => {
                let row = self.permission_asks.rows.get(&name).copied();
                if row.is_some_and(|r| self.still_off(&r)) && self.agent_quiet() {
                    self.permission_call(name, Call::Restart);
                }
            }
        }
    }

    fn permission_call(&mut self, name: PermissionName, call: Call) {
        let request = match call {
            Call::Ask => InstallerRequest::AskPermission { permission: name },
            Call::Reset => InstallerRequest::ResetPermission { permission: name },
            Call::Restart => InstallerRequest::Restart,
        };
        let row = self.permission_asks.rows.entry(name).or_default();
        row.failed = None;
        if call != Call::Ask {
            row.on_since = None;
        }
        match self.call(OwnCall::Permission, request) {
            Ok(id) => {
                self.permission_asks.calls.insert(id, (name, call));
            }
            Err(failure) => {
                self.permission_asks.rows.entry(name).or_default().failed =
                    Some(failure_text(&failure));
            }
        }
    }

    /// The agent's answer to an ask, a reset or a restart. An answer is never a grant: only a
    /// later Status shows that.
    pub(in crate::live) fn permission_reply(&mut self, reply: AgentReply) {
        let Some((name, call)) = self.permission_asks.calls.remove(&reply.id) else {
            return;
        };
        let now = self.now;
        match (call, reply.result) {
            (Call::Ask, Ok(_)) => {
                let row = self.permission_asks.rows.entry(name).or_default();
                row.asked_at = Some(now);
                row.on_since = None;
            }
            // The entry is fresh again: ask at once, so its request can show.
            (Call::Reset, Ok(_)) => self.permission_call(name, Call::Ask),
            (Call::Restart, Ok(_)) => {}
            (_, Err(failure)) => {
                self.permission_asks.rows.entry(name).or_default().failed =
                    Some(failure_text(&failure));
            }
        }
    }

    /// Each pass on the permissions screen: forget the history of granted rows, and once all are
    /// granted, check the permissions step again so the screen can move on by itself.
    pub(in crate::live) fn permissions_tick(&mut self) {
        if !self.permission_rows_active() {
            return;
        }
        if let Some(required) = self.required_permissions() {
            for (name, state) in required {
                if state == PermissionState::Granted {
                    self.permission_asks.rows.remove(&name);
                }
            }
        }
        let Some(step) = self.permissions_step() else {
            return;
        };
        if self.screen != ScreenId::Permissions
            || !self.all_granted()
            || !self.prerequisites_valid(step)
            || matches!(
                self.step_state(step),
                StepState::Checking
                    | StepState::Planning
                    | StepState::Running
                    | StepState::Verifying
                    | StepState::Satisfied
            )
            || self
                .auto_begun
                .get(&step)
                .is_some_and(|at| self.now < at.saturating_add(BEGIN_EVERY_MS))
        {
            return;
        }
        self.begin(step);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_ids_round_trip_and_stay_in_their_range() {
        for (index, name) in ORDER.iter().enumerate() {
            for action in ACTIONS {
                let id = button_id(index, action);
                assert!(PERMISSION_BUTTON_IDS.contains(&id));
                assert_eq!(decode(id), Some((*name, action)));
                assert_eq!(
                    crate::screens::permission_button_row(id),
                    Some(PERMISSION_ROW_IDS.start + index as u16)
                );
            }
        }
        assert_eq!(decode(PERMISSION_BUTTON_IDS.end), None);
        assert_eq!(decode(PERMISSION_BUTTON_IDS.start + 4), None);
    }

    #[test]
    fn macos_27_names_the_accessibility_pane_device_control_and_data_access() {
        assert_eq!(
            pane_name(PermissionName::Accessibility, Some(26)),
            "Accessibility"
        );
        assert_eq!(
            pane_name(PermissionName::Accessibility, Some(27)),
            "Device Control and Data Access"
        );
        assert_eq!(
            pane_name(PermissionName::InputMonitoring, Some(27)),
            "Input Monitoring"
        );
        assert_eq!(
            pane_name(PermissionName::Accessibility, None),
            "Accessibility"
        );
    }
}
