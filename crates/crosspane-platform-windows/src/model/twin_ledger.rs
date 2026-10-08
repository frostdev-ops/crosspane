//! Twin ledger orchestration (WP-W3.1b): the order of journal saves, IOCTLs and lane closes. Pure
//! logic; the native client supplies a [`TwinBackend`] and makes no ordering decisions itself.
//!
//! Rules enforced here:
//! - A record is saved before any IOCTL or handle close that adds or removes its twin.
//! - A failed save issues no IOCTL. The journal stages changes in memory, so a failed save leaves
//!   the record staged. Each path then forgets it or keeps it for a retry.
//! - Closing a lane retires its twin. A failed operation after its lane opens closes the lane and
//!   forgets the record, except a failed `remove` save, which keeps the lane for a retry. The
//!   journal on disk still covers the twin until the final save.

use std::collections::BTreeMap;
use std::fmt;

use crosspane_platform::PlatformError;

use super::cpd::MAX_OPENS;
use super::journal::{JournalFile, MAX_TWINS, TwinPhase, TwinRecord};
use super::twin::{
    ADD_TIMEOUT_MS, LIST_TIMEOUT_MS, NEW_PATH_TIMEOUT_MS, OwnPath, PATH_GONE_TIMEOUT_MS,
    PATH_POLL_MS, PathWait, REMOVE_TIMEOUT_MS, Refusal, STALE_POLL_MS, STALE_TIMEOUT_MS,
    TwinDisplay, TwinError, TwinKey, TwinMode, same_path, select_new_path,
};

/// The OS side of the ledger. Each call is one bounded native operation; the ledger decides when
/// to make it.
pub trait TwinBackend {
    /// One open control handle. A lane carries at most one twin.
    type Lane;
    /// Opens a control handle. Its heartbeat lease starts here.
    fn open_lane(&mut self) -> Result<Self::Lane, TwinError>;
    /// Adds a twin on this lane and returns its monitor id.
    fn add(&mut self, lane: &Self::Lane, mode: TwinMode, timeout_ms: u32)
    -> Result<u32, TwinError>;
    /// Removes the twin with this monitor id from this lane.
    fn remove(
        &mut self,
        lane: &Self::Lane,
        monitor_id: u32,
        timeout_ms: u32,
    ) -> Result<(), TwinError>;
    /// Lists this handle's own twin, if any.
    fn list(&mut self, lane: &Self::Lane, timeout_ms: u32) -> Result<Option<u32>, TwinError>;
    /// Closes the handle. The driver retires the lane's twin.
    fn close_lane(&mut self, lane: Self::Lane);
    /// True once the lane's heartbeat has failed.
    fn lane_lost(&self, lane: &Self::Lane) -> bool;
    /// Our own adapter's active paths.
    fn own_paths(&mut self) -> Result<Vec<OwnPath>, TwinError>;
    /// The DPI of the desktop monitor at this rect.
    fn dpi(&mut self, rect: [i32; 4]) -> Result<u32, TwinError>;
    /// A monotonic clock in milliseconds.
    fn now_ms(&self) -> u64;
    /// Sleeps for at least this many milliseconds.
    fn sleep_ms(&mut self, ms: u32);
}

/// What startup recovery found and did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TwinStartup {
    /// The driver answered, so recovery ran against it.
    pub driver_present: bool,
    /// Ledger records found before they were cleared.
    pub journaled: usize,
    /// The largest number of our own twins seen at once while waiting for them to vanish.
    pub stale_seen: usize,
    /// How long the wait for stale twins took.
    pub waited_ms: u64,
}

/// Live lanes, one per twin. Each call takes the journal and saves it at the points the ordering
/// rules need.
pub struct TwinLedger<B: TwinBackend> {
    backend: B,
    lanes: BTreeMap<TwinKey, (B::Lane, TwinDisplay)>,
    next: u32,
}

impl<B: TwinBackend> fmt::Debug for TwinLedger<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TwinLedger")
            .field("keys", &self.keys())
            .finish_non_exhaustive()
    }
}

impl<B: TwinBackend> TwinLedger<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            lanes: BTreeMap::new(),
            next: 1,
        }
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    pub fn display(&self, key: TwinKey) -> Option<&TwinDisplay> {
        self.lanes.get(&key).map(|(_, display)| display)
    }

    pub fn keys(&self) -> Vec<TwinKey> {
        self.lanes.keys().copied().collect()
    }

    /// Adds a twin. Journal `Adding`, open a lane, ADD, journal `Up`, then wait for its display.
    /// Any failure after the lane opens closes it and forgets the record.
    pub fn add(
        &mut self,
        journal: &mut JournalFile,
        mode: TwinMode,
    ) -> Result<TwinDisplay, TwinError> {
        if self.lanes.len() >= MAX_OPENS {
            return Err(TwinError::Refused(Refusal::Capacity));
        }
        let key = self.allocate(journal)?;
        if let Err(error) = stage(journal, twin_record(key, mode, TwinPhase::Adding, None)) {
            // Nothing reached disk, so memory must match disk again.
            journal.forget_twin(key.0);
            return Err(error);
        }
        let lane = match self.backend.open_lane() {
            Ok(lane) => lane,
            Err(error) => {
                let _ = forget_and_save(journal, key);
                return Err(error);
            }
        };
        let before = match self.backend.own_paths() {
            Ok(paths) => paths,
            Err(error) => {
                let _ = retire(&mut self.backend, journal, key, lane);
                return Err(error);
            }
        };
        let id = match self.backend.add(&lane, mode, ADD_TIMEOUT_MS) {
            Ok(id) => id,
            Err(error) => {
                let _ = retire(&mut self.backend, journal, key, lane);
                return Err(error);
            }
        };
        match complete_add(&mut self.backend, journal, key, mode, id, &before) {
            Ok(display) => {
                self.lanes.insert(key, (lane, display.clone()));
                Ok(display)
            }
            Err(error) => {
                let _ = retire(&mut self.backend, journal, key, lane);
                Err(error)
            }
        }
    }

    /// Changes a twin's mode: REMOVE, then ADD on the same lane, which gives a new monitor id.
    /// An unchanged mode is a no-op. Any failure closes the lane and forgets the record.
    pub fn resize(
        &mut self,
        journal: &mut JournalFile,
        key: TwinKey,
        mode: TwinMode,
    ) -> Result<TwinDisplay, TwinError> {
        let old = self.display(key).cloned().ok_or(TwinError::UnknownKey)?;
        if old.mode == mode {
            return Ok(old);
        }
        let (lane, _) = self.lanes.remove(&key).ok_or(TwinError::UnknownKey)?;
        match replace_twin(&mut self.backend, journal, key, &old, mode, &lane) {
            Ok(display) => {
                self.lanes.insert(key, (lane, display.clone()));
                Ok(display)
            }
            Err(error) => {
                let _ = retire(&mut self.backend, journal, key, lane);
                Err(error)
            }
        }
    }

    /// Removes a twin: journal `Removing`, REMOVE, wait for its path to go, close, forget.
    /// Removing an unknown key is `Ok`, after forgetting any leftover record. A failed journal
    /// save keeps the lane and sends no IOCTL, so the caller can retry. A refused REMOVE still
    /// closes the lane and forgets the record, then reports the refusal.
    pub fn remove(&mut self, journal: &mut JournalFile, key: TwinKey) -> Result<(), TwinError> {
        let Some((lane, display)) = self.lanes.remove(&key) else {
            return forget_stale(journal, key);
        };
        let removing = twin_record(
            key,
            display.mode,
            TwinPhase::Removing,
            Some(display.monitor_id),
        );
        if let Err(error) = stage(journal, removing) {
            self.lanes.insert(key, (lane, display));
            return Err(error);
        }
        // A lost lane takes no REMOVE. The driver retires its twin when the handle closes, so there
        // is no path to wait for.
        if self.backend.lane_lost(&lane) {
            return retire(&mut self.backend, journal, key, lane);
        }
        // The path must be read before REMOVE makes it disappear. Best effort.
        let path = self
            .backend
            .own_paths()
            .ok()
            .and_then(|paths| find_path(&paths, &display));
        if let Err(error) = self
            .backend
            .remove(&lane, display.monitor_id, REMOVE_TIMEOUT_MS)
        {
            let _ = retire(&mut self.backend, journal, key, lane);
            return Err(error);
        }
        if let Some(path) = path {
            let _ = wait_path_gone(&mut self.backend, &path);
        }
        retire(&mut self.backend, journal, key, lane)
    }

    /// The fast rollback: stage `Removing` if the twin is `Up`, save, close, forget, save. The
    /// lane is closed even when the staging save fails, because the saved record still covers
    /// the twin.
    pub fn discard(&mut self, journal: &mut JournalFile, key: TwinKey) -> Result<(), TwinError> {
        let Some((lane, _)) = self.lanes.remove(&key) else {
            return forget_stale(journal, key);
        };
        retire(&mut self.backend, journal, key, lane)
    }

    /// Closes and forgets every lane whose heartbeat has failed, then saves once. No `Removing`
    /// is staged: the driver retires a lost lane's twin, and the saved record covers it until
    /// then.
    pub fn reap_lost(&mut self, journal: &mut JournalFile) -> Result<Vec<TwinKey>, TwinError> {
        let lost: Vec<TwinKey> = self
            .lanes
            .iter()
            .filter(|(_, entry)| self.backend.lane_lost(&entry.0))
            .map(|(key, _)| *key)
            .collect();
        for key in &lost {
            if let Some((lane, _)) = self.lanes.remove(key) {
                self.backend.close_lane(lane);
                journal.forget_twin(key.0);
            }
        }
        if !lost.is_empty() {
            save(journal)?;
        }
        Ok(lost)
    }

    /// Closes every lane without touching the journal. The records stay on disk, so startup
    /// recovery finds and clears them.
    pub fn close_all(&mut self) {
        for (_, (lane, _)) in std::mem::take(&mut self.lanes) {
            self.backend.close_lane(lane);
        }
    }

    /// A key that is neither live nor journaled. The bound is above the most keys the ledger can
    /// hold, so a free key is always found.
    fn allocate(&mut self, journal: &JournalFile) -> Result<TwinKey, TwinError> {
        for _ in 0..=(MAX_TWINS + MAX_OPENS) {
            let key = TwinKey(self.next);
            self.next = self.next.checked_add(1).unwrap_or(1);
            if key.0 != 0 && !self.lanes.contains_key(&key) && journal.twin(key.0).is_none() {
                return Ok(key);
            }
        }
        Err(TwinError::Protocol("no free twin key"))
    }
}

/// Waits until a new twin path appears after an ADD. Exactly one new path of the mode's size is
/// the twin. No such path before the timeout is `NotOnDesktop`.
pub fn wait_new_path<B: TwinBackend>(
    backend: &mut B,
    before: &[OwnPath],
    mode: TwinMode,
) -> Result<OwnPath, TwinError> {
    let start = backend.now_ms();
    loop {
        let after = backend.own_paths()?;
        match select_new_path(before, &after, mode) {
            PathWait::Found(index) => {
                return after
                    .get(index)
                    .cloned()
                    .ok_or(TwinError::Protocol("new twin path index is out of range"));
            }
            PathWait::Ambiguous => {
                return Err(TwinError::Protocol("more than one new twin path appeared"));
            }
            PathWait::Pending => {}
        }
        if backend.now_ms().saturating_sub(start) >= u64::from(NEW_PATH_TIMEOUT_MS) {
            return Err(TwinError::NotOnDesktop);
        }
        backend.sleep_ms(PATH_POLL_MS);
    }
}

/// Waits until this path is gone from our own paths. At the bound it gives `Timeout`.
pub fn wait_path_gone<B: TwinBackend>(backend: &mut B, path: &OwnPath) -> Result<(), TwinError> {
    let start = backend.now_ms();
    loop {
        let paths = backend.own_paths()?;
        if !paths.iter().any(|candidate| same_path(candidate, path)) {
            return Ok(());
        }
        if backend.now_ms().saturating_sub(start) >= u64::from(PATH_GONE_TIMEOUT_MS) {
            return Err(TwinError::Timeout("twin display removal"));
        }
        backend.sleep_ms(PATH_POLL_MS);
    }
}

/// Startup recovery. Waits up to `STALE_TIMEOUT_MS` for our own paths to vanish. If they do, a
/// fresh handle must list nothing, and only then are the records cleared and saved. Twins that
/// never vanish give `Stale(n)` and the ledger is kept. A listed twin gives `Protocol`, and the
/// ledger is kept too.
pub fn recover_ledger<B: TwinBackend>(
    backend: &mut B,
    journal: &mut JournalFile,
) -> Result<TwinStartup, TwinError> {
    let journaled = journal.twins().count();
    let start = backend.now_ms();
    let mut stale_seen = 0;
    loop {
        let stale = backend.own_paths()?.len();
        stale_seen = stale_seen.max(stale);
        if stale == 0 {
            break;
        }
        if backend.now_ms().saturating_sub(start) >= u64::from(STALE_TIMEOUT_MS) {
            return Err(TwinError::Stale(stale));
        }
        backend.sleep_ms(STALE_POLL_MS);
    }
    let waited_ms = backend.now_ms().saturating_sub(start);
    let lane = backend.open_lane()?;
    let listed = backend.list(&lane, LIST_TIMEOUT_MS);
    backend.close_lane(lane);
    if listed?.is_some() {
        return Err(TwinError::Protocol("a fresh handle still lists a twin"));
    }
    journal.clear_twins();
    save(journal)?;
    Ok(TwinStartup {
        driver_present: true,
        journaled,
        stale_seen,
        waited_ms,
    })
}

/// Resize, after the lane is taken out. REMOVE the old twin, then ADD the new mode on the same
/// lane. Each step is journaled first. The caller retires the lane on any error.
fn replace_twin<B: TwinBackend>(
    backend: &mut B,
    journal: &mut JournalFile,
    key: TwinKey,
    old: &TwinDisplay,
    mode: TwinMode,
    lane: &B::Lane,
) -> Result<TwinDisplay, TwinError> {
    let removing = twin_record(key, old.mode, TwinPhase::Removing, Some(old.monitor_id));
    stage(journal, removing)?;
    let old_path = find_path(&backend.own_paths()?, old);
    backend.remove(lane, old.monitor_id, REMOVE_TIMEOUT_MS)?;
    if let Some(path) = old_path {
        wait_path_gone(backend, &path)?;
    }
    stage(journal, twin_record(key, mode, TwinPhase::Adding, None))?;
    let before = backend.own_paths()?;
    let id = backend.add(lane, mode, ADD_TIMEOUT_MS)?;
    complete_add(backend, journal, key, mode, id, &before)
}

/// The ADD succeeded. Journal `Up` with the monitor id, then find the new display and its DPI.
fn complete_add<B: TwinBackend>(
    backend: &mut B,
    journal: &mut JournalFile,
    key: TwinKey,
    mode: TwinMode,
    id: u32,
    before: &[OwnPath],
) -> Result<TwinDisplay, TwinError> {
    stage(journal, twin_record(key, mode, TwinPhase::Up, Some(id)))?;
    // `wait_new_path` reads the clock first thing, so this is the start of its wait.
    let start = backend.now_ms();
    let path = wait_new_path(backend, before, mode)?;
    let dpi = dpi_after_path(backend, path.rect, start)?;
    Ok(TwinDisplay {
        key,
        monitor_id: id,
        mode,
        gdi_name: path.gdi_name,
        monitor_path: path.monitor_path,
        rect: path.rect,
        dpi,
    })
}

/// The DPI at a new path's rect. Windows' monitor list can lag the path list, so `NotOnDesktop`
/// is polled every `PATH_POLL_MS` until `NEW_PATH_TIMEOUT_MS` after `start`, the start of the path
/// wait. Any other error is returned at once.
fn dpi_after_path<B: TwinBackend>(
    backend: &mut B,
    rect: [i32; 4],
    start: u64,
) -> Result<u32, TwinError> {
    loop {
        match backend.dpi(rect) {
            Err(TwinError::NotOnDesktop) => {}
            result => return result,
        }
        if backend.now_ms().saturating_sub(start) >= u64::from(NEW_PATH_TIMEOUT_MS) {
            return Err(TwinError::NotOnDesktop);
        }
        backend.sleep_ms(PATH_POLL_MS);
    }
}

/// Closes a lane and forgets its record. An `Up` record is first moved to `Removing` and saved,
/// so the journal says the twin is going before its handle closes. A failed staging save does
/// not stop the close: the record already on disk covers the twin, and startup recovery clears
/// it. The final save's result is returned.
fn retire<B: TwinBackend>(
    backend: &mut B,
    journal: &mut JournalFile,
    key: TwinKey,
    lane: B::Lane,
) -> Result<(), TwinError> {
    if let Some(up) = journal
        .twin(key.0)
        .filter(|record| record.phase == TwinPhase::Up)
        .cloned()
    {
        let removing = TwinRecord {
            phase: TwinPhase::Removing,
            ..up
        };
        if journal.put_twin(removing).is_ok() {
            let _ = journal.save();
        }
    }
    backend.close_lane(lane);
    journal.forget_twin(key.0);
    save(journal)
}

/// Forgets a record that has no live lane. Only a real removal needs a save.
fn forget_stale(journal: &mut JournalFile, key: TwinKey) -> Result<(), TwinError> {
    if journal.forget_twin(key.0).is_some() {
        save(journal)
    } else {
        Ok(())
    }
}

fn forget_and_save(journal: &mut JournalFile, key: TwinKey) -> Result<(), TwinError> {
    journal.forget_twin(key.0);
    save(journal)
}

/// Stages one record in memory, then saves. A failed save leaves the record staged, so the caller
/// must forget it or retry.
fn stage(journal: &mut JournalFile, record: TwinRecord) -> Result<(), TwinError> {
    journal.put_twin(record).map_err(journal_error)?;
    save(journal)
}

fn save(journal: &JournalFile) -> Result<(), TwinError> {
    journal.save().map_err(journal_error)
}

fn journal_error(error: PlatformError) -> TwinError {
    TwinError::Journal(error.to_string())
}

fn twin_record(
    key: TwinKey,
    mode: TwinMode,
    phase: TwinPhase,
    monitor_id: Option<u32>,
) -> TwinRecord {
    TwinRecord {
        key: key.0,
        phase,
        mode: (mode.width, mode.height),
        size_mm: (mode.width_mm, mode.height_mm),
        monitor_id,
    }
}

/// Our own path for a display. The monitor path and GDI name identify it, since the LUID and
/// target are not stored in the display.
fn find_path(paths: &[OwnPath], display: &TwinDisplay) -> Option<OwnPath> {
    paths
        .iter()
        .find(|path| {
            path.monitor_path
                .eq_ignore_ascii_case(&display.monitor_path)
                && path.gdi_name.eq_ignore_ascii_case(&display.gdi_name)
        })
        .cloned()
}
