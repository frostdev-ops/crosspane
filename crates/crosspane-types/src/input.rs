//! Input primitives shared by capture, injection, routing and the wire protocol (03 §3).

use serde::{Deserialize, Serialize};

use crate::geom::VectorLogical;

/// Where a scroll gesture is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ScrollPhase {
    /// A wheel detent or other step with no gesture around it.
    Discrete,
    /// Fingers touched the touchpad; a gesture may follow (macOS `MayBegin`).
    MayBegin,
    /// A touchpad gesture began, continued, ended, or was cancelled.
    Began,
    Changed,
    Ended,
    Cancelled,
    /// Inertial scrolling after the fingers lifted.
    MomentumBegan,
    MomentumChanged,
    MomentumEnded,
}

/// One scroll step.
///
/// - Direction: positive `y` moves the content as "scroll up" (away from the user) does, and
///   positive `x` as "scroll right" (the HID and Windows `WHEEL_DELTA` convention). Capture applies
///   the controller's natural-scrolling setting once, so the delta is what the user intends;
///   injectors don't invert it again.
/// - `v120` and `pixels` are two representations of the same displacement and are never added.
///   Injectors use `pixels` where the OS accepts smooth scrolling and `v120` otherwise.
/// - Lifecycle events with zero displacement (`MayBegin`, `Ended`, `Cancelled`, …) still matter
///   and must be forwarded.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScrollDelta {
    /// High-resolution wheel units: 120 per detent.
    pub v120_x: i32,
    pub v120_y: i32,
    /// Smooth touchpad deltas in logical pixels of the controller's display, if the device has them.
    /// The target applies them 1:1 in its own logical units.
    pub pixels: Option<VectorLogical>,
    pub phase: ScrollPhase,
    /// Scrolling stopped on this axis (Wayland `axis_stop`), so kinetic scrolling may start.
    pub stop_x: bool,
    pub stop_y: bool,
}

/// The toggle state of the lock keys, synchronised when control moves to a target (04 §8).
///
/// `None` means unknown or not supported when reading (macOS has no Num or Scroll Lock state), and
/// "leave unchanged" when used as a request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LockKeys {
    pub caps_lock: Option<bool>,
    pub num_lock: Option<bool>,
    pub scroll_lock: Option<bool>,
}
