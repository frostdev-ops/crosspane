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
use crosspane_types::id::{DisplayId, ProjectionId, WindowId};
use crosspane_types::input::ScrollDelta;

use crate::msg::Refusal;

/// Where a proxy's content, or a returned window's content, goes (DRAG-v0 D-9): its top-left at
/// (`x`, `y`) device pixels on the receiver's `display`. `drag`: a continuation follows (DRAG-v0
/// D-5); always `false` in `ReturnAt`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProxyPlacement {
    pub display: DisplayId,
    pub x: i32,
    pub y: i32,
    pub drag: bool,
}

/// The protocol feature that enables the messages below (DRAG-v0 D-8). Neither side sends them
/// unless both `Hello`s carry it.
pub const DRAG_FEATURE: &str = "drag/0";

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
    /// Source → destination: `Start`, with a placement for the proxy and, with `place.drag`, the
    /// continuation `token` and `anchor`: the press point relative to the content's top-left, in the
    /// destination's device pixels, always inside the content (the grab point clamped into the content
    /// inset by 8 px; DRAG-v0 §3). If the destination clamps the placement, the anchor moves with the
    /// content. Without `place.drag`, `token` and `anchor` are ignored.
    StartAt {
        projection: ProjectionId,
        window: WindowSummary,
        size: PixelSize,
        place: ProxyPlacement,
        token: u32,
        anchor: (i32, i32),
    },
    /// Destination → source: `Close { reason: Returned }`, restoring the window at `place`.
    ReturnAt {
        projection: ProjectionId,
        place: ProxyPlacement,
    },
    /// Destination → source: the proxy for `token` is placed on `display`, and `position` (device
    /// pixels on that display) is its placed content origin plus `anchor`: the continuation press point.
    DragReady {
        projection: ProjectionId,
        token: u32,
        display: DisplayId,
        position: PointDevice,
    },
    /// Source → destination: continuation `token` is dropped; disarm, and ignore it from now on.
    DragCancel {
        projection: ProjectionId,
        token: u32,
    },
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
    /// Destination → source: the proxy's content area changed. `request` numbers this
    /// projection's resize requests: 1 for the first, one more for each later one.
    Resize {
        projection: ProjectionId,
        request: u32,
        size: PixelSize,
        scale: f64,
    },
    /// Source → destination: the window's actual content size and parking, after start or a
    /// resize. Frames carry their own size too. `answers` is the `request` of the newest Resize
    /// this geometry reflects (0: none yet, or a source that predates request numbers).
    Geometry {
        projection: ProjectionId,
        size: PixelSize,
        parking: ParkingKind,
        answers: u32,
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
    /// Destination → source (WP-2.43): where the proxy's content is on the destination.
    /// `generation` grows by one with every *change* for this projection and never restarts
    /// while the projection lives; a report re-sent after `Accepted` on a reconnect repeats the
    /// newest report with its generation unchanged. The source keeps a high-water mark and
    /// accepts a report only if its generation is higher, or equal with identical contents.
    /// `display`, `origin`, `size`: the content's display, its top-left in that display's device
    /// pixels, and its current size in device pixels. `display: None`: the proxy is on no display
    /// right now (minimised, fully occluded, or the host can't tell); `origin` and `size` are
    /// then stale. Sent once the proxy is open, on every change, and again after `Accepted` on a
    /// reconnect. A destination whose generation would overflow sends one final report with
    /// `generation: u32::MAX` and `display: None`, then nothing more.
    ProxyPlaced {
        projection: ProjectionId,
        generation: u32,
        display: Option<DisplayId>,
        origin: PointDevice,
        size: PixelSize,
    },
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
