//! E2 source side: the windows a node can project, and where it keeps them while projected
//! (03 §4.1, §4.3). Frozen by WP-2.1 (docs/wp/E2-v0.md).

use std::sync::Arc;

use crosspane_types::geom::{PixelRect, PixelSize, RectLogical};
use crosspane_types::id::{DisplayId, WindowId};

use crate::{EventSink, PlatformError};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WindowState {
    Normal,
    Minimized,
    Fullscreen,
    /// On a hidden workspace or Space, or parked by Crosspane.
    Hidden,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WindowRole {
    Toplevel,
    Dialog,
    /// Menus, tooltips, IME candidate windows: never projected on their own.
    Popup,
    Other,
}

/// One top-level window on this node.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowInfo {
    pub id: WindowId,
    pub title: String,
    /// Hyprland window class, macOS bundle identifier.
    pub app_id: String,
    pub pid: Option<u32>,
    /// The display the window is (mostly) on.
    pub display: Option<DisplayId>,
    /// Frame in the node's logical desktop coordinates (03 §5).
    pub frame: RectLogical,
    pub state: WindowState,
    pub role: WindowRole,
    pub parent: Option<WindowId>,
}

#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum WindowEvent {
    Added(WindowInfo),
    Changed(WindowInfo),
    Removed(WindowId),
    /// Keyboard focus moved (`None`: no window of this node has focus).
    Focused(Option<WindowId>),
}

/// Lists and watches this node's windows.
pub trait WindowSource: Send {
    /// Every projectable window (top-levels and dialogs; popups are included only as children).
    fn windows(&self) -> Result<Vec<WindowInfo>, PlatformError>;

    /// The window with keyboard focus.
    fn focused(&self) -> Result<Option<WindowId>, PlatformError>;

    /// Give `window` keyboard focus (and raise it under M1), for forwarding keys (03 §4.5). Never
    /// un-parks a parked window.
    fn activate(&mut self, window: WindowId) -> Result<(), PlatformError>;

    /// The current list first (as `Added`), then changes. Called once.
    fn subscribe(&mut self, sink: Arc<dyn EventSink<WindowEvent>>) -> Result<(), PlatformError>;
}

/// Where a parked window lives (03 §4.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ParkingKind {
    /// M2: alone on a hidden twin display; invisible on this node's screens.
    Twin,
    /// M1: in place on this node's screen (fallback, reported to the user).
    Mirror,
}

/// A parked window's geometry, for capture and input mapping.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Parked {
    pub window: WindowId,
    pub kind: ParkingKind,
    /// The display the window's content is on (the twin display for M2).
    pub display: DisplayId,
    /// The window's content on `display`, in device pixels. Capture crops to it; projected input at
    /// content position `p` is injected at `content.min + p` on `display`.
    pub content: PixelRect,
}

/// Keeps projected windows out of the source user's way (D2), and gives them back (04 §8
/// invariant 4: no window is lost).
///
/// - Every change is journaled on disk *before* it is made, so a crash can be undone.
/// - Implementations never minimise and never close the app's window (03 §4.1).
pub trait WindowParking: Send {
    /// Park `window` with a content size of `size` device pixels at `scale` device pixels per
    /// logical unit (the destination's scale: a twin display uses it, so the app renders at the
    /// destination's density). Best effort: the app may refuse a size; the returned geometry is
    /// what it took.
    fn park(
        &mut self,
        window: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError>;

    /// Resize a parked window's content (and change the scale). Returns the geometry it took.
    fn resize(
        &mut self,
        window: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError>;

    /// The current geometry of a parked window (it may have moved or resized itself).
    fn geometry(&self, window: WindowId) -> Result<Parked, PlatformError>;

    /// Return `window` to where it was before parking. Idempotent; `Ok` for a window that has
    /// since closed.
    fn restore(&mut self, window: WindowId) -> Result<(), PlatformError>;

    /// Undo everything a previous run left parked, from the journal, and remove leftover twin
    /// displays. The agent calls this at startup before anything else. Returns the windows
    /// restored.
    fn recover(&mut self) -> Result<Vec<WindowId>, PlatformError>;
}
