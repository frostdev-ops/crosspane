//! Status report, consent and journal model of the elevated helper (WP-W4.1c). OS-free: the
//! helper prints a `StatusReport`, the launcher checks it with `verified`, and the journal records
//! each mutation before it runs and after it is verified.
use super::{
    DriverState, ElevatedError, FirewallState, HARDWARE_ID, Outcome, Verb, VerbName,
    is_published_name,
};
use crate::{MutationOutcome, ResourceObservation, ResourceOwnership, ResourceReceipt};

pub const STATUS_SCHEMA: u32 = 1;
pub const MAX_STATUS_BYTES: usize = 64 * 1024;
pub const MAX_ITEMS: usize = 16;
pub const MAX_INSTANCE_ID: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceStatus {
    pub instance_id: String,
    pub driver_inf: Option<String>,
    pub problem: Option<u32>,
    pub present: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriverStatus {
    pub state: DriverState,
    pub packages: Vec<String>,
    pub devices: Vec<DeviceStatus>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirewallStatus {
    pub state: FirewallState,
    pub family_count: u32,
    pub local_rules_apply: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusReport {
    pub schema: u32,
    pub elevated: bool,
    pub driver: DriverStatus,
    pub firewall: FirewallStatus,
}

impl StatusReport {
    /// Checks the schema and the bounds of a report read from the helper. A report that fails is
    /// `Report`.
    pub fn validate(&self) -> Result<(), ElevatedError> {
        let valid = self.schema == STATUS_SCHEMA
            && self.driver.packages.len() <= MAX_ITEMS
            && self.driver.devices.len() <= MAX_ITEMS
            && self
                .driver
                .packages
                .iter()
                .all(|name| is_published_name(name))
            && self.driver.devices.iter().all(|device| {
                device.driver_inf.as_deref().is_none_or(is_published_name)
                    && (1..=MAX_INSTANCE_ID).contains(&device.instance_id.chars().count())
            });
        if valid {
            Ok(())
        } else {
            Err(ElevatedError::Report)
        }
    }
}

/// Whether `report` shows the state that `verb` is meant to reach. `Status` only reports, so it is
/// always verified.
pub fn verified(verb: &Verb, report: &StatusReport) -> bool {
    match verb {
        Verb::Status(_) => true,
        Verb::InstallDriver => report.driver.state == DriverState::Installed,
        Verb::RemoveDriver => report.driver.state == DriverState::Absent,
        Verb::AddFirewall(_) => report.firewall.state == FirewallState::Present,
        Verb::RemoveFirewall(_) => report.firewall.state == FirewallState::Missing,
        Verb::Setup(_) => {
            report.firewall.state == FirewallState::Present
                && report.driver.state == DriverState::Installed
        }
        Verb::Teardown(_) => {
            report.firewall.state == FirewallState::Missing
                && report.driver.state == DriverState::Absent
        }
    }
}

/// The consent lines shown to the user for `verb`, in the frozen wording. `Status` has none. A
/// combined verb shows the lines of its parts in run order.
pub fn describe(verb: &Verb) -> Vec<String> {
    match verb {
        Verb::Status(_) => Vec::new(),
        Verb::Setup(_) | Verb::Teardown(_) => {
            verb.parts().iter().flat_map(describe).collect()
        }
        Verb::InstallDriver => vec![
            "Add the Crosspane display driver (CrosspaneIdd.inf, publisher Crosspane) to the Windows driver store.".to_owned(),
            "Create one Crosspane virtual display adapter (hardware ID Crosspane\\IddTwinV1). It shows no display until Crosspane needs one.".to_owned(),
        ],
        Verb::RemoveDriver => vec![
            "Remove the Crosspane virtual display adapter (hardware ID Crosspane\\IddTwinV1).".to_owned(),
            "Remove the Crosspane display driver from the Windows driver store.".to_owned(),
        ],
        Verb::AddFirewall(scope) => vec![format!(
            "Allow {} to receive UDP traffic from your local subnet on private networks (Windows Defender Firewall rule \"{}\").",
            scope.program.as_str(),
            scope.id.rule_name()
        )],
        Verb::RemoveFirewall(scope) => vec![format!(
            "Remove the Windows Defender Firewall rule \"{}\".",
            scope.id.rule_name()
        )],
    }
}

/// Consent to one mutating verb. It can only be made from the exact lines `describe` returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Consent {
    verb: Verb,
}

impl Consent {
    /// Accepts `shown` only when `verb` mutates and `shown` equals `describe(verb)` line for line.
    pub fn presented(verb: &Verb, shown: &[String]) -> Result<Self, ElevatedError> {
        if verb.mutates() && shown == describe(verb).as_slice() {
            Ok(Self { verb: verb.clone() })
        } else {
            Err(ElevatedError::Consent)
        }
    }

    pub fn verb(&self) -> VerbName {
        self.verb.name()
    }

    /// The exact verb consented to, scope included.
    pub fn action(&self) -> &Verb {
        &self.verb
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum JournalPhase {
    Intent,
    Outcome,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalEntry {
    pub phase: JournalPhase,
    pub verb: VerbName,
    pub exit: Option<i32>,
    pub receipt: ResourceReceipt,
}

/// Durable record of the helper's own steps. A failed `record` stops the caller.
pub trait ElevatedJournal {
    fn record(&mut self, entry: &JournalEntry) -> Result<(), ElevatedError>;
}

/// The Intent record, written before the verb runs. `before` is the report observed beforehand.
pub fn intent_entry(verb: &Verb, before: Option<&StatusReport>) -> JournalEntry {
    JournalEntry {
        phase: JournalPhase::Intent,
        verb: verb.name(),
        exit: None,
        receipt: receipt(
            verb,
            before.map_or(ResourceObservation::Unknown, |report| observe(verb, report)),
            ResourceObservation::Unknown,
            MutationOutcome::Unknown,
        ),
    }
}

/// The Outcome record, written after the verb ran. `after` is the report observed afterwards.
pub fn outcome_entry(
    verb: &Verb,
    outcome: Option<Outcome>,
    after: Option<&StatusReport>,
) -> JournalEntry {
    JournalEntry {
        phase: JournalPhase::Outcome,
        verb: verb.name(),
        exit: outcome.map(Outcome::exit_code),
        receipt: receipt(
            verb,
            ResourceObservation::Unknown,
            after.map_or(ResourceObservation::Unknown, |report| observe(verb, report)),
            mutation_outcome(verb, outcome, after),
        ),
    }
}

fn receipt(
    verb: &Verb,
    before: ResourceObservation,
    after: ResourceObservation,
    outcome: MutationOutcome,
) -> ResourceReceipt {
    ResourceReceipt {
        resource_id: resource_id(verb).to_owned(),
        resolved_path: resolved_path(verb),
        ownership: ResourceOwnership::Created,
        before,
        after,
        outcome,
    }
}

fn resource_id(verb: &Verb) -> &'static str {
    match verb {
        Verb::Status(_) => "windows-elevated-status",
        Verb::InstallDriver | Verb::RemoveDriver => "windows-idd-driver",
        Verb::AddFirewall(_) | Verb::RemoveFirewall(_) => "windows-firewall-rule",
        Verb::Setup(_) | Verb::Teardown(_) => "windows-elevated-setup",
    }
}

fn resolved_path(verb: &Verb) -> String {
    match verb {
        Verb::AddFirewall(scope)
        | Verb::RemoveFirewall(scope)
        | Verb::Setup(scope)
        | Verb::Teardown(scope) => scope.id.rule_name(),
        Verb::Status(_) | Verb::InstallDriver | Verb::RemoveDriver => HARDWARE_ID.to_owned(),
    }
}

/// Firewall verbs observe the firewall state; every other single verb observes the driver state.
/// Combined verbs are never journaled, so they observe nothing.
fn observe(verb: &Verb, report: &StatusReport) -> ResourceObservation {
    match verb {
        Verb::AddFirewall(_) | Verb::RemoveFirewall(_) => {
            firewall_observation(report.firewall.state)
        }
        Verb::Status(_) | Verb::InstallDriver | Verb::RemoveDriver => {
            driver_observation(report.driver.state)
        }
        Verb::Setup(_) | Verb::Teardown(_) => ResourceObservation::Unknown,
    }
}

fn driver_observation(state: DriverState) -> ResourceObservation {
    match state {
        DriverState::Absent => ResourceObservation::Absent,
        DriverState::Installed => ResourceObservation::Matching,
        DriverState::PackageOnly | DriverState::DeviceWithoutDriver | DriverState::Mismatch => {
            ResourceObservation::Different
        }
        DriverState::Unavailable => ResourceObservation::Unknown,
    }
}

fn firewall_observation(state: FirewallState) -> ResourceObservation {
    match state {
        FirewallState::Missing => ResourceObservation::Absent,
        FirewallState::Present => ResourceObservation::Matching,
        FirewallState::Mismatch => ResourceObservation::Different,
        FirewallState::Unavailable | FirewallState::NotRequested => ResourceObservation::Unknown,
    }
}

fn mutation_outcome(
    verb: &Verb,
    outcome: Option<Outcome>,
    after: Option<&StatusReport>,
) -> MutationOutcome {
    let verified_after = after.is_some_and(|report| verified(verb, report));
    match outcome {
        Some(outcome) if outcome.succeeded() && verified_after => MutationOutcome::Verified,
        Some(Outcome::Refused | Outcome::NotElevated | Outcome::Mismatch) => {
            MutationOutcome::Refused
        }
        Some(Outcome::Failed) => MutationOutcome::Failed,
        _ => MutationOutcome::Unknown,
    }
}
