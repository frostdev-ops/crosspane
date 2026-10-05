use super::{NativeError, NativeResult, files::MAX_RECORD_BYTES, files::check_read_size};
use aws_lc_rs::digest::{SHA256, digest};
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
    Receipt,
    StageCatalog,
    Operation,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordName {
    Receipt,
    StageCatalog,
    Operation([u8; 16]),
}
impl RecordName {
    pub fn file_name(&self) -> NativeResult<super::files::PrivateName> {
        match self {
            Self::Receipt => super::files::PrivateName::new("receipt.json"),
            Self::StageCatalog => super::files::PrivateName::new("stage-catalog.json"),
            Self::Operation(id) if *id != [0; 16] => {
                super::files::PrivateName::new(&format!("operation-{}.json", hex(id)))
            }
            _ => Err(NativeError::Invalid),
        }
    }
    fn kind(&self) -> RecordKind {
        match self {
            Self::Receipt => RecordKind::Receipt,
            Self::StageCatalog => RecordKind::StageCatalog,
            Self::Operation(_) => RecordKind::Operation,
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
struct Envelope {
    schema_version: u32,
    kind: RecordKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    operation: Option<[u8; 16]>,
    data: serde_json::Value,
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
