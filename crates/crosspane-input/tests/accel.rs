#![allow(clippy::unwrap_used)]

use crosspane_input::accel::{AccelProfile, Accelerator};
use crosspane_types::geom::{DisplayGeometry, PixelSize, PointLogical, SizeMm, VectorMm};
use crosspane_types::time::MonoTime;
use proptest::prelude::*;

fn sample(dx: f64, dy: f64, dt_ms: u64) -> VectorMm {
    let mut accel = Accelerator::new(AccelProfile::default());
    accel.unaccelerated(0.0, 0.0, MonoTime::ZERO);
    accel.unaccelerated(dx, dy, MonoTime::from_nanos(dt_ms * 1_000_000))
}

fn close(a: f64, b: f64) {
    assert!(
        (a - b).abs() <= 1e-10 * a.abs().max(b.abs()).max(1.0),
        "{a} != {b}"
    );
}

#[test]
fn zero_in_gives_zero_out() {
    let profile = AccelProfile::default();
    assert_eq!(profile.base_mm_per_unit, 0.04);
    assert_eq!(profile.threshold, 50.0);
    assert_eq!(profile.slope, 1.0);
    assert_eq!(profile.max_gain, 4.0);
    let mut accel = Accelerator::new(profile);
    assert_eq!(
        accel.unaccelerated(0.0, 0.0, MonoTime::ZERO),
        VectorMm::zero()
    );
    assert_eq!(sample(0.0, 0.0, 1), VectorMm::zero());
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 1_024,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn direction_is_preserved(dx in -10_000.0_f64..10_000.0, dy in -10_000.0_f64..10_000.0) {
        let output = sample(dx, dy, 8);
        let cross = dx * output.y - dy * output.x;
        prop_assert!(cross.abs() <= 1e-8 * dx.hypot(dy).max(1.0));
        prop_assert!(dx * output.x + dy * output.y >= 0.0);
    }

    #[test]
    fn gain_stays_in_range(
        dx in 0.001_f64..10_000.0,
        dy in -10_000.0_f64..10_000.0,
        dt_ms in 0_u64..100,
        base in 0.001_f64..0.2,
        threshold in 1.0_f64..200.0,
        slope in 0.0_f64..5.0,
        max_gain in 1.0_f64..10.0,
    ) {
        let profile = AccelProfile { base_mm_per_unit: base, threshold, slope, max_gain };
        let mut accel = Accelerator::new(profile);
        accel.unaccelerated(0.0, 0.0, MonoTime::ZERO);
        let output = accel.unaccelerated(dx, dy, MonoTime::from_nanos(dt_ms * 1_000_000));
        let gain = output.x.hypot(output.y) / (dx.hypot(dy) * base);
        prop_assert!(gain >= 1.0 - 1e-12 && gain <= max_gain + 1e-12);
    }

    #[test]
    fn magnitude_is_monotone_at_fixed_dt(
        low in 0.0_f64..10_000.0,
        increase in 0.0_f64..10_000.0,
        angle in -std::f64::consts::PI..std::f64::consts::PI,
        dt_ms in 1_u64..=50,
    ) {
        let (sin, cos) = angle.sin_cos();
        let a = sample(low * cos, low * sin, dt_ms);
        let b = sample((low + increase) * cos, (low + increase) * sin, dt_ms);
        prop_assert!(b.x.hypot(b.y) + 1e-10 >= a.x.hypot(a.y));
    }
}

#[test]
fn dt_clamping_and_first_sample() {
    let mut accel = Accelerator::new(AccelProfile::default());
    // 0.8 base mm / 8 ms = 100 mm/s, so the first sample has gain 2.
    close(accel.unaccelerated(20.0, 0.0, MonoTime::ZERO).x, 1.6);
    close(sample(2.0, 0.0, 0).x, 0.128);
    assert_eq!(sample(2.0, 0.0, 0), sample(2.0, 0.0, 1));
    assert_eq!(sample(100.0, 0.0, 50), sample(100.0, 0.0, 500));
    close(sample(100.0, 0.0, 50).x, 6.4);
    close(sample(10.0, 0.0, 8).x, 0.4); // Exactly at the threshold.
    let mut accel = Accelerator::new(AccelProfile::default());
    accel.unaccelerated(0.0, 0.0, MonoTime::from_nanos(10_000_000));
    assert_eq!(
        accel.unaccelerated(2.0, 0.0, MonoTime::ZERO),
        sample(2.0, 0.0, 1)
    );
    let profile = AccelProfile {
        slope: 0.5,
        max_gain: 3.0,
        ..AccelProfile::default()
    };
    let mut accel = Accelerator::new(profile);
    close(accel.unaccelerated(20.0, 0.0, MonoTime::ZERO).x, 1.2);
}

#[test]
fn non_finite_input_gives_zero() {
    let display = geometry();
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        for (dx, dy) in [(value, 1.0), (1.0, value), (value, value)] {
            let mut accel = Accelerator::new(AccelProfile::default());
            assert_eq!(
                accel.unaccelerated(dx, dy, MonoTime::ZERO),
                VectorMm::zero()
            );
            assert_eq!(accel.accelerated(dx, dy, &display), VectorMm::zero());
        }
    }
}

fn geometry() -> DisplayGeometry {
    DisplayGeometry {
        physical_size: SizeMm::new(300.0, 200.0),
        pixel_size: PixelSize::new(3_000, 1_000),
        scale: 2.0,
        logical_origin: PointLogical::new(-1_000.0, 500.0),
    }
}

#[test]
fn accelerated_divides_by_density_per_axis() {
    let mut accel = Accelerator::new(AccelProfile::default());
    let display = geometry();
    assert_eq!(
        accel.accelerated(100.0, -100.0, &display),
        VectorMm::new(10.0, -20.0)
    );
    assert_eq!(accel.accelerated(0.0, 0.0, &display), VectorMm::zero());
    // Accelerated samples do not replace the previous unaccelerated timestamp.
    close(accel.unaccelerated(20.0, 0.0, MonoTime::ZERO).x, 1.6);
    accel.accelerated(100.0, 100.0, &display);
    assert_eq!(
        accel.unaccelerated(2.0, 0.0, MonoTime::from_nanos(1_000_000)),
        sample(2.0, 0.0, 1),
    );
}
