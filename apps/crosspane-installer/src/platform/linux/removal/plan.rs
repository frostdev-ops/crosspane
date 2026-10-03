use super::*;
use crosspane_installer_core::{
    OperationId, ResourceObservation, ResourceOwnership, ResourceReceipt,
};
use std::sync::{Arc, atomic::Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IdentityChoice {
    #[default]
    Keep,
    DeleteIdentityAndPairings,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanKind {
    Repair,
    Uninstall,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceAction {
    Repair,
    Remove,
    RemoveAfterCleanExit,
    AlreadyAbsent,
    RetainRecovery,
    Retain,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RemovalSelection {
    pub identity: IdentityChoice,
    pub lan_rule: bool,
    pub mdns_rule: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CleanupForm {
    TrackedAwaitingCleanExit,
    /// No correlated original: identity and recovery material remain. Explicit Delete requests
    /// stay pending without CleanAuthority; consent alone cannot manufacture that authority.
    NotCleanRetainIdentityAndRecovery,
}
#[derive(Debug)]
pub struct RemovalPlan {
    pub(super) owner: Arc<()>,
    pub(super) inventory: Inventory,
    revision: u64,
    operation: OperationId,
    kind: PlanKind,
    selection: RemovalSelection,
}
#[derive(Debug)]
pub struct RemovalConsent {
    owner: Arc<()>,
    revision: u64,
    operation: OperationId,
}
impl RemovalPlan {
    pub fn facts(&self) -> &InventoryFacts {
        &self.inventory.facts
    }
    pub fn kind(&self) -> PlanKind {
        self.kind
    }
    pub fn selection(&self) -> RemovalSelection {
        self.selection
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn operation(&self) -> OperationId {
        self.operation
    }
    pub fn cleanup_form(&self) -> CleanupForm {
        if self.inventory.tracked.is_some() {
            CleanupForm::TrackedAwaitingCleanExit
        } else {
            CleanupForm::NotCleanRetainIdentityAndRecovery
        }
    }
    pub fn tracked(&self) -> Option<&Arc<TrackedAgent>> {
        self.inventory.tracked.as_ref()
    }
    pub fn consent(
        &self,
        revision: u64,
        operation: OperationId,
        accept_interruption: bool,
    ) -> Result<RemovalConsent> {
        if revision != self.revision || operation != self.operation {
            return Err(RemovalError::Stale);
        }
        if !accept_interruption {
            return Err(RemovalError::Invalid);
        }
        Ok(RemovalConsent {
            owner: self.owner.clone(),
            revision,
            operation,
        })
    }
    pub fn preview(&self) -> String {
        let identity = if self.selection.identity == IdentityChoice::Keep {
            "Keep this machine's identity, pairings, revocations and their config/state paths."
        } else {
            "Delete this machine's identity and pairings only after matched clean exit. Re-pairing is required; remote offline trust remains."
        };
        let mut result = format!(
            "{:?}: {identity}\nInput, projections and audio may be interrupted; unverified activity remains unknown.\nRecovery tools remain until matched clean exit.\n{:?}\nService: {:?}\nOriginal instance: {:?}; correlation: {:?}\nActivity: {:?}",
            self.kind,
            self.cleanup_form(),
            self.facts().service,
            self.facts().instance_id,
            self.facts().correlation,
            self.facts().activity
        );
        if self.selection.lan_rule || self.selection.mdns_rule {
            result.push_str("\nRule deletion affects every Crosspane user; each exact receipt-bound LAN/mDNS preview needs separate firewall consent.");
        }
        if self.inventory.facts.resources.is_ok() {
            for (r, action) in self.actions() {
                result.push_str(&format!(
                    "\n{}: {:?} ({:?}/{:?})",
                    r.resolved_path, action, r.ownership, r.before
                ));
            }
        } else {
            result.push_str("\nResource ownership is unverified; retain files and recovery tools.");
        }
        result
    }
    /// Compatible repair delta only. Foreign/adopted/unknown and package migrations are retained.
    pub fn repair_delta(&self) -> Vec<&ResourceReceipt> {
        self.actions()
            .into_iter()
            .filter_map(|(r, a)| (a == ResourceAction::Repair).then_some(r))
            .collect()
    }
    /// Planned effects are disclosure, never ownership, clean-exit or executor authority.
    pub fn actions(&self) -> Vec<(&ResourceReceipt, ResourceAction)> {
        self.inventory
            .facts
            .resources
            .as_ref()
            .map(|rows| {
                rows.iter()
                    .map(|r| {
                        let recovery = r.resource_id.starts_with("bin/")
                            || r.resource_id.ends_with(".service");
                        let action = if r.ownership != ResourceOwnership::Created {
                            ResourceAction::Retain
                        } else if self.kind == PlanKind::Repair {
                            if matches!(
                                r.before,
                                ResourceObservation::Absent | ResourceObservation::Different
                            ) {
                                ResourceAction::Repair
                            } else {
                                ResourceAction::Retain
                            }
                        } else {
                            match r.before {
                                ResourceObservation::Absent => ResourceAction::AlreadyAbsent,
                                ResourceObservation::Matching
                                    if recovery && self.inventory.tracked.is_none() =>
                                {
                                    ResourceAction::RetainRecovery
                                }
                                ResourceObservation::Matching if recovery => {
                                    ResourceAction::RemoveAfterCleanExit
                                }
                                ResourceObservation::Matching => ResourceAction::Remove,
                                _ => ResourceAction::Retain,
                            }
                        };
                        (r, action)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}
impl RemovalPlanner {
    pub fn plan(
        &mut self,
        inventory: Inventory,
        revision: u64,
        operation: OperationId,
        kind: PlanKind,
        selection: RemovalSelection,
    ) -> Result<RemovalPlan> {
        if !Arc::ptr_eq(&inventory.owner, &self.owner)
            || inventory.generation != self.generation.load(Ordering::Acquire)
            || !inventory.fresh(inventory.observed_ms)
        {
            return Err(RemovalError::Stale);
        }
        if revision == 0
            || operation.0 == 0
            || revision <= self.last.0
            || operation.0 <= self.last.1
        {
            return Err(RemovalError::Invalid);
        }
        if kind == PlanKind::Repair && selection != RemovalSelection::default() {
            return Err(RemovalError::Invalid);
        }
        self.last = (revision, operation.0);
        Ok(RemovalPlan {
            owner: self.owner.clone(),
            inventory,
            revision,
            operation,
            kind,
            selection,
        })
    }
    /// Call before any executor I/O. A new preview retires coherent old-plan/old-consent pairs.
    pub fn validate(
        &self,
        plan: &RemovalPlan,
        consent: &RemovalConsent,
        current: &Inventory,
        input: InventoryRequest<'_>,
    ) -> Result<()> {
        if !Arc::ptr_eq(&self.owner, &plan.owner)
            || !Arc::ptr_eq(&self.owner, &consent.owner)
            || !Arc::ptr_eq(&self.owner, &current.owner)
            || self.last != (plan.revision, plan.operation.0)
            || plan.inventory.generation <= self.retired.load(Ordering::Acquire)
            || consent.revision != plan.revision
            || consent.operation != plan.operation
            || current.facts != plan.inventory.facts
            || current.generation != self.generation.load(Ordering::Acquire)
            || !current.fresh(input.now_ms)
            || !plan.inventory.fresh(input.now_ms)
        {
            return Err(RemovalError::Stale);
        }
        let fresh = self.inventory(
            input.proof,
            input.package,
            input.service,
            input.reply,
            input.now_ms,
            input.deadline,
        )?;
        if fresh.facts != plan.inventory.facts
            || plan.inventory.generation <= self.retired.load(Ordering::Acquire)
            || fresh.generation != self.generation.load(Ordering::Acquire)
            || !fresh.fresh(input.now_ms)
            || !plan.inventory.fresh(input.now_ms)
        {
            return Err(RemovalError::Stale);
        }
        Ok(())
    }
}
