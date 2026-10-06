//! Strict journal correlation and interruption decisions; deserialization is never authority.
use super::super::native_io::{NativeError, NativeResult, files::FileIdentity};
use super::inventory::{PayloadRole, PeFacts};
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Phase {
    Intent,
    StopIntent,
    ImageReleased,
    StageIntent,
    Staged,
    BackupIntent,
    BackedUp,
    PublishIntent,
    Published,
    HandoffIntent,
    HelperDispatched,
    ParentExited,
    StartIntent,
    NewInstanceObserved,
    Verified,
    PruneIntent,
    RollbackIntent,
    RolledBack,
    Complete,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileStamp {
    pub volume: u64,
    pub file: [u8; 16],
}
impl From<FileIdentity> for FileStamp {
    fn from(value: FileIdentity) -> Self {
        Self {
            volume: value.volume,
            file: value.file,
        }
    }
}
impl FileStamp {
    pub(crate) fn valid(self) -> bool {
        self.volume != 0 && self.file != [0; 16]
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImageObservation {
    pub identity: FileStamp,
    pub facts: PeFacts,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum OriginalLeaf {
    Unobserved,
    Missing,
    Present(FileStamp),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RoleState {
    pub role: PayloadRole,
    pub original: OriginalLeaf,
    pub staged: Option<ImageObservation>,
    pub backup: Option<FileStamp>,
    pub published: Option<ImageObservation>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OperationRecord {
    schema_version: u32,
    operation: [u8; 16],
    phase: Phase,
    current_role: Option<PayloadRole>,
    roles: Vec<RoleState>,
    original_instance: Option<String>,
    new_instance: Option<String>,
    handoff: Option<super::helper::HandoffRecord>,
    retention_incomplete: bool,
}
impl OperationRecord {
    // A4b/A5 starts new operations and reports retained recovery; current native entry only reopens.
    #[allow(dead_code)]
    pub(crate) fn new(operation: [u8; 16]) -> NativeResult<Self> {
        if operation == [0; 16] {
            return Err(NativeError::Invalid);
        }
        Ok(Self {
            schema_version: 1,
            operation,
            phase: Phase::Intent,
            current_role: None,
            roles: PayloadRole::ALL
                .into_iter()
                .map(|role| RoleState {
                    role,
                    original: OriginalLeaf::Unobserved,
                    staged: None,
                    backup: None,
                    published: None,
                })
                .collect(),
            original_instance: None,
            new_instance: None,
            handoff: None,
            retention_incomplete: false,
        })
    }
    pub(crate) fn operation(&self) -> [u8; 16] {
        self.operation
    }
    pub(crate) fn phase(&self) -> Phase {
        self.phase
    }
    pub(crate) fn current_role(&self) -> Option<PayloadRole> {
        self.current_role
    }
    pub(crate) fn handoff(&self) -> Option<&super::helper::HandoffRecord> {
        self.handoff.as_ref()
    }
    pub(crate) fn set_handoff(&mut self, value: super::helper::HandoffRecord) -> NativeResult<()> {
        value.validate()?;
        self.handoff = Some(value);
        Ok(())
    }
    pub(crate) fn set_phase(&mut self, phase: Phase) {
        self.phase = phase;
        self.current_role = None;
    }
    // A4b/A5 starts new operations and reports retained recovery; current native entry only reopens.
    #[allow(dead_code)]
    pub(crate) fn roles(&self) -> &[RoleState] {
        &self.roles
    }
    pub(crate) fn original_instance(&self) -> Option<&str> {
        self.original_instance.as_deref()
    }
    pub(crate) fn new_instance(&self) -> Option<&str> {
        self.new_instance.as_deref()
    }
    // A4b/A5 starts new operations and reports retained recovery; current native entry only reopens.
    #[allow(dead_code)]
    pub(crate) fn retention_incomplete(&self) -> bool {
        self.retention_incomplete
    }
    pub(crate) fn validate(&self) -> NativeResult<()> {
        if self.schema_version != 1
            || self.operation == [0; 16]
            || self.roles.len() != 4
            || PayloadRole::ALL
                .iter()
                .any(|role| self.roles.iter().filter(|row| row.role == *role).count() != 1)
            || [
                self.original_instance.as_deref(),
                self.new_instance.as_deref(),
            ]
            .into_iter()
            .flatten()
            .any(|id| {
                id.len() != 32
                    || !id
                        .bytes()
                        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                    || u64::from_str_radix(id, 16).is_err()
                    || u64::from_str_radix(id, 16) == Ok(0)
            })
        {
            return Err(NativeError::Invalid);
        }
        for role in &self.roles {
            for image in [&role.staged, &role.published].into_iter().flatten() {
                if !image.identity.valid() || !image.facts.valid() {
                    return Err(NativeError::Invalid);
                }
            }
            if matches!(role.original,OriginalLeaf::Present(stamp) if !stamp.valid()) {
                return Err(NativeError::Invalid);
            }
            if role.backup.is_some_and(|stamp| !stamp.valid()) {
                return Err(NativeError::Invalid);
            }
        }
        if let Some(handoff) = &self.handoff {
            handoff.validate()?;
        }
        Ok(())
    }
    pub(crate) fn advance(&mut self, phase: Phase, role: Option<PayloadRole>) {
        self.phase = phase;
        self.current_role = role;
    }
    pub(crate) fn role(&self, role: PayloadRole) -> NativeResult<&RoleState> {
        self.roles
            .iter()
            .find(|row| row.role == role)
            .ok_or(NativeError::Invalid)
    }
    pub(crate) fn role_mut(&mut self, role: PayloadRole) -> NativeResult<&mut RoleState> {
        self.roles
            .iter_mut()
            .find(|row| row.role == role)
            .ok_or(NativeError::Invalid)
    }
    pub(crate) fn set_original_instance(&mut self, id: String) {
        self.original_instance = Some(id);
    }
    pub(crate) fn set_new_instance(&mut self, id: String) {
        self.new_instance = Some(id);
    }
    pub(crate) fn set_retention_incomplete(&mut self, value: bool) {
        self.retention_incomplete = value;
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BackupGeneration {
    pub operation: [u8; 16],
    pub sequence: u64,
    pub completed: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StageCatalog {
    schema_version: u32,
    pub active: Option<[u8; 16]>,
    pub generations: Vec<BackupGeneration>,
}
impl Default for StageCatalog {
    fn default() -> Self {
        Self {
            schema_version: 1,
            active: None,
            generations: Vec::new(),
        }
    }
}
impl StageCatalog {
    pub(crate) fn validate(&self) -> NativeResult<()> {
        if self.schema_version != 1
            || self.active == Some([0; 16])
            || self.generations.len() > 1024
            || self
                .generations
                .iter()
                .any(|g| g.operation == [0; 16] || g.sequence == 0)
            || self.generations.iter().enumerate().any(|(i, g)| {
                self.generations[..i]
                    .iter()
                    .any(|old| old.operation == g.operation || old.sequence == g.sequence)
            })
        {
            return Err(NativeError::Invalid);
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecoveryDecision {
    // Portable phase-classification view; actual resume executes the explicit recovery branches.
    #[allow(dead_code)]
    Nothing,
    // Portable phase-classification view; actual resume executes the explicit recovery branches.
    #[allow(dead_code)]
    ResumeFiles,
    // Portable phase-classification view; actual resume executes the explicit recovery branches.
    #[allow(dead_code)]
    RollbackStage,
    // Portable phase-classification view; actual resume executes the explicit recovery branches.
    #[allow(dead_code)]
    VerifyOnly,
    RecoveryRequired,
    Complete,
}
/// A dispatched stop/start/helper operation can never be replayed from a journal claim.
// Portable classification tested alongside the actual executable resume/rollback driver.
#[allow(dead_code)]
pub(crate) fn recovery_decision(record: &OperationRecord) -> RecoveryDecision {
    match record.phase {
        Phase::Intent | Phase::RollbackIntent => RecoveryDecision::RollbackStage,
        Phase::StopIntent | Phase::Unknown | Phase::HandoffIntent | Phase::HelperDispatched => {
            RecoveryDecision::RecoveryRequired
        }
        Phase::ImageReleased
        | Phase::StageIntent
        | Phase::Staged
        | Phase::BackupIntent
        | Phase::BackedUp
        | Phase::PublishIntent
        | Phase::Published
        | Phase::ParentExited => RecoveryDecision::ResumeFiles,
        Phase::StartIntent | Phase::NewInstanceObserved => RecoveryDecision::VerifyOnly,
        Phase::Verified | Phase::PruneIntent => RecoveryDecision::ResumeFiles,
        Phase::Complete | Phase::RolledBack => RecoveryDecision::Complete,
    }
}
/// Active/incomplete generations and unremovable old material are never force-deleted.
pub(crate) fn prune_candidates(catalog: &StageCatalog) -> NativeResult<Vec<[u8; 16]>> {
    catalog.validate()?;
    let mut complete: Vec<_> = catalog.generations.iter().filter(|g| g.completed).collect();
    complete.sort_by_key(|g| std::cmp::Reverse(g.sequence));
    Ok(complete
        .into_iter()
        .skip(3)
        .filter(|g| Some(g.operation) != catalog.active)
        .map(|g| g.operation)
        .collect())
}
/// Metadata retirement only; the native caller separately requires a sealed positive absence.
pub(crate) fn retire_completed(
    catalog: &mut StageCatalog,
    operation: [u8; 16],
) -> NativeResult<()> {
    if !prune_candidates(catalog)?.contains(&operation) {
        return Err(NativeError::Foreign);
    }
    catalog
        .generations
        .retain(|generation| generation.operation != operation);
    catalog.validate()
}
#[cfg(windows)]
mod native {
    use super::super::super::native_io::{
        Deadline, InstallerLock, SupportProof, WindowsNativeIo,
        records::{self, PublicationRecovery, RecordName},
    };
    use super::*;
    use std::sync::Arc;
    pub(crate) struct MutationPermit {
        io: Arc<WindowsNativeIo>,
        operation: [u8; 16],
        phase: Phase,
        role: Option<PayloadRole>,
        bytes: Vec<u8>,
    }
    impl MutationPermit {
        pub(crate) fn io(&self) -> &Arc<WindowsNativeIo> {
            &self.io
        }
        pub(crate) fn operation(&self) -> [u8; 16] {
            self.operation
        }
        pub(crate) fn phase(&self) -> Phase {
            self.phase
        }
        pub(crate) fn role(&self) -> Option<PayloadRole> {
            self.role
        }
        pub(crate) fn bytes(&self) -> &[u8] {
            &self.bytes
        }
    }
    fn publish(
        io: &WindowsNativeIo,
        proof: &SupportProof,
        lock: &InstallerLock,
        name: RecordName,
        bytes: &[u8],
        deadline: &Deadline,
    ) -> NativeResult<()> {
        let outcome = io.publish_record(proof, lock, name, bytes, deadline)?;
        if outcome.state != PublicationRecovery::NewPublished || outcome.native_failure.is_some() {
            return Err(NativeError::OutcomeUnknown);
        }
        Ok(())
    }
    pub(crate) fn catalog(
        io: &WindowsNativeIo,
        proof: &SupportProof,
        deadline: &Deadline,
    ) -> NativeResult<StageCatalog> {
        let catalog = match io.read_record(
            proof,
            RecordName::StageCatalog,
            super::super::super::native_io::files::MAX_RECORD_BYTES,
            deadline,
        )? {
            Some(record) => records::record_data(&RecordName::StageCatalog, record.bytes())?,
            None => StageCatalog::default(),
        };
        catalog.validate()?;
        Ok(catalog)
    }
    pub(crate) fn selected_operation(
        io: &WindowsNativeIo,
        proof: &SupportProof,
        deadline: &Deadline,
    ) -> NativeResult<Option<OperationRecord>> {
        let Some(id) = catalog(io, proof, deadline)?.active else {
            return Ok(None);
        };
        let name = RecordName::Operation(id);
        let bytes = io
            .read_record(
                proof,
                name.clone(),
                super::super::super::native_io::files::MAX_RECORD_BYTES,
                deadline,
            )?
            .ok_or(NativeError::Unavailable)?;
        let record: OperationRecord = records::record_data(&name, bytes.bytes())?;
        record.validate()?;
        if record.operation() != id {
            return Err(NativeError::Foreign);
        }
        Ok(Some(record))
    }
    pub(crate) fn active_helper_operation(
        io: &WindowsNativeIo,
        proof: &SupportProof,
        deadline: &Deadline,
    ) -> NativeResult<Option<OperationRecord>> {
        let Some(record) = selected_operation(io, proof, deadline)? else {
            return Ok(None);
        };
        if record.handoff().is_none()
            || !matches!(
                record.phase(),
                Phase::HandoffIntent | Phase::HelperDispatched | Phase::ParentExited
            )
        {
            return Err(NativeError::Unsupported);
        }
        Ok(Some(record))
    }
    pub(crate) fn save_operation(
        io: Arc<WindowsNativeIo>,
        proof: &SupportProof,
        lock: &InstallerLock,
        record: &OperationRecord,
        deadline: &Deadline,
    ) -> NativeResult<MutationPermit> {
        record.validate()?;
        let mut current = catalog(&io, proof, deadline)?;
        if current.active.is_some_and(|id| id != record.operation()) {
            return Err(NativeError::Busy);
        }
        let bytes = records::encode_record(
            &RecordName::Operation(record.operation()),
            serde_json::to_value(record).map_err(|_| NativeError::Invalid)?,
        )?;
        if current.active.is_none() {
            let previous = io.read_record(
                proof,
                RecordName::Operation(record.operation()),
                super::super::super::native_io::files::MAX_RECORD_BYTES,
                deadline,
            )?;
            if matches!(record.phase(), Phase::Complete | Phase::RolledBack)
                && previous
                    .as_ref()
                    .is_some_and(|old| old.bytes() == bytes.as_slice())
            {
                return Ok(MutationPermit {
                    io,
                    operation: record.operation(),
                    phase: record.phase(),
                    role: record.current_role(),
                    bytes,
                });
            }
            if record.phase() != Phase::Intent {
                return Err(NativeError::Foreign);
            }
            if previous.is_some()
                || current
                    .generations
                    .iter()
                    .any(|g| g.operation == record.operation())
            {
                return Err(NativeError::Busy);
            }
        }
        publish(
            &io,
            proof,
            lock,
            RecordName::Operation(record.operation()),
            &bytes,
            deadline,
        )?;
        if current.active.is_none() {
            current.active = Some(record.operation());
            let sequence = current
                .generations
                .iter()
                .map(|g| g.sequence)
                .max()
                .unwrap_or(0)
                .checked_add(1)
                .ok_or(NativeError::Invalid)?;
            current.generations.push(BackupGeneration {
                operation: record.operation(),
                sequence,
                completed: false,
            });
        }
        if matches!(
            record.phase(),
            Phase::Verified | Phase::PruneIntent | Phase::Complete
        ) {
            let generation = current
                .generations
                .iter_mut()
                .find(|g| g.operation == record.operation())
                .ok_or(NativeError::Invalid)?;
            generation.completed = true;
        }
        if record.phase() == Phase::Complete {
            current.active = None;
        }
        if record.phase() == Phase::RolledBack {
            current.active = None;
            current
                .generations
                .retain(|g| g.operation != record.operation());
        }
        let catalog_bytes = records::encode_record(
            &RecordName::StageCatalog,
            serde_json::to_value(&current).map_err(|_| NativeError::Invalid)?,
        )?;
        publish(
            &io,
            proof,
            lock,
            RecordName::StageCatalog,
            &catalog_bytes,
            deadline,
        )?;
        Ok(MutationPermit {
            io,
            operation: record.operation(),
            phase: record.phase(),
            role: record.current_role(),
            bytes,
        })
    }
}
#[cfg(windows)]
pub(crate) use native::{
    MutationPermit, active_helper_operation, catalog, save_operation, selected_operation,
};

#[cfg(windows)]
#[allow(dead_code)] // The frozen borrowed helper API stays available but cannot release a caller's lock.
pub(crate) fn resume_helper(
    io: std::sync::Arc<super::super::native_io::WindowsNativeIo>,
    proof: &super::super::native_io::SupportProof,
    lock: &super::super::native_io::InstallerLock,
    operation: OperationRecord,
    parent: &super::helper::ParentExited,
    deadline: &super::super::native_io::Deadline,
) -> NativeResult<RecoveryDecision> {
    let (payload, mut operation) =
        prepare_helper_resume(io.clone(), proof, lock, operation, parent, deadline)?;
    let mut service = super::super::service::NativeUpgradePort::new();
    service.bind_io(io, deadline)?;
    // Parent exit proves only the installer copy's exit, never the old supervisor/job tree.
    payload.recover(&mut service, lock, &mut operation, Vec::new(), deadline)
}

/// Internal entry consumes the actual lock held by HelperEntryPort, preserving its signatures.
#[cfg(windows)]
pub(crate) fn resume_helper_owned(
    io: std::sync::Arc<super::super::native_io::WindowsNativeIo>,
    proof: &super::super::native_io::SupportProof,
    lock: super::super::native_io::InstallerLock,
    operation: OperationRecord,
    parent: &super::helper::ParentExited,
    deadline: &super::super::native_io::Deadline,
) -> NativeResult<RecoveryDecision> {
    let (payload, mut operation) =
        prepare_helper_resume(io, proof, &lock, operation, parent, deadline)?;
    let mut service = super::super::service::NativeUpgradePort::new();
    // Cold helper recovery cannot rebuild a lost original-owner capability from ParentExited.
    payload.recover_owned(&mut service, lock, &mut operation, Vec::new(), deadline)
}

#[cfg(windows)]
fn prepare_helper_resume(
    io: std::sync::Arc<super::super::native_io::WindowsNativeIo>,
    proof: &super::super::native_io::SupportProof,
    lock: &super::super::native_io::InstallerLock,
    operation: OperationRecord,
    parent: &super::helper::ParentExited,
    deadline: &super::super::native_io::Deadline,
) -> NativeResult<(super::WindowsPayload, OperationRecord)> {
    if parent.operation() != operation.operation() {
        return Err(NativeError::Foreign);
    }
    parent.reverify(deadline)?;
    let current = active_helper_operation(&io, proof, deadline)?.ok_or(NativeError::Missing)?;
    if current.operation() != operation.operation() || current.handoff() != operation.handoff() {
        return Err(NativeError::Foreign);
    }
    let module = io.self_image(proof, deadline)?;
    let pin = super::inventory::ApprovedPe::own_image(&module)?;
    let root = io.payload_root(proof, lock, deadline)?;
    let image = root.open_helper(&io, proof, operation.operation(), &pin, deadline)?;
    if image.identity() != module.identity() {
        return Err(NativeError::Foreign);
    }
    drop(image);
    drop(module);
    let payload = super::WindowsPayload::new(io.clone(), proof, lock, deadline)?;
    let mut operation = operation;
    operation.set_phase(Phase::ParentExited);
    save_operation(io, proof, lock, &operation, deadline)?;
    Ok((payload, operation))
}

/// Read-only observations select a recovery branch. The port retains fresh native capabilities;
/// these serializable facts themselves do not authorize a mutation or construct service proof.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StageObservation {
    Missing,
    Ready(ImageObservation),
    Unknown,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FixedObservation {
    Missing,
    Original(FileStamp),
    Published(ImageObservation),
    Unknown,
}
#[derive(Clone, Debug)]
pub(crate) struct ReopenedRole {
    pub staged: StageObservation,
    pub fixed: FixedObservation,
    pub backup: Option<FileStamp>,
    pub unknown_backup: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RollbackOutcome {
    RolledBack,
    Retained,
}
pub(crate) trait RecoveryPort: super::staging::PayloadPort {
    fn recover_stop(&mut self, record: &OperationRecord) -> NativeResult<Self::Stop>;
    fn observe_role(
        &mut self,
        record: &OperationRecord,
        role: PayloadRole,
    ) -> NativeResult<ReopenedRole>;
    fn recover_started(
        &mut self,
        record: &OperationRecord,
        verified: &Self::Verified,
    ) -> NativeResult<Option<Self::Started>>;
    fn settle_stage(
        &mut self,
        record: &OperationRecord,
        role: PayloadRole,
    ) -> NativeResult<ImageObservation>;
    fn rollback_stage(&mut self, record: &OperationRecord) -> NativeResult<RollbackOutcome>;
}
fn unavailable(error: NativeError) -> NativeResult<RecoveryDecision> {
    match error {
        NativeError::Unsupported
        | NativeError::Unavailable
        | NativeError::Missing
        | NativeError::Busy => Ok(RecoveryDecision::RecoveryRequired),
        other => Err(other),
    }
}
/// Fresh reopen uses the same mutation sequence/port. Stop, child handoff, and Start are never
/// replayed from persisted intent. Unrecognized partial data/identity combinations stay retained.
pub(crate) fn resume<P: RecoveryPort>(
    port: &mut P,
    record: &mut OperationRecord,
) -> NativeResult<RecoveryDecision> {
    use super::staging::persist;
    record.validate()?;
    let original_phase = record.phase();
    match original_phase {
        Phase::Complete | Phase::RolledBack => {
            port.journal(record)?;
            return Ok(RecoveryDecision::Complete);
        }
        Phase::Unknown | Phase::HandoffIntent | Phase::HelperDispatched => {
            return Ok(RecoveryDecision::RecoveryRequired);
        }
        Phase::Intent | Phase::RollbackIntent => {
            persist(port, record, Phase::RollbackIntent, None)?;
            return match port.rollback_stage(record)? {
                RollbackOutcome::RolledBack => {
                    persist(port, record, Phase::RolledBack, None)?;
                    Ok(RecoveryDecision::Complete)
                }
                RollbackOutcome::Retained => Ok(RecoveryDecision::RecoveryRequired),
            };
        }
        Phase::StartIntent | Phase::NewInstanceObserved | Phase::Verified | Phase::PruneIntent => {
            let verified = match port.verify(record.operation()) {
                Ok(value) => value,
                Err(error) => return unavailable(error),
            };
            let started = match port.recover_started(record, &verified) {
                Ok(Some(value)) => value,
                Ok(None) => return Ok(RecoveryDecision::RecoveryRequired),
                Err(error) => return unavailable(error),
            };
            super::staging::finish(port, record, verified, Some(started))?;
            return Ok(RecoveryDecision::Complete);
        }
        _ => {}
    }
    let stop = match port.recover_stop(record) {
        Ok(value) => value,
        Err(error) => return unavailable(error),
    };
    let mut recovered = record.clone();
    match port.original_instance(&stop) {
        Some(instance) => {
            if record
                .original_instance()
                .is_some_and(|old| old != format!("{instance:032x}"))
            {
                return Ok(RecoveryDecision::RecoveryRequired);
            }
            recovered.set_original_instance(format!("{instance:032x}"));
        }
        None if record.original_instance().is_some() => {
            return Ok(RecoveryDecision::RecoveryRequired);
        }
        None => {}
    }
    if let Err(error) = port.released(&stop) {
        return unavailable(error);
    }
    let mut observed = Vec::new();
    for role in PayloadRole::ALL {
        let view = port.observe_role(record, role)?;
        if matches!(view.staged, StageObservation::Unknown)
            || matches!(view.fixed, FixedObservation::Unknown)
            || view.unknown_backup
        {
            return Ok(RecoveryDecision::RecoveryRequired);
        }
        let old = record.role(role)?;
        if let Some(backup) = view.backup
            && old.original != OriginalLeaf::Present(backup)
        {
            return Ok(RecoveryDecision::RecoveryRequired);
        }
        if let FixedObservation::Published(image) = &view.fixed
            && (old.staged.as_ref() != Some(image)
                || !matches!(view.staged, StageObservation::Missing)
                || matches!(old.original,OriginalLeaf::Present(stamp) if view.backup!=Some(stamp)))
        {
            return Ok(RecoveryDecision::RecoveryRequired);
        }
        observed.push((role, view));
    }
    persist(port, &mut recovered, Phase::ImageReleased, None)?;
    *record = recovered;
    // Reconcile complete staged after-effects. Missing stages without a durable prior stage may
    // be created once; partial or disappeared committed stages remain unknown, never overwritten.
    for (role, view) in &observed {
        if let FixedObservation::Published(image) = &view.fixed {
            let mut next = record.clone();
            next.role_mut(*role)?.published = Some(image.clone());
            persist(port, &mut next, Phase::Published, Some(*role))?;
            *record = next;
            continue;
        }
        match &view.staged {
            StageObservation::Ready(image) => {
                if record
                    .role(*role)?
                    .staged
                    .as_ref()
                    .is_some_and(|old| old != image)
                {
                    return Ok(RecoveryDecision::RecoveryRequired);
                }
                let mut next = record.clone();
                next.role_mut(*role)?.staged = Some(image.clone());
                persist(port, &mut next, Phase::StageIntent, Some(*role))?;
                *record = next;
                let settled = port.settle_stage(record, *role)?;
                if &settled != image {
                    return Err(NativeError::OutcomeUnknown);
                }
                persist(port, record, Phase::Staged, Some(*role))?;
            }
            StageObservation::Missing => {
                if record.role(*role)?.staged.is_some() {
                    return Ok(RecoveryDecision::RecoveryRequired);
                }
                persist(port, record, Phase::StageIntent, Some(*role))?;
                let image = port.stage(record.operation(), *role)?;
                let mut next = record.clone();
                next.role_mut(*role)?.staged = Some(image);
                persist(port, &mut next, Phase::Staged, Some(*role))?;
                *record = next;
            }
            StageObservation::Unknown => return Ok(RecoveryDecision::RecoveryRequired),
        }
    }
    for (role, view) in observed {
        if matches!(view.fixed, FixedObservation::Published(_)) {
            continue;
        }
        match view.fixed {
            FixedObservation::Original(stamp) => {
                if view.backup.is_some()
                    || matches!(record.role(role)?.original,OriginalLeaf::Present(old) if old!=stamp)
                {
                    return Ok(RecoveryDecision::RecoveryRequired);
                }
                let mut next = record.clone();
                next.role_mut(role)?.original = OriginalLeaf::Present(stamp);
                persist(port, &mut next, Phase::BackupIntent, Some(role))?;
                *record = next;
                let backup = port.backup(record.operation(), role)?;
                if backup != Some(stamp) {
                    return Err(NativeError::OutcomeUnknown);
                }
                let mut next = record.clone();
                next.role_mut(role)?.backup = backup;
                persist(port, &mut next, Phase::BackedUp, Some(role))?;
                *record = next;
            }
            FixedObservation::Missing => {
                let original = record.role(role)?.original;
                if matches!(original, OriginalLeaf::Present(_)) && view.backup.is_none() {
                    return Ok(RecoveryDecision::RecoveryRequired);
                }
                let mut next = record.clone();
                if original == OriginalLeaf::Unobserved {
                    next.role_mut(role)?.original = OriginalLeaf::Missing
                }
                next.role_mut(role)?.backup = view.backup;
                persist(port, &mut next, Phase::BackedUp, Some(role))?;
                *record = next;
            }
            _ => return Ok(RecoveryDecision::RecoveryRequired),
        }
        persist(port, record, Phase::PublishIntent, Some(role))?;
        let image = port.publish(record.operation(), role)?;
        let mut next = record.clone();
        next.role_mut(role)?.published = Some(image);
        persist(port, &mut next, Phase::Published, Some(role))?;
        *record = next;
    }
    let verified = port.verify(record.operation())?;
    super::staging::finish(port, record, verified, None)?;
    Ok(RecoveryDecision::Complete)
}
