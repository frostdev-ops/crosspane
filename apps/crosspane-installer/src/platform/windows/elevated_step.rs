//! Elevated setup step runner (WP-W4.1c2 N3). It ties the consent, the kit, the journal store and
//! the unelevated launcher together for one administrator step. Nothing here elevates: the only
//! elevated run is `ElevatedHelper::run`, and every other read is observation only.

use super::{
    elevated_kit::{self, KitHold, KitSources},
    elevated_launch::{ElevatedHelper, LaunchOutcome},
    elevated_store::{LockSource, StoreJournal},
    first_install,
    native_io::{
        Cancellation, Deadline, MonotonicClock, NativeError, NativeResult, WindowsNativeIo,
    },
};
use crosspane_installer_core::elevated::{
    RuleScope, Verb,
    kit::KitManifest,
    status::StatusReport,
    step::{ElevatedPlan, StepResult},
};
use std::{path::Path, sync::Arc, time::Duration};

/// Frozen NotRun reasons of the administrator step. The report shows each one verbatim.
const CONSENT_MISMATCH: &str = "the administrator step doesn't match what was shown";
const AGENT_NOT_PLANNED: &str = "the installed agent isn't where the plan said";
const KIT_UNAVAILABLE: &str =
    "the administrator setup kit isn't available or doesn't match this installer";
const RECORD_UNAVAILABLE: &str =
    "the administrator setup record can't be read or belongs to another install";
const COULD_NOT_START: &str = "Windows administrator setup couldn't start";

/// Longest one native observation of the step may take. Each observation gets a fresh budget.
const OBSERVE_MS: u64 = 30_000;

/// Everything one administrator step needs. `lock` is `Held` while the caller keeps the installer
/// lock, and `PerWrite` when only the journal writes take it.
pub(crate) struct ElevatedRun<'a> {
    pub io: &'a WindowsNativeIo,
    pub lock: LockSource<'a>,
    pub plan: &'a ElevatedPlan,
    pub manifest: &'a KitManifest,
    pub sources: Option<&'a KitSources>,
}

/// Runs one consented administrator step. Every refusal before the launch is `NotRun` with a frozen
/// reason, so nothing elevated has happened. A launch maps the launcher's result to a `StepResult`.
/// Never panics.
pub(crate) fn run(input: ElevatedRun<'_>) -> StepResult {
    let ElevatedRun {
        io,
        lock,
        plan,
        manifest,
        sources,
    } = input;
    let verb = plan.verb();
    let Ok(consent) = plan.consent() else {
        return StepResult::not_run(verb, CONSENT_MISMATCH);
    };
    // The plan's scope names the rule and the program. Every mutating plan has one.
    let Some(scope) = verb.scope() else {
        return StepResult::not_run(verb, CONSENT_MISMATCH);
    };
    if matches!(verb, Verb::Setup(_) | Verb::AddFirewall(_)) && !planned_agent(io, &lock, scope) {
        return StepResult::not_run(verb, AGENT_NOT_PLANNED);
    }
    // `_hold` keeps the four kit files unreplaceable until this function returns: after the helper
    // has run and its result is mapped.
    let Ok((helper, _hold)) = kit_helper(io, manifest, sources) else {
        return StepResult::not_run(verb, KIT_UNAVAILABLE);
    };
    let Ok(mut store) = StoreJournal::open(io, lock, scope) else {
        return StepResult::not_run(verb, RECORD_UNAVAILABLE);
    };
    // A pending Intent is settled by observation first, so the record is current before the run.
    if store.settle_pending(&helper).is_err() {
        return if store.failed() {
            StepResult::journal_failure(verb)
        } else {
            StepResult::not_run(verb, RECORD_UNAVAILABLE)
        };
    }
    match helper.run(verb, &consent, &mut store) {
        Ok(LaunchOutcome::Declined) => StepResult::declined(verb),
        Ok(LaunchOutcome::Verified { outcome, after }) => {
            StepResult::launched(verb, &[(verb.clone(), Some(outcome))], Some(&after))
        }
        Ok(LaunchOutcome::Unverified { outcome, after }) => {
            StepResult::launched(verb, &[(verb.clone(), outcome)], after.as_ref())
        }
        Ok(LaunchOutcome::Parts { outcomes, after }) => {
            StepResult::launched(verb, &outcomes, after.as_ref())
        }
        Err(NativeError::OutcomeUnknown) => StepResult::journal_failure(verb),
        Err(_) if store.failed() => StepResult::journal_failure(verb),
        Err(_) => StepResult::not_run(verb, COULD_NOT_START),
    }
}

/// Read-only: the placed helper's status after `verify_placed`. It never places the kit and never
/// elevates. `scope` is the rule the status is asked about.
pub(crate) fn observe(
    io: &WindowsNativeIo,
    manifest: &KitManifest,
    scope: &RuleScope,
) -> NativeResult<StatusReport> {
    let image = elevated_kit::verify_placed(Path::new(io.target().paths().install()), manifest)?;
    ElevatedHelper::locate(image)?.status(Some(scope))
}

/// The placed kit's helper and its hold. `ensure` places the verified bytes first when `sources`
/// allows it. `hold_placed` then verifies the placed files through handles that refuse writes and
/// deletes, so they can't change while the hold lives. The helper image is the one it returned.
fn kit_helper(
    io: &WindowsNativeIo,
    manifest: &KitManifest,
    sources: Option<&KitSources>,
) -> NativeResult<(ElevatedHelper, KitHold)> {
    let install = Path::new(io.target().paths().install());
    elevated_kit::ensure(install, manifest, sources)?;
    let (image, hold) = elevated_kit::hold_placed(install, manifest)?;
    Ok((ElevatedHelper::locate(image)?, hold))
}

/// Whether the installed agent's canonical path is the one `scope` names. Any error is `false`, so
/// the step is `NotRun`.
fn planned_agent(io: &WindowsNativeIo, lock: &LockSource<'_>, scope: &RuleScope) -> bool {
    matches!(payload_agent_matches(io, lock, scope), Ok(true))
}

/// `payload_root` needs the installer lock. A held lock is used as it is; for `PerWrite` one lock is
/// taken for this observation only, and it is released before the journal opens.
fn payload_agent_matches(
    io: &WindowsNativeIo,
    lock: &LockSource<'_>,
    scope: &RuleScope,
) -> NativeResult<bool> {
    let deadline = observation_deadline()?;
    let root = match lock {
        LockSource::Held(held) => {
            let proof = io.admit_support(&deadline)?;
            io.payload_root(&proof, held, &deadline)?
        }
        LockSource::PerWrite => {
            let taken = first_install::retry_busy(
                &deadline,
                || {
                    let proof = io.admit_support(&deadline)?;
                    io.acquire_installer_lock(&proof, &deadline)
                },
                |ms| std::thread::sleep(Duration::from_millis(ms)),
            )?;
            let proof = io.admit_support(&deadline)?;
            io.payload_root(&proof, &taken, &deadline)?
        }
    };
    let agent = format!(r"{}\crosspane-agent.exe", root.canonical_dos_path());
    Ok(scope.program.same_path(&agent))
}

/// A fresh `OBSERVE_MS` budget on the monotonic clock with no cancellation.
fn observation_deadline() -> NativeResult<Deadline> {
    Deadline::new(
        OBSERVE_MS,
        Arc::new(MonotonicClock::default()),
        Cancellation::default(),
    )
}
