#![allow(dead_code, unused_imports)] // Source-included seams used independently by the three binaries.

#[path = "../src/platform/windows/detect.rs"]
mod detect;
#[path = "../src/platform/windows/native_io.rs"]
mod native_io;
#[path = "../src/platform/windows/payload.rs"]
mod payload;
#[path = "../src/platform/windows/service.rs"]
mod service;
#[cfg(windows)]
#[path = "../src/platform/windows/transport.rs"]
mod transport;

use crosspane_installer::agent_contract;
use native_io::{NativeError, records::*};

#[test]
fn publication_recovery_recognizes_complete_old_or_new_only() {
    let intent = PublicationIntent::fixture(b"old", b"new");
    assert_eq!(
        recover(&intent, Some(b"old"), Some(b"new")),
        PublicationRecovery::OldRetained
    );
    assert_eq!(
        recover(&intent, Some(b"new"), None),
        PublicationRecovery::NewPublished
    );
    assert_eq!(
        recover(&intent, None, Some(b"new")),
        PublicationRecovery::RecoveryRequired
    );
    assert_eq!(
        recover(&intent, Some(b"other"), Some(b"new")),
        PublicationRecovery::Unknown
    );
    assert_eq!(
        recover(&intent, Some(b"ne"), None),
        PublicationRecovery::Unknown
    );
}

#[test]
fn incomplete_temp_never_becomes_a_complete_record() {
    let intent = PublicationIntent::fixture(b"old", b"new");
    assert_eq!(
        recover(&intent, Some(b"old"), Some(b"n")),
        PublicationRecovery::RecoveryRequired
    );
}

#[test]
fn record_bytes_are_bounded_versioned_and_never_path_authority() {
    assert_eq!(
        validate_record(br#"{"schema_version":1,"kind":"receipt","data":{}}"#),
        Ok(())
    );
    for bad in [
        br#"{"schema_version":2,"kind":"receipt","data":{}}"#.as_slice(),
        b"{",
        br#"{"schema_version":1,"kind":"receipt","path":"C:\\foreign","data":{}}"#,
    ] {
        assert_eq!(validate_record(bad), Err(NativeError::Invalid));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    None,
    Create,
    Write,
    Flush,
    BeforePublish,
    AfterPublish,
    Inspect,
}
struct Store {
    final_bytes: Option<Vec<u8>>,
    temporary: Option<Vec<u8>>,
    failure: Failure,
    steps: Vec<&'static str>,
}
impl RecordStore for Store {
    type Temporary = ();
    fn read_final(&mut self) -> Result<Option<Vec<u8>>, NativeError> {
        self.steps.push("observe-final");
        if self.failure == Failure::Inspect && self.steps.len() > 1 {
            Err(NativeError::Unavailable)
        } else {
            Ok(self.final_bytes.clone())
        }
    }
    fn create(&mut self) -> Result<(native_io::files::PrivateName, ()), NativeError> {
        self.steps.push("create-new");
        if self.failure == Failure::Create {
            Err(NativeError::Busy)
        } else {
            self.temporary = Some(Vec::new());
            Ok((
                native_io::files::PrivateName::new("record-temp-fixture.json")?,
                (),
            ))
        }
    }
    fn write(&mut self, _: &mut (), bytes: &[u8]) -> Result<(), NativeError> {
        self.steps.push("write-complete");
        if self.failure == Failure::Write {
            self.temporary = Some(bytes[..2].to_vec());
            Err(NativeError::Unavailable)
        } else {
            self.temporary = Some(bytes.to_vec());
            Ok(())
        }
    }
    fn flush(&mut self, _: &()) -> Result<(), NativeError> {
        self.steps.push("flush");
        if self.failure == Failure::Flush {
            Err(NativeError::Unavailable)
        } else {
            Ok(())
        }
    }
    fn publish(&mut self, _: &()) -> Result<(), NativeError> {
        self.steps.push("publish-once");
        if self.failure == Failure::BeforePublish {
            return Err(NativeError::Unavailable);
        }
        self.final_bytes = self.temporary.take();
        if matches!(self.failure, Failure::AfterPublish | Failure::Inspect) {
            Err(NativeError::Unavailable)
        } else {
            Ok(())
        }
    }
    fn read_temporary(
        &mut self,
        _: &native_io::files::PrivateName,
    ) -> Result<Option<Vec<u8>>, NativeError> {
        self.steps.push("observe-temporary");
        Ok(self.temporary.clone())
    }
}
#[allow(clippy::unwrap_used)] // Fixed fake-test budget; no native operation is dispatched.
fn budget() -> native_io::Deadline {
    native_io::Deadline::new(
        5000,
        std::sync::Arc::new(native_io::MonotonicClock::default()),
        native_io::Cancellation::default(),
    )
    .unwrap()
}
fn old_record() -> &'static [u8] {
    br#"{"schema_version":1,"kind":"receipt","data":{"generation":1}}"#
}
fn new_record() -> &'static [u8] {
    br#"{"schema_version":1,"kind":"receipt","data":{"generation":2}}"#
}
fn fixture_store(failure: Failure) -> Store {
    Store {
        final_bytes: Some(old_record().to_vec()),
        temporary: None,
        failure,
        steps: Vec::new(),
    }
}

#[test]
fn production_publication_sequence_is_create_write_flush_publish_not_truncate() {
    let mut store = fixture_store(Failure::None);
    let result = publish(&mut store, new_record(), &budget()).unwrap();
    assert_eq!(result.state, PublicationRecovery::NewPublished);
    assert_eq!(result.native_failure, None);
    assert_eq!(store.final_bytes.as_deref(), Some(new_record()));
    assert!(store.temporary.is_none());
    assert_eq!(
        &store.steps[..5],
        [
            "observe-final",
            "create-new",
            "write-complete",
            "flush",
            "publish-once"
        ]
    );
}

#[test]
fn failure_before_dispatch_leaves_original_and_does_not_retry() {
    let mut store = fixture_store(Failure::Create);
    assert!(matches!(
        publish(&mut store, new_record(), &budget()),
        Err(NativeError::Busy)
    ));
    assert_eq!(store.final_bytes.as_deref(), Some(old_record()));
    assert!(store.temporary.is_none());
    assert_eq!(store.steps, ["observe-final", "create-new"]);
}

#[test]
fn partial_write_and_flush_failure_retain_old_and_interrupted_name() {
    for failure in [Failure::Write, Failure::Flush, Failure::BeforePublish] {
        let mut store = fixture_store(failure);
        let result = publish(&mut store, new_record(), &budget()).unwrap();
        assert_eq!(store.final_bytes.as_deref(), Some(old_record()));
        assert!(store.temporary.is_some());
        assert_eq!(result.native_failure, Some(NativeError::Unavailable));
        assert_eq!(
            result.state,
            if failure == Failure::Write {
                PublicationRecovery::RecoveryRequired
            } else {
                PublicationRecovery::OldRetained
            }
        );
        assert!(
            store
                .steps
                .iter()
                .filter(|step| **step == "publish-once")
                .count()
                <= 1
        );
    }
}

#[test]
fn replacement_error_is_reinspected_and_never_blindly_retried() {
    let mut store = fixture_store(Failure::AfterPublish);
    let result = publish(&mut store, new_record(), &budget()).unwrap();
    assert_eq!(result.state, PublicationRecovery::NewPublished);
    assert_eq!(result.native_failure, Some(NativeError::Unavailable));
    assert_eq!(store.final_bytes.as_deref(), Some(new_record()));
    assert_eq!(
        store
            .steps
            .iter()
            .filter(|step| **step == "publish-once")
            .count(),
        1
    );
    let result = publish(
        &mut fixture_store(Failure::Inspect),
        new_record(),
        &budget(),
    )
    .unwrap();
    assert_eq!(result.state, PublicationRecovery::Unknown);
}

#[test]
fn record_name_and_operation_are_correlated_before_any_native_write() {
    let bytes =
        encode_record(&RecordName::StageCatalog, serde_json::json!({"entries":[]})).unwrap();
    assert_eq!(
        validate_for(&RecordName::Receipt, &bytes),
        Err(NativeError::Invalid)
    );
    let operation = RecordName::Operation([1; 16]);
    let bytes = encode_record(&operation, serde_json::json!({"phase":"planned"})).unwrap();
    assert_eq!(
        validate_for(&RecordName::Operation([2; 16]), &bytes),
        Err(NativeError::Invalid)
    );
    assert_eq!(validate_for(&operation, &bytes), Ok(()));
    assert_eq!(
        RecordName::Operation([0; 16]).file_name().err(),
        Some(NativeError::Invalid)
    );
    let mut store = fixture_store(Failure::None);
    assert!(matches!(
        publish(&mut store, &bytes, &budget()),
        Err(NativeError::Invalid)
    ));
    assert!(store.steps.is_empty());
}

#[test]
fn record_encoding_caps_the_writer_before_allocating_or_appending_excess() {
    use std::io::Write;
    let mut writer = BoundedRecordWriter::new(8).unwrap();
    writer.write_all(b"12345678").unwrap();
    assert!(writer.write_all(b"9").is_err());
    assert_eq!(writer.bytes(), b"12345678");
    let value = serde_json::json!({"fixture": "x".repeat(native_io::files::MAX_RECORD_BYTES)});
    assert_eq!(
        encode_record(&RecordName::Receipt, value).err(),
        Some(NativeError::Oversize)
    );
}

/// The rule scope of the elevated install under test, the same fixture as the core journal tests.
#[allow(clippy::unwrap_used)] // Fixed fixture values; the parsers accept them by construction.
fn elevated_scope() -> crosspane_installer_core::elevated::RuleScope {
    use crosspane_installer_core::elevated::{AgentProgram, InstallId, RuleScope};
    RuleScope {
        id: InstallId::parse("w41c-vm-1").unwrap(),
        program: AgentProgram::parse(
            r"C:\Users\user\AppData\Local\Programs\Crosspane\crosspane-agent.exe",
        )
        .unwrap(),
    }
}

/// A real core journal with one Intent: the body the elevated store publishes.
#[allow(clippy::unwrap_used)] // Fixed fixture: an empty record accepts one journaled Intent.
fn elevated_record() -> crosspane_installer_core::elevated::journal::ElevatedRecord {
    use crosspane_installer_core::elevated::Verb;
    use crosspane_installer_core::elevated::journal::ElevatedRecord;
    use crosspane_installer_core::elevated::status::intent_entry;
    let scope = elevated_scope();
    let mut record = ElevatedRecord::new(&scope);
    record
        .append(&intent_entry(&Verb::AddFirewall(scope), None))
        .unwrap();
    record
}

#[test]
fn elevated_setup_name_is_the_journal_leaf_and_writes_the_elevated_kind() {
    use crosspane_installer_core::elevated::journal::RECORD_LEAF;
    let name = RecordName::ElevatedSetup;
    assert_eq!(name.file_name().unwrap().as_str(), "elevated-setup.json");
    assert_eq!(name.file_name().unwrap().as_str(), RECORD_LEAF);
    // `RecordName::kind` is private to `records`, so the kind is read from the envelope it sets.
    let bytes = encode_record(&name, serde_json::json!({})).unwrap();
    let envelope: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(envelope["kind"], serde_json::json!("elevated-setup"));
    assert_eq!(validate_for(&name, &bytes), Ok(()));
}

#[test]
fn elevated_setup_kind_is_the_journal_kind_and_round_trips() {
    use crosspane_installer_core::elevated::journal::RECORD_KIND;
    let kind = RecordKind::ElevatedSetup;
    let wire = serde_json::to_value(kind).unwrap();
    assert_eq!(wire, serde_json::json!("elevated-setup"));
    assert_eq!(wire, serde_json::json!(RECORD_KIND));
    assert_eq!(serde_json::from_value::<RecordKind>(wire).unwrap(), kind);
    // The kebab-case name is exact; near misses are not this kind.
    for near in ["elevated_setup", "ElevatedSetup"] {
        assert!(serde_json::from_value::<RecordKind>(serde_json::json!(near)).is_err());
    }
}

#[test]
fn elevated_setup_record_round_trips_through_encode_and_decode() {
    use crosspane_installer_core::elevated::journal::ElevatedRecord;
    let record = elevated_record();
    let value = serde_json::to_value(&record).unwrap();
    let bytes = encode_record(&RecordName::ElevatedSetup, value).unwrap();
    let envelope: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(envelope["schema_version"], serde_json::json!(1));
    assert_eq!(envelope["kind"], serde_json::json!("elevated-setup"));
    assert!(envelope.get("operation").is_none());
    assert_eq!(validate_for(&RecordName::ElevatedSetup, &bytes), Ok(()));
    let back: ElevatedRecord = record_data(&RecordName::ElevatedSetup, &bytes).unwrap();
    assert_eq!(back, record);
    assert_eq!(back.validate(), Ok(()));
}

#[test]
fn elevated_setup_kind_must_match_its_name_in_both_directions() {
    use crosspane_installer_core::elevated::journal::ElevatedRecord;
    let value = serde_json::to_value(elevated_record()).unwrap();
    let elevated = encode_record(&RecordName::ElevatedSetup, value.clone()).unwrap();
    let receipt = encode_record(&RecordName::Receipt, value).unwrap();
    assert_eq!(
        validate_for(&RecordName::Receipt, &elevated),
        Err(NativeError::Invalid)
    );
    assert_eq!(
        validate_for(&RecordName::ElevatedSetup, &receipt),
        Err(NativeError::Invalid)
    );
    assert_eq!(
        record_data::<ElevatedRecord>(&RecordName::Receipt, &elevated),
        Err(NativeError::Invalid)
    );
    assert_eq!(
        record_data::<ElevatedRecord>(&RecordName::ElevatedSetup, &receipt),
        Err(NativeError::Invalid)
    );
    // An envelope whose kind is a near miss of "elevated-setup" is not this record either.
    let near = br#"{"schema_version":1,"kind":"elevated_setup","data":{}}"#;
    assert_eq!(validate_record(near), Err(NativeError::Invalid));
    assert_eq!(
        validate_for(&RecordName::ElevatedSetup, near),
        Err(NativeError::Invalid)
    );
}
