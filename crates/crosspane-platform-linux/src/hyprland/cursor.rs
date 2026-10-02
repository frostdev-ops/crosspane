//! Where the physical pointer is, in display-local device pixels (WP-2.43 amendment A3).
//!
//! `end(Some(warp_to))` returns `Ok` even when it skipped the warp (the I/O gate was closed), so a
//! caller that must know where the pointer ended up reads it back with [`cursor_position`]. The
//! helper is a plain reader: it knows nothing about the I/O gate (the caller checks that,
//! amendment B2).
//!
//! Two short-lived IPC requests, each bounded by the [`HyprIpc`] timeout: `cursorpos`, then
//! `monitors`. Hyprland reports the cursor in layout (logical) coordinates, rounded to whole
//! pixels, and each monitor's layout origin, panel size, scale and transform; the position on the
//! monitor under the cursor is `(cursor - origin) * scale`. Every monitor is considered,
//! including Crosspane's own twin outputs (`CROSSPANE-*`), which `Displays` leaves out.
//!
//! The reading is only as exact as Hyprland's rounding: up to half a layout pixel, which is half
//! a scale factor in device pixels (one device pixel at scale 2).

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
    // The cursor first: the layout can only be as new as, or newer than, the position.
    let cursor = ipc.json("cursorpos")?;
    let monitors = ipc.json("monitors")?;
    locate(&cursor, &monitors)
}

struct Monitor {
    id: u32,
    x: f64,
    y: f64,
    /// Post-transform size in device pixels.
    width: f64,
    height: f64,
    scale: f64,
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
fn locate(cursor: &Value, monitors: &Value) -> Result<(DisplayId, PointDevice), PlatformError> {
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
    for m in list {
        let m = Monitor::parse(m)?;
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
    Ok((
        DisplayId(m.id),
        PointDevice::new(device(x, m.x, m.width), device(y, m.y, m.height)),
    ))
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
