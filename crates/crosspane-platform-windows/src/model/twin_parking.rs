//! Pure M2 parking orchestration (WP-W3.2 T15). A window's original goes into the M1 journal
//! through [`Controller`], then a native twin is added and the window moves onto it. The order of
//! native calls, the rollbacks and the restore order live here. No Win32 types or calls: native
//! code implements [`TwinOps`] and [`NativePort`] and carries out what this model asks.

use super::{
    geometry::MonitorProbe,
    journal::Show,
    parking::{Controller, NativePort, Observed, rect_size, validate_size_scale},
    twin::{
        TWIN_UNAVAILABLE_REASON, TwinDisplay, TwinError, TwinKey, TwinMode, fallback, twin_mode,
    },
};
use crosspane_platform::{Parked, ParkingKind, PlatformError};
use crosspane_types::{
    geom::PixelSize,
    id::{DisplayId, WindowId},
};
use std::collections::BTreeMap;

/// Window originals for twin parking, in the M1 journal format under their own names. The twin
/// ledger keeps `parking-twin.journal`.
pub const WINDOW_COMMITTED_NAME: &str = "parking-twin-window.journal";
pub const WINDOW_PENDING_NAME: &str = "parking-twin-window.pending";
pub const TWIN_MAXIMIZED_REASON: &str =
    "A maximized or full-screen window stays visible in place (mirror mode).";
pub const TWIN_DISABLED_REASON: &str = "Twin display parking is off in this mode.";
/// The bound on one native twin locate, used by the native `TwinOps` implementation.
pub const LOCATE_TIMEOUT_MS: u32 = 1_000;

/// The native twin operations the orchestration drives. Each call is bounded natively.
pub trait TwinOps {
    /// Checks that the twin driver is present and usable. Opens it lazily.
    fn ready(&mut self) -> Result<(), TwinError>;
    /// Adds a twin of `mode` and returns it once it is a desktop display.
    fn add(&mut self, mode: TwinMode) -> Result<TwinDisplay, TwinError>;
    /// Changes a twin's mode. The returned twin carries the key the orchestration keeps from now on.
    fn resize(&mut self, key: TwinKey, mode: TwinMode) -> Result<TwinDisplay, TwinError>;
    /// Removes a twin. A failure is not fatal to a restore.
    fn remove(&mut self, key: TwinKey) -> Result<(), TwinError>;
    /// Forgets a twin. Windows moves any window still on it back to a real monitor.
    fn discard(&mut self, key: TwinKey);
    /// Finds the twin's display id and its desktop rect.
    fn locate(&mut self, display: &TwinDisplay) -> Result<(DisplayId, [i32; 4]), PlatformError>;
    /// The keys of twins whose lease was lost since the last call.
    fn lost(&mut self) -> Vec<TwinKey>;
}

/// A window placed on a twin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    pub key: TwinKey,
    pub mode: TwinMode,
    pub display: DisplayId,
    /// The twin's desktop rect when it was located.
    pub rect: [i32; 4],
}

/// M2 parking for one owner thread. `windows` keeps the window originals in the M1 journal.
pub struct TwinParking<P> {
    pub windows: Controller<P>,
    placed: BTreeMap<WindowId, Placement>,
}

impl<P> std::fmt::Debug for TwinParking<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TwinParking")
            .field("windows", &self.windows)
            .field("placed", &self.placed.len())
            .finish_non_exhaustive()
    }
}

impl<P: NativePort> TwinParking<P> {
    pub fn new(windows: Controller<P>) -> Self {
        Self {
            windows,
            placed: BTreeMap::new(),
        }
    }

    pub fn placement(&self, id: WindowId) -> Option<&Placement> {
        self.placed.get(&id)
    }

    /// Parks `id` on a twin. A window that is already placed is resized instead.
    pub fn park(
        &mut self,
        ops: &mut dyn TwinOps,
        id: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError> {
        self.reap_lost(ops);
        if self.placed.contains_key(&id) {
            return self.resize_placed(ops, id, size, scale);
        }
        validate_size_scale(size, scale)?;
        if let Err(error) = ops.ready() {
            return Err(fallback(&error));
        }
        let mode = twin_mode(size, scale)?;
        let observed = self.windows.inspect(id)?;
        if !observed.eligible {
            return Err(PlatformError::Locked);
        }
        if observed.show == Show::Maximized || observed.fullscreen {
            return Err(PlatformError::Unsupported(TWIN_MAXIMIZED_REASON));
        }
        // Commits the original to the journal. Nothing moves yet, so a failure here needs no rollback.
        self.windows.park(id, size, scale)?;
        let twin = match ops.add(mode) {
            Ok(twin) => twin,
            Err(error) => return Err(self.rollback(ops, id, None, fallback(&error))),
        };
        let (display, rect) = match locate_twin(ops, &twin) {
            Ok(found) => found,
            Err(cause) => return Err(self.rollback(ops, id, Some(twin.key), cause)),
        };
        match self.move_onto_twin(id, &observed, display, rect, mode, size) {
            Ok(parked) => {
                self.placed.insert(
                    id,
                    Placement {
                        key: twin.key,
                        mode,
                        display,
                        rect,
                    },
                );
                Ok(parked)
            }
            Err(cause) => Err(self.rollback(ops, id, Some(twin.key), cause)),
        }
    }

    /// Resizes a placed window: in place when the twin mode is unchanged, otherwise on a re-moded
    /// twin. A failure drops the placement, returns the window and discards the twin.
    pub fn resize(
        &mut self,
        ops: &mut dyn TwinOps,
        id: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError> {
        self.reap_lost(ops);
        self.resize_placed(ops, id, size, scale)
    }

    /// The current geometry of a parked window.
    pub fn geometry(&mut self, id: WindowId) -> Result<Parked, PlatformError> {
        self.windows.geometry(id)
    }

    pub fn set_fullscreen(&mut self, id: WindowId, fullscreen: bool) -> Result<(), PlatformError> {
        self.windows.set_fullscreen(id, fullscreen)
    }

    /// Returns the window to its original place, then removes its twin. A failed window restore
    /// discards the twin instead, drops the placement and returns the error. The journal entry
    /// stays, so a later call retries.
    pub fn restore(&mut self, ops: &mut dyn TwinOps, id: WindowId) -> Result<(), PlatformError> {
        self.reap_lost(ops);
        self.restore_placed(ops, id)
    }

    /// Restores every placed window, as [`TwinParking::restore`] does. Then restores every other
    /// journaled window, such as one whose placement a failed restore dropped. Returns the windows
    /// restored, including windows whose twin was lost. Every window is tried. The first error is
    /// returned.
    pub fn recover(&mut self, ops: &mut dyn TwinOps) -> Result<Vec<WindowId>, PlatformError> {
        let mut restored = self.reap_lost(ops);
        let ids: Vec<WindowId> = self.placed.keys().copied().collect();
        let mut first = None;
        for id in ids {
            match self.restore_placed(ops, id) {
                Ok(()) => restored.push(id),
                Err(error) => {
                    if first.is_none() {
                        first = Some(error);
                    }
                }
            }
        }
        match self.windows.recover() {
            Ok(more) => restored.extend(more),
            Err(error) => {
                if first.is_none() {
                    first = Some(error);
                }
            }
        }
        match first {
            Some(error) => Err(error),
            None => Ok(restored),
        }
    }

    /// Returns each window whose twin lease was lost, then forgets the twin. Returns the windows
    /// restored. A window whose restore fails loses its placement but keeps its journal entry, so
    /// startup retries it. Every call that takes `ops` reaps first.
    pub fn reap_lost(&mut self, ops: &mut dyn TwinOps) -> Vec<WindowId> {
        let mut reaped = Vec::new();
        for key in ops.lost() {
            let ids: Vec<WindowId> = self
                .placed
                .iter()
                .filter(|(_, placement)| placement.key == key)
                .map(|(id, _)| *id)
                .collect();
            for id in ids {
                self.placed.remove(&id);
                if self.windows.restore(id).is_ok() {
                    reaped.push(id);
                }
            }
            ops.discard(key);
        }
        reaped
    }

    fn resize_placed(
        &mut self,
        ops: &mut dyn TwinOps,
        id: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError> {
        validate_size_scale(size, scale)?;
        let placement = self
            .placed
            .get(&id)
            .cloned()
            .ok_or(PlatformError::NotFound)?;
        let mode = twin_mode(size, scale)?;
        // The twin is always 1920x1080 and its mm depend on the scale alone, so the full mode
        // changes only on a scale change. Ordinary resizes never re-create the twin (no flash).
        let outcome = if mode == placement.mode {
            self.windows.resize(id, clamp_to_twin(size, mode), scale)
        } else {
            self.remode(ops, id, size, mode)
        };
        match outcome {
            Ok(parked) => Ok(parked),
            Err(cause) => {
                // `remode` has already stored the twin's current key, so this discards the right twin.
                let key = self.placed.remove(&id).map(|placement| placement.key);
                Err(self.rollback(ops, id, key, cause))
            }
        }
    }

    /// A mode change: the twin is re-moded, then the window is placed on it again.
    fn remode(
        &mut self,
        ops: &mut dyn TwinOps,
        id: WindowId,
        size: PixelSize,
        mode: TwinMode,
    ) -> Result<Parked, PlatformError> {
        let key = self
            .placed
            .get(&id)
            .map(|placement| placement.key)
            .ok_or(PlatformError::NotFound)?;
        let twin = ops.resize(key, mode).map_err(|error| fallback(&error))?;
        // From here on the twin has the new key and mode, so a rollback must discard those.
        if let Some(placement) = self.placed.get_mut(&id) {
            placement.key = twin.key;
            placement.mode = mode;
        }
        let (display, rect) = locate_twin(ops, &twin)?;
        let observed = self.windows.inspect(id).map_err(window_failure)?;
        let parked = self.move_onto_twin(id, &observed, display, rect, mode, size)?;
        if let Some(placement) = self.placed.get_mut(&id) {
            placement.display = display;
            placement.rect = rect;
        }
        Ok(parked)
    }

    /// Moves the window onto the located twin and checks that it landed there. Any failure is the
    /// twin path's failure, so the caller rolls back.
    fn move_onto_twin(
        &mut self,
        id: WindowId,
        observed: &Observed,
        display: DisplayId,
        rect: [i32; 4],
        mode: TwinMode,
        size: PixelSize,
    ) -> Result<Parked, PlatformError> {
        let outer =
            twin_outer(observed, rect, clamp_to_twin(size, mode)).map_err(|_| unavailable())?;
        let parked = self.windows.relocate(id, outer).map_err(window_failure)?;
        if parked.kind != ParkingKind::Twin || parked.display != display {
            return Err(unavailable());
        }
        Ok(parked)
    }

    fn restore_placed(&mut self, ops: &mut dyn TwinOps, id: WindowId) -> Result<(), PlatformError> {
        let Some(key) = self.placed.get(&id).map(|placement| placement.key) else {
            return self.windows.restore(id);
        };
        match self.windows.restore(id) {
            Ok(()) => {
                self.placed.remove(&id);
                // A failed remove is not fatal: the twin is discarded, so Windows moves its windows
                // back to a real monitor and the handle close removes it.
                if ops.remove(key).is_err() {
                    ops.discard(key);
                }
                Ok(())
            }
            Err(error) => {
                ops.discard(key);
                self.placed.remove(&id);
                Err(error)
            }
        }
    }

    /// Undoes a failed placement: the window goes back first, then the twin is discarded. If the
    /// window restore fails, the journal keeps its entry and the error says so.
    fn rollback(
        &mut self,
        ops: &mut dyn TwinOps,
        id: WindowId,
        key: Option<TwinKey>,
        cause: PlatformError,
    ) -> PlatformError {
        let restored = self.windows.restore(id);
        if let Some(key) = key {
            ops.discard(key);
        }
        match restored {
            Ok(()) => cause,
            Err(error) => PlatformError::Backend(format!(
                "{cause}; rollback failed: {error}; journal retained"
            )),
        }
    }
}

fn unavailable() -> PlatformError {
    PlatformError::Unsupported(TWIN_UNAVAILABLE_REASON)
}

/// A window-side failure after the twin changed. The UIPI preflight's `SecureInput` is refused
/// as itself, with no fallback. Anything else means the twin path failed, which falls back to M1.
fn window_failure(error: PlatformError) -> PlatformError {
    match error {
        PlatformError::SecureInput => error,
        _ => unavailable(),
    }
}

fn locate_twin(
    ops: &mut dyn TwinOps,
    twin: &TwinDisplay,
) -> Result<(DisplayId, [i32; 4]), PlatformError> {
    ops.locate(twin).map_err(|_| unavailable())
}

fn out_of_range() -> PlatformError {
    PlatformError::Backend("twin placement is out of range".into())
}

/// The outer rect that puts the window's visible frame at the twin's top-left, `size` visible
/// pixels big. The frame borders stay as observed. The visible frame must fit on the twin.
pub fn twin_outer(
    observed: &Observed,
    twin_rect: [i32; 4],
    size: PixelSize,
) -> Result<[i32; 4], PlatformError> {
    validate_size_scale(size, 1.0)?;
    rect_size(observed.outer)?;
    rect_size(observed.visible)?;
    let twin = rect_size(twin_rect)?;
    if size.width > twin.width || size.height > twin.height {
        return Err(PlatformError::Backend(
            "the window is larger than its twin display".into(),
        ));
    }
    let left = i64::from(observed.visible[0]) - i64::from(observed.outer[0]);
    let top = i64::from(observed.visible[1]) - i64::from(observed.outer[1]);
    let right = i64::from(observed.outer[2]) - i64::from(observed.visible[2]);
    let bottom = i64::from(observed.outer[3]) - i64::from(observed.visible[3]);
    let x = i64::from(twin_rect[0]);
    let y = i64::from(twin_rect[1]);
    let values = [
        x - left,
        y - top,
        x + i64::from(size.width) + right,
        y + i64::from(size.height) + bottom,
    ];
    let mut outer = [0; 4];
    for (slot, value) in outer.iter_mut().zip(values) {
        *slot = i32::try_from(value).map_err(|_| out_of_range())?;
    }
    rect_size(outer)?;
    Ok(outer)
}

/// The window's size, capped at the twin mode's pixels on each axis.
pub fn clamp_to_twin(size: PixelSize, mode: TwinMode) -> PixelSize {
    PixelSize::new(size.width.min(mode.width), size.height.min(mode.height))
}

/// The one twin probe with this GDI name and rect. None when there is none. Two or more are
/// ambiguous and refused.
pub fn find_twin_probe<'a>(
    probes: &'a [MonitorProbe],
    gdi_name: &str,
    rect: [i32; 4],
) -> Result<Option<&'a MonitorProbe>, PlatformError> {
    let mut matching = probes
        .iter()
        .filter(|probe| probe.twin && probe.name == gdi_name && probe.rc_monitor == rect);
    let Some(probe) = matching.next() else {
        return Ok(None);
    };
    if matching.next().is_some() {
        return Err(PlatformError::Backend(
            "more than one twin display probe matches".into(),
        ));
    }
    Ok(Some(probe))
}
