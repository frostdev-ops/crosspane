//! `FrameCapture` of single windows on GNOME (WP-G2.2/G2.3a): a window identified by the Shell
//! bridge, captured from its monitor's ScreenCast stream and cropped to the window.
//!
//! Window identity comes from the bridge (`WindowId` = Shell id, `ShellEpoch`-qualified), never
//! from portal metadata (WP-G0.1 rulings). This is the M1 mirror path: the window stays visible on
//! its source monitor, so whatever covers it is captured too (the reported fallback).
//!
//! - `start(Display(id), ..)` passes through to the inner [`PortalScreenCast`] unchanged.
//! - `start(Window(id), crop, max_fps, sink)`: look the window up (bridge `ListWindows`, 2 s);
//!   unknown is `NotFound`. Pick the display containing the window frame's centre (displays
//!   snapshot; none is `NotFound`), convert the frame to device pixels on that display
//!   (logical − display origin, × scale, rounded; clamped to the display), intersect with the
//!   caller's `crop` if given (the caller's crop is relative to the window's content, i.e. it is
//!   offset by the window's device-pixel origin), and start an inner `Display` stream with that
//!   crop. Returns an outer `StreamId` of this adapter's own numbering.
//! - **Following the window.** On every bridge `WindowsChanged` (coalesced on one worker), re-read
//!   the window: a new rect on the same display updates the inner crop (`set_crop`); a window now
//!   on another display stops the inner stream and starts one on the new display under the same
//!   outer `StreamId` (frames keep flowing to the same sink); a window that is gone, or a bridge
//!   `Lost`, ends the outer stream with the frozen "source ended" reason (`StreamEndReason`, the
//!   variant the Hyprland backend uses when the captured window closes).
//! - `set_crop(outer, crop)`: store the caller crop and re-apply it against the current window
//!   rect. `stop(outer)`: stop the inner stream, emit nothing extra beyond what the inner stream
//!   emits (`Ended { Requested }` exactly once, through the same sink), forget the window.
//! - Events from inner streams are forwarded to the caller's sink, except an inner `Ended` caused
//!   by a display switch, which is swallowed.
//! - The gate is the inner capture's business (it ends streams with `Blocked`).

use std::sync::Arc;

use crosspane_platform::{
    CaptureTarget, EventSink, FrameCapture, FrameEvent, PlatformError, StreamId,
};
use crosspane_types::geom::PixelRect;

use super::shell::ShellBridge;
use crate::portal::eis::DisplaysFn;
use crate::portal::screencast::PortalScreenCast;

/// Window capture on GNOME: a monitor stream cropped to a bridge-identified window.
#[derive(Debug)]
pub struct GnomeWindowCapture {}

impl GnomeWindowCapture {
    pub fn new(
        inner: PortalScreenCast,
        bridge: ShellBridge,
        displays: DisplaysFn,
    ) -> Result<GnomeWindowCapture, PlatformError> {
        let _ = (inner, bridge, displays);
        Err(PlatformError::Unsupported(
            "GNOME window capture not implemented yet",
        ))
    }
}

impl FrameCapture for GnomeWindowCapture {
    fn start(
        &mut self,
        target: CaptureTarget,
        crop: Option<PixelRect>,
        max_fps: u32,
        sink: Arc<dyn EventSink<FrameEvent>>,
    ) -> Result<StreamId, PlatformError> {
        let _ = (target, crop, max_fps, sink);
        Err(PlatformError::Unsupported(
            "GNOME window capture not implemented yet",
        ))
    }

    fn set_crop(&mut self, stream: StreamId, crop: Option<PixelRect>) -> Result<(), PlatformError> {
        let _ = (stream, crop);
        Err(PlatformError::NotFound)
    }

    fn stop(&mut self, stream: StreamId) -> Result<(), PlatformError> {
        let _ = stream;
        Ok(())
    }
}
