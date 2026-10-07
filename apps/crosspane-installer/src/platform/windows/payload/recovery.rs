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
/// The one fixed outer selection is correlation only; it cannot construct a native owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, allow(dead_code))]
pub(crate) enum OuterPhase {
    Selecting,
    Preparing,
    Prepared,
    Ready,
    Committed,
    Complete,
    Cancelled,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, allow(dead_code))]
pub(crate) enum OuterLaunchPhase {
    None,
    CreateIntent,
    Created,
    ResumeIntent,
    Resumed,
}
/// Fixed-copy cleanup observations never establish process/job completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, allow(dead_code))]
pub(crate) enum OuterCopyCleanup {
    None,
    DeleteIntent,
    Absent,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, allow(dead_code))]
pub(crate) struct OuterContextCorrelation {
    user: Vec<u8>,
    logon: Vec<u8>,
    authentication_id: u64,
    session: u32,
}
#[cfg_attr(test, allow(dead_code))]
impl OuterContextCorrelation {
    /// Read-only lineage fields; callers must freshly match the strict source record.
    pub(crate) fn authentication_id(&self) -> u64 {
        self.authentication_id
    }
    pub(crate) fn logon_sid(&self) -> &[u8] {
        &self.logon
    }
    pub(crate) fn new(facts: &super::super::native_io::identity::TokenFacts) -> NativeResult<Self> {
        super::super::native_io::identity::LimitedIdentity::admit(facts.clone())?;
        Ok(Self {
            user: facts.user.bytes().to_vec(),
            logon: facts.logon.bytes().to_vec(),
            authentication_id: facts.authentication_id,
            session: facts.session,
        })
    }
    fn validate(&self) -> NativeResult<()> {
        use super::super::native_io::identity::{LimitedIdentity, Sid, TokenFacts};
        LimitedIdentity::admit(TokenFacts {
            user: Sid::from_bytes(self.user.clone())?,
            logon: Sid::from_bytes(self.logon.clone())?,
            authentication_id: self.authentication_id,
            session: self.session,
            elevated: false,
            integrity: 0x2000,
            impersonating: false,
        })?;
        Ok(())
    }
    pub(crate) fn matches(
        &self,
        facts: &super::super::native_io::identity::TokenFacts,
    ) -> NativeResult<()> {
        self.validate()?;
        if *self != Self::new(facts)? {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
    /// Terminal history may belong to an earlier logon. This compares only the same user and
    /// admits no old process/job or session-disposition capability; current native guards remain.
    pub(crate) fn same_user(
        &self,
        facts: &super::super::native_io::identity::TokenFacts,
    ) -> NativeResult<()> {
        self.validate()?;
        let current = Self::new(facts)?;
        if self.user != current.user {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, allow(dead_code))]
pub(crate) struct OuterProcessCorrelation {
    pid: u32,
    creation: u64,
    module: FileStamp,
    image: super::inventory::PeFacts,
}
#[cfg_attr(test, allow(dead_code))]
impl OuterProcessCorrelation {
    pub(crate) fn new(
        pid: u32,
        creation: u64,
        module: FileStamp,
        image: super::inventory::PeFacts,
    ) -> NativeResult<Self> {
        let value = Self {
            pid,
            creation,
            module,
            image,
        };
        value.validate()?;
        Ok(value)
    }
    fn validate(&self) -> NativeResult<()> {
        if self.pid == 0
            || self.creation == 0
            || self.module.volume == 0
            || self.module.file == [0; 16]
            || !self.image.valid()
        {
            return Err(NativeError::Invalid);
        }
        Ok(())
    }
    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }
    pub(crate) fn creation(&self) -> u64 {
        self.creation
    }
    pub(crate) fn image(&self) -> &super::inventory::PeFacts {
        &self.image
    }
    pub(crate) fn matches(
        &self,
        pid: u32,
        creation: u64,
        module: FileStamp,
        image: &super::inventory::PeFacts,
    ) -> NativeResult<()> {
        self.validate()?;
        if self.pid != pid
            || self.creation != creation
            || self.module != module
            || self.image != *image
        {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, allow(dead_code))]
pub(crate) struct OuterUpgradeRecord {
    schema_version: u32,
    operation: [u8; 16],
    phase: OuterPhase,
    outer: OuterProcessCorrelation,
    context: OuterContextCorrelation,
    sources: [super::inventory::PeFacts; 3],
    keeper_image: Option<FileStamp>,
    keeper: Option<OuterProcessCorrelation>,
    inherited_parent_handle: Option<u64>,
    launch_stage: OuterLaunchPhase,
    copy_cleanup: OuterCopyCleanup,
}
#[cfg_attr(test, allow(dead_code))]
impl OuterUpgradeRecord {
    pub(crate) fn new(
        operation: [u8; 16],
        outer: OuterProcessCorrelation,
        context: OuterContextCorrelation,
        sources: [super::inventory::PeFacts; 3],
    ) -> NativeResult<Self> {
        let record = Self {
            schema_version: 1,
            operation,
            phase: OuterPhase::Selecting,
            outer,
            context,
            sources,
            keeper_image: None,
            keeper: None,
            inherited_parent_handle: None,
            launch_stage: OuterLaunchPhase::None,
            copy_cleanup: OuterCopyCleanup::None,
        };
        record.validate()?;
        Ok(record)
    }
    pub(crate) fn validate(&self) -> NativeResult<()> {
        if self.schema_version != 1 || self.operation == [0; 16] {
            return Err(NativeError::Invalid);
        }
        self.outer.validate()?;
        self.context.validate()?;
        if self.copy_cleanup != OuterCopyCleanup::None
            && !matches!(self.phase, OuterPhase::Complete | OuterPhase::Cancelled)
        {
            return Err(NativeError::Invalid);
        }
        if self.sources.iter().any(|facts| !facts.valid()) {
            return Err(NativeError::Invalid);
        }
        let source_bytes = self
            .sources
            .iter()
            .try_fold(0u64, |sum, p| sum.checked_add(p.size))
            .ok_or(NativeError::Oversize)?;
        let total = self
            .outer
            .image
            .size
            .checked_mul(2)
            .and_then(|own| own.checked_add(source_bytes))
            .ok_or(NativeError::Oversize)?;
        if total > super::inventory::MAX_STAGING_BYTES {
            return Err(NativeError::Oversize);
        }
        if let Some(image) = self.keeper_image
            && (image.volume == 0 || image.file == [0; 16])
        {
            return Err(NativeError::Invalid);
        }
        if matches!(
            self.launch_stage,
            OuterLaunchPhase::Created | OuterLaunchPhase::ResumeIntent | OuterLaunchPhase::Resumed
        ) != self.keeper.is_some()
        {
            return Err(NativeError::Invalid);
        }
        if self.launch_stage != OuterLaunchPhase::None && self.keeper_image.is_none() {
            return Err(NativeError::Invalid);
        }
        if self.keeper.is_some() != self.inherited_parent_handle.is_some() {
            return Err(NativeError::Invalid);
        }
        if let Some(keeper) = &self.keeper {
            keeper.validate()?;
            let handle = self.inherited_parent_handle.ok_or(NativeError::Invalid)?;
            if handle < 4
                || handle > usize::MAX as u64
                || handle >= u64::MAX - 15
                || !handle.is_multiple_of(4)
                || keeper.pid == self.outer.pid
                || keeper.module != self.keeper_image.ok_or(NativeError::Invalid)?
                || keeper.image != self.outer.image
            {
                return Err(NativeError::Invalid);
            }
        }
        if matches!(
            self.phase,
            OuterPhase::Prepared | OuterPhase::Ready | OuterPhase::Committed | OuterPhase::Complete
        ) && self.keeper_image.is_none()
        {
            return Err(NativeError::Invalid);
        }
        if matches!(
            self.phase,
            OuterPhase::Ready | OuterPhase::Committed | OuterPhase::Complete
        ) && (self.keeper.is_none() || self.launch_stage != OuterLaunchPhase::Resumed)
        {
            return Err(NativeError::Invalid);
        }
        Ok(())
    }
    pub(crate) fn operation(&self) -> [u8; 16] {
        self.operation
    }
    pub(crate) fn phase(&self) -> OuterPhase {
        self.phase
    }
    pub(crate) fn outer(&self) -> &OuterProcessCorrelation {
        &self.outer
    }
    pub(crate) fn context(&self) -> &OuterContextCorrelation {
        &self.context
    }
    pub(crate) fn sources(&self) -> &[super::inventory::PeFacts; 3] {
        &self.sources
    }
    pub(crate) fn keeper_image(&self) -> Option<FileStamp> {
        self.keeper_image
    }
    pub(crate) fn keeper(&self) -> Option<&OuterProcessCorrelation> {
        self.keeper.as_ref()
    }
    pub(crate) fn inherited_parent_handle(&self) -> Option<u64> {
        self.inherited_parent_handle
    }
    pub(crate) fn launch_stage(&self) -> OuterLaunchPhase {
        self.launch_stage
    }
    pub(crate) fn launch_phase(&self) -> OuterLaunchPhase {
        self.launch_stage
    }
    pub(crate) fn copy_cleanup(&self) -> OuterCopyCleanup {
        self.copy_cleanup
    }
    pub(crate) fn begin_cleanup(&mut self) -> NativeResult<()> {
        self.validate()?;
        if !matches!(self.phase, OuterPhase::Complete | OuterPhase::Cancelled)
            || self.copy_cleanup != OuterCopyCleanup::None
        {
            return Err(NativeError::OutcomeUnknown);
        }
        self.copy_cleanup = OuterCopyCleanup::DeleteIntent;
        self.validate()
    }
    pub(crate) fn copy_absent(&mut self) -> NativeResult<()> {
        self.validate()?;
        if !matches!(self.phase, OuterPhase::Complete | OuterPhase::Cancelled)
            || self.copy_cleanup != OuterCopyCleanup::DeleteIntent
        {
            return Err(NativeError::OutcomeUnknown);
        }
        self.copy_cleanup = OuterCopyCleanup::Absent;
        self.validate()
    }
    pub(crate) fn selection_matches(&self, old: &Self) -> bool {
        self.operation == old.operation
            && self.outer == old.outer
            && self.context == old.context
            && self.sources == old.sources
    }
    pub(crate) fn encode(&self) -> NativeResult<Vec<u8>> {
        use super::super::native_io::records;
        self.validate()?;
        records::encode_record(
            &records::RecordName::OuterUpgrade,
            serde_json::to_value(self).map_err(|_| NativeError::Invalid)?,
        )
    }
    pub(crate) fn decode(bytes: &[u8]) -> NativeResult<Self> {
        use super::super::native_io::records;
        let value: Self = records::record_data(&records::RecordName::OuterUpgrade, bytes)?;
        value.validate()?;
        Ok(value)
    }
    pub(crate) fn same_selection(&self, old: &Self) -> NativeResult<()> {
        self.validate()?;
        old.validate()?;
        if self.operation != old.operation
            || self.outer != old.outer
            || self.context != old.context
            || self.sources != old.sources
        {
            return Err(NativeError::Foreign);
        }
        if old.keeper_image.is_some() && old.keeper_image != self.keeper_image
            || old.keeper.is_some() && old.keeper != self.keeper
            || old.inherited_parent_handle.is_some()
                && old.inherited_parent_handle != self.inherited_parent_handle
        {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
    pub(crate) fn advance(&mut self, next: OuterPhase) -> NativeResult<()> {
        let allowed = self.phase == next
            || matches!(
                (self.phase, next),
                (OuterPhase::Selecting, OuterPhase::Preparing)
                    | (OuterPhase::Preparing, OuterPhase::Prepared)
                    | (OuterPhase::Prepared, OuterPhase::Ready)
                    | (OuterPhase::Ready, OuterPhase::Committed)
                    | (OuterPhase::Committed, OuterPhase::Complete)
            );
        let cancel = next == OuterPhase::Cancelled
            && matches!(
                self.phase,
                OuterPhase::Selecting
                    | OuterPhase::Preparing
                    | OuterPhase::Prepared
                    | OuterPhase::Ready
            );
        if !allowed && !cancel && next != OuterPhase::Unknown {
            return Err(NativeError::OutcomeUnknown);
        }
        let mut value = self.clone();
        value.phase = next;
        value.validate()?;
        *self = value;
        Ok(())
    }
    pub(crate) fn set_keeper_image(&mut self, image: FileStamp) -> NativeResult<()> {
        if self.phase != OuterPhase::Preparing || self.keeper_image.is_some() {
            return Err(NativeError::OutcomeUnknown);
        }
        let mut value = self.clone();
        value.keeper_image = Some(image);
        value.advance(OuterPhase::Prepared)?;
        *self = value;
        Ok(())
    }
    pub(crate) fn record_created_keeper(
        &mut self,
        handle: u64,
        keeper: OuterProcessCorrelation,
    ) -> NativeResult<()> {
        if self.phase != OuterPhase::Prepared
            || self.launch_stage != OuterLaunchPhase::CreateIntent
            || self.keeper.is_some()
        {
            return Err(NativeError::OutcomeUnknown);
        }
        let mut value = self.clone();
        value.keeper = Some(keeper);
        value.inherited_parent_handle = Some(handle);
        value.launch_stage = OuterLaunchPhase::Created;
        value.validate()?;
        *self = value;
        Ok(())
    }
    pub(crate) fn advance_launch(&mut self, next: OuterLaunchPhase) -> NativeResult<()> {
        if self.phase != OuterPhase::Prepared
            || !matches!(
                (self.launch_stage, next),
                (OuterLaunchPhase::None, OuterLaunchPhase::CreateIntent)
                    | (OuterLaunchPhase::Created, OuterLaunchPhase::ResumeIntent)
                    | (OuterLaunchPhase::ResumeIntent, OuterLaunchPhase::Resumed)
            )
        {
            return Err(NativeError::OutcomeUnknown);
        }
        let mut value = self.clone();
        value.launch_stage = next;
        value.validate()?;
        *self = value;
        Ok(())
    }
    pub(crate) fn peer_matches(
        &self,
        pid: u32,
        creation: u64,
        module: FileStamp,
        image: &super::inventory::PeFacts,
        facts: &super::super::native_io::identity::TokenFacts,
    ) -> NativeResult<()> {
        self.validate()?;
        self.context.matches(facts)?;
        self.keeper
            .as_ref()
            .ok_or(NativeError::Foreign)?
            .matches(pid, creation, module, image)
    }
    /// Read only the fixed rooted leaf; values are observations, not an owner/lock factory.
    #[cfg(windows)]
    pub(crate) fn read(
        io: &super::super::native_io::WindowsNativeIo,
        proof: &super::super::native_io::SupportProof,
        deadline: &super::super::native_io::Deadline,
    ) -> NativeResult<Option<Self>> {
        use super::super::native_io::{files::MAX_RECORD_BYTES, records};
        let observed = io.read_record(
            proof,
            records::RecordName::OuterUpgrade,
            MAX_RECORD_BYTES,
            deadline,
        )?;
        observed
            .map(|record| {
                let value: Self =
                    records::record_data(&records::RecordName::OuterUpgrade, record.bytes())?;
                value.validate()?;
                Ok(value)
            })
            .transpose()
    }
}

/// Observation-only terminal policy shared by native retirement and focused pure fakes.
/// Native publication additionally requires the real exclusive-namespace/absence capability.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn validate_outer_terminal_retirement(
    outer: &OuterUpgradeRecord,
    operation: &OperationRecord,
    catalog: &StageCatalog,
) -> NativeResult<()> {
    outer.validate()?;
    operation.validate()?;
    catalog.validate()?;
    if operation.operation() != outer.operation()
        || outer.copy_cleanup() != OuterCopyCleanup::Absent
    {
        return Err(NativeError::Foreign);
    }
    match outer.phase() {
        OuterPhase::Complete
            if operation.phase() == Phase::Complete && catalog.active.is_none() =>
        {
            Ok(())
        }
        OuterPhase::Cancelled => {
            if !matches!(operation.phase(), Phase::Intent | Phase::RolledBack)
                || catalog.active.is_some_and(|id| id != outer.operation())
                || operation.current_role().is_some()
                || operation.original_instance().is_some()
                || operation.new_instance().is_some()
                || operation.handoff().is_some()
                || operation.retention_incomplete()
                || operation.roles().iter().any(|role| {
                    role.original != OriginalLeaf::Unobserved
                        || role.staged.is_some()
                        || role.backup.is_some()
                        || role.published.is_some()
                })
            {
                return Err(NativeError::Foreign);
            }
            Ok(())
        }
        _ => Err(NativeError::OutcomeUnknown),
    }
}

/// The actual two-publication entry order, shared with the six focused interruption fakes.
#[cfg_attr(test, allow(dead_code))]
pub(crate) trait OuterSelectionPort {
    fn publish_selection(&mut self, record: &OuterUpgradeRecord) -> NativeResult<()>;
    fn create_operation(&mut self, record: &OperationRecord) -> NativeResult<()>;
}
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn publish_outer_selection<P: OuterSelectionPort>(
    port: &mut P,
    record: &OuterUpgradeRecord,
) -> NativeResult<()> {
    record.validate()?;
    if record.phase() != OuterPhase::Selecting || record.launch_phase() != OuterLaunchPhase::None {
        return Err(NativeError::OutcomeUnknown);
    }
    port.publish_selection(record)?;
    port.create_operation(&OperationRecord::new(record.operation())?)
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
        #[cfg(not(test))]
        io.refuse_unsettled_repair_readonly(proof, deadline)?;
        #[cfg(not(test))]
        io.refuse_unsettled_payload_repair_readonly(proof, deadline)?;
        // Removal is a distinct operation: observing its unresolved selection grants no
        // upgrade authority and cannot be disguised as an empty upgrade catalog.
        #[cfg(not(test))]
        super::super::super::removal::admit_upgrade_selection(
            io.read_removal(proof, deadline)?.as_ref(),
        )?;
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
    /// Terminal correlation is retired only AFTER the real native cleanup capability settles
    /// the fixed keeper copy. This cannot reconstruct an old process/tree or authorize Stop/Run.
    #[cfg(not(test))]
    pub(crate) fn retire_outer_terminal(
        io: &Arc<WindowsNativeIo>,
        proof: &SupportProof,
        lock: &InstallerLock,
        absence: &super::super::super::native_io::keeper::KeeperCopyAbsent,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        absence.reverify(io, proof, lock, deadline)?;
        let outer = OuterUpgradeRecord::read(io, proof, deadline)?.ok_or(NativeError::Foreign)?;
        outer.context().same_user(io.target().identity())?;
        if outer.operation() != absence.operation()
            || outer.copy_cleanup() != OuterCopyCleanup::Absent
        {
            return Err(NativeError::Foreign);
        }
        let name = RecordName::Operation(outer.operation());
        let observed = io
            .read_record(
                proof,
                name.clone(),
                super::super::super::native_io::files::MAX_RECORD_BYTES,
                deadline,
            )?
            .ok_or(NativeError::Foreign)?;
        let mut operation: OperationRecord = records::record_data(&name, observed.bytes())?;
        let mut current = catalog(io, proof, deadline)?;
        validate_outer_terminal_retirement(&outer, &operation, &current)?;
        if outer.phase() == OuterPhase::Complete {
            return absence.reverify(io, proof, lock, deadline);
        }
        // Intent has never admitted Stop or payload mutation. Commit its rollback observation
        // before clearing the selected catalog; an interrupted result remains safe to reobserve.
        if operation.phase() == Phase::Intent {
            operation.set_phase(Phase::RolledBack);
            let bytes = records::encode_record(
                &name,
                serde_json::to_value(&operation).map_err(|_| NativeError::Invalid)?,
            )?;
            absence.reverify(io, proof, lock, deadline)?;
            publish(io, proof, lock, name, &bytes, deadline)?;
        }
        if current.active == Some(outer.operation()) {
            current.active = None;
            current
                .generations
                .retain(|row| row.operation != outer.operation());
            current.validate()?;
            let bytes = records::encode_record(
                &RecordName::StageCatalog,
                serde_json::to_value(&current).map_err(|_| NativeError::Invalid)?,
            )?;
            absence.reverify(io, proof, lock, deadline)?;
            publish(io, proof, lock, RecordName::StageCatalog, &bytes, deadline)?;
        }
        absence.reverify(io, proof, lock, deadline)
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
#[cfg(all(windows, not(test)))]
pub(crate) use native::retire_outer_terminal;
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

// The new FILE-only graph is consumed by shipping native bridges and separate integration
// fakes; the library unit-test graph intentionally excludes those native entry points.
/// Exact immutable source-record observations. These cannot open a root or authorize effects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, allow(dead_code))]
pub(crate) struct FileRecordStamp {
    identity: FileStamp,
    sha256: [u8; 32],
}
#[cfg_attr(test, allow(dead_code))]
impl FileRecordStamp {
    pub(crate) fn new(identity: FileStamp, sha256: [u8; 32]) -> NativeResult<Self> {
        if !identity.valid() || sha256 == [0; 32] {
            return Err(NativeError::Invalid);
        }
        Ok(Self { identity, sha256 })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, allow(dead_code))]
pub(crate) enum FileRecoveryDirection {
    Rollback,
    Forward,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, allow(dead_code))]
pub(crate) struct FileRecoveryRole {
    role: PayloadRole,
    original: OriginalLeaf,
    new_image: Option<ImageObservation>,
}
#[cfg_attr(test, allow(dead_code))]
impl FileRecoveryRole {
    pub(crate) fn original(&self) -> OriginalLeaf {
        self.original
    }
    pub(crate) fn new_image(&self) -> Option<&ImageObservation> {
        self.new_image.as_ref()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, allow(dead_code))]
pub(crate) enum FileRecoveryRoleStep {
    Begin,
    ReturnPublishedIntent,
    ReturnedToStage,
    RestoreOriginalIntent,
    OriginalRestored,
    SettleStageIntent,
    StageSettled,
    BackupOriginalIntent,
    OriginalBackedUp,
    PublishStageIntent,
    NewPublished,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
#[cfg_attr(test, allow(dead_code))]
pub(crate) enum FileRecoveryCursor {
    Role {
        role: PayloadRole,
        step: FileRecoveryRoleStep,
    },
    FilesConverged,
    CopyDeleteIntent,
    CopyAbsent,
    CatalogRetireIntent,
    CatalogInactive,
    OuterRetireIntent,
    Retired,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, allow(dead_code))]
pub(crate) struct FileRecoveryJournal {
    schema_version: u32,
    operation: [u8; 16],
    outer_snapshot: OuterUpgradeRecord,
    outer_record: FileRecordStamp,
    operation_record: FileRecordStamp,
    direction: FileRecoveryDirection,
    roles: [FileRecoveryRole; 4],
    cursor: FileRecoveryCursor,
}
#[cfg_attr(test, allow(dead_code))]
impl FileRecoveryJournal {
    pub(crate) fn prepare(
        outer: OuterUpgradeRecord,
        outer_record: FileRecordStamp,
        operation: &OperationRecord,
        operation_record: FileRecordStamp,
        views: [ReopenedRole; 4],
    ) -> NativeResult<Self> {
        outer.validate()?;
        operation.validate()?;
        if outer.operation() != operation.operation() || outer.phase() != OuterPhase::Committed {
            return Err(NativeError::Foreign);
        }
        let direction = match operation.phase() {
            Phase::BackupIntent | Phase::BackedUp => FileRecoveryDirection::Rollback,
            Phase::PublishIntent | Phase::Published => FileRecoveryDirection::Forward,
            _ => return Err(NativeError::Unsupported),
        };
        let mut roles = Vec::with_capacity(4);
        for (role, view) in PayloadRole::ALL.into_iter().zip(views) {
            let source = operation.role(role)?;
            let new_image = source.staged.clone().ok_or(NativeError::Foreign)?;
            if view.unknown_backup
                || matches!(view.fixed, FixedObservation::Unknown)
                || matches!(view.staged, StageObservation::Unknown)
            {
                return Err(NativeError::Foreign);
            }
            let original = match (source.original, &view.fixed, view.backup) {
                (OriginalLeaf::Unobserved, FixedObservation::Original(id), None) => {
                    OriginalLeaf::Present(*id)
                }
                (OriginalLeaf::Unobserved, FixedObservation::Missing, None) => {
                    OriginalLeaf::Missing
                }
                (OriginalLeaf::Unobserved, _, _) => return Err(NativeError::Foreign),
                (value, _, _) => value,
            };
            let row = FileRecoveryRole {
                role,
                original,
                new_image: Some(new_image),
            };
            if source
                .backup
                .is_some_and(|id| Some(id) != expected_backup(&row))
                || source
                    .published
                    .as_ref()
                    .is_some_and(|image| Some(image) != row.new_image.as_ref())
            {
                return Err(NativeError::Foreign);
            }
            if !file_role_available(&row, &view) {
                return Err(NativeError::Foreign);
            }
            roles.push(row);
        }
        let roles: [FileRecoveryRole; 4] = roles.try_into().map_err(|_| NativeError::Invalid)?;
        let first = match direction {
            FileRecoveryDirection::Rollback => PayloadRole::Ctl,
            FileRecoveryDirection::Forward => PayloadRole::Installer,
        };
        let value = Self {
            schema_version: 1,
            operation: operation.operation(),
            outer_snapshot: outer,
            outer_record,
            operation_record,
            direction,
            roles,
            cursor: FileRecoveryCursor::Role {
                role: first,
                step: FileRecoveryRoleStep::Begin,
            },
        };
        value.validate()?;
        Ok(value)
    }
    pub(crate) fn operation(&self) -> [u8; 16] {
        self.operation
    }
    pub(crate) fn outer_snapshot(&self) -> &OuterUpgradeRecord {
        &self.outer_snapshot
    }
    pub(crate) fn outer_record(&self) -> FileRecordStamp {
        self.outer_record
    }
    // Only the source-included lineage negatives inspect this observation getter.
    #[cfg(test)]
    pub(crate) fn operation_record(&self) -> FileRecordStamp {
        self.operation_record
    }
    pub(crate) fn direction(&self) -> FileRecoveryDirection {
        self.direction
    }
    pub(crate) fn role(&self, role: PayloadRole) -> NativeResult<&FileRecoveryRole> {
        self.roles
            .iter()
            .find(|r| r.role == role)
            .ok_or(NativeError::Invalid)
    }
    pub(crate) fn cursor(&self) -> FileRecoveryCursor {
        self.cursor
    }
    pub(crate) fn validate(&self) -> NativeResult<()> {
        self.outer_snapshot.validate()?;
        FileRecordStamp::new(self.outer_record.identity, self.outer_record.sha256)?;
        FileRecordStamp::new(self.operation_record.identity, self.operation_record.sha256)?;
        if self.schema_version != 1
            || self.operation == [0; 16]
            || self.outer_snapshot.operation() != self.operation
            || self.outer_snapshot.phase() != OuterPhase::Committed
        {
            return Err(NativeError::Invalid);
        }
        for (index, (expected, row)) in PayloadRole::ALL.into_iter().zip(&self.roles).enumerate() {
            if row.role != expected
                || row.original == OriginalLeaf::Unobserved
                || matches!(row.original,OriginalLeaf::Present(id) if !id.valid())
            {
                return Err(NativeError::Invalid);
            }
            let image = row.new_image.as_ref().ok_or(NativeError::Invalid)?;
            let expected_facts = if index == 0 {
                self.outer_snapshot.outer().image()
            } else {
                &self.outer_snapshot.sources()[index - 1]
            };
            if !image.identity.valid()
                || !image.facts.valid()
                || image.facts != *expected_facts
                || row.original == OriginalLeaf::Present(image.identity)
            {
                return Err(NativeError::Invalid);
            }
        }
        self.cursor_rank(self.cursor)?;
        Ok(())
    }
    fn cursor_rank(&self, cursor: FileRecoveryCursor) -> NativeResult<u16> {
        use FileRecoveryCursor::*;
        use FileRecoveryRoleStep::*;
        Ok(match cursor {
            Role { role, step } => {
                let index = PayloadRole::ALL
                    .iter()
                    .position(|r| *r == role)
                    .ok_or(NativeError::Invalid)?;
                let step_rank = match (self.direction, step) {
                    (_, Begin) => 0,
                    (FileRecoveryDirection::Rollback, ReturnPublishedIntent) => 1,
                    (FileRecoveryDirection::Rollback, ReturnedToStage) => 2,
                    (FileRecoveryDirection::Rollback, RestoreOriginalIntent) => 3,
                    (FileRecoveryDirection::Rollback, OriginalRestored) => 4,
                    (FileRecoveryDirection::Forward, SettleStageIntent) => 1,
                    (FileRecoveryDirection::Forward, StageSettled) => 2,
                    (FileRecoveryDirection::Forward, BackupOriginalIntent) => 3,
                    (FileRecoveryDirection::Forward, OriginalBackedUp) => 4,
                    (FileRecoveryDirection::Forward, PublishStageIntent) => 5,
                    (FileRecoveryDirection::Forward, NewPublished) => 6,
                    _ => return Err(NativeError::Invalid),
                };
                let index = if self.direction == FileRecoveryDirection::Rollback {
                    3 - index
                } else {
                    index
                };
                index as u16 * 10 + step_rank
            }
            FilesConverged => 40,
            CopyDeleteIntent => 41,
            CopyAbsent => 42,
            CatalogRetireIntent => 43,
            CatalogInactive => 44,
            OuterRetireIntent => 45,
            Retired => 46,
        })
    }
    pub(crate) fn advance(&mut self, cursor: FileRecoveryCursor) -> NativeResult<()> {
        self.validate()?;
        if self.cursor_rank(cursor)? <= self.cursor_rank(self.cursor)? {
            return Err(NativeError::Foreign);
        }
        self.cursor = cursor;
        self.validate()
    }
    pub(crate) fn same_plan(&self, other: &Self) -> NativeResult<()> {
        self.validate()?;
        other.validate()?;
        if self.operation != other.operation
            || self.outer_snapshot != other.outer_snapshot
            || self.outer_record != other.outer_record
            || self.operation_record != other.operation_record
            || self.direction != other.direction
            || self.roles != other.roles
        {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
    pub(crate) fn matches_sources(
        &self,
        outer: &OuterUpgradeRecord,
        outer_record: FileRecordStamp,
        operation: &OperationRecord,
        operation_record: FileRecordStamp,
    ) -> NativeResult<()> {
        self.validate()?;
        operation.validate()?;
        outer.validate()?;
        if *outer != self.outer_snapshot
            || outer_record != self.outer_record
            || operation.operation() != self.operation
            || operation_record != self.operation_record
        {
            return Err(NativeError::Foreign);
        }
        let expected = match operation.phase() {
            Phase::BackupIntent | Phase::BackedUp => FileRecoveryDirection::Rollback,
            Phase::PublishIntent | Phase::Published => FileRecoveryDirection::Forward,
            _ => return Err(NativeError::Foreign),
        };
        if expected != self.direction {
            return Err(NativeError::Foreign);
        }
        for row in &self.roles {
            let old = operation.role(row.role)?;
            if old.staged != row.new_image
                || old.original != OriginalLeaf::Unobserved && old.original != row.original
            {
                return Err(NativeError::Foreign);
            }
        }
        Ok(())
    }
    pub(crate) fn encode(&self) -> NativeResult<Vec<u8>> {
        use super::super::native_io::records;
        self.validate()?;
        records::encode_record(
            &records::RecordName::FileRecovery,
            serde_json::to_value(self).map_err(|_| NativeError::Invalid)?,
        )
    }
    pub(crate) fn decode(bytes: &[u8]) -> NativeResult<Self> {
        use super::super::native_io::records;
        let value: Self = records::record_data(&records::RecordName::FileRecovery, bytes)?;
        value.validate()?;
        Ok(value)
    }
}
#[cfg_attr(test, allow(dead_code))]
fn expected_backup(row: &FileRecoveryRole) -> Option<FileStamp> {
    match row.original {
        OriginalLeaf::Present(id) => Some(id),
        _ => None,
    }
}
#[cfg_attr(test, allow(dead_code))]
fn stage_matches(row: &FileRecoveryRole, view: &ReopenedRole) -> bool {
    matches!(&view.staged, StageObservation::Ready(image) if Some(image) == row.new_image.as_ref())
}
#[cfg_attr(test, allow(dead_code))]
fn fixed_matches_original(row: &FileRecoveryRole, view: &ReopenedRole) -> bool {
    match (&view.fixed, row.original) {
        (FixedObservation::Original(id), OriginalLeaf::Present(expected)) => *id == expected,
        (FixedObservation::Missing, OriginalLeaf::Missing) => true,
        _ => false,
    }
}
#[cfg_attr(test, allow(dead_code))]
fn fixed_matches_new(row: &FileRecoveryRole, view: &ReopenedRole) -> bool {
    matches!(&view.fixed,FixedObservation::Published(image) if Some(image) == row.new_image.as_ref())
}
#[cfg_attr(test, allow(dead_code))]
fn file_role_available(row: &FileRecoveryRole, view: &ReopenedRole) -> bool {
    if view.unknown_backup {
        return false;
    }
    if fixed_matches_new(row, view) {
        matches!(view.staged, StageObservation::Missing) && view.backup == expected_backup(row)
    } else if stage_matches(row, view) {
        (fixed_matches_original(row, view) && view.backup.is_none())
            || (matches!(view.fixed, FixedObservation::Missing)
                && view.backup == expected_backup(row))
    } else {
        false
    }
}
#[cfg_attr(test, allow(dead_code))]
fn file_role_converged(
    direction: FileRecoveryDirection,
    row: &FileRecoveryRole,
    view: &ReopenedRole,
) -> bool {
    !view.unknown_backup
        && match direction {
            FileRecoveryDirection::Rollback => {
                stage_matches(row, view)
                    && fixed_matches_original(row, view)
                    && view.backup.is_none()
            }
            FileRecoveryDirection::Forward => {
                fixed_matches_new(row, view)
                    && matches!(view.staged, StageObservation::Missing)
                    && view.backup == expected_backup(row)
            }
        }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(test, allow(dead_code))]
pub(crate) enum FileRecoveryDecision {
    Retained,
    FilesRestored,
    FilesForwardComplete,
}
/// The FILE-only port has no service, image approval, process, input, start or pruning method.
#[cfg_attr(test, allow(dead_code))]
pub(crate) trait FileRecoveryPort {
    fn renew(&mut self, journal: &FileRecoveryJournal) -> NativeResult<()>;
    fn journal(&mut self, journal: &FileRecoveryJournal) -> NativeResult<()>;
    fn observe_role(
        &mut self,
        journal: &FileRecoveryJournal,
        role: PayloadRole,
    ) -> NativeResult<ReopenedRole>;
    fn return_published(
        &mut self,
        journal: &FileRecoveryJournal,
        role: PayloadRole,
    ) -> NativeResult<()>;
    fn restore_original(
        &mut self,
        journal: &FileRecoveryJournal,
        role: PayloadRole,
    ) -> NativeResult<()>;
    fn settle_stage(
        &mut self,
        journal: &FileRecoveryJournal,
        role: PayloadRole,
    ) -> NativeResult<()>;
    fn backup_original(
        &mut self,
        journal: &FileRecoveryJournal,
        role: PayloadRole,
    ) -> NativeResult<()>;
    fn publish_stage(
        &mut self,
        journal: &FileRecoveryJournal,
        role: PayloadRole,
    ) -> NativeResult<()>;
    fn cleanup_keeper_copy(&mut self, journal: &FileRecoveryJournal) -> NativeResult<()>;
    fn retire_catalog(&mut self, journal: &FileRecoveryJournal) -> NativeResult<()>;
    fn retire_outer(&mut self, journal: &FileRecoveryJournal) -> NativeResult<()>;
    fn observe_terminal(&mut self, journal: &FileRecoveryJournal) -> NativeResult<()>;
}
#[cfg_attr(test, allow(dead_code))]
fn file_persist<P: FileRecoveryPort>(
    port: &mut P,
    journal: &mut FileRecoveryJournal,
    cursor: FileRecoveryCursor,
) -> NativeResult<()> {
    if journal.cursor_rank(journal.cursor)? >= journal.cursor_rank(cursor)? {
        return Ok(());
    }
    let mut next = journal.clone();
    next.advance(cursor)?;
    port.renew(journal)?;
    port.journal(&next)?;
    *journal = next;
    Ok(())
}
#[cfg_attr(test, allow(dead_code))]
fn file_observe<P: FileRecoveryPort>(
    port: &mut P,
    journal: &FileRecoveryJournal,
    role: PayloadRole,
) -> NativeResult<ReopenedRole> {
    port.renew(journal)?;
    port.observe_role(journal, role)
}
#[cfg_attr(test, allow(dead_code))]
fn file_role_cursor(role: PayloadRole, step: FileRecoveryRoleStep) -> FileRecoveryCursor {
    FileRecoveryCursor::Role { role, step }
}
/// Resumes only fixed identity-checked file operations. A durable intent plus a fresh positive
/// after-effect observation suppresses replay; a missing or conflicting slot retains recovery.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn recover_file_only<P: FileRecoveryPort>(
    port: &mut P,
    journal: &mut FileRecoveryJournal,
) -> NativeResult<FileRecoveryDecision> {
    use FileRecoveryCursor::*;
    use FileRecoveryRoleStep::*;
    journal.validate()?;
    port.renew(journal)?;
    let mut order = PayloadRole::ALL;
    if journal.direction == FileRecoveryDirection::Rollback {
        order.reverse();
    }
    for role in order {
        let row = journal.role(role)?.clone();
        let mut view = file_observe(port, journal, role)?;
        let last = file_role_cursor(
            role,
            match journal.direction {
                FileRecoveryDirection::Rollback => OriginalRestored,
                FileRecoveryDirection::Forward => NewPublished,
            },
        );
        if journal.cursor_rank(journal.cursor)? >= journal.cursor_rank(last)? {
            if !file_role_converged(journal.direction, &row, &view) {
                return Ok(FileRecoveryDecision::Retained);
            }
            continue;
        }
        if !file_role_available(&row, &view) {
            return Ok(FileRecoveryDecision::Retained);
        }
        match journal.direction {
            FileRecoveryDirection::Rollback => {
                if fixed_matches_new(&row, &view) {
                    let intent = file_role_cursor(role, ReturnPublishedIntent);
                    if journal.cursor_rank(journal.cursor)? > journal.cursor_rank(intent)? {
                        return Ok(FileRecoveryDecision::Retained);
                    }
                    file_persist(port, journal, intent)?;
                    port.renew(journal)?;
                    port.return_published(journal, role)?;
                    view = file_observe(port, journal, role)?;
                    if !stage_matches(&row, &view)
                        || !matches!(view.fixed, FixedObservation::Missing)
                        || view.backup != expected_backup(&row)
                    {
                        return Ok(FileRecoveryDecision::Retained);
                    }
                }
                file_persist(port, journal, file_role_cursor(role, ReturnedToStage))?;
                if !fixed_matches_original(&row, &view) {
                    if !stage_matches(&row, &view)
                        || !matches!(view.fixed, FixedObservation::Missing)
                        || view.backup != expected_backup(&row)
                    {
                        return Ok(FileRecoveryDecision::Retained);
                    }
                    let intent = file_role_cursor(role, RestoreOriginalIntent);
                    if journal.cursor_rank(journal.cursor)? > journal.cursor_rank(intent)? {
                        return Ok(FileRecoveryDecision::Retained);
                    }
                    file_persist(port, journal, intent)?;
                    port.renew(journal)?;
                    port.restore_original(journal, role)?;
                    view = file_observe(port, journal, role)?;
                }
            }
            FileRecoveryDirection::Forward => {
                if !fixed_matches_new(&row, &view) {
                    let settle = file_role_cursor(role, SettleStageIntent);
                    if journal.cursor_rank(journal.cursor)? <= journal.cursor_rank(settle)? {
                        file_persist(port, journal, settle)?;
                        port.renew(journal)?;
                        port.settle_stage(journal, role)?;
                        view = file_observe(port, journal, role)?;
                        if !file_role_available(&row, &view) {
                            return Ok(FileRecoveryDecision::Retained);
                        }
                        file_persist(port, journal, file_role_cursor(role, StageSettled))?;
                    }
                    if !matches!(view.fixed, FixedObservation::Missing) {
                        let intent = file_role_cursor(role, BackupOriginalIntent);
                        if journal.cursor_rank(journal.cursor)? > journal.cursor_rank(intent)? {
                            return Ok(FileRecoveryDecision::Retained);
                        }
                        file_persist(port, journal, intent)?;
                        port.renew(journal)?;
                        port.backup_original(journal, role)?;
                        view = file_observe(port, journal, role)?;
                    }
                    if !stage_matches(&row, &view)
                        || !matches!(view.fixed, FixedObservation::Missing)
                        || view.backup != expected_backup(&row)
                    {
                        return Ok(FileRecoveryDecision::Retained);
                    }
                    file_persist(port, journal, file_role_cursor(role, OriginalBackedUp))?;
                    let intent = file_role_cursor(role, PublishStageIntent);
                    if journal.cursor_rank(journal.cursor)? > journal.cursor_rank(intent)? {
                        return Ok(FileRecoveryDecision::Retained);
                    }
                    file_persist(port, journal, intent)?;
                    // The native effect freshly measures/flushes a reopened stage under this
                    // intent before rename; StageSettled is not a cross-process flush proof.
                    port.renew(journal)?;
                    port.publish_stage(journal, role)?;
                    view = file_observe(port, journal, role)?;
                }
            }
        }
        if !file_role_converged(journal.direction, &row, &view) {
            return Ok(FileRecoveryDecision::Retained);
        }
        file_persist(port, journal, last)?;
    }
    file_persist(port, journal, FilesConverged)?;
    if journal.cursor_rank(journal.cursor)? < journal.cursor_rank(CopyAbsent)? {
        file_persist(port, journal, CopyDeleteIntent)?;
        port.renew(journal)?;
        port.cleanup_keeper_copy(journal)?;
        file_persist(port, journal, CopyAbsent)?;
    }
    if journal.cursor_rank(journal.cursor)? < journal.cursor_rank(CatalogInactive)? {
        file_persist(port, journal, CatalogRetireIntent)?;
        port.renew(journal)?;
        port.retire_catalog(journal)?;
        file_persist(port, journal, CatalogInactive)?;
    }
    if journal.cursor != Retired {
        file_persist(port, journal, OuterRetireIntent)?;
        port.renew(journal)?;
        port.retire_outer(journal)?;
        port.renew(journal)?;
        port.observe_terminal(journal)?;
        file_persist(port, journal, Retired)?;
    } else {
        port.renew(journal)?;
        port.observe_terminal(journal)?;
    }
    Ok(match journal.direction {
        FileRecoveryDirection::Rollback => FileRecoveryDecision::FilesRestored,
        FileRecoveryDirection::Forward => FileRecoveryDecision::FilesForwardComplete,
    })
}

#[cfg(all(windows, not(test)))]
mod file_native {
    use super::super::super::native_io::{
        Deadline, FileRecoverySeal, InstallerLock, SupportProof, WindowsNativeIo,
        files::MAX_RECORD_BYTES,
        records::{PublicationRecovery, RecordName},
    };
    use super::*;
    use std::sync::Arc;
    /// Separate FILE-only cursor permit, minted from successful publication or a fresh actual
    /// fixed-record read. A terminal permit admits observation, never a role mutation.
    pub(crate) struct FileRecoveryMutationPermit {
        io: Arc<WindowsNativeIo>,
        operation: [u8; 16],
        cursor: FileRecoveryCursor,
        bytes: Vec<u8>,
    }
    impl FileRecoveryMutationPermit {
        pub(crate) fn io(&self) -> &Arc<WindowsNativeIo> {
            &self.io
        }
        pub(crate) fn cursor(&self) -> FileRecoveryCursor {
            self.cursor
        }
        pub(crate) fn bytes(&self) -> &[u8] {
            &self.bytes
        }
        pub(crate) fn reverify(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            lock: &InstallerLock,
            seal: &FileRecoverySeal,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if !std::ptr::eq(io, self.io.as_ref()) {
                return Err(NativeError::Foreign);
            }
            seal.reverify(io, proof, lock, deadline)?;
            let journal = FileRecoveryJournal::decode(&self.bytes)?;
            seal.matches_journal(&journal)?;
            if journal.operation() != self.operation || journal.cursor() != self.cursor {
                return Err(NativeError::Foreign);
            }
            let actual = io
                .read_record(proof, RecordName::FileRecovery, MAX_RECORD_BYTES, deadline)?
                .ok_or(NativeError::Foreign)?;
            if actual.bytes() != self.bytes {
                return Err(NativeError::Foreign);
            }
            seal.reverify(io, proof, lock, deadline)
        }
    }
    pub(crate) fn read_file_recovery(
        io: &WindowsNativeIo,
        proof: &SupportProof,
        deadline: &Deadline,
    ) -> NativeResult<Option<FileRecoveryJournal>> {
        io.read_record(proof, RecordName::FileRecovery, MAX_RECORD_BYTES, deadline)?
            .map(|record| FileRecoveryJournal::decode(record.bytes()))
            .transpose()
    }
    pub(crate) fn admit_file_recovery_permit(
        io: &Arc<WindowsNativeIo>,
        proof: &SupportProof,
        lock: &InstallerLock,
        seal: &FileRecoverySeal,
        deadline: &Deadline,
    ) -> NativeResult<FileRecoveryMutationPermit> {
        seal.reverify(io, proof, lock, deadline)?;
        let actual = io
            .read_record(proof, RecordName::FileRecovery, MAX_RECORD_BYTES, deadline)?
            .ok_or(NativeError::Foreign)?;
        let journal = FileRecoveryJournal::decode(actual.bytes())?;
        seal.matches_journal(&journal)?;
        let permit = FileRecoveryMutationPermit {
            io: io.clone(),
            operation: journal.operation(),
            cursor: journal.cursor(),
            bytes: actual.bytes().to_vec(),
        };
        permit.reverify(io, proof, lock, seal, deadline)?;
        Ok(permit)
    }
    pub(crate) fn publish_file_recovery(
        io: &Arc<WindowsNativeIo>,
        proof: &SupportProof,
        lock: &InstallerLock,
        seal: &FileRecoverySeal,
        journal: &FileRecoveryJournal,
        deadline: &Deadline,
    ) -> NativeResult<FileRecoveryMutationPermit> {
        seal.reverify(io, proof, lock, deadline)?;
        seal.matches_journal(journal)?;
        let first = FileRecoveryCursor::Role {
            role: match journal.direction() {
                FileRecoveryDirection::Rollback => PayloadRole::Ctl,
                FileRecoveryDirection::Forward => PayloadRole::Installer,
            },
            step: FileRecoveryRoleStep::Begin,
        };
        match read_file_recovery(io, proof, deadline)? {
            Some(old) if old.operation() == journal.operation() => {
                old.same_plan(journal)?;
                if old.cursor_rank(journal.cursor())? <= old.cursor_rank(old.cursor())? {
                    return Err(NativeError::Foreign);
                }
            }
            Some(old) => {
                if old.cursor() != FileRecoveryCursor::Retired || journal.cursor() != first {
                    return Err(NativeError::OutcomeUnknown);
                }
                old.outer_snapshot()
                    .context()
                    .same_user(io.target().identity())?;
            }
            None if journal.cursor() == first => {}
            None => return Err(NativeError::Foreign),
        }
        let bytes = journal.encode()?;
        seal.reverify(io, proof, lock, deadline)?;
        let result = io.publish_record(proof, lock, RecordName::FileRecovery, &bytes, deadline)?;
        if result.state != PublicationRecovery::NewPublished || result.native_failure.is_some() {
            return Err(NativeError::OutcomeUnknown);
        }
        // Reopen the actual published record; a success observation is not a bytes constructor.
        let permit = admit_file_recovery_permit(io, proof, lock, seal, deadline)?;
        if permit.bytes() != bytes {
            return Err(NativeError::OutcomeUnknown);
        }
        Ok(permit)
    }
}
#[cfg(all(windows, not(test)))]
pub(crate) use file_native::{
    FileRecoveryMutationPermit, admit_file_recovery_permit, publish_file_recovery,
    read_file_recovery,
};

/// Read-only lineage selection, not prior-session disposition. Exact current context preserves
/// the ordinary warm path. An old same-user tuple still requires the genuine native LSA seal.
// Shipping native admission and source-included integration fakes consume this classifier.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn file_recovery_prior_logon(
    outer: &OuterUpgradeRecord,
    current: &super::super::native_io::identity::TokenFacts,
) -> NativeResult<Option<u64>> {
    outer.validate()?;
    outer.context().same_user(current)?;
    if outer.context().matches(current).is_ok() {
        return Ok(None);
    }
    if outer.context().authentication_id() == current.authentication_id
        || outer.context().logon_sid() == current.logon.bytes()
    {
        return Err(NativeError::Foreign);
    }
    Ok(Some(outer.context().authentication_id()))
}
