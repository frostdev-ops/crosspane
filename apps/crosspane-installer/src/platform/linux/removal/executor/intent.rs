use super::super::{CleanupForm, RemovalError, Result};
use crate::platform::linux::native_io::{CleanupLease, Deadline};
use crate::platform::linux::payload::{FILES, MAX_RECORD_BYTES};
use crosspane_installer_core::OperationId;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CleanupResult {
    Pending,
    Removed,
    AlreadyAbsent,
    Kept,
    Refused,
    Failed,
    Unknown,
}
impl CleanupResult {
    fn decode(code: u8) -> Result<Self> {
        match code {
            0 => Ok(Self::Pending),
            1 => Ok(Self::Removed),
            2 => Ok(Self::AlreadyAbsent),
            3 => Ok(Self::Kept),
            4 => Ok(Self::Refused),
            5 => Ok(Self::Failed),
            6 => Ok(Self::Unknown),
            _ => Err(RemovalError::Invalid),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CleanupStage {
    Prepared,
    Disabled,
    StopObserved,
    IdentityObserved,
    FirewallObserved,
    FilesObserved,
    Finished,
}
impl CleanupStage {
    fn decode(code: u8) -> Result<Self> {
        match code {
            0 => Ok(Self::Prepared),
            1 => Ok(Self::Disabled),
            2 => Ok(Self::StopObserved),
            3 => Ok(Self::IdentityObserved),
            4 => Ok(Self::FirewallObserved),
            5 => Ok(Self::FilesObserved),
            6 => Ok(Self::Finished),
            _ => Err(RemovalError::Invalid),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanupProgress {
    pub stage: CleanupStage,
    /// Fixed order is the frozen payload::FILES inventory, not caller-supplied paths.
    pub resources: [CleanupResult; FILES.len()],
    pub autostart: CleanupResult,
    pub stop: CleanupResult,
    pub identity: CleanupResult,
    pub lan: CleanupResult,
    pub mdns: CleanupResult,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanupIntent {
    pub revision: u64,
    pub operation: OperationId,
    pub delete_identity: bool,
    pub lan_rule: bool,
    pub mdns_rule: bool,
    pub progress: CleanupProgress,
    /// Association data only; inventory validates this against its genuine proof.
    pub ledger_digest: [u8; 32],
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    schema_version: u8,
    revision: u64,
    operation: u64,
    ledger_digest: [u8; 32],
    delete_identity: bool,
    lan_rule: bool,
    mdns_rule: bool,
    stage: u8,
    resources: [u8; FILES.len()],
    autostart: u8,
    stop: u8,
    identity: u8,
    lan: u8,
    mdns: u8,
}
impl CleanupIntent {
    /// Crash records never reconstruct an in-memory process watch or erase permission.
    pub fn form(&self) -> CleanupForm {
        CleanupForm::NotCleanRetainIdentityAndRecovery
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_RECORD_BYTES
            || bytes.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{')
        {
            return Err(RemovalError::Invalid);
        }
        let wire: Wire = serde_json::from_slice(bytes).map_err(|_| RemovalError::Invalid)?;
        if wire.schema_version != 1 || wire.revision == 0 || wire.operation == 0 {
            return Err(RemovalError::Invalid);
        }
        let mut results = [CleanupResult::Pending; FILES.len()];
        for (result, code) in results.iter_mut().zip(wire.resources) {
            *result = CleanupResult::decode(code)?;
        }
        Ok(Self {
            revision: wire.revision,
            operation: OperationId(wire.operation),
            ledger_digest: wire.ledger_digest,
            delete_identity: wire.delete_identity,
            lan_rule: wire.lan_rule,
            mdns_rule: wire.mdns_rule,
            progress: CleanupProgress {
                stage: CleanupStage::decode(wire.stage)?,
                resources: results,
                autostart: CleanupResult::decode(wire.autostart)?,
                stop: CleanupResult::decode(wire.stop)?,
                identity: CleanupResult::decode(wire.identity)?,
                lan: CleanupResult::decode(wire.lan)?,
                mdns: CleanupResult::decode(wire.mdns)?,
            },
        })
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.revision == 0 || self.operation.0 == 0 {
            return Err(RemovalError::Invalid);
        }
        serde_json::to_vec(&Wire {
            schema_version: 1,
            revision: self.revision,
            operation: self.operation.0,
            ledger_digest: self.ledger_digest,
            delete_identity: self.delete_identity,
            lan_rule: self.lan_rule,
            mdns_rule: self.mdns_rule,
            stage: self.progress.stage as u8,
            resources: self.progress.resources.map(|r| r as u8),
            autostart: self.progress.autostart as u8,
            stop: self.progress.stop as u8,
            identity: self.progress.identity as u8,
            lan: self.progress.lan as u8,
            mdns: self.progress.mdns as u8,
        })
        .map_err(|_| RemovalError::Invalid)
    }
}
/// Low-level durable observations, not consent, clean-exit or resource-deletion authority.
#[derive(Debug)]
pub struct CleanupStore(CleanupLease);
impl CleanupStore {
    pub fn new(lease: CleanupLease) -> Self {
        Self(lease)
    }
    pub fn read(&self, deadline: &Deadline) -> Result<Option<CleanupIntent>> {
        self.0
            .read_intent(deadline)?
            .map(|bytes| CleanupIntent::decode(&bytes))
            .transpose()
    }
    pub fn write(&self, record: &CleanupIntent, deadline: &Deadline) -> Result<()> {
        self.0.write_intent(&record.encode()?, deadline)?;
        Ok(())
    }
}
