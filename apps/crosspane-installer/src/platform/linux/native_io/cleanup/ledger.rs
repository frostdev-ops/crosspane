use super::*;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    receipt: InstallReceipt,
    items: Vec<Item>,
    phase: String,
    source: ObservationSource,
    previous_instance: Option<u64>,
    base_generation: Option<[u8; 32]>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Item {
    old: Option<[u8; 32]>,
    new: [u8; 32],
    template: [u8; 32],
    ownership: ResourceOwnership,
    replacement: Option<Replacement>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Replacement {
    file: (u64, u64),
    parent: (u64, u64),
    hash: [u8; 32],
    mode: u32,
}
fn object(value: &Value, keys: &str) -> Result<()> {
    let map = value.as_object().ok_or(NativeError::Foreign)?;
    if map.len() != keys.split_whitespace().count()
        || keys.split_whitespace().any(|key| !map.contains_key(key))
    {
        return Err(NativeError::Foreign);
    }
    Ok(())
}
impl LinuxNativeIo {
    /// No receipt grants deletion by itself: every path is independently opened and hashed.
    pub fn admit_cleanup(self: &Arc<Self>, deadline: &Deadline) -> Result<CleanupProof> {
        let io = self.clone();
        let d = deadline.clone();
        bounded_launch(&READ_WORKERS, deadline, move || {
            io.validate_target()?;
            let paths = PayloadInstaller::new(io.clone())
                .map_err(|_| NativeError::Foreign)?
                .targets()
                .to_vec();
            let path = io
                .target
                .paths
                .state_home
                .join("crosspane/installer/payload-outcome.json");
            let (snapshot, bytes) = Snapshot::open(&io, &path, 0o600, MAX_RECORD_BYTES, &d)?;
            if snapshot.hash().is_none() {
                return Err(NativeError::Unavailable);
            }
            let value: Value = serde_json::from_slice(&bytes).map_err(|_| NativeError::Foreign)?;
            object(
                &value,
                "receipt items phase source previous_instance base_generation",
            )?;
            object(
                &value["receipt"],
                "schema_version operation_id product_version manifest_sha256 payload_sha256 resources unfinished",
            )?;
            for item in value["items"].as_array().ok_or(NativeError::Foreign)? {
                object(item, "old new template ownership replacement")?;
                if !item["ownership"].is_string() {
                    return Err(NativeError::Foreign);
                }
                if !item["replacement"].is_null() {
                    object(&item["replacement"], "file parent hash mode")?;
                }
            }
            for row in value["receipt"]["resources"]
                .as_array()
                .ok_or(NativeError::Foreign)?
            {
                object(
                    row,
                    "resource_id resolved_path ownership before after outcome",
                )?;
                if ["ownership", "before", "after", "outcome"]
                    .iter()
                    .any(|key| !row[*key].is_string())
                {
                    return Err(NativeError::Foreign);
                }
            }
            if !value["source"].is_string() {
                return Err(NativeError::Foreign);
            }
            let ledger: Ledger =
                serde_json::from_slice(&bytes).map_err(|_| NativeError::Foreign)?;
            let _ = (ledger.previous_instance, ledger.base_generation);
            if ledger.phase != "Verified"
                || ledger.source != io.target.source()
                || ledger.receipt.schema_version != 1
                || ledger.receipt.operation_id.0 == 0
                || ledger.receipt.product_version.is_empty()
                || ledger.receipt.product_version.len() > 128
                || ledger.items.len() != FILES.len()
                || ledger.receipt.resources.len() != FILES.len()
                || !ledger.receipt.unfinished.is_empty()
            {
                return Err(NativeError::Foreign);
            }
            let mut entries = Vec::new();
            for (index, (item, row)) in ledger
                .items
                .iter()
                .zip(&ledger.receipt.resources)
                .enumerate()
            {
                let mode = if index < 5 { 0o755 } else { 0o644 };
                if row.resource_id != FILES[index]
                    || Path::new(&row.resolved_path) != paths[index]
                    || item.ownership != row.ownership
                    || row.after != ResourceObservation::Matching
                    || row.outcome != MutationOutcome::Verified
                    || (item.ownership == ResourceOwnership::Created
                        && item.replacement.is_none()
                        && (item.old != Some(item.new)
                            || row.before != ResourceObservation::Matching))
                    || item
                        .replacement
                        .as_ref()
                        .is_some_and(|r| r.hash != item.new || r.mode != mode)
                {
                    return Err(NativeError::Foreign);
                }
                let _ = (item.old, item.template);
                let (snapshot, _) = Snapshot::open(&io, &paths[index], mode, MAX_MEMBER_BYTES, &d)?;
                let owned = item.ownership == ResourceOwnership::Created
                    && match (&snapshot.file_pair(), &item.replacement) {
                        (Some(pair), Some(r)) => {
                            *pair == r.file
                                && snapshot.parent_pair()? == r.parent
                                && snapshot.hash() == Some(r.hash)
                        }
                        // Absent-owned grants nothing to delete; a2 must freshly establish presence.
                        (None, Some(r)) => snapshot.parent_pair()? == r.parent,
                        _ => false,
                    };
                entries.push(Resource {
                    snapshot,
                    hash: item.new,
                    owned,
                });
            }
            // A journal that isn't a private regular file of the expected shape is kept and
            // reported as unknown; it never blocks removal and grants nothing.
            let (repair, repair_foreign) =
                match Snapshot::open(&io, &repair::fixed_path(&io), 0o600, MAX_RECORD_BYTES, &d) {
                    Ok((repair, _)) => (Some(repair), false),
                    Err(NativeError::Foreign) => (None, true),
                    Err(error) => return Err(error),
                };
            let proof = CleanupProof(Arc::new(Admitted {
                io,
                ledger: snapshot,
                repair,
                repair_foreign,
                receipt: ledger.receipt,
                entries: Mutex::new(entries),
            }));
            proof.check(&d)?;
            Ok(proof)
        })
    }
}
