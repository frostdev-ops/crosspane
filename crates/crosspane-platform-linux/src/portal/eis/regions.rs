//! Absolute-pointer region lookup (pure; no EIS objects).
//!
//! EIS absolute motion is in the compositor's logical space, the same space the regions are
//! announced in: a region is `offset + size` and a coordinate is valid when it lies inside one
//! (the offset is part of the coordinate, nothing is subtracted). A coordinate outside every
//! region is silently dropped by the compositor, so the lookup decides, on the exact `f32` values
//! that go on the wire, whether and where a point can be sent.

use crosspane_platform::PlatformError;
use crosspane_types::geom::{DisplayGeometry, PointDevice};

/// A region of an absolute-pointer device, in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct RegionRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl RegionRect {
    pub(super) fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        RegionRect {
            x: f64::from(x),
            y: f64::from(y),
            width: f64::from(width),
            height: f64::from(height),
        }
    }

    /// A display's rectangle in the desktop's logical space (`logical_origin`, logical size).
    pub(super) fn of_display(geometry: &DisplayGeometry) -> Self {
        let bounds = geometry.logical_bounds();
        RegionRect {
            x: bounds.origin.x,
            y: bounds.origin.y,
            width: bounds.size.width,
            height: bounds.size.height,
        }
    }

    fn contains(&self, x: f64, y: f64) -> bool {
        self.width > 0.0
            && self.height > 0.0
            && x >= self.x
            && x < self.x + self.width
            && y >= self.y
            && y < self.y + self.height
    }

    /// The two share some area (touching edges don't count).
    fn overlaps(&self, other: &RegionRect) -> bool {
        self.width > 0.0
            && self.height > 0.0
            && other.width > 0.0
            && other.height > 0.0
            && self.x < other.x + other.width
            && other.x < self.x + self.width
            && self.y < other.y + other.height
            && other.y < self.y + self.height
    }

    /// The point of the region nearest `(x, y)`, kept strictly inside (by `INSET`), and the
    /// distance to it.
    fn nearest(&self, x: f64, y: f64) -> Option<(f64, f64, f64)> {
        if self.width <= 0.0 || self.height <= 0.0 {
            return None;
        }
        let max_x = (self.x + self.width - INSET).max(self.x);
        let max_y = (self.y + self.height - INSET).max(self.y);
        let nx = x.clamp(self.x, max_x);
        let ny = y.clamp(self.y, max_y);
        Some((nx, ny, (x - nx).hypot(y - ny)))
    }
}

/// How far outside a region a point may be and still be moved onto its edge: one logical pixel,
/// the rounding a fractional scale puts between a display's exact logical size and the integer
/// region the compositor announces for it.
pub(super) const EDGE_TOLERANCE: f64 = 1.0;

/// Kept clear of a region's far edge so the `f32` the compositor sees is still inside it.
const INSET: f64 = 0.01;

/// Where a position goes: the candidate (device) that owns the region and the `f32` coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Target<K> {
    pub key: K,
    pub x: f32,
    pub y: f32,
}

/// The region that contains `(x, y)`, searched in order. `prefer` wins when several candidates
/// contain the point (the device used last).
///
/// A point outside every region may be moved onto a region's edge, but only a region that overlaps
/// `display` (the logical rectangle of the display the caller asked for) and lies within
/// `EDGE_TOLERANCE` of the point: the slack is the rounding on that display's own edge, never a
/// licence to land on a neighbouring display's region. Otherwise there is no target.
pub(super) fn locate<K: Copy + PartialEq>(
    candidates: &[(K, RegionRect)],
    prefer: Option<K>,
    display: &RegionRect,
    x: f64,
    y: f64,
) -> Option<Target<K>> {
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    // Test the values as the compositor will read them.
    let (fx, fy) = (x as f32, y as f32);
    if !fx.is_finite() || !fy.is_finite() {
        return None;
    }
    let (px, py) = (f64::from(fx), f64::from(fy));
    let inside = |c: &&(K, RegionRect)| c.1.contains(px, py);
    let hit = candidates
        .iter()
        .filter(inside)
        .find(|c| Some(c.0) == prefer)
        .or_else(|| candidates.iter().find(inside));
    if let Some(&(key, _)) = hit {
        return Some(Target { key, x: fx, y: fy });
    }
    let mut best: Option<(K, f64, f64, f64)> = None;
    for &(key, region) in candidates {
        if region.overlaps(display)
            && let Some((nx, ny, distance)) = region.nearest(px, py)
            && distance <= EDGE_TOLERANCE
            && best.is_none_or(|(_, _, _, d)| distance < d)
        {
            best = Some((key, nx, ny, distance));
        }
    }
    let (key, nx, ny, _) = best?;
    let target = Target {
        key,
        x: nx as f32,
        y: ny as f32,
    };
    // The clamped point must survive the cast.
    candidates
        .iter()
        .any(|c| c.0 == key && c.1.contains(f64::from(target.x), f64::from(target.y)))
        .then_some(target)
}

/// A display-local device-pixel position as desktop logical coordinates
/// (`logical_origin + position / scale`). The position must be on the display.
pub(super) fn logical_point(
    geometry: &DisplayGeometry,
    position: PointDevice,
) -> Result<(f64, f64), PlatformError> {
    if !geometry.scale.is_finite()
        || geometry.scale <= 0.0
        || geometry.pixel_size.width == 0
        || geometry.pixel_size.height == 0
        || !geometry.logical_origin.x.is_finite()
        || !geometry.logical_origin.y.is_finite()
    {
        return Err(PlatformError::Unsupported("invalid display geometry"));
    }
    if !position.x.is_finite()
        || !position.y.is_finite()
        || position.x < 0.0
        || position.y < 0.0
        || position.x >= f64::from(geometry.pixel_size.width)
        || position.y >= f64::from(geometry.pixel_size.height)
    {
        return Err(PlatformError::Unsupported(
            "position outside display device pixels",
        ));
    }
    let logical = geometry.device_to_logical(position);
    Ok((logical.x, logical.y))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosspane_types::geom::{PixelSize, PointLogical, SizeMm};

    fn region(x: u32, y: u32, w: u32, h: u32) -> RegionRect {
        RegionRect::new(x, y, w, h)
    }

    fn geometry(width: u32, height: u32, scale: f64, ox: f64, oy: f64) -> DisplayGeometry {
        DisplayGeometry {
            physical_size: SizeMm::new(600.0, 340.0),
            pixel_size: PixelSize::new(width, height),
            scale,
            logical_origin: PointLogical::new(ox, oy),
        }
    }

    /// Where a test doesn't care which display was asked for: all of the plane.
    fn everywhere() -> RegionRect {
        RegionRect {
            x: -1e9,
            y: -1e9,
            width: 2e9,
            height: 2e9,
        }
    }

    /// `super::locate` for the display-agnostic tests.
    fn locate<K: Copy + PartialEq>(
        candidates: &[(K, RegionRect)],
        prefer: Option<K>,
        x: f64,
        y: f64,
    ) -> Option<Target<K>> {
        super::locate(candidates, prefer, &everywhere(), x, y)
    }

    #[test]
    fn edge_slack_is_only_taken_from_a_region_of_the_display_asked_for() {
        // Two displays 3 logical pixels apart, one region each.
        let left = RegionRect::new(0, 0, 100, 100);
        let right = RegionRect::new(103, 0, 100, 100);
        let candidates = [(1u32, left), (2, right)];
        // Past the left display's edge, asked for on the left display: its own region takes it.
        let target = super::locate(&candidates, None, &left, 100.5, 50.0).unwrap();
        assert_eq!(target.key, 1);
        assert!(f64::from(target.x) < 100.0);
        // The same point asked for on the right display must not land on the left display's
        // region, however close it is; the right display's own region is too far.
        assert_eq!(super::locate(&candidates, None, &right, 100.5, 50.0), None);
        // Just short of the right display's own region it is taken.
        let target = super::locate(&candidates, None, &right, 102.4, 50.0).unwrap();
        assert_eq!((target.key, target.x), (2, 103.0));
        // A display with no region of its own near the point has no target at all.
        let elsewhere = RegionRect::new(500, 0, 100, 100);
        assert_eq!(
            super::locate(&candidates, None, &elsewhere, 100.5, 50.0),
            None
        );
        // A point inside a region is found whichever display was asked for (the slack is the only
        // thing the display restricts).
        let target = super::locate(&candidates, None, &right, 50.0, 50.0).unwrap();
        assert_eq!(target.key, 1);
    }

    #[test]
    fn the_nearer_of_two_overlapping_regions_takes_the_edge_point() {
        // The display's rectangle overlaps regions 1 and 2 (not 3): the one nearer the point wins.
        let candidates = [
            (1u32, RegionRect::new(0, 0, 100, 100)),
            (2, RegionRect::new(101, 0, 100, 100)),
            (3, RegionRect::new(300, 0, 100, 100)),
        ];
        let display = RegionRect {
            x: 90.0,
            y: 0.0,
            width: 120.0,
            height: 100.0,
        };
        let target = super::locate(&candidates, None, &display, 100.8, 50.0).unwrap();
        assert_eq!(target.key, 2);
        let target = super::locate(&candidates, None, &display, 100.2, 50.0).unwrap();
        assert_eq!(target.key, 1);
    }

    #[test]
    fn regions_overlap_only_with_shared_area() {
        let a = RegionRect::new(0, 0, 100, 100);
        assert!(a.overlaps(&RegionRect::new(99, 99, 10, 10)));
        // Touching edges and corners share no area.
        assert!(!a.overlaps(&RegionRect::new(100, 0, 100, 100)));
        assert!(!a.overlaps(&RegionRect::new(0, 100, 100, 100)));
        assert!(!a.overlaps(&RegionRect::new(100, 100, 10, 10)));
        assert!(!a.overlaps(&RegionRect::new(10, 10, 0, 10)));
        assert!(!RegionRect::new(10, 10, 10, 0).overlaps(&a));
    }

    #[test]
    fn a_display_rectangle_is_its_logical_bounds() {
        let rect = RegionRect::of_display(&geometry(1366, 768, 1.5, 10.0, 20.0));
        assert_eq!((rect.x, rect.y, rect.height), (10.0, 20.0, 512.0));
        assert!((rect.width - 910.666_666_666_666_6).abs() < 1e-9);
    }

    #[test]
    fn the_offset_is_part_of_the_coordinate() {
        // A second monitor to the right: the coordinate sent is global, not region-local.
        let candidates = [
            (1u32, region(0, 0, 1920, 1080)),
            (2, region(1920, 0, 1280, 720)),
        ];
        let target = locate(&candidates, None, 2000.5, 100.25).unwrap();
        assert_eq!(target.key, 2);
        assert_eq!((target.x, target.y), (2000.5, 100.25));
        let target = locate(&candidates, None, 10.0, 10.0).unwrap();
        assert_eq!(target.key, 1);
    }

    #[test]
    fn the_far_edge_is_outside() {
        let candidates = [(1u32, region(0, 0, 100, 100))];
        // Inside by a hair: allowed.
        assert!(locate(&candidates, None, 99.5, 50.0).is_some());
        // Exactly on the far edge is not in the region; it is moved just inside.
        let target = locate(&candidates, None, 100.0, 50.0).unwrap();
        assert!(f64::from(target.x) < 100.0 && target.x > 99.9);
    }

    #[test]
    fn a_point_more_than_a_pixel_outside_has_no_region() {
        let candidates = [(1u32, region(0, 0, 100, 100))];
        assert_eq!(locate(&candidates, None, 101.5, 50.0), None);
        assert_eq!(locate(&candidates, None, -1.5, 50.0), None);
        assert_eq!(locate(&candidates, None, 50.0, 250.0), None);
        assert_eq!(locate::<u32>(&[], None, 1.0, 1.0), None);
    }

    #[test]
    fn rounding_slack_at_an_edge_is_clamped_onto_the_region() {
        // 1366 / 1.5 = 910.67 logical wide, announced as 910: the last sliver still maps.
        let candidates = [(7u32, region(0, 0, 910, 512))];
        let target = locate(&candidates, None, 910.4, 100.0).unwrap();
        assert_eq!(target.key, 7);
        assert!(f64::from(target.x) < 910.0);
        assert!(
            candidates[0]
                .1
                .contains(f64::from(target.x), f64::from(target.y))
        );
    }

    #[test]
    fn the_nearest_region_takes_an_edge_point_between_two() {
        let candidates = [
            (1u32, region(0, 0, 100, 100)),
            (2, region(101, 0, 100, 100)),
        ];
        // 100.4 is in the gap: nearer to region 2's left edge (0.6) than region 1's right (0.4)?
        // Region 1: x clamps to 99.99, distance 0.41. Region 2: clamps to 101, distance 0.6.
        let target = locate(&candidates, None, 100.4, 50.0).unwrap();
        assert_eq!(target.key, 1);
        let target = locate(&candidates, None, 100.8, 50.0).unwrap();
        assert_eq!(target.key, 2);
    }

    #[test]
    fn the_preferred_device_wins_on_overlap() {
        let candidates = [(1u32, region(0, 0, 100, 100)), (2, region(50, 0, 100, 100))];
        assert_eq!(locate(&candidates, None, 60.0, 5.0).unwrap().key, 1);
        assert_eq!(locate(&candidates, Some(2), 60.0, 5.0).unwrap().key, 2);
        // The preference doesn't pull a point into a region that doesn't contain it.
        assert_eq!(locate(&candidates, Some(2), 10.0, 5.0).unwrap().key, 1);
    }

    #[test]
    fn non_finite_and_huge_points_have_no_target() {
        let candidates = [(1u32, region(0, 0, 100, 100))];
        assert_eq!(locate(&candidates, None, f64::NAN, 1.0), None);
        assert_eq!(locate(&candidates, None, 1.0, f64::INFINITY), None);
        assert_eq!(locate(&candidates, None, 1e300, 1.0), None);
    }

    #[test]
    fn zero_sized_regions_never_match() {
        let candidates = [(1u32, region(0, 0, 0, 100)), (2, region(0, 0, 100, 0))];
        assert_eq!(locate(&candidates, None, 0.0, 0.0), None);
    }

    #[test]
    fn logical_point_uses_origin_and_scale() {
        // 2560x1440 at 1.25 placed at logical x = 1920.
        let g = geometry(2560, 1440, 1.25, 1920.0, 0.0);
        let (x, y) = logical_point(&g, PointDevice::new(1280.0, 720.0)).unwrap();
        assert_eq!((x, y), (1920.0 + 1024.0, 576.0));
        // Mixed scale: a 1.0 monitor to the left, a 2.0 monitor to the right.
        let left = geometry(1920, 1080, 1.0, 0.0, 0.0);
        let right = geometry(3840, 2160, 2.0, 1920.0, 0.0);
        assert_eq!(
            logical_point(&left, PointDevice::new(100.0, 50.0)).unwrap(),
            (100.0, 50.0)
        );
        assert_eq!(
            logical_point(&right, PointDevice::new(100.0, 50.0)).unwrap(),
            (1970.0, 25.0)
        );
        // A negative origin (a monitor left of the primary) is plain addition.
        let neg = geometry(1920, 1080, 1.0, -1920.0, 100.0);
        assert_eq!(
            logical_point(&neg, PointDevice::new(5.0, 5.0)).unwrap(),
            (-1915.0, 105.0)
        );
    }

    #[test]
    fn logical_point_rejects_positions_off_the_display() {
        let g = geometry(1920, 1080, 1.0, 0.0, 0.0);
        for bad in [
            PointDevice::new(-0.5, 1.0),
            PointDevice::new(1.0, -1.0),
            PointDevice::new(1920.0, 1.0),
            PointDevice::new(1.0, 1080.0),
            PointDevice::new(f64::NAN, 1.0),
        ] {
            assert!(matches!(
                logical_point(&g, bad),
                Err(PlatformError::Unsupported(_))
            ));
        }
        let broken = geometry(1920, 1080, 0.0, 0.0, 0.0);
        assert!(logical_point(&broken, PointDevice::new(1.0, 1.0)).is_err());
        let empty = geometry(0, 1080, 1.0, 0.0, 0.0);
        assert!(logical_point(&empty, PointDevice::new(0.0, 0.0)).is_err());
    }

    #[test]
    fn a_mixed_scale_round_trip_lands_in_the_right_region() {
        // 3440x1440 @1.25 at the origin and 1920x1080 @1.0 to its right, the regions as a
        // compositor announces them (logical, integer).
        let big = geometry(3440, 1440, 1.25, 0.0, 0.0);
        let small = geometry(1920, 1080, 1.0, 2752.0, 0.0);
        let candidates = [
            (1u32, region(0, 0, 2752, 1152)),
            (2, region(2752, 0, 1920, 1080)),
        ];
        let (x, y) = logical_point(&big, PointDevice::new(3439.0, 1439.0)).unwrap();
        assert_eq!(locate(&candidates, None, x, y).unwrap().key, 1);
        let (x, y) = logical_point(&small, PointDevice::new(0.0, 0.0)).unwrap();
        assert_eq!(locate(&candidates, None, x, y).unwrap().key, 2);
        let (x, y) = logical_point(&small, PointDevice::new(1919.0, 1079.0)).unwrap();
        let target = locate(&candidates, None, x, y).unwrap();
        assert_eq!((target.key, target.x, target.y), (2, 4671.0, 1079.0));
    }
}
