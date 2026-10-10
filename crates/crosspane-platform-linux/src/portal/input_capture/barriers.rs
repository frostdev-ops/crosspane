//! Pointer barriers for the InputCapture portal backend (GNOME and KDE).
//!
//! A barrier is a stretch of a display edge that the pointer may push through to start a capture.
//! The compositor accepts only barriers that lie on the edges of its layout, so this module plans
//! one barrier per [`CapturePortal`] and validates each with a faithful port of mutter's
//! `check_barrier`. Everything here is pure: no I/O, no portal calls, no D-Bus.

use std::cmp::Reverse;
use std::collections::HashSet;

use crosspane_platform::{CapturePortal, Edge, PortalId};
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{DisplayGeometry, PointDevice};
use crosspane_types::id::DisplayId;

/// How far (logical px) the cursor may be from a barrier's line to activate it without an id.
const REACH_PX: f64 = 2.5;
/// Slack (logical px) past a stretch's ends for the along-edge test of [`Plan::activation`].
const SLACK_PX: f64 = 1.0;
/// The shortest stretch that can be a barrier: a one-pixel inclusive line is a singularity.
const MIN_BARRIER_PX: i64 = 2;

/// A zone of the portal's GetZones answer: one logical monitor, logical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Zone {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// A barrier line (x1,y1)-(x2,y2), both ends INCLUSIVE, axis aligned, as sent to
/// SetPointerBarriers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Line {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}

/// Why a barrier or a portal set was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Rejection {
    /// The line is neither vertical nor horizontal.
    NotAxisAligned,
    /// Both ends are the same point.
    Singularity,
    /// The line crosses a zone's interior ("Line overlaps with monitor region").
    Overlap,
    /// The line lies on a zone edge line but extends beyond that zone's edge ("Line partially with
    /// monitor region").
    Partial,
    /// The line is contained in the edges of more than one zone ("Adjacent to multiple monitor
    /// edges").
    Multiple,
    /// The line is on no zone edge at all.
    NotOnEdge,
    /// A horizontal barrier reaches into the area right of the rightmost zone ("Line extends into
    /// nonexisting monitor region").
    PastLayout,
    /// The portal names a display that is not in the display list.
    UnknownDisplay(DisplayId),
    /// The display has no zone of the portal's zone set that matches its logical rectangle.
    NoZone(DisplayId),
    /// `from`/`to` not finite, or `from >= to`, or the stretch lies outside the display edge.
    BadSpan(PortalId),
    /// Two portals in the set carry the same id.
    DuplicatePortal(PortalId),
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAxisAligned => f.write_str("barrier is not axis aligned"),
            Self::Singularity => f.write_str("barrier is a single point"),
            Self::Overlap => f.write_str("Line overlaps with monitor region"),
            Self::Partial => f.write_str("Line partially with monitor region"),
            Self::Multiple => f.write_str("Adjacent to multiple monitor edges"),
            Self::NotOnEdge => f.write_str("Line is not on any monitor edge"),
            Self::PastLayout => f.write_str("Line extends into nonexisting monitor region"),
            Self::UnknownDisplay(id) => write!(f, "display {} is not known", id.0),
            Self::NoZone(id) => write!(f, "display {} has no matching monitor zone", id.0),
            Self::BadSpan(id) => write!(f, "portal {} has a bad stretch", id.0),
            Self::DuplicatePortal(id) => write!(f, "portal {} is listed twice", id.0),
        }
    }
}

impl std::error::Error for Rejection {}

/// What an activation needs to know about the portal it came through (consumed by pressure.rs).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Entry {
    pub portal: PortalId,
    pub display: DisplayId,
    pub edge: Edge,
    /// Device pixels per logical pixel of `display`.
    pub scale: f64,
    /// The portal's stretch along the edge in ABSOLUTE logical coordinates (y for Left/Right edges,
    /// x for Top/Bottom edges): `(lo, hi)` with lo < hi; lo = first covered logical pixel, hi = one
    /// past the last covered pixel (line end + 1).
    pub span: (f64, f64),
}

/// One planned barrier.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Barrier {
    /// Non-zero, 1-based, in portal order.
    pub id: u32,
    pub line: Line,
    pub entry: Entry,
}

/// A validated set of barriers, one per portal, in portal order.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Plan {
    pub barriers: Vec<Barrier>,
    /// The zone set the barriers were checked against.
    pub zone_set: u32,
}

/// How a line meets one zone (mutter's `get_barrier_adjacency`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Adjacency {
    /// The line does not touch the zone.
    Outside,
    /// The line lies on one of the zone's edges and inside its extent.
    Contained,
    /// The line crosses the zone's interior.
    Overlap,
    /// The line lies on one of the zone's edge lines but extends beyond that edge.
    Partial,
}

/// A zone's edges widened to i64, so that the far edges (`x + width`) cannot overflow.
#[derive(Clone, Copy, Debug)]
struct Bounds {
    left: i64,
    top: i64,
    right: i64,
    bottom: i64,
}

impl Bounds {
    fn of(zone: Zone) -> Self {
        let left = i64::from(zone.x);
        let top = i64::from(zone.y);
        Self {
            left,
            top,
            right: left + i64::from(zone.width),
            bottom: top + i64::from(zone.height),
        }
    }
}

/// Port of mutter's `get_barrier_adjacency` for one axis-aligned line. A vertical line counts only
/// when its x lies in `[left, right]` and its extent reaches into `[top, bottom)` (horizontal lines
/// swap the axes). On an edge line (`x == left` or `x == right`, `y == top` or `y == bottom`) it is
/// `Contained` when its whole inclusive extent lies within the zone's span on that axis, and
/// `Partial` when it reaches past. Any other line in that range crosses the interior: `Overlap`.
fn adjacency(rect: Bounds, line: Line) -> Adjacency {
    let x_min = i64::from(line.x1.min(line.x2));
    let x_max = i64::from(line.x1.max(line.x2));
    let y_min = i64::from(line.y1.min(line.y2));
    let y_max = i64::from(line.y1.max(line.y2));

    if line.x1 == line.x2 {
        let x = i64::from(line.x1);
        if x < rect.left || x > rect.right || y_max < rect.top || y_min >= rect.bottom {
            return Adjacency::Outside;
        }
        if rect.right == x || rect.left == x {
            if y_max > rect.bottom || y_min < rect.top {
                Adjacency::Partial
            } else {
                Adjacency::Contained
            }
        } else {
            Adjacency::Overlap
        }
    } else if line.y1 == line.y2 {
        let y = i64::from(line.y1);
        if y < rect.top || y > rect.bottom || x_max < rect.left || x_min >= rect.right {
            return Adjacency::Outside;
        }
        if rect.bottom == y || rect.top == y {
            if x_max > rect.right || x_min < rect.left {
                Adjacency::Partial
            } else {
                Adjacency::Contained
            }
        } else {
            Adjacency::Overlap
        }
    } else {
        Adjacency::Outside
    }
}

/// The zone with the largest right edge; ties go to the smallest top edge. Mutter walks right from
/// the primary monitor; this approximates that walk.
fn rightmost(zones: &[Zone]) -> Option<Bounds> {
    zones
        .iter()
        .map(|zone| Bounds::of(*zone))
        .max_by_key(|bounds| (bounds.right, Reverse(bounds.top)))
}

/// Mutter's second horizontal test: a horizontal line that reaches the area right of the rightmost
/// zone, on that zone's top row, is past the layout.
fn past_layout(zones: &[Zone], line: Line) -> bool {
    rightmost(zones).is_some_and(|right| {
        let width = right.right - right.left;
        let beyond = Bounds {
            left: right.right,
            top: right.top,
            right: right.right + width,
            bottom: right.bottom,
        };
        adjacency(beyond, line) != Adjacency::Outside
    })
}

/// Port of mutter's `check_barrier`. `Ok(())` = accepted. Zones are checked in slice order, and the
/// first error in that order wins.
pub(super) fn check_barrier(zones: &[Zone], line: Line) -> Result<(), Rejection> {
    if line.x1 != line.x2 && line.y1 != line.y2 {
        return Err(Rejection::NotAxisAligned);
    }
    if line.x1 == line.x2 && line.y1 == line.y2 {
        return Err(Rejection::Singularity);
    }
    let mut touching = false;
    for zone in zones {
        match adjacency(Bounds::of(*zone), line) {
            Adjacency::Outside => {}
            Adjacency::Contained if touching => return Err(Rejection::Multiple),
            Adjacency::Contained => touching = true,
            Adjacency::Overlap => return Err(Rejection::Overlap),
            Adjacency::Partial => return Err(Rejection::Partial),
        }
    }
    if !touching {
        return Err(Rejection::NotOnEdge);
    }
    if line.y1 == line.y2 && past_layout(zones, line) {
        return Err(Rejection::PastLayout);
    }
    Ok(())
}

/// The zone that matches `geometry`'s logical rectangle. Each of the four values may differ by one
/// logical pixel, which absorbs fractional-scale rounding (2560x1440 at 1.5 is 1706.67 wide). The
/// closest match by the sum of the four differences wins; ties go to the first zone.
fn zone_for(zones: &[Zone], geometry: &DisplayGeometry) -> Option<Zone> {
    let size = geometry.logical_size();
    let origin_x = geometry.logical_origin.x.round();
    let origin_y = geometry.logical_origin.y.round();
    let width = size.width.round();
    let height = size.height.round();
    let mut best: Option<(f64, Zone)> = None;
    for &zone in zones {
        let diffs = [
            (f64::from(zone.x) - origin_x).abs(),
            (f64::from(zone.y) - origin_y).abs(),
            (f64::from(zone.width) - width).abs(),
            (f64::from(zone.height) - height).abs(),
        ];
        // `all` is false for NaN, so a non-finite origin or size matches nothing.
        if !diffs.iter().all(|diff| *diff <= 1.0) {
            continue;
        }
        let score: f64 = diffs.iter().sum();
        if best.is_none_or(|(best_score, _)| score < best_score) {
            best = Some((score, zone));
        }
    }
    best.map(|(_, zone)| zone)
}

/// The line on `zone`'s edge that covers `portal`'s stretch, and the stretch's span.
///
/// Device pixels become logical pixels by dividing by `scale`, then whole pixels, clipped to the
/// edge. A stretch shorter than [`MIN_BARRIER_PX`] is widened, because a shorter line is a
/// singularity.
fn edge_stretch(
    portal: &CapturePortal,
    zone: Zone,
    scale: f64,
) -> Result<(Line, (f64, f64)), Rejection> {
    let bad = || Rejection::BadSpan(portal.id);
    let bounds = Bounds::of(zone);
    let vertical_edge = matches!(portal.edge, Edge::Left | Edge::Right);
    let len = if vertical_edge {
        bounds.bottom - bounds.top
    } else {
        bounds.right - bounds.left
    };
    let mut a = (portal.from / scale).round() as i64;
    let mut b = (portal.to / scale).round() as i64;
    a = a.max(0);
    b = b.min(len);
    if b <= a {
        return Err(bad());
    }
    if b - a < MIN_BARRIER_PX {
        if len < MIN_BARRIER_PX {
            return Err(bad());
        }
        b = (a + MIN_BARRIER_PX).min(len);
        a = b - MIN_BARRIER_PX;
    }
    let (x1, y1, x2, y2) = match portal.edge {
        Edge::Left => (bounds.left, bounds.top + a, bounds.left, bounds.top + b - 1),
        Edge::Right => (
            bounds.right,
            bounds.top + a,
            bounds.right,
            bounds.top + b - 1,
        ),
        Edge::Top => (bounds.left + a, bounds.top, bounds.left + b - 1, bounds.top),
        Edge::Bottom => (
            bounds.left + a,
            bounds.bottom,
            bounds.left + b - 1,
            bounds.bottom,
        ),
    };
    let to_i32 = |value: i64| i32::try_from(value).map_err(|_| bad());
    let line = Line {
        x1: to_i32(x1)?,
        y1: to_i32(y1)?,
        x2: to_i32(x2)?,
        y2: to_i32(y2)?,
    };
    // The span is the covered coordinates along the edge: first covered, and last covered + 1.
    let span = if vertical_edge {
        (f64::from(line.y1), f64::from(line.y2) + 1.0)
    } else {
        (f64::from(line.x1), f64::from(line.x2) + 1.0)
    };
    Ok((line, span))
}

/// Plan one barrier per portal, in slice order (ids 1, 2, 3, ...). Atomic: the first problem
/// rejects the whole set.
///
/// Per portal, the checks run in this order: the stretch, duplicate ids, the display, its scale,
/// its zone, the stretch on the zone's edge, then [`check_barrier`].
pub(super) fn plan(
    portals: &[CapturePortal],
    displays: &[DisplayInfo],
    zones: &[Zone],
    zone_set: u32,
) -> Result<Plan, Rejection> {
    let mut seen = HashSet::with_capacity(portals.len());
    let mut barriers = Vec::with_capacity(portals.len());
    for (index, portal) in portals.iter().enumerate() {
        if !(portal.from.is_finite() && portal.to.is_finite() && portal.from < portal.to) {
            return Err(Rejection::BadSpan(portal.id));
        }
        if !seen.insert(portal.id) {
            return Err(Rejection::DuplicatePortal(portal.id));
        }
        let info = displays
            .iter()
            .find(|display| display.id == portal.display)
            .ok_or(Rejection::UnknownDisplay(portal.display))?;
        let scale = info.geometry.scale;
        if !(scale.is_finite() && scale > 0.0) {
            return Err(Rejection::UnknownDisplay(portal.display));
        }
        let zone = zone_for(zones, &info.geometry).ok_or(Rejection::NoZone(portal.display))?;
        let (line, span) = edge_stretch(portal, zone, scale)?;
        check_barrier(zones, line)?;
        let id = u32::try_from(index + 1).map_err(|_| Rejection::BadSpan(portal.id))?;
        barriers.push(Barrier {
            id,
            line,
            entry: Entry {
                portal: portal.id,
                display: portal.display,
                edge: portal.edge,
                scale,
                span,
            },
        });
    }
    Ok(Plan { barriers, zone_set })
}

impl Plan {
    /// The portal an activation belongs to, and where along it the pointer is (0.0..=1.0).
    ///
    /// `barrier` is the Activated signal's barrier id (`None` = unknown or ambiguous). `cursor` is
    /// the pointer position at activation, in the zones' logical space. A known id that is in the
    /// plan selects that barrier. Otherwise the nearest barrier is used: see [`Plan::nearest`].
    /// `position` = (along - lo) / (hi - lo), clamped to 0..=1, where `along` is the cursor's y on
    /// a Left/Right edge and its x on a Top/Bottom edge. Returns `None` when there is no match or
    /// the cursor is not finite.
    pub(super) fn activation(
        &self,
        barrier: Option<u32>,
        cursor: (f64, f64),
    ) -> Option<(Entry, f64)> {
        if !(cursor.0.is_finite() && cursor.1.is_finite()) {
            return None;
        }
        let chosen = match barrier.and_then(|id| self.barriers.iter().find(|b| b.id == id)) {
            Some(known) => known,
            None => self.nearest(cursor)?,
        };
        let entry = chosen.entry;
        let along = match entry.edge {
            Edge::Left | Edge::Right => cursor.1,
            Edge::Top | Edge::Bottom => cursor.0,
        };
        let (lo, hi) = entry.span;
        let position = ((along - lo) / (hi - lo)).clamp(0.0, 1.0);
        Some((entry, position))
    }

    /// The barrier whose line is nearest to `cursor`. Its distance along the line's normal must be
    /// within [`REACH_PX`], and its along-edge coordinate within the stretch widened by
    /// [`SLACK_PX`] at both ends. Ties go to the lowest id.
    fn nearest(&self, cursor: (f64, f64)) -> Option<&Barrier> {
        let mut best: Option<(f64, &Barrier)> = None;
        for barrier in &self.barriers {
            let (distance, along) = match barrier.entry.edge {
                Edge::Left | Edge::Right => {
                    ((cursor.0 - f64::from(barrier.line.x1)).abs(), cursor.1)
                }
                Edge::Top | Edge::Bottom => {
                    ((cursor.1 - f64::from(barrier.line.y1)).abs(), cursor.0)
                }
            };
            let (lo, hi) = barrier.entry.span;
            let inside = (lo - SLACK_PX..=hi + SLACK_PX).contains(&along);
            if distance <= REACH_PX && inside && best.is_none_or(|(d, _)| distance < d) {
                best = Some((distance, barrier));
            }
        }
        best.map(|(_, barrier)| barrier)
    }

    /// The entry of the barrier planned for `portal`, if the plan has one.
    #[cfg(test)]
    pub(super) fn entry_for_portal(&self, portal: PortalId) -> Option<Entry> {
        self.barriers
            .iter()
            .find(|b| b.entry.portal == portal)
            .map(|b| b.entry)
    }
}

/// Where to put the pointer when releasing a capture. `warp` is the engine's (display, device-pixel
/// point on that display). It is converted with that display's geometry, then clamped into the
/// display's logical rectangle `[origin, origin + logical_size - 1]` on each axis. A non-finite
/// point, an unusable display, an unknown display, or `None` gives `fallback` unchanged.
pub(super) fn release_point(
    warp: Option<(DisplayId, PointDevice)>,
    displays: &[DisplayInfo],
    fallback: (f64, f64),
) -> (f64, f64) {
    let Some((display, point)) = warp else {
        return fallback;
    };
    let Some(info) = displays.iter().find(|d| d.id == display) else {
        return fallback;
    };
    let geometry = &info.geometry;
    let usable = geometry.scale.is_finite()
        && geometry.scale > 0.0
        && geometry.logical_origin.x.is_finite()
        && geometry.logical_origin.y.is_finite()
        && geometry.pixel_size.width >= 1
        && geometry.pixel_size.height >= 1;
    if !usable || !(point.x.is_finite() && point.y.is_finite()) {
        return fallback;
    }
    let logical = geometry.device_to_logical(point);
    let size = geometry.logical_size();
    let min_x = geometry.logical_origin.x;
    let min_y = geometry.logical_origin.y;
    let max_x = min_x + size.width - 1.0;
    let max_y = min_y + size.height - 1.0;
    // `min` then `max`, not `clamp`: an inverted range (a sub-pixel display) must not panic.
    let x = logical.x.min(max_x).max(min_x);
    let y = logical.y.min(max_y).max(min_y);
    if x.is_finite() && y.is_finite() {
        (x, y)
    } else {
        fallback
    }
}

#[cfg(test)]
mod tests {
    use crosspane_types::color::ColorSpace;
    use crosspane_types::geom::{PixelSize, PointLogical, SizeMm};

    use super::*;

    const DP3: DisplayId = DisplayId(1);
    const HDMI1: DisplayId = DisplayId(3);

    fn display(id: u32, pixels: (u32, u32), scale: f64, origin: (f64, f64)) -> DisplayInfo {
        DisplayInfo {
            id: DisplayId(id),
            name: format!("display-{id}"),
            geometry: DisplayGeometry {
                physical_size: SizeMm::new(600.0, 340.0),
                pixel_size: PixelSize::new(pixels.0, pixels.1),
                scale,
                logical_origin: PointLogical::new(origin.0, origin.1),
            },
            refresh_millihz: 60_000,
            color_space: ColorSpace::Srgb,
            hdr: false,
        }
    }

    fn zone(x: i32, y: i32, width: u32, height: u32) -> Zone {
        Zone {
            x,
            y,
            width,
            height,
        }
    }

    fn line(x1: i32, y1: i32, x2: i32, y2: i32) -> Line {
        Line { x1, y1, x2, y2 }
    }

    fn portal(id: u32, display: DisplayId, edge: Edge, from: f64, to: f64) -> CapturePortal {
        CapturePortal {
            id: PortalId(id),
            display,
            edge,
            from,
            to,
        }
    }

    /// The dev layout, logical px: DP-3 3440x1440 at (1080,1080) (id 1); DP-2 1920x1080 at
    /// (1817,0) (id 2); HDMI-1 1080x1920 at (0,600), portrait (id 3).
    fn dev_displays() -> Vec<DisplayInfo> {
        vec![
            display(1, (3440, 1440), 1.0, (1080.0, 1080.0)),
            display(2, (1920, 1080), 1.0, (1817.0, 0.0)),
            display(3, (1080, 1920), 1.0, (0.0, 600.0)),
        ]
    }

    fn dev_zones() -> Vec<Zone> {
        vec![
            zone(1080, 1080, 3440, 1440),
            zone(1817, 0, 1920, 1080),
            zone(0, 600, 1080, 1920),
        ]
    }

    /// Portal 1 on HDMI-1's right edge (y 600..1080) and portal 2 on DP-3's bottom edge.
    fn two_barrier_plan() -> Plan {
        let portals = [
            portal(1, HDMI1, Edge::Right, 0.0, 480.0),
            portal(2, DP3, Edge::Bottom, 0.0, 3440.0),
        ];
        plan(&portals, &dev_displays(), &dev_zones(), 1).unwrap()
    }

    // ---- check_barrier --------------------------------------------------------------------------

    #[test]
    fn check_barrier_rejects_malformed_lines() {
        let zones = dev_zones();
        assert_eq!(
            check_barrier(&zones, line(0, 600, 10, 610)),
            Err(Rejection::NotAxisAligned)
        );
        assert_eq!(
            check_barrier(&zones, line(100, 100, 100, 100)),
            Err(Rejection::Singularity)
        );
    }

    #[test]
    fn check_barrier_rejects_interior_and_free_lines() {
        let zones = dev_zones();
        // x = 500 is neither edge of HDMI-1: the line crosses its interior.
        assert_eq!(
            check_barrier(&zones, line(500, 700, 500, 800)),
            Err(Rejection::Overlap)
        );
        // Horizontal through DP-3's interior.
        assert_eq!(
            check_barrier(&zones, line(1100, 1500, 1200, 1500)),
            Err(Rejection::Overlap)
        );
        // Outside every zone.
        assert_eq!(
            check_barrier(&zones, line(5000, 100, 5000, 200)),
            Err(Rejection::NotOnEdge)
        );
        assert_eq!(
            check_barrier(&zones, line(0, 100, 100, 100)),
            Err(Rejection::NotOnEdge)
        );
    }

    #[test]
    fn check_barrier_vertical_edge_ends_are_inclusive() {
        let zones = dev_zones();
        // DP-2's right edge (x = 3737) above DP-3, whose top is y = 1080: accepted.
        assert_eq!(check_barrier(&zones, line(3737, 0, 3737, 1079)), Ok(()));
        // Reaching y = 1080 puts the line in DP-3's interior.
        assert_eq!(
            check_barrier(&zones, line(3737, 0, 3737, 1080)),
            Err(Rejection::Overlap)
        );
    }

    #[test]
    fn check_barrier_portrait_monitor_shared_edge() {
        let zones = dev_zones();
        // HDMI-1's right edge (x = 1080, y 600..2520 exclusive). Above DP-3's top, DP-3's left edge
        // is not touched: accepted.
        assert_eq!(check_barrier(&zones, line(1080, 600, 1080, 1079)), Ok(()));
        // Reaching y = 1080 touches DP-3's left edge too, but not for its whole extent.
        assert_eq!(
            check_barrier(&zones, line(1080, 600, 1080, 1080)),
            Err(Rejection::Partial)
        );
        assert_eq!(
            check_barrier(&zones, line(1080, 600, 1080, 2519)),
            Err(Rejection::Partial)
        );
        // The shared stretch is contained in both HDMI-1's right edge and DP-3's left edge.
        assert_eq!(
            check_barrier(&zones, line(1080, 1080, 1080, 2519)),
            Err(Rejection::Multiple)
        );
    }

    #[test]
    fn check_barrier_horizontal_top_edge_and_layout_end() {
        let zones = dev_zones();
        // DP-2's bottom edge (y = 1080, x 1817..3737) shares the line's row and extends past it.
        assert_eq!(
            check_barrier(&zones, line(1080, 1080, 4519, 1080)),
            Err(Rejection::Partial)
        );
        // Right of DP-2, only DP-3's top edge is touched: accepted.
        assert_eq!(check_barrier(&zones, line(3737, 1080, 4519, 1080)), Ok(()));
        // Ending at x = 4520 (the first pixel past DP-3's right edge) reaches the area right of
        // the rightmost zone. Mutter's fake zone there is partially touched, so PastLayout.
        assert_eq!(
            check_barrier(&zones, line(3737, 1080, 4520, 1080)),
            Err(Rejection::PastLayout)
        );
    }

    #[test]
    fn check_barrier_past_layout_only_for_horizontal_lines() {
        // A vertical line at the rightmost zone's right edge is not checked against the fake zone.
        let zones = dev_zones();
        assert_eq!(check_barrier(&zones, line(4520, 1100, 4520, 1200)), Ok(()));
    }

    // ---- plan -----------------------------------------------------------------------------------

    #[test]
    fn plan_builds_portrait_right_edge_stretch() {
        let portals = [portal(7, HDMI1, Edge::Right, 0.0, 480.0)];
        let planned = plan(&portals, &dev_displays(), &dev_zones(), 5).unwrap();
        assert_eq!(planned.zone_set, 5);
        assert_eq!(planned.barriers.len(), 1);
        assert_eq!(
            planned.barriers[0],
            Barrier {
                id: 1,
                line: line(1080, 600, 1080, 1079),
                entry: Entry {
                    portal: PortalId(7),
                    display: HDMI1,
                    edge: Edge::Right,
                    scale: 1.0,
                    span: (600.0, 1080.0),
                },
            }
        );
    }

    #[test]
    fn plan_rejects_stretch_that_runs_past_the_shared_edge() {
        // Up to 481 px reaches y = 1080, where DP-3's left edge starts.
        let portals = [portal(1, HDMI1, Edge::Right, 0.0, 481.0)];
        assert_eq!(
            plan(&portals, &dev_displays(), &dev_zones(), 1),
            Err(Rejection::Partial)
        );
    }

    #[test]
    fn plan_rejects_shared_stretch_and_keeps_the_set_atomic() {
        // DP-3's left edge at y 1080..1559 is also HDMI-1's right edge: Multiple. The first portal
        // is fine on its own, but the whole set is refused.
        let portals = [
            portal(1, HDMI1, Edge::Right, 0.0, 480.0),
            portal(2, DP3, Edge::Left, 0.0, 480.0),
        ];
        assert_eq!(
            plan(&portals, &dev_displays(), &dev_zones(), 1),
            Err(Rejection::Multiple)
        );
    }

    #[test]
    fn plan_builds_dp3_bottom_full_width() {
        let portals = [portal(1, DP3, Edge::Bottom, 0.0, 3440.0)];
        let planned = plan(&portals, &dev_displays(), &dev_zones(), 1).unwrap();
        assert_eq!(planned.barriers[0].line, line(1080, 2520, 4519, 2520));
        assert_eq!(planned.barriers[0].entry.span, (1080.0, 4520.0));
        assert_eq!(planned.barriers[0].entry.edge, Edge::Bottom);
    }

    #[test]
    fn plan_widens_a_one_pixel_stretch_to_two() {
        // [10, 11) becomes [10, 12): b = min(a + 2, len) = 12, a = b - 2 = 10. The line covers
        // x 1090..1091 on DP-3's bottom edge. DP-3's left edge would be Multiple (HDMI-1 shares).
        let portals = [portal(1, DP3, Edge::Bottom, 10.0, 11.0)];
        let planned = plan(&portals, &dev_displays(), &dev_zones(), 1).unwrap();
        assert_eq!(planned.barriers[0].line, line(1090, 2520, 1091, 2520));
        assert_eq!(planned.barriers[0].entry.span, (1090.0, 1092.0));
    }

    #[test]
    fn plan_rotated_monitor_edges_are_accepted() {
        // HDMI-1 is portrait: its left edge (x = 0) and top edge (y = 600) are on the layout's
        // outer boundary.
        let portals = [
            portal(1, HDMI1, Edge::Left, 0.0, 1920.0),
            portal(2, HDMI1, Edge::Top, 0.0, 1080.0),
        ];
        let planned = plan(&portals, &dev_displays(), &dev_zones(), 1).unwrap();
        assert_eq!(planned.barriers[0].line, line(0, 600, 0, 2519));
        assert_eq!(planned.barriers[0].entry.span, (600.0, 2520.0));
        assert_eq!(planned.barriers[1].line, line(0, 600, 1079, 600));
        assert_eq!(planned.barriers[1].entry.span, (0.0, 1080.0));
    }

    #[test]
    fn plan_scale_one_and_a_half_converts_device_pixels() {
        // 2160x1440 px at scale 1.5 is 1440x960 logical.
        let displays = [display(4, (2160, 1440), 1.5, (0.0, 0.0))];
        let zones = [zone(0, 0, 1440, 960)];
        let portals = [portal(1, DisplayId(4), Edge::Right, 0.0, 1440.0)];
        let planned = plan(&portals, &displays, &zones, 1).unwrap();
        assert_eq!(planned.barriers[0].line, line(1440, 0, 1440, 959));
        assert_eq!(planned.barriers[0].entry.span, (0.0, 960.0));
        assert_eq!(planned.barriers[0].entry.scale, 1.5);
    }

    #[test]
    fn plan_fractional_scale_matches_rounded_zone() {
        // 2560x1440 px at scale 1.5 is 1706.67x960 logical, which rounds to the 1707x960 zone.
        let displays = [display(4, (2560, 1440), 1.5, (0.0, 0.0))];
        let zones = [zone(0, 0, 1707, 960)];
        let portals = [portal(1, DisplayId(4), Edge::Top, 0.0, 2560.0)];
        let planned = plan(&portals, &displays, &zones, 1).unwrap();
        assert_eq!(planned.barriers[0].line, line(0, 0, 1706, 0));
        assert_eq!(planned.barriers[0].entry.span, (0.0, 1707.0));
    }

    #[test]
    fn plan_bad_spans_are_refused() {
        let zones = dev_zones();
        let displays = dev_displays();
        let bad = |from: f64, to: f64| {
            plan(
                &[portal(1, HDMI1, Edge::Right, from, to)],
                &displays,
                &zones,
                1,
            )
        };
        assert_eq!(bad(f64::NAN, 480.0), Err(Rejection::BadSpan(PortalId(1))));
        assert_eq!(
            bad(0.0, f64::INFINITY),
            Err(Rejection::BadSpan(PortalId(1)))
        );
        assert_eq!(bad(100.0, 50.0), Err(Rejection::BadSpan(PortalId(1))));
        assert_eq!(bad(10.0, 10.0), Err(Rejection::BadSpan(PortalId(1))));
        // Entirely before the top of the display: clipped to nothing.
        assert_eq!(bad(-100.0, -50.0), Err(Rejection::BadSpan(PortalId(1))));
    }

    #[test]
    fn plan_unknown_display_no_zone_and_duplicates_are_refused() {
        let zones = dev_zones();
        let displays = dev_displays();
        assert_eq!(
            plan(
                &[portal(1, DisplayId(9), Edge::Right, 0.0, 480.0)],
                &displays,
                &zones,
                1
            ),
            Err(Rejection::UnknownDisplay(DisplayId(9)))
        );
        // A zero scale is not a usable display.
        assert_eq!(
            plan(
                &[portal(1, HDMI1, Edge::Right, 0.0, 480.0)],
                &[display(3, (1080, 1920), 0.0, (0.0, 600.0))],
                &zones,
                1
            ),
            Err(Rejection::UnknownDisplay(HDMI1))
        );
        // Nothing in the zone set is at (0,0) with 800x600.
        assert_eq!(
            plan(
                &[portal(1, DisplayId(4), Edge::Right, 0.0, 480.0)],
                &[display(4, (800, 600), 1.0, (0.0, 0.0))],
                &zones,
                1
            ),
            Err(Rejection::NoZone(DisplayId(4)))
        );
        let twice = [
            portal(1, HDMI1, Edge::Right, 0.0, 480.0),
            portal(1, HDMI1, Edge::Right, 0.0, 480.0),
        ];
        assert_eq!(
            plan(&twice, &displays, &zones, 1),
            Err(Rejection::DuplicatePortal(PortalId(1)))
        );
    }

    // ---- activation -----------------------------------------------------------------------------

    #[test]
    fn activation_known_barrier_gives_position_along_its_stretch() {
        let planned = two_barrier_plan();
        let (entry, position) = planned.activation(Some(1), (1079.5, 840.0)).unwrap();
        assert_eq!(entry.portal, PortalId(1));
        assert_eq!(position, 0.5);
        // Beyond either end the position is clamped.
        assert_eq!(planned.activation(Some(1), (1079.5, 100.0)).unwrap().1, 0.0);
        assert_eq!(
            planned.activation(Some(1), (1079.5, 5000.0)).unwrap().1,
            1.0
        );
        // Bottom edge: the along coordinate is x. (2800 - 1080) / 3440 = 0.5.
        let (entry, position) = planned.activation(Some(2), (2800.0, 2519.0)).unwrap();
        assert_eq!(entry.portal, PortalId(2));
        assert_eq!(position, 0.5);
    }

    #[test]
    fn activation_unknown_barrier_falls_back_to_proximity() {
        let planned = two_barrier_plan();
        // 1 px from HDMI-1's right-edge line.
        let (entry, position) = planned.activation(None, (1079.0, 840.0)).unwrap();
        assert_eq!(entry.portal, PortalId(1));
        assert_eq!(position, 0.5);
        // An id that is not in the plan behaves like None.
        let (entry, _) = planned.activation(Some(99), (1079.0, 840.0)).unwrap();
        assert_eq!(entry.portal, PortalId(1));
        // 1 px from DP-3's bottom line, at x = 2800.
        let (entry, position) = planned.activation(None, (2800.0, 2519.0)).unwrap();
        assert_eq!(entry.portal, PortalId(2));
        assert_eq!(position, 0.5);
    }

    #[test]
    fn activation_proximity_limits() {
        let planned = two_barrier_plan();
        // 10 px from the line: too far to activate without an id.
        assert_eq!(planned.activation(None, (1070.0, 840.0)), None);
        // Right distance, but past the stretch's end plus the slack.
        assert_eq!(planned.activation(None, (1079.0, 2600.0)), None);
        assert_eq!(planned.activation(None, (f64::NAN, 840.0)), None);
        assert_eq!(planned.activation(Some(1), (f64::NAN, 840.0)), None);
    }

    #[test]
    fn activation_ties_go_to_the_lowest_id() {
        // Two portals on the same HDMI-1 stretch give identical lines.
        let portals = [
            portal(1, HDMI1, Edge::Right, 0.0, 480.0),
            portal(2, HDMI1, Edge::Right, 0.0, 480.0),
        ];
        let planned = plan(&portals, &dev_displays(), &dev_zones(), 1).unwrap();
        let (entry, _) = planned.activation(None, (1079.0, 840.0)).unwrap();
        assert_eq!(entry.portal, PortalId(1));
    }

    #[test]
    fn entry_for_portal_finds_the_planned_entry() {
        let planned = two_barrier_plan();
        assert_eq!(
            planned.entry_for_portal(PortalId(2)).map(|e| e.edge),
            Some(Edge::Bottom)
        );
        assert_eq!(planned.entry_for_portal(PortalId(9)), None);
    }

    // ---- release_point --------------------------------------------------------------------------

    #[test]
    fn release_point_maps_device_pixels_and_clamps() {
        let displays = dev_displays();
        let fallback = (7.0, 8.0);
        // HDMI-1's origin is (0,600) at scale 1.
        assert_eq!(
            release_point(
                Some((HDMI1, PointDevice::new(500.0, 100.0))),
                &displays,
                fallback
            ),
            (500.0, 700.0)
        );
        // Past the far pixel: the logical rectangle ends at origin + size - 1.
        assert_eq!(
            release_point(
                Some((HDMI1, PointDevice::new(5000.0, 5000.0))),
                &displays,
                fallback
            ),
            (1079.0, 2519.0)
        );
        assert_eq!(
            release_point(
                Some((HDMI1, PointDevice::new(-50.0, -50.0))),
                &displays,
                fallback
            ),
            (0.0, 600.0)
        );
    }

    #[test]
    fn release_point_converts_fractional_scale() {
        // 2160x1440 px at scale 1.5 is 1440x960 logical, at (100,50).
        let displays = [display(4, (2160, 1440), 1.5, (100.0, 50.0))];
        let fallback = (7.0, 8.0);
        assert_eq!(
            release_point(
                Some((DisplayId(4), PointDevice::new(1080.0, 720.0))),
                &displays,
                fallback
            ),
            (820.0, 530.0)
        );
        // Clamped to (100 + 1439, 50 + 959).
        assert_eq!(
            release_point(
                Some((DisplayId(4), PointDevice::new(3000.0, 3000.0))),
                &displays,
                fallback
            ),
            (1539.0, 1009.0)
        );
    }

    #[test]
    fn release_point_falls_back_without_a_usable_warp() {
        let displays = dev_displays();
        let fallback = (7.0, 8.0);
        assert_eq!(release_point(None, &displays, fallback), fallback);
        assert_eq!(
            release_point(
                Some((DisplayId(9), PointDevice::new(1.0, 1.0))),
                &displays,
                fallback
            ),
            fallback
        );
        assert_eq!(
            release_point(
                Some((HDMI1, PointDevice::new(f64::NAN, 100.0))),
                &displays,
                fallback
            ),
            fallback
        );
        assert_eq!(
            release_point(
                Some((HDMI1, PointDevice::new(1.0, f64::INFINITY))),
                &displays,
                fallback
            ),
            fallback
        );
    }
}
