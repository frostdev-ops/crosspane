//! `WindowSource` through the Crosspane Shell extension bridge (WP-G2.3).
//!
//! - `WindowId(shell id)`. Ids are only meaningful within the bridge's epoch: when the bridge
//!   reports `Lost` (Shell restart, extension disabled), every known window is `Removed` and the
//!   source reconnects with backoff (1 s doubling to 30 s); windows seen after that are `Added`
//!   anew. An id from an old epoch is never reused for a new window.
//! - `WindowInfo`: `app_id`, `title`, `pid` (`None` for 0), `frame` = the bridge rect in logical
//!   coordinates, `display` = the display containing the frame's centre (displays snapshot),
//!   `state` = `Minimized`/`Fullscreen`/`Normal`, `role` = `Toplevel` (the bridge lists only
//!   normal and dialog windows; dialogs are `Dialog` once the bridge reports type — v1 doesn't, so
//!   `Toplevel`), `parent` = `None`.
//! - `subscribe` (once): current windows as `Added` plus the current `Focused`, then diffs of
//!   each `ListWindows` refresh triggered by `WindowsChanged` (`Added`/`Changed`/`Removed`,
//!   `Focused` when the focused id changes). Refreshes run on one worker thread, coalesced.
//! - `activate` → bridge `Activate`; unknown is `NotFound`.
//!
//! # Design
//!
//! - **One worker.** `subscribe` registers a callback on the bridge, takes the first list on the
//!   caller (bounded by the bridge's 2 s call timeout) and starts one worker thread. The callback
//!   only sets flags in a mailbox, so it never blocks the bridge's signal thread and never holds
//!   the bridge (no reference cycle). The worker waits on the mailbox, so any number of
//!   `WindowsChanged` signals that arrive while it is busy cost one more list.
//! - **Snapshot.** The worker publishes each list it takes as the latest snapshot, before it sends
//!   the events of that change. `windows` and `focused` answer from it without a D-Bus call. Only
//!   when no snapshot exists (before `subscribe`) do they make one bounded `ListWindows` call, which
//!   is not cached. While the bridge is lost and not yet reconnected they fail with `Backend`.
//! - **Events.** One diff of two snapshots gives, in this order: `Removed` (ascending id), `Added`
//!   and `Changed` (ascending id), then `Focused` when the focused id differs. All events are sent
//!   from the worker thread, so the sink sees them in the order the worker saw them. Nothing is
//!   sent from the thread that calls the trait methods.
//! - **Loss.** `Lost` has priority over every other wake. The worker publishes "no windows", sends
//!   `Removed` for every known window (and `Focused(None)` if one had focus), drops the old
//!   bridge, then calls `ShellBridge::connect` after 1 s, 2 s, 4 s … up to 30 s between attempts.
//!   An attempt counts only when `connect`, the subscription and the first list all succeed; the
//!   new windows then arrive as `Added` plus the current `Focused`. Signals from a bridge that has
//!   been replaced are ignored (each bridge has its own generation).
//! - **Failed list.** A failed `ListWindows` is not evidence that a window closed: the snapshot is
//!   kept and the list is retried after 0.5 s, doubling to 5 s, until it succeeds, a new signal
//!   arrives, or the bridge is lost.
//! - **Display.** `display` is resolved against the displays snapshot at each refresh. It is
//!   re-resolved only when the list changes or is refreshed, not when displays alone change.
//! - **Activate.** After `subscribe`, an id the last refresh did not list is `NotFound` without a
//!   call, and while the bridge is lost it is a `Backend` error: an id the engine learned before a
//!   loss is never passed to the new epoch's bridge. The bridge only promises that ids are unique
//!   for one Shell's lifetime, and the id is the `WindowId` itself, so a new epoch may hand an old
//!   number to a different window. The guarantee above is therefore carried by order: every
//!   old-epoch `Removed` is sent before any new-epoch `Added`.
//! - Window titles are never logged above `debug`; the source logs only counts and error names.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crosspane_platform::{
    EventSink, PlatformError, WindowEvent, WindowInfo, WindowRole, WindowSource, WindowState,
};
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{DisplayGeometry, PointLogical, RectLogical, SizeLogical};
use crosspane_types::id::{DisplayId, WindowId};

use super::shell::{ShellBridge, ShellCallback, ShellEvent, ShellWindow};
use crate::portal::eis::DisplaysFn;

/// Waits between reconnect attempts: this, doubling, up to [`Timing::reconnect_max`].
const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
/// Waits before the retry of a failed list: this, doubling, up to [`Timing::retry_max`].
const RETRY_MIN: Duration = Duration::from_millis(500);
const RETRY_MAX: Duration = Duration::from_secs(5);

/// Windows listed by the Crosspane Shell extension.
pub struct GnomeWindows {
    source: Source<ShellBridge>,
}

impl fmt::Debug for GnomeWindows {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GnomeWindows").finish_non_exhaustive()
    }
}

impl GnomeWindows {
    /// Takes an already connected bridge and connects nothing itself. After a `Lost` bridge the
    /// worker started by `subscribe` reconnects with [`ShellBridge::connect`].
    pub fn new(bridge: ShellBridge, displays: DisplaysFn) -> Result<GnomeWindows, PlatformError> {
        Ok(GnomeWindows {
            source: Source::new(
                bridge,
                displays,
                Box::new(ShellBridge::connect),
                Timing::DEFAULT,
            ),
        })
    }
}

impl WindowSource for GnomeWindows {
    fn windows(&self) -> Result<Vec<WindowInfo>, PlatformError> {
        self.source
            .core
            .read(|snapshot| snapshot.windows.values().cloned().collect())
    }

    fn focused(&self) -> Result<Option<WindowId>, PlatformError> {
        self.source.core.read(|snapshot| snapshot.focused)
    }

    fn activate(&mut self, window: WindowId) -> Result<(), PlatformError> {
        self.source.core.activate(window)
    }

    fn subscribe(&mut self, sink: Arc<dyn EventSink<WindowEvent>>) -> Result<(), PlatformError> {
        self.source.subscribe(sink)
    }
}

/// What this source needs from a bridge. [`ShellBridge`] is the only real implementation; the tests
/// script a fake one, since the real one needs a session bus and a Shell.
trait Shell: Clone + Send + Sync + 'static {
    fn list(&self) -> Result<Vec<ShellWindow>, PlatformError>;
    fn focus(&self, id: u64) -> Result<(), PlatformError>;
    fn watch(&self, callback: ShellCallback) -> Result<(), PlatformError>;
}

impl Shell for ShellBridge {
    fn list(&self) -> Result<Vec<ShellWindow>, PlatformError> {
        self.list_windows()
    }

    fn focus(&self, id: u64) -> Result<(), PlatformError> {
        self.activate(id)
    }

    fn watch(&self, callback: ShellCallback) -> Result<(), PlatformError> {
        self.subscribe(callback)
    }
}

/// Opens a new connection to the bridge after a loss.
type Connect<S> = Box<dyn Fn() -> Result<S, PlatformError> + Send + Sync>;

/// The waits of the worker; the tests use short ones.
#[derive(Clone, Copy, Debug)]
struct Timing {
    reconnect_min: Duration,
    reconnect_max: Duration,
    retry_min: Duration,
    retry_max: Duration,
}

impl Timing {
    const DEFAULT: Timing = Timing {
        reconnect_min: RECONNECT_MIN,
        reconnect_max: RECONNECT_MAX,
        retry_min: RETRY_MIN,
        retry_max: RETRY_MAX,
    };

    /// The wait after a failed list: the first retry delay, then doubling up to the cap.
    fn next_retry(self, previous: Option<Duration>) -> Duration {
        previous.map_or(self.retry_min, |delay| (delay * 2).min(self.retry_max))
    }

    /// The wait after a failed reconnect attempt.
    fn next_reconnect(self, previous: Duration) -> Duration {
        (previous * 2).min(self.reconnect_max)
    }
}

/// The shared core plus the handle of its worker thread, which `Drop` stops and joins so the sink
/// is never called after the source is dropped.
struct Source<S: Shell> {
    core: Arc<Core<S>>,
    worker: Option<JoinHandle<()>>,
}

impl<S: Shell> Source<S> {
    fn new(bridge: S, displays: DisplaysFn, connect: Connect<S>, timing: Timing) -> Source<S> {
        Source {
            core: Arc::new(Core {
                live: Mutex::new(Live {
                    bridge: Some(bridge),
                    snapshot: None,
                    subscribed: false,
                }),
                mail: Arc::new(Mailbox::default()),
                displays,
                connect,
                timing,
            }),
            worker: None,
        }
    }

    fn subscribe(&mut self, sink: Arc<dyn EventSink<WindowEvent>>) -> Result<(), PlatformError> {
        self.worker = Some(self.core.start(sink)?);
        Ok(())
    }
}

impl<S: Shell> Drop for Source<S> {
    fn drop(&mut self) {
        self.core.mail.stop();
        if let Some(worker) = self.worker.take()
            // The sink could own the last handle to this source, which would then be dropped on the
            // worker itself; joining oneself never returns.
            && worker.thread().id() != thread::current().id()
        {
            let _ = worker.join();
        }
    }
}

/// What the trait methods and the worker share.
///
/// The bridge callback holds only the mailbox, never the core: the core owns the bridge, which owns
/// the callback, so a reference back to the core would be a cycle.
struct Core<S: Shell> {
    live: Mutex<Live<S>>,
    mail: Arc<Mailbox>,
    displays: DisplaysFn,
    connect: Connect<S>,
    timing: Timing,
}

struct Live<S> {
    /// `None` between a loss and the reconnect.
    bridge: Option<S>,
    /// The worker's latest list. `None` before `subscribe` and while the bridge is lost.
    snapshot: Option<Snapshot>,
    subscribed: bool,
}

impl<S: Shell> Core<S> {
    fn live(&self) -> MutexGuard<'_, Live<S>> {
        lock(&self.live)
    }

    fn bridge(&self) -> Option<S> {
        self.live().bridge.clone()
    }

    /// Reads from the worker's snapshot; with none yet (before `subscribe`), from one bounded
    /// `ListWindows` call, which is not kept.
    fn read<T>(&self, pick: impl FnOnce(&Snapshot) -> T) -> Result<T, PlatformError> {
        let bridge = {
            let live = self.live();
            if let Some(snapshot) = &live.snapshot {
                return Ok(pick(snapshot));
            }
            live.bridge.clone().ok_or_else(unavailable)?
        };
        let list = bridge.list()?;
        Ok(pick(&snapshot_of(&list, &(self.displays)())))
    }

    fn activate(&self, window: WindowId) -> Result<(), PlatformError> {
        let bridge = {
            let live = self.live();
            if live
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| !snapshot.windows.contains_key(&window))
            {
                return Err(PlatformError::NotFound);
            }
            live.bridge.clone().ok_or_else(unavailable)?
        };
        bridge.focus(window.0)
    }

    /// Registers on the bridge and takes the first list, as `subscribe`, and starts the worker.
    /// A failure leaves the source unsubscribed. A callback registered by a failed attempt stays on
    /// the bridge but is inert: the next attempt starts a new generation.
    fn start(
        self: &Arc<Self>,
        sink: Arc<dyn EventSink<WindowEvent>>,
    ) -> Result<JoinHandle<()>, PlatformError> {
        let bridge = {
            let mut live = self.live();
            if live.subscribed {
                return Err(PlatformError::Backend(
                    "WindowSource::subscribe called twice".into(),
                ));
            }
            let bridge = live.bridge.clone().ok_or_else(unavailable)?;
            live.subscribed = true;
            bridge
        };
        let started = self.attach(&bridge).and_then(|first| {
            // Published before the worker runs, so `windows` is answered from the snapshot as soon
            // as `subscribe` has returned.
            self.live().snapshot = Some(first.clone());
            let core = self.clone();
            thread::Builder::new()
                .name("crosspane-gnome-windows".into())
                .spawn(move || core.run(&*sink, first))
                .map_err(|e| PlatformError::Backend(format!("spawn GNOME windows thread: {e}")))
        });
        if started.is_err() {
            let mut live = self.live();
            live.subscribed = false;
            live.snapshot = None;
        }
        started
    }

    /// Starts a new generation, registers the mailbox callback on `bridge` and takes its first
    /// list. The callback is registered before the list, so no change in between is missed.
    fn attach(&self, bridge: &S) -> Result<Snapshot, PlatformError> {
        let generation = self.mail.next_generation();
        let mail = self.mail.clone();
        bridge.watch(Arc::new(move |event: ShellEvent| {
            mail.push(generation, &event);
        }))?;
        let list = bridge.list()?;
        Ok(snapshot_of(&list, &(self.displays)()))
    }

    /// The worker: sends the first events, then follows the mailbox until the source is dropped.
    fn run(&self, sink: &dyn EventSink<WindowEvent>, first: Snapshot) {
        let mut current = first;
        emit(sink, initial_events(&current));
        let mut retry: Option<Duration> = None;
        loop {
            match self.mail.wait(retry) {
                Wake::Stop => return,
                Wake::Lost => {
                    if !self.recover(sink, &mut current) {
                        return;
                    }
                    retry = None;
                }
                Wake::Changed | Wake::Timer => retry = self.refresh(sink, &mut current, retry),
            }
        }
    }

    /// One `ListWindows`, published and diffed against `current`. Returns the wait before the next
    /// attempt when the list failed.
    fn refresh(
        &self,
        sink: &dyn EventSink<WindowEvent>,
        current: &mut Snapshot,
        retry: Option<Duration>,
    ) -> Option<Duration> {
        let bridge = self.bridge()?;
        match bridge.list() {
            Ok(list) => {
                let new = snapshot_of(&list, &(self.displays)());
                if new != *current {
                    let events = diff(current, &new);
                    tracing::debug!(
                        windows = new.windows.len(),
                        events = events.len(),
                        "GNOME windows refreshed"
                    );
                    // Published first: a sink that reacts to an event by reading sees it applied.
                    self.live().snapshot = Some(new.clone());
                    *current = new;
                    emit(sink, events);
                }
                None
            }
            Err(error) => {
                // Not evidence that a window closed. `Lost` takes priority when it is the cause.
                if retry.is_none() {
                    tracing::warn!(%error, "GNOME window list failed; retrying");
                } else {
                    tracing::debug!(%error, "GNOME window list failed again");
                }
                Some(self.timing.next_retry(retry))
            }
        }
    }

    /// The bridge is lost: every window is removed, then the bridge is reconnected with backoff.
    /// Returns false when the source was dropped meanwhile.
    fn recover(&self, sink: &dyn EventSink<WindowEvent>, current: &mut Snapshot) -> bool {
        tracing::info!("Shell bridge lost; windows removed, reconnecting");
        let lost = std::mem::take(current);
        let old = {
            let mut live = self.live();
            live.snapshot = None;
            live.bridge.take()
        };
        // Outside the lock: dropping the last clone closes the bridge's signal connection.
        drop(old);
        emit(sink, diff(&lost, &Snapshot::default()));
        let mut delay = self.timing.reconnect_min;
        loop {
            if self.mail.sleep(delay) {
                return false;
            }
            match self.reconnect() {
                Ok((bridge, snapshot)) => {
                    if self.mail.stopped() {
                        return false;
                    }
                    {
                        let mut live = self.live();
                        live.bridge = Some(bridge);
                        live.snapshot = Some(snapshot.clone());
                    }
                    tracing::info!(windows = snapshot.windows.len(), "Shell bridge reconnected");
                    *current = snapshot;
                    emit(sink, initial_events(current));
                    return true;
                }
                Err(error) => {
                    tracing::debug!(%error, "Shell bridge reconnect failed");
                    delay = self.timing.next_reconnect(delay);
                }
            }
        }
    }

    fn reconnect(&self) -> Result<(S, Snapshot), PlatformError> {
        let bridge = (self.connect)()?;
        let snapshot = self.attach(&bridge)?;
        Ok((bridge, snapshot))
    }
}

fn emit(sink: &dyn EventSink<WindowEvent>, events: Vec<WindowEvent>) {
    for event in events {
        sink.send(event);
    }
}

fn unavailable() -> PlatformError {
    PlatformError::Backend("Shell bridge lost; reconnecting".into())
}

/// Locks `mutex`. Every update is a whole step on plain data, so a poisoned lock is taken anyway.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What woke the worker, in priority order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wake {
    Stop,
    Lost,
    Changed,
    /// The wait given to [`Mailbox::wait`] ran out with nothing else pending.
    Timer,
}

/// Signals from the bridge's signal thread to the worker, coalesced into flags. Events are tagged
/// with the generation of the bridge that sent them; those of a replaced bridge are dropped.
#[derive(Default)]
struct Mailbox {
    mail: Mutex<Mail>,
    wake: Condvar,
}

#[derive(Default)]
struct Mail {
    generation: u64,
    changed: bool,
    lost: bool,
    stop: bool,
}

impl Mailbox {
    /// Starts the next generation: pending flags belong to the bridge before and are cleared.
    fn next_generation(&self) -> u64 {
        let mut mail = lock(&self.mail);
        mail.generation += 1;
        mail.changed = false;
        mail.lost = false;
        mail.generation
    }

    /// Called on the bridge's signal thread: only sets a flag and never blocks.
    fn push(&self, generation: u64, event: &ShellEvent) {
        let mut mail = lock(&self.mail);
        if mail.stop || mail.generation != generation {
            return;
        }
        match event {
            ShellEvent::WindowsChanged { .. } => mail.changed = true,
            ShellEvent::Lost => mail.lost = true,
            ShellEvent::OverlayState { .. } => return,
        }
        self.wake.notify_all();
    }

    fn stop(&self) {
        lock(&self.mail).stop = true;
        self.wake.notify_all();
    }

    fn stopped(&self) -> bool {
        lock(&self.mail).stop
    }

    /// Blocks until something is pending (or `timeout` runs out). `Changed` is consumed; `Lost`
    /// stays set until the next generation starts.
    fn wait(&self, timeout: Option<Duration>) -> Wake {
        let idle = |mail: &mut Mail| !(mail.stop || mail.lost || mail.changed);
        let mail = lock(&self.mail);
        let mut mail = match timeout {
            None => self
                .wake
                .wait_while(mail, idle)
                .unwrap_or_else(PoisonError::into_inner),
            Some(timeout) => {
                self.wake
                    .wait_timeout_while(mail, timeout, idle)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0
            }
        };
        if mail.stop {
            Wake::Stop
        } else if mail.lost {
            Wake::Lost
        } else if mail.changed {
            mail.changed = false;
            Wake::Changed
        } else {
            Wake::Timer
        }
    }

    /// Sleeps for `delay`; true if the source was dropped before it ran out.
    fn sleep(&self, delay: Duration) -> bool {
        let mail = lock(&self.mail);
        self.wake
            .wait_timeout_while(mail, delay, |mail| !mail.stop)
            .unwrap_or_else(PoisonError::into_inner)
            .0
            .stop
    }
}

/// One list of the bridge: the windows by id, and which one has focus.
#[derive(Clone, Debug, Default, PartialEq)]
struct Snapshot {
    windows: BTreeMap<WindowId, WindowInfo>,
    focused: Option<WindowId>,
}

/// The snapshot of one bridge list. A repeated id keeps its first row (the bridge promises unique
/// ids; dropping a window over a duplicate would be worse). The first row marked focused wins.
fn snapshot_of(list: &[ShellWindow], displays: &[DisplayInfo]) -> Snapshot {
    let mut snapshot = Snapshot::default();
    for window in list {
        let id = WindowId(window.id);
        if snapshot.windows.contains_key(&id) {
            continue;
        }
        if window.focused && snapshot.focused.is_none() {
            snapshot.focused = Some(id);
        }
        snapshot.windows.insert(id, window_info(window, displays));
    }
    snapshot
}

fn window_info(window: &ShellWindow, displays: &[DisplayInfo]) -> WindowInfo {
    // The bridge sends signed sizes; a negative one can't be a frame.
    let frame = RectLogical::new(
        PointLogical::new(f64::from(window.x), f64::from(window.y)),
        SizeLogical::new(
            f64::from(window.width.max(0)),
            f64::from(window.height.max(0)),
        ),
    );
    WindowInfo {
        id: WindowId(window.id),
        title: window.title.clone(),
        app_id: window.app_id.clone(),
        pid: (window.pid != 0).then_some(window.pid),
        display: assign_display(&frame, displays),
        frame,
        // As Hyprland's `Hidden` outranks `Fullscreen`, a minimized window is `Minimized` first.
        state: if window.minimized {
            WindowState::Minimized
        } else if window.fullscreen {
            WindowState::Fullscreen
        } else {
            WindowState::Normal
        },
        role: WindowRole::Toplevel,
        parent: None,
    }
}

/// The display whose logical rect contains the centre of `frame` (left and top edges inside, right
/// and bottom edges outside), or `None`. Displays that overlap (mirrored) give the lowest id, so
/// the answer doesn't depend on the order of the snapshot. A display with invalid geometry never
/// matches.
fn assign_display(frame: &RectLogical, displays: &[DisplayInfo]) -> Option<DisplayId> {
    let x = frame.origin.x + frame.size.width / 2.0;
    let y = frame.origin.y + frame.size.height / 2.0;
    displays
        .iter()
        .filter(|display| contains(&display.geometry, x, y))
        .map(|display| display.id)
        .min()
}

fn contains(geometry: &DisplayGeometry, x: f64, y: f64) -> bool {
    let size = geometry.pixel_size;
    let origin = geometry.logical_origin;
    if !geometry.scale.is_finite()
        || geometry.scale <= 0.0
        || size.width == 0
        || size.height == 0
        || !origin.x.is_finite()
        || !origin.y.is_finite()
    {
        return false;
    }
    let bounds = geometry.logical_bounds();
    x >= bounds.min_x() && x < bounds.max_x() && y >= bounds.min_y() && y < bounds.max_y()
}

/// The events from `old` to `new`: `Removed` for each id that went (ascending), `Added` or
/// `Changed` for each new or different window (ascending), then `Focused` if the focused id
/// differs. Nothing for equal snapshots.
fn diff(old: &Snapshot, new: &Snapshot) -> Vec<WindowEvent> {
    let mut events = Vec::new();
    for id in old.windows.keys() {
        if !new.windows.contains_key(id) {
            events.push(WindowEvent::Removed(*id));
        }
    }
    for (id, info) in &new.windows {
        match old.windows.get(id) {
            None => events.push(WindowEvent::Added(info.clone())),
            Some(previous) if previous != info => events.push(WindowEvent::Changed(info.clone())),
            Some(_) => {}
        }
    }
    if old.focused != new.focused {
        events.push(WindowEvent::Focused(new.focused));
    }
    events
}

/// What `subscribe` (and a reconnect) sends first: every window as `Added` in ascending id, then
/// the current `Focused`, which is `Focused(None)` when no window has focus.
fn initial_events(snapshot: &Snapshot) -> Vec<WindowEvent> {
    snapshot
        .windows
        .values()
        .map(|info| WindowEvent::Added(info.clone()))
        .chain(std::iter::once(WindowEvent::Focused(snapshot.focused)))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Instant;

    use crosspane_types::color::ColorSpace;
    use crosspane_types::geom::{PixelSize, SizeMm};

    use super::*;

    const WAIT: Duration = Duration::from_secs(5);
    const QUIET: Duration = Duration::from_millis(150);

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

    fn frame(x: f64, y: f64, width: f64, height: f64) -> RectLogical {
        RectLogical::new(PointLogical::new(x, y), SizeLogical::new(width, height))
    }

    fn shell_window(id: u64, x: i32, y: i32, width: i32, height: i32) -> ShellWindow {
        ShellWindow {
            id,
            app_id: format!("org.example.App{id}.desktop"),
            title: format!("window {id}"),
            pid: 1000 + u32::try_from(id).unwrap(),
            x,
            y,
            width,
            height,
            focused: false,
            minimized: false,
            fullscreen: false,
        }
    }

    fn one_display() -> Vec<DisplayInfo> {
        vec![display(1, 1920, 1080, 1.0, (0.0, 0.0))]
    }

    fn snapshot(windows: &[ShellWindow]) -> Snapshot {
        snapshot_of(windows, &one_display())
    }

    #[test]
    fn gnome_windows_is_a_sendable_window_source() {
        fn assert_source<T: WindowSource + Send + fmt::Debug>() {}
        assert_source::<GnomeWindows>();
    }

    /// Runs only against a live Crosspane Shell extension (`CROSSPANE_GNOME_SHELL_LIVE=1`, in the
    /// GNOME session). Read-only: lists the windows, subscribes and checks the first events. It
    /// never activates, moves or closes a window, and prints counts, never titles.
    #[test]
    fn live_smoke() {
        use crosspane_platform::Displays;

        if std::env::var("CROSSPANE_GNOME_SHELL_LIVE").as_deref() != Ok("1") {
            return;
        }
        let outputs = crate::wayland_outputs::WaylandOutputs::new().unwrap();
        let known = outputs.displays().unwrap();
        let displays: DisplaysFn = Arc::new(move || known.clone());
        let bridge = ShellBridge::connect().unwrap();
        let mut source = GnomeWindows::new(bridge, displays).unwrap();
        let before = source.windows().unwrap();
        let (tx, rx) = mpsc::channel();
        source.subscribe(sink(tx)).unwrap();
        let mut added = 0usize;
        loop {
            match rx.recv_timeout(WAIT).unwrap() {
                WindowEvent::Added(_) => added += 1,
                WindowEvent::Focused(focus) => {
                    println!("focused window listed: {}", focus.is_some());
                    break;
                }
                other => panic!("unexpected first event: {:?}", ids(&[other])),
            }
        }
        let after = source.windows().unwrap();
        println!(
            "GNOME windows: {} before, {added} added, {} after, {} with a display",
            before.len(),
            after.len(),
            after.iter().filter(|w| w.display.is_some()).count()
        );
        // Windows may open or close while this runs; the lists can differ a little, not a lot.
        assert!(added.abs_diff(before.len()) <= 2, "the list jumped");
        assert!(added.abs_diff(after.len()) <= 2, "the list jumped");
        assert!(after.iter().all(|w| w.frame.size.width >= 0.0));
        if let Some(focus) = source.focused().unwrap() {
            assert!(after.iter().any(|w| w.id == focus));
        }
    }

    // ---- display assignment ----

    #[test]
    fn display_is_the_one_containing_the_frame_centre() {
        let displays = [
            display(1, 1920, 1080, 1.0, (0.0, 0.0)),
            display(2, 1920, 1080, 1.0, (1920.0, 0.0)),
        ];
        // The window starts on display 1 but its centre (2100) is on display 2.
        let straddling = frame(1800.0, 100.0, 600.0, 400.0);
        assert_eq!(assign_display(&straddling, &displays), Some(DisplayId(2)));
        let left = frame(100.0, 100.0, 600.0, 400.0);
        assert_eq!(assign_display(&left, &displays), Some(DisplayId(1)));
    }

    #[test]
    fn display_edges_are_half_open() {
        let displays = [
            display(1, 1920, 1080, 1.0, (0.0, 0.0)),
            display(2, 1920, 1080, 1.0, (1920.0, 0.0)),
        ];
        // Centre exactly on the shared edge belongs to the display on its right.
        let on_edge = frame(1900.0, 0.0, 40.0, 10.0);
        assert_eq!(assign_display(&on_edge, &displays), Some(DisplayId(2)));
        // Centre on the left edge of display 1 is inside; just past the bottom edge is outside.
        assert_eq!(
            assign_display(&frame(-10.0, -10.0, 20.0, 20.0), &displays),
            Some(DisplayId(1))
        );
        assert_eq!(
            assign_display(&frame(0.0, 1070.0, 10.0, 20.0), &displays),
            None
        );
    }

    #[test]
    fn display_is_none_outside_every_display_and_without_displays() {
        let displays = one_display();
        assert_eq!(
            assign_display(&frame(5000.0, 5000.0, 100.0, 100.0), &displays),
            None
        );
        assert_eq!(assign_display(&frame(0.0, 0.0, 100.0, 100.0), &[]), None);
    }

    #[test]
    fn display_uses_logical_size_and_negative_origins() {
        // 3840x2160 device px at 2x is 1920x1080 logical, placed left of the origin.
        let displays = [
            display(7, 3840, 2160, 2.0, (-1920.0, 0.0)),
            display(8, 1920, 1080, 1.0, (0.0, 0.0)),
        ];
        assert_eq!(
            assign_display(&frame(-1000.0, 500.0, 200.0, 200.0), &displays),
            Some(DisplayId(7))
        );
        // Fractional scale: 2560 px at 1.25x is 2048 logical wide.
        let fractional = [display(3, 2560, 1440, 1.25, (0.0, 0.0))];
        assert_eq!(
            assign_display(&frame(1950.0, 0.0, 100.0, 100.0), &fractional),
            Some(DisplayId(3))
        );
        assert_eq!(
            assign_display(&frame(2040.0, 0.0, 100.0, 100.0), &fractional),
            None
        );
    }

    #[test]
    fn overlapping_displays_give_the_lowest_id_in_any_order() {
        let a = display(9, 1920, 1080, 1.0, (0.0, 0.0));
        let b = display(4, 1920, 1080, 1.0, (0.0, 0.0));
        let window = frame(100.0, 100.0, 100.0, 100.0);
        assert_eq!(
            assign_display(&window, &[a.clone(), b.clone()]),
            Some(DisplayId(4))
        );
        assert_eq!(assign_display(&window, &[b, a]), Some(DisplayId(4)));
    }

    #[test]
    fn displays_with_invalid_geometry_never_match() {
        let window = frame(10.0, 10.0, 10.0, 10.0);
        for bad in [
            display(1, 1920, 1080, 0.0, (0.0, 0.0)),
            display(1, 1920, 1080, -1.0, (0.0, 0.0)),
            display(1, 1920, 1080, f64::NAN, (0.0, 0.0)),
            display(1, 0, 1080, 1.0, (0.0, 0.0)),
            display(1, 1920, 0, 1.0, (0.0, 0.0)),
            display(1, 1920, 1080, 1.0, (f64::NAN, 0.0)),
        ] {
            assert_eq!(assign_display(&window, &[bad]), None);
        }
        // A bad display doesn't hide a good one.
        let displays = [
            display(1, 1920, 1080, 0.0, (0.0, 0.0)),
            display(2, 1920, 1080, 1.0, (0.0, 0.0)),
        ];
        assert_eq!(assign_display(&window, &displays), Some(DisplayId(2)));
    }

    // ---- window mapping ----

    #[test]
    fn window_info_maps_the_bridge_row() {
        let mut row = shell_window(7, 100, 200, 640, 480);
        row.app_id = "org.gnome.Nautilus.desktop".into();
        row.title = "Files".into();
        row.pid = 4321;
        let info = window_info(&row, &one_display());
        assert_eq!(
            info,
            WindowInfo {
                id: WindowId(7),
                title: "Files".into(),
                app_id: "org.gnome.Nautilus.desktop".into(),
                pid: Some(4321),
                display: Some(DisplayId(1)),
                frame: frame(100.0, 200.0, 640.0, 480.0),
                state: WindowState::Normal,
                role: WindowRole::Toplevel,
                parent: None,
            }
        );
    }

    #[test]
    fn unknown_pid_is_none_and_negative_sizes_are_empty() {
        let mut row = shell_window(1, -50, -60, -5, -7);
        row.pid = 0;
        let info = window_info(&row, &one_display());
        assert_eq!(info.pid, None);
        assert_eq!(info.frame, frame(-50.0, -60.0, 0.0, 0.0));
    }

    #[test]
    fn state_is_minimized_before_fullscreen_before_normal() {
        let state = |minimized, fullscreen| {
            let mut row = shell_window(1, 0, 0, 100, 100);
            row.minimized = minimized;
            row.fullscreen = fullscreen;
            window_info(&row, &one_display()).state
        };
        assert_eq!(state(false, false), WindowState::Normal);
        assert_eq!(state(false, true), WindowState::Fullscreen);
        assert_eq!(state(true, false), WindowState::Minimized);
        assert_eq!(state(true, true), WindowState::Minimized);
    }

    #[test]
    fn snapshot_orders_by_id_picks_the_first_focus_and_keeps_the_first_duplicate() {
        let mut a = shell_window(30, 0, 0, 10, 10);
        let mut b = shell_window(10, 0, 0, 10, 10);
        let mut c = shell_window(20, 0, 0, 10, 10);
        a.focused = true;
        c.focused = true;
        b.title = "first ten".into();
        let mut dup = shell_window(10, 5, 5, 99, 99);
        dup.title = "second ten".into();
        dup.focused = true;
        let s = snapshot(&[a, b, c, dup]);
        let ids: Vec<u64> = s.windows.keys().map(|id| id.0).collect();
        assert_eq!(ids, [10, 20, 30]);
        assert_eq!(s.windows[&WindowId(10)].title, "first ten");
        // Focus: the first focused row (30), not the later rows or the dropped duplicate.
        assert_eq!(s.focused, Some(WindowId(30)));
        assert_eq!(snapshot(&[]).focused, None);
    }

    // ---- diff ----

    fn ids(events: &[WindowEvent]) -> Vec<String> {
        events
            .iter()
            .map(|event| match event {
                WindowEvent::Added(info) => format!("+{}", info.id.0),
                WindowEvent::Changed(info) => format!("~{}", info.id.0),
                WindowEvent::Removed(id) => format!("-{}", id.0),
                WindowEvent::Focused(Some(id)) => format!("focus {}", id.0),
                WindowEvent::Focused(None) => "focus none".to_owned(),
                _ => "other".to_owned(),
            })
            .collect()
    }

    #[test]
    fn equal_snapshots_give_no_events() {
        let mut one = shell_window(1, 0, 0, 10, 10);
        one.focused = true;
        let s = snapshot(&[one, shell_window(2, 5, 5, 10, 10)]);
        assert!(diff(&s, &s.clone()).is_empty());
        assert!(diff(&Snapshot::default(), &Snapshot::default()).is_empty());
    }

    #[test]
    fn diff_orders_removed_then_added_and_changed_then_focus() {
        let mut keep = shell_window(2, 0, 0, 100, 100);
        keep.focused = true;
        let old = snapshot(&[
            shell_window(1, 0, 0, 10, 10),
            keep.clone(),
            shell_window(3, 0, 0, 10, 10),
            shell_window(4, 0, 0, 10, 10),
        ]);
        let mut moved = keep;
        moved.x = 50;
        moved.focused = false;
        let mut now_focused = shell_window(9, 0, 0, 10, 10);
        now_focused.focused = true;
        let new = snapshot(&[
            moved,
            shell_window(4, 0, 0, 10, 10),
            now_focused,
            shell_window(5, 0, 0, 10, 10),
        ]);
        assert_eq!(
            ids(&diff(&old, &new)),
            ["-1", "-3", "~2", "+5", "+9", "focus 9"]
        );
    }

    #[test]
    fn diff_reports_each_changed_field_once_and_focus_alone() {
        let base = shell_window(1, 0, 0, 100, 100);
        let s = snapshot(std::slice::from_ref(&base));
        let changed = |edit: fn(&mut ShellWindow)| {
            let mut row = base.clone();
            edit(&mut row);
            ids(&diff(&s, &snapshot(&[row])))
        };
        assert_eq!(changed(|w| w.title = "other".into()), ["~1"]);
        assert_eq!(changed(|w| w.width = 101), ["~1"]);
        assert_eq!(changed(|w| w.minimized = true), ["~1"]);
        assert_eq!(changed(|w| w.fullscreen = true), ["~1"]);
        assert_eq!(changed(|w| w.pid = 0), ["~1"]);
        assert_eq!(changed(|w| w.app_id = "other".into()), ["~1"]);
        // Focus alone is only a `Focused` event: the `WindowInfo` carries no focus.
        assert_eq!(changed(|w| w.focused = true), ["focus 1"]);
        // A change of display alone is a change of the info.
        assert_eq!(changed(|w| w.x = 5000), ["~1"]);
    }

    #[test]
    fn losing_focus_alone_is_focused_none() {
        let mut row = shell_window(1, 0, 0, 10, 10);
        row.focused = true;
        let focused = snapshot(std::slice::from_ref(&row));
        row.focused = false;
        let unfocused = snapshot(&[row]);
        assert_eq!(ids(&diff(&focused, &unfocused)), ["focus none"]);
        assert_eq!(ids(&diff(&unfocused, &focused)), ["focus 1"]);
    }

    #[test]
    fn a_loss_is_removed_for_every_window_then_no_focus() {
        let mut a = shell_window(5, 0, 0, 10, 10);
        a.focused = true;
        let known = snapshot(&[shell_window(3, 0, 0, 10, 10), a]);
        assert_eq!(
            ids(&diff(&known, &Snapshot::default())),
            ["-3", "-5", "focus none"]
        );
        // No focus before the loss: no `Focused` after it.
        let unfocused = snapshot(&[shell_window(3, 0, 0, 10, 10)]);
        assert_eq!(ids(&diff(&unfocused, &Snapshot::default())), ["-3"]);
        assert!(diff(&Snapshot::default(), &Snapshot::default()).is_empty());
    }

    #[test]
    fn initial_events_are_added_in_id_order_then_the_current_focus() {
        let mut b = shell_window(2, 0, 0, 10, 10);
        b.focused = true;
        let s = snapshot(&[b, shell_window(1, 0, 0, 10, 10)]);
        assert_eq!(ids(&initial_events(&s)), ["+1", "+2", "focus 2"]);
        assert_eq!(ids(&initial_events(&Snapshot::default())), ["focus none"]);
    }

    // ---- mailbox ----

    #[test]
    fn mailbox_coalesces_changes_and_gives_lost_priority() {
        let mail = Mailbox::default();
        let generation = mail.next_generation();
        let change = ShellEvent::WindowsChanged { epoch: 1 };
        for _ in 0..5 {
            mail.push(generation, &change);
        }
        assert_eq!(mail.wait(Some(Duration::ZERO)), Wake::Changed);
        assert_eq!(mail.wait(Some(Duration::ZERO)), Wake::Timer);
        mail.push(generation, &change);
        mail.push(generation, &ShellEvent::Lost);
        assert_eq!(mail.wait(None), Wake::Lost);
        // `Lost` stays until the next generation.
        assert_eq!(mail.wait(None), Wake::Lost);
        mail.next_generation();
        assert_eq!(mail.wait(Some(Duration::ZERO)), Wake::Timer);
    }

    #[test]
    fn mailbox_ignores_other_generations_overlay_events_and_pushes_after_stop() {
        let mail = Mailbox::default();
        let old = mail.next_generation();
        let current = mail.next_generation();
        mail.push(old, &ShellEvent::Lost);
        mail.push(old, &ShellEvent::WindowsChanged { epoch: 1 });
        mail.push(
            current,
            &ShellEvent::OverlayState {
                id: 1,
                visible: true,
            },
        );
        assert_eq!(mail.wait(Some(Duration::ZERO)), Wake::Timer);
        mail.stop();
        assert!(mail.stopped());
        assert_eq!(mail.wait(None), Wake::Stop);
        mail.push(current, &ShellEvent::Lost);
        assert_eq!(mail.wait(None), Wake::Stop);
        assert!(mail.sleep(Duration::from_secs(60)));
    }

    #[test]
    fn mailbox_stop_wakes_a_sleeping_waiter() {
        let mail = Arc::new(Mailbox::default());
        let sleeper = {
            let mail = mail.clone();
            thread::spawn(move || {
                let started = Instant::now();
                (mail.sleep(Duration::from_secs(60)), started.elapsed())
            })
        };
        thread::sleep(Duration::from_millis(30));
        mail.stop();
        let (stopped, waited) = sleeper.join().unwrap();
        assert!(stopped);
        assert!(waited < Duration::from_secs(30));
        let idle = Mailbox::default();
        assert!(!idle.sleep(Duration::from_millis(5)));
    }

    #[test]
    fn backoff_doubles_to_the_caps() {
        let t = Timing::DEFAULT;
        let mut delay = t.reconnect_min;
        let mut seen = vec![delay.as_secs()];
        for _ in 0..7 {
            delay = t.next_reconnect(delay);
            seen.push(delay.as_secs());
        }
        assert_eq!(seen, [1, 2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(t.next_retry(None), Duration::from_millis(500));
        assert_eq!(
            t.next_retry(Some(Duration::from_millis(500))),
            Duration::from_secs(1)
        );
        assert_eq!(t.next_retry(Some(Duration::from_secs(4))), RETRY_MAX);
        assert_eq!(t.next_retry(Some(RETRY_MAX)), RETRY_MAX);
    }

    // ---- the worker, against a scripted bridge ----

    const FAST: Timing = Timing {
        reconnect_min: Duration::from_millis(5),
        reconnect_max: Duration::from_millis(20),
        retry_min: Duration::from_millis(5),
        retry_max: Duration::from_millis(20),
    };

    /// A bridge that answers from a list the test edits and delivers signals when the test says.
    #[derive(Clone)]
    struct Fake(Arc<FakeState>);

    #[derive(Default)]
    struct FakeState {
        windows: Mutex<Vec<ShellWindow>>,
        callbacks: Mutex<Vec<ShellCallback>>,
        lost: AtomicBool,
        lists: AtomicUsize,
        failing_lists: AtomicUsize,
        activated: Mutex<Vec<u64>>,
    }

    impl Fake {
        fn new(windows: Vec<ShellWindow>) -> Fake {
            let fake = Fake(Arc::default());
            fake.set(windows);
            fake
        }

        fn set(&self, windows: Vec<ShellWindow>) {
            *self.0.windows.lock().unwrap() = windows;
        }

        fn signal(&self, event: ShellEvent) {
            let callbacks = self.0.callbacks.lock().unwrap().clone();
            for callback in callbacks {
                callback(event.clone());
            }
        }

        fn changed(&self) {
            self.signal(ShellEvent::WindowsChanged { epoch: 1 });
        }

        /// What the real bridge does: fail calls fast, then tell the subscribers.
        fn lose(&self) {
            self.0.lost.store(true, Ordering::SeqCst);
            self.signal(ShellEvent::Lost);
        }

        fn lists(&self) -> usize {
            self.0.lists.load(Ordering::SeqCst)
        }

        fn watchers(&self) -> usize {
            self.0.callbacks.lock().unwrap().len()
        }
    }

    impl Shell for Fake {
        fn list(&self) -> Result<Vec<ShellWindow>, PlatformError> {
            self.0.lists.fetch_add(1, Ordering::SeqCst);
            if self.0.lost.load(Ordering::SeqCst) {
                return Err(PlatformError::Backend("Shell bridge lost".into()));
            }
            let failing = self.0.failing_lists.load(Ordering::SeqCst);
            if failing > 0 {
                self.0.failing_lists.store(failing - 1, Ordering::SeqCst);
                return Err(PlatformError::Timeout);
            }
            Ok(self.0.windows.lock().unwrap().clone())
        }

        fn focus(&self, id: u64) -> Result<(), PlatformError> {
            self.0.activated.lock().unwrap().push(id);
            if self.0.windows.lock().unwrap().iter().any(|w| w.id == id) {
                Ok(())
            } else {
                Err(PlatformError::NotFound)
            }
        }

        fn watch(&self, callback: ShellCallback) -> Result<(), PlatformError> {
            if self.0.lost.load(Ordering::SeqCst) {
                return Err(PlatformError::Backend("Shell bridge lost".into()));
            }
            self.0.callbacks.lock().unwrap().push(callback);
            Ok(())
        }
    }

    type Script = Arc<Mutex<VecDeque<Result<Fake, PlatformError>>>>;

    struct Rig {
        source: Source<Fake>,
        events: mpsc::Receiver<WindowEvent>,
        first: Fake,
        script: Script,
        connects: Arc<AtomicUsize>,
    }

    impl Rig {
        /// An unsubscribed source over `windows`; `reconnects` is what `connect` answers in turn
        /// (and "not running" once it runs out).
        fn new(
            windows: Vec<ShellWindow>,
            reconnects: Vec<Result<Fake, PlatformError>>,
        ) -> (Rig, mpsc::Sender<WindowEvent>) {
            let first = Fake::new(windows);
            let script: Script = Arc::new(Mutex::new(reconnects.into()));
            let connects = Arc::new(AtomicUsize::new(0));
            let connect: Connect<Fake> =
                {
                    let (script, connects) = (script.clone(), connects.clone());
                    Box::new(move || {
                        connects.fetch_add(1, Ordering::SeqCst);
                        script.lock().unwrap().pop_front().unwrap_or(Err(
                            PlatformError::Unsupported(
                                "the Crosspane Shell extension is not running",
                            ),
                        ))
                    })
                };
            let displays: DisplaysFn = Arc::new(one_display);
            let (tx, events) = mpsc::channel();
            let rig = Rig {
                source: Source::new(first.clone(), displays, connect, FAST),
                events,
                first,
                script,
                connects,
            };
            (rig, tx)
        }

        fn subscribed(
            windows: Vec<ShellWindow>,
            reconnects: Vec<Result<Fake, PlatformError>>,
        ) -> Rig {
            let (mut rig, tx) = Rig::new(windows, reconnects);
            rig.source.subscribe(sink(tx)).unwrap();
            rig
        }

        fn next(&self) -> WindowEvent {
            self.events.recv_timeout(WAIT).unwrap()
        }

        fn next_n(&self, n: usize) -> Vec<String> {
            ids(&(0..n).map(|_| self.next()).collect::<Vec<_>>())
        }

        fn assert_quiet(&self) {
            assert!(
                self.events.recv_timeout(QUIET).is_err(),
                "unexpected window event"
            );
        }
    }

    fn sink(tx: mpsc::Sender<WindowEvent>) -> Arc<dyn EventSink<WindowEvent>> {
        Arc::new(move |event: WindowEvent| {
            let _ = tx.send(event);
        })
    }

    fn focused(mut row: ShellWindow) -> ShellWindow {
        row.focused = true;
        row
    }

    #[test]
    fn subscribe_sends_the_current_windows_then_the_focus() {
        let rig = Rig::subscribed(
            vec![
                shell_window(2, 0, 0, 100, 100),
                focused(shell_window(1, 0, 0, 100, 100)),
            ],
            vec![],
        );
        assert_eq!(rig.next_n(3), ["+1", "+2", "focus 1"]);
        rig.assert_quiet();
    }

    #[test]
    fn subscribe_with_no_windows_sends_focus_none() {
        let rig = Rig::subscribed(vec![], vec![]);
        assert_eq!(rig.next_n(1), ["focus none"]);
    }

    #[test]
    fn windows_and_focused_read_the_snapshot_without_a_list_call() {
        let rig = Rig::subscribed(
            vec![
                shell_window(2, 0, 0, 100, 100),
                focused(shell_window(1, 0, 0, 100, 100)),
            ],
            vec![],
        );
        let lists = rig.first.lists();
        assert_eq!(lists, 1);
        let source = &rig.source;
        for _ in 0..20 {
            let windows = source
                .core
                .read(|s| s.windows.keys().map(|id| id.0).collect::<Vec<_>>());
            assert_eq!(windows.unwrap(), [1, 2]);
            assert_eq!(source.core.read(|s| s.focused).unwrap(), Some(WindowId(1)));
        }
        assert_eq!(rig.first.lists(), lists);
    }

    #[test]
    fn before_subscribe_each_read_is_one_uncached_list() {
        let (rig, _tx) = Rig::new(vec![shell_window(1, 0, 0, 100, 100)], vec![]);
        let before = rig.first.lists();
        let infos = rig.source.core.read(|s| s.windows.len()).unwrap();
        assert_eq!(infos, 1);
        rig.first.set(vec![]);
        // Not cached: the second read sees the change.
        assert_eq!(rig.source.core.read(|s| s.windows.len()).unwrap(), 0);
        assert_eq!(rig.first.lists(), before + 2);
    }

    #[test]
    fn subscribe_twice_is_refused_and_a_failed_subscribe_can_be_retried() {
        let (mut rig, tx) = Rig::new(vec![shell_window(1, 0, 0, 100, 100)], vec![]);
        rig.first.0.failing_lists.store(1, Ordering::SeqCst);
        // The first list fails: subscribe fails, nothing is published, nothing is sent.
        assert!(matches!(
            rig.source.subscribe(sink(tx.clone())),
            Err(PlatformError::Timeout)
        ));
        assert!(rig.source.core.live().snapshot.is_none());
        rig.assert_quiet();
        // A retry works; the callback of the failed attempt is inert.
        rig.source.subscribe(sink(tx.clone())).unwrap();
        assert_eq!(rig.next_n(2), ["+1", "focus none"]);
        assert_eq!(rig.first.watchers(), 2);
        assert!(matches!(
            rig.source.subscribe(sink(tx)),
            Err(PlatformError::Backend(_))
        ));
        rig.first.changed();
        rig.assert_quiet();
    }

    #[test]
    fn a_change_signal_diffs_against_the_snapshot() {
        let rig = Rig::subscribed(
            vec![
                focused(shell_window(1, 0, 0, 100, 100)),
                shell_window(2, 0, 0, 100, 100),
            ],
            vec![],
        );
        assert_eq!(rig.next_n(3), ["+1", "+2", "focus 1"]);
        rig.first.set(vec![
            shell_window(1, 0, 0, 100, 100),
            focused(shell_window(2, 40, 0, 100, 100)),
            shell_window(3, 0, 0, 100, 100),
        ]);
        rig.first.changed();
        assert_eq!(rig.next_n(3), ["~2", "+3", "focus 2"]);
        // The snapshot was already updated by the time the events were sent.
        assert_eq!(rig.source.core.read(|s| s.windows.len()).unwrap(), 3);
        rig.first.set(vec![shell_window(3, 0, 0, 100, 100)]);
        rig.first.changed();
        assert_eq!(rig.next_n(3), ["-1", "-2", "focus none"]);
        // An unchanged list sends nothing.
        rig.first.changed();
        rig.assert_quiet();
    }

    #[test]
    fn a_burst_of_signals_gives_one_change() {
        let rig = Rig::subscribed(vec![shell_window(1, 0, 0, 100, 100)], vec![]);
        assert_eq!(rig.next_n(2), ["+1", "focus none"]);
        rig.first.set(vec![shell_window(1, 7, 0, 100, 100)]);
        for _ in 0..50 {
            rig.first.changed();
        }
        assert_eq!(rig.next_n(1), ["~1"]);
        rig.assert_quiet();
    }

    #[test]
    fn a_failed_list_keeps_the_snapshot_and_is_retried_without_another_signal() {
        let rig = Rig::subscribed(vec![shell_window(1, 0, 0, 100, 100)], vec![]);
        assert_eq!(rig.next_n(2), ["+1", "focus none"]);
        rig.first.set(vec![]);
        rig.first.0.failing_lists.store(2, Ordering::SeqCst);
        rig.first.changed();
        // The two failures removed nothing; the third list sees the window gone.
        assert_eq!(rig.next_n(1), ["-1"]);
        assert_eq!(rig.first.0.failing_lists.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_lost_bridge_removes_every_window_then_reconnects_and_adds_again() {
        let second = Fake::new(vec![
            shell_window(1, 10, 10, 200, 200),
            focused(shell_window(4, 0, 0, 100, 100)),
        ]);
        let rig = Rig::subscribed(
            vec![
                shell_window(1, 0, 0, 100, 100),
                focused(shell_window(2, 0, 0, 100, 100)),
            ],
            vec![
                Err(PlatformError::Unsupported("not running")),
                Err(PlatformError::Backend("busy".into())),
                Ok(second.clone()),
            ],
        );
        assert_eq!(rig.next_n(3), ["+1", "+2", "focus 2"]);
        rig.first.lose();
        // Removed for every window, and the focus goes with them; the lost bridge is dropped.
        assert_eq!(rig.next_n(3), ["-1", "-2", "focus none"]);
        // The new epoch's windows come as Added, even where an id matches an old one.
        assert_eq!(rig.next_n(3), ["+1", "+4", "focus 4"]);
        assert_eq!(rig.connects.load(Ordering::SeqCst), 3);
        assert!(rig.script.lock().unwrap().is_empty());
        assert_eq!(rig.source.core.read(|s| s.windows.len()).unwrap(), 2);
        // The new bridge's signals are followed; the old bridge's are not.
        second.set(vec![shell_window(4, 0, 0, 100, 100)]);
        rig.first.changed();
        rig.assert_quiet();
        second.changed();
        assert_eq!(rig.next_n(2), ["-1", "focus none"]);
        // Signals from a bridge that was replaced can't start another recovery.
        rig.first.lose();
        rig.assert_quiet();
    }

    #[test]
    fn while_lost_reads_fail_and_activate_is_refused() {
        // The script is empty, so every reconnect attempt fails until the test hands one over.
        let (mut rig, tx) = Rig::new(vec![shell_window(1, 0, 0, 100, 100)], vec![]);
        rig.source.subscribe(sink(tx)).unwrap();
        assert_eq!(rig.next_n(2), ["+1", "focus none"]);
        rig.first.lose();
        assert_eq!(rig.next_n(1), ["-1"]);
        assert!(matches!(
            rig.source.core.read(|s| s.windows.len()),
            Err(PlatformError::Backend(_))
        ));
        assert!(matches!(
            rig.source.core.read(|s| s.focused),
            Err(PlatformError::Backend(_))
        ));
        // An id from before the loss is not passed on to any bridge.
        assert!(matches!(
            rig.source.core.activate(WindowId(1)),
            Err(PlatformError::Backend(_))
        ));
        assert!(rig.first.0.activated.lock().unwrap().is_empty());
        assert!(rig.source.core.live().bridge.is_none());
        let deadline = Instant::now() + WAIT;
        while rig.connects.load(Ordering::SeqCst) < 3 {
            assert!(Instant::now() < deadline, "no reconnect attempts");
            thread::sleep(Duration::from_millis(2));
        }
        rig.script
            .lock()
            .unwrap()
            .push_back(Ok(Fake::new(vec![shell_window(3, 0, 0, 10, 10)])));
        assert_eq!(rig.next_n(2), ["+3", "focus none"]);
        assert!(rig.source.core.live().bridge.is_some());
        assert_eq!(rig.source.core.read(|s| s.windows.len()).unwrap(), 1);
    }

    #[test]
    fn activate_reaches_the_bridge_only_for_a_listed_window() {
        let rig = Rig::subscribed(
            vec![
                shell_window(1, 0, 0, 100, 100),
                shell_window(2, 0, 0, 100, 100),
            ],
            vec![],
        );
        assert_eq!(rig.next_n(3), ["+1", "+2", "focus none"]);
        rig.source.core.activate(WindowId(2)).unwrap();
        assert_eq!(*rig.first.0.activated.lock().unwrap(), [2]);
        // Not in the snapshot: refused without a call, even if the bridge would know it.
        rig.first.set(vec![shell_window(9, 0, 0, 10, 10)]);
        assert!(matches!(
            rig.source.core.activate(WindowId(9)),
            Err(PlatformError::NotFound)
        ));
        assert_eq!(*rig.first.0.activated.lock().unwrap(), [2]);
        // The bridge's own `NotFound` passes through (the window closed since the last refresh).
        rig.first.set(vec![]);
        assert!(matches!(
            rig.source.core.activate(WindowId(2)),
            Err(PlatformError::NotFound)
        ));
        assert_eq!(*rig.first.0.activated.lock().unwrap(), [2, 2]);
    }

    #[test]
    fn activate_before_subscribe_asks_the_bridge() {
        let (rig, _tx) = Rig::new(vec![shell_window(5, 0, 0, 10, 10)], vec![]);
        rig.source.core.activate(WindowId(5)).unwrap();
        assert!(matches!(
            rig.source.core.activate(WindowId(6)),
            Err(PlatformError::NotFound)
        ));
        assert_eq!(*rig.first.0.activated.lock().unwrap(), [5, 6]);
    }

    #[test]
    fn dropping_the_source_stops_the_worker_and_releases_the_sink() {
        let rig = Rig::subscribed(vec![shell_window(1, 0, 0, 100, 100)], vec![]);
        let Rig {
            source,
            events,
            first,
            ..
        } = rig;
        assert_eq!(ids(&[events.recv_timeout(WAIT).unwrap()]), ["+1"]);
        drop(source);
        // The worker was joined and its sink dropped: the channel is closed, and a late signal on
        // the still-registered callback does nothing.
        first.changed();
        first.lose();
        let mut rest = Vec::new();
        while let Ok(event) = events.recv_timeout(QUIET) {
            rest.push(event);
        }
        assert_eq!(ids(&rest), ["focus none"]);
        assert!(matches!(
            events.recv_timeout(QUIET),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn dropping_while_reconnecting_does_not_wait_out_the_backoff() {
        let mut timing = FAST;
        timing.reconnect_min = Duration::from_secs(60);
        timing.reconnect_max = Duration::from_secs(60);
        let first = Fake::new(vec![shell_window(1, 0, 0, 100, 100)]);
        let connect: Connect<Fake> = Box::new(|| Err(PlatformError::Timeout));
        let displays: DisplaysFn = Arc::new(one_display);
        let mut source = Source::new(first.clone(), displays, connect, timing);
        let (tx, events) = mpsc::channel();
        source.subscribe(sink(tx)).unwrap();
        events.recv_timeout(WAIT).unwrap();
        events.recv_timeout(WAIT).unwrap();
        first.lose();
        assert!(matches!(
            events.recv_timeout(WAIT).unwrap(),
            WindowEvent::Removed(_)
        ));
        // Give the worker time to enter its 60 s backoff.
        thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        drop(source);
        assert!(started.elapsed() < Duration::from_secs(30));
    }
}
