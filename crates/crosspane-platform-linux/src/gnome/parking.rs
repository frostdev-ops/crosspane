//! M1 mirror parking on GNOME through the Shell bridge (WP-G2.3a, the reported fallback; the
//! VIRTUAL twin, M2, is WP-G2.4).
//!
//! The window stays where it is and stays visible on the source (`ParkingKind::Mirror`).
//!
//! - **Journal first** (04 §8 invariant 4). Before the first change to a window, its original
//!   frame rect (logical), its fullscreen state as far as the bridge reports it, and the bridge
//!   epoch are written to the JSON journal at `journal` (temp file with mode 0600, fsynced,
//!   renamed over the journal, directory fsynced). If that write fails, the window is not touched.
//!   An entry is retired only after its restore succeeded or the window is gone.
//! - **park(size, scale)**: the window's content is resized with `MoveResize` keeping its top-left,
//!   to `size / display scale` logical (the destination's `scale` is ignored: M1 renders at the
//!   source display's density). Returns `Parked{kind: Mirror, display, content}` with `content` =
//!   the frame rect converted to device pixels on its display (logical − display origin, × scale,
//!   rounded), `fullscreen` from the bridge. The "display" is the one containing the window's
//!   centre in the `displays` snapshot, or the nearest one when the centre is in a gap.
//! - **resize**: same as park for an already parked window (and `NotFound` for one that isn't
//!   parked). **set_fullscreen**: v1 of the bridge has no fullscreen request → `Unsupported`.
//!   **geometry**: fresh `ListWindows` read of a parked window.
//! - **restore / restore_at**: `MoveResize` back to the journaled rect (`restore_at`: same size,
//!   top-left at the requested point converted to logical, clamped into that display's logical
//!   bounds, in one request), then retire the entry. A window that no longer exists is `Ok`
//!   (retired). A `restore_at` whose display or origin can't be resolved restores in place.
//! - **recover**: entries whose epoch equals the bridge's current epoch are restored; entries from
//!   another epoch refer to windows that can't be identified any more and are retired with a
//!   warning (no window was moved off-screen by M1, so none is lost).
//! - Never minimizes and never closes a window: the private bridge seam below offers only
//!   `ListWindows` and `MoveResize`.
//!
//! # Decisions the bridge forces
//!
//! - `MoveResize` ends maximize and fullscreen in the Shell, and the v1 bridge can restore neither
//!   (it reports `fullscreen` but not maximize, and has no maximize or fullscreen request). So a
//!   window that is **fullscreen** when parked (or resized) is *not* resized: it is journaled and
//!   reported as it is (`fullscreen: true`, content = its frame). Restoring it leaves it
//!   fullscreen as long as it still is. A maximized window cannot be told from one that fills the
//!   work area: its first resize unmaximizes it, and restore puts back its rect, not the maximize
//!   state.
//! - `MoveResize` is a request: the app may refuse the size and the Shell constrains the result.
//!   After a resize the window is read back for up to 300 ms until it reports the wanted size, and
//!   the geometry it then reports is returned. A restore is retired once the Shell accepted the
//!   request; it is not read back.
//! - Window ids mean something only within one Shell epoch (an id of another epoch is unknown to
//!   the extension, and a restarted Shell may reuse ids), so every entry carries the epoch of the
//!   bridge that wrote it and every lookup is keyed by `(epoch, window id)`.
//!
//! # Journal format
//!
//! One JSON object, `{"version": 1, "entries": [...]}`, entries sorted by `(epoch, window)`:
//!
//! ```json
//! {"epoch": 8371940512, "window": 5, "x": 100, "y": 100, "width": 800, "height": 600,
//!  "fullscreen": false}
//! ```
//!
//! An unreadable or unrecognised journal (bad JSON, unknown version or field, duplicate key, a
//! frame the bridge would not accept) makes [`GnomeMirrorParking::new`] fail rather than being
//! discarded: it is the only record of the original frames.
//!
//! # Crash points
//!
//! A park is `list → write journal → MoveResize → read back`. A crash before the rename leaves the
//! old journal and an untouched window. A crash after the rename and before `MoveResize` leaves an
//! entry whose window is still at its journaled rect; `recover` finds nothing to move and retires
//! it. A crash after `MoveResize` leaves a resized window with an entry, which `recover` undoes. A
//! park that fails after the journal write is rolled back at once (the window is put back and the
//! entry retired), except when the failure is a timeout: the Shell may still carry out a request it
//! did not answer, so the entry stays for `recover` instead of being retired on a stale read. A
//! restore is `list → MoveResize → write journal`: a crash before the write repeats the restore on
//! the next `recover`, which is a no-op once the window is at its rect. The agent's `recover` runs
//! in a new process whose bridge has the same epoch if the Shell kept running, so the windows are
//! still identifiable; across a Shell restart no window survives anyway.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use crosspane_platform::{Parked, ParkingKind, PlatformError, WindowParking};
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::euclid::point2;
use crosspane_types::geom::{DisplayGeometry, PixelRect, PixelSize, PointDevice};
use crosspane_types::id::{DisplayId, WindowId};
use serde::{Deserialize, Serialize};

use super::shell::{ShellBridge, ShellWindow};
use crate::portal::eis::DisplaysFn;

/// The journal format version this module reads and writes.
const JOURNAL_VERSION: u32 = 1;
/// The largest window width or height the bridge's `MoveResize` accepts.
const MAX_EXTENT: i32 = 32_768;
/// The largest coordinate magnitude the bridge's `MoveResize` accepts.
const MAX_COORD: i32 = 1 << 20;
/// How long a resize waits for the window to report the size it was asked for, and how often it
/// looks.
const SETTLE: Settle = Settle {
    interval: Duration::from_millis(20),
    budget: Duration::from_millis(300),
};
const NO_FULLSCREEN: &str = "fullscreen through the GNOME Shell bridge v1";

/// In-place (M1) parking through the Shell bridge.
#[derive(Debug)]
pub struct GnomeMirrorParking {
    core: Core<ShellBridge>,
}

impl GnomeMirrorParking {
    /// Load the journal at `journal` (a missing file is empty; the directory is created with mode
    /// 0700 at the first write). Call [`WindowParking::recover`] before parking anything.
    pub fn new(
        bridge: ShellBridge,
        displays: DisplaysFn,
        journal: PathBuf,
    ) -> Result<GnomeMirrorParking, PlatformError> {
        Ok(GnomeMirrorParking {
            core: Core::open(bridge, displays, journal)?,
        })
    }
}

impl WindowParking for GnomeMirrorParking {
    fn park(
        &mut self,
        window: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError> {
        self.core.park(window, size, scale)
    }

    fn resize(
        &mut self,
        window: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError> {
        self.core.resize(window, size, scale)
    }

    fn set_fullscreen(&mut self, window: WindowId, fullscreen: bool) -> Result<(), PlatformError> {
        self.core.set_fullscreen(window, fullscreen)
    }

    fn geometry(&self, window: WindowId) -> Result<Parked, PlatformError> {
        self.core.geometry(window)
    }

    fn restore(&mut self, window: WindowId) -> Result<(), PlatformError> {
        self.core.restore(window)
    }

    fn restore_at(
        &mut self,
        window: WindowId,
        display: DisplayId,
        origin: PointDevice,
    ) -> Result<(), PlatformError> {
        self.core.restore_at(window, display, origin)
    }

    fn recover(&mut self) -> Result<Vec<WindowId>, PlatformError> {
        self.core.recover()
    }
}

/// The bridge calls parking makes. A private seam so that tests run without D-Bus; it deliberately
/// has no minimize and no close.
trait Shell: Send {
    fn epoch(&self) -> u64;
    fn list_windows(&self) -> Result<Vec<ShellWindow>, PlatformError>;
    fn move_resize(
        &self,
        id: u64,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<(), PlatformError>;
}

impl Shell for ShellBridge {
    fn epoch(&self) -> u64 {
        ShellBridge::epoch(self)
    }

    fn list_windows(&self) -> Result<Vec<ShellWindow>, PlatformError> {
        ShellBridge::list_windows(self)
    }

    fn move_resize(
        &self,
        id: u64,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<(), PlatformError> {
        ShellBridge::move_resize(self, id, x, y, width, height)
    }
}

#[derive(Clone, Copy, Debug)]
struct Settle {
    interval: Duration,
    budget: Duration,
}

/// Where `restore_at` was asked to put the window's top-left.
#[derive(Clone, Copy, Debug)]
struct Placement {
    display: DisplayId,
    origin: PointDevice,
}

/// All the parking logic, generic over the bridge so tests can drive it with a fake.
struct Core<S: Shell> {
    shell: S,
    /// The bridge's epoch: every entry this run writes or looks up carries it.
    epoch: u64,
    displays: DisplaysFn,
    path: PathBuf,
    journal: Journal,
    settle: Settle,
}

impl<S: Shell> fmt::Debug for Core<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Core")
            .field("epoch", &self.epoch)
            .field("journal", &self.path)
            .field("entries", &self.journal.len())
            .finish_non_exhaustive()
    }
}

impl<S: Shell> Core<S> {
    fn open(shell: S, displays: DisplaysFn, path: PathBuf) -> Result<Core<S>, PlatformError> {
        let journal = read_journal(&path)?;
        Ok(Core {
            epoch: shell.epoch(),
            shell,
            displays,
            path,
            journal,
            settle: SETTLE,
        })
    }

    fn key(&self, window: WindowId) -> Key {
        (self.epoch, window.0)
    }

    /// Writes `next` to disk and, only when that worked, makes it the current journal.
    fn commit(&mut self, next: Journal) -> Result<(), PlatformError> {
        write_journal(&self.path, &next)?;
        self.journal = next;
        Ok(())
    }

    fn retire(&mut self, key: Key) -> Result<(), PlatformError> {
        let next = self.journal.without(key);
        self.commit(next)
    }

    /// A fresh `ListWindows` read of one window (`None`: it is not listed).
    fn window(&self, id: u64) -> Result<Option<ShellWindow>, PlatformError> {
        Ok(self.shell.list_windows()?.into_iter().find(|w| w.id == id))
    }

    fn park(
        &mut self,
        window: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError> {
        if self.journal.contains(self.key(window)) {
            return self.resize(window, size, scale);
        }
        check_size(size)?;
        let current = self.window(window.0)?.ok_or(PlatformError::NotFound)?;
        let frame = Frame::of(&current);
        if !frame.is_valid() {
            return Err(backend("the window has no usable frame"));
        }
        // Everything that can be computed without touching the window is computed before the
        // journal write, so a refusal here leaves neither journal nor window changed.
        let displays = (self.displays)();
        let target = plan_resize(&current, size, &displays)?;
        parked(&current, &displays)?;
        let entry = Entry::new(self.epoch, window.0, frame, current.fullscreen);
        let next = self.journal.with(entry);
        self.commit(next)?;
        match self.apply(&current, target) {
            Ok(parked) => Ok(parked),
            Err(PlatformError::Timeout) => {
                // The Shell may still carry out the request it did not answer, so a read now
                // could show the old rect and a rollback could retire the entry too early. The
                // entry stays; if the window was not changed, `recover` finds nothing to undo.
                tracing::warn!(
                    window = window.0,
                    "mirror park timed out; journal retained for recovery"
                );
                Err(PlatformError::Timeout)
            }
            Err(error) => {
                // The window may or may not have changed. Put it back if it did; if that fails
                // too, the entry stays for `recover`.
                self.rollback(window.0);
                Err(error)
            }
        }
    }

    fn resize(
        &mut self,
        window: WindowId,
        size: PixelSize,
        _scale: f64,
    ) -> Result<Parked, PlatformError> {
        if !self.journal.contains(self.key(window)) {
            return Err(PlatformError::NotFound);
        }
        check_size(size)?;
        let current = self.window(window.0)?.ok_or(PlatformError::NotFound)?;
        let target = plan_resize(&current, size, &(self.displays)())?;
        self.apply(&current, target)
    }

    fn set_fullscreen(&mut self, window: WindowId, fullscreen: bool) -> Result<(), PlatformError> {
        let _ = (window, fullscreen);
        Err(PlatformError::Unsupported(NO_FULLSCREEN))
    }

    fn geometry(&self, window: WindowId) -> Result<Parked, PlatformError> {
        if !self.journal.contains(self.key(window)) {
            return Err(PlatformError::NotFound);
        }
        let current = self.window(window.0)?.ok_or(PlatformError::NotFound)?;
        parked(&current, &(self.displays)())
    }

    /// Carries out a planned resize (`None`: nothing to change) and reports what the window took.
    fn apply(&self, current: &ShellWindow, target: Option<Frame>) -> Result<Parked, PlatformError> {
        let after = match target {
            None => current.clone(),
            Some(target) => {
                self.shell.move_resize(
                    current.id,
                    target.x,
                    target.y,
                    target.width,
                    target.height,
                )?;
                self.settle(current.id, target)?
            }
        };
        parked(&after, &(self.displays)())
    }

    /// Reads the window back until it reports `target`'s size or the budget runs out; the Shell
    /// applies a `MoveResize` asynchronously and the app may refuse the size.
    fn settle(&self, id: u64, target: Frame) -> Result<ShellWindow, PlatformError> {
        let deadline = Instant::now() + self.settle.budget;
        loop {
            let window = self.window(id)?.ok_or(PlatformError::NotFound)?;
            if (window.width, window.height) == (target.width, target.height)
                || Instant::now() >= deadline
            {
                return Ok(window);
            }
            thread::sleep(self.settle.interval);
        }
    }

    fn rollback(&mut self, id: u64) {
        if let Err(error) = self.restore_with(id, None) {
            tracing::warn!(
                window = id,
                %error,
                "failed mirror park rollback; journal retained for recovery"
            );
        }
    }

    fn restore(&mut self, window: WindowId) -> Result<(), PlatformError> {
        self.restore_with(window.0, None).map(|_| ())
    }

    fn restore_at(
        &mut self,
        window: WindowId,
        display: DisplayId,
        origin: PointDevice,
    ) -> Result<(), PlatformError> {
        self.restore_with(window.0, Some(Placement { display, origin }))
            .map(|_| ())
    }

    /// Puts one window back (at `placement` when given and resolvable) and retires its entry.
    /// `Ok(true)`: the window exists and is back; `Ok(false)`: there was no entry, or the window
    /// is gone (and its entry is retired). Any bridge failure keeps the entry.
    fn restore_with(
        &mut self,
        id: u64,
        placement: Option<Placement>,
    ) -> Result<bool, PlatformError> {
        let key = (self.epoch, id);
        let Some(entry) = self.journal.get(key).copied() else {
            return Ok(false);
        };
        let Some(current) = self.window(id)? else {
            self.retire(key)?;
            return Ok(false);
        };
        // A window that was fullscreen and still is stays so: a move would end fullscreen, and
        // the bridge cannot bring it back.
        let untouched = entry.fullscreen && current.fullscreen;
        let target = match placement {
            Some(placement) if !untouched => {
                match place_frame(entry.frame(), placement, &(self.displays)()) {
                    Ok(placed) => placed,
                    Err(error) => {
                        tracing::warn!(
                            window = id,
                            %error,
                            "restore placement failed; restoring in place"
                        );
                        entry.frame()
                    }
                }
            }
            _ => entry.frame(),
        };
        if !untouched && (current.fullscreen || Frame::of(&current) != target) {
            match self
                .shell
                .move_resize(id, target.x, target.y, target.width, target.height)
            {
                Ok(()) => {}
                Err(PlatformError::NotFound) => {
                    self.retire(key)?;
                    return Ok(false);
                }
                Err(error) => return Err(error),
            }
        }
        self.retire(key)?;
        Ok(true)
    }

    fn recover(&mut self) -> Result<Vec<WindowId>, PlatformError> {
        let mut restored = Vec::new();
        let mut failure = None;
        for id in self.journal.windows_of(self.epoch) {
            match self.restore_with(id, None) {
                Ok(true) => restored.push(WindowId(id)),
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(window = id, %error, "mirror window not restored; journal retained");
                    let timed_out = matches!(error, PlatformError::Timeout);
                    failure.get_or_insert(error);
                    if timed_out {
                        // The bridge is not answering; do not wait 2 s per remaining window.
                        break;
                    }
                }
            }
        }
        // Entries of another epoch name windows that cannot be identified any more. M1 moved
        // nothing off-screen, so retiring them loses no window.
        let (current, stale) = self.journal.split_stale(self.epoch);
        if stale > 0 {
            tracing::warn!(
                count = stale,
                "retiring mirror journal entries from another Shell epoch"
            );
            if let Err(error) = self.commit(current) {
                tracing::warn!(%error, "could not retire stale mirror journal entries");
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(restored),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Geometry (pure)
// ---------------------------------------------------------------------------------------------

/// A window rect in the Shell's global logical coordinates, as the bridge reports and accepts it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Frame {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl Frame {
    fn of(window: &ShellWindow) -> Frame {
        Frame {
            x: window.x,
            y: window.y,
            width: window.width,
            height: window.height,
        }
    }

    /// Whether the bridge would accept this rect back in `MoveResize`.
    fn is_valid(self) -> bool {
        (1..=MAX_EXTENT).contains(&self.width)
            && (1..=MAX_EXTENT).contains(&self.height)
            && (-MAX_COORD..=MAX_COORD).contains(&self.x)
            && (-MAX_COORD..=MAX_COORD).contains(&self.y)
    }
}

fn check_size(size: PixelSize) -> Result<(), PlatformError> {
    if size.width == 0 || size.height == 0 {
        Err(backend("invalid parking size"))
    } else {
        Ok(())
    }
}

/// Whether conversions can use this display: a finite positive scale, a finite origin, a size.
fn usable(geometry: &DisplayGeometry) -> bool {
    geometry.scale.is_finite()
        && geometry.scale > 0.0
        && geometry.logical_origin.x.is_finite()
        && geometry.logical_origin.y.is_finite()
        && geometry.pixel_size.width >= 1
        && geometry.pixel_size.height >= 1
}

/// The display `frame` is on: the first one whose logical bounds contain the frame's centre, else
/// (the centre is in a gap or outside every display) the nearest one to the centre.
fn display_for(frame: Frame, displays: &[DisplayInfo]) -> Option<&DisplayInfo> {
    let cx = f64::from(frame.x) + f64::from(frame.width) / 2.0;
    let cy = f64::from(frame.y) + f64::from(frame.height) / 2.0;
    let mut nearest: Option<(&DisplayInfo, f64)> = None;
    for display in displays.iter().filter(|d| usable(&d.geometry)) {
        let bounds = display.geometry.logical_bounds();
        // Half-open: a centre on the right or bottom edge belongs to the next display.
        let dx = (bounds.min_x() - cx).max(cx - bounds.max_x()).max(0.0);
        let dy = (bounds.min_y() - cy).max(cy - bounds.max_y()).max(0.0);
        let contained = cx >= bounds.min_x()
            && cx < bounds.max_x()
            && cy >= bounds.min_y()
            && cy < bounds.max_y();
        if contained {
            return Some(display);
        }
        let distance = dx * dx + dy * dy;
        if nearest.is_none_or(|(_, best)| distance < best) {
            nearest = Some((display, distance));
        }
    }
    nearest.map(|(display, _)| display)
}

/// A logical frame as a device-pixel rect on the display `geometry` describes: origin
/// `(logical − display origin) × scale` and size `logical size × scale`, each rounded (the size
/// separately from the origin, so a window's content size does not wobble as it moves).
fn to_device(frame: Frame, geometry: &DisplayGeometry) -> Result<PixelRect, PlatformError> {
    let scale = geometry.scale;
    let axis = |position: i32, origin: f64, extent: i32| -> Result<(i32, i32), PlatformError> {
        let min = ((f64::from(position) - origin) * scale).round();
        let size = (f64::from(extent) * scale).round();
        let limit = f64::from(i32::MAX);
        if !(min.abs() <= limit && size >= 1.0 && size <= limit) {
            return Err(backend("invalid mirror content geometry"));
        }
        let (min, size) = (min as i32, size as i32);
        let max = min
            .checked_add(size)
            .ok_or_else(|| backend("mirror geometry overflow"))?;
        Ok((min, max))
    };
    let (min_x, max_x) = axis(frame.x, geometry.logical_origin.x, frame.width)?;
    let (min_y, max_y) = axis(frame.y, geometry.logical_origin.y, frame.height)?;
    Ok(PixelRect::new(point2(min_x, min_y), point2(max_x, max_y)))
}

/// A content size in device pixels as a logical window size on a display of `scale`, rounded.
fn logical_extent(size: PixelSize, scale: f64) -> Result<(i32, i32), PlatformError> {
    let axis = |pixels: u32| -> Result<i32, PlatformError> {
        let logical = (f64::from(pixels) / scale).round();
        if (1.0..=f64::from(MAX_EXTENT)).contains(&logical) {
            Ok(logical as i32)
        } else {
            Err(backend("parking size out of range"))
        }
    };
    Ok((axis(size.width)?, axis(size.height)?))
}

/// What a resize of `window` to `size` device pixels asks of the Shell: the window's top-left
/// kept, the size converted with the scale of the display it is on. `None`: nothing to change,
/// because the window is fullscreen (a move would end that) or already has the size.
fn plan_resize(
    window: &ShellWindow,
    size: PixelSize,
    displays: &[DisplayInfo],
) -> Result<Option<Frame>, PlatformError> {
    if window.fullscreen {
        return Ok(None);
    }
    let current = Frame::of(window);
    let display =
        display_for(current, displays).ok_or_else(|| backend("no display for the window"))?;
    let (width, height) = logical_extent(size, display.geometry.scale)?;
    let target = Frame {
        x: current.x,
        y: current.y,
        width,
        height,
    };
    Ok((target != current).then_some(target))
}

/// `Parked` for a listed window: its display and its frame in that display's device pixels.
fn parked(window: &ShellWindow, displays: &[DisplayInfo]) -> Result<Parked, PlatformError> {
    let frame = Frame::of(window);
    let display =
        display_for(frame, displays).ok_or_else(|| backend("no display for the window"))?;
    Ok(Parked {
        window: WindowId(window.id),
        kind: ParkingKind::Mirror,
        display: display.id,
        content: to_device(frame, &display.geometry)?,
        fullscreen: window.fullscreen,
    })
}

/// Where `restore_at` puts a window that was journaled at `original`: its size kept, its top-left
/// at `placement.origin` (device pixels on `placement.display`) converted to logical and clamped
/// so the window stays inside that display's logical bounds (as far as it fits). The bridge has no
/// work area, so the Shell's own constraints may still nudge the window.
fn place_frame(
    original: Frame,
    placement: Placement,
    displays: &[DisplayInfo],
) -> Result<Frame, PlatformError> {
    let display = displays
        .iter()
        .find(|d| d.id == placement.display && usable(&d.geometry))
        .ok_or(PlatformError::NotFound)?;
    let origin = placement.origin;
    if !origin.x.is_finite() || !origin.y.is_finite() {
        return Err(backend("non-finite placement origin"));
    }
    let at = display.geometry.device_to_logical(origin);
    let bounds = display.geometry.logical_bounds();
    let axis = |wanted: f64, low: f64, high: f64, extent: i32| -> Result<i32, PlatformError> {
        let min = low.ceil();
        // The highest top-left that keeps the far edge on the display; a window larger than the
        // display sits at its near edge.
        let max = (high.floor() - f64::from(extent)).max(min);
        let value = wanted.round().clamp(min, max);
        if value.abs() <= f64::from(MAX_COORD) {
            Ok(value as i32)
        } else {
            Err(backend(
                "placement outside the coordinates the bridge accepts",
            ))
        }
    };
    Ok(Frame {
        x: axis(at.x, bounds.min_x(), bounds.max_x(), original.width)?,
        y: axis(at.y, bounds.min_y(), bounds.max_y(), original.height)?,
        width: original.width,
        height: original.height,
    })
}

fn backend(error: impl fmt::Display) -> PlatformError {
    PlatformError::Backend(error.to_string())
}

// ---------------------------------------------------------------------------------------------
// Journal (pure state machine + atomic file write)
// ---------------------------------------------------------------------------------------------

/// `(Shell epoch, window id)`.
type Key = (u64, u64);

/// One parked window's original state. Fields are the journal's JSON fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    epoch: u64,
    window: u64,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    fullscreen: bool,
}

impl Entry {
    fn new(epoch: u64, window: u64, frame: Frame, fullscreen: bool) -> Entry {
        Entry {
            epoch,
            window,
            x: frame.x,
            y: frame.y,
            width: frame.width,
            height: frame.height,
            fullscreen,
        }
    }

    fn key(&self) -> Key {
        (self.epoch, self.window)
    }

    fn frame(&self) -> Frame {
        Frame {
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
        }
    }
}

/// The journal file's JSON.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalFile {
    version: u32,
    entries: Vec<Entry>,
}

/// The set of parked windows. Every change returns a new value, so the caller can persist it
/// before it replaces the current one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Journal {
    entries: BTreeMap<Key, Entry>,
}

impl Journal {
    fn parse(bytes: &[u8]) -> Result<Journal, PlatformError> {
        let file: JournalFile = serde_json::from_slice(bytes)
            .map_err(|e| backend(format!("mirror parking journal is unreadable: {e}")))?;
        if file.version != JOURNAL_VERSION {
            return Err(backend("mirror parking journal has an unknown version"));
        }
        let mut entries = BTreeMap::new();
        for entry in file.entries {
            if !entry.frame().is_valid() {
                return Err(backend("mirror parking journal has an invalid frame"));
            }
            if entries.insert(entry.key(), entry).is_some() {
                return Err(backend("mirror parking journal has a duplicate window"));
            }
        }
        Ok(Journal { entries })
    }

    fn render(&self) -> Result<Vec<u8>, PlatformError> {
        let file = JournalFile {
            version: JOURNAL_VERSION,
            entries: self.entries.values().copied().collect(),
        };
        serde_json::to_vec_pretty(&file).map_err(backend)
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn get(&self, key: Key) -> Option<&Entry> {
        self.entries.get(&key)
    }

    fn contains(&self, key: Key) -> bool {
        self.entries.contains_key(&key)
    }

    /// With `entry` added. An entry for the same window is kept as it is: the original frame is
    /// never overwritten by a later one.
    fn with(&self, entry: Entry) -> Journal {
        let mut next = self.clone();
        next.entries.entry(entry.key()).or_insert(entry);
        next
    }

    fn without(&self, key: Key) -> Journal {
        let mut next = self.clone();
        next.entries.remove(&key);
        next
    }

    /// The ids of the windows journaled under `epoch`, in order.
    fn windows_of(&self, epoch: u64) -> Vec<u64> {
        self.entries
            .keys()
            .filter(|(e, _)| *e == epoch)
            .map(|(_, window)| *window)
            .collect()
    }

    /// The journal without entries of any other epoch than `epoch`, and how many that dropped.
    fn split_stale(&self, epoch: u64) -> (Journal, usize) {
        let entries: BTreeMap<Key, Entry> = self
            .entries
            .iter()
            .filter(|((e, _), _)| *e == epoch)
            .map(|(key, entry)| (*key, *entry))
            .collect();
        let dropped = self.entries.len() - entries.len();
        (Journal { entries }, dropped)
    }
}

/// The journal at `path`; a missing file is empty.
fn read_journal(path: &Path) -> Result<Journal, PlatformError> {
    match fs::read(path) {
        Ok(bytes) => Journal::parse(&bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Journal::default()),
        Err(error) => Err(journal_io(&error)),
    }
}

/// Replaces the journal at `path` with `journal`, durably: the content goes to a sibling temp file
/// created with mode 0600, is fsynced, renamed over `path`, and the directory is fsynced. A reader
/// sees the old journal or the new one, never a mix. The parent directory is created (mode 0700)
/// if missing. A failure leaves the old journal in place (or, if only the directory fsync failed,
/// the new one: callers treat that as a failure and keep their own state).
fn write_journal(path: &Path, journal: &Journal) -> Result<(), PlatformError> {
    let bytes = journal.render()?;
    let name = path
        .file_name()
        .ok_or_else(|| backend("mirror parking journal path has no file name"))?;
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| journal_io(&e))?;
    let mut temp_name = name.to_os_string();
    temp_name.push(".tmp");
    let temp = dir.join(temp_name);
    let written = (|| -> io::Result<()> {
        // A temp file left by a crashed run may have any mode; start from a fresh one.
        match fs::remove_file(&temp) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        // The umask may have cleared bits; the journal is private to its owner either way.
        file.set_permissions(Permissions::from_mode(0o600))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)
    })();
    if let Err(error) = written {
        let _ = fs::remove_file(&temp);
        return Err(journal_io(&error));
    }
    File::open(dir)
        .and_then(|dir| dir.sync_all())
        .map_err(|e| journal_io(&e))
}

/// An I/O failure on the journal. The error text carries no window data.
fn journal_io(error: &io::Error) -> PlatformError {
    backend(format!("mirror parking journal: {error}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use crosspane_types::color::ColorSpace;
    use crosspane_types::geom::{PointLogical, SizeMm};

    use super::*;

    // ---- fixtures ----

    fn display(id: u32, pixels: (u32, u32), scale: f64, origin: (f64, f64)) -> DisplayInfo {
        DisplayInfo {
            id: DisplayId(id),
            name: format!("display-{id}"),
            geometry: DisplayGeometry {
                physical_size: SizeMm::new(600.0, 340.0),
                pixel_size: PixelSize::new(pixels.0, pixels.1),
                scale,
                logical_origin: PointLogical::new(origin.0, origin.1),
            },
            refresh_millihz: 60_000,
            color_space: ColorSpace::Srgb,
            hdr: false,
        }
    }

    fn frame(x: i32, y: i32, width: i32, height: i32) -> Frame {
        Frame {
            x,
            y,
            width,
            height,
        }
    }

    fn shell_window(id: u64, f: Frame) -> ShellWindow {
        ShellWindow {
            id,
            app_id: "org.example.App.desktop".into(),
            title: "title".into(),
            pid: 100 + id as u32,
            x: f.x,
            y: f.y,
            width: f.width,
            height: f.height,
            focused: false,
            minimized: false,
            fullscreen: false,
        }
    }

    fn px(width: u32, height: u32) -> PixelSize {
        PixelSize::new(width, height)
    }

    fn rect(min: (i32, i32), max: (i32, i32)) -> PixelRect {
        PixelRect::new(point2(min.0, min.1), point2(max.0, max.1))
    }

    /// A fresh directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "crosspane-gnome-parking-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }

        fn journal(&self) -> PathBuf {
            self.0.join("parking-gnome.json")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum Fault {
        Timeout,
        Backend,
        NotFound,
    }

    impl Fault {
        fn error(self) -> PlatformError {
            match self {
                Fault::Timeout => PlatformError::Timeout,
                Fault::Backend => PlatformError::Backend("Shell bridge lost".into()),
                Fault::NotFound => PlatformError::NotFound,
            }
        }
    }

    /// What the fake Shell holds. A `MoveResize` takes effect after `lag` more `ListWindows`
    /// calls, like the real Shell, and ends fullscreen at once.
    #[derive(Default)]
    struct World {
        epoch: u64,
        windows: Vec<ShellWindow>,
        pending: Vec<(u64, Frame, usize)>,
        lag: usize,
        min_size: (i32, i32),
        moves: Vec<(u64, Frame)>,
        /// The journal file's text at the time of each `MoveResize`.
        journal_at_move: Vec<Option<String>>,
        journal_path: Option<PathBuf>,
        fail_list: Option<Fault>,
        /// `ListWindows` calls so far, and the one (counting from zero) that fails once.
        lists: usize,
        fail_list_at: Option<usize>,
        /// Fails the next `MoveResize`; `true`: the move was nevertheless carried out.
        fail_move: Option<(Fault, bool)>,
    }

    #[derive(Clone)]
    struct FakeShell(Arc<Mutex<World>>);

    impl FakeShell {
        fn new(epoch: u64, windows: Vec<ShellWindow>) -> FakeShell {
            FakeShell(Arc::new(Mutex::new(World {
                epoch,
                windows,
                ..World::default()
            })))
        }

        fn world(&self) -> std::sync::MutexGuard<'_, World> {
            self.0.lock().unwrap()
        }

        /// The window's frame once the Shell has applied every move asked of it so far.
        fn frame_of(&self, id: u64) -> Option<Frame> {
            let mut world = self.world();
            for (moved, target, _) in std::mem::take(&mut world.pending) {
                apply_move(&mut world, moved, target);
            }
            world.windows.iter().find(|w| w.id == id).map(Frame::of)
        }

        fn set_frame(&self, id: u64, f: Frame) {
            let mut world = self.world();
            let w = world.windows.iter_mut().find(|w| w.id == id).unwrap();
            (w.x, w.y, w.width, w.height) = (f.x, f.y, f.width, f.height);
        }

        fn set_fullscreen(&self, id: u64, fullscreen: bool) {
            let mut world = self.world();
            world
                .windows
                .iter_mut()
                .find(|w| w.id == id)
                .unwrap()
                .fullscreen = fullscreen;
        }

        fn close(&self, id: u64) {
            self.world().windows.retain(|w| w.id != id);
        }

        fn moves(&self) -> Vec<(u64, Frame)> {
            self.world().moves.clone()
        }
    }

    fn apply_move(world: &mut World, id: u64, target: Frame) {
        let min = world.min_size;
        if let Some(w) = world.windows.iter_mut().find(|w| w.id == id) {
            w.x = target.x;
            w.y = target.y;
            w.width = target.width.max(min.0);
            w.height = target.height.max(min.1);
        }
    }

    impl Shell for FakeShell {
        fn epoch(&self) -> u64 {
            self.world().epoch
        }

        fn list_windows(&self) -> Result<Vec<ShellWindow>, PlatformError> {
            let mut world = self.world();
            let call = world.lists;
            world.lists += 1;
            if world.fail_list_at == Some(call) {
                return Err(Fault::Backend.error());
            }
            if let Some(fault) = world.fail_list {
                return Err(fault.error());
            }
            let pending = std::mem::take(&mut world.pending);
            for (id, target, remaining) in pending {
                if remaining == 0 {
                    apply_move(&mut world, id, target);
                } else {
                    world.pending.push((id, target, remaining - 1));
                }
            }
            Ok(world.windows.clone())
        }

        fn move_resize(
            &self,
            id: u64,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
        ) -> Result<(), PlatformError> {
            let mut world = self.world();
            let target = frame(x, y, width, height);
            world.moves.push((id, target));
            let seen = world
                .journal_path
                .as_ref()
                .and_then(|p| fs::read_to_string(p).ok());
            world.journal_at_move.push(seen);
            let fault = world.fail_move.take();
            if let Some((fault, applied)) = fault {
                if applied {
                    let lag = world.lag;
                    world.pending.push((id, target, lag));
                }
                return Err(fault.error());
            }
            let Some(window) = world.windows.iter_mut().find(|w| w.id == id) else {
                return Err(PlatformError::NotFound);
            };
            window.fullscreen = false;
            let lag = world.lag;
            world.pending.push((id, target, lag));
            Ok(())
        }
    }

    const FAST: Settle = Settle {
        interval: Duration::from_millis(1),
        budget: Duration::from_millis(40),
    };

    fn displays_fn(displays: Vec<DisplayInfo>) -> DisplaysFn {
        Arc::new(move || displays.clone())
    }

    struct Harness {
        dir: TempDir,
        shell: FakeShell,
        displays: Vec<DisplayInfo>,
        core: Core<FakeShell>,
    }

    impl Harness {
        fn open(&self) -> Core<FakeShell> {
            let mut core = Core::open(
                self.shell.clone(),
                displays_fn(self.displays.clone()),
                self.dir.journal(),
            )
            .unwrap();
            core.settle = FAST;
            core
        }

        /// A new `Core` over the same Shell and journal, as after an agent restart.
        fn restart(&mut self) {
            self.core = self.open();
        }
    }

    const W: u64 = 7;

    fn default_displays() -> Vec<DisplayInfo> {
        vec![display(1, (1920, 1080), 1.0, (0.0, 0.0))]
    }

    fn harness_with(displays: Vec<DisplayInfo>, windows: Vec<ShellWindow>) -> Harness {
        let dir = TempDir::new();
        let shell = FakeShell::new(41, windows);
        shell.world().journal_path = Some(dir.journal());
        let mut harness = Harness {
            core: Core::open(shell.clone(), displays_fn(displays.clone()), dir.journal()).unwrap(),
            dir,
            shell,
            displays,
        };
        harness.core.settle = FAST;
        harness
    }

    fn harness() -> Harness {
        harness_with(
            default_displays(),
            vec![shell_window(W, frame(100, 100, 800, 600))],
        )
    }

    // ---- geometry ----

    #[test]
    fn display_for_picks_the_display_containing_the_centre() {
        let displays = vec![
            display(1, (1920, 1080), 1.0, (0.0, 0.0)),
            display(2, (3840, 2160), 2.0, (1920.0, 0.0)),
        ];
        // Mostly on display 1, but the centre (1950, 100) is on display 2.
        let f = frame(1500, 0, 900, 200);
        assert_eq!(display_for(f, &displays).unwrap().id, DisplayId(2));
        assert_eq!(
            display_for(frame(0, 0, 100, 100), &displays).unwrap().id,
            DisplayId(1)
        );
    }

    #[test]
    fn display_for_edges_are_half_open_and_gaps_use_the_nearest() {
        let displays = vec![
            display(1, (1920, 1080), 1.0, (0.0, 0.0)),
            display(2, (1920, 1080), 1.0, (1920.0, 0.0)),
        ];
        // A centre exactly on the shared edge belongs to the right display.
        assert_eq!(
            display_for(frame(1900, 0, 40, 40), &displays).unwrap().id,
            DisplayId(2)
        );
        // Below both displays: the nearest wins.
        assert_eq!(
            display_for(frame(100, 1500, 100, 100), &displays)
                .unwrap()
                .id,
            DisplayId(1)
        );
        assert_eq!(
            display_for(frame(2500, 1500, 100, 100), &displays)
                .unwrap()
                .id,
            DisplayId(2)
        );
        // Left of everything, and a display on its left, with a gap between.
        let gap = vec![
            display(1, (1000, 1000), 1.0, (0.0, 0.0)),
            display(2, (1000, 1000), 1.0, (1500.0, 0.0)),
        ];
        assert_eq!(
            display_for(frame(1100, 0, 100, 100), &gap).unwrap().id,
            DisplayId(1)
        );
        assert_eq!(
            display_for(frame(1300, 0, 100, 100), &gap).unwrap().id,
            DisplayId(2)
        );
    }

    #[test]
    fn display_for_skips_unusable_displays() {
        let mut broken = display(1, (1920, 1080), 1.0, (0.0, 0.0));
        broken.geometry.scale = 0.0;
        let good = display(2, (1920, 1080), 1.0, (5000.0, 0.0));
        let displays = vec![broken, good];
        assert_eq!(
            display_for(frame(0, 0, 10, 10), &displays).unwrap().id,
            DisplayId(2)
        );
        assert!(display_for(frame(0, 0, 10, 10), &[]).is_none());
        let mut nan = display(3, (1920, 1080), 1.0, (0.0, 0.0));
        nan.geometry.logical_origin.x = f64::NAN;
        assert!(display_for(frame(0, 0, 10, 10), &[nan]).is_none());
    }

    #[test]
    fn to_device_converts_with_the_display_origin_and_scale() {
        // The window at (110, -80) 800x600 on a display at (100, -100).
        let f = frame(110, -80, 800, 600);
        let one = display(7, (1920, 1080), 1.0, (100.0, -100.0));
        assert_eq!(
            to_device(f, &one.geometry).unwrap(),
            rect((10, 20), (810, 620))
        );
        let two = display(7, (3840, 2160), 2.0, (100.0, -100.0));
        assert_eq!(
            to_device(f, &two.geometry).unwrap(),
            rect((20, 40), (1620, 1240))
        );
        // Fractional scale: the size is rounded on its own, so it is round(800 * 1.25).
        let frac = display(7, (2400, 1350), 1.25, (100.0, -100.0));
        assert_eq!(
            to_device(f, &frac.geometry).unwrap(),
            rect((13, 25), (13 + 1000, 25 + 750))
        );
        // A display to the left: negative logical origin, and a window partly left of it.
        let left = display(7, (1920, 1080), 1.0, (-1920.0, 0.0));
        assert_eq!(
            to_device(frame(-1900, 10, 100, 50), &left.geometry).unwrap(),
            rect((20, 10), (120, 60))
        );
    }

    #[test]
    fn to_device_rejects_degenerate_and_overflowing_geometry() {
        let d = display(1, (1920, 1080), 1.0, (0.0, 0.0));
        assert!(to_device(frame(0, 0, 0, 10), &d.geometry).is_err());
        let huge = display(1, (1920, 1080), 1.0e9, (0.0, 0.0));
        assert!(to_device(frame(1, 1, 1000, 1000), &huge.geometry).is_err());
        let mut nan = display(1, (1920, 1080), 1.0, (0.0, 0.0));
        nan.geometry.scale = f64::NAN;
        assert!(to_device(frame(0, 0, 10, 10), &nan.geometry).is_err());
    }

    #[test]
    fn logical_extent_divides_by_the_scale_and_rounds() {
        assert_eq!(logical_extent(px(800, 600), 1.0).unwrap(), (800, 600));
        assert_eq!(logical_extent(px(1600, 1200), 2.0).unwrap(), (800, 600));
        assert_eq!(logical_extent(px(1000, 750), 1.25).unwrap(), (800, 600));
        assert_eq!(logical_extent(px(1001, 3), 2.0).unwrap(), (501, 2));
        assert!(logical_extent(px(1, 1), 4.0).is_err());
        assert!(logical_extent(px(40_000, 100), 1.0).is_err());
        assert!(logical_extent(px(100, 100), f64::NAN).is_err());
    }

    #[test]
    fn plan_resize_keeps_the_top_left_and_converts_with_the_window_display() {
        let displays = vec![
            display(1, (1920, 1080), 1.0, (0.0, 0.0)),
            display(2, (3840, 2160), 2.0, (1920.0, 0.0)),
        ];
        let on_two = shell_window(1, frame(2000, 50, 400, 300));
        assert_eq!(
            plan_resize(&on_two, px(1600, 1200), &displays).unwrap(),
            Some(frame(2000, 50, 800, 600))
        );
        // Already that size: nothing to ask the Shell.
        assert_eq!(plan_resize(&on_two, px(800, 600), &displays).unwrap(), None);
        // Fullscreen: never resized.
        let mut full = shell_window(1, frame(0, 0, 1920, 1080));
        full.fullscreen = true;
        assert_eq!(plan_resize(&full, px(500, 500), &displays).unwrap(), None);
        // No display at all.
        assert!(plan_resize(&on_two, px(800, 600), &[]).is_err());
    }

    #[test]
    fn place_frame_converts_and_clamps_into_the_display() {
        let displays = vec![
            display(1, (1920, 1080), 1.0, (0.0, 0.0)),
            display(2, (3840, 2160), 2.0, (1920.0, 0.0)),
        ];
        let original = frame(100, 100, 800, 600);
        let at = |display: u32, x: f64, y: f64| {
            place_frame(
                original,
                Placement {
                    display: DisplayId(display),
                    origin: PointDevice::new(x, y),
                },
                &displays,
            )
        };
        // Device (200, 100) on the 2x display is logical (1920 + 100, 50).
        assert_eq!(at(2, 200.0, 100.0).unwrap(), frame(2020, 50, 800, 600));
        // Past the right and bottom edges: the window ends at the edge.
        assert_eq!(at(2, 7000.0, 3000.0).unwrap(), frame(3040, 480, 800, 600));
        // Before the left and top edges: the top-left stays on the display.
        assert_eq!(at(2, -50.0, -9.0).unwrap(), frame(1920, 0, 800, 600));
        assert_eq!(at(1, 10.4, 10.6).unwrap(), frame(10, 11, 800, 600));
        // A window larger than the display sits at the near edge.
        let big = place_frame(
            frame(0, 0, 4000, 3000),
            Placement {
                display: DisplayId(1),
                origin: PointDevice::new(500.0, 500.0),
            },
            &displays,
        )
        .unwrap();
        assert_eq!(big, frame(0, 0, 4000, 3000));
        assert!(matches!(at(9, 0.0, 0.0), Err(PlatformError::NotFound)));
        assert!(at(1, f64::NAN, 0.0).is_err());
        assert!(at(1, f64::INFINITY, 0.0).is_err());
        // A huge but finite origin is clamped to the display, not passed through.
        assert_eq!(at(1, 1.0e300, -1.0e300).unwrap(), frame(1120, 0, 800, 600));
    }

    #[test]
    fn parked_reports_the_mirror_geometry() {
        let displays = vec![display(7, (3840, 2160), 2.0, (100.0, -100.0))];
        let mut window = shell_window(5, frame(110, -80, 800, 600));
        let p = parked(&window, &displays).unwrap();
        assert_eq!(p.window, WindowId(5));
        assert_eq!(p.kind, ParkingKind::Mirror);
        assert_eq!(p.display, DisplayId(7));
        assert_eq!(p.content, rect((20, 40), (1620, 1240)));
        assert!(!p.fullscreen);
        window.fullscreen = true;
        assert!(parked(&window, &displays).unwrap().fullscreen);
    }

    // ---- journal ----

    fn entry(epoch: u64, window: u64) -> Entry {
        Entry::new(epoch, window, frame(1, 2, 300, 400), false)
    }

    #[test]
    fn journal_round_trips_through_json() {
        let journal = Journal::default()
            .with(entry(u64::MAX, 9))
            .with(Entry::new(3, 1, frame(-50, 20, 640, 480), true))
            .with(entry(3, 2));
        let bytes = journal.render().unwrap();
        assert_eq!(Journal::parse(&bytes).unwrap(), journal);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("\"version\": 1"));
        assert!(text.contains(&format!("\"epoch\": {}", u64::MAX)));
        // Sorted by (epoch, window).
        let first = text.find("\"window\": 1").unwrap();
        let second = text.find("\"window\": 2").unwrap();
        let third = text.find("\"window\": 9").unwrap();
        assert!(first < second && second < third);
        assert_eq!(
            Journal::parse(&Journal::default().render().unwrap()).unwrap(),
            Journal::default()
        );
    }

    #[test]
    fn journal_rejects_what_it_cannot_trust() {
        let parse = |text: &str| Journal::parse(text.as_bytes());
        assert!(parse("").is_err());
        assert!(parse("not json").is_err());
        assert!(parse(r#"{"version": 2, "entries": []}"#).is_err());
        assert!(parse(r#"{"version": 1}"#).is_err());
        assert!(parse(r#"{"version": 1, "entries": [], "extra": 1}"#).is_err());
        let one = r#"{"epoch":1,"window":2,"x":0,"y":0,"width":10,"height":10,"fullscreen":false}"#;
        assert!(parse(&format!(r#"{{"version":1,"entries":[{one}]}}"#)).is_ok());
        assert!(parse(&format!(r#"{{"version":1,"entries":[{one},{one}]}}"#)).is_err());
        let extra = one.replace(
            "\"fullscreen\":false",
            "\"fullscreen\":false,\"title\":\"x\"",
        );
        assert!(parse(&format!(r#"{{"version":1,"entries":[{extra}]}}"#)).is_err());
        let zero = one.replace("\"width\":10", "\"width\":0");
        assert!(parse(&format!(r#"{{"version":1,"entries":[{zero}]}}"#)).is_err());
        let far = one.replace("\"x\":0", "\"x\":99999999");
        assert!(parse(&format!(r#"{{"version":1,"entries":[{far}]}}"#)).is_err());
    }

    #[test]
    fn journal_never_overwrites_an_original_frame() {
        let original = Entry::new(1, 2, frame(10, 10, 100, 100), false);
        let later = Entry::new(1, 2, frame(500, 500, 50, 50), true);
        let journal = Journal::default().with(original).with(later);
        assert_eq!(journal.get((1, 2)), Some(&original));
        assert_eq!(journal.len(), 1);
        // The same window id under another epoch is another window.
        let both = journal.with(Entry::new(2, 2, frame(0, 0, 5, 5), false));
        assert_eq!(both.len(), 2);
        assert_eq!(both.without((1, 2)).len(), 1);
        assert!(!both.without((1, 2)).contains((1, 2)));
        assert!(both.contains((2, 2)));
    }

    #[test]
    fn journal_splits_off_other_epochs() {
        let journal = Journal::default()
            .with(entry(1, 5))
            .with(entry(2, 5))
            .with(entry(2, 6))
            .with(entry(3, 7));
        assert_eq!(journal.windows_of(2), vec![5, 6]);
        assert_eq!(journal.windows_of(9), Vec::<u64>::new());
        let (kept, dropped) = journal.split_stale(2);
        assert_eq!(dropped, 2);
        assert_eq!(kept.windows_of(2), vec![5, 6]);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn write_journal_is_private_atomic_and_leaves_no_temp_file() {
        use std::os::unix::fs::MetadataExt;
        let dir = TempDir::new();
        // The parent directory does not exist yet: it is created, 0700.
        let path = dir.0.join("state").join("parking.json");
        let journal = Journal::default().with(entry(1, 1));
        // A crashed run's temp file, world-readable and with junk in it.
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let temp = path.parent().unwrap().join("parking.json.tmp");
        fs::write(&temp, "junk").unwrap();
        fs::set_permissions(&temp, Permissions::from_mode(0o644)).unwrap();
        fs::set_permissions(path.parent().unwrap(), Permissions::from_mode(0o755)).unwrap();
        write_journal(&path, &journal).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(read_journal(&path).unwrap(), journal);
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("parking.json")]);
        // Replacing keeps the mode and the newest content.
        write_journal(&path, &Journal::default()).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(read_journal(&path).unwrap(), Journal::default());

        let fresh = dir.0.join("made").join("deeper").join("parking.json");
        write_journal(&fresh, &journal).unwrap();
        assert_eq!(
            fs::metadata(fresh.parent().unwrap()).unwrap().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn write_journal_fails_when_the_directory_cannot_exist() {
        let dir = TempDir::new();
        let blocker = dir.0.join("blocker");
        fs::write(&blocker, "a file").unwrap();
        let path = blocker.join("parking.json");
        assert!(write_journal(&path, &Journal::default()).is_err());
        // No file name to write to: refused before anything is created.
        assert!(write_journal(Path::new(""), &Journal::default()).is_err());
    }

    #[test]
    fn a_missing_journal_is_empty_and_a_corrupt_one_is_an_error() {
        let dir = TempDir::new();
        assert_eq!(read_journal(&dir.journal()).unwrap(), Journal::default());
        fs::write(dir.journal(), "{ torn").unwrap();
        let shell = FakeShell::new(1, vec![]);
        assert!(Core::open(shell, displays_fn(vec![]), dir.journal()).is_err());
        // The corrupt file is left for the owner to look at.
        assert_eq!(fs::read_to_string(dir.journal()).unwrap(), "{ torn");
    }

    // ---- park / resize / geometry ----

    #[test]
    fn park_journals_before_it_moves_and_resizes_keeping_the_top_left() {
        let mut h = harness();
        let parked = h.core.park(WindowId(W), px(500, 300), 2.0).unwrap();
        assert_eq!(h.shell.moves(), vec![(W, frame(100, 100, 500, 300))]);
        // The journal already held the original frame when the Shell was asked to move.
        let seen = h.shell.world().journal_at_move[0].clone().unwrap();
        let journal = Journal::parse(seen.as_bytes()).unwrap();
        assert_eq!(
            journal.get((41, W)),
            Some(&Entry::new(41, W, frame(100, 100, 800, 600), false))
        );
        assert_eq!(parked.window, WindowId(W));
        assert_eq!(parked.kind, ParkingKind::Mirror);
        assert_eq!(parked.display, DisplayId(1));
        assert_eq!(parked.content, rect((100, 100), (600, 400)));
        assert!(!parked.fullscreen);
        assert_eq!(h.core.geometry(WindowId(W)).unwrap(), parked);
    }

    #[test]
    fn park_ignores_the_destination_scale_and_uses_the_source_display_density() {
        let mut h = harness_with(
            vec![display(1, (3840, 2160), 2.0, (0.0, 0.0))],
            vec![shell_window(W, frame(100, 100, 400, 300))],
        );
        // 1600x1200 device pixels on a 2x source display is 800x600 logical, whatever `scale` says.
        let parked = h.core.park(WindowId(W), px(1600, 1200), 1.0).unwrap();
        assert_eq!(h.shell.frame_of(W), Some(frame(100, 100, 800, 600)));
        assert_eq!(parked.content, rect((200, 200), (1800, 1400)));
        let again = h.core.resize(WindowId(W), px(1600, 1200), 3.5).unwrap();
        assert_eq!(again, parked);
    }

    #[test]
    fn parking_a_parked_window_is_a_resize_and_keeps_the_original() {
        let mut h = harness();
        h.core.park(WindowId(W), px(500, 300), 1.0).unwrap();
        // The user moves the parked window; the second park keeps the new top-left but not the
        // new frame as "original".
        h.shell.set_frame(W, frame(300, 250, 500, 300));
        let again = h.core.park(WindowId(W), px(640, 360), 1.0).unwrap();
        assert_eq!(h.shell.frame_of(W), Some(frame(300, 250, 640, 360)));
        assert_eq!(again.content, rect((300, 250), (940, 610)));
        let journal = read_journal(&h.dir.journal()).unwrap();
        assert_eq!(journal.len(), 1);
        assert_eq!(
            journal.get((41, W)).unwrap().frame(),
            frame(100, 100, 800, 600)
        );
    }

    #[test]
    fn resize_and_geometry_need_a_parked_window() {
        let mut h = harness();
        assert!(matches!(
            h.core.resize(WindowId(W), px(500, 300), 1.0),
            Err(PlatformError::NotFound)
        ));
        assert!(matches!(
            h.core.geometry(WindowId(W)),
            Err(PlatformError::NotFound)
        ));
        assert!(h.shell.moves().is_empty());
        assert!(!h.dir.journal().exists());
    }

    #[test]
    fn park_of_an_unknown_window_is_not_found_and_writes_nothing() {
        let mut h = harness();
        assert!(matches!(
            h.core.park(WindowId(99), px(500, 300), 1.0),
            Err(PlatformError::NotFound)
        ));
        assert!(!h.dir.journal().exists());
    }

    #[test]
    fn park_refuses_bad_sizes_before_touching_anything() {
        let mut h = harness();
        assert!(h.core.park(WindowId(W), px(0, 300), 1.0).is_err());
        assert!(h.core.park(WindowId(W), px(500, 0), 1.0).is_err());
        assert!(h.core.park(WindowId(W), px(40_000, 300), 1.0).is_err());
        assert!(h.shell.moves().is_empty());
        assert!(!h.dir.journal().exists());
    }

    #[test]
    fn park_without_a_display_changes_nothing() {
        let mut h = harness_with(vec![], vec![shell_window(W, frame(100, 100, 800, 600))]);
        assert!(h.core.park(WindowId(W), px(500, 300), 1.0).is_err());
        assert!(h.shell.moves().is_empty());
        assert!(!h.dir.journal().exists());
    }

    #[test]
    fn park_with_the_window_already_at_the_size_still_journals_but_does_not_move() {
        let mut h = harness();
        let parked = h.core.park(WindowId(W), px(800, 600), 1.0).unwrap();
        assert!(h.shell.moves().is_empty());
        assert_eq!(parked.content, rect((100, 100), (900, 700)));
        assert!(read_journal(&h.dir.journal()).unwrap().contains((41, W)));
    }

    #[test]
    fn park_waits_for_the_shell_to_apply_the_size() {
        let mut h = harness();
        h.shell.world().lag = 3;
        let parked = h.core.park(WindowId(W), px(500, 300), 1.0).unwrap();
        assert_eq!(parked.content, rect((100, 100), (600, 400)));
    }

    #[test]
    fn park_reports_the_size_an_app_took_when_it_refuses_the_request() {
        let mut h = harness();
        h.shell.world().min_size = (640, 480);
        let parked = h.core.park(WindowId(W), px(500, 300), 1.0).unwrap();
        // The request was 500x300; the app kept 640x480 and that is what is reported.
        assert_eq!(parked.content, rect((100, 100), (740, 580)));
        assert_eq!(h.core.geometry(WindowId(W)).unwrap(), parked);
    }

    #[test]
    fn park_survives_a_window_that_moved_between_calls() {
        let mut h = harness();
        h.core.park(WindowId(W), px(500, 300), 1.0).unwrap();
        h.shell.set_frame(W, frame(40, 60, 500, 300));
        assert_eq!(
            h.core.geometry(WindowId(W)).unwrap().content,
            rect((40, 60), (540, 360))
        );
    }

    // ---- fullscreen ----

    #[test]
    fn set_fullscreen_is_unsupported_and_changes_nothing() {
        let mut h = harness();
        h.core.park(WindowId(W), px(500, 300), 1.0).unwrap();
        let before = h.shell.moves();
        assert!(matches!(
            h.core.set_fullscreen(WindowId(W), true),
            Err(PlatformError::Unsupported(_))
        ));
        assert!(matches!(
            h.core.set_fullscreen(WindowId(W), false),
            Err(PlatformError::Unsupported(_))
        ));
        assert_eq!(h.shell.moves(), before);
    }

    #[test]
    fn a_fullscreen_window_is_journaled_but_not_resized_or_restored_out_of_fullscreen() {
        let mut full = shell_window(W, frame(0, 0, 1920, 1080));
        full.fullscreen = true;
        let mut h = harness_with(default_displays(), vec![full]);
        let parked = h.core.park(WindowId(W), px(500, 300), 1.0).unwrap();
        assert!(h.shell.moves().is_empty());
        assert!(parked.fullscreen);
        assert_eq!(parked.content, rect((0, 0), (1920, 1080)));
        let journal = read_journal(&h.dir.journal()).unwrap();
        assert!(journal.get((41, W)).unwrap().fullscreen);
        // Resize keeps leaving it alone, and so does restore while it is fullscreen.
        h.core.resize(WindowId(W), px(700, 400), 1.0).unwrap();
        h.core.restore(WindowId(W)).unwrap();
        assert!(h.shell.moves().is_empty());
        assert_eq!(read_journal(&h.dir.journal()).unwrap(), Journal::default());
    }

    #[test]
    fn restore_ends_fullscreen_the_user_entered_after_parking() {
        let mut h = harness();
        h.core.park(WindowId(W), px(500, 300), 1.0).unwrap();
        h.shell.set_frame(W, frame(0, 0, 1920, 1080));
        h.shell.set_fullscreen(W, true);
        h.core.restore(WindowId(W)).unwrap();
        assert_eq!(h.shell.frame_of(W), Some(frame(100, 100, 800, 600)));
        assert!(!h.shell.world().windows[0].fullscreen);
    }

    // ---- restore ----

    #[test]
    fn restore_moves_the_window_back_and_retires_the_entry() {
        let mut h = harness();
        h.core.park(WindowId(W), px(500, 300), 1.0).unwrap();
        h.shell.set_frame(W, frame(333, 222, 500, 300));
        h.core.restore(WindowId(W)).unwrap();
        // The journaled rect, wherever the window had wandered.
        assert_eq!(
            h.shell.moves().last(),
            Some(&(W, frame(100, 100, 800, 600)))
        );
        assert_eq!(h.shell.frame_of(W), Some(frame(100, 100, 800, 600)));
        assert_eq!(read_journal(&h.dir.journal()).unwrap(), Journal::default());
        // Idempotent: nothing is left to do and nothing is sent.
        let sent = h.shell.moves().len();
        h.core.restore(WindowId(W)).unwrap();
        assert_eq!(h.shell.moves().len(), sent);
    }

    #[test]
    fn restore_of_a_window_never_parked_does_nothing() {
        let mut h = harness();
        h.core.restore(WindowId(W)).unwrap();
        h.core.restore(WindowId(1234)).unwrap();
        assert!(h.shell.moves().is_empty());
    }

    #[test]
    fn restore_of_a_closed_window_is_ok_and_retires_the_entry() {
        let mut h = harness();
        h.core.park(WindowId(W), px(500, 300), 1.0).unwrap();
        h.shell.close(W);
        h.core.restore(WindowId(W)).unwrap();
        assert_eq!(read_journal(&h.dir.journal()).unwrap(), Journal::default());
        assert!(h.shell.moves().len() == 1);
    }

    #[test]
    fn restore_treats_a_window_that_vanishes_during_the_move_as_closed() {
        let mut h = harness();
        h.core.park(WindowId(W), px(500, 300), 1.0).unwrap();
        // The window is listed, and gone by the time the move arrives: the extension answers
        // `false`, which the bridge client reports as `NotFound`.
        h.shell.world().fail_move = Some((Fault::NotFound, false));
        h.core.restore(WindowId(W)).unwrap();
        assert_eq!(read_journal(&h.dir.journal()).unwrap(), Journal::default());
        assert_eq!(h.shell.moves().len(), 2);
    }

    #[test]
    fn a_bridge_failure_keeps_the_entry_for_a_retry() {
        let mut h = harness();
        h.core.park(WindowId(W), px(500, 300), 1.0).unwrap();
        h.shell.world().fail_list = Some(Fault::Backend);
        assert!(h.core.restore(WindowId(W)).is_err());
        assert!(read_journal(&h.dir.journal()).unwrap().contains((41, W)));
        h.shell.world().fail_list = None;
        h.shell.world().fail_move = Some((Fault::Timeout, false));
        assert!(matches!(
            h.core.restore(WindowId(W)),
            Err(PlatformError::Timeout)
        ));
        assert!(read_journal(&h.dir.journal()).unwrap().contains((41, W)));
        // The retry succeeds and retires.
        h.core.restore(WindowId(W)).unwrap();
        assert_eq!(h.shell.frame_of(W), Some(frame(100, 100, 800, 600)));
        assert_eq!(read_journal(&h.dir.journal()).unwrap(), Journal::default());
    }

    #[test]
    fn a_journal_write_failure_during_restore_keeps_the_entry_in_memory() {
        let mut h = harness();
        h.core.park(WindowId(W), px(500, 300), 1.0).unwrap();
        // Make the journal directory unwritable by pointing the core at an impossible path.
        let good = h.core.path.clone();
        let blocker = h.dir.0.join("blocker");
        fs::write(&blocker, "file").unwrap();
        h.core.path = blocker.join("parking.json");
        assert!(h.core.restore(WindowId(W)).is_err());
        assert!(h.core.journal.contains((41, W)));
        // Disk is healthy again: the retry finds the window already back and retires the entry.
        h.core.path = good;
        h.core.restore(WindowId(W)).unwrap();
        assert!(!h.core.journal.contains((41, W)));
        assert_eq!(read_journal(&h.dir.journal()).unwrap(), Journal::default());
    }

    #[test]
    fn restore_at_places_the_journaled_size_at_the_converted_origin() {
        let displays = vec![
            display(1, (1920, 1080), 1.0, (0.0, 0.0)),
            display(2, (3840, 2160), 2.0, (1920.0, 0.0)),
        ];
        let mut h = harness_with(displays, vec![shell_window(W, frame(100, 100, 800, 600))]);
        h.core.park(WindowId(W), px(500, 300), 1.0).unwrap();
        h.core
            .restore_at(WindowId(W), DisplayId(2), PointDevice::new(200.0, 100.0))
            .unwrap();
        // One request: the journaled 800x600 at logical (1920 + 100, 50).
        assert_eq!(
            h.shell.moves().last(),
            Some(&(W, frame(2020, 50, 800, 600)))
        );
        assert_eq!(h.shell.moves().len(), 2);
        assert_eq!(h.shell.frame_of(W), Some(frame(2020, 50, 800, 600)));
        assert_eq!(read_journal(&h.dir.journal()).unwrap(), Journal::default());
    }

    #[test]
    fn restore_at_clamps_to_the_display() {
        let mut h = harness();
        h.core.park(WindowId(W), px(500, 300), 1.0).unwrap();
        h.core
            .restore_at(WindowId(W), DisplayId(1), PointDevice::new(5000.0, -40.0))
            .unwrap();
        assert_eq!(h.shell.frame_of(W), Some(frame(1120, 0, 800, 600)));
    }

    #[test]
    fn restore_at_with_an_unknown_display_or_origin_restores_in_place() {
        let mut h = harness();
        h.core.park(WindowId(W), px(500, 300), 1.0).unwrap();
        h.core
            .restore_at(WindowId(W), DisplayId(99), PointDevice::new(10.0, 10.0))
            .unwrap();
        assert_eq!(h.shell.frame_of(W), Some(frame(100, 100, 800, 600)));
        assert_eq!(read_journal(&h.dir.journal()).unwrap(), Journal::default());

        h.core.park(WindowId(W), px(500, 300), 1.0).unwrap();
        h.core
            .restore_at(WindowId(W), DisplayId(1), PointDevice::new(f64::NAN, 1.0))
            .unwrap();
        assert_eq!(h.shell.frame_of(W), Some(frame(100, 100, 800, 600)));
    }

    #[test]
    fn restore_at_leaves_a_still_fullscreen_window_alone() {
        let mut full = shell_window(W, frame(0, 0, 1920, 1080));
        full.fullscreen = true;
        let mut h = harness_with(default_displays(), vec![full]);
        h.core.park(WindowId(W), px(500, 300), 1.0).unwrap();
        h.core
            .restore_at(WindowId(W), DisplayId(1), PointDevice::new(300.0, 300.0))
            .unwrap();
        assert!(h.shell.moves().is_empty());
        assert!(h.shell.world().windows[0].fullscreen);
        assert_eq!(read_journal(&h.dir.journal()).unwrap(), Journal::default());
    }

    #[test]
    fn restore_at_without_an_entry_places_nothing() {
        let mut h = harness();
        h.core
            .restore_at(WindowId(W), DisplayId(1), PointDevice::new(300.0, 300.0))
            .unwrap();
        assert!(h.shell.moves().is_empty());
    }

    // ---- failure and crash points ----

    #[test]
    fn a_journal_write_failure_means_the_window_is_not_touched() {
        let mut h = harness();
        let blocker = h.dir.0.join("blocker");
        fs::write(&blocker, "file").unwrap();
        h.core.path = blocker.join("parking.json");
        assert!(h.core.park(WindowId(W), px(500, 300), 1.0).is_err());
        assert!(h.shell.moves().is_empty());
        assert_eq!(h.shell.frame_of(W), Some(frame(100, 100, 800, 600)));
        assert!(!h.core.journal.contains((41, W)));
    }

    #[test]
    fn a_refused_move_leaves_no_entry() {
        let mut h = harness();
        // An error reply: the Shell did nothing.
        h.shell.world().fail_move = Some((Fault::Backend, false));
        assert!(matches!(
            h.core.park(WindowId(W), px(500, 300), 1.0),
            Err(PlatformError::Backend(_))
        ));
        // The rollback found the window where it was and retired the entry.
        assert_eq!(h.shell.moves().len(), 1);
        assert_eq!(read_journal(&h.dir.journal()).unwrap(), Journal::default());
        assert_eq!(h.shell.frame_of(W), Some(frame(100, 100, 800, 600)));
    }

    #[test]
    fn a_timed_out_move_keeps_the_entry_because_its_outcome_is_unknown() {
        let mut h = harness();
        // The reply never came, but the Shell did resize the window.
        h.shell.world().fail_move = Some((Fault::Timeout, true));
        assert!(matches!(
            h.core.park(WindowId(W), px(500, 300), 1.0),
            Err(PlatformError::Timeout)
        ));
        // No rollback was attempted: it could have read the old rect and retired the entry.
        assert_eq!(h.shell.moves().len(), 1);
        assert!(read_journal(&h.dir.journal()).unwrap().contains((41, W)));
        assert_eq!(h.shell.frame_of(W), Some(frame(100, 100, 500, 300)));
        // The next run undoes it.
        h.restart();
        assert_eq!(h.core.recover().unwrap(), vec![WindowId(W)]);
        assert_eq!(h.shell.frame_of(W), Some(frame(100, 100, 800, 600)));
        assert_eq!(read_journal(&h.dir.journal()).unwrap(), Journal::default());
    }

    #[test]
    fn a_park_that_fails_after_the_move_is_rolled_back() {
        let mut h = harness();
        // The move went through; reading the window back fails once (list calls: the park's own
        // read, then the read-back), and the rollback's read works.
        h.shell.world().fail_list_at = Some(1);
        assert!(matches!(
            h.core.park(WindowId(W), px(500, 300), 1.0),
            Err(PlatformError::Backend(_))
        ));
        assert_eq!(h.shell.moves().len(), 2);
        assert_eq!(h.shell.frame_of(W), Some(frame(100, 100, 800, 600)));
        assert_eq!(read_journal(&h.dir.journal()).unwrap(), Journal::default());
    }

    #[test]
    fn a_park_that_fails_after_the_move_with_a_dead_bridge_keeps_the_entry_for_recover() {
        let mut h = harness();
        // The bridge answers the park's first `ListWindows` and then dies: the move went through,
        // the read-back and the rollback cannot reach the bridge.
        let flaky = FlakyShell {
            inner: h.shell.clone(),
            lists_left: Mutex::new(1),
        };
        let mut core = Core::open(flaky, displays_fn(h.displays.clone()), h.dir.journal()).unwrap();
        core.settle = FAST;
        assert!(core.park(WindowId(W), px(500, 300), 1.0).is_err());
        // The window is resized and the entry is still on disk.
        assert!(read_journal(&h.dir.journal()).unwrap().contains((41, W)));
        assert_eq!(h.shell.moves().len(), 1);
        assert_eq!(h.shell.frame_of(W), Some(frame(100, 100, 500, 300)));

        // A restart with a healthy bridge puts it back.
        h.restart();
        assert_eq!(h.core.recover().unwrap(), vec![WindowId(W)]);
        assert_eq!(h.shell.frame_of(W), Some(frame(100, 100, 800, 600)));
        assert_eq!(read_journal(&h.dir.journal()).unwrap(), Journal::default());
    }

    /// A Shell that answers `lists_left` calls to `list_windows` and then fails them all.
    struct FlakyShell {
        inner: FakeShell,
        lists_left: Mutex<usize>,
    }

    impl Shell for FlakyShell {
        fn epoch(&self) -> u64 {
            self.inner.epoch()
        }

        fn list_windows(&self) -> Result<Vec<ShellWindow>, PlatformError> {
            let mut left = self.lists_left.lock().unwrap();
            if *left == 0 {
                return Err(Fault::Backend.error());
            }
            *left -= 1;
            self.inner.list_windows()
        }

        fn move_resize(
            &self,
            id: u64,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
        ) -> Result<(), PlatformError> {
            self.inner.move_resize(id, x, y, width, height)
        }
    }

    #[test]
    fn a_crash_after_the_journal_write_and_before_the_move_is_a_no_op_to_recover() {
        let mut h = harness();
        // The state a crash between the rename and `MoveResize` leaves behind.
        let entry = Entry::new(41, W, frame(100, 100, 800, 600), false);
        write_journal(&h.dir.journal(), &Journal::default().with(entry)).unwrap();
        h.restart();
        assert_eq!(h.core.recover().unwrap(), vec![WindowId(W)]);
        assert!(h.shell.moves().is_empty());
        assert_eq!(read_journal(&h.dir.journal()).unwrap(), Journal::default());
    }

    #[test]
    fn a_crash_after_the_move_is_undone_by_the_next_runs_recover() {
        let mut h = harness();
        h.core.park(WindowId(W), px(500, 300), 1.0).unwrap();
        assert_eq!(h.shell.frame_of(W), Some(frame(100, 100, 500, 300)));
        // The process dies here; a new one loads the journal.
        h.restart();
        assert_eq!(h.core.recover().unwrap(), vec![WindowId(W)]);
        assert_eq!(h.shell.frame_of(W), Some(frame(100, 100, 800, 600)));
        assert_eq!(read_journal(&h.dir.journal()).unwrap(), Journal::default());
        // A second recover has nothing to do.
        let sent = h.shell.moves().len();
        assert_eq!(h.core.recover().unwrap(), Vec::<WindowId>::new());
        assert_eq!(h.shell.moves().len(), sent);
    }

    #[test]
    fn a_crash_after_the_restore_move_and_before_the_retire_is_repeated_harmlessly() {
        let mut h = harness();
        h.core.park(WindowId(W), px(500, 300), 1.0).unwrap();
        // The restore move happened (window at its rect), the journal write did not.
        h.shell.set_frame(W, frame(100, 100, 800, 600));
        h.restart();
        let sent = h.shell.moves().len();
        assert_eq!(h.core.recover().unwrap(), vec![WindowId(W)]);
        assert_eq!(h.shell.moves().len(), sent);
        assert_eq!(read_journal(&h.dir.journal()).unwrap(), Journal::default());
    }

    // ---- recover ----

    #[test]
    fn recover_restores_this_epoch_and_retires_other_epochs() {
        let mut h = harness_with(
            default_displays(),
            vec![
                shell_window(7, frame(100, 100, 800, 600)),
                shell_window(8, frame(10, 10, 300, 200)),
            ],
        );
        h.core.park(WindowId(7), px(500, 300), 1.0).unwrap();
        h.core.park(WindowId(8), px(100, 100), 1.0).unwrap();
        // Entries from earlier Shell epochs, one of them naming a window id that is live now.
        let journal = read_journal(&h.dir.journal())
            .unwrap()
            .with(Entry::new(40, 7, frame(5, 5, 50, 50), false))
            .with(Entry::new(39, 99, frame(5, 5, 50, 50), false));
        write_journal(&h.dir.journal(), &journal).unwrap();
        h.restart();
        let sent = h.shell.moves().len();
        let restored = h.core.recover().unwrap();
        assert_eq!(restored, vec![WindowId(7), WindowId(8)]);
        assert_eq!(h.shell.frame_of(7), Some(frame(100, 100, 800, 600)));
        assert_eq!(h.shell.frame_of(8), Some(frame(10, 10, 300, 200)));
        // Only the two current-epoch windows were moved; the stale entries moved nothing.
        assert_eq!(h.shell.moves().len(), sent + 2);
        assert_eq!(read_journal(&h.dir.journal()).unwrap(), Journal::default());
    }

    #[test]
    fn recover_does_not_report_closed_windows_and_retires_them() {
        let mut h = harness_with(
            default_displays(),
            vec![
                shell_window(7, frame(100, 100, 800, 600)),
                shell_window(8, frame(10, 10, 300, 200)),
            ],
        );
        h.core.park(WindowId(7), px(500, 300), 1.0).unwrap();
        h.core.park(WindowId(8), px(100, 100), 1.0).unwrap();
        h.shell.close(7);
        h.restart();
        assert_eq!(h.core.recover().unwrap(), vec![WindowId(8)]);
        assert_eq!(read_journal(&h.dir.journal()).unwrap(), Journal::default());
    }

    #[test]
    fn recover_keeps_failed_entries_and_still_restores_the_others() {
        let mut h = harness_with(
            default_displays(),
            vec![
                shell_window(7, frame(100, 100, 800, 600)),
                shell_window(8, frame(10, 10, 300, 200)),
            ],
        );
        h.core.park(WindowId(7), px(500, 300), 1.0).unwrap();
        h.core.park(WindowId(8), px(100, 100), 1.0).unwrap();
        h.restart();
        h.shell.world().fail_move = Some((Fault::Backend, false));
        assert!(h.core.recover().is_err());
        // Window 7 failed (first in order) and stays journaled; window 8 was restored.
        let journal = read_journal(&h.dir.journal()).unwrap();
        assert!(journal.contains((41, 7)));
        assert!(!journal.contains((41, 8)));
        assert_eq!(h.shell.frame_of(8), Some(frame(10, 10, 300, 200)));
        // The next recover finishes the job.
        assert_eq!(h.core.recover().unwrap(), vec![WindowId(7)]);
        assert_eq!(read_journal(&h.dir.journal()).unwrap(), Journal::default());
    }

    #[test]
    fn recover_stops_waiting_after_a_timeout() {
        let mut h = harness_with(
            default_displays(),
            vec![
                shell_window(7, frame(100, 100, 800, 600)),
                shell_window(8, frame(10, 10, 300, 200)),
            ],
        );
        h.core.park(WindowId(7), px(500, 300), 1.0).unwrap();
        h.core.park(WindowId(8), px(100, 100), 1.0).unwrap();
        h.restart();
        h.shell.world().fail_move = Some((Fault::Timeout, false));
        let sent = h.shell.moves().len();
        assert!(matches!(h.core.recover(), Err(PlatformError::Timeout)));
        // The first window timed out; the second was not attempted.
        assert_eq!(h.shell.moves().len(), sent + 1);
        assert_eq!(read_journal(&h.dir.journal()).unwrap().len(), 2);
    }

    #[test]
    fn recover_with_an_empty_journal_is_empty() {
        let mut h = harness();
        assert_eq!(h.core.recover().unwrap(), Vec::<WindowId>::new());
        assert!(!h.dir.journal().exists());
    }

    #[test]
    fn a_new_epoch_does_not_confuse_a_reused_window_id() {
        let mut h = harness();
        h.core.park(WindowId(W), px(500, 300), 1.0).unwrap();
        // The Shell restarted: same id, new epoch, a different window at another place.
        h.shell.world().epoch = 42;
        h.shell.set_frame(W, frame(700, 700, 300, 300));
        h.restart();
        // Not parked in this epoch: geometry and resize say so, and restore moves nothing.
        assert!(matches!(
            h.core.geometry(WindowId(W)),
            Err(PlatformError::NotFound)
        ));
        h.core.restore(WindowId(W)).unwrap();
        assert!(h.core.recover().unwrap().is_empty());
        assert_eq!(h.shell.frame_of(W), Some(frame(700, 700, 300, 300)));
        assert_eq!(h.shell.moves().len(), 1);
        assert_eq!(read_journal(&h.dir.journal()).unwrap(), Journal::default());
    }

    #[test]
    fn debug_output_names_no_windows() {
        let h = harness();
        let text = format!("{:?}", h.core);
        assert!(text.contains("epoch"));
        assert!(!text.contains("title"));
    }
}
