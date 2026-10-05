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
    pub(super) fn remnants(&self) -> bool {
        self.app_stage_present
            || self.ctl_stage_present
            || self.app_previous_present
            || self.ctl_previous_present
            || !self.retained_temporaries.is_empty()
    }
    pub(super) fn unfinished(&self) -> bool {
        self.remnants()
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
                    && (matches!(
                        base,
                        ".Crosspane.app"
                            | "..Crosspane.app.crosspane-stage"
                            | "..Crosspane.app.crosspane-previous"
                            | ".crosspanectl"
                            | "..crosspanectl.crosspane-stage"
                            | "..crosspanectl.crosspane-previous"
                            | ".payload.json"
                    ) || base
                        .strip_prefix(".payload-inventory-")
                        .and_then(|name| name.strip_suffix(".json"))
                        .is_some_and(|hash| {
                            hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())
                        }))
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
        self.persist_inventory(deadline)?;
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

impl MacPayload {
    // Only the private repair context can select this; its genuine prior origins are Created/Absent.
    pub(super) fn repair_receipt(&self, operation: u64, phase: PayloadPhase) -> PayloadRecord {
        self.receipt(operation, phase, false, false)
    }
}

// The receipt is advisory ownership evidence. Signing identities still come only from the
// current embedded inventory; the companion supplies prior byte hashes, never a trust root.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PriorInventory {
    inventory: ApprovedInventory,
}
#[derive(Clone)]
pub(super) struct RecoveryOrigin {
    record: PayloadRecord,
    identity: FileIdentity,
    bytes: Vec<u8>,
    companion: Option<(PathBuf, FileIdentity, Vec<u8>)>,
}
impl RecoveryOrigin {
    pub(super) fn unfinished(&self) -> bool {
        self.record.phase != PayloadPhase::Verified
    }
    pub(super) fn adopted(&self) -> (bool, bool) {
        (
            self.record.receipt.resources[0].ownership == ResourceOwnership::Adopted,
            self.record.receipt.resources[1].ownership == ResourceOwnership::Adopted,
        )
    }
    pub(super) fn check(&self, payload: &MacPayload, deadline: &Deadline) -> NativeResult<()> {
        let path = payload.record_path();
        if payload.io.metadata(&path)? != Some(self.identity.clone())
            || payload.io.read(&path, 64 * 1024, true, deadline)? != self.bytes
            || payload.io.metadata(&path)? != Some(self.identity.clone())
        {
            return Err(NativeError::Foreign);
        }
        if let Some((path, identity, bytes)) = &self.companion
            && (payload.io.metadata(path)? != Some(identity.clone())
                || payload.io.read(path, 256 * 1024, true, deadline)? != *bytes
                || payload.io.metadata(path)? != Some(identity.clone()))
        {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
}
impl MacPayload {
    fn inventory_path(&self, digest: [u8; 32]) -> PathBuf {
        let name: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        self.io
            .target()
            .installer_dir()
            .join(format!("payload-inventory-{name}.json"))
    }
    fn persist_inventory(&self, deadline: &Deadline) -> NativeResult<()> {
        let path = self.inventory_path(self.digest);
        let bytes = serde_json::to_vec(&PriorInventory {
            inventory: self.approved.clone(),
        })
        .map_err(|_| NativeError::Invalid)?;
        if bytes.len() > 256 * 1024 {
            return Err(NativeError::Oversize);
        }
        if self.io.metadata(&path)?.is_some() {
            if self.io.read(&path, 256 * 1024, true, deadline)? != bytes {
                return Err(NativeError::Foreign);
            }
        } else {
            self.io
                .atomic_write(&self.support, &path, &bytes, None, deadline)?;
        }
        Ok(())
    }
    pub(super) fn recovery_origin(
        &self,
        recovery: &RecoveryInventory,
        deadline: &Deadline,
    ) -> NativeResult<Option<Arc<RecoveryOrigin>>> {
        let Some(record) = &recovery.record else {
            return Ok(None);
        };
        if !matches!(
            record.phase,
            PayloadPhase::Published | PayloadPhase::Verified
        ) {
            return Ok(None);
        }
        let receipt = &record.receipt;
        if receipt.operation_id.0 == 0 || receipt.resources.len() != 2 {
            return Ok(None);
        }
        let mut companion = None;
        let approved = if receipt.manifest_sha256 == self.digest {
            self.approved.clone()
        } else {
            let path = self.inventory_path(receipt.manifest_sha256);
            let Some(identity) = self.io.metadata(&path)? else {
                return Ok(None);
            };
            let bytes = self.io.read(&path, 256 * 1024, true, deadline)?;
            let mut prior: PriorInventory =
                serde_json::from_slice(&bytes).map_err(|_| NativeError::Foreign)?;
            prior.inventory.validate()?;
            // Runtime JSON cannot choose a team, requirement, entitlement or code role.
            for file in &prior.inventory.files {
                if let Some(rule) = &file.signing
                    && !self.approved.files.iter().any(|current| {
                        current.path == file.path && current.signing.as_ref() == Some(rule)
                    })
                {
                    return Err(NativeError::Foreign);
                }
            }
            if prior.inventory.digest()? != receipt.manifest_sha256
                || self.io.metadata(&path)? != Some(identity.clone())
            {
                return Err(NativeError::Foreign);
            }
            companion = Some((path, identity, bytes));
            prior.inventory
        };
        let prior = MacPayload {
            io: self.io.clone(),
            approved,
            main: self.main.clone(),
            support: self.support.clone(),
            digest: receipt.manifest_sha256,
        };
        let adopted = |i: usize| match receipt.resources[i].ownership {
            ResourceOwnership::Created => Some(false),
            ResourceOwnership::Adopted => Some(true),
            ResourceOwnership::Foreign => None,
        };
        let (Some(app), Some(ctl)) = (adopted(0), adopted(1)) else {
            return Ok(None);
        };
        if *receipt
            != prior
                .receipt(receipt.operation_id.0, record.phase, app, ctl)
                .receipt
        {
            return Ok(None);
        }
        let installed = match prior.installed(deadline) {
            Ok(observed) => observed,
            // A damaged completed install is still handled by the existing receipt-bound
            // repair path, which checks remaining bytes and genuine Created origins itself.
            // This supplies no ownership or recovery authority to ordinary installation.
            Err(NativeError::Foreign | NativeError::Unsupported)
                if record.phase == PayloadPhase::Verified =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        if !installed.2 {
            return if record.phase == PayloadPhase::Verified {
                Ok(None)
            } else {
                Err(NativeError::Foreign)
            };
        }
        let path = self.record_path();
        let identity = self.io.metadata(&path)?.ok_or(NativeError::Foreign)?;
        let bytes = self.io.read(&path, 64 * 1024, true, deadline)?;
        if serde_json::from_slice::<PayloadRecord>(&bytes).map_err(|_| NativeError::Foreign)?
            != *record
        {
            return Err(NativeError::Foreign);
        }
        let origin = Arc::new(RecoveryOrigin {
            record: record.clone(),
            identity,
            bytes,
            companion,
        });
        origin.check(self, deadline)?;
        Ok(Some(origin))
    }
}

impl MacPayload {
    pub(crate) fn owns_receipt_identity(
        &self,
        version: &str,
        manifest: [u8; 32],
        deadline: &Deadline,
    ) -> NativeResult<bool> {
        Ok(self
            .recovery_origin(&self.recovery(deadline)?, deadline)?
            .is_some_and(|origin| {
                origin.record.receipt.product_version == version
                    && origin.record.receipt.manifest_sha256 == manifest
            }))
    }
}
