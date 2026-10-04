use super::uninstall::*;
use super::*;

fn rank(stage: CleanupStage) -> u8 {
    // Frozen wire discriminants predate the explicit firewall-before-stop ordering ruling.
    match stage {
        CleanupStage::Prepared => 0,
        CleanupStage::Disabled => 1,
        CleanupStage::FirewallObserved => 2,
        CleanupStage::StopObserved => 3,
        CleanupStage::IdentityObserved => 4,
        CleanupStage::FilesObserved => 5,
        CleanupStage::Finished => 6,
    }
}
impl UninstallRun {
    pub(super) fn mark(&mut self, stage: CleanupStage) {
        if rank(stage) > rank(self.progress.stage) {
            self.progress.stage = stage;
        }
    }
    pub(super) fn resumed_attempt(&self, stage: CleanupStage) -> bool {
        self.resume
            .as_ref()
            .is_some_and(|p| rank(p.stage) >= rank(stage))
    }
    pub(super) fn admit_resume(
        &mut self,
        record: CleanupIntent,
        deadline: &Deadline,
    ) -> UninstallResult<()> {
        let selection = self.plan.cleanup.selection();
        if record.revision >= self.plan.cleanup.revision()
            || record.operation.0 >= self.plan.cleanup.operation().0
            || record.delete_identity
                != (selection.identity == super::super::IdentityChoice::DeleteIdentityAndPairings)
            || record.lan_rule != selection.lan_rule
            || record.mdns_rule != selection.mdns_rule
        {
            return Err(UninstallError::Invalid);
        }
        let p = &record.progress;
        let before = |stage| rank(p.stage) < rank(stage);
        if (!record.delete_identity && p.identity != CleanupResult::Kept)
            || (!record.lan_rule && p.lan != CleanupResult::Kept)
            || (!record.mdns_rule && p.mdns != CleanupResult::Kept)
            || (before(CleanupStage::Disabled) && p.autostart != CleanupResult::Pending)
            || (before(CleanupStage::FirewallObserved)
                && ((record.lan_rule && p.lan != CleanupResult::Pending)
                    || (record.mdns_rule && p.mdns != CleanupResult::Pending)))
            || (before(CleanupStage::StopObserved) && p.stop != CleanupResult::Pending)
            || (before(CleanupStage::IdentityObserved)
                && record.delete_identity
                && p.identity != CleanupResult::Pending)
            || (before(CleanupStage::FilesObserved)
                && p.resources.iter().any(|r| *r != CleanupResult::Pending))
            || (rank(p.stage) >= rank(CleanupStage::StopObserved)
                && ((record.lan_rule && p.lan == CleanupResult::Pending)
                    || (record.mdns_rule && p.mdns == CleanupResult::Pending)))
            || (p.stage == CleanupStage::Finished && p.resources.contains(&CleanupResult::Pending))
        {
            return Err(UninstallError::Invalid);
        }
        for (result, row) in p.resources.iter().zip(self.plan.cleanup.resources()) {
            if matches!(
                result,
                CleanupResult::Removed | CleanupResult::AlreadyAbsent
            ) && row.observation != ResourceObservation::Absent
            {
                return Err(UninstallError::Invalid);
            }
        }
        self.plan.original = None;
        self.resume = Some(p.clone());
        self.progress = p.clone();
        // Reconciliation never replays erase, regardless of what the old receipt claims.
        self.progress.identity = CleanupResult::Kept;
        for (selected, result) in [
            (selection.lan_rule, &mut self.progress.lan),
            (selection.mdns_rule, &mut self.progress.mdns),
        ] {
            if selected
                && *result == CleanupResult::Pending
                && rank(p.stage) >= rank(CleanupStage::FirewallObserved)
            {
                *result = CleanupResult::Unknown;
            }
        }
        self.persist(deadline)
    }
}
