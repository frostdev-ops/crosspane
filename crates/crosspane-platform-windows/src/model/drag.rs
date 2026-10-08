//! Pure DRAG-v0 move classification and placed return (D-6); no hook, injection ledger or parking
//! authority of its own. `PlacedRestore` reaches parking only through the injected
//! [`WindowParking`], and places a window only after that backend has restored it.
//! `[E]` MOVESIZESTART/END identify a move OR resize, not a button or a title-bar drag:
//! <https://learn.microsoft.com/en-us/windows/win32/winauto/event-constants>.
//! `[P]` A fresh admitted identity, primary-only physical state, stable client extent/offset,
//! and four physical pixels of travel distinguish a move from resize/keyboard motion.
//! `[P]` An entered edge repeats at most 50 Hz while that same native move remains held;
//! observable inward/parallel travel, identity/geometry loss, or END releases it.
//! `[U]` Native observations and their freshness are the adapter's responsibility. These facts
//! never establish settlement, input delivery, a native move ending, or a parked window.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crosspane_platform::{CaptureEvent, CapturePortal, Edge, PlatformError, PortalId};
use crosspane_types::{
    geom::{PixelRect, PointDevice},
    id::WindowId,
    time::MonoTime,
};

use super::window::Identity;

/// `[E]` System-generated lifecycle constants, with the ambiguity documented above.
pub const EVENT_SYSTEM_MOVESIZESTART: u32 = 0x000a;
pub const EVENT_SYSTEM_MOVESIZEEND: u32 = 0x000b;

/// `[P]` Content is a PMv2 global physical rectangle, never an outer-window/title-bar rectangle.
/// `frame` is the DWM extended-frame (visible) rectangle. `grab` is relative to its top-left, the
/// origin `restore_at` places (D8, parity with the Mac). Classification still uses `content`.
/// Identity and WindowId come from the existing WindowSource/WindowResolver, not HWND encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowFact {
    pub window: WindowId,
    pub identity: Identity,
    pub content: PixelRect,
    pub frame: PixelRect,
}

impl WindowFact {
    fn valid(self) -> bool {
        self.identity.hwnd != 0
            && self.identity.pid != 0
            && self.identity.tid != 0
            && self.identity.process_created != 0
            && self.content.min.x < self.content.max.x
            && self.content.min.y < self.content.max.y
    }

    fn extent(self) -> (i64, i64) {
        (
            i64::from(self.content.max.x) - i64::from(self.content.min.x),
            i64::from(self.content.max.y) - i64::from(self.content.min.y),
        )
    }

    fn grab(self, point: (i32, i32)) -> PointDevice {
        // Widen before subtraction: negative virtual-screen origins are ordinary observations.
        PointDevice::new(
            (i64::from(point.0) - i64::from(self.frame.min.x)) as f64,
            (i64::from(point.1) - i64::from(self.frame.min.y)) as f64,
        )
    }
}

#[derive(Clone, Copy)]
struct Sample {
    fact: WindowFact,
    point: (i32, i32),
}

/// `[P]` Gesture-only state; physical button and suppressed-tail ownership remain in HookState.
#[derive(Default)]
pub struct Detector {
    portals: BTreeMap<PortalId, (CapturePortal, PixelRect)>,
    anchor: Option<Sample>,
    current: Option<Sample>,
    moving: bool,
    pressed: BTreeMap<PortalId, MonoTime>,
}

impl std::fmt::Debug for Detector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Detector(..)")
    }
}

impl Detector {
    /// Atomic portal replacement. Invalid geometry leaves the previous set and gesture intact.
    pub fn set_portals(
        &mut self,
        portals: &[(CapturePortal, PixelRect)],
        at: MonoTime,
    ) -> Result<Vec<CaptureEvent>, PlatformError> {
        let mut next = BTreeMap::new();
        for &(portal, rect) in portals {
            if !portal.from.is_finite()
                || !portal.to.is_finite()
                || portal.from < 0.0
                || portal.from >= portal.to
                || rect.min.x >= rect.max.x
                || rect.min.y >= rect.max.y
                || next.insert(portal.id, (portal, rect)).is_some()
            {
                return Err(PlatformError::Backend("invalid drag portal".into()));
            }
        }
        let mut events = Vec::new();
        self.pressed.retain(|portal, _| {
            let keep = self.portals.get(portal) == next.get(portal);
            if !keep {
                events.push(CaptureEvent::EdgeReleased {
                    portal: *portal,
                    at,
                });
            }
            keep
        });
        self.portals = next;
        Ok(events)
    }

    /// `[P]` Called only for an admitted MOVESIZESTART; keyboard/non-primary starts stay inert.
    pub fn start(
        &mut self,
        fact: WindowFact,
        pointer: (i32, i32),
        primary_only: bool,
        at: MonoTime,
    ) -> Vec<CaptureEvent> {
        let events = self.end(at);
        if primary_only && fact.valid() {
            let sample = Sample {
                fact,
                point: pointer,
            };
            self.anchor = Some(sample);
            self.current = Some(sample);
        }
        events
    }

    /// Caller supplies original receipt time. A later click never refreshes this observation.
    pub fn sample(
        &mut self,
        fact: Option<WindowFact>,
        pointer: (i32, i32),
        delta: Option<(f64, f64)>,
        primary_only: bool,
        at: MonoTime,
    ) -> Vec<CaptureEvent> {
        let Some(anchor) = self.anchor else {
            return Vec::new();
        };
        let Some(fact) = fact.filter(|fact| fact.valid()) else {
            return self.end(at);
        };
        let grab = fact.grab(pointer);
        let original_grab = anchor.fact.grab(anchor.point);
        let extent = fact.extent();
        let original_extent = anchor.fact.extent();
        if !primary_only
            || fact.window != anchor.fact.window
            || fact.identity != anchor.fact.identity
            || (extent.0 - original_extent.0).abs() > 1
            || (extent.1 - original_extent.1).abs() > 1
            || grab.distance_to(original_grab) > 1.0
        {
            return self.end(at);
        }
        let previous = self.current.replace(Sample {
            fact,
            point: pointer,
        });
        let delta = delta.or_else(|| {
            previous.map(|old| {
                (
                    f64::from(pointer.0) - f64::from(old.point.0),
                    f64::from(pointer.1) - f64::from(old.point.1),
                )
            })
        });
        self.moving |= (f64::from(pointer.0) - f64::from(anchor.point.0))
            .hypot(f64::from(pointer.1) - f64::from(anchor.point.1))
            >= 4.0;
        if !self.moving {
            return Vec::new();
        }
        let mut hits = BTreeSet::new();
        let mut events = Vec::new();
        for (&id, &(portal, rect)) in &self.portals {
            let pressure = delta.is_some_and(|(dx, dy)| {
                dx.is_finite()
                    && dy.is_finite()
                    && match portal.edge {
                        Edge::Left => dx < 0.0,
                        Edge::Right => dx > 0.0,
                        Edge::Top => dy < 0.0,
                        Edge::Bottom => dy > 0.0,
                    }
            }) || (delta == Some((0.0, 0.0)) && self.pressed.contains_key(&id));
            if !pressure
                || pointer.0 < rect.min.x
                || pointer.0 >= rect.max.x
                || pointer.1 < rect.min.y
                || pointer.1 >= rect.max.y
            {
                continue;
            }
            hits.insert(id);
            if self
                .pressed
                .get(&id)
                .is_none_or(|last| at.saturating_duration_since(*last) >= Duration::from_millis(20))
            {
                let (along, from, to) = match portal.edge {
                    Edge::Left | Edge::Right => (pointer.1, rect.min.y, rect.max.y),
                    Edge::Top | Edge::Bottom => (pointer.0, rect.min.x, rect.max.x),
                };
                events.push(CaptureEvent::DragAtEdge {
                    portal: id,
                    position: (f64::from(along) - f64::from(from))
                        / (f64::from(to) - f64::from(from)),
                    window: fact.window,
                    grab,
                    at,
                });
                self.pressed.insert(id, at);
            }
        }
        self.pressed.retain(|portal, _| {
            let keep = hits.contains(portal);
            if !keep {
                events.push(CaptureEvent::EdgeReleased {
                    portal: *portal,
                    at,
                })
            }
            keep
        });
        events
    }

    /// Current owned move candidate; does not establish a clean or settled input state.
    pub fn at_edge(&self, portal: PortalId) -> Option<WindowFact> {
        self.current
            .filter(|_| self.moving && self.pressed.contains_key(&portal))
            .map(|s| s.fact)
    }

    /// Activation owns the entered portal now; the synthetic local move end must not publish
    /// a pre-Started EdgeReleased. This retires only classification, never a physical tail.
    #[allow(dead_code)]
    pub(crate) fn consume(&mut self) {
        self.anchor = None;
        self.current = None;
        self.moving = false;
        self.pressed.clear();
    }

    pub fn end(&mut self, at: MonoTime) -> Vec<CaptureEvent> {
        self.anchor = None;
        self.current = None;
        self.moving = false;
        std::mem::take(&mut self.pressed)
            .into_keys()
            .map(|portal| CaptureEvent::EdgeReleased { portal, at })
            .collect()
    }
}

use super::geometry::{self, DisplayIds, MonitorProbe};
use crosspane_platform::{Parked, WindowParking};
use crosspane_types::{geom::PixelSize, id::DisplayId};
use std::time::Instant;

/// DRAG-v0 D-6: total budget for `restore` plus placement (the platform 2 s call bound).
pub const RESTORE_AT_BOUND: Duration = Duration::from_secs(2);

/// Outer-rectangle move that puts a restored window's visible (DWM extended-frame) top-left at
/// the clamped request. PMv2 global physical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    /// `SetWindowPos(.., SWP_NOSIZE ..)` x, y for the outer (`GetWindowRect`) rectangle.
    pub outer: (i32, i32),
    /// The visible top-left the native read-back must observe.
    pub visible: (i32, i32),
}

/// The unique non-twin probe whose retained-allocator id is `display`.
pub fn placement_monitor(
    display: DisplayId,
    probes: &[MonitorProbe],
    ids: &mut DisplayIds,
) -> Result<MonitorProbe, PlatformError> {
    geometry::displays(probes, ids)
        .map_err(|_| PlatformError::Backend("placement display layout".into()))?;
    let mut matching = probes
        .iter()
        .filter(|probe| !probe.twin && ids.assign(&probe.device_path) == Ok(display));
    let probe = matching.next().ok_or(PlatformError::NotFound)?;
    if matching.next().is_some() {
        return Err(PlatformError::Backend("ambiguous placement display".into()));
    }
    Ok(probe.clone())
}

/// `origin`: requested visible top-left, device pixels relative to `monitor.rc_monitor`'s
/// top-left. `outer`/`visible`: the restored window's current GetWindowRect and DWM bounds.
pub fn placed_origin(
    outer: [i32; 4],
    visible: [i32; 4],
    origin: PointDevice,
    monitor: &MonitorProbe,
) -> Result<Placement, PlatformError> {
    let invalid = || PlatformError::Backend("invalid placement geometry".into());
    let size = rect_pixel_size(visible).ok_or_else(invalid)?;
    if rect_pixel_size(outer).is_none() || !origin.x.is_finite() || !origin.y.is_finite() {
        return Err(invalid());
    }
    let clamped = geometry::work_area_clamp(origin, size, monitor.rc_work, monitor.rc_monitor)
        .map_err(|_| invalid())?;
    let to_i32 = |value: i64| i32::try_from(value).map_err(|_| invalid());
    // Whole device pixels. `clamped` lies inside `rc_monitor`, so the sums below stay in i64.
    let x = i64::from(monitor.rc_monitor[0]) + clamped.x.round() as i64;
    let y = i64::from(monitor.rc_monitor[1]) + clamped.y.round() as i64;
    // The outer rectangle keeps the frame offset it had before the move.
    let dx = i64::from(visible[0]) - i64::from(outer[0]);
    let dy = i64::from(visible[1]) - i64::from(outer[1]);
    Ok(Placement {
        outer: (to_i32(x - dx)?, to_i32(y - dy)?),
        visible: (to_i32(x)?, to_i32(y)?),
    })
}

/// Size of a non-empty `[left, top, right, bottom]` rectangle; `None` when it is empty or inverted.
fn rect_pixel_size(rect: [i32; 4]) -> Option<PixelSize> {
    let width = u32::try_from(i64::from(rect[2]) - i64::from(rect[0])).ok()?;
    let height = u32::try_from(i64::from(rect[3]) - i64::from(rect[1])).ok()?;
    (width > 0 && height > 0).then(|| PixelSize::new(width, height))
}

/// Read-back recomputes allowed for one placement while the frame keeps changing (W3.2c, W1.6b
/// Low 2): a mixed-DPI app re-lays out after the `WM_DPICHANGED` move, so the target is re-derived.
pub const PLACE_RECOMPUTES: u32 = 2;

/// Visible size or outer→visible offset differs; a pure translation is not a change.
/// Rectangles are `[left, top, right, bottom]`, the convention of [`placed_origin`].
pub fn frame_changed(
    before_outer: [i32; 4],
    before_visible: [i32; 4],
    now_outer: [i32; 4],
    now_visible: [i32; 4],
) -> bool {
    frame_shape(before_outer, before_visible) != frame_shape(now_outer, now_visible)
}

/// Visible width and height, then the visible rectangle's inset from the outer one on the left,
/// top, right and bottom sides. Translating both rectangles together leaves every value unchanged.
/// Coordinates are widened to `i64` first, so the saturating subtractions never clip a difference.
fn frame_shape(outer: [i32; 4], visible: [i32; 4]) -> [i64; 6] {
    let [outer_left, outer_top, outer_right, outer_bottom] = outer.map(i64::from);
    let [left, top, right, bottom] = visible.map(i64::from);
    [
        right.saturating_sub(left),
        bottom.saturating_sub(top),
        left.saturating_sub(outer_left),
        top.saturating_sub(outer_top),
        outer_right.saturating_sub(right),
        outer_bottom.saturating_sub(bottom),
    ]
}

/// Native placement of an already restored window. Never parks, restores or journals.
pub trait RestorePlacer: Send {
    /// Moves the window's visible top-left to `origin` and reads it back until `deadline`.
    fn place(
        &mut self,
        window: WindowId,
        display: DisplayId,
        origin: PointDevice,
        deadline: Instant,
    ) -> Result<(), PlatformError>;
}

/// DRAG-v0 placed return over any `WindowParking` (M1 now, M2 after W3.2).
pub struct PlacedRestore<P, L> {
    inner: P,
    placer: L,
    report: Box<dyn Fn(&PlatformError) + Send>,
}

impl<P, L> std::fmt::Debug for PlacedRestore<P, L> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlacedRestore").finish_non_exhaustive()
    }
}

impl<P: WindowParking, L: RestorePlacer> PlacedRestore<P, L> {
    /// `report` receives a placement failure; the restore it follows is still `Ok`.
    pub fn new(inner: P, placer: L, report: Box<dyn Fn(&PlatformError) + Send>) -> Self {
        Self {
            inner,
            placer,
            report,
        }
    }

    pub fn inner(&self) -> &P {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut P {
        &mut self.inner
    }
}

impl<P: WindowParking, L: RestorePlacer> WindowParking for PlacedRestore<P, L> {
    fn park(
        &mut self,
        window: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError> {
        self.inner.park(window, size, scale)
    }

    fn resize(
        &mut self,
        window: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError> {
        self.inner.resize(window, size, scale)
    }

    fn set_fullscreen(&mut self, window: WindowId, fullscreen: bool) -> Result<(), PlatformError> {
        self.inner.set_fullscreen(window, fullscreen)
    }

    fn geometry(&self, window: WindowId) -> Result<Parked, PlatformError> {
        self.inner.geometry(window)
    }

    fn restore(&mut self, window: WindowId) -> Result<(), PlatformError> {
        self.inner.restore(window)
    }

    // The placer runs only after the backend's `restore` has returned `Ok`, so the parking
    // entry is already retired. A crash before the placement leaves the window at its original
    // place, unparked, and nothing is lost.
    fn restore_at(
        &mut self,
        window: WindowId,
        display: DisplayId,
        origin: PointDevice,
    ) -> Result<(), PlatformError> {
        let deadline = Instant::now() + RESTORE_AT_BOUND;
        self.inner.restore(window)?;
        if let Err(error) = self.placer.place(window, display, origin, deadline) {
            (self.report)(&error);
        }
        Ok(())
    }

    fn recover(&mut self) -> Result<Vec<WindowId>, PlatformError> {
        self.inner.recover()
    }
}
