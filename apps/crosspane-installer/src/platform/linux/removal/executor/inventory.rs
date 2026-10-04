use super::*;
type_only_debug!(
    CleanupResource,
    CleanupInventory,
    CleanupPlan,
    CleanupConsent,
    CleanupPlanner
);

#[derive(Clone)]
pub struct CleanupResource {
    pub receipt: ResourceReceipt,
    pub observation: ResourceObservation,
    pub owned: bool,
    pub action: ResourceAction,
}
/// Read-only facts from the genuine captured ledger and descriptor snapshots.
#[derive(Clone)]
pub struct CleanupInventory {
    proof: CleanupProof,
    resources: Vec<CleanupResource>,
    pub(super) digest: [u8; 32],
}
impl CleanupInventory {
    pub fn admit(proof: CleanupProof, deadline: &Deadline) -> Result<Self> {
        proof.revalidate(deadline)?;
        let digest =
            sha256(&serde_json::to_vec(proof.receipt()).map_err(|_| RemovalError::Invalid)?);
        let resources = proof
            .receipt()
            .resources
            .iter()
            .enumerate()
            .map(|(index, receipt)| {
                let observation = proof.observation(index)?;
                let owned = proof.owned(index)?;
                let action = if observation == ResourceObservation::Absent {
                    ResourceAction::AlreadyAbsent
                } else if !owned || observation != ResourceObservation::Matching {
                    ResourceAction::Retain
                } else if receipt.resource_id.starts_with("bin/")
                    || receipt.resource_id == "resources/crosspane-agent.service"
                {
                    ResourceAction::RetainRecovery
                } else {
                    ResourceAction::Remove
                };
                Ok(CleanupResource {
                    receipt: receipt.clone(),
                    observation,
                    owned,
                    action,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        proof.revalidate(deadline)?;
        Ok(Self {
            proof,
            resources,
            digest,
        })
    }
    pub fn resources(&self) -> &[CleanupResource] {
        &self.resources
    }
}
#[derive(Clone)]
pub struct CleanupPlan {
    pub(super) inventory: CleanupInventory,
    pub(super) revision: u64,
    pub(super) operation: OperationId,
    pub(super) selection: RemovalSelection,
    pub(super) current: Arc<Current>,
}
#[derive(Clone)]
pub struct CleanupConsent {
    revision: u64,
    operation: OperationId,
    current: Arc<Current>,
}
/// Retirement keeps the last IDs; neither a cached inventory nor old consent revives them.
#[derive(Default)]
pub struct CleanupPlanner {
    current: Arc<Current>,
}
impl CleanupPlanner {
    pub fn plan(
        &self,
        inventory: CleanupInventory,
        revision: u64,
        operation: OperationId,
        selection: RemovalSelection,
    ) -> Result<CleanupPlan> {
        let mut current = self.current.0.try_lock().map_err(|_| RemovalError::Stale)?;
        if revision == 0
            || operation.0 == 0
            || current
                .last
                .is_some_and(|(r, o)| revision <= r || operation.0 <= o.0)
        {
            return Err(RemovalError::Stale);
        }
        current.last = Some((revision, operation));
        current.active = true;
        current.lease = None;
        Ok(CleanupPlan {
            inventory,
            revision,
            operation,
            selection,
            current: self.current.clone(),
        })
    }
    pub fn consent(
        &self,
        plan: &CleanupPlan,
        revision: u64,
        operation: OperationId,
        deadline: &Deadline,
    ) -> Result<CleanupConsent> {
        if !Arc::ptr_eq(&self.current, &plan.current)
            || revision != plan.revision
            || operation != plan.operation
        {
            return Err(RemovalError::Stale);
        }
        let mut current = self.current.matching(revision, operation)?;
        if let Err(error) = plan.inventory.proof.revalidate(deadline) {
            current.active = false;
            current.lease = None;
            return Err(error.into());
        }
        Ok(CleanupConsent {
            revision,
            operation,
            current: self.current.clone(),
        })
    }
}
impl CleanupPlan {
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn operation(&self) -> OperationId {
        self.operation
    }
    pub fn selection(&self) -> RemovalSelection {
        self.selection
    }
    pub fn form(&self) -> CleanupForm {
        CleanupForm::NotCleanRetainIdentityAndRecovery
    }
    pub fn resources(&self) -> &[CleanupResource] {
        self.inventory.resources()
    }

    // Keep the binding guard through the bounded call and its error retirement.
    // Only trusted executor children may use the lease; public callers never receive it.
    pub(super) fn with_lease<T>(
        &self,
        consent: &CleanupConsent,
        deadline: &Deadline,
        action: impl FnOnce(CleanupLease) -> Result<T>,
    ) -> Result<T> {
        if !Arc::ptr_eq(&self.current, &consent.current)
            || consent.revision != self.revision
            || consent.operation != self.operation
        {
            return Err(RemovalError::Stale);
        }
        let mut current = self.current.matching(self.revision, self.operation)?;
        let result = (|| {
            if current.lease.is_none() {
                current.lease = Some(self.inventory.proof.lease(deadline)?);
            }
            action(current.lease.as_ref().ok_or(RemovalError::Stale)?.clone())
        })();
        if result.is_err() {
            current.active = false;
            current.lease = None;
        }
        result
    }
    pub fn read_intent(
        &self,
        consent: &CleanupConsent,
        deadline: &Deadline,
    ) -> Result<Option<CleanupIntent>> {
        self.with_lease(consent, deadline, |lease| {
            let record = CleanupStore::new(lease).read(deadline)?;
            if record
                .as_ref()
                .is_some_and(|r| r.ledger_digest != self.inventory.digest)
            {
                return Err(RemovalError::Stale);
            }
            Ok(record)
        })
    }
    pub fn write_intent(
        &self,
        consent: &CleanupConsent,
        progress: CleanupProgress,
        deadline: &Deadline,
    ) -> Result<CleanupIntent> {
        let record = CleanupIntent {
            revision: self.revision,
            operation: self.operation,
            ledger_digest: self.inventory.digest,
            delete_identity: self.selection.identity
                == super::super::IdentityChoice::DeleteIdentityAndPairings,
            lan_rule: self.selection.lan_rule,
            mdns_rule: self.selection.mdns_rule,
            progress,
        };
        self.with_lease(consent, deadline, |lease| {
            CleanupStore::new(lease).write(&record, deadline)
        })?;
        Ok(record)
    }
}
