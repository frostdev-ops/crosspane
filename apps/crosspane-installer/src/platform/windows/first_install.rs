//! First-install observations never provide the old-tree authority required by an upgrade.
#[path = "first_install/driver.rs"]
pub(crate) mod driver;
#[path = "first_install/record.rs"]
pub(crate) mod record;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Presence {
    Missing,
    Present,
    AccessDenied,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum History {
    None,
    FirstInstall,
    CompletedRemoval,
    Other,
    Partial,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FirstInstallFacts {
    pub task: Presence,
    pub agent: Presence,
    pub supervisor: Presence,
    pub history: History,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FirstInstallDisposition {
    Eligible,
    Resume,
    Existing,
    CompletedRemoval,
    AccessDenied,
    Partial,
    Unknown,
}
pub(crate) const REMOVAL_REINSTALL: &str = "reinstall after removal: WP-W4.1a8b";
/// Preview grants no authority. The native reservation renews every absence under the real lock.
pub(crate) fn preview(facts: FirstInstallFacts) -> FirstInstallDisposition {
    use FirstInstallDisposition as D;
    use Presence as P;
    if facts.history == History::CompletedRemoval {
        return D::CompletedRemoval;
    }
    let observations = [facts.task, facts.agent, facts.supervisor];
    if observations.contains(&P::AccessDenied) {
        return D::AccessDenied;
    }
    if observations.contains(&P::Unknown) || facts.history == History::Unknown {
        return D::Unknown;
    }
    if facts.history == History::FirstInstall {
        return D::Resume;
    }
    if observations.contains(&P::Present) {
        return D::Existing;
    }
    match facts.history {
        History::None => D::Eligible,
        History::Partial | History::Other => D::Partial,
        _ => D::Unknown,
    }
}

/// Pure decoded-record routing only. No history field grants native authority.
pub(crate) struct DecodedHistory<'a> {
    pub removal: Option<super::removal::RemovalCursor>,
    pub first: Option<record::Phase>,
    pub journal: Option<super::service::journal::Phase>,
    pub logon: bool,
    pub claimed_activation: bool,
    pub names: &'a [String],
}
pub(crate) fn classify_history(history: DecodedHistory<'_>) -> FirstInstallDisposition {
    use super::removal::RemovalCursor as R;
    use FirstInstallDisposition as D;
    use crosspane_installer_core::elevated::journal;
    // The elevated-setup record is never deleted, so it must never change the branch taken.
    let names: Vec<&str> = history
        .names
        .iter()
        .map(String::as_str)
        .filter(|name| *name != journal::RECORD_LEAF)
        .collect();
    if let Some(removal) = history.removal {
        return if matches!(
            removal,
            R::Complete { .. }
                | R::FinalCopyCleanupIntent { .. }
                | R::FinalCopyAbsent { .. }
                | R::Retired
        ) {
            D::CompletedRemoval
        } else {
            D::Partial
        };
    }
    if let Some(first) = history.first {
        return match first {
            record::Phase::Complete => D::Existing,
            record::Phase::Unknown => D::Unknown,
            record::Phase::Intent
                if names
                    .iter()
                    .all(|n| matches!(*n, "install.lock" | "first-install.json")) =>
            {
                D::Resume
            }
            phase
                if phase.rank() >= record::Phase::RunIntent.rank()
                    && names.iter().all(|n| {
                        matches!(
                            *n,
                            "install.lock"
                                | "first-install.json"
                                | "supervisor.json"
                                | "supervisor-logon.json"
                                | "task-activation.json"
                                | "supervisor-epoch-0.json"
                                | "supervisor-epoch-1.json"
                                | "supervisor-epoch-2.json"
                        )
                    }) =>
            {
                D::Resume
            }
            _ => D::Partial,
        };
    }
    if history.journal == Some(super::service::journal::Phase::Finished)
        || history.logon
        || history.claimed_activation
        || names.iter().any(|n| {
            matches!(
                *n,
                "supervisor-epoch-0.json" | "supervisor-epoch-1.json" | "supervisor-epoch-2.json"
            )
        })
    {
        return D::CompletedRemoval;
    }
    if names.iter().any(|n| *n != "install.lock") {
        D::Partial
    } else {
        D::Eligible
    }
}
/// Production begin uses this gate before crossing the lock-foundation boundary.
pub(crate) fn before_foundation<T>(
    disposition: FirstInstallDisposition,
    admitted: impl FnOnce() -> super::native_io::NativeResult<T>,
) -> super::native_io::NativeResult<Result<T, &'static str>> {
    use FirstInstallDisposition as D;
    match disposition {
        D::Eligible | D::Resume => admitted().map(Ok),
        D::CompletedRemoval => Ok(Err(REMOVAL_REINSTALL)),
        D::AccessDenied => Ok(Err("first install access denied; permissions unchanged")),
        D::Existing => Ok(Err("an existing installation requires an update")),
        D::Partial | D::Unknown => Err(super::native_io::NativeError::OutcomeUnknown),
    }
}
#[derive(Debug)]
pub(crate) enum Reopen {
    Restart(record::FirstInstallRecord),
    Observe(record::FirstInstallRecord),
}
/// Only same-user lineage is accepted. This is not an absence, process or launch capability.
pub(crate) fn reopen(
    record: &record::FirstInstallRecord,
    current: &super::native_io::identity::TokenFacts,
) -> super::native_io::NativeResult<Reopen> {
    use super::{native_io::NativeError, payload::recovery::OuterContextCorrelation};
    let old: OuterContextCorrelation =
        serde_json::from_slice(record.context()).map_err(|_| NativeError::Invalid)?;
    old.same_user(current)?;
    record.validate()?;
    match record.phase() {
        record::Phase::Intent => Ok(Reopen::Restart(
            record.restart_intent(
                serde_json::to_vec(&OuterContextCorrelation::new(current)?)
                    .map_err(|_| NativeError::Invalid)?,
            )?,
        )),
        phase
            if phase.rank() >= record::Phase::RunIntent.rank()
                && phase.rank() < record::Phase::Complete.rank() =>
        {
            Ok(Reopen::Observe(record.clone()))
        }
        _ => Err(NativeError::OutcomeUnknown),
    }
}
/// Retry only positively Busy calls, within the original deadline; no unknown call is replayed.
pub(crate) fn retry_busy<T>(
    deadline: &super::native_io::Deadline,
    mut attempt: impl FnMut() -> super::native_io::NativeResult<T>,
    mut wait: impl FnMut(u64),
) -> super::native_io::NativeResult<T> {
    use super::native_io::NativeError;
    loop {
        deadline.check()?;
        match attempt() {
            Err(NativeError::Busy) => wait(deadline.remaining_ms()?.min(20)),
            result => {
                deadline.check()?;
                return result;
            }
        }
    }
}

/// Routing of decoded observations only; all native absence is independently renewed.
pub(crate) fn recovery_mode_allowed(
    source: &record::FirstInstallRecord,
    mode: record::FirstRecoveryMode,
) -> super::native_io::NativeResult<()> {
    use record::{FirstRecoveryMode as M, Phase as P};
    source.validate()?;
    match mode {
        M::Rollback | M::Remove if source.phase().rank() < P::RunIntent.rank() => Ok(()),
        M::Supersede | M::RetireStale
            if source.phase().rank() >= P::RunIntent.rank()
                && source.phase().rank() < P::Complete.rank()
                && source.phase() != P::Unknown =>
        {
            Ok(())
        }
        _ => Err(super::native_io::NativeError::OutcomeUnknown),
    }
}
