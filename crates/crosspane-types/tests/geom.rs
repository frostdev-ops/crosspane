use crosspane_types::geom::{
    DisplayGeometry, PixelSize, PointDevice, PointLogical, PointMm, SizeLogical, SizeMm,
};
use proptest::prelude::*;

const TOLERANCE: f64 = 1e-9;
const CASES: u32 = 1_024;

fn geometry() -> impl Strategy<Value = DisplayGeometry> {
    (
        50.0..=2_000.0,
        50.0..=2_000.0,
        320_u32..=8_192,
        320_u32..=8_192,
        prop_oneof![
            proptest::sample::select(vec![1.0, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0]),
            0.5..=4.0,
        ],
        -20_000.0..=20_000.0,
        -20_000.0..=20_000.0,
    )
        .prop_map(
            |(mm_width, mm_height, width, height, scale, x, y)| DisplayGeometry {
                physical_size: SizeMm::new(mm_width, mm_height),
                pixel_size: PixelSize::new(width, height),
                scale,
                logical_origin: PointLogical::new(x, y),
            },
        )
}

fn relative_error_limit(coordinate: f64) -> f64 {
    TOLERANCE * coordinate.abs().max(1.0)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: CASES,
        // Keep failure artifacts out of the work package's read-only files.
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn device_logical_round_trip(g in geometry(), x in -2.0_f64..=3.0, y in -2.0_f64..=3.0) {
        let p = PointDevice::new(
            x * f64::from(g.pixel_size.width),
            y * f64::from(g.pixel_size.height),
        );
        let result = g.logical_to_device(g.device_to_logical(p));
        prop_assert!((result.x - p.x).abs() <= relative_error_limit(p.x));
        prop_assert!((result.y - p.y).abs() <= relative_error_limit(p.y));
    }

    #[test]
    fn mm_device_round_trip(g in geometry(), x in -2.0_f64..=3.0, y in -2.0_f64..=3.0) {
        let p = PointMm::new(x * g.physical_size.width, y * g.physical_size.height);
        let result = g.device_to_mm(g.mm_to_device(p));
        prop_assert!((result.x - p.x).abs() <= relative_error_limit(p.x));
        prop_assert!((result.y - p.y).abs() <= relative_error_limit(p.y));
    }

    #[test]
    fn corners(g in geometry()) {
        let size = SizeLogical::new(
            f64::from(g.pixel_size.width) / g.scale,
            f64::from(g.pixel_size.height) / g.scale,
        );
        let (density_x, density_y) = g.pixels_per_mm();
        prop_assert!(g.is_valid());
        prop_assert_eq!(g.logical_size(), size);
        prop_assert_eq!(g.logical_bounds().origin, g.logical_origin);
        prop_assert_eq!(g.logical_bounds().size, size);
        prop_assert_eq!(density_x, f64::from(g.pixel_size.width) / g.physical_size.width);
        prop_assert_eq!(density_y, f64::from(g.pixel_size.height) / g.physical_size.height);
        prop_assert_eq!(g.device_to_logical(PointDevice::zero()), g.logical_origin);
        prop_assert_eq!(
            g.device_to_logical(PointDevice::new(
                f64::from(g.pixel_size.width),
                f64::from(g.pixel_size.height),
            )),
            g.logical_bounds().max(),
        );
    }

    #[test]
    fn strict_monotonicity(g in geometry(), low in -2.0_f64..=2.5, gap in 0.0001_f64..=0.5) {
        // Separate the inputs enough to remain distinct after floating-point translation.
        let high = low + gap;
        let device_low = PointDevice::new(
            low * f64::from(g.pixel_size.width),
            low * f64::from(g.pixel_size.height),
        );
        let device_x = PointDevice::new(high * f64::from(g.pixel_size.width), device_low.y);
        let device_y = PointDevice::new(device_low.x, high * f64::from(g.pixel_size.height));

        let logical_low = g.device_to_logical(device_low);
        let logical_x = g.device_to_logical(device_x);
        let logical_y = g.device_to_logical(device_y);
        prop_assert!(logical_low.x < logical_x.x);
        prop_assert_eq!(logical_low.y, logical_x.y);
        prop_assert!(logical_low.y < logical_y.y);
        prop_assert_eq!(logical_low.x, logical_y.x);

        let device_from_logical = g.logical_to_device(logical_low);
        let device_from_logical_x = g.logical_to_device(logical_x);
        let device_from_logical_y = g.logical_to_device(logical_y);
        prop_assert!(device_from_logical.x < device_from_logical_x.x);
        prop_assert_eq!(device_from_logical.y, device_from_logical_x.y);
        prop_assert!(device_from_logical.y < device_from_logical_y.y);
        prop_assert_eq!(device_from_logical.x, device_from_logical_y.x);

        let mm_low = PointMm::new(low * g.physical_size.width, low * g.physical_size.height);
        let mm_x = PointMm::new(high * g.physical_size.width, mm_low.y);
        let mm_y = PointMm::new(mm_low.x, high * g.physical_size.height);
        let device_from_mm = g.mm_to_device(mm_low);
        let device_from_mm_x = g.mm_to_device(mm_x);
        let device_from_mm_y = g.mm_to_device(mm_y);
        prop_assert!(device_from_mm.x < device_from_mm_x.x);
        prop_assert_eq!(device_from_mm.y, device_from_mm_x.y);
        prop_assert!(device_from_mm.y < device_from_mm_y.y);
        prop_assert_eq!(device_from_mm.x, device_from_mm_y.x);

        let mm_from_device = g.device_to_mm(device_low);
        let mm_from_device_x = g.device_to_mm(device_x);
        let mm_from_device_y = g.device_to_mm(device_y);
        prop_assert!(mm_from_device.x < mm_from_device_x.x);
        prop_assert_eq!(mm_from_device.y, mm_from_device_x.y);
        prop_assert!(mm_from_device.y < mm_from_device_y.y);
        prop_assert_eq!(mm_from_device.x, mm_from_device_y.x);
    }

    #[test]
    fn inside_device_maps_inside_logical(g in geometry(), x in 0.0_f64..1.0, y in 0.0_f64..1.0) {
        let width = f64::from(g.pixel_size.width);
        let height = f64::from(g.pixel_size.height);
        let bounds = g.logical_bounds();
        let min = bounds.min();
        let max = bounds.max();
        for p in [
            PointDevice::zero(),
            PointDevice::new(x * width, y * height),
            PointDevice::new(width * (1.0 - f64::EPSILON), height * (1.0 - f64::EPSILON)),
        ] {
            prop_assert!(p.x >= 0.0 && p.x < width);
            prop_assert!(p.y >= 0.0 && p.y < height);
            let logical = g.device_to_logical(p);
            prop_assert!(logical.x >= min.x - TOLERANCE && logical.x < max.x + TOLERANCE);
            prop_assert!(logical.y >= min.y - TOLERANCE && logical.y < max.y + TOLERANCE);
        }
    }

    #[test]
    fn clamp_device(g in geometry(), x in -2.0_f64..=3.0, y in -2.0_f64..=3.0) {
        let max_x = f64::from(g.pixel_size.width) - 1.0;
        let max_y = f64::from(g.pixel_size.height) - 1.0;
        let p = PointDevice::new(
            x * f64::from(g.pixel_size.width),
            y * f64::from(g.pixel_size.height),
        );
        for point in [
            p,
            PointDevice::zero(),
            PointDevice::new(max_x, max_y),
            PointDevice::new(f64::NAN, p.y),
            PointDevice::new(p.x, f64::NAN),
            PointDevice::new(f64::NAN, f64::NAN),
            PointDevice::new(f64::NEG_INFINITY, f64::INFINITY),
            PointDevice::new(f64::INFINITY, f64::NEG_INFINITY),
        ] {
            let clamped = g.clamp_device(point);
            prop_assert!(clamped.x >= 0.0 && clamped.x <= max_x);
            prop_assert!(clamped.y >= 0.0 && clamped.y <= max_y);
            prop_assert_eq!(g.clamp_device(clamped), clamped);
            if point.x.is_nan() {
                prop_assert_eq!(clamped.x, 0.0);
            }
            if point.y.is_nan() {
                prop_assert_eq!(clamped.y, 0.0);
            }
            if point.x >= 0.0 && point.x <= max_x && point.y >= 0.0 && point.y <= max_y {
                prop_assert_eq!(clamped, point);
            }
        }
        // Check preservation for an interior point on every generated display.
        let inside = PointDevice::new(x.abs() / 3.0 * max_x, y.abs() / 3.0 * max_y);
        prop_assert_eq!(g.clamp_device(inside), inside);
    }
}

fn retina() -> DisplayGeometry {
    DisplayGeometry {
        physical_size: SizeMm::new(302.0, 196.0),
        pixel_size: PixelSize::new(3_024, 1_964),
        scale: 2.0,
        logical_origin: PointLogical::zero(),
    }
}

#[test]
fn is_valid_rejects_invalid_fields() {
    let valid = retina();
    for value in [0.0, -0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        for invalid in [
            DisplayGeometry {
                physical_size: SizeMm::new(value, valid.physical_size.height),
                ..valid
            },
            DisplayGeometry {
                physical_size: SizeMm::new(valid.physical_size.width, value),
                ..valid
            },
            DisplayGeometry {
                scale: value,
                ..valid
            },
        ] {
            assert!(
                !invalid.is_valid(),
                "accepted invalid geometry: {invalid:?}"
            );
            // Invalid geometry has unspecified results, but all methods must remain panic-free.
            let _ = invalid.logical_size();
            let _ = invalid.logical_bounds();
            let _ = invalid.device_to_logical(PointDevice::zero());
            let _ = invalid.logical_to_device(PointLogical::zero());
            let _ = invalid.pixels_per_mm();
            let _ = invalid.mm_to_device(PointMm::zero());
            let _ = invalid.device_to_mm(PointDevice::zero());
            let _ = invalid.clamp_device(PointDevice::new(f64::NAN, f64::INFINITY));
        }
    }
    // Pixel dimensions are unsigned; zero is their only invalid representable value.
    for size in [
        PixelSize::new(0, 1_964),
        PixelSize::new(3_024, 0),
        PixelSize::zero(),
    ] {
        let invalid = DisplayGeometry {
            pixel_size: size,
            ..valid
        };
        assert!(!invalid.is_valid());
        let _ = invalid.clamp_device(PointDevice::new(f64::NAN, f64::INFINITY));
    }
    // Origins may be zero or negative, but neither axis may be non-finite.
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        for origin in [PointLogical::new(value, 0.0), PointLogical::new(0.0, value)] {
            assert!(
                !DisplayGeometry {
                    logical_origin: origin,
                    ..valid
                }
                .is_valid()
            );
        }
    }
}

#[test]
fn is_valid_retina_example() {
    let g = retina();
    assert!(g.is_valid());
    assert_eq!(g.logical_size(), SizeLogical::new(1_512.0, 982.0));
    assert!(
        DisplayGeometry {
            logical_origin: PointLogical::new(-20_000.0, -20_000.0),
            ..g
        }
        .is_valid()
    );
}
