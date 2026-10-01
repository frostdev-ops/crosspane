//! E2 capture of window pixels (03 §4.2). Frozen by WP-2.1 (docs/wp/E2-v0.md).
//!
//! v0 delivers CPU frames (Hyprland SHM, a mapped IOSurface copy on macOS). GPU frames are v1.

use std::sync::Arc;

use crosspane_types::geom::{PixelRect, PixelSize};
use crosspane_types::id::{DisplayId, WindowId};
use crosspane_types::time::MonoTime;

use crate::{EventSink, PlatformError};

/// What to capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CaptureTarget {
    /// A whole display (Hyprland M2: the twin output).
    Display(DisplayId),
    /// One window with its child windows (macOS M1).
    Window(WindowId),
}

/// Identifies one capture stream on this node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StreamId(pub u64);

/// One captured image: 8-bit **BGRA** (alpha ignored), rows of `stride` bytes, top row first.
#[derive(Clone, Debug)]
pub struct Frame {
    pub size: PixelSize,
    pub stride: u32,
    pub pixels: Arc<[u8]>,
    /// What changed since the previous frame of this stream, if the OS reported it. `None` means
    /// unknown: treat the whole frame as damaged.
    pub damage: Option<Vec<PixelRect>>,
    /// Capture time on this node's monotonic clock.
    pub at: MonoTime,
}

/// The cursor the pointer shows over captured content (03 §4.6, WP-2.16): 8-bit **BGRA** with
/// straight (not premultiplied) alpha, rows of `size.width * 4` bytes, top row first, at the
/// captured content's pixel density. `hotspot` is the click point in pixels from the top-left
/// corner, inside the image. Each side is at most 256 pixels (backends scale a larger cursor
/// down).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorImage {
    pub size: PixelSize,
    pub hotspot: (u32, u32),
    pub pixels: Arc<[u8]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StreamEndReason {
    Requested,
    /// The target went away (window closed, display removed).
    TargetGone,
    /// The I/O gate closed (lock, sleep, panic; 04 §7) or permission was revoked.
    Blocked,
    Failed,
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum FrameEvent {
    Frame {
        stream: StreamId,
        frame: Frame,
    },
    Ended {
        stream: StreamId,
        reason: StreamEndReason,
    },
    /// The cursor shape over the captured content changed (WP-2.16). Sent when the pointer is over
    /// the content and its shape differs from the last one this stream reported (so at least
    /// once, the first time the pointer is over it); `None` when the app hid the cursor. Never
    /// sent while the pointer is elsewhere. A backend that can't observe the cursor never sends
    /// it, and the destination keeps its default cursor.
    Cursor {
        stream: StreamId,
        cursor: Option<CursorImage>,
    },
}

/// Captures pixels for projection.
///
/// - **Gate:** backends receive the node's `IoGate` at construction. While it is closed they
///   deliver no frames, and they end every stream with `Blocked` when it closes (never capture a
///   lock screen, 04 §7).
/// - Frames are delivered at most `max_fps` per second; a backend may skip frames when the sink is
///   behind (only the newest matters).
pub trait FrameCapture: Send {
    /// Start capturing `target`, cropped to `crop` (device pixels on the target) if given.
    fn start(
        &mut self,
        target: CaptureTarget,
        crop: Option<PixelRect>,
        max_fps: u32,
        sink: Arc<dyn EventSink<FrameEvent>>,
    ) -> Result<StreamId, PlatformError>;

    /// Change a running stream's crop (after a resize).
    fn set_crop(&mut self, stream: StreamId, crop: Option<PixelRect>) -> Result<(), PlatformError>;

    /// Stop a stream; emits `Ended { Requested }`.
    fn stop(&mut self, stream: StreamId) -> Result<(), PlatformError>;
}
