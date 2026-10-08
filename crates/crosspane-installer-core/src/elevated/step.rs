//! Consent preview, step results and gates of the elevated setup (WP-W4.1c2 C5). OS-free: the
//! native step reports each administrator step as a `StepResult`, and the gates decide what the
//! install or the removal may do next.
use super::status::{Consent, StatusReport, describe, verified};
use super::{DriverState, ElevatedError, FirewallState, Outcome, Verb, VerbName};

pub const PREVIEW_HEADER: &str = "Windows will ask once for administrator approval to:";
/// Bytes, not characters: the controller truncates previews at 2,400 bytes
/// (`live/controller.rs:203`), so this bound keeps the whole block visible with room to spare.
pub const MAX_PREVIEW_BLOCK: usize = 1_800;
pub const INSTALL_DECLINE_NOTE: &str = "If you decline, Crosspane is still installed without them.";
pub const INSTALL_WITHOUT_SETUP: &str = "Crosspane is installed without them. Until the firewall rule is added, other computers may not reach this PC; without the display driver, Crosspane mirrors windows in place.";
pub const REMOVAL_NOT_STARTED: &str = "Nothing was removed. To remove Crosspane and keep the firewall rule and display driver, clear that choice and try again.";
pub const JOURNAL_LOST: &str = "The record of this administrator step could not be saved, so its result is unknown. It is checked again before the next one runs.";

/// The consent preview of one mutating verb. It is built from `describe`, so the consent it hands
/// out always matches the lines the person was shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ElevatedPlan {
    verb: Verb,
    lines: Vec<String>,
}

impl ElevatedPlan {
    /// `lines` is `describe(&verb)`. A non-mutating verb, or a preview block longer than
    /// `MAX_PREVIEW_BLOCK` bytes (`preview_block().len()`, not characters), is `Consent`. The
    /// controller shows at most 2,400 bytes, so the bound is in bytes to keep every line visible.
    pub fn new(verb: Verb) -> Result<Self, ElevatedError> {
        if !verb.mutates() {
            return Err(ElevatedError::Consent);
        }
        let plan = Self {
            lines: describe(&verb),
            verb,
        };
        if plan.preview_block().len() > MAX_PREVIEW_BLOCK {
            return Err(ElevatedError::Consent);
        }
        Ok(plan)
    }

    pub fn verb(&self) -> &Verb {
        &self.verb
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// `PREVIEW_HEADER`, then each line verbatim, joined by '\n'.
    pub fn preview_block(&self) -> String {
        std::iter::once(PREVIEW_HEADER)
            .chain(self.lines.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The only way native code obtains a `Consent`: `Consent::presented(&verb, &lines)`.
    pub fn consent(&self) -> Result<Consent, ElevatedError> {
        Consent::presented(&self.verb, &self.lines)
    }
}

/// What became of one part of an administrator step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartResult {
    Verified,
    RebootRequired,
    Declined,
    NotRun,
    Refused,
    Failed,
    Unknown,
}

/// None → Unknown; RebootRequired → RebootRequired; succeeded && verified(part, after) → Verified;
/// Done|AlreadyDone unverified → Unknown; Refused|NotElevated|Mismatch → Refused; Failed → Failed.
pub fn classify(part: &Verb, outcome: Option<Outcome>, after: Option<&StatusReport>) -> PartResult {
    let verified_after = after.is_some_and(|report| verified(part, report));
    match outcome {
        None => PartResult::Unknown,
        Some(Outcome::RebootRequired) => PartResult::RebootRequired,
        Some(outcome) if outcome.succeeded() && verified_after => PartResult::Verified,
        Some(Outcome::Done | Outcome::AlreadyDone) => PartResult::Unknown,
        Some(Outcome::Refused | Outcome::NotElevated | Outcome::Mismatch) => PartResult::Refused,
        Some(Outcome::Failed) => PartResult::Failed,
    }
}

/// The result of one administrator step: one entry per part, in run order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepResult {
    pub parts: Vec<(VerbName, PartResult)>,
    pub not_run: Option<&'static str>,
    pub journal_failed: bool,
}

impl StepResult {
    /// Every part `NotRun`, because `reason` stopped the step before anything ran.
    pub fn not_run(verb: &Verb, reason: &'static str) -> Self {
        Self {
            parts: uniform(verb, PartResult::NotRun),
            not_run: Some(reason),
            journal_failed: false,
        }
    }

    /// Every part `Declined`: the person refused the administrator prompt.
    pub fn declined(verb: &Verb) -> Self {
        Self {
            parts: uniform(verb, PartResult::Declined),
            not_run: None,
            journal_failed: false,
        }
    }

    /// Every part `Unknown`, and the journal flagged: the record of the step could not be kept.
    pub fn journal_failure(verb: &Verb) -> Self {
        Self {
            parts: uniform(verb, PartResult::Unknown),
            not_run: None,
            journal_failed: true,
        }
    }

    /// One entry per `verb.parts()`, in order. A part missing from `outcomes` is `Unknown`.
    pub fn launched(
        verb: &Verb,
        outcomes: &[(Verb, Option<Outcome>)],
        after: Option<&StatusReport>,
    ) -> Self {
        let parts = verb
            .parts()
            .into_iter()
            .map(|part| {
                let outcome = outcomes
                    .iter()
                    .find(|(ran, _)| *ran == part)
                    .and_then(|(_, outcome)| *outcome);
                (part.name(), classify(&part, outcome, after))
            })
            .collect();
        Self {
            parts,
            not_run: None,
            journal_failed: false,
        }
    }

    /// Every part `Verified`. An empty result is not verified.
    pub fn all_verified(&self) -> bool {
        all_parts(self, |part| part == PartResult::Verified)
    }
}

fn uniform(verb: &Verb, result: PartResult) -> Vec<(VerbName, PartResult)> {
    verb.parts()
        .iter()
        .map(|part| (part.name(), result))
        .collect()
}

/// True only when there is at least one part and every part satisfies `accept`.
fn all_parts(result: &StepResult, accept: impl Fn(PartResult) -> bool) -> bool {
    !result.parts.is_empty() && result.parts.iter().all(|(_, part)| accept(*part))
}

/// Whether the next step may run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    Proceed,
    NotSubmitted,
    Unknown,
}

/// journal_failed → Unknown; all parts Verified|RebootRequired → Proceed; all parts
/// Declined|NotRun|Refused → NotSubmitted; otherwise Unknown. An empty result is Unknown.
pub fn removal_gate(result: &StepResult) -> Gate {
    if result.journal_failed {
        Gate::Unknown
    } else if all_parts(result, |part| {
        matches!(part, PartResult::Verified | PartResult::RebootRequired)
    }) {
        Gate::Proceed
    } else if all_parts(result, |part| {
        matches!(
            part,
            PartResult::Declined | PartResult::NotRun | PartResult::Refused
        )
    }) {
        Gate::NotSubmitted
    } else {
        Gate::Unknown
    }
}

/// journal_failed or any Unknown → Unknown; all Declined|NotRun|Refused → NotSubmitted; else
/// Proceed. An empty result is Unknown.
pub fn admin_gate(result: &StepResult) -> Gate {
    if result.journal_failed
        || result.parts.is_empty()
        || result
            .parts
            .iter()
            .any(|(_, part)| *part == PartResult::Unknown)
    {
        Gate::Unknown
    } else if all_parts(result, |part| {
        matches!(
            part,
            PartResult::Declined | PartResult::NotRun | PartResult::Refused
        )
    }) {
        Gate::NotSubmitted
    } else {
        Gate::Proceed
    }
}

/// One `"<subject>: <state>."` line per part, then `JOURNAL_LOST` when `journal_failed`.
pub fn report_lines(result: &StepResult) -> Vec<String> {
    let mut lines: Vec<String> = result
        .parts
        .iter()
        .map(|(name, part)| {
            format!(
                "{}: {}.",
                subject(*name),
                state(*name, *part, result.not_run)
            )
        })
        .collect();
    if result.journal_failed {
        lines.push(JOURNAL_LOST.to_owned());
    }
    lines
}

/// The subject of a part. Only the four leaf verbs are ever parts; the others (never produced by
/// `parts()`) name themselves.
fn subject(name: VerbName) -> &'static str {
    match name {
        VerbName::AddFirewall | VerbName::RemoveFirewall => "Windows Defender Firewall rule",
        VerbName::InstallDriver | VerbName::RemoveDriver => "Crosspane display driver",
        VerbName::Status | VerbName::Setup | VerbName::Teardown => name.as_str(),
    }
}

fn state(name: VerbName, part: PartResult, not_run: Option<&'static str>) -> String {
    match part {
        PartResult::Verified => match name {
            VerbName::AddFirewall => "added".to_owned(),
            VerbName::InstallDriver => "installed".to_owned(),
            VerbName::RemoveFirewall | VerbName::RemoveDriver => "removed".to_owned(),
            VerbName::Status | VerbName::Setup | VerbName::Teardown => "verified".to_owned(),
        },
        PartResult::RebootRequired => "Windows needs a restart to finish".to_owned(),
        PartResult::Declined => "unchanged, because administrator approval was declined".to_owned(),
        PartResult::NotRun => match not_run {
            Some(reason) => format!("unchanged ({reason})"),
            None => "unchanged".to_owned(),
        },
        PartResult::Refused => {
            "unchanged; Windows or an existing conflicting object refused the change".to_owned()
        }
        PartResult::Failed => {
            "could not be changed; anything this step created was undone".to_owned()
        }
        PartResult::Unknown => {
            "result unknown; it is checked again before the next administrator step".to_owned()
        }
    }
}

/// `(configured, "Firewall rule: <fw>. Display driver: <drv>.[ pending note]")`. `configured` is the
/// rule of `verified` for `Setup`: the firewall rule is present and the driver installed.
pub fn status_summary(report: &StatusReport, pending: bool) -> (bool, String) {
    let configured = report.firewall.state == FirewallState::Present
        && report.driver.state == DriverState::Installed;
    let mut text = format!(
        "Firewall rule: {}. Display driver: {}.",
        firewall_words(report.firewall.state),
        driver_words(report.driver.state)
    );
    if pending {
        text.push_str(
            " An earlier administrator step was interrupted; it is checked again before the next one runs.",
        );
    }
    (configured, text)
}

fn firewall_words(state: FirewallState) -> &'static str {
    match state {
        FirewallState::Present => "present",
        FirewallState::Missing => "missing",
        FirewallState::Mismatch => "different from Crosspane's rule",
        FirewallState::Unavailable => "unreadable",
        FirewallState::NotRequested => "not checked",
    }
}

fn driver_words(state: DriverState) -> &'static str {
    match state {
        DriverState::Absent => "not installed",
        DriverState::PackageOnly | DriverState::DeviceWithoutDriver => "partly installed",
        DriverState::Installed => "installed",
        DriverState::Mismatch => "different from Crosspane's driver",
        DriverState::Unavailable => "unreadable",
    }
}
