//! A recovery hint, never serialized mutation or clean-exit authority.
use super::*;
use serde::{Deserialize, Serialize};

const LIMIT: usize = 64 * 1024;

/// These are coordinator boundaries, not the executor's internal dispatch phases.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairBoundary {
    BeforeStop,
    Stopped,
    Applying,
    Applied,
    HealthWait,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Data {
    schema_version: u32,
    target: [u8; 32],
    inventory: [u8; 32],
    operation: u64,
    revision: u64,
    plan_fingerprint: [u8; 32],
    payload_receipt: [u8; 32],
    launch_receipt: [u8; 32],
    step: RepairBoundary,
    backups: Vec<PathBuf>,
    status_watermark: u64,
}

/// Strictly read hints remain bound to their exact file identity and bytes.
pub struct RepairRecord {
    data: Data,
    identity: FileIdentity,
    bytes: Vec<u8>,
}
impl std::fmt::Debug for RepairRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RepairRecord")
    }
}
impl RepairRecord {
    pub(super) fn data_binding(&self) -> (u64, u64, [u8; 32], [u8; 32], [u8; 32]) {
        (
            self.data.operation,
            self.data.revision,
            self.data.plan_fingerprint,
            self.data.payload_receipt,
            self.data.launch_receipt,
        )
    }
    pub(super) fn check_current(
        &self,
        io: &MacNativeIo,
        inventory: &ApprovedInventory,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        let (target, approved) = bindings(io, inventory)?;
        if self.data.target != target || self.data.inventory != approved {
            return Err(NativeError::Foreign);
        }
        self.check(io, deadline)
    }
    pub fn operation_id(&self) -> u64 {
        self.data.operation
    }
    pub fn boundary(&self) -> RepairBoundary {
        self.data.step
    }
    pub fn status_watermark(&self) -> u64 {
        self.data.status_watermark
    }
    pub fn backups(&self) -> &[PathBuf] {
        &self.data.backups
    }
    fn check(&self, io: &MacNativeIo, deadline: &Deadline) -> NativeResult<()> {
        let path = path(io);
        if io.metadata(&path)? != Some(self.identity.clone())
            || io.read(&path, LIMIT, true, deadline)? != self.bytes
            || io.metadata(&path)? != Some(self.identity.clone())
        {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
}

pub(super) fn path(io: &MacNativeIo) -> PathBuf {
    io.target().installer_dir().join("repair.json")
}
fn digest(bytes: &[u8]) -> [u8; 32] {
    let mut sha = [0; 32];
    sha.copy_from_slice(aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes).as_ref());
    sha
}
fn fingerprint(value: &impl Serialize) -> NativeResult<[u8; 32]> {
    serde_json::to_vec(value)
        .map(|bytes| digest(&bytes))
        .map_err(|_| NativeError::Invalid)
}
fn bindings(io: &MacNativeIo, inventory: &ApprovedInventory) -> NativeResult<([u8; 32], [u8; 32])> {
    let target = io.target();
    let paths = target.paths();
    let mut inventory = inventory.clone();
    inventory.files.sort_by(|a, b| a.path.cmp(&b.path));
    inventory.features.sort();
    Ok((
        fingerprint(&(
            paths.uid,
            &paths.home,
            &paths.gui_tmpdir,
            target.runtime_dir(),
            &paths.payload_root,
        ))?,
        fingerprint(&inventory)?,
    ))
}
fn backup_paths(io: &MacNativeIo, operation: u64) -> [PathBuf; 3] {
    let home = &io.target().paths().home;
    [
        home.join("Applications/.Crosspane.app.crosspane-previous"),
        home.join(".local/bin/.crosspanectl.crosspane-previous"),
        io.target()
            .installer_dir()
            .join(format!("launch-agent-prior-{operation}.plist")),
    ]
}
fn backup_hints(io: &MacNativeIo, operation: u64) -> NativeResult<Vec<PathBuf>> {
    backup_paths(io, operation)
        .into_iter()
        .filter_map(|path| match io.metadata(&path) {
            Ok(Some(_)) => Some(Ok(path)),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

pub(super) fn load(
    io: &MacNativeIo,
    inventory: &ApprovedInventory,
    deadline: &Deadline,
) -> NativeResult<Option<RepairRecord>> {
    deadline.check()?;
    let path = path(io);
    let Some(identity) = io.metadata(&path)? else {
        return Ok(None);
    };
    identity.regular(io.target().paths().uid, true)?;
    if identity.mode & 0o777 != 0o600 {
        return Err(NativeError::Foreign);
    }
    let bytes = io.read(&path, LIMIT, true, deadline)?;
    let data: Data = serde_json::from_slice(&bytes).map_err(|_| NativeError::Invalid)?;
    let (target, approved) = bindings(io, inventory)?;
    let allowed = backup_paths(io, data.operation);
    if data.schema_version != 1
        || data.target != target
        || data.inventory != approved
        || data.operation == 0
        || data.revision == 0
        || data.status_watermark == u64::MAX
        || data.plan_fingerprint == [0; 32]
        || data.payload_receipt == [0; 32]
        || data.launch_receipt == [0; 32]
        || data.backups.len() > allowed.len()
        || data
            .backups
            .iter()
            .enumerate()
            .any(|(i, path)| !allowed.contains(path) || data.backups[..i].contains(path))
        || io.metadata(&path)? != Some(identity.clone())
    {
        return Err(NativeError::Foreign);
    }
    Ok(Some(RepairRecord {
        data,
        identity,
        bytes,
    }))
}

fn proof(
    io: &MacNativeIo,
    inventory: &ApprovedInventory,
    deadline: &Deadline,
) -> NativeResult<SupportProof> {
    let rule = inventory
        .files
        .iter()
        .find(|file| file.path == "Crosspane.app/Contents/MacOS/Crosspane")
        .and_then(|file| file.signing.as_ref())
        .ok_or(NativeError::Invalid)?;
    let requirement = SigningRequirement {
        role: ArtifactRole::Agent,
        identifier: rule.identifier.clone(),
        designated_requirement: rule.designated_requirement.clone(),
        entitlements: rule.entitlements.clone(),
    };
    let main = io.admit_main_signature(
        &io.target()
            .paths()
            .payload_root
            .join("Crosspane.app/Contents/MacOS/Crosspane"),
        &requirement,
        deadline,
    )?;
    io.admit_support(&main, deadline)
}
fn parents(io: &MacNativeIo, proof: &SupportProof, deadline: &Deadline) -> NativeResult<()> {
    let directory = io.target().installer_dir();
    let mut missing = Vec::new();
    let mut cursor = directory.as_path();
    while io.metadata(cursor)?.is_none() {
        if cursor == io.target().paths().home || !cursor.starts_with(&io.target().paths().home) {
            return Err(NativeError::Foreign);
        }
        missing.push(cursor.to_owned());
        cursor = cursor.parent().ok_or(NativeError::Invalid)?;
    }
    for path in missing.into_iter().rev() {
        io.create_directory(proof, &path, 0o700, deadline)?;
    }
    Ok(())
}
fn write(
    io: &MacNativeIo,
    inventory: &ApprovedInventory,
    data: Data,
    expected: Option<&RepairRecord>,
    deadline: &Deadline,
) -> NativeResult<RepairRecord> {
    let proof = proof(io, inventory, deadline)?;
    parents(io, &proof, deadline)?;
    let _lock = io.lock(&proof, deadline)?;
    if let Some(expected) = expected {
        expected.check(io, deadline)?;
    }
    let bytes = serde_json::to_vec(&data).map_err(|_| NativeError::Invalid)?;
    if bytes.len() > LIMIT {
        return Err(NativeError::Oversize);
    }
    let identity = io.atomic_write(
        &proof,
        &path(io),
        &bytes,
        expected.map(|record| &record.identity),
        deadline,
    )?;
    Ok(RepairRecord {
        data,
        identity,
        bytes,
    })
}
pub(super) fn begin(
    io: &MacNativeIo,
    inventory: &ApprovedInventory,
    plan: &RepairPlan,
    watermark: u64,
    deadline: &Deadline,
) -> NativeResult<RepairRecord> {
    // One repair at a time: an earlier window's record is reassessed, never overwritten.
    if load(io, inventory, deadline)?.is_some() {
        return Err(NativeError::Refused);
    }
    let (target, approved) = bindings(io, inventory)?;
    let receipt = |name: &str, limit| {
        io.read(
            &io.target().installer_dir().join(name),
            limit,
            true,
            deadline,
        )
        .map(|bytes| digest(&bytes))
    };
    let payload_receipt = receipt("payload.json", 64 * 1024)?;
    let launch_receipt = receipt("launch-agent.json", 512 * 1024)?;
    let plan_fingerprint = fingerprint(&(
        plan.operation,
        plan.revision,
        &plan.current,
        &plan.config_revision,
        payload_receipt,
        launch_receipt,
    ))?;
    write(
        io,
        inventory,
        Data {
            schema_version: 1,
            target,
            inventory: approved,
            operation: plan.operation,
            revision: plan.revision,
            plan_fingerprint,
            payload_receipt,
            launch_receipt,
            step: RepairBoundary::BeforeStop,
            backups: backup_hints(io, plan.operation)?,
            status_watermark: watermark,
        },
        None,
        deadline,
    )
}
pub(super) fn update(
    io: &MacNativeIo,
    inventory: &ApprovedInventory,
    record: &RepairRecord,
    step: RepairBoundary,
    watermark: u64,
    deadline: &Deadline,
) -> NativeResult<RepairRecord> {
    if watermark < record.status_watermark() || watermark == u64::MAX {
        return Err(NativeError::IdExhausted);
    }
    let mut data = record.data.clone();
    data.step = step;
    data.status_watermark = watermark;
    data.backups = backup_hints(io, data.operation)?;
    write(io, inventory, data, Some(record), deadline)
}
pub(super) fn retire(
    io: &MacNativeIo,
    inventory: &ApprovedInventory,
    record: &RepairRecord,
    deadline: &Deadline,
) -> NativeResult<()> {
    let proof = proof(io, inventory, deadline)?;
    let _lock = io.lock(&proof, deadline)?;
    record.check(io, deadline)?;
    io.remove_owned_leaf(&proof, &path(io), &record.identity, deadline)
}

/// Removal may classify this exact private leaf; it still supplies no cleanup authority.
pub(crate) fn removal_hint(
    io: &MacNativeIo,
    inventory: &ApprovedInventory,
    deadline: &Deadline,
) -> NativeResult<Option<(PathBuf, FileIdentity, [u8; 32])>> {
    Ok(load(io, inventory, deadline)?
        .map(|record| (path(io), record.identity, digest(&record.bytes))))
}
