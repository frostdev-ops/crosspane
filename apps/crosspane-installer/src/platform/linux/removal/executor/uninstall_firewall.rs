use super::uninstall::*;
use super::*;
use crate::platform::linux::{
    firewall::{
        FirewallConsent, FirewallError, LinuxFirewall, ManagerSelection, Presence, RuleKind,
        RuleResult,
        receipts::{AdmittedReceipt, DurableIntentStore, RemovalPlan},
    },
    native_io::{NativeError, SupportProof},
};

/// Genuine firewall controller, fresh support and its own durable journal; no cleanup-only bypass.
pub struct UninstallFirewall<'a> {
    pub firewall: &'a mut LinuxFirewall,
    pub support: &'a SupportProof,
    pub store: &'a mut DurableIntentStore,
    pub manager: ManagerSelection,
}
pub struct UninstallRulePlan {
    kind: RuleKind,
    operation: OperationId,
    revision: u64,
    owner: Arc<()>,
    plan: RemovalPlan,
}
pub struct UninstallRuleConsent {
    kind: RuleKind,
    operation: OperationId,
    revision: u64,
    owner: Arc<()>,
    consent: FirewallConsent,
}
type_only_debug!(UninstallRulePlan, UninstallRuleConsent);
impl std::fmt::Debug for UninstallFirewall<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UninstallFirewall(..)")
    }
}
impl UninstallRulePlan {
    pub fn preview(&self) -> String {
        self.plan.preview()
    }
    pub fn consent(
        &self,
        operation: OperationId,
        revision: u64,
    ) -> UninstallResult<UninstallRuleConsent> {
        if (operation, revision) != (self.operation, self.revision) {
            return Err(UninstallError::Stale);
        }
        Ok(UninstallRuleConsent {
            kind: self.kind,
            operation,
            revision,
            owner: self.owner.clone(),
            consent: self.plan.consent(operation, revision)?,
        })
    }
}
impl UninstallRun {
    fn persist_firewall(&mut self, deadline: &Deadline) -> UninstallResult<()> {
        // The next fresh stage publishes the in-memory result; an expired budget is never reused.
        if deadline.check().is_ok() {
            self.persist(deadline)?;
        }
        Ok(())
    }
    fn selected(&self, kind: RuleKind) -> bool {
        let selection = self.plan.cleanup.selection();
        match kind {
            RuleKind::Lan => selection.lan_rule,
            RuleKind::Mdns => selection.mdns_rule,
        }
    }
    fn rule(&mut self, kind: RuleKind) -> &mut CleanupResult {
        match kind {
            RuleKind::Lan => &mut self.progress.lan,
            RuleKind::Mdns => &mut self.progress.mdns,
        }
    }
    pub(super) fn rules_done(&mut self) {
        if (!self.selected(RuleKind::Lan) || self.progress.lan != CleanupResult::Pending)
            && (!self.selected(RuleKind::Mdns) || self.progress.mdns != CleanupResult::Pending)
        {
            self.stage = UninstallStage::Stop;
        }
    }
    /// Check literal kind/selection before even detecting a firewall. LAN and mDNS plans are
    /// prepared sequentially because the frozen controller retires its prior plan on detection.
    pub fn prepare_rule(
        &mut self,
        context: &mut UninstallFirewall<'_>,
        kind: RuleKind,
        receipt: AdmittedReceipt,
        operation: OperationId,
        deadline: &Deadline,
    ) -> UninstallResult<UninstallRulePlan> {
        self.guarded(|run| {
            run.require(UninstallStage::Firewall)?;
            if receipt.kind() != kind
                || !run.selected(kind)
                || *run.rule(kind) != CleanupResult::Pending
                || run.rule_pending.is_some()
                || operation.0 <= run.plan.cleanup.operation().0
                || operation.0 <= run.rule_serial
            {
                return Err(UninstallError::Invalid);
            }
            run.rule_serial = operation.0;
            run.mark(CleanupStage::FirewallObserved);
            run.persist_firewall(deadline)?;
            let plan = (|| {
                context.support.check(&run.plan.target)?;
                let snapshot = context.firewall.detect(context.manager, deadline)?;
                context.firewall.plan_removal(
                    &snapshot,
                    receipt,
                    operation,
                    run.plan.cleanup.revision(),
                )
            })();
            let plan = match plan {
                Ok(plan) => plan,
                Err(error) => {
                    run.mark(CleanupStage::FirewallObserved);
                    *run.rule(kind) = if deadline.check().is_err()
                        || matches!(
                            error,
                            FirewallError::Native(
                                NativeError::Timeout
                                    | NativeError::Cancelled
                                    | NativeError::OutcomeUnknown
                            )
                        ) {
                        CleanupResult::Unknown
                    } else {
                        CleanupResult::Refused
                    };
                    run.issues.push(UninstallIssue::Rule(kind, error));
                    run.persist_firewall(deadline)?;
                    run.rules_done();
                    return Err(error.into());
                }
            };
            let owner = Arc::new(());
            run.rule_pending = Some((kind, operation, owner.clone()));
            Ok(UninstallRulePlan {
                kind,
                operation,
                revision: run.plan.cleanup.revision(),
                owner,
                plan,
            })
        })
    }
    pub fn apply_rule(
        &mut self,
        context: &mut UninstallFirewall<'_>,
        plan: UninstallRulePlan,
        consent: UninstallRuleConsent,
        deadline: &Deadline,
    ) -> UninstallResult<()> {
        self.guarded(|run| {
            run.require(UninstallStage::Firewall)?;
            if !run
                .rule_pending
                .as_ref()
                .is_some_and(|(kind, operation, owner)| {
                    (*kind, *operation) == (plan.kind, plan.operation)
                        && Arc::ptr_eq(owner, &plan.owner)
                })
                || !Arc::ptr_eq(&plan.owner, &consent.owner)
                || (plan.kind, plan.operation, plan.revision)
                    != (consent.kind, consent.operation, consent.revision)
            {
                return Err(UninstallError::Stale);
            }
            run.mark(CleanupStage::FirewallObserved);
            run.persist_firewall(deadline)?;
            let kind = plan.kind;
            let outcome = context
                .support
                .check(&run.plan.target)
                .map_err(FirewallError::from)
                .and_then(|_| {
                    context.firewall.apply_removal(
                        context.support,
                        context.manager,
                        plan.plan,
                        consent.consent,
                        context.store,
                        deadline,
                    )
                });
            *run.rule(kind) = match outcome {
                Ok(result)
                    if result.result == RuleResult::Absent
                        && result.inventory == Presence::Absent =>
                {
                    CleanupResult::AlreadyAbsent
                }
                Ok(result) if result.result == RuleResult::Kept => CleanupResult::Kept,
                Ok(result) if result.result == RuleResult::NotDispatched => CleanupResult::Refused,
                Ok(result) => {
                    run.issues.push(UninstallIssue::RuleOutcome(
                        kind,
                        result.result,
                        result.inventory,
                    ));
                    CleanupResult::Unknown
                }
                Err(error) => {
                    run.issues.push(UninstallIssue::Rule(kind, error));
                    if matches!(
                        error,
                        FirewallError::Native(
                            NativeError::OutcomeUnknown
                                | NativeError::Timeout
                                | NativeError::Cancelled
                        )
                    ) {
                        CleanupResult::Unknown
                    } else {
                        CleanupResult::Refused
                    }
                }
            };
            if *run.rule(kind) != CleanupResult::AlreadyAbsent
                && !run
                    .issues
                    .iter()
                    .any(|i| matches!(i, UninstallIssue::Rule(k, _) if *k == kind))
            {
                run.issues
                    .push(UninstallIssue::Rule(kind, FirewallError::CurrentRequired));
            }
            run.rule_pending = None;
            // Never retire the cleanup lease by writing with the exhausted firewall budget.
            // Its prior durable Pending survives; the next fresh stage persists Unknown first.
            run.persist_firewall(deadline)?;
            run.rules_done();
            Ok(())
        })
    }
    /// Explicitly retain a selected rule when support, receipt or separate consent is unavailable.
    /// The rule is named in the partial result; uninstall continues without a generic bypass.
    pub fn retain_rule(
        &mut self,
        kind: RuleKind,
        reason: FirewallError,
        deadline: &Deadline,
    ) -> UninstallResult<()> {
        self.guarded(|run| {
            run.require(UninstallStage::Firewall)?;
            if !run.selected(kind) || *run.rule(kind) != CleanupResult::Pending {
                return Err(UninstallError::Invalid);
            }
            run.mark(CleanupStage::FirewallObserved);
            *run.rule(kind) = CleanupResult::Kept;
            run.issues.push(UninstallIssue::Rule(kind, reason));
            run.rule_pending = None;
            run.persist_firewall(deadline)?;
            run.rules_done();
            Ok(())
        })
    }
}
