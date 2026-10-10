//! `OverlayHost` through the Shell bridge (WP-G1.5): the controller HUD and the target indicator.
//!
//! `show` maps the overlay's display to a point inside it (the centre of its logical rect, from
//! the displays snapshot function) and calls `ShowOverlay`; `OverlayState` signals become
//! `OverlayEvent::Visible`/`Unavailable`. A `Lost` bridge emits `Unavailable` for every shown
//! overlay. An unknown display is `PlatformError::NotFound`.

use std::sync::Arc;

use crosspane_platform::{EventSink, Overlay, OverlayEvent, OverlayHost, OverlayId, PlatformError};

use super::shell::ShellBridge;
use crate::portal::eis::DisplaysFn;

/// Overlays drawn by the Crosspane Shell extension.
#[derive(Debug)]
pub struct GnomeOverlay {}

impl GnomeOverlay {
    pub fn new(bridge: ShellBridge, displays: DisplaysFn) -> GnomeOverlay {
        let _ = (bridge, displays);
        GnomeOverlay {}
    }
}

impl OverlayHost for GnomeOverlay {
    fn subscribe(&mut self, sink: Arc<dyn EventSink<OverlayEvent>>) -> Result<(), PlatformError> {
        let _ = sink;
        Err(PlatformError::Unsupported(
            "GNOME overlay not implemented yet",
        ))
    }

    fn show(&mut self, id: OverlayId, overlay: &Overlay) -> Result<(), PlatformError> {
        let _ = (id, overlay);
        Err(PlatformError::Unsupported(
            "GNOME overlay not implemented yet",
        ))
    }

    fn hide(&mut self, id: OverlayId) -> Result<(), PlatformError> {
        let _ = id;
        Ok(())
    }
}
