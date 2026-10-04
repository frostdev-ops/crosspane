//! Compatible repair on the merged Linux repair coordinator (`platform/linux/repair`).
//!
//! The coordinator's plan, consent and run are opaque and live only seconds, so this adapter
//! only sequences them: it plans for the preview, and when the person confirms it observes
//! everything again, starts only if that still previews exactly what was shown, and then drives
//! the stages (stop, replace, start) and the later health check. It adds no policy about what a
//! result means beyond wording: every typed outcome of the coordinator reaches the person.
//!
//! Authority is the ordinary support proof, taken fresh before every stage. A repair that was
//! interrupted is only ever resumed from its on-disk record, never replayed.

use std::sync::Arc;

use crosspane_installer_core::{OperationId, ResourceObservation};

use super::super::native_io::{
    Cancellation, ChildEnvironment, Deadline, ExitReader, LinuxNativeIo, MAX_NATIVE_TIMEOUT_MS,
    NativeError, SupportProof,
};
use super::super::payload::{Package, PayloadError, PayloadInstaller};
use super::super::removal::RemovalError;
use super::super::repair::{
    CompatibilityIssue, LinuxRepair, RecoveryPresence, RepairError, RepairInput, RepairOutcome,
    RepairPlan, RepairResult, RepairStage,
};
use super::super::service::{LinuxService, ServiceError};
use super::domains::{RepairFinish, RepairOffer, RepairStep, Repairer, Support, SupportOutcome};
use crate::agent_contract::AgentReply;
use crate::live::{Availability, RepairOutcome as Shown};

const READ_MS: u64 = 15_000;
/// A stage that stops the agent, replaces files or starts it again. Each stage gets its own
/// deadline, and an expired call is never extended.
const STAGE_MS: u64 = 60_000;
/// Keep the preview under the controller's text bound so the tail is never cut off.
const PREVIEW_MAX: usize = 560;
const MAX_MATERIAL_LINES: usize = 6;
const REMOVE_AND_REINSTALL: &str = "Remove Crosspane and install it again to start clean.";
/// A resume only ever settles what the record proves; when it can't, removal is the way out.
const RESUME_OR_REINSTALL: &str =
    "If Resume can't settle it, remove Crosspane and install it again.";

fn stage_deadline(ms: u64) -> Result<Deadline, String> {
    Deadline::new(ms.clamp(1, MAX_NATIVE_TIMEOUT_MS), Cancellation::default())
        .map_err(|_| "Repair can't start right now. Nothing was changed.".to_owned())
}

pub struct NativeRepairer {
    io: Arc<LinuxNativeIo>,
    env: ChildEnvironment,
    support: Arc<dyn Support>,
    scratch_reader: Option<Arc<dyn ExitReader>>,
    held: Option<Held>,
    active: Option<Active>,
}

opaque_debug!(NativeRepairer);

/// What the person was shown: the repair is only started if a fresh plan still has this basis.
struct Held {
    plan: OperationId,
    basis: Basis,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Basis {
    delta: Vec<(String, ResourceObservation)>,
    manifest: [u8; 32],
    instance: Option<u64>,
    /// Which activity the preview named, when the agent's status was available to read it.
    activity: Option<(Option<bool>, Option<bool>, Option<bool>)>,
}

impl Basis {
    /// Everything the person read must still hold. Activity is compared only when the preview
    /// named it; a preview that said it couldn't tell already covered any activity.
    fn still_holds(&self, fresh: &Basis) -> bool {
        self.delta == fresh.delta
            && self.manifest == fresh.manifest
            && self.instance == fresh.instance
            && (self.activity.is_none() || self.activity == fresh.activity)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Wait {
    /// The old agent was stopped; its clean exit hasn't been proved yet, so nothing was replaced.
    CleanExit,
    /// Files replaced and the new agent started; its health isn't proved yet.
    Health,
}

/// A running repair that is waiting for the next stage.
struct Active {
    repair: LinuxRepair,
    wait: Wait,
    /// How many files the confirmed plan puts back, for the final line.
    replaced: usize,
}

/// What an earlier repair left on this computer.
enum Earlier {
    /// No record, or a record of a repair that was verified: a new repair may start.
    None,
    /// An unfinished repair: only resuming it can settle it.
    Unfinished(RepairResult),
    /// A record that can't be read or doesn't match this install.
    Unreadable(String),
}

impl NativeRepairer {
    pub fn new(io: Arc<LinuxNativeIo>, env: ChildEnvironment, support: Arc<dyn Support>) -> Self {
        Self {
            io,
            env,
            support,
            scratch_reader: None,
            held: None,
            active: None,
        }
    }

    /// The scratch-only seam for the original-process exit reader; the coordinator refuses it on
    /// anything but an explicit scratch target.
    #[doc(hidden)]
    pub fn scratch_exit_reader(&mut self, reader: Arc<dyn ExitReader>) {
        self.scratch_reader = Some(reader);
    }

    fn proof(&self, package: &Package) -> Result<SupportProof, String> {
        let deadline = stage_deadline(READ_MS)?;
        match self.support.detect(Some(package), &deadline) {
            SupportOutcome::Supported(proof) => Ok(proof),
            SupportOutcome::NotSupported(text) | SupportOutcome::Pending(text) => Err(text),
        }
    }

    fn service(&self, package: &Package, deadline: &Deadline) -> Result<LinuxService, String> {
        let installer = PayloadInstaller::new(self.io.clone()).map_err(|_| {
            "The install locations can't be admitted. Nothing was changed.".to_owned()
        })?;
        let resources = installer.rendered_resources(package).map_err(|_| {
            "The staged payload doesn't describe this install. Nothing was changed.".to_owned()
        })?;
        LinuxService::new(
            self.io.clone(),
            super::session_of(&self.env),
            resources,
            deadline,
        )
        .map_err(|_| "Crosspane's service can't be read just now. Nothing was changed.".to_owned())
    }

    fn new_repair(&self) -> Result<LinuxRepair, String> {
        let mut repair = LinuxRepair::new(self.io.clone()).map_err(|e| guidance(&e))?;
        if let Some(reader) = &self.scratch_reader {
            repair
                .scratch_exit_reader(reader.clone())
                .map_err(|e| guidance(&e))?;
        }
        Ok(repair)
    }

    /// A read-only look at the repair record. It is deliberately given no Status: without one the
    /// coordinator can't prove the new instance's health, so this look never completes, verifies
    /// or retires anything. It only reports where an earlier repair stopped.
    fn earlier(&self, input: &RepairInput<'_>) -> Result<Earlier, String> {
        let mut repair = self.new_repair()?;
        let blind = RepairInput {
            proof: input.proof,
            package: input.package,
            service: input.service,
            reply: None,
            expected_reply_id: 0,
            now_ms: input.now_ms,
            deadline: input.deadline,
        };
        Ok(match repair.resume(&blind) {
            Err(RepairError::RecoveryPending) => Earlier::None,
            Ok(result) if result.stage == RepairStage::Verified => Earlier::None,
            Ok(result) => Earlier::Unfinished(result),
            Err(error) => Earlier::Unreadable(format!(
                "A repair record on this computer can't be read or doesn't match this install. {}",
                guidance(&error)
            )),
        })
    }

    /// Refuse when an unfinished repair record exists: a new repair would be refused by the
    /// coordinator anyway, and the unfinished files would otherwise look like foreign ones.
    fn ensure_settled(&self, input: &RepairInput<'_>) -> Result<(), String> {
        match self.earlier(input)? {
            Earlier::None => Ok(()),
            Earlier::Unfinished(_) => Err(guidance(&RepairError::RecoveryPending)),
            Earlier::Unreadable(text) => Err(text),
        }
    }

    fn package(package: Option<&Package>) -> Result<&Package, String> {
        package.ok_or_else(|| {
            "Repair needs the staged payload (--payload) that matches the installed version, to \
             check the installed files before replacing any. Nothing was changed."
                .to_owned()
        })
    }
}

fn input<'a>(
    proof: &'a SupportProof,
    package: &'a Package,
    service: &'a LinuxService,
    status: Option<&'a AgentReply>,
    now_ms: u64,
    deadline: &'a Deadline,
) -> RepairInput<'a> {
    RepairInput {
        proof,
        package,
        service,
        reply: status,
        expected_reply_id: status.map_or(0, |r| r.id),
        now_ms,
        deadline,
    }
}

fn basis(plan: &RepairPlan) -> Basis {
    let facts = plan.facts();
    Basis {
        delta: plan
            .delta()
            .iter()
            .map(|r| (r.resource_id.clone(), r.before))
            .collect(),
        manifest: facts.package_manifest_sha256,
        instance: facts.instance_id,
        activity: facts
            .activity
            .as_ref()
            .map(|a| (a.input, a.projections, a.audio)),
    }
}

/// The person-readable preview: what is put back, what it interrupts, what is kept.
fn preview(plan: &RepairPlan) -> String {
    let delta = plan.delta();
    let word = |before: ResourceObservation| match before {
        ResourceObservation::Absent => "missing",
        _ => "different",
    };
    let activity = match &plan.facts().activity {
        Some(a) => {
            let mut busy = Vec::new();
            if a.input == Some(true) {
                busy.push("shared keyboard and mouse");
            }
            if a.projections == Some(true) {
                busy.push("shared windows");
            }
            if a.audio == Some(true) {
                busy.push("shared sound");
            }
            if busy.is_empty() {
                "nothing was in use when this was checked".to_owned()
            } else {
                format!("this ends: {}", busy.join(", "))
            }
        }
        None => "whether anything is in use couldn't be checked, so anything in progress may end"
            .to_owned(),
    };
    let tail = format!(
        "Crosspane is stopped, then started again; {activity}. Settings, identity, pairings and \
         permissions are kept, and files Crosspane didn't create are left alone. Old files stay \
         as backups until the new Crosspane reports healthy. No firewall change is made."
    );
    // Name as many files as fit; the rest are counted.
    for shown in (0..=delta.len().min(5)).rev() {
        let names: Vec<String> = delta
            .iter()
            .take(shown)
            .map(|r| format!("{} ({})", r.resource_id, word(r.before)))
            .collect();
        let more = delta.len() - shown;
        let list = match (names.is_empty(), more) {
            (true, n) => format!("{n} file(s)"),
            (false, 0) => names.join(", "),
            (false, n) => format!("{} and {n} more", names.join(", ")),
        };
        let text = format!("Repair puts back what this installer ships: {list}. {tail}");
        if text.len() <= PREVIEW_MAX || shown == 0 {
            return text;
        }
    }
    unreachable_text()
}

fn unreachable_text() -> String {
    "Repair puts back what this installer ships. Crosspane is stopped, then started again. \
     Settings and identity are kept."
        .to_owned()
}

// ---- wording ----------------------------------------------------------------------------------

/// Why nothing was changed: a refusal with the next step.
fn guidance(error: &RepairError) -> String {
    let foreign = format!(
        "Some files where Crosspane installs weren't put there by Crosspane, or were taken over \
         by it, so repair would have to guess. They are left exactly as they are. \
         {REMOVE_AND_REINSTALL}"
    );
    match error {
        RepairError::Tier2(CompatibilityIssue::MixedOwnership)
        | RepairError::Payload(PayloadError::Foreign) => foreign,
        RepairError::Tier2(CompatibilityIssue::FallbackIdentity) => format!(
            "This computer's identity is kept in a file instead of the system keyring. Moving it \
             is a manual step that repair never does, so nothing was changed. \
             {REMOVE_AND_REINSTALL}"
        ),
        RepairError::Admission(RemovalError::NotClean | RemovalError::Service(_))
        | RepairError::Service(ServiceError::Foreign) => format!(
            "Repair needs the Crosspane that setup installed to be running, so it can be stopped \
             cleanly and checked. It isn't, or it doesn't match this install. Nothing was \
             changed. Start it, or {}",
            REMOVE_AND_REINSTALL.to_lowercase()
        ),
        RepairError::Admission(RemovalError::Stale) => {
            "What was checked changed before it could be used. Nothing was changed; review the \
             repair again."
                .to_owned()
        }
        RepairError::RecoveryPending => {
            "An earlier repair didn't finish, so no new one can start. Resume the earlier one. \
             Nothing was changed."
                .to_owned()
        }
        RepairError::Native(NativeError::Timeout | NativeError::Cancelled) => {
            "Reading Crosspane's install ran out of time. Nothing was changed.".to_owned()
        }
        RepairError::Native(NativeError::Unsupported) => {
            "Support for this computer couldn't be proved just now. Nothing was changed.".to_owned()
        }
        _ => "Crosspane's install can't be read just now, so nothing was changed.".to_owned(),
    }
}

/// Why a repair that did change something stopped.
fn reason(error: &RepairError) -> String {
    match error {
        RepairError::Admission(RemovalError::NotClean) => {
            "The original Crosspane's clean exit couldn't be proved.".to_owned()
        }
        RepairError::Tier2(CompatibilityIssue::FallbackIdentity) => {
            "The new instance keeps this computer's identity in a file instead of the keyring. \
             Moving it is a manual step, so the backups were kept."
                .to_owned()
        }
        RepairError::Tier2(CompatibilityIssue::MixedOwnership) => {
            "Some files aren't Crosspane's own, so they were left alone.".to_owned()
        }
        RepairError::NotReady => "The new instance hasn't reported healthy yet.".to_owned(),
        RepairError::Native(NativeError::Timeout | NativeError::Cancelled) => {
            "A step ran out of time.".to_owned()
        }
        RepairError::Native(NativeError::OutcomeUnknown)
        | RepairError::Service(ServiceError::OutcomeUnknown)
        | RepairError::Payload(PayloadError::OutcomeUnknown) => {
            "A step may have completed, so it was not repeated.".to_owned()
        }
        RepairError::Service(ServiceError::Foreign) => {
            "The running agent isn't the new instance yet.".to_owned()
        }
        RepairError::RecoveryPending => "The repair record changed or is unfinished.".to_owned(),
        _ => "A step couldn't be completed.".to_owned(),
    }
}

fn describe_stage(stage: RepairStage) -> &'static str {
    match stage {
        RepairStage::Recorded | RepairStage::StopPending => "before Crosspane was stopped",
        RepairStage::Stopped => "after Crosspane was stopped, before any file was replaced",
        RepairStage::PayloadPending => "while files were being replaced",
        RepairStage::PayloadApplied | RepairStage::ReloadPending | RepairStage::StartPending => {
            "after the files were replaced, before Crosspane was started again"
        }
        RepairStage::AwaitingAgent => {
            "after Crosspane was started again, before it was seen healthy"
        }
        RepairStage::Verified => "after the new Crosspane was verified",
    }
}

/// Crosspane is down in every stage before it was started again and seen healthy.
fn may_be_stopped(stage: RepairStage) -> bool {
    !matches!(stage, RepairStage::AwaitingAgent | RepairStage::Verified)
}

fn material_lines(result: &RepairResult) -> Vec<String> {
    let mut lines: Vec<String> = result
        .recovery_material
        .iter()
        .take(MAX_MATERIAL_LINES)
        .map(|m| match m.presence {
            RecoveryPresence::Present => format!("Kept: {}", m.path.display()),
            RecoveryPresence::Unknown => {
                format!("Kept, or couldn't be checked: {}", m.path.display())
            }
        })
        .collect();
    let more = result
        .recovery_material
        .len()
        .saturating_sub(MAX_MATERIAL_LINES);
    if more > 0 {
        lines.push(format!("…and {more} more kept file(s)."));
    }
    lines
}

/// Every typed outcome of the coordinator, in the person's words.
fn finish_of(result: &RepairResult) -> RepairFinish {
    let (outcome, mut lines, resumable) = match result.outcome {
        // The count of files put back is added by the caller that knows the confirmed delta.
        RepairOutcome::Verified => {
            return RepairFinish {
                outcome: Shown::Verified,
                lines: vec![
                    "The new Crosspane reported healthy, and the old backups were retired."
                        .to_owned(),
                ],
                resumable: false,
            };
        }
        RepairOutcome::HealthVerifiedCleanupIncomplete => (
            Shown::HealthVerifiedCleanupIncomplete,
            vec![
                "The new Crosspane reported healthy. Some backup or record cleanup is still \
                 left; it does no harm."
                    .to_owned(),
            ],
            false,
        ),
        RepairOutcome::OutcomeUnknownAfterVerify => (
            Shown::OutcomeUnknown,
            vec![
                "Setup couldn't prove the result of the last check, and nothing was retried."
                    .to_owned(),
                "Resume looks at the new Crosspane again without repeating any change.".to_owned(),
            ],
            true,
        ),
        // A change was attempted and the repair didn't complete: everything is kept.
        _ => (
            Shown::RecoveryRetained,
            vec![format!(
                "The repair stopped {}.",
                describe_stage(result.stage)
            )],
            true,
        ),
    };
    if let Some(error) = &result.error {
        lines.push(reason(error));
    }
    if outcome == Shown::RecoveryRetained && may_be_stopped(result.stage) {
        lines.push(
            "Crosspane may not be running. Start it from your applications menu, or sign out \
             and in again."
                .to_owned(),
        );
    }
    if resumable {
        lines.push(RESUME_OR_REINSTALL.to_owned());
    }
    lines.extend(material_lines(result));
    RepairFinish {
        outcome,
        lines,
        resumable,
    }
}

fn unknown_finish(detail: &str) -> RepairFinish {
    RepairFinish {
        outcome: Shown::OutcomeUnknown,
        lines: vec![
            detail.to_owned(),
            "Nothing was retried. Resume looks at the new Crosspane again without repeating any \
             change."
                .to_owned(),
        ],
        resumable: true,
    }
}

fn retained_finish(detail: &str) -> RepairFinish {
    RepairFinish {
        outcome: Shown::RecoveryRetained,
        lines: vec![
            detail.to_owned(),
            "Backups and recovery files were kept.".to_owned(),
        ],
        resumable: true,
    }
}

/// Where an interrupted repair stopped, for the resume offer.
fn resumable_lines(result: &RepairResult) -> Vec<String> {
    let mut lines = vec![format!("It stopped {}.", describe_stage(result.stage))];
    if may_be_stopped(result.stage) {
        lines.push("Crosspane may not be running right now.".to_owned());
    }
    lines.extend(material_lines(result));
    lines.push(RESUME_OR_REINSTALL.to_owned());
    lines
}

// ---- the adapter ------------------------------------------------------------------------------

impl NativeRepairer {
    /// A fresh observation and plan, with the proof and service it rests on.
    fn fresh_plan(
        &self,
        package: &Package,
        status: Option<&AgentReply>,
        operation: OperationId,
        now_ms: u64,
    ) -> Result<(LinuxRepair, RepairPlan), String> {
        let deadline = stage_deadline(READ_MS)?;
        let proof = self.proof(package)?;
        let service = self.service(package, &deadline)?;
        let input = input(&proof, package, &service, status, now_ms, &deadline);
        self.ensure_settled(&input)?;
        let mut repair = self.new_repair()?;
        let inventory = repair.inventory(&input).map_err(|e| guidance(&e))?;
        let plan = repair
            .plan(inventory, operation.0, operation)
            .map_err(|e| guidance(&e))?;
        Ok((repair, plan))
    }

    fn waiting(&self, wait: Wait, result: &RepairResult) -> RepairStep {
        match wait {
            Wait::CleanExit => RepairStep::Waiting {
                detail: "Crosspane was stopped. Waiting for it to exit cleanly before any file is \
                         replaced…"
                    .to_owned(),
                if_timed_out: finish_of(result),
                // The old agent is down and the files are still to be replaced. Closing would
                // drop the original's exit watch, which a resume can never rebuild.
                closeable: false,
            },
            Wait::Health => RepairStep::Waiting {
                detail: "Crosspane was started again. Waiting for the new instance to report \
                         healthy…"
                    .to_owned(),
                if_timed_out: unknown_finish(
                    "The new Crosspane didn't report healthy in time, so what the repair did can't \
                     be proved.",
                ),
                // Every change is done and the on-disk record lets the next visit resume.
                closeable: true,
            },
        }
    }

    /// Replace the files and start the new agent after the original's clean stop.
    fn continue_stage(
        &mut self,
        mut repair: LinuxRepair,
        package: &Package,
        now_ms: u64,
        replaced: usize,
    ) -> RepairStep {
        let attempt = (|| {
            let deadline = stage_deadline(STAGE_MS)?;
            let proof = self.proof(package)?;
            let service = self.service(package, &deadline)?;
            let input = input(&proof, package, &service, None, now_ms, &deadline);
            repair.continue_after_stop(&input).map_err(|e| guidance(&e))
        })();
        match attempt {
            Ok(result) => match result.outcome {
                RepairOutcome::AwaitingAgent => {
                    let step = self.waiting(Wait::Health, &result);
                    self.active = Some(Active {
                        repair,
                        wait: Wait::Health,
                        replaced,
                    });
                    step
                }
                // The exit wasn't proved yet and nothing was replaced: the same stage is tried
                // again with the next look, and only a timeout makes it final.
                RepairOutcome::RecoveryRetained if result.stage == RepairStage::Stopped => {
                    let step = self.waiting(Wait::CleanExit, &result);
                    self.active = Some(Active {
                        repair,
                        wait: Wait::CleanExit,
                        replaced,
                    });
                    step
                }
                _ => RepairStep::Finished(finish_of(&result)),
            },
            // No proof or service could be had for this stage: nothing ran, so it is tried again.
            Err(_) => {
                self.active = Some(Active {
                    repair,
                    wait: Wait::CleanExit,
                    replaced,
                });
                RepairStep::Waiting {
                    detail: "Waiting to continue the repair…".to_owned(),
                    if_timed_out: retained_finish(
                        "The repair stopped before any file was replaced, and couldn't continue.",
                    ),
                    closeable: false,
                }
            }
        }
    }
}

impl Repairer for NativeRepairer {
    fn inspect(&mut self, package: Option<&Package>, now_ms: u64) -> RepairOffer {
        self.held = None;
        let unavailable = |text: String| RepairOffer {
            repair: Availability::Unavailable(text),
            resumable: None,
        };
        let package = match Self::package(package) {
            Ok(package) => package,
            Err(text) => return unavailable(text),
        };
        let (deadline, proof) = match (stage_deadline(READ_MS), self.proof(package)) {
            (Ok(deadline), Ok(proof)) => (deadline, proof),
            (_, Err(text)) | (Err(text), _) => return unavailable(text),
        };
        let service = match self.service(package, &deadline) {
            Ok(service) => service,
            Err(text) => return unavailable(text),
        };
        let input = input(&proof, package, &service, None, now_ms, &deadline);
        // An earlier repair's record decides first: while one is unfinished no new repair can
        // start.
        match self.earlier(&input) {
            Ok(Earlier::None) => {}
            Ok(Earlier::Unfinished(result)) => {
                return RepairOffer {
                    repair: Availability::Unavailable(
                        "An earlier repair didn't finish. Resume it first; nothing else can be \
                         repaired until it is settled."
                            .to_owned(),
                    ),
                    resumable: Some(resumable_lines(&result)),
                };
            }
            Ok(Earlier::Unreadable(text)) | Err(text) => return unavailable(text),
        }
        let repair = match self.new_repair() {
            Ok(repair) => repair,
            Err(text) => return unavailable(text),
        };
        let compatible = repair
            .inventory(&input)
            .map_err(|e| guidance(&e))
            .and_then(|inventory| {
                if let Some(issue) = inventory.compatibility_issue() {
                    return Err(guidance(&RepairError::Tier2(issue)));
                }
                let facts = inventory.facts();
                if let Err(error) = &facts.resources {
                    return Err(guidance(&RepairError::Payload(*error)));
                }
                if let Err(error) = &facts.service {
                    return Err(guidance(&RepairError::Service(*error)));
                }
                if let Err(error) = &facts.correlation {
                    return Err(guidance(&RepairError::Admission(error.clone())));
                }
                Ok(())
            });
        match compatible {
            Ok(()) => RepairOffer {
                repair: Availability::Available,
                resumable: None,
            },
            Err(text) => unavailable(text),
        }
    }

    fn plan(
        &mut self,
        package: Option<&Package>,
        status: Option<&AgentReply>,
        operation: OperationId,
        now_ms: u64,
    ) -> Result<String, String> {
        self.held = None;
        let package = Self::package(package)?;
        if self.active.is_some() {
            return Err(guidance(&RepairError::RecoveryPending));
        }
        let (_repair, plan) = self.fresh_plan(package, status, operation, now_ms)?;
        if plan.delta().is_empty() {
            return Err(
                "Nothing needs repair: every file Crosspane installed is in place and \
                        matches this installer. Nothing was changed."
                    .to_owned(),
            );
        }
        let text = preview(&plan);
        self.held = Some(Held {
            plan: operation,
            basis: basis(&plan),
        });
        Ok(text)
    }

    fn confirm(
        &mut self,
        package: Option<&Package>,
        status: Option<&AgentReply>,
        plan: OperationId,
        operation: OperationId,
        now_ms: u64,
    ) -> Result<RepairStep, String> {
        let held = self.held.take().filter(|h| h.plan == plan).ok_or_else(|| {
            "That repair preview is no longer current. Review the repair again. Nothing was \
             changed."
                .to_owned()
        })?;
        let package = Self::package(package)?;
        if self.active.is_some() {
            return Err(guidance(&RepairError::RecoveryPending));
        }
        let deadline = stage_deadline(STAGE_MS)?;
        let proof = self.proof(package)?;
        let service = self.service(package, &deadline)?;
        let input = input(&proof, package, &service, status, now_ms, &deadline);
        // A preview's evidence lives seconds and a person takes longer to read it, so everything
        // is observed again now and the repair starts only if it still previews what was shown.
        self.ensure_settled(&input)?;
        let mut repair = self.new_repair()?;
        let inventory = repair.inventory(&input).map_err(|e| guidance(&e))?;
        let fresh = repair
            .plan(inventory, operation.0, operation)
            .map_err(|e| guidance(&e))?;
        if !held.basis.still_holds(&basis(&fresh)) {
            return Err(
                "Things changed since the preview was shown. Nothing was changed; review \
                        the repair again."
                    .to_owned(),
            );
        }
        let consent = fresh
            .consent(operation.0, operation, true)
            .map_err(|e| guidance(&e))?;
        let current = repair.inventory(&input).map_err(|e| guidance(&e))?;
        // An error here is before the coordinator's first change.
        let result = repair
            .begin(fresh, consent, &current, &input)
            .map_err(|e| guidance(&e))?;
        match result.outcome {
            RepairOutcome::NoDelta => {
                Err("Nothing needs repair any more. Nothing was changed.".to_owned())
            }
            RepairOutcome::AwaitingCleanExit => {
                Ok(self.continue_stage(repair, package, now_ms, held.basis.delta.len()))
            }
            _ => Ok(RepairStep::Finished(finish_of(&result))),
        }
    }

    fn verify(
        &mut self,
        package: Option<&Package>,
        status: Option<&AgentReply>,
        now_ms: u64,
    ) -> RepairStep {
        let Some(Active {
            repair,
            wait,
            replaced,
        }) = self.active.take()
        else {
            return RepairStep::Finished(unknown_finish(
                "No repair is running in this window any more.",
            ));
        };
        let package = match Self::package(package) {
            Ok(package) => package,
            Err(text) => {
                self.active = Some(Active {
                    repair,
                    wait,
                    replaced,
                });
                return RepairStep::Waiting {
                    detail: text,
                    if_timed_out: retained_finish("The repair couldn't continue."),
                    closeable: wait == Wait::Health,
                };
            }
        };
        if wait == Wait::CleanExit {
            return self.continue_stage(repair, package, now_ms, replaced);
        }
        let mut repair = repair;
        let attempt = (|| {
            let deadline = stage_deadline(READ_MS)?;
            let proof = self.proof(package)?;
            let service = self.service(package, &deadline)?;
            let input = input(&proof, package, &service, status, now_ms, &deadline);
            repair.verify(&input).map_err(|e| guidance(&e))
        })();
        let keep = |this: &mut Self, repair: LinuxRepair| {
            this.active = Some(Active {
                repair,
                wait: Wait::Health,
                replaced,
            });
        };
        match attempt {
            Ok(result) => match result.outcome {
                RepairOutcome::AwaitingAgent => {
                    let step = self.waiting(Wait::Health, &result);
                    keep(self, repair);
                    step
                }
                // A fallback-identity migration is final; anything else may be a new instance
                // that isn't up yet, so it is looked at again until the wait runs out.
                RepairOutcome::RecoveryRetained
                    if !matches!(result.error, Some(RepairError::Tier2(_))) =>
                {
                    let step = RepairStep::Waiting {
                        detail: "Waiting for the new instance to report healthy…".to_owned(),
                        if_timed_out: finish_of(&result),
                        closeable: true,
                    };
                    keep(self, repair);
                    step
                }
                RepairOutcome::Verified => {
                    let mut done = finish_of(&result);
                    done.lines
                        .push(format!("{replaced} file(s) were put back."));
                    RepairStep::Finished(done)
                }
                _ => RepairStep::Finished(finish_of(&result)),
            },
            Err(text) => {
                keep(self, repair);
                RepairStep::Waiting {
                    detail: format!("Waiting to check the new instance. {text}"),
                    if_timed_out: unknown_finish(
                        "The new Crosspane couldn't be checked, so what the repair did can't be \
                         proved.",
                    ),
                    closeable: true,
                }
            }
        }
    }

    fn resume(
        &mut self,
        package: Option<&Package>,
        status: Option<&AgentReply>,
        now_ms: u64,
    ) -> Result<RepairFinish, String> {
        // The record on disk is the truth, not anything this window remembers.
        self.held = None;
        self.active = None;
        let package = Self::package(package)?;
        let deadline = stage_deadline(READ_MS)?;
        let proof = self.proof(package)?;
        let service = self.service(package, &deadline)?;
        let input = input(&proof, package, &service, status, now_ms, &deadline);
        let mut repair = self.new_repair()?;
        match repair.resume(&input) {
            Err(RepairError::RecoveryPending) => {
                Err("There is no earlier repair to resume. Nothing was changed.".to_owned())
            }
            Err(error) => Err(guidance(&error)),
            Ok(result) => Ok(finish_of(&result)),
        }
    }
}
