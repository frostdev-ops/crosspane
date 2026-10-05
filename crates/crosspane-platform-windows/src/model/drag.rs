//! Pure DRAG-v0 move classification; no hook, injection ledger or parking authority.
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
/// Identity and WindowId come from the existing WindowSource/WindowResolver, not HWND encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowFact {
    pub window: WindowId,
    pub identity: Identity,
    pub content: PixelRect,
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
            (i64::from(point.0) - i64::from(self.content.min.x)) as f64,
            (i64::from(point.1) - i64::from(self.content.min.y)) as f64,
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
