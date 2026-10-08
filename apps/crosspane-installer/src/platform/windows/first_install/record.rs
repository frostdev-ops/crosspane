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
