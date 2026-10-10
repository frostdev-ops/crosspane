//! `FrameCapture` of single windows on GNOME (WP-G2.2/G2.3a): a window identified by the Shell
//! bridge, captured from its monitor's ScreenCast stream and cropped to the window.
//!
//! Window identity comes from the bridge (`WindowId` = Shell id, `ShellEpoch`-qualified), never
//! from portal metadata (WP-G0.1 rulings). This is the M1 mirror path: the window stays visible on
//! its source monitor, so whatever covers it is captured too (the reported fallback).
//!
//! - `start(Display(id), ..)` passes through to the inner [`PortalScreenCast`] unchanged.
//! - `start(Window(id), crop, max_fps, sink)`: look the window up (bridge `ListWindows`, 2 s);
//!   unknown is `NotFound`. Pick the display containing the window frame's centre (displays
//!   snapshot; none is `NotFound`), convert the frame to device pixels on that display
//!   (logical − display origin, × scale, rounded; clamped to the display), intersect with the
//!   caller's `crop` if given (the caller's crop is relative to the window's content, i.e. it is
//!   offset by the window's device-pixel origin), and start an inner `Display` stream with that
//!   crop. Returns an outer `StreamId` of this adapter's own numbering.
//! - **Following the window.** On every bridge `WindowsChanged` (coalesced on one worker), re-read
//!   the window: a new rect on the same display updates the inner crop (`set_crop`); a window now
//!   on another display stops the inner stream and starts one on the new display under the same
//!   outer `StreamId` (frames keep flowing to the same sink); a window that is gone, or a bridge
//!   `Lost`, ends the outer stream with the frozen "source ended" reason (`StreamEndReason`, the
//!   variant the Hyprland backend uses when the captured window closes: `TargetGone`).
//! - `set_crop(outer, crop)`: store the caller crop and re-apply it against the current window
//!   rect. `stop(outer)`: stop the inner stream, emit nothing extra beyond what the inner stream
//!   emits (`Ended { Requested }` exactly once, through the same sink), forget the window.
//! - Events from inner streams are forwarded to the caller's sink, except an inner `Ended` caused
//!   by a display switch, which is swallowed.
//! - The gate is the inner capture's business (it ends streams with `Blocked`).
//!
//! # Design
//!
//! - **Stream ids.** A window stream's outer id is this adapter's own number, counted from
//!   `OUTER_BASE` (2^62). The inner capture hands out small counters, and a `Display` stream
//!   keeps the inner's id, so the two ranges can never collide and `stop`/`set_crop` tell them
//!   apart by the id alone: an id below the base goes to the inner untouched.
//! - **One lock for the streams.** `state` holds the inner capture and the outer-to-inner map
//!   together, so every operation (start, crop, stop, a follow step) is one atomic change of both.
//!   It is held across inner calls, each of which the inner bounds at 2 s. It is never held across
//!   a bridge call: the worker lists windows first, then takes the lock to apply the result. The
//!   sinks of inner streams never take it.
//! - **One worker.** `new` subscribes a callback that only sets flags in a `Mailbox` (it neither
//!   blocks the bridge's signal thread nor holds the bridge, so there is no reference cycle). The
//!   worker waits on the mailbox, so any number of `WindowsChanged` signals that arrive while it is
//!   busy cost one more `ListWindows`. With no window stream there is no call at all. A failed list
//!   is not evidence that a window closed: streams are left as they are and the list is retried
//!   after 0.5 s, doubling to 5 s.
//! - **Loss.** `Lost` has priority. Every window stream ends with `TargetGone` (their ids belong to
//!   a Shell that is gone), then the worker reconnects with 1 s, 2 s, … 30 s between attempts.
//!   While it has no bridge `start(Window)` fails with `Backend`. A stream is bound to the bridge
//!   generation its window was looked up on and is refused (not inserted) if the bridge was
//!   replaced meanwhile, so an id of an old epoch is never followed on the new one.
//! - **Sinks.** Each inner stream gets its own `Link` (retired when the adapter replaces or ends
//!   that stream, which makes it drop everything, `Ended` included) over the outer stream's
//!   `Outer`. `Outer` addresses events with the outer id, serialises every send to the caller's
//!   sink under one adapter-wide lock (the `EventSink` contract asks a multi-threaded backend to),
//!   and delivers `Ended` exactly once; nothing follows it. A display switch retires the old link
//!   first, so its `Ended { Requested }` is swallowed. A stream that the inner ends by itself
//!   (`Blocked`, `Failed`, …) forwards that `Ended` once and is forgotten at the next worker pass.
//! - **Errors of a switch.** The old stream is stopped before the new one starts. If the new one
//!   cannot start the outer stream ends: `Blocked` when the gate is closed, `TargetGone` when the
//!   new display has no ScreenCast stream, `Failed` otherwise; the engine restarts the capture.
//! - **A window that cannot be placed** (no display holds its centre, no visible area, the caller's
//!   crop lies outside it) keeps the last crop until the next change. A minimized window is not
//!   special-cased: the Shell keeps its frame, so the monitor stream shows what is behind it. The
//!   engine learns the window's state from the `WindowSource` and decides what to project.
//! - **Bounds.** Nothing waits without a limit. The bridge bounds `ListWindows` and the inner
//!   bounds each of its calls at 2 s, and no lock is held across a bridge call. Calls take the one
//!   state lock, so they can queue behind a single step of the worker: in the worst case (a slow
//!   bridge and a slow inner) `set_crop` and `stop` take about 4 s and `start(Window)` about 6 s,
//!   and `set_crop` and `stop` never wait on the bridge.
//! - Window titles are never read or logged here, only ids and counts.

mod geometry;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crosspane_platform::{
    CaptureTarget, EventSink, FrameCapture, FrameEvent, PlatformError, StreamEndReason, StreamId,
};
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{PixelRect, PointLogical, RectLogical, SizeLogical};
use crosspane_types::id::{DisplayId, WindowId};

use self::geometry::{compose_crop, device_rect, display_of};
use super::shell::{ShellBridge, ShellCallback, ShellEvent, ShellWindow};
use crate::portal::eis::DisplaysFn;
use crate::portal::screencast::PortalScreenCast;

/// The first outer stream id. See the module documentation.
const OUTER_BASE: u64 = 1 << 62;
/// Waits between reconnect attempts: this, doubling, up to [`Timing::reconnect_max`].
const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
/// Waits before the retry of a failed list: this, doubling, up to [`Timing::retry_max`].
const RETRY_MIN: Duration = Duration::from_millis(500);
const RETRY_MAX: Duration = Duration::from_secs(5);

/// Window capture on GNOME: a monitor stream cropped to a bridge-identified window.
pub struct GnomeWindowCapture {
    capture: Capture<PortalScreenCast, ShellBridge>,
}

impl fmt::Debug for GnomeWindowCapture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GnomeWindowCapture").finish_non_exhaustive()
    }
}

impl GnomeWindowCapture {
    /// Takes an already connected bridge: subscribes to it (bounded at 2 s by the bridge) and
    /// starts the worker. After a `Lost` bridge the worker reconnects with
    /// [`ShellBridge::connect`].
    pub fn new(
        inner: PortalScreenCast,
        bridge: ShellBridge,
        displays: DisplaysFn,
    ) -> Result<GnomeWindowCapture, PlatformError> {
        Ok(GnomeWindowCapture {
            capture: Capture::new(
                inner,
                bridge,
                displays,
                Box::new(ShellBridge::connect),
                Timing::DEFAULT,
            )?,
        })
    }
}

impl FrameCapture for GnomeWindowCapture {
    fn start(
        &mut self,
        target: CaptureTarget,
        crop: Option<PixelRect>,
        max_fps: u32,
        sink: Arc<dyn EventSink<FrameEvent>>,
    ) -> Result<StreamId, PlatformError> {
        self.capture.start(target, crop, max_fps, sink)
    }

    fn set_crop(&mut self, stream: StreamId, crop: Option<PixelRect>) -> Result<(), PlatformError> {
        self.capture.set_crop(stream, crop)
    }

    fn stop(&mut self, stream: StreamId) -> Result<(), PlatformError> {
        self.capture.stop(stream)
    }
}

/// What this adapter needs from a bridge. [`ShellBridge`] is the only real implementation; the
/// tests script a fake one, since the real one needs a session bus and a Shell.
trait Bridge: Clone + Send + Sync + 'static {
    fn list(&self) -> Result<Vec<ShellWindow>, PlatformError>;
    fn watch(&self, callback: ShellCallback) -> Result<(), PlatformError>;
}

impl Bridge for ShellBridge {
    fn list(&self) -> Result<Vec<ShellWindow>, PlatformError> {
        self.list_windows()
    }

    fn watch(&self, callback: ShellCallback) -> Result<(), PlatformError> {
        self.subscribe(callback)
    }
}

/// Opens a new connection to the bridge after a loss.
type Connect<B> = Box<dyn Fn() -> Result<B, PlatformError> + Send + Sync>;

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

/// The capture and the handle of its worker, which `Drop` stops and joins so the caller's sinks
/// are never called by the worker after the adapter is gone.
struct Capture<I: FrameCapture + 'static, B: Bridge> {
    core: Arc<Core<I, B>>,
    worker: Option<JoinHandle<()>>,
}

impl<I: FrameCapture + 'static, B: Bridge> Capture<I, B> {
    fn new(
        inner: I,
        bridge: B,
        displays: DisplaysFn,
        connect: Connect<B>,
        timing: Timing,
    ) -> Result<Capture<I, B>, PlatformError> {
        let core = Arc::new(Core {
            state: Mutex::new(State {
                inner,
                streams: BTreeMap::new(),
                next: 0,
            }),
            live: Mutex::new(Live { bridge: None }),
            mail: Mailbox::default(),
            emit: Arc::new(Mutex::new(())),
            displays,
            connect,
            timing,
        });
        core.attach(bridge)?;
        let worker = {
            let core = core.clone();
            thread::Builder::new()
                .name("crosspane-gnome-capture".into())
                .spawn(move || core.run())
                .map_err(|e| PlatformError::Backend(format!("spawn GNOME capture thread: {e}")))?
        };
        Ok(Capture {
            core,
            worker: Some(worker),
        })
    }

    fn start(
        &mut self,
        target: CaptureTarget,
        crop: Option<PixelRect>,
        max_fps: u32,
        sink: Arc<dyn EventSink<FrameEvent>>,
    ) -> Result<StreamId, PlatformError> {
        match target {
            CaptureTarget::Display(_) => lock(&self.core.state)
                .inner
                .start(target, crop, max_fps, sink),
            CaptureTarget::Window(window) => self.start_window(window, crop, max_fps, sink),
            _ => Err(PlatformError::Unsupported(
                "GNOME window capture: unknown capture target",
            )),
        }
    }

    /// Looks the window up on the bridge (no lock held), then starts the inner stream and records
    /// it under one hold of the state lock.
    fn start_window(
        &mut self,
        window: WindowId,
        crop: Option<PixelRect>,
        max_fps: u32,
        sink: Arc<dyn EventSink<FrameEvent>>,
    ) -> Result<StreamId, PlatformError> {
        check_crop(crop)?;
        if max_fps == 0 {
            return Err(PlatformError::Backend(
                "GNOME window capture: max_fps must be positive".into(),
            ));
        }
        let core = &self.core;
        let (bridge, generation) = core.current().ok_or_else(unavailable)?;
        let list = bridge.list()?;
        let row = list
            .iter()
            .find(|row| row.id == window.0)
            .ok_or(PlatformError::NotFound)?;
        let place = place_of(row, &(core.displays)()).map_err(|unplaced| match unplaced {
            Unplaced::NoDisplay => PlatformError::NotFound,
            Unplaced::NoArea => PlatformError::Backend(
                "GNOME window capture: the window has no visible area".into(),
            ),
        })?;
        let first = compose_crop(place.rect, crop).ok_or_else(outside_window)?;

        let mut guard = lock(&core.state);
        let State {
            inner,
            streams,
            next,
        } = &mut *guard;
        let id = StreamId(OUTER_BASE + *next);
        let outer = Arc::new(Outer {
            id,
            sink,
            emit: core.emit.clone(),
            ended: AtomicBool::new(false),
        });
        let link = Arc::new(Link {
            outer: outer.clone(),
            retired: AtomicBool::new(false),
        });
        let started = inner.start(
            CaptureTarget::Display(place.display),
            Some(first),
            max_fps,
            link.clone(),
        )?;
        *next += 1;
        // The bridge may have been replaced while the stream started: the id belongs to a Shell
        // that is gone, and `recover` has already emptied the map. Checked under the state lock,
        // so a replacement after this point finds the entry and ends it.
        if core.current().map(|(_, now)| now) != Some(generation) {
            link.retire();
            if let Err(error) = inner.stop(started) {
                tracing::debug!(%error, "GNOME window capture: stop after a lost bridge failed");
            }
            return Err(unavailable());
        }
        streams.insert(
            id,
            Entry {
                window: window.0,
                caller: crop,
                max_fps,
                place,
                applied: first,
                inner: started,
                link,
                outer,
            },
        );
        drop(guard);
        // The window may have moved since the list was taken; one pass settles it.
        core.mail.poke();
        Ok(id)
    }

    fn set_crop(&mut self, stream: StreamId, crop: Option<PixelRect>) -> Result<(), PlatformError> {
        let mut guard = lock(&self.core.state);
        let State { inner, streams, .. } = &mut *guard;
        if !is_window_stream(stream) {
            return inner.set_crop(stream, crop);
        }
        check_crop(crop)?;
        let entry = streams
            .get_mut(&stream)
            .filter(|entry| !entry.outer.is_ended())
            .ok_or(PlatformError::NotFound)?;
        let composed = compose_crop(entry.place.rect, crop).ok_or_else(outside_window)?;
        if composed != entry.applied {
            inner.set_crop(entry.inner, Some(composed))?;
            entry.applied = composed;
        }
        entry.caller = crop;
        Ok(())
    }

    fn stop(&mut self, stream: StreamId) -> Result<(), PlatformError> {
        let mut guard = lock(&self.core.state);
        let State { inner, streams, .. } = &mut *guard;
        if !is_window_stream(stream) {
            return inner.stop(stream);
        }
        // A stream that is not in the map is already over (ended, or stopped before): nothing to do.
        let Some(entry) = streams.remove(&stream) else {
            return Ok(());
        };
        stop_requested(inner, entry)
    }
}

impl<I: FrameCapture + 'static, B: Bridge> Drop for Capture<I, B> {
    fn drop(&mut self) {
        self.core.mail.stop();
        if let Some(worker) = self.worker.take()
            // The sink could own the last handle to this adapter, which would then be dropped on
            // the worker itself; joining oneself never returns.
            && worker.thread().id() != thread::current().id()
        {
            let _ = worker.join();
        }
        // Whatever is still running ends once, as if it had been stopped.
        let mut guard = lock(&self.core.state);
        let State { inner, streams, .. } = &mut *guard;
        for entry in std::mem::take(streams).into_values() {
            let _ = stop_requested(inner, entry);
        }
    }
}

impl<I: FrameCapture + 'static, B: Bridge> fmt::Debug for Capture<I, B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Capture").finish_non_exhaustive()
    }
}

/// What the trait methods and the worker share.
///
/// The bridge callback holds only the mailbox, never the core: the core owns the bridge, which owns
/// the callback, so a reference back to the core would be a cycle.
struct Core<I: FrameCapture, B: Bridge> {
    state: Mutex<State<I>>,
    live: Mutex<Live<B>>,
    mail: Mailbox,
    /// Serialises every send to a caller's sink; see [`Outer`].
    emit: Arc<Mutex<()>>,
    displays: DisplaysFn,
    connect: Connect<B>,
    timing: Timing,
}

/// The inner capture and the window streams built on it.
struct State<I> {
    inner: I,
    streams: BTreeMap<StreamId, Entry>,
    /// The next outer stream number, counted from [`OUTER_BASE`].
    next: u64,
}

struct Live<B> {
    /// The connected bridge and its generation; `None` between a loss and the reconnect.
    bridge: Option<(B, u64)>,
}

/// One window stream.
struct Entry {
    /// The bridge's window id.
    window: u64,
    /// The caller's crop, relative to the window.
    caller: Option<PixelRect>,
    max_fps: u32,
    /// Where the window was at the last step, and the crop that was applied for it.
    place: Place,
    applied: PixelRect,
    /// The current inner stream and its sink.
    inner: StreamId,
    link: Arc<Link>,
    outer: Arc<Outer>,
}

impl Entry {
    fn follow(&self) -> Follow {
        Follow {
            place: self.place,
            caller: self.caller,
            applied: self.applied,
        }
    }
}

/// The outer stream's side of the sink: shared by every inner stream the outer stream has.
struct Outer {
    id: StreamId,
    sink: Arc<dyn EventSink<FrameEvent>>,
    /// The adapter-wide send lock. Held across `sink.send`, which never blocks (the `EventSink`
    /// contract) and never reaches back into this adapter.
    emit: Arc<Mutex<()>>,
    ended: AtomicBool,
}

impl Outer {
    fn is_ended(&self) -> bool {
        self.ended.load(Ordering::SeqCst)
    }

    /// Sends the event `build` makes for this stream, unless the stream is over.
    fn forward(&self, build: impl FnOnce(StreamId) -> FrameEvent) {
        let _serial = lock(&self.emit);
        if !self.is_ended() {
            self.sink.send(build(self.id));
        }
    }

    /// Ends the stream: the first call sends `Ended { reason }`, later ones do nothing, and no
    /// event follows.
    fn end(&self, reason: StreamEndReason) {
        let _serial = lock(&self.emit);
        if !self.ended.swap(true, Ordering::SeqCst) {
            self.sink.send(FrameEvent::Ended {
                stream: self.id,
                reason,
            });
        }
    }
}

/// The sink of one inner stream: it re-addresses the inner's events to the outer stream.
struct Link {
    outer: Arc<Outer>,
    /// Set when the adapter replaces or ends this inner stream by itself: from then on nothing it
    /// says is forwarded, its `Ended { Requested }` included.
    retired: AtomicBool,
}

impl Link {
    fn retire(&self) {
        self.retired.store(true, Ordering::SeqCst);
    }
}

impl EventSink<FrameEvent> for Link {
    fn send(&self, event: FrameEvent) {
        if self.retired.load(Ordering::SeqCst) {
            return;
        }
        match event {
            FrameEvent::Frame { frame, .. } => self
                .outer
                .forward(|stream| FrameEvent::Frame { stream, frame }),
            FrameEvent::Ended { reason, .. } => self.outer.end(reason),
            FrameEvent::Cursor { cursor, .. } => self
                .outer
                .forward(|stream| FrameEvent::Cursor { stream, cursor }),
            FrameEvent::CursorDefault { .. } => self
                .outer
                .forward(|stream| FrameEvent::CursorDefault { stream }),
            // An event this adapter does not know can't be re-addressed to the outer stream.
            _ => {}
        }
    }
}

impl<I: FrameCapture, B: Bridge> Core<I, B> {
    /// The connected bridge and its generation.
    fn current(&self) -> Option<(B, u64)> {
        lock(&self.live).bridge.clone()
    }

    fn bridge(&self) -> Option<B> {
        self.current().map(|(bridge, _)| bridge)
    }

    /// Registers the mailbox callback on `bridge` under a new generation and makes it current.
    fn attach(&self, bridge: B) -> Result<(), PlatformError> {
        let generation = self.mail.next_generation();
        let mail = self.mail.shared.clone();
        bridge.watch(Arc::new(move |event: ShellEvent| {
            mail.push(generation, &event);
        }))?;
        lock(&self.live).bridge = Some((bridge, generation));
        Ok(())
    }

    /// The worker: follows the mailbox until the adapter is dropped.
    fn run(&self) {
        let mut retry: Option<Duration> = None;
        loop {
            match self.mail.wait(retry) {
                Wake::Stop => return,
                Wake::Lost => {
                    if !self.recover() {
                        return;
                    }
                    retry = None;
                }
                Wake::Changed | Wake::Timer => retry = self.refresh(retry),
            }
        }
    }

    /// One `ListWindows` for every window stream, each then followed. Returns the wait before the
    /// next attempt when a list or a crop update failed.
    fn refresh(&self, retry: Option<Duration>) -> Option<Duration> {
        let ids: Vec<StreamId> = {
            let mut state = lock(&self.state);
            state.streams.retain(|_, entry| !entry.outer.is_ended());
            state.streams.keys().copied().collect()
        };
        if ids.is_empty() {
            return None;
        }
        // Taken before the list: a stream started later is not judged by an older list.
        let bridge = self.bridge()?;
        let list = match bridge.list() {
            Ok(list) => list,
            Err(error) => {
                // Not evidence that a window closed. `Lost` takes priority when it is the cause.
                if retry.is_none() {
                    tracing::warn!(%error, "GNOME capture: window list failed; retrying");
                } else {
                    tracing::debug!(%error, "GNOME capture: window list failed again");
                }
                return Some(self.timing.next_retry(retry));
            }
        };
        let displays = (self.displays)();
        let mut again = false;
        for id in ids {
            again |= self.follow(id, &list, &displays);
        }
        again.then(|| self.timing.next_retry(retry))
    }

    /// Applies what one list says about one window stream. Returns true when it should be tried
    /// again later.
    fn follow(&self, id: StreamId, list: &[ShellWindow], displays: &[DisplayInfo]) -> bool {
        let mut guard = lock(&self.state);
        let State { inner, streams, .. } = &mut *guard;
        let Some(entry) = streams.get_mut(&id) else {
            return false;
        };
        if entry.outer.is_ended() {
            streams.remove(&id);
            return false;
        }
        let window = list.iter().find(|row| row.id == entry.window);
        match decide(&entry.follow(), window, displays) {
            Update::Keep | Update::Hold => false,
            Update::Crop { place, crop } => {
                if crop != entry.applied {
                    match inner.set_crop(entry.inner, Some(crop)) {
                        Ok(()) => {}
                        // The inner stream is gone and its `Ended` is on its way.
                        Err(PlatformError::NotFound) => return false,
                        Err(error) => {
                            tracing::debug!(%error, "GNOME capture: crop update failed; retrying");
                            return true;
                        }
                    }
                }
                entry.place = place;
                entry.applied = crop;
                false
            }
            Update::Gone => {
                if let Some(entry) = streams.remove(&id) {
                    end_by_adapter(inner, entry, StreamEndReason::TargetGone);
                }
                false
            }
            Update::Switch { place, crop } => {
                let Some(old) = streams.remove(&id) else {
                    return false;
                };
                // Retired first: the old stream's `Ended { Requested }` and any late frame are
                // dropped, and the frames of the new stream follow under the same outer id.
                old.link.retire();
                if let Err(error) = inner.stop(old.inner) {
                    tracing::debug!(%error, "GNOME capture: stopping the old display stream failed");
                }
                if old.outer.is_ended() {
                    return false;
                }
                let link = Arc::new(Link {
                    outer: old.outer.clone(),
                    retired: AtomicBool::new(false),
                });
                match inner.start(
                    CaptureTarget::Display(place.display),
                    Some(crop),
                    old.max_fps,
                    link.clone(),
                ) {
                    Ok(started) => {
                        streams.insert(
                            id,
                            Entry {
                                place,
                                applied: crop,
                                inner: started,
                                link,
                                ..old
                            },
                        );
                    }
                    Err(error) => {
                        let reason = end_reason(&error);
                        tracing::info!(%error, ?reason, "GNOME capture: window moved to a display that can't be captured");
                        old.outer.end(reason);
                    }
                }
                false
            }
        }
    }

    /// The bridge is lost: every window stream ends, then the bridge is reconnected with backoff.
    /// Returns false when the adapter was dropped meanwhile.
    fn recover(&self) -> bool {
        tracing::info!("Shell bridge lost; window streams ended, reconnecting");
        // Replaced first, so a `start` that is in flight sees the change when it inserts.
        let old = lock(&self.live).bridge.take();
        // Outside the lock: dropping the last clone closes the bridge's signal connection.
        drop(old);
        {
            let mut guard = lock(&self.state);
            let State { inner, streams, .. } = &mut *guard;
            for entry in std::mem::take(streams).into_values() {
                end_by_adapter(inner, entry, StreamEndReason::TargetGone);
            }
        }
        let mut delay = self.timing.reconnect_min;
        loop {
            if self.mail.sleep(delay) {
                return false;
            }
            match (self.connect)().and_then(|bridge| self.attach(bridge)) {
                Ok(()) => {
                    tracing::info!("Shell bridge reconnected");
                    return true;
                }
                Err(error) => {
                    tracing::debug!(%error, "Shell bridge reconnect failed");
                    delay = self.timing.next_reconnect(delay);
                }
            }
        }
    }
}

/// Ends `entry` on the caller's request. The inner stream's own `Ended { Requested }` goes out
/// through its live link; if the inner stream does not send one (it had ended already, or the stop
/// failed) the stream still ends once, now.
fn stop_requested<I: FrameCapture>(inner: &mut I, entry: Entry) -> Result<(), PlatformError> {
    let result = if entry.outer.is_ended() {
        Ok(())
    } else {
        inner.stop(entry.inner)
    };
    entry.link.retire();
    entry.outer.end(StreamEndReason::Requested);
    result
}

/// Ends `entry` because of the window or the bridge: the stream's end is `reason`, and the inner
/// stream's own `Ended { Requested }` is swallowed.
fn end_by_adapter<I: FrameCapture>(inner: &mut I, entry: Entry, reason: StreamEndReason) {
    entry.link.retire();
    let live = !entry.outer.is_ended();
    entry.outer.end(reason);
    if live && let Err(error) = inner.stop(entry.inner) {
        tracing::debug!(%error, "GNOME capture: stopping an inner stream failed");
    }
}

/// How a failed start of the inner stream on a new display ends the outer stream.
fn end_reason(error: &PlatformError) -> StreamEndReason {
    match error {
        PlatformError::Locked => StreamEndReason::Blocked,
        PlatformError::NotFound => StreamEndReason::TargetGone,
        _ => StreamEndReason::Failed,
    }
}

fn is_window_stream(stream: StreamId) -> bool {
    stream.0 >= OUTER_BASE
}

/// A crop the inner would refuse anyway, refused before any work.
fn check_crop(crop: Option<PixelRect>) -> Result<(), PlatformError> {
    if crop.is_some_and(|crop| crop.is_empty()) {
        Err(PlatformError::Backend(
            "GNOME window capture: crop must be nonempty".into(),
        ))
    } else {
        Ok(())
    }
}

fn outside_window() -> PlatformError {
    PlatformError::Backend("GNOME window capture: the crop lies outside the window".into())
}

fn unavailable() -> PlatformError {
    PlatformError::Backend("Shell bridge lost; reconnecting".into())
}

/// Locks `mutex`. Every update is a whole step on plain data, so a poisoned lock is taken anyway.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---------------------------------------------------------------------------------------------
// Pure decisions
// ---------------------------------------------------------------------------------------------

/// Where a window is on the displays: the display holding its frame's centre and the window's rect
/// in that display's device pixels (clamped to it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Place {
    display: DisplayId,
    rect: PixelRect,
}

/// Why a window has no [`Place`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unplaced {
    /// No display contains the frame's centre.
    NoDisplay,
    /// The frame has no visible area on its display.
    NoArea,
}

fn place_of(window: &ShellWindow, displays: &[DisplayInfo]) -> Result<Place, Unplaced> {
    // The bridge sends signed sizes; a negative one can't be a frame.
    let frame = RectLogical::new(
        PointLogical::new(f64::from(window.x), f64::from(window.y)),
        SizeLogical::new(
            f64::from(window.width.max(0)),
            f64::from(window.height.max(0)),
        ),
    );
    let display = display_of(&frame, displays).ok_or(Unplaced::NoDisplay)?;
    let rect = device_rect(&frame, &display.geometry).ok_or(Unplaced::NoArea)?;
    Ok(Place {
        display: display.id,
        rect,
    })
}

/// What a stream is doing now, the input of [`decide`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Follow {
    place: Place,
    caller: Option<PixelRect>,
    applied: PixelRect,
}

/// What one window update asks of a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Update {
    /// Same place, same crop.
    Keep,
    /// Same display, a new rect or crop.
    Crop { place: Place, crop: PixelRect },
    /// Another display: restart the inner stream there.
    Switch { place: Place, crop: PixelRect },
    /// The window is not in the list any more.
    Gone,
    /// The window can't be placed or the crop doesn't fit it just now: keep the stream as it is.
    Hold,
}

/// The decision for a stream that is `current`, given the bridge's row for its window (`None`: not
/// listed) and the displays snapshot.
fn decide(current: &Follow, window: Option<&ShellWindow>, displays: &[DisplayInfo]) -> Update {
    let Some(window) = window else {
        return Update::Gone;
    };
    let Ok(place) = place_of(window, displays) else {
        return Update::Hold;
    };
    let Some(crop) = compose_crop(place.rect, current.caller) else {
        return Update::Hold;
    };
    if place.display != current.place.display {
        Update::Switch { place, crop }
    } else if place == current.place && crop == current.applied {
        Update::Keep
    } else {
        Update::Crop { place, crop }
    }
}

// ---------------------------------------------------------------------------------------------
// Mailbox
// ---------------------------------------------------------------------------------------------

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
    shared: Arc<Shared>,
}

#[derive(Default)]
struct Shared {
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

impl Shared {
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
}

impl Mailbox {
    /// Starts the next generation: pending flags belong to the bridge before and are cleared.
    fn next_generation(&self) -> u64 {
        let mut mail = lock(&self.shared.mail);
        mail.generation += 1;
        mail.changed = false;
        mail.lost = false;
        mail.generation
    }

    /// Asks for one more pass, as if the bridge had signalled a change.
    fn poke(&self) {
        let mut mail = lock(&self.shared.mail);
        if !mail.stop {
            mail.changed = true;
            self.shared.wake.notify_all();
        }
    }

    fn stop(&self) {
        lock(&self.shared.mail).stop = true;
        self.shared.wake.notify_all();
    }

    /// Blocks until something is pending (or `timeout` runs out). `Changed` is consumed; `Lost`
    /// stays set until the next generation starts.
    fn wait(&self, timeout: Option<Duration>) -> Wake {
        let idle = |mail: &mut Mail| !(mail.stop || mail.lost || mail.changed);
        let mail = lock(&self.shared.mail);
        let mut mail = match timeout {
            None => self
                .shared
                .wake
                .wait_while(mail, idle)
                .unwrap_or_else(PoisonError::into_inner),
            Some(timeout) => {
                self.shared
                    .wake
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

    /// Sleeps for `delay`; true if the adapter was dropped before it ran out.
    fn sleep(&self, delay: Duration) -> bool {
        let mail = lock(&self.shared.mail);
        self.shared
            .wake
            .wait_timeout_while(mail, delay, |mail| !mail.stop)
            .unwrap_or_else(PoisonError::into_inner)
            .0
            .stop
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, AtomicUsize};
    use std::sync::mpsc;
    use std::time::Instant;

    use crosspane_platform::Frame;
    use crosspane_types::color::ColorSpace;
    use crosspane_types::geom::euclid::point2;
    use crosspane_types::geom::{DisplayGeometry, PixelSize, SizeMm};
    use crosspane_types::time::MonoTime;

    use super::*;

    const WAIT: Duration = Duration::from_secs(5);
    const QUIET: Duration = Duration::from_millis(150);
    const FAST: Timing = Timing {
        reconnect_min: Duration::from_millis(5),
        reconnect_max: Duration::from_millis(20),
        retry_min: Duration::from_millis(5),
        retry_max: Duration::from_millis(20),
    };

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

    /// Two 1920x1080 displays side by side at scale 1: ids 1 and 2.
    fn two_displays() -> Vec<DisplayInfo> {
        vec![
            display(1, 1920, 1080, 1.0, (0.0, 0.0)),
            display(2, 1920, 1080, 1.0, (1920.0, 0.0)),
        ]
    }

    fn win(id: u64, x: i32, y: i32, width: i32, height: i32) -> ShellWindow {
        ShellWindow {
            id,
            app_id: String::new(),
            title: String::new(),
            pid: 0,
            x,
            y,
            width,
            height,
            focused: false,
            minimized: false,
            fullscreen: false,
        }
    }

    fn rect(x0: i32, y0: i32, x1: i32, y1: i32) -> PixelRect {
        PixelRect::new(point2(x0, y0), point2(x1, y1))
    }

    fn on_display(id: u32) -> CaptureTarget {
        CaptureTarget::Display(DisplayId(id))
    }

    fn window(id: u64) -> CaptureTarget {
        CaptureTarget::Window(WindowId(id))
    }

    // ---- pure decisions ----

    fn follow(display: u32, rect: PixelRect, caller: Option<PixelRect>) -> Follow {
        Follow {
            place: Place {
                display: DisplayId(display),
                rect,
            },
            caller,
            applied: compose_crop(rect, caller).unwrap(),
        }
    }

    fn place(display: u32, rect: PixelRect) -> Place {
        Place {
            display: DisplayId(display),
            rect,
        }
    }

    #[test]
    fn place_is_the_display_of_the_centre_and_the_rect_on_it() {
        let displays = two_displays();
        assert_eq!(
            place_of(&win(1, 100, 50, 640, 480), &displays),
            Ok(place(1, rect(100, 50, 740, 530)))
        );
        // Starts on display 1, but the centre (2100) is on display 2.
        assert_eq!(
            place_of(&win(1, 1800, 100, 600, 400), &displays),
            Ok(place(2, rect(0, 100, 480, 500)))
        );
        assert_eq!(
            place_of(&win(1, 5000, 100, 600, 400), &displays),
            Err(Unplaced::NoDisplay)
        );
        assert_eq!(
            place_of(&win(1, 100, 100, 0, 0), &displays),
            Err(Unplaced::NoArea)
        );
        assert_eq!(
            place_of(&win(1, 100, 100, -5, 300), &displays),
            Err(Unplaced::NoArea)
        );
    }

    #[test]
    fn decide_keeps_a_window_that_did_not_change() {
        let current = follow(1, rect(100, 50, 740, 530), None);
        let row = win(7, 100, 50, 640, 480);
        assert_eq!(decide(&current, Some(&row), &two_displays()), Update::Keep);
    }

    #[test]
    fn decide_updates_the_crop_for_a_move_or_resize_on_the_same_display() {
        let current = follow(1, rect(100, 50, 740, 530), None);
        let displays = two_displays();
        assert_eq!(
            decide(&current, Some(&win(7, 300, 200, 640, 480)), &displays),
            Update::Crop {
                place: place(1, rect(300, 200, 940, 680)),
                crop: rect(300, 200, 940, 680)
            }
        );
        assert_eq!(
            decide(&current, Some(&win(7, 100, 50, 800, 600)), &displays),
            Update::Crop {
                place: place(1, rect(100, 50, 900, 650)),
                crop: rect(100, 50, 900, 650)
            }
        );
    }

    #[test]
    fn decide_keeps_the_callers_crop_relative_to_the_window() {
        let caller = Some(rect(10, 20, 110, 120));
        let current = follow(1, rect(100, 50, 740, 530), caller);
        assert_eq!(current.applied, rect(110, 70, 210, 170));
        assert_eq!(
            decide(&current, Some(&win(7, 300, 200, 640, 480)), &two_displays()),
            Update::Crop {
                place: place(1, rect(300, 200, 940, 680)),
                crop: rect(310, 220, 410, 320)
            }
        );
        // The window moved but the clamped crop is the same: only the place is stored.
        let wide = follow(1, rect(0, 0, 100, 100), Some(rect(0, 0, 500, 500)));
        assert_eq!(wide.applied, rect(0, 0, 100, 100));
        assert_eq!(
            decide(&wide, Some(&win(7, 0, 0, 100, 100)), &two_displays()),
            Update::Keep
        );
    }

    #[test]
    fn decide_switches_when_the_centre_moves_to_another_display() {
        let current = follow(1, rect(100, 50, 740, 530), Some(rect(5, 5, 55, 55)));
        assert_eq!(
            decide(
                &current,
                Some(&win(7, 2020, 100, 640, 480)),
                &two_displays()
            ),
            Update::Switch {
                place: place(2, rect(100, 100, 740, 580)),
                crop: rect(105, 105, 155, 155)
            }
        );
    }

    #[test]
    fn decide_ends_a_window_that_left_the_list() {
        let current = follow(1, rect(100, 50, 740, 530), None);
        assert_eq!(decide(&current, None, &two_displays()), Update::Gone);
    }

    #[test]
    fn decide_holds_what_it_cannot_place() {
        let current = follow(1, rect(100, 50, 740, 530), None);
        let displays = two_displays();
        // Off every display.
        assert_eq!(
            decide(&current, Some(&win(7, 9000, 0, 640, 480)), &displays),
            Update::Hold
        );
        // No area.
        assert_eq!(
            decide(&current, Some(&win(7, 100, 50, 0, 480)), &displays),
            Update::Hold
        );
        // No displays at all.
        assert_eq!(
            decide(&current, Some(&win(7, 100, 50, 640, 480)), &[]),
            Update::Hold
        );
        // The caller's crop no longer lies inside the (smaller) window.
        let cropped = follow(1, rect(100, 50, 740, 530), Some(rect(400, 300, 600, 450)));
        assert_eq!(
            decide(&cropped, Some(&win(7, 100, 50, 200, 100)), &displays),
            Update::Hold
        );
        // A switch is held the same way.
        assert_eq!(
            decide(&cropped, Some(&win(7, 2020, 100, 200, 100)), &displays),
            Update::Hold
        );
    }

    #[test]
    fn end_reasons_follow_the_error_of_the_failed_start() {
        assert_eq!(end_reason(&PlatformError::Locked), StreamEndReason::Blocked);
        assert_eq!(
            end_reason(&PlatformError::NotFound),
            StreamEndReason::TargetGone
        );
        assert_eq!(end_reason(&PlatformError::Timeout), StreamEndReason::Failed);
        assert_eq!(
            end_reason(&PlatformError::Backend("x".into())),
            StreamEndReason::Failed
        );
    }

    // ---- mailbox ----

    #[test]
    fn mailbox_coalesces_changes_and_gives_lost_priority() {
        let mail = Mailbox::default();
        let generation = mail.next_generation();
        let changed = ShellEvent::WindowsChanged { epoch: 1 };
        for _ in 0..5 {
            mail.shared.push(generation, &changed);
        }
        assert_eq!(mail.wait(Some(Duration::ZERO)), Wake::Changed);
        assert_eq!(mail.wait(Some(Duration::ZERO)), Wake::Timer);
        mail.shared.push(generation, &changed);
        mail.shared.push(generation, &ShellEvent::Lost);
        assert_eq!(mail.wait(Some(Duration::ZERO)), Wake::Lost);
        // Lost stays until the next generation starts.
        assert_eq!(mail.wait(Some(Duration::ZERO)), Wake::Lost);
        let next = mail.next_generation();
        assert_eq!(mail.wait(Some(Duration::ZERO)), Wake::Timer);
        // The old bridge's signals are ignored, and so is overlay traffic.
        mail.shared.push(generation, &changed);
        mail.shared.push(
            next,
            &ShellEvent::OverlayState {
                id: 1,
                visible: true,
            },
        );
        assert_eq!(mail.wait(Some(Duration::ZERO)), Wake::Timer);
        mail.poke();
        assert_eq!(mail.wait(Some(Duration::ZERO)), Wake::Changed);
        mail.stop();
        mail.poke();
        assert_eq!(mail.wait(Some(Duration::ZERO)), Wake::Stop);
        assert!(mail.sleep(WAIT));
    }

    // ---- fakes ----

    /// One call the inner capture received.
    #[derive(Clone, Debug, PartialEq)]
    enum Call {
        Start {
            target: CaptureTarget,
            crop: Option<PixelRect>,
            max_fps: u32,
            stream: StreamId,
        },
        SetCrop {
            stream: StreamId,
            crop: Option<PixelRect>,
        },
        Stop {
            stream: StreamId,
        },
    }

    type Sink = Arc<dyn EventSink<FrameEvent>>;

    /// A stand-in for the portal capture: it numbers streams, records calls and says
    /// `Ended { Requested }` when stopped, like the real one.
    #[derive(Clone, Default)]
    struct FakeInner(Arc<InnerState>);

    #[derive(Default)]
    struct InnerState {
        calls: Mutex<Vec<Call>>,
        live: Mutex<BTreeMap<StreamId, Sink>>,
        /// Every sink ever given, so a test can send a late event from a stopped stream.
        sinks: Mutex<BTreeMap<StreamId, Sink>>,
        next: AtomicU64,
        failures: Mutex<VecDeque<PlatformError>>,
        silent_stop: AtomicBool,
        on_start: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }

    impl FakeInner {
        fn calls(&self) -> Vec<Call> {
            self.0.calls.lock().unwrap().clone()
        }

        fn starts(&self) -> Vec<(CaptureTarget, Option<PixelRect>, StreamId)> {
            self.calls()
                .into_iter()
                .filter_map(|call| match call {
                    Call::Start {
                        target,
                        crop,
                        stream,
                        ..
                    } => Some((target, crop, stream)),
                    _ => None,
                })
                .collect()
        }

        fn crops(&self) -> Vec<(StreamId, Option<PixelRect>)> {
            self.calls()
                .into_iter()
                .filter_map(|call| match call {
                    Call::SetCrop { stream, crop } => Some((stream, crop)),
                    _ => None,
                })
                .collect()
        }

        fn stops(&self) -> Vec<StreamId> {
            self.calls()
                .into_iter()
                .filter_map(|call| match call {
                    Call::Stop { stream } => Some(stream),
                    _ => None,
                })
                .collect()
        }

        fn live_count(&self) -> usize {
            self.0.live.lock().unwrap().len()
        }

        fn fail_next_start(&self, error: PlatformError) {
            self.0.failures.lock().unwrap().push_back(error);
        }

        /// Sends `event` through the sink `stream` was started with, live or not.
        fn late(&self, stream: StreamId, event: FrameEvent) {
            let sink = self.0.sinks.lock().unwrap().get(&stream).cloned().unwrap();
            sink.send(event);
        }

        fn frame(&self, stream: StreamId) {
            self.late(
                stream,
                FrameEvent::Frame {
                    stream,
                    frame: Frame::cpu(
                        PixelSize::new(1, 1),
                        4,
                        Arc::from(vec![0u8; 4]),
                        None,
                        MonoTime::ZERO,
                    ),
                },
            );
        }

        /// The stream ends by itself.
        fn end(&self, stream: StreamId, reason: StreamEndReason) {
            self.0.live.lock().unwrap().remove(&stream);
            self.late(stream, FrameEvent::Ended { stream, reason });
        }
    }

    impl FrameCapture for FakeInner {
        fn start(
            &mut self,
            target: CaptureTarget,
            crop: Option<PixelRect>,
            max_fps: u32,
            sink: Sink,
        ) -> Result<StreamId, PlatformError> {
            let hook = self.0.on_start.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
            if let Some(error) = self.0.failures.lock().unwrap().pop_front() {
                return Err(error);
            }
            let stream = StreamId(self.0.next.fetch_add(1, Ordering::SeqCst) + 1);
            self.0.calls.lock().unwrap().push(Call::Start {
                target,
                crop,
                max_fps,
                stream,
            });
            self.0.live.lock().unwrap().insert(stream, sink.clone());
            self.0.sinks.lock().unwrap().insert(stream, sink);
            Ok(stream)
        }

        fn set_crop(
            &mut self,
            stream: StreamId,
            crop: Option<PixelRect>,
        ) -> Result<(), PlatformError> {
            if !self.0.live.lock().unwrap().contains_key(&stream) {
                return Err(PlatformError::NotFound);
            }
            self.0
                .calls
                .lock()
                .unwrap()
                .push(Call::SetCrop { stream, crop });
            Ok(())
        }

        fn stop(&mut self, stream: StreamId) -> Result<(), PlatformError> {
            self.0.calls.lock().unwrap().push(Call::Stop { stream });
            let sink = self.0.live.lock().unwrap().remove(&stream);
            if let Some(sink) = sink
                && !self.0.silent_stop.load(Ordering::SeqCst)
            {
                sink.send(FrameEvent::Ended {
                    stream,
                    reason: StreamEndReason::Requested,
                });
            }
            Ok(())
        }
    }

    /// A bridge that answers from a list the test edits and delivers signals when the test says.
    #[derive(Clone, Default)]
    struct FakeBridge(Arc<BridgeState>);

    #[derive(Default)]
    struct BridgeState {
        windows: Mutex<Vec<ShellWindow>>,
        callbacks: Mutex<Vec<ShellCallback>>,
        lost: AtomicBool,
        lists: AtomicUsize,
        failing_lists: AtomicUsize,
    }

    impl FakeBridge {
        fn new(windows: Vec<ShellWindow>) -> FakeBridge {
            let bridge = FakeBridge::default();
            bridge.set(windows);
            bridge
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

    impl Bridge for FakeBridge {
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

        fn watch(&self, callback: ShellCallback) -> Result<(), PlatformError> {
            if self.0.lost.load(Ordering::SeqCst) {
                return Err(PlatformError::Backend("Shell bridge lost".into()));
            }
            self.0.callbacks.lock().unwrap().push(callback);
            Ok(())
        }
    }

    type Script = Arc<Mutex<VecDeque<Result<FakeBridge, PlatformError>>>>;

    /// What a test sees of an event.
    #[derive(Debug, PartialEq)]
    enum Seen {
        Frame(StreamId),
        Ended(StreamId, StreamEndReason),
        Cursor(StreamId),
        CursorDefault(StreamId),
        Other,
    }

    fn seen(event: &FrameEvent) -> Seen {
        match event {
            FrameEvent::Frame { stream, .. } => Seen::Frame(*stream),
            FrameEvent::Ended { stream, reason } => Seen::Ended(*stream, *reason),
            FrameEvent::Cursor { stream, .. } => Seen::Cursor(*stream),
            FrameEvent::CursorDefault { stream } => Seen::CursorDefault(*stream),
            _ => Seen::Other,
        }
    }

    struct Rig {
        capture: Capture<FakeInner, FakeBridge>,
        inner: FakeInner,
        bridge: FakeBridge,
        displays: Arc<Mutex<Vec<DisplayInfo>>>,
        script: Script,
        connects: Arc<AtomicUsize>,
        sink: Sink,
        events: mpsc::Receiver<FrameEvent>,
    }

    impl Rig {
        fn new(windows: Vec<ShellWindow>) -> Rig {
            Rig::with_displays(windows, two_displays())
        }

        fn with_displays(windows: Vec<ShellWindow>, displays: Vec<DisplayInfo>) -> Rig {
            let inner = FakeInner::default();
            let bridge = FakeBridge::new(windows);
            let script: Script = Arc::default();
            let connects = Arc::new(AtomicUsize::new(0));
            let connect: Connect<FakeBridge> =
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
            let displays = Arc::new(Mutex::new(displays));
            let snapshot: DisplaysFn = {
                let displays = displays.clone();
                Arc::new(move || displays.lock().unwrap().clone())
            };
            let (tx, events) = mpsc::channel();
            let sink: Sink = Arc::new(move |event: FrameEvent| {
                let _ = tx.send(event);
            });
            let capture =
                Capture::new(inner.clone(), bridge.clone(), snapshot, connect, FAST).unwrap();
            Rig {
                capture,
                inner,
                bridge,
                displays,
                script,
                connects,
                sink,
                events,
            }
        }

        fn start(&mut self, id: u64, crop: Option<PixelRect>) -> StreamId {
            self.capture
                .start(window(id), crop, 30, self.sink.clone())
                .unwrap()
        }

        fn next(&self) -> Seen {
            seen(&self.events.recv_timeout(WAIT).unwrap())
        }

        fn assert_quiet(&self) {
            if let Ok(event) = self.events.recv_timeout(QUIET) {
                panic!("unexpected event {:?}", seen(&event));
            }
        }

        /// Returns once the worker has finished every pass that was pending: two passes in a row
        /// have listed the windows, and the second starts only after the first has been applied.
        fn settle(&self) {
            for _ in 0..2 {
                let before = self.bridge.lists();
                self.bridge.changed();
                wait_for("a window list", || self.bridge.lists() > before);
            }
        }
    }

    fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + WAIT;
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(2));
        }
    }

    // ---- start ----

    #[test]
    fn a_window_starts_the_monitor_stream_cropped_to_it() {
        let mut rig = Rig::new(vec![win(7, 100, 50, 640, 480)]);
        let stream = rig.start(7, None);
        assert!(is_window_stream(stream));
        assert_eq!(
            rig.inner.calls(),
            vec![Call::Start {
                target: on_display(1),
                crop: Some(rect(100, 50, 740, 530)),
                max_fps: 30,
                stream: StreamId(1),
            }]
        );
    }

    #[test]
    fn the_crop_is_in_device_pixels_of_the_display_the_window_is_on() {
        // 3840x2160 device pixels at scale 2 is 1920x1080 logical; display 2 sits to the right.
        let displays = vec![
            display(1, 1920, 1080, 1.0, (0.0, 0.0)),
            display(2, 3840, 2160, 2.0, (1920.0, 0.0)),
        ];
        let mut rig = Rig::with_displays(vec![win(7, 2020, 50, 640, 480)], displays);
        rig.start(7, None);
        assert_eq!(
            rig.inner.starts(),
            vec![(on_display(2), Some(rect(200, 100, 1480, 1060)), StreamId(1))]
        );
    }

    #[test]
    fn the_callers_crop_is_relative_to_the_window() {
        let mut rig = Rig::new(vec![win(7, 100, 50, 640, 480)]);
        rig.start(7, Some(rect(10, 20, 110, 120)));
        assert_eq!(
            rig.inner.starts(),
            vec![(on_display(1), Some(rect(110, 70, 210, 170)), StreamId(1))]
        );
    }

    #[test]
    fn outer_ids_are_the_adapters_own_and_never_repeat() {
        let mut rig = Rig::new(vec![win(7, 100, 50, 640, 480)]);
        let first = rig.start(7, None);
        let second = rig.start(7, None);
        assert_eq!(first, StreamId(OUTER_BASE));
        assert_eq!(second, StreamId(OUTER_BASE + 1));
        rig.capture.stop(first).unwrap();
        assert_eq!(rig.start(7, None), StreamId(OUTER_BASE + 2));
    }

    #[test]
    fn a_window_that_cannot_be_captured_is_refused_before_any_inner_stream() {
        let mut rig = Rig::new(vec![
            win(7, 100, 50, 640, 480),
            win(8, 9000, 0, 100, 100),
            win(9, 100, 100, 0, 0),
        ]);
        let mut start = |target, crop, max_fps| {
            rig.capture
                .start(target, crop, max_fps, rig.sink.clone())
                .unwrap_err()
        };
        assert!(matches!(
            start(window(404), None, 30),
            PlatformError::NotFound
        ));
        assert!(matches!(
            start(window(8), None, 30),
            PlatformError::NotFound
        ));
        assert!(matches!(
            start(window(9), None, 30),
            PlatformError::Backend(_)
        ));
        assert!(matches!(
            start(window(7), None, 0),
            PlatformError::Backend(_)
        ));
        // An empty crop, and a crop outside the window.
        assert!(matches!(
            start(window(7), Some(rect(5, 5, 5, 50)), 30),
            PlatformError::Backend(_)
        ));
        assert!(matches!(
            start(window(7), Some(rect(700, 0, 800, 50)), 30),
            PlatformError::Backend(_)
        ));
        assert!(rig.inner.calls().is_empty());
    }

    #[test]
    fn a_failing_inner_start_leaves_no_stream_behind() {
        let mut rig = Rig::new(vec![win(7, 100, 50, 640, 480)]);
        rig.inner.fail_next_start(PlatformError::Locked);
        let error = rig
            .capture
            .start(window(7), None, 30, rig.sink.clone())
            .unwrap_err();
        assert!(matches!(error, PlatformError::Locked));
        assert_eq!(rig.inner.live_count(), 0);
        // The number is not spent, and the adapter works afterwards.
        assert_eq!(rig.start(7, None), StreamId(OUTER_BASE));
        rig.assert_quiet();
    }

    #[test]
    fn display_streams_pass_through_unchanged() {
        let mut rig = Rig::new(vec![win(7, 100, 50, 640, 480)]);
        let crop = Some(rect(0, 0, 640, 360));
        let stream = rig
            .capture
            .start(on_display(2), crop, 60, rig.sink.clone())
            .unwrap();
        assert!(!is_window_stream(stream));
        assert_eq!(stream, StreamId(1));
        assert_eq!(
            rig.inner.calls(),
            vec![Call::Start {
                target: on_display(2),
                crop,
                max_fps: 60,
                stream: StreamId(1),
            }]
        );
        // No bridge traffic, events untouched, and crop and stop reach the inner as they are.
        assert_eq!(rig.bridge.lists(), 0);
        rig.inner.frame(stream);
        assert_eq!(rig.next(), Seen::Frame(StreamId(1)));
        rig.capture.set_crop(stream, None).unwrap();
        rig.capture.stop(stream).unwrap();
        assert_eq!(
            &rig.inner.calls()[1..],
            &[Call::SetCrop { stream, crop: None }, Call::Stop { stream }]
        );
        assert_eq!(rig.next(), Seen::Ended(stream, StreamEndReason::Requested));
        rig.assert_quiet();
        // An inner id the inner does not know is the inner's to answer.
        assert!(matches!(
            rig.capture.set_crop(StreamId(99), None),
            Err(PlatformError::NotFound)
        ));
    }

    #[test]
    fn inner_events_are_readdressed_to_the_outer_stream() {
        let mut rig = Rig::new(vec![win(7, 100, 50, 640, 480)]);
        let outer = rig.start(7, None);
        let inner = StreamId(1);
        rig.inner.frame(inner);
        rig.inner.late(
            inner,
            FrameEvent::Cursor {
                stream: inner,
                cursor: None,
            },
        );
        rig.inner
            .late(inner, FrameEvent::CursorDefault { stream: inner });
        assert_eq!(rig.next(), Seen::Frame(outer));
        assert_eq!(rig.next(), Seen::Cursor(outer));
        assert_eq!(rig.next(), Seen::CursorDefault(outer));
        rig.assert_quiet();
    }

    // ---- following the window ----

    #[test]
    fn a_moved_or_resized_window_updates_the_crop_of_the_same_inner_stream() {
        let mut rig = Rig::new(vec![win(7, 100, 50, 640, 480)]);
        rig.start(7, None);
        // Nothing changed: no crop call. The next real change is the first one.
        rig.settle();
        assert!(rig.inner.crops().is_empty());
        rig.bridge.set(vec![win(7, 300, 200, 640, 480)]);
        rig.bridge.changed();
        wait_for("the move", || rig.inner.crops().len() == 1);
        rig.bridge.set(vec![win(7, 300, 200, 800, 600)]);
        rig.bridge.changed();
        wait_for("the resize", || rig.inner.crops().len() == 2);
        assert_eq!(
            rig.inner.crops(),
            vec![
                (StreamId(1), Some(rect(300, 200, 940, 680))),
                (StreamId(1), Some(rect(300, 200, 1100, 800))),
            ]
        );
        assert_eq!(rig.inner.starts().len(), 1);
        rig.assert_quiet();
    }

    #[test]
    fn the_callers_crop_follows_the_window_when_it_moves() {
        let mut rig = Rig::new(vec![win(7, 100, 50, 640, 480)]);
        rig.start(7, Some(rect(10, 20, 110, 120)));
        rig.bridge.set(vec![win(7, 300, 200, 640, 480)]);
        rig.bridge.changed();
        wait_for("the move", || rig.inner.crops().len() == 1);
        assert_eq!(
            rig.inner.crops(),
            vec![(StreamId(1), Some(rect(310, 220, 410, 320)))]
        );
    }

    #[test]
    fn a_window_on_another_display_moves_the_stream_under_the_same_outer_id() {
        let mut rig = Rig::new(vec![win(7, 100, 50, 640, 480)]);
        let outer = rig.start(7, Some(rect(5, 5, 55, 55)));
        rig.inner.frame(StreamId(1));
        assert_eq!(rig.next(), Seen::Frame(outer));

        rig.bridge.set(vec![win(7, 2020, 100, 640, 480)]);
        rig.bridge.changed();
        wait_for("the switch", || rig.inner.starts().len() == 2);
        assert_eq!(
            rig.inner.starts(),
            vec![
                (on_display(1), Some(rect(105, 55, 155, 105)), StreamId(1)),
                (on_display(2), Some(rect(105, 105, 155, 155)), StreamId(2)),
            ]
        );
        // The old stream was stopped (before the new one started) and its `Ended` swallowed.
        assert_eq!(rig.inner.stops(), vec![StreamId(1)]);
        assert_eq!(
            rig.inner.calls()[1],
            Call::Stop {
                stream: StreamId(1)
            }
        );
        rig.assert_quiet();

        // Frames of the new stream keep the outer id; late events of the old one are dropped.
        rig.inner.late(
            StreamId(1),
            FrameEvent::Frame {
                stream: StreamId(1),
                frame: Frame::cpu(
                    PixelSize::new(1, 1),
                    4,
                    Arc::from(vec![0u8; 4]),
                    None,
                    MonoTime::ZERO,
                ),
            },
        );
        rig.inner.late(
            StreamId(1),
            FrameEvent::Ended {
                stream: StreamId(1),
                reason: StreamEndReason::Requested,
            },
        );
        rig.inner.frame(StreamId(2));
        assert_eq!(rig.next(), Seen::Frame(outer));
        rig.assert_quiet();

        // Stopping the outer stream stops the current inner one and ends it exactly once.
        rig.capture.stop(outer).unwrap();
        assert_eq!(rig.next(), Seen::Ended(outer, StreamEndReason::Requested));
        assert_eq!(rig.inner.stops(), vec![StreamId(1), StreamId(2)]);
        assert_eq!(rig.inner.live_count(), 0);
        rig.capture.stop(outer).unwrap();
        rig.assert_quiet();
    }

    #[test]
    fn a_switch_that_cannot_start_ends_the_stream_with_the_reason_of_the_error() {
        for (error, reason) in [
            (PlatformError::NotFound, StreamEndReason::TargetGone),
            (PlatformError::Locked, StreamEndReason::Blocked),
            (PlatformError::Timeout, StreamEndReason::Failed),
            (PlatformError::Backend("x".into()), StreamEndReason::Failed),
        ] {
            let mut rig = Rig::new(vec![win(7, 100, 50, 640, 480)]);
            let outer = rig.start(7, None);
            rig.inner.fail_next_start(error);
            rig.bridge.set(vec![win(7, 2020, 100, 640, 480)]);
            rig.bridge.changed();
            assert_eq!(rig.next(), Seen::Ended(outer, reason));
            rig.assert_quiet();
            assert_eq!(rig.inner.live_count(), 0);
            // Nothing is left to stop or to crop.
            rig.capture.stop(outer).unwrap();
            assert!(matches!(
                rig.capture.set_crop(outer, None),
                Err(PlatformError::NotFound)
            ));
            rig.assert_quiet();
        }
    }

    #[test]
    fn a_window_that_closes_ends_the_stream_once_with_target_gone() {
        let mut rig = Rig::new(vec![win(7, 100, 50, 640, 480), win(8, 0, 0, 100, 100)]);
        let closing = rig.start(7, None);
        let staying = rig.start(8, None);
        rig.bridge.set(vec![win(8, 0, 0, 100, 100)]);
        rig.bridge.changed();
        assert_eq!(
            rig.next(),
            Seen::Ended(closing, StreamEndReason::TargetGone)
        );
        rig.assert_quiet();
        // The inner stream was stopped quietly; the other stream is untouched.
        assert_eq!(rig.inner.stops(), vec![StreamId(1)]);
        assert_eq!(rig.inner.live_count(), 1);
        rig.inner.frame(StreamId(2));
        assert_eq!(rig.next(), Seen::Frame(staying));
        // Stopping what is already over is fine and silent; a crop on it is not found.
        rig.capture.stop(closing).unwrap();
        assert!(matches!(
            rig.capture.set_crop(closing, None),
            Err(PlatformError::NotFound)
        ));
        rig.assert_quiet();
    }

    #[test]
    fn a_stream_the_inner_ends_by_itself_is_forwarded_once_and_forgotten() {
        let mut rig = Rig::new(vec![win(7, 100, 50, 640, 480)]);
        let outer = rig.start(7, None);
        rig.inner.end(StreamId(1), StreamEndReason::Blocked);
        assert_eq!(rig.next(), Seen::Ended(outer, StreamEndReason::Blocked));
        // Nothing follows: not another end, not a frame, and the window's moves are not followed.
        rig.inner.frame(StreamId(1));
        rig.bridge.set(vec![win(7, 2020, 100, 640, 480)]);
        rig.bridge.changed();
        thread::sleep(QUIET);
        rig.assert_quiet();
        assert!(rig.inner.crops().is_empty());
        assert_eq!(rig.inner.starts().len(), 1);
        // The stop of an ended stream asks the inner for nothing.
        rig.capture.stop(outer).unwrap();
        assert!(rig.inner.stops().is_empty());
        rig.assert_quiet();
    }

    #[test]
    fn stop_ends_the_stream_exactly_once() {
        let mut rig = Rig::new(vec![win(7, 100, 50, 640, 480)]);
        let outer = rig.start(7, None);
        rig.capture.stop(outer).unwrap();
        assert_eq!(rig.next(), Seen::Ended(outer, StreamEndReason::Requested));
        rig.assert_quiet();
        assert_eq!(rig.inner.stops(), vec![StreamId(1)]);
        // A late frame or end from the stopped stream goes nowhere.
        rig.inner.late(
            StreamId(1),
            FrameEvent::Ended {
                stream: StreamId(1),
                reason: StreamEndReason::Failed,
            },
        );
        rig.assert_quiet();
    }

    #[test]
    fn stop_ends_the_stream_even_when_the_inner_says_nothing() {
        let mut rig = Rig::new(vec![win(7, 100, 50, 640, 480)]);
        let outer = rig.start(7, None);
        rig.inner.0.silent_stop.store(true, Ordering::SeqCst);
        rig.capture.stop(outer).unwrap();
        assert_eq!(rig.next(), Seen::Ended(outer, StreamEndReason::Requested));
        rig.assert_quiet();
        rig.inner.late(
            StreamId(1),
            FrameEvent::Ended {
                stream: StreamId(1),
                reason: StreamEndReason::Requested,
            },
        );
        rig.assert_quiet();
    }

    // ---- set_crop ----

    #[test]
    fn set_crop_applies_the_callers_crop_against_the_current_window() {
        let mut rig = Rig::new(vec![win(7, 100, 50, 640, 480)]);
        let outer = rig.start(7, None);
        rig.capture
            .set_crop(outer, Some(rect(10, 10, 60, 60)))
            .unwrap();
        // The same crop again changes nothing.
        rig.capture
            .set_crop(outer, Some(rect(10, 10, 60, 60)))
            .unwrap();
        rig.capture.set_crop(outer, None).unwrap();
        assert_eq!(
            rig.inner.crops(),
            vec![
                (StreamId(1), Some(rect(110, 60, 160, 110))),
                (StreamId(1), Some(rect(100, 50, 740, 530))),
            ]
        );
    }

    #[test]
    fn set_crop_refuses_a_crop_outside_the_window_and_keeps_the_old_one() {
        let mut rig = Rig::new(vec![win(7, 100, 50, 640, 480)]);
        let outer = rig.start(7, Some(rect(10, 10, 60, 60)));
        assert!(matches!(
            rig.capture.set_crop(outer, Some(rect(700, 0, 800, 50))),
            Err(PlatformError::Backend(_))
        ));
        assert!(matches!(
            rig.capture.set_crop(outer, Some(rect(5, 5, 5, 5))),
            Err(PlatformError::Backend(_))
        ));
        assert!(rig.inner.crops().is_empty());
        // The stored crop is still the first one: a move re-applies that.
        rig.bridge.set(vec![win(7, 300, 200, 640, 480)]);
        rig.bridge.changed();
        wait_for("the move", || rig.inner.crops().len() == 1);
        assert_eq!(
            rig.inner.crops(),
            vec![(StreamId(1), Some(rect(310, 210, 360, 260)))]
        );
    }

    #[test]
    fn set_crop_of_an_unknown_window_stream_is_not_found() {
        let mut rig = Rig::new(vec![win(7, 100, 50, 640, 480)]);
        assert!(matches!(
            rig.capture.set_crop(StreamId(OUTER_BASE + 40), None),
            Err(PlatformError::NotFound)
        ));
        rig.capture.stop(StreamId(OUTER_BASE + 40)).unwrap();
        assert!(rig.inner.calls().is_empty());
    }

    #[test]
    fn a_window_held_by_its_crop_keeps_the_last_crop() {
        let mut rig = Rig::new(vec![win(7, 100, 50, 640, 480)]);
        rig.start(7, Some(rect(400, 300, 600, 450)));
        // Shrunk below the caller's crop: held. Then big enough again: updated.
        rig.bridge.set(vec![win(7, 100, 50, 200, 100)]);
        rig.settle();
        assert!(rig.inner.crops().is_empty());
        rig.bridge.set(vec![win(7, 200, 50, 700, 500)]);
        rig.bridge.changed();
        wait_for("the growth", || rig.inner.crops().len() == 1);
        assert_eq!(
            rig.inner.crops(),
            vec![(StreamId(1), Some(rect(600, 350, 800, 500)))]
        );
        rig.assert_quiet();
    }

    // ---- bridge trouble ----

    #[test]
    fn a_lost_bridge_ends_every_window_stream_and_the_worker_reconnects() {
        let mut rig = Rig::new(vec![win(7, 100, 50, 640, 480), win(8, 0, 0, 100, 100)]);
        let first = rig.start(7, None);
        let second = rig.start(8, None);
        // A display stream is not the bridge's business and survives.
        let display = rig
            .capture
            .start(on_display(1), None, 30, rig.sink.clone())
            .unwrap();

        rig.bridge.lose();
        assert_eq!(rig.next(), Seen::Ended(first, StreamEndReason::TargetGone));
        assert_eq!(rig.next(), Seen::Ended(second, StreamEndReason::TargetGone));
        rig.assert_quiet();
        assert_eq!(rig.inner.live_count(), 1);
        rig.inner.frame(display);
        assert_eq!(rig.next(), Seen::Frame(display));

        // Without a bridge a window can't be captured, and the worker keeps trying.
        assert!(matches!(
            rig.capture.start(window(7), None, 30, rig.sink.clone()),
            Err(PlatformError::Backend(_))
        ));
        wait_for("two reconnect attempts", || {
            rig.connects.load(Ordering::SeqCst) >= 2
        });

        // The extension is back: new ids, the same adapter.
        let again = FakeBridge::new(vec![win(7, 100, 50, 640, 480)]);
        rig.script.lock().unwrap().push_back(Ok(again.clone()));
        wait_for("the new bridge", || {
            again.watchers() == 1 && rig.capture.core.current().is_some()
        });
        let outer = rig.start(7, None);
        assert_eq!(outer, StreamId(OUTER_BASE + 2));
        again.set(vec![win(7, 300, 200, 640, 480)]);
        again.changed();
        wait_for("following on the new bridge", || {
            !rig.inner.crops().is_empty()
        });
        // The old bridge no longer drives anything.
        rig.bridge.changed();
        rig.assert_quiet();
    }

    #[test]
    fn a_start_that_races_a_lost_bridge_is_refused_and_leaves_nothing_running() {
        let mut rig = Rig::new(vec![win(7, 100, 50, 640, 480)]);
        // The bridge is replaced while the inner stream starts, as the worker's first step of a
        // recovery would do it.
        let core = rig.capture.core.clone();
        *rig.inner.0.on_start.lock().unwrap() = Some(Box::new(move || {
            lock(&core.live).bridge = None;
        }));
        let error = rig
            .capture
            .start(window(7), None, 30, rig.sink.clone())
            .unwrap_err();
        assert!(matches!(error, PlatformError::Backend(_)));
        assert_eq!(rig.inner.live_count(), 0);
        assert_eq!(rig.inner.stops(), vec![StreamId(1)]);
        rig.assert_quiet();
        assert!(lock(&rig.capture.core.state).streams.is_empty());
    }

    #[test]
    fn a_failed_list_keeps_the_streams_and_is_retried() {
        let mut rig = Rig::new(vec![win(7, 100, 50, 640, 480)]);
        rig.start(7, None);
        rig.bridge.0.failing_lists.store(3, Ordering::SeqCst);
        rig.bridge.set(vec![win(7, 300, 200, 640, 480)]);
        let before = rig.bridge.lists();
        rig.bridge.changed();
        wait_for("the retried crop", || rig.inner.crops().len() == 1);
        assert!(rig.bridge.lists() >= before + 3);
        rig.assert_quiet();
        assert_eq!(rig.inner.live_count(), 1);
    }

    #[test]
    fn without_window_streams_a_change_costs_no_list() {
        let rig = Rig::new(vec![win(7, 100, 50, 640, 480)]);
        rig.bridge.changed();
        rig.bridge.changed();
        thread::sleep(QUIET);
        assert_eq!(rig.bridge.lists(), 0);
    }

    #[test]
    fn a_changed_display_scale_is_followed_on_the_next_signal() {
        let mut rig = Rig::new(vec![win(7, 100, 50, 640, 480)]);
        rig.start(7, None);
        // The same logical desktop at twice the pixel density.
        rig.displays.lock().unwrap()[0] = display(1, 3840, 2160, 2.0, (0.0, 0.0));
        rig.bridge.changed();
        wait_for("the new crop", || rig.inner.crops().len() == 1);
        assert_eq!(
            rig.inner.crops(),
            vec![(StreamId(1), Some(rect(200, 100, 1480, 1060)))]
        );
    }

    // ---- teardown ----

    #[test]
    fn dropping_the_adapter_ends_every_window_stream_once_and_stops_the_worker() {
        let mut rig = Rig::new(vec![win(7, 100, 50, 640, 480), win(8, 0, 0, 100, 100)]);
        let first = rig.start(7, None);
        let second = rig.start(8, None);
        let stopped = rig.start(8, None);
        rig.capture.stop(stopped).unwrap();
        assert_eq!(rig.next(), Seen::Ended(stopped, StreamEndReason::Requested));
        let Rig {
            capture,
            inner,
            bridge,
            events,
            ..
        } = rig;
        drop(capture);
        let ended: Vec<Seen> = (0..2)
            .map(|_| seen(&events.recv_timeout(WAIT).unwrap()))
            .collect();
        assert_eq!(
            ended,
            vec![
                Seen::Ended(first, StreamEndReason::Requested),
                Seen::Ended(second, StreamEndReason::Requested)
            ]
        );
        assert!(events.recv_timeout(QUIET).is_err());
        assert_eq!(inner.live_count(), 0);
        // The worker is gone: a signal from the bridge starts no pass.
        let lists = bridge.lists();
        bridge.changed();
        thread::sleep(QUIET);
        assert_eq!(bridge.lists(), lists);
    }

    #[test]
    fn dropping_while_reconnecting_does_not_wait_out_the_backoff() {
        let rig = Rig::new(vec![win(7, 100, 50, 640, 480)]);
        rig.bridge.lose();
        wait_for("a reconnect attempt", || {
            rig.connects.load(Ordering::SeqCst) >= 1
        });
        let started = Instant::now();
        drop(rig.capture);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn the_adapter_and_its_pieces_are_sendable() {
        fn assert_capture<T: FrameCapture + Send + fmt::Debug>() {}
        assert_capture::<GnomeWindowCapture>();
    }
}
