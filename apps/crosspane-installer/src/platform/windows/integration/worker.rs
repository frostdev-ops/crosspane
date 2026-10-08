//! One bounded background owner; no GUI method waits for an operation or thread settlement.
use super::domains::{Cold, Dispatch, Domains, Failure, Handoff, Operation, Snapshot};
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

#[derive(Clone, Default)]
pub(crate) struct Fence {
    current: Arc<AtomicU64>,
    cancelled: Arc<AtomicBool>,
}
impl Fence {
    pub fn select(&self, operation: u64) {
        self.current.store(operation, Ordering::Release);
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
    pub fn admits(&self, operation: u64) -> bool {
        operation != 0
            && !self.cancelled.load(Ordering::Acquire)
            && self.current.load(Ordering::Acquire) == operation
    }
}
#[derive(Clone)]
pub(crate) struct Plan {
    pub ticket: u64,
    pub operation: Operation,
    pub snapshot: Snapshot,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    NotSubmitted,
    Unknown,
    Refused,
    Submitted,
    Verified,
}
impl Outcome {
    pub fn refunds_consent(self) -> bool {
        self == Self::NotSubmitted
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Applied {
    pub outcome: Outcome,
    pub handoff: Handoff,
    pub complete: bool,
}
impl Applied {
    fn failure(outcome: Outcome) -> Self {
        Self {
            outcome,
            handoff: Handoff::Unknown,
            complete: false,
        }
    }
}
pub(crate) struct Coordinator<D> {
    pub domains: D,
    plans: BTreeMap<u16, Plan>,
    dispatched: BTreeMap<u16, u64>,
    uncertain: bool,
}
impl<D: Domains> Coordinator<D> {
    pub fn new(domains: D) -> Self {
        Self {
            domains,
            plans: BTreeMap::new(),
            dispatched: BTreeMap::new(),
            uncertain: false,
        }
    }
    pub fn detect(&mut self) -> Result<Snapshot, Failure> {
        self.domains.observe()
    }
    pub fn plan(&mut self, slot: u16, ticket: u64, operation: Operation) -> Result<Plan, Failure> {
        let snapshot = self.domains.observe()?;
        let plan = Plan {
            ticket,
            operation,
            snapshot,
        };
        self.plans.insert(slot, plan.clone());
        Ok(plan)
    }
    pub fn apply(
        &mut self,
        slot: u16,
        ticket: u64,
        plan_ticket: u64,
        revision: u64,
        fence: &Fence,
    ) -> Applied {
        if !fence.admits(ticket) || revision == 0 {
            return Applied::failure(Outcome::NotSubmitted);
        }
        let Some(plan) = self.plans.get(&slot).cloned() else {
            return Applied::failure(Outcome::NotSubmitted);
        };
        if plan.ticket != plan_ticket {
            return Applied::failure(Outcome::NotSubmitted);
        }
        if self.uncertain || self.dispatched.contains_key(&slot) {
            return Applied::failure(Outcome::Unknown);
        }
        let current = match self.domains.observe() {
            Ok(value) => value,
            Err(_) => return Applied::failure(Outcome::NotSubmitted),
        };
        if current != plan.snapshot || !current.allowed(plan.operation) {
            return Applied::failure(Outcome::NotSubmitted);
        }
        if !fence.admits(ticket) {
            return Applied::failure(Outcome::NotSubmitted);
        }
        // Reserve before either settlement or new mutation. No ambiguous return removes it.
        self.dispatched.insert(slot, ticket);
        self.uncertain = true;
        let settled = match self.domains.settle(plan.operation) {
            Ok(value) => value,
            Err(_) => return Applied::failure(Outcome::Unknown),
        };
        if !fence.admits(ticket) {
            if !settled {
                self.dispatched.remove(&slot);
                self.uncertain = false;
            }
            return Applied::failure(if settled {
                Outcome::Unknown
            } else {
                Outcome::NotSubmitted
            });
        }
        let result = self.domains.apply(plan.operation);
        match result {
            Ok(Dispatch { handoff, complete }) => {
                // A first-install recovery removal completes in place: there is no keeper to hand off.
                let recovery_removal = matches!(plan.operation, Operation::Removal { .. })
                    && matches!(plan.snapshot.cold, Cold::Partial | Cold::Stale)
                    && complete
                    && handoff == Handoff::NotCommitted;
                if matches!(
                    plan.operation,
                    Operation::Upgrade | Operation::PayloadRepair | Operation::Removal { .. }
                ) && !handoff.permits_exit()
                    && !recovery_removal
                {
                    return Applied::failure(Outcome::Unknown);
                }
                let complete = complete && self.verify(plan.operation) == Outcome::Verified;
                if complete {
                    self.uncertain = false;
                }
                Applied {
                    outcome: Outcome::Submitted,
                    handoff: if plan.operation == Operation::MetadataRepair {
                        Handoff::NotCommitted
                    } else {
                        handoff
                    },
                    complete,
                }
            }
            Err(Failure::NotSubmitted) if !settled => {
                self.dispatched.remove(&slot);
                self.uncertain = false;
                Applied::failure(Outcome::NotSubmitted)
            }
            Err(_) => Applied::failure(Outcome::Unknown),
        }
    }
    pub fn verify(&mut self, operation: Operation) -> Outcome {
        if !matches!(self.domains.observe(), Ok(snapshot) if snapshot.source == crosspane_installer_core::ObservationSource::Live)
        {
            return Outcome::Refused;
        }
        match self.domains.verify(operation) {
            Ok(true) => Outcome::Verified,
            Ok(false) | Err(Failure::NotSubmitted) => Outcome::Refused,
            Err(Failure::Unknown) => Outcome::Unknown,
        }
    }
}
