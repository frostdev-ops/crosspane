//! The pure sizing and scale rules of the twin (WP-G2.4 B2). No I/O.
//!
//! **Size.** The twin is sized in device pixels to the content parked on it plus headroom, and it
//! only grows. Every side is a multiple of [`STEP`] (64) within [`MIN_EXTENT`]`..=`[`MAX_EXTENT`].
//! A twin that already holds the content is left alone; one that does not grows, per side that
//! does not fit, to `max(needed * 1.25, current)` rounded up to [`STEP`]; a new twin is made at
//! `needed * 1.25` rounded up. Every side of a twin that fits stays as it is.
//!
//! **Scale.** The twin's logical monitor gets the destination's scale, reduced to the largest
//! value the twin's mode lists as supported that is not above it **and** gives an integral logical
//! width and height (Mutter rejects "Scaled logical monitor size is fractional"), else `1.0`.

use crosspane_types::geom::PixelSize;

use super::super::display_config::{ModeState, pick_scale};

/// Every side of a twin is a multiple of this many device pixels.
pub const STEP: u32 = 64;
/// The smallest side of a twin (device pixels).
pub const MIN_EXTENT: u32 = 64;
/// The largest side of a twin (device pixels).
pub const MAX_EXTENT: u32 = 8192;
/// Headroom a twin is made or grown with, so a proxy that grows a little does not renegotiate.
const HEADROOM: f64 = 1.25;
/// How far a logical size may be from a whole number and still count as integral.
const INTEGRAL_SLACK: f64 = 1e-6;

/// What the twin has to do about a content size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resize {
    /// The twin already holds the content (or, with no twin yet, never: see [`plan`]).
    Keep,
    /// Create the twin at, or grow it to, this size.
    To(PixelSize),
}

/// What a twin holding `current` (none yet: `None`) has to do to hold `needed` device pixels of
/// content. `None` when `needed` cannot be served by any twin: a zero side, or a side beyond
/// [`MAX_EXTENT`].
pub fn plan(needed: PixelSize, current: Option<PixelSize>) -> Option<Resize> {
    let width = axis(needed.width, current.map(|c| c.width))?;
    let height = axis(needed.height, current.map(|c| c.height))?;
    let size = PixelSize::new(width, height);
    Some(if current == Some(size) {
        Resize::Keep
    } else {
        Resize::To(size)
    })
}

/// One side: kept when the current one holds `needed`, else grown (or created) with headroom.
fn axis(needed: u32, current: Option<u32>) -> Option<u32> {
    if needed == 0 || needed > MAX_EXTENT {
        return None;
    }
    if let Some(current) = current
        && needed <= current
    {
        return Some(current);
    }
    let wanted = (f64::from(needed) * HEADROOM).ceil();
    // `wanted` is at most 10_240, far inside `u32`.
    let wanted = (wanted as u32).max(current.unwrap_or(0));
    Some(round_up(wanted).clamp(MIN_EXTENT, MAX_EXTENT))
}

/// `value` rounded up to a multiple of [`STEP`].
fn round_up(value: u32) -> u32 {
    value.div_ceil(STEP).saturating_mul(STEP)
}

/// Whether `scale` gives `mode` an integral logical width and height.
pub fn is_integral(mode: &ModeState, scale: f64) -> bool {
    let whole = |pixels: i32| {
        let logical = f64::from(pixels) / scale;
        logical >= 1.0 && (logical - logical.round()).abs() < INTEGRAL_SLACK
    };
    scale.is_finite() && scale > 0.0 && whole(mode.width) && whole(mode.height)
}

/// The scale for the twin's logical monitor: the largest of the mode's supported scales that is at
/// most `wanted` and gives an integral logical size, else `1.0`.
pub fn pick_twin_scale(mode: &ModeState, wanted: f64) -> f64 {
    let usable: Vec<f64> = mode
        .supported_scales
        .iter()
        .copied()
        .filter(|scale| is_integral(mode, *scale))
        .collect();
    pick_scale(&usable, wanted)
}

/// Like [`pick_twin_scale`], but a scale the twin already has stays when the (new) mode still
/// supports it with an integral logical size: windows parked on the twin keep their density.
pub fn keep_or_pick(existing: Option<f64>, mode: &ModeState, wanted: f64) -> f64 {
    match existing {
        Some(scale)
            if is_integral(mode, scale)
                && mode
                    .supported_scales
                    .iter()
                    .any(|supported| (supported - scale).abs() < INTEGRAL_SLACK) =>
        {
            scale
        }
        _ => pick_twin_scale(mode, wanted),
    }
}

/// The scale a display has when `logical` units cover `pixels` device pixels, as
/// `wayland_outputs` derives it (`pixels / logical`). `None` for a non-positive extent.
pub fn effective_scale(pixels: u32, logical: i32) -> Option<f64> {
    (pixels > 0 && logical > 0).then(|| f64::from(pixels) / f64::from(logical))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(width: u32, height: u32) -> PixelSize {
        PixelSize::new(width, height)
    }

    fn mode(width: i32, height: i32, scales: &[f64]) -> ModeState {
        ModeState {
            id: format!("{width}x{height}@60.000"),
            width,
            height,
            refresh: 60.0,
            preferred_scale: 1.0,
            supported_scales: scales.to_vec(),
            current: true,
            preferred: false,
        }
    }

    #[test]
    fn a_new_twin_has_headroom_and_whole_steps() {
        // 1800 * 1.25 = 2250 -> 2304; 1169 * 1.25 = 1461.25 -> 1462 -> 1472.
        assert_eq!(plan(px(1800, 1169), None), Some(Resize::To(px(2304, 1472))));
        // Exactly one step of headroom: 64 * 1.25 = 80 -> 128.
        assert_eq!(plan(px(64, 64), None), Some(Resize::To(px(128, 128))));
        // 1 pixel still makes a whole step minimum.
        assert_eq!(plan(px(1, 1), None), Some(Resize::To(px(64, 64))));
    }

    #[test]
    fn the_largest_side_is_clamped_to_the_maximum() {
        assert_eq!(plan(px(8192, 8192), None), Some(Resize::To(px(8192, 8192))));
        assert_eq!(plan(px(7000, 100), None), Some(Resize::To(px(8192, 128))));
    }

    #[test]
    fn content_no_twin_can_hold_is_refused() {
        assert_eq!(plan(px(0, 100), None), None);
        assert_eq!(plan(px(100, 0), Some(px(256, 256))), None);
        assert_eq!(plan(px(8193, 100), None), None);
        assert_eq!(plan(px(100, 8193), Some(px(8192, 8192))), None);
    }

    #[test]
    fn a_twin_that_holds_the_content_is_kept() {
        assert_eq!(
            plan(px(1800, 1000), Some(px(2304, 1472))),
            Some(Resize::Keep)
        );
        assert_eq!(
            plan(px(2304, 1472), Some(px(2304, 1472))),
            Some(Resize::Keep)
        );
        // Smaller content never shrinks it.
        assert_eq!(plan(px(64, 64), Some(px(2304, 1472))), Some(Resize::Keep));
    }

    #[test]
    fn only_the_sides_that_do_not_fit_grow() {
        // Width 2400 > 2304 grows to 3000 -> 3008; the height fits and stays 1472.
        assert_eq!(
            plan(px(2400, 1000), Some(px(2304, 1472))),
            Some(Resize::To(px(3008, 1472)))
        );
        // Height only: 1500 * 1.25 = 1875 -> 1920.
        assert_eq!(
            plan(px(1000, 1500), Some(px(2304, 1472))),
            Some(Resize::To(px(2304, 1920)))
        );
        // Both: 3750 -> 3776 and 2500 -> 2560.
        assert_eq!(
            plan(px(3000, 2000), Some(px(2304, 1472))),
            Some(Resize::To(px(3776, 2560)))
        );
    }

    #[test]
    fn growing_just_past_the_current_size_adds_the_headroom() {
        // One pixel past 2304 asks for 2881 -> 2944, not 2368.
        assert_eq!(
            plan(px(2305, 100), Some(px(2304, 1472))),
            Some(Resize::To(px(2944, 1472)))
        );
    }

    #[test]
    fn a_grown_side_never_ends_smaller_than_it_was() {
        for current in [64, 640, 2304, 8192] {
            for needed in [1, 63, 64, 65, 1000, 2305, 8191, 8192] {
                match plan(px(needed, 100), Some(px(current, 128))) {
                    Some(Resize::To(size)) => {
                        assert!(size.width > current, "{needed} against {current}");
                        assert!(size.width >= needed);
                        assert_eq!(size.width % STEP, 0);
                        assert!(size.width <= MAX_EXTENT);
                        assert_eq!(size.height, 128);
                    }
                    Some(Resize::Keep) => assert!(needed <= current && 100 <= 128),
                    None => panic!("{needed} against {current}"),
                }
            }
        }
    }

    #[test]
    fn effective_scale_is_pixels_over_logical() {
        assert_eq!(effective_scale(2304, 1152), Some(2.0));
        assert_eq!(effective_scale(1920, 1920), Some(1.0));
        assert_eq!(effective_scale(0, 10), None);
        assert_eq!(effective_scale(10, 0), None);
        assert_eq!(effective_scale(10, -1), None);
    }

    #[test]
    fn integral_logical_sizes_are_recognised() {
        let m = mode(2304, 1472, &[1.0, 2.0]);
        assert!(is_integral(&m, 1.0));
        assert!(is_integral(&m, 2.0));
        // 2304 / 1.5 = 1536 but 1472 / 1.5 = 981.33.
        assert!(!is_integral(&m, 1.5));
        assert!(!is_integral(&m, 0.0));
        assert!(!is_integral(&m, f64::NAN));
        assert!(!is_integral(&m, -1.0));
    }

    #[test]
    fn the_scale_is_the_largest_supported_integral_one_not_above_the_wanted() {
        let m = mode(2304, 1472, &[1.0, 1.5, 2.0, 4.0]);
        // 1.5 is listed but fractional here; 2.0 is the destination's.
        assert_eq!(pick_twin_scale(&m, 2.0), 2.0);
        // A destination at 1.5 falls to 1.0, not to the fractional 1.5.
        assert_eq!(pick_twin_scale(&m, 1.5), 1.0);
        // Never above the wanted.
        assert_eq!(pick_twin_scale(&m, 1.99), 1.0);
        // Above everything listed: the largest listed that is integral (4.0 divides both sides).
        assert_eq!(pick_twin_scale(&m, 8.0), 4.0);
        // Nothing listed: 1.0.
        assert_eq!(pick_twin_scale(&mode(2304, 1472, &[]), 2.0), 1.0);
        // Everything listed is fractional: 1.0.
        assert_eq!(pick_twin_scale(&mode(1000, 1000, &[3.0, 7.0]), 7.0), 1.0);
    }

    #[test]
    fn fractional_destination_scales_work_when_the_size_allows() {
        // 1920x1920 divides by 1.25 and 1.5 into whole numbers.
        let m = mode(1920, 1920, &[1.0, 1.25, 1.5, 2.0]);
        assert_eq!(pick_twin_scale(&m, 1.5), 1.5);
        assert_eq!(pick_twin_scale(&m, 1.25), 1.25);
    }

    #[test]
    fn an_existing_scale_stays_while_the_new_mode_supports_it() {
        let grown = mode(3008, 1472, &[1.0, 2.0]);
        assert_eq!(keep_or_pick(Some(2.0), &grown, 1.0), 2.0);
        // Unsupported or fractional in the new mode: the wanted scale is picked again.
        assert_eq!(keep_or_pick(Some(1.5), &grown, 2.0), 2.0);
        // No scale yet: picked.
        assert_eq!(keep_or_pick(None, &grown, 2.0), 2.0);
        // Supported but not integral in the new mode.
        let odd = mode(3008 + 64, 1473, &[1.0, 2.0]);
        assert_eq!(keep_or_pick(Some(2.0), &odd, 1.0), 1.0);
    }
}
