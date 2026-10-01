//! `Displays` from Hyprland IPC: `monitors` for the snapshot, the event socket for changes.

use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use crosspane_platform::{Displays, EventSink, PlatformError};
use crosspane_types::color::ColorSpace;
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{DisplayGeometry, PixelSize, PointLogical, SizeMm};
use crosspane_types::id::DisplayId;

use super::ipc::{EventStream, HyprIpc, IpcEvent};

/// Changes that arrive within this window produce one snapshot.
const DEBOUNCE: Duration = Duration::from_millis(100);
/// After losing the event socket for this long, deliver an empty snapshot (stale data is worse).
const LOST_AFTER: Duration = Duration::from_secs(2);
/// Millimetres per pixel at 96 DPI, used when a monitor reports no physical size (nested, virtual).
const MM_PER_PX_96DPI: f64 = 25.4 / 96.0;

#[derive(Debug)]
pub struct HyprlandDisplays {
    ipc: HyprIpc,
    events: Option<EventStream>,
}

impl HyprlandDisplays {
    pub fn new(ipc: HyprIpc) -> Result<HyprlandDisplays, PlatformError> {
        ipc.require_supported()?;
        Ok(HyprlandDisplays { ipc, events: None })
    }
}

impl Displays for HyprlandDisplays {
    fn displays(&self) -> Result<Vec<DisplayInfo>, PlatformError> {
        snapshot(&self.ipc)
    }

    fn subscribe(
        &mut self,
        sink: Arc<dyn EventSink<Vec<DisplayInfo>>>,
    ) -> Result<(), PlatformError> {
        if self.events.is_some() {
            return Err(PlatformError::Backend(
                "Displays::subscribe called twice".into(),
            ));
        }
        sink.send(snapshot(&self.ipc)?);
        let (tx, rx) = mpsc::channel::<Change>();
        let events = self.ipc.events(Box::new(move |event| {
            let change = match event {
                IpcEvent::Connected => Change::Refresh,
                IpcEvent::Disconnected => Change::Lost,
                IpcEvent::Event { name, .. } if is_display_event(name) => Change::Refresh,
                IpcEvent::Event { .. } => return,
            };
            // The worker is gone only while we are being dropped.
            let _ = tx.send(change);
        }))?;
        let ipc = self.ipc.clone();
        std::thread::Builder::new()
            .name("hypr-displays".into())
            .spawn(move || worker(&ipc, &rx, &*sink))
            .map_err(|e| PlatformError::Backend(format!("spawn displays thread: {e}")))?;
        self.events = Some(events);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    Refresh,
    Lost,
}

fn is_display_event(name: &str) -> bool {
    matches!(
        name,
        "monitoradded"
            | "monitoraddedv2"
            | "monitorremoved"
            | "monitorremovedv2"
            | "configreloaded"
    )
}

/// Runs until the event stream (and with it the sender) is dropped.
fn worker(ipc: &HyprIpc, rx: &mpsc::Receiver<Change>, sink: &dyn EventSink<Vec<DisplayInfo>>) {
    // The first `Connected` repeats the snapshot `subscribe` already sent; harmless.
    while let Ok(change) = rx.recv() {
        match change {
            Change::Refresh => {
                if !drain_for(rx, DEBOUNCE) {
                    return;
                }
                deliver(ipc, sink);
            }
            Change::Lost => match wait_reconnect(rx) {
                Some(true) => deliver(ipc, sink),
                Some(false) => {
                    tracing::warn!("hyprland event socket lost; reporting no displays");
                    sink.send(Vec::new());
                }
                None => return,
            },
        }
    }
}

/// Swallow further changes for `window`. False if the channel closed.
fn drain_for(rx: &mpsc::Receiver<Change>, window: Duration) -> bool {
    let end = std::time::Instant::now() + window;
    loop {
        let left = end.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => return true,
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

/// `Some(true)` if the socket came back within [`LOST_AFTER`], `Some(false)` if not, `None` if the
/// channel closed.
fn wait_reconnect(rx: &mpsc::Receiver<Change>) -> Option<bool> {
    let end = std::time::Instant::now() + LOST_AFTER;
    loop {
        let left = end.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok(Change::Refresh) => return Some(drain_for(rx, DEBOUNCE)).filter(|ok| *ok),
            Ok(Change::Lost) => {}
            Err(RecvTimeoutError::Timeout) => return Some(false),
            Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

fn deliver(ipc: &HyprIpc, sink: &dyn EventSink<Vec<DisplayInfo>>) {
    match snapshot(ipc) {
        Ok(displays) => sink.send(displays),
        Err(e) => {
            tracing::warn!(error = %e, "hyprland monitors query failed; reporting no displays");
            sink.send(Vec::new());
        }
    }
}

fn snapshot(ipc: &HyprIpc) -> Result<Vec<DisplayInfo>, PlatformError> {
    parse_monitors(&ipc.json("monitors")?)
}

fn parse_monitors(json: &serde_json::Value) -> Result<Vec<DisplayInfo>, PlatformError> {
    let list = json
        .as_array()
        .ok_or_else(|| PlatformError::Backend("hyprland monitors: not a list".into()))?;
    list.iter().map(parse_monitor).collect()
}

fn parse_monitor(m: &serde_json::Value) -> Result<DisplayInfo, PlatformError> {
    let field = |key: &str| {
        m.get(key)
            .ok_or_else(|| PlatformError::Backend(format!("hyprland monitor: missing `{key}`")))
    };
    let uint = |key: &str| -> Result<u32, PlatformError> {
        field(key)?
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| PlatformError::Backend(format!("hyprland monitor: bad `{key}`")))
    };
    let float = |key: &str| -> Result<f64, PlatformError> {
        field(key)?
            .as_f64()
            .ok_or_else(|| PlatformError::Backend(format!("hyprland monitor: bad `{key}`")))
    };
    let int = |key: &str| -> Result<f64, PlatformError> {
        field(key)?
            .as_i64()
            .map(|v| v as f64)
            .ok_or_else(|| PlatformError::Backend(format!("hyprland monitor: bad `{key}`")))
    };

    // `width`/`height` and the physical size are the panel's, before the output transform;
    // transforms 1, 3, 5 and 7 rotate by 90° or 270°.
    let rotated = uint("transform")? % 2 == 1;
    let (mut w, mut h) = (uint("width")?, uint("height")?);
    let (mut pw, mut ph) = (
        f64::from(uint("physicalWidth").unwrap_or(0)),
        f64::from(uint("physicalHeight").unwrap_or(0)),
    );
    if pw <= 0.0 || ph <= 0.0 {
        pw = f64::from(w) * MM_PER_PX_96DPI;
        ph = f64::from(h) * MM_PER_PX_96DPI;
    }
    if rotated {
        std::mem::swap(&mut w, &mut h);
        std::mem::swap(&mut pw, &mut ph);
    }
    let scale = float("scale")?;
    if !(scale.is_finite() && scale > 0.0) {
        return Err(PlatformError::Backend(
            "hyprland monitor: bad `scale`".into(),
        ));
    }
    let refresh = float("refreshRate")?;
    Ok(DisplayInfo {
        id: DisplayId(uint("id")?),
        name: field("name")?.as_str().unwrap_or_default().to_owned(),
        geometry: DisplayGeometry {
            physical_size: SizeMm::new(pw, ph),
            pixel_size: PixelSize::new(w, h),
            scale,
            logical_origin: PointLogical::new(int("x")?, int("y")?),
        },
        refresh_millihz: (refresh * 1000.0).round().clamp(0.0, f64::from(u32::MAX)) as u32,
        color_space: ColorSpace::Srgb,
        hdr: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE_LIKE: &str = r#"[
        {"id":0,"name":"HDMI-A-1","width":1920,"height":1080,"x":0,"y":600,"scale":1,
         "transform":3,"physicalWidth":600,"physicalHeight":340,"refreshRate":74.973},
        {"id":2,"name":"DP-3","width":3440,"height":1440,"x":1080,"y":1080,"scale":1.25,
         "transform":0,"physicalWidth":800,"physicalHeight":340,"refreshRate":164.99899},
        {"id":5,"name":"WAYLAND-1","width":950,"height":1046,"x":-950,"y":0,"scale":1,
         "transform":0,"physicalWidth":0,"physicalHeight":0,"refreshRate":60.0}
    ]"#;

    #[test]
    fn maps_monitors() {
        let d = parse_monitors(&serde_json::from_str(LIVE_LIKE).unwrap()).unwrap();
        assert_eq!(d.len(), 3);

        let rotated = &d[0];
        assert_eq!(rotated.id, DisplayId(0));
        assert_eq!(rotated.name, "HDMI-A-1");
        assert_eq!(rotated.geometry.pixel_size, PixelSize::new(1080, 1920));
        assert_eq!(rotated.geometry.physical_size, SizeMm::new(340.0, 600.0));
        assert_eq!(
            rotated.geometry.logical_origin,
            PointLogical::new(0.0, 600.0)
        );
        assert_eq!(rotated.refresh_millihz, 74_973);

        let wide = &d[1];
        assert_eq!(wide.geometry.pixel_size, PixelSize::new(3440, 1440));
        assert_eq!(wide.geometry.scale, 1.25);
        assert_eq!(wide.refresh_millihz, 164_999);
        assert!(wide.geometry.is_valid());

        let nested = &d[2];
        assert!((nested.geometry.physical_size.width - 950.0 * 25.4 / 96.0).abs() < 1e-9);
        assert_eq!(
            nested.geometry.logical_origin,
            PointLogical::new(-950.0, 0.0)
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_monitors(&serde_json::json!({})).is_err());
        assert!(parse_monitors(&serde_json::json!([{"id": 0}])).is_err());
        let mut bad: serde_json::Value = serde_json::from_str(LIVE_LIKE).unwrap();
        bad[1]["scale"] = serde_json::json!(0);
        assert!(parse_monitors(&bad).is_err());
    }

    #[test]
    fn display_events() {
        assert!(is_display_event("monitoraddedv2"));
        assert!(is_display_event("configreloaded"));
        assert!(!is_display_event("openwindow"));
    }
}
