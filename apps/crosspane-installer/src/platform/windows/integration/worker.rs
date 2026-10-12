//! One bounded background owner; no GUI method waits for an operation or thread settlement.
use super::domains::{
    Cold, Dispatch, Domains, ElevatedPlanning, ElevatedRequest, Failure, Handoff, Operation,
    Snapshot,
};
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
    /// The selected operation and whether the fence is cancelled, for the setup log.
    pub fn state(&self) -> (u64, bool) {
        (
            self.current.load(Ordering::Acquire),
            self.cancelled.load(Ordering::Acquire),
        )
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
    pub elevated: Option<ElevatedPlanning>,
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
/// The planning result when the elevated state can't be read.
pub(crate) const ELEVATED_UNCHECKED: &str =
    "The firewall rule and display driver can't be checked right now.";
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
        self.plan_elevated(slot, ticket, operation, None)
    }
    /// An Err from elevated_plan becomes Unavailable("The firewall rule and display driver can't
    /// be checked right now."). A None request never calls elevated_plan.
    pub fn plan_elevated(
        &mut self,
        slot: u16,
        ticket: u64,
        operation: Operation,
        request: Option<ElevatedRequest>,
    ) -> Result<Plan, Failure> {
        let snapshot = self.domains.observe()?;
        let elevated = request.map(|r| {
            self.domains
                .elevated_plan(r)
                .unwrap_or(ElevatedPlanning::Unavailable(ELEVATED_UNCHECKED))
        });
        let plan = Plan {
            ticket,
            operation,
            snapshot,
            elevated,
        };
        self.plans.insert(slot, plan.clone());
        Ok(plan)
    }
    /// Whether the plan in `slot` with `plan_ticket` carries a planned administrator step.
    pub fn elevated_planned(&self, slot: u16, plan_ticket: u64) -> bool {
        self.plans.get(&slot).is_some_and(|plan| {
            plan.ticket == plan_ticket
                && matches!(plan.elevated, Some(ElevatedPlanning::Planned(_)))
        })
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
            return refused("operation fence or revision", Outcome::NotSubmitted);
        }
        let Some(plan) = self.plans.get(&slot).cloned() else {
            return refused("no plan for the step", Outcome::NotSubmitted);
        };
        if plan.ticket != plan_ticket {
            return refused("plan ticket changed", Outcome::NotSubmitted);
        }
        if self.uncertain || self.dispatched.contains_key(&slot) {
            return refused("an earlier submission is unsettled", Outcome::Unknown);
        }
        let current = match self.domains.observe() {
            Ok(value) => value,
            Err(_) => return refused("observation failed", Outcome::NotSubmitted),
        };
        if current != plan.snapshot {
            log_changed(&plan.snapshot, &current);
            return refused("state changed since the plan", Outcome::NotSubmitted);
        }
        if !current.allowed(plan.operation) {
            log_disallowed(&current, plan.operation);
            return refused("operation not allowed in this state", Outcome::NotSubmitted);
        }
        if !fence.admits(ticket) {
            return refused("operation fence", Outcome::NotSubmitted);
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
        let planned = match &plan.elevated {
            Some(ElevatedPlanning::Planned(consent)) => Some(consent),
            _ => None,
        };
        let result = self.domains.apply_elevated(plan.operation, planned);
        match result {
            Ok(Dispatch { handoff, complete }) => {
                // A first-install recovery (rollback or removal of a partial or stale first install)
                // completes in place: there is no keeper to hand off and nothing installed to verify.
                let recovery = matches!(
                    plan.operation,
                    Operation::Removal { .. } | Operation::MetadataRepair
                ) && matches!(plan.snapshot.cold, Cold::Partial | Cold::Stale)
                    && complete
                    && handoff == Handoff::NotCommitted;
                let recovery_removal =
                    recovery && matches!(plan.operation, Operation::Removal { .. });
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
                #[cfg(not(test))]
                if recovery && !complete {
                    crate::platform::windows::setup_log::setup_note!(
                        "first-install recovery submitted but not settled"
                    );
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
            // A declined teardown gate is a refund even when settle archived metadata (L15): the gate
            // runs before any other effect, and that archival is idempotent.
            Err(Failure::NotSubmitted)
                if !settled
                    || (matches!(plan.operation, Operation::Removal { .. })
                        && matches!(plan.elevated, Some(ElevatedPlanning::Planned(_)))) =>
            {
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

/// A refused apply, with its reason in the setup log.
fn refused(why: &str, outcome: Outcome) -> Applied {
    #[cfg(not(test))]
    crate::platform::windows::setup_log::setup_note!("apply refused: {why}");
    #[cfg(test)]
    let _ = why;
    Applied::failure(outcome)
}

/// Logs which snapshot fields differ between the plan and the apply (names and states only).
fn log_changed(planned: &Snapshot, current: &Snapshot) {
    let mut changed = Vec::new();
    if planned.supported != current.supported {
        changed.push(format!(
            "supported {}->{}",
            planned.supported, current.supported
        ));
    }
    if planned.inventory != current.inventory {
        changed.push(format!(
            "inventory {}->{}",
            planned.inventory, current.inventory
        ));
    }
    if planned.sources != current.sources {
        changed.push(format!("sources {}->{}", planned.sources, current.sources));
    }
    if planned.payload != current.payload {
        changed.push(format!(
            "payload {:?}->{:?}",
            planned.payload, current.payload
        ));
    }
    if planned.task != current.task {
        changed.push(format!("task {:?}->{:?}", planned.task, current.task));
    }
    if planned.agent != current.agent {
        changed.push(format!("agent {:?}->{:?}", planned.agent, current.agent));
    }
    if planned.cold != current.cold {
        changed.push(format!("cold {:?}->{:?}", planned.cold, current.cold));
    }
    if planned.terminal_history != current.terminal_history {
        changed.push("terminal_history".to_owned());
    }
    if planned.unsettled != current.unsettled {
        changed.push(format!(
            "unsettled {}->{}",
            planned.unsettled, current.unsettled
        ));
    }
    if planned.correlation != current.correlation {
        changed.push("correlation".to_owned());
    }
    if planned.source != current.source {
        changed.push("source".to_owned());
    }
    #[cfg(not(test))]
    crate::platform::windows::setup_log::setup_note!("snapshot changed: {}", changed.join(", "));
    #[cfg(test)]
    let _ = changed;
}

/// Logs the snapshot facts that `allowed` reads when it refuses an operation.
fn log_disallowed(current: &Snapshot, operation: Operation) {
    #[cfg(not(test))]
    crate::platform::windows::setup_log::setup_note!(
        "not allowed: op={operation:?} supported={} inventory={} sources={} unsettled={} cold={:?} payload={:?} task={:?} agent={:?}",
        current.supported,
        current.inventory,
        current.sources,
        current.unsettled,
        current.cold,
        current.payload,
        current.task,
        current.agent
    );
    #[cfg(test)]
    let _ = (current, operation);
}
