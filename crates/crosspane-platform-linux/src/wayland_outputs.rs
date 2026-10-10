//! `Displays` from public Wayland protocols (WP-G1.1): `wl_output` (v4: name, mode, physical
//! size) plus `zxdg_output_manager_v1` (logical position and size). Compositor-neutral: used on
//! GNOME and KDE.
//!
//! - **Identity.** `DisplayId` is a stable hash of the output's `wl_output.name` (the connector,
//!   e.g. `DP-1`): FNV-1a 32-bit of the UTF-8 bytes. Two outputs with the same name, or an output
//!   without a name, make the snapshot fail (`Backend`), never a guessed id. (A compositor that
//!   only offers `wl_output` below v4 gets its names from `zxdg_output_v1.name` instead.)
//! - **Geometry.** `pixel_size` is the current mode; `logical_origin` is the xdg-output logical
//!   position; `scale` is `mode width / logical width` (the real, possibly fractional scale, not
//!   the rounded `wl_output.scale`), with the transform applied (a 90°/270° rotation swaps the
//!   mode's width and height first, and the physical size with them). `physical_size` from
//!   `wl_output.geometry`, or 96 DPI from the pixel size when it reports 0.
//! - **Colour.** `ColorSpace::Srgb`, `hdr: false`.
//! - **Virtual monitors.** Outputs named `Meta-*` (Mutter's virtual monitors, WP-G2.4) are not
//!   displays: `assemble` drops them (`is_twin_output`) before validating or waiting for anything.
//! - **Threading.** One worker thread owns its own Wayland connection and event queue. `displays`
//!   returns its latest complete snapshot (every output has `done` after its xdg-output). Changes
//!   (outputs added/removed, mode, scale, position) produce one new snapshot after a 100 ms
//!   debounce. A lost connection delivers an empty snapshot and the worker ends.
//!
//! # Details
//!
//! - **No `smithay-client-toolkit`.** Its `OutputState` panics (`todo!`, `expect`, `panic!`) on
//!   protocol values it doesn't know and hides whether the xdg-output manager exists, so the
//!   handful of events are dispatched here directly, panic-free.
//! - **xdg-output is bound at version 2**, whose `done` event is not deprecated: a snapshot is
//!   complete once every output has seen both `wl_output.done` and `zxdg_output_v1.done`, with no
//!   dependence on the compositor sending a further `wl_output.done` after the xdg properties
//!   (the version 3 rule). Version 2 still carries `name` and `description`.
//! - **Snapshots.** Every event updates one record per output; `done` events only mark a change.
//!   After 100 ms without further changes (at most one second after the first) the records are
//!   assembled (`assemble`): not yet complete means "wait" (an error after two seconds), invalid
//!   (no name, duplicate name or id, non-positive size) means an error snapshot: `displays`
//!   returns `Backend` and a subscription gets an empty list and a logged warning, because stale
//!   data is worse than none. A new snapshot is delivered only if it differs from the last one.
//! - **Construction.** `new` fails with `Unsupported` without `WAYLAND_DISPLAY` or without the
//!   xdg-output manager, `Timeout` if no complete snapshot arrives within two seconds, and with
//!   the `Backend` error of the first snapshot if that is invalid.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
use std::io::ErrorKind;
use std::os::fd::OwnedFd;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crosspane_platform::{Displays, EventSink, PlatformError};
use crosspane_types::color::ColorSpace;
use crosspane_types::display::DisplayInfo;
use crosspane_types::geom::{DisplayGeometry, PixelSize, PointLogical, SizeMm};
use crosspane_types::id::DisplayId;
use rustix::event::{EventfdFlags, PollFd, PollFlags, Timespec, eventfd, poll};
use wayland_client::backend::WaylandError;
use wayland_client::globals::{BindError, GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1;
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::{self, ZxdgOutputV1};

/// Changes that arrive within this window produce one snapshot.
const DEBOUNCE: Duration = Duration::from_millis(100);
/// A steady stream of changes still produces a snapshot this long after the first.
const MAX_DEBOUNCE: Duration = Duration::from_secs(1);
/// How long `new` waits for the first complete snapshot.
const FIRST_SNAPSHOT_WAIT: Duration = Duration::from_secs(2);
/// The worker's own budget for the first snapshot, shorter than the constructor's wait so a
/// stalled compositor is reported by the worker (which then ends) rather than abandoned.
const WORKER_INIT_BUDGET: Duration = Duration::from_millis(1800);
/// Output data that stays incomplete this long becomes an error snapshot.
const INCOMPLETE_GRACE: Duration = Duration::from_secs(2);
/// How long `Drop` waits for the worker before detaching it.
const JOIN_WAIT: Duration = Duration::from_millis(500);
/// Millimetres per pixel at 96 DPI, used when an output reports no physical size.
const MM_PER_PX_96DPI: f64 = 25.4 / 96.0;
/// `wl_output` version bound: 4 adds `name` and `description`; 2 is the floor (`done`).
const WL_OUTPUT_VERSION: u32 = 4;
/// `zxdg_output_manager_v1` version bound; see the module docs for why not 3.
const XDG_OUTPUT_VERSION: u32 = 2;

fn backend(error: impl fmt::Display) -> PlatformError {
    PlatformError::Backend(format!("wayland outputs: {error}"))
}

/// Displays from `wl_output` + `xdg-output`.
#[derive(Debug)]
pub struct WaylandOutputs {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    wake: Arc<OwnedFd>,
    thread: Option<JoinHandle<()>>,
}

impl WaylandOutputs {
    /// Connect with `WAYLAND_DISPLAY`, bind the globals and wait for the first complete snapshot
    /// (bounded: 2 s). No xdg-output manager is `Unsupported`.
    pub fn new() -> Result<WaylandOutputs, PlatformError> {
        let shared = Arc::new(Shared::new());
        let stop = Arc::new(AtomicBool::new(false));
        let wake =
            Arc::new(eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK).map_err(backend)?);
        let (ready, ready_rx) = mpsc::channel::<Result<(), PlatformError>>();
        let thread = {
            let (shared, stop, wake) = (shared.clone(), stop.clone(), wake.clone());
            std::thread::Builder::new()
                .name("wayland-outputs".into())
                .spawn(move || worker_main(&stop, wake, &shared, &ready))
                .map_err(backend)?
        };
        match ready_rx.recv_timeout(FIRST_SNAPSHOT_WAIT) {
            Ok(Ok(())) => Ok(WaylandOutputs {
                shared,
                stop,
                wake,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                // The worker returns right after reporting a failure.
                join_bounded(thread, JOIN_WAIT);
                Err(error)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Not a blocking join: the worker may be stuck on a stalled socket. It ends by
                // itself once it notices the flag (or never, if the compositor never answers).
                stop.store(true, Ordering::Release);
                let _ = signal(&wake);
                Err(PlatformError::Timeout)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                join_bounded(thread, JOIN_WAIT);
                Err(backend("worker ended during startup"))
            }
        }
    }
}

impl Displays for WaylandOutputs {
    fn displays(&self) -> Result<Vec<DisplayInfo>, PlatformError> {
        self.shared.displays()
    }

    fn subscribe(
        &mut self,
        sink: Arc<dyn EventSink<Vec<DisplayInfo>>>,
    ) -> Result<(), PlatformError> {
        self.shared.subscribe(sink)
    }
}

impl Drop for WaylandOutputs {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = signal(&self.wake);
        if let Some(thread) = self.thread.take() {
            join_bounded(thread, JOIN_WAIT);
        }
    }
}

/// Join `thread` if it ends within `wait`; otherwise detach it (it ends by itself once it
/// notices the stop flag). Never joins the calling thread (a sink dropping its host).
fn join_bounded(thread: JoinHandle<()>, wait: Duration) {
    if thread.thread().id() == std::thread::current().id() {
        return;
    }
    let end = Instant::now() + wait;
    while !thread.is_finished() {
        if Instant::now() >= end {
            tracing::warn!("wayland outputs worker did not stop in time; detaching it");
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = thread.join();
}

fn signal(fd: &OwnedFd) -> Result<(), PlatformError> {
    loop {
        match rustix::io::write(fd, &1_u64.to_ne_bytes()) {
            Ok(8) | Err(rustix::io::Errno::AGAIN) => return Ok(()),
            Err(rustix::io::Errno::INTR) => (),
            Ok(_) => return Err(backend("short eventfd write")),
            Err(error) => return Err(backend(error)),
        }
    }
}

fn drain_wakeup(fd: &OwnedFd) -> Result<(), PlatformError> {
    let mut bytes = [0; 8];
    loop {
        match rustix::io::read(fd, &mut bytes) {
            Ok(8) | Err(rustix::io::Errno::INTR) => (),
            Err(rustix::io::Errno::AGAIN) => return Ok(()),
            Ok(_) => return Err(backend("short eventfd read")),
            Err(error) => return Err(backend(error)),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Latest snapshot and the subscription (shared between the handle and the worker).
// ---------------------------------------------------------------------------------------------

/// What `displays` returns and what the subscription's sink has been told. All sink calls happen
/// under the lock, which serialises them as `EventSink` requires (`send` never blocks).
struct Shared {
    inner: Mutex<Inner>,
}

struct Inner {
    /// The latest snapshot, or why there is none.
    snapshot: Result<Vec<DisplayInfo>, String>,
    /// Set when the connection ended; sticky.
    lost: Option<String>,
    sink: Option<Arc<dyn EventSink<Vec<DisplayInfo>>>>,
    /// The last list handed to the sink (`None` before the first delivery).
    sent: Option<Vec<DisplayInfo>>,
}

impl fmt::Debug for Shared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shared").finish_non_exhaustive()
    }
}

impl Shared {
    fn new() -> Shared {
        Shared {
            inner: Mutex::new(Inner {
                snapshot: Ok(Vec::new()),
                lost: None,
                sink: None,
                sent: None,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn displays(&self) -> Result<Vec<DisplayInfo>, PlatformError> {
        let inner = self.lock();
        if let Some(reason) = &inner.lost {
            return Err(backend(format!("connection lost: {reason}")));
        }
        inner.snapshot.clone().map_err(PlatformError::Backend)
    }

    fn subscribe(&self, sink: Arc<dyn EventSink<Vec<DisplayInfo>>>) -> Result<(), PlatformError> {
        let mut inner = self.lock();
        if inner.sink.is_some() {
            return Err(PlatformError::Backend(
                "Displays::subscribe called twice".into(),
            ));
        }
        inner.sink = Some(sink);
        inner.deliver();
        Ok(())
    }

    /// A new snapshot (or the reason there is none). Delivered if it changes what subscribers saw.
    fn publish(&self, outcome: Result<Vec<DisplayInfo>, String>) {
        let mut inner = self.lock();
        if inner.lost.is_some() {
            return;
        }
        if let Err(reason) = &outcome
            && inner.snapshot.as_ref().err() != Some(reason)
        {
            tracing::warn!(%reason, "wayland outputs unusable; reporting no displays");
        }
        inner.snapshot = outcome;
        inner.deliver();
    }

    /// The connection ended. Subscribers always get the empty list, even if it was empty before.
    fn lost(&self, reason: String) {
        let mut inner = self.lock();
        inner.snapshot = Err(format!("connection lost: {reason}"));
        inner.lost = Some(reason);
        inner.sent = None;
        inner.deliver();
    }
}

impl Inner {
    /// Hand the sink what a consumer should believe now: the snapshot, or no displays when there
    /// is none or the connection is gone. Skips repeats.
    fn deliver(&mut self) {
        let Some(sink) = &self.sink else { return };
        let view = if self.lost.is_some() {
            Vec::new()
        } else {
            self.snapshot.clone().unwrap_or_default()
        };
        if self.sent.as_ref() != Some(&view) {
            sink.send(view.clone());
            self.sent = Some(view);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Pure snapshot assembly.
// ---------------------------------------------------------------------------------------------

/// The current mode of an output, in the panel's own orientation (before the output transform).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ModeRecord {
    width: i32,
    height: i32,
    /// `wl_output.mode.refresh`: millihertz, 0 if unknown.
    refresh_millihz: i32,
}

/// Everything the protocols have said about one output.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct OutputRecord {
    /// `wl_output.name` (v4), else `zxdg_output_v1.name`.
    name: Option<String>,
    description: Option<String>,
    /// The transform turns the panel by 90° or 270° (flipped or not).
    swaps_axes: bool,
    /// `wl_output.geometry` physical size in millimetres, in the panel's orientation; 0 = unknown.
    physical_mm: (i32, i32),
    mode: Option<ModeRecord>,
    logical_position: Option<(i32, i32)>,
    logical_size: Option<(i32, i32)>,
    /// `wl_output.done` has arrived at least once.
    wl_done: bool,
    /// `zxdg_output_v1.done` has arrived at least once.
    xdg_done: bool,
}

impl OutputRecord {
    /// The parts a snapshot needs, once the output is fully described.
    fn settled(&self) -> Option<Settled> {
        if !(self.wl_done && self.xdg_done) {
            return None;
        }
        Some(Settled {
            mode: self.mode?,
            position: self.logical_position?,
            logical: self.logical_size?,
        })
    }
}

/// An output's data once it is all there.
#[derive(Clone, Copy, Debug)]
struct Settled {
    mode: ModeRecord,
    /// Logical position `(x, y)`.
    position: (i32, i32),
    /// Logical size `(width, height)`.
    logical: (i32, i32),
}

/// Why no snapshot could be assembled.
#[derive(Debug, PartialEq, Eq)]
enum SnapshotError {
    /// Some output hasn't been fully described yet; wait for more events.
    Incomplete,
    /// The outputs can't be turned into displays (described for logs).
    Invalid(String),
}

fn invalid(reason: impl Into<String>) -> SnapshotError {
    SnapshotError::Invalid(reason.into())
}

/// FNV-1a, 32 bit, over the name's UTF-8 bytes. (`pub(crate)`: the GNOME twin builds its own
/// display id from the connector name the same way.)
pub(crate) fn display_id(name: &str) -> DisplayId {
    DisplayId(name.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    }))
}

/// Whether `wl_output.transform` turns the panel by a quarter (90°, 270°, flipped or not).
fn swaps_axes(transform: wl_output::Transform) -> bool {
    matches!(
        transform,
        wl_output::Transform::_90
            | wl_output::Transform::_270
            | wl_output::Transform::Flipped90
            | wl_output::Transform::Flipped270
    )
}

fn positive(value: i32) -> Option<u32> {
    u32::try_from(value).ok().filter(|v| *v > 0)
}

/// Whether `name` is the connector of a Mutter virtual monitor (`Meta-0`, ...): the twin a
/// projected window is parked on (WP-G2.4). It is not one of the user's displays, so it is never
/// reported by [`WaylandOutputs`] (peers would be offered it and E1 would build edges on it); the
/// twin's own `DisplayInfo` is built from the DisplayConfig client where it is needed.
pub fn is_twin_output(name: &str) -> bool {
    name.starts_with(crate::gnome::display_config::TWIN_PREFIX)
}

/// Turn per-output records (in a stable order) into the display list. Mutter's virtual monitors
/// ([`is_twin_output`]) are left out before anything else, so an incomplete or odd twin never
/// delays or spoils the snapshot of the real displays.
fn assemble(records: &[OutputRecord]) -> Result<Vec<DisplayInfo>, SnapshotError> {
    let records: Vec<&OutputRecord> = records
        .iter()
        .filter(|record| !record.name.as_deref().is_some_and(is_twin_output))
        .collect();
    let parts = records
        .iter()
        .map(|record| record.settled())
        .collect::<Option<Vec<_>>>()
        .ok_or(SnapshotError::Incomplete)?;
    let mut displays = Vec::with_capacity(records.len());
    let mut taken: BTreeMap<DisplayId, &str> = BTreeMap::new();
    for (record, settled) in records.into_iter().zip(parts) {
        let display = display_info(record, settled)?;
        // `display_info` checked the name.
        let name = record.name.as_deref().unwrap_or_default();
        if let Some(other) = taken.insert(display.id, name) {
            return Err(if other == name {
                invalid(format!("two outputs are named {name:?}"))
            } else {
                invalid(format!(
                    "outputs {other:?} and {name:?} hash to the same display id"
                ))
            });
        }
        displays.push(display);
    }
    Ok(displays)
}

fn display_info(record: &OutputRecord, settled: Settled) -> Result<DisplayInfo, SnapshotError> {
    let Settled {
        mode,
        position,
        logical,
    } = settled;
    let name = record
        .name
        .as_deref()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| invalid("an output has no name"))?;
    let (Some(mode_w), Some(mode_h)) = (positive(mode.width), positive(mode.height)) else {
        return Err(invalid(format!(
            "output {name}: bad mode {}x{}",
            mode.width, mode.height
        )));
    };
    let (Some(logical_w), Some(_)) = (positive(logical.0), positive(logical.1)) else {
        return Err(invalid(format!(
            "output {name}: bad logical size {}x{}",
            logical.0, logical.1
        )));
    };
    // The mode is in the panel's orientation; the desktop sees it turned.
    let (pixel_w, pixel_h) = if record.swaps_axes {
        (mode_h, mode_w)
    } else {
        (mode_w, mode_h)
    };
    let physical = match record.physical_mm {
        (w, h) if w > 0 && h > 0 => {
            let (w, h) = (f64::from(w), f64::from(h));
            if record.swaps_axes { (h, w) } else { (w, h) }
        }
        _ => (
            f64::from(pixel_w) * MM_PER_PX_96DPI,
            f64::from(pixel_h) * MM_PER_PX_96DPI,
        ),
    };
    let display_name = match record.description.as_deref().filter(|d| !d.is_empty()) {
        Some(description) => format!("{name} ({description})"),
        None => name.to_owned(),
    };
    Ok(DisplayInfo {
        id: display_id(name),
        name: display_name,
        geometry: DisplayGeometry {
            physical_size: SizeMm::new(physical.0, physical.1),
            pixel_size: PixelSize::new(pixel_w, pixel_h),
            scale: f64::from(pixel_w) / f64::from(logical_w),
            logical_origin: PointLogical::new(f64::from(position.0), f64::from(position.1)),
        },
        refresh_millihz: u32::try_from(mode.refresh_millihz).unwrap_or(0),
        color_space: ColorSpace::Srgb,
        hdr: false,
    })
}

/// Change debouncing: one snapshot [`DEBOUNCE`] after the last change, but at most
/// [`MAX_DEBOUNCE`] after the first.
#[derive(Debug, Default)]
struct Debounce {
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Debounce {
    fn note(&mut self, now: Instant) {
        self.first.get_or_insert(now);
        self.last = Some(now);
    }

    fn deadline(&self) -> Option<Instant> {
        Some((self.last? + DEBOUNCE).min(self.first? + MAX_DEBOUNCE))
    }

    fn due(&self, now: Instant) -> bool {
        self.deadline().is_some_and(|deadline| now >= deadline)
    }

    fn clear(&mut self) {
        *self = Debounce::default();
    }
}

/// `Unsupported` unless the environment names a Wayland display. (`Connection::connect_to_env`
/// would otherwise fall back to `wayland-0`.)
fn require_display(var: Option<&OsStr>) -> Result<(), PlatformError> {
    match var {
        Some(display) if !display.is_empty() => Ok(()),
        _ => Err(PlatformError::Unsupported("no Wayland display")),
    }
}

// ---------------------------------------------------------------------------------------------
// The worker: connection, event dispatch, snapshots.
// ---------------------------------------------------------------------------------------------

/// Thread body: start, report readiness, run until stopped or the connection ends.
fn worker_main(
    stop: &AtomicBool,
    wake: Arc<OwnedFd>,
    shared: &Arc<Shared>,
    ready: &mpsc::Sender<Result<(), PlatformError>>,
) {
    let mut worker =
        match catch_unwind(AssertUnwindSafe(|| Worker::new(stop, wake, shared.clone()))) {
            Ok(Ok(worker)) => worker,
            Ok(Err(error)) => {
                tracing::debug!(%error, "wayland outputs worker did not start");
                let _ = ready.send(Err(error));
                return;
            }
            Err(_) => {
                let _ = ready.send(Err(backend("worker panicked during startup")));
                return;
            }
        };
    if ready.send(Ok(())).is_err() {
        // The constructor gave up waiting.
        return;
    }
    let reason = match catch_unwind(AssertUnwindSafe(|| worker.run(stop))) {
        Ok(Ok(())) => return,
        Ok(Err(error)) => error.to_string(),
        Err(_) => "worker panicked".to_owned(),
    };
    if stop.load(Ordering::Acquire) {
        // Errors while shutting down are not a loss anyone needs to hear about.
        return;
    }
    tracing::warn!(%reason, "wayland outputs connection lost; reporting no displays");
    shared.lost(reason);
}

struct Worker {
    connection: Connection,
    queue: EventQueue<State>,
    state: State,
    wake: Arc<OwnedFd>,
    shared: Arc<Shared>,
    debounce: Debounce,
    /// Since when the outputs have been incomplete, while they are.
    incomplete_since: Option<Instant>,
}

impl Worker {
    /// Connect, bind the globals and wait for the first usable snapshot, which is published.
    fn new(
        stop: &AtomicBool,
        wake: Arc<OwnedFd>,
        shared: Arc<Shared>,
    ) -> Result<Worker, PlatformError> {
        require_display(std::env::var_os("WAYLAND_DISPLAY").as_deref())?;
        let deadline = Instant::now() + WORKER_INIT_BUDGET;
        let connection = Connection::connect_to_env().map_err(backend)?;
        let (globals, queue) = registry_queue_init::<State>(&connection).map_err(backend)?;
        let qh = queue.handle();
        let manager = globals
            .bind::<ZxdgOutputManagerV1, _, _>(&qh, 1..=XDG_OUTPUT_VERSION, ())
            .map_err(|error| match error {
                BindError::NotPresent | BindError::UnsupportedVersion => {
                    PlatformError::Unsupported("compositor has no zxdg_output_manager_v1")
                }
            })?;
        let mut state = State {
            manager,
            outputs: BTreeMap::new(),
            changed: false,
            failure: None,
        };
        globals.contents().with_list(|list| {
            for global in list {
                if global.interface == "wl_output" {
                    state.add_output(globals.registry(), &qh, global.name, global.version);
                }
            }
        });
        let mut worker = Worker {
            connection,
            queue,
            state,
            wake,
            shared,
            debounce: Debounce::default(),
            incomplete_since: None,
        };
        loop {
            worker.dispatch()?;
            match assemble(&worker.state.records()) {
                Ok(displays) => {
                    worker.shared.publish(Ok(displays));
                    return Ok(worker);
                }
                Err(SnapshotError::Invalid(reason)) => return Err(backend(reason)),
                Err(SnapshotError::Incomplete) => {}
            }
            if stop.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Err(PlatformError::Timeout);
            }
            worker.wait(Some(deadline))?;
        }
    }

    /// Run until `stop`; an error means the connection is gone.
    fn run(&mut self, stop: &AtomicBool) -> Result<(), PlatformError> {
        while !stop.load(Ordering::Acquire) {
            let now = Instant::now();
            if self.dispatch()? {
                self.debounce.note(now);
            }
            if self.debounce.due(now) {
                self.debounce.clear();
                self.publish(now);
            }
            self.wait(self.debounce.deadline())?;
        }
        Ok(())
    }

    /// Assemble and publish the current outputs. Incomplete ones are looked at again shortly,
    /// until [`INCOMPLETE_GRACE`] has passed.
    fn publish(&mut self, now: Instant) {
        let outcome = match assemble(&self.state.records()) {
            Ok(displays) => {
                self.incomplete_since = None;
                Ok(displays)
            }
            Err(SnapshotError::Invalid(reason)) => {
                self.incomplete_since = None;
                Err(reason)
            }
            Err(SnapshotError::Incomplete) => {
                let since = *self.incomplete_since.get_or_insert(now);
                if now.saturating_duration_since(since) < INCOMPLETE_GRACE {
                    self.debounce.note(now);
                    return;
                }
                Err("an output is still missing data".to_owned())
            }
        };
        self.shared.publish(outcome);
    }

    /// Handle every queued event. True if any of them may have changed the outputs.
    fn dispatch(&mut self) -> Result<bool, PlatformError> {
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(backend)?;
        if let Some(error) = self.state.failure.take() {
            return Err(error);
        }
        Ok(std::mem::take(&mut self.state.changed))
    }

    /// Flush requests, then block for compositor events, the wake-up or `deadline`. Events read
    /// are queued for the next [`Worker::dispatch`].
    fn wait(&mut self, deadline: Option<Instant>) -> Result<(), PlatformError> {
        let writable = match self.connection.flush() {
            Ok(()) => false,
            Err(WaylandError::Io(error)) if error.kind() == ErrorKind::WouldBlock => true,
            Err(error) => return Err(backend(error)),
        };
        // `None`: events are already queued, so there is nothing to wait for.
        let Some(guard) = self.connection.prepare_read() else {
            return Ok(());
        };
        let flags = if writable {
            PollFlags::IN | PollFlags::OUT
        } else {
            PollFlags::IN
        };
        let mut fds = [
            PollFd::new(&self.connection, flags),
            PollFd::new(&self.wake, PollFlags::IN),
        ];
        let timeout = deadline
            .map(|deadline| Timespec::try_from(deadline.saturating_duration_since(Instant::now())))
            .transpose()
            .map_err(backend)?;
        match poll(&mut fds, timeout.as_ref()) {
            Ok(_) => {
                if fds[1].revents().contains(PollFlags::IN) {
                    drain_wakeup(&self.wake)?;
                }
                if fds[0]
                    .revents()
                    .intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR)
                {
                    match guard.read() {
                        Ok(_) => (),
                        Err(WaylandError::Io(error)) if error.kind() == ErrorKind::WouldBlock => {}
                        Err(error) => return Err(backend(error)),
                    }
                }
            }
            // Dropping the guard cancels the read.
            Err(rustix::io::Errno::INTR) => (),
            Err(error) => return Err(backend(error)),
        }
        Ok(())
    }
}

/// Protocol state, filled in by the `Dispatch` impls below.
struct State {
    manager: ZxdgOutputManagerV1,
    /// Keyed by the output's registry name, which keeps snapshots in a stable order.
    outputs: BTreeMap<u32, Output>,
    /// A `done` arrived or an output came or went since the worker last looked.
    changed: bool,
    /// Something the worker can't continue after.
    failure: Option<PlatformError>,
}

struct Output {
    wl: wl_output::WlOutput,
    xdg: ZxdgOutputV1,
    record: OutputRecord,
}

impl State {
    fn add_output(
        &mut self,
        registry: &wl_registry::WlRegistry,
        qh: &QueueHandle<State>,
        name: u32,
        version: u32,
    ) {
        if version < 2 {
            // No `done`, so no way to tell when the output is described.
            self.failure
                .get_or_insert(PlatformError::Unsupported("wl_output version 2 required"));
            return;
        }
        let wl: wl_output::WlOutput = registry.bind(name, version.min(WL_OUTPUT_VERSION), qh, name);
        let xdg = self.manager.get_xdg_output(&wl, qh, name);
        self.outputs.insert(
            name,
            Output {
                wl,
                xdg,
                record: OutputRecord::default(),
            },
        );
        self.changed = true;
    }

    fn records(&self) -> Vec<OutputRecord> {
        self.outputs
            .values()
            .map(|output| output.record.clone())
            .collect()
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == "wl_output" => state.add_output(registry, qh, name, version),
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(output) = state.outputs.remove(&name) {
                    output.xdg.destroy();
                    if output.wl.version() >= 3 {
                        output.wl.release();
                    }
                    state.changed = true;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state.outputs.get_mut(global) else {
            return;
        };
        let record = &mut output.record;
        match event {
            wl_output::Event::Geometry {
                physical_width,
                physical_height,
                transform,
                ..
            } => {
                record.physical_mm = (physical_width, physical_height);
                // A transform this client doesn't know leaves the orientation as it was.
                if let WEnum::Value(transform) = transform {
                    record.swaps_axes = swaps_axes(transform);
                }
            }
            wl_output::Event::Mode {
                flags: WEnum::Value(flags),
                width,
                height,
                refresh,
            } if flags.contains(wl_output::Mode::Current) => {
                record.mode = Some(ModeRecord {
                    width,
                    height,
                    refresh_millihz: refresh,
                });
            }
            wl_output::Event::Name { name } => record.name = Some(name),
            wl_output::Event::Description { description } => {
                record.description = Some(description);
            }
            wl_output::Event::Done => {
                record.wl_done = true;
                state.changed = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state.outputs.get_mut(global) else {
            return;
        };
        let record = &mut output.record;
        // `wl_output` v4 names win; xdg-output names only fill in below that.
        let xdg_names = output.wl.version() < WL_OUTPUT_VERSION;
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                record.logical_position = Some((x, y));
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                record.logical_size = Some((width, height));
            }
            zxdg_output_v1::Event::Name { name } if xdg_names => record.name = Some(name),
            zxdg_output_v1::Event::Description { description } if xdg_names => {
                record.description = Some(description);
            }
            zxdg_output_v1::Event::Done => {
                record.xdg_done = true;
                state.changed = true;
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ignore ZxdgOutputManagerV1);

#[cfg(test)]
mod tests {
    use super::*;

    /// A fully described output: `mode` is `(w, h, mHz)`, `logical` is `(x, y, w, h)`.
    fn record(name: &str, mode: (i32, i32, i32), logical: (i32, i32, i32, i32)) -> OutputRecord {
        OutputRecord {
            name: Some(name.to_owned()),
            description: None,
            swaps_axes: false,
            physical_mm: (600, 340),
            mode: Some(ModeRecord {
                width: mode.0,
                height: mode.1,
                refresh_millihz: mode.2,
            }),
            logical_position: Some((logical.0, logical.1)),
            logical_size: Some((logical.2, logical.3)),
            wl_done: true,
            xdg_done: true,
        }
    }

    fn one(record: OutputRecord) -> DisplayInfo {
        let mut displays = assemble(&[record]).unwrap();
        assert_eq!(displays.len(), 1);
        displays.remove(0)
    }

    #[test]
    fn scale_one() {
        let d = one(record("DP-1", (1920, 1080, 60_000), (0, 0, 1920, 1080)));
        assert_eq!(d.name, "DP-1");
        assert_eq!(d.id, display_id("DP-1"));
        assert_eq!(d.geometry.scale, 1.0);
        assert_eq!(d.geometry.pixel_size, PixelSize::new(1920, 1080));
        assert_eq!(d.geometry.physical_size, SizeMm::new(600.0, 340.0));
        assert_eq!(d.geometry.logical_origin, PointLogical::new(0.0, 0.0));
        assert_eq!(d.refresh_millihz, 60_000);
        assert_eq!(d.color_space, ColorSpace::Srgb);
        assert!(!d.hdr);
        assert!(d.geometry.is_valid());
    }

    #[test]
    fn scale_one_and_a_quarter() {
        // 3440x1440 at 125 %: logical 2752x1152, exact.
        let d = one(record("DP-3", (3440, 1440, 164_999), (1920, 0, 2752, 1152)));
        assert_eq!(d.geometry.scale, 1.25);
        assert_eq!(d.geometry.pixel_size, PixelSize::new(3440, 1440));
        assert_eq!(d.refresh_millihz, 164_999);
        assert!(d.geometry.is_valid());
    }

    #[test]
    fn scale_one_and_a_half() {
        let d = one(record("eDP-1", (3840, 2160, 60_000), (0, 0, 2560, 1440)));
        assert_eq!(d.geometry.scale, 1.5);
        // A compositor rounds an inexact logical size (2560 / 1.5 = 1706.67 -> 1707); the scale
        // then follows the rounded width and is off by less than a tenth of a percent.
        let d = one(record("eDP-1", (2560, 1440, 60_000), (0, 0, 1707, 960)));
        assert!((d.geometry.scale - 1.5).abs() < 1.5e-3);
    }

    #[test]
    fn scale_two() {
        let d = one(record("eDP-1", (2880, 1800, 120_000), (0, 0, 1440, 900)));
        assert_eq!(d.geometry.scale, 2.0);
        assert_eq!(d.geometry.pixel_size, PixelSize::new(2880, 1800));
        assert!(d.geometry.is_valid());
    }

    #[test]
    fn rotation_swaps_axes_for_pixels_physical_and_scale() {
        // A 1920x1080 panel turned 90 degrees: the desktop sees 1080x1920.
        let mut r = record("HDMI-A-1", (1920, 1080, 74_973), (0, 600, 1080, 1920));
        r.swaps_axes = true;
        let d = one(r);
        assert_eq!(d.geometry.pixel_size, PixelSize::new(1080, 1920));
        assert_eq!(d.geometry.physical_size, SizeMm::new(340.0, 600.0));
        assert_eq!(d.geometry.scale, 1.0);
        assert_eq!(d.geometry.logical_origin, PointLogical::new(0.0, 600.0));
        assert_eq!(d.refresh_millihz, 74_973);

        // Turned and scaled: 3840x2160 panel, logical 1080x1920, so 2x.
        let mut r = record("DP-2", (3840, 2160, 60_000), (0, 0, 1080, 1920));
        r.swaps_axes = true;
        let d = one(r);
        assert_eq!(d.geometry.pixel_size, PixelSize::new(2160, 3840));
        assert_eq!(d.geometry.scale, 2.0);
        assert!(d.geometry.is_valid());
    }

    #[test]
    fn transforms_that_swap_axes() {
        use wl_output::Transform;
        for (transform, swaps) in [
            (Transform::Normal, false),
            (Transform::_90, true),
            (Transform::_180, false),
            (Transform::_270, true),
            (Transform::Flipped, false),
            (Transform::Flipped90, true),
            (Transform::Flipped180, false),
            (Transform::Flipped270, true),
        ] {
            assert_eq!(swaps_axes(transform), swaps, "{transform:?}");
        }
    }

    #[test]
    fn negative_origin() {
        let d = one(record(
            "DP-2",
            (1920, 1080, 60_000),
            (-1920, -200, 1920, 1080),
        ));
        assert_eq!(
            d.geometry.logical_origin,
            PointLogical::new(-1920.0, -200.0)
        );
        assert!(d.geometry.is_valid());
    }

    #[test]
    fn several_outputs_keep_their_order_and_get_distinct_ids() {
        let displays = assemble(&[
            record("DP-1", (1920, 1080, 60_000), (0, 0, 1920, 1080)),
            record("HDMI-A-1", (1920, 1080, 60_000), (1920, 0, 1920, 1080)),
            // Cloned: same place, different connector.
            record("DP-2", (1920, 1080, 60_000), (0, 0, 1920, 1080)),
        ])
        .unwrap();
        let names: Vec<_> = displays.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["DP-1", "HDMI-A-1", "DP-2"]);
        let mut ids: Vec<_> = displays.iter().map(|d| d.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 3);
        assert_eq!(assemble(&[]).unwrap(), Vec::new());
    }

    #[test]
    fn mutter_virtual_monitors_are_not_displays() {
        let twin = record("Meta-0", (1800, 1169, 60_000), (4520, 1080, 1800, 1169));
        let displays = assemble(&[
            record("DP-3", (3440, 1440, 165_000), (1080, 1080, 3440, 1440)),
            twin.clone(),
            record("DP-2", (1920, 1080, 75_000), (1817, 0, 1920, 1080)),
        ])
        .unwrap();
        let names: Vec<_> = displays.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["DP-3", "DP-2"]);
        // Only the twin: no displays at all, not an error.
        assert_eq!(assemble(std::slice::from_ref(&twin)).unwrap(), Vec::new());
        // A twin that is not fully described yet does not hold the real displays back ...
        let mut unfinished = twin.clone();
        unfinished.xdg_done = false;
        unfinished.logical_size = None;
        let displays = assemble(&[
            record("DP-3", (3440, 1440, 165_000), (1080, 1080, 3440, 1440)),
            unfinished,
        ])
        .unwrap();
        assert_eq!(displays.len(), 1);
        // ... nor does a second twin with a broken size or the same name spoil them.
        let mut broken = twin.clone();
        broken.logical_size = Some((0, 0));
        let displays = assemble(&[
            record("DP-3", (3440, 1440, 165_000), (1080, 1080, 3440, 1440)),
            twin.clone(),
            twin,
            broken,
        ])
        .unwrap();
        assert_eq!(displays.len(), 1);
        // The prefix match is on the whole connector name's start: other names stay displays.
        assert_eq!(
            assemble(&[record(
                "eDP-Meta-1",
                (1920, 1080, 60_000),
                (0, 0, 1920, 1080)
            )])
            .unwrap()
            .len(),
            1
        );
    }

    #[test]
    fn is_twin_output_matches_mutter_virtual_connectors() {
        assert!(is_twin_output("Meta-0"));
        assert!(is_twin_output("Meta-17"));
        assert!(is_twin_output("Meta-"));
        assert!(!is_twin_output("DP-3"));
        assert!(!is_twin_output("eDP-1"));
        assert!(!is_twin_output("HDMI-A-1"));
        assert!(!is_twin_output("meta-0"));
        assert!(!is_twin_output("eDP-Meta-1"));
        assert!(!is_twin_output(""));
    }

    #[test]
    fn duplicate_names_fail_the_snapshot() {
        let a = record("DP-1", (1920, 1080, 60_000), (0, 0, 1920, 1080));
        let b = record("DP-1", (1920, 1080, 60_000), (1920, 0, 1920, 1080));
        match assemble(&[a, b]) {
            Err(SnapshotError::Invalid(reason)) => assert!(reason.contains("DP-1"), "{reason}"),
            other => panic!("expected an invalid snapshot, got {other:?}"),
        }
    }

    #[test]
    fn missing_or_empty_names_fail_the_snapshot() {
        let mut nameless = record("x", (1920, 1080, 60_000), (0, 0, 1920, 1080));
        nameless.name = None;
        assert!(matches!(
            assemble(&[nameless.clone()]),
            Err(SnapshotError::Invalid(_))
        ));
        nameless.name = Some(String::new());
        assert!(matches!(
            assemble(&[nameless]),
            Err(SnapshotError::Invalid(_))
        ));
    }

    #[test]
    fn missing_xdg_or_mode_data_is_incomplete_not_invalid() {
        let good = record("DP-1", (1920, 1080, 60_000), (0, 0, 1920, 1080));
        let mut no_size = good.clone();
        no_size.logical_size = None;
        let mut no_position = good.clone();
        no_position.logical_position = None;
        let mut no_xdg_done = good.clone();
        no_xdg_done.xdg_done = false;
        let mut no_wl_done = good.clone();
        no_wl_done.wl_done = false;
        let mut no_mode = good.clone();
        no_mode.mode = None;
        for incomplete in [no_size, no_position, no_xdg_done, no_wl_done, no_mode] {
            assert_eq!(
                assemble(&[good.clone(), incomplete]),
                Err(SnapshotError::Incomplete)
            );
        }
        // Waiting for one output beats judging another: a bad record next to an incomplete one
        // is reported once everything has arrived.
        let mut nameless = good.clone();
        nameless.name = None;
        let mut no_size = good;
        no_size.logical_size = None;
        assert_eq!(
            assemble(&[nameless, no_size]),
            Err(SnapshotError::Incomplete)
        );
        assert_eq!(
            assemble(&[OutputRecord::default()]),
            Err(SnapshotError::Incomplete)
        );
    }

    #[test]
    fn non_positive_sizes_fail_the_snapshot() {
        for logical in [(0, 0, 0, 1080), (0, 0, 1920, 0), (0, 0, -1920, 1080)] {
            let r = record("DP-1", (1920, 1080, 60_000), logical);
            assert!(
                matches!(assemble(&[r]), Err(SnapshotError::Invalid(_))),
                "{logical:?}"
            );
        }
        for mode in [(0, 1080, 60_000), (1920, -1, 60_000)] {
            let r = record("DP-1", mode, (0, 0, 1920, 1080));
            assert!(
                matches!(assemble(&[r]), Err(SnapshotError::Invalid(_))),
                "{mode:?}"
            );
        }
    }

    #[test]
    fn unknown_physical_size_falls_back_to_96_dpi() {
        let mut r = record("WL-1", (950, 1046, 60_000), (-950, 0, 950, 1046));
        r.physical_mm = (0, 0);
        let d = one(r.clone());
        assert!((d.geometry.physical_size.width - 950.0 * 25.4 / 96.0).abs() < 1e-9);
        assert!((d.geometry.physical_size.height - 1046.0 * 25.4 / 96.0).abs() < 1e-9);
        // One zero is as good as none, and a rotated output derives from the rotated pixels.
        r.physical_mm = (600, 0);
        r.swaps_axes = true;
        r.logical_size = Some((1046, 950));
        let d = one(r);
        assert!((d.geometry.physical_size.width - 1046.0 * 25.4 / 96.0).abs() < 1e-9);
        assert!((d.geometry.physical_size.height - 950.0 * 25.4 / 96.0).abs() < 1e-9);
        assert!(d.geometry.is_valid());
    }

    #[test]
    fn description_is_appended_and_refresh_is_clamped() {
        let mut r = record("DP-1", (1920, 1080, -5), (0, 0, 1920, 1080));
        r.description = Some("Dell Inc. U2720Q".into());
        let d = one(r.clone());
        assert_eq!(d.name, "DP-1 (Dell Inc. U2720Q)");
        assert_eq!(d.refresh_millihz, 0);
        // The id follows the connector name alone.
        assert_eq!(d.id, display_id("DP-1"));
        r.description = Some(String::new());
        assert_eq!(one(r).name, "DP-1");
    }

    #[test]
    fn display_ids_are_fnv_1a_32() {
        // Published FNV-1a 32-bit test vectors.
        assert_eq!(display_id(""), DisplayId(0x811c_9dc5));
        assert_eq!(display_id("a"), DisplayId(0xe40c_292c));
        assert_eq!(display_id("foobar"), DisplayId(0xbf9c_f968));
        assert_ne!(display_id("DP-1"), display_id("DP-2"));
    }

    #[test]
    fn debounce_waits_for_quiet_but_not_forever() {
        let t0 = Instant::now();
        let mut debounce = Debounce::default();
        assert_eq!(debounce.deadline(), None);
        assert!(!debounce.due(t0));

        debounce.note(t0);
        assert_eq!(debounce.deadline(), Some(t0 + DEBOUNCE));
        assert!(!debounce.due(t0 + DEBOUNCE / 2));
        // Another change pushes the deadline out.
        debounce.note(t0 + DEBOUNCE / 2);
        assert_eq!(debounce.deadline(), Some(t0 + DEBOUNCE / 2 + DEBOUNCE));
        assert!(!debounce.due(t0 + DEBOUNCE));
        assert!(debounce.due(t0 + DEBOUNCE / 2 + DEBOUNCE));

        // A constant stream is cut off one second after its first change.
        let mut debounce = Debounce::default();
        debounce.note(t0);
        let mut now = t0;
        while now < t0 + MAX_DEBOUNCE {
            now += DEBOUNCE / 2;
            debounce.note(now.min(t0 + MAX_DEBOUNCE));
        }
        assert_eq!(debounce.deadline(), Some(t0 + MAX_DEBOUNCE));
        assert!(debounce.due(t0 + MAX_DEBOUNCE));

        debounce.clear();
        assert_eq!(debounce.deadline(), None);
    }

    #[test]
    fn needs_a_wayland_display() {
        assert!(matches!(
            require_display(None),
            Err(PlatformError::Unsupported("no Wayland display"))
        ));
        assert!(matches!(
            require_display(Some(OsStr::new(""))),
            Err(PlatformError::Unsupported("no Wayland display"))
        ));
        assert!(require_display(Some(OsStr::new("wayland-1"))).is_ok());
    }

    type Seen = Arc<Mutex<Vec<Vec<DisplayInfo>>>>;

    fn collector() -> (Seen, Arc<dyn EventSink<Vec<DisplayInfo>>>) {
        let seen: Seen = Arc::default();
        let sink = {
            let seen = seen.clone();
            Arc::new(move |displays: Vec<DisplayInfo>| seen.lock().unwrap().push(displays))
        };
        (seen, sink)
    }

    fn list(names: &[&str]) -> Vec<DisplayInfo> {
        let records: Vec<_> = names
            .iter()
            .map(|n| record(n, (1920, 1080, 60_000), (0, 0, 1920, 1080)))
            .collect();
        assemble(&records).unwrap()
    }

    #[test]
    fn subscription_delivers_current_then_changes_without_repeats() {
        let shared = Shared::new();
        shared.publish(Ok(list(&["DP-1"])));
        assert_eq!(shared.displays().unwrap(), list(&["DP-1"]));

        let (seen, sink) = collector();
        shared.subscribe(sink.clone()).unwrap();
        assert_eq!(*seen.lock().unwrap(), [list(&["DP-1"])]);
        assert!(matches!(
            shared.subscribe(sink),
            Err(PlatformError::Backend(_))
        ));

        // The same list again is not a change; a new one is.
        shared.publish(Ok(list(&["DP-1"])));
        shared.publish(Ok(list(&["DP-1", "DP-2"])));
        shared.publish(Ok(list(&["DP-1", "DP-2"])));
        assert_eq!(
            *seen.lock().unwrap(),
            [list(&["DP-1"]), list(&["DP-1", "DP-2"])]
        );
        assert_eq!(shared.displays().unwrap(), list(&["DP-1", "DP-2"]));
    }

    #[test]
    fn an_invalid_snapshot_is_an_error_and_an_empty_list() {
        let shared = Shared::new();
        shared.publish(Ok(list(&["DP-1"])));
        let (seen, sink) = collector();
        shared.subscribe(sink).unwrap();

        shared.publish(Err("two outputs are named \"DP-1\"".into()));
        assert!(matches!(shared.displays(), Err(PlatformError::Backend(_))));
        assert_eq!(*seen.lock().unwrap(), [list(&["DP-1"]), Vec::new()]);

        // Recovery delivers the real list again.
        shared.publish(Ok(list(&["DP-1"])));
        assert_eq!(shared.displays().unwrap(), list(&["DP-1"]));
        assert_eq!(seen.lock().unwrap().last().unwrap(), &list(&["DP-1"]));
    }

    #[test]
    fn a_lost_connection_is_an_empty_list_even_when_it_was_empty() {
        let shared = Shared::new();
        shared.publish(Ok(Vec::new()));
        let (seen, sink) = collector();
        shared.subscribe(sink).unwrap();
        shared.lost("broken pipe".into());
        assert_eq!(*seen.lock().unwrap(), [Vec::new(), Vec::new()]);
        assert!(matches!(shared.displays(), Err(PlatformError::Backend(_))));
        // Nothing resurrects it.
        shared.publish(Ok(list(&["DP-1"])));
        assert_eq!(seen.lock().unwrap().len(), 2);
        assert!(shared.displays().is_err());
    }

    #[test]
    fn subscribing_after_a_loss_gets_the_empty_list() {
        let shared = Shared::new();
        shared.publish(Ok(list(&["DP-1"])));
        shared.lost("gone".into());
        let (seen, sink) = collector();
        shared.subscribe(sink).unwrap();
        assert_eq!(*seen.lock().unwrap(), [Vec::new()]);
    }

    /// Wait for a delivered list that satisfies `wanted`.
    fn wait_for(
        rx: &mpsc::Receiver<Vec<DisplayInfo>>,
        wanted: impl Fn(&[DisplayInfo]) -> bool,
    ) -> Vec<DisplayInfo> {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            let left = end.saturating_duration_since(Instant::now());
            let displays = rx.recv_timeout(left).expect("no matching snapshot in 5 s");
            if wanted(&displays) {
                return displays;
            }
        }
    }

    /// Against the nested Hyprland (`eval "$(scripts/hypr-nested.sh env --name N)"`); skipped
    /// otherwise. Reads the nested output, compares it with the Hyprland adapter's view of the
    /// same output, then rescales it and expects the subscription to follow. (Rotation is left to
    /// the unit tests: the nested Hyprland's window-backed output keeps reporting the unrotated
    /// geometry when only its transform changes.)
    #[test]
    fn nested_hyprland_reports_and_follows_its_output() {
        use crate::hyprland::displays::HyprlandDisplays;
        use crate::hyprland::ipc::HyprIpc;

        if std::env::var("CROSSPANE_NESTED_HYPR").as_deref() != Ok("1") {
            eprintln!("skipped: needs a nested Hyprland (eval \"$(scripts/hypr-nested.sh env)\")");
            return;
        }
        let ipc = HyprIpc::from_env().unwrap();
        let mut outputs = WaylandOutputs::new().unwrap();
        let displays = outputs.displays().unwrap();
        eprintln!("wayland outputs: {displays:#?}");
        assert!(!displays.is_empty());
        for d in &displays {
            assert!(d.geometry.is_valid(), "{d:?}");
        }

        // The Hyprland adapter sees the same outputs.
        let hypr = HyprlandDisplays::new(ipc.clone())
            .unwrap()
            .displays()
            .unwrap();
        assert_eq!(hypr.len(), displays.len());
        for h in &hypr {
            let d = displays
                .iter()
                .find(|d| d.name.starts_with(&h.name))
                .unwrap_or_else(|| panic!("no Wayland output for {}", h.name));
            assert_eq!(d.geometry.pixel_size, h.geometry.pixel_size);
            assert_eq!(d.geometry.logical_origin, h.geometry.logical_origin);
            assert!((d.geometry.scale - h.geometry.scale).abs() < 0.01);
            assert!((d.geometry.physical_size.width - h.geometry.physical_size.width).abs() < 1.0);
            assert!((d.refresh_millihz.abs_diff(h.refresh_millihz)) < 1_000);
        }

        let (tx, rx) = mpsc::channel();
        outputs
            .subscribe(Arc::new(move |list: Vec<DisplayInfo>| {
                let _ = tx.send(list);
            }))
            .unwrap();
        assert!(matches!(
            outputs.subscribe(Arc::new(|_: Vec<DisplayInfo>| {})),
            Err(PlatformError::Backend(_))
        ));
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), displays);

        // Rescaled: the fractional scale comes back (Hyprland itself snaps 1.5 to 1.6 here, the
        // nearest scale with an exact logical size, so ask for that).
        let first = &displays[0];
        let rescale = |scale: f64| {
            ipc.eval(&format!(
                "hl.monitor({{ output = {:?}, mode = \"{}x{}@{}\", position = \"{}x{}\", \
                 scale = {scale} }})",
                hypr[0].name,
                first.geometry.pixel_size.width,
                first.geometry.pixel_size.height,
                first.refresh_millihz / 1000,
                first.geometry.logical_origin.x,
                first.geometry.logical_origin.y,
            ))
            .unwrap();
        };
        rescale(1.6);
        let scaled = wait_for(&rx, |l| {
            l.len() == displays.len() && (l[0].geometry.scale - 1.6).abs() < 0.001
        });
        eprintln!("at scale 1.6: {scaled:#?}");
        assert_eq!(scaled[0].geometry.pixel_size, first.geometry.pixel_size);
        rescale(first.geometry.scale);
        wait_for(&rx, |l| l == displays.as_slice());

        let start = Instant::now();
        drop(outputs);
        assert!(start.elapsed() < JOIN_WAIT, "drop was not prompt");
    }
}
