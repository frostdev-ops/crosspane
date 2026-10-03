//! Original-instance lifecycle proof; no remote restoration claim.
use super::*;

pub struct OriginalAgent {
    pub(super) selected: SelectedAgent,
    pub(super) node: NodeId,
}
impl OriginalAgent {
    pub fn capture(
        selected: SelectedAgent,
        health: &HealthSnapshot,
        deadline: &Deadline,
    ) -> NativeResult<Self> {
        selected
            .instance
            .revalidate(&selected.io, &selected.support, deadline)?;
        selected
            .instance
            .admit_status(&health.installer().instance)?;
        if health.installer().startup_recovery == StartupRecovery::Failed
            || health.installer().keystore != KeyStoreProvenance::OsStore
        {
            return Err(NativeError::Refused);
        }
        Ok(Self {
            node: health.installer().node,
            selected,
        })
    }
    pub fn instance_id(&self) -> u64 {
        self.selected.instance.bootstrap().instance_id
    }
}
pub struct CleanStopGate {
    original: Arc<OriginalAgent>,
    receipt: crate::agent_contract::LastExitV1,
}
impl CleanStopGate {
    /// Local lifecycle proof only; last_exit does not establish remote projection restoration.
    pub fn observe(
        original: Arc<OriginalAgent>,
        deadline: &Deadline,
    ) -> NativeResult<Option<Self>> {
        let io = &original.selected.io;
        if !io.process_exited(original.selected.instance.process(), deadline)? {
            return Ok(None);
        }
        let Some(receipt) = io.exit_receipt(
            original.selected.instance.process(),
            original.instance_id(),
            deadline,
        )?
        else {
            return Ok(None);
        };
        if !receipt.clean {
            return Ok(None);
        }
        Ok(Some(Self { original, receipt }))
    }
    pub fn instance_id(&self) -> u64 {
        self.original.instance_id()
    }
    pub(super) fn check(
        &self,
        original: &Arc<OriginalAgent>,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        if !Arc::ptr_eq(&self.original, original) {
            return Err(NativeError::Foreign);
        }
        let current = original.selected.io.exit_receipt(
            original.selected.instance.process(),
            self.instance_id(),
            deadline,
        )?;
        if current.as_ref() != Some(&self.receipt) {
            return Err(NativeError::Refused);
        }
        Ok(())
    }
}
