//! `OverlayHost` through the Shell bridge (WP-G1.5): the controller HUD and the target indicator.
//!
//! `show` maps the overlay's display to a point inside it (the centre of its logical rect, from
//! the displays snapshot function) and calls `ShowOverlay`; `OverlayState` signals become
//! `OverlayEvent::Visible`/`Unavailable`. A `Lost` bridge emits `Unavailable` for every shown
//! overlay. An unknown display is `PlatformError::NotFound`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crosspane_platform::{
    EventSink, Overlay, OverlayAnchor, OverlayEvent, OverlayHost, OverlayId, PlatformError, Rgb8,
};
use crosspane_types::display::DisplayInfo;
use crosspane_types::id::DisplayId;

use super::shell::{ShellBridge, ShellCallback, ShellEvent};
use crate::portal::eis::DisplaysFn;

/// Longest overlay text in characters; a longer text is cut and ends in an ellipsis.
const MAX_TEXT_CHARS: usize = 128;

/// Overlays drawn by the Crosspane Shell extension.
pub struct GnomeOverlay {
    bridge: ShellBridge,
    displays: DisplaysFn,
    /// Shared with the bridge callback, which holds this `Arc` and never the bridge itself.
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    /// Set by `subscribe`; the bridge callback sends through it.
    sink: Option<Arc<dyn EventSink<OverlayEvent>>>,
    /// Ids the engine asked to show and has not hidden or lost.
    shown: HashSet<OverlayId>,
}

impl std::fmt::Debug for GnomeOverlay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GnomeOverlay").finish_non_exhaustive()
    }
}

impl GnomeOverlay {
    pub fn new(bridge: ShellBridge, displays: DisplaysFn) -> GnomeOverlay {
        GnomeOverlay {
            bridge,
            displays,
            state: Arc::new(Mutex::new(State::default())),
        }
    }
}

impl OverlayHost for GnomeOverlay {
    fn subscribe(&mut self, sink: Arc<dyn EventSink<OverlayEvent>>) -> Result<(), PlatformError> {
        {
            let mut state = lock(&self.state);
            if state.sink.is_some() {
                return Err(PlatformError::Backend("overlay already subscribed".into()));
            }
            // Stored before the callback is registered, so no event can arrive without a sink.
            state.sink = Some(sink);
        }
        let shared = self.state.clone();
        let callback: ShellCallback = Arc::new(move |event: ShellEvent| deliver(&shared, event));
        let result = self.bridge.subscribe(callback);
        if result.is_err() {
            // The host stays unsubscribed, so a later call can retry.
            lock(&self.state).sink = None;
        }
        result
    }

    fn show(&mut self, id: OverlayId, overlay: &Overlay) -> Result<(), PlatformError> {
        let displays = (self.displays)();
        let (x, y) = display_point(find_display(&displays, overlay.display)?)?;
        let anchor = anchor_code(overlay.anchor);
        let accent = accent_rgb(overlay.accent);
        let text = truncate_text(&overlay.text);
        // Recorded before the call, so a fast `OverlayState(true)` is not taken for a stale one.
        let newly_shown = lock(&self.state).shown.insert(id);
        let result = self.bridge.show_overlay(id.0, x, y, anchor, &text, accent);
        if result.is_err() && newly_shown {
            lock(&self.state).shown.remove(&id);
        }
        result
    }

    fn hide(&mut self, id: OverlayId) -> Result<(), PlatformError> {
        // Forgotten first, so the `OverlayState(false)` echo of this hide is ignored.
        // Known accepted race: a rapid hide(id) then show(id) can let the old echo arrive after the
        // re-insert, which reports a spurious `Unavailable`. That errs on the safe side: the engine
        // ends a capture on `Unavailable`.
        lock(&self.state).shown.remove(&id);
        self.bridge.hide_overlay(id.0)
    }
}

/// Locks the state. Every update is a whole step on plain data, so a poisoned lock is taken anyway.
fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs on the bridge's signal thread, the only caller of `send`, so events keep the bridge's
/// order. The lock is released before anything is sent.
fn deliver(shared: &Mutex<State>, event: ShellEvent) {
    let (sink, events) = {
        let mut state = lock(shared);
        let events = overlay_events(&mut state, event);
        (state.sink.clone(), events)
    };
    // `sink` is only missing after a failed `subscribe`, when nobody listens.
    if let Some(sink) = sink {
        for event in events {
            sink.send(event);
        }
    }
}

/// The events one bridge signal produces, updating `state`.
fn overlay_events(state: &mut State, event: ShellEvent) -> Vec<OverlayEvent> {
    match event {
        ShellEvent::OverlayState { id, visible: true } => {
            let id = OverlayId(id);
            // An id that is not shown any more is a stale signal.
            if state.shown.contains(&id) {
                vec![OverlayEvent::Visible(id)]
            } else {
                Vec::new()
            }
        }
        ShellEvent::OverlayState { id, visible: false } => {
            let id = OverlayId(id);
            // Not shown is the echo of our own `hide`. Shown is unsolicited: the overlay can't be
            // kept on screen (monitor gone, extension disabled).
            if state.shown.remove(&id) {
                vec![OverlayEvent::Unavailable(id)]
            } else {
                Vec::new()
            }
        }
        ShellEvent::Lost => {
            let mut ids: Vec<OverlayId> = std::mem::take(&mut state.shown).into_iter().collect();
            ids.sort_unstable();
            ids.into_iter().map(OverlayEvent::Unavailable).collect()
        }
        ShellEvent::WindowsChanged { .. } => Vec::new(),
    }
}

fn find_display(displays: &[DisplayInfo], id: DisplayId) -> Result<&DisplayInfo, PlatformError> {
    displays
        .iter()
        .find(|display| display.id == id)
        .ok_or(PlatformError::NotFound)
}

/// The point the overlay is shown at: the centre of the display's logical rect, in the Shell's
/// global logical coordinates.
fn display_point(display: &DisplayInfo) -> Result<(i32, i32), PlatformError> {
    let geometry = &display.geometry;
    let scale = geometry.scale;
    let size = geometry.pixel_size;
    let origin = geometry.logical_origin;
    if !scale.is_finite()
        || scale <= 0.0
        || size.width == 0
        || size.height == 0
        || !origin.x.is_finite()
        || !origin.y.is_finite()
    {
        return Err(PlatformError::Backend(
            "overlay display geometry is invalid".into(),
        ));
    }
    let x = origin.x + f64::from(size.width) / scale / 2.0;
    let y = origin.y + f64::from(size.height) / scale / 2.0;
    Ok((to_i32(x), to_i32(y)))
}

/// Rounds to the nearest whole unit and saturates at the `i32` range (a float `as` cast never
/// wraps). The inputs are finite, so NaN can't reach it.
fn to_i32(value: f64) -> i32 {
    value
        .round()
        .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

/// The `ShowOverlay` anchor codes of `io.frostdev.Crosspane.Shell1`.
fn anchor_code(anchor: OverlayAnchor) -> u32 {
    match anchor {
        OverlayAnchor::TopCenter => 0,
        OverlayAnchor::TopRight => 1,
        OverlayAnchor::BottomRight => 2,
        OverlayAnchor::Center => 3,
    }
}

/// The `ShowOverlay` accent: 0xRRGGBB.
fn accent_rgb(color: Rgb8) -> u32 {
    (u32::from(color.r) << 16) | (u32::from(color.g) << 8) | u32::from(color.b)
}

fn truncate_text(text: &str) -> String {
    let mut chars = text.chars();
    let mut result: String = chars.by_ref().take(MAX_TEXT_CHARS).collect();
    if chars.next().is_some() {
        result.pop();
        result.push('…');
    }
    result
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use crosspane_types::color::ColorSpace;
    use crosspane_types::geom::{DisplayGeometry, PixelSize, PointLogical, SizeMm};

    use super::*;

    fn display(id: u32, width: u32, height: u32, scale: f64, origin: (f64, f64)) -> DisplayInfo {
        DisplayInfo {
            id: DisplayId(id),
            name: format!("test-{id}"),
            geometry: DisplayGeometry {
                physical_size: SizeMm::new(600.0, 340.0),
                pixel_size: PixelSize::new(width, height),
                scale,
                logical_origin: PointLogical::new(origin.0, origin.1),
            },
            refresh_millihz: 60_000,
            color_space: ColorSpace::Srgb,
            hdr: false,
        }
    }

    fn state_with(ids: &[u32]) -> State {
        State {
            sink: None,
            shown: ids.iter().map(|&id| OverlayId(id)).collect(),
        }
    }

    #[test]
    fn display_point_is_the_centre_of_the_logical_rect() {
        let primary = display(1, 1920, 1080, 1.0, (0.0, 0.0));
        assert_eq!(display_point(&primary).unwrap(), (960, 540));
        // 3024x1964 device px at 2x, starting at logical x 1920.
        let laptop = display(2, 3024, 1964, 2.0, (1920.0, 0.0));
        assert_eq!(display_point(&laptop).unwrap(), (2676, 491));
    }

    #[test]
    fn display_point_rounds_fractional_centres() {
        // 1000x500 device px at 1.5x: the centre (333.33, 166.67) rounds to (333, 167).
        let fractional = display(3, 1000, 500, 1.5, (0.0, 0.0));
        assert_eq!(display_point(&fractional).unwrap(), (333, 167));
    }

    #[test]
    fn display_point_rejects_invalid_geometry() {
        assert!(display_point(&display(1, 1920, 1080, 0.0, (0.0, 0.0))).is_err());
        assert!(display_point(&display(1, 1920, 1080, f64::NAN, (0.0, 0.0))).is_err());
        assert!(display_point(&display(1, 1920, 1080, -1.0, (0.0, 0.0))).is_err());
        assert!(display_point(&display(1, 0, 1080, 1.0, (0.0, 0.0))).is_err());
        assert!(display_point(&display(1, 1920, 1080, 1.0, (f64::NAN, 0.0))).is_err());
    }

    #[test]
    fn unknown_display_is_not_found() {
        let displays = [display(1, 1920, 1080, 1.0, (0.0, 0.0))];
        assert!(find_display(&displays, DisplayId(1)).is_ok());
        assert!(matches!(
            find_display(&displays, DisplayId(2)),
            Err(PlatformError::NotFound)
        ));
    }

    #[test]
    fn to_i32_rounds_and_saturates() {
        assert_eq!(to_i32(2.5), 3);
        assert_eq!(to_i32(-2.5), -3);
        assert_eq!(to_i32(1e300), i32::MAX);
        assert_eq!(to_i32(-1e300), i32::MIN);
    }

    #[test]
    fn anchor_codes_match_the_extension() {
        assert_eq!(anchor_code(OverlayAnchor::TopCenter), 0);
        assert_eq!(anchor_code(OverlayAnchor::TopRight), 1);
        assert_eq!(anchor_code(OverlayAnchor::BottomRight), 2);
        assert_eq!(anchor_code(OverlayAnchor::Center), 3);
    }

    #[test]
    fn accent_is_packed_rgb() {
        assert_eq!(
            accent_rgb(Rgb8 {
                r: 0x12,
                g: 0x34,
                b: 0x56
            }),
            0x12_34_56
        );
        assert_eq!(accent_rgb(Rgb8 { r: 255, g: 0, b: 0 }), 0xFF_00_00);
    }

    #[test]
    fn text_is_cut_to_128_chars_with_an_ellipsis() {
        assert_eq!(truncate_text("short"), "short");
        let exact = "x".repeat(128);
        assert_eq!(truncate_text(&exact), exact);
        let long = "x".repeat(200);
        let cut = truncate_text(&long);
        assert_eq!(cut.chars().count(), 128);
        assert!(cut.ends_with('…'));
        assert_eq!(cut.chars().filter(|&c| c == 'x').count(), 127);
    }

    #[test]
    fn visible_only_for_a_shown_overlay() {
        let mut state = state_with(&[3]);
        let shown = overlay_events(
            &mut state,
            ShellEvent::OverlayState {
                id: 3,
                visible: true,
            },
        );
        assert_eq!(shown, [OverlayEvent::Visible(OverlayId(3))]);
        let stale = overlay_events(
            &mut state,
            ShellEvent::OverlayState {
                id: 4,
                visible: true,
            },
        );
        assert!(stale.is_empty());
    }

    #[test]
    fn unsolicited_hide_of_a_shown_overlay_is_unavailable_and_forgotten() {
        let mut state = state_with(&[3]);
        let events = overlay_events(
            &mut state,
            ShellEvent::OverlayState {
                id: 3,
                visible: false,
            },
        );
        assert_eq!(events, [OverlayEvent::Unavailable(OverlayId(3))]);
        assert!(state.shown.is_empty());
    }

    #[test]
    fn echo_of_our_own_hide_is_ignored() {
        let mut state = state_with(&[]);
        let events = overlay_events(
            &mut state,
            ShellEvent::OverlayState {
                id: 3,
                visible: false,
            },
        );
        assert!(events.is_empty());
    }

    #[test]
    fn lost_reports_every_shown_overlay_in_order_and_clears_them() {
        let mut state = state_with(&[3, 1, 2]);
        let events = overlay_events(&mut state, ShellEvent::Lost);
        assert_eq!(
            events,
            [
                OverlayEvent::Unavailable(OverlayId(1)),
                OverlayEvent::Unavailable(OverlayId(2)),
                OverlayEvent::Unavailable(OverlayId(3)),
            ]
        );
        assert!(state.shown.is_empty());
    }

    #[test]
    fn windows_changed_produces_nothing() {
        let mut state = state_with(&[3]);
        let events = overlay_events(&mut state, ShellEvent::WindowsChanged { epoch: 7 });
        assert!(events.is_empty());
        assert!(state.shown.contains(&OverlayId(3)));
    }
}
