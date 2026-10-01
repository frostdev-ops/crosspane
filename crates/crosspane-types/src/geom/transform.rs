//! Transforms between the coordinate spaces of one display.

use super::{DisplayGeometry, PointDevice, PointLogical, PointMm, RectLogical, SizeLogical};

impl DisplayGeometry {
    /// True iff physical dimensions and scale are finite and positive, pixel dimensions are
    /// nonzero, and the logical origin is finite.
    pub fn is_valid(&self) -> bool {
        self.physical_size.width.is_finite()
            && self.physical_size.width > 0.0
            && self.physical_size.height.is_finite()
            && self.physical_size.height > 0.0
            && self.pixel_size.width >= 1
            && self.pixel_size.height >= 1
            && self.scale.is_finite()
            && self.scale > 0.0
            && self.logical_origin.x.is_finite()
            && self.logical_origin.y.is_finite()
    }

    /// Resolution in device pixels divided by the scale.
    pub fn logical_size(&self) -> SizeLogical {
        SizeLogical::new(
            f64::from(self.pixel_size.width) / self.scale,
            f64::from(self.pixel_size.height) / self.scale,
        )
    }

    /// Desktop logical bounds with this display's origin and logical size.
    pub fn logical_bounds(&self) -> RectLogical {
        RectLogical::new(self.logical_origin, self.logical_size())
    }

    /// Display-local device pixels to desktop logical coordinates: origin + p / scale.
    pub fn device_to_logical(&self, p: PointDevice) -> PointLogical {
        PointLogical::new(
            self.logical_origin.x + p.x / self.scale,
            self.logical_origin.y + p.y / self.scale,
        )
    }

    /// Desktop logical coordinates to display-local device pixels: (p - origin) * scale.
    pub fn logical_to_device(&self, p: PointLogical) -> PointDevice {
        PointDevice::new(
            (p.x - self.logical_origin.x) * self.scale,
            (p.y - self.logical_origin.y) * self.scale,
        )
    }

    /// Device pixels per millimetre along each axis.
    pub fn pixels_per_mm(&self) -> (f64, f64) {
        (
            f64::from(self.pixel_size.width) / self.physical_size.width,
            f64::from(self.pixel_size.height) / self.physical_size.height,
        )
    }

    /// Display-local millimetres, relative to the panel's top-left, to device pixels.
    pub fn mm_to_device(&self, p: PointMm) -> PointDevice {
        let (x, y) = self.pixels_per_mm();
        PointDevice::new(p.x * x, p.y * y)
    }

    /// Device pixels to display-local millimetres.
    pub fn device_to_mm(&self, p: PointDevice) -> PointMm {
        let (x, y) = self.pixels_per_mm();
        PointMm::new(p.x / x, p.y / y)
    }

    /// Clamp to [0, width - 1] × [0, height - 1]; a NaN coordinate becomes zero.
    pub fn clamp_device(&self, p: PointDevice) -> PointDevice {
        PointDevice::new(
            p.x.max(0.0)
                .min(f64::from(self.pixel_size.width.saturating_sub(1))),
            p.y.max(0.0)
                .min(f64::from(self.pixel_size.height.saturating_sub(1))),
        )
    }
}
