//! Where a twin display sits on the Windows desktop (WP-W4.1d TWIN-ISO). Pure geometry, no Win32.
//!
//! Windows places a new IddCx monitor flush against the right edge of the desktop, where the
//! user's cursor, new windows and Win+arrow snapping can reach it. The twin only exists to hold a
//! parked window, so it is moved to where nothing reaches it by accident: diagonally past the
//! bottom-right of the desktop, touching the other displays at one corner point and sharing no
//! edge with any of them. A cursor cannot slide onto a display through an edge it does not share.
//!
//! The rule, in order:
//! 1. A candidate puts one corner of the twin exactly on a corner of another display (or another
//!    twin), with the twin on the far side of that corner (outside it in both axes).
//! 2. A candidate is refused when it overlaps any display, shares an edge segment of positive
//!    length with any display, or leaves the coordinate bound.
//! 3. The valid candidate with the largest `right + bottom` wins; ties go to the larger `bottom`,
//!    then the larger `right`. That is the bottom-right extreme of the desktop.
//!
//! There is always a valid candidate for a non-empty desktop inside the bound: the display with
//! the greatest bottom (and, among those, the greatest right) has its outer bottom-right corner
//! free. A second twin therefore chains off the first one's corner.

/// Left, top, right, bottom, in desktop pixels.
pub type Rect = [i32; 4];

/// Desktop coordinates stay inside +/-30000, well within what the window system supports.
pub const COORD_LIMIT: i64 = 30_000;

/// The twin's top-left position, or `None` when there is nothing to hang the twin on (no other
/// display, an empty twin size, or no candidate inside the coordinate bound). `others` are the
/// rects of every other active desktop display, real monitors and other twins alike.
pub fn twin_corner(others: &[Rect], width: u32, height: u32) -> Option<[i32; 2]> {
    if width == 0 || height == 0 {
        return None;
    }
    let (width, height) = (i64::from(width), i64::from(height));
    let others: Vec<[i64; 4]> = others
        .iter()
        .map(|r| r.map(i64::from))
        .filter(|r| r[0] < r[2] && r[1] < r[3])
        .collect();
    let mut best: Option<([i64; 2], (i64, i64, i64))> = None;
    for anchor in &others {
        let [left, top, right, bottom] = *anchor;
        // The twin's far-side positions at each of the anchor's four corners.
        let corners = [
            [right, bottom],
            [left - width, bottom],
            [right, top - height],
            [left - width, top - height],
        ];
        for [x, y] in corners {
            let twin = [x, y, x + width, y + height];
            if !inside_bound(twin) || others.iter().any(|other| conflicts(twin, *other)) {
                continue;
            }
            let score = (twin[2] + twin[3], twin[3], twin[2]);
            if best.is_none_or(|(_, held)| score > held) {
                best = Some(([x, y], score));
            }
        }
    }
    best.and_then(|([x, y], _)| Some([i32::try_from(x).ok()?, i32::try_from(y).ok()?]))
}

fn inside_bound(rect: [i64; 4]) -> bool {
    rect.iter().all(|value| value.abs() <= COORD_LIMIT)
}

/// The length of the overlap of two ranges, zero when they only touch or are apart.
fn overlap(a0: i64, a1: i64, b0: i64, b1: i64) -> i64 {
    (a1.min(b1) - a0.max(b0)).max(0)
}

/// True when the twin overlaps the other display or shares an edge segment of positive length
/// with it. Touching at a corner point is not a conflict.
fn conflicts(twin: [i64; 4], other: [i64; 4]) -> bool {
    let across = overlap(twin[0], twin[2], other[0], other[2]);
    let down = overlap(twin[1], twin[3], other[1], other[3]);
    if across > 0 && down > 0 {
        return true;
    }
    let shares_horizontal_edge = across > 0 && (twin[3] == other[1] || twin[1] == other[3]);
    let shares_vertical_edge = down > 0 && (twin[2] == other[0] || twin[0] == other[2]);
    shares_horizontal_edge || shares_vertical_edge
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRIMARY: Rect = [0, 0, 3440, 1440];
    const LEFT: Rect = [-1080, -492, 840, 588];
    const ABOVE: Rect = [755, -1080, 2675, 0];

    fn rect_at(position: [i32; 2], width: i32, height: i32) -> Rect {
        [
            position[0],
            position[1],
            position[0] + width,
            position[1] + height,
        ]
    }

    fn wide(rect: Rect) -> [i64; 4] {
        rect.map(i64::from)
    }

    /// The twin's relation to every other rect, for the invariants below.
    fn assert_isolated(twin: Rect, others: &[Rect]) {
        let mut corner_touch = false;
        for other in others {
            assert!(
                !conflicts(wide(twin), wide(*other)),
                "twin {twin:?} overlaps or shares an edge with {other:?}"
            );
            let (t, o) = (wide(twin), wide(*other));
            corner_touch |= (t[0] == o[2] || t[2] == o[0]) && (t[1] == o[3] || t[3] == o[1]);
        }
        assert!(corner_touch, "twin {twin:?} touches nothing at a corner");
    }

    #[test]
    fn twin_corner_owner_layout_is_below_right_of_the_primary() {
        // The live layout of 2026-10-12: Windows itself put the twin flush right at (3440, 0).
        let others = [PRIMARY, LEFT, ABOVE];
        let position = twin_corner(&others, 1920, 1080);
        assert_eq!(position, Some([3440, 1440]));
        assert_isolated(rect_at([3440, 1440], 1920, 1080), &others);
    }

    #[test]
    fn twin_corner_flush_right_default_is_never_chosen() {
        let others = [PRIMARY, LEFT, ABOVE];
        assert_ne!(twin_corner(&others, 1920, 1080), Some([3440, 0]));
    }

    #[test]
    fn twin_corner_single_monitor() {
        assert_eq!(
            twin_corner(&[[0, 0, 1920, 1080]], 1280, 720),
            Some([1920, 1080])
        );
    }

    #[test]
    fn twin_corner_skips_a_corner_that_would_share_an_edge() {
        // The first display's bottom-right corner sits under the second one's bottom edge, so a
        // twin there would share an edge with it. The second display's corner is used instead.
        let others = [[0, 0, 1920, 1080], [1920, 0, 3840, 1080]];
        assert_eq!(twin_corner(&others, 1920, 1080), Some([3840, 1080]));
    }

    #[test]
    fn twin_corner_staggered_monitors_use_the_lowest() {
        let others = [[0, 0, 1920, 1080], [1920, 200, 3840, 1280]];
        let position = twin_corner(&others, 1920, 1080);
        assert_eq!(position, Some([3840, 1280]));
    }

    #[test]
    fn twin_corner_second_twin_chains_off_the_first() {
        let first = rect_at([3440, 1440], 1920, 1080);
        let others = [PRIMARY, LEFT, ABOVE, first];
        let position = twin_corner(&others, 1920, 1080);
        assert_eq!(position, Some([5360, 2520]));
        assert_isolated(rect_at([5360, 2520], 1920, 1080), &others);
    }

    #[test]
    fn twin_corner_has_nothing_to_hang_on() {
        assert_eq!(twin_corner(&[], 1920, 1080), None);
        // An empty "display" is not an anchor.
        assert_eq!(twin_corner(&[[5, 5, 5, 9]], 1920, 1080), None);
        assert_eq!(twin_corner(&[PRIMARY], 0, 1080), None);
        assert_eq!(twin_corner(&[PRIMARY], 1920, 0), None);
    }

    #[test]
    fn twin_corner_refuses_to_leave_the_coordinate_bound() {
        let edge = i32::try_from(COORD_LIMIT).unwrap_or(i32::MAX);
        let others = [[edge - 1920, edge - 1080, edge, edge]];
        // Every candidate beyond the bottom or right is out; the top-left ones remain.
        let position = twin_corner(&others, 1920, 1080);
        assert_eq!(position, Some([edge - 3840, edge - 2160]));
        let only_far = [[edge - 100, -edge, edge, edge]];
        // A display that spans the whole bound leaves no room beyond it on either side.
        assert_eq!(twin_corner(&only_far, 1920, 1080), None);
    }

    #[test]
    fn twin_corner_invariants_hold_for_generated_layouts() {
        // A small deterministic generator: 1..=4 non-overlapping displays on a grid of cells.
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move |bound: u64| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) % bound
        };
        for _ in 0..2_000 {
            let mut layout: Vec<Rect> = Vec::new();
            for _ in 0..(1 + next(4)) {
                let width = 640 * (1 + next(6) as i32);
                let height = 360 * (1 + next(6) as i32);
                let x = 160 * (next(40) as i32 - 20);
                let y = 90 * (next(40) as i32 - 20);
                let candidate = rect_at([x, y], width, height);
                if layout.iter().all(|other| overlap_free(candidate, *other)) {
                    layout.push(candidate);
                }
            }
            let (width, height) = (1280 + 640 * next(2) as u32, 720 + 360 * next(2) as u32);
            let position = twin_corner(&layout, width, height);
            let Some(position) = position else {
                panic!("no candidate for {layout:?}");
            };
            let twin = rect_at(position, width as i32, height as i32);
            assert_isolated(twin, &layout);
            assert!(inside_bound(wide(twin)));
        }
    }

    fn overlap_free(a: Rect, b: Rect) -> bool {
        overlap(
            i64::from(a[0]),
            i64::from(a[2]),
            i64::from(b[0]),
            i64::from(b[2]),
        ) == 0
            || overlap(
                i64::from(a[1]),
                i64::from(a[3]),
                i64::from(b[1]),
                i64::from(b[3]),
            ) == 0
    }
}
