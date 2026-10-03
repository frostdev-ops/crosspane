use super::{
    Abort, Delivery, Socket, SocketGuard, Source, backend, drag, events, rejected, unmark,
};
use crosspane_platform::{
    CaptureAbort, CaptureEvent, CaptureId, CapturePortal, CaptureStart, Edge, EndReason, IoGate,
    MotionKind, PlatformError, PortalId,
};
use crosspane_types::{
    geom::{PointDevice, VectorLogical},
    hid::{HidUsage, MouseButton, evdev_to_hid},
    id::DisplayId,
    input::{LockKeys, ScrollDelta, ScrollPhase},
    time::MonoTime,
};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    os::fd::AsFd,
    os::unix::{fs::FileExt, net::UnixStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry,
        wl_seat, wl_shm, wl_shm_pool, wl_surface,
    },
};
use wayland_protocols::wp::{
    cursor_shape::v1::client::{
        wp_cursor_shape_device_v1::{Shape, WpCursorShapeDeviceV1},
        wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
    },
    keyboard_shortcuts_inhibit::zv1::client::{
        zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1,
        zwp_keyboard_shortcuts_inhibitor_v1::{self, ZwpKeyboardShortcutsInhibitorV1},
    },
    pointer_constraints::zv1::client::{
        zwp_locked_pointer_v1::{self, ZwpLockedPointerV1},
        zwp_pointer_constraints_v1::{Lifetime, ZwpPointerConstraintsV1},
    },
    relative_pointer::zv1::client::{
        zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
        zwp_relative_pointer_v1::{self, ZwpRelativePointerV1},
    },
};
use wayland_protocols_wlr::{
    layer_shell::v1::client::{
        zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
        zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
    },
    virtual_pointer::v1::client::{
        zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
        zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    },
};
use xkbcommon::xkb;
use zeroize::Zeroizing;

/// The bound on one monitor-cache refresh (two short IPC requests).
const REFRESH_BOUND: Duration = Duration::from_millis(20);
/// Time a refresh leaves of the caller's budget for the strips' own roundtrip and the reply.
const REFRESH_RESERVE: Duration = Duration::from_millis(10);
/// Time `set_portals` keeps back from the caller's deadline, so that a slow compositor makes the
/// worker report its own rejection (previous set and capture intact) before the caller's receive
/// timeout fires and aborts everything.
const REPLY_RESERVE: Duration = Duration::from_millis(5);
/// How soon the idle path tries again after a failed refresh.
const REFRESH_RETRY: Duration = Duration::from_millis(250);
/// How many [`Cause`]s a connection remembers.
const CAUSES_KEPT: usize = 32;

/// How [`Client::set_portals`] treats a stale monitor cache.
#[derive(Clone, Copy)]
pub(super) enum Refresh {
    /// Refresh it first; a refresh that misses its bound rejects the set.
    Required,
    /// Rebuilding the previous set on a new connection: try to refresh, but go on with the cached
    /// list if that fails (as before WP-2.43d), so a tight budget never costs the rebuild.
    BestEffort,
    /// The caller just refreshed (or tried to): use the cache as it is.
    Done,
    /// Nested tests only: refresh with this bound whether or not the cache is stale, and sit on
    /// the freshly read list for `stall` before publishing it (a refresh that parses late).
    Forced { bound: Duration, stall: Duration },
}

/// Why the backend itself ended a capture. The compositor's doing (it unlocked the pointer, took
/// a focus away) and the backend's own decision (it removed the strip, the output went) are
/// different things; the nested tests read this log (`HyprlandCapture::end_causes_for_test`) to
/// tell them apart. Every `State::finish` names one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Cause {
    /// `set_portals` replaced or removed the captured strip's portal (backend decision).
    StripReplaced,
    /// The output holding the captured strip was removed (backend decision, §3.6.3).
    OutputRemoved,
    /// Another capture-critical global (seat, constraints, layer shell...) went away.
    GlobalRemoved,
    /// The compositor closed the captured strip's layer surface.
    StripClosed,
    /// The compositor sent `wl_pointer.leave` for the captured strip.
    PointerLeft,
    /// The compositor sent `wl_keyboard.leave` for the captured strip.
    KeyboardLeft,
    /// The compositor unlocked the pointer (`zwp_locked_pointer_v1.unlocked`). Hyprland 0.56.2
    /// does this whenever it relocates the cursor, e.g. when any output is removed.
    Unlocked,
    /// The compositor deactivated the shortcuts inhibitor.
    InhibitorInactive,
    /// The seat lost the pointer or keyboard capability.
    SeatLost,
    /// The keymap was unusable.
    KeymapUnusable,
    /// The I/O gate closed.
    GateClosed,
    /// The connection went away or was aborted.
    Disconnected,
    /// `begin` failed after the capture was partly set up.
    ActivationFailed,
}

/// An acknowledgement held back for a controlled time (nested tests, `PortalsTestHooks`).
enum Deferred {
    /// A layer-surface configure: its size becomes known to the preparation only now.
    Size(u64, (u32, u32)),
    /// A `wl_display.sync` callback: the roundtrip completes only now.
    Sync(u64),
}

pub(super) struct Client {
    _socket: SocketGuard,
    conn: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    globals: Globals,
    sync: u64,
    /// The compositor this connection was made to, for its IPC too.
    source: Source,
}
struct Globals {
    compositor: wl_compositor::WlCompositor,
    shm: wl_shm::WlShm,
    shell: ZwlrLayerShellV1,
    seat: wl_seat::WlSeat,
    pointer: wl_pointer::WlPointer,
    _keyboard: wl_keyboard::WlKeyboard,
    constraints: ZwpPointerConstraintsV1,
    relative: ZwpRelativePointerManagerV1,
    inhibit: ZwpKeyboardShortcutsInhibitManagerV1,
    virtual_pointer: ZwlrVirtualPointerManagerV1,
}
#[derive(Clone, PartialEq)]
pub(super) struct Monitor {
    pub(super) id: DisplayId,
    pub(super) name: String,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) scale: f64,
    pub(super) origin: [f64; 2],
}
struct Output {
    global: u32,
    proxy: wl_output::WlOutput,
    name: String,
    mode: Option<(u32, u32)>,
    rotated: bool,
}
struct Strip {
    portal: CapturePortal,
    /// The output (and the monitor name it had) this strip was created on; a strip is kept across
    /// a replacement only while the portal's display still resolves to the same monitor at the
    /// same scale.
    output: wl_output::WlOutput,
    monitor: String,
    /// The monitor's size in device pixels when the strip was made. Hyprland does not always move
    /// an edge-anchored layer surface when its output is resized afterwards (a nested output
    /// resized by its parent window leaves a right-edge strip behind), so a strip that is not
    /// being captured on is replaced once its output's size is no longer this.
    monitor_size: (u32, u32),
    scale: f64,
    offset: f64,
    surface: wl_surface::WlSurface,
    layer: ZwlrLayerSurfaceV1,
    size: Option<(u32, u32)>,
    buffer: Option<wl_buffer::WlBuffer>,
    mapped: bool,
    closed: bool,
}
impl Strip {
    fn destroy(self) {
        self.layer.destroy();
        self.surface.destroy();
        if let Some(b) = self.buffer {
            b.destroy();
        }
    }
}
/// One requested portal, validated and resolved to its output (`Client::set_portals`).
struct Plan {
    portal: CapturePortal,
    output: wl_output::WlOutput,
    monitor: String,
    monitor_size: (u32, u32),
    scale: f64,
    offset: f64,
    length: u32,
    /// The existing strip that stays for this portal, if any.
    keep: Option<u64>,
}
struct Capture {
    id: CaptureId,
    generation: u64,
    strip: u64,
    lock: Option<ZwpLockedPointerV1>,
    inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,
    relative: Option<ZwpRelativePointerV1>,
    locked: bool,
    inhibited: bool,
    keyboard: bool,
    started: bool,
    held: Vec<HidUsage>,
    pending: Vec<CaptureEvent>,
}
struct State {
    connection: Connection,
    socket: Arc<Socket>,
    gate: Arc<IoGate>,
    abort: Arc<Abort>,
    epoch: u64,
    advertised: Vec<(u32, String, u32)>,
    outputs: Vec<Output>,
    monitors: Vec<Monitor>,
    monitors_dirty: bool,
    /// The idle path does not retry a failed refresh before this.
    refresh_after: Instant,
    /// Counts what a rejection must not hide: a started capture ended, or a mapped strip was
    /// destroyed or closed. `set_portals` compares it before and after its dispatching steps.
    lost: u64,
    /// Why the backend ended captures, oldest first (at most [`CAUSES_KEPT`]).
    causes: Vec<Cause>,
    /// Nested tests only: how long configure and sync acknowledgements are held back.
    ack_delay: Duration,
    deferred: Vec<(Instant, Deferred)>,
    strips: BTreeMap<u64, Strip>,
    portals: BTreeMap<PortalId, u64>,
    next_strip: u64,
    entered: Option<u64>,
    pressing: Option<PortalId>,
    pointer_position: (f64, f64),
    enter_serial: u32,
    buttons: BTreeSet<u32>,
    keys: BTreeSet<u32>,
    capture: Option<Capture>,
    gesture: drag::Gesture,
    drag: drag::Poller,
    generation: u64,
    cursor: Option<WpCursorShapeDeviceV1>,
    xkb: Option<xkb::State>,
    locks: LockKeys,
    keyboard_surface: Option<wl_surface::WlSurface>,
    sync_done: u64,
    error: Option<PlatformError>,
    scroll: AxisFrame,
    scrolling: (bool, bool),
}
#[derive(Default)]
struct AxisFrame {
    x: f64,
    y: f64,
    v120_x: Option<i32>,
    v120_y: Option<i32>,
    discrete_x: i32,
    discrete_y: i32,
    stop_x: bool,
    stop_y: bool,
    source: Option<wl_pointer::AxisSource>,
    at: Option<MonoTime>,
    present: bool,
}

pub(super) fn now() -> MonoTime {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    MonoTime::from_nanos(
        (t.tv_sec.max(0) as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(t.tv_nsec.max(0) as u64),
    )
}
fn milliseconds(time: u32) -> MonoTime {
    // Wayland's 32-bit millisecond counter wraps every ~49 days. Hyprland uses CLOCK_MONOTONIC;
    // choose the nearest epoch of that same node clock, rather than using process-relative time.
    let current = now().as_nanos() / 1_000_000;
    let difference = time.wrapping_sub(current as u32) as i32 as i64;
    timestamp(if time == 0 {
        0
    } else {
        current
            .saturating_add_signed(difference)
            .saturating_mul(1_000_000)
    })
}
fn timestamp(nanos: u64) -> MonoTime {
    let current = now();
    if nanos == 0 || current.as_nanos().abs_diff(nanos) > 1_000_000_000 {
        current
    } else {
        MonoTime::from_nanos(nanos)
    }
}
fn connect(deadline: Instant, source: &Source) -> Result<UnixStream, PlatformError> {
    let path = source.socket_path()?;
    let fd = rustix::net::socket_with(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::STREAM,
        rustix::net::SocketFlags::NONBLOCK | rustix::net::SocketFlags::CLOEXEC,
        None,
    )
    .map_err(backend)?;
    let address = rustix::net::SocketAddrUnix::new(path).map_err(backend)?;
    loop {
        match rustix::net::connect(&fd, &address) {
            Ok(()) => return Ok(UnixStream::from(fd)),
            Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR)
                if Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(1))
            }
            Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => {
                return Err(PlatformError::Timeout);
            }
            Err(error) => return Err(backend(error)),
        }
    }
}
impl Client {
    pub(super) fn new(
        gate: Arc<IoGate>,
        abort: Arc<Abort>,
        delivery: mpsc::Sender<Delivery>,
        deadline: Instant,
        monitors: Option<Vec<Monitor>>,
        source: &Source,
    ) -> Result<Self, PlatformError> {
        let epoch = abort.epoch.load(Ordering::Acquire);
        let stream = connect(deadline, source)?;
        let socket = Arc::new(Socket {
            stream: stream.try_clone().map_err(backend)?,
            lost: AtomicBool::new(false),
        });
        // Shutdown covers every descriptor, including the delivery thread's clone, if any
        // subsequent registry, keymap, protocol binding or initial roundtrip fails.
        let socket_guard = SocketGuard(socket.clone());
        let (ready, rx) = mpsc::channel();
        delivery
            .send(Delivery::Connection {
                epoch,
                socket: SocketGuard(socket.clone()),
                ready,
            })
            .map_err(|_| backend("delivery unavailable"))?;
        let _ = abort.wake.send(&[1]);
        rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| PlatformError::Timeout)?;
        let conn = Connection::from_socket(stream).map_err(backend)?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let registry = conn.display().get_registry(&qh, ());
        let mut state = State {
            connection: conn.clone(),
            socket,
            gate: gate.clone(),
            abort: abort.clone(),
            epoch,
            advertised: Vec::new(),
            outputs: Vec::new(),
            monitors: Vec::new(),
            monitors_dirty: true,
            refresh_after: Instant::now(),
            lost: 0,
            causes: Vec::new(),
            ack_delay: Duration::ZERO,
            deferred: Vec::new(),
            strips: BTreeMap::new(),
            portals: BTreeMap::new(),
            next_strip: 0,
            entered: None,
            pressing: None,
            pointer_position: (0.0, 0.0),
            enter_serial: 0,
            buttons: BTreeSet::new(),
            keys: BTreeSet::new(),
            capture: None,
            gesture: drag::Gesture::default(),
            drag: drag::Poller::new(source.clone(), gate, abort, epoch)?,
            generation: 0,
            cursor: None,
            xkb: None,
            locks: LockKeys::default(),
            keyboard_surface: None,
            sync_done: 0,
            error: None,
            scroll: AxisFrame::default(),
            scrolling: (false, false),
        };
        conn.display().sync(&qh, 1);
        while state.sync_done < 1 {
            pump(&conn, &mut queue, &mut state, super::POLL)?;
            state.check_deadline(deadline)?;
        }
        macro_rules! bind {
            ($ty:ty, $min:expr, $max:expr) => {{
                let (name, _, version) = state
                    .advertised
                    .iter()
                    .find(|(_, interface, version)| {
                        interface == <$ty>::interface().name && *version >= $min
                    })
                    .ok_or(PlatformError::Unsupported(concat!(
                        stringify!($ty),
                        " missing"
                    )))?;
                registry.bind::<$ty, _, _>(*name, (*version).min($max), &qh, ())
            }};
        }
        let compositor = bind!(wl_compositor::WlCompositor, 4, 6);
        let shm = bind!(wl_shm::WlShm, 1, 1);
        let shell = bind!(ZwlrLayerShellV1, 3, 5);
        let seat = bind!(wl_seat::WlSeat, 5, 9);
        let pointer = seat.get_pointer(&qh, ());
        let keyboard = seat.get_keyboard(&qh, ());
        let cursor_manager = bind!(WpCursorShapeManagerV1, 1, 1);
        let cursor = cursor_manager.get_pointer(&pointer, &qh, ());
        cursor_manager.destroy();
        state.cursor = Some(cursor.clone());
        let globals = Globals {
            compositor,
            shm,
            shell,
            seat,
            pointer,
            _keyboard: keyboard,
            constraints: bind!(ZwpPointerConstraintsV1, 1, 1),
            relative: bind!(ZwpRelativePointerManagerV1, 1, 1),
            inhibit: bind!(ZwpKeyboardShortcutsInhibitManagerV1, 1, 1),
            virtual_pointer: bind!(ZwlrVirtualPointerManagerV1, 2, 2),
        };
        let mut client = Self {
            _socket: socket_guard,
            conn,
            queue,
            qh,
            state,
            globals,
            sync: 1,
            source: source.clone(),
        };
        client.roundtrip(deadline)?;
        if let Some(monitors) = monitors {
            client.state.monitors = monitors;
        } else {
            client.monitors(deadline)?;
        }
        Ok(client)
    }
    pub(super) fn monitor_cache(&self) -> Vec<Monitor> {
        self.state.monitors.clone()
    }
    /// Refresh a stale monitor cache from the idle path, also while a capture is active (WP-2.43d).
    /// The request is bounded at [`REFRESH_BOUND`]; after a failure the next try waits
    /// [`REFRESH_RETRY`], so a broken IPC socket can't keep the capture thread from dispatching.
    pub(super) fn refresh_monitors(&mut self) {
        if self.state.monitors_dirty && Instant::now() >= self.state.refresh_after {
            let started = Instant::now();
            if self.monitors(started + REFRESH_BOUND).is_err() {
                self.state.refresh_after = Instant::now() + REFRESH_RETRY;
            }
            tracing::trace!(elapsed = ?started.elapsed(), "monitor cache refreshed on the idle path");
        }
    }
    pub(super) fn inject_worker_error(&mut self) {
        self.state.error = Some(backend("injected worker dispatch failure"));
    }
    pub(super) fn locks(&self) -> LockKeys {
        self.state.locks
    }
    pub(super) fn pump(&mut self, timeout: Duration) -> Result<(), PlatformError> {
        self.state.check_gate();
        self.state.check_epoch()?;
        self.state.drag.configure(
            self.state
                .strips
                .values()
                .filter(|p| p.mapped)
                .map(|p| p.portal)
                .collect(),
            self.state.monitors.clone(),
            self.state.capture.is_some() || !self.state.gate.is_open(),
        );
        if let Some(error) = self.state.error.take() {
            return Err(error);
        }
        self.state.poll_drag(); // Process ready cancellation before any queued strip enter.
        pump(&self.conn, &mut self.queue, &mut self.state, timeout)?;
        self.state.release_deferred(false);
        self.state.check_epoch()?;
        self.state.poll_drag();
        if let Some(error) = self.state.error.take() {
            return Err(error);
        }
        // The ruled R3 nudge is confined to a live watch, never ordinary idle/capture traffic.
        if self.state.capture.is_none() && self.state.gate.is_open() {
            let _ = self.nudge_watch();
        }
        Ok(())
    }
    fn nudge_watch(&mut self) -> Result<(), PlatformError> {
        let portals: Vec<_> = self
            .state
            .strips
            .values()
            .filter(|p| p.mapped)
            .map(|p| p.portal)
            .collect();
        let Some(watch) = self.state.gesture.watch.as_mut() else {
            return Ok(());
        };
        if !watch.valid(
            Some(&watch.hit.sample),
            &portals,
            &self.state.monitors,
            Instant::now(),
        ) {
            self.state.gesture = drag::Gesture::default();
            return Ok(());
        }
        if !watch.nudge(Instant::now()) {
            return Ok(());
        }
        let ipc = self.source.ipc(Duration::from_millis(8))?;
        self.state.check_epoch()?;
        if self.state.gate.is_open() {
            // Atomic public Lua query keeps fractional coordinates and cannot overwrite motion
            // that occurred after a separate IPC read (j/cursorpos also floors its coordinates).
            ipc.eval("local p = hl.get_cursor_pos() if p then hl.dispatch(hl.dsp.cursor.move({x = p.x, y = p.y})) end")?;
        }
        Ok(())
    }
    /// Nested tests only: hold configure and sync acknowledgements back for `delay` (zero: off),
    /// so a test can see that `set_portals` is still waiting for them. Turning it off releases
    /// whatever is still held.
    pub(super) fn set_ack_delay(&mut self, delay: Duration) {
        self.state.ack_delay = delay;
        if delay.is_zero() {
            self.state.release_deferred(true);
        }
    }
    /// Why this connection ended captures, oldest first (nested tests).
    pub(super) fn end_causes(&self) -> Vec<String> {
        self.state.causes.iter().map(|c| format!("{c:?}")).collect()
    }
    /// Nested tests: run the backend's handling of the removal of the output named `name` without
    /// removing it from the compositor. The compositor's own reaction (Hyprland 0.56.2 ends any
    /// capture when any output goes) is then out of the picture, and what is left is the
    /// backend's decision, exactly as `GlobalRemove` makes it.
    pub(super) fn simulate_output_removal(&mut self, name: &str) -> Result<(), PlatformError> {
        let proxy = self
            .state
            .outputs
            .iter()
            .find(|o| o.name == name)
            .map(|o| o.proxy.clone())
            .ok_or(PlatformError::NotFound)?;
        self.state.output_removed(&proxy);
        let _ = self.conn.flush();
        Ok(())
    }
    fn wait(
        &mut self,
        deadline: Instant,
        predicate: impl Fn(&State) -> bool,
    ) -> Result<(), PlatformError> {
        loop {
            self.state.check_deadline(deadline)?;
            if predicate(&self.state) {
                return Ok(());
            }
            self.pump(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(super::POLL),
            )?;
        }
    }
    fn roundtrip(&mut self, deadline: Instant) -> Result<(), PlatformError> {
        self.sync += 1;
        let token = self.sync;
        self.conn.display().sync(&self.qh, token);
        self.wait(deadline, |s| s.sync_done >= token)
    }
    fn barrier(&self, deadline: Instant) -> Result<(), PlatformError> {
        let token = self
            .state
            .abort
            .next_barrier
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        self.state
            .abort
            .send_packet(self.state.epoch, &events::barrier(token), Some(deadline))?;
        while self.state.abort.barrier_reached.load(Ordering::Acquire) < token {
            self.state.check_deadline(deadline)?;
            std::thread::sleep(Duration::from_micros(100));
        }
        Ok(())
    }
    /// Refresh the monitor cache: read the list, and publish it only if that was still in time.
    fn monitors(&mut self, deadline: Instant) -> Result<(), PlatformError> {
        self.refresh_cache(deadline, Duration::ZERO)
    }
    /// [`Self::monitors`], sitting on the list that was read for `stall` before publishing it
    /// (nested tests: a refresh that parses late).
    fn refresh_cache(&mut self, deadline: Instant, stall: Duration) -> Result<(), PlatformError> {
        let list = self.read_monitors(deadline)?;
        if !stall.is_zero() {
            std::thread::sleep(stall);
        }
        self.state.check_epoch()?;
        publish_monitors(
            &mut self.state.monitors,
            &mut self.state.monitors_dirty,
            list,
            deadline,
            Instant::now(),
        )
    }
    /// Read the monitor list from Hyprland's IPC. Touches no state.
    fn read_monitors(&self, deadline: Instant) -> Result<Vec<Monitor>, PlatformError> {
        self.state.check_deadline(deadline)?;
        // Each IPC call has its own short-lived, bounded connection. No product hyprctl calls.
        // The compositor is the one this connection was made to (`Source`).
        let ipc = self.source.ipc(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(8)),
        )?;
        let ids = ipc.monitor_ids()?;
        let json = ipc.json("monitors")?;
        let list = json
            .as_array()
            .ok_or_else(|| backend("invalid monitor list"))?;
        list.iter()
            .map(|m| {
                let name = m["name"]
                    .as_str()
                    .ok_or_else(|| backend("monitor missing name"))?;
                let id = ids
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, id)| DisplayId(*id))
                    .ok_or(PlatformError::NotFound)?;
                let dimension = |key: &str| {
                    m[key]
                        .as_u64()
                        .and_then(|x| u32::try_from(x).ok())
                        .filter(|v| *v > 0)
                        .ok_or_else(|| backend("invalid monitor size"))
                };
                let (mut width, mut height) = (dimension("width")?, dimension("height")?);
                if m["transform"].as_u64().unwrap_or(0) % 2 == 1 {
                    std::mem::swap(&mut width, &mut height);
                }
                let scale = m["scale"]
                    .as_f64()
                    .filter(|s| s.is_finite() && *s > 0.0)
                    .ok_or_else(|| backend("invalid monitor scale"))?;
                Ok(Monitor {
                    id,
                    name: name.into(),
                    width,
                    height,
                    scale,
                    origin: [
                        m["x"]
                            .as_f64()
                            .filter(|x| x.is_finite())
                            .ok_or_else(|| backend("invalid monitor origin"))?,
                        m["y"]
                            .as_f64()
                            .filter(|y| y.is_finite())
                            .ok_or_else(|| backend("invalid monitor origin"))?,
                    ],
                })
            })
            .collect::<Result<_, PlatformError>>()
    }
    /// A rejection claims that the previous set and any capture are intact. That holds only if the
    /// backend was not aborted and nothing was lost while the command dispatched events: no started
    /// capture ended and no mapped strip was destroyed or closed (`State::lost` is still `since`).
    /// Otherwise the error is returned unmarked: whether anything survived is unknown.
    fn settle(&self, error: PlatformError, since: u64) -> PlatformError {
        if self.state.check_epoch().is_ok() && self.state.lost == since {
            error
        } else {
            unmark(error)
        }
    }
    /// A failure before anything live was touched: a rejection, subject to [`Self::settle`].
    fn reject(&self, error: PlatformError, since: u64) -> PlatformError {
        self.settle(rejected(error), since)
    }
    /// [`Self::reject`] for an error from waiting on the compositor: only a missed deadline or a
    /// strip the compositor closed is a rejection. A failed connection is not: it ends the capture.
    fn reject_if_waiting(&self, error: PlatformError, since: u64) -> PlatformError {
        match error {
            PlatformError::Timeout | PlatformError::NotFound => self.reject(error, since),
            other => self.settle(other, since),
        }
    }
    /// Step 1 of [`Self::set_portals`]: bring the monitor cache up to date (if it is stale) and make
    /// sure every output a requested portal names has been announced on this connection.
    fn refresh_topology(
        &mut self,
        portals: &[CapturePortal],
        deadline: Instant,
        work_deadline: Instant,
        refresh: Refresh,
        since: u64,
    ) -> Result<(), PlatformError> {
        let stall = match refresh {
            Refresh::Forced { stall, .. } => {
                self.state.monitors_dirty = true;
                stall
            }
            _ => Duration::ZERO,
        };
        if self.state.monitors_dirty && !matches!(refresh, Refresh::Done) {
            let bound = match refresh {
                Refresh::Forced { bound, .. } => bound,
                _ => REFRESH_BOUND,
            };
            let until = (Instant::now() + bound)
                .min(deadline.checked_sub(REFRESH_RESERVE).unwrap_or(deadline));
            let started = Instant::now();
            let refreshed = self.refresh_cache(until, stall);
            tracing::debug!(
                elapsed = ?started.elapsed(),
                ok = refreshed.is_ok(),
                "monitor refresh for set_portals"
            );
            if let Err(error) = refreshed
                && (!matches!(refresh, Refresh::BestEffort) || self.state.check_epoch().is_err())
            {
                return Err(self.reject(error, since));
            }
        }
        // An output created since the connection is announced on the Wayland connection too; the
        // IPC list may be ahead of what this thread has dispatched.
        if portals.iter().any(|p| {
            self.state
                .monitors
                .iter()
                .find(|m| m.id == p.display)
                .is_some_and(|m| !self.state.outputs.iter().any(|o| o.name == m.name))
        }) {
            self.roundtrip(work_deadline)
                .map_err(|e| self.reject_if_waiting(e, since))?;
        }
        Ok(())
    }
    /// Whether `portal` can be placed on the outputs as they are now.
    fn placeable(&self, portal: &CapturePortal) -> bool {
        let Some(m) = self.state.monitors.iter().find(|m| m.id == portal.display) else {
            return false;
        };
        let extent = if matches!(portal.edge, Edge::Left | Edge::Right) {
            m.height
        } else {
            m.width
        };
        portal.to <= f64::from(extent) && self.state.outputs.iter().any(|o| o.name == m.name)
    }
    /// Rebuild the previous portal set on a new connection. Portals whose output has gone since
    /// (it was removed, then the connection was lost) are left out instead of failing the whole
    /// set, so that one removed output can't keep every other strip from coming back. Returns the
    /// portals that are installed now.
    pub(super) fn rebuild_portals(
        &mut self,
        portals: &[CapturePortal],
        deadline: Instant,
    ) -> Result<Vec<CapturePortal>, PlatformError> {
        let since = self.state.lost;
        let Some(work_deadline) = deadline
            .checked_sub(REPLY_RESERVE)
            .filter(|d| *d > Instant::now())
        else {
            return Err(backend("no time left to rebuild the strips"));
        };
        self.refresh_topology(portals, deadline, work_deadline, Refresh::BestEffort, since)?;
        let placeable: Vec<CapturePortal> = portals
            .iter()
            .filter(|p| self.placeable(p))
            .copied()
            .collect();
        if placeable.len() != portals.len() {
            tracing::debug!(
                dropped = portals.len() - placeable.len(),
                "rebuilding capture strips without the outputs that are gone"
            );
        }
        self.set_portals(&placeable, deadline, Refresh::Done)?;
        Ok(placeable)
    }
    /// Replace the strips (WP-2.43d; `capture.rs` module docs). `Ok` only once every requested
    /// strip is mapped with its buffer attached; failure leaves the previous set installed.
    ///
    /// 1. A stale monitor cache is refreshed first, also while a capture is active. If the refresh
    ///    misses its bound the cache and the previous set stay in force (a rejection).
    /// 2. The *whole* replacement is validated before any surface request is issued.
    /// 3. A portal equal to an existing strip's `(id, display, edge, from, to)` keeps that strip
    ///    (same layer surface, tag, edge state, any capture on it) while its display still
    ///    resolves to the same monitor at the same scale. An identical set is a no-op.
    /// 4. Strips for the other portals are created and prepared (configured, buffer allocated,
    ///    attached, committed, one roundtrip) before anything live changes.
    /// 5. Only then are a capture whose strip went away ended `Lost` and the absent strips
    ///    destroyed.
    ///
    /// Everything up to step 5 is reported as a rejection ([`rejected`]), but only while the
    /// guarantee behind a rejection still holds ([`Self::settle`]): an event dispatched meanwhile
    /// may have ended the capture or destroyed an installed strip.
    pub(super) fn set_portals(
        &mut self,
        portals: &[CapturePortal],
        deadline: Instant,
        refresh: Refresh,
    ) -> Result<(), PlatformError> {
        let since = self.state.lost;
        self.state.check_deadline(deadline)?;
        // The worker must answer before the caller's deadline, or the caller aborts everything. A
        // reserve that is already gone is not made up from the caller's time: reject at once.
        let Some(work_deadline) = deadline
            .checked_sub(REPLY_RESERVE)
            .filter(|d| *d > Instant::now())
        else {
            return Err(self.reject(backend("no time left to prepare the strips"), since));
        };
        // 1. Topology.
        self.refresh_topology(portals, deadline, work_deadline, refresh, since)?;
        // 2. Validate the whole replacement; 3. decide which strips stay.
        let mut ids = BTreeSet::new();
        let mut plans = Vec::with_capacity(portals.len());
        for p in portals {
            if !ids.insert(p.id)
                || !p.from.is_finite()
                || !p.to.is_finite()
                || p.from < 0.0
                || p.from >= p.to
            {
                return Err(self.reject(backend("invalid capture portal"), since));
            }
            let Some(m) = self.state.monitors.iter().find(|m| m.id == p.display) else {
                return Err(self.reject(backend("capture portal on an unknown display"), since));
            };
            let vertical = matches!(p.edge, Edge::Left | Edge::Right);
            if p.to > f64::from(if vertical { m.height } else { m.width }) {
                return Err(self.reject(backend("portal outside output"), since));
            }
            let Some(output) = self
                .state
                .outputs
                .iter()
                .find(|o| o.name == m.name)
                .map(|o| o.proxy.clone())
            else {
                return Err(self.reject(backend("no wl_output for the portal's display"), since));
            };
            let start = (p.from / m.scale).floor();
            let length = (p.to / m.scale).ceil() - start;
            if length > f64::from(i32::MAX / 4) || start > f64::from(i32::MAX) {
                return Err(self.reject(backend("portal too large"), since));
            }
            let monitor_size = (m.width, m.height);
            let keep = self.state.portals.get(&p.id).copied().filter(|tag| {
                let capturing = self.state.capture.as_ref().is_some_and(|c| c.strip == *tag);
                self.state.strips.get(tag).is_some_and(|s| {
                    s.portal == *p
                        && s.mapped
                        && !s.closed
                        && s.buffer.is_some()
                        && s.output == output
                        && s.monitor == m.name
                        && s.scale == m.scale
                        && (capturing || s.monitor_size == monitor_size)
                })
            });
            plans.push(Plan {
                portal: *p,
                output,
                monitor: m.name.clone(),
                monitor_size,
                scale: m.scale,
                offset: start,
                length: length as u32,
                keep,
            });
        }
        if plans.iter().all(|p| p.keep.is_some()) && self.state.portals.len() == plans.len() {
            return Ok(()); // Identical set: no requests, no roundtrip.
        }
        // 4. Create and prepare the new strips.
        let mut candidates = Vec::new();
        let mut tags = Vec::with_capacity(plans.len());
        for plan in plans {
            if let Some(tag) = plan.keep {
                tags.push((plan.portal.id, tag));
                continue;
            }
            let Plan {
                portal,
                output,
                monitor,
                monitor_size,
                scale,
                offset,
                length,
                ..
            } = plan;
            self.state.next_strip += 1;
            let tag = self.state.next_strip;
            let surface = self.globals.compositor.create_surface(&self.qh, tag);
            let layer = self.globals.shell.get_layer_surface(
                &surface,
                Some(&output),
                Layer::Overlay,
                "crosspane-capture".into(),
                &self.qh,
                tag,
            );
            let (anchor, width, height, top, left) = match portal.edge {
                Edge::Left => (Anchor::Left | Anchor::Top, 1, length, offset as i32, 0),
                Edge::Right => (Anchor::Right | Anchor::Top, 1, length, offset as i32, 0),
                Edge::Top => (Anchor::Top | Anchor::Left, length, 1, 0, offset as i32),
                Edge::Bottom => (Anchor::Bottom | Anchor::Left, length, 1, 0, offset as i32),
            };
            layer.set_size(width, height);
            layer.set_anchor(anchor);
            layer.set_margin(top, 0, 0, left);
            // -1 reserves no space and places strips on the physical edge, including across bars.
            layer.set_exclusive_zone(-1);
            layer.set_keyboard_interactivity(KeyboardInteractivity::None);
            surface.commit();
            self.state.strips.insert(
                tag,
                Strip {
                    portal,
                    output,
                    monitor,
                    monitor_size,
                    scale,
                    offset,
                    surface,
                    layer,
                    size: None,
                    buffer: None,
                    mapped: false,
                    closed: false,
                },
            );
            candidates.push(tag);
            tags.push((portal.id, tag));
        }
        let prepare = (|| {
            if candidates.is_empty() {
                return Ok(()); // Only removals: nothing to wait for.
            }
            self.wait(work_deadline, |s| {
                candidates.iter().all(|tag| {
                    s.strips
                        .get(tag)
                        .is_none_or(|p| p.size.is_some() || p.closed)
                })
            })?;
            // Allocate every buffer before mapping anything. Failed replacements leave old strips.
            for tag in &candidates {
                let p = self
                    .state
                    .strips
                    .get_mut(tag)
                    .ok_or(PlatformError::NotFound)?;
                if p.closed {
                    return Err(PlatformError::NotFound);
                }
                let (w, h) = p.size.ok_or(PlatformError::NotFound)?;
                p.buffer = Some(buffer(&self.globals.shm, &self.qh, w, h).map_err(rejected)?);
            }
            self.state.check_deadline(work_deadline)?;
            for tag in &candidates {
                if let Some(p) = self.state.strips.get(tag) {
                    p.surface.attach(p.buffer.as_ref(), 0, 0);
                    p.surface.commit();
                }
            }
            self.roundtrip(work_deadline)
        })();
        // The pump inside the preparation may have closed a strip (its output went away, or the
        // compositor refused it): every strip of the new set must still be alive and mapped.
        let alive = prepare.and_then(|()| {
            let healthy = tags.iter().all(|(_, tag)| {
                self.state
                    .strips
                    .get(tag)
                    .is_some_and(|s| !s.closed && (s.mapped || candidates.contains(tag)))
            });
            if healthy {
                Ok(())
            } else {
                Err(PlatformError::NotFound)
            }
        });
        if let Err(error) = alive {
            for tag in candidates {
                if let Some(strip) = self.state.strips.remove(&tag) {
                    strip.destroy();
                }
            }
            let _ = self.conn.flush();
            return Err(self.reject_if_waiting(error, since));
        }
        // 5. Apply. From here the previous set is being replaced.
        let new_tags: BTreeSet<u64> = tags.iter().map(|(_, tag)| *tag).collect();
        if let Some(c) = self.state.capture.as_ref()
            && !new_tags.contains(&c.strip)
        {
            self.state.finish(EndReason::Lost, Cause::StripReplaced);
        }
        if let Some(tag) = self.state.entered.filter(|tag| !new_tags.contains(tag)) {
            if let Some(p) = self.state.strips.get(&tag) {
                let portal = p.portal.id;
                self.state.pressing = None;
                self.state
                    .emit(CaptureEvent::EdgeReleased { portal, at: now() });
            }
            self.state.entered = None;
        }
        let stale: Vec<u64> = self
            .state
            .strips
            .keys()
            .copied()
            .filter(|tag| !new_tags.contains(tag))
            .collect();
        for tag in stale {
            if let Some(strip) = self.state.strips.remove(&tag) {
                strip.destroy();
            }
        }
        self.state.portals = tags.into_iter().collect();
        for tag in &candidates {
            if let Some(strip) = self.state.strips.get_mut(tag) {
                strip.mapped = true;
            }
        }
        // A kept strip keeps its entered/pressing state. The pointer may have entered a new strip
        // while it was being prepared, before it counted as mapped: report that press now.
        if let Some(tag) = self.state.entered.filter(|tag| candidates.contains(tag)) {
            let (x, y) = self.state.pointer_position;
            self.state.pressed(tag, x, y, now());
        }
        if self
            .state
            .pressing
            .is_some_and(|id| self.state.portals.get(&id).copied() != self.state.entered)
        {
            self.state.pressing = None;
        }
        self.conn.flush().map_err(backend)?;
        Ok(())
    }
    pub(super) fn refuse_drag(&mut self, portal: PortalId, button: MouseButton) {
        self.state.poll_drag();
        if self.state.gate.is_open()
            && self.state.capture.is_none()
            && self.state.portals.contains_key(&portal)
        {
            self.state.gesture.refuse(portal, button, Instant::now());
        }
    }
    pub(super) fn begin(
        &mut self,
        id: CaptureId,
        portal: PortalId,
        deadline: Instant,
    ) -> Result<CaptureStart, PlatformError> {
        self.pump(Duration::ZERO)?;
        self.state.drag.pause(deadline)?;
        self.state.gesture = drag::Gesture::default();
        if !self.state.gate.is_open() {
            return Err(PlatformError::Locked);
        }
        if !self.state.buttons.is_empty() {
            return Err(PlatformError::PointerButtonHeld);
        }
        if self.state.capture.is_some() {
            return Err(backend("capture already active"));
        }
        let tag = *self
            .state
            .portals
            .get(&portal)
            .ok_or(PlatformError::NotFound)?;
        if self.state.entered != Some(tag) || self.state.pressing != Some(portal) {
            return Err(PlatformError::NotFound);
        }
        let strip = self.state.strips.get(&tag).ok_or(PlatformError::NotFound)?;
        self.state.generation = self.state.abort.next_capture.fetch_add(1, Ordering::AcqRel) + 1;
        let generation = self.state.generation;
        strip
            .layer
            .set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        strip.surface.commit();
        let lock = self.globals.constraints.lock_pointer(
            &strip.surface,
            &self.globals.pointer,
            None,
            Lifetime::Oneshot,
            &self.qh,
            generation,
        );
        self.state.capture = Some(Capture {
            id,
            generation,
            strip: tag,
            lock: Some(lock),
            inhibitor: None,
            relative: None,
            locked: false,
            inhibited: false,
            keyboard: false,
            started: false,
            held: Vec::new(),
            pending: Vec::new(),
        });
        let activation_deadline = deadline
            .checked_sub(Duration::from_millis(8))
            .unwrap_or(deadline);
        let activate = (|| {
            self.wait_capture(activation_deadline, |c| c.locked)?;
            let relative = self.globals.relative.get_relative_pointer(
                &self.globals.pointer,
                &self.qh,
                generation,
            );
            let surface = self
                .state
                .strips
                .get(&tag)
                .ok_or(PlatformError::NotFound)?
                .surface
                .clone();
            let inhibitor = self.globals.inhibit.inhibit_shortcuts(
                &surface,
                &self.globals.seat,
                &self.qh,
                generation,
            );
            if let Some(c) = self.state.capture.as_mut() {
                c.relative = Some(relative);
                c.inhibitor = Some(inhibitor);
            }
            self.wait_capture(activation_deadline, |c| {
                c.locked && c.inhibited && c.keyboard
            })?;
            self.globals
                .pointer
                .set_cursor(self.state.enter_serial, None, 0, 0);
            self.roundtrip(activation_deadline)?; // Includes cursor hiding and the keyboard's modifier snapshot.
            self.state.check_gate();
            self.state.check_deadline(deadline)?;
            let c = self.state.capture.as_mut().ok_or(PlatformError::Locked)?;
            c.started = true;
            let held_keys = c.held.clone();
            let pending = std::mem::take(&mut c.pending);
            self.state.emit(CaptureEvent::Started { id });
            for event in pending {
                self.state.emit(event);
            }
            self.barrier(deadline)?;
            Ok(CaptureStart {
                held_keys,
                lock_keys: self.state.locks,
            })
        })();
        if activate.is_err() {
            self.state.finish(EndReason::Lost, Cause::ActivationFailed);
            // Destroy partial objects, invalidate their generation, and prove the requests were
            // processed in the rollback reserve. If the compositor cannot respond, independent
            // connection shutdown also cancels any buffered requests that could activate later.
            if self.roundtrip(deadline).is_err() {
                self.state.abort.abort();
            }
        }
        activate
    }
    fn wait_capture(
        &mut self,
        deadline: Instant,
        predicate: impl Fn(&Capture) -> bool,
    ) -> Result<(), PlatformError> {
        loop {
            self.state.check_deadline(deadline)?;
            let c = self.state.capture.as_ref().ok_or_else(|| {
                if self.state.gate.is_open() {
                    backend("capture activation lost")
                } else {
                    PlatformError::Locked
                }
            })?;
            if predicate(c) {
                return Ok(());
            }
            self.pump(super::POLL)?;
        }
    }
    pub(super) fn end(
        &mut self,
        warp: Option<(DisplayId, PointDevice)>,
        deadline: Instant,
    ) -> Result<(), PlatformError> {
        let ended = self.state.release();
        let result = (|| {
            if let Some((display, point)) = warp.filter(|_| self.state.gate.is_open()) {
                if !point.x.is_finite() || !point.y.is_finite() {
                    return Err(backend("invalid warp point"));
                }
                let monitor = self
                    .state
                    .monitors
                    .iter()
                    .find(|m| m.id == display)
                    .ok_or(PlatformError::NotFound)?;
                let output = self
                    .state
                    .outputs
                    .iter()
                    .find(|o| o.name == monitor.name)
                    .ok_or(PlatformError::NotFound)?;
                // WlOutput already delivers mode/transform changes on this queue. Avoid two
                // synchronous monitor IPC requests on the time-bounded unlock/warp path.
                let (mut width, mut height) =
                    output.mode.unwrap_or((monitor.width, monitor.height));
                if output.mode.is_some() && output.rotated {
                    std::mem::swap(&mut width, &mut height);
                }
                let pointer = self
                    .globals
                    .virtual_pointer
                    .create_virtual_pointer_with_output(
                        Some(&self.globals.seat),
                        Some(&output.proxy),
                        &self.qh,
                        (),
                    );
                // The lock must be destroyed before virtual-pointer warping. The lead accepts this
                // brief unlock-to-warp window; the roundtrip finishes the warp before Ended.
                if self.state.gate.is_open() {
                    pointer.motion_absolute(
                        (now().as_nanos() / 1_000_000) as u32,
                        point.x.round().clamp(0.0, f64::from(width - 1)) as u32,
                        point.y.round().clamp(0.0, f64::from(height - 1)) as u32,
                        width,
                        height,
                    );
                    pointer.frame();
                }
                pointer.destroy();
            }
            self.roundtrip(deadline)
        })();
        if result.is_err() {
            // Errors (including an invalid warp target) still return input. A failed sync uses
            // independent shutdown instead of leaving destroy requests buffered indefinitely.
            if self.roundtrip(deadline).is_err() {
                self.state.abort.abort();
            }
        }
        if let Some((id, generation)) = ended {
            self.state.emit_generation(
                generation,
                CaptureEvent::Ended {
                    id,
                    reason: EndReason::Requested,
                },
            );
        }
        let _ = self.conn.flush();
        result?;
        self.barrier(deadline)
    }
    pub(super) fn disconnected(&mut self) {
        self.state.finish(
            if self.state.epoch == self.state.abort.epoch.load(Ordering::Acquire) {
                EndReason::Lost
            } else {
                EndReason::Aborted
            },
            Cause::Disconnected,
        );
        // Even a healthy socket must close on a dispatch/keymap error: merely dropping the
        // Connection leaves delivery's descriptor alive and the compositor's grabs in force.
        let _ = self.conn.flush();
        self.state.socket.lost();
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        self.disconnected();
    }
}
/// What the removal of an output does to the strips and the capture (§3.6.3), decided on the
/// strips' outputs alone so that it can be tested without a compositor.
#[derive(Debug, PartialEq, Eq)]
struct RemovalEffect {
    /// The backend ends the capture: its own strip is on the removed output.
    ends_capture: bool,
    /// The strips (by tag) on the removed output, destroyed in every case.
    destroys: Vec<u64>,
}
fn removal_effect<O: PartialEq>(
    capture_strip: Option<u64>,
    strips: &[(u64, O)],
    removed: &O,
) -> RemovalEffect {
    RemovalEffect {
        ends_capture: capture_strip
            .is_some_and(|tag| strips.iter().any(|(t, out)| *t == tag && out == removed)),
        destroys: strips
            .iter()
            .filter(|(_, out)| out == removed)
            .map(|(tag, _)| *tag)
            .collect(),
    }
}
/// Publish a freshly read monitor list (§3.6.2), but only if that is still in time: a refresh that
/// missed its deadline leaves the cache *and* the stale flag exactly as they were, so the idle
/// path retries and `set_portals` can report the rejection with the previous state in force.
fn publish_monitors(
    cache: &mut Vec<Monitor>,
    dirty: &mut bool,
    list: Vec<Monitor>,
    deadline: Instant,
    now: Instant,
) -> Result<(), PlatformError> {
    if now >= deadline {
        return Err(PlatformError::Timeout);
    }
    *cache = list;
    *dirty = false;
    Ok(())
}
fn buffer(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<State>,
    width: u32,
    height: u32,
) -> Result<wl_buffer::WlBuffer, PlatformError> {
    let bytes = width
        .checked_mul(height)
        .and_then(|n| n.checked_mul(4))
        .and_then(|n| i32::try_from(n).ok())
        .filter(|n| *n > 0)
        .ok_or_else(|| backend("invalid strip buffer size"))?;
    let file = File::from(
        rustix::fs::memfd_create("crosspane-edge", rustix::fs::MemfdFlags::CLOEXEC)
            .map_err(backend)?,
    );
    file.set_len(bytes as u64).map_err(backend)?; // Zero-filled transparent ARGB; no mmap/unsafe.
    let pool = shm.create_pool(file.as_fd(), bytes, qh, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        (width * 4) as i32,
        wl_shm::Format::Argb8888,
        qh,
        (),
    );
    pool.destroy();
    Ok(buffer)
}
fn pump(
    conn: &Connection,
    queue: &mut EventQueue<State>,
    state: &mut State,
    timeout: Duration,
) -> Result<(), PlatformError> {
    queue.dispatch_pending(state).map_err(backend)?;
    match conn.flush() {
        Ok(()) => (),
        Err(wayland_client::backend::WaylandError::Io(e))
            if e.kind() == std::io::ErrorKind::WouldBlock => {}
        Err(e) => return Err(backend(e)),
    }
    if let Some(guard) = conn.prepare_read() {
        let mut fd = [PollFd::new(
            conn,
            PollFlags::IN | PollFlags::ERR | PollFlags::HUP,
        )];
        let time = Timespec::try_from(timeout).map_err(backend)?;
        let readable = match poll(&mut fd, Some(&time)) {
            Ok(count) => count > 0,
            Err(rustix::io::Errno::INTR) => false, // Drop read guard and retry on the next pump.
            Err(error) => return Err(backend(error)),
        };
        if readable {
            match guard.read() {
                Ok(_) => (),
                Err(wayland_client::backend::WaylandError::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(backend(e)),
            }
        }
    }
    queue.dispatch_pending(state).map_err(backend)?;
    Ok(())
}
impl State {
    fn poll_drag(&mut self) {
        if self.capture.is_some() || !self.gate.is_open() || self.check_epoch().is_err() {
            return;
        }
        let samples = self.drag.take();
        if samples.is_empty() {
            return;
        }
        let portals: Vec<_> = self
            .strips
            .values()
            .filter(|p| p.mapped)
            .map(|p| p.portal)
            .collect();
        for sample in samples {
            let (released, hit) =
                self.gesture
                    .observe(sample, &portals, &self.monitors, Instant::now());
            if let Some(portal) = released {
                self.emit(CaptureEvent::EdgeReleased { portal, at: now() });
            }
            if let Some(hit) = hit {
                self.emit(CaptureEvent::DragAtEdge {
                    portal: hit.portal,
                    position: hit.position,
                    window: hit.sample.window,
                    grab: hit.grab,
                    at: hit.sample.at,
                });
            }
        }
    }
    fn check_epoch(&self) -> Result<(), PlatformError> {
        if self.epoch != self.abort.epoch.load(Ordering::Acquire) {
            Err(backend("capture cancelled"))
        } else {
            Ok(())
        }
    }
    fn check_deadline(&self, deadline: Instant) -> Result<(), PlatformError> {
        self.check_epoch()?;
        if Instant::now() >= deadline {
            Err(PlatformError::Timeout)
        } else {
            Ok(())
        }
    }
    fn check_gate(&mut self) {
        if !self.gate.is_open() {
            self.gesture = drag::Gesture::default();
        }
        if !self.gate.is_open() && self.capture.is_some() {
            self.finish(EndReason::Lost, Cause::GateClosed);
        }
    }
    fn emit(&mut self, event: CaptureEvent) {
        self.emit_generation(self.generation, event);
    }
    fn emit_generation(&mut self, generation: u64, event: CaptureEvent) {
        if self.error.is_some() {
            return;
        }
        if let Some(packet) = events::encode(self.epoch, generation, &event) {
            // Bound backpressure even when the consumer is stuck inside EventSink::send.
            // Release before shutdown, without recursively sending into the stalled queue.
            if let Err(error) = self.abort.send_packet(
                self.epoch,
                &packet,
                Some(Instant::now() + Duration::from_millis(20)),
            ) && self.check_epoch().is_ok()
            {
                self.release();
                let _ = self.connection.flush();
                self.socket.lost();
                self.error = Some(error);
                // Delivery synthesizes Lost after draining committed packets when it resumes.
            }
        }
    }
    fn input(&mut self, event: CaptureEvent) {
        self.check_gate();
        if self.check_epoch().is_err() {
            return;
        }
        if let Some(c) = self.capture.as_mut() {
            if c.started {
                self.emit(event);
            } else {
                c.pending.push(event);
            }
        }
    }
    fn release(&mut self) -> Option<(CaptureId, u64)> {
        let c = self.capture.take()?;
        // Taking the capture invalidates all queued proxy events before issuing destroy requests.
        if let Some(lock) = c.lock {
            lock.destroy();
        }
        if let Some(inhibitor) = c.inhibitor {
            inhibitor.destroy();
        }
        if let Some(relative) = c.relative {
            relative.destroy();
        }
        if let Some(strip) = self.strips.get(&c.strip) {
            strip
                .layer
                .set_keyboard_interactivity(KeyboardInteractivity::None);
            strip.surface.commit();
        }
        if let Some(cursor) = &self.cursor {
            cursor.set_shape(self.enter_serial, Shape::Default);
        }
        self.scroll = AxisFrame::default();
        self.scrolling = (false, false);
        if c.started {
            self.lost += 1;
        }
        c.started.then_some((c.id, c.generation))
    }
    /// End the capture, if one exists, with `Ended { reason }`, and log why (`cause`).
    fn finish(&mut self, reason: EndReason, cause: Cause) {
        if self.capture.is_some() {
            if self.causes.len() == CAUSES_KEPT {
                self.causes.remove(0);
            }
            self.causes.push(cause);
        }
        if let Some((id, generation)) = self.release() {
            self.emit_generation(generation, CaptureEvent::Ended { id, reason });
        }
    }
    /// Make held-back acknowledgements that are due (all of them with `all`) take effect (nested
    /// tests only; `deferred` is empty otherwise).
    fn release_deferred(&mut self, all: bool) {
        if self.deferred.is_empty() {
            return;
        }
        let now = Instant::now();
        let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.deferred)
            .into_iter()
            .partition(|(at, _)| all || *at <= now);
        self.deferred = later;
        for (_, ack) in due {
            match ack {
                Deferred::Size(tag, size) => {
                    if let Some(strip) = self.strips.get_mut(&tag) {
                        strip.size = Some(size);
                    }
                }
                Deferred::Sync(token) => self.sync_done = self.sync_done.max(token),
            }
        }
    }
    /// A `wl_output` global went away (WP-2.43d, §3.6.3). The capture ends `Lost` only if its own
    /// strip is on that output (the backend's decision, [`removal_effect`]); the strips on it are
    /// destroyed in every case.
    ///
    /// **Hyprland 0.56.2 ends an active capture on the removal of *any* output** by itself: it
    /// relocates the cursor and drops the pointer lock, and `Unlocked` ends the capture `Lost`
    /// (cause [`Cause::Unlocked`], never [`Cause::OutputRemoved`]). That is an OS fact; the backend
    /// reports `Lost`, never reacquires the lock to hide it, and must not end a capture for an
    /// unrelated output's removal on its own.
    fn output_removed(&mut self, output: &wl_output::WlOutput) {
        let strips: Vec<(u64, wl_output::WlOutput)> = self
            .strips
            .iter()
            .map(|(tag, strip)| (*tag, strip.output.clone()))
            .collect();
        let effect = removal_effect(self.capture.as_ref().map(|c| c.strip), &strips, output);
        if effect.ends_capture {
            self.finish(EndReason::Lost, Cause::OutputRemoved);
        }
        for tag in effect.destroys {
            if self.entered == Some(tag) {
                if let Some(strip) = self.strips.get(&tag).filter(|strip| strip.mapped) {
                    let portal = strip.portal.id;
                    self.pressing = None;
                    self.emit(CaptureEvent::EdgeReleased { portal, at: now() });
                }
                self.entered = None;
            }
            if let Some(strip) = self.strips.remove(&tag) {
                if strip.mapped {
                    self.lost += 1;
                }
                strip.destroy();
            }
            self.portals.retain(|_, t| *t != tag);
        }
    }
    fn pressed(&mut self, tag: u64, x: f64, y: f64, at: MonoTime) {
        self.check_gate();
        if self.capture.is_some() || !self.gate.is_open() || self.check_epoch().is_err() {
            return;
        }
        let Some(strip) = self.strips.get(&tag).filter(|p| p.mapped) else {
            return;
        };
        let along = if matches!(strip.portal.edge, Edge::Left | Edge::Right) {
            y
        } else {
            x
        };
        let position = ((strip.offset + along) * strip.scale - strip.portal.from)
            / (strip.portal.to - strip.portal.from);
        let portal = strip.portal.id;
        // Layer-shell size and margins are integral logical pixels, so round the *surface*
        // outwards, but honour the exact device-pixel stretch when detecting an edge press.
        if !(0.0..=1.0).contains(&position) {
            if self.pressing == Some(portal) {
                self.pressing = None;
                self.emit(CaptureEvent::EdgeReleased { portal, at });
            }
            return;
        }
        self.pressing = Some(portal);
        let portals: Vec<_> = self
            .strips
            .values()
            .filter(|p| p.mapped)
            .map(|p| p.portal)
            .collect();
        let hit = self.drag.fence(|sample| {
            self.gesture
                .pressed(sample, portal, &portals, &self.monitors, Instant::now())
        });
        if let Some(hit) = hit.filter(|_| self.gate.is_open() && self.check_epoch().is_ok()) {
            self.emit(CaptureEvent::DragDroppedAtEdge {
                portal,
                position: hit.position,
                window: hit.sample.window,
                grab: hit.grab,
                at,
            });
        }
        self.emit(CaptureEvent::EdgePressed {
            portal,
            position,
            at,
        });
    }
    fn scroll_frame(&mut self) {
        let frame = std::mem::take(&mut self.scroll);
        if !frame.present {
            self.scroll.source = frame.source;
            return;
        }
        let smooth = matches!(
            frame.source,
            Some(wl_pointer::AxisSource::Finger | wl_pointer::AxisSource::Continuous)
        );
        let phase = if smooth {
            let was_scrolling = self.scrolling.0 || self.scrolling.1;
            if frame.x != 0.0 {
                self.scrolling.0 = true;
            }
            if frame.y != 0.0 {
                self.scrolling.1 = true;
            }
            if frame.stop_x {
                self.scrolling.0 = false;
            }
            if frame.stop_y {
                self.scrolling.1 = false;
            }
            if (frame.stop_x || frame.stop_y) && !self.scrolling.0 && !self.scrolling.1 {
                ScrollPhase::Ended
            } else if was_scrolling {
                ScrollPhase::Changed
            } else {
                ScrollPhase::Began
            }
        } else {
            ScrollPhase::Discrete
        };
        // wl_pointer axis values already include the compositor's natural-scrolling setting;
        // convert only the Wayland-down to HID-up sign, without inverting that preference again.
        self.input(CaptureEvent::Scroll {
            delta: ScrollDelta {
                v120_x: frame.v120_x.unwrap_or(frame.discrete_x.saturating_mul(120)),
                v120_y: frame
                    .v120_y
                    .unwrap_or(frame.discrete_y.saturating_mul(120))
                    .saturating_neg(),
                pixels: smooth.then_some(VectorLogical::new(frame.x, -frame.y)),
                phase,
                stop_x: frame.stop_x,
                stop_y: frame.stop_y,
            },
            at: frame.at.unwrap_or_else(now),
        });
        self.scroll.source = frame.source;
    }
}
impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        s: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        s.check_gate();
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == "wl_output" && version >= 4 {
                    let proxy = registry.bind(name, 4, qh, ());
                    s.monitors_dirty = true;
                    s.outputs.push(Output {
                        global: name,
                        proxy,
                        name: String::new(),
                        mode: None,
                        rotated: false,
                    });
                }
                s.advertised.push((name, interface, version));
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(removed) = s
                    .outputs
                    .iter()
                    .find(|o| o.global == name)
                    .map(|o| o.proxy.clone())
                {
                    s.output_removed(&removed);
                    s.outputs.retain(|o| o.global != name);
                    s.monitors_dirty = true;
                }
                if s.advertised.iter().any(|(n, i, _)| {
                    *n == name
                        && matches!(
                            i.as_str(),
                            "wl_seat"
                                | "zwp_pointer_constraints_v1"
                                | "zwp_relative_pointer_manager_v1"
                                | "zwp_keyboard_shortcuts_inhibit_manager_v1"
                                | "zwlr_layer_shell_v1"
                        )
                }) {
                    s.finish(EndReason::Lost, Cause::GlobalRemoved);
                }
                s.advertised.retain(|(n, _, _)| *n != name);
            }
            _ => (),
        }
    }
}
impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        s: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        s.check_gate();
        if let Some(output) = s.outputs.iter_mut().find(|o| o.proxy == *output) {
            match event {
                wl_output::Event::Name { name } => {
                    s.monitors_dirty |= output.name != name;
                    output.name = name;
                }
                wl_output::Event::Geometry {
                    transform: WEnum::Value(transform),
                    ..
                } => {
                    let rotated = transform as u32 % 2 == 1;
                    s.monitors_dirty |= output.rotated != rotated;
                    output.rotated = rotated;
                }
                wl_output::Event::Mode {
                    flags: WEnum::Value(flags),
                    width,
                    height,
                    ..
                } if flags.contains(wl_output::Mode::Current) && width > 0 && height > 0 => {
                    let mode = Some((width as u32, height as u32));
                    s.monitors_dirty |= output.mode != mode;
                    output.mode = mode;
                }
                wl_output::Event::Scale { .. } => {
                    // IPC supplies fractional scale; wl_output only exposes its integer ceiling.
                    s.monitors_dirty = true;
                }
                _ => (),
            }
        }
    }
}
impl Dispatch<ZwlrLayerSurfaceV1, u64> for State {
    fn event(
        s: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        tag: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        s.check_gate();
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer.ack_configure(serial);
                if !s.ack_delay.is_zero() {
                    // Nested tests: the preparation learns of the configure only later.
                    let due = Instant::now() + s.ack_delay;
                    s.deferred
                        .push((due, Deferred::Size(*tag, (width, height))));
                } else if let Some(p) = s.strips.get_mut(tag) {
                    p.size = Some((width, height));
                }
            }
            zwlr_layer_surface_v1::Event::Closed => {
                if let Some(p) = s.strips.get_mut(tag) {
                    p.closed = true;
                    if p.mapped {
                        s.lost += 1;
                    }
                }
                if s.capture.as_ref().is_some_and(|c| c.strip == *tag) {
                    s.finish(EndReason::Lost, Cause::StripClosed);
                }
                if s.entered == Some(*tag) {
                    if let Some(p) = s.strips.get(tag) {
                        s.pressing = None;
                        s.emit(CaptureEvent::EdgeReleased {
                            portal: p.portal.id,
                            at: now(),
                        });
                    }
                    s.entered = None;
                }
            }
            _ => (),
        }
    }
}
impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        s: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        s.check_gate();
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } => {
                if let Some(tag) = surface.data::<u64>().copied() {
                    s.entered = Some(tag);
                    s.enter_serial = serial;
                    s.pointer_position = (surface_x, surface_y);
                    s.buttons.clear();
                    s.pressed(tag, surface_x, surface_y, now());
                }
            }
            wl_pointer::Event::Motion {
                time,
                surface_x,
                surface_y,
            } => {
                s.pointer_position = (surface_x, surface_y);
                if let Some(tag) = s.entered {
                    s.pressed(tag, surface_x, surface_y, milliseconds(time));
                }
            }
            wl_pointer::Event::Leave { surface, .. } => {
                if let Some(tag) = surface.data::<u64>().copied() {
                    if s.capture.as_ref().is_some_and(|c| c.strip == tag) {
                        s.finish(EndReason::Lost, Cause::PointerLeft);
                    }
                    if s.entered == Some(tag) {
                        if let Some(p) = s.strips.get(&tag)
                            && p.mapped
                        {
                            s.pressing = None;
                            s.emit(CaptureEvent::EdgeReleased {
                                portal: p.portal.id,
                                at: now(),
                            });
                        }
                        s.entered = None;
                    }
                }
            }
            wl_pointer::Event::Button {
                time,
                button,
                state: WEnum::Value(state),
                ..
            } => {
                let down = state == wl_pointer::ButtonState::Pressed;
                let changed = if down {
                    s.buttons.insert(button)
                } else {
                    s.buttons.remove(&button)
                };
                let mapped = match button {
                    0x110 => Some(MouseButton::PRIMARY),
                    0x111 => Some(MouseButton::SECONDARY),
                    0x112 => Some(MouseButton::TERTIARY),
                    0x113 => Some(MouseButton::BACK),
                    0x114 => Some(MouseButton::FORWARD),
                    _ => None,
                };
                if changed && let Some(button) = mapped {
                    s.input(CaptureEvent::Button {
                        button,
                        down,
                        at: milliseconds(time),
                    });
                }
            }
            wl_pointer::Event::Axis {
                time,
                axis: WEnum::Value(axis),
                value,
            } => {
                if axis == wl_pointer::Axis::HorizontalScroll {
                    s.scroll.x += value;
                } else {
                    s.scroll.y += value;
                }
                s.scroll.at = Some(milliseconds(time));
                s.scroll.present = true;
            }
            wl_pointer::Event::AxisSource {
                axis_source: WEnum::Value(source),
            } => {
                s.scroll.source = Some(source);
            }
            wl_pointer::Event::AxisValue120 {
                axis: WEnum::Value(axis),
                value120,
            } => {
                if axis == wl_pointer::Axis::HorizontalScroll {
                    s.scroll.v120_x = Some(value120);
                } else {
                    s.scroll.v120_y = Some(value120);
                }
                s.scroll.present = true;
            }
            wl_pointer::Event::AxisDiscrete {
                axis: WEnum::Value(axis),
                discrete,
            } => {
                if axis == wl_pointer::Axis::HorizontalScroll {
                    s.scroll.discrete_x = discrete;
                } else {
                    s.scroll.discrete_y = discrete;
                }
                s.scroll.present = true;
            }
            wl_pointer::Event::AxisStop {
                time,
                axis: WEnum::Value(axis),
            } => {
                if axis == wl_pointer::Axis::HorizontalScroll {
                    s.scroll.stop_x = true;
                } else {
                    s.scroll.stop_y = true;
                }
                s.scroll.at = Some(milliseconds(time));
                s.scroll.present = true;
            }
            wl_pointer::Event::Frame => s.scroll_frame(),
            _ => (),
        }
    }
}
impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        s: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        s.check_gate();
        match event {
            wl_keyboard::Event::Keymap {
                format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                fd,
                size,
            } => {
                let file = File::from(fd);
                let read = (|| {
                    if size > 8 * 1024 * 1024 {
                        return Err(());
                    }
                    let mut bytes = Zeroizing::new(vec![0; size as usize]);
                    let mut offset = 0;
                    // SCM_RIGHTS duplicates share the file offset. Read positionally, like the
                    // protocol's MAP_PRIVATE recommendation, so reconnects/other clients work.
                    while offset < bytes.len() {
                        let n = file
                            .read_at(&mut bytes[offset..], offset as u64)
                            .map_err(|_| ())?;
                        if n == 0 {
                            return Err(());
                        }
                        offset += n;
                    }
                    String::from_utf8(bytes.to_vec())
                        .map(Zeroizing::new)
                        .map_err(|_| ())
                })();
                let Ok(text) = read else {
                    s.error = Some(backend("invalid keyboard keymap"));
                    s.finish(EndReason::Lost, Cause::KeymapUnusable);
                    return;
                };
                let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
                if let Some(keymap) = xkb::Keymap::new_from_string(
                    &context,
                    text.trim_end_matches('\0').to_owned(),
                    xkb::KEYMAP_FORMAT_TEXT_V1,
                    xkb::KEYMAP_COMPILE_NO_FLAGS,
                ) {
                    s.xkb = Some(xkb::State::new(&keymap));
                } else {
                    s.error = Some(backend("keyboard keymap unavailable"));
                    s.finish(EndReason::Lost, Cause::KeymapUnusable);
                }
            }
            wl_keyboard::Event::Keymap { .. } => {
                s.error = Some(PlatformError::Unsupported("keyboard keymap format"));
                s.finish(EndReason::Lost, Cause::KeymapUnusable);
            }
            wl_keyboard::Event::Enter { surface, keys, .. } => {
                s.keyboard_surface = Some(surface.clone());
                s.keys = keys
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|k| u32::from_ne_bytes([k[0], k[1], k[2], k[3]]))
                    .collect();
                if let Some(c) = s.capture.as_mut()
                    && s.strips.get(&c.strip).is_some_and(|p| p.surface == surface)
                {
                    c.keyboard = true;
                    c.held = s
                        .keys
                        .iter()
                        .filter_map(|key| u16::try_from(*key).ok().and_then(evdev_to_hid))
                        .collect();
                    // Wayland leave released these keys in the original client; the compositor
                    // retains its physical state. Never replay enter's snapshot as key downs.
                }
            }
            wl_keyboard::Event::Leave { surface, .. } => {
                if s.keyboard_surface.as_ref() == Some(&surface) {
                    s.keyboard_surface = None;
                }
                if s.capture
                    .as_ref()
                    .is_some_and(|c| s.strips.get(&c.strip).is_some_and(|p| p.surface == surface))
                {
                    s.finish(EndReason::Lost, Cause::KeyboardLeft);
                }
            }
            wl_keyboard::Event::Key {
                time,
                key,
                state: WEnum::Value(state),
                ..
            } => {
                let down = state == wl_keyboard::KeyState::Pressed;
                let changed = if down {
                    s.keys.insert(key)
                } else {
                    s.keys.remove(&key)
                };
                if changed && let Some(usage) = u16::try_from(key).ok().and_then(evdev_to_hid) {
                    // This includes the up of a key in enter's snapshot. The router keeps an up
                    // without a remote down local, while release-chord tracking needs to see it.
                    s.input(CaptureEvent::Key {
                        usage,
                        down,
                        at: milliseconds(time),
                    });
                }
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(xkb) = s.xkb.as_mut() {
                    xkb.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                    let locks = LockKeys {
                        caps_lock: Some(xkb.led_name_is_active(&xkb::LED_NAME_CAPS)),
                        num_lock: Some(xkb.led_name_is_active(&xkb::LED_NAME_NUM)),
                        scroll_lock: None,
                    };
                    if locks != s.locks {
                        s.locks = locks;
                        if let Some(c) = s.capture.as_mut()
                            && !c.started
                        {
                            c.pending.push(CaptureEvent::LockKeys(locks));
                        } else {
                            s.emit(CaptureEvent::LockKeys(locks));
                        }
                    }
                }
            }
            // RepeatInfo is advisory: no repeat timer is ever started by this adapter.
            _ => (),
        }
    }
}
impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        s: &mut Self,
        _: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        s.check_gate();
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
            && !caps.contains(wl_seat::Capability::Pointer | wl_seat::Capability::Keyboard)
        {
            s.finish(EndReason::Lost, Cause::SeatLost);
        }
    }
}
impl Dispatch<ZwpLockedPointerV1, u64> for State {
    fn event(
        s: &mut Self,
        _: &ZwpLockedPointerV1,
        event: zwp_locked_pointer_v1::Event,
        generation: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        s.check_gate();
        if let Some(c) = s.capture.as_mut()
            && c.generation == *generation
        {
            match event {
                zwp_locked_pointer_v1::Event::Locked => c.locked = true,
                zwp_locked_pointer_v1::Event::Unlocked => {
                    s.finish(EndReason::Lost, Cause::Unlocked)
                }
                _ => (),
            }
        }
    }
}
impl Dispatch<ZwpKeyboardShortcutsInhibitorV1, u64> for State {
    fn event(
        s: &mut Self,
        _: &ZwpKeyboardShortcutsInhibitorV1,
        event: zwp_keyboard_shortcuts_inhibitor_v1::Event,
        generation: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        s.check_gate();
        if let Some(c) = s.capture.as_mut()
            && c.generation == *generation
        {
            match event {
                zwp_keyboard_shortcuts_inhibitor_v1::Event::Active => c.inhibited = true,
                zwp_keyboard_shortcuts_inhibitor_v1::Event::Inactive => {
                    s.finish(EndReason::Lost, Cause::InhibitorInactive)
                }
                _ => (),
            }
        }
    }
}
impl Dispatch<ZwpRelativePointerV1, u64> for State {
    fn event(
        s: &mut Self,
        _: &ZwpRelativePointerV1,
        event: zwp_relative_pointer_v1::Event,
        generation: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        s.check_gate();
        if s.capture
            .as_ref()
            .is_some_and(|c| c.generation == *generation)
            && let zwp_relative_pointer_v1::Event::RelativeMotion {
                utime_hi,
                utime_lo,
                dx_unaccel,
                dy_unaccel,
                ..
            } = event
        {
            // Hyprland's relative-pointer timestamp is CLOCK_MONOTONIC microseconds, unlike the
            // wrapping millisecond timestamps of wl_pointer/wl_keyboard. Convert to engine ns.
            let micros = (u64::from(utime_hi) << 32) | u64::from(utime_lo);
            s.input(CaptureEvent::Motion {
                dx: dx_unaccel,
                dy: dy_unaccel,
                kind: MotionKind::Unaccelerated,
                at: timestamp(micros.saturating_mul(1000)),
            });
        }
    }
}
impl Dispatch<wl_callback::WlCallback, u64> for State {
    fn event(
        s: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        token: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        s.check_gate();
        if s.ack_delay.is_zero() {
            s.sync_done = s.sync_done.max(*token);
        } else {
            // Nested tests: the roundtrip completes only after the held-back time.
            let due = Instant::now() + s.ack_delay;
            s.deferred.push((due, Deferred::Sync(*token)));
        }
    }
}
delegate_noop!(State: ignore wl_compositor::WlCompositor);
impl Dispatch<wl_surface::WlSurface, u64> for State {
    fn event(
        s: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        s.check_gate();
    }
}
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore ZwlrLayerShellV1);
delegate_noop!(State: ignore ZwpPointerConstraintsV1);
delegate_noop!(State: ignore ZwpRelativePointerManagerV1);
delegate_noop!(State: ignore ZwpKeyboardShortcutsInhibitManagerV1);
delegate_noop!(State: ignore WpCursorShapeManagerV1);
delegate_noop!(State: ignore WpCursorShapeDeviceV1);
delegate_noop!(State: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ignore ZwlrVirtualPointerV1);

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(id: u32) -> Monitor {
        Monitor {
            id: DisplayId(id),
            name: format!("M{id}"),
            width: 100,
            height: 100,
            scale: 1.0,
            origin: [0.0, 0.0],
        }
    }

    #[test]
    fn removing_another_output_never_ends_the_capture() {
        // Output 1 holds the capture's strip (tag 10) and another strip (tag 11); output 2 holds
        // tag 20. Whatever the order or number of strips, the removal of output 2 destroys its
        // strips and leaves the capture alone: the backend must not end a capture for an
        // unrelated output's removal.
        let strips = [(10u64, 1u32), (20, 2), (11, 1), (21, 2)];
        assert_eq!(
            removal_effect(Some(10), &strips, &2),
            RemovalEffect {
                ends_capture: false,
                destroys: vec![20, 21]
            }
        );
        assert!(!removal_effect(Some(11), &strips, &2).ends_capture);
        // The capture's own output going away is the backend's decision to end it.
        assert_eq!(
            removal_effect(Some(10), &strips, &1),
            RemovalEffect {
                ends_capture: true,
                destroys: vec![10, 11]
            }
        );
        assert!(removal_effect(Some(20), &strips, &2).ends_capture);
        // Without a capture nothing ends; an output without strips changes nothing.
        assert!(!removal_effect(None, &strips, &1).ends_capture);
        assert_eq!(
            removal_effect(Some(10), &strips, &3),
            RemovalEffect {
                ends_capture: false,
                destroys: vec![]
            }
        );
        // A capture strip that is no longer listed is not on any output.
        assert!(!removal_effect(Some(99), &strips, &1).ends_capture);
    }

    #[test]
    fn a_late_refresh_publishes_nothing() {
        let (start, deadline) = (Instant::now(), Instant::now() + Duration::from_millis(20));
        let mut cache = vec![monitor(1)];
        let mut dirty = true;
        // Read, parsed late: after the deadline the cache and the stale flag stay as they were.
        let late = deadline + Duration::from_millis(1);
        assert!(matches!(
            publish_monitors(
                &mut cache,
                &mut dirty,
                vec![monitor(1), monitor(2)],
                deadline,
                late
            ),
            Err(PlatformError::Timeout)
        ));
        assert_eq!(cache.len(), 1);
        assert!(
            dirty,
            "a refresh that timed out must leave the retry pending"
        );
        // In time: published, and the flag cleared.
        publish_monitors(
            &mut cache,
            &mut dirty,
            vec![monitor(1), monitor(2)],
            deadline,
            start,
        )
        .unwrap();
        assert_eq!(cache.len(), 2);
        assert!(!dirty);
        // Exactly at the deadline is late too.
        let mut dirty = true;
        assert!(publish_monitors(&mut cache, &mut dirty, vec![], deadline, deadline).is_err());
        assert_eq!(cache.len(), 2);
        assert!(dirty);
    }
}
