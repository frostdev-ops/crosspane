//! Native-move samples; the Wayland worker alone publishes events and owns capture activation.
use super::{Abort, Source, backend, wayland::Monitor};
use crosspane_platform::{CaptureEvent, CapturePortal, Edge, IoGate, PlatformError, PortalId};
use crosspane_types::{
    geom::{PixelSize, PointDevice},
    id::{DisplayId, WindowId},
    time::MonoTime,
};
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
    display: Option<DisplayId>,
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
        let window = WindowId(super::super::windows::parse_hex(
            client["stableId"].as_str()?,
        )?);
        Some(Self {
            window,
            workspace: client["workspace"].clone(),
            display: client["monitor"]
                .as_u64()
                .and_then(|id| u32::try_from(id).ok())
                .map(DisplayId),
            origin,
            size,
            cursor,
            at: super::wayland::now(),
            received: Instant::now(),
        })
    }
    /// Device pixels from the client's own monitor, independently of any portal.
    pub fn native_geometry(&self, monitors: &[Monitor]) -> Option<(PointDevice, PixelSize)> {
        let display = self.display?;
        let mut matching = monitors.iter().filter(|m| m.id == display);
        let monitor = matching.next()?;
        if matching.next().is_some() || !monitor.scale.is_finite() || monitor.scale <= 0.0 {
            return None;
        }
        let size = self.size.map(|size| (size * monitor.scale).round());
        let grab = PointDevice::new(
            (self.cursor[0] - self.origin[0]) * monitor.scale,
            (self.cursor[1] - self.origin[1]) * monitor.scale,
        );
        (size
            .iter()
            .all(|size| size.is_finite() && *size >= 1.0 && *size <= f64::from(u32::MAX))
            && grab.x.is_finite()
            && grab.y.is_finite())
        .then_some((grab, PixelSize::new(size[0] as u32, size[1] as u32)))
    }
    fn moves_with(&self, previous: &Self, moving: bool) -> bool {
        self.same(previous)
            && (0..2).all(|i| {
                ((self.cursor[i] - self.origin[i]) - (previous.cursor[i] - previous.origin[i]))
                    .abs()
                    <= 1.0
            })
            && (moving || (0..2).any(|i| (self.cursor[i] - previous.cursor[i]).abs() >= 4.0))
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
fn period(
    sample: Option<&Sample>,
    portals: &[CapturePortal],
    monitors: &[Monitor],
    moving: bool,
) -> Duration {
    Duration::from_millis(
        if moving
            || sample.is_some_and(|s| {
                portals
                    .iter()
                    .any(|p| location(s, p, monitors, 48.0).is_some())
            })
        {
            20
        } else {
            100
        },
    )
}
/// Geometry facts have no first-miss tolerance. A stationary release is indistinguishable
/// from a held stationary move: only incoherence/loss ends this latch (DRAG-v0b ruling).
#[derive(Default)]
pub(super) struct NativeDetector {
    previous: Option<Sample>,
    moving: Option<WindowId>,
    published_sample: Option<MonoTime>,
    published_observation: Option<MonoTime>,
    reported_window: Option<WindowId>,
}
impl NativeDetector {
    pub fn observe(&mut self, sample: Option<&Sample>) -> (Option<WindowId>, bool) {
        let next = sample
            .zip(self.previous.as_ref())
            .filter(|(sample, previous)| sample.moves_with(previous, self.moving.is_some()))
            .map(|(sample, _)| sample.window);
        let ended = self.moving.filter(|window| Some(*window) != next);
        self.moving = next;
        self.previous = sample.cloned();
        (ended, next.is_some())
    }
    /// Memory-only selection shared by polling and the strip-enter publication fence.
    /// Every sample changes the latch, including a throttled repeat. Completed reads may
    /// be closer than their start times; queued samples must not burst on one observation.
    pub fn events(
        &mut self,
        sample: Option<&Sample>,
        monitors: &[Monitor],
        observed_at: MonoTime,
    ) -> [Option<CaptureEvent>; 2] {
        let geometry = sample.and_then(|sample| sample.native_geometry(monitors));
        let (ended, moving) = self.observe(sample.filter(|_| geometry.is_some()));
        // A geometric latch can restart while publication is throttled. Only a move
        // actually reported to the consumer owes an end; loss/re-latch/loss in one batch
        // must not produce a second end without an intervening published fact.
        let ended = ended
            .filter(|window| self.reported_window == Some(*window))
            .map(|window| {
                self.reported_window = None;
                CaptureEvent::NativeMoveEnded {
                    window,
                    at: observed_at,
                }
            });
        let due = |now: MonoTime, previous: Option<MonoTime>| {
            previous.is_none_or(|previous| {
                now.as_nanos()
                    .checked_sub(previous.as_nanos())
                    .is_some_and(|elapsed| elapsed >= 20_000_000)
            })
        };
        let fact = if moving
            && let (Some(sample), Some((grab, size))) = (sample, geometry)
            && due(sample.at, self.published_sample)
            && due(observed_at, self.published_observation)
        {
            // Keep these clocks across latch breaks: a new move cannot bypass the bound.
            self.published_sample = Some(sample.at);
            self.published_observation = Some(observed_at);
            self.reported_window = Some(sample.window);
            Some(CaptureEvent::NativeMove {
                window: sample.window,
                grab,
                size,
                at: sample.at,
            })
        } else {
            None
        };
        [ended, fact]
    }
}

const INJECTED_IDLE_WINDOW: Duration = Duration::from_secs(2);
fn injected_recent(at: Option<Instant>, now: Instant) -> bool {
    at.and_then(|at| now.checked_duration_since(at))
        .is_some_and(|age| age <= INJECTED_IDLE_WINDOW)
}

#[derive(Default)]
pub(super) struct Detector {
    previous: Option<Sample>,
    moving: bool,
    misses: u8,
    pub hit: Option<Hit>,
}
impl Detector {
    pub fn observe(
        &mut self,
        sample: Option<Sample>,
        portals: &[CapturePortal],
        monitors: &[Monitor],
    ) -> (Option<PortalId>, Option<Hit>) {
        if sample.is_none() {
            self.misses = self.misses.saturating_add(1);
            if self.misses == 1 {
                // Keep the gesture, but publish no observation with invented freshness.
                return (None, None);
            }
        } else {
            self.misses = 0;
        }
        self.moving = sample
            .as_ref()
            .zip(self.previous.as_ref())
            .is_some_and(|(s, p)| s.moves_with(p, self.moving));
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
                return (self.cancel_watch(), None);
            }
            return (None, None);
        }
        let result = self.detector.observe(sample, portals, monitors);
        // A tolerated miss keeps the cache; a real release or second miss clears it.
        self.last = self.detector.hit.clone();
        result
    }
    pub fn cancel_watch(&mut self) -> Option<PortalId> {
        let portal = self.watch.as_ref()?.hit.portal;
        *self = Self::default();
        Some(portal)
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
        position: f64,
        portals: &[CapturePortal],
        monitors: &[Monitor],
        now: Instant,
    ) -> (Option<PortalId>, Option<Hit>) {
        let mut released = None;
        for sample in pending {
            released = released.or(self.observe(sample, portals, monitors, now).0);
        }
        if self.watch.as_ref().is_some_and(|w| now >= w.until) {
            released = self.cancel_watch().or(released);
        }
        let hit = Watch::take(&mut self.watch, portal, now).map(|mut hit| {
            hit.position = position; // Validated strip entry is finer and newer than polled coordinates.
            hit
        });
        self.detector = Detector::default();
        self.last = None;
        (released, hit)
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
            // Two misses reset even a detector that tolerates one failed poll.
            self.pending.extend([None, None]);
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
        let injection_gate = gate.clone();
        Self::spawn_with_injection(
            move || {
                let ipc = source.ipc(Duration::from_millis(20)).ok()?;
                let cursor = ipc.json("cursorpos").ok()?;
                let client = ipc.json("activewindow").ok()?;
                Sample::parse(&cursor, &client)
            },
            move || gate.is_open() && abort.epoch.load(Ordering::Acquire) == epoch,
            move || {
                super::super::inject::injected_position_for(&injection_gate)
                    .and_then(|position| position.last())
                    .map(|injection| injection.at)
            },
        )
    }
    #[cfg(test)]
    fn spawn(
        read: impl FnMut() -> Option<Sample> + Send + 'static,
        allowed: impl Fn() -> bool + Send + 'static,
    ) -> Result<Self, PlatformError> {
        Self::spawn_with_injection(read, allowed, || None)
    }
    fn spawn_with_injection(
        mut read: impl FnMut() -> Option<Sample> + Send + 'static,
        allowed: impl Fn() -> bool + Send + 'static,
        injected_at: impl Fn() -> Option<Instant> + Send + 'static,
    ) -> Result<Self, PlatformError> {
        let shared = Arc::new((Mutex::new(Shared::default()), Condvar::new()));
        let worker = shared.clone();
        let thread = std::thread::Builder::new()
            .name("crosspane-drag".into())
            .spawn(move || {
                let (lock, wake) = &*worker;
                let mut due = Instant::now();
                let mut native = NativeDetector::default();
                loop {
                    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                    if state.stopped {
                        return;
                    }
                    if state.config.paused || !allowed() {
                        native = NativeDetector::default();
                        drop(wake.wait(state).unwrap_or_else(|e| e.into_inner()));
                        continue;
                    }
                    if state.config.portals.is_empty()
                        && !injected_recent(injected_at(), Instant::now())
                    {
                        // Only inspect process-local injection metadata while idle. No owner
                        // compositor IPC without a portal or admitted recent injected motion.
                        if native.moving.is_some() {
                            state.publish(None);
                        }
                        native = NativeDetector::default();
                        drop(
                            wake.wait_timeout(state, Duration::from_millis(100))
                                .unwrap_or_else(|e| e.into_inner()),
                        );
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
                    let (_, moving) = native.observe(sample.as_ref());
                    due = started
                        + period(
                            sample.as_ref(),
                            &state.config.portals,
                            &state.config.monitors,
                            moving,
                        );
                    if state.revision == revision
                        && !state.config.paused
                        && allowed()
                        && (!state.config.portals.is_empty()
                            || injected_recent(injected_at(), Instant::now()))
                    {
                        state.publish(sample);
                    } else {
                        if native.moving.is_some() && !state.config.paused && allowed() {
                            state.publish(None);
                        }
                        native = NativeDetector::default();
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
        Sample::parse(&cursor, &json!({"stableId":"1800000a", "address":"0x123", "at":[cursor["x"].as_f64().unwrap() - 80.0, cursor["y"].as_f64().unwrap() - 80.0], "size":size, "workspace":{"id":1,"name":"1"},"monitor":1,"floating":true})).unwrap()
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
    fn native_move_anywhere_uses_twenty_ms_and_client_monitor_pixels() {
        for scale in [1.0, 1.5] {
            let (_, monitors) = layout(scale);
            let mut native = NativeDetector::default();
            assert_eq!(
                native.observe(Some(&recorded(400.0, scale, false))),
                (None, false)
            );
            for n in 1..=10 {
                let mut sample = recorded(400.0 + f64::from(n) * 8.0, scale, false);
                sample.at = MonoTime::from_nanos(n as u64 * 20_000_000);
                assert_eq!(native.observe(Some(&sample)), (None, true));
                assert_eq!(
                    period(Some(&sample), &[], &monitors, true),
                    Duration::from_millis(20)
                );
                let (grab, size) = sample.native_geometry(&monitors).unwrap();
                assert_eq!(grab, PointDevice::new(80.0 * scale, 80.0 * scale));
                assert_eq!(
                    size,
                    PixelSize::new((360.0 * scale) as u32, (240.0 * scale) as u32)
                );
            }
        }
        let sample = recorded(400.0, 1.0, false);
        let (_, mut monitors) = layout(1.0);
        assert!(sample.native_geometry(&[]).is_none());
        monitors.push(monitors[0].clone());
        assert!(sample.native_geometry(&monitors).is_none());
    }

    #[test]
    fn native_move_ends_once_on_incoherence_none_or_changed_client_workspace_size() {
        for why in ["release motion", "none", "client", "workspace", "size"] {
            let mut native = NativeDetector::default();
            native.observe(Some(&recorded(400.0, 1.0, false)));
            let moving = recorded(408.0, 1.0, false);
            assert_eq!(native.observe(Some(&moving)), (None, true));
            let mut next = moving.clone();
            match why {
                // After release, the cursor moves while the window stays put: native fact,
                // not a button-release observation. Stationary release is tested separately.
                "release motion" => next.cursor[0] += 4.0,
                "client" => next.window = WindowId(2),
                "workspace" => next.workspace = json!({"id":2}),
                "size" => next.size[0] += 1.0,
                "none" => (),
                _ => unreachable!(),
            }
            let sample = (why != "none").then_some(&next);
            assert_eq!(
                native.observe(sample),
                (Some(moving.window), false),
                "{why}"
            );
            assert_eq!(native.observe(sample), (None, false), "{why}");
        }
    }

    #[test]
    fn native_stationary_release_is_unobservable_and_edge_first_miss_is_unchanged() {
        let (portals, monitors) = layout(1.0);
        let mut native = NativeDetector::default();
        let mut gesture = Gesture::default();
        for x in [1067.0, 1071.0] {
            let sample = recorded(x, 1.0, false);
            native.observe(Some(&sample));
            gesture.observe(Some(sample), &portals, &monitors, Instant::now());
        }
        // Geometry carries no button state. Both stationary held and released are identical.
        let stationary = recorded(1071.0, 1.0, false);
        assert_eq!(native.observe(Some(&stationary)), (None, true));
        let (released, hit) = gesture.observe(
            Some(stationary.clone()),
            &portals,
            &monitors,
            Instant::now(),
        );
        assert!(released.is_none());
        assert_eq!(hit.unwrap().sample.window, stationary.window);
        assert_eq!(native.observe(None), (Some(stationary.window), false));
        let (released, hit) = gesture.observe(None, &portals, &monitors, Instant::now());
        assert!(released.is_none() && hit.is_none());
        assert_eq!(
            gesture.detector.hit.as_ref().unwrap().sample.window,
            stationary.window
        );
        assert_eq!(
            gesture.observe(None, &portals, &monitors, Instant::now()).0,
            Some(portals[0].id)
        );
    }

    #[test]
    fn native_publication_throttles_completed_samples_and_queued_batch_replay() {
        let (_, monitors) = layout(1.0);
        let at = |ms: u64| MonoTime::from_nanos(ms * 1_000_000);
        let sample = |x, ms| {
            let mut sample = recorded(x, 1.0, false);
            sample.at = at(ms);
            sample
        };
        let mut native = NativeDetector::default();
        native.events(Some(&sample(400.0, 0)), &monitors, at(0));
        // Starts can be 20 ms apart yet a slow 30 ms read and a fast 1 ms read
        // complete at 30/31 ms. Neither sample nor emission freshness may be invented.
        let first = native.events(Some(&sample(408.0, 30)), &monitors, at(30));
        assert!(
            matches!(first[1], Some(CaptureEvent::NativeMove { at: timestamp, .. }) if timestamp == at(30))
        );
        assert_eq!(
            native.events(Some(&sample(416.0, 31)), &monitors, at(31)),
            [None, None]
        );
        assert!(native.moving.is_some());
        let next = native.events(Some(&sample(424.0, 51)), &monitors, at(51));
        assert!(
            matches!(next[1], Some(CaptureEvent::NativeMove { at: timestamp, .. }) if timestamp == at(51))
        );

        let mut native = NativeDetector::default();
        native.events(Some(&sample(400.0, 0)), &monitors, at(100));
        let first = native.events(Some(&sample(408.0, 20)), &monitors, at(100));
        assert!(
            matches!(first[1], Some(CaptureEvent::NativeMove { at: timestamp, .. }) if timestamp == at(20))
        );
        for (x, ms) in [(416.0, 40), (424.0, 60)] {
            assert_eq!(
                native.events(Some(&sample(x, ms)), &monitors, at(100)),
                [None, None]
            );
        }
        // Loss is immediate even in the same delivery batch, with exactly one end.
        assert!(matches!(
            native.events(None, &monitors, at(100))[0],
            Some(CaptureEvent::NativeMoveEnded { .. })
        ));
        assert_eq!(native.events(None, &monitors, at(100)), [None, None]);
        native.events(Some(&sample(432.0, 80)), &monitors, at(100));
        assert_eq!(
            native.events(Some(&sample(440.0, 100)), &monitors, at(100)),
            [None, None]
        );
        // Same-batch [loss, coherent A1, coherent A2, loss] owes just the first end:
        // the geometric re-latch never published a new move while still throttled.
        assert_eq!(native.events(None, &monitors, at(100)), [None, None]);
        assert!(native.reported_window.is_none());
        native.events(Some(&sample(448.0, 120)), &monitors, at(120));
        // A newer latch later publishes the untouched real sample time and owes one end.
        let next = native.events(Some(&sample(456.0, 140)), &monitors, at(140));
        assert!(
            matches!(next[1], Some(CaptureEvent::NativeMove { at: timestamp, .. }) if timestamp == at(140))
        );
        assert!(matches!(
            native.events(None, &monitors, at(140))[0],
            Some(CaptureEvent::NativeMoveEnded { .. })
        ));
        assert_eq!(native.events(None, &monitors, at(140)), [None, None]);
    }

    #[test]
    fn native_cancellation_between_poll_and_strip_enter_survives_the_memory_fence() {
        let (portals, monitors) = layout(1.0);
        let now = Instant::now();
        for missing in [false, true] {
            let mut native = NativeDetector::default();
            let first = recorded(1067.0, 1.0, false);
            let moving = recorded(1071.0, 1.0, false);
            native.events(Some(&first), &monitors, first.at);
            assert!(matches!(
                native.events(Some(&moving), &monitors, moving.at)[1],
                Some(CaptureEvent::NativeMove { .. })
            ));
            let hit = hit();
            let mut gesture = Gesture {
                watch: Some(Watch::new(hit.clone(), now)),
                last: Some(hit.clone()),
                ..Default::default()
            };
            let mut cancelled = moving.clone();
            cancelled.cursor[0] -= 4.0; // Window stays still: incoherent post-release motion.
            let poller = Poller::spawn(|| None, || false).unwrap();
            {
                let mut publication = poller.shared.0.lock().unwrap();
                publication.publish((!missing).then_some(cancelled));
                publication.publish(Some(moving.clone()));
            }
            // Same pending batch goes through both detectors under the memory-only fence;
            // event delivery happens later. The legacy strict watch cancellation still wins.
            let observed_at = moving.at;
            let (events, released, drop) = poller.fence(|pending| {
                let events: Vec<_> = pending
                    .iter()
                    .flat_map(|sample| native.events(sample.as_ref(), &monitors, observed_at))
                    .flatten()
                    .collect();
                let (released, drop) =
                    gesture.pressed(pending, PortalId(1), 0.5, &portals, &monitors, now);
                (events, released, drop)
            });
            assert_eq!(events.iter().filter(|event| matches!(event, CaptureEvent::NativeMoveEnded { window, .. } if *window == moving.window)).count(), 1);
            assert_eq!(released, Some(PortalId(1)));
            assert!(drop.is_none());
            assert!(gesture.watch.is_none());
            assert!(poller.take().is_empty());
        }
    }

    #[test]
    fn no_portal_polling_requires_injected_motion_within_two_seconds() {
        let now = Instant::now();
        assert!(!injected_recent(None, now));
        assert!(injected_recent(Some(now), now));
        assert!(injected_recent(Some(now - Duration::from_secs(2)), now));
        assert!(!injected_recent(
            Some(now - Duration::from_secs(2) - Duration::from_nanos(1)),
            now
        ));
        assert!(!injected_recent(Some(now + Duration::from_nanos(1)), now));
        let injected = Arc::new(Mutex::new(None));
        let observed = injected.clone();
        let (tx, rx) = mpsc::channel();
        let poller = Poller::spawn_with_injection(
            move || {
                tx.send(()).unwrap();
                Some(recorded(400.0, 1.0, false))
            },
            || true,
            move || *observed.lock().unwrap(),
        )
        .unwrap();
        let (_, monitors) = layout(1.0);
        poller.configure(vec![], monitors, false);
        // Process-local metadata inspection may wake, but no IPC/sample read is admitted.
        assert!(rx.recv_timeout(Duration::from_millis(120)).is_err());
        *injected.lock().unwrap() = Some(Instant::now());
        poller.shared.1.notify_all();
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        poller
            .pause(Instant::now() + Duration::from_secs(1))
            .unwrap();
    }

    #[test]
    fn detector_window_id_matches_window_source_for_the_same_client_json() {
        use super::super::super::{ipc::HyprIpc, windows::HyprlandWindows};
        use crosspane_platform::WindowSource;
        use std::{
            io::{Read, Write},
            os::unix::net::UnixListener,
        };

        struct Temp(std::path::PathBuf);
        impl Drop for Temp {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let dir = Temp(std::env::temp_dir().join(format!(
            "crosspane-wp255b-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
        std::fs::create_dir(&dir.0).unwrap();
        std::fs::create_dir_all(dir.0.join("hypr/fake")).unwrap();
        let client = json!({
            "stableId":"1800000a", "address":"0x60aba49bf930", "mapped":true,
            "hidden":false, "at":[200,100], "size":[360,240],
            "workspace":{"id":1,"name":"1"}, "monitor":0, "class":"crosspane-test",
            "initialClass":"crosspane-test", "title":"fixture", "pid":1234, "fullscreen":0
        });
        let sample = Sample::parse(&json!({"x":280,"y":180}), &client).unwrap();
        let reply = serde_json::to_vec(&json!([client])).unwrap();
        let listener = UnixListener::bind(dir.0.join("hypr/fake/.socket.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let deadline = Instant::now() + Duration::from_secs(1);
                let mut connection = loop {
                    match listener.accept() {
                        Ok((connection, _)) => break connection,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline);
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(e) => panic!("fake IPC accept: {e}"),
                    }
                };
                connection
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                connection
                    .set_write_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut request = [0; 9];
                connection.read_exact(&mut request).unwrap();
                assert_eq!(&request, b"j/clients");
                connection.write_all(&reply).unwrap();
            }
        });
        let source =
            HyprlandWindows::new(HyprIpc::new("fake", &dir.0, Duration::from_secs(1))).unwrap();
        let windows = source.windows().unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(sample.window, windows[0].id);
        assert_eq!(sample.window, WindowId(0x1800000a));
        assert_ne!(sample.window, WindowId(0x60aba49bf930));
        assert_eq!(
            source.address(sample.window).as_deref(),
            Some("0x60aba49bf930")
        );
        server.join().unwrap();
    }
    #[test]
    fn sample_requires_valid_stable_id_and_never_uses_address_for_identity() {
        let cursor = json!({"x":280,"y":180});
        let mut client = json!({
            "stableId":"1800000a", "address":"0x123", "at":[200,100],
            "size":[360,240], "workspace":{"id":1,"name":"1"}
        });
        for (stable_id, expected) in [
            ("1800000a", 0x1800000a),
            ("0x1800000a", 0x1800000a),
            ("0", 0),
            ("ffffffffffffffff", u64::MAX),
        ] {
            client["stableId"] = json!(stable_id);
            assert_eq!(
                Sample::parse(&cursor, &client).unwrap().window,
                WindowId(expected)
            );
        }
        for invalid in [
            Value::Null,
            json!(123),
            json!(""),
            json!("0x"),
            json!("not-hex"),
            json!("10000000000000000"),
        ] {
            client["stableId"] = invalid;
            assert!(Sample::parse(&cursor, &client).is_none());
        }
        client.as_object_mut().unwrap().remove("stableId");
        assert!(Sample::parse(&cursor, &client).is_none());
        client["stableId"] = json!("1800000a");
        client["address"] = json!("0x456");
        assert_eq!(
            Sample::parse(&cursor, &client).unwrap().window,
            WindowId(0x1800000a)
        );
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
                    assert_eq!(result.sample.window, WindowId(0x1800000a));
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
    fn one_missed_drag_sample_preserves_move_portal_and_real_timestamp() {
        let (portals, monitors) = layout(1.0);
        let now = Instant::now();
        let mut gesture = Gesture::default();
        gesture.observe(Some(recorded(1067.0, 1.0, false)), &portals, &monitors, now);
        let mut edge = recorded(1071.0, 1.0, false);
        edge.at = MonoTime::from_nanos(20_000_000);
        let observed = gesture
            .observe(Some(edge.clone()), &portals, &monitors, now)
            .1
            .unwrap();
        assert_eq!(observed.portal, PortalId(1));
        let (released, observation) = gesture.observe(None, &portals, &monitors, now);
        assert!(released.is_none());
        assert!(observation.is_none());
        assert!(gesture.detector.moving);
        assert_eq!(gesture.detector.hit.as_ref().unwrap().portal, PortalId(1));
        assert_eq!(gesture.last.as_ref().unwrap().sample.at, edge.at);
        assert_eq!(
            gesture.last.as_ref().unwrap().sample.received,
            edge.received
        );
        // A stationary coherent sample after the miss keeps the native move active.
        edge.at = MonoTime::from_nanos(60_000_000);
        let (released, observation) = gesture.observe(Some(edge), &portals, &monitors, now);
        assert!(released.is_none());
        assert_eq!(
            observation.unwrap().sample.at,
            MonoTime::from_nanos(60_000_000)
        );
        assert_eq!(gesture.detector.misses, 0);
        // A new isolated miss is tolerated too, rather than accumulating old failures.
        assert!(gesture.observe(None, &portals, &monitors, now).0.is_none());
        assert!(gesture.detector.hit.is_some());
    }

    #[test]
    fn two_missed_drag_samples_or_changed_identity_reset_and_overflow_still_cancels() {
        let (portals, monitors) = layout(1.0);
        for why in ["two misses", "window", "workspace", "size", "overflow"] {
            let mut detector = Detector::default();
            detector.observe(Some(recorded(1067.0, 1.0, false)), &portals, &monitors);
            let edge = recorded(1071.0, 1.0, false);
            assert!(
                detector
                    .observe(Some(edge.clone()), &portals, &monitors)
                    .1
                    .is_some()
            );
            assert!(detector.observe(None, &portals, &monitors).0.is_none());
            let released = if why == "overflow" {
                // Reset the miss count so overflow must supply its own cancellation fence.
                assert!(
                    detector
                        .observe(Some(edge.clone()), &portals, &monitors)
                        .1
                        .is_some()
                );
                let mut publication = Shared::default();
                for _ in 0..9 {
                    publication.publish(Some(edge.clone()));
                }
                assert!(publication.pending.len() <= 8);
                // Overflow starts with two ordered misses, even if a valid sample follows.
                assert!(publication.pending[0].is_none());
                assert!(publication.pending[1].is_none());
                publication
                    .pending
                    .into_iter()
                    .fold(None, |released, sample| {
                        released.or(detector.observe(sample, &portals, &monitors).0)
                    })
            } else {
                let mut changed = edge.clone();
                match why {
                    "two misses" => (),
                    "window" => changed.window = WindowId(0x456),
                    "workspace" => changed.workspace = json!({"id":2}),
                    "size" => changed.size[0] += 1.0,
                    _ => unreachable!(),
                }
                detector
                    .observe(
                        (why != "two misses").then_some(changed),
                        &portals,
                        &monitors,
                    )
                    .0
            };
            assert_eq!(released, Some(PortalId(1)), "{why}");
            assert!(!detector.moving, "{why}");
            assert!(detector.hit.is_none(), "{why}");
            assert!(detector.observe(None, &portals, &monitors).0.is_none());
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
            period(Some(&sample), &portals, &monitors, false),
            Duration::from_millis(20)
        );
        assert_eq!(
            period(
                Some(&recorded(900.0, 1.0, false)),
                &portals,
                &monitors,
                false
            ),
            Duration::from_millis(100)
        );
        assert_eq!(
            period(Some(&sample), &[], &monitors, false),
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
            let (released, drop) = poller.fence(|pending| {
                gesture.pressed(pending, PortalId(1), 0.5, &portals, &monitors, now)
            });
            assert_eq!(released, Some(PortalId(1)));
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
                let (released, drop) = poller.fence(|pending| {
                    gesture.pressed(pending, PortalId(1), 0.5, &portals, &monitors, now)
                });
                assert_eq!(released, Some(PortalId(1)));
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
                .fence(|pending| gesture.pressed(
                    pending,
                    PortalId(1),
                    0.5,
                    &portals,
                    &monitors,
                    now
                ))
                .1
                .is_some()
        );
        assert!(
            poller
                .fence(|pending| gesture.pressed(
                    pending,
                    PortalId(1),
                    0.5,
                    &portals,
                    &monitors,
                    now
                ))
                .1
                .is_none()
        );
    }
    #[test]
    fn fence_preserves_cancelled_watch_portal_before_a_later_gesture_release() {
        let (mut portals, monitors) = layout(1.0);
        let mut other = portals[0];
        other.id = PortalId(2);
        other.from = 0.0;
        other.to = 200.0;
        portals.push(other);
        let now = Instant::now();
        let original = hit();
        let mut gesture = Gesture {
            watch: Some(Watch::new(original, now)),
            ..Default::default()
        };
        let poller = Poller::spawn(|| None, || false).unwrap();
        {
            let mut publication = poller.shared.0.lock().unwrap();
            publication.publish(Some(recorded(1067.0, 1.0, false))); // Cancel portal 1's watch.
            for x in [1067.0, 1071.0, 1067.0] {
                let mut sample = recorded(x, 1.0, false);
                sample.cursor[1] = monitors[0].origin[1] + 100.0;
                sample.origin[1] = sample.cursor[1] - 80.0;
                publication.publish(Some(sample)); // Enter then leave portal 2 in the same batch.
            }
        }
        let (released, drop) = poller
            .fence(|pending| gesture.pressed(pending, PortalId(1), 0.5, &portals, &monitors, now));
        assert_eq!(released, Some(PortalId(1)));
        assert!(drop.is_none());
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
            // An unarmed gesture tolerates one missed poll; two misses release it.
            gesture.observe(None, &portals, &monitors, now + Duration::from_millis(2));
            gesture.refuse(
                PortalId(1),
                crosspane_types::hid::MouseButton::PRIMARY,
                now + Duration::from_millis(3),
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
        let (released, drop) =
            gesture.pressed(Vec::new(), PortalId(1), 0.5, &portals, &monitors, until);
        assert_eq!(released, Some(PortalId(1)));
        assert!(drop.is_none());
        assert!(gesture.watch.is_none());
        gesture.refuse(
            PortalId(1),
            crosspane_types::hid::MouseButton::PRIMARY,
            until,
        );
        assert!(gesture.watch.is_none());
    }
    #[test]
    fn watch_cancellation_releases_portal_and_allows_immediate_retry_drop() {
        let (portals, monitors) = layout(1.0);
        for why in ["leave", "workspace", "client", "failed", "timeout"] {
            let now = Instant::now();
            let original = hit();
            let mut gesture = Gesture {
                watch: Some(Watch::new(original.clone(), now)),
                last: Some(original.clone()),
                ..Default::default()
            };
            let mut sample = original.sample.clone();
            match why {
                "leave" => sample.cursor[0] -= 4.0,
                "workspace" => sample.workspace = json!({"id":2}),
                "client" => sample.window = WindowId(0x456),
                "failed" | "timeout" => (),
                _ => unreachable!(),
            }
            let cancelled_at = now
                + if why == "timeout" {
                    Duration::from_secs(10)
                } else {
                    Duration::from_millis(1)
                };
            let (released, hit) = gesture.observe(
                (why != "failed").then_some(sample),
                &portals,
                &monitors,
                cancelled_at,
            );
            assert_eq!(released, Some(PortalId(1)), "{why}");
            assert!(hit.is_none());
            assert_eq!(gesture.cancel_watch(), None); // No duplicate release after consumption.
            let retry_at = cancelled_at + Duration::from_millis(1);
            for x in [1067.0, 1071.0] {
                let mut sample = recorded(x, 1.0, false);
                sample.received = retry_at;
                gesture.observe(Some(sample), &portals, &monitors, retry_at);
            }
            gesture.refuse(
                PortalId(1),
                crosspane_types::hid::MouseButton::PRIMARY,
                retry_at,
            );
            assert!(gesture.watch.is_some(), "{why}: retry did not arm");
            let (released, drop) =
                gesture.pressed(Vec::new(), PortalId(1), 0.75, &portals, &monitors, retry_at);
            assert!(released.is_none());
            let drop = drop.unwrap();
            assert_eq!(drop.position, 0.75);
            assert_eq!(drop.grab, original.grab);
            assert!(gesture.watch.is_none());
        }
    }
    #[test]
    fn drop_reports_current_validated_release_position_and_preserves_original_grab() {
        let (portals, monitors) = layout(1.0);
        let now = Instant::now();
        let original = hit();
        let mut gesture = Gesture {
            watch: Some(Watch::new(original.clone(), now)),
            ..Default::default()
        };
        let mut moved = original.sample.clone();
        moved.cursor[1] =
            monitors[0].origin[1] + portals[0].from + 0.8 * (portals[0].to - portals[0].from);
        moved.origin[1] = moved.cursor[1] - original.grab.y;
        let (released, drop) = gesture.pressed(
            vec![Some(moved)],
            PortalId(1),
            0.8,
            &portals,
            &monitors,
            now,
        );
        assert!(released.is_none());
        let drop = drop.unwrap();
        assert_ne!(drop.position, original.position);
        assert_eq!(drop.position, 0.8);
        assert_eq!(drop.grab, original.grab);
        assert_eq!(drop.sample.window, original.sample.window);
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
