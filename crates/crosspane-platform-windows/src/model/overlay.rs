//! `[P]` HUD layout and presentation generations; no native acquisition or input policy.
//! `[E]` PMv2 window positions use physical pixels. Logical conversion remains the shared
//! DisplayGeometry convention: <https://learn.microsoft.com/en-us/windows/win32/hidpi/high-dpi-desktop-application-development-on-windows>.

use std::collections::BTreeMap;

use crosspane_platform::{Overlay, OverlayAnchor, OverlayEvent, OverlayId, PlatformError, Rgb8};
use crosspane_types::geom::{DisplayGeometry, PointDevice, PointLogical};

use super::geometry::MonitorProbe;

/// `[P]` Following succeeds only on current membership or a successful owned move followed
/// by fresh membership. API failure, including unknown membership, never proves visibility.
pub fn follow_current(
    mut current: impl FnMut() -> Result<bool, PlatformError>,
    move_owned: impl FnOnce() -> Result<(), PlatformError>,
) -> Result<(), PlatformError> {
    if current()? {
        return Ok(());
    }
    move_owned()?;
    if current()? {
        Ok(())
    } else {
        Err(PlatformError::Backend(
            "owned window remains on another desktop".into(),
        ))
    }
}

/// `[P]` Bounds a label without splitting Unicode scalars; control characters become spaces.
pub fn text_line(text: &str) -> String {
    let mut chars = text.chars();
    let mut line: String = chars
        .by_ref()
        .take(128)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if chars.next().is_some() {
        line.pop();
        line.push('…');
    }
    line
}

/// `[P]` Physical top-left and extent, with the shared logical scale retained for drawing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub origin: [i32; 2],
    pub size: [i32; 2],
    pub scale: f64,
}

/// `[P]` Composes a native grayscale text mask over the parity pill and accent bar. Output is
/// premultiplied BGRA, bounded to 16 MiB; corners are transparent and text uses source-over.
pub fn compose(
    pixels: &mut [u8],
    size: [i32; 2],
    scale: f64,
    accent: Rgb8,
) -> Result<(), PlatformError> {
    let length = i64::from(size[0])
        .checked_mul(i64::from(size[1]))
        .and_then(|n| n.checked_mul(4));
    if size.iter().any(|n| *n <= 0)
        || !scale.is_finite()
        || scale <= 0.0
        || length.is_none_or(|n| n <= 0 || n > 16 * 1024 * 1024 || n as usize != pixels.len())
    {
        return Err(PlatformError::Backend(
            "invalid overlay pixel buffer".into(),
        ));
    }
    let radius = (10.0 * scale)
        .min(f64::from(size[0]) / 2.0)
        .min(f64::from(size[1]) / 2.0);
    for y in 0..size[1] {
        for x in 0..size[0] {
            let offset = ((i64::from(y) * i64::from(size[0]) + i64::from(x)) * 4) as usize;
            let coverage =
                f64::from(*pixels[offset..offset + 3].iter().max().unwrap_or(&0)) / 255.0;
            let dx = (f64::from(x) + 0.5 - f64::from(size[0]) / 2.0).abs()
                - (f64::from(size[0]) / 2.0 - radius);
            let dy = (f64::from(y) + 0.5 - f64::from(size[1]) / 2.0).abs()
                - (f64::from(size[1]) / 2.0 - radius);
            if dx.max(0.0).powi(2) + dy.max(0.0).powi(2) > radius * radius {
                pixels[offset..offset + 4].fill(0);
                continue;
            }
            let bar = (8.0..12.0).contains(&(f64::from(x) / scale))
                && (8.0..32.0).contains(&(f64::from(y) / scale));
            let (colour, alpha) = if bar {
                ([accent.b, accent.g, accent.r], 255.0)
            } else {
                ([55, 41, 31], 230.0)
            };
            for channel in 0..3 {
                pixels[offset + channel] = (255.0 * coverage
                    + f64::from(colour[channel]) * alpha * (1.0 - coverage) / 255.0)
                    .round() as u8;
            }
            pixels[offset + 3] = (255.0 * coverage + alpha * (1.0 - coverage)).round() as u8;
        }
    }
    Ok(())
}

/// `[P]` A 40-logical-pixel pill with 24-pixel margins, clipped to the selected work area.
/// `text_width` is the native font's measured width in logical pixels, never a display origin.
pub fn placement(
    overlay: &Overlay,
    monitor: &MonitorProbe,
    geometry: &DisplayGeometry,
    text_width: f64,
) -> Result<Placement, PlatformError> {
    let bad = || PlatformError::Backend("invalid overlay geometry".into());
    let [left, top, right, bottom] = monitor.rc_monitor;
    let [wl, wt, wr, wb] = monitor.rc_work;
    if !geometry.is_valid()
        || !text_width.is_finite()
        || text_width < 0.0
        || wl < left
        || wt < top
        || wr > right
        || wb > bottom
        || wr <= wl
        || wb <= wt
        || i64::from(right) - i64::from(left) != i64::from(geometry.pixel_size.width)
        || i64::from(bottom) - i64::from(top) != i64::from(geometry.pixel_size.height)
        || (geometry.scale - f64::from(monitor.dpi) / 96.0).abs() > 0.001
    {
        return Err(bad());
    }
    let origin = geometry.device_to_logical(PointDevice::new(
        f64::from(wl) - f64::from(left),
        f64::from(wt) - f64::from(top),
    ));
    let extent = geometry.device_to_logical(PointDevice::new(
        f64::from(wr) - f64::from(left),
        f64::from(wb) - f64::from(top),
    ));
    let available = [extent.x - origin.x, extent.y - origin.y];
    if available[1] < 40.0 {
        return Err(bad());
    }
    let width = (text_width + 40.0).min(available[0]);
    let mx = 24.0_f64.min((available[0] - width) / 2.0);
    let my = 24.0_f64.min((available[1] - 40.0) / 2.0);
    let x = match overlay.anchor {
        OverlayAnchor::TopCenter | OverlayAnchor::Center => origin.x + (available[0] - width) / 2.0,
        OverlayAnchor::TopRight | OverlayAnchor::BottomRight => extent.x - width - mx,
    };
    let y = match overlay.anchor {
        OverlayAnchor::TopCenter | OverlayAnchor::TopRight => origin.y + my,
        OverlayAnchor::BottomRight => extent.y - 40.0 - my,
        OverlayAnchor::Center => origin.y + (available[1] - 40.0) / 2.0,
    };
    let local = geometry.logical_to_device(PointLogical::new(x, y));
    let values = [
        local.x + f64::from(left),
        local.y + f64::from(top),
        width * geometry.scale,
        40.0 * geometry.scale,
    ];
    if values
        .iter()
        .any(|v| !v.is_finite() || *v < f64::from(i32::MIN) || *v > f64::from(i32::MAX))
        || values[2] < 1.0
        || values[3] < 1.0
    {
        return Err(bad());
    }
    Ok(Placement {
        origin: [values[0].round() as i32, values[1].round() as i32],
        size: [values[2].round() as i32, values[3].round() as i32],
        scale: geometry.scale,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Presence {
    Pending,
    Visible,
    Unavailable,
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    generation: u64,
    deadline: u64,
    presence: Presence,
}

/// `[P]` Only a current, timely, native presentation observation can emit Visible.
/// A retired generation cannot recover; the caller must explicitly show it again.
#[derive(Clone, Debug, Default)]
pub struct OverlayModel {
    next: u64,
    stopped: bool,
    entries: BTreeMap<OverlayId, Entry>,
}

impl OverlayModel {
    pub fn contains(&self, id: OverlayId) -> bool {
        self.entries.contains_key(&id)
    }

    pub fn show(&mut self, id: OverlayId, now_ms: u64) -> Result<u64, PlatformError> {
        if self.entries.len() >= 32 && !self.entries.contains_key(&id) {
            return Err(PlatformError::Backend("overlay capacity exhausted".into()));
        }
        if self.stopped {
            return Err(PlatformError::Backend("overlay host stopped".into()));
        }
        self.next = self.next.checked_add(1).ok_or(PlatformError::Timeout)?;
        self.entries.insert(
            id,
            Entry {
                generation: self.next,
                deadline: now_ms.saturating_add(500),
                presence: Presence::Pending,
            },
        );
        Ok(self.next)
    }

    pub fn hide(&mut self, id: OverlayId) -> Result<(), PlatformError> {
        if self.stopped {
            return Err(PlatformError::Backend("overlay host stopped".into()));
        }
        self.entries.remove(&id);
        Ok(())
    }

    pub fn current(&self, id: OverlayId, generation: u64) -> bool {
        !self.stopped
            && self.entries.get(&id).is_some_and(|entry| {
                entry.generation == generation && entry.presence != Presence::Unavailable
            })
    }

    pub fn observe(
        &mut self,
        id: OverlayId,
        generation: u64,
        visible: bool,
        now_ms: u64,
    ) -> Vec<OverlayEvent> {
        if !self.current(id, generation) {
            return Vec::new();
        }
        let Some(entry) = self.entries.get_mut(&id) else {
            return Vec::new();
        };
        let presence = if visible && now_ms < entry.deadline {
            Presence::Visible
        } else {
            Presence::Unavailable
        };
        let changed = presence != entry.presence;
        entry.presence = presence;
        entry.deadline = now_ms.saturating_add(500);
        if changed {
            vec![match presence {
                Presence::Visible => OverlayEvent::Visible(id),
                _ => OverlayEvent::Unavailable(id),
            }]
        } else {
            Vec::new()
        }
    }

    pub fn expire(&mut self, now_ms: u64) -> Vec<OverlayEvent> {
        self.entries
            .iter_mut()
            .filter_map(|(id, entry)| {
                if entry.presence != Presence::Unavailable && now_ms >= entry.deadline {
                    entry.presence = Presence::Unavailable;
                    Some(OverlayEvent::Unavailable(*id))
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn replay(&self) -> Vec<OverlayEvent> {
        self.entries
            .iter()
            .filter_map(|(id, entry)| match entry.presence {
                Presence::Pending => None,
                Presence::Visible => Some(OverlayEvent::Visible(*id)),
                Presence::Unavailable => Some(OverlayEvent::Unavailable(*id)),
            })
            .collect()
    }

    pub fn shutdown(&mut self) -> Vec<OverlayEvent> {
        self.stopped = true;
        let events = self
            .entries
            .iter()
            .filter(|(_, e)| e.presence != Presence::Unavailable)
            .map(|(id, _)| OverlayEvent::Unavailable(*id))
            .collect();
        self.entries.clear();
        events
    }
}
