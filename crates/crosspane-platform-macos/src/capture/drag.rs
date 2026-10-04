//! Event-driven native window-move facts. No window contents or titles are retained.

use std::collections::{HashMap, HashSet};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use crosspane_platform::{CaptureEvent, PlatformError, PortalId};
use crosspane_types::geom::{PointDevice, PointLogical, RectLogical, SizeLogical};
use crosspane_types::id::WindowId;
use crosspane_types::time::MonoTime;
use objc2_core_foundation::{CFDictionary, CFNumber, CFString, CFType, CGPoint, CGRect};
use objc2_core_graphics::{
    CGGetDisplaysWithPoint, CGRectMakeWithDictionaryRepresentation, CGWindowListCopyWindowInfo,
    CGWindowListOption, kCGNullWindowID, kCGWindowBounds, kCGWindowLayer, kCGWindowNumber,
    kCGWindowOwnerPID,
};

use super::{Edge, Portal};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct WindowFact {
    pub window: WindowId,
    pub pid: i32,
    pub frame: RectLogical,
    pub scale: f64,
}

#[derive(Clone, Copy)]
struct Sample {
    window: WindowFact,
    pointer: PointLogical,
    offset: PointLogical,
}

#[derive(Default)]
pub(super) struct Detector {
    anchor: Option<Sample>,
}

impl Detector {
    pub fn sample(&mut self, window: WindowFact, pointer: PointLogical) -> bool {
        let now = Sample {
            window,
            pointer,
            offset: PointLogical::new(
                pointer.x - window.frame.origin.x,
                pointer.y - window.frame.origin.y,
            ),
        };
        // A move keeps the size too: resizing by the left or top edge also keeps the pointer's
        // offset to the origin constant, and must never read as a move.
        let stable = self.anchor.is_some_and(|old| {
            old.window.window == window.window
                && old.window.pid == window.pid
                && old.offset.distance_to(now.offset) <= 1.0
                && (old.window.frame.size.width - window.frame.size.width).abs() <= 1.0
                && (old.window.frame.size.height - window.frame.size.height).abs() <= 1.0
        });
        let detected = stable
            && self
                .anchor
                .is_some_and(|old| old.pointer.distance_to(pointer) >= 4.0);
        // The first stable sample lets slow native motion accumulate the four-point threshold.
        if !stable {
            self.anchor = Some(now);
        }
        detected
    }
}

pub(super) fn distance(portal: Portal, pointer: CGPoint) -> f64 {
    let bounds = portal.display.bounds;
    match portal.portal.edge {
        Edge::Left => (pointer.x - bounds.origin.x).abs(),
        Edge::Right => (pointer.x - bounds.origin.x - bounds.size.width).abs(),
        Edge::Top => (pointer.y - bounds.origin.y).abs(),
        Edge::Bottom => (pointer.y - bounds.origin.y - bounds.size.height).abs(),
    }
}

#[derive(Default)]
pub(super) struct Move {
    detector: Detector,
    pub window: Option<WindowFact>,
    pub pointer: CGPoint,
    moving: bool,
    pre_edge: HashMap<PortalId, RectLogical>,
    pressed: HashSet<PortalId>,
    emitted: HashMap<PortalId, MonoTime>,
}

impl Move {
    pub fn should_lookup(&self, held: bool, portals: &[Portal], pointer: CGPoint) -> bool {
        held && !portals.is_empty()
            && (self.moving || portals.iter().any(|p| distance(*p, pointer) <= 64.0))
    }

    pub fn sample(&mut self, window: Option<WindowFact>, pointer: CGPoint, portals: &[Portal]) {
        if self.window.map(|w| (w.window, w.pid)) != window.map(|w| (w.window, w.pid)) {
            self.detector = Detector::default();
            self.moving = false;
            self.pre_edge.clear();
        }
        self.window = window;
        self.pointer = pointer;
        if let Some(window) = window {
            self.moving |= self
                .detector
                .sample(window, PointLogical::new(pointer.x, pointer.y));
            for portal in portals {
                if distance(*portal, pointer) >= 48.0 {
                    self.pre_edge.insert(portal.portal.id, window.frame);
                }
            }
        }
    }

    pub fn update(
        &mut self,
        portals: &[Portal],
        hits: &[(PortalId, f64)],
        at: MonoTime,
    ) -> Vec<CaptureEvent> {
        let now: HashSet<_> = if self.moving {
            hits.iter().map(|(id, _)| *id).collect()
        } else {
            HashSet::new()
        };
        let mut events = Vec::new();
        for &portal in self.pressed.difference(&now) {
            events.push(CaptureEvent::EdgeReleased { portal, at });
            self.emitted.remove(&portal);
        }
        if let Some(window) = self.window.filter(|_| self.moving) {
            for &(portal, position) in hits {
                if self.emitted.get(&portal).is_some_and(|last| {
                    at.saturating_duration_since(*last) < Duration::from_millis(20)
                }) {
                    continue;
                }
                if portals.iter().any(|p| p.portal.id == portal) {
                    self.emitted.insert(portal, at);
                    events.push(CaptureEvent::DragAtEdge {
                        portal,
                        position,
                        window: window.window,
                        grab: PointDevice::new(
                            (self.pointer.x - window.frame.origin.x) * window.scale,
                            (self.pointer.y - window.frame.origin.y) * window.scale,
                        ),
                        at,
                    });
                }
            }
        }
        self.pressed = now;
        events
    }

    pub fn at_edge(&self, portal: PortalId) -> Option<(WindowFact, Option<RectLogical>)> {
        self.window
            .filter(|_| self.moving && self.pressed.contains(&portal))
            .map(|window| (window, self.pre_edge.get(&portal).copied()))
    }

    pub fn clear(&mut self, at: MonoTime) -> Vec<CaptureEvent> {
        // Every untracked motion event lands here: keep that path free of work.
        if self.window.is_none()
            && !self.moving
            && self.pressed.is_empty()
            && self.pre_edge.is_empty()
            && self.emitted.is_empty()
        {
            return Vec::new();
        }
        let events = self
            .pressed
            .iter()
            .map(|&portal| CaptureEvent::EdgeReleased { portal, at })
            .collect();
        *self = Self::default();
        events
    }
}

fn contains(frame: RectLogical, point: CGPoint) -> bool {
    point.x >= frame.min_x()
        && point.x < frame.max_x()
        && point.y >= frame.min_y()
        && point.y < frame.max_y()
}

fn scale_for_frame(
    frame: RectLogical,
    pointer: PointLogical,
    mut read: impl FnMut(PointLogical) -> Result<f64, PlatformError>,
) -> Result<f64, PlatformError> {
    match read(frame.center()) {
        Err(PlatformError::NotFound) => read(pointer),
        result => result,
    }
}

/// Public Quartz facts only, front to back, with no window title/content lookup.
pub(super) fn under_pointer(point: CGPoint) -> Result<Option<WindowFact>, PlatformError> {
    let list = CGWindowListCopyWindowInfo(
        CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements,
        kCGNullWindowID,
    )
    .ok_or_else(|| PlatformError::Backend("drag window list unavailable".into()))?;
    // SAFETY: Quartz returns CFDictionary CF objects; every dynamic value is checked below.
    let list = unsafe { list.cast_unchecked::<CFType>() };
    for value in list.iter() {
        let Ok(dictionary) = value.downcast::<CFDictionary>() else {
            continue;
        };
        // SAFETY: public Quartz dictionaries have CFString keys and CF object values.
        let dictionary = unsafe { dictionary.cast_unchecked::<CFString, CFType>() };
        // SAFETY: immutable public CoreGraphics dictionary keys.
        let (layer, number, pid, bounds) = unsafe {
            (
                kCGWindowLayer,
                kCGWindowNumber,
                kCGWindowOwnerPID,
                kCGWindowBounds,
            )
        };
        let integer = |key| dictionary.get(key)?.downcast::<CFNumber>().ok()?.as_i64();
        let fact = (|| {
            if integer(layer)? != 0 {
                return None;
            }
            let bounds = dictionary.get(bounds)?.downcast::<CFDictionary>().ok()?;
            let mut frame = CGRect::default();
            // SAFETY: checked dictionary and initialized writable CGRect storage.
            if !unsafe { CGRectMakeWithDictionaryRepresentation(Some(&bounds), &mut frame) } {
                return None;
            }
            let frame = RectLogical::new(
                PointLogical::new(frame.origin.x, frame.origin.y),
                SizeLogical::new(frame.size.width, frame.size.height),
            );
            if !crate::windows::valid_frame(frame) || !contains(frame, point) {
                return None;
            }
            let scale = scale_for_frame(frame, PointLogical::new(point.x, point.y), |point| {
                let mut display = 0;
                let mut count = 0;
                super::cg_result(
                    // SAFETY: public read-only query with one writable display slot and count.
                    unsafe {
                        CGGetDisplaysWithPoint(
                            CGPoint::new(point.x, point.y),
                            1,
                            &mut display,
                            &mut count,
                        )
                    },
                    "locate drag display",
                )?;
                if count == 0 {
                    return Err(PlatformError::NotFound);
                }
                Ok(super::Display::read(crosspane_types::id::DisplayId(display))?.scale)
            })
            .ok()?;
            Some(WindowFact {
                window: WindowId(u64::from(u32::try_from(integer(number)?).ok()?)),
                pid: i32::try_from(integer(pid)?).ok()?,
                frame,
                scale,
            })
        })();
        if fact.is_some() {
            return Ok(fact);
        }
    }
    Ok(None)
}

/// A changed size is necessary: ordinary final movement must never be undone.
pub(super) fn tiled(
    before: RectLogical,
    current: RectLogical,
    work: RectLogical,
    started: Instant,
    now: Instant,
) -> bool {
    if now.saturating_duration_since(started) > Duration::from_millis(500)
        || !crate::windows::valid_frame(current)
        || ((current.size.width - before.size.width).abs() <= 2.0
            && (current.size.height - before.size.height).abs() <= 2.0)
    {
        return false;
    }
    // Native half/quarter/maximized tiles reach a work-area edge. Allow the small public
    // window border/gap seen in P8b; arbitrary app resizes in the middle are left alone.
    let near = |a: f64, b: f64| (a - b).abs() <= 16.0;
    let width = near(current.size.width, work.size.width)
        || near(current.size.width, work.size.width / 2.0);
    let height = near(current.size.height, work.size.height)
        || near(current.size.height, work.size.height / 2.0);
    width
        && height
        && (near(current.min_x(), work.min_x()) || near(current.max_x(), work.max_x()))
        && (near(current.min_y(), work.min_y()) || near(current.max_y(), work.max_y()))
}

fn restored_size_ready(
    window: WindowFact,
    before: RectLogical,
    observed: RectLogical,
    deadline: Instant,
    mut read: impl FnMut() -> Result<(WindowId, i32, RectLogical, bool), PlatformError>,
    mut pause: impl FnMut() -> Result<(), PlatformError>,
) -> Result<bool, PlatformError> {
    loop {
        if Instant::now() >= deadline {
            return Err(PlatformError::Timeout);
        }
        let (id, pid, frame, visible) = read()?;
        if Instant::now() >= deadline {
            return Err(PlatformError::Timeout);
        }
        if id != window.window || pid != window.pid || !visible {
            return Ok(false);
        }
        if (frame.size.width - before.size.width).abs() <= 2.0
            && (frame.size.height - before.size.height).abs() <= 2.0
        {
            return Ok(true);
        }
        // Quartz may still show the original tile after AXSize. Wait only through exactly
        // that stale observation, never through an unrelated move/resize or identity change.
        if !crate::windows::bounds_equal(frame, observed) {
            return Ok(false);
        }
        pause()?;
    }
}

pub(super) fn correct_tiling(
    window: WindowFact,
    before: RectLogical,
    display: crosspane_types::id::DisplayId,
    busy: Arc<AtomicBool>,
) {
    if busy.swap(true, Ordering::AcqRel) {
        tracing::warn!("drag tiling correction skipped: previous native operation still pending");
        return;
    }
    struct Lease(Arc<AtomicBool>);
    impl Drop for Lease {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }
    let lease = Lease(busy);
    let started = Instant::now();
    let result = std::thread::Builder::new()
        .name("mac-drag-tiling".into())
        .spawn(move || {
            let _lease = lease;
            let deadline = started + Duration::from_millis(500);
            let result: Result<(), PlatformError> = (|| {
                let work = crate::displays::visible_frame(display)?;
                let query = crate::parking::frame_query()?;
                while Instant::now() < deadline {
                    let raw = query
                        .list_until(false, deadline)?
                        .into_iter()
                        .find(|raw| raw.id == window.window && raw.pid == window.pid)
                        .ok_or(PlatformError::NotFound)?;
                    if tiled(before, raw.frame, work, started, Instant::now()) {
                        let observed = raw.frame;
                        let mut size_written = false;
                        crate::parking::set_window_frame(&raw, before, deadline, || {
                            if size_written {
                                return restored_size_ready(
                                    window,
                                    before,
                                    observed,
                                    deadline,
                                    || {
                                        query
                                            .list_until(false, deadline)?
                                            .into_iter()
                                            .find(|raw| {
                                                raw.id == window.window && raw.pid == window.pid
                                            })
                                            .map(|raw| (raw.id, raw.pid, raw.frame, raw.on_screen))
                                            .ok_or(PlatformError::NotFound)
                                    },
                                    || {
                                        let sleep = crate::windows::next_sleep(
                                            deadline,
                                            Instant::now(),
                                            Duration::from_millis(20),
                                        )
                                        .ok_or(PlatformError::Timeout)?;
                                        std::thread::sleep(sleep);
                                        Ok(())
                                    },
                                )
                                .unwrap_or(false);
                            }
                            let allowed = Instant::now() < deadline
                                && query.list_until(false, deadline).is_ok_and(|windows| {
                                    windows.iter().any(|raw| {
                                        raw.id == window.window
                                            && raw.pid == window.pid
                                            && raw.on_screen
                                            && crate::windows::bounds_equal(raw.frame, observed)
                                    })
                                });
                            size_written = true;
                            allowed
                        })?;
                        return Ok(());
                    }
                    std::thread::sleep(
                        deadline
                            .saturating_duration_since(Instant::now())
                            .min(Duration::from_millis(20)),
                    );
                }
                Ok(())
            })();
            if let Err(error) = result {
                tracing::warn!(%error, "drag tiling correction unavailable; capture continues");
            }
        });
    if let Err(error) = result {
        tracing::warn!(%error, "spawn drag tiling correction failed");
    }
}

#[cfg(test)]
#[path = "../../tests/drag/detector.rs"]
mod tests;
