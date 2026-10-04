//! Cleanup-only plans do not imply support, clean exit or identity-erasure authority.
mod intent;
pub use intent::{CleanupIntent, CleanupProgress, CleanupResult, CleanupStage, CleanupStore};
macro_rules! type_only_debug {
    ($($kind:ident),+ $(,)?) => {
        $(impl std::fmt::Debug for $kind {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(concat!(stringify!($kind), "(..)"))
            }
        })+
    };
}
mod inventory;
mod uninstall;
mod uninstall_firewall;
mod uninstall_resume;
#[cfg(test)]
mod uninstall_tests;
pub(crate) use super::authority::OriginalRunning;
use super::{CleanupForm, RemovalError, RemovalSelection, ResourceAction, Result};
use crate::platform::linux::native_io::{CleanupLease, CleanupProof, Deadline};
use crate::platform::linux::payload::sha256;
use crosspane_installer_core::{OperationId, ResourceObservation, ResourceReceipt};
pub use inventory::{
    CleanupConsent, CleanupInventory, CleanupPlan, CleanupPlanner, CleanupResource,
};
use std::sync::{Arc, Mutex};
pub use uninstall::{
    UninstallConsent, UninstallError, UninstallForm, UninstallIssue, UninstallPlan,
    UninstallPlanner, UninstallReport, UninstallRun, UninstallStage,
};
pub use uninstall_firewall::{UninstallFirewall, UninstallRuleConsent, UninstallRulePlan};

#[derive(Default)]
struct Current(Mutex<Binding>);
#[derive(Default)]
struct Binding {
    last: Option<(u64, OperationId)>,
    active: bool,
    lease: Option<CleanupLease>,
}
type_only_debug!(Current, Binding);
impl Current {
    fn matching(
        &self,
        revision: u64,
        operation: OperationId,
    ) -> Result<std::sync::MutexGuard<'_, Binding>> {
        let current = self.0.try_lock().map_err(|_| RemovalError::Stale)?;
        if !current.active || current.last != Some((revision, operation)) {
            return Err(RemovalError::Stale);
        }
        Ok(current)
    }
}
