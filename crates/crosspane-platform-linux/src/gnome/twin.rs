//! The GNOME twin: one Mutter virtual monitor that projected windows are parked on (WP-G2.4 task
//! B2, M2). [`GnomeTwin`] owns its lifecycle; `twin_parking` parks windows on it.
//!
//! # What a twin is
//!
//! A ScreenCast VIRTUAL portal session plus a PipeWire consumer that fixates the monitor's size
//! ([`VirtualScreen`], task A). It is created **lazily** by the first window parked on it
//! (`GnomeTwin::ensure`), **grows** (never shrinks) when a window needs more room, and is dropped
//! [`TWIN_LINGER`] after the last parked window is released (and when the twin manager is dropped,
//! at agent stop). One twin serves every window parked on this node at once; they are stacked at
//! its origin and the capture crops to each one's content.
//!
//! # Creation and growth
//!
//! Mutter lays every new or resized virtual monitor out linearly and drops the user's rotation and
//! placement while it exists, and the relayout can un-tile windows. So both run the same sequence,
//! on the caller's thread and bounded (`bring_up`):
//!
//! 1. creation only: `DisplayConfig.GetCurrentState` while no `Meta-*` monitor exists, and the
//!    physical layout taken from it (the **snapshot**); a foreign `Meta-*` monitor makes the twin
//!    `Unsupported`, because applying a layout would drop it;
//! 2. `ShellBridge::save_layout` (the window frames, before anything changes);
//! 3. `VirtualScreen::open` (creation) or `resize` (growth) to the size `sizing::plan` chose;
//! 4. poll `GetCurrentState` (10 ms, at most 2 s) for the `Meta-*` monitor whose current mode has
//!    that size;
//! 5. `ApplyMonitorsConfig` (**temporary only**, through `display_config`) with the snapshot's
//!    positions, scales and transforms plus the twin at the right edge of the rightmost physical
//!    monitor, top-aligned; a serial race (`is_serial_race`) re-reads and retries up to 3 times;
//! 6. `ShellBridge::restore_layout` for the saved frames, skipping the windows parked on the twin;
//! 7. `ShellBridge::set_pointer_fence` for the twin's logical rectangle.
//!
//! Any failure in 3 to 5 drops the screen (Mutter puts its stored layout back), restores the saved
//! frames, and is `Unsupported`, so `TwinOrMirror` mirrors the window instead. A closed gate is
//! `Locked` instead (no mirror fallback while locked); a growth that finds the gate closed leaves
//! the twin as it is. A failed growth drops the twin, which restores every window parked on it
//! (the `GnomeTwin::on_lost` listeners). Failures in 6 and 7 are logged and never fail the call.
//!
//! When the linger ends the twin is dropped between a `save_layout` and a `restore_layout`
//! (taken once the monitor is gone from `GetCurrentState`), because removing a monitor is a
//! relayout too. That guard is best effort and not part of the frozen sequence; a loss has no
//! such guard (the monitor is gone before the twin knows).
//!
//! The snapshot is refreshed (on the timer thread, 250 ms after the last `MonitorsChanged`) when
//! the physical layout the user has now differs from it: they changed it while the twin exists.
//! Positions are the only thing ever put back; no physical monitor's mode, scale or transform is
//! changed.
//!
//! # Sizing and scale
//!
//! See `sizing`: sides are multiples of 64 in 64..=8192 device pixels, grown with 25 % headroom.
//! The twin's logical scale is the destination's, reduced to a scale the mode supports that gives
//! a whole logical size, else 1.0 (so a fractional destination scale needs sizes it divides).
//! Every conversion uses the *effective* scale, device pixels over the logical width Mutter
//! reports, exactly as `wayland_outputs` derives it.
//!
//! # Displays, fence, capture
//!
//! - The twin's [`DisplayInfo`] (id = FNV-1a of the connector name, like every other display) is
//!   never reported by `WaylandOutputs`. [`GnomeTwin::displays`] adds it to the list the EIS
//!   injector maps coordinates with, while the twin exists.
//! - The pointer fence (`fence`) keeps the local pointer out; barriers stop injected motion too,
//!   so [`GnomeTwin::before_move`] is the EIS hook that lowers it for moves onto the twin and
//!   [`FENCE_REARM`] after the last one raises it again.
//! - [`GnomeTwin::capture`] routes `CaptureTarget::Display(twin id)` to the twin's PipeWire stream
//!   and everything else to the capture it wraps. Streams on a twin that goes away end
//!   `TargetGone`. A stream that gets no frame soon after it starts (the gate closed and reopened,
//!   and a static window sends none) makes the twin grow by one 64 px step to force one, at most
//!   eight times per twin and not for a stream that was stopped or has ended meanwhile.
//!
//! # Threads and locks
//!
//! One timer thread (`crosspane-gnome-twin`) runs everything that is not a caller's: the linger,
//! the fence re-arm, the layout refresh, the loss handling and the first-frame nudge. Locks, in
//! order: `state` (held for a whole sequence, bridge and Mutter calls included), then the screen's
//! own lock; `published`, `fence` and the timers' lock are short and never held across a bridge or
//! Mutter call, except the fence lock (one fence call). Listeners are called with no twin lock
//! held. Outward handles (the EIS hook and display function) hold the twin weakly, so they never keep a
//! portal session alive.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::thread;
use std::time::{Duration, Instant};

use crosspane_platform::{FrameCapture, IoGate, PlatformError};
use crosspane_types::color::ColorSpace;
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{DisplayGeometry, PixelSize, PointLogical, SizeMm};
use crosspane_types::id::DisplayId;

use super::display_config::{
    DisplayConfig, DisplayState, LogicalConfig, LogicalState, ModeState, TWIN_PREFIX,
    is_serial_race, physical, plan, twin_rect,
};
use super::parking::bridge_lost;
use super::shell::ShellBridge;
use crate::portal::eis::{DisplaysFn, MoveHook};
use crate::portal::virtual_screen::{VirtualScreen, VirtualScreenConfig};
use crate::wayland_outputs::display_id;

mod capture;
mod fence;
pub(super) mod sizing;
#[cfg(test)]
pub(super) mod testing;
#[cfg(test)]
mod tests;

pub use capture::TwinCapture;
use fence::{Fence, Rect};

/// How long the twin lives on after the last window parked on it is released.
pub const TWIN_LINGER: Duration = Duration::from_secs(30);
/// How long after the last move onto the twin the pointer fence is put up again.
pub const FENCE_REARM: Duration = Duration::from_millis(1500);

/// A pointer: `ApplyMonitorsConfig` retries after a serial race (the first attempt is extra).
const SERIAL_RETRIES: usize = 3;
/// The pixel pitch Mutter's virtual monitor has no EDID for: 96 dpi.
const MM_PER_PX_96DPI: f64 = 25.4 / 96.0;

// ---- the seams ---------------------------------------------------------------------------------

/// The Shell bridge calls the twin makes besides window moves (`twin_parking` makes those): the
/// layout snapshot and the pointer fence. [`ShellBridge`] is the real one.
pub(super) trait Layout: Send + Sync {
    fn save_layout(&self) -> Result<u32, PlatformError>;
    fn restore_layout(&self, token: u32, skip: &[u64]) -> Result<u32, PlatformError>;
    fn set_pointer_fence(
        &self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<(), PlatformError>;
    fn clear_pointer_fence(&self) -> Result<(), PlatformError>;
}

/// The Mutter DisplayConfig calls the twin makes. [`DisplayConfig`] is the real one; it can only
/// ever apply a temporary configuration.
pub(super) trait Monitors: Send + Sync {
    fn current_state(&self) -> Result<DisplayState, PlatformError>;
    fn apply_temporary(&self, serial: u32, logical: &[LogicalConfig]) -> Result<(), PlatformError>;
    fn subscribe(&self, callback: Arc<dyn Fn() + Send + Sync>) -> Result<(), PlatformError>;
}

impl Monitors for DisplayConfig {
    fn current_state(&self) -> Result<DisplayState, PlatformError> {
        DisplayConfig::current_state(self)
    }

    fn apply_temporary(&self, serial: u32, logical: &[LogicalConfig]) -> Result<(), PlatformError> {
        DisplayConfig::apply_temporary(self, serial, logical)
    }

    fn subscribe(&self, callback: Arc<dyn Fn() + Send + Sync>) -> Result<(), PlatformError> {
        DisplayConfig::subscribe(self, callback)
    }
}

/// The bridge the twin uses: a connection that is replaced once, at most once a second, when the
/// extension goes away and comes back (the mirror parking and the window capture do the same for
/// their own connections). Fences belong to a connection, so a new one starts without one.
struct SharedBridge {
    bridge: Mutex<ShellBridge>,
    last_attempt: Mutex<Option<Instant>>,
}

impl SharedBridge {
    fn new(bridge: ShellBridge) -> SharedBridge {
        SharedBridge {
            bridge: Mutex::new(bridge),
            last_attempt: Mutex::new(None),
        }
    }

    /// `op` on the current connection; if the bridge says it is lost, on a fresh one (once).
    fn run<T>(
        &self,
        op: impl Fn(&ShellBridge) -> Result<T, PlatformError>,
    ) -> Result<T, PlatformError> {
        let bridge = lock(&self.bridge).clone();
        match op(&bridge) {
            Err(error) if bridge_lost(&error) => match self.reconnect() {
                Some(fresh) => op(&fresh),
                None => Err(error),
            },
            other => other,
        }
    }

    fn reconnect(&self) -> Option<ShellBridge> {
        let mut last = lock(&self.last_attempt);
        if last.is_some_and(|at| at.elapsed() < Duration::from_secs(1)) {
            return None;
        }
        let connected = ShellBridge::connect();
        *last = Some(Instant::now());
        match connected {
            Ok(fresh) => {
                tracing::info!("twin: the Shell bridge is connected again");
                *lock(&self.bridge) = fresh.clone();
                Some(fresh)
            }
            Err(error) => {
                tracing::warn!(%error, "twin: could not connect to the Shell bridge again");
                None
            }
        }
    }
}

impl Layout for SharedBridge {
    fn save_layout(&self) -> Result<u32, PlatformError> {
        self.run(ShellBridge::save_layout)
    }

    fn restore_layout(&self, token: u32, skip: &[u64]) -> Result<u32, PlatformError> {
        self.run(|bridge| bridge.restore_layout(token, skip))
    }

    fn set_pointer_fence(
        &self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<(), PlatformError> {
        self.run(|bridge| bridge.set_pointer_fence(x, y, width, height))
    }

    fn clear_pointer_fence(&self) -> Result<(), PlatformError> {
        self.run(ShellBridge::clear_pointer_fence)
    }
}

/// What the twin needs from its virtual screen. [`VirtualScreen`] is the real one.
pub(super) trait Screen: FrameCapture + Send {
    fn resize(&mut self, size: PixelSize) -> Result<(), PlatformError>;
    fn is_live(&self) -> bool;
    fn on_lost(&self, callback: Arc<dyn Fn() + Send + Sync>);
}

impl Screen for VirtualScreen {
    fn resize(&mut self, size: PixelSize) -> Result<(), PlatformError> {
        VirtualScreen::resize(self, size)
    }

    fn is_live(&self) -> bool {
        VirtualScreen::is_live(self)
    }

    fn on_lost(&self, callback: Arc<dyn Fn() + Send + Sync>) {
        VirtualScreen::on_lost(self, callback);
    }
}

/// Makes a virtual screen of the given size (device pixels).
pub(super) type OpenScreen =
    Box<dyn Fn(PixelSize) -> Result<Box<dyn Screen>, PlatformError> + Send + Sync>;

type ScreenHandle = Arc<Mutex<Box<dyn Screen>>>;
/// Called, off the caller's locks, with the windows that were parked on a twin that is gone.
type Listener = Arc<dyn Fn(&[u64]) + Send + Sync>;

/// The waits and deadlines of the twin; tests use short ones.
#[derive(Clone, Copy, Debug)]
pub(super) struct Timing {
    pub linger: Duration,
    pub fence_rearm: Duration,
    /// How often, and for how long, the new monitor is waited for in `GetCurrentState`.
    pub poll_interval: Duration,
    pub poll_budget: Duration,
    /// How long after the last `MonitorsChanged` the layout is looked at.
    pub debounce: Duration,
    /// How long a capture stream may have no frame before the twin forces one.
    pub first_frame: Duration,
}

impl Timing {
    pub(super) const DEFAULT: Timing = Timing {
        linger: TWIN_LINGER,
        fence_rearm: FENCE_REARM,
        poll_interval: Duration::from_millis(10),
        poll_budget: Duration::from_secs(2),
        debounce: Duration::from_millis(250),
        first_frame: Duration::from_millis(600),
    };
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn backend(message: impl fmt::Display) -> PlatformError {
    PlatformError::Backend(message.to_string())
}

// ---- the twin ----------------------------------------------------------------------------------

/// What `ensure` hands the parking: the twin as it is now.
#[derive(Clone, Debug)]
pub(super) struct TwinView {
    /// The twin as a display: its id, device size, effective scale and logical origin.
    pub display: DisplayInfo,
    /// The twin was created, grown or moved by this call: windows already on it may have moved.
    pub changed: bool,
}

/// What other threads read about the live twin without waiting for a sequence.
#[derive(Clone)]
struct Published {
    info: DisplayInfo,
    screen: ScreenHandle,
}

/// The twin that exists.
struct Live {
    screen: ScreenHandle,
    /// Set by the screen's own loss callback.
    lost: Arc<AtomicBool>,
    connector: String,
    /// What the screen was asked for (device pixels).
    size: PixelSize,
    /// The scale of the twin's logical monitor.
    scale: f64,
    /// The physical layout to put back after every relayout.
    snapshot: Vec<LogicalState>,
    rect: Rect,
    info: DisplayInfo,
}

impl Live {
    fn is_dead(&self) -> bool {
        self.lost.load(Ordering::SeqCst) || !lock(&self.screen).is_live()
    }
}

#[derive(Default)]
struct State {
    live: Option<Live>,
    /// The windows parked on the twin (their Shell ids), for `RestoreLayout`'s skip list and for
    /// knowing when the twin is idle.
    parked: BTreeSet<u64>,
    /// Windows that were parked on a twin that is gone and have not been released since: the
    /// caller is restoring them, and until it has, they are not parked on a new twin.
    lost: BTreeSet<u64>,
    /// When the last parked window was released.
    idle_since: Option<Instant>,
    /// How many times this twin was grown to start a stream (`Core::poke`).
    nudges: u32,
}

/// The most times one twin is grown to make a stream start.
const MAX_NUDGES: u32 = 8;

struct Core {
    layout: Box<dyn Layout>,
    monitors: Box<dyn Monitors>,
    open: OpenScreen,
    timing: Timing,
    state: Mutex<State>,
    published: Mutex<Option<Published>>,
    fence: Mutex<Fence>,
    timers: Arc<Timers>,
    listeners: Mutex<Vec<Listener>>,
}

/// The twin manager. Cloning shares it.
#[derive(Clone)]
pub struct GnomeTwin {
    core: Arc<Core>,
}

impl fmt::Debug for GnomeTwin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("GnomeTwin");
        // Not `lock`: a sequence holds the state for seconds, and Debug must not wait for it.
        if let Ok(st) = self.core.state.try_lock() {
            debug
                .field("live", &st.live.is_some())
                .field("parked", &st.parked.len());
        }
        debug.finish_non_exhaustive()
    }
}

impl GnomeTwin {
    /// The twin manager for this session: connects to Mutter's DisplayConfig (`Unsupported` when
    /// it is not there) and uses `bridge` for the layout snapshot and the pointer fence. Nothing
    /// is created until a window is parked. `bridge` must speak version 2.
    pub fn new(
        gate: Arc<IoGate>,
        config: VirtualScreenConfig,
        bridge: ShellBridge,
    ) -> Result<GnomeTwin, PlatformError> {
        let monitors = DisplayConfig::connect()?;
        let open: OpenScreen = Box::new(move |size| {
            VirtualScreen::open(Arc::clone(&gate), config.clone(), size)
                .map(|screen| Box::new(screen) as Box<dyn Screen>)
        });
        GnomeTwin::with_parts(
            Box::new(SharedBridge::new(bridge)),
            Box::new(monitors),
            open,
            Timing::DEFAULT,
        )
    }

    pub(super) fn with_parts(
        layout: Box<dyn Layout>,
        monitors: Box<dyn Monitors>,
        open: OpenScreen,
        timing: Timing,
    ) -> Result<GnomeTwin, PlatformError> {
        let timers = Arc::new(Timers::default());
        let changed = Arc::clone(&timers);
        if let Err(error) = monitors.subscribe(Arc::new(move || changed.touch_changed())) {
            // Without the signal the snapshot is never refreshed after a layout change by the
            // user; the twin itself works.
            tracing::warn!(%error, "twin: monitor changes are not watched");
        }
        let core = Arc::new(Core {
            layout,
            monitors,
            open,
            timing,
            state: Mutex::new(State::default()),
            published: Mutex::new(None),
            fence: Mutex::new(Fence::default()),
            timers: Arc::clone(&timers),
            listeners: Mutex::new(Vec::new()),
        });
        let weak = Arc::downgrade(&core);
        thread::Builder::new()
            .name("crosspane-gnome-twin".into())
            .spawn(move || janitor(&timers, &weak, timing))
            .map_err(|e| backend(format!("twin timer thread: {e}")))?;
        Ok(GnomeTwin { core })
    }

    /// Registers a listener for the loss of the twin: the user stopped its sharing, PipeWire or
    /// the portal failed, or a growth failed and the twin was dropped. It gets the ids of the
    /// windows that were parked on it (they are no longer hidden: the caller restores them), and
    /// runs on the twin's timer thread with none of the twin's locks held.
    pub(super) fn on_lost(&self, listener: Listener) {
        lock(&self.core.listeners).push(listener);
    }

    /// Makes sure a twin exists that holds `needed` device pixels for `window`, creating or
    /// growing it with the sequence in the module documentation, and registers `window` as parked
    /// on it. `dest_scale` is the destination's scale (the twin's scale is chosen from it when the
    /// twin is created or no other window is on it). `Unsupported` means "mirror this window";
    /// `Locked` means the gate is closed.
    pub(super) fn ensure(
        &self,
        window: u64,
        needed: PixelSize,
        dest_scale: f64,
    ) -> Result<TwinView, PlatformError> {
        let core = &self.core;
        let mut st = lock(&core.state);
        // A twin that died and was not yet handled is replaced, not reused.
        if st.live.as_ref().is_some_and(Live::is_dead) {
            core.teardown(&mut st, true);
        }
        // A window that was on a twin that is gone is being restored by the listeners; it is not
        // moved onto a new twin meanwhile.
        if st.lost.contains(&window) {
            return Err(backend("the virtual monitor this window was on is gone"));
        }
        let known = st.parked.contains(&window);
        let result = core.ensure_locked(&mut st, window, needed, dest_scale);
        match &result {
            Ok(_) => {
                st.parked.insert(window);
                st.idle_since = None;
            }
            Err(_) => {
                if !known {
                    st.parked.remove(&window);
                }
                if st.live.is_some() && st.parked.is_empty() {
                    core.start_idle(&mut st);
                }
            }
        }
        result
    }

    /// `window` is no longer parked on the twin (restored, or its park failed). When none is left
    /// the twin lingers for [`TWIN_LINGER`] and then goes.
    pub(super) fn release(&self, window: u64) {
        let core = &self.core;
        let mut st = lock(&core.state);
        st.parked.remove(&window);
        st.lost.remove(&window);
        if st.live.is_some() && st.parked.is_empty() {
            core.start_idle(&mut st);
        }
    }

    /// The windows registered as parked on the twin now.
    pub(super) fn parked(&self) -> Vec<u64> {
        lock(&self.core.state).parked.iter().copied().collect()
    }

    /// The physical layout the twin puts back (empty without a twin).
    #[cfg(test)]
    pub(super) fn snapshot(&self) -> Vec<LogicalState> {
        lock(&self.core.state)
            .live
            .as_ref()
            .map(|live| live.snapshot.clone())
            .unwrap_or_default()
    }

    /// The twin as it is now, if one exists.
    pub(super) fn view(&self) -> Option<TwinView> {
        self.core.published().map(|published| TwinView {
            display: published.info,
            changed: false,
        })
    }

    /// The list of displays the EIS injector maps coordinates with: `physical` plus the twin's
    /// own [`DisplayInfo`] while it exists. The agent advertises `physical` alone.
    pub fn displays(&self, physical: DisplaysFn) -> DisplaysFn {
        let weak = Arc::downgrade(&self.core);
        Arc::new(move || {
            let mut all = physical();
            if let Some(core) = weak.upgrade()
                && let Some(published) = core.published()
            {
                all.push(published.info);
            }
            all
        })
    }

    /// The hook for `EisSource::set_before_move`: moves that target the twin lower the fence.
    pub fn before_move(&self) -> MoveHook {
        let weak = Arc::downgrade(&self.core);
        Arc::new(move |display| {
            if let Some(core) = weak.upgrade() {
                core.on_move(display);
            }
        })
    }

    /// Wraps `inner`: `CaptureTarget::Display(twin id)` is served by the twin's stream, anything
    /// else by `inner`.
    pub fn capture(&self, inner: Box<dyn FrameCapture>) -> TwinCapture {
        TwinCapture::new(self.clone(), inner)
    }

    /// The twin's screen when `display` is the twin.
    fn screen_for(&self, display: DisplayId) -> Option<ScreenHandle> {
        self.core
            .published()
            .filter(|published| published.info.id == display)
            .map(|published| published.screen)
    }

    /// Asks the timer thread to force a frame if `saw_frame` is still unset in a moment.
    fn watch_first_frame(&self, saw_frame: Arc<AtomicBool>) {
        self.core
            .timers
            .add_watch(Instant::now() + self.core.timing.first_frame, saw_frame);
    }
}

impl Core {
    fn published(&self) -> Option<Published> {
        lock(&self.published).clone()
    }

    /// An absolute move was asked for on `display` (the EIS hook).
    fn on_move(&self, display: DisplayId) {
        let on_twin = lock(&self.published)
            .as_ref()
            .is_some_and(|published| published.info.id == display);
        if !on_twin {
            return;
        }
        let next = lock(&self.fence).note_move(
            self.layout.as_ref(),
            Instant::now(),
            self.timing.fence_rearm,
        );
        self.timers.set_rearm(next);
    }

    fn start_idle(&self, st: &mut State) {
        let now = Instant::now();
        st.idle_since = Some(now);
        self.timers.set_linger(Some(now + self.timing.linger));
    }

    fn ensure_locked(
        &self,
        st: &mut State,
        window: u64,
        needed: PixelSize,
        dest_scale: f64,
    ) -> Result<TwinView, PlatformError> {
        let current = st.live.as_ref().map(|live| live.size);
        match sizing::plan(needed, current) {
            None => Err(PlatformError::Unsupported(
                "the window is larger than a virtual monitor can be",
            )),
            Some(sizing::Resize::Keep) => {
                let live = st.live.as_ref().ok_or_else(|| backend("twin state lost"))?;
                Ok(TwinView {
                    display: live.info.clone(),
                    changed: false,
                })
            }
            Some(sizing::Resize::To(size)) => {
                // The window is registered first: it is on the skip list of the layout restore.
                st.parked.insert(window);
                st.idle_since = None;
                self.timers.set_linger(None);
                self.bring_up(st, size, dest_scale)
            }
        }
    }

    /// Creates the twin at `size`, or grows the live one to it: the sequence of the module
    /// documentation.
    fn bring_up(
        &self,
        st: &mut State,
        size: PixelSize,
        dest_scale: f64,
    ) -> Result<TwinView, PlatformError> {
        let growing = st.live.is_some();
        let skip: Vec<u64> = st.parked.iter().copied().collect();
        // 1. The layout to put back. A creation reads it now, while no `Meta-*` monitor exists.
        let snapshot = match st.live.as_ref() {
            Some(live) => live.snapshot.clone(),
            None => self.read_snapshot().map_err(|e| refusal("snapshot", e))?,
        };
        // 2. The window frames, before anything changes.
        let token = self
            .layout
            .save_layout()
            .map_err(|e| refusal("save_layout", e))?;
        let built = if growing {
            self.grow(st, size, dest_scale, &snapshot)
        } else {
            self.create(size, dest_scale, snapshot)
        };
        let live = match built {
            Ok(live) => live,
            Err(error) => {
                let gate_closed = matches!(error, PlatformError::Locked);
                if !(growing && gate_closed) {
                    // Drops the screen: Mutter puts its stored layout back. A growth that failed
                    // takes the windows on the twin with it.
                    self.teardown(st, growing);
                }
                self.restore_frames(token, &skip);
                return Err(refusal("bring-up", error));
            }
        };
        // 6. The saved frames, except the windows being parked (a failure is not fatal).
        self.restore_frames(token, &skip);
        // 7. The fence.
        let next = lock(&self.fence).target(
            self.layout.as_ref(),
            live.rect,
            Instant::now(),
            self.timing.fence_rearm,
        );
        self.timers.set_rearm(next);
        let view = TwinView {
            display: live.info.clone(),
            changed: true,
        };
        *lock(&self.published) = Some(Published {
            info: live.info.clone(),
            screen: Arc::clone(&live.screen),
        });
        st.live = Some(live);
        Ok(view)
    }

    /// Step 1: the physical layout, while no `Meta-*` monitor exists.
    fn read_snapshot(&self) -> Result<Vec<LogicalState>, PlatformError> {
        let state = self.monitors.current_state()?;
        if state
            .monitors
            .iter()
            .any(|monitor| monitor.connector.starts_with(TWIN_PREFIX))
        {
            return Err(PlatformError::Unsupported(
                "another virtual monitor exists; Crosspane leaves it alone",
            ));
        }
        let snapshot = physical(&state);
        if snapshot.is_empty() {
            return Err(PlatformError::Unsupported("Mutter reports no monitors"));
        }
        Ok(snapshot)
    }

    /// Steps 3 to 5 for a new twin.
    fn create(
        &self,
        size: PixelSize,
        dest_scale: f64,
        snapshot: Vec<LogicalState>,
    ) -> Result<Live, PlatformError> {
        let screen = (self.open)(size)?;
        let lost = Arc::new(AtomicBool::new(false));
        {
            let lost = Arc::clone(&lost);
            let timers = Arc::clone(&self.timers);
            screen.on_lost(Arc::new(move || {
                lost.store(true, Ordering::SeqCst);
                timers.flag_check_live();
            }));
        }
        let screen: ScreenHandle = Arc::new(Mutex::new(screen));
        // On an error the handle drops here: the screen closes and Mutter restores its layout.
        let laid = self.lay_out(size, None, None, dest_scale, &snapshot)?;
        Ok(Live {
            screen,
            lost,
            size,
            connector: laid.connector,
            scale: laid.scale,
            snapshot,
            rect: laid.rect,
            info: laid.info,
        })
    }

    /// Steps 3 to 5 for the live twin. On an error the live twin is still in `st`, for the caller
    /// to tear down (or, for a closed gate, to keep).
    fn grow(
        &self,
        st: &State,
        size: PixelSize,
        dest_scale: f64,
        snapshot: &[LogicalState],
    ) -> Result<Live, PlatformError> {
        let live = st.live.as_ref().ok_or_else(|| backend("twin state lost"))?;
        lock(&live.screen).resize(size)?;
        // Windows already on the twin keep their density while the mode still supports it.
        let keep = (st.parked.len() > 1).then_some(live.scale);
        let laid = self.lay_out(size, Some(&live.connector), keep, dest_scale, snapshot)?;
        Ok(Live {
            screen: Arc::clone(&live.screen),
            lost: Arc::clone(&live.lost),
            size,
            connector: laid.connector,
            scale: laid.scale,
            snapshot: snapshot.to_vec(),
            rect: laid.rect,
            info: laid.info,
        })
    }

    /// Steps 4 and 5: wait for the monitor at `size`, then put the physical layout back around it.
    fn lay_out(
        &self,
        size: PixelSize,
        prefer: Option<&str>,
        keep_scale: Option<f64>,
        dest_scale: f64,
        snapshot: &[LogicalState],
    ) -> Result<Laid, PlatformError> {
        let (mut state, connector, mode) = self.await_monitor(size, prefer)?;
        let scale = sizing::keep_or_pick(keep_scale, &mode, dest_scale);
        let mut attempt = 0;
        let config = loop {
            let config = plan(&state, snapshot, &connector, scale)
                .ok_or_else(|| backend("the monitor layout cannot be planned"))?;
            match self.monitors.apply_temporary(state.serial, &config) {
                Ok(()) => break config,
                Err(error) if is_serial_race(&error) && attempt < SERIAL_RETRIES => {
                    attempt += 1;
                    tracing::debug!(attempt, "twin: the monitor layout moved on; trying again");
                    state = self.monitors.current_state()?;
                }
                Err(error) => return Err(error),
            }
        };
        let rect = twin_rect(&config, &state, &connector)
            .ok_or_else(|| backend("the twin's rectangle cannot be derived"))?;
        // `await_monitor` found this mode in `state`, which `plan` has just used.
        let mode = state
            .monitors
            .iter()
            .find(|monitor| monitor.connector == connector)
            .and_then(|monitor| monitor.modes.iter().find(|m| m.current))
            .unwrap_or(&mode);
        let info = twin_info(&connector, mode, rect)
            .ok_or_else(|| backend("the twin's display cannot be described"))?;
        Ok(Laid {
            connector,
            scale,
            rect,
            info,
        })
    }

    /// Step 4: `GetCurrentState` until a `Meta-*` monitor runs at `size`.
    fn await_monitor(
        &self,
        size: PixelSize,
        prefer: Option<&str>,
    ) -> Result<(DisplayState, String, ModeState), PlatformError> {
        let deadline = Instant::now() + self.timing.poll_budget;
        loop {
            let state = self.monitors.current_state()?;
            if let Some((connector, mode)) = find_twin(&state, size, prefer) {
                return Ok((state, connector, mode));
            }
            if Instant::now() >= deadline {
                return Err(PlatformError::Timeout);
            }
            thread::sleep(self.timing.poll_interval);
        }
    }

    /// Step 6, and the undo of a failed attempt: failures are only logged.
    fn restore_frames(&self, token: u32, skip: &[u64]) {
        if let Err(error) = self.layout.restore_layout(token, skip) {
            tracing::warn!(%error, "twin: the window layout was not restored");
        }
    }

    /// Drops the live twin: out of sight first, then the fence, then the screen (the portal
    /// session closes and Mutter removes the monitor). Returns the windows that were parked on it;
    /// with `notify` the listeners are told to restore them.
    fn teardown(&self, st: &mut State, notify: bool) -> Vec<u64> {
        let victims: Vec<u64> = st.parked.iter().copied().collect();
        st.parked.clear();
        st.idle_since = None;
        st.nudges = 0;
        self.timers.set_linger(None);
        *lock(&self.published) = None;
        if let Some(live) = st.live.take() {
            lock(&self.fence).clear(self.layout.as_ref());
            self.timers.set_rearm(None);
            drop(live);
        }
        if notify && !victims.is_empty() {
            st.lost.extend(victims.iter().copied());
            self.timers.push_batch(victims.clone());
        }
        victims
    }

    // ---- the timer thread's work ----

    fn run(&self, work: Work) {
        if work.check_live {
            let mut st = lock(&self.state);
            if st.live.as_ref().is_some_and(Live::is_dead) {
                tracing::warn!("twin: the virtual monitor was lost");
                self.teardown(&mut st, true);
            }
        }
        let mut batches = work.batches;
        batches.extend(self.timers.take_batches());
        for victims in batches {
            self.notify_lost(&victims);
        }
        if work.changed {
            self.refresh_layout();
        }
        if work.rearm {
            let next = lock(&self.fence).rearm_due(
                self.layout.as_ref(),
                Instant::now(),
                self.timing.fence_rearm,
            );
            self.timers.set_rearm(next);
        }
        if work.linger {
            self.linger();
        }
        if work.poke {
            self.poke();
        }
    }

    fn notify_lost(&self, victims: &[u64]) {
        let listeners: Vec<Listener> = lock(&self.listeners).clone();
        for listener in listeners {
            listener(victims);
        }
    }

    /// The user's layout may have changed under the twin: adopt it as the one to put back.
    fn refresh_layout(&self) {
        let mut st = lock(&self.state);
        let Some(live) = st.live.as_mut() else {
            return;
        };
        let state = match self.monitors.current_state() {
            Ok(state) => state,
            Err(error) => {
                tracing::debug!(%error, "twin: monitor change not read");
                return;
            }
        };
        let Some(logical) = state
            .logical
            .iter()
            .find(|l| l.connectors.contains(&live.connector))
        else {
            // The twin is gone from Mutter's state; the screen's own loss handling follows.
            return;
        };
        let now_physical = physical(&state);
        if !same_layout(&now_physical, &live.snapshot) {
            tracing::info!("twin: the monitor layout was changed; the new one is kept");
            live.snapshot = now_physical;
        }
        let mode_id = state
            .monitors
            .iter()
            .find(|monitor| monitor.connector == live.connector)
            .and_then(|monitor| monitor.modes.iter().find(|mode| mode.current))
            .map(|mode| mode.id.clone());
        let Some(mode_id) = mode_id else {
            return;
        };
        let config = LogicalConfig {
            x: logical.x,
            y: logical.y,
            scale: logical.scale,
            transform: logical.transform,
            primary: logical.primary,
            monitors: vec![(live.connector.clone(), mode_id)],
        };
        let Some(rect) = twin_rect(&[config], &state, &live.connector) else {
            return;
        };
        if rect == live.rect {
            return;
        }
        let Some(mode) = state
            .monitors
            .iter()
            .find(|monitor| monitor.connector == live.connector)
            .and_then(|monitor| monitor.modes.iter().find(|mode| mode.current))
        else {
            return;
        };
        let Some(info) = twin_info(&live.connector, mode, rect) else {
            return;
        };
        tracing::info!("twin: the virtual monitor moved");
        live.rect = rect;
        live.info = info.clone();
        *lock(&self.published) = Some(Published {
            info,
            screen: Arc::clone(&live.screen),
        });
        let next = lock(&self.fence).target(
            self.layout.as_ref(),
            rect,
            Instant::now(),
            self.timing.fence_rearm,
        );
        self.timers.set_rearm(next);
    }

    /// The linger ran out: drop the twin if it is still idle.
    fn linger(&self) {
        let mut st = lock(&self.state);
        let Some(since) = st.idle_since else {
            return;
        };
        if st.live.is_none() || !st.parked.is_empty() {
            return;
        }
        let due = since + self.timing.linger;
        if Instant::now() >= due {
            tracing::debug!("twin: idle; dropping the virtual monitor");
            // Removing the monitor is a relayout too, and a relayout is what un-tiled a window
            // when the monitor appeared: the window frames are saved first and put back once
            // Mutter has dropped the monitor. Best effort, like every use of the snapshot.
            let token = match self.layout.save_layout() {
                Ok(token) => Some(token),
                Err(error) => {
                    tracing::debug!(%error, "twin: window layout not saved before the drop");
                    None
                }
            };
            self.teardown(&mut st, false);
            if let Some(token) = token {
                self.await_monitor_gone();
                self.restore_frames(token, &[]);
            }
        } else {
            self.timers.set_linger(Some(due));
        }
    }

    /// After the screen was dropped: `GetCurrentState` until no `Meta-*` monitor is left (at most
    /// the poll budget). Errors end the wait: the caller's next step is best effort anyway.
    fn await_monitor_gone(&self) {
        let deadline = Instant::now() + self.timing.poll_budget;
        loop {
            match self.monitors.current_state() {
                Ok(state)
                    if state
                        .monitors
                        .iter()
                        .any(|monitor| monitor.connector.starts_with(TWIN_PREFIX)) => {}
                _ => return,
            }
            if Instant::now() >= deadline {
                return;
            }
            thread::sleep(self.timing.poll_interval);
        }
    }

    /// A capture stream got no frame (the gate had closed and a static window repaints nothing):
    /// grow the twin by one step, which makes Mutter send the new size. At most
    /// [`MAX_NUDGES`] times per twin: every step is a relayout, and the width only grows.
    fn poke(&self) {
        let mut st = lock(&self.state);
        let Some(live) = st.live.as_ref() else {
            return;
        };
        if st.parked.is_empty() {
            return;
        }
        if st.nudges >= MAX_NUDGES {
            tracing::debug!("twin: no more growing to start streams on this twin");
            return;
        }
        let (width, height) = (live.size.width, live.size.height);
        let target = if width < sizing::MAX_EXTENT {
            PixelSize::new((width + sizing::STEP).min(sizing::MAX_EXTENT), height)
        } else if height < sizing::MAX_EXTENT {
            PixelSize::new(width, (height + sizing::STEP).min(sizing::MAX_EXTENT))
        } else {
            return;
        };
        let scale = live.scale;
        st.nudges += 1;
        match self.bring_up(&mut st, target, scale) {
            Ok(_) => tracing::debug!("twin: grown one step to start a stream"),
            Err(error) => tracing::warn!(%error, "twin: could not start a stream by growing"),
        }
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        self.timers.stop();
        *self
            .published
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner) = None;
        // The fence goes down while the twin still exists; then the screen drops, which closes
        // the portal session and removes the monitor.
        self.fence
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .clear(self.layout.as_ref());
        let st = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
        drop(st.live.take());
    }
}

/// What steps 4 and 5 learned.
struct Laid {
    connector: String,
    scale: f64,
    rect: Rect,
    info: DisplayInfo,
}

/// Logs why the sequence failed and maps the error for the parking: a closed gate stays `Locked`
/// (no mirror fallback while locked), what is already `Unsupported` stays so, anything else
/// becomes `Unsupported` so that `TwinOrMirror` mirrors the window.
fn refusal(step: &str, error: PlatformError) -> PlatformError {
    match error {
        PlatformError::Locked => PlatformError::Locked,
        PlatformError::Unsupported(reason) => {
            tracing::debug!(step, reason, "twin: not available");
            PlatformError::Unsupported(reason)
        }
        other => {
            tracing::warn!(step, error = %other, "twin: could not be set up; mirroring instead");
            PlatformError::Unsupported("the virtual monitor could not be set up")
        }
    }
}

/// The `Meta-*` monitor of `state` whose current mode is `size`; `prefer` wins when several are.
fn find_twin(
    state: &DisplayState,
    size: PixelSize,
    prefer: Option<&str>,
) -> Option<(String, ModeState)> {
    let (width, height) = (
        i32::try_from(size.width).ok()?,
        i32::try_from(size.height).ok()?,
    );
    let mut found: Vec<(String, ModeState)> = state
        .monitors
        .iter()
        .filter(|monitor| monitor.connector.starts_with(TWIN_PREFIX))
        .filter_map(|monitor| {
            let mode = monitor
                .modes
                .iter()
                .find(|mode| mode.current && mode.width == width && mode.height == height)?;
            Some((monitor.connector.clone(), mode.clone()))
        })
        .collect();
    let at = prefer
        .and_then(|name| found.iter().position(|(connector, _)| connector == name))
        .unwrap_or(0);
    (at < found.len()).then(|| found.swap_remove(at))
}

/// Whether two physical layouts are the same set of logical monitors.
fn same_layout(a: &[LogicalState], b: &[LogicalState]) -> bool {
    a.len() == b.len() && a.iter().all(|l| b.contains(l))
}

/// The twin as a display. The scale is the effective one (device pixels over the logical width),
/// as `wayland_outputs` derives it for the real displays.
fn twin_info(connector: &str, mode: &ModeState, rect: Rect) -> Option<DisplayInfo> {
    let (x, y, logical_width, _) = rect;
    let (pixel_width, pixel_height) = (
        u32::try_from(mode.width).ok()?,
        u32::try_from(mode.height).ok()?,
    );
    let scale = sizing::effective_scale(pixel_width, logical_width)?;
    let millihz = (mode.refresh * 1000.0).round();
    Some(DisplayInfo {
        id: display_id(connector),
        name: format!("{connector} (Crosspane virtual monitor)"),
        geometry: DisplayGeometry {
            physical_size: SizeMm::new(
                f64::from(pixel_width) * MM_PER_PX_96DPI,
                f64::from(pixel_height) * MM_PER_PX_96DPI,
            ),
            pixel_size: PixelSize::new(pixel_width, pixel_height),
            scale,
            logical_origin: PointLogical::new(f64::from(x), f64::from(y)),
        },
        refresh_millihz: if millihz.is_finite() && (0.0..=f64::from(u32::MAX)).contains(&millihz) {
            millihz as u32
        } else {
            0
        },
        color_space: ColorSpace::Srgb,
        hdr: false,
    })
}

// ---- timers ------------------------------------------------------------------------------------

/// What the timer thread waits for, shared with everything that sets a deadline.
#[derive(Default)]
struct Timers {
    state: Mutex<TimerState>,
    wake: Condvar,
}

#[derive(Default)]
struct TimerState {
    stop: bool,
    /// A screen reported its loss: look at the live twin.
    check_live: bool,
    /// `MonitorsChanged` last arrived at this time.
    changed_at: Option<Instant>,
    rearm_at: Option<Instant>,
    linger_at: Option<Instant>,
    /// Windows parked on twins that are gone, to be handed to the listeners.
    batches: Vec<Vec<u64>>,
    /// Capture streams to look at: (when, whether a frame has arrived).
    watches: Vec<(Instant, Arc<AtomicBool>)>,
}

/// What came due.
#[derive(Default)]
struct Work {
    check_live: bool,
    batches: Vec<Vec<u64>>,
    changed: bool,
    rearm: bool,
    linger: bool,
    poke: bool,
}

impl Work {
    fn is_empty(&self) -> bool {
        !(self.check_live
            || !self.batches.is_empty()
            || self.changed
            || self.rearm
            || self.linger
            || self.poke)
    }
}

impl Timers {
    fn with<R>(&self, f: impl FnOnce(&mut TimerState) -> R) -> R {
        let result = f(&mut lock(&self.state));
        self.wake.notify_all();
        result
    }

    /// Records a deadline and wakes the timer thread only if it is sooner than the one it sleeps
    /// for: a later one is found when the thread wakes at the old one. (The fence re-arm moves
    /// with every pointer move, which must not wake a thread each time.)
    fn set_deadline(&self, slot: fn(&mut TimerState) -> &mut Option<Instant>, at: Option<Instant>) {
        let sooner = {
            let mut state = lock(&self.state);
            let slot = slot(&mut state);
            let sooner = at.is_some_and(|new| slot.is_none_or(|old| new < old));
            *slot = at;
            sooner
        };
        if sooner {
            self.wake.notify_all();
        }
    }

    fn touch_changed(&self) {
        // Bursts of signals only move the quiet-time deadline later; the first one wakes the thread.
        let first = {
            let mut state = lock(&self.state);
            let first = state.changed_at.is_none();
            state.changed_at = Some(Instant::now());
            first
        };
        if first {
            self.wake.notify_all();
        }
    }

    fn flag_check_live(&self) {
        self.with(|t| t.check_live = true);
    }

    fn set_rearm(&self, at: Option<Instant>) {
        self.set_deadline(|t| &mut t.rearm_at, at);
    }

    fn set_linger(&self, at: Option<Instant>) {
        self.set_deadline(|t| &mut t.linger_at, at);
    }

    fn push_batch(&self, victims: Vec<u64>) {
        self.with(|t| t.batches.push(victims));
    }

    fn take_batches(&self) -> Vec<Vec<u64>> {
        std::mem::take(&mut lock(&self.state).batches)
    }

    fn add_watch(&self, at: Instant, saw_frame: Arc<AtomicBool>) {
        self.with(|t| t.watches.push((at, saw_frame)));
    }

    fn stop(&self) {
        self.with(|t| t.stop = true);
    }
}

impl TimerState {
    /// Takes what is due at `now`.
    fn take_due(&mut self, now: Instant, debounce: Duration) -> Work {
        let mut work = Work {
            check_live: std::mem::take(&mut self.check_live),
            batches: std::mem::take(&mut self.batches),
            ..Work::default()
        };
        if self.changed_at.is_some_and(|at| at + debounce <= now) {
            self.changed_at = None;
            work.changed = true;
        }
        if self.rearm_at.is_some_and(|at| at <= now) {
            self.rearm_at = None;
            work.rearm = true;
        }
        if self.linger_at.is_some_and(|at| at <= now) {
            self.linger_at = None;
            work.linger = true;
        }
        self.watches.retain(|(at, saw_frame)| {
            if *at > now {
                return true;
            }
            if !saw_frame.load(Ordering::SeqCst) {
                work.poke = true;
            }
            false
        });
        work
    }

    /// The earliest deadline, if any.
    fn next_deadline(&self, debounce: Duration) -> Option<Instant> {
        [
            self.changed_at.map(|at| at + debounce),
            self.rearm_at,
            self.linger_at,
        ]
        .into_iter()
        .flatten()
        .chain(self.watches.iter().map(|(at, _)| *at))
        .min()
    }
}

/// The body of the timer thread. It holds the twin weakly, so it never keeps the twin (and with it
/// a portal session) alive, and ends when the twin is dropped.
fn janitor(timers: &Timers, core: &Weak<Core>, timing: Timing) {
    loop {
        let work = {
            let mut state = lock(&timers.state);
            loop {
                if state.stop {
                    return;
                }
                let now = Instant::now();
                let work = state.take_due(now, timing.debounce);
                if !work.is_empty() {
                    break work;
                }
                state = match state.next_deadline(timing.debounce) {
                    Some(at) => {
                        timers
                            .wake
                            .wait_timeout(state, at.saturating_duration_since(now))
                            .unwrap_or_else(PoisonError::into_inner)
                            .0
                    }
                    None => timers
                        .wake
                        .wait(state)
                        .unwrap_or_else(PoisonError::into_inner),
                };
            }
        };
        let Some(core) = core.upgrade() else {
            return;
        };
        core.run(work);
        // The last strong reference may be this one: the twin is then dropped on this thread,
        // which has finished with it.
        drop(core);
    }
}
