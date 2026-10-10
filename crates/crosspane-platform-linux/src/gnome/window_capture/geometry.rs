//! Pure geometry of window capture: the display a window is on, its device-pixel rect on that
//! display, and a caller's crop within the window. No OS calls, so it is tested without a session.

use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::euclid::point2;
use crosspane_types::geom::{DisplayGeometry, PixelRect, RectLogical};

/// The display whose logical rect contains the centre of `frame` (left and top edges inside, right
/// and bottom edges outside), or `None`. Displays that overlap (mirrored) give the lowest
/// `DisplayId`, so the answer doesn't depend on the order of `displays`. A display with invalid
/// geometry never matches.
pub(super) fn display_of<'a>(
    frame: &RectLogical,
    displays: &'a [DisplayInfo],
) -> Option<&'a DisplayInfo> {
    let x = frame.origin.x + frame.size.width / 2.0;
    let y = frame.origin.y + frame.size.height / 2.0;
    displays
        .iter()
        .filter(|display| centre_is_inside(&display.geometry, x, y))
        .min_by_key(|display| display.id)
}

/// The device-pixel rect of `frame` (logical) on the display `geometry` describes, clamped to that
/// display's own pixels. Per axis the origin is `round((frame.origin - display origin) * scale)`
/// and the size is `round(frame.size * scale)`, each rounded on its own as the parking code does.
/// `None` for invalid geometry, values that don't fit `i32` after scaling, a size under one pixel,
/// or a frame that misses the display.
pub(super) fn device_rect(frame: &RectLogical, geometry: &DisplayGeometry) -> Option<PixelRect> {
    if !geometry_is_usable(geometry) {
        return None;
    }
    // The display's own rect must be a `PixelRect` too, so a display past `i32` is refused.
    let width = i32::try_from(geometry.pixel_size.width).ok()?;
    let height = i32::try_from(geometry.pixel_size.height).ok()?;
    let scale = geometry.scale;
    let (min_x, max_x) = device_span(
        frame.origin.x,
        frame.size.width,
        geometry.logical_origin.x,
        scale,
        width,
    )?;
    let (min_y, max_y) = device_span(
        frame.origin.y,
        frame.size.height,
        geometry.logical_origin.y,
        scale,
        height,
    )?;
    Some(PixelRect::new(point2(min_x, min_y), point2(max_x, max_y)))
}

/// The crop of a window's capture that a caller asked for, in device pixels. `window` is the
/// window's rect; `caller` is a crop relative to the window's content, so its `(0, 0)` is
/// `window.min`. `None` passes the window through. Otherwise the crop is moved by `window.min` and
/// cut to the window. The result is `None` when it is empty, which includes an empty window or an
/// empty crop. The result always lies inside `window`.
pub(super) fn compose_crop(window: PixelRect, caller: Option<PixelRect>) -> Option<PixelRect> {
    if is_empty(&window) {
        return None;
    }
    let Some(caller) = caller else {
        return Some(window);
    };
    if is_empty(&caller) {
        return None;
    }
    // i64, so that moving the crop by `window.min` can't overflow (both operands fit i32).
    let (wx, wy) = (i64::from(window.min.x), i64::from(window.min.y));
    let low_x = wx.max(i64::from(caller.min.x) + wx);
    let low_y = wy.max(i64::from(caller.min.y) + wy);
    let high_x = i64::from(window.max.x).min(i64::from(caller.max.x) + wx);
    let high_y = i64::from(window.max.y).min(i64::from(caller.max.y) + wy);
    if low_x >= high_x || low_y >= high_y {
        return None;
    }
    // Between the window's own bounds, so the conversions to i32 succeed.
    Some(PixelRect::new(
        point2(i32::try_from(low_x).ok()?, i32::try_from(low_y).ok()?),
        point2(i32::try_from(high_x).ok()?, i32::try_from(high_y).ok()?),
    ))
}

/// The display checks of `windows.rs`: a finite positive scale, a nonzero pixel size and a finite
/// logical origin. A display failing one of them has no usable geometry.
fn geometry_is_usable(geometry: &DisplayGeometry) -> bool {
    let origin = geometry.logical_origin;
    geometry.scale.is_finite()
        && geometry.scale > 0.0
        && geometry.pixel_size.width != 0
        && geometry.pixel_size.height != 0
        && origin.x.is_finite()
        && origin.y.is_finite()
}

/// Whether the point (`x`, `y`) lies in the logical bounds of a display with usable geometry. The
/// bounds are half-open: left and top edges are inside, right and bottom edges are not. NaN is
/// never inside.
fn centre_is_inside(geometry: &DisplayGeometry, x: f64, y: f64) -> bool {
    if !geometry_is_usable(geometry) {
        return false;
    }
    let bounds = geometry.logical_bounds();
    x >= bounds.min_x() && x < bounds.max_x() && y >= bounds.min_y() && y < bounds.max_y()
}

/// One axis of [`device_rect`]: the span `[min, max)` of a frame in device pixels, clamped to
/// `0..pixels`. `None` when a value doesn't fit `i32`, the size is under one pixel, or nothing of
/// the frame is left on the display.
fn device_span(
    position: f64,
    extent: f64,
    origin: f64,
    scale: f64,
    pixels: i32,
) -> Option<(i32, i32)> {
    let min = round_to_i32((position - origin) * scale)?;
    let size = round_to_i32(extent * scale)?;
    if size < 1 {
        return None;
    }
    let max = i64::from(min).checked_add(i64::from(size))?;
    let low = i64::from(min).max(0);
    let high = max.min(i64::from(pixels));
    if low >= high {
        return None;
    }
    Some((i32::try_from(low).ok()?, i32::try_from(high).ok()?))
}

/// `value` rounded half away from zero, or `None` when it is not finite or doesn't fit `i32`. The
/// range is checked in `f64` before the cast, so nothing saturates silently.
fn round_to_i32(value: f64) -> Option<i32> {
    let rounded = value.round();
    if rounded.is_finite() && (f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(&rounded) {
        // In range by the check above, so the cast is exact.
        Some(rounded as i32)
    } else {
        None
    }
}

/// Whether `rect` has no pixels: `max` is not past `min` on some axis.
fn is_empty(rect: &PixelRect) -> bool {
    rect.max.x <= rect.min.x || rect.max.y <= rect.min.y
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use crosspane_types::color::ColorSpace;
    use crosspane_types::geom::{PixelSize, PointLogical, SizeLogical, SizeMm};
    use crosspane_types::id::DisplayId;

    use super::*;

    fn display(id: u32, width: u32, height: u32, scale: f64, origin: (f64, f64)) -> DisplayInfo {
        DisplayInfo {
            id: DisplayId(id),
            name: format!("test-{id}"),
            geometry: DisplayGeometry {
                physical_size: SizeMm::new(600.0, 340.0),
                pixel_size: PixelSize::new(width, height),
                scale,
                logical_origin: PointLogical::new(origin.0, origin.1),
            },
            refresh_millihz: 60_000,
            color_space: ColorSpace::Srgb,
            hdr: false,
        }
    }

    fn frame(x: f64, y: f64, width: f64, height: f64) -> RectLogical {
        RectLogical::new(PointLogical::new(x, y), SizeLogical::new(width, height))
    }

    fn rect(x0: i32, y0: i32, x1: i32, y1: i32) -> PixelRect {
        PixelRect::new(point2(x0, y0), point2(x1, y1))
    }

    /// The id of the display `display_of` picks.
    fn id_of(frame: &RectLogical, displays: &[DisplayInfo]) -> Option<DisplayId> {
        display_of(frame, displays).map(|display| display.id)
    }

    // ---- display_of ----

    #[test]
    fn display_is_the_one_containing_the_frame_centre() {
        let displays = [
            display(1, 1920, 1080, 1.0, (0.0, 0.0)),
            display(2, 1920, 1080, 1.0, (1920.0, 0.0)),
        ];
        // The window starts on display 1 but its centre (2100) is on display 2.
        let straddling = frame(1800.0, 100.0, 600.0, 400.0);
        assert_eq!(display_of(&straddling, &displays), Some(&displays[1]));
        let left = frame(100.0, 100.0, 600.0, 400.0);
        assert_eq!(id_of(&left, &displays), Some(DisplayId(1)));
    }

    #[test]
    fn display_edges_are_half_open() {
        let displays = [
            display(1, 1920, 1080, 1.0, (0.0, 0.0)),
            display(2, 1920, 1080, 1.0, (1920.0, 0.0)),
        ];
        // Centre exactly on the shared edge belongs to the display on its right.
        let on_edge = frame(1900.0, 0.0, 40.0, 10.0);
        assert_eq!(id_of(&on_edge, &displays), Some(DisplayId(2)));
        // Centre on the left edge of display 1 is inside; just past the bottom edge is outside.
        assert_eq!(
            id_of(&frame(-10.0, -10.0, 20.0, 20.0), &displays),
            Some(DisplayId(1))
        );
        assert_eq!(id_of(&frame(0.0, 1070.0, 10.0, 20.0), &displays), None);
    }

    #[test]
    fn display_is_none_outside_every_display_and_without_displays() {
        let displays = [display(1, 1920, 1080, 1.0, (0.0, 0.0))];
        assert_eq!(id_of(&frame(5000.0, 5000.0, 100.0, 100.0), &displays), None);
        assert_eq!(id_of(&frame(0.0, 0.0, 100.0, 100.0), &[]), None);
        // A NaN coordinate has no centre inside any display.
        assert_eq!(id_of(&frame(f64::NAN, 0.0, 100.0, 100.0), &displays), None);
    }

    #[test]
    fn display_uses_logical_size_and_negative_origins() {
        // 3840x2160 device px at 2x is 1920x1080 logical, placed left of the origin.
        let displays = [
            display(7, 3840, 2160, 2.0, (-1920.0, 0.0)),
            display(8, 1920, 1080, 1.0, (0.0, 0.0)),
        ];
        assert_eq!(
            id_of(&frame(-1000.0, 500.0, 200.0, 200.0), &displays),
            Some(DisplayId(7))
        );
        // Fractional scale: 2560 px at 1.25x is 2048 logical wide.
        let fractional = [display(3, 2560, 1440, 1.25, (0.0, 0.0))];
        assert_eq!(
            id_of(&frame(1950.0, 0.0, 100.0, 100.0), &fractional),
            Some(DisplayId(3))
        );
        assert_eq!(id_of(&frame(2040.0, 0.0, 100.0, 100.0), &fractional), None);
    }

    #[test]
    fn overlapping_displays_give_the_lowest_id_in_any_order() {
        let a = display(9, 1920, 1080, 1.0, (0.0, 0.0));
        let b = display(4, 1920, 1080, 1.0, (0.0, 0.0));
        let window = frame(100.0, 100.0, 100.0, 100.0);
        assert_eq!(id_of(&window, &[a.clone(), b.clone()]), Some(DisplayId(4)));
        assert_eq!(id_of(&window, &[b, a]), Some(DisplayId(4)));
    }

    #[test]
    fn displays_with_invalid_geometry_never_match() {
        let window = frame(10.0, 10.0, 10.0, 10.0);
        for bad in [
            display(1, 1920, 1080, 0.0, (0.0, 0.0)),
            display(1, 1920, 1080, -1.0, (0.0, 0.0)),
            display(1, 1920, 1080, f64::NAN, (0.0, 0.0)),
            display(1, 1920, 1080, f64::INFINITY, (0.0, 0.0)),
            display(1, 0, 1080, 1.0, (0.0, 0.0)),
            display(1, 1920, 0, 1.0, (0.0, 0.0)),
            display(1, 1920, 1080, 1.0, (f64::NAN, 0.0)),
            display(1, 1920, 1080, 1.0, (0.0, f64::INFINITY)),
        ] {
            assert_eq!(id_of(&window, &[bad]), None);
        }
        // A bad display doesn't hide a good one.
        let displays = [
            display(1, 1920, 1080, 0.0, (0.0, 0.0)),
            display(2, 1920, 1080, 1.0, (0.0, 0.0)),
        ];
        assert_eq!(id_of(&window, &displays), Some(DisplayId(2)));
    }

    // ---- device_rect ----

    #[test]
    fn device_rect_at_scale_one_on_the_origin_display() {
        let d = display(1, 1920, 1080, 1.0, (0.0, 0.0));
        assert_eq!(
            device_rect(&frame(100.0, 50.0, 300.0, 200.0), &d.geometry),
            Some(rect(100, 50, 400, 250))
        );
    }

    #[test]
    fn device_rect_scales_by_two() {
        let two = display(1, 3840, 2160, 2.0, (0.0, 0.0));
        assert_eq!(
            device_rect(&frame(10.0, 20.0, 100.0, 50.0), &two.geometry),
            Some(rect(20, 40, 220, 140))
        );
    }

    #[test]
    fn device_rect_rounds_origin_and_size_separately_at_fractional_scale() {
        let frac = display(1, 2560, 1440, 1.25, (0.0, 0.0));
        // 3 * 1.25 = 3.75 rounds to 4; 100 * 1.25 = 125 exactly.
        assert_eq!(
            device_rect(&frame(3.0, 3.0, 100.0, 100.0), &frac.geometry),
            Some(rect(4, 4, 129, 129))
        );
        // Halves round away from zero: 2 * 1.25 = 2.5 rounds to 3, and the size 1.25 to 1.
        assert_eq!(
            device_rect(&frame(2.0, 0.0, 1.0, 1.0), &frac.geometry),
            Some(rect(3, 0, 4, 1))
        );
    }

    #[test]
    fn device_rect_subtracts_the_display_origin() {
        // A display right of the origin: the frame at 1930 is 10 px into it.
        let right = display(2, 1920, 1080, 1.0, (1920.0, 0.0));
        assert_eq!(
            device_rect(&frame(1930.0, 10.0, 100.0, 50.0), &right.geometry),
            Some(rect(10, 10, 110, 60))
        );
        let right_two = display(2, 3840, 2160, 2.0, (1920.0, 0.0));
        assert_eq!(
            device_rect(&frame(2000.0, 100.0, 400.0, 200.0), &right_two.geometry),
            Some(rect(160, 200, 960, 600))
        );
    }

    #[test]
    fn device_rect_handles_a_negative_display_origin() {
        let left = display(3, 1920, 1080, 1.0, (-1920.0, -1080.0));
        assert_eq!(
            device_rect(&frame(-1900.0, -1000.0, 100.0, 50.0), &left.geometry),
            Some(rect(20, 80, 120, 130))
        );
        let left_two = display(3, 3840, 2160, 2.0, (-1920.0, -1080.0));
        assert_eq!(
            device_rect(&frame(-1000.0, -500.0, 100.0, 50.0), &left_two.geometry),
            Some(rect(1840, 1160, 2040, 1260))
        );
    }

    #[test]
    fn device_rect_clamps_on_each_side() {
        let d = display(1, 1920, 1080, 1.0, (0.0, 0.0));
        // Left: starts 20 px before the display.
        assert_eq!(
            device_rect(&frame(-20.0, 100.0, 100.0, 50.0), &d.geometry),
            Some(rect(0, 100, 80, 150))
        );
        // Top: starts 30 px above the display.
        assert_eq!(
            device_rect(&frame(100.0, -30.0, 100.0, 50.0), &d.geometry),
            Some(rect(100, 0, 200, 20))
        );
        // Right: runs 80 px past the right edge.
        assert_eq!(
            device_rect(&frame(1900.0, 100.0, 100.0, 50.0), &d.geometry),
            Some(rect(1900, 100, 1920, 150))
        );
        // Bottom: runs 20 px past the bottom edge.
        assert_eq!(
            device_rect(&frame(100.0, 1050.0, 100.0, 50.0), &d.geometry),
            Some(rect(100, 1050, 200, 1080))
        );
    }

    #[test]
    fn device_rect_is_none_for_a_frame_missing_the_display() {
        let d = display(1, 1920, 1080, 1.0, (0.0, 0.0));
        for off in [
            frame(2000.0, 100.0, 100.0, 50.0),
            // Touching the right edge is outside: the display's edges are half-open.
            frame(1920.0, 0.0, 100.0, 100.0),
            frame(-200.0, 0.0, 100.0, 50.0),
            frame(0.0, -100.0, 10.0, 50.0),
            frame(100.0, 1080.0, 10.0, 10.0),
        ] {
            assert_eq!(device_rect(&off, &d.geometry), None);
        }
    }

    #[test]
    fn device_rect_is_none_for_an_empty_or_negative_frame() {
        let d = display(1, 1920, 1080, 1.0, (0.0, 0.0));
        assert_eq!(
            device_rect(&frame(10.0, 10.0, 0.0, 50.0), &d.geometry),
            None
        );
        assert_eq!(
            device_rect(&frame(10.0, 10.0, -5.0, 50.0), &d.geometry),
            None
        );
        assert_eq!(
            device_rect(&frame(10.0, 10.0, 50.0, -1.0), &d.geometry),
            None
        );
        assert_eq!(
            device_rect(&frame(10.0, 10.0, 50.0, 0.0), &d.geometry),
            None
        );
    }

    #[test]
    fn device_rect_is_none_when_the_scaled_size_rounds_below_one_pixel() {
        let d = display(1, 1920, 1080, 1.0, (0.0, 0.0));
        assert_eq!(device_rect(&frame(10.0, 10.0, 0.4, 0.4), &d.geometry), None);
        // Half a pixel rounds up to one.
        assert_eq!(
            device_rect(&frame(10.0, 10.0, 0.5, 0.5), &d.geometry),
            Some(rect(10, 10, 11, 11))
        );
        let frac = display(2, 2560, 1440, 1.25, (0.0, 0.0));
        // 0.3 * 1.25 = 0.375 rounds to 0.
        assert_eq!(
            device_rect(&frame(10.0, 10.0, 0.3, 0.3), &frac.geometry),
            None
        );
    }

    #[test]
    fn device_rect_is_none_for_non_finite_frames() {
        let d = display(1, 1920, 1080, 1.0, (0.0, 0.0));
        for bad in [
            frame(f64::NAN, 0.0, 10.0, 10.0),
            frame(0.0, f64::NAN, 10.0, 10.0),
            frame(0.0, 0.0, f64::NAN, 10.0),
            frame(0.0, 0.0, 10.0, f64::NAN),
            frame(f64::INFINITY, 0.0, 10.0, 10.0),
            frame(f64::NEG_INFINITY, 0.0, 10.0, 10.0),
            frame(0.0, f64::INFINITY, 10.0, 10.0),
            frame(0.0, 0.0, f64::INFINITY, 10.0),
            frame(0.0, 0.0, 10.0, f64::NEG_INFINITY),
        ] {
            assert_eq!(device_rect(&bad, &d.geometry), None);
        }
    }

    #[test]
    fn device_rect_is_none_for_values_past_i32() {
        let d = display(1, 1920, 1080, 1.0, (0.0, 0.0));
        assert_eq!(
            device_rect(&frame(3.0e9, 0.0, 10.0, 10.0), &d.geometry),
            None
        );
        assert_eq!(
            device_rect(&frame(-3.0e9, 0.0, 10.0, 10.0), &d.geometry),
            None
        );
        assert_eq!(
            device_rect(&frame(0.0, 0.0, 3.0e9, 10.0), &d.geometry),
            None
        );
        assert_eq!(
            device_rect(&frame(f64::MAX, 0.0, 10.0, 10.0), &d.geometry),
            None
        );
        // Each value fits i32, and so does their sum, but the rect lies past the display.
        assert_eq!(
            device_rect(&frame(2.0e9, 0.0, 2.0e9, 10.0), &d.geometry),
            None
        );
        // A huge scale pushes even a one-pixel frame past i32 or past the display.
        let huge = display(2, 1920, 1080, 1.0e12, (0.0, 0.0));
        assert_eq!(
            device_rect(&frame(1.0, 1.0, 1.0, 1.0), &huge.geometry),
            None
        );
        let big = display(3, 1920, 1080, 1.0e9, (0.0, 0.0));
        assert_eq!(device_rect(&frame(1.0, 1.0, 1.0, 1.0), &big.geometry), None);
        // The difference of origins overflows f64 to infinity: refused, without a panic.
        let far = display(4, 1920, 1080, 1.0, (-f64::MAX, 0.0));
        assert_eq!(
            device_rect(&frame(f64::MAX, 0.0, 10.0, 10.0), &far.geometry),
            None
        );
    }

    #[test]
    fn device_rect_is_none_for_invalid_display_geometry() {
        let window = frame(10.0, 10.0, 10.0, 10.0);
        let valid = display(1, 1920, 1080, 1.0, (0.0, 0.0));
        assert_eq!(
            device_rect(&window, &valid.geometry),
            Some(rect(10, 10, 20, 20))
        );
        for bad in [
            display(1, 1920, 1080, 0.0, (0.0, 0.0)),
            display(1, 1920, 1080, -1.0, (0.0, 0.0)),
            display(1, 1920, 1080, f64::NAN, (0.0, 0.0)),
            display(1, 1920, 1080, f64::INFINITY, (0.0, 0.0)),
            display(1, 0, 1080, 1.0, (0.0, 0.0)),
            display(1, 1920, 0, 1.0, (0.0, 0.0)),
            display(1, 1920, 1080, 1.0, (f64::NAN, 0.0)),
            display(1, 1920, 1080, 1.0, (0.0, f64::INFINITY)),
        ] {
            assert_eq!(device_rect(&window, &bad.geometry), None);
        }
        // A pixel size past i32 has no `PixelRect` to clamp to.
        let mut wide = display(1, 1920, 1080, 1.0, (0.0, 0.0));
        wide.geometry.pixel_size = PixelSize::new(u32::MAX, 1080);
        assert_eq!(device_rect(&window, &wide.geometry), None);
    }

    // ---- compose_crop ----

    #[test]
    fn compose_crop_without_a_caller_passes_the_window_through() {
        assert_eq!(
            compose_crop(rect(100, 100, 200, 200), None),
            Some(rect(100, 100, 200, 200))
        );
    }

    #[test]
    fn compose_crop_at_the_window_top_left() {
        assert_eq!(
            compose_crop(rect(100, 50, 500, 400), Some(rect(0, 0, 100, 80))),
            Some(rect(100, 50, 200, 130))
        );
    }

    #[test]
    fn compose_crop_in_the_middle_of_a_window_not_at_the_origin() {
        // The crop is relative to the window's content: (10, 20) is at (-40 + 10, 30 + 20).
        assert_eq!(
            compose_crop(rect(-40, 30, 760, 630), Some(rect(10, 20, 110, 70))),
            Some(rect(-30, 50, 70, 100))
        );
    }

    #[test]
    fn compose_crop_partly_outside_the_window_is_cut_to_it() {
        let window = rect(0, 0, 100, 100);
        assert_eq!(
            compose_crop(window, Some(rect(80, 80, 200, 200))),
            Some(rect(80, 80, 100, 100))
        );
        assert_eq!(
            compose_crop(window, Some(rect(-10, -10, 30, 30))),
            Some(rect(0, 0, 30, 30))
        );
    }

    #[test]
    fn compose_crop_entirely_outside_the_window_is_none() {
        let window = rect(0, 0, 100, 100);
        assert_eq!(compose_crop(window, Some(rect(100, 0, 200, 50))), None);
        assert_eq!(compose_crop(window, Some(rect(200, 200, 300, 300))), None);
        assert_eq!(
            compose_crop(window, Some(rect(-200, -200, -100, -100))),
            None
        );
        // Touching the window's top edge from above is outside too (max is exclusive).
        assert_eq!(compose_crop(window, Some(rect(0, -50, 100, 0))), None);
    }

    #[test]
    fn compose_crop_with_a_negative_origin_partly_overlapping() {
        // Crop origin (-20, -20) moves to (80, 80): its part inside the window is 100..130.
        assert_eq!(
            compose_crop(rect(100, 100, 200, 200), Some(rect(-20, -20, 30, 30))),
            Some(rect(100, 100, 130, 130))
        );
        // The same on a window with negative coordinates: the crop moves to
        // (-220, -120)..(-170, -70) and is cut to the window's (-200, -100)..(-100, 0).
        assert_eq!(
            compose_crop(rect(-200, -100, -100, 0), Some(rect(-20, -20, 30, 30))),
            Some(rect(-200, -100, -170, -70))
        );
    }

    #[test]
    fn compose_crop_of_an_empty_crop_is_none() {
        let window = rect(0, 0, 100, 100);
        assert_eq!(compose_crop(window, Some(rect(10, 10, 10, 50))), None);
        assert_eq!(compose_crop(window, Some(rect(10, 10, 5, 50))), None);
        assert_eq!(compose_crop(window, Some(rect(10, 10, 50, 10))), None);
        assert_eq!(compose_crop(window, Some(rect(10, 10, 50, 5))), None);
        assert_eq!(compose_crop(window, Some(rect(0, 0, 0, 0))), None);
    }

    #[test]
    fn compose_crop_of_an_empty_window_is_none() {
        assert_eq!(compose_crop(rect(10, 10, 10, 50), None), None);
        assert_eq!(compose_crop(rect(10, 10, 50, 10), None), None);
        assert_eq!(compose_crop(rect(10, 10, 5, 5), None), None);
        assert_eq!(
            compose_crop(rect(10, 10, 10, 50), Some(rect(-100, -100, 100, 100))),
            None
        );
    }

    #[test]
    fn compose_crop_at_the_extremes_does_not_overflow() {
        let full = rect(i32::MIN, i32::MIN, i32::MAX, i32::MAX);
        // No caller: the window itself, however large.
        assert_eq!(compose_crop(full, None), Some(full));
        // A caller as big as the window moves past it: it is cut to the window's start.
        assert_eq!(
            compose_crop(full, Some(full)),
            Some(rect(i32::MIN, i32::MIN, -1, -1))
        );
        // A crop of the window's last pixel, at the window's far corner.
        assert_eq!(
            compose_crop(
                rect(0, 0, i32::MAX, i32::MAX),
                Some(rect(i32::MAX - 1, i32::MAX - 1, i32::MAX, i32::MAX))
            ),
            Some(rect(i32::MAX - 1, i32::MAX - 1, i32::MAX, i32::MAX))
        );
        // A window at the far corner and a crop moved past it: nothing left, and no overflow.
        assert_eq!(
            compose_crop(
                rect(i32::MAX - 1, i32::MAX - 1, i32::MAX, i32::MAX),
                Some(rect(i32::MAX - 1, i32::MAX - 1, i32::MAX, i32::MAX))
            ),
            None
        );
        // A one-pixel window in the corner is kept whole by a crop that covers it.
        let corner = rect(i32::MIN, i32::MIN, i32::MIN + 1, i32::MIN + 1);
        assert_eq!(compose_crop(corner, Some(full)), Some(corner));
        // A crop of an extreme empty box is none.
        assert_eq!(
            compose_crop(full, Some(rect(i32::MIN, i32::MIN, i32::MIN, i32::MIN))),
            None
        );
    }

    #[test]
    fn compose_crop_result_is_none_or_a_non_empty_part_of_the_window() {
        const GRID: [i32; 5] = [-3, 0, 2, 5, 9];
        let mut crops = vec![None];
        for &x0 in &GRID {
            for &y0 in &GRID {
                for &x1 in &GRID {
                    for &y1 in &GRID {
                        crops.push(Some(rect(x0, y0, x1, y1)));
                    }
                }
            }
        }
        for &x0 in &GRID {
            for &y0 in &GRID {
                for &x1 in &GRID {
                    for &y1 in &GRID {
                        let window = rect(x0, y0, x1, y1);
                        for caller in &crops {
                            let result = compose_crop(window, *caller);
                            if caller.is_none() {
                                // No caller: the window itself, unless the window is empty.
                                assert_eq!(result.is_some(), !is_empty(&window), "{window:?}");
                            }
                            if let Some(part) = result {
                                assert!(
                                    part.min.x < part.max.x && part.min.y < part.max.y,
                                    "empty result for {window:?} and {caller:?}"
                                );
                                assert!(
                                    window.min.x <= part.min.x
                                        && window.min.y <= part.min.y
                                        && part.max.x <= window.max.x
                                        && part.max.y <= window.max.y,
                                    "{part:?} is outside {window:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
