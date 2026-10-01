//! E2 window projection messages (docs/wp/E2-v0.md). Frozen by WP-2.1; encoded by WP-2.3.
//!
//! - Control: [`ProjectionMessage`], carried in [`crate::msg::ControlMessage::Projection`].
//! - Input to a projected window: [`ProjInput`], carried as [`crate::msg::InputMessage::Proj`] on
//!   the input stream (reliable, ordered). Pointer motion is coalesced by the destination to at most
//!   120 Hz (v0: no datagram path for E2).
//! - Pixels: media streams (stream type 0x03), format owned by `crosspane-media`.
//!
//! Roles: the **source** owns the real window. The **destination** shows the proxy and owns its
//! geometry (03 §4.4).

use crosspane_types::geom::{PixelSize, PointDevice};
use crosspane_types::hid::{HidUsage, MouseButton};
use crosspane_types::id::{ProjectionId, WindowId};
use crosspane_types::input::ScrollDelta;

use crate::msg::Refusal;

/// What the destination needs to know about the window being projected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowSummary {
    pub title: String,
    /// Hyprland window class or macOS bundle identifier.
    pub app_id: String,
}

/// One window a source lets a peer pull (`Capability::WindowBrowse`, WP-2.13).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowsableWindow {
    /// The source's id for the window, opaque to the destination: it only sends it back in `Pull`.
    pub window: WindowId,
    pub summary: WindowSummary,
    /// The window's content size in the source's device pixels.
    pub size: PixelSize,
}

/// At most this many windows in one `WindowList`; a source with more sends the first ones in its
/// own order.
pub const MAX_BROWSE_WINDOWS: usize = 256;

/// How the source keeps the real window while it is projected (03 §4.3), reported to the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ParkingKind {
    /// M2: on a hidden twin display; invisible on the source.
    Twin,
    /// M1: in place on the source's screen (fallback; visible there).
    Mirror,
}

/// Why a projection ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ProjectionEndReason {
    /// The user returned the window (closed the proxy, or `crosspanectl return`).
    Returned,
    /// The app closed the window on the source.
    WindowClosed,
    /// Permission revoked (04 §2).
    Revoked,
    /// Either node locked or slept (04 §7).
    Locked,
    /// The link to the peer was lost.
    LinkLost,
    /// Capture, parking or rendering failed.
    Failed,
}

#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ProjectionMessage {
    /// Source → destination: offer to show a window. `size` is the window's current content size in
    /// the source's device pixels, as a starting point for the proxy.
    Start {
        projection: ProjectionId,
        window: WindowSummary,
        size: PixelSize,
    },
    /// Destination → source: accepted; the proxy's content area is `size` device pixels at `scale`
    /// (device pixels per logical unit on the destination display).
    Accepted {
        projection: ProjectionId,
        size: PixelSize,
        scale: f64,
    },
    /// Destination → source: refused (e.g. `WindowPresent` not granted).
    Refused {
        projection: ProjectionId,
        reason: Refusal,
    },
    /// Destination → source: the proxy's content area changed.
    Resize {
        projection: ProjectionId,
        size: PixelSize,
        scale: f64,
    },
    /// Source → destination: the window's actual content size and parking, after start or a
    /// resize. Frames carry their own size too.
    Geometry {
        projection: ProjectionId,
        size: PixelSize,
        parking: ParkingKind,
    },
    /// Source → destination: the window's title changed.
    Title {
        projection: ProjectionId,
        title: String,
    },
    /// Destination → source: the proxy gained or lost keyboard focus.
    Focus {
        projection: ProjectionId,
        focused: bool,
    },
    /// Destination → source: send a key frame (full frame) next.
    KeyFrameRequest { projection: ProjectionId },
    /// Source → destination: the projection is over (window closed, returned locally, lock, …).
    /// The destination closes the proxy.
    End {
        projection: ProjectionId,
        reason: ProjectionEndReason,
    },
    /// Destination → source: the destination ended the projection (proxy closed by the user, lock,
    /// …). The source restores the window (04 §8 invariant 4). A separate variant from `End` because
    /// two nodes can project to each other with equal `ProjectionId`s: `End` always names a
    /// projection the *sender* is the source of, `Close` one the *receiver* is the source of.
    Close {
        projection: ProjectionId,
        reason: ProjectionEndReason,
    },
    /// Destination → source: list the windows this node may pull. Needs `WindowBrowse` granted by
    /// the source. `request` is the requester's, echoed in the answer.
    ListWindows { request: u32 },
    /// Source → destination: the answer to `ListWindows` (at most [`MAX_BROWSE_WINDOWS`]).
    WindowList {
        request: u32,
        windows: Vec<BrowsableWindow>,
    },
    /// Destination → source: project `window` to me. Needs `WindowBrowse`. The source answers
    /// with a normal `Start` (the projection then runs as if the source's user had started it),
    /// or with `BrowseRefused`.
    Pull { request: u32, window: WindowId },
    /// Source → destination: `ListWindows` or `Pull` number `request` refused.
    BrowseRefused { request: u32, reason: Refusal },
}

/// Destination → source input for a projected window, on the input stream. `seq` increases per
/// projection from 1. The source acknowledges nothing: release guarantees come from `Held`
/// heartbeats and the source's lease, exactly as in E1 (04 §8 invariants 1–2).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ProjInput {
    Key {
        projection: ProjectionId,
        seq: u32,
        usage: HidUsage,
        down: bool,
    },
    Button {
        projection: ProjectionId,
        seq: u32,
        button: MouseButton,
        down: bool,
        /// Where the pointer was, in device pixels of the window's content.
        position: PointDevice,
    },
    Scroll {
        projection: ProjectionId,
        seq: u32,
        delta: ScrollDelta,
        position: PointDevice,
    },
    /// The pointer moved over the proxy (coalesced to ≤ 120 Hz by the destination).
    Motion {
        projection: ProjectionId,
        seq: u32,
        /// Device pixels of the window's content, origin top-left.
        position: PointDevice,
    },
    /// Heartbeat: everything the destination holds down in this projection. Every 50 ms while
    /// anything is held, every 250 ms otherwise (crosspane-input timing).
    Held {
        projection: ProjectionId,
        seq: u32,
        keys: Vec<HidUsage>,
        buttons: Vec<MouseButton>,
    },
}
