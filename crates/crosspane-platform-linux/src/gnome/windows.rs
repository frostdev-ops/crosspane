//! `WindowSource` through the Crosspane Shell extension bridge (WP-G2.3).
//!
//! - `WindowId(shell id)`. Ids are only meaningful within the bridge's epoch: when the bridge
//!   reports `Lost` (Shell restart, extension disabled), every known window is `Removed` and the
//!   source reconnects with backoff (1 s doubling to 30 s); windows seen after that are `Added`
//!   anew. An id from an old epoch is never reused for a new window.
//! - `WindowInfo`: `app_id`, `title`, `pid` (`None` for 0), `frame` = the bridge rect in logical
//!   coordinates, `display` = the display containing the frame's centre (displays snapshot),
//!   `state` = `Minimized`/`Fullscreen`/`Normal`, `role` = `Toplevel` (the bridge lists only
//!   normal and dialog windows; dialogs are `Dialog` once the bridge reports type — v1 doesn't, so
//!   `Toplevel`), `parent` = `None`.
//! - `subscribe` (once): current windows as `Added` plus the current `Focused`, then diffs of
//!   each `ListWindows` refresh triggered by `WindowsChanged` (`Added`/`Changed`/`Removed`,
//!   `Focused` when the focused id changes). Refreshes run on one worker thread, coalesced.
//! - `activate` → bridge `Activate`; unknown is `NotFound`.

use std::sync::Arc;

use crosspane_platform::{EventSink, PlatformError, WindowEvent, WindowInfo, WindowSource};
use crosspane_types::id::WindowId;

use super::shell::ShellBridge;
use crate::portal::eis::DisplaysFn;

/// Windows listed by the Crosspane Shell extension.
#[derive(Debug)]
pub struct GnomeWindows {}

impl GnomeWindows {
    pub fn new(bridge: ShellBridge, displays: DisplaysFn) -> Result<GnomeWindows, PlatformError> {
        let _ = (bridge, displays);
        Err(PlatformError::Unsupported(
            "GNOME windows not implemented yet",
        ))
    }
}

impl WindowSource for GnomeWindows {
    fn windows(&self) -> Result<Vec<WindowInfo>, PlatformError> {
        Err(PlatformError::Unsupported(
            "GNOME windows not implemented yet",
        ))
    }

    fn focused(&self) -> Result<Option<WindowId>, PlatformError> {
        Err(PlatformError::Unsupported(
            "GNOME windows not implemented yet",
        ))
    }

    fn activate(&mut self, window: WindowId) -> Result<(), PlatformError> {
        let _ = window;
        Err(PlatformError::Unsupported(
            "GNOME windows not implemented yet",
        ))
    }

    fn subscribe(&mut self, sink: Arc<dyn EventSink<WindowEvent>>) -> Result<(), PlatformError> {
        let _ = sink;
        Err(PlatformError::Unsupported(
            "GNOME windows not implemented yet",
        ))
    }
}
