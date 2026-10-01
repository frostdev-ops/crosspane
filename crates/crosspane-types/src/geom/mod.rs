//! Coordinate spaces and geometry (03 §3, §5).
//!
//! Three unit spaces, kept apart by the type system:
//! - [`Mm`]: the layout canvas in millimetres, used for E1 edge mapping.
//! - [`Logical`]: an OS's logical coordinates (macOS points, Wayland logical pixels) on one node's
//!   desktop.
//! - [`Device`]: physical pixels of one display, origin at its top-left corner.
//!
//! Display-level transforms between the spaces are methods on [`DisplayGeometry`] (`transform.rs`).

mod transform;

use serde::{Deserialize, Serialize};

pub use euclid;

/// Millimetres on the layout canvas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mm {}

/// An OS's logical coordinates on one node's desktop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Logical {}

/// Physical pixels of one display, origin at its top-left corner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Device {}

pub type PointMm = euclid::Point2D<f64, Mm>;
pub type VectorMm = euclid::Vector2D<f64, Mm>;
pub type SizeMm = euclid::Size2D<f64, Mm>;
pub type RectMm = euclid::Rect<f64, Mm>;

pub type PointLogical = euclid::Point2D<f64, Logical>;
pub type VectorLogical = euclid::Vector2D<f64, Logical>;
pub type SizeLogical = euclid::Size2D<f64, Logical>;
pub type RectLogical = euclid::Rect<f64, Logical>;

/// A position in device pixels; fractional for sub-pixel pointer positions.
pub type PointDevice = euclid::Point2D<f64, Device>;
pub type VectorDevice = euclid::Vector2D<f64, Device>;
/// Whole-pixel dimensions of a display, window or surface.
pub type PixelSize = euclid::Size2D<u32, Device>;
/// A whole-pixel rectangle (damage, tiles): `min` inclusive, `max` exclusive.
pub type PixelRect = euclid::Box2D<i32, Device>;

/// Geometry of one display (03 §5).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct DisplayGeometry {
    /// Physical panel size in millimetres (EDID, user-correctable).
    pub physical_size: SizeMm,
    /// Resolution in device pixels.
    pub pixel_size: PixelSize,
    /// Device pixels per logical unit: 2.0 on Retina, 1.5 for 150 % fractional scaling.
    pub scale: f64,
    /// Top-left corner of this display in its node's logical desktop space.
    pub logical_origin: PointLogical,
}
