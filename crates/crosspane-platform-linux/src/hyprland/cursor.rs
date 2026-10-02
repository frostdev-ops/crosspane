//! Where the physical pointer is, in display-local device pixels (WP-2.43 amendment A3).
//!
//! `end(Some(warp_to))` returns `Ok` even when it skipped the warp (the I/O gate was closed), so a
//! caller that must know where the pointer ended up reads it back with [`cursor_position`]. The
//! helper is a plain reader: it knows nothing about the I/O gate (the caller checks that,
//! amendment B2).
//!
//! Two short-lived IPC requests, each bounded by the [`HyprIpc`] timeout: `cursorpos`, then
//! `monitors`. Hyprland reports the cursor in layout (logical) coordinates, floored to whole
//! pixels, and each monitor's layout origin, panel size, scale and transform; the position on the
//! monitor under the cursor is `(cursor - origin) * scale`. Every monitor is considered,
//! including Crosspane's own twin outputs (`CROSSPANE-*`), which `Displays` leaves out.
//!
//! IPC also rounds scale to two decimals. `CursorProjection` keeps the geometry needed to
//! compare injections with possible readings until a settled reading provides a baseline. That
//! initial envelope is capped at eight device pixels per axis to preserve override sensitivity.

use crosspane_platform::PlatformError;
use crosspane_types::{geom::PointDevice, id::DisplayId};
use serde_json::Value;

use super::ipc::HyprIpc;

/// A cursor this far (layout pixels) outside every monitor still counts as on the nearest one:
/// the cursor position is rounded, and a cursor at a monitor's far edge rounds onto the next
/// pixel.
const EDGE_SLACK: f64 = 1.0;

/// The physical pointer's display (Hyprland's monitor id, the same as `DisplayId` everywhere) and
/// its position in that display's device pixels, post-transform, from the display's top-left.
///
/// `NotFound`: the pointer is on no monitor. `Timeout` or `Backend`: the IPC failed or answered
/// something unusable.
pub fn cursor_position(ipc: &HyprIpc) -> Result<(DisplayId, PointDevice), PlatformError> {
    cursor_sample(ipc).map(|sample| (sample.display, sample.position))
}

/// The public reading plus the unreconstructed layout position and all monitor projections.
/// Monitoring needs the injected display's geometry even if rounded IPC geometry attributes a
/// cursor near an edge to its neighbour.
pub(super) struct CursorSample {
    pub(super) display: DisplayId,
    pub(super) position: PointDevice,
    pub(super) layout: (f64, f64),
    pub(super) projections: Vec<CursorProjection>,
}

pub(super) fn cursor_sample(ipc: &HyprIpc) -> Result<CursorSample, PlatformError> {
    // The cursor first: the layout can only be as new as, or newer than, the position.
    let cursor = ipc.json("cursorpos")?;
    let monitors = ipc.json("monitors")?;
    locate_sample(&cursor, &monitors)
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Monitor {
    id: u32,
    x: f64,
    y: f64,
    /// Post-transform size in device pixels.
    width: f64,
    height: f64,
    scale: f64,
}

/// Hyprland 0.56: absolute motion maps onto `round(panel_size / true_scale)` layout pixels;
/// cursorpos floors the resulting global point, and monitor IPC serializes scale to two
/// decimals and origin as integers. This projection accounts for those reductions without
/// guessing precise scale, with a capped initial envelope until a settled baseline is available.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct CursorProjection(Monitor);

impl CursorProjection {
    const CAP: f64 = 8.0;
    pub(super) fn display(&self) -> DisplayId {
        DisplayId(self.0.id)
    }

    /// Distance outside the injection's possible reported positions, capped to eight device
    /// pixels per axis about its nominal floored projection. Extreme scale-rounding errors can
    /// exceed that cap; rounded IPC cannot guarantee both arbitrary-scale injection suppression
    /// and a bounded override threshold before a baseline exists.
    pub(super) fn injection_distance(&self, injected: PointDevice, layout: (f64, f64)) -> f64 {
        let m = self.0;
        let axis = |point: f64, observed: f64, origin: f64, extent: f64| {
            // `{:.2f}` loses at most half of 0.01. next_up/down include floating-point
            // representation error at the interval endpoints.
            let scale_low = (m.scale - 0.005).next_down().max(f64::MIN_POSITIVE);
            let scale_high = (m.scale + 0.005).next_up();
            let fraction = point / extent;
            let nominal = (origin + (extent / m.scale).round() * fraction).floor();
            // Monitor origins are cast to int in IPC. Their true values differ by less than
            // one layout pixel; include both signs (also valid for negative origins).
            let low = (origin - 1.0 + (extent / scale_high).round() * fraction)
                .next_down()
                .floor()
                .max(nominal - Self::CAP / m.scale);
            let high = (origin + 1.0 + (extent / scale_low).round() * fraction)
                .next_up()
                .floor()
                .min(nominal + Self::CAP / m.scale);
            // Compare the retained IPC layout point before the public device reconstruction's
            // edge clamp. Thus that clamp cannot hide or introduce a difference at either edge.
            (low - observed).max(observed - high).max(0.0) * m.scale
        };
        axis(injected.x, layout.0, m.x, m.width).hypot(axis(injected.y, layout.1, m.y, m.height))
    }

    /// The complete pre-baseline tolerance, including the normal divergence threshold, cannot
    /// conceal a movement beyond the cap along either axis.
    pub(super) fn outside_injection_cap(&self, injected: PointDevice, layout: (f64, f64)) -> bool {
        let m = self.0;
        let outside = |point: f64, observed: f64, origin: f64, extent: f64| {
            let fraction = point / extent;
            let nominal = (origin + (extent / m.scale).round() * fraction).floor();
            (observed - nominal).abs() * m.scale > Self::CAP
        };
        outside(injected.x, layout.0, m.x, m.width) || outside(injected.y, layout.1, m.y, m.height)
    }

    pub(super) fn observation_distance(&self, before: (f64, f64), now: (f64, f64)) -> f64 {
        (before.0 - now.0).hypot(before.1 - now.1) * self.0.scale
    }
}

impl Monitor {
    fn parse(m: &Value) -> Result<Monitor, PlatformError> {
        let bad = |key: &str| PlatformError::Backend(format!("hyprland monitor: bad `{key}`"));
        let uint = |key: &str| m.get(key).and_then(Value::as_u64).ok_or_else(|| bad(key));
        let float = |key: &str| {
            m.get(key)
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite())
                .ok_or_else(|| bad(key))
        };
        let (mut width, mut height) = (uint("width")?, uint("height")?);
        if width == 0 || height == 0 {
            return Err(bad("width"));
        }
        // `width`/`height` are the panel's, before the output transform; odd transforms rotate by
        // 90 or 270 degrees.
        if m.get("transform").and_then(Value::as_u64).unwrap_or(0) % 2 == 1 {
            std::mem::swap(&mut width, &mut height);
        }
        let scale = float("scale")?;
        if scale <= 0.0 {
            return Err(bad("scale"));
        }
        Ok(Monitor {
            id: u32::try_from(uint("id")?).map_err(|_| bad("id"))?,
            x: float("x")?,
            y: float("y")?,
            width: width as f64,
            height: height as f64,
            scale,
        })
    }

    /// How far a layout point is outside this monitor's half-open layout rectangle: zero inside.
    /// A point on the far edge (where a rounded cursor can land) counts as half a pixel away, so a
    /// neighbour that contains it wins.
    fn distance(&self, x: f64, y: f64) -> f64 {
        let (w, h) = (self.width / self.scale, self.height / self.scale);
        if x >= self.x && x < self.x + w && y >= self.y && y < self.y + h {
            return 0.0;
        }
        let dx = (self.x - x).max(x - (self.x + w)).max(0.0);
        let dy = (self.y - y).max(y - (self.y + h)).max(0.0);
        dx.hypot(dy).max(0.5)
    }
}

/// The pure part of [`cursor_position`], for the unit tests.
#[cfg(test)]
fn locate(cursor: &Value, monitors: &Value) -> Result<(DisplayId, PointDevice), PlatformError> {
    locate_sample(cursor, monitors).map(|sample| (sample.display, sample.position))
}

pub(super) fn locate_sample(
    cursor: &Value,
    monitors: &Value,
) -> Result<CursorSample, PlatformError> {
    let coordinate = |key: &str| {
        cursor
            .get(key)
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite())
            .ok_or_else(|| PlatformError::Backend(format!("hyprland cursorpos: bad `{key}`")))
    };
    let (x, y) = (coordinate("x")?, coordinate("y")?);
    let list = monitors
        .as_array()
        .ok_or_else(|| PlatformError::Backend("hyprland monitors: not a list".into()))?;
    let mut best: Option<(f64, Monitor)> = None;
    let mut projections = Vec::with_capacity(list.len());
    for m in list {
        let m = Monitor::parse(m)?;
        projections.push(CursorProjection(m));
        let distance = m.distance(x, y);
        // The nearest monitor wins; a point inside one (half-open) beats one that merely touches it.
        if best.as_ref().is_none_or(|(d, _)| distance < *d) {
            best = Some((distance, m));
        }
    }
    let (distance, m) = best.ok_or(PlatformError::NotFound)?;
    if distance > EDGE_SLACK {
        return Err(PlatformError::NotFound);
    }
    let device = |layout: f64, origin: f64, extent: f64| {
        ((layout - origin) * m.scale).clamp(0.0, extent - 1.0)
    };
    Ok(CursorSample {
        display: DisplayId(m.id),
        position: PointDevice::new(device(x, m.x, m.width), device(y, m.y, m.height)),
        layout: (x, y),
        projections,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn monitors() -> Value {
        json!([
            {"id":0,"name":"HDMI-A-1","width":1920,"height":1080,"x":0,"y":600,"scale":1,"transform":3},
            {"id":2,"name":"DP-3","width":3440,"height":1440,"x":1080,"y":1080,"scale":1.25,"transform":0},
            {"id":5,"name":"WAYLAND-1","width":950,"height":1046,"x":-950,"y":0,"scale":1,"transform":0},
            {"id":7,"name":"CROSSPANE-180027f1","width":1072,"height":938,"x":1048576,"y":0,
             "scale":2,"transform":0}
        ])
    }

    fn at(x: i64, y: i64) -> Result<(DisplayId, PointDevice), PlatformError> {
        locate(&json!({"x": x, "y": y}), &monitors())
    }

    #[test]
    fn injection_projection_bounds_fractional_scale_and_edge_readings() {
        for scale in [4.0 / 3.0, 1.5, 1.25, 0.5, 3.0, 4.25, 16.0] {
            let reported: f64 = format!("{scale:.2}").parse().unwrap();
            for (width, height) in [(1920.0, 1080.0), (1080.0, 1920.0), (511.0, 333.0)] {
                let projection = CursorProjection(Monitor {
                    id: 0,
                    x: -950.0,
                    y: 600.0,
                    width,
                    height,
                    scale: reported,
                });
                for point in [
                    PointDevice::new(0.0, 0.0),
                    PointDevice::new(width - 1.0 / 256.0, height - 1.0 / 256.0),
                    PointDevice::new(width * 0.78125, height * 0.46296296296),
                ] {
                    let layout = (
                        (-950.0 + (width / scale).round() * point.x / width).floor(),
                        (600.0 + (height / scale).round() * point.y / height).floor(),
                    );
                    assert_eq!(
                        projection.injection_distance(point, layout),
                        0.0,
                        "scale {scale}, extent {width}x{height}, point {point:?}, reading {layout:?}"
                    );
                    assert!(
                        projection.injection_distance(point, (layout.0 + 100.0, layout.1 + 100.0))
                            > 3.0,
                        "real divergence disappeared at scale {scale}"
                    );
                }
            }
        }
    }

    #[test]
    fn injection_projection_cap_applies_throughout_two_decimal_scale_buckets() {
        // The implementation derives its envelope from a positive scale interval, rather than
        // recognizing particular scales. Probe rounding endpoints throughout and beyond the
        // usual scale range, negative/fractional origins and fixed-point edges.
        for cents in (1..=6400).step_by(7) {
            for offset in [-0.004999999, 0.0, 0.004999999] {
                let scale = f64::from(cents) / 100.0 + offset;
                let reported: f64 = format!("{scale:.2}").parse().unwrap();
                for origin in [-950.9_f64, -0.9, 0.9, 1048576.9] {
                    let projection = CursorProjection(Monitor {
                        id: 0,
                        x: origin.trunc(),
                        y: origin.trunc(),
                        width: 3840.0,
                        height: 2160.0,
                        scale: reported,
                    });
                    for point in [
                        PointDevice::new(0.0, 0.0),
                        PointDevice::new(1500.0, 500.0),
                        PointDevice::new(3839.99609375, 2159.99609375),
                    ] {
                        let layout = (
                            (origin + (3840.0 / scale).round() * point.x / 3840.0).floor(),
                            (origin + (2160.0 / scale).round() * point.y / 2160.0).floor(),
                        );
                        let distance = projection.injection_distance(point, layout);
                        if projection.outside_injection_cap(point, layout) {
                            assert!(distance > 0.0, "uncertainty exceeded cap at scale {scale}");
                        } else {
                            assert_eq!(
                                distance, 0.0,
                                "scale {scale}, origin {origin}, point {point:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn injection_projection_caps_each_axis_at_eight_device_pixels() {
        let projection = CursorProjection(Monitor {
            id: 0,
            x: 0.0,
            y: 0.0,
            width: 7680.0,
            height: 4320.0,
            scale: 0.25,
        });
        let point = PointDevice::new(7000.0, 2000.0);
        for layout in [
            (28032.0, 8000.0),
            (27968.0, 8000.0),
            (28000.0, 8032.0),
            (28000.0, 7968.0),
        ] {
            assert_eq!(projection.injection_distance(point, layout), 0.0);
            assert!(!projection.outside_injection_cap(point, layout));
        }
        for layout in [
            (28033.0, 8000.0),
            (27967.0, 8000.0),
            (28000.0, 8033.0),
            (28000.0, 7967.0),
            (28400.0, 8000.0),
        ] {
            assert!(projection.injection_distance(point, layout) > 0.0);
            assert!(projection.outside_injection_cap(point, layout));
        }
    }

    #[test]
    fn scale_one_and_rotated() {
        // HDMI-A-1 is rotated: its layout rectangle is 1080 wide and 1920 high.
        assert_eq!(
            at(10, 700).unwrap(),
            (DisplayId(0), PointDevice::new(10.0, 100.0))
        );
        assert_eq!(
            at(1079, 2519).unwrap(),
            (DisplayId(0), PointDevice::new(1079.0, 1919.0))
        );
    }

    #[test]
    fn fractional_scale_maps_logical_to_device() {
        // DP-3 at scale 1.25: layout (1180, 1180) is 100 layout pixels in, 125 device pixels.
        assert_eq!(
            at(1180, 1180).unwrap(),
            (DisplayId(2), PointDevice::new(125.0, 125.0))
        );
    }

    #[test]
    fn negative_origin() {
        assert_eq!(
            at(-950, 0).unwrap(),
            (DisplayId(5), PointDevice::new(0.0, 0.0))
        );
        assert_eq!(
            at(-1, 1045).unwrap(),
            (DisplayId(5), PointDevice::new(949.0, 1045.0))
        );
    }

    #[test]
    fn twin_output_far_right_at_scale_two() {
        // The twin sits at x = 2^20 with a scale of two: 536 x 469 layout pixels.
        assert_eq!(
            at((1 << 20) + 300, 200).unwrap(),
            (DisplayId(7), PointDevice::new(600.0, 400.0))
        );
    }

    #[test]
    fn rounding_at_the_far_edge_clamps_onto_the_last_pixel() {
        // Layout x = 950 - 950 = 0 is the first column of the next monitor; a cursor rounded one
        // layout pixel past a monitor's far edge still reads as that monitor's last pixel.
        assert_eq!(
            at(1080, 2519).unwrap().0,
            DisplayId(0),
            "one pixel past the rotated monitor's right edge, nearest is itself"
        );
        let (_, p) = at(1080, 2519).unwrap();
        assert_eq!(p, PointDevice::new(1079.0, 1919.0));
        // Where two monitors meet, the one that contains the point (half-open) wins, whichever
        // comes first in the list.
        assert_eq!(
            at(1080, 1500).unwrap(),
            (DisplayId(2), PointDevice::new(0.0, 525.0))
        );
    }

    #[test]
    fn a_cursor_on_no_monitor_is_not_found() {
        assert!(matches!(at(-5000, -5000), Err(PlatformError::NotFound)));
        assert!(matches!(
            locate(&json!({"x": 0, "y": 0}), &json!([])),
            Err(PlatformError::NotFound)
        ));
    }

    #[test]
    fn garbage_is_a_backend_error() {
        for (cursor, monitors) in [
            (json!({"x": "a", "y": 1}), monitors()),
            (json!({"y": 1}), monitors()),
            (json!({"x": 1, "y": 1}), json!({})),
            (json!({"x": 1, "y": 1}), json!([{"id": 0}])),
            (
                json!({"x": 1, "y": 1}),
                json!([{"id":0,"width":10,"height":10,"x":0,"y":0,"scale":0}]),
            ),
        ] {
            assert!(
                matches!(locate(&cursor, &monitors), Err(PlatformError::Backend(_))),
                "{cursor} {monitors}"
            );
        }
    }
}
