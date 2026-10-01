#![allow(clippy::unwrap_used)]

use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use crosspane_input::Held;
use crosspane_input::journal::{FileJournal, Journal, MemoryJournal};
use crosspane_types::hid::{HidUsage, MouseButton};
use proptest::prelude::*;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
const KEY: Held = Held::Key(HidUsage {
    page: 0x1234,
    id: 0x5678,
});
const BUTTON: Held = Held::Button(MouseButton(u8::MAX));

struct TempJournal {
    directory: PathBuf,
    path: PathBuf,
}

impl TempJournal {
    fn new() -> Self {
        loop {
            let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir().join(format!(
                "crosspane-journal-test-{}-{serial}",
                std::process::id()
            ));
            match fs::create_dir(&directory) {
                Ok(()) => {
                    return Self {
                        path: directory.join("journal"),
                        directory,
                    };
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("cannot create test directory: {error}"),
            }
        }
    }

    fn bytes(&self) -> u64 {
        fs::metadata(&self.path).unwrap().len()
    }
}

impl Drop for TempJournal {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn file_round_trip_across_reopen() {
    let temp = TempJournal::new();
    let mut journal = FileJournal::open(&temp.path).unwrap();
    assert!(journal.held().unwrap().is_empty());
    journal.record_down(BUTTON).unwrap();
    journal.record_down(KEY).unwrap();
    journal.record_down(KEY).unwrap();
    journal
        .record_up(Held::Button(MouseButton::PRIMARY))
        .unwrap();
    assert_eq!(journal.held().unwrap(), [KEY, BUTTON]);
    assert_eq!(temp.bytes(), 32);
    let records = fs::read(&temp.path).unwrap();
    assert_eq!(&records[..8], &[0xD1, 1, 0, 0, 0xFF, 0, 0xF1, 0xC0]);
    assert_eq!(
        &records[8..16],
        &[0xD1, 0, 0x34, 0x12, 0x78, 0x56, 0x43, 0x84]
    );
    assert_eq!(&records[24..], &[0x0F, 1, 0, 0, 1, 0, 0xD1, 0xC0]);
    drop(journal);
    let mut journal = FileJournal::open(&temp.path).unwrap();
    assert_eq!(journal.held().unwrap(), [KEY, BUTTON]);
    journal.record_up(KEY).unwrap();
    assert_eq!(temp.bytes(), 40);
    drop(journal);
    assert_eq!(
        FileJournal::open(&temp.path).unwrap().held().unwrap(),
        [BUTTON]
    );
}

#[test]
fn torn_final_record_is_ignored_and_repaired() {
    for partial_bytes in 1..8 {
        let temp = TempJournal::new();
        let mut journal = FileJournal::open(&temp.path).unwrap();
        journal.record_down(KEY).unwrap();
        journal.record_down(BUTTON).unwrap();
        journal.record_up(KEY).unwrap();
        drop(journal);
        OpenOptions::new()
            .write(true)
            .open(&temp.path)
            .unwrap()
            .set_len(16 + partial_bytes)
            .unwrap();
        let mut journal = FileJournal::open(&temp.path).unwrap();
        assert_eq!(journal.held().unwrap(), [KEY, BUTTON]);
        assert_eq!(temp.bytes(), 16);
        journal.record_up(KEY).unwrap();
        drop(journal);
        assert_eq!(
            FileJournal::open(&temp.path).unwrap().held().unwrap(),
            [BUTTON]
        );
        assert_eq!(temp.bytes(), 24);
    }
}

#[test]
fn corrupted_check_is_ignored() {
    for corrupt_record in [1, 2] {
        let temp = TempJournal::new();
        let mut journal = FileJournal::open(&temp.path).unwrap();
        journal.record_down(KEY).unwrap();
        journal.record_down(BUTTON).unwrap();
        journal.record_up(KEY).unwrap();
        drop(journal);
        let mut file = OpenOptions::new().write(true).open(&temp.path).unwrap();
        file.seek(SeekFrom::Start(corrupt_record * 8 + 6)).unwrap();
        file.write_all(&[0, 0]).unwrap();
        drop(file);
        let mut journal = FileJournal::open(&temp.path).unwrap();
        let expected = if corrupt_record == 1 {
            vec![KEY]
        } else {
            vec![KEY, BUTTON]
        };
        assert_eq!(journal.held().unwrap(), expected);
        assert_eq!(temp.bytes(), corrupt_record * 8);
        journal.record_up(KEY).unwrap();
        drop(journal);
        let expected = if corrupt_record == 1 {
            vec![]
        } else {
            vec![BUTTON]
        };
        assert_eq!(
            FileJournal::open(&temp.path).unwrap().held().unwrap(),
            expected
        );
    }
}

#[test]
fn compaction_to_zero_when_empty() {
    let temp = TempJournal::new();
    let mut journal = FileJournal::open(&temp.path).unwrap();
    for _ in 0..256 {
        journal.record_down(KEY).unwrap();
        journal.record_up(KEY).unwrap();
    }
    assert_eq!(temp.bytes(), 4096);
    assert!(journal.held().unwrap().is_empty());
    // An up for an already-up item makes the empty journal exceed the threshold.
    journal.record_up(KEY).unwrap();
    assert_eq!(temp.bytes(), 0);
    journal.record_down(BUTTON).unwrap();
    assert_eq!(temp.bytes(), 8);
    drop(journal);
    assert_eq!(
        FileJournal::open(&temp.path).unwrap().held().unwrap(),
        [BUTTON]
    );
}

#[test]
fn compaction_rewrites_held_items() {
    let temp = TempJournal::new();
    let mut journal = FileJournal::open(&temp.path).unwrap();
    journal.record_down(BUTTON).unwrap();
    journal.record_down(KEY).unwrap();
    for _ in 0..510 {
        journal.record_down(KEY).unwrap();
    }
    assert_eq!(temp.bytes(), 4096);
    journal.record_down(KEY).unwrap();
    assert_eq!(temp.bytes(), 16);
    assert_eq!(journal.held().unwrap(), [KEY, BUTTON]);
    let compacted = fs::read(&temp.path).unwrap();
    assert_eq!(compacted[0], 0xD1);
    assert_eq!(compacted[1], 0);
    assert_eq!(compacted[8], 0xD1);
    assert_eq!(compacted[9], 1);
    assert_eq!(fs::read_dir(&temp.directory).unwrap().count(), 1);
    // Further writes must reach the renamed file, rather than the old file descriptor.
    journal.record_up(KEY).unwrap();
    assert_eq!(temp.bytes(), 24);
    drop(journal);
    assert_eq!(
        FileJournal::open(&temp.path).unwrap().held().unwrap(),
        [BUTTON]
    );

    // Oversized journals left by a process that stopped before compaction are compacted on open.
    let mut file = OpenOptions::new().append(true).open(&temp.path).unwrap();
    for _ in 0..512 {
        file.write_all(&compacted[8..16]).unwrap();
    }
    drop(file);
    assert_eq!(temp.bytes(), 4120);
    assert_eq!(
        FileJournal::open(&temp.path).unwrap().held().unwrap(),
        [BUTTON]
    );
    assert_eq!(temp.bytes(), 8);
}

fn held_item() -> impl Strategy<Value = Held> {
    prop_oneof![
        (any::<u16>(), any::<u16>()).prop_map(|(page, id)| Held::Key(HidUsage { page, id })),
        any::<u8>().prop_map(|button| Held::Button(MouseButton(button))),
        Just(KEY),
        Just(BUTTON),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn memory_matches_file_for_random_operations(
        items in prop::collection::vec(held_item(), 1..16),
        operations in prop::collection::vec((any::<u8>(), any::<bool>(), 0_u8..16), 1..700),
    ) {
        let temp = TempJournal::new();
        let mut memory = MemoryJournal::default();
        let mut file = FileJournal::open(&temp.path).unwrap();
        for (index, down, reopen) in operations {
            let item = items[usize::from(index) % items.len()];
            if down {
                memory.record_down(item).unwrap();
                file.record_down(item).unwrap();
            } else {
                memory.record_up(item).unwrap();
                file.record_up(item).unwrap();
            }
            prop_assert_eq!(memory.held().unwrap(), file.held().unwrap());
            prop_assert_eq!(temp.bytes() % 8, 0);
            if reopen == 0 {
                drop(file);
                file = FileJournal::open(&temp.path).unwrap();
                prop_assert_eq!(memory.held().unwrap(), file.held().unwrap());
            }
        }
        drop(file);
        prop_assert_eq!(memory.held().unwrap(), FileJournal::open(&temp.path).unwrap().held().unwrap());
    }
}
