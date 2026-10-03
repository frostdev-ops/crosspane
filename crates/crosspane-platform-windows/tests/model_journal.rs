use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crosspane_platform::ParkingKind;
use crosspane_platform_windows::model::journal::*;
use crosspane_types::geom::{SizeMm, euclid::point2};
use crosspane_types::id::{DisplayId, WindowId};
use proptest::prelude::*;

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

struct Scratch(PathBuf);

impl Scratch {
    #[allow(clippy::unwrap_used)] // The owned fixture clock must be available.
    fn new() -> Self {
        for _ in 0..32 {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "crosspane-w02d-{}-{stamp}-{}",
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

    fn file(&self, leaf: &str) -> PathBuf {
        assert_eq!(Path::new(leaf).components().count(), 1);
        assert!(matches!(
            Path::new(leaf).components().next(),
            Some(std::path::Component::Normal(_))
        ));
        self.0.join(leaf)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn entry(window: u64, serial: u32, phase: Phase) -> Entry {
    Entry {
        window,
        pid: 10,
        process_start: 20,
        phase,
        original: Original {
            rect_physical: [100, 200, 700, 600],
            monitor_path: r"\\?\DISPLAY#INJECTED".into(),
            show: Show::Maximized,
            dpi: 144,
        },
        twin: Twin {
            serial,
            device_path: (phase != Phase::Journaled).then(|| format!(r"\\?\DISPLAY#TWIN-{serial}")),
            mode: (1920, 1080),
            refresh_millihz: 60000,
            dpi: 144,
            origin: (1920, 0),
        },
    }
}

#[allow(clippy::unwrap_used)] // Invalid injected serialization must fail the test.
fn bytes(entries: &[Entry]) -> Vec<u8> {
    let entries = serde_json::to_string(entries).unwrap();
    format!(r#"{{"format":"{FORMAT}","entries":{entries}}}"#).into_bytes()
}

#[test]
fn format_path_and_windows_constants_match_documented_model() {
    assert_eq!(FORMAT, "crosspane-win-twin-v1");
    assert_eq!(JOURNAL_NAME, "parking-twin.journal");
    assert_eq!(
        (SHORT_MIN, SHORT_MAX, BASE_DPI, EDID_BYTES),
        (-32768, 32767, 96.0, 128)
    );
}

#[test]
fn missing_file_is_empty_but_existing_empty_file_is_invalid() {
    let root = Scratch::new();
    let path = root.file(JOURNAL_NAME);
    let journal = JournalFile::open(&path).unwrap();
    assert_eq!(journal.entries().count(), 0);
    assert!(!path.exists());
    fs::write(&path, []).unwrap();
    assert!(JournalFile::open(&path).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"");
}

#[test]
fn json_round_trip_preserves_all_fields_and_sorts_entries() {
    let root = Scratch::new();
    let path = root.file(JOURNAL_NAME);
    let entries = [entry(2, 2, Phase::Moved), entry(1, 1, Phase::Journaled)];
    let mut journal = JournalFile::open(&path).unwrap();
    for e in &entries {
        journal.insert(e.clone()).unwrap();
    }
    assert!(!path.exists(), "in-memory edits are staged");
    journal.save().unwrap();
    let reopened = JournalFile::open(&path).unwrap();
    assert_eq!(
        reopened.entries().cloned().collect::<Vec<_>>(),
        vec![entries[1].clone(), entries[0].clone()]
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        bytes(&[entries[1].clone(), entries[0].clone()])
    );
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 1);
}

#[test]
fn unknown_format_unknown_fields_duplicate_fields_and_trailing_object_are_refused() {
    let root = Scratch::new();
    let path = root.file(JOURNAL_NAME);
    let e = entry(1, 1, Phase::Moved);
    let mut value = serde_json::json!({ "format": FORMAT, "entries": [e] });
    value["format"] = "crosspane-win-twin-v99".into();
    let unknown = serde_json::to_vec(&value).unwrap();
    fs::write(&path, &unknown).unwrap();
    assert!(JournalFile::open(&path).is_err());
    assert_eq!(fs::read(&path).unwrap(), unknown);
    value["format"] = FORMAT.into();
    value["extra"] = true.into();
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(JournalFile::open(&path).is_err());
    let duplicate = format!(r#"{{"format":"{FORMAT}","format":"{FORMAT}","entries":[]}}"#);
    fs::write(&path, duplicate).unwrap();
    assert!(JournalFile::open(&path).is_err());
    let mut trailing = bytes(&[e]);
    trailing.extend_from_slice(b"{}");
    fs::write(&path, trailing).unwrap();
    assert!(JournalFile::open(&path).is_err());
}

#[test]
fn truncation_at_every_byte_never_loads_a_partial_entry_or_rewrites_input() {
    let root = Scratch::new();
    let path = root.file(JOURNAL_NAME);
    let complete = bytes(&[entry(1, 1, Phase::Journaled), entry(2, 2, Phase::Moved)]);
    for cut in 0..complete.len() {
        fs::write(&path, &complete[..cut]).unwrap();
        assert!(JournalFile::open(&path).is_err(), "cut at {cut}");
        assert_eq!(fs::read(&path).unwrap(), &complete[..cut]);
    }
    fs::write(&path, &complete).unwrap();
    assert_eq!(JournalFile::open(&path).unwrap().entries().count(), 2);
}

#[test]
fn duplicate_window_or_twin_serial_and_invalid_entries_are_refused_without_mutation() {
    let root = Scratch::new();
    let path = root.file(JOURNAL_NAME);
    let e = entry(1, 1, Phase::Moved);
    let mut journal = JournalFile::open(&path).unwrap();
    journal.insert(e.clone()).unwrap();
    journal.save().unwrap();
    let baseline = fs::read(&path).unwrap();
    let mut variants = vec![e.clone(), entry(2, 1, Phase::Moved)];
    let mut invalid = e.clone();
    invalid.window = 0;
    variants.push(invalid);
    let mut invalid = entry(2, 2, Phase::Moved);
    invalid.pid = 0;
    variants.push(invalid);
    let mut invalid = entry(2, 2, Phase::Moved);
    invalid.original.rect_physical = [0, 0, 0, 20];
    variants.push(invalid);
    let mut invalid = entry(2, 2, Phase::Moved);
    invalid.original.monitor_path = "typed\ncontrol".into();
    variants.push(invalid);
    let mut invalid = entry(2, 2, Phase::Moved);
    invalid.original.monitor_path = "x".repeat(4097);
    variants.push(invalid);
    let mut invalid = entry(2, 2, Phase::Moved);
    invalid.original.dpi = 0;
    variants.push(invalid);
    let mut invalid = entry(2, 2, Phase::Moved);
    invalid.twin.serial = 0;
    variants.push(invalid);
    let mut invalid = entry(2, 2, Phase::Moved);
    invalid.twin.mode = (u32::MAX, 1080);
    variants.push(invalid);
    let mut invalid = entry(2, 2, Phase::Moved);
    invalid.twin.origin = (SHORT_MIN - 1, 0);
    variants.push(invalid);
    let mut invalid = entry(2, 2, Phase::Moved);
    invalid.twin.refresh_millihz = 0;
    variants.push(invalid);
    let mut invalid = entry(2, 2, Phase::Moved);
    invalid.twin.dpi = 0;
    variants.push(invalid);
    let mut invalid = entry(2, 2, Phase::Moved);
    invalid.twin.device_path = None;
    variants.push(invalid);
    for invalid in variants {
        assert!(journal.insert(invalid.clone()).is_err());
        assert_eq!(
            journal.entries().cloned().collect::<Vec<_>>(),
            vec![e.clone()]
        );
        assert_eq!(fs::read(&path).unwrap(), baseline);
        fs::write(&path, bytes(&[e.clone(), invalid])).unwrap();
        assert!(JournalFile::open(&path).is_err());
        fs::write(&path, &baseline).unwrap();
    }
}

#[test]
fn journal_count_and_byte_bounds_fail_closed() {
    let root = Scratch::new();
    let path = root.file(JOURNAL_NAME);
    let entries: Vec<_> = (1..=129)
        .map(|id| entry(id, id as u32, Phase::Journaled))
        .collect();
    fs::write(&path, bytes(&entries)).unwrap();
    assert!(JournalFile::open(&path).is_err());
    fs::write(&path, vec![b' '; 1024 * 1024 + 1]).unwrap();
    assert!(JournalFile::open(&path).is_err());
    fs::remove_file(&path).unwrap();
    let mut journal = JournalFile::open(&path).unwrap();
    for e in entries.iter().take(128) {
        journal.insert(e.clone()).unwrap();
    }
    assert!(journal.insert(entries[128].clone()).is_err());
    assert_eq!(journal.entries().count(), 128);
    let many: Vec<_> = (1..=128)
        .map(|id| {
            let mut e = entry(id, id as u32, Phase::Moved);
            e.original.monitor_path = "a".repeat(4096);
            e.twin.device_path = Some("b".repeat(4096));
            e
        })
        .collect();
    let mut journal = JournalFile::open(&path).unwrap();
    for e in many {
        journal.insert(e).unwrap();
    }
    assert!(journal.save().is_err());
    assert!(!path.exists());
}

#[test]
fn phases_are_monotonic_path_bound_and_only_persisted_by_save() {
    let root = Scratch::new();
    let path = root.file(JOURNAL_NAME);
    let mut journal = JournalFile::open(&path).unwrap();
    journal.insert(entry(1, 1, Phase::Journaled)).unwrap();
    journal.save().unwrap();
    let before = fs::read(&path).unwrap();
    assert!(journal.set_phase(1, Phase::TwinUp, None).is_err());
    assert!(
        journal
            .set_phase(2, Phase::TwinUp, Some("device".into()))
            .is_err()
    );
    assert_eq!(journal.entries().next().unwrap().phase, Phase::Journaled);
    journal
        .set_phase(1, Phase::TwinUp, Some("device".into()))
        .unwrap();
    assert_eq!(fs::read(&path).unwrap(), before);
    journal.save().unwrap();
    let twin_up = fs::read(&path).unwrap();
    assert!(
        journal
            .set_phase(1, Phase::Journaled, Some("device".into()))
            .is_err()
    );
    assert!(
        journal
            .set_phase(1, Phase::Moved, Some("other-device".into()))
            .is_err()
    );
    assert!(journal.set_phase(1, Phase::Moved, None).is_err());
    assert_eq!(fs::read(&path).unwrap(), twin_up);
    journal
        .set_phase(1, Phase::Moved, Some("device".into()))
        .unwrap();
    journal.save().unwrap();
    assert_eq!(
        JournalFile::open(&path)
            .unwrap()
            .entries()
            .next()
            .unwrap()
            .phase,
        Phase::Moved
    );
    assert!(journal.remove(2).is_none());
    assert!(journal.remove(1).is_some());
    journal.save().unwrap();
    assert_eq!(JournalFile::open(&path).unwrap().entries().count(), 0);
}

#[test]
fn save_failure_keeps_existing_destination_and_cleans_owned_temporary() {
    let root = Scratch::new();
    let path = root.file(JOURNAL_NAME);
    let mut journal = JournalFile::open(&path).unwrap();
    journal.insert(entry(1, 1, Phase::Journaled)).unwrap();
    fs::create_dir(&path).unwrap();
    fs::write(path.join("retained"), b"injected-owned-marker").unwrap();
    assert!(journal.save().is_err());
    assert_eq!(
        fs::read(path.join("retained")).unwrap(),
        b"injected-owned-marker"
    );
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 1);
}

#[test]
fn atomic_save_reader_observes_complete_old_or_new_document_never_absence() {
    let root = Scratch::new();
    let path = root.file(JOURNAL_NAME);
    let a = entry(1, 1, Phase::Journaled);
    let b = entry(2, 2, Phase::Moved);
    let mut journal = JournalFile::open(&path).unwrap();
    journal.insert(a.clone()).unwrap();
    journal.save().unwrap();
    let initial = bytes(std::slice::from_ref(&a));
    let replacement = bytes(std::slice::from_ref(&b));
    let stop = AtomicBool::new(false);
    let observations = AtomicU64::new(0);
    struct Stop<'a>(&'a AtomicBool);
    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    std::thread::scope(|scope| {
        let guard = Stop(&stop);
        let reader = scope.spawn(|| {
            while !stop.load(Ordering::Acquire) {
                let seen = fs::read(&path).unwrap();
                assert!(seen == initial || seen == replacement);
                observations.fetch_add(1, Ordering::Relaxed);
            }
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while observations.load(Ordering::Relaxed) == 0
            && !reader.is_finished()
            && std::time::Instant::now() < deadline
        {
            std::thread::yield_now();
        }
        assert!(observations.load(Ordering::Relaxed) > 0);
        for i in 0..40 {
            let existing: Vec<_> = journal.entries().map(|e| e.window).collect();
            for window in existing {
                journal.remove(window);
            }
            journal
                .insert(if i % 2 == 0 { b.clone() } else { a.clone() })
                .unwrap();
            journal.save().unwrap();
        }
        drop(guard);
        reader.join().unwrap();
    });
    assert!(observations.load(Ordering::Relaxed) > 0);
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 1);
}

#[test]
fn recovery_dead_pid_and_reused_window_never_restore_foreign_process() {
    let e = entry(1, 1, Phase::Moved);
    for live in [
        vec![],
        vec![(1, 11, 20, [1920, 0, 2520, 400])],
        vec![(1, 10, 21, [1920, 0, 2520, 400])],
    ] {
        assert_eq!(
            recovery_plan(std::slice::from_ref(&e), &live, &[]),
            vec![
                RecoveryStep::RemoveTwin { serial: 1 },
                RecoveryStep::Forget(1)
            ]
        );
    }
}

#[test]
fn recovery_crash_before_move_keeps_current_original_placement() {
    for phase in [Phase::TwinUp, Phase::Moved] {
        let e = entry(1, 1, phase);
        assert_eq!(
            recovery_plan(
                std::slice::from_ref(&e),
                &[(1, 10, 20, e.original.rect_physical)],
                &[]
            ),
            vec![
                RecoveryStep::RemoveTwin { serial: 1 },
                RecoveryStep::Forget(1)
            ]
        );
    }
}

#[test]
fn recovery_crash_after_move_on_twin_restores_original() {
    for phase in [Phase::TwinUp, Phase::Moved] {
        let e = entry(1, 1, phase);
        for rect in [[1920, 0, 2520, 400], [1919, 0, 1921, 1]] {
            assert_eq!(
                recovery_plan(std::slice::from_ref(&e), &[(1, 10, 20, rect)], &[]),
                vec![
                    RecoveryStep::RestoreWindow {
                        window: 1,
                        original: e.original.clone()
                    },
                    RecoveryStep::RemoveTwin { serial: 1 },
                    RecoveryStep::Forget(1)
                ]
            );
        }
    }
}

#[test]
fn recovery_user_moved_elsewhere_is_forgotten_without_restoring() {
    for phase in [Phase::TwinUp, Phase::Moved] {
        let e = entry(1, 1, phase);
        for rect in [
            [5000, 0, 5600, 400],
            [1320, 0, 1920, 400],
            [1920, 0, 1920, 400],
        ] {
            assert_eq!(
                recovery_plan(std::slice::from_ref(&e), &[(1, 10, 20, rect)], &[]),
                vec![
                    RecoveryStep::RemoveTwin { serial: 1 },
                    RecoveryStep::Forget(1)
                ]
            );
        }
    }
}

#[test]
fn recovery_journaled_never_moves_window_and_twin_up_handles_interrupted_move() {
    for phase in [Phase::Journaled, Phase::TwinUp, Phase::Moved] {
        let e = entry(1, 1, phase);
        let mut expected = vec![];
        if phase != Phase::Journaled {
            expected.push(RecoveryStep::RestoreWindow {
                window: 1,
                original: e.original.clone(),
            });
        }
        expected.extend([
            RecoveryStep::RemoveTwin { serial: 1 },
            RecoveryStep::Forget(1),
        ]);
        assert_eq!(
            recovery_plan(&[e], &[(1, 10, 20, [1920, 0, 2520, 400])], &[]),
            expected
        );
    }
}

#[test]
fn recovery_orphans_are_swept_without_journal_and_removals_are_deduplicated() {
    let a = entry(1, 1, Phase::Moved);
    let b = entry(2, 2, Phase::Moved);
    assert_eq!(
        recovery_plan(&[], &[], &[a.twin.clone(), b.twin.clone(), a.twin.clone()]),
        vec![
            RecoveryStep::RemoveTwin { serial: 1 },
            RecoveryStep::RemoveTwin { serial: 2 }
        ]
    );
    assert_eq!(
        recovery_plan(
            std::slice::from_ref(&a),
            &[(1, 10, 20, [1920, 0, 2520, 400])],
            &[a.twin.clone(), b.twin]
        ),
        vec![
            RecoveryStep::RestoreWindow {
                window: 1,
                original: a.original
            },
            RecoveryStep::RemoveTwin { serial: 1 },
            RecoveryStep::Forget(1),
            RecoveryStep::RemoveTwin { serial: 2 }
        ]
    );
}

#[test]
fn ambiguous_live_window_identity_never_authorizes_restore() {
    let e = entry(1, 1, Phase::Moved);
    for live in [
        vec![
            (1, 10, 20, [1920, 0, 2520, 400]),
            (1, 10, 20, [1920, 0, 2520, 400]),
        ],
        vec![
            (1, 10, 20, [1920, 0, 2520, 400]),
            (1, 11, 30, [1920, 0, 2520, 400]),
        ],
    ] {
        assert_eq!(
            recovery_plan(std::slice::from_ref(&e), &live, &[]),
            vec![
                RecoveryStep::RemoveTwin { serial: 1 },
                RecoveryStep::Forget(1)
            ]
        );
    }
}

#[test]
fn twin_origin_prefers_available_right_slot_and_handles_negative_and_short_limits() {
    assert_eq!(
        twin_origin([0, 0, 1920, 1080], (1920, 1080), &[]),
        Some((1920, 0))
    );
    assert_eq!(
        twin_origin([0, 0, 1920, 1080], (1920, 1080), &[[1920, 0, 3840, 1080]]),
        Some((1920, 1080))
    );
    for real in [
        [-1920, -1080, 0, 0],
        [SHORT_MIN, -1080, -100, 1080],
        [100, -500, SHORT_MAX, 500],
    ] {
        let (x, y) = twin_origin(real, (1920, 1080), &[]).unwrap();
        assert!(x >= SHORT_MIN && y >= SHORT_MIN);
        assert!(i64::from(x) + 1920 <= i64::from(SHORT_MAX));
        assert!(i64::from(y) + 1080 <= i64::from(SHORT_MAX));
        assert!(!(x < real[2] && x + 1920 > real[0] && y < real[3] && y + 1080 > real[1]));
    }
}

#[test]
fn twin_origin_full_screen_invalid_rectangles_or_oversized_mode_have_no_candidate() {
    assert_eq!(
        twin_origin([SHORT_MIN, SHORT_MIN, SHORT_MAX, SHORT_MAX], (1, 1), &[]),
        None
    );
    assert_eq!(twin_origin([0, 0, 1920, 1080], (u32::MAX, 1080), &[]), None);
    assert_eq!(twin_origin([0, 0, 1920, 1080], (0, 1080), &[]), None);
    assert_eq!(twin_origin([1, 0, 1, 100], (10, 10), &[]), None);
    assert_eq!(
        twin_origin([0, 0, 1920, 1080], (10, 10), &[[SHORT_MIN - 1, 0, 0, 100]]),
        None
    );
}

#[test]
fn edid_checksum_marker_serial_mode_size_and_all_descriptors_are_literal() {
    for (serial, mode, refresh, scale) in [
        (1, (1920, 1080), 60000, 1.0),
        (u32::MAX, (3840, 2160), 30000, 2.0),
        (7, (2560, 1440), 59940, 1.5),
    ] {
        let size = twin_size_mm(mode, scale).unwrap();
        let e = twin_edid(serial, mode, refresh, size).unwrap();
        assert_eq!(e.len(), 128);
        assert_eq!(e.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)), 0);
        assert_eq!(&e[..8], &[0, 255, 255, 255, 255, 255, 255, 0]);
        assert_eq!(&e[12..16], &serial.to_le_bytes());
        assert_eq!(&e[18..21], &[1, 4, 0x80]);
        assert_eq!(&e[72..77], &[0, 0, 0, 0xfc, 0]);
        assert_eq!(&e[77..90], EDID_MARKER);
        assert_eq!(&e[90..95], &[0, 0, 0, 0xff, 0]);
        assert_eq!(&e[95..104], format!("{serial:08X}\n").as_bytes());
        assert_eq!(&e[104..108], b"    ");
        assert_eq!(&e[108..113], &[0, 0, 0, 0x10, 0]);
        assert_eq!(&e[113..127], &[0; 14]);
        assert_eq!(u32::from(e[56]) | (u32::from(e[58] >> 4) << 8), mode.0);
        assert_eq!(u32::from(e[59]) | (u32::from(e[61] >> 4) << 8), mode.1);
        assert_eq!(
            u16::from(e[66]) | (u16::from(e[68] >> 4) << 8),
            size.width as u16
        );
        assert_eq!(
            u16::from(e[67]) | (u16::from(e[68] & 15) << 8),
            size.height as u16
        );
        let expected_clock =
            (u64::from(mode.0 + 160) * u64::from(mode.1 + 45) * u64::from(refresh) + 5_000_000)
                / 10_000_000;
        assert_eq!(
            u64::from(u16::from_le_bytes([e[54], e[55]])),
            expected_clock
        );
    }
}

#[test]
fn edid_and_size_refuse_invalid_or_unrepresentable_candidates() {
    for scale in [
        0.0,
        -1.0,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        0.000001,
        1e300,
    ] {
        assert!(twin_size_mm((1920, 1080), scale).is_err());
    }
    assert!(twin_size_mm((0, 1080), 1.0).is_err());
    let size = SizeMm::new(500.0, 300.0);
    for mode in [
        (0, 1080),
        (1920, 0),
        (4096, 1080),
        (1920, 4096),
        (u32::MAX, u32::MAX),
    ] {
        assert!(twin_edid(1, mode, 60000, size).is_err());
    }
    for refresh in [0, 1, u32::MAX] {
        assert!(twin_edid(1, (1920, 1080), refresh, size).is_err());
    }
    assert!(twin_edid(0, (1920, 1080), 60000, size).is_err());
    assert!(twin_edid(1, (4095, 4095), 60000, size).is_err());
    for size in [
        SizeMm::new(0.0, 300.0),
        SizeMm::new(9.0, 300.0),
        SizeMm::new(500.0, f64::NAN),
        SizeMm::new(f64::INFINITY, 300.0),
        SizeMm::new(2551.0, 300.0),
    ] {
        assert!(twin_edid(1, (1920, 1080), 60000, size).is_err());
    }
}

#[test]
fn desired_edid_size_tracks_scale_without_claiming_windows_selected_dpi() {
    let full = twin_size_mm((1920, 1080), 1.0).unwrap();
    let half = twin_size_mm((1920, 1080), 2.0).unwrap();
    assert_eq!(full, SizeMm::new(508.0, 286.0));
    assert_eq!(half, SizeMm::new(254.0, 143.0));
}

#[test]
fn parked_geometry_clips_and_translates_to_twin_local_pixels_with_literal_identity() {
    let p = parked_geometry(
        WindowId(77),
        [1800, -100, 2600, 1200],
        [1920, 0, 3840, 1080],
        DisplayId(9),
    )
    .unwrap();
    assert_eq!(
        (p.window, p.kind, p.display),
        (WindowId(77), ParkingKind::Twin, DisplayId(9))
    );
    assert_eq!(p.content.min, point2(0, 0));
    assert_eq!(p.content.max, point2(680, 1080));
    let p = parked_geometry(
        WindowId(1),
        [-3000, -1000, -1000, 500],
        [-1920, -1080, 0, 0],
        DisplayId(2),
    )
    .unwrap();
    assert_eq!(p.content.min, point2(0, 80));
    assert_eq!(p.content.max, point2(920, 1080));
    let p = parked_geometry(
        WindowId(1),
        [i32::MIN, i32::MIN, i32::MAX, i32::MAX],
        [SHORT_MIN, SHORT_MIN, SHORT_MAX, SHORT_MAX],
        DisplayId(2),
    )
    .unwrap();
    assert_eq!(p.content.min, point2(0, 0));
    assert_eq!(p.content.max, point2(65535, 65535));
}

#[test]
fn parked_geometry_rejects_empty_touching_disjoint_or_invalid_twin() {
    for window in [[0, 0, 0, 10], [0, 0, 100, 100], [3840, 0, 4000, 100]] {
        assert!(parked_geometry(WindowId(1), window, [1920, 0, 3840, 1080], DisplayId(2)).is_err());
    }
    assert!(parked_geometry(WindowId(1), [0, 0, 100, 100], [0, 0, 0, 100], DisplayId(2)).is_err());
    assert!(
        parked_geometry(
            WindowId(1),
            [0, 0, 100, 100],
            [SHORT_MIN - 1, 0, 100, 100],
            DisplayId(2)
        )
        .is_err()
    );
}

proptest! {
    #[test]
    fn twin_origin_matches_exhaustive_small_free_grid(
        occupied in prop::collection::vec(any::<bool>(), 16),
        w in 1u32..=4, h in 1u32..=4,
    ) {
        let virtual_screen = [SHORT_MIN, SHORT_MIN, 0, SHORT_MAX];
        let mut taken = vec![[0, SHORT_MIN, SHORT_MAX, 0], [0, 4, SHORT_MAX, SHORT_MAX], [4, 0, SHORT_MAX, 4]];
        for (i, occupied) in occupied.iter().enumerate() {
            if *occupied {
                let (x, y) = ((i % 4) as i32, (i / 4) as i32);
                taken.push([x, y, x + 1, y + 1]);
            }
        }
        let fits = |x: i32, y: i32| {
            !taken.iter().any(|r| x < r[2] && x + w as i32 > r[0] && y < r[3] && y + h as i32 > r[1])
        };
        let expected = (0..=4 - w as i32).any(|x| (0..=4 - h as i32).any(|y| fits(x, y)));
        let found = twin_origin(virtual_screen, (w, h), &taken);
        prop_assert_eq!(found.is_some(), expected);
        if let Some((x, y)) = found {
            prop_assert!(x >= 0 && y >= 0 && x + w as i32 <= 4 && y + h as i32 <= 4);
            prop_assert!(fits(x, y));
        }
    }
}
