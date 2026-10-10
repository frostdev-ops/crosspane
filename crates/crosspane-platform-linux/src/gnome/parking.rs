//! M1 mirror parking on GNOME through the Shell bridge (WP-G2.3a, the reported fallback; the
//! VIRTUAL twin, M2, is WP-G2.4).
//!
//! The window stays where it is and stays visible on the source (`ParkingKind::Mirror`).
//!
//! - **Journal first** (04 §8 invariant 4). Before the first change to a window, its original
//!   frame rect (logical), its maximized/fullscreen state as far as the bridge reports it, and the
//!   bridge epoch are appended to the JSON journal at `journal` (written to a temp file, fsynced,
//!   renamed; 0600). An entry is retired only after its restore succeeded or the window is gone.
//! - **park(size, scale)**: the window's content is resized with `MoveResize` keeping its top-left,
//!   to `size / display scale` logical (the destination's `scale` is ignored: M1 renders at the
//!   source display's density). Returns `Parked{kind: Mirror, display, content}` with `content` =
//!   the frame rect converted to device pixels on its display (logical − display origin, × scale,
//!   rounded), `fullscreen` from the bridge.
//! - **resize**: same as park for an already parked window. **set_fullscreen**: v1 of the bridge
//!   has no fullscreen request → `Unsupported`. **geometry**: fresh `ListWindows` read.
//! - **restore / restore_at**: `MoveResize` back to the journaled rect (`restore_at`: same size,
//!   top-left at the requested point converted to logical), then retire the entry. A window that
//!   no longer exists is `Ok` (retired).
//! - **recover**: entries whose epoch equals the bridge's current epoch are restored; entries from
//!   another epoch refer to windows that can't be identified any more and are retired with a
//!   warning (no window was moved off-screen by M1, so none is lost).
//! - Never minimizes and never closes a window.

use std::path::PathBuf;

use crosspane_platform::{Parked, PlatformError, WindowParking};
use crosspane_types::geom::{PixelSize, PointDevice};
use crosspane_types::id::{DisplayId, WindowId};

use super::shell::ShellBridge;
use crate::portal::eis::DisplaysFn;

/// In-place (M1) parking through the Shell bridge.
#[derive(Debug)]
pub struct GnomeMirrorParking {}

impl GnomeMirrorParking {
    pub fn new(
        bridge: ShellBridge,
        displays: DisplaysFn,
        journal: PathBuf,
    ) -> Result<GnomeMirrorParking, PlatformError> {
        let _ = (bridge, displays, journal);
        Err(PlatformError::Unsupported(
            "GNOME mirror parking not implemented yet",
        ))
    }
}

impl WindowParking for GnomeMirrorParking {
    fn park(
        &mut self,
        window: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError> {
        let _ = (window, size, scale);
        Err(PlatformError::Unsupported(
            "GNOME mirror parking not implemented yet",
        ))
    }

    fn resize(
        &mut self,
        window: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError> {
        let _ = (window, size, scale);
        Err(PlatformError::Unsupported(
            "GNOME mirror parking not implemented yet",
        ))
    }

    fn set_fullscreen(&mut self, window: WindowId, fullscreen: bool) -> Result<(), PlatformError> {
        let _ = (window, fullscreen);
        Err(PlatformError::Unsupported(
            "fullscreen through the GNOME Shell bridge v1",
        ))
    }

    fn geometry(&self, window: WindowId) -> Result<Parked, PlatformError> {
        let _ = window;
        Err(PlatformError::Unsupported(
            "GNOME mirror parking not implemented yet",
        ))
    }

    fn restore(&mut self, window: WindowId) -> Result<(), PlatformError> {
        let _ = window;
        Ok(())
    }

    fn restore_at(
        &mut self,
        window: WindowId,
        display: DisplayId,
        origin: PointDevice,
    ) -> Result<(), PlatformError> {
        let _ = (display, origin);
        self.restore(window)
    }

    fn recover(&mut self) -> Result<Vec<WindowId>, PlatformError> {
        Ok(Vec::new())
    }
}
