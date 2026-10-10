//! Fakes for the twin's tests (WP-G2.4 B2): a Mutter that lays virtual monitors out the way the
//! real one does (linearly, dropping rotation), a Shell bridge's layout and fence calls, and a
//! virtual screen whose creation, resizing and loss the tests drive. One [`World`] holds the state
//! all three share and a log of the calls that matter for ordering.
//!
//! It is not Mutter: the real placement, the relayout transient and its effect on windows are live
//! checks.

#![allow(clippy::unwrap_used)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crosspane_platform::{
    CaptureTarget, EventSink, Frame, FrameCapture, FrameEvent, PlatformError, StreamEndReason,
    StreamId,
};
use crosspane_types::geom::{PixelRect, PixelSize};
use crosspane_types::time::MonoTime;

use super::{GnomeTwin, Layout, Monitors, OpenScreen, Screen, Timing};
use crate::gnome::display_config::{
    DisplayState, LogicalConfig, LogicalState, ModeState, MonitorState,
};

/// Short waits, so tests that wait for the timer thread stay quick.
pub(in crate::gnome) const TEST_TIMING: Timing = Timing {
    linger: Duration::from_millis(120),
    fence_rearm: Duration::from_millis(80),
    poll_interval: Duration::from_millis(1),
    poll_budget: Duration::from_millis(150),
    debounce: Duration::from_millis(25),
    first_frame: Duration::from_millis(90),
};

/// Polls `condition` for up to three seconds.
pub(in crate::gnome) fn wait_until(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// A physical monitor of the fake Mutter.
#[derive(Clone, Debug)]
pub(in crate::gnome) struct Phys {
    pub connector: &'static str,
    pub width: i32,
    pub height: i32,
}

pub(in crate::gnome) struct Inner {
    pub serial: u32,
    pub physical: Vec<Phys>,
    /// The layout Mutter has stored for the physical monitors (what it applies when the twin goes).
    pub stored: Vec<LogicalState>,
    /// The layout now.
    pub logical: Vec<LogicalState>,
    pub twin: Option<(String, u32, u32)>,
    /// A virtual monitor somebody else made, present from the start.
    pub foreign: bool,
    pub applies: Vec<(u32, Vec<LogicalConfig>)>,
    pub fail_apply: VecDeque<PlatformError>,
    /// The next this many applies find the serial stale.
    pub serial_races: usize,
    /// The twin never shows up in `GetCurrentState`.
    pub hide_twin: bool,
    pub fail_open: Option<PlatformError>,
    pub fail_resize: Option<PlatformError>,
    pub fail_save: Option<PlatformError>,
    pub fail_restore: bool,
    pub fail_fence: bool,
    pub next_token: u32,
    pub changed: Option<Arc<dyn Fn() + Send + Sync>>,
    pub screens: Vec<Arc<ScreenShared>>,
    pub fence: Option<(i32, i32, i32, i32)>,
    /// How many times `GetCurrentState` was asked.
    pub reads: usize,
}

/// What a probe is asked at every logged call.
type Probe = Box<dyn Fn() -> bool + Send + Sync>;

/// The state the fakes share, and the log of calls.
pub(in crate::gnome) struct World {
    pub inner: Mutex<Inner>,
    log: Mutex<Vec<String>>,
    /// Asked at every logged call; the answers are kept with the call's line.
    probe: Mutex<Option<Probe>>,
    probed: Mutex<Vec<(String, bool)>>,
}

fn logical(x: i32, connector: &str, transform: u32, primary: bool, y: i32) -> LogicalState {
    LogicalState {
        x,
        y,
        scale: 1.0,
        transform,
        primary,
        connectors: vec![connector.to_owned()],
    }
}

impl World {
    /// DP-3 3440x1440 at the origin, DP-2 1920x1080 turned 90 degrees right of it (1080x1920
    /// logical), HDMI-1 1920x1080 after that: the rightmost edge is x = 6440.
    pub(in crate::gnome) fn new() -> Arc<World> {
        let physical = vec![
            Phys {
                connector: "DP-3",
                width: 3440,
                height: 1440,
            },
            Phys {
                connector: "DP-2",
                width: 1920,
                height: 1080,
            },
            Phys {
                connector: "HDMI-1",
                width: 1920,
                height: 1080,
            },
        ];
        let stored = vec![
            logical(0, "DP-3", 0, true, 0),
            logical(3440, "DP-2", 1, false, 0),
            logical(4520, "HDMI-1", 0, false, 0),
        ];
        Arc::new(World {
            inner: Mutex::new(Inner {
                serial: 10,
                physical,
                logical: stored.clone(),
                stored,
                twin: None,
                foreign: false,
                applies: Vec::new(),
                fail_apply: VecDeque::new(),
                serial_races: 0,
                hide_twin: false,
                fail_open: None,
                fail_resize: None,
                fail_save: None,
                fail_restore: false,
                fail_fence: false,
                next_token: 1,
                changed: None,
                screens: Vec::new(),
                fence: None,
                reads: 0,
            }),
            log: Mutex::new(Vec::new()),
            probe: Mutex::new(None),
            probed: Mutex::new(Vec::new()),
        })
    }

    pub(in crate::gnome) fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap()
    }

    pub(in crate::gnome) fn note(&self, line: impl Into<String>) {
        let line = line.into();
        if let Some(probe) = self.probe.lock().unwrap().as_ref() {
            let answer = probe();
            self.probed.lock().unwrap().push((line.clone(), answer));
        }
        self.log.lock().unwrap().push(line);
    }

    /// From now on `probe` is asked at every logged call (which must not log itself).
    pub(in crate::gnome) fn set_probe(&self, probe: impl Fn() -> bool + Send + Sync + 'static) {
        *self.probe.lock().unwrap() = Some(Box::new(probe));
    }

    /// The calls logged since the probe was set, each with the probe's answer at that moment.
    pub(in crate::gnome) fn probed(&self) -> Vec<(String, bool)> {
        self.probed.lock().unwrap().clone()
    }

    /// The ordered calls so far (everything but state reads).
    pub(in crate::gnome) fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    pub(in crate::gnome) fn clear_log(&self) {
        self.log.lock().unwrap().clear();
    }

    pub(in crate::gnome) fn opener(self: &Arc<World>) -> OpenScreen {
        let world = Arc::clone(self);
        Box::new(move |size| world.open_screen(size))
    }

    fn open_screen(self: &Arc<World>, size: PixelSize) -> Result<Box<dyn Screen>, PlatformError> {
        let shared = Arc::new(ScreenShared::default());
        {
            let mut inner = self.lock();
            if let Some(error) = inner.fail_open.take() {
                return Err(error);
            }
            inner.screens.push(Arc::clone(&shared));
        }
        self.note(format!("open {}x{}", size.width, size.height));
        self.add_twin(size.width, size.height);
        Ok(Box::new(FakeScreen {
            world: Arc::clone(self),
            shared,
            next: 1,
        }))
    }

    /// Mutter reacts to a new virtual monitor: linear relayout, rotation dropped.
    fn add_twin(&self, width: u32, height: u32) {
        let changed = {
            let mut inner = self.lock();
            inner.twin = Some(("Meta-0".to_owned(), width, height));
            inner.relayout_linear();
            inner.changed.clone()
        };
        if let Some(changed) = changed {
            changed();
        }
    }

    fn resize_twin(&self, width: u32, height: u32) {
        let changed = {
            let mut inner = self.lock();
            if let Some(twin) = inner.twin.as_mut() {
                twin.1 = width;
                twin.2 = height;
            }
            inner.relayout_linear();
            inner.changed.clone()
        };
        if let Some(changed) = changed {
            changed();
        }
    }

    /// Mutter removes the monitor and applies the stored layout again.
    fn remove_twin(&self) {
        let changed = {
            let mut inner = self.lock();
            inner.twin = None;
            inner.logical = inner.stored.clone();
            inner.serial += 1;
            inner.changed.clone()
        };
        if let Some(changed) = changed {
            changed();
        }
    }

    /// The user changes their layout (the stored one and the one applied): HDMI-1 moves down.
    pub(in crate::gnome) fn user_moves_hdmi(&self, y: i32) {
        let changed = {
            let mut guard = self.lock();
            let inner = &mut *guard;
            for layout in [&mut inner.stored, &mut inner.logical] {
                for l in layout.iter_mut().filter(|l| l.connectors[0] == "HDMI-1") {
                    l.y = y;
                }
            }
            inner.serial += 1;
            inner.changed.clone()
        };
        if let Some(changed) = changed {
            changed();
        }
    }

    /// A `MonitorsChanged` without a change.
    pub(in crate::gnome) fn signal_change(&self) {
        let changed = self.lock().changed.clone();
        if let Some(changed) = changed {
            changed();
        }
    }

    /// How many times `GetCurrentState` was asked.
    pub(in crate::gnome) fn reads(&self) -> usize {
        self.lock().reads
    }

    /// Something (the user, Mutter) moves the twin to `x`.
    pub(in crate::gnome) fn move_twin(&self, x: i32) {
        let changed = {
            let mut inner = self.lock();
            for l in inner
                .logical
                .iter_mut()
                .filter(|l| l.connectors[0].starts_with("Meta-"))
            {
                l.x = x;
            }
            inner.serial += 1;
            inner.changed.clone()
        };
        if let Some(changed) = changed {
            changed();
        }
    }

    /// Screen `index` is lost (the user stopped the share): its streams end `TargetGone`, its
    /// callback runs, and Mutter removes the monitor.
    pub(in crate::gnome) fn lose_screen(&self, index: usize) {
        let shared = Arc::clone(&self.lock().screens[index]);
        shared.lost.store(true, Ordering::SeqCst);
        shared.end_streams(StreamEndReason::TargetGone);
        self.remove_twin();
        let callback = shared.on_lost.lock().unwrap().clone();
        if let Some(callback) = callback {
            callback();
        }
    }

    /// Like [`lose_screen`](Self::lose_screen) without telling the twin: its callback never runs.
    pub(in crate::gnome) fn lose_screen_silently(&self, index: usize) {
        let shared = Arc::clone(&self.lock().screens[index]);
        shared.lost.store(true, Ordering::SeqCst);
        shared.end_streams(StreamEndReason::TargetGone);
        self.remove_twin();
    }

    pub(in crate::gnome) fn screen(&self, index: usize) -> Arc<ScreenShared> {
        Arc::clone(&self.lock().screens[index])
    }

    pub(in crate::gnome) fn screens_dropped(&self) -> usize {
        self.lock()
            .screens
            .iter()
            .filter(|s| s.dropped.load(Ordering::SeqCst))
            .count()
    }

    pub(in crate::gnome) fn screens_made(&self) -> usize {
        self.lock().screens.len()
    }

    /// The configs applied so far, with the serial each was applied against.
    pub(in crate::gnome) fn applies(&self) -> Vec<(u32, Vec<LogicalConfig>)> {
        self.lock().applies.clone()
    }
}

impl Inner {
    fn relayout_linear(&mut self) {
        let mut x = 0;
        let mut layout: Vec<LogicalState> = self
            .physical
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let state = logical(x, p.connector, 0, i == 0, 0);
                x += p.width;
                state
            })
            .collect();
        if let Some((connector, ..)) = &self.twin {
            layout.push(logical(x, connector, 0, false, 0));
        }
        self.logical = layout;
        self.serial += 1;
    }

    fn state(&self) -> DisplayState {
        let mode = |width: i32, height: i32, scales: &[f64]| ModeState {
            id: format!("{width}x{height}@60.000"),
            width,
            height,
            refresh: 60.0,
            preferred_scale: 1.0,
            supported_scales: scales.to_vec(),
            current: true,
            preferred: true,
        };
        let monitor = |connector: &str, vendor: &str, modes: Vec<ModeState>| MonitorState {
            connector: connector.to_owned(),
            vendor: vendor.to_owned(),
            product: "P".to_owned(),
            serial: "S".to_owned(),
            modes,
            is_builtin: false,
        };
        let mut monitors: Vec<MonitorState> = self
            .physical
            .iter()
            .map(|p| monitor(p.connector, "V", vec![mode(p.width, p.height, &[1.0, 2.0])]))
            .collect();
        if self.foreign {
            monitors.push(monitor(
                "Meta-7",
                "MetaVendor",
                vec![mode(1024, 768, &[1.0])],
            ));
        }
        if let Some((connector, width, height)) = &self.twin
            && !self.hide_twin
        {
            monitors.push(monitor(
                connector,
                "MetaVendor",
                vec![mode(
                    i32::try_from(*width).unwrap(),
                    i32::try_from(*height).unwrap(),
                    &[1.0, 1.25, 1.5, 2.0],
                )],
            ));
        }
        DisplayState {
            serial: self.serial,
            monitors,
            logical: self.logical.clone(),
            layout_mode: None,
        }
    }
}

/// The fake `DisplayConfig`.
pub(in crate::gnome) struct FakeMonitors(pub Arc<World>);

impl Monitors for FakeMonitors {
    fn current_state(&self) -> Result<DisplayState, PlatformError> {
        let mut inner = self.0.lock();
        inner.reads += 1;
        Ok(inner.state())
    }

    fn apply_temporary(&self, serial: u32, config: &[LogicalConfig]) -> Result<(), PlatformError> {
        let changed = {
            let mut inner = self.0.lock();
            if let Some(error) = inner.fail_apply.pop_front() {
                return Err(error);
            }
            if inner.serial_races > 0 {
                inner.serial_races -= 1;
                inner.serial += 1;
                return Err(PlatformError::Backend(
                    "DisplayConfig: org.freedesktop.DBus.Error.AccessDenied: The requested \
                     configuration is based on stale information"
                        .into(),
                ));
            }
            if serial != inner.serial {
                return Err(PlatformError::Backend(
                    "DisplayConfig: org.freedesktop.DBus.Error.AccessDenied: The requested \
                     configuration is based on stale information"
                        .into(),
                ));
            }
            inner.applies.push((serial, config.to_vec()));
            inner.logical = config
                .iter()
                .map(|c| LogicalState {
                    x: c.x,
                    y: c.y,
                    scale: c.scale,
                    transform: c.transform,
                    primary: c.primary,
                    connectors: c.monitors.iter().map(|(name, _)| name.clone()).collect(),
                })
                .collect();
            inner.serial += 1;
            inner.changed.clone()
        };
        self.0.note("apply");
        if let Some(changed) = changed {
            changed();
        }
        Ok(())
    }

    fn subscribe(&self, callback: Arc<dyn Fn() + Send + Sync>) -> Result<(), PlatformError> {
        self.0.lock().changed = Some(callback);
        Ok(())
    }
}

/// The fake Shell bridge's layout and fence calls.
pub(in crate::gnome) struct FakeLayout(pub Arc<World>);

impl Layout for FakeLayout {
    fn save_layout(&self) -> Result<u32, PlatformError> {
        let token = {
            let mut inner = self.0.lock();
            if let Some(error) = inner.fail_save.take() {
                return Err(error);
            }
            let token = inner.next_token;
            inner.next_token += 1;
            token
        };
        self.0.note(format!("save_layout {token}"));
        Ok(token)
    }

    fn restore_layout(&self, token: u32, skip: &[u64]) -> Result<u32, PlatformError> {
        self.0.note(format!("restore_layout {token} skip={skip:?}"));
        if self.0.lock().fail_restore {
            return Err(PlatformError::NotFound);
        }
        Ok(0)
    }

    fn set_pointer_fence(
        &self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<(), PlatformError> {
        let mut inner = self.0.lock();
        if inner.fail_fence {
            return Err(PlatformError::Backend("fence refused".into()));
        }
        inner.fence = Some((x, y, width, height));
        drop(inner);
        self.0.note(format!("set_fence {x},{y} {width}x{height}"));
        Ok(())
    }

    fn clear_pointer_fence(&self) -> Result<(), PlatformError> {
        self.0.lock().fence = None;
        self.0.note("clear_fence");
        Ok(())
    }
}

type Streams = Vec<(StreamId, Arc<dyn EventSink<FrameEvent>>)>;

/// What the test keeps of a screen after the twin owns it.
#[derive(Default)]
pub(in crate::gnome) struct ScreenShared {
    pub lost: AtomicBool,
    pub dropped: AtomicBool,
    on_lost: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    streams: Mutex<Streams>,
}

impl ScreenShared {
    fn end_streams(&self, reason: StreamEndReason) {
        for (stream, sink) in std::mem::take(&mut *self.streams.lock().unwrap()) {
            sink.send(FrameEvent::Ended { stream, reason });
        }
    }

    /// The gate closed: the screen ends its streams `Blocked` and stays.
    pub(in crate::gnome) fn end_streams_blocked(&self) {
        self.end_streams(StreamEndReason::Blocked);
    }

    /// A frame on the screen's first stream.
    pub(in crate::gnome) fn emit_frame(&self) {
        let streams = self.streams.lock().unwrap();
        let (stream, sink) = streams.first().expect("a stream is running");
        let frame = Frame::cpu(
            PixelSize::new(1, 1),
            4,
            Arc::from(vec![0u8; 4]),
            None,
            MonoTime::from_nanos(1),
        );
        sink.send(FrameEvent::Frame {
            stream: *stream,
            frame,
        });
    }

    pub(in crate::gnome) fn stream_count(&self) -> usize {
        self.streams.lock().unwrap().len()
    }
}

/// The fake virtual screen. Dropping it ends its streams `Requested` and removes the monitor.
pub(in crate::gnome) struct FakeScreen {
    world: Arc<World>,
    shared: Arc<ScreenShared>,
    next: u64,
}

impl Drop for FakeScreen {
    fn drop(&mut self) {
        self.shared.dropped.store(true, Ordering::SeqCst);
        self.world.note("screen dropped");
        if !self.shared.lost.load(Ordering::SeqCst) {
            self.shared.end_streams(StreamEndReason::Requested);
            self.world.remove_twin();
        }
    }
}

impl Screen for FakeScreen {
    fn resize(&mut self, size: PixelSize) -> Result<(), PlatformError> {
        if let Some(error) = self.world.lock().fail_resize.take() {
            return Err(error);
        }
        self.world
            .note(format!("resize {}x{}", size.width, size.height));
        self.world.resize_twin(size.width, size.height);
        Ok(())
    }

    fn is_live(&self) -> bool {
        !self.shared.lost.load(Ordering::SeqCst)
    }

    fn on_lost(&self, callback: Arc<dyn Fn() + Send + Sync>) {
        *self.shared.on_lost.lock().unwrap() = Some(callback);
    }
}

impl FrameCapture for FakeScreen {
    fn start(
        &mut self,
        target: CaptureTarget,
        _crop: Option<PixelRect>,
        _max_fps: u32,
        sink: Arc<dyn EventSink<FrameEvent>>,
    ) -> Result<StreamId, PlatformError> {
        match target {
            CaptureTarget::Display(_) => {}
            _ => return Err(PlatformError::NotFound),
        }
        let id = StreamId(self.next);
        self.next += 1;
        self.shared.streams.lock().unwrap().push((id, sink));
        self.world.note(format!("stream {}", id.0));
        Ok(id)
    }

    fn set_crop(
        &mut self,
        stream: StreamId,
        _crop: Option<PixelRect>,
    ) -> Result<(), PlatformError> {
        if self
            .shared
            .streams
            .lock()
            .unwrap()
            .iter()
            .any(|(id, _)| *id == stream)
        {
            Ok(())
        } else {
            Err(PlatformError::NotFound)
        }
    }

    fn stop(&mut self, stream: StreamId) -> Result<(), PlatformError> {
        let mut streams = self.shared.streams.lock().unwrap();
        let Some(at) = streams.iter().position(|(id, _)| *id == stream) else {
            return Err(PlatformError::NotFound);
        };
        let (id, sink) = streams.remove(at);
        drop(streams);
        sink.send(FrameEvent::Ended {
            stream: id,
            reason: StreamEndReason::Requested,
        });
        Ok(())
    }
}

/// A twin over the fakes.
pub(in crate::gnome) struct Rig {
    pub world: Arc<World>,
    pub twin: GnomeTwin,
}

impl Rig {
    pub(in crate::gnome) fn new() -> Rig {
        Rig::with_timing(TEST_TIMING)
    }

    pub(in crate::gnome) fn with_timing(timing: Timing) -> Rig {
        let world = World::new();
        let twin = GnomeTwin::with_parts(
            Box::new(FakeLayout(Arc::clone(&world))),
            Box::new(FakeMonitors(Arc::clone(&world))),
            world.opener(),
            timing,
        )
        .unwrap();
        Rig { world, twin }
    }
}
