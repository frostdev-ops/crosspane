#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use crosspane_platform_windows::model::journal::*;

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

const SMALL: (u32, u32) = (1280, 720);
const SMALL_MM: (u32, u32) = (339, 191);
const BIG: (u32, u32) = (1920, 1080);
const BIG_MM: (u32, u32) = (508, 286);

/// One entry in the pre-ledger shape (the bytes `model_journal.rs` already pins).
const LEGACY_ENTRY: &str = r#"{"window":1,"pid":10,"process_start":20,"phase":"Journaled","original":{"rect_physical":[100,200,700,600],"monitor_path":"DISPLAY1","show":"Normal","dpi":96},"twin":{"serial":1,"device_path":null,"mode":[1920,1080],"refresh_millihz":60000,"dpi":96,"origin":[1920,0]}}"#;
const UP_JSON: &str =
    r#"{"key":1,"phase":"Up","mode":[1280,720],"size_mm":[339,191],"monitor_id":7}"#;
const ADDING_JSON: &str =
    r#"{"key":2,"phase":"Adding","mode":[1920,1080],"size_mm":[508,286],"monitor_id":null}"#;

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        for _ in 0..32 {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "crosspane-w31b-twin-journal-{}-{stamp}-{}",
                std::process::id(),
                NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("exclusive scratch creation failed: {error}"),
            }
        }
        panic!("exclusive scratch namespace exhausted");
    }

    fn journal_path(&self) -> PathBuf {
        self.0.join(JOURNAL_NAME)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn record(
    key: u32,
    phase: TwinPhase,
    mode: (u32, u32),
    size_mm: (u32, u32),
    monitor_id: Option<u32>,
) -> TwinRecord {
    TwinRecord {
        key,
        phase,
        mode,
        size_mm,
        monitor_id,
    }
}

fn adding(key: u32, mode: (u32, u32), size_mm: (u32, u32)) -> TwinRecord {
    record(key, TwinPhase::Adding, mode, size_mm, None)
}

fn up(key: u32, mode: (u32, u32), size_mm: (u32, u32), id: u32) -> TwinRecord {
    record(key, TwinPhase::Up, mode, size_mm, Some(id))
}

fn removing(key: u32, mode: (u32, u32), size_mm: (u32, u32), id: u32) -> TwinRecord {
    record(key, TwinPhase::Removing, mode, size_mm, Some(id))
}

/// Builds a raw document. An empty `twins` omits the key, as a pre-ledger document does.
fn document(entries: &str, twins: &str) -> Vec<u8> {
    let twins = if twins.is_empty() {
        String::new()
    } else {
        format!(r#","twins":[{twins}]"#)
    };
    format!(r#"{{"format":"{FORMAT}","entries":[{entries}]{twins}}}"#).into_bytes()
}

/// Builds one raw twin record. Arguments are JSON text, so invalid values can be expressed.
fn twin_json(key: &str, phase: &str, mode: &str, mm: &str, id: &str) -> String {
    format!(r#"{{"key":{key},"phase":"{phase}","mode":{mode},"size_mm":{mm},"monitor_id":{id}}}"#)
}

fn opens(root: &Scratch, bytes: &[u8]) -> bool {
    fs::write(root.journal_path(), bytes).unwrap();
    JournalFile::open(&root.journal_path()).is_ok()
}

#[test]
fn round_trip_keeps_twin_records_in_key_order() {
    let root = Scratch::new();
    let path = root.journal_path();
    let mut journal = JournalFile::open(&path).unwrap();
    // Key 2 is still adding while key 1 is up. Saved records are sorted by key.
    journal.put_twin(adding(2, BIG, BIG_MM)).unwrap();
    journal.put_twin(adding(1, SMALL, SMALL_MM)).unwrap();
    journal.put_twin(up(1, SMALL, SMALL_MM, 7)).unwrap();
    journal.save().unwrap();
    assert_eq!(
        fs::read(&path).unwrap(),
        document("", &format!("{UP_JSON},{ADDING_JSON}"))
    );

    let reopened = JournalFile::open(&path).unwrap();
    let expected_up = up(1, SMALL, SMALL_MM, 7);
    let expected_adding = adding(2, BIG, BIG_MM);
    assert_eq!(
        reopened.twins().cloned().collect::<Vec<_>>(),
        vec![expected_up.clone(), expected_adding.clone()]
    );
    assert_eq!(reopened.twin(1), Some(&expected_up));
    assert_eq!(reopened.twin(3), None);

    // Entries and twins coexist; a loaded document saves back to the same bytes.
    let mixed = document(LEGACY_ENTRY, &format!("{UP_JSON},{ADDING_JSON}"));
    fs::write(&path, &mixed).unwrap();
    let mut journal = JournalFile::open(&path).unwrap();
    journal.save().unwrap();
    assert_eq!(fs::read(&path).unwrap(), mixed);

    assert_eq!(journal.forget_twin(2), Some(expected_adding));
    assert_eq!(journal.forget_twin(2), None);
    journal.save().unwrap();
    let reopened = JournalFile::open(&path).unwrap();
    assert_eq!(
        reopened.twins().cloned().collect::<Vec<_>>(),
        vec![expected_up]
    );
    assert_eq!(reopened.entries().count(), 1);
}

#[test]
fn document_without_twins_loads_and_saves_byte_identical() {
    let root = Scratch::new();
    let path = root.journal_path();
    // The exact no-twin shape, with no `twins` key at all.
    assert_eq!(
        document("", ""),
        br#"{"format":"crosspane-win-twin-v1","entries":[]}"#.to_vec()
    );
    for original in [document("", ""), document(LEGACY_ENTRY, "")] {
        fs::write(&path, &original).unwrap();
        let journal = JournalFile::open(&path).unwrap();
        assert_eq!(journal.twins().count(), 0);
        journal.save().unwrap();
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    // A journal created fresh and saved with no twins writes the same shape.
    let fresh = root.0.join("fresh.journal");
    JournalFile::open(&fresh).unwrap().save().unwrap();
    assert_eq!(fs::read(&fresh).unwrap(), document("", ""));
}

#[test]
fn transition_table_allows_only_the_documented_moves() {
    let cases: Vec<(&str, Option<TwinRecord>, TwinRecord, bool)> = vec![
        ("none -> Adding", None, adding(1, SMALL, SMALL_MM), true),
        ("none -> Up", None, up(1, SMALL, SMALL_MM, 7), false),
        (
            "none -> Removing",
            None,
            removing(1, SMALL, SMALL_MM, 7),
            false,
        ),
        (
            "Adding -> Up, same geometry",
            Some(adding(1, SMALL, SMALL_MM)),
            up(1, SMALL, SMALL_MM, 7),
            true,
        ),
        (
            "Adding -> Up, other mode",
            Some(adding(1, SMALL, SMALL_MM)),
            up(1, BIG, SMALL_MM, 7),
            false,
        ),
        (
            "Adding -> Up, other millimetres",
            Some(adding(1, SMALL, SMALL_MM)),
            up(1, SMALL, BIG_MM, 7),
            false,
        ),
        (
            "Adding -> Adding, identical",
            Some(adding(1, SMALL, SMALL_MM)),
            adding(1, SMALL, SMALL_MM),
            true,
        ),
        (
            "Adding -> Adding, other mode",
            Some(adding(1, SMALL, SMALL_MM)),
            adding(1, BIG, BIG_MM),
            false,
        ),
        (
            "Adding -> Removing",
            Some(adding(1, SMALL, SMALL_MM)),
            removing(1, SMALL, SMALL_MM, 7),
            false,
        ),
        (
            "Up -> Removing, same geometry and id",
            Some(up(1, SMALL, SMALL_MM, 7)),
            removing(1, SMALL, SMALL_MM, 7),
            true,
        ),
        (
            "Up -> Removing, other id",
            Some(up(1, SMALL, SMALL_MM, 7)),
            removing(1, SMALL, SMALL_MM, 8),
            false,
        ),
        (
            "Up -> Removing, other mode",
            Some(up(1, SMALL, SMALL_MM, 7)),
            removing(1, BIG, SMALL_MM, 7),
            false,
        ),
        (
            "Up -> Removing, other millimetres",
            Some(up(1, SMALL, SMALL_MM, 7)),
            removing(1, SMALL, BIG_MM, 7),
            false,
        ),
        (
            "Up -> Up, other id",
            Some(up(1, SMALL, SMALL_MM, 7)),
            up(1, SMALL, SMALL_MM, 8),
            false,
        ),
        (
            "Up -> Adding",
            Some(up(1, SMALL, SMALL_MM, 7)),
            adding(1, SMALL, SMALL_MM),
            false,
        ),
        (
            "Up -> Up, identical",
            Some(up(1, SMALL, SMALL_MM, 7)),
            up(1, SMALL, SMALL_MM, 7),
            true,
        ),
        (
            "Removing -> Adding, other mode (resize)",
            Some(removing(1, SMALL, SMALL_MM, 7)),
            adding(1, BIG, BIG_MM),
            true,
        ),
        (
            "Removing -> Adding, same mode",
            Some(removing(1, SMALL, SMALL_MM, 7)),
            adding(1, SMALL, SMALL_MM),
            true,
        ),
        (
            "Removing -> Up",
            Some(removing(1, SMALL, SMALL_MM, 7)),
            up(1, SMALL, SMALL_MM, 8),
            false,
        ),
        (
            "Removing -> Removing, other id",
            Some(removing(1, SMALL, SMALL_MM, 7)),
            removing(1, SMALL, SMALL_MM, 8),
            false,
        ),
        (
            "Removing -> Removing, identical",
            Some(removing(1, SMALL, SMALL_MM, 7)),
            removing(1, SMALL, SMALL_MM, 7),
            true,
        ),
        (
            "key mismatch",
            Some(up(1, SMALL, SMALL_MM, 7)),
            removing(2, SMALL, SMALL_MM, 7),
            false,
        ),
        (
            "invalid new record: Adding with an id",
            None,
            record(1, TwinPhase::Adding, SMALL, SMALL_MM, Some(7)),
            false,
        ),
        (
            "invalid new record: unknown mode",
            None,
            adding(1, (800, 600), SMALL_MM),
            false,
        ),
    ];
    for (name, old, new, allowed) in cases {
        assert_eq!(
            twin_transition_allowed(old.as_ref(), &new),
            allowed,
            "{name}"
        );
    }

    // put_twin applies the same rule. A refused put leaves the stored record unchanged.
    let root = Scratch::new();
    let mut journal = JournalFile::open(&root.journal_path()).unwrap();
    journal.put_twin(adding(1, SMALL, SMALL_MM)).unwrap();
    assert!(journal.put_twin(removing(1, SMALL, SMALL_MM, 7)).is_err());
    assert_eq!(journal.twin(1), Some(&adding(1, SMALL, SMALL_MM)));
    journal.put_twin(up(1, SMALL, SMALL_MM, 7)).unwrap();
    assert!(journal.put_twin(up(1, SMALL, SMALL_MM, 8)).is_err());
    assert_eq!(journal.twin(1), Some(&up(1, SMALL, SMALL_MM, 7)));
}

#[test]
fn invalid_records_are_refused_at_open() {
    let root = Scratch::new();
    // Positive controls: a valid record opens, and so do millimetres at both bounds.
    assert!(opens(
        &root,
        &document(
            "",
            &twin_json("1", "Adding", "[1280,720]", "[339,191]", "null")
        )
    ));
    assert!(opens(
        &root,
        &document(
            "",
            &twin_json("1", "Adding", "[1920,1080]", "[10,2000]", "null")
        )
    ));

    let unknown_field = r#"{"key":1,"phase":"Adding","mode":[1280,720],"size_mm":[339,191],"monitor_id":null,"extra":1}"#;
    let refused: Vec<(&str, String)> = vec![
        (
            "zero key",
            twin_json("0", "Adding", "[1280,720]", "[339,191]", "null"),
        ),
        (
            "bad mode",
            twin_json("1", "Adding", "[800,600]", "[339,191]", "null"),
        ),
        (
            "mm 9 width",
            twin_json("1", "Adding", "[1280,720]", "[9,191]", "null"),
        ),
        (
            "mm 9 height",
            twin_json("1", "Adding", "[1280,720]", "[339,9]", "null"),
        ),
        (
            "mm 2001 width",
            twin_json("1", "Adding", "[1280,720]", "[2001,191]", "null"),
        ),
        (
            "mm 2001 height",
            twin_json("1", "Adding", "[1280,720]", "[339,2001]", "null"),
        ),
        (
            "Adding with an id",
            twin_json("1", "Adding", "[1280,720]", "[339,191]", "7"),
        ),
        (
            "Up without an id",
            twin_json("1", "Up", "[1280,720]", "[339,191]", "null"),
        ),
        (
            "Removing without an id",
            twin_json("1", "Removing", "[1280,720]", "[339,191]", "null"),
        ),
        (
            "Up with id zero",
            twin_json("1", "Up", "[1280,720]", "[339,191]", "0"),
        ),
        ("unknown field", unknown_field.to_owned()),
        (
            "duplicate key",
            format!(
                "{},{}",
                twin_json("1", "Adding", "[1280,720]", "[339,191]", "null"),
                twin_json("1", "Adding", "[1920,1080]", "[508,286]", "null")
            ),
        ),
        (
            "nine records",
            (1..=9)
                .map(|key| {
                    twin_json(
                        &key.to_string(),
                        "Adding",
                        "[1280,720]",
                        "[339,191]",
                        "null",
                    )
                })
                .collect::<Vec<_>>()
                .join(","),
        ),
    ];
    for (name, twins) in refused {
        assert!(
            !opens(&root, &document("", &twins)),
            "{name} must be refused"
        );
    }

    // Eight records is the limit and still opens.
    let eight = (1..=8)
        .map(|key| {
            twin_json(
                &key.to_string(),
                "Adding",
                "[1280,720]",
                "[339,191]",
                "null",
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    assert!(opens(&root, &document("", &eight)));
    let mut journal = JournalFile::open(&root.journal_path()).unwrap();
    assert_eq!(journal.twins().count(), 8);
    // put_twin enforces the same ceiling for a new key.
    assert!(journal.put_twin(adding(9, SMALL, SMALL_MM)).is_err());
    assert_eq!(journal.twins().count(), 8);
}

#[test]
fn truncation_at_every_byte_never_loads_partially() {
    let root = Scratch::new();
    let path = root.journal_path();
    let full = document(LEGACY_ENTRY, &format!("{UP_JSON},{ADDING_JSON}"));
    fs::write(&path, &full).unwrap();
    assert_eq!(JournalFile::open(&path).unwrap().twins().count(), 2);
    for len in 0..full.len() {
        fs::write(&path, &full[..len]).unwrap();
        assert!(
            JournalFile::open(&path).is_err(),
            "a {len}-byte prefix must not load"
        );
    }
}

#[test]
fn clear_twins_then_save_restores_the_no_twin_document() {
    let root = Scratch::new();
    let path = root.journal_path();
    fs::write(
        &path,
        document(LEGACY_ENTRY, &format!("{UP_JSON},{ADDING_JSON}")),
    )
    .unwrap();
    let mut journal = JournalFile::open(&path).unwrap();
    assert_eq!(journal.twins().count(), 2);
    journal.clear_twins();
    assert_eq!(journal.twins().count(), 0);
    assert_eq!(journal.twin(1), None);
    journal.save().unwrap();
    assert_eq!(fs::read(&path).unwrap(), document(LEGACY_ENTRY, ""));

    let reopened = JournalFile::open(&path).unwrap();
    assert_eq!(reopened.twins().count(), 0);
    assert_eq!(reopened.entries().count(), 1);
}
