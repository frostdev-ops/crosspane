use super::{NativeError, NativeResult, files::MAX_RECORD_BYTES, files::check_read_size};
use aws_lc_rs::digest::{SHA256, digest};
use crosspane_installer_core::elevated::journal;
use serde::{Deserialize, Serialize};

/// Content expectations alone confer no namespace or mutation authority. A fresh support proof
/// and retained installer lock are always required to inspect or publish these names.
#[derive(Clone)]
pub struct PublicationIntent {
    pub(crate) old: Option<[u8; 32]>,
    pub(crate) new: [u8; 32],
    pub(crate) temporary: Option<super::files::PrivateName>,
    pub(crate) target: RecordName,
    pub(crate) context: Option<[u8; 16]>,
}
impl PublicationIntent {
    #[cfg(test)]
    pub fn fixture(old: &[u8], new: &[u8]) -> Self {
        Self {
            old: Some(fingerprint(old)),
            new: fingerprint(new),
            temporary: None,
            target: RecordName::Receipt,
            context: None,
        }
    }
    pub fn temporary_name(&self) -> Option<&super::files::PrivateName> {
        self.temporary.as_ref()
    }
}
impl std::fmt::Debug for PublicationIntent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PublicationIntent")
    }
}
fn fingerprint(bytes: &[u8]) -> [u8; 32] {
    let hash = digest(&SHA256, bytes);
    let mut value = [0; 32];
    value.copy_from_slice(hash.as_ref());
    value
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublicationRecovery {
    OldRetained,
    NewPublished,
    RecoveryRequired,
    Unknown,
}
pub fn recover(
    intent: &PublicationIntent,
    record: Option<&[u8]>,
    temporary: Option<&[u8]>,
) -> PublicationRecovery {
    if record.is_some_and(|b| b.len() > MAX_RECORD_BYTES)
        || temporary.is_some_and(|b| b.len() > MAX_RECORD_BYTES)
    {
        return PublicationRecovery::Unknown;
    }
    let current = record.map(fingerprint);
    let pending = temporary.map(fingerprint);
    if current == Some(intent.new) && pending.is_none() {
        PublicationRecovery::NewPublished
    } else if current == intent.old && (pending.is_none() || pending == Some(intent.new)) {
        PublicationRecovery::OldRetained
    } else if current == intent.old || current.is_none() {
        PublicationRecovery::RecoveryRequired
    } else {
        PublicationRecovery::Unknown
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecordKind {
    FirstInstall,
    FirstInstallHistoryIntent,
    FirstInstallHistoryIndex,
    FirstInstallRecovery,
    Receipt,
    StageCatalog,
    OuterUpgrade,
    FileRecovery,
    Removal,
    Repair,
    RepairPublicationIntent,
    RepairPending,
    RepairEvidence,
    RepairPayload,
    RepairPayloadPublicationIntent,
    RepairPayloadPending,
    RepairPayloadCatalog,
    Operation,
    Supervisor,
    TaskActivation,
    SupervisorLogon,
    SupervisorArchiveIntent,
    ElevatedSetup,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordName {
    FirstInstall,
    FirstInstallHistoryIntent,
    FirstInstallHistoryIndex,
    FirstInstallRecovery,
    Receipt,
    StageCatalog,
    OuterUpgrade,
    FileRecovery,
    Removal,
    Repair,
    RepairPublicationIntent,
    RepairPending,
    RepairEvidence,
    RepairPayload,
    RepairPayloadPublicationIntent,
    RepairPayloadPending,
    RepairPayloadCatalog,
    Operation([u8; 16]),
    Supervisor,
    TaskActivation,
    SupervisorLogon,
    SupervisorEpoch(u8),
    SupervisorArchiveIntent,
    ElevatedSetup,
}
impl RecordName {
    pub fn file_name(&self) -> NativeResult<super::files::PrivateName> {
        match self {
            Self::FirstInstall => super::files::PrivateName::new("first-install.json"),
            Self::FirstInstallHistoryIntent => {
                super::files::PrivateName::new("first-install-history-intent.json")
            }
            Self::FirstInstallHistoryIndex => {
                super::files::PrivateName::new("first-install-history-index.json")
            }
            Self::FirstInstallRecovery => {
                super::files::PrivateName::new("first-install-recovery.json")
            }
            Self::Receipt => super::files::PrivateName::new("receipt.json"),
            Self::StageCatalog => super::files::PrivateName::new("stage-catalog.json"),
            Self::OuterUpgrade => super::files::PrivateName::new("outer-upgrade.json"),
            Self::FileRecovery => super::files::PrivateName::new("file-recovery.json"),
            Self::Removal => super::files::PrivateName::new("removal.json"),
            Self::Repair => super::files::PrivateName::new("repair.json"),
            Self::RepairPublicationIntent => {
                super::files::PrivateName::new("repair-publication-intent.json")
            }
            Self::RepairPending => super::files::PrivateName::new("repair-pending.json"),
            Self::RepairEvidence => super::files::PrivateName::new("repair-evidence-index.json"),
            Self::RepairPayload => super::files::PrivateName::new("repair-payload.json"),
            Self::RepairPayloadPublicationIntent => {
                super::files::PrivateName::new("repair-payload-publication-intent.json")
            }
            Self::RepairPayloadPending => {
                super::files::PrivateName::new("repair-payload-pending.json")
            }
            Self::RepairPayloadCatalog => {
                super::files::PrivateName::new("repair-payload-catalog.json")
            }
            Self::Supervisor => super::files::PrivateName::new("supervisor.json"),
            Self::TaskActivation => super::files::PrivateName::new("task-activation.json"),
            Self::SupervisorLogon => super::files::PrivateName::new("supervisor-logon.json"),
            Self::SupervisorEpoch(0) => super::files::PrivateName::new("supervisor-epoch-0.json"),
            Self::SupervisorEpoch(1) => super::files::PrivateName::new("supervisor-epoch-1.json"),
            Self::SupervisorEpoch(2) => super::files::PrivateName::new("supervisor-epoch-2.json"),
            Self::SupervisorArchiveIntent => {
                super::files::PrivateName::new("supervisor-archive-intent.json")
            }
            Self::ElevatedSetup => super::files::PrivateName::new(journal::RECORD_LEAF),
            Self::Operation(id) if *id != [0; 16] => {
                super::files::PrivateName::new(&format!("operation-{}.json", hex(id)))
            }
            _ => Err(NativeError::Invalid),
        }
    }
    fn kind(&self) -> RecordKind {
        match self {
            Self::FirstInstall => RecordKind::FirstInstall,
            Self::FirstInstallHistoryIntent => RecordKind::FirstInstallHistoryIntent,
            Self::FirstInstallHistoryIndex => RecordKind::FirstInstallHistoryIndex,
            Self::FirstInstallRecovery => RecordKind::FirstInstallRecovery,
            Self::Receipt => RecordKind::Receipt,
            Self::StageCatalog => RecordKind::StageCatalog,
            Self::OuterUpgrade => RecordKind::OuterUpgrade,
            Self::FileRecovery => RecordKind::FileRecovery,
            Self::Removal => RecordKind::Removal,
            Self::Repair => RecordKind::Repair,
            Self::RepairPublicationIntent => RecordKind::RepairPublicationIntent,
            Self::RepairPending => RecordKind::RepairPending,
            Self::RepairEvidence => RecordKind::RepairEvidence,
            Self::RepairPayload => RecordKind::RepairPayload,
            Self::RepairPayloadPublicationIntent => RecordKind::RepairPayloadPublicationIntent,
            Self::RepairPayloadPending => RecordKind::RepairPayloadPending,
            Self::RepairPayloadCatalog => RecordKind::RepairPayloadCatalog,
            Self::Operation(_) => RecordKind::Operation,
            Self::Supervisor => RecordKind::Supervisor,
            Self::TaskActivation => RecordKind::TaskActivation,
            Self::SupervisorLogon => RecordKind::SupervisorLogon,
            Self::SupervisorEpoch(_) => RecordKind::Supervisor,
            Self::SupervisorArchiveIntent => RecordKind::SupervisorArchiveIntent,
            Self::ElevatedSetup => RecordKind::ElevatedSetup,
        }
    }
    fn operation(&self) -> Option<[u8; 16]> {
        match self {
            Self::Operation(id) => Some(*id),
            _ => None,
        }
    }
}
pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|b| {
            [
                char::from(DIGITS[(b >> 4) as usize]),
                char::from(DIGITS[(b & 15) as usize]),
            ]
        })
        .collect()
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope<T = serde_json::Value> {
    schema_version: u32,
    kind: RecordKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    operation: Option<[u8; 16]>,
    data: T,
}
fn parse_record(bytes: &[u8]) -> NativeResult<Envelope> {
    check_read_size(bytes.len(), MAX_RECORD_BYTES)?;
    let envelope: Envelope = serde_json::from_slice(bytes).map_err(|_| NativeError::Invalid)?;
    if envelope.schema_version != 1
        || !envelope.data.is_object()
        || (envelope.kind == RecordKind::Operation) != envelope.operation.is_some()
        || envelope.operation == Some([0; 16])
    {
        return Err(NativeError::Invalid);
    }
    Ok(envelope)
}
pub fn validate_record(bytes: &[u8]) -> NativeResult<()> {
    parse_record(bytes).map(|_| ())
}
pub fn encode_record(name: &RecordName, data: serde_json::Value) -> NativeResult<Vec<u8>> {
    name.file_name()?;
    if !data.is_object() {
        return Err(NativeError::Invalid);
    }
    let mut writer = BoundedRecordWriter::new(MAX_RECORD_BYTES)?;
    if serde_json::to_writer(
        &mut writer,
        &Envelope {
            schema_version: 1,
            kind: name.kind(),
            operation: name.operation(),
            data,
        },
    )
    .is_err()
    {
        return Err(if writer.exceeded {
            NativeError::Oversize
        } else {
            NativeError::Invalid
        });
    }
    Ok(writer.bytes)
}
pub fn validate_for(name: &RecordName, bytes: &[u8]) -> NativeResult<()> {
    name.file_name()?;
    let record = parse_record(bytes)?;
    if record.kind != name.kind() || record.operation != name.operation() {
        return Err(NativeError::Invalid);
    }
    Ok(())
}

/// Validated body observation only; preserves the private envelope's existing parser/correlation.
pub(crate) fn record_data<T: serde::de::DeserializeOwned>(
    name: &RecordName,
    bytes: &[u8],
) -> NativeResult<T> {
    validate_for(name, bytes)?;
    // Deserialize through the SAME envelope with the typed body intact. Parsing first into
    // Value alone would erase duplicate body keys before the body's deny_unknown_fields check.
    let envelope: Envelope<T> = serde_json::from_slice(bytes).map_err(|_| NativeError::Invalid)?;
    Ok(envelope.data)
}

pub(crate) struct BoundedRecordWriter {
    bytes: Vec<u8>,
    cap: usize,
    exceeded: bool,
}
impl BoundedRecordWriter {
    pub(crate) fn new(cap: usize) -> NativeResult<Self> {
        check_read_size(0, cap)?;
        Ok(Self {
            bytes: Vec::with_capacity(cap),
            cap,
            exceeded: false,
        })
    }
    #[cfg(test)]
    #[allow(dead_code)] // Inspected by source-included bounded-writer regression.
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}
impl std::io::Write for BoundedRecordWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self
            .bytes
            .len()
            .checked_add(bytes.len())
            .is_none_or(|length| length > self.cap)
        {
            self.exceeded = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                "bounded record exceeded",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Contains observed bytes, not a receipt or path capability. Debug deliberately exposes no bytes.
pub struct ObservedRecord {
    pub identity: super::files::FileIdentity,
    bytes: Vec<u8>,
}
impl ObservedRecord {
    pub(crate) fn new(identity: super::files::FileIdentity, bytes: Vec<u8>) -> Self {
        Self { identity, bytes }
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}
impl std::fmt::Debug for ObservedRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ObservedRecord")
    }
}

#[derive(Debug)]
pub struct Publication {
    pub intent: PublicationIntent,
    pub state: PublicationRecovery,
    pub native_failure: Option<NativeError>,
}
/// The production sequence uses this same private seam for owned native handles and fake failures.
/// It cannot select a target directory; only the already-admitted store can supply one.
pub(crate) trait RecordStore {
    type Temporary;
    fn target(&self) -> RecordName {
        RecordName::Receipt
    }
    fn context(&self) -> Option<[u8; 16]> {
        None
    }
    fn read_final(&mut self) -> NativeResult<Option<Vec<u8>>>;
    fn create(&mut self) -> NativeResult<(super::files::PrivateName, Self::Temporary)>;
    fn write(&mut self, temporary: &mut Self::Temporary, bytes: &[u8]) -> NativeResult<()>;
    fn flush(&mut self, temporary: &Self::Temporary) -> NativeResult<()>;
    fn publish(&mut self, temporary: &Self::Temporary) -> NativeResult<()>;
    fn read_temporary(&mut self, name: &super::files::PrivateName)
    -> NativeResult<Option<Vec<u8>>>;
}
pub(crate) fn publish<S: RecordStore>(
    store: &mut S,
    bytes: &[u8],
    deadline: &super::Deadline,
) -> NativeResult<Publication> {
    deadline.check()?;
    let target = store.target();
    validate_for(&target, bytes)?;
    let old = store.read_final()?;
    if let Some(old) = &old {
        check_read_size(old.len(), MAX_RECORD_BYTES)?;
    }
    deadline.check()?;
    let (name, mut temporary) = store.create()?;
    let intent = PublicationIntent {
        old: old.as_deref().map(fingerprint),
        new: fingerprint(bytes),
        temporary: Some(name.clone()),
        target,
        context: store.context(),
    };
    let mut failure = (|| {
        deadline.check()?;
        store.write(&mut temporary, bytes)?;
        deadline.check()?;
        store.flush(&temporary)?;
        deadline.check()?;
        store.publish(&temporary)?;
        deadline.check()
    })()
    .err();
    // The submitted native call has finished before releasing this handle. In-flight timeouts
    // retain it in the worker; closing after completion allows read-only recovery inspection.
    drop(temporary);
    let state = if deadline.check().is_err() {
        PublicationRecovery::Unknown
    } else {
        match (store.read_final(), store.read_temporary(&name)) {
            (Ok(record), Ok(temporary)) => {
                recover(&intent, record.as_deref(), temporary.as_deref())
            }
            _ => PublicationRecovery::Unknown,
        }
    };
    if failure.is_none() && state != PublicationRecovery::NewPublished {
        failure = Some(NativeError::OutcomeUnknown);
    }
    Ok(Publication {
        intent,
        state,
        native_failure: failure,
    })
}

/// A6 fixed cold-publication protocol. These values correlate complete bytes only; the
/// actual original-context lock and native owner remain mandatory for every write/rename.
#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(test, allow(dead_code))]
pub(crate) enum RepairPublicationTarget {
    Repair,
    EvidenceIndex,
}
#[cfg(any(windows, test))]
#[cfg_attr(test, allow(dead_code))]
impl RepairPublicationTarget {
    pub(crate) fn name(self) -> RecordName {
        match self {
            Self::Repair => RecordName::Repair,
            Self::EvidenceIndex => RecordName::RepairEvidence,
        }
    }
}
#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, allow(dead_code))]
pub(crate) struct RepairPublicationStamp {
    volume: u64,
    file: [u8; 16],
    sha256: [u8; 32],
    length: u64,
}
#[cfg(any(windows, test))]
#[cfg_attr(test, allow(dead_code))]
impl RepairPublicationStamp {
    pub(crate) fn new(identity: super::files::FileIdentity, bytes: &[u8]) -> NativeResult<Self> {
        check_read_size(bytes.len(), MAX_RECORD_BYTES)?;
        let stamp = Self {
            volume: identity.volume,
            file: identity.file,
            sha256: fingerprint(bytes),
            length: bytes.len() as u64,
        };
        stamp.validate()?;
        Ok(stamp)
    }
    fn validate(&self) -> NativeResult<()> {
        if self.volume == 0
            || self.file == [0; 16]
            || self.sha256 == [0; 32]
            || self.length == 0
            || self.length > MAX_RECORD_BYTES as u64
        {
            return Err(NativeError::Invalid);
        }
        Ok(())
    }
    pub(crate) fn identity(&self) -> super::files::FileIdentity {
        super::files::FileIdentity {
            volume: self.volume,
            file: self.file,
        }
    }
}
#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[cfg_attr(test, allow(dead_code))]
pub(crate) enum RepairPublicationPhase {
    Preparing,
    PendingReady,
    ReplaceIntent,
    Published,
}
#[cfg(any(windows, test))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, allow(dead_code))]
pub(crate) struct RepairPublicationIntent {
    schema_version: u32,
    operation: [u8; 16],
    target: RepairPublicationTarget,
    old: Option<RepairPublicationStamp>,
    new_sha256: [u8; 32],
    new_length: u64,
    pending: Option<RepairPublicationStamp>,
    phase: RepairPublicationPhase,
}
#[cfg(any(windows, test))]
#[cfg_attr(test, allow(dead_code))]
impl RepairPublicationIntent {
    pub(crate) fn new(
        operation: [u8; 16],
        target: RepairPublicationTarget,
        old: Option<RepairPublicationStamp>,
        bytes: &[u8],
    ) -> NativeResult<Self> {
        validate_for(&target.name(), bytes)?;
        let value = Self {
            schema_version: 1,
            operation,
            target,
            old,
            new_sha256: fingerprint(bytes),
            new_length: bytes.len() as u64,
            pending: None,
            phase: RepairPublicationPhase::Preparing,
        };
        value.validate()?;
        Ok(value)
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
            (RepairPublicationPhase::Preparing, None) => {}
            (
                RepairPublicationPhase::PendingReady
                | RepairPublicationPhase::ReplaceIntent
                | RepairPublicationPhase::Published,
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
    pub(crate) fn target(&self) -> RepairPublicationTarget {
        self.target
    }
    pub(crate) fn phase(&self) -> RepairPublicationPhase {
        self.phase
    }
    pub(crate) fn pending(&self) -> Option<RepairPublicationStamp> {
        self.pending
    }
    pub(crate) fn matches_request(
        &self,
        operation: [u8; 16],
        target: RepairPublicationTarget,
        bytes: &[u8],
    ) -> bool {
        self.operation == operation
            && self.target == target
            && self.new_length == bytes.len() as u64
            && self.new_sha256 == fingerprint(bytes)
    }
    pub(crate) fn pending_ready(&mut self, stamp: RepairPublicationStamp) -> NativeResult<()> {
        if self.phase != RepairPublicationPhase::Preparing
            || self.pending.is_some()
            || stamp.length != self.new_length
            || stamp.sha256 != self.new_sha256
        {
            return Err(NativeError::Foreign);
        }
        stamp.validate()?;
        self.pending = Some(stamp);
        self.phase = RepairPublicationPhase::PendingReady;
        self.validate()
    }
    pub(crate) fn replace_intent(&mut self) -> NativeResult<()> {
        if self.phase != RepairPublicationPhase::PendingReady {
            return Err(NativeError::Foreign);
        }
        self.phase = RepairPublicationPhase::ReplaceIntent;
        self.validate()
    }
    pub(crate) fn published(&mut self) -> NativeResult<()> {
        if self.phase != RepairPublicationPhase::ReplaceIntent {
            return Err(NativeError::Foreign);
        }
        self.phase = RepairPublicationPhase::Published;
        self.validate()
    }
    pub(crate) fn encode(&self) -> NativeResult<Vec<u8>> {
        self.validate()?;
        encode_record(
            &RecordName::RepairPublicationIntent,
            serde_json::to_value(self).map_err(|_| NativeError::Invalid)?,
        )
    }
    pub(crate) fn decode(bytes: &[u8]) -> NativeResult<Self> {
        let value: Self = record_data(&RecordName::RepairPublicationIntent, bytes)?;
        value.validate()?;
        Ok(value)
    }
}
#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(test, allow(dead_code))]
pub(crate) enum RepairPublicationObservation {
    Preparing,
    PendingReady,
    Published,
    Unknown,
}
/// A pending name without its durably recorded exact flushed stamp is ALWAYS Unknown.
/// Destination-only exact identity can reconcile a rename; absence alone cannot.
#[cfg(any(windows, test))]
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn recover_repair_publication(
    intent: &RepairPublicationIntent,
    current: Option<RepairPublicationStamp>,
    pending: Option<RepairPublicationStamp>,
) -> RepairPublicationObservation {
    if intent.validate().is_err() {
        return RepairPublicationObservation::Unknown;
    }
    match intent.phase {
        RepairPublicationPhase::Preparing if current == intent.old && pending.is_none() => {
            RepairPublicationObservation::Preparing
        }
        RepairPublicationPhase::PendingReady | RepairPublicationPhase::ReplaceIntent
            if current == intent.old && pending == intent.pending =>
        {
            RepairPublicationObservation::PendingReady
        }
        RepairPublicationPhase::ReplaceIntent | RepairPublicationPhase::Published
            if current == intent.pending && pending.is_none() =>
        {
            RepairPublicationObservation::Published
        }
        _ => RepairPublicationObservation::Unknown,
    }
}
