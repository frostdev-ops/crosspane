//! Fresh admitted replacement evidence and private cleanup authority.
use super::*;

pub struct PendingPayload {
    pub(super) operation: u64,
    pub(super) manifest: [u8; 32],
    pub(super) original: Option<Arc<OriginalAgent>>,
    pub(super) baseline_instance: Option<u64>,
    pub(super) target: TargetPaths,
    pub(super) previous_app: Tree,
    pub(super) previous_ctl: Tree,
    pub(super) published_at: u64,
    pub(super) health_call: Option<u64>,
    pub(super) phase: PayloadPhase,
    pub(super) repair: Option<Arc<inventory::RepairOrigins>>,
    pub(super) origins: Option<(bool, bool)>,
}
impl PendingPayload {
    pub(crate) fn repair_launch_owned(&self) -> bool {
        self.repair.is_some()
    }
    pub fn phase(&self) -> PayloadPhase {
        self.phase
    }
    /// Caller uses the same monotonic clock as native I/O/AgentReply and issues a new Status call.
    pub fn expect_health(&mut self, id: u64) -> NativeResult<()> {
        if id == 0
            || self.phase != PayloadPhase::Published
            || self.health_call.is_some_and(|last| id <= last)
        {
            return Err(NativeError::Invalid);
        }
        self.health_call = Some(id);
        Ok(())
    }
}
pub struct VerifiedPayload {
    pub receipt: InstallReceipt,
    pub instance_id: u64,
    pub source: ObservationSource,
}

pub(super) struct Replacement<'a> {
    payload: &'a MacPayload,
    selected: &'a SelectedAgent,
    reply: &'a AgentReply,
    app: Tree,
    ctl: Tree,
}
impl Replacement<'_> {
    pub(super) fn check(&self, deadline: &Deadline) -> NativeResult<()> {
        if tree(
            &self.payload.io,
            &self.payload.io.target().app_path(),
            deadline,
        )? != self.app
            || tree(&self.payload.io, &self.payload.ctl(), deadline)? != self.ctl
        {
            return Err(NativeError::Foreign);
        }
        self.selected
            .instance
            .revalidate(&self.selected.io, &self.selected.support, deadline)?;
        let now = self.selected.io.clock().now_ms();
        if self.reply.observed_at_ms > now || now - self.reply.observed_at_ms > SUPPORT_LIFETIME_MS
        {
            return Err(NativeError::Refused);
        }
        deadline.check()
    }
}

impl MacPayload {
    pub fn verify(
        &self,
        pending: &mut PendingPayload,
        selected: &SelectedAgent,
        reply: &AgentReply,
        returned_revision: Option<&str>,
        deadline: &Deadline,
    ) -> NativeResult<VerifiedPayload> {
        let now = selected.io.clock().now_ms();
        if pending.phase != PayloadPhase::Published
            || pending.manifest != self.digest
            || pending.target != *selected.io.target().paths()
            || pending.health_call != Some(reply.id)
            || reply.source != selected.io.target().source()
            || reply.observed_at_ms < pending.published_at
            || reply.observed_at_ms > now
            || now - reply.observed_at_ms > SUPPORT_LIFETIME_MS
        {
            return Err(NativeError::Foreign);
        }
        let DecodedReply::Status(StatusAdmission::Supported(health)) = reply
            .result
            .as_ref()
            .map_err(|_| NativeError::Unavailable)?
        else {
            return Err(NativeError::Unavailable);
        };
        selected
            .instance
            .revalidate(&selected.io, &selected.support, deadline)?;
        selected
            .instance
            .admit_status(&health.installer().instance)?;
        let status = health.installer();
        let mut features = status.build.features.clone();
        features.sort();
        if selected.instance.bootstrap().phase != BootstrapPhase::Ready
            || status.startup_recovery == StartupRecovery::Failed
            || status.recovery_pending != 0
            || status.keystore != KeyStoreProvenance::OsStore
            || status.build.version != self.approved.product_version
            || features != self.approved.features
            || returned_revision.is_some_and(|r| r != status.config_revision)
            || pending.baseline_instance == Some(status.instance.id)
            || pending.original.as_ref().is_some_and(|old| {
                old.instance_id() == status.instance.id || old.node != status.node
            })
        {
            return Err(NativeError::Refused);
        }
        // A restart may intentionally create a runtime. Re-admit the frozen native proofs in that
        // new detector rather than reuse the prior native target's pinned directory identities.
        let current = Self::admit(selected.io.clone(), self.approved.clone(), deadline)?;
        let (app, ctl, matching) = current.installed(deadline)?;
        if !matching {
            return Err(NativeError::Foreign);
        }
        if tree(&current.io, &current.app_previous(), deadline)? != pending.previous_app
            || tree(&current.io, &current.ctl_previous(), deadline)? != pending.previous_ctl
        {
            return Err(NativeError::Foreign);
        }
        let _lock = current.io.lock(&current.support, deadline)?;
        let replacement = Replacement {
            payload: &current,
            selected,
            reply,
            app,
            ctl,
        };
        replacement.check(deadline)?;
        let result: NativeResult<InstallReceipt> = (|| {
            current.remove_tree(
                &current.app_previous(),
                &pending.previous_app,
                &replacement,
                deadline,
            )?;
            current.remove_tree(
                &current.ctl_previous(),
                &pending.previous_ctl,
                &replacement,
                deadline,
            )?;
            replacement.check(deadline)?;
            let record = if pending.repair.is_some() {
                current.repair_receipt(pending.operation, PayloadPhase::Verified)
            } else {
                let (app, ctl) = pending.origins.unwrap_or((
                    pending.previous_app.root.is_some(),
                    pending.previous_ctl.root.is_some(),
                ));
                current.receipt(pending.operation, PayloadPhase::Verified, app, ctl)
            };
            current.persist(&record, deadline)?;
            Ok(record.receipt)
        })();
        match result {
            Ok(receipt) => {
                pending.phase = PayloadPhase::Verified;
                Ok(VerifiedPayload {
                    receipt,
                    instance_id: status.instance.id,
                    source: reply.source,
                })
            }
            Err(_) => {
                pending.phase = PayloadPhase::Unknown;
                Err(NativeError::OutcomeUnknown)
            }
        }
    }
}
