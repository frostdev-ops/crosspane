#![allow(clippy::unwrap_used)]
//! Twin ledger orchestration (WP-W3.1b T5) against a scripted fake backend. No OS calls.
//!
//! The fake re-opens the journal file on every IOCTL and lane close, so each test can check that
//! the record was on disk before the native call it guards.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crosspane_platform_windows::model::cpd::MAX_OPENS;
use crosspane_platform_windows::model::journal::{
    JOURNAL_NAME, JournalFile, TwinPhase, TwinRecord,
};
use crosspane_platform_windows::model::twin::{
    NEW_PATH_TIMEOUT_MS, OwnPath, PATH_POLL_MS, Refusal, STALE_TIMEOUT_MS, TwinError, TwinKey,
    TwinMode,
};
use crosspane_platform_windows::model::twin_ledger::*;

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

const MODE_720: TwinMode = TwinMode {
    width: 1280,
    height: 720,
    width_mm: 339,
    height_mm: 191,
};
const MODE_1080: TwinMode = TwinMode {
    width: 1920,
    height: 1080,
    width_mm: 508,
    height_mm: 286,
};

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        for _ in 0..32 {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "crosspane-w31b-twin-ledger-{}-{stamp}-{}",
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

fn record(key: u32, phase: TwinPhase, mode: TwinMode, monitor_id: Option<u32>) -> TwinRecord {
    TwinRecord {
        key,
        phase,
        mode: (mode.width, mode.height),
        size_mm: (mode.width_mm, mode.height_mm),
        monitor_id,
    }
}

fn adding(key: u32, mode: TwinMode) -> TwinRecord {
    record(key, TwinPhase::Adding, mode, None)
}

fn up(key: u32, mode: TwinMode, id: u32) -> TwinRecord {
    record(key, TwinPhase::Up, mode, Some(id))
}

fn removing(key: u32, mode: TwinMode, id: u32) -> TwinRecord {
    record(key, TwinPhase::Removing, mode, Some(id))
}

/// Stages `Adding` then `Up` for a key that has no record yet, and saves.
fn put_up(journal: &mut JournalFile, key: u32, mode: TwinMode, id: u32) {
    journal.put_twin(adding(key, mode)).unwrap();
    journal.put_twin(up(key, mode, id)).unwrap();
    journal.save().unwrap();
}

/// Makes every save fail: a directory at the journal path cannot be renamed over.
fn break_disk(path: &Path) {
    let _ = fs::remove_file(path);
    fs::create_dir(path).unwrap();
}

fn mend_disk(path: &Path) {
    fs::remove_dir(path).unwrap();
}

fn path_for(id: u32, mode: TwinMode) -> OwnPath {
    OwnPath {
        luid: 1,
        target: id,
        source: id,
        monitor_path: format!(r"\\?\DISPLAY#CRSP#{id}"),
        gdi_name: format!(r"\\.\DISPLAY{id}"),
        rect: [0, 0, mode.width as i32, mode.height as i32],
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    Open(u32),
    Add { lane: u32, mode: TwinMode },
    Remove { lane: u32, id: u32 },
    List(u32),
    Close(u32),
}

/// One native call, with the journal as it was on disk when the call was made.
struct Seen {
    call: Call,
    disk: Option<Vec<TwinRecord>>,
}

#[derive(Default)]
struct Fake {
    journal: PathBuf,
    seen: Vec<Seen>,
    now: u64,
    next_lane: u32,
    next_id: u32,
    dpi_value: u32,
    /// The monitor id each lane's ADD produced, so closing the lane retires that path.
    lane_monitor: BTreeMap<u32, u32>,
    lost: BTreeSet<u32>,
    paths: Vec<OwnPath>,
    /// Our paths vanish once the clock reaches this time (for startup recovery).
    vanish_at: Option<u64>,
    /// Whether an ADD makes its new path appear.
    appear_on_add: bool,
    /// Whether a REMOVE makes its path vanish.
    vanish_on_remove: bool,
    /// The next ADD replaces the journal with a directory, so the save after it fails.
    break_disk_on_add: bool,
    list_reply: Option<u32>,
    open_error: Option<TwinError>,
    add_error: Option<TwinError>,
    remove_error: Option<TwinError>,
    list_error: Option<TwinError>,
    own_paths_error: Option<TwinError>,
    dpi_error: Option<TwinError>,
    /// The next this many DPI reads say `NotOnDesktop`, as when the monitor list lags.
    dpi_not_on_desktop: u32,
}

impl Fake {
    fn new(journal: PathBuf) -> Self {
        Self {
            journal,
            next_lane: 1,
            next_id: 7,
            dpi_value: 144,
            appear_on_add: true,
            vanish_on_remove: true,
            ..Self::default()
        }
    }

    /// The journal as it is on disk now. None when the file cannot be read.
    fn disk(&self) -> Option<Vec<TwinRecord>> {
        JournalFile::open(&self.journal)
            .ok()
            .map(|journal| journal.twins().cloned().collect())
    }

    /// Records a native call, with the journal re-read from disk at this moment.
    fn note(&mut self, call: Call) {
        let disk = self.disk();
        self.seen.push(Seen { call, disk });
    }

    fn calls(&self) -> Vec<Call> {
        self.seen.iter().map(|seen| seen.call.clone()).collect()
    }

    fn disk_at(&self, index: usize) -> Option<Vec<TwinRecord>> {
        self.seen[index].disk.clone()
    }
}

impl TwinBackend for Fake {
    type Lane = u32;

    fn open_lane(&mut self) -> Result<u32, TwinError> {
        if let Some(error) = self.open_error.take() {
            return Err(error);
        }
        let lane = self.next_lane;
        self.next_lane += 1;
        self.note(Call::Open(lane));
        Ok(lane)
    }

    fn add(&mut self, lane: &u32, mode: TwinMode, _timeout_ms: u32) -> Result<u32, TwinError> {
        self.note(Call::Add { lane: *lane, mode });
        if let Some(error) = self.add_error.take() {
            return Err(error);
        }
        let id = self.next_id;
        self.next_id += 1;
        self.lane_monitor.insert(*lane, id);
        if self.appear_on_add {
            self.paths.push(path_for(id, mode));
        }
        if self.break_disk_on_add {
            break_disk(&self.journal);
        }
        Ok(id)
    }

    fn remove(&mut self, lane: &u32, monitor_id: u32, _timeout_ms: u32) -> Result<(), TwinError> {
        self.note(Call::Remove {
            lane: *lane,
            id: monitor_id,
        });
        if let Some(error) = self.remove_error.take() {
            return Err(error);
        }
        if self.vanish_on_remove {
            self.paths.retain(|path| path.target != monitor_id);
        }
        Ok(())
    }

    fn list(&mut self, lane: &u32, _timeout_ms: u32) -> Result<Option<u32>, TwinError> {
        self.note(Call::List(*lane));
        if let Some(error) = self.list_error.take() {
            return Err(error);
        }
        Ok(self.list_reply)
    }

    fn close_lane(&mut self, lane: u32) {
        self.note(Call::Close(lane));
        // Closing a handle retires the twin it carried.
        if let Some(id) = self.lane_monitor.remove(&lane) {
            self.paths.retain(|path| path.target != id);
        }
    }

    fn lane_lost(&self, lane: &u32) -> bool {
        self.lost.contains(lane)
    }

    fn own_paths(&mut self) -> Result<Vec<OwnPath>, TwinError> {
        if let Some(error) = self.own_paths_error.take() {
            return Err(error);
        }
        if self.vanish_at.is_some_and(|at| self.now >= at) {
            self.paths.clear();
        }
        Ok(self.paths.clone())
    }

    fn dpi(&mut self, _rect: [i32; 4]) -> Result<u32, TwinError> {
        if self.dpi_not_on_desktop > 0 {
            self.dpi_not_on_desktop -= 1;
            return Err(TwinError::NotOnDesktop);
        }
        if let Some(error) = self.dpi_error.take() {
            return Err(error);
        }
        Ok(self.dpi_value)
    }

    fn now_ms(&self) -> u64 {
        self.now
    }

    fn sleep_ms(&mut self, ms: u32) {
        self.now += u64::from(ms);
    }
}

/// A fresh journal, ledger and fake, with the journal in a scratch directory.
fn rig() -> (Scratch, JournalFile, TwinLedger<Fake>) {
    let scratch = Scratch::new();
    let path = scratch.journal_path();
    let journal = JournalFile::open(&path).unwrap();
    let ledger = TwinLedger::new(Fake::new(path));
    (scratch, journal, ledger)
}

#[test]
fn add_saves_adding_before_the_add_and_up_after_it() {
    let (_scratch, mut journal, mut ledger) = rig();
    let display = ledger.add(&mut journal, MODE_720).unwrap();

    assert_eq!(display.key, TwinKey(1));
    assert_eq!(display.monitor_id, 7);
    assert_eq!(display.mode, MODE_720);
    assert_eq!(display.gdi_name, r"\\.\DISPLAY7");
    assert_eq!(display.monitor_path, r"\\?\DISPLAY#CRSP#7");
    assert_eq!(display.rect, [0, 0, 1280, 720]);
    assert_eq!(display.dpi, 144);
    assert_eq!(ledger.keys(), vec![TwinKey(1)]);
    assert_eq!(ledger.display(TwinKey(1)), Some(&display));

    let fake = ledger.backend();
    assert_eq!(
        fake.calls(),
        vec![
            Call::Open(1),
            Call::Add {
                lane: 1,
                mode: MODE_720
            }
        ]
    );
    assert_eq!(fake.disk_at(1), Some(vec![adding(1, MODE_720)]));
    assert_eq!(fake.disk(), Some(vec![up(1, MODE_720, 7)]));
    assert_eq!(journal.twin(1), Some(&up(1, MODE_720, 7)));
}

#[test]
fn add_refusal_closes_the_lane_and_forgets_the_record() {
    let (_scratch, mut journal, mut ledger) = rig();
    ledger.backend_mut().add_error = Some(TwinError::Refused(Refusal::Busy));

    assert_eq!(
        ledger.add(&mut journal, MODE_720),
        Err(TwinError::Refused(Refusal::Busy))
    );
    let fake = ledger.backend();
    assert_eq!(
        fake.calls(),
        vec![
            Call::Open(1),
            Call::Add {
                lane: 1,
                mode: MODE_720
            },
            Call::Close(1)
        ]
    );
    // Nothing was added, so the close sees only the Adding record, and nothing is left after.
    assert_eq!(fake.disk_at(2), Some(vec![adding(1, MODE_720)]));
    assert_eq!(fake.disk(), Some(vec![]));
    assert!(ledger.keys().is_empty());
    assert!(journal.twin(1).is_none());
}

#[test]
fn add_without_a_desktop_path_stages_removing_before_the_close() {
    let (_scratch, mut journal, mut ledger) = rig();
    ledger.backend_mut().appear_on_add = false;

    assert_eq!(
        ledger.add(&mut journal, MODE_720),
        Err(TwinError::NotOnDesktop)
    );
    let fake = ledger.backend();
    assert_eq!(fake.calls().last(), Some(&Call::Close(1)));
    assert_eq!(fake.disk_at(2), Some(vec![removing(1, MODE_720, 7)]));
    assert!(fake.now_ms() >= u64::from(NEW_PATH_TIMEOUT_MS));
    assert!(ledger.keys().is_empty());
    assert_eq!(fake.disk(), Some(vec![]));
}

#[test]
fn dpi_failure_rolls_back_like_a_missing_desktop_path() {
    let (_scratch, mut journal, mut ledger) = rig();
    ledger.backend_mut().dpi_error = Some(TwinError::Native("GetDpiForMonitor", 5));

    assert_eq!(
        ledger.add(&mut journal, MODE_720),
        Err(TwinError::Native("GetDpiForMonitor", 5))
    );
    let fake = ledger.backend();
    assert_eq!(fake.calls().last(), Some(&Call::Close(1)));
    assert_eq!(fake.disk_at(2), Some(vec![removing(1, MODE_720, 7)]));
    assert!(ledger.keys().is_empty());
    assert_eq!(fake.disk(), Some(vec![]));
}

#[test]
fn a_lagging_monitor_list_is_polled_for_the_dpi() {
    let (_scratch, mut journal, mut ledger) = rig();
    ledger.backend_mut().dpi_not_on_desktop = 2;

    let display = ledger.add(&mut journal, MODE_720).unwrap();

    assert_eq!(display.dpi, 144);
    let fake = ledger.backend();
    // The path was listed at once, so the clock moves only for the two DPI polls.
    assert_eq!(fake.now_ms(), 2 * u64::from(PATH_POLL_MS));
    assert_eq!(
        fake.calls(),
        vec![
            Call::Open(1),
            Call::Add {
                lane: 1,
                mode: MODE_720
            }
        ]
    );
    assert_eq!(fake.disk(), Some(vec![up(1, MODE_720, 7)]));
    assert_eq!(ledger.keys(), vec![TwinKey(1)]);
}

#[test]
fn the_fifth_lane_is_refused_before_any_open_or_journal_write() {
    let (_scratch, mut journal, mut ledger) = rig();
    for _ in 0..MAX_OPENS {
        ledger.add(&mut journal, MODE_720).unwrap();
    }
    let calls_before = ledger.backend().calls();
    let disk_before = ledger.backend().disk();

    assert_eq!(
        ledger.add(&mut journal, MODE_1080),
        Err(TwinError::Refused(Refusal::Capacity))
    );
    assert_eq!(ledger.backend().calls(), calls_before);
    assert_eq!(ledger.backend().disk(), disk_before);
    assert_eq!(journal.twins().count(), MAX_OPENS);
    assert_eq!(ledger.keys().len(), MAX_OPENS);
}

#[test]
fn a_failed_adding_save_sends_no_ioctl_and_leaves_no_record() {
    let (scratch, mut journal, mut ledger) = rig();
    break_disk(&scratch.journal_path());

    assert!(matches!(
        ledger.add(&mut journal, MODE_720),
        Err(TwinError::Journal(_))
    ));
    assert!(ledger.backend().calls().is_empty());
    assert!(journal.twins().next().is_none());
    assert!(ledger.keys().is_empty());

    // Once the disk is back, the same ledger works.
    mend_disk(&scratch.journal_path());
    let display = ledger.add(&mut journal, MODE_720).unwrap();
    assert_eq!(display.monitor_id, 7);
    assert_eq!(ledger.keys().len(), 1);
}

#[test]
fn a_failed_up_save_after_the_add_closes_without_a_remove() {
    let (scratch, mut journal, mut ledger) = rig();
    ledger.backend_mut().break_disk_on_add = true;

    assert!(matches!(
        ledger.add(&mut journal, MODE_720),
        Err(TwinError::Journal(_))
    ));
    let fake = ledger.backend();
    assert_eq!(
        fake.calls(),
        vec![
            Call::Open(1),
            Call::Add {
                lane: 1,
                mode: MODE_720
            },
            Call::Close(1)
        ]
    );
    assert_eq!(fake.disk_at(1), Some(vec![adding(1, MODE_720)]));
    assert!(ledger.keys().is_empty());
    assert!(journal.twins().next().is_none());
    mend_disk(&scratch.journal_path());
}

#[test]
fn a_failed_path_snapshot_before_the_add_closes_the_lane() {
    let (_scratch, mut journal, mut ledger) = rig();
    ledger.backend_mut().own_paths_error = Some(TwinError::Native("QueryDisplayConfig", 87));

    assert_eq!(
        ledger.add(&mut journal, MODE_720),
        Err(TwinError::Native("QueryDisplayConfig", 87))
    );
    let fake = ledger.backend();
    assert_eq!(fake.calls(), vec![Call::Open(1), Call::Close(1)]);
    assert_eq!(fake.disk_at(1), Some(vec![adding(1, MODE_720)]));
    assert!(ledger.keys().is_empty());
    assert_eq!(fake.disk(), Some(vec![]));
}

#[test]
fn remove_stages_removing_before_the_ioctl_and_forgets_after_the_close() {
    let (_scratch, mut journal, mut ledger) = rig();
    ledger.add(&mut journal, MODE_720).unwrap();

    ledger.remove(&mut journal, TwinKey(1)).unwrap();

    let fake = ledger.backend();
    assert_eq!(
        fake.calls(),
        vec![
            Call::Open(1),
            Call::Add {
                lane: 1,
                mode: MODE_720
            },
            Call::Remove { lane: 1, id: 7 },
            Call::Close(1)
        ]
    );
    assert_eq!(fake.disk_at(2), Some(vec![removing(1, MODE_720, 7)]));
    // The close comes before the forget, so the journal still covers the twin at close.
    assert_eq!(fake.disk_at(3), Some(vec![removing(1, MODE_720, 7)]));
    assert_eq!(fake.disk(), Some(vec![]));
    assert!(ledger.keys().is_empty());
    assert!(journal.twins().next().is_none());
}

#[test]
fn remove_of_an_unknown_key_forgets_a_leftover_record() {
    let (_scratch, mut journal, mut ledger) = rig();
    put_up(&mut journal, 9, MODE_720, 3);

    ledger.remove(&mut journal, TwinKey(9)).unwrap();

    assert!(journal.twin(9).is_none());
    assert_eq!(ledger.backend().disk(), Some(vec![]));
    assert!(ledger.backend().calls().is_empty());
    // With no record and no lane, a second remove is a plain no-op.
    ledger.remove(&mut journal, TwinKey(9)).unwrap();
}

#[test]
fn a_failed_removing_save_keeps_the_lane_and_a_retry_removes_it() {
    let (scratch, mut journal, mut ledger) = rig();
    ledger.add(&mut journal, MODE_720).unwrap();
    break_disk(&scratch.journal_path());

    assert!(matches!(
        ledger.remove(&mut journal, TwinKey(1)),
        Err(TwinError::Journal(_))
    ));
    assert_eq!(
        ledger.backend().calls(),
        vec![
            Call::Open(1),
            Call::Add {
                lane: 1,
                mode: MODE_720
            }
        ]
    );
    assert!(ledger.display(TwinKey(1)).is_some());

    mend_disk(&scratch.journal_path());
    ledger.remove(&mut journal, TwinKey(1)).unwrap();
    assert_eq!(ledger.backend().calls().last(), Some(&Call::Close(1)));
    assert_eq!(ledger.backend().disk(), Some(vec![]));
    assert!(ledger.keys().is_empty());
}

#[test]
fn refused_remove_still_closes_and_forgets_then_reports_the_refusal() {
    let (_scratch, mut journal, mut ledger) = rig();
    ledger.add(&mut journal, MODE_720).unwrap();
    ledger.backend_mut().remove_error = Some(TwinError::Refused(Refusal::NotFound));

    assert_eq!(
        ledger.remove(&mut journal, TwinKey(1)),
        Err(TwinError::Refused(Refusal::NotFound))
    );
    let fake = ledger.backend();
    assert_eq!(fake.calls().last(), Some(&Call::Close(1)));
    assert_eq!(fake.disk_at(3), Some(vec![removing(1, MODE_720, 7)]));
    assert_eq!(fake.disk(), Some(vec![]));
    assert!(ledger.keys().is_empty());
}

#[test]
fn a_lost_lane_is_closed_without_a_remove_ioctl() {
    let (_scratch, mut journal, mut ledger) = rig();
    ledger.add(&mut journal, MODE_720).unwrap();
    ledger.backend_mut().lost.insert(1);

    ledger.remove(&mut journal, TwinKey(1)).unwrap();

    let fake = ledger.backend();
    assert_eq!(
        fake.calls(),
        vec![
            Call::Open(1),
            Call::Add {
                lane: 1,
                mode: MODE_720
            },
            Call::Close(1)
        ]
    );
    assert_eq!(fake.disk(), Some(vec![]));
    assert!(ledger.keys().is_empty());
}

#[test]
fn a_lost_lane_skips_the_path_gone_wait() {
    let (_scratch, mut journal, mut ledger) = rig();
    ledger.add(&mut journal, MODE_720).unwrap();
    ledger.backend_mut().lost.insert(1);
    let before = ledger.backend().now_ms();

    ledger.remove(&mut journal, TwinKey(1)).unwrap();

    // The fake clock moves only when the ledger sleeps, so an unchanged clock means no poll.
    assert_eq!(ledger.backend().now_ms(), before);
    assert_eq!(ledger.backend().disk(), Some(vec![]));
    assert!(ledger.keys().is_empty());
}

#[test]
fn resize_removes_then_adds_on_the_same_lane_under_a_new_monitor_id() {
    let (_scratch, mut journal, mut ledger) = rig();
    ledger.add(&mut journal, MODE_720).unwrap();

    let display = ledger.resize(&mut journal, TwinKey(1), MODE_1080).unwrap();

    assert_eq!(display.key, TwinKey(1));
    assert_eq!(display.monitor_id, 8);
    assert_eq!(display.mode, MODE_1080);
    assert_eq!(display.rect, [0, 0, 1920, 1080]);
    assert_eq!(ledger.keys(), vec![TwinKey(1)]);
    assert_eq!(ledger.display(TwinKey(1)), Some(&display));

    let fake = ledger.backend();
    assert_eq!(
        fake.calls(),
        vec![
            Call::Open(1),
            Call::Add {
                lane: 1,
                mode: MODE_720
            },
            Call::Remove { lane: 1, id: 7 },
            Call::Add {
                lane: 1,
                mode: MODE_1080
            }
        ]
    );
    assert_eq!(fake.disk_at(2), Some(vec![removing(1, MODE_720, 7)]));
    assert_eq!(fake.disk_at(3), Some(vec![adding(1, MODE_1080)]));
    assert_eq!(fake.disk(), Some(vec![up(1, MODE_1080, 8)]));
    assert_eq!(journal.twin(1), Some(&up(1, MODE_1080, 8)));
}

#[test]
fn resize_to_the_same_mode_is_a_no_op() {
    let (_scratch, mut journal, mut ledger) = rig();
    let display = ledger.add(&mut journal, MODE_720).unwrap();
    let calls_before = ledger.backend().calls();

    let same = ledger.resize(&mut journal, TwinKey(1), MODE_720).unwrap();

    assert_eq!(same, display);
    assert_eq!(ledger.backend().calls(), calls_before);
}

#[test]
fn resize_of_an_unknown_key_is_refused() {
    let (_scratch, mut journal, mut ledger) = rig();

    assert_eq!(
        ledger.resize(&mut journal, TwinKey(5), MODE_1080),
        Err(TwinError::UnknownKey)
    );
    assert!(ledger.backend().calls().is_empty());
}

#[test]
fn resize_add_refusal_closes_the_lane_and_forgets_the_record() {
    let (_scratch, mut journal, mut ledger) = rig();
    ledger.add(&mut journal, MODE_720).unwrap();
    ledger.backend_mut().add_error = Some(TwinError::Refused(Refusal::Capacity));

    assert_eq!(
        ledger.resize(&mut journal, TwinKey(1), MODE_1080),
        Err(TwinError::Refused(Refusal::Capacity))
    );
    let fake = ledger.backend();
    assert_eq!(fake.calls().last(), Some(&Call::Close(1)));
    assert_eq!(fake.disk_at(4), Some(vec![adding(1, MODE_1080)]));
    assert_eq!(fake.disk(), Some(vec![]));
    assert!(ledger.keys().is_empty());
}

#[test]
fn resize_whose_old_path_never_vanishes_closes_the_lane() {
    let (_scratch, mut journal, mut ledger) = rig();
    ledger.add(&mut journal, MODE_720).unwrap();
    ledger.backend_mut().vanish_on_remove = false;

    assert!(matches!(
        ledger.resize(&mut journal, TwinKey(1), MODE_1080),
        Err(TwinError::Timeout(_))
    ));
    let fake = ledger.backend();
    assert_eq!(
        fake.calls(),
        vec![
            Call::Open(1),
            Call::Add {
                lane: 1,
                mode: MODE_720
            },
            Call::Remove { lane: 1, id: 7 },
            Call::Close(1)
        ]
    );
    assert_eq!(fake.disk(), Some(vec![]));
    assert!(ledger.keys().is_empty());
}

#[test]
fn resize_without_a_new_desktop_path_stages_removing_for_the_new_id() {
    let (_scratch, mut journal, mut ledger) = rig();
    ledger.add(&mut journal, MODE_720).unwrap();
    ledger.backend_mut().appear_on_add = false;

    assert_eq!(
        ledger.resize(&mut journal, TwinKey(1), MODE_1080),
        Err(TwinError::NotOnDesktop)
    );
    let fake = ledger.backend();
    assert_eq!(fake.calls().last(), Some(&Call::Close(1)));
    assert_eq!(fake.disk_at(4), Some(vec![removing(1, MODE_1080, 8)]));
    assert_eq!(fake.disk(), Some(vec![]));
    assert!(ledger.keys().is_empty());
}

#[test]
fn reap_lost_closes_only_the_lost_lane_and_saves_its_removal() {
    let (_scratch, mut journal, mut ledger) = rig();
    ledger.add(&mut journal, MODE_720).unwrap();
    ledger.add(&mut journal, MODE_720).unwrap();
    ledger.backend_mut().lost.insert(1);

    let reaped = ledger.reap_lost(&mut journal).unwrap();

    assert_eq!(reaped, vec![TwinKey(1)]);
    assert_eq!(ledger.keys(), vec![TwinKey(2)]);
    let fake = ledger.backend();
    assert_eq!(fake.calls().len(), 5);
    assert_eq!(fake.calls().last(), Some(&Call::Close(1)));
    assert_eq!(fake.disk(), Some(vec![up(2, MODE_720, 8)]));
    assert!(journal.twin(1).is_none());
}

#[test]
fn close_all_closes_every_lane_and_leaves_the_records_for_recovery() {
    let (_scratch, mut journal, mut ledger) = rig();
    ledger.add(&mut journal, MODE_720).unwrap();
    ledger.add(&mut journal, MODE_720).unwrap();

    ledger.close_all();

    let fake = ledger.backend();
    assert_eq!(&fake.calls()[4..], &[Call::Close(1), Call::Close(2)]);
    assert!(ledger.keys().is_empty());
    assert_eq!(
        fake.disk(),
        Some(vec![up(1, MODE_720, 7), up(2, MODE_720, 8)])
    );
}

#[test]
fn two_new_paths_after_one_add_are_refused_as_ambiguous() {
    let mut fake = Fake::new(PathBuf::new());
    fake.paths = vec![path_for(1, MODE_720), path_for(2, MODE_720)];

    assert!(matches!(
        wait_new_path(&mut fake, &[], MODE_720),
        Err(TwinError::Protocol(_))
    ));
}

#[test]
fn recovery_clears_the_ledger_once_our_stale_paths_vanish() {
    let scratch = Scratch::new();
    let mut journal = JournalFile::open(&scratch.journal_path()).unwrap();
    let mut fake = Fake::new(scratch.journal_path());
    put_up(&mut journal, 1, MODE_720, 3);
    journal.put_twin(adding(2, MODE_1080)).unwrap();
    journal.save().unwrap();
    fake.paths.push(path_for(3, MODE_720));
    fake.vanish_at = Some(250);

    let startup = recover_ledger(&mut fake, &mut journal).unwrap();

    assert!(startup.driver_present);
    assert_eq!(startup.journaled, 2);
    assert_eq!(startup.stale_seen, 1);
    assert!(startup.waited_ms >= 250 && startup.waited_ms < u64::from(STALE_TIMEOUT_MS));
    assert_eq!(
        fake.calls(),
        vec![Call::Open(1), Call::List(1), Call::Close(1)]
    );
    assert_eq!(journal.twins().count(), 0);
    assert_eq!(fake.disk(), Some(vec![]));
}

#[test]
fn recovery_keeps_the_ledger_when_stale_twins_never_vanish() {
    let scratch = Scratch::new();
    let mut journal = JournalFile::open(&scratch.journal_path()).unwrap();
    let mut fake = Fake::new(scratch.journal_path());
    put_up(&mut journal, 1, MODE_720, 3);
    fake.paths.push(path_for(3, MODE_720));
    fake.paths.push(path_for(4, MODE_1080));

    assert_eq!(
        recover_ledger(&mut fake, &mut journal),
        Err(TwinError::Stale(2))
    );
    assert!(fake.calls().is_empty());
    assert!(fake.now >= u64::from(STALE_TIMEOUT_MS));
    assert_eq!(journal.twins().count(), 1);
    assert_eq!(fake.disk(), Some(vec![up(1, MODE_720, 3)]));
}

#[test]
fn recovery_refuses_when_a_fresh_handle_still_lists_a_twin() {
    let scratch = Scratch::new();
    let mut journal = JournalFile::open(&scratch.journal_path()).unwrap();
    let mut fake = Fake::new(scratch.journal_path());
    put_up(&mut journal, 1, MODE_720, 3);
    fake.list_reply = Some(3);

    assert!(matches!(
        recover_ledger(&mut fake, &mut journal),
        Err(TwinError::Protocol(_))
    ));
    assert_eq!(
        fake.calls(),
        vec![Call::Open(1), Call::List(1), Call::Close(1)]
    );
    assert_eq!(journal.twins().count(), 1);
    assert_eq!(fake.disk(), Some(vec![up(1, MODE_720, 3)]));
}
