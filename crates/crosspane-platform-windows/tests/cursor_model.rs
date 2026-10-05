#![allow(clippy::unwrap_used)] // Pure test fixtures may assert successful conversion.
use crosspane_platform_windows::model::cursor::{
    Cache, History, Mask, Rows, Shape, convert, over_content,
};
use crosspane_types::geom::{PixelRect, PixelSize};
use proptest::prelude::*;

fn mask(size: PixelSize, bytes: &[u8]) -> Mask<'_> {
    Mask {
        size,
        rows: Rows {
            stride: size.width.div_ceil(8) as usize,
            pixels: bytes,
        },
    }
}
fn colour(size: PixelSize, bytes: &[u8], and: &[u8]) -> crosspane_platform::CursorImage {
    convert(
        size,
        (0, 0),
        Some(Rows {
            stride: size.width as usize * 4,
            pixels: bytes,
        }),
        mask(size, and),
        (1, 1),
    )
    .unwrap()
}

#[test]
fn alpha_colour_is_straight_bgra_and_top_row_first() {
    let image = colour(
        PixelSize::new(1, 2),
        &[32, 64, 128, 128, 7, 8, 9, 0],
        &[0, 0],
    );
    assert_eq!(&*image.pixels, &[64, 128, 255, 128, 0, 0, 0, 0]);
}
#[test]
fn colour_without_alpha_uses_and_mask_not_rgb_black() {
    let image = colour(
        PixelSize::new(2, 1),
        &[0, 0, 0, 0, 3, 4, 5, 0],
        &[0b01000000],
    );
    assert_eq!(&*image.pixels, &[0, 0, 0, 255, 0, 0, 0, 0]);
}
#[test]
fn monochrome_and_xor_table_has_documented_inversion_approximation() {
    let image = convert(
        PixelSize::new(4, 1),
        (1, 0),
        None,
        mask(PixelSize::new(4, 2), &[0b00110000, 0b01010000]),
        (1, 1),
    )
    .unwrap();
    assert_eq!(
        &*image.pixels,
        &[
            0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 0, 128, 128, 128, 255
        ]
    );
    assert_eq!(image.hotspot, (1, 0));
}
#[test]
fn density_and_256_bound_scale_image_and_hotspot_together() {
    let size = PixelSize::new(300, 200);
    let pixels = vec![255; 300 * 200 * 4];
    let and = vec![0; 38 * 200];
    let image = convert(
        size,
        (299, 199),
        Some(Rows {
            stride: 1200,
            pixels: &pixels,
        }),
        Mask {
            size,
            rows: Rows {
                stride: 38,
                pixels: &and,
            },
        },
        (2, 1),
    )
    .unwrap();
    assert_eq!(image.size, PixelSize::new(256, 170));
    assert_eq!(image.hotspot, (255, 169));
    assert_eq!(image.pixels.len(), 256 * 170 * 4);
    let image = convert(
        PixelSize::new(2, 1),
        (1, 0),
        Some(Rows {
            stride: 8,
            pixels: &[255; 8],
        }),
        mask(PixelSize::new(2, 1), &[0]),
        (3, 2),
    )
    .unwrap();
    assert_eq!(image.size, PixelSize::new(3, 1));
    assert_eq!(image.hotspot, (1, 0));
}
#[test]
fn malformed_dimensions_stride_masks_and_density_refuse() {
    for size in [
        PixelSize::new(0, 1),
        PixelSize::new(u32::MAX, u32::MAX),
        PixelSize::new(4097, 1),
    ] {
        assert!(
            convert(
                size,
                (0, 0),
                None,
                mask(PixelSize::new(1, 2), &[0, 0]),
                (1, 1)
            )
            .is_none()
        );
    }
    let size = PixelSize::new(2, 2);
    for (stride, bytes, density) in [
        (7, vec![255; 16], (1, 1)),
        (8, vec![255; 15], (1, 1)),
        (8, vec![255; 16], (0, 1)),
        (8, vec![255; 16], (1, 0)),
    ] {
        assert!(
            convert(
                size,
                (0, 0),
                Some(Rows {
                    stride,
                    pixels: &bytes
                }),
                mask(size, &[0, 0]),
                density
            )
            .is_none()
        );
    }
    assert!(
        convert(
            size,
            (0, 0),
            None,
            mask(PixelSize::new(2, 3), &[0; 3]),
            (1, 1)
        )
        .is_none()
    );
}
#[test]
fn row_padding_and_last_row_need_no_trailing_padding() {
    let image = convert(
        PixelSize::new(1, 2),
        (u32::MAX, u32::MAX),
        Some(Rows {
            stride: 8,
            pixels: &[1, 2, 3, 255, 99, 99, 99, 99, 4, 5, 6, 255],
        }),
        Mask {
            size: PixelSize::new(1, 2),
            rows: Rows {
                stride: 4,
                pixels: &[0; 5],
            },
        },
        (1, 1),
    )
    .unwrap();
    assert_eq!(&*image.pixels, &[1, 2, 3, 255, 4, 5, 6, 255]);
    assert_eq!(image.hotspot, (0, 1));
}
#[test]
fn physical_content_crop_root_and_resize_guards_are_half_open() {
    let bounds = [-100, 20, 100, 120];
    let captured = PixelSize::new(200, 100);
    let crop = Some(PixelRect::new((20, 10).into(), (80, 50).into()));
    assert!(over_content((-80, 30), bounds, captured, crop, true));
    for point in [(-81, 30), (-20, 30), (-80, 70), (-101, 30), (100, 120)] {
        assert!(!over_content(point, bounds, captured, crop, true));
    }
    assert!(!over_content((-80, 30), bounds, captured, crop, false));
    assert!(!over_content(
        (-80, 30),
        bounds,
        PixelSize::new(201, 100),
        crop,
        true
    ));
    assert!(!over_content(
        (0, 0),
        [i32::MIN, 0, i32::MAX, 1],
        captured,
        None,
        true
    ));
}
#[test]
fn first_change_hide_default_and_reentry_are_reported_once() {
    let mut history = History::default();
    let image = Shape::Image(colour(PixelSize::new(1, 1), &[1, 2, 3, 255], &[0]));
    for shape in [image.clone(), Shape::Hidden, Shape::Default, image.clone()] {
        assert_eq!(history.observe(true, shape.clone()), Some(shape.clone()));
        assert_eq!(history.observe(true, shape), None);
    }
    assert_eq!(history.observe(false, Shape::Hidden), None);
    assert_eq!(history.observe(true, image.clone()), Some(image));
}
#[test]
fn same_handle_new_content_invalidates_cache_and_equal_shape_is_suppressed() {
    let mut cache = Cache::default();
    let mut calls = 0;
    for (handle, content, expected_calls) in [(1, 10, 1), (1, 10, 1), (1, 11, 2), (2, 11, 3)] {
        let value = cache.read(handle, content, || {
            calls += 1;
            Some(colour(
                PixelSize::new(1, 1),
                &[content as u8, 0, 0, 255],
                &[0],
            ))
        });
        assert!(value.is_some());
        assert_eq!(calls, expected_calls);
    }
    let mut history = History::default();
    let a = colour(PixelSize::new(1, 1), &[1, 2, 3, 255], &[0]);
    assert!(history.observe(true, Shape::Image(a.clone())).is_some());
    assert!(history.observe(true, Shape::Image(a)).is_none());
}
proptest! {
    #[test]
    fn arbitrary_cursor_bytes_and_bounds_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..1024), width in any::<u32>(), height in any::<u32>(), stride in any::<u16>()) {
        let _ = convert(PixelSize::new(width, height), (width, height), Some(Rows { stride: stride as usize, pixels: &bytes }), Mask { size: PixelSize::new(width, height), rows: Rows { stride: stride as usize, pixels: &bytes } }, (1, 1));
        let _ = over_content((0, 0), [i32::MIN, i32::MIN, i32::MAX, i32::MAX], PixelSize::new(width, height), None, true);
    }
}
