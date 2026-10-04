use super::super::{native_io::RepairJournalSnapshot, service::AgentEvidence};
use super::resume::Journal;
use super::*;
use crosspane_installer_core::{MutationOutcome, ResourceObservation};

/// Captures one unapplied record; never payload, process or clean-exit authority.
#[derive(Debug)]
pub struct RetireUnapplied {
    snapshot: RepairJournalSnapshot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetirementOutcome {
    Retired,
}

/// The removal caller already owns a genuine ledger and the fixed native snapshot. The record
/// contributes no paths, payload provenance or clean authority; unknown/foreign records stay kept.
pub(crate) fn validate_removal_journal_for_cleanup(
    proof: &super::super::native_io::CleanupProof,
    bytes: &[u8],
) -> Result<()> {
    Journal::validate_cleanup(proof, bytes)
}

impl LinuxRepair {
    fn intact_install(&self, input: &RepairInput<'_>) -> Result<()> {
        input.deadline.check()?;
        input.proof.check(&self.io)?;
        let state = self
            .io
            .target()
            .paths()
            .state_home
            .join("crosspane/installer");
        if self
            .io
            .metadata(&state.join("payload-intent.json"))?
            .is_some()
        {
            return Err(RepairError::RecoveryPending);
        }
        let plan = self.installer.resume_plan(input.proof, input.package)?;
        let receipt = plan.receipt();
        if !receipt.unfinished.is_empty()
            || receipt.resources.iter().any(|r| {
                r.ownership != ResourceOwnership::Created
                    || r.after != ResourceObservation::Matching
                    || r.outcome != MutationOutcome::Verified
            })
        {
            return Err(RepairError::RecoveryPending);
        }
        let current = self.installer.detect(input.proof, input.package)?;
        if current.len() != receipt.resources.len()
            || current.iter().any(|r| {
                r.ownership != ResourceOwnership::Created
                    || r.before != ResourceObservation::Matching
            })
        {
            return Err(RepairError::RecoveryPending);
        }
        input.deadline.check()?;
        input.proof.check(&self.io)?;
        Ok(())
    }
    pub fn inspect_retire_unapplied(&self, input: &RepairInput<'_>) -> Result<RetireUnapplied> {
        if self.run.is_some() {
            return Err(RepairError::RecoveryPending);
        }
        let _lease = self.io.install_lease(input.proof)?;
        let snapshot = self
            .io
            .capture_repair_journal(input.proof, input.deadline)?;
        let journal = Journal::decode(
            &self.io,
            snapshot.bytes().ok_or(RepairError::RecoveryPending)?,
        )?;
        journal.check_package(input.package)?;
        if !matches!(
            journal.stage(),
            RepairStage::Recorded | RepairStage::StopPending | RepairStage::Stopped
        ) {
            return Err(RepairError::RecoveryPending);
        }
        self.intact_install(input)?;
        Ok(RetireUnapplied { snapshot })
    }
    fn retirement_health(&self, input: &RepairInput<'_>) -> Result<()> {
        let facts = input.service.observe(input.deadline)?;
        match input.service.agent(
            &facts,
            input.correlated_reply(),
            input.expected_reply_id,
            input.now_ms,
            None,
            input.deadline,
        )? {
            AgentEvidence::Matched(health) => {
                let h = health.installer();
                use crate::agent_contract::{BackendName::*, BackendState, StartupRecovery};
                let features = &input.package.manifest().members[0].features;
                if h.build.version != input.package.manifest().product_version
                    || &h.build.features != features
                    || !matches!(
                        h.startup_recovery,
                        StartupRecovery::Restored | StartupRecovery::NothingParked
                    )
                    || h.recovery_pending != 0
                    || h.backends.iter().any(|b| b.state == BackendState::Failed)
                    || [
                        Keystore, Links, Parking, Windows, Frames, Capture, Keys, Pointer,
                    ]
                    .iter()
                    .any(|n| {
                        !h.backends
                            .iter()
                            .any(|b| b.name == *n && b.state == BackendState::Ready)
                    })
                {
                    return Err(RepairError::NotReady);
                }
            }
            AgentEvidence::ManagerInactive => {
                // Manager PID=0 is insufficient. Stale bootstrap/socket records remain unknown;
                // do not guess a process absence or fabricate the lost original's exit watch.
                for path in [
                    self.io.target().socket_path(),
                    self.io.target().runtime_dir().join("bootstrap.json"),
                ] {
                    if self.io.metadata(&path)?.is_some() {
                        return Err(RepairError::NotReady);
                    }
                }
                if input.service.observe(input.deadline)? != facts {
                    return Err(RepairError::NotReady);
                }
            }
            _ => return Err(RepairError::NotReady),
        }
        input.deadline.check()?;
        input.proof.check(&self.io)?;
        Ok(())
    }
    pub fn retire_unapplied(
        &mut self,
        intent: RetireUnapplied,
        input: &RepairInput<'_>,
    ) -> Result<RetirementOutcome> {
        if self.run.is_some() {
            return Err(RepairError::RecoveryPending);
        }
        let lease = self.io.install_lease(input.proof)?;
        self.intact_install(input)?;
        self.retirement_health(input)?;
        self.intact_install(input)?;
        // The worker owns the lease and may still unlink after a timeout or cancellation here,
        // and a late deadline can hide a completed unlink: those are unknown, never "unchanged".
        intent
            .snapshot
            .remove(input.proof, lease, input.deadline)
            .map_err(|error| match error {
                NativeError::Timeout | NativeError::Cancelled | NativeError::Unavailable => {
                    NativeError::OutcomeUnknown
                }
                other => other,
            })?;
        Ok(RetirementOutcome::Retired)
    }
}
