//! Pure twin candidates and a parking journal; no VDD IOCTL, window move or DPI setter.
//! \[E] Virtual-screen rectangle coordinates are signed SHORT:
//! <https://learn.microsoft.com/en-us/windows/win32/gdi/the-virtual-screen>.
//! \[E] A DPI-aware moved window receives WM_DPICHANGED when its effective DPI changes:
//! <https://learn.microsoft.com/en-us/windows/win32/hidpi/wm-dpichanged>.
//! \[U] HWND reuse requires the complete (window, pid, process-start) identity.
//! \[P9f] Native ownership, IOCTLs, actual scaling, crash recovery and NTFS rename durability
//! remain unproven. The physical pointer can reach a twin; placement does not prevent that.
//! Callers admit their state directory and independently verify live identities and owned twins.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crosspane_platform::{Parked, ParkingKind, PlatformError};
use crosspane_types::geom::{PixelRect, SizeMm, euclid::point2};
use crosspane_types::id::{DisplayId, WindowId};
use serde::{Deserialize, Serialize};

pub const FORMAT: &str = "crosspane-win-twin-v1";
pub const JOURNAL_NAME: &str = "parking-twin.journal";
/// \[E] <https://learn.microsoft.com/en-us/windows/win32/gdi/the-virtual-screen>
pub const SHORT_MIN: i32 = -32768;
/// \[E] <https://learn.microsoft.com/en-us/windows/win32/gdi/the-virtual-screen>
pub const SHORT_MAX: i32 = 32767;
/// \[E] <https://learn.microsoft.com/en-us/windows/win32/hidpi/wm-dpichanged>
pub const BASE_DPI: f64 = 96.0;
/// \[E] <https://learn.microsoft.com/en-us/windows-hardware/drivers/display/overriding-monitor-edids>
pub const EDID_BYTES: usize = 128;
/// Candidate product-name marker; not an ownership credential or assigned manufacturer identity.
pub const EDID_MARKER: &[u8; 13] = b"CrosspaneTwin";
const MAX_BYTES: u64 = 1024 * 1024;
const MAX_ENTRIES: usize = 128;
/// Ledger capacity. The native client also caps open lanes separately (`cpd::MAX_OPENS`).
pub const MAX_TWINS: usize = 8;
/// Mirrors `model/twin.rs` (`MODES`, `MM_MIN`, `MM_MAX`); kept here so this file has no
/// dependency on the twin model's wording.
const TWIN_MODES: [(u32, u32); 2] = [(1280, 720), (1920, 1080)];
const TWIN_MM_MIN: u32 = 10;
const TWIN_MM_MAX: u32 = 2_000;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// A complete physical RECT [left, top, right, bottom]; DPI is a supplied observation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Original {
    pub rect_physical: [i32; 4],
    pub monitor_path: String,
    pub show: Show,
    pub dpi: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Show {
    Normal,
    Maximized,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Twin {
    pub serial: u32,
    pub device_path: Option<String>,
    pub mode: (u32, u32),
    pub refresh_millihz: u32,
    pub dpi: u32,
    pub origin: (i32, i32),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub window: u64,
    pub pid: u32,
    pub process_start: u64,
    pub phase: Phase,
    pub original: Original,
    pub twin: Twin,
}

/// Journaled precedes creating the twin; TwinUp precedes any possibly interrupted window move.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Phase {
    Journaled,
    TwinUp,
    Moved,
}

/// Lifecycle of one twin in the ledger. `Adding` has no monitor id yet; `Up` and `Removing` do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TwinPhase {
    Adding,
    Up,
    Removing,
}

/// One ledger record for a twin lane. `mode` and `size_mm` are the requested geometry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TwinRecord {
    pub key: u32,
    pub phase: TwinPhase,
    pub mode: (u32, u32),
    pub size_mm: (u32, u32),
    pub monitor_id: Option<u32>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    format: String,
    entries: Vec<Entry>,
    /// Absent when empty, so a document without twins keeps its pre-ledger bytes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    twins: Vec<TwinRecord>,
}

/// Edits are staged in memory: save must succeed before a caller performs a native mutation.
#[derive(Debug)]
pub struct JournalFile {
    path: PathBuf,
    entries: BTreeMap<u64, Entry>,
    twins: BTreeMap<u32, TwinRecord>,
}

impl JournalFile {
    pub fn open(path: &Path) -> Result<Self, PlatformError> {
        let mut bytes = Vec::new();
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    path: path.to_owned(),
                    entries: BTreeMap::new(),
                    twins: BTreeMap::new(),
                });
            }
            Err(error) => return Err(io_error(error)),
        };
        if !file.metadata().map_err(io_error)?.is_file() {
            return Err(invalid());
        }
        file.take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(invalid());
        }
        let document: Document = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        if document.format != FORMAT
            || document.entries.len() > MAX_ENTRIES
            || document.twins.len() > MAX_TWINS
        {
            return Err(invalid());
        }
        let mut journal = Self {
            path: path.to_owned(),
            entries: BTreeMap::new(),
            twins: BTreeMap::new(),
        };
        for entry in document.entries {
            journal.insert(entry)?;
        }
        for record in document.twins {
            let key = record.key;
            if !valid_twin(&record) || journal.twins.insert(key, record).is_some() {
                return Err(invalid());
            }
        }
        Ok(journal)
    }

    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values()
    }

    pub fn insert(&mut self, entry: Entry) -> Result<(), PlatformError> {
        validate(&entry)?;
        if self.entries.len() >= MAX_ENTRIES
            || self.entries.contains_key(&entry.window)
            || self
                .entries
                .values()
                .any(|old| old.twin.serial == entry.twin.serial)
        {
            return Err(invalid());
        }
        self.entries.insert(entry.window, entry);
        Ok(())
    }

    pub fn remove(&mut self, window: u64) -> Option<Entry> {
        self.entries.remove(&window)
    }

    /// An admitted device path is supplied at TwinUp and remains stable for this entry.
    pub fn set_phase(
        &mut self,
        window: u64,
        phase: Phase,
        device_path: Option<String>,
    ) -> Result<(), PlatformError> {
        let old = self.entries.get(&window).ok_or(PlatformError::NotFound)?;
        if phase < old.phase
            || old
                .twin
                .device_path
                .as_ref()
                .is_some_and(|path| Some(path) != device_path.as_ref())
        {
            return Err(invalid());
        }
        let mut next = old.clone();
        next.phase = phase;
        next.twin.device_path = device_path;
        validate(&next)?;
        self.entries.insert(window, next);
        Ok(())
    }

    /// Complete compact JSON, exclusive same-directory temp, sync, then one rename.
    /// \[P9f] Windows replacement/open-handle and power-loss semantics require native verification.
    pub fn save(&self) -> Result<(), PlatformError> {
        let bytes = serde_json::to_vec(&Document {
            format: FORMAT.to_owned(),
            entries: self.entries.values().cloned().collect(),
            twins: self.twins.values().cloned().collect(),
        })
        .map_err(|_| invalid())?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(invalid());
        }
        let (temp, mut file) = self.temporary()?;
        let result = (|| {
            file.write_all(&bytes).map_err(io_error)?;
            file.sync_all().map_err(io_error)?;
            drop(file);
            fs::rename(&temp, &self.path).map_err(io_error)
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }

    fn temporary(&self) -> Result<(PathBuf, File), PlatformError> {
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let name = self.path.file_name().ok_or_else(invalid)?;
        for _ in 0..32 {
            let mut name = name.to_os_string();
            name.push(format!(
                ".{}.{}.tmp",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            let path = parent.join(name);
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => return Ok((path, file)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(io_error(error)),
            }
        }
        Err(invalid())
    }
}

/// Twin ledger. Like entries, edits are staged in memory and `save` makes them durable.
impl JournalFile {
    /// Records in key order.
    pub fn twins(&self) -> impl Iterator<Item = &TwinRecord> {
        self.twins.values()
    }

    pub fn twin(&self, key: u32) -> Option<&TwinRecord> {
        self.twins.get(&key)
    }

    /// Stages one record. It must be valid and a permitted transition from the current record
    /// (see [`twin_transition_allowed`]); a new key also needs spare capacity.
    pub fn put_twin(&mut self, record: TwinRecord) -> Result<(), PlatformError> {
        let is_new = !self.twins.contains_key(&record.key);
        if !twin_transition_allowed(self.twins.get(&record.key), &record)
            || (is_new && self.twins.len() >= MAX_TWINS)
        {
            return Err(invalid());
        }
        self.twins.insert(record.key, record);
        Ok(())
    }

    /// Stages the removal of a record; the caller saves afterwards.
    pub fn forget_twin(&mut self, key: u32) -> Option<TwinRecord> {
        self.twins.remove(&key)
    }

    pub fn clear_twins(&mut self) {
        self.twins.clear();
    }
}

/// Whether a twin may move from `old` (`None` for a new key) to `new`. Allowed moves are
/// none → `Adding`, `Adding` → `Up` and `Up` → `Removing` (same mode, millimetres and id),
/// `Removing` → `Adding` (resize, any mode), and an identical record (idempotent).
pub fn twin_transition_allowed(old: Option<&TwinRecord>, new: &TwinRecord) -> bool {
    if !valid_twin(new) {
        return false;
    }
    match old {
        None => new.phase == TwinPhase::Adding,
        Some(old) if old == new => true,
        Some(old) if old.key != new.key => false,
        Some(old) => match (old.phase, new.phase) {
            (TwinPhase::Adding, TwinPhase::Up) => {
                old.mode == new.mode && old.size_mm == new.size_mm
            }
            (TwinPhase::Up, TwinPhase::Removing) => {
                old.mode == new.mode
                    && old.size_mm == new.size_mm
                    && old.monitor_id == new.monitor_id
            }
            (TwinPhase::Removing, TwinPhase::Adding) => true,
            _ => false,
        },
    }
}

/// Nonzero key, a known mode, millimetres within bounds, and a monitor id exactly when the
/// phase has one (`Adding` has none; `Up` and `Removing` have a nonzero one).
fn valid_twin(record: &TwinRecord) -> bool {
    let (width_mm, height_mm) = record.size_mm;
    record.key != 0
        && TWIN_MODES.contains(&record.mode)
        && (TWIN_MM_MIN..=TWIN_MM_MAX).contains(&width_mm)
        && (TWIN_MM_MIN..=TWIN_MM_MAX).contains(&height_mm)
        && match (record.phase, record.monitor_id) {
            (TwinPhase::Adding, None) => true,
            (TwinPhase::Up | TwinPhase::Removing, Some(id)) => id != 0,
            _ => false,
        }
}

fn invalid() -> PlatformError {
    PlatformError::Backend("invalid twin journal or geometry; retained for inspection".into())
}

fn io_error(error: std::io::Error) -> PlatformError {
    PlatformError::Backend(format!("twin journal I/O: {error}"))
}

fn valid_rect(rect: [i32; 4]) -> bool {
    rect[0] < rect[2] && rect[1] < rect[3]
}

fn short_rect(rect: [i32; 4]) -> bool {
    valid_rect(rect)
        && rect[0] >= SHORT_MIN
        && rect[1] >= SHORT_MIN
        && rect[2] <= SHORT_MAX
        && rect[3] <= SHORT_MAX
}

fn valid_path(path: &str) -> bool {
    !path.is_empty() && path.len() <= 4096 && !path.chars().any(char::is_control)
}

fn validate(entry: &Entry) -> Result<(), PlatformError> {
    let t = &entry.twin;
    if entry.window == 0
        || entry.pid == 0
        || !valid_rect(entry.original.rect_physical)
        || !valid_path(&entry.original.monitor_path)
        || entry.original.dpi == 0
        || t.serial == 0
        || t.mode.0 == 0
        || t.mode.1 == 0
        || t.refresh_millihz == 0
        || t.dpi == 0
        || i64::from(t.origin.0) < i64::from(SHORT_MIN)
        || i64::from(t.origin.1) < i64::from(SHORT_MIN)
        || i64::from(t.origin.0) + i64::from(t.mode.0) > i64::from(SHORT_MAX)
        || i64::from(t.origin.1) + i64::from(t.mode.1) > i64::from(SHORT_MAX)
        || t.device_path
            .as_deref()
            .is_some_and(|path| !valid_path(path))
        || (entry.phase != Phase::Journaled && t.device_path.is_none())
    {
        return Err(invalid());
    }
    Ok(())
}

/// Complete edge-aligned empty-rectangle search, with half-open physical RECTs.
/// \[P9f] A candidate is not confirmation of the layout the native driver will actually choose.
pub fn twin_origin(
    virtual_screen: [i32; 4],
    mode: (u32, u32),
    taken: &[[i32; 4]],
) -> Option<(i32, i32)> {
    let (w, h) = (i64::from(mode.0), i64::from(mode.1));
    let blocks: Vec<_> = std::iter::once(virtual_screen)
        .chain(taken.iter().copied())
        .collect();
    if w == 0 || h == 0 || blocks.iter().any(|r| !short_rect(*r)) {
        return None;
    }
    let mut xs = vec![
        i64::from(virtual_screen[2]),
        i64::from(virtual_screen[0]) - w,
        i64::from(SHORT_MIN),
    ];
    let mut ys = vec![
        i64::from(virtual_screen[1]),
        i64::from(virtual_screen[3]),
        i64::from(SHORT_MIN),
    ];
    for r in &blocks {
        xs.extend([i64::from(r[2]), i64::from(r[0]) - w]);
        ys.extend([i64::from(r[3]), i64::from(r[1]) - h]);
    }
    for x in xs {
        for &y in &ys {
            if x < i64::from(SHORT_MIN)
                || y < i64::from(SHORT_MIN)
                || x + w > i64::from(SHORT_MAX)
                || y + h > i64::from(SHORT_MAX)
                || blocks.iter().any(|r| {
                    x < i64::from(r[2])
                        && x + w > i64::from(r[0])
                        && y < i64::from(r[3])
                        && y + h > i64::from(r[1])
                })
            {
                continue;
            }
            return Some((x as i32, y as i32));
        }
    }
    None
}

/// Desired physical size only; EDID centimetre/millimetre quantization can alter actual scaling.
/// \[P9f] Windows must confirm the DPI it chose; no undocumented DPI setter is used.
pub fn twin_size_mm(mode: (u32, u32), wanted_scale: f64) -> Result<SizeMm, PlatformError> {
    let size = SizeMm::new(
        (f64::from(mode.0) * 25.4 / (BASE_DPI * wanted_scale)).round(),
        (f64::from(mode.1) * 25.4 / (BASE_DPI * wanted_scale)).round(),
    );
    if !wanted_scale.is_finite()
        || wanted_scale <= 0.0
        || !size.width.is_finite()
        || !size.height.is_finite()
        || !(10.0..=2550.0).contains(&size.width)
        || !(10.0..=2550.0).contains(&size.height)
    {
        return Err(invalid());
    }
    Ok(size)
}

/// EDID 1.4 candidate with one bounded detailed timing, product-name marker and serial.
/// Layout/units: VESA E-EDID A2 tables 3.21/3.22/3.40:
/// <https://glenwing.github.io/docs/VESA-EEDID-A2.pdf>.
/// \[P9f] The provisional CPN manufacturer code, timing and scaling need driver validation.
pub fn twin_edid(
    serial: u32,
    mode: (u32, u32),
    refresh_millihz: u32,
    size_mm: SizeMm,
) -> Result<[u8; EDID_BYTES], PlatformError> {
    let (w, h) = mode;
    if serial == 0
        || w == 0
        || h == 0
        || w > 4095
        || h > 4095
        || refresh_millihz == 0
        || !size_mm.width.is_finite()
        || !size_mm.height.is_finite()
        || !(10.0..=2550.0).contains(&size_mm.width)
        || !(10.0..=2550.0).contains(&size_mm.height)
    {
        return Err(invalid());
    }
    let clock = (u64::from(w + 160) * u64::from(h + 45) * u64::from(refresh_millihz) + 5_000_000)
        / 10_000_000;
    let clock = u16::try_from(clock)
        .ok()
        .filter(|v| *v != 0)
        .ok_or_else(invalid)?;
    let (mmw, mmh) = (size_mm.width.round() as u16, size_mm.height.round() as u16);
    let mut e = [0u8; EDID_BYTES];
    e[..8].copy_from_slice(&[0, 255, 255, 255, 255, 255, 255, 0]);
    e[8..12].copy_from_slice(&[0x0e, 0x0e, 1, 0]);
    e[12..16].copy_from_slice(&serial.to_le_bytes());
    e[17..25].copy_from_slice(&[
        36,
        1,
        4,
        0x80,
        ((mmw + 5) / 10) as u8,
        ((mmh + 5) / 10) as u8,
        120,
        6,
    ]);
    e[25..35].copy_from_slice(&[0xee, 0x91, 0xa3, 0x54, 0x4c, 0x99, 0x26, 0x0f, 0x50, 0x54]);
    e[38..54].fill(1);
    e[54..56].copy_from_slice(&clock.to_le_bytes());
    e[56..62].copy_from_slice(&[
        w as u8,
        160,
        ((w >> 8) as u8) << 4,
        h as u8,
        45,
        ((h >> 8) as u8) << 4,
    ]);
    e[62..66].copy_from_slice(&[48, 32, 0x36, 0]);
    e[66..69].copy_from_slice(&[
        mmw as u8,
        mmh as u8,
        ((mmw >> 8) as u8) << 4 | (mmh >> 8) as u8,
    ]);
    e[71] = 0x1e;
    e[75] = 0xfc;
    e[77..90].copy_from_slice(EDID_MARKER);
    e[93] = 0xff;
    e[95..108].fill(b' ');
    let serial_text = format!("{serial:08X}\n");
    e[95..104].copy_from_slice(serial_text.as_bytes());
    e[111] = 0x10;
    e[127] = e[..127]
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte))
        .wrapping_neg();
    Ok(e)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryStep {
    RestoreWindow { window: u64, original: Original },
    RemoveTwin { serial: u32 },
    Forget(u64),
}

/// Live tuples carry (window, pid, process-start, current physical RECT) from one native snapshot.
/// Restore only an unambiguous matching window still intersecting its twin; Journaled never moves.
/// Execute dependent removal/forget steps only after successful restore; recheck native ownership.
/// \[P9f] twins are independently verified Crosspane twins, not arbitrary serial lookalikes.
pub fn recovery_plan(
    entries: &[Entry],
    live: &[(u64, u32, u64, [i32; 4])],
    twins: &[Twin],
) -> Vec<RecoveryStep> {
    let mut steps = Vec::new();
    let mut removed = BTreeSet::new();
    for entry in entries {
        let matching: Vec<_> = live
            .iter()
            .filter(|identity| identity.0 == entry.window)
            .collect();
        let parked = match matching.as_slice() {
            [current] => {
                let rect = current.3;
                (current.0, current.1, current.2) == (entry.window, entry.pid, entry.process_start)
                    && valid_rect(rect)
                    && i64::from(rect[0])
                        < i64::from(entry.twin.origin.0) + i64::from(entry.twin.mode.0)
                    && i64::from(rect[1])
                        < i64::from(entry.twin.origin.1) + i64::from(entry.twin.mode.1)
                    && rect[2] > entry.twin.origin.0
                    && rect[3] > entry.twin.origin.1
            }
            _ => false,
        };
        if entry.phase != Phase::Journaled && parked {
            steps.push(RecoveryStep::RestoreWindow {
                window: entry.window,
                original: entry.original.clone(),
            });
        }
        if removed.insert(entry.twin.serial) {
            steps.push(RecoveryStep::RemoveTwin {
                serial: entry.twin.serial,
            });
        }
        steps.push(RecoveryStep::Forget(entry.window));
    }
    for twin in twins {
        if removed.insert(twin.serial) {
            steps.push(RecoveryStep::RemoveTwin {
                serial: twin.serial,
            });
        }
    }
    steps
}

/// Clip physical content to the twin, then translate to twin-local device pixels.
/// The supplied window identity is retained verbatim in the frozen Parked result.
pub fn parked_geometry(
    window: WindowId,
    window_rect: [i32; 4],
    twin_rect: [i32; 4],
    display: DisplayId,
) -> Result<Parked, PlatformError> {
    if !valid_rect(window_rect) || !short_rect(twin_rect) {
        return Err(invalid());
    }
    let edges = [
        i64::from(window_rect[0].max(twin_rect[0])) - i64::from(twin_rect[0]),
        i64::from(window_rect[1].max(twin_rect[1])) - i64::from(twin_rect[1]),
        i64::from(window_rect[2].min(twin_rect[2])) - i64::from(twin_rect[0]),
        i64::from(window_rect[3].min(twin_rect[3])) - i64::from(twin_rect[1]),
    ];
    if edges[0] >= edges[2] || edges[1] >= edges[3] {
        return Err(invalid());
    }
    Ok(Parked {
        window,
        kind: ParkingKind::Twin,
        display,
        content: PixelRect::new(
            point2(edges[0] as i32, edges[1] as i32),
            point2(edges[2] as i32, edges[3] as i32),
        ),
        fullscreen: false,
    })
}
