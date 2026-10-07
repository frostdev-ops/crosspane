//! Separate payload-repair correlation. Decoding never grants a native capability.
use super::super::{
    native_io::{NativeError, NativeResult, files::MAX_RECORD_BYTES, records},
    payload::{
        inventory::{MAX_STAGING_BYTES, PayloadRole, PeFacts},
        recovery::{FileStamp, ImageObservation, OriginalLeaf, OuterContextCorrelation},
    },
    service::supervisor::Generation,
};
use super::RepairDiagnostic;
use aws_lc_rs::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};

fn hash(bytes: &[u8]) -> [u8; 32] {
    let mut result = [0; 32];
    result.copy_from_slice(digest(&SHA256, bytes).as_ref());
    result
}
fn valid_generation(g: Generation) -> bool {
    g.pid != 0 && g.creation != 0 && g.instance != 0
}
fn valid_image(image: &ImageObservation) -> bool {
    image.identity.valid() && image.facts.valid()
}
fn validate_context(context: &OuterContextCorrelation) -> NativeResult<()> {
    // Reuse the strict existing context parser; these values remain observations only.
    let bytes = serde_json::to_vec(context).map_err(|_| NativeError::Invalid)?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(NativeError::Oversize);
    }
    // Its existing same_user/matches perform validation on native admission. Here enforce
    // the closed serialized tuple without constructing an identity authority.
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| NativeError::Invalid)?;
    let user = value
        .get("user")
        .and_then(serde_json::Value::as_array)
        .ok_or(NativeError::Invalid)?;
    let logon = value
        .get("logon")
        .and_then(serde_json::Value::as_array)
        .ok_or(NativeError::Invalid)?;
    let sid = |parts: &[serde_json::Value]| -> NativeResult<()> {
        let bytes: Vec<u8> = parts
            .iter()
            .map(|v| {
                v.as_u64()
                    .and_then(|n| u8::try_from(n).ok())
                    .ok_or(NativeError::Invalid)
            })
            .collect::<NativeResult<_>>()?;
        super::super::native_io::identity::Sid::from_bytes(bytes).map(|_| ())
    };
    sid(user)?;
    sid(logon)?;
    let sid_bytes = |parts: &[serde_json::Value]| -> NativeResult<Vec<u8>> {
        parts
            .iter()
            .map(|v| {
                v.as_u64()
                    .and_then(|n| u8::try_from(n).ok())
                    .ok_or(NativeError::Invalid)
            })
            .collect()
    };
    let session = value
        .get("session")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or(NativeError::Invalid)?;
    use super::super::native_io::identity::{Sid, TokenFacts};
    context.matches(&TokenFacts {
        user: Sid::from_bytes(sid_bytes(user)?)?,
        logon: Sid::from_bytes(sid_bytes(logon)?)?,
        authentication_id: context.authentication_id(),
        session,
        elevated: false,
        integrity: 0x2000,
        impersonating: false,
    })
}
fn valid_submission(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 38
        && bytes[0] == b'{'
        && bytes[37] == b'}'
        && bytes[1..37].iter().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}
fn role_index(role: PayloadRole) -> usize {
    match role {
        PayloadRole::Installer => 0,
        PayloadRole::Agent => 1,
        PayloadRole::Ui => 2,
        PayloadRole::Ctl => 3,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PayloadRepairProcess {
    pid: u32,
    creation: u64,
}
impl PayloadRepairProcess {
    pub(crate) fn new(pid: u32, creation: u64) -> NativeResult<Self> {
        let result = Self { pid, creation };
        result.validate()?;
        Ok(result)
    }
    pub(crate) fn validate(&self) -> NativeResult<()> {
        if self.pid == 0 || self.creation == 0 {
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
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PayloadRepairTask {
    diagnostic: RepairDiagnostic,
    xml: String,
}
impl PayloadRepairTask {
    pub(crate) fn new(diagnostic: RepairDiagnostic, xml: String) -> NativeResult<Self> {
        let result = Self { diagnostic, xml };
        result.validate()?;
        Ok(result)
    }
    pub(crate) fn validate(&self) -> NativeResult<()> {
        if self.xml.is_empty() || self.xml.len() > MAX_RECORD_BYTES || self.xml.contains('\0') {
            return Err(NativeError::Invalid);
        }
        Ok(())
    }
    pub(crate) fn diagnostic(&self) -> RepairDiagnostic {
        self.diagnostic
    }
    pub(crate) fn xml(&self) -> &str {
        &self.xml
    }
    pub(crate) fn enabled_or_absent(&self) -> bool {
        matches!(
            self.diagnostic,
            RepairDiagnostic::Healthy | RepairDiagnostic::Missing
        )
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PayloadRepairSelection {
    context: OuterContextCorrelation,
    source_process: PayloadRepairProcess,
    own_module: ImageObservation,
    original_generation: Generation,
    original_started_unix_ms: u64,
    task: PayloadRepairTask,
    sources: [PeFacts; 4],
    fixed: [OriginalLeaf; 4],
}
impl PayloadRepairSelection {
    // Independent native admissions supply each correlation; combining them here grants no rights.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        context: OuterContextCorrelation,
        source_process: PayloadRepairProcess,
        own_module: ImageObservation,
        original_generation: Generation,
        original_started_unix_ms: u64,
        task: PayloadRepairTask,
        sources: [PeFacts; 4],
        fixed: [OriginalLeaf; 4],
    ) -> NativeResult<Self> {
        let result = Self {
            context,
            source_process,
            own_module,
            original_generation,
            original_started_unix_ms,
            task,
            sources,
            fixed,
        };
        result.validate()?;
        Ok(result)
    }
    pub(crate) fn validate(&self) -> NativeResult<()> {
        validate_context(&self.context)?;
        self.source_process.validate()?;
        self.task.validate()?;
        if !self.task.enabled_or_absent()
            || !valid_generation(self.original_generation)
            || self.original_started_unix_ms == 0
            || !valid_image(&self.own_module)
            || self.sources[0] != self.own_module.facts
            || self.sources.iter().any(|p| !p.valid())
        {
            return Err(NativeError::Invalid);
        }
        let mut budget = self.own_module.facts.size;
        for source in &self.sources {
            budget = budget
                .checked_add(source.size)
                .ok_or(NativeError::Oversize)?;
        }
        if budget > MAX_STAGING_BYTES {
            return Err(NativeError::Oversize);
        }
        for original in self.fixed {
            match original {
                OriginalLeaf::Present(id) if id.valid() => {}
                OriginalLeaf::Missing => {}
                _ => return Err(NativeError::Invalid),
            }
        }
        Ok(())
    }
    pub(crate) fn context(&self) -> &OuterContextCorrelation {
        &self.context
    }
    pub(crate) fn source_process(&self) -> PayloadRepairProcess {
        self.source_process
    }
    pub(crate) fn own_module(&self) -> &ImageObservation {
        &self.own_module
    }
    pub(crate) fn original_generation(&self) -> Generation {
        self.original_generation
    }
    pub(crate) fn original_started_unix_ms(&self) -> u64 {
        self.original_started_unix_ms
    }
    pub(crate) fn task(&self) -> &PayloadRepairTask {
        &self.task
    }
    pub(crate) fn sources(&self) -> &[PeFacts; 4] {
        &self.sources
    }
    pub(crate) fn source(&self, role: PayloadRole) -> &PeFacts {
        &self.sources[role_index(role)]
    }
    pub(crate) fn fixed(&self, role: PayloadRole) -> OriginalLeaf {
        self.fixed[role_index(role)]
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum PayloadRepairPhase {
    Selected,
    CopyIntent,
    CopyReady,
    HandoffIntent,
    Created,
    ResumeIntent,
    Ready,
    CommitIntent,
    Committed,
    StopIntent,
    TreeCompleted,
    StageIntent { role: PayloadRole },
    Staged { role: PayloadRole },
    BackupIntent { role: PayloadRole },
    BackedUp { role: PayloadRole },
    PublishIntent { role: PayloadRole },
    Published { role: PayloadRole },
    FixedVerified,
    StartIntent,
    StartSubmitted,
    ReadyObserved,
    Complete,
    Cancelled,
    Unknown,
    Retired,
}
/// Record correlation only; the adapter must also prove the actual source owner and
/// that its create worker settled without any Resume attempt. Never a cold capability.
pub(crate) fn source_can_cancel(phase: PayloadRepairPhase, resume_attempted: bool) -> bool {
    !resume_attempted && phase.rank() <= PayloadRepairPhase::ResumeIntent.rank()
}
impl PayloadRepairPhase {
    pub(crate) fn rank(self) -> u16 {
        use PayloadRepairPhase::*;
        match self {
            Selected => 0,
            CopyIntent => 1,
            CopyReady => 2,
            HandoffIntent => 3,
            Created => 4,
            ResumeIntent => 5,
            Ready => 6,
            CommitIntent => 7,
            Committed => 8,
            StopIntent => 9,
            TreeCompleted => 10,
            StageIntent { role } => 11 + 6 * role_index(role) as u16,
            Staged { role } => 12 + 6 * role_index(role) as u16,
            BackupIntent { role } => 13 + 6 * role_index(role) as u16,
            BackedUp { role } => 14 + 6 * role_index(role) as u16,
            PublishIntent { role } => 15 + 6 * role_index(role) as u16,
            Published { role } => 16 + 6 * role_index(role) as u16,
            FixedVerified => 35,
            StartIntent => 36,
            StartSubmitted => 37,
            ReadyObserved => 38,
            Complete => 39,
            Retired => 40,
            Cancelled => 41,
            Unknown => 42,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct PayloadRepairKeeper {
    image: Option<ImageObservation>,
    parent: Option<PayloadRepairProcess>,
    child: Option<PayloadRepairProcess>,
    inherited_parent_handle: Option<u64>,
}
impl PayloadRepairKeeper {
    pub(crate) fn image(&self) -> Option<&ImageObservation> {
        self.image.as_ref()
    }
    pub(crate) fn parent(&self) -> Option<PayloadRepairProcess> {
        self.parent
    }
    pub(crate) fn child(&self) -> Option<PayloadRepairProcess> {
        self.child
    }
    pub(crate) fn inherited_parent_handle(&self) -> Option<u64> {
        self.inherited_parent_handle
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct PayloadRepairRole {
    original: Option<OriginalLeaf>,
    staged: Option<ImageObservation>,
    backup: Option<FileStamp>,
    published: Option<ImageObservation>,
}
impl PayloadRepairRole {
    pub(crate) fn original(&self) -> Option<OriginalLeaf> {
        self.original
    }
    pub(crate) fn staged(&self) -> Option<&ImageObservation> {
        self.staged.as_ref()
    }
    pub(crate) fn published(&self) -> Option<&ImageObservation> {
        self.published.as_ref()
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PayloadRepairRecord {
    schema_version: u32,
    operation: [u8; 16],
    selection: PayloadRepairSelection,
    slot: Option<u8>,
    phase: PayloadRepairPhase,
    keeper: PayloadRepairKeeper,
    roles: [PayloadRepairRole; 4],
    submission: Option<String>,
    ready: Option<Generation>,
}
impl PayloadRepairRecord {
    pub(crate) fn new(
        operation: [u8; 16],
        selection: PayloadRepairSelection,
        slot: Option<u8>,
    ) -> NativeResult<Self> {
        let result = Self {
            schema_version: 1,
            operation,
            selection,
            slot,
            phase: PayloadRepairPhase::Selected,
            keeper: PayloadRepairKeeper::default(),
            roles: std::array::from_fn(|_| PayloadRepairRole::default()),
            submission: None,
            ready: None,
        };
        result.validate()?;
        Ok(result)
    }
    pub(crate) fn operation(&self) -> [u8; 16] {
        self.operation
    }
    pub(crate) fn selection(&self) -> &PayloadRepairSelection {
        &self.selection
    }
    pub(crate) fn context(&self) -> &OuterContextCorrelation {
        self.selection.context()
    }
    pub(crate) fn original_generation(&self) -> Generation {
        self.selection.original_generation()
    }
    pub(crate) fn original_started_unix_ms(&self) -> u64 {
        self.selection.original_started_unix_ms()
    }
    pub(crate) fn task(&self) -> &PayloadRepairTask {
        self.selection.task()
    }
    pub(crate) fn own_module(&self) -> &ImageObservation {
        self.selection.own_module()
    }
    pub(crate) fn sources(&self) -> &[PeFacts; 4] {
        self.selection.sources()
    }
    pub(crate) fn role(&self, role: PayloadRole) -> &PayloadRepairRole {
        &self.roles[role_index(role)]
    }
    pub(crate) fn keeper(&self) -> &PayloadRepairKeeper {
        &self.keeper
    }
    pub(crate) fn slot(&self) -> Option<u8> {
        self.slot
    }
    pub(crate) fn phase(&self) -> PayloadRepairPhase {
        self.phase
    }
    pub(crate) fn ready(&self) -> Option<Generation> {
        self.ready
    }
    pub(crate) fn same_selection(&self, next: &Self) -> bool {
        self.operation == next.operation && self.selection == next.selection
    }
    pub(crate) fn bind_slot(&mut self, slot: u8) -> NativeResult<()> {
        if slot >= 3 || self.slot.is_some() || self.phase != PayloadRepairPhase::Selected {
            return Err(NativeError::Foreign);
        }
        self.slot = Some(slot);
        self.validate()
    }
    pub(crate) fn bind_keeper_copy(&mut self, image: ImageObservation) -> NativeResult<()> {
        if self.phase != PayloadRepairPhase::CopyIntent
            || self.keeper.image.is_some()
            || !valid_image(&image)
            || image.facts != self.own_module().facts
            || image.identity == self.own_module().identity
        {
            return Err(NativeError::Foreign);
        }
        self.keeper.image = Some(image);
        self.validate()
    }
    pub(crate) fn bind_keeper_child(
        &mut self,
        parent: PayloadRepairProcess,
        child: PayloadRepairProcess,
        inherited_parent_handle: u64,
    ) -> NativeResult<()> {
        parent.validate()?;
        child.validate()?;
        if self.phase != PayloadRepairPhase::HandoffIntent
            || self.keeper.child.is_some()
            || parent != self.selection.source_process
            || parent == child
            || inherited_parent_handle == 0
            || inherited_parent_handle >= u64::MAX - 15
        {
            return Err(NativeError::Foreign);
        }
        self.keeper.parent = Some(parent);
        self.keeper.child = Some(child);
        self.keeper.inherited_parent_handle = Some(inherited_parent_handle);
        self.validate()
    }
    pub(crate) fn bind_original(
        &mut self,
        role: PayloadRole,
        original: OriginalLeaf,
    ) -> NativeResult<()> {
        if self.phase != (PayloadRepairPhase::Staged { role })
            || self.role(role).original.is_some()
            || original != self.selection.fixed(role)
        {
            return Err(NativeError::Foreign);
        }
        self.roles[role_index(role)].original = Some(original);
        self.validate()
    }
    pub(crate) fn bind_staged(
        &mut self,
        role: PayloadRole,
        image: ImageObservation,
    ) -> NativeResult<()> {
        if self.phase != (PayloadRepairPhase::StageIntent { role })
            || self.role(role).staged.is_some()
            || !valid_image(&image)
            || image.facts != *self.selection.source(role)
            || self.selection.fixed(role) == OriginalLeaf::Present(image.identity)
        {
            return Err(NativeError::Foreign);
        }
        self.roles[role_index(role)].staged = Some(image);
        self.validate()
    }
    pub(crate) fn bind_backup(
        &mut self,
        role: PayloadRole,
        backup: Option<FileStamp>,
    ) -> NativeResult<()> {
        if self.phase != (PayloadRepairPhase::BackupIntent { role }) {
            return Err(NativeError::Foreign);
        }
        let expected = match self.role(role).original {
            Some(OriginalLeaf::Present(id)) => Some(id),
            Some(OriginalLeaf::Missing) => None,
            _ => return Err(NativeError::Foreign),
        };
        if backup != expected || self.role(role).backup.is_some() {
            return Err(NativeError::Foreign);
        }
        self.roles[role_index(role)].backup = backup;
        self.validate()
    }
    pub(crate) fn bind_published(
        &mut self,
        role: PayloadRole,
        image: ImageObservation,
    ) -> NativeResult<()> {
        if self.phase != (PayloadRepairPhase::PublishIntent { role })
            || self.role(role).published.is_some()
            || self.role(role).staged.as_ref() != Some(&image)
        {
            return Err(NativeError::Foreign);
        }
        self.roles[role_index(role)].published = Some(image);
        self.validate()
    }
    pub(crate) fn bind_submission(&mut self, submission: String) -> NativeResult<()> {
        if self.phase != PayloadRepairPhase::StartIntent
            || self.submission.is_some()
            || !valid_submission(&submission)
        {
            return Err(NativeError::Foreign);
        }
        self.submission = Some(submission);
        self.validate()
    }
    pub(crate) fn bind_ready(&mut self, generation: Generation) -> NativeResult<()> {
        if self.phase != PayloadRepairPhase::StartSubmitted
            || self.ready.is_some()
            || !valid_generation(generation)
            || generation.instance == self.original_generation().instance
            || (generation.pid == self.original_generation().pid
                && generation.creation == self.original_generation().creation)
        {
            return Err(NativeError::Foreign);
        }
        self.ready = Some(generation);
        self.validate()
    }
    pub(crate) fn validate(&self) -> NativeResult<()> {
        self.selection.validate()?;
        if self.schema_version != 1
            || self.operation == [0; 16]
            || self.slot.is_some_and(|s| s >= 3)
        {
            return Err(NativeError::Invalid);
        }
        let interrupted = matches!(
            self.phase,
            PayloadRepairPhase::Unknown | PayloadRepairPhase::Cancelled
        );
        let rank = self.phase.rank();
        if let Some(image) = &self.keeper.image {
            if !interrupted && rank < PayloadRepairPhase::CopyIntent.rank() {
                return Err(NativeError::Invalid);
            }
            if !valid_image(image)
                || image.facts != self.own_module().facts
                || image.identity == self.own_module().identity
            {
                return Err(NativeError::Invalid);
            }
        } else if !interrupted && rank >= PayloadRepairPhase::CopyReady.rank() {
            return Err(NativeError::Invalid);
        }
        match (
            self.keeper.parent,
            self.keeper.child,
            self.keeper.inherited_parent_handle,
        ) {
            (Some(parent), Some(child), Some(handle)) => {
                parent.validate()?;
                child.validate()?;
                if !interrupted && rank < PayloadRepairPhase::HandoffIntent.rank() {
                    return Err(NativeError::Invalid);
                }
                if parent != self.selection.source_process
                    || parent == child
                    || handle == 0
                    || handle >= u64::MAX - 15
                {
                    return Err(NativeError::Invalid);
                }
            }
            (None, None, None) if interrupted || rank < PayloadRepairPhase::Created.rank() => {}
            _ => return Err(NativeError::Invalid),
        }
        for role in PayloadRole::ALL {
            let state = self.role(role);
            if let Some(original) = state.original {
                if !interrupted && rank < (PayloadRepairPhase::Staged { role }).rank() {
                    return Err(NativeError::Invalid);
                }
                if original != self.selection.fixed(role) {
                    return Err(NativeError::Invalid);
                }
            } else if !interrupted && rank >= (PayloadRepairPhase::BackupIntent { role }).rank() {
                return Err(NativeError::Invalid);
            }
            if let Some(image) = &state.staged {
                if !interrupted && rank < (PayloadRepairPhase::StageIntent { role }).rank() {
                    return Err(NativeError::Invalid);
                }
                if self
                    .roles
                    .iter()
                    .filter_map(|s| s.staged.as_ref())
                    .filter(|s| s.identity == image.identity)
                    .count()
                    != 1
                    || self
                        .keeper
                        .image
                        .as_ref()
                        .is_some_and(|k| k.identity == image.identity)
                {
                    return Err(NativeError::Invalid);
                }
                if !valid_image(image)
                    || image.facts != *self.selection.source(role)
                    || self.selection.fixed(role) == OriginalLeaf::Present(image.identity)
                {
                    return Err(NativeError::Invalid);
                }
            } else if !interrupted && rank >= (PayloadRepairPhase::Staged { role }).rank() {
                return Err(NativeError::Invalid);
            }
            let backup = match state.original {
                Some(OriginalLeaf::Present(id)) => Some(id),
                _ => None,
            };
            if state.backup.is_some()
                && (state.backup != backup
                    || !interrupted && rank < (PayloadRepairPhase::BackupIntent { role }).rank())
            {
                return Err(NativeError::Invalid);
            }
            if !interrupted
                && rank >= (PayloadRepairPhase::BackedUp { role }).rank()
                && state.backup != backup
            {
                return Err(NativeError::Invalid);
            }
            if let Some(image) = &state.published {
                if !interrupted && rank < (PayloadRepairPhase::PublishIntent { role }).rank() {
                    return Err(NativeError::Invalid);
                }
                if state.staged.as_ref() != Some(image) {
                    return Err(NativeError::Invalid);
                }
            } else if !interrupted && rank >= (PayloadRepairPhase::Published { role }).rank() {
                return Err(NativeError::Invalid);
            }
        }
        if let Some(submission) = &self.submission {
            if !interrupted && rank < PayloadRepairPhase::StartIntent.rank() {
                return Err(NativeError::Invalid);
            }
            if !valid_submission(submission) {
                return Err(NativeError::Invalid);
            }
        } else if !interrupted && rank >= PayloadRepairPhase::StartSubmitted.rank() {
            return Err(NativeError::Invalid);
        }
        if let Some(ready) = self.ready {
            if !interrupted && rank < PayloadRepairPhase::StartSubmitted.rank() {
                return Err(NativeError::Invalid);
            }
            if !valid_generation(ready)
                || ready.instance == self.original_generation().instance
                || (ready.pid == self.original_generation().pid
                    && ready.creation == self.original_generation().creation)
            {
                return Err(NativeError::Invalid);
            }
        } else if !interrupted && rank >= PayloadRepairPhase::ReadyObserved.rank() {
            return Err(NativeError::Invalid);
        }
        Ok(())
    }
    pub(crate) fn advance(&mut self, phase: PayloadRepairPhase) -> NativeResult<()> {
        use PayloadRepairPhase::*;
        if phase == self.phase {
            return self.validate();
        }
        if matches!(self.phase, Retired | Unknown | Cancelled) {
            return Err(NativeError::Foreign);
        }
        let allowed = if phase == Unknown {
            true
        } else if phase == Cancelled {
            self.phase.rank() < CommitIntent.rank()
        } else {
            phase.rank() == self.phase.rank() + 1
        };
        if !allowed {
            return Err(NativeError::Foreign);
        }
        let mut next = self.clone();
        next.phase = phase;
        next.validate()?;
        *self = next;
        Ok(())
    }
    pub(crate) fn publication_successor(&self, next: &Self) -> NativeResult<()> {
        self.validate()?;
        next.validate()?;
        if !self.same_selection(next) || self.slot.is_some() && self.slot != next.slot {
            return Err(NativeError::Foreign);
        }
        for role in PayloadRole::ALL {
            let old = self.role(role);
            let new = next.role(role);
            if old.original.is_some() && old.original != new.original
                || old.staged.is_some() && old.staged != new.staged
                || old.backup.is_some() && old.backup != new.backup
                || old.published.is_some() && old.published != new.published
            {
                return Err(NativeError::Foreign);
            }
        }
        if self.keeper.image.is_some() && self.keeper.image != next.keeper.image
            || self.keeper.child.is_some() && self.keeper != next.keeper
            || self.submission.is_some() && self.submission != next.submission
            || self.ready.is_some() && self.ready != next.ready
        {
            return Err(NativeError::Foreign);
        }
        // Result facts may bind before the next result publication. Check the closed
        // transition independently of the old snapshot, which has not received those facts.
        if next.phase != self.phase {
            let allowed = !matches!(
                self.phase,
                PayloadRepairPhase::Retired
                    | PayloadRepairPhase::Unknown
                    | PayloadRepairPhase::Cancelled
            ) && (next.phase == PayloadRepairPhase::Unknown
                || next.phase == PayloadRepairPhase::Cancelled
                    && self.phase.rank() < PayloadRepairPhase::CommitIntent.rank()
                || next.phase.rank() == self.phase.rank() + 1);
            if !allowed {
                return Err(NativeError::Foreign);
            }
        }
        Ok(())
    }
    pub(crate) fn encode(&self) -> NativeResult<Vec<u8>> {
        self.validate()?;
        records::encode_record(
            &records::RecordName::RepairPayload,
            serde_json::to_value(self).map_err(|_| NativeError::Invalid)?,
        )
    }
    pub(crate) fn decode(bytes: &[u8]) -> NativeResult<Self> {
        let result: Self = records::record_data(&records::RecordName::RepairPayload, bytes)?;
        result.validate()?;
        Ok(result)
    }
    pub(crate) fn selection_hash(&self) -> NativeResult<[u8; 32]> {
        Ok(hash(
            &serde_json::to_vec(&self.selection).map_err(|_| NativeError::Invalid)?,
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PayloadRepairCatalogPhase {
    Active,
    Complete,
    Retired,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PayloadRepairCatalogSlot {
    operation: [u8; 16],
    selection_sha256: [u8; 32],
    phase: PayloadRepairCatalogPhase,
}
impl PayloadRepairCatalogSlot {
    pub(crate) fn phase(&self) -> PayloadRepairCatalogPhase {
        self.phase
    }
    pub(crate) fn matches(&self, record: &PayloadRepairRecord) -> NativeResult<()> {
        if self.operation != record.operation()
            || self.selection_sha256 != record.selection_hash()?
        {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PayloadRepairCatalog {
    schema_version: u32,
    slots: [Option<PayloadRepairCatalogSlot>; 3],
}
impl Default for PayloadRepairCatalog {
    fn default() -> Self {
        Self {
            schema_version: 1,
            slots: [None, None, None],
        }
    }
}
impl PayloadRepairCatalog {
    pub(crate) fn get(&self, slot: u8) -> Option<&PayloadRepairCatalogSlot> {
        self.slots.get(usize::from(slot)).and_then(Option::as_ref)
    }
    pub(crate) fn validate(&self) -> NativeResult<()> {
        if self.schema_version != 1 {
            return Err(NativeError::Invalid);
        }
        let mut active = 0;
        for (index, slot) in self.slots.iter().enumerate() {
            if let Some(slot) = slot {
                if slot.operation == [0; 16]
                    || slot.selection_sha256 == [0; 32]
                    || self.slots[..index]
                        .iter()
                        .flatten()
                        .any(|s| s.operation == slot.operation)
                {
                    return Err(NativeError::Invalid);
                }
                active += usize::from(slot.phase == PayloadRepairCatalogPhase::Active);
            }
        }
        if active > 1 {
            return Err(NativeError::Invalid);
        }
        Ok(())
    }
    pub(crate) fn reserve(&mut self, record: &PayloadRepairRecord) -> NativeResult<u8> {
        self.validate()?;
        record.validate()?;
        for (index, slot) in self.slots.iter().enumerate() {
            if let Some(slot) = slot {
                if slot.operation == record.operation() {
                    slot.matches(record)?;
                    return Ok(index as u8);
                }
                if slot.phase == PayloadRepairCatalogPhase::Active {
                    return Err(NativeError::Busy);
                }
            }
        }
        if record.phase() != PayloadRepairPhase::Selected {
            return Err(NativeError::Foreign);
        }
        let index = self
            .slots
            .iter()
            .position(Option::is_none)
            .ok_or(NativeError::Busy)?;
        self.slots[index] = Some(PayloadRepairCatalogSlot {
            operation: record.operation(),
            selection_sha256: record.selection_hash()?,
            phase: PayloadRepairCatalogPhase::Active,
        });
        self.validate()?;
        Ok(index as u8)
    }
    pub(crate) fn complete(&mut self, slot: u8, record: &PayloadRepairRecord) -> NativeResult<()> {
        if record.phase() != PayloadRepairPhase::Complete || record.slot() != Some(slot) {
            return Err(NativeError::Foreign);
        }
        let entry = self
            .slots
            .get_mut(usize::from(slot))
            .and_then(Option::as_mut)
            .ok_or(NativeError::Foreign)?;
        entry.matches(record)?;
        if entry.phase == PayloadRepairCatalogPhase::Retired {
            return Err(NativeError::Foreign);
        }
        entry.phase = PayloadRepairCatalogPhase::Complete;
        self.validate()
    }
    pub(crate) fn retire(&mut self, slot: u8, record: &PayloadRepairRecord) -> NativeResult<()> {
        if record.phase() != PayloadRepairPhase::Retired || record.slot() != Some(slot) {
            return Err(NativeError::Foreign);
        }
        let entry = self
            .slots
            .get_mut(usize::from(slot))
            .and_then(Option::as_mut)
            .ok_or(NativeError::Foreign)?;
        entry.matches(record)?;
        if !matches!(
            entry.phase,
            PayloadRepairCatalogPhase::Complete | PayloadRepairCatalogPhase::Retired
        ) {
            return Err(NativeError::Foreign);
        }
        entry.phase = PayloadRepairCatalogPhase::Retired;
        self.validate()
    }
    /// Called only after durable Cancelled publication and positive source-child/copy settlement.
    pub(crate) fn release_cancelled(&mut self, record: &PayloadRepairRecord) -> NativeResult<()> {
        record.validate()?;
        if record.phase() != PayloadRepairPhase::Cancelled {
            return Err(NativeError::Foreign);
        }
        let index = usize::from(record.slot().ok_or(NativeError::Foreign)?);
        let entry = self.slots.get(index).ok_or(NativeError::Foreign)?;
        if let Some(entry) = entry {
            entry.matches(record)?;
            if entry.phase != PayloadRepairCatalogPhase::Active {
                return Err(NativeError::Foreign);
            }
            self.slots[index] = None;
        }
        self.validate()
    }
    pub(crate) fn publication_successor(&self, next: &Self) -> NativeResult<()> {
        self.validate()?;
        next.validate()?;
        for (old, new) in self.slots.iter().zip(&next.slots) {
            if let Some(old) = old {
                let Some(new) = new.as_ref() else {
                    // Native publication additionally requires the exact durable Cancelled
                    // record before this one Active slot may be removed.
                    if old.phase != PayloadRepairCatalogPhase::Active {
                        return Err(NativeError::Foreign);
                    }
                    continue;
                };
                if old.operation != new.operation
                    || old.selection_sha256 != new.selection_sha256
                    || !matches!(
                        (old.phase, new.phase),
                        (
                            PayloadRepairCatalogPhase::Active,
                            PayloadRepairCatalogPhase::Active | PayloadRepairCatalogPhase::Complete
                        ) | (
                            PayloadRepairCatalogPhase::Complete,
                            PayloadRepairCatalogPhase::Complete
                                | PayloadRepairCatalogPhase::Retired
                        ) | (
                            PayloadRepairCatalogPhase::Retired,
                            PayloadRepairCatalogPhase::Retired
                        )
                    )
                {
                    return Err(NativeError::Foreign);
                }
            }
        }
        Ok(())
    }
    pub(crate) fn encode(&self) -> NativeResult<Vec<u8>> {
        self.validate()?;
        records::encode_record(
            &records::RecordName::RepairPayloadCatalog,
            serde_json::to_value(self).map_err(|_| NativeError::Invalid)?,
        )
    }
    pub(crate) fn decode(bytes: &[u8]) -> NativeResult<Self> {
        let result: Self = records::record_data(&records::RecordName::RepairPayloadCatalog, bytes)?;
        result.validate()?;
        Ok(result)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PayloadRepairPublicationTarget {
    Record,
    Catalog,
}
impl PayloadRepairPublicationTarget {
    pub(crate) fn name(self) -> records::RecordName {
        match self {
            Self::Record => records::RecordName::RepairPayload,
            Self::Catalog => records::RecordName::RepairPayloadCatalog,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PayloadRepairPublicationStamp {
    identity: FileStamp,
    sha256: [u8; 32],
    length: u64,
}
impl PayloadRepairPublicationStamp {
    pub(crate) fn new(identity: FileStamp, bytes: &[u8]) -> NativeResult<Self> {
        let result = Self {
            identity,
            sha256: hash(bytes),
            length: bytes.len() as u64,
        };
        result.validate()?;
        Ok(result)
    }
    fn validate(&self) -> NativeResult<()> {
        if !self.identity.valid()
            || self.sha256 == [0; 32]
            || self.length == 0
            || self.length > MAX_RECORD_BYTES as u64
        {
            return Err(NativeError::Invalid);
        }
        Ok(())
    }
    pub(crate) fn identity(&self) -> FileStamp {
        self.identity
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PayloadRepairPublicationPhase {
    Preparing,
    PendingReady,
    ReplaceIntent,
    Published,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PayloadRepairPublicationIntent {
    schema_version: u32,
    operation: [u8; 16],
    target: PayloadRepairPublicationTarget,
    old: Option<PayloadRepairPublicationStamp>,
    new_sha256: [u8; 32],
    new_length: u64,
    pending: Option<PayloadRepairPublicationStamp>,
    phase: PayloadRepairPublicationPhase,
}
impl PayloadRepairPublicationIntent {
    pub(crate) fn new(
        operation: [u8; 16],
        target: PayloadRepairPublicationTarget,
        old: Option<PayloadRepairPublicationStamp>,
        bytes: &[u8],
    ) -> NativeResult<Self> {
        records::validate_for(&target.name(), bytes)?;
        let result = Self {
            schema_version: 1,
            operation,
            target,
            old,
            new_sha256: hash(bytes),
            new_length: bytes.len() as u64,
            pending: None,
            phase: PayloadRepairPublicationPhase::Preparing,
        };
        result.validate()?;
        Ok(result)
    }
    pub(crate) fn validate(&self) -> NativeResult<()> {
        if self.schema_version != 1
            || self.operation == [0; 16]
            || self.new_sha256 == [0; 32]
            || self.new_length == 0
            || self.new_length > MAX_RECORD_BYTES as u64
        {
            return Err(NativeError::Invalid);
        }
        if let Some(old) = self.old {
            old.validate()?;
        }
        match (self.phase, self.pending) {
            (PayloadRepairPublicationPhase::Preparing, None) => {}
            (
                PayloadRepairPublicationPhase::PendingReady
                | PayloadRepairPublicationPhase::ReplaceIntent
                | PayloadRepairPublicationPhase::Published,
                Some(pending),
            ) => {
                pending.validate()?;
                if pending.sha256 != self.new_sha256 || pending.length != self.new_length {
                    return Err(NativeError::Invalid);
                }
            }
            _ => return Err(NativeError::Invalid),
        }
        Ok(())
    }
    pub(crate) fn target(&self) -> PayloadRepairPublicationTarget {
        self.target
    }
    pub(crate) fn phase(&self) -> PayloadRepairPublicationPhase {
        self.phase
    }
    pub(crate) fn pending(&self) -> Option<PayloadRepairPublicationStamp> {
        self.pending
    }
    pub(crate) fn matches_request(
        &self,
        operation: [u8; 16],
        target: PayloadRepairPublicationTarget,
        bytes: &[u8],
    ) -> bool {
        self.operation == operation
            && self.target == target
            && self.new_sha256 == hash(bytes)
            && self.new_length == bytes.len() as u64
    }
    pub(crate) fn pending_ready(
        &mut self,
        stamp: PayloadRepairPublicationStamp,
    ) -> NativeResult<()> {
        if self.phase != PayloadRepairPublicationPhase::Preparing
            || self.pending.is_some()
            || stamp.sha256 != self.new_sha256
            || stamp.length != self.new_length
        {
            return Err(NativeError::Foreign);
        }
        stamp.validate()?;
        self.pending = Some(stamp);
        self.phase = PayloadRepairPublicationPhase::PendingReady;
        self.validate()
    }
    pub(crate) fn replace_intent(&mut self) -> NativeResult<()> {
        if self.phase != PayloadRepairPublicationPhase::PendingReady {
            return Err(NativeError::Foreign);
        }
        self.phase = PayloadRepairPublicationPhase::ReplaceIntent;
        self.validate()
    }
    pub(crate) fn published(&mut self) -> NativeResult<()> {
        if self.phase != PayloadRepairPublicationPhase::ReplaceIntent {
            return Err(NativeError::Foreign);
        }
        self.phase = PayloadRepairPublicationPhase::Published;
        self.validate()
    }
    pub(crate) fn encode(&self) -> NativeResult<Vec<u8>> {
        self.validate()?;
        records::encode_record(
            &records::RecordName::RepairPayloadPublicationIntent,
            serde_json::to_value(self).map_err(|_| NativeError::Invalid)?,
        )
    }
    pub(crate) fn decode(bytes: &[u8]) -> NativeResult<Self> {
        let result: Self =
            records::record_data(&records::RecordName::RepairPayloadPublicationIntent, bytes)?;
        result.validate()?;
        Ok(result)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PayloadRepairPublicationObservation {
    Preparing,
    PendingReady,
    Published,
    Unknown,
}
pub(crate) fn recover_payload_repair_publication(
    intent: &PayloadRepairPublicationIntent,
    current: Option<PayloadRepairPublicationStamp>,
    pending: Option<PayloadRepairPublicationStamp>,
) -> PayloadRepairPublicationObservation {
    use PayloadRepairPublicationObservation::*;
    if intent.validate().is_err() {
        return Unknown;
    }
    match intent.phase {
        PayloadRepairPublicationPhase::Preparing if current == intent.old && pending.is_none() => {
            Preparing
        }
        PayloadRepairPublicationPhase::PendingReady
        | PayloadRepairPublicationPhase::ReplaceIntent
            if current == intent.old && pending == intent.pending =>
        {
            PendingReady
        }
        PayloadRepairPublicationPhase::ReplaceIntent | PayloadRepairPublicationPhase::Published
            if current == intent.pending && pending.is_none() =>
        {
            Published
        }
        _ => Unknown,
    }
}

/// Only the exact requested publication can be adopted across a transient terminal
/// file effect. An unrelated or torn intent is still Unknown. This is correlation only.
pub(crate) fn terminal_publication_admitted(
    intent: Option<&PayloadRepairPublicationIntent>,
    state: PayloadRepairPublicationObservation,
    operation: [u8; 16],
    target: PayloadRepairPublicationTarget,
    bytes: &[u8],
) -> bool {
    if state == PayloadRepairPublicationObservation::Unknown {
        return false;
    }
    match intent {
        None => state == PayloadRepairPublicationObservation::Published,
        Some(intent) => {
            intent.phase() == PayloadRepairPublicationPhase::Published
                && state == PayloadRepairPublicationObservation::Published
                || intent.matches_request(operation, target, bytes)
        }
    }
}

/// A settled retirement needs no cleanup capability. An unfinished catalog publication
/// remains resumable only for its exact operation and bytes; these are FILE facts only.
pub(crate) fn retired_cleanup_settled(
    record: &PayloadRepairRecord,
    history: &PayloadRepairCatalog,
    intent: Option<&PayloadRepairPublicationIntent>,
    state: PayloadRepairPublicationObservation,
) -> NativeResult<bool> {
    record.validate()?;
    history.validate()?;
    if record.phase() != PayloadRepairPhase::Retired {
        return Ok(false);
    }
    let entry = history
        .get(record.slot().ok_or(NativeError::Foreign)?)
        .ok_or(NativeError::Foreign)?;
    entry.matches(record)?;
    match entry.phase() {
        PayloadRepairCatalogPhase::Complete => Ok(false),
        PayloadRepairCatalogPhase::Retired => {
            // The protocol retains a settled Published intent as evidence. Clean means
            // no outstanding intent, not deleting that evidence file.
            if state == PayloadRepairPublicationObservation::Published
                && intent.is_none_or(|i| i.phase() == PayloadRepairPublicationPhase::Published)
            {
                return Ok(true);
            }
            let catalog_bytes = history.encode()?;
            if state != PayloadRepairPublicationObservation::Unknown
                && intent.is_some_and(|i| {
                    i.phase() != PayloadRepairPublicationPhase::Published
                        && i.matches_request(
                            record.operation(),
                            PayloadRepairPublicationTarget::Catalog,
                            &catalog_bytes,
                        )
                })
            {
                return Ok(false);
            }
            Err(NativeError::OutcomeUnknown)
        }
        PayloadRepairCatalogPhase::Active => Err(NativeError::Foreign),
    }
}

/// Repair-specific immutable history correlation. This observation is neither tree completion
/// nor native ownership; only a freshly claimed native task and exclusive owner may consume it.
#[cfg(any(windows, test))]
pub(crate) struct PayloadRepairLineage {
    operation: [u8; 16],
    predecessor: super::super::service::journal::Journal,
}
#[cfg(any(windows, test))]
impl PayloadRepairLineage {
    pub(crate) fn operation(&self) -> [u8; 16] {
        self.operation
    }
    pub(crate) fn matches_predecessor(
        &self,
        actual: &super::super::service::journal::Journal,
    ) -> bool {
        &self.predecessor == actual
    }
}
/// Matches only the strict repair selection and its exact completed predecessor. No upgrade
/// OperationRecord is fabricated and no native ownership is inferred from these bytes.
#[cfg(any(windows, test))]
pub(crate) fn correlate_repair(
    operation: [u8; 16],
    user: &str,
    selected: &PayloadRepairRecord,
    predecessor: &super::super::service::journal::Journal,
) -> NativeResult<PayloadRepairLineage> {
    use super::super::service::journal;
    selected.validate()?;
    let actual = journal::Journal::decode(&predecessor.encode()?)?;
    let original = selected.original_generation();
    if operation == [0; 16]
        || selected.operation() != operation
        || !matches!(
            selected.phase(),
            PayloadRepairPhase::StartIntent | PayloadRepairPhase::StartSubmitted
        )
        || actual.operation == operation
        || actual.user != user
        || actual.current != Some(original)
        || actual.phase != journal::Phase::Finished
        || actual.stop_instance.is_some()
    {
        return Err(NativeError::Foreign);
    }
    Ok(PayloadRepairLineage {
        operation,
        predecessor: actual,
    })
}
