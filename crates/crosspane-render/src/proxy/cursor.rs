//! Source cursor images for proxies (03 §4.6, WP-2.16).
//!
//! winit sizes a custom cursor in logical units, one per image pixel, so an image captured at the
//! proxy's density (2 device pixels per point on a Retina Mac) is box-filtered down by the
//! window's integer scale first; otherwise it would show at twice its size.

/// A cursor ready for `CustomCursor::from_rgba`.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Rgba {
    pub rgba: Vec<u8>,
    pub width: u16,
    pub height: u16,
    pub hotspot: (u16, u16),
}

/// What the proxy should show for a source cursor.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Shape {
    Hidden,
    Image(Rgba),
}

/// Convert a BGRA (straight alpha) cursor of `width` × `height` with `hotspot` for a window at
/// `scale`. `None` if the buffer doesn't match the size.
pub(super) fn shape(
    width: u32,
    height: u32,
    hotspot: (u32, u32),
    bgra: &[u8],
    scale: f64,
) -> Option<Shape> {
    let len = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)?;
    if width == 0 || height == 0 || bgra.len() != len || width > 256 || height > 256 {
        return None;
    }
    if bgra.as_chunks::<4>().0.iter().all(|p| p[3] == 0) {
        return Some(Shape::Hidden);
    }
    // Only whole factors that divide the image; anything else is shown as is.
    let factor = scale.round().max(1.0) as u32;
    let factor = if factor > 1 && (scale - f64::from(factor)).abs() < 0.01 {
        factor
    } else {
        1
    };
    let (w, h) = (width.div_ceil(factor), height.div_ceil(factor));
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h {
        for x in 0..w {
            // Average the factor × factor block with alpha weighting (straight alpha in and out).
            let (mut r, mut g, mut b, mut a, mut n) = (0u32, 0u32, 0u32, 0u32, 0u32);
            for sy in y * factor..((y + 1) * factor).min(height) {
                for sx in x * factor..((x + 1) * factor).min(width) {
                    let i = (sy as usize * width as usize + sx as usize) * 4;
                    let alpha = u32::from(bgra[i + 3]);
                    b += u32::from(bgra[i]) * alpha;
                    g += u32::from(bgra[i + 1]) * alpha;
                    r += u32::from(bgra[i + 2]) * alpha;
                    a += alpha;
                    n += 1;
                }
            }
            let pixel = match (r.checked_div(a), g.checked_div(a), b.checked_div(a)) {
                (Some(r), Some(g), Some(b)) => [r as u8, g as u8, b as u8, (a / n) as u8],
                _ => [0, 0, 0, 0],
            };
            rgba.extend_from_slice(&pixel);
        }
    }
    Some(Shape::Image(Rgba {
        rgba,
        width: w as u16,
        height: h as u16,
        hotspot: (
            (hotspot.0 / factor).min(w - 1) as u16,
            (hotspot.1 / factor).min(h - 1) as u16,
        ),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, bgra: [u8; 4]) -> Vec<u8> {
        bgra.repeat((width * height) as usize)
    }

    #[test]
    fn bgra_becomes_rgba_at_scale_one() {
        let shape = shape(2, 1, (1, 0), &[1, 2, 3, 255, 4, 5, 6, 128], 1.0).unwrap();
        assert_eq!(
            shape,
            Shape::Image(Rgba {
                rgba: vec![3, 2, 1, 255, 6, 5, 4, 128],
                width: 2,
                height: 1,
                hotspot: (1, 0),
            })
        );
    }

    #[test]
    fn a_transparent_cursor_is_hidden() {
        assert_eq!(
            shape(3, 3, (0, 0), &solid(3, 3, [9, 9, 9, 0]), 1.0),
            Some(Shape::Hidden)
        );
    }

    #[test]
    fn retina_images_are_halved() {
        let Some(Shape::Image(rgba)) =
            shape(48, 48, (9, 17), &solid(48, 48, [10, 20, 30, 255]), 2.0)
        else {
            panic!("expected an image");
        };
        assert_eq!((rgba.width, rgba.height, rgba.hotspot), (24, 24, (4, 8)));
        assert_eq!(&rgba.rgba[..4], &[30, 20, 10, 255]);
    }

    #[test]
    fn transparent_pixels_do_not_darken_edges() {
        // One opaque white pixel and three transparent black ones average to white at 1/4 alpha.
        let bgra = [255, 255, 255, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let Some(Shape::Image(rgba)) = shape(2, 2, (0, 0), &bgra, 2.0) else {
            panic!("expected an image");
        };
        assert_eq!(rgba.rgba, vec![255, 255, 255, 63]);
    }

    #[test]
    fn fractional_scales_and_bad_buffers() {
        let Some(Shape::Image(rgba)) = shape(4, 4, (3, 3), &solid(4, 4, [0, 0, 0, 255]), 1.5)
        else {
            panic!("expected an image");
        };
        assert_eq!((rgba.width, rgba.hotspot), (4, (3, 3)));
        assert_eq!(shape(4, 4, (0, 0), &[0; 10], 1.0), None);
        assert_eq!(shape(0, 4, (0, 0), &[], 1.0), None);
        assert_eq!(shape(300, 1, (0, 0), &solid(300, 1, [0; 4]), 1.0), None);
    }

    #[test]
    fn odd_sizes_keep_the_last_column() {
        let Some(Shape::Image(rgba)) = shape(5, 3, (4, 2), &solid(5, 3, [0, 0, 255, 255]), 2.0)
        else {
            panic!("expected an image");
        };
        assert_eq!((rgba.width, rgba.height, rgba.hotspot), (3, 2, (2, 1)));
        assert!(
            rgba.rgba
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| *p == [255, 0, 0, 255])
        );
    }
}
