//! Native-move samples; the Wayland worker alone publishes events and owns capture activation.
use super::{Abort, Source, backend, wayland::Monitor};
use crosspane_platform::{CapturePortal, Edge, IoGate, PlatformError, PortalId};
use crosspane_types::{geom::PointDevice, id::WindowId, time::MonoTime};
use serde_json::Value;
use std::{
    sync::{Arc, Condvar, Mutex, atomic::Ordering},
    thread::JoinHandle,
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub(super) struct Sample {
    pub window: WindowId,
    pub workspace: Value,
    origin: [f64; 2],
    size: [f64; 2],
    pub cursor: [f64; 2],
    pub at: MonoTime,
    pub received: Instant,
}
impl Sample {
    pub fn parse(cursor: &Value, client: &Value) -> Option<Self> {
        let pair = |v: &Value| -> Option<[f64; 2]> { Some([v[0].as_f64()?, v[1].as_f64()?]) };
        let origin = pair(&client["at"])?;
        let size = pair(&client["size"])?;
        let cursor = [cursor["x"].as_f64()?, cursor["y"].as_f64()?];
        if origin
            .into_iter()
            .chain(size)
            .chain(cursor)
            .any(|x| !x.is_finite())
            || size.iter().any(|x| *x <= 0.0)
            || client["workspace"].is_null()
        {
            return None;
        }
        let window = WindowId(
            u64::from_str_radix(client["address"].as_str()?.strip_prefix("0x")?, 16).ok()?,
        );
        (window.0 != 0).then(|| Self {
            window,
            workspace: client["workspace"].clone(),
            origin,
            size,
            cursor,
            at: super::wayland::now(),
            received: Instant::now(),
        })
    }
    fn same(&self, other: &Self) -> bool {
        self.window == other.window && self.workspace == other.workspace && self.size == other.size
    }
}
#[derive(Clone, Debug)]
pub(super) struct Hit {
    pub portal: PortalId,
    pub position: f64,
    pub grab: PointDevice,
    pub sample: Sample,
}
fn location(
    sample: &Sample,
    portal: &CapturePortal,
    monitors: &[Monitor],
    tolerance: f64,
) -> Option<(f64, f64)> {
    let m = monitors.iter().find(|m| m.id == portal.display)?;
    let [x, y] = [
        sample.cursor[0] - m.origin[0],
        sample.cursor[1] - m.origin[1],
    ];
    let (distance, along) = match portal.edge {
        Edge::Left => (x.abs(), y),
        Edge::Right => ((x - f64::from(m.width - 1) / m.scale).abs(), y),
        Edge::Top => (y.abs(), x),
        Edge::Bottom => ((y - f64::from(m.height - 1) / m.scale).abs(), x),
    };
    let position = (along * m.scale - portal.from) / (portal.to - portal.from);
    let padding = if tolerance > 1.0 {
        tolerance * m.scale / (portal.to - portal.from)
    } else {
        0.0
    };
    (distance <= tolerance && (-padding..=1.0 + padding).contains(&position))
        .then_some((position, m.scale))
}
fn period(sample: Option<&Sample>, portals: &[CapturePortal], monitors: &[Monitor]) -> Duration {
    Duration::from_millis(
        if sample.is_some_and(|s| {
            portals
                .iter()
                .any(|p| location(s, p, monitors, 48.0).is_some())
        }) {
            20
        } else {
            100
        },
    )
}
#[derive(Default)]
pub(super) struct Detector {
    previous: Option<Sample>,
    moving: bool,
    pub hit: Option<Hit>,
}
impl Detector {
    pub fn observe(
        &mut self,
        sample: Option<Sample>,
        portals: &[CapturePortal],
        monitors: &[Monitor],
    ) -> (Option<PortalId>, Option<Hit>) {
        self.moving = sample
            .as_ref()
            .zip(self.previous.as_ref())
            .is_some_and(|(s, p)| {
                let coherent = s.same(p)
                    && (0..2).all(|i| {
                        ((s.cursor[i] - s.origin[i]) - (p.cursor[i] - p.origin[i])).abs() <= 1.0
                    });
                coherent
                    && (self.moving || (0..2).any(|i| (s.cursor[i] - p.cursor[i]).abs() >= 4.0))
            });
        let next = sample.as_ref().filter(|_| self.moving).and_then(|s| {
            portals.iter().find_map(|p| {
                let (position, scale) = location(s, p, monitors, 1.0)?;
                Some(Hit {
                    portal: p.id,
                    position,
                    grab: PointDevice::new(
                        (s.cursor[0] - s.origin[0]) * scale,
                        (s.cursor[1] - s.origin[1]) * scale,
                    ),
                    sample: s.clone(),
                })
            })
        });
        let released = self
            .hit
            .as_ref()
            .map(|h| h.portal)
            .filter(|p| next.as_ref().map(|h| h.portal) != Some(*p));
        self.previous = sample;
        self.hit = next.clone();
        (released, next)
    }
}
#[derive(Default)]
pub(super) struct Gesture {
    detector: Detector,
    last: Option<Hit>,
    pub watch: Option<Watch>,
}
impl Gesture {
    pub fn observe(
        &mut self,
        sample: Option<Sample>,
        portals: &[CapturePortal],
        monitors: &[Monitor],
        now: Instant,
    ) -> (Option<PortalId>, Option<Hit>) {
        if let Some(watch) = &self.watch {
            if !watch.valid(sample.as_ref(), portals, monitors, now) {
                *self = Self::default();
            }
            return (None, None);
        }
        let result = self.detector.observe(sample, portals, monitors);
        self.last = result.1.clone(); // Release/cancel invalidates the cached gesture immediately.
        result
    }
    pub fn refuse(
        &mut self,
        portal: PortalId,
        button: crosspane_types::hid::MouseButton,
        now: Instant,
    ) {
        if self.watch.is_some() {
            return; // Duplicate refusals preserve both the ten-second bound and 20 Hz cadence.
        }
        let hit = self.last.clone().filter(|h| {
            button == crosspane_types::hid::MouseButton::PRIMARY
                && h.portal == portal
                && now.saturating_duration_since(h.sample.received) <= Duration::from_millis(250)
                && self.detector.hit.as_ref().is_some_and(|current| {
                    current.portal == portal && current.sample.window == h.sample.window
                })
        });
        let _ = refuse(self, hit, now, |s, watch| s.watch = Some(watch));
    }
    pub fn pressed(
        &mut self,
        pending: Vec<Option<Sample>>,
        portal: PortalId,
        portals: &[CapturePortal],
        monitors: &[Monitor],
        now: Instant,
    ) -> Option<Hit> {
        for sample in pending {
            self.observe(sample, portals, monitors, now);
        }
        let hit = Watch::take(&mut self.watch, portal, now);
        self.detector = Detector::default();
        self.last = None;
        hit
    }
}
pub(super) struct Watch {
    pub hit: Hit,
    until: Instant,
    next_nudge: Instant,
}
/// Refusal is memory-only; the watch performs no native stop or capture activation.
pub(super) fn refuse<T>(
    seat: &mut T,
    hit: Option<Hit>,
    now: Instant,
    arm: impl FnOnce(&mut T, Watch),
) -> Result<(), PlatformError> {
    if let Some(hit) = hit {
        arm(seat, Watch::new(hit, now));
    }
    Err(PlatformError::PointerButtonHeld)
}
impl Watch {
    pub fn new(hit: Hit, now: Instant) -> Self {
        Self {
            hit,
            until: now + Duration::from_secs(10),
            next_nudge: now,
        }
    }
    pub fn valid(
        &self,
        sample: Option<&Sample>,
        portals: &[CapturePortal],
        monitors: &[Monitor],
        now: Instant,
    ) -> bool {
        now < self.until
            && sample.is_some_and(|s| {
                s.window == self.hit.sample.window
                    && s.workspace == self.hit.sample.workspace
                    && portals
                        .iter()
                        .any(|p| p.id == self.hit.portal && location(s, p, monitors, 1.0).is_some())
            })
    }
    pub fn nudge(&mut self, now: Instant) -> bool {
        if now < self.next_nudge || now >= self.until {
            return false;
        }
        self.next_nudge = now + Duration::from_millis(50);
        true
    }
    pub fn take(slot: &mut Option<Self>, portal: PortalId, now: Instant) -> Option<Hit> {
        slot.as_ref()
            .filter(|w| now < w.until && w.hit.portal == portal)?;
        slot.take().map(|w| w.hit)
    }
}

#[derive(Default, PartialEq)]
struct Config {
    portals: Vec<CapturePortal>,
    monitors: Vec<Monitor>,
    paused: bool,
}
#[derive(Default)]
struct Shared {
    config: Config,
    revision: u64,
    pending: Vec<Option<Sample>>,
    busy: bool,
    stopped: bool,
}
impl Shared {
    fn publish(&mut self, sample: Option<Sample>) {
        // Preserve ordered cancellation until consumed; overflow conservatively cancels too.
        if self.pending.len() >= 8 {
            self.pending.clear();
            self.pending.push(None);
        }
        self.pending.push(sample);
    }
}
pub(super) struct Poller {
    shared: Arc<(Mutex<Shared>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}
impl Poller {
    pub fn new(
        source: Source,
        gate: Arc<IoGate>,
        abort: Arc<Abort>,
        epoch: u64,
    ) -> Result<Self, PlatformError> {
        Self::spawn(
            move || {
                let ipc = source.ipc(Duration::from_millis(20)).ok()?;
                let cursor = ipc.json("cursorpos").ok()?;
                let client = ipc.json("activewindow").ok()?;
                Sample::parse(&cursor, &client)
            },
            move || gate.is_open() && abort.epoch.load(Ordering::Acquire) == epoch,
        )
    }
    fn spawn(
        mut read: impl FnMut() -> Option<Sample> + Send + 'static,
        allowed: impl Fn() -> bool + Send + 'static,
    ) -> Result<Self, PlatformError> {
        let shared = Arc::new((Mutex::new(Shared::default()), Condvar::new()));
        let worker = shared.clone();
        let thread = std::thread::Builder::new()
            .name("crosspane-drag".into())
            .spawn(move || {
                let (lock, wake) = &*worker;
                let mut due = Instant::now();
                loop {
                    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                    if state.stopped {
                        return;
                    }
                    if state.config.paused || state.config.portals.is_empty() || !allowed() {
                        drop(wake.wait(state).unwrap_or_else(|e| e.into_inner()));
                        continue;
                    }
                    if Instant::now() < due {
                        drop(
                            wake.wait_timeout(state, due.saturating_duration_since(Instant::now()))
                                .unwrap_or_else(|e| e.into_inner()),
                        );
                        continue;
                    }
                    let revision = state.revision;
                    let started = Instant::now();
                    state.busy = true;
                    drop(state);
                    let sample = read(); // Fresh bounded requests, never while holding the shared mutex.
                    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                    state.busy = false;
                    due = started
                        + period(
                            sample.as_ref(),
                            &state.config.portals,
                            &state.config.monitors,
                        );
                    if state.revision == revision && !state.config.paused && allowed() {
                        state.publish(sample);
                    }
                    wake.notify_all();
                }
            })
            .map_err(backend)?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }
    pub fn configure(&self, portals: Vec<CapturePortal>, monitors: Vec<Monitor>, paused: bool) {
        let (lock, wake) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        let config = Config {
            portals,
            monitors,
            paused,
        };
        if state.config != config {
            state.config = config;
            state.revision = state.revision.wrapping_add(1);
            state.pending.clear();
            wake.notify_all();
        }
    }
    /// Quiesce an in-flight pair before lock activation; its two requests each have a 20 ms bound.
    pub fn pause(&self, deadline: Instant) -> Result<(), PlatformError> {
        let (lock, wake) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        state.config.paused = true;
        state.revision = state.revision.wrapping_add(1);
        state.pending.clear();
        wake.notify_all();
        while state.busy {
            if Instant::now() >= deadline {
                return Err(PlatformError::Timeout);
            }
            state = wake
                .wait_timeout(state, deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        Ok(())
    }
    pub fn take(&self) -> Vec<Option<Sample>> {
        self.fence(|sample| sample)
    }
    /// Select a drop under the publication lock: cancellation already published cannot pass it.
    /// The closure is memory-only; never hold this lock across IPC or event delivery.
    pub fn fence<T>(&self, select: impl FnOnce(Vec<Option<Sample>>) -> T) -> T {
        let mut state = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        select(std::mem::take(&mut state.pending))
    }
}
impl Drop for Poller {
    fn drop(&mut self) {
        self.shared
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stopped = true;
        self.shared.1.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosspane_types::id::DisplayId;
    use serde_json::json;
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize},
        mpsc,
    };

    fn layout(scale: f64) -> (Vec<CapturePortal>, Vec<Monitor>) {
        (
            vec![CapturePortal {
                id: PortalId(1),
                display: DisplayId(1),
                edge: Edge::Right,
                from: 937.0 / 4.0,
                to: 937.0 * 3.0 / 4.0,
            }],
            vec![Monitor {
                id: DisplayId(1),
                name: "P8a".into(),
                width: 1072,
                height: 937,
                scale,
                origin: [-200.0, 100.0],
            }],
        )
    }
    fn recorded(x: f64, scale: f64, tiled: bool) -> Sample {
        let cursor = json!({"x": -200.0 + x / scale, "y": 100.0 + 468.0 / scale});
        let size = if tiled { [516, 898] } else { [360, 240] };
        Sample::parse(&cursor, &json!({"address":"0x123", "at":[cursor["x"].as_f64().unwrap() - 80.0, cursor["y"].as_f64().unwrap() - 80.0], "size":size, "workspace":{"id":1,"name":"1"},"floating":true})).unwrap()
    }
    fn hit() -> Hit {
        let (portals, monitors) = layout(1.0);
        let mut detector = Detector::default();
        detector.observe(Some(recorded(1067.0, 1.0, false)), &portals, &monitors);
        detector
            .observe(Some(recorded(1071.0, 1.0, false)), &portals, &monitors)
            .1
            .unwrap()
    }
    #[test]
    fn recorded_bindm_and_xdg_float_and_tile_moves_track_and_keep_pressing() {
        for path in ["bindm", "xdg"] {
            for tiled in [false, true] {
                for scale in [1.0, 1.5] {
                    let (portals, monitors) = layout(scale);
                    let mut detector = Detector::default();
                    // A tile changes size as the native move starts: that transition is reset.
                    let mut old = recorded(1063.0, scale, tiled);
                    old.size = if tiled { [509.0, 895.0] } else { old.size };
                    assert!(detector.observe(Some(old), &portals, &monitors).1.is_none());
                    let first = recorded(1063.0, scale, tiled);
                    assert!(
                        detector
                            .observe(Some(first), &portals, &monitors)
                            .1
                            .is_none()
                    );
                    let edge = recorded(1071.0, scale, tiled);
                    let result = detector
                        .observe(Some(edge.clone()), &portals, &monitors)
                        .1
                        .unwrap_or_else(|| panic!("{path}/{tiled}/{scale}"));
                    assert_eq!(result.sample.window, WindowId(0x123));
                    assert_eq!(result.grab, PointDevice::new(80.0 * scale, 80.0 * scale));
                    assert!((result.position - (468.0 - 937.0 / 4.0) / (937.0 / 2.0)).abs() < 1e-9);
                    assert!(
                        detector
                            .observe(Some(edge), &portals, &monitors)
                            .1
                            .is_some()
                    );
                    let left = recorded(1067.0, scale, tiled);
                    assert_eq!(
                        detector.observe(Some(left), &portals, &monitors).0,
                        Some(PortalId(1))
                    );
                    assert_eq!(detector.observe(None, &portals, &monitors).0, None);
                }
            }
        }
    }
    #[test]
    fn recorded_plain_pointer_keyboard_move_resize_workspace_and_client_reset() {
        let (portals, monitors) = layout(1.0);
        for control in ["plain", "keyboard", "resize", "workspace", "client"] {
            let first = recorded(1067.0, 1.0, false);
            let mut next = recorded(1071.0, 1.0, false);
            match control {
                "plain" => next.origin = first.origin,
                "keyboard" => next.cursor = first.cursor,
                "resize" => next.size[0] += 4.0,
                "workspace" => next.workspace = json!({"id":2,"name":"2"}),
                "client" => next.window = WindowId(0x456),
                _ => unreachable!(),
            }
            let mut detector = Detector::default();
            detector.observe(Some(first), &portals, &monitors);
            assert!(
                detector
                    .observe(Some(next), &portals, &monitors)
                    .1
                    .is_none(),
                "{control}"
            );
        }
        let sample = recorded(1071.0, 1.0, false);
        assert!(Sample::parse(&json!({"x":0,"y":0}), &json!({})).is_none());
        assert_eq!(
            period(Some(&sample), &portals, &monitors),
            Duration::from_millis(20)
        );
        assert_eq!(
            period(Some(&recorded(900.0, 1.0, false)), &portals, &monitors),
            Duration::from_millis(100)
        );
        assert_eq!(
            period(Some(&sample), &[], &monitors),
            Duration::from_millis(100)
        );
    }
    #[test]
    fn drop_watch_once_leave_workspace_client_timeout_and_twenty_hz_nudges() {
        let (portals, monitors) = layout(1.0);
        let now = Instant::now();
        let hit = hit();
        let mut watch = Watch::new(hit.clone(), now);
        assert!(watch.nudge(now));
        assert!(!watch.nudge(now + Duration::from_millis(49)));
        assert!(watch.nudge(now + Duration::from_millis(50)));
        let mut resized = hit.sample.clone();
        resized.size = [509.0, 895.0]; // Ending a tiled drag changes its layout, not the watch identity.
        assert!(watch.valid(Some(&resized), &portals, &monitors, now));
        assert!(!watch.valid(
            Some(&hit.sample),
            &portals,
            &monitors,
            now + Duration::from_secs(10)
        ));
        assert!(!watch.nudge(now + Duration::from_secs(10)));
        for why in ["leave", "workspace", "client"] {
            let mut sample = hit.sample.clone();
            match why {
                "leave" => sample.cursor[0] -= 4.0,
                "workspace" => sample.workspace = json!({"id":2}),
                "client" => sample.window = WindowId(0x456),
                _ => unreachable!(),
            }
            assert!(
                !watch.valid(Some(&sample), &portals, &monitors, now),
                "{why}"
            );
        }
        let mut slot = Some(watch);
        assert!(Watch::take(&mut slot, PortalId(2), now).is_none());
        assert!(Watch::take(&mut slot, PortalId(1), now).is_some());
        assert!(Watch::take(&mut slot, PortalId(1), now).is_none());
        assert!(
            Watch::take(
                &mut Some(Watch::new(hit, now)),
                PortalId(1),
                now + Duration::from_secs(10)
            )
            .is_none()
        );
    }
    #[test]
    fn a_ready_cancellation_fences_a_strip_enter_in_the_same_pump() {
        let (portals, monitors) = layout(1.0);
        for why in ["leave", "workspace", "client", "failed"] {
            let now = Instant::now();
            let hit = hit();
            let mut gesture = Gesture {
                watch: Some(Watch::new(hit.clone(), now)),
                last: Some(hit.clone()),
                ..Default::default()
            };
            let mut sample = hit.sample.clone();
            match why {
                "leave" => sample.cursor[0] -= 4.0,
                "workspace" => sample.workspace = json!({"id":2}),
                "client" => sample.window = WindowId(0x456),
                "failed" => (),
                _ => unreachable!(),
            }
            let poller = Poller::spawn(|| None, || false).unwrap();
            // The pump's initial check saw a valid sample. A cancellation then arrives while
            // the queued enter is being dispatched, and must still win at drop selection.
            poller.shared.0.lock().unwrap().publish(Some(hit.sample));
            for sample in poller.take() {
                gesture.observe(sample, &portals, &monitors, now);
            }
            // Both are ready: a cancellation on the polling channel and a queued strip enter.
            poller
                .shared
                .0
                .lock()
                .unwrap()
                .publish((why != "failed").then_some(sample));
            let drop = poller
                .fence(|pending| gesture.pressed(pending, PortalId(1), &portals, &monitors, now));
            assert!(drop.is_none(), "{why} escaped the strip-event fence");
            assert!(gesture.last.is_none());
            assert!(gesture.watch.is_none());
        }
    }
    #[test]
    fn queued_cancellation_survives_valid_samples_and_overflow_before_strip_enter() {
        let (portals, monitors) = layout(1.0);
        for why in ["leave", "workspace", "client", "failed", "overflow"] {
            for valid_samples in [1, 16] {
                if why == "overflow" && valid_samples == 1 {
                    continue;
                }
                let now = Instant::now();
                let hit = hit();
                let mut gesture = Gesture {
                    watch: Some(Watch::new(hit.clone(), now)),
                    last: Some(hit.clone()),
                    ..Default::default()
                };
                let poller = Poller::spawn(|| None, || false).unwrap();
                let mut cancelled = hit.sample.clone();
                match why {
                    "leave" => cancelled.cursor[0] -= 4.0,
                    "workspace" => cancelled.workspace = json!({"id":2}),
                    "client" => cancelled.window = WindowId(0x456),
                    "failed" | "overflow" => (),
                    _ => unreachable!(),
                }
                {
                    let mut publication = poller.shared.0.lock().unwrap();
                    publication.publish((why != "failed").then_some(cancelled));
                    // The worker has not consumed the cancellation when polling sees a return.
                    for _ in 0..valid_samples {
                        publication.publish(Some(hit.sample.clone()));
                    }
                    assert!(publication.pending.len() <= 8);
                }
                let drop = poller.fence(|pending| {
                    gesture.pressed(pending, PortalId(1), &portals, &monitors, now)
                });
                assert!(drop.is_none(), "{why}/{valid_samples} lost cancellation");
                assert!(gesture.watch.is_none());
                assert!(gesture.last.is_none());
                assert!(poller.take().is_empty());
            }
        }
        // A bounded batch containing only valid observations preserves a genuine drop.
        let now = Instant::now();
        let hit = hit();
        let mut gesture = Gesture {
            watch: Some(Watch::new(hit.clone(), now)),
            ..Default::default()
        };
        let poller = Poller::spawn(|| None, || false).unwrap();
        {
            let mut publication = poller.shared.0.lock().unwrap();
            publication.publish(Some(hit.sample.clone()));
            publication.publish(Some(hit.sample));
        }
        assert!(
            poller
                .fence(|pending| gesture.pressed(pending, PortalId(1), &portals, &monitors, now))
                .is_some()
        );
        assert!(
            poller
                .fence(|pending| gesture.pressed(pending, PortalId(1), &portals, &monitors, now))
                .is_none()
        );
    }
    #[test]
    fn released_or_cancelled_hits_cannot_rearm_and_duplicates_keep_bounds() {
        let (portals, monitors) = layout(1.0);
        let now = Instant::now();
        let make = || {
            let mut gesture = Gesture::default();
            gesture.observe(Some(recorded(1067.0, 1.0, false)), &portals, &monitors, now);
            gesture.observe(Some(recorded(1071.0, 1.0, false)), &portals, &monitors, now);
            gesture
        };
        for armed in [false, true] {
            let mut gesture = make();
            if armed {
                gesture.refuse(PortalId(1), crosspane_types::hid::MouseButton::PRIMARY, now);
            }
            gesture.observe(None, &portals, &monitors, now + Duration::from_millis(1));
            gesture.refuse(
                PortalId(1),
                crosspane_types::hid::MouseButton::PRIMARY,
                now + Duration::from_millis(2),
            );
            assert!(gesture.last.is_none());
            assert!(gesture.watch.is_none());
        }
        let mut gesture = make();
        gesture.refuse(PortalId(1), crosspane_types::hid::MouseButton::PRIMARY, now);
        assert!(gesture.watch.as_mut().unwrap().nudge(now));
        let until = gesture.watch.as_ref().unwrap().until;
        for ms in 1..50 {
            let at = now + Duration::from_millis(ms);
            gesture.refuse(PortalId(1), crosspane_types::hid::MouseButton::PRIMARY, at);
            assert!(!gesture.watch.as_mut().unwrap().nudge(at));
            assert_eq!(gesture.watch.as_ref().unwrap().until, until);
        }
        assert!(
            gesture
                .watch
                .as_mut()
                .unwrap()
                .nudge(now + Duration::from_millis(50))
        );
        gesture.refuse(
            PortalId(1),
            crosspane_types::hid::MouseButton::PRIMARY,
            until - Duration::from_millis(1),
        );
        assert_eq!(gesture.watch.as_ref().unwrap().until, until);
        assert!(
            gesture
                .pressed(Vec::new(), PortalId(1), &portals, &monitors, until)
                .is_none()
        );
        gesture.refuse(
            PortalId(1),
            crosspane_types::hid::MouseButton::PRIMARY,
            until,
        );
        assert_eq!(gesture.watch.as_ref().unwrap().until, until);
    }
    #[test]
    fn fake_refusal_arms_watch_without_native_ipc_or_strip_activation() {
        #[derive(Default)]
        struct Seat {
            native_requests: Vec<&'static str>,
            locks: usize,
            watch: Option<Watch>,
        }
        for detected in [true, false] {
            let mut seat = Seat::default();
            let result = refuse(&mut seat, detected.then(hit), Instant::now(), |s, watch| {
                s.watch = Some(watch)
            });
            assert!(matches!(result, Err(PlatformError::PointerButtonHeld)));
            assert!(seat.native_requests.is_empty());
            assert_eq!(seat.locks, 0);
            assert_eq!(seat.watch.is_some(), detected);
        }
    }
    #[test]
    fn poller_has_no_empty_gate_closed_or_captured_reads_and_never_exceeds_fifty_hz() {
        let reads = Arc::new(Mutex::new(Vec::new()));
        let recording = reads.clone();
        let allowed = Arc::new(AtomicBool::new(true));
        let gate = allowed.clone();
        let poller = Poller::spawn(
            move || {
                recording.lock().unwrap().push(Instant::now());
                Some(recorded(1071.0, 1.0, false))
            },
            move || gate.load(Ordering::Acquire),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(30));
        assert!(reads.lock().unwrap().is_empty());
        let (portals, monitors) = layout(1.0);
        poller.configure(portals.clone(), monitors.clone(), false);
        let deadline = Instant::now() + Duration::from_secs(1);
        while reads.lock().unwrap().len() < 5 {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        poller.pause(deadline).unwrap();
        let times = reads.lock().unwrap().clone();
        assert!(
            times
                .windows(2)
                .all(|pair| pair[1].duration_since(pair[0]) >= Duration::from_millis(20))
        );
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(reads.lock().unwrap().len(), times.len());
        allowed.store(false, Ordering::Release);
        poller.configure(portals, monitors, false);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(reads.lock().unwrap().len(), times.len());
    }
    #[test]
    fn pause_drains_inflight_sample_and_discards_its_stale_publication() {
        let (entered, seen) = mpsc::channel();
        let (release, resumed) = mpsc::channel();
        let reads = Arc::new(AtomicUsize::new(0));
        let count = reads.clone();
        let poller = Poller::spawn(
            move || {
                count.fetch_add(1, Ordering::Relaxed);
                entered.send(()).unwrap();
                resumed.recv().unwrap();
                Some(recorded(1071.0, 1.0, false))
            },
            || true,
        )
        .unwrap();
        let (portals, monitors) = layout(1.0);
        poller.configure(portals, monitors, false);
        seen.recv_timeout(Duration::from_secs(1)).unwrap();
        std::thread::scope(|scope| {
            let (done, drained) = mpsc::channel();
            let waiting = &poller;
            scope.spawn(move || {
                waiting
                    .pause(Instant::now() + Duration::from_secs(1))
                    .unwrap();
                done.send(()).unwrap();
            });
            assert!(drained.recv_timeout(Duration::from_millis(20)).is_err());
            release.send(()).unwrap();
            drained.recv_timeout(Duration::from_secs(1)).unwrap();
        });
        assert!(poller.take().is_empty());
        assert_eq!(reads.load(Ordering::Relaxed), 1);
    }
}
