//! On-screen indicators: the controller HUD and the target indicator (04 §5).

use std::sync::Arc;

use crosspane_types::id::DisplayId;

use crate::{EventSink, PlatformError};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OverlayId(pub u32);

/// Where an overlay sits on its display.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OverlayAnchor {
    TopCenter,
    TopRight,
    BottomRight,
    Center,
}

/// An 8-bit sRGB colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rgb8 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// What an overlay shows: one line of text with an accent colour (the peer node's colour).
#[derive(Clone, Debug, PartialEq)]
pub struct Overlay {
    /// The display to show it on. The engine picks the display (e.g. the one the pointer left
    /// from) and changes it with `show` when needed.
    pub display: DisplayId,
    pub anchor: OverlayAnchor,
    pub text: String,
    pub accent: Rgb8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlayEvent {
    /// The overlay is on screen: configured and presented.
    Visible(OverlayId),
    /// The overlay is not on screen and can't be kept there (surface closed, display gone,
    /// renderer failure). For the capture HUD, the engine ends the capture (04 §8 invariant 5).
    Unavailable(OverlayId),
}

/// Shows overlays that stay above other windows, never take focus or input, and are visible on
/// every workspace or Space.
///
/// `show` succeeding means the request was accepted, not that anything is on screen: the engine
/// starts a capture only after the HUD's `Visible` event.
pub trait OverlayHost: Send {
    /// Start delivering events. Called once.
    fn subscribe(&mut self, sink: Arc<dyn EventSink<OverlayEvent>>) -> Result<(), PlatformError>;

    /// Show an overlay, or update it if `id` is already shown. Emits `Visible` once it is
    /// presented (again after a move to another display).
    fn show(&mut self, id: OverlayId, overlay: &Overlay) -> Result<(), PlatformError>;

    /// Hide an overlay. Hiding one that isn't shown succeeds.
    fn hide(&mut self, id: OverlayId) -> Result<(), PlatformError>;
}
