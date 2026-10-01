//! The shared, timestamp-driven pointer acceleration curve.

use crosspane_types::geom::{DisplayGeometry, VectorMm};
use crosspane_types::time::MonoTime;

/// One acceleration curve for all controllers (03 §3 "Pointer feel").
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AccelProfile {
    /// Canvas mm per unaccelerated device unit at low speed. Default 0.04.
    pub base_mm_per_unit: f64,
    /// Hand speed (in base mm per second) above which gain rises. Default 50.0.
    pub threshold: f64,
    /// Slope of the gain above the threshold. Default 1.0.
    pub slope: f64,
    /// Maximum gain. Default 4.0.
    pub max_gain: f64,
}

impl Default for AccelProfile {
    fn default() -> Self {
        Self {
            base_mm_per_unit: 0.04,
            threshold: 50.0,
            slope: 1.0,
            max_gain: 4.0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Accelerator {
    profile: AccelProfile,
    previous: Option<MonoTime>,
}

impl Accelerator {
    pub fn new(profile: AccelProfile) -> Self {
        Self {
            profile,
            previous: None,
        }
    }

    /// Unaccelerated motion `(dx, dy)` sampled at `at` → canvas millimetres.
    pub fn unaccelerated(&mut self, dx: f64, dy: f64, at: MonoTime) -> VectorMm {
        let dt = self.previous.map_or(0.008, |previous| {
            at.saturating_duration_since(previous)
                .as_secs_f64()
                .clamp(0.001, 0.050)
        });
        self.previous = Some(at);
        if !dx.is_finite() || !dy.is_finite() {
            return VectorMm::zero();
        }

        let profile = self.profile;
        let speed = dx.hypot(dy) * profile.base_mm_per_unit / dt;
        let gain = if speed <= profile.threshold {
            1.0
        } else {
            (1.0 + profile.slope * (speed - profile.threshold) / profile.threshold)
                .min(profile.max_gain)
        };
        VectorMm::new(
            dx * profile.base_mm_per_unit * gain,
            dy * profile.base_mm_per_unit * gain,
        )
    }

    /// Already-accelerated device pixels on a display → canvas millimetres through that display's
    /// pixel density, with no further acceleration.
    pub fn accelerated(&mut self, dx: f64, dy: f64, display: &DisplayGeometry) -> VectorMm {
        if !dx.is_finite() || !dy.is_finite() {
            return VectorMm::zero();
        }
        let (x, y) = display.pixels_per_mm();
        VectorMm::new(dx / x, dy / y)
    }
}
