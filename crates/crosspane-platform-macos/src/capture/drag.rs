//! Event-driven native window-move facts. No window contents or titles are retained.

use std::collections::{HashMap, HashSet};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use crosspane_platform::{CaptureEvent, PlatformError, PortalId};
use crosspane_types::geom::{PixelSize, PointDevice, PointLogical, RectLogical, SizeLogical};
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
    latched: bool,
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
                && old.window.frame.size == window.frame.size
        });
        let detected = stable
            && self
                .anchor
                .is_some_and(|old| old.pointer.distance_to(pointer) >= 4.0);
        // The first stable sample lets slow native motion accumulate the four-point threshold.
        if !stable {
            self.anchor = Some(now);
            self.latched = false;
        }
        self.latched |= detected;
        self.latched
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
    last_lookup: Option<MonoTime>,
    native_reported: Option<WindowId>,
}

impl Move {
    pub fn should_lookup(&self, held: bool, portals: &[Portal], _pointer: CGPoint) -> bool {
        held && !portals.is_empty()
    }

    pub fn lookup_due(&mut self, at: MonoTime) -> bool {
        if self
            .last_lookup
            .is_some_and(|last| at.saturating_duration_since(last) < Duration::from_millis(20))
        {
            return false;
        }
        self.last_lookup = Some(at);
        true
    }

    pub fn sample(&mut self, window: Option<WindowFact>, pointer: CGPoint, portals: &[Portal]) {
        let was_moving = self.moving;
        let old_window = self.window;
        if self.window.map(|w| (w.window, w.pid)) != window.map(|w| (w.window, w.pid)) {
            self.detector = Detector::default();
            self.moving = false;
            self.pre_edge.clear();
        }
        self.window = window;
        self.pointer = pointer;
        if let Some(window) = window {
            self.moving = self
                .detector
                .sample(window, PointLogical::new(pointer.x, pointer.y));
            for portal in portals {
                if distance(*portal, pointer) >= 48.0 {
                    self.pre_edge.insert(portal.portal.id, window.frame);
                }
            }
        }
        if was_moving != self.moving {
            if !self.moving {
                self.pre_edge.clear();
            }
            let reason = if self.moving {
                "stable move"
            } else if old_window.map(|w| (w.window, w.pid)) != window.map(|w| (w.window, w.pid)) {
                "window identity changed"
            } else if old_window.map(|w| w.frame.size) != window.map(|w| w.frame.size) {
                "window size changed"
            } else {
                "grab offset changed"
            };
            tracing::debug!(window = ?old_window.or(window).map(|w| w.window), latched = self.moving, reason, "drag detector state changed");
        }
    }

    pub fn update(
        &mut self,
        portals: &[Portal],
        hits: &[(PortalId, f64)],
        at: MonoTime,
    ) -> Vec<CaptureEvent> {
        // Only an outward hit starts a press; a pressed drag tolerates pinned or inward motion
        // within the edge band. Ordinary E1 continues to use portal_hit unchanged.
        let mut hits = hits.to_vec();
        for portal in portals {
            if self.pressed.contains(&portal.portal.id)
                && !hits.iter().any(|(id, _)| *id == portal.portal.id)
                && let Some(position) = super::drag_hit(*portal, self.pointer)
            {
                hits.push((portal.portal.id, position));
            }
        }
        let now: HashSet<_> = if self.moving {
            hits.iter().map(|(id, _)| *id).collect()
        } else {
            HashSet::new()
        };
        let mut events = Vec::new();
        for &portal in self.pressed.difference(&now) {
            tracing::debug!(window = ?self.window.map(|w| w.window), portal = portal.0, reason = if self.moving { "left edge band or span" } else { "move latch broken" }, "drag portal released");
            events.push(CaptureEvent::EdgeReleased { portal, at });
            self.emitted.remove(&portal);
        }
        if let (Some(window), Some(sample)) =
            (self.window.filter(|_| self.moving), self.detector.anchor)
        {
            for &(portal, position) in &hits {
                if self.emitted.get(&portal).is_some_and(|last| {
                    at.saturating_duration_since(*last) < Duration::from_millis(20)
                }) {
                    continue;
                }
                if portals.iter().any(|p| p.portal.id == portal) {
                    if !self.pressed.contains(&portal) {
                        tracing::debug!(
                            window = window.window.0,
                            portal = portal.0,
                            "drag portal pressed"
                        );
                    }
                    self.emitted.insert(portal, at);
                    events.push(CaptureEvent::DragAtEdge {
                        portal,
                        position,
                        window: window.window,
                        // The pointer may be newer than the rate-limited window observation.
                        // Keep the coherent latched grab rather than mixing those two samples.
                        grab: PointDevice::new(
                            sample.offset.x * window.scale,
                            sample.offset.y * window.scale,
                        ),
                        at,
                    });
                }
            }
        }
        self.pressed = now;
        events
    }

    /// Publish only after sample(): pointer, frame and scale then describe one observation.
    /// The native path has no portals and shares the physical move's coherence detector.
    pub fn native(&mut self, at: MonoTime) -> Vec<CaptureEvent> {
        let fact = self.window.filter(|_| self.moving).and_then(|window| {
            let width = (window.frame.size.width * window.scale).round();
            let height = (window.frame.size.height * window.scale).round();
            let grab = PointDevice::new(
                (self.pointer.x - window.frame.origin.x) * window.scale,
                (self.pointer.y - window.frame.origin.y) * window.scale,
            );
            if !crate::windows::valid_frame(window.frame)
                || !window.scale.is_finite()
                || window.scale <= 0.0
                || ![width, height, grab.x, grab.y]
                    .into_iter()
                    .all(f64::is_finite)
                || width < 1.0
                || height < 1.0
                || width > f64::from(u32::MAX)
                || height > f64::from(u32::MAX)
                || grab.x < 0.0
                || grab.y < 0.0
                || grab.x >= width
                || grab.y >= height
            {
                return None;
            }
            Some((
                window.window,
                grab,
                PixelSize::new(width as u32, height as u32),
            ))
        });
        let mut events = Vec::new();
        if self.native_reported != fact.map(|(window, _, _)| window)
            && let Some(window) = self.native_reported.take()
        {
            events.push(CaptureEvent::NativeMoveEnded { window, at });
        }
        if let Some((window, grab, size)) = fact {
            self.native_reported = Some(window);
            events.push(CaptureEvent::NativeMove {
                window,
                grab,
                size,
                at,
            });
        }
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
            && self.last_lookup.is_none()
            && self.native_reported.is_none()
        {
            return Vec::new();
        }
        let mut events: Vec<_> = self
            .pressed
            .iter()
            .map(|&portal| {
                tracing::debug!(window = ?self.window.map(|w| w.window), portal = portal.0, reason = "gesture ended", "drag portal released");
                CaptureEvent::EdgeReleased { portal, at }
            })
            .collect();
        if let Some(window) = self.native_reported {
            events.push(CaptureEvent::NativeMoveEnded { window, at });
        }
        if self.moving {
            tracing::debug!(window = ?self.window.map(|w| w.window), latched = false, reason = "gesture ended", "drag detector state changed");
        }
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

#[cfg(test)]
mod wp267 {
    use super::*;
    use objc2_core_foundation::CGSize;

    fn portal() -> Portal {
        Portal {
            portal: crosspane_platform::CapturePortal {
                id: PortalId(1),
                display: crosspane_types::id::DisplayId(1),
                edge: Edge::Right,
                from: 40.0,
                to: 160.0,
            },
            display: super::super::Display {
                id: crosspane_types::id::DisplayId(1),
                bounds: CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(500.0, 400.0)),
                scale: 2.0,
            },
        }
    }

    fn fact(point: CGPoint) -> WindowFact {
        WindowFact {
            window: WindowId(42),
            pid: 7,
            frame: RectLogical::new(
                PointLogical::new(point.x - 80.0, point.y - 12.0),
                SizeLogical::new(200.0, 100.0),
            ),
            scale: 2.0,
        }
    }

    fn at(ms: u64) -> MonoTime {
        MonoTime::from_nanos(ms * 1_000_000)
    }

    fn pressed() -> Move {
        let mut drag = Move::default();
        for x in [100.0, 104.0, 499.0] {
            let pointer = CGPoint::new(x, 50.0);
            drag.sample(Some(fact(pointer)), pointer, &[portal()]);
        }
        assert!(matches!(
            drag.update(&[portal()], &[(PortalId(1), 0.5)], at(0))
                .as_slice(),
            [CaptureEvent::DragAtEdge {
                window: WindowId(42),
                ..
            }]
        ));
        drag
    }

    #[test]
    fn lookup_starts_far_from_edge_only_while_held_with_portals() {
        let drag = Move::default();
        let point = CGPoint::new(100.0, 50.0);
        assert!(!drag.should_lookup(false, &[portal()], point));
        assert!(!drag.should_lookup(true, &[], point));
        assert!(drag.should_lookup(true, &[portal()], point));
    }

    #[test]
    fn far_edge_latch_then_pinned_push_emits_steady_drag_at_edge() {
        let mut drag = pressed();
        for ms in [20, 40, 60, 80, 100, 120, 140, 160, 180, 200, 220, 240, 260] {
            let pointer = CGPoint::new(499.0, 50.0);
            drag.sample(Some(fact(pointer)), pointer, &[portal()]);
            assert!(matches!(
                drag.update(&[portal()], &[], at(ms)).as_slice(),
                [CaptureEvent::DragAtEdge { grab, position, .. }]
                    if *grab == PointDevice::new(160.0, 24.0) && *position == 0.5
            ));
            assert!(drag.at_edge(PortalId(1)).is_some());
        }
    }

    #[test]
    fn one_point_jitter_keeps_pressed_edge_without_outward_delta() {
        let mut drag = pressed();
        for (i, x) in [500.0, 499.5, 499.0, 500.5, 501.0].into_iter().enumerate() {
            let pointer = CGPoint::new(x, 50.0);
            drag.sample(Some(fact(pointer)), pointer, &[portal()]);
            assert!(matches!(
                drag.update(&[portal()], &[], at((i as u64 + 1) * 20))
                    .as_slice(),
                [CaptureEvent::DragAtEdge { .. }]
            ));
        }
    }

    #[test]
    fn leaving_edge_band_or_span_releases_once() {
        for pointer in [CGPoint::new(498.9, 50.0), CGPoint::new(499.0, 80.1)] {
            let mut drag = pressed();
            drag.sample(Some(fact(pointer)), pointer, &[portal()]);
            assert!(matches!(
                drag.update(&[portal()], &[], at(20)).as_slice(),
                [CaptureEvent::EdgeReleased {
                    portal: PortalId(1),
                    ..
                }]
            ));
            assert!(drag.update(&[portal()], &[], at(40)).is_empty());
        }
    }

    #[test]
    fn size_change_breaks_latch_even_below_one_point() {
        let mut drag = pressed();
        let pointer = CGPoint::new(499.0, 50.0);
        let mut resized = fact(pointer);
        resized.frame.size.width += 0.5;
        drag.sample(Some(resized), pointer, &[portal()]);
        assert!(matches!(
            drag.update(&[portal()], &[(PortalId(1), 0.5)], at(20))
                .as_slice(),
            [CaptureEvent::EdgeReleased { .. }]
        ));
        assert!(drag.at_edge(PortalId(1)).is_none());
    }

    #[test]
    fn identity_pid_and_grab_break_release_and_physical_up_clears() {
        let pointer = CGPoint::new(499.0, 50.0);
        for change in 0..3 {
            let mut drag = pressed();
            let mut changed = fact(pointer);
            match change {
                0 => changed.window = WindowId(43),
                1 => changed.pid = 8,
                _ => changed.frame.origin.y -= 1.01,
            }
            drag.sample(Some(changed), pointer, &[portal()]);
            assert!(matches!(
                drag.update(&[portal()], &[(PortalId(1), 0.5)], at(20))
                    .as_slice(),
                [CaptureEvent::EdgeReleased { .. }]
            ));
        }
        let mut drag = pressed();
        assert!(matches!(
            drag.clear(at(20)).as_slice(),
            [CaptureEvent::EdgeReleased { .. }]
        ));
        assert!(drag.clear(at(40)).is_empty());
        drag.sample(Some(fact(pointer)), pointer, &[portal()]);
        assert!(
            drag.update(&[portal()], &[(PortalId(1), 0.5)], at(60))
                .is_empty()
        );
    }

    #[test]
    fn ordinary_e1_still_requires_outward_delta_at_every_edge() {
        for (edge, point, dx, dy) in [
            (Edge::Left, CGPoint::new(0.0, 50.0), -1.0, 0.0),
            (Edge::Right, CGPoint::new(500.0, 50.0), 1.0, 0.0),
            (Edge::Top, CGPoint::new(50.0, 0.0), 0.0, -1.0),
            (Edge::Bottom, CGPoint::new(50.0, 400.0), 0.0, 1.0),
        ] {
            let mut portal = portal();
            portal.portal.edge = edge;
            assert_eq!(super::super::portal_hit(portal, point, dx, dy), Some(0.5));
            assert_eq!(super::super::portal_hit(portal, point, -dx, -dy), None);
            assert_eq!(super::super::portal_hit(portal, point, 0.0, 0.0), None);
        }
    }

    #[test]
    fn lookup_due_keeps_pressed_state_and_checks_current_pointer_between_reads() {
        let mut drag = pressed();
        assert!(drag.lookup_due(at(0)));
        for ms in [1, 19] {
            drag.pointer = CGPoint::new(499.5, 50.0);
            assert!(!drag.lookup_due(at(ms)));
            assert!(drag.update(&[portal()], &[], at(ms)).is_empty());
            assert!(drag.at_edge(PortalId(1)).is_some());
        }
        assert!(drag.lookup_due(at(20)));
        assert!(matches!(
            drag.update(&[portal()], &[], at(20)).as_slice(),
            [CaptureEvent::DragAtEdge { .. }]
        ));
        drag.pointer = CGPoint::new(498.9, 50.0);
        assert!(!drag.lookup_due(at(21)));
        assert!(matches!(
            drag.update(&[portal()], &[], at(21)).as_slice(),
            [CaptureEvent::EdgeReleased { .. }]
        ));
        drag.clear(at(22));
        assert!(
            drag.lookup_due(at(22)),
            "a new gesture starts a fresh lookup cadence"
        );
    }

    #[test]
    fn pinned_latch_without_initial_outward_hit_does_not_press() {
        let mut drag = Move::default();
        for x in [100.0, 104.0, 499.0, 499.0] {
            let pointer = CGPoint::new(x, 50.0);
            drag.sample(Some(fact(pointer)), pointer, &[portal()]);
        }
        assert!(drag.update(&[portal()], &[], at(20)).is_empty());
    }

    #[test]
    fn skipped_lookup_retains_coherent_grab_while_band_uses_current_pointer() {
        let mut drag = pressed();
        assert!(drag.lookup_due(at(10)));
        drag.pointer = CGPoint::new(500.0, 50.0);
        assert!(!drag.lookup_due(at(20)));
        assert!(matches!(
            drag.update(&[portal()], &[], at(20)).as_slice(),
            [CaptureEvent::DragAtEdge { grab, .. }] if *grab == PointDevice::new(160.0, 24.0)
        ));
    }

    #[test]
    fn drag_band_all_edges_respects_density_span_and_one_point_limit() {
        for (edge, point, normal) in [
            (Edge::Left, CGPoint::new(0.0, 50.0), CGPoint::new(1.0, 0.0)),
            (
                Edge::Right,
                CGPoint::new(500.0, 50.0),
                CGPoint::new(1.0, 0.0),
            ),
            (Edge::Top, CGPoint::new(50.0, 0.0), CGPoint::new(0.0, 1.0)),
            (
                Edge::Bottom,
                CGPoint::new(50.0, 400.0),
                CGPoint::new(0.0, 1.0),
            ),
        ] {
            let mut portal = portal();
            portal.portal.edge = edge;
            assert_eq!(super::super::drag_hit(portal, point), Some(0.5));
            assert_eq!(
                super::super::drag_hit(
                    portal,
                    CGPoint::new(point.x + normal.x, point.y + normal.y)
                ),
                Some(0.5)
            );
            assert_eq!(
                super::super::drag_hit(
                    portal,
                    CGPoint::new(point.x + 1.01 * normal.x, point.y + 1.01 * normal.y)
                ),
                None
            );
            let outside = if normal.x != 0.0 {
                CGPoint::new(point.x, 80.01)
            } else {
                CGPoint::new(80.01, point.y)
            };
            assert_eq!(super::super::drag_hit(portal, outside), None);
        }
    }
}
