//! Advisory receipts, interrupted-state detection and owned backup cleanup.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PayloadPhase {
    Intent,
    Staged,
    Published,
    Verified,
    Unknown,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayloadRecord {
    pub phase: PayloadPhase,
    pub receipt: InstallReceipt,
}
pub struct RecoveryInventory {
    pub record: Option<PayloadRecord>,
    pub app_present: bool,
    pub ctl_present: bool,
    pub app_stage_present: bool,
    pub ctl_stage_present: bool,
    pub app_previous_present: bool,
    pub ctl_previous_present: bool,
    /// Native quarantine/publication remnants: retained for inspection, never deletion authority.
    pub retained_temporaries: Vec<PathBuf>,
}
impl RecoveryInventory {
    pub(super) fn unfinished(&self) -> bool {
        self.app_stage_present
            || self.ctl_stage_present
            || self.app_previous_present
            || self.ctl_previous_present
            || !self.retained_temporaries.is_empty()
            || self
                .record
                .as_ref()
                .is_some_and(|r| r.phase != PayloadPhase::Verified)
    }
}

impl MacPayload {
    pub fn recovery(&self, deadline: &Deadline) -> NativeResult<RecoveryInventory> {
        let mut retained_temporaries = Vec::new();
        for parent in [
            self.io.target().paths().home.join("Applications"),
            self.io.target().paths().home.join(".local/bin"),
            self.io.target().installer_dir(),
        ] {
            if self.io.metadata(&parent)?.is_none() {
                continue;
            }
            for (name, _) in self.io.entries(&parent, 4096, deadline)? {
                if let Some((base, nonce)) = name.rsplit_once(".crosspane-temp-")
                    && matches!(
                        base,
                        ".Crosspane.app"
                            | "..Crosspane.app.crosspane-stage"
                            | "..Crosspane.app.crosspane-previous"
                            | ".crosspanectl"
                            | "..crosspanectl.crosspane-stage"
                            | "..crosspanectl.crosspane-previous"
                            | ".payload.json"
                    )
                    && nonce.len() == 32
                    && nonce.bytes().all(|b| b.is_ascii_hexdigit())
                {
                    if retained_temporaries.len() == 64 {
                        return Err(NativeError::Oversize);
                    }
                    retained_temporaries.push(parent.join(name));
                }
            }
        }
        let record = if self.io.metadata(&self.record_path())?.is_some() {
            let record: PayloadRecord = serde_json::from_slice(&self.io.read(
                &self.record_path(),
                64 * 1024,
                true,
                deadline,
            )?)
            .map_err(|_| NativeError::Invalid)?;
            if record.receipt.schema_version != 1
                || record.receipt.resources.len() > 2
                || record.receipt.unfinished.len() > 1
            {
                return Err(NativeError::Invalid);
            }
            Some(record)
        } else {
            None
        };
        Ok(RecoveryInventory {
            retained_temporaries,
            record,
            app_present: self.io.metadata(&self.io.target().app_path())?.is_some(),
            ctl_present: self.io.metadata(&self.ctl())?.is_some(),
            app_stage_present: self.io.metadata(&self.app_stage())?.is_some(),
            ctl_stage_present: self.io.metadata(&self.ctl_stage())?.is_some(),
            app_previous_present: self.io.metadata(&self.app_previous())?.is_some(),
            ctl_previous_present: self.io.metadata(&self.ctl_previous())?.is_some(),
        })
    }
    pub(super) fn receipt(
        &self,
        operation: u64,
        phase: PayloadPhase,
        app_before: bool,
        ctl_before: bool,
    ) -> PayloadRecord {
        let outcome = if phase == PayloadPhase::Verified {
            MutationOutcome::Verified
        } else {
            MutationOutcome::Unknown
        };
        let resource = |id: &str, path: PathBuf, existed| ResourceReceipt {
            resource_id: id.into(),
            resolved_path: path.to_string_lossy().into_owned(),
            ownership: if existed {
                ResourceOwnership::Adopted
            } else {
                ResourceOwnership::Created
            },
            before: if existed {
                ResourceObservation::Different
            } else {
                ResourceObservation::Absent
            },
            after: if phase == PayloadPhase::Verified {
                ResourceObservation::Matching
            } else {
                ResourceObservation::Unknown
            },
            outcome,
        };
        PayloadRecord {
            phase,
            receipt: InstallReceipt {
                schema_version: 1,
                operation_id: OperationId(operation),
                product_version: self.approved.product_version.clone(),
                manifest_sha256: self.digest,
                payload_sha256: self.approved.payload_digest(),
                resources: vec![
                    resource("mac.app", self.io.target().app_path(), app_before),
                    resource("mac.ctl", self.ctl(), ctl_before),
                ],
                unfinished: if phase == PayloadPhase::Verified {
                    vec![]
                } else {
                    vec![StepId(12)]
                },
            },
        }
    }
    pub(super) fn persist(&self, record: &PayloadRecord, deadline: &Deadline) -> NativeResult<()> {
        let path = self.record_path();
        let prior = self.io.metadata(&path)?;
        let bytes = serde_json::to_vec(record).map_err(|_| NativeError::Invalid)?;
        self.io
            .atomic_write(&self.support, &path, &bytes, prior.as_ref(), deadline)?;
        Ok(())
    }
    pub(super) fn remove_tree(
        &self,
        path: &Path,
        expected: &Tree,
        replacement: &Replacement<'_>,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        let Some(root) = &expected.root else {
            return Ok(());
        };
        if tree(&self.io, path, deadline)? != *expected {
            return Err(NativeError::Foreign);
        }
        let mut entries: Vec<_> = expected.nodes.iter().collect();
        entries.sort_by(|(a, _), (b, _)| {
            b.split('/')
                .count()
                .cmp(&a.split('/').count())
                .then_with(|| b.cmp(a))
        });
        for (name, old) in entries {
            replacement.check(deadline)?;
            let child = path.join(name);
            let identity = self.io.metadata(&child)?.ok_or(NativeError::Foreign)?;
            if !same_object(old, &identity)
                || (identity.mode & 0o170000 == 0o100000 && *old != identity)
            {
                return Err(NativeError::Foreign);
            }
            self.io
                .remove_owned_leaf(&self.support, &child, &identity, deadline)?;
        }
        let identity = self.io.metadata(path)?.ok_or(NativeError::Foreign)?;
        if !same_object(root, &identity)
            || (identity.mode & 0o170000 == 0o100000 && *root != identity)
        {
            return Err(NativeError::Foreign);
        }
        replacement.check(deadline)?;
        // Accepted same-UID residual: drift during native quarantine/unlink is undetected
        // and may lose a backup or ctl copy; checks up to this native call remain required.
        self.io
            .remove_owned_leaf(&self.support, path, &identity, deadline)
    }
}
