//! Strict first-install lineage; decoding these observations never constructs a native permit.
use super::super::{
    native_io::{
        NativeError, NativeResult,
        records::{self, RecordName},
    },
    payload::{
        inventory::{PayloadRole, PeFacts},
        recovery::{FileStamp, ImageObservation, OriginalLeaf},
    },
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Phase {
    Intent,
    StageIntent(PayloadRole),
    Staged(PayloadRole),
    BackupIntent(PayloadRole),
    BackedUp(PayloadRole),
    PublishIntent(PayloadRole),
    Published(PayloadRole),
    FilesVerified,
    TaskIntent,
    TaskRegistered,
    RunIntent,
    RunObserved,
    Ready,
    PruneIntent,
    Complete,
    Unknown,
}
impl Phase {
    pub(crate) fn rank(self) -> u16 {
        match self {
            Self::Intent => 0,
            Self::StageIntent(r) => 1 + 2 * r as u16,
            Self::Staged(r) => 2 + 2 * r as u16,
            Self::BackupIntent(r) => 9 + 4 * r as u16,
            Self::BackedUp(r) => 10 + 4 * r as u16,
            Self::PublishIntent(r) => 11 + 4 * r as u16,
            Self::Published(r) => 12 + 4 * r as u16,
            Self::FilesVerified => 25,
            Self::TaskIntent => 26,
            Self::TaskRegistered => 27,
            Self::RunIntent => 28,
            Self::RunObserved => 29,
            Self::Ready => 30,
            Self::PruneIntent => 31,
            Self::Complete => 32,
            Self::Unknown => 33,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Role {
    pub role: PayloadRole,
    pub approved: PeFacts,
    pub original: OriginalLeaf,
    pub staged: Option<ImageObservation>,
    pub backup: Option<FileStamp>,
    pub published: Option<ImageObservation>,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FirstInstallRecord {
    schema_version: u32,
    operation: [u8; 16],
    /// Exact bounded native context correlation. Only a live native context can renew it.
    context: Vec<u8>,
    phase: Phase,
    roles: Vec<Role>,
    instance: Option<u64>,
    retention_incomplete: bool,
}
impl std::fmt::Debug for FirstInstallRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FirstInstallRecord(..)")
    }
}
impl FirstInstallRecord {
    pub(crate) fn new(
        operation: [u8; 16],
        context: Vec<u8>,
        pins: [PeFacts; 4],
    ) -> NativeResult<Self> {
        let value = Self {
            schema_version: 1,
            operation,
            context,
            phase: Phase::Intent,
            roles: PayloadRole::ALL
                .into_iter()
                .zip(pins)
                .map(|(role, approved)| Role {
                    role,
                    approved,
                    original: OriginalLeaf::Unobserved,
                    staged: None,
                    backup: None,
                    published: None,
                })
                .collect(),
            instance: None,
            retention_incomplete: false,
        };
        value.validate()?;
        Ok(value)
    }
    /// Intent has no staged, backed-up or published effect. Rebinding it preserves the
    /// operation and approved pins; actual current-context absence must still be reserved.
    pub(crate) fn restart_intent(&self, context: Vec<u8>) -> NativeResult<Self> {
        self.validate()?;
        if self.phase != Phase::Intent
            || self.instance.is_some()
            || self.retention_incomplete
            || self.roles.iter().any(|r| {
                r.original != OriginalLeaf::Unobserved
                    || r.staged.is_some()
                    || r.backup.is_some()
                    || r.published.is_some()
            })
        {
            return Err(NativeError::OutcomeUnknown);
        }
        let mut next = self.clone();
        next.context = context;
        next.validate()?;
        Ok(next)
    }
    pub(crate) fn operation(&self) -> [u8; 16] {
        self.operation
    }
    pub(crate) fn context(&self) -> &[u8] {
        &self.context
    }
    pub(crate) fn phase(&self) -> Phase {
        self.phase
    }
    pub(crate) fn instance(&self) -> Option<u64> {
        self.instance
    }
    pub(crate) fn role(&self, role: PayloadRole) -> NativeResult<&Role> {
        self.roles
            .get(role as usize)
            .filter(|r| r.role == role)
            .ok_or(NativeError::Invalid)
    }
    pub(crate) fn role_mut(&mut self, role: PayloadRole) -> NativeResult<&mut Role> {
        self.roles
            .get_mut(role as usize)
            .filter(|r| r.role == role)
            .ok_or(NativeError::Invalid)
    }
    /// The only role whose stage leaf can be a stray: a first install cut at `StageIntent(role)`
    /// before that role's staged identity was persisted.
    pub(crate) fn stray_stage_role(&self) -> Option<PayloadRole> {
        match self.phase {
            Phase::StageIntent(role) if self.role(role).is_ok_and(|r| r.staged.is_none()) => {
                Some(role)
            }
            _ => None,
        }
    }
    pub(crate) fn advance(&mut self, phase: Phase) -> NativeResult<()> {
        if self.phase == Phase::Unknown || phase.rank() < self.phase.rank() {
            return Err(NativeError::OutcomeUnknown);
        }
        self.phase = phase;
        self.validate()
    }
    pub(crate) fn ready(&mut self, instance: u64) -> NativeResult<()> {
        if instance == 0 {
            return Err(NativeError::Invalid);
        }
        self.instance = Some(instance);
        self.advance(Phase::Ready)
    }
    pub(crate) fn retention(&mut self, incomplete: bool) {
        self.retention_incomplete = incomplete;
    }
    pub(crate) fn same_selection(&self, old: &Self) -> bool {
        self.operation == old.operation
            && self.context == old.context
            && self
                .roles
                .iter()
                .zip(&old.roles)
                .all(|(a, b)| a.role == b.role && a.approved == b.approved)
    }
    pub(crate) fn validate(&self) -> NativeResult<()> {
        if self.schema_version != 1
            || self.operation == [0; 16]
            || self.context.is_empty()
            || self.context.len() > 2048
            || self.roles.len() != 4
            || self.instance == Some(0)
        {
            return Err(NativeError::Invalid);
        }
        for role in PayloadRole::ALL {
            let r = self.role(role)?;
            if !r.approved.valid()
                || matches!(r.original, OriginalLeaf::Present(id) if !id.valid())
                || r.backup.is_some_and(|id| !id.valid())
            {
                return Err(NativeError::Invalid);
            }
            for image in [&r.staged, &r.published].into_iter().flatten() {
                if !image.identity.valid() || image.facts != r.approved {
                    return Err(NativeError::Invalid);
                }
            }
            if self.phase != Phase::Unknown {
                if self.phase.rank() >= Phase::Staged(role).rank() && r.staged.is_none() {
                    return Err(NativeError::Invalid);
                }
                if self.phase.rank() >= Phase::BackupIntent(role).rank()
                    && r.original == OriginalLeaf::Unobserved
                {
                    return Err(NativeError::Invalid);
                }
                if self.phase.rank() >= Phase::BackedUp(role).rank() {
                    match (r.original, r.backup) {
                        (OriginalLeaf::Missing, None) => {}
                        (OriginalLeaf::Present(id), Some(actual)) if id == actual => {}
                        _ => return Err(NativeError::Invalid),
                    }
                }
                if self.phase.rank() >= Phase::Published(role).rank()
                    && (r.published.is_none() || r.published != r.staged)
                {
                    return Err(NativeError::Invalid);
                }
            }
        }
        if self.phase.rank() >= Phase::Ready.rank()
            && self.phase != Phase::Unknown
            && self.instance.is_none()
        {
            return Err(NativeError::Invalid);
        }
        Ok(())
    }
    pub(crate) fn encode(&self) -> NativeResult<Vec<u8>> {
        self.validate()?;
        records::encode_record(
            &RecordName::FirstInstall,
            serde_json::to_value(self).map_err(|_| NativeError::Invalid)?,
        )
    }
    pub(crate) fn decode(bytes: &[u8]) -> NativeResult<Self> {
        let value: Self = records::record_data(&RecordName::FirstInstall, bytes)?;
        value.validate()?;
        Ok(value)
    }
}

/// Immutable correlations only. These models cannot construct any native owner or launch seal.
pub(crate) const MAX_HISTORY_LEAVES: usize = 32;
pub(crate) fn history_digest(bytes: &[u8]) -> [u8; 32] {
    let hash = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes);
    let mut out = [0; 32];
    out.copy_from_slice(hash.as_ref());
    out
}
pub(crate) fn history_leaf_allowed(leaf: &str) -> bool {
    matches!(
        leaf,
        "supervisor.json"
            | "supervisor-logon.json"
            | "task-activation.json"
            | "supervisor-epoch-0.json"
            | "supervisor-epoch-1.json"
            | "supervisor-epoch-2.json"
            | "supervisor-archive-intent.json"
            | "removal.json"
            | "first-install.json"
            | "first-install-recovery.json"
            | "stage-catalog.json"
            | "outer-upgrade.json"
            | "file-recovery.json"
            | "repair.json"
            | "repair-publication-intent.json"
            | "repair-pending.json"
            | "repair-evidence-index.json"
            | "repair-payload.json"
            | "repair-payload-catalog.json"
            | "repair-payload-publication-intent.json"
            | "repair-payload-pending.json"
    ) || leaf
        .strip_prefix("operation-")
        .and_then(|s| s.strip_suffix(".json"))
        .is_some_and(|id| {
            id.len() == 32
                && id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FirstHistorySource {
    Removal,
    Partial,
    Stale,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HistoryLeaf {
    pub leaf: String,
    pub identity: FileStamp,
    pub length: u32,
    pub digest: [u8; 32],
}
impl HistoryLeaf {
    pub(crate) fn observe(leaf: String, identity: FileStamp, bytes: &[u8]) -> NativeResult<Self> {
        let value = Self {
            leaf,
            identity,
            length: u32::try_from(bytes.len()).map_err(|_| NativeError::Oversize)?,
            digest: history_digest(bytes),
        };
        value.validate()?;
        Ok(value)
    }
    pub(crate) fn validate(&self) -> NativeResult<()> {
        if !history_leaf_allowed(&self.leaf)
            || !self.identity.valid()
            || self.length == 0
            || self.length as usize > super::super::native_io::files::MAX_RECORD_BYTES
            || self.digest == [0; 32]
        {
            return Err(NativeError::Invalid);
        }
        Ok(())
    }
    pub(crate) fn matches(&self, identity: FileStamp, bytes: &[u8]) -> bool {
        self.identity == identity
            && self.length as usize == bytes.len()
            && self.digest == history_digest(bytes)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FirstHistorySlot {
    pub source: FirstHistorySource,
    pub operation: [u8; 16],
    pub next_operation: [u8; 16],
    pub context: Vec<u8>,
    pub leaves: Vec<HistoryLeaf>,
}
impl FirstHistorySlot {
    pub(crate) fn validate(&self) -> NativeResult<()> {
        if self.operation == [0; 16]
            || self.next_operation == [0; 16]
            || self.operation == self.next_operation
            || self.context.is_empty()
            || self.context.len() > 2048
            || self.leaves.is_empty()
            || self.leaves.len() > MAX_HISTORY_LEAVES
        {
            return Err(NativeError::Invalid);
        }
        for (index, leaf) in self.leaves.iter().enumerate() {
            leaf.validate()?;
            if self.leaves[..index]
                .iter()
                .any(|old| old.leaf == leaf.leaf || old.identity == leaf.identity)
            {
                return Err(NativeError::Foreign);
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FirstHistoryIntent {
    pub schema_version: u32,
    pub slot: u8,
    pub selected: FirstHistorySlot,
    pub moved: usize,
    pub complete: bool,
}
impl FirstHistoryIntent {
    pub(crate) fn validate(&self) -> NativeResult<()> {
        self.selected.validate()?;
        if self.schema_version != 1
            || self.slot > 2
            || self.moved > self.selected.leaves.len()
            || self.complete && self.moved != self.selected.leaves.len()
        {
            return Err(NativeError::Invalid);
        }
        Ok(())
    }
    pub(crate) fn encode(&self) -> NativeResult<Vec<u8>> {
        self.validate()?;
        records::encode_record(
            &RecordName::FirstInstallHistoryIntent,
            serde_json::to_value(self).map_err(|_| NativeError::Invalid)?,
        )
    }
    pub(crate) fn decode(bytes: &[u8]) -> NativeResult<Self> {
        let value: Self = records::record_data(&RecordName::FirstInstallHistoryIntent, bytes)?;
        value.validate()?;
        Ok(value)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FirstHistoryIndex {
    pub schema_version: u32,
    pub slots: [Option<FirstHistorySlot>; 3],
}
impl Default for FirstHistoryIndex {
    fn default() -> Self {
        Self {
            schema_version: 1,
            slots: [None, None, None],
        }
    }
}
impl FirstHistoryIndex {
    pub(crate) fn validate(&self) -> NativeResult<()> {
        if self.schema_version != 1 {
            return Err(NativeError::Invalid);
        }
        for (index, value) in self.slots.iter().enumerate() {
            if let Some(value) = value {
                value.validate()?;
                if self.slots[..index].iter().flatten().any(|old| {
                    old.operation == value.operation || old.next_operation == value.next_operation
                }) {
                    return Err(NativeError::Foreign);
                }
            }
        }
        Ok(())
    }
    pub(crate) fn vacant(&self) -> NativeResult<u8> {
        self.validate()?;
        self.slots
            .iter()
            .position(Option::is_none)
            .map(|n| n as u8)
            .ok_or(NativeError::Busy)
    }
    pub(crate) fn select(&self, selected: FirstHistorySlot) -> NativeResult<FirstHistoryIntent> {
        selected.validate()?;
        let value = FirstHistoryIntent {
            schema_version: 1,
            slot: self.vacant()?,
            selected,
            moved: 0,
            complete: false,
        };
        value.validate()?;
        Ok(value)
    }
    pub(crate) fn commit(&mut self, intent: &FirstHistoryIntent) -> NativeResult<()> {
        self.validate()?;
        intent.validate()?;
        if intent.moved != intent.selected.leaves.len() {
            return Err(NativeError::OutcomeUnknown);
        }
        match &self.slots[usize::from(intent.slot)] {
            None => self.slots[usize::from(intent.slot)] = Some(intent.selected.clone()),
            Some(old) if old == &intent.selected => {}
            _ => return Err(NativeError::Foreign),
        }
        self.validate()
    }
    pub(crate) fn encode(&self) -> NativeResult<Vec<u8>> {
        self.validate()?;
        records::encode_record(
            &RecordName::FirstInstallHistoryIndex,
            serde_json::to_value(self).map_err(|_| NativeError::Invalid)?,
        )
    }
    pub(crate) fn decode(bytes: &[u8]) -> NativeResult<Self> {
        let value: Self = records::record_data(&RecordName::FirstInstallHistoryIndex, bytes)?;
        value.validate()?;
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FirstRecoveryMode {
    Rollback,
    Remove,
    Supersede,
    RetireStale,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum FirstRecoveryCursor {
    Selected,
    TaskDeleteIntent,
    TaskAbsent,
    Role { index: u8, step: u8 },
    RolledBack,
    FilesRemoved,
    Superseded,
    CleanupIntent,
    CleanupDone,
    RetireIntent,
    Retired,
}
impl FirstRecoveryCursor {
    fn rank(self) -> NativeResult<u32> {
        Ok(match self {
            Self::Selected => 0,
            Self::TaskDeleteIntent => 1,
            Self::TaskAbsent => 2,
            Self::Role { index, step } if index < 4 && step < 6 => {
                3 + u32::from(index) * 6 + u32::from(step)
            }
            Self::Role { .. } => return Err(NativeError::Invalid),
            Self::RolledBack | Self::FilesRemoved | Self::Superseded => 27,
            Self::CleanupIntent => 28,
            Self::CleanupDone => 29,
            Self::RetireIntent => 30,
            Self::Retired => 31,
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FirstRecoveryRecord {
    pub schema_version: u32,
    pub source: HistoryLeaf,
    pub document: FirstInstallRecord,
    pub mode: FirstRecoveryMode,
    pub cursor: FirstRecoveryCursor,
    pub pass: u32,
    pub pending: [bool; 4],
    pub scaffold: [Option<FileStamp>; 5],
    pub pending_scaffold: bool,
    /// The stage leaf observed before any effect, while the first install was at
    /// `StageIntent(role)` with no persisted staged identity. Only this exact FileId may be
    /// deleted for that role. A record without the field decodes as `None`.
    #[serde(default)]
    pub stray_stage: Option<(PayloadRole, FileStamp)>,
}
impl FirstRecoveryRecord {
    pub(crate) fn new(
        source: HistoryLeaf,
        document: FirstInstallRecord,
        mode: FirstRecoveryMode,
    ) -> NativeResult<Self> {
        let value = Self {
            schema_version: 1,
            source,
            document,
            mode,
            cursor: FirstRecoveryCursor::Selected,
            pass: 0,
            pending: [false; 4],
            scaffold: [None; 5],
            pending_scaffold: false,
            stray_stage: None,
        };
        value.validate()?;
        Ok(value)
    }
    /// The stage identity this record may delete for `role`: the persisted staged identity, or
    /// the recorded stray leaf for that one role. Never a leaf of another role.
    pub(crate) fn stage_identity(&self, role: PayloadRole) -> NativeResult<Option<FileStamp>> {
        let selected = self.document.role(role)?;
        Ok(match (&selected.staged, self.stray_stage) {
            (Some(image), _) => Some(image.identity),
            (None, Some((stray_role, identity))) if stray_role == role => Some(identity),
            (None, _) => None,
        })
    }
    pub(crate) fn validate(&self) -> NativeResult<()> {
        self.source.validate()?;
        self.document.validate()?;
        self.cursor.rank()?;
        if self.scaffold.iter().flatten().any(|id| !id.valid())
            || (self.cursor == FirstRecoveryCursor::Retired
                && (self.pending_scaffold || self.pending.iter().any(|p| *p)))
        {
            return Err(NativeError::Invalid);
        }
        if let Some((role, identity)) = self.stray_stage {
            let selected = self.document.role(role)?;
            if !identity.valid()
                || !matches!(
                    self.mode,
                    FirstRecoveryMode::Rollback | FirstRecoveryMode::Remove
                )
                || self.document.phase() != Phase::StageIntent(role)
                || selected.staged.is_some()
            {
                return Err(NativeError::Invalid);
            }
        }
        if self.schema_version != 1
            || self.source.leaf != "first-install.json"
            || !self
                .source
                .matches(self.source.identity, &self.document.encode()?)
            || matches!(self.cursor, FirstRecoveryCursor::RolledBack)
                && self.mode != FirstRecoveryMode::Rollback
            || matches!(self.cursor, FirstRecoveryCursor::FilesRemoved)
                && self.mode != FirstRecoveryMode::Remove
            || matches!(self.cursor, FirstRecoveryCursor::Superseded)
                && self.mode != FirstRecoveryMode::Supersede
        {
            return Err(NativeError::Invalid);
        }
        Ok(())
    }
    /// A new explicit removal attempt observes every name again. Previous effects are never
    /// redispatched unless a fresh exact object is still present under the same selection.
    pub(crate) fn retry_retained(&self) -> NativeResult<Self> {
        self.validate()?;
        if self.mode != FirstRecoveryMode::Remove
            || !(self.pending_scaffold || self.pending.iter().any(|p| *p))
            || !matches!(
                self.cursor,
                FirstRecoveryCursor::Role { index: 3, step: 5 }
                    | FirstRecoveryCursor::CleanupIntent
            )
        {
            return Err(NativeError::Foreign);
        }
        let mut next = self.clone();
        next.pass = next.pass.checked_add(1).ok_or(NativeError::Oversize)?;
        next.cursor = FirstRecoveryCursor::Selected;
        next.pending = [false; 4];
        next.pending_scaffold = false;
        next.validate()?;
        Ok(next)
    }
    pub(crate) fn select_retirement(&self) -> NativeResult<Self> {
        self.validate()?;
        if self.mode != FirstRecoveryMode::Supersede
            || self.cursor != FirstRecoveryCursor::Superseded
        {
            return Err(NativeError::Foreign);
        }
        let mut next = Self::new(
            self.source.clone(),
            self.document.clone(),
            FirstRecoveryMode::RetireStale,
        )?;
        next.scaffold = self.scaffold;
        next.stray_stage = self.stray_stage;
        next.pass = self.pass.checked_add(1).ok_or(NativeError::Oversize)?;
        Ok(next)
    }
    pub(crate) fn select_removal(&self) -> NativeResult<Self> {
        self.validate()?;
        if self.mode != FirstRecoveryMode::Rollback
            || matches!(
                self.cursor,
                FirstRecoveryCursor::RetireIntent | FirstRecoveryCursor::Retired
            )
        {
            return Err(NativeError::Foreign);
        }
        let mut next = Self::new(
            self.source.clone(),
            self.document.clone(),
            FirstRecoveryMode::Remove,
        )?;
        next.scaffold = self.scaffold;
        next.stray_stage = self.stray_stage;
        next.pass = self.pass.checked_add(1).ok_or(NativeError::Oversize)?;
        Ok(next)
    }
    pub(crate) fn follows(&self, old: &Self) -> NativeResult<()> {
        self.validate()?;
        old.validate()?;
        if self.source != old.source
            || self.document != old.document
            || self.scaffold != old.scaffold
            || self.stray_stage != old.stray_stage
        {
            return Err(NativeError::Foreign);
        }
        if self.pass == old.pass {
            if self.mode != old.mode || self.cursor.rank()? < old.cursor.rank()? {
                return Err(NativeError::Foreign);
            }
        } else {
            let next = if self.mode == FirstRecoveryMode::RetireStale
                && old.mode == FirstRecoveryMode::Supersede
            {
                old.select_retirement()?
            } else if self.mode == old.mode {
                old.retry_retained()?
            } else {
                old.select_removal()?
            };
            if *self != next {
                return Err(NativeError::Foreign);
            }
        }
        Ok(())
    }
    pub(crate) fn advance(&mut self, next: FirstRecoveryCursor) -> NativeResult<()> {
        self.validate()?;
        if next.rank()? < self.cursor.rank()? {
            return Err(NativeError::Foreign);
        }
        self.cursor = next;
        self.validate()
    }
    pub(crate) fn encode(&self) -> NativeResult<Vec<u8>> {
        self.validate()?;
        records::encode_record(
            &RecordName::FirstInstallRecovery,
            serde_json::to_value(self).map_err(|_| NativeError::Invalid)?,
        )
    }
    pub(crate) fn decode(bytes: &[u8]) -> NativeResult<Self> {
        let value: Self = records::record_data(&RecordName::FirstInstallRecovery, bytes)?;
        value.validate()?;
        Ok(value)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FirstRecoveryOutcome {
    RolledBack,
    Removed,
    Superseded,
    Retired,
    Retained { pending: usize },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RoleTriad {
    pub stage: Option<FileStamp>,
    pub fixed: Option<FileStamp>,
    pub backup: Option<FileStamp>,
}
