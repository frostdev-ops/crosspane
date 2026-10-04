//! Each recorded request is consumed before dispatch. A timeout is never an automatic retry.
use super::super::{
    payload::{MatchingFiles, PayloadPlan},
    removal::{CleanStop, InventoryRequest, TrackedAgent},
    service::{AgentEvidence, ServiceAction, ServiceResult},
};
use super::resume::Journal;
use super::*;
use crosspane_installer_core::MutationOutcome;
use std::collections::BTreeMap;

pub(super) struct Run {
    journal: Journal,
    original: Arc<TrackedAgent>,
    payload: Option<PayloadPlan>,
    resources: Vec<ResourceReceipt>,
    service: Option<LinuxService>,
}
impl Run {
    fn result(&self, outcome: RepairOutcome, error: Option<RepairError>) -> RepairResult {
        RepairResult {
            operation: self.journal.operation(),
            stage: self.journal.stage(),
            outcome,
            resources: self.resources.clone(),
            activity_retired: self.journal.stage() != RepairStage::Recorded,
            recovery_material: Vec::new(),
            error,
        }
    }
    fn checkpoint(
        &mut self,
        io: &LinuxNativeIo,
        input: &RepairInput<'_>,
        stage: RepairStage,
    ) -> Result<()> {
        let previous = self.journal.stage();
        self.journal.advance(stage);
        self.journal.checkpoint(io, input, previous)
    }
}

impl LinuxRepair {
    /// Consent admission and a last inventory check happen before any journal/service mutation.
    /// The ordinary Stop route is used while the originally admitted process is still alive.
    pub fn begin(
        &mut self,
        plan: RepairPlan,
        consent: RepairConsent,
        current: &RepairInventory,
        input: &RepairInput<'_>,
    ) -> Result<RepairResult> {
        if self.run.is_some() {
            return Err(RepairError::RecoveryPending);
        }
        if current.compatibility.is_some() {
            return Err(RemovalError::Stale.into());
        }
        self.planner.validate(
            &plan.plan,
            &consent.consent,
            &current.inventory,
            InventoryRequest {
                proof: input.proof,
                package: input.package,
                service: input.service,
                reply: input.correlated_reply(),
                now_ms: input.now_ms,
                deadline: input.deadline,
            },
        )?;
        if plan.delta().is_empty() {
            return Ok(RepairResult {
                operation: plan.operation(),
                stage: RepairStage::Recorded,
                outcome: RepairOutcome::NoDelta,
                resources: plan
                    .facts()
                    .resources
                    .as_ref()
                    .map_err(Clone::clone)?
                    .clone(),
                activity_retired: false,
                recovery_material: Vec::new(),
                error: None,
            });
        }
        let original = plan.plan.tracked().cloned().ok_or(RemovalError::NotClean)?;
        let payload = self.installer.plan(
            input.proof,
            input.package,
            plan.operation(),
            MatchingFiles::Preserve,
        )?;
        // No extra resource/adoption can be introduced between the preview and transaction plan.
        if &payload.receipt().resources != plan.facts().resources.as_ref().map_err(Clone::clone)? {
            return Err(RemovalError::Stale.into());
        }
        if &input.service.observe(input.deadline)?
            != plan.facts().service.as_ref().map_err(|e| *e)?
        {
            return Err(RemovalError::Stale.into());
        }
        let stop = input
            .service
            .plan(input.proof, ServiceAction::Stop, input.deadline)?;
        let journal = Journal::new(&self.io, &plan)?;
        let mut run = Run {
            journal,
            original,
            resources: payload.receipt().resources.clone(),
            payload: Some(payload),
            service: None,
        };
        // Atomic publication can fail after its rename. Do not infer that the intent is absent,
        // remove an ambiguous prior record, or dispatch Stop after uncertain journal creation.
        if let Err(error) = run.journal.create(&self.io, input) {
            let result = run.result(RepairOutcome::RecoveryRetained, Some(error));
            self.run = Some(run);
            return Ok(self.with_material(result, input.deadline));
        }
        if let Err(error) = run.checkpoint(&self.io, input, RepairStage::StopPending) {
            let result = run.result(RepairOutcome::RecoveryRetained, Some(error));
            self.run = Some(run);
            return Ok(self.with_material(result, input.deadline));
        }
        let stopped = input.service.apply(input.proof, stop, input.deadline);
        let result = match stopped {
            Ok(result) if result.outcome == MutationOutcome::Verified => {
                match run.checkpoint(&self.io, input, RepairStage::Stopped) {
                    Ok(()) => run.result(RepairOutcome::AwaitingCleanExit, None),
                    Err(error) => run.result(RepairOutcome::RecoveryRetained, Some(error)),
                }
            }
            Ok(_) => run.result(
                RepairOutcome::RecoveryRetained,
                Some(NativeError::OutcomeUnknown.into()),
            ),
            Err(error) => run.result(RepairOutcome::RecoveryRetained, Some(error.into())),
        };
        self.run = Some(run);
        Ok(self.with_material(result, input.deadline))
    }

    /// Each call has one absolute deadline. Stop zero alone never reaches payload apply.
    /// A missing/unclean/ambiguous original exit leaves the stage Stopped and all files intact.
    pub fn continue_after_stop(&mut self, input: &RepairInput<'_>) -> Result<RepairResult> {
        let mut run = self.run.take().ok_or(RepairError::NotReady)?;
        let result = if run.journal.stage() != RepairStage::Stopped {
            Err(RepairError::NotReady)
        } else {
            self.replace_and_start(&mut run, input)
                .map(|()| run.result(RepairOutcome::AwaitingAgent, None))
        };
        let result = match result {
            Ok(result) => result,
            Err(RepairError::NotReady) => {
                self.run = Some(run);
                return Err(RepairError::NotReady);
            }
            Err(error) => run.result(RepairOutcome::RecoveryRetained, Some(error)),
        };
        self.run = Some(run);
        Ok(self.with_material(result, input.deadline))
    }

    fn replace_and_start(&self, run: &mut Run, input: &RepairInput<'_>) -> Result<()> {
        input.deadline.check()?;
        input.proof.check(&self.io)?;
        run.journal.check_package(input.package)?;
        let authority = run.original.clean_authority(input.deadline)?;
        let clean = authority.clean_stop(&self.io, input.deadline)?;
        if clean.instance_id() != run.journal.original_instance() {
            return Err(RemovalError::NotClean.into());
        }
        run.checkpoint(&self.io, input, RepairStage::PayloadPending)?;
        let payload = run.payload.take().ok_or(RepairError::RecoveryPending)?;
        let receipt = self.installer.apply_after_clean_stop(
            input.proof,
            input.package,
            payload,
            &clean,
            input.deadline,
        )?;
        run.resources = receipt.resources;
        run.checkpoint(&self.io, input, RepairStage::PayloadApplied)?;
        let service = LinuxService::new(
            self.io.clone(),
            BTreeMap::new(),
            self.installer.rendered_resources(input.package)?,
            input.deadline,
        )?;
        if service.observe(input.deadline)?.needs_reload {
            let reload = service.plan(input.proof, ServiceAction::Reload, input.deadline)?;
            run.checkpoint(&self.io, input, RepairStage::ReloadPending)?;
            let result = service.apply(input.proof, reload, input.deadline)?;
            if result.outcome != MutationOutcome::Verified {
                return Err(NativeError::OutcomeUnknown.into());
            }
        }
        self.start_after_clean_stop(run, &service, &clean, input)?;
        run.service = Some(service);
        run.checkpoint(&self.io, input, RepairStage::AwaitingAgent)
    }

    fn start_after_clean_stop(
        &self,
        run: &mut Run,
        service: &LinuxService,
        clean: &CleanStop,
        input: &RepairInput<'_>,
    ) -> Result<ServiceResult> {
        let start = service.plan_after_clean_stop(
            input.proof,
            ServiceAction::Start,
            clean,
            input.deadline,
        )?;
        run.checkpoint(&self.io, input, RepairStage::StartPending)?;
        // This is the sole Start dispatch. Unknown never re-enters this path; only new health
        // can resolve it through verify/resume. CleanStop is revalidated again by apply.
        Ok(service.apply_after_clean_stop(input.proof, start, clean, input.deadline)?)
    }

    /// Positive health must belong to the new native-admitted instance. A completed receipt
    /// alone never restores health/readiness; every retry performs admission again.
    pub fn verify(&mut self, input: &RepairInput<'_>) -> Result<RepairResult> {
        let mut run = self.run.take().ok_or(RepairError::NotReady)?;
        if run.journal.stage() != RepairStage::AwaitingAgent {
            self.run = Some(run);
            return Err(RepairError::NotReady);
        }
        let result = match self.verify_run(&mut run, input) {
            Ok(result) => result,
            Err(error) => self.with_material(
                run.result(RepairOutcome::RecoveryRetained, Some(error)),
                input.deadline,
            ),
        };
        self.run = Some(run);
        Ok(result)
    }

    fn verify_run(&self, run: &mut Run, input: &RepairInput<'_>) -> Result<RepairResult> {
        run.journal.check_package(input.package)?;
        let service = run.service.as_ref().ok_or(RepairError::NotReady)?;
        if !self.new_health(service, &run.journal, input)? {
            return Ok(run.result(RepairOutcome::AwaitingAgent, Some(RepairError::NotReady)));
        }
        let reply = input.correlated_reply().ok_or(RepairError::NotReady)?;
        match self.installer.verify(
            input.proof,
            input.package,
            input.expected_reply_id,
            input.now_ms,
            reply,
            input.deadline,
        ) {
            Ok(receipt) => {
                run.resources = receipt.resources;
                let error = run.checkpoint(&self.io, input, RepairStage::Verified).err();
                let outcome = if error.is_none() {
                    RepairOutcome::Verified
                } else {
                    RepairOutcome::HealthVerifiedCleanupIncomplete
                };
                Ok(self.with_material(run.result(outcome, error), input.deadline))
            }
            Err(error) => {
                let (outcome, resources) =
                    self.after_verify_outcome(run.journal.operation(), input);
                if let Some(resources) = resources {
                    run.resources = resources;
                }
                Ok(self.with_material(run.result(outcome, Some(error.into())), input.deadline))
            }
        }
    }

    fn new_health(
        &self,
        service: &LinuxService,
        journal: &Journal,
        input: &RepairInput<'_>,
    ) -> Result<bool> {
        input.deadline.check()?;
        input.proof.check(&self.io)?;
        let facts = service.observe(input.deadline)?;
        match service.agent(
            &facts,
            input.correlated_reply(),
            input.expected_reply_id,
            input.now_ms,
            Some(journal.original_instance()),
            input.deadline,
        )? {
            // Fallback identity migration stays Tier 2, including a newly admitted instance.
            // Never let payload verification retire backups on its otherwise Ready backends.
            AgentEvidence::Matched(health)
                if health.installer().keystore == KeyStoreProvenance::File =>
            {
                Err(RepairError::Tier2(CompatibilityIssue::FallbackIdentity))
            }
            AgentEvidence::Matched(_) => Ok(true),
            _ => Ok(false),
        }
    }

    fn after_verify_outcome(
        &self,
        operation: OperationId,
        input: &RepairInput<'_>,
    ) -> (RepairOutcome, Option<Vec<ResourceReceipt>>) {
        // The frozen native typed resume observation validates actual files/siblings. Its receipt
        // is used only after this call admitted fresh new-instance health and attempted verify.
        // No private payload codec is duplicated. Cancellation/expiry makes the boundary Unknown.
        if input.deadline.check().is_err() {
            return (RepairOutcome::OutcomeUnknownAfterVerify, None);
        }
        let observation = self.installer.resume_plan(input.proof, input.package);
        if input.deadline.check().is_err() {
            return (RepairOutcome::OutcomeUnknownAfterVerify, None);
        }
        match observation {
            Ok(plan)
                if plan.receipt().operation_id == operation
                    && plan.receipt().unfinished.is_empty()
                    && plan
                        .receipt()
                        .resources
                        .iter()
                        .all(|r| r.outcome == MutationOutcome::Verified) =>
            {
                (
                    RepairOutcome::HealthVerifiedCleanupIncomplete,
                    Some(plan.receipt().resources.clone()),
                )
            }
            Ok(plan) if plan.receipt().operation_id == operation => (
                RepairOutcome::RecoveryRetained,
                Some(plan.receipt().resources.clone()),
            ),
            _ => (RepairOutcome::OutcomeUnknownAfterVerify, None),
        }
    }

    fn with_material(&self, mut result: RepairResult, deadline: &Deadline) -> RepairResult {
        let state = self
            .io
            .target()
            .paths()
            .state_home
            .join("crosspane/installer");
        let mut paths = vec![
            state.clone(),
            state.join("repair-intent.json"),
            state.join("payload-intent.json"),
            state.join("payload-outcome.json"),
        ];
        for (i, target) in self.installer.targets().iter().enumerate() {
            for kind in ["previous", "stage"] {
                let name = format!(".crosspane-{kind}-{}-{i}", result.operation.0);
                paths.push(target.with_file_name(&name));
                paths.push(target.with_file_name(format!("{name}.retired")));
            }
        }
        result.recovery_material = paths
            .into_iter()
            .filter_map(|path| {
                let presence = if deadline.check().is_err() {
                    RecoveryPresence::Unknown
                } else {
                    match self.io.metadata(&path) {
                        Ok(None) => return None,
                        Ok(Some(_)) => RecoveryPresence::Present,
                        Err(_) => RecoveryPresence::Unknown,
                    }
                };
                Some(RecoveryMaterial { path, presence })
            })
            .collect();
        result
    }

    /// Crash recovery inspects actual payload intent/outcomes. It can finish verification of an
    /// already running new instance; it never replays a mutation or fabricates the lost exit watch.
    pub fn resume(&mut self, input: &RepairInput<'_>) -> Result<RepairResult> {
        if self.run.is_some() {
            return Err(RepairError::RecoveryPending);
        }
        let mut journal = {
            let _lease = self.io.install_lease(input.proof)?;
            Journal::load(&self.io, input)?.ok_or(RepairError::RecoveryPending)?
        };
        journal.check_package(input.package)?;
        let mut result = RepairResult {
            operation: journal.operation(),
            stage: journal.stage(),
            outcome: RepairOutcome::RecoveryRetained,
            resources: Vec::new(),
            activity_retired: true,
            recovery_material: Vec::new(),
            error: None,
        };
        input.deadline.check()?;
        let plan = match self.installer.resume_plan(input.proof, input.package) {
            Ok(plan) if plan.receipt().operation_id == journal.operation() => plan,
            Ok(_) => {
                result.error = Some(NativeError::Foreign.into());
                return Ok(self.with_material(result, input.deadline));
            }
            Err(error) => {
                result.error = Some(error.into());
                return Ok(self.with_material(result, input.deadline));
            }
        };
        input.deadline.check()?;
        result.resources = plan.receipt().resources.clone();
        let service = LinuxService::new(
            self.io.clone(),
            BTreeMap::new(),
            self.installer.rendered_resources(input.package)?,
            input.deadline,
        )?;
        match self.new_health(&service, &journal, input) {
            Ok(true) => {}
            Ok(false) => {
                result.error = Some(RepairError::NotReady);
                return Ok(self.with_material(result, input.deadline));
            }
            Err(error) => {
                result.error = Some(error);
                return Ok(self.with_material(result, input.deadline));
            }
        }
        // No original/clean proof is reconstructed. Only the existing native verification path
        // may retire disposable backups after the new instance's recovery and health admission.
        self.resume_verified(&mut journal, result, input)
    }

    fn resume_verified(
        &self,
        journal: &mut Journal,
        mut result: RepairResult,
        input: &RepairInput<'_>,
    ) -> Result<RepairResult> {
        let reply = input.correlated_reply().ok_or(RepairError::NotReady)?;
        match self.installer.verify(
            input.proof,
            input.package,
            input.expected_reply_id,
            input.now_ms,
            reply,
            input.deadline,
        ) {
            Ok(receipt) => {
                result.resources = receipt.resources;
                let previous = journal.stage();
                journal.advance(RepairStage::Verified);
                result.error = journal.checkpoint(&self.io, input, previous).err();
                result.outcome = if result.error.is_none() {
                    RepairOutcome::Verified
                } else {
                    RepairOutcome::HealthVerifiedCleanupIncomplete
                };
                result.stage = journal.stage();
            }
            Err(error) => {
                let (outcome, resources) = self.after_verify_outcome(journal.operation(), input);
                result.outcome = outcome;
                if let Some(resources) = resources {
                    result.resources = resources;
                }
                result.error = Some(error.into());
            }
        }
        Ok(self.with_material(result, input.deadline))
    }
}
