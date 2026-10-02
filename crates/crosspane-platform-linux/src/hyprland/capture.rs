//! Native objects live on a dedicated Wayland thread. Abort wakes a separate shutdown/delivery
//! thread holding an independent duplicate of the Wayland socket. It shuts down that connection
//! without touching proxies, dispatch, or Wayland backend mutexes, even if dispatch is stuck.
//! Subscription delivery is serialised and generation-fenced there, including `Aborted`.
//! The worker reconnects and rebuilds portals after abort, with bounded backoff. No unsafe code is
//! needed. Every socket owner shuts down the underlying connection on failure or unwinding:
//! shutdown affects every duplicated descriptor, unlike merely dropping a Connection.
//!
//! **`set_portals` (WP-2.43d).** Strips whose portal is unchanged are kept across a replacement
//! (the same layer surface, its edge state and an active capture on it); only new portals get
//! strips and only absent portals lose them. The monitor cache is refreshed first when it is
//! stale, also during a capture. The backend ends a capture `Lost` only when its own strip goes
//! away (its portal removed or changed, its output removed, or its output's id or scale changed).
//! `Ok` means every requested strip is mapped.
//!
//! **Output removal (Hyprland 0.56.2, an OS fact).** Removing *any* output ends an active
//! capture: the compositor relocates the cursor and drops the pointer lock, and the backend
//! reports that as `Lost` (`zwp_locked_pointer_v1.unlocked`). The backend never reacquires the
//! lock to hide it. The backend itself must not end a capture for an unrelated output's removal:
//! it destroys the strips on the removed output and leaves the capture alone. Why a capture ended
//! is recorded per connection (the backend's own decision against the compositor's doing) and the
//! nested tests read it through `end_causes_for_test`.
//!
//! **Why a failed `set_portals` failed (WP-2.43 B1).** The platform trait has one error type, so the
//! two situations the engine must tell apart are told apart by value (see
//! [`set_portals_failure`]): a worker-reported rejection is a [`PlatformError::Backend`] whose
//! message starts with [`PORTALS_REJECTED`] and leaves the previous set and any capture intact;
//! everything else (the caller's bare [`PlatformError::Timeout`], a worker that is gone, an abort,
//! a lost connection) leaves their fate unknown.
//!
//! **Local activity (WP-1.43, [`InputCapture::set_monitor_local_activity`]).** Hyprland gives
//! clients no per-device input stream, so while monitoring is on a thread of its own (it uses no
//! Wayland connection and none of the capture machinery above) reads the pointer over the
//! compositor's IPC every 100 ms and reports [`CaptureEvent::LocalActivity`] when the pointer is
//! somewhere the pointer injector did not put it: the injector records every absolute position it
//! injects ([`super::inject`]), and the monitor reports when the pointer has moved since its
//! previous reading, is more than 3 device pixels (or a display) away from the last injected
//! position, and the last injection is older than 150 ms, at most once per 500 ms. Injected motion
//! therefore never counts, so the device already driving a session cannot take it back. It stops
//! on `false`, on drop and when the compositor goes away, and never reads while monitoring is off.
//! **Keyboard-only local input is not detected**: best effort, pointer only. See the
//! `local_activity` module below for the exact rules.
mod events;
mod wayland;
use wayland::Refresh;

use super::inject::injected_position_for;
use super::ipc::HyprIpc;
use crosspane_platform::{
    CaptureAbort, CaptureEvent, CaptureId, CapturePortal, CaptureStart, EndReason, EventSink,
    InputCapture, IoGate, PlatformError, PortalId,
};
use crosspane_types::{geom::PointDevice, id::DisplayId, input::LockKeys};
use std::{
    ffi::OsString,
    fmt,
    net::Shutdown,
    os::unix::net::{UnixDatagram, UnixStream},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
const CALL_BUDGET: Duration = Duration::from_millis(40);
const POLL: Duration = Duration::from_millis(10);

/// The start of the message of every [`PlatformError::Backend`] that [`HyprlandCapture`]'s
/// `set_portals` returns for a **worker-reported rejection** (WP-2.43 B1): the worker refused or
/// could not finish the replacement before changing anything, so the previous portal set and any
/// active capture are intact.
pub const PORTALS_REJECTED: &str = "Hyprland capture: set_portals rejected: ";

/// What a failed [`HyprlandCapture`] `set_portals` left behind (WP-2.43 B1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SetPortalsFailure {
    /// The worker reported the failure: an invalid portal, an unknown display or output, a
    /// monitor-cache refresh or strip preparation that did not finish in time (or had no time
    /// left to start), a closed strip. The previous set stays installed and an active capture
    /// continues, and the worker verified that over the whole command: no capture ended and no
    /// installed strip was destroyed or closed while it dispatched events, the backend was not
    /// aborted, and the connection is the one the previous set was installed on.
    Rejected,
    /// The caller's receive timeout (which aborts the backend), a worker that is gone, an abort,
    /// a lost connection, a failure on a connection made for this command, a capture or installed
    /// strip lost while the command ran, or any other error: whether the previous set and the
    /// capture survive is unknown. An abort or a lost connection ends the capture with an `Ended`
    /// event.
    Uncertain,
}

/// Classify the error of a failed [`HyprlandCapture`] `set_portals`.
///
/// Only a worker-reported rejection (see [`PORTALS_REJECTED`]) is [`SetPortalsFailure::Rejected`];
/// a bare [`PlatformError::Timeout`] always comes from the caller's receive timeout, because the
/// worker reports its own timeouts as rejections. Everything else, including errors this function
/// has never seen, is [`SetPortalsFailure::Uncertain`], the conservative class.
pub fn set_portals_failure(error: &PlatformError) -> SetPortalsFailure {
    match error {
        PlatformError::Backend(message) if message.starts_with(PORTALS_REJECTED) => {
            SetPortalsFailure::Rejected
        }
        _ => SetPortalsFailure::Uncertain,
    }
}

/// A worker-reported rejection: the previous set and any capture are intact. Idempotent.
pub(super) fn rejected(error: PlatformError) -> PlatformError {
    match error {
        PlatformError::Backend(message) if message.starts_with(PORTALS_REJECTED) => {
            PlatformError::Backend(message)
        }
        PlatformError::Backend(message) => PlatformError::Backend(format!(
            "{PORTALS_REJECTED}{}",
            message
                .strip_prefix("Hyprland capture: ")
                .unwrap_or(&message)
        )),
        other => PlatformError::Backend(format!("{PORTALS_REJECTED}{other}")),
    }
}

/// Strip the rejection mark: after an abort the capture's fate is unknown whatever the worker saw.
pub(super) fn unmark(error: PlatformError) -> PlatformError {
    match error {
        PlatformError::Backend(message) => match message.strip_prefix(PORTALS_REJECTED) {
            Some(reason) => backend(format!("set_portals interrupted: {reason}")),
            None => PlatformError::Backend(message),
        },
        other => other,
    }
}

struct Socket {
    stream: UnixStream,
    lost: AtomicBool,
}
impl Socket {
    fn shutdown(&self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
    fn lost(&self) {
        self.lost.store(true, Ordering::Release);
        self.shutdown();
    }
}
// Used by both native dispatch and delivery, including constructor failures and sink panics.
pub(super) struct SocketGuard(Arc<Socket>);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}
struct WorkerGuard(Arc<Abort>);
impl Drop for WorkerGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// One compositor: its XDG runtime directory, Wayland display and Hyprland instance signature.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Endpoint {
    runtime_dir: PathBuf,
    display: String,
    signature: String,
}

/// Where a backend connects, for the Wayland socket and for Hyprland's IPC alike.
#[derive(Clone, Debug)]
enum Source {
    /// The compositor the environment names, read at every (re)connection: production.
    Environment,
    /// Exactly this compositor, whatever the environment says: the nested tests, which verified
    /// it before connecting ([`NestProof`]).
    Pinned(Endpoint),
}
impl Source {
    /// The Wayland socket to connect to.
    fn socket_path(&self) -> Result<PathBuf, PlatformError> {
        match self {
            Source::Environment => {
                let name = std::env::var_os("WAYLAND_DISPLAY")
                    .ok_or(PlatformError::Unsupported("not on Wayland"))?;
                let path = PathBuf::from(name);
                if path.is_absolute() {
                    return Ok(path);
                }
                let runtime = std::env::var_os("XDG_RUNTIME_DIR")
                    .ok_or(PlatformError::Unsupported("no Wayland runtime directory"))?;
                Ok(PathBuf::from(runtime).join(path))
            }
            Source::Pinned(endpoint) => {
                let path = PathBuf::from(&endpoint.display);
                Ok(if path.is_absolute() {
                    path
                } else {
                    endpoint.runtime_dir.join(path)
                })
            }
        }
    }
    /// The runtime directory and instance signature of the same compositor.
    fn instance(&self) -> Result<(PathBuf, String), PlatformError> {
        match self {
            Source::Environment => {
                let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
                    .map_err(|_| PlatformError::Unsupported("not under Hyprland"))?;
                let runtime = std::env::var_os("XDG_RUNTIME_DIR")
                    .ok_or(PlatformError::Unsupported("no runtime directory"))?;
                Ok((PathBuf::from(runtime), signature))
            }
            Source::Pinned(endpoint) => {
                Ok((endpoint.runtime_dir.clone(), endpoint.signature.clone()))
            }
        }
    }
    /// Hyprland's IPC for the same compositor.
    fn ipc(&self, timeout: Duration) -> Result<HyprIpc, PlatformError> {
        let (runtime, signature) = self.instance()?;
        Ok(HyprIpc::new(&signature, &runtime, timeout))
    }
    /// The instance's `hyprland.lock`, whose first line is the compositor's process id.
    fn lock_path(&self) -> Result<PathBuf, PlatformError> {
        let (runtime, signature) = self.instance()?;
        Ok(runtime.join("hypr").join(signature).join("hyprland.lock"))
    }
}

/// What the environment says about the compositors this process would reach.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct NestEndpoints {
    pub runtime_dir: PathBuf,
    /// `wayland_client::Connection::connect_to_env` prefers an inherited `WAYLAND_SOCKET` (a file
    /// descriptor) over `WAYLAND_DISPLAY`.
    pub wayland_socket: Option<OsString>,
    pub wayland_display: Option<String>,
    pub signature: Option<String>,
}
impl NestEndpoints {
    /// The environment of this process.
    pub fn from_process() -> NestEndpoints {
        NestEndpoints {
            runtime_dir: PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_default()),
            wayland_socket: std::env::var_os("WAYLAND_SOCKET"),
            wayland_display: std::env::var("WAYLAND_DISPLAY").ok(),
            signature: std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok(),
        }
    }
}

/// Proof that one compositor endpoint (runtime directory, Wayland display and instance signature)
/// belongs to a running nest that `scripts/hypr-nested.sh` started. **Only [`verify_nest`] makes
/// one**: the field is private, so nothing outside this module can build a proof. It authorises
/// the test hooks of the one backend that [`HyprlandCapture::new_for_nest_test`] connects to
/// exactly this endpoint with it; a backend made by [`HyprlandCapture::new`] has no proof and
/// every hook refuses it.
///
/// ```compile_fail,E0451
/// use crosspane_platform_linux::hyprland::capture::NestProof;
/// // The field is private (E0451): no proof can be written by hand outside this crate. (The
/// // repository runs its tests with nextest, which skips doc tests: run this one with
/// // `cargo test -p crosspane-platform-linux --doc`.)
/// let _ = NestProof { endpoint: panic!() };
/// ```
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct NestProof {
    endpoint: Endpoint,
}

/// Verify that `e` names a running nest started by `scripts/hypr-nested.sh`, and mint the proof
/// for exactly that endpoint (the same guard as WP-2.43f's `tests/home_bind.rs`). It reads files
/// only and connects to nothing; callers run it **before** connecting.
///
/// 1. `WAYLAND_SOCKET` must be unset.
/// 2. Hyprland's own `hyprland.lock` for the signature (its pid and its Wayland socket name) must
///    name the `WAYLAND_DISPLAY` we would connect to.
/// 3. `$XDG_RUNTIME_DIR/crosspane-hypr-<name>/` (written by `scripts/hypr-nested.sh start`) must
///    record the same signature and Wayland socket, and the pid of that same, still running
///    compositor. The live session has no such directory, so its signature is refused.
#[doc(hidden)]
pub fn verify_nest(e: &NestEndpoints) -> Result<NestProof, String> {
    if let Some(fd) = &e.wayland_socket {
        return Err(format!(
            "WAYLAND_SOCKET={fd:?} is set; wayland-client would connect to that inherited socket \
             instead of WAYLAND_DISPLAY. Use `eval \"$(scripts/hypr-nested.sh env)\"`, which \
             unsets it"
        ));
    }
    if e.runtime_dir.as_os_str().is_empty() {
        return Err("XDG_RUNTIME_DIR is not set".into());
    }
    let display = e
        .wayland_display
        .as_deref()
        .filter(|d| !d.is_empty())
        .ok_or("WAYLAND_DISPLAY is not set")?;
    let signature = e
        .signature
        .as_deref()
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
        .ok_or("HYPRLAND_INSTANCE_SIGNATURE is not set or not a plain signature")?;
    let lock_path = e
        .runtime_dir
        .join("hypr")
        .join(signature)
        .join("hyprland.lock");
    let lock = std::fs::read_to_string(&lock_path)
        .map_err(|err| format!("can't read {}: {err}", lock_path.display()))?;
    let mut lock = lock.lines();
    let lock_pid = lock.next().unwrap_or_default().trim();
    let lock_display = lock.next().unwrap_or_default().trim();
    if lock_pid.is_empty()
        || !lock_pid.bytes().all(|b| b.is_ascii_digit())
        || lock_display != display
    {
        return Err(format!(
            "instance {signature} serves {lock_display:?}, but WAYLAND_DISPLAY is {display:?}"
        ));
    }
    let states = std::fs::read_dir(&e.runtime_dir)
        .map_err(|err| format!("can't list {}: {err}", e.runtime_dir.display()))?;
    for state in states.flatten() {
        let is_nest_dir = state
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with("crosspane-hypr-"));
        if !is_nest_dir {
            continue;
        }
        let env = std::fs::read_to_string(state.path().join("env")).unwrap_or_default();
        let pid = std::fs::read_to_string(state.path().join("pid")).unwrap_or_default();
        let exported = |name: &str| {
            env.lines().find_map(|line| {
                line.strip_prefix("export ")?
                    .strip_prefix(name)?
                    .strip_prefix('=')
            })
        };
        if exported("HYPRLAND_INSTANCE_SIGNATURE") == Some(signature)
            && exported("WAYLAND_DISPLAY") == Some(display)
            && pid.trim() == lock_pid
            && std::path::Path::new("/proc").join(lock_pid).exists()
        {
            return Ok(NestProof {
                endpoint: Endpoint {
                    runtime_dir: e.runtime_dir.clone(),
                    display: display.to_owned(),
                    signature: signature.to_owned(),
                },
            });
        }
    }
    Err(format!(
        "no running nest started by scripts/hypr-nested.sh owns instance {signature} on {display} \
         (is this the live session?)"
    ))
}

/// What the test hooks check against the process environment when they are called.
struct HookEnvironment {
    flag: bool,
    wayland_socket: bool,
    runtime_dir: Option<PathBuf>,
    display: Option<String>,
    signature: Option<String>,
}
impl HookEnvironment {
    fn from_process() -> HookEnvironment {
        HookEnvironment {
            flag: std::env::var("CROSSPANE_NESTED_HYPR").as_deref() == Ok("1"),
            wayland_socket: std::env::var_os("WAYLAND_SOCKET").is_some(),
            runtime_dir: std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
            display: std::env::var("WAYLAND_DISPLAY").ok(),
            signature: std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok(),
        }
    }
}

/// Whether the test hooks may act on a backend: only one that was built through
/// [`HyprlandCapture::new_for_nest_test`] with a [`NestProof`] (`nest` is that proof's endpoint,
/// which is also the endpoint it is connected to), and only while the environment still names
/// exactly that endpoint, has the nested flag and carries no inherited `WAYLAND_SOCKET`. A backend
/// built by the production constructor (`nest` is `None`) is refused whatever the environment says.
fn authorise(nest: Option<&Endpoint>, env: &HookEnvironment) -> Result<(), PlatformError> {
    let refused = Err(PlatformError::Unsupported("nested capture test hook"));
    let Some(nest) = nest else {
        return refused;
    };
    if env.flag
        && !env.wayland_socket
        && env.runtime_dir.as_deref() == Some(nest.runtime_dir.as_path())
        && env.display.as_deref() == Some(nest.display.as_str())
        && env.signature.as_deref() == Some(nest.signature.as_str())
    {
        Ok(())
    } else {
        refused
    }
}

/// Command handle for Hyprland's layer-shell capture adapter.
pub struct HyprlandCapture {
    commands: mpsc::Sender<Command>,
    abort: Arc<Abort>,
    /// The verified nest endpoint this backend is connected to, if it was built for a nested test
    /// ([`Self::new_for_nest_test`]). `None` for every production backend: the test hooks refuse it.
    nest: Option<Endpoint>,
    /// Local-activity monitoring (WP-1.43): the gate that links this backend to the pointer
    /// injector, the compositor to poll, the subscriber to report to, and the running monitor.
    /// The monitor is stopped and joined when the handle drops (after `drop` has aborted the
    /// capture, so it never delays that).
    gate: Arc<IoGate>,
    source: Source,
    sink: Option<Arc<dyn EventSink<CaptureEvent>>>,
    activity_delivery: mpsc::Sender<Delivery>,
    monitor: Option<local_activity::Monitor>,
    #[cfg(test)]
    monitor_source: Option<local_activity::SourceFactory>,
}
impl fmt::Debug for HyprlandCapture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HyprlandCapture").finish_non_exhaustive()
    }
}
#[derive(Debug)]
struct Abort {
    epoch: AtomicU64,
    next_capture: AtomicU64,
    acknowledged: AtomicU64,
    next_barrier: AtomicU64,
    barrier_reached: AtomicU64,
    wake: UnixDatagram,
}
impl CaptureAbort for Abort {
    fn abort(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        // No capture-thread locks or waiting. A full wake socket is harmless: the independent
        // shutdown thread checks the authoritative cancellation epoch at least every 10 ms.
        let _ = self.wake.send(&[1]);
    }
}
impl Abort {
    fn send_packet(
        &self,
        epoch: u64,
        packet: &[u8; events::SIZE],
        deadline: Option<Instant>,
    ) -> Result<(), PlatformError> {
        loop {
            if epoch != self.epoch.load(Ordering::Acquire) {
                return Err(backend("capture cancelled"));
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Err(PlatformError::Timeout);
            }
            match self.wake.send(packet) {
                Ok(_) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_micros(100))
                }
                Err(e) => return Err(backend(e)),
            }
        }
    }
}

pub(super) enum Delivery {
    Connection {
        epoch: u64,
        socket: SocketGuard,
        ready: mpsc::Sender<()>,
    },
    Subscribe {
        sink: Arc<dyn EventSink<CaptureEvent>>,
        locks: LockKeys,
        ready: mpsc::Sender<()>,
    },
    LocalActivity {
        at: crosspane_types::time::MonoTime,
        generation: Arc<local_activity::Generation>,
    },
}
/// Controlled interference with one `set_portals`, for the nested-compositor tests
/// ([`HyprlandCapture::set_portals_for_test`]). Everything is off by default.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PortalsTestHooks {
    /// Force a monitor-cache refresh with this bound (zero: it times out at once).
    pub refresh_bound: Option<Duration>,
    /// With `refresh_bound`: sit on the freshly read list this long before publishing it.
    pub refresh_stall: Duration,
    /// Hold configure and sync acknowledgements back this long, so the call stays pending.
    pub ack_delay: Duration,
    /// The worker sleeps until only this much of the call budget is left before it starts the
    /// command (a command that queued behind slow work).
    pub leave: Option<Duration>,
}
enum Operation {
    /// The replacement set, with the test hooks (all off for the trait's `set_portals`).
    Portals(Vec<CapturePortal>, PortalsTestHooks),
    Subscribe(Arc<dyn EventSink<CaptureEvent>>),
    Begin(CaptureId, PortalId),
    End(Option<(DisplayId, PointDevice)>),
    InjectWorkerError,
    /// Nested tests: why this connection ended captures.
    Causes,
    /// Nested tests: the backend's handling of an output's removal, without the removal.
    RemoveOutput(String),
    Stop,
}
enum Reply {
    Done,
    Started(CaptureStart),
    Causes(Vec<String>),
}
struct Command {
    operation: Operation,
    deadline: Instant,
    epoch: u64,
    reply: mpsc::Sender<Result<Reply, PlatformError>>,
}
impl HyprlandCapture {
    fn local_activity_source(&mut self) -> Result<local_activity::CursorSource, PlatformError> {
        #[cfg(test)]
        if let Some(factory) = &mut self.monitor_source {
            return Ok(factory());
        }
        let ipc = self.source.ipc(local_activity::IPC_TIMEOUT)?;
        let lock = self.source.lock_path()?;
        Ok(local_activity::CursorSource {
            reader: local_activity::cursor_reader(ipc),
            alive: local_activity::compositor_alive(lock),
            clock: Arc::new(local_activity::SystemClock::default()),
        })
    }
    /// Connect to `$WAYLAND_DISPLAY`; strips are created per portal on `set_portals`. The test
    /// hooks refuse a backend made here, whatever the environment says.
    pub fn new(gate: Arc<IoGate>) -> Result<Self, PlatformError> {
        Self::build(gate, Source::Environment, None)
    }
    /// Connect to exactly the nest that `proof` was made for ([`verify_nest`], which the caller
    /// ran before this), whatever the environment says now, and authorise the test hooks for this
    /// backend. Nested tests only.
    #[doc(hidden)]
    pub fn new_for_nest_test(gate: Arc<IoGate>, proof: &NestProof) -> Result<Self, PlatformError> {
        Self::build(
            gate,
            Source::Pinned(proof.endpoint.clone()),
            Some(proof.endpoint.clone()),
        )
    }
    fn build(
        gate: Arc<IoGate>,
        source: Source,
        nest: Option<Endpoint>,
    ) -> Result<Self, PlatformError> {
        let (wake, receive) = UnixDatagram::pair().map_err(backend)?;
        wake.set_nonblocking(true).map_err(backend)?;
        receive.set_nonblocking(true).map_err(backend)?;
        let abort = Arc::new(Abort {
            epoch: AtomicU64::new(0),
            next_capture: AtomicU64::new(0),
            acknowledged: AtomicU64::new(0),
            next_barrier: AtomicU64::new(0),
            barrier_reached: AtomicU64::new(0),
            wake,
        });
        let (delivery, events) = mpsc::channel();
        let control = abort.clone();
        std::thread::Builder::new()
            .name("hypr-capture-shutdown".into())
            .spawn(move || deliver(events, receive, control))
            .map_err(backend)?;
        let (commands, requests) = mpsc::channel();
        let (ready, initialized) = mpsc::channel();
        let control = abort.clone();
        let (monitor_gate, monitor_source) = (gate.clone(), source.clone());
        let activity_delivery = delivery.clone();
        std::thread::Builder::new()
            .name("hypr-capture".into())
            .spawn(move || worker(requests, delivery, gate, control, ready, source))
            .map_err(backend)?;
        let handle = Self {
            commands,
            abort,
            nest,
            gate: monitor_gate,
            source: monitor_source,
            sink: None,
            activity_delivery,
            monitor: None,
            #[cfg(test)]
            monitor_source: None,
        };
        initialized
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| PlatformError::Timeout)??;
        Ok(handle)
    }
    /// The test hooks are authorised by how the backend was built, not by its environment: only a
    /// backend made by [`Self::new_for_nest_test`] from a [`NestProof`] passes, and only while the
    /// environment still names exactly the endpoint it is connected to ([`authorise`]).
    fn authorised(&self) -> Result<(), PlatformError> {
        authorise(self.nest.as_ref(), &HookEnvironment::from_process())
    }
    /// Inject a recoverable dispatch failure for the nested-compositor regression test.
    #[doc(hidden)]
    pub fn inject_worker_error_for_test(&self) -> Result<(), PlatformError> {
        self.authorised()?;
        self.call(Operation::InjectWorkerError).map(|_| ())
    }
    /// `set_portals` with the caller's budget and controlled interference ([`PortalsTestHooks`]),
    /// for the nested-compositor tests of the error classes ([`set_portals_failure`]): a zero
    /// `budget` makes the caller time out (and abort); a zero `refresh_bound` makes the worker's
    /// refresh time out and report a rejection.
    #[doc(hidden)]
    pub fn set_portals_for_test(
        &mut self,
        portals: &[CapturePortal],
        budget: Duration,
        hooks: PortalsTestHooks,
    ) -> Result<(), PlatformError> {
        self.authorised()?;
        self.call_with_budget(Operation::Portals(portals.to_vec(), hooks), budget)
            .map(|_| ())
    }
    /// Run the backend's handling of the removal of the output named `name` (a monitor name)
    /// without removing it from the compositor, so that only the backend's own decision is
    /// observed: the capture ends only if its strip is on that output, and the strips on it are
    /// destroyed. Nested tests only.
    #[doc(hidden)]
    pub fn output_removal_for_test(&mut self, name: &str) -> Result<(), PlatformError> {
        self.authorised()?;
        self.call(Operation::RemoveOutput(name.to_owned()))
            .map(|_| ())
    }
    /// Why this connection ended captures, oldest first (`StripReplaced` and `OutputRemoved` are
    /// the backend's own decisions; `Unlocked`, `PointerLeft`, `StripClosed`... are the
    /// compositor's doing). Nested tests only.
    #[doc(hidden)]
    pub fn end_causes_for_test(&self) -> Result<Vec<String>, PlatformError> {
        self.authorised()?;
        match self.call(Operation::Causes)? {
            Reply::Causes(causes) => Ok(causes),
            _ => Err(backend("invalid capture response")),
        }
    }
    fn call(&self, operation: Operation) -> Result<Reply, PlatformError> {
        self.call_with_budget(operation, CALL_BUDGET)
    }
    fn call_with_budget(
        &self,
        operation: Operation,
        budget: Duration,
    ) -> Result<Reply, PlatformError> {
        let deadline = Instant::now() + budget;
        let (reply, result) = mpsc::channel();
        let epoch = self.abort.epoch.load(Ordering::Acquire);
        self.commands
            .send(Command {
                operation,
                deadline,
                epoch,
                reply,
            })
            .map_err(|_| backend("capture worker unavailable"))?;
        match result.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(result) => {
                if result.is_err() && epoch != self.abort.epoch.load(Ordering::Acquire) {
                    self.wait_shutdown(deadline);
                    // Aborted meanwhile: the worker's own verdict no longer says what survived.
                    return result.map_err(unmark);
                }
                result
            }
            Err(_) => {
                // Reserve 10 ms of the shared 50 ms bound for independent emergency rollback.
                self.abort.abort();
                self.wait_shutdown(deadline);
                Err(PlatformError::Timeout)
            }
        }
    }
    fn wait_shutdown(&self, deadline: Instant) {
        let epoch = self.abort.epoch.load(Ordering::Acquire);
        let rollback_deadline = deadline + Duration::from_millis(9);
        while self.abort.acknowledged.load(Ordering::Acquire) < epoch
            && Instant::now() < rollback_deadline
        {
            std::thread::sleep(Duration::from_micros(100));
        }
    }
}
impl InputCapture for HyprlandCapture {
    /// Replace the strips (module docs). Unchanged portals keep their strips, so a capture on one
    /// survives; a capture whose own strip is removed or changed ends `EndReason::Lost`. On failure
    /// [`set_portals_failure`] says whether the previous set and the capture are known intact.
    fn set_portals(&mut self, portals: &[CapturePortal]) -> Result<(), PlatformError> {
        self.call(Operation::Portals(
            portals.to_vec(),
            PortalsTestHooks::default(),
        ))
        .map(|_| ())
    }
    fn subscribe(&mut self, sink: Arc<dyn EventSink<CaptureEvent>>) -> Result<(), PlatformError> {
        self.call(Operation::Subscribe(sink.clone()))?;
        // The local-activity monitor reports to the same subscriber.
        self.sink = Some(sink);
        Ok(())
    }
    fn begin(&mut self, id: CaptureId, portal: PortalId) -> Result<CaptureStart, PlatformError> {
        match self.call(Operation::Begin(id, portal))? {
            Reply::Started(start) => Ok(start),
            Reply::Done | Reply::Causes(_) => Err(backend("invalid capture response")),
        }
    }
    fn end(&mut self, warp: Option<(DisplayId, PointDevice)>) -> Result<(), PlatformError> {
        self.call(Operation::End(warp)).map(|_| ())
    }
    fn abort_handle(&self) -> Arc<dyn CaptureAbort> {
        self.abort.clone()
    }
    /// Module docs, WP-1.43. `false` stops and joins the monitor (within about 50 ms: a reading
    /// is two IPC requests of 20 ms at most) and is always `Ok`; `true` starts it, or leaves a
    /// running one alone. Needs a subscriber, and a pointer injector built with the same gate:
    /// without one the monitor could not tell injected motion from the owner's, so it refuses
    /// ([`PlatformError::Unsupported`]) instead of ever reporting injected motion as local.
    fn set_monitor_local_activity(&mut self, on: bool) -> Result<(), PlatformError> {
        if !on {
            if let Some(mut monitor) = self.monitor.take() {
                monitor.stop();
            }
            return Ok(());
        }
        if self
            .monitor
            .as_ref()
            .is_some_and(local_activity::Monitor::is_running)
        {
            return Ok(());
        }
        // One that stopped by itself (its compositor went away) is replaced.
        if let Some(mut finished) = self.monitor.take() {
            finished.stop();
        }
        if self.sink.is_none() {
            return Err(backend("local activity monitoring needs a subscriber"));
        }
        let injected = injected_position_for(&self.gate).ok_or(PlatformError::Unsupported(
            "Hyprland local activity monitoring needs the pointer injector",
        ))?;
        let source = self.local_activity_source()?;
        // The same delivery thread sends every subscription event. The monitor never calls the
        // subscriber concurrently with Wayland dispatch or the independent abort path.
        let delivery = self.activity_delivery.clone();
        let report: local_activity::Report = Box::new(move |generation, at| {
            let _ = delivery.send(Delivery::LocalActivity { at, generation });
        });
        self.monitor = Some(local_activity::Monitor::start_source(
            source, injected, report,
        )?);
        Ok(())
    }
}
impl Drop for HyprlandCapture {
    fn drop(&mut self) {
        self.abort.abort();
        if let Some(mut monitor) = self.monitor.take() {
            monitor.stop();
        }
        let (reply, _) = mpsc::channel();
        let _ = self.commands.send(Command {
            operation: Operation::Stop,
            deadline: Instant::now(),
            epoch: self.abort.epoch.load(Ordering::Acquire),
            reply,
        });
        // Never join the potentially stuck Wayland thread; independent shutdown releases input.
    }
}
/// Run one `set_portals` command on `client`, with the test hooks (all off in production).
fn install(
    client: &mut wayland::Client,
    portals: &[CapturePortal],
    deadline: Instant,
    hooks: &PortalsTestHooks,
) -> Result<(), PlatformError> {
    let refresh = match hooks.refresh_bound {
        Some(bound) => Refresh::Forced {
            bound,
            stall: hooks.refresh_stall,
        },
        None => Refresh::Required,
    };
    client.set_ack_delay(hooks.ack_delay);
    let result = client.set_portals(portals, deadline, refresh);
    client.set_ack_delay(Duration::ZERO);
    result
}
fn worker(
    commands: mpsc::Receiver<Command>,
    delivery: mpsc::Sender<Delivery>,
    gate: Arc<IoGate>,
    abort: Arc<Abort>,
    ready: mpsc::Sender<Result<(), PlatformError>>,
    source: Source,
) {
    let _unwind = WorkerGuard(abort.clone());
    let initialized = wayland::Client::new(
        gate.clone(),
        abort.clone(),
        delivery.clone(),
        Instant::now() + Duration::from_millis(1900),
        None,
        &source,
    );
    let mut client = match initialized {
        Ok(client) => {
            let _ = ready.send(Ok(()));
            Some(client)
        }
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let mut monitors = client
        .as_ref()
        .map(|c| c.monitor_cache())
        .unwrap_or_default();
    let mut portals = Vec::new();
    let mut subscribed = false;
    let mut backoff = Duration::from_millis(50);
    let mut reconnect_at = Instant::now() + backoff;
    loop {
        if let Some(c) = client.as_mut()
            && c.pump(POLL).is_err()
        {
            c.disconnected();
            client = None;
            backoff = Duration::from_millis(50);
            reconnect_at = Instant::now() + backoff;
        }
        let command = match commands.recv_timeout(if client.is_some() {
            Duration::ZERO
        } else {
            POLL
        }) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if client.is_none() && !portals.is_empty() && Instant::now() >= reconnect_at {
                    let deadline = Instant::now() + CALL_BUDGET;
                    let fresh = wayland::Client::new(
                        gate.clone(),
                        abort.clone(),
                        delivery.clone(),
                        deadline,
                        Some(monitors.clone()),
                        &source,
                    )
                    .and_then(|mut c| {
                        // Portals whose output has gone since are left out, not fatal.
                        let installed = c.rebuild_portals(&portals, deadline)?;
                        Ok((c, installed))
                    });
                    match fresh {
                        Ok((c, installed)) => {
                            client = Some(c);
                            portals = installed;
                            backoff = Duration::from_millis(50);
                        }
                        Err(_) => backoff = (backoff * 2).min(Duration::from_secs(1)),
                    }
                    reconnect_at = Instant::now() + backoff;
                } else if let Some(c) = client.as_mut() {
                    // Refresh a stale topology outside the caller's command budget. This also runs
                    // while a capture is active (WP-2.43d): a twin output that appears during one
                    // must be known by the time `end` warps onto it. A failed refresh is retried
                    // at a bounded rate, so it can't starve the capture's own dispatch.
                    c.refresh_monitors();
                    monitors = c.monitor_cache();
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        if matches!(command.operation, Operation::Stop) {
            return;
        }
        let result = (|| {
            if let Operation::Portals(_, hooks) = &command.operation
                && let Some(leave) = hooks.leave
            {
                // A command that queued behind slow work (nested tests).
                let start = command
                    .deadline
                    .checked_sub(leave)
                    .unwrap_or(command.deadline);
                std::thread::sleep(start.saturating_duration_since(Instant::now()));
            }
            if Instant::now() >= command.deadline {
                return Err(PlatformError::Timeout);
            }
            if command.epoch != abort.epoch.load(Ordering::Acquire) {
                return Err(backend("capture operation cancelled"));
            }
            if matches!(command.operation, Operation::Begin(..)) && !gate.is_open() {
                return Err(PlatformError::Locked);
            }
            if client.is_none() {
                let mut fresh = wayland::Client::new(
                    gate.clone(),
                    abort.clone(),
                    delivery.clone(),
                    command.deadline,
                    Some(monitors.clone()),
                    &source,
                )?;
                // Nothing of the previous connection survives on this one: whatever fails now says
                // nothing about a previous set or capture, so it is never a rejection.
                if let Operation::Portals(new, hooks) = &command.operation {
                    // The replacement supersedes the cached set: install it directly. Rebuilding
                    // the old one first could fail on an output that has gone since, and then no
                    // replacement (not even an empty one) would ever be tried.
                    install(&mut fresh, new, command.deadline, hooks).map_err(unmark)?;
                    portals = new.clone();
                    client = Some(fresh);
                    return Ok(Reply::Done);
                }
                portals = fresh
                    .rebuild_portals(&portals, command.deadline)
                    .map_err(unmark)?;
                client = Some(fresh);
            }
            let c = client.as_mut().ok_or(PlatformError::NotFound)?;
            match command.operation {
                Operation::Portals(new, hooks) => {
                    install(c, &new, command.deadline, &hooks)?;
                    portals = new;
                    Ok(Reply::Done)
                }
                Operation::Causes => Ok(Reply::Causes(c.end_causes())),
                Operation::RemoveOutput(name) => {
                    c.simulate_output_removal(&name).map(|()| Reply::Done)
                }
                Operation::Subscribe(sink) => {
                    if subscribed {
                        return Err(backend("capture subscribe called twice"));
                    }
                    let (ready, result) = mpsc::channel();
                    delivery
                        .send(Delivery::Subscribe {
                            sink,
                            locks: c.locks(),
                            ready,
                        })
                        .map_err(|_| backend("capture delivery unavailable"))?;
                    let _ = abort.wake.send(&[1]);
                    result
                        .recv_timeout(command.deadline.saturating_duration_since(Instant::now()))
                        .map_err(|_| PlatformError::Timeout)?;
                    subscribed = true;
                    Ok(Reply::Done)
                }
                Operation::Begin(id, portal) => {
                    if !subscribed {
                        return Err(backend("capture is not subscribed"));
                    }
                    c.begin(id, portal, command.deadline).map(Reply::Started)
                }
                Operation::End(warp) => c.end(warp, command.deadline).map(|_| Reply::Done),
                Operation::InjectWorkerError => {
                    c.inject_worker_error();
                    Ok(Reply::Done)
                }
                Operation::Stop => Ok(Reply::Done),
            }
        })();
        let _ = command.reply.send(result);
    }
}
fn deliver(events: mpsc::Receiver<Delivery>, wake: UnixDatagram, abort: Arc<Abort>) {
    let mut socket: Option<(u64, SocketGuard)> = None;
    let mut sink: Option<Arc<dyn EventSink<CaptureEvent>>> = None;
    let mut active: Option<(u64, u64, CaptureId)> = None;
    let mut ended: Option<(u64, u64)> = None;
    loop {
        let epoch = abort.epoch.load(Ordering::Acquire);
        if socket
            .as_ref()
            .is_some_and(|(e, stream)| *e != epoch || stream.0.lost.load(Ordering::Acquire))
        {
            let reason = if socket.as_ref().is_some_and(|(e, _)| *e != epoch) {
                EndReason::Aborted
            } else {
                EndReason::Lost
            };
            if let Some((cancelled, stream)) = socket.take() {
                stream.0.shutdown();
                // Input is free already. Deliver packets committed before the abort wakeup in
                // FIFO order, so discrete events and unaccelerated samples are not discarded.
                // The nonblocking receive stops at the wakeup marker (or at an empty socket if
                // its send buffer was full); it never waits for the capture producer.
                let mut packet = [0; events::SIZE];
                while let Ok(length) = wake.recv(&mut packet) {
                    if length != events::SIZE {
                        break;
                    }
                    match events::decode(&packet) {
                        Some(events::Packet::Event {
                            epoch: e,
                            generation,
                            event,
                        }) if e == cancelled => forward(
                            cancelled,
                            e,
                            generation,
                            event,
                            &sink,
                            &mut active,
                            &mut ended,
                        ),
                        Some(events::Packet::Barrier(token)) => {
                            abort.barrier_reached.fetch_max(token, Ordering::Release);
                        }
                        _ => (),
                    }
                }
            }
            if let Some((e, generation, id)) = active.take() {
                if let Some(sink) = &sink {
                    sink.send(CaptureEvent::Ended { id, reason });
                }
                ended = Some((e, generation));
            }
        }
        abort.acknowledged.store(epoch, Ordering::Release);
        // The control channel is only used before capture: registration and the one subscribe.
        // In particular no event/activation acknowledgement can strand this thread on a channel
        // slot that a stuck capture thread has reserved but not filled.
        let message = if active.is_none() {
            events.try_recv()
        } else {
            Err(mpsc::TryRecvError::Empty)
        };
        let message = match message {
            Ok(message) => Some(message),
            Err(mpsc::TryRecvError::Disconnected) => {
                if let Some((_, stream)) = socket.take() {
                    stream.0.shutdown();
                }
                return;
            }
            Err(mpsc::TryRecvError::Empty) => None,
        };
        if let Some(message) = message {
            match message {
                Delivery::Connection {
                    epoch: e,
                    socket: stream,
                    ready,
                } => {
                    if e != abort.epoch.load(Ordering::Acquire) {
                        stream.0.shutdown();
                    } else {
                        socket = Some((e, stream));
                    }
                    let _ = ready.send(());
                }
                Delivery::Subscribe {
                    sink: new,
                    locks,
                    ready,
                } => {
                    if locks.caps_lock.is_some()
                        || locks.num_lock.is_some()
                        || locks.scroll_lock.is_some()
                    {
                        new.send(CaptureEvent::LockKeys(locks));
                    }
                    new.send(CaptureEvent::KeyboardBlinded(false));
                    sink = Some(new);
                    let _ = ready.send(());
                }
                Delivery::LocalActivity { at, generation } => {
                    generation.deliver(at, &sink);
                }
            }
        }
        let mut packet = [0; events::SIZE];
        let length = match wake.recv(&mut packet) {
            Ok(length) => length,
            Err(_) => {
                let mut fd = [rustix::event::PollFd::new(
                    &wake,
                    rustix::event::PollFlags::IN,
                )];
                let timeout = rustix::event::Timespec::try_from(POLL).ok();
                let _ = rustix::event::poll(&mut fd, timeout.as_ref());
                continue;
            }
        };
        if length != events::SIZE {
            continue;
        } // A one-byte packet is just an abort wakeup.
        match events::decode(&packet) {
            Some(events::Packet::Barrier(token)) => {
                abort.barrier_reached.fetch_max(token, Ordering::Release);
            }
            Some(events::Packet::Event {
                epoch: e,
                generation,
                event,
            }) => {
                forward(
                    abort.epoch.load(Ordering::Acquire),
                    e,
                    generation,
                    event,
                    &sink,
                    &mut active,
                    &mut ended,
                );
            }
            None => (),
        }
    }
}
fn forward(
    current: u64,
    e: u64,
    generation: u64,
    event: CaptureEvent,
    sink: &Option<Arc<dyn EventSink<CaptureEvent>>>,
    active: &mut Option<(u64, u64, CaptureId)>,
    ended: &mut Option<(u64, u64)>,
) {
    let Some(sink) = sink else {
        return;
    };
    match event {
        CaptureEvent::Started { id } => {
            if *ended == Some((e, generation)) {
                return;
            }
            sink.send(CaptureEvent::Started { id });
            if e != current {
                sink.send(CaptureEvent::Ended {
                    id,
                    reason: EndReason::Aborted,
                });
                *ended = Some((e, generation));
            } else {
                *active = Some((e, generation, id));
            }
        }
        CaptureEvent::Ended { id, reason } => {
            if *active == Some((e, generation, id)) {
                sink.send(CaptureEvent::Ended { id, reason });
                *active = None;
                *ended = Some((e, generation));
            }
        }
        CaptureEvent::Motion { .. }
        | CaptureEvent::Key { .. }
        | CaptureEvent::Button { .. }
        | CaptureEvent::Scroll { .. } => {
            if active.is_some_and(|(epoch, g, _)| (epoch, g) == (e, generation)) && e == current {
                sink.send(event);
            }
        }
        _ if e == current => sink.send(event),
        _ => (),
    }
}

pub(super) fn backend(error: impl fmt::Display) -> PlatformError {
    PlatformError::Backend(format!("Hyprland capture: {error}"))
}

/// Local pointer activity on a target, found by **cursor divergence** (WP-1.43).
///
/// Hyprland exposes no per-device input stream to clients, so the owner's own mouse is told from
/// injected motion by where the pointer is. While monitoring is on, a thread reads the pointer
/// ([`cursor_position`](super::super::cursor::cursor_position): two short-lived IPC requests) and
/// compares it with what the pointer injector recorded ([`InjectedPosition`]). It reports
/// [`CaptureEvent::LocalActivity`] ([`is_local_activity`]) when **all** of these hold:
///
/// - the pointer **moved since the previous successful reading**, or a move beyond the cap is
///   pending from settling (the first reading only sets a baseline; stale divergence reports once);
/// - it is **more than 3 device pixels** from the last injected position (Euclidean, on the same
///   display), or on **another display**, or the injector never moved it;
/// - the **last injection is older than 150 ms**, so injected motion still in flight settles
///   first;
/// - nothing was reported in the last **500 ms**.
///
/// Hyprland floors cursor coordinates and serializes output scale to two decimals. Before a
/// baseline exists, the comparison projects the injection onto possible IPC coordinates, capped
/// at eight device pixels per axis; anything beyond that cap counts as divergence. The first
/// settled reading inside this envelope becomes the baseline for that injection and geometry.
/// Subsequent readings compare with it using the normal three-device-pixel threshold, so rounding
/// uncertainty does not permanently conceal local movement. Any new injection, output geometry
/// change or monitor restart clears the baseline.
///
/// An IPC failure is logged at debug level, skipped, and never reports. The compositor's pid and
/// process start time are pinned when monitoring starts; the thread ends if that instance goes
/// away or restarts. It stops on `false` and on drop, and nothing polls while monitoring is off.
/// Each queued report carries a monitor generation: stopping invalidates it and synchronizes with
/// delivery, so an old target session cannot report into a later one.
///
/// Limits, best effort by design: **only the pointer is watched**, so keyboard-only local input is
/// not detected (04 §6, the Wayland note); anything else that moves the pointer without the
/// injector (a compositor warp, another client's warp) also counts as local; and the owner's mouse
/// is not noticed while the controller injects faster than every 150 ms. Local motion before the
/// first settled baseline can become that baseline if it remains inside the capped envelope;
/// later movement is detected. Conversely, injection rounding beyond the eight-pixel cap can
/// appear local before a baseline exists: rounded IPC cannot resolve that ambiguity.
/// IPC cannot distinguish compositor warps from physical movement, so a warp can end a
/// remote-control session with a safe return in the rare locked-pointer application case.
mod local_activity {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Condvar, Mutex, PoisonError};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use crosspane_platform::{CaptureEvent, EventSink, PlatformError};
    use crosspane_types::{geom::PointDevice, id::DisplayId, time::MonoTime};

    use super::backend;
    use crate::hyprland::cursor::{CursorProjection, CursorSample, cursor_sample};
    use crate::hyprland::inject::{InjectedPosition, Injection};
    use crate::hyprland::ipc::HyprIpc;

    /// How often the pointer is read while monitoring is on.
    pub(super) const INTERVAL: Duration = Duration::from_millis(100);
    /// The bound on each of the two requests of one reading. Together they bound how long
    /// stopping the monitor can wait for a reading in progress.
    pub(super) const IPC_TIMEOUT: Duration = Duration::from_millis(20);
    /// The pointer must be farther than this (device pixels) from the injected position.
    const DIVERGENCE: f64 = 3.0;
    /// An injection this recent may not have been applied yet.
    const SETTLE: Duration = Duration::from_millis(150);
    /// At most one report in this time.
    const REPORT_INTERVAL: Duration = Duration::from_millis(500);
    /// What `/proc/<pid>/comm` says of the compositor (the agent's own compositor watch asks the
    /// same).
    const COMPOSITOR: &str = "Hyprland";

    /// The pointer on a display, in that display's device pixels from its top-left: what the
    /// injector was asked for, or what the compositor reports.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub(super) struct Reading {
        pub(super) display: DisplayId,
        pub(super) position: PointDevice,
        layout: Option<(f64, f64)>,
        projection: Option<CursorProjection>,
    }

    impl From<Injection> for Reading {
        fn from(injection: Injection) -> Reading {
            Reading {
                display: injection.display,
                position: injection.position,
                layout: None,
                projection: None,
            }
        }
    }

    /// How long ago things happened, as of the reading being judged; `None`: never.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) struct Timings {
        pub(super) since_injection: Option<Duration>,
        pub(super) since_report: Option<Duration>,
    }

    /// Whether a reading is local activity (module docs). Pure: `injected` is the last injected
    /// position or its settled baseline (`None`: never injected), `observed` this reading and
    /// `previous` the last good one before it (`None`: this is the first, which only sets the
    /// baseline).
    pub(super) fn is_local_activity(
        injected: Option<Reading>,
        observed: Reading,
        previous: Option<Reading>,
        timings: Timings,
    ) -> bool {
        activity_with_movement(injected, observed, moved_since(previous, observed), timings)
    }

    fn activity_with_movement(
        injected: Option<Reading>,
        observed: Reading,
        moved: bool,
        timings: Timings,
    ) -> bool {
        let diverged = injected.is_none_or(|put| {
            if let (Some(projection), Some(layout)) = (put.projection, observed.layout) {
                if let Some(baseline) = put.layout {
                    put.display != observed.display
                        || projection.observation_distance(baseline, layout) > DIVERGENCE
                } else {
                    let distance = projection.injection_distance(put.position, layout);
                    // Rounded geometry can attribute an injected edge point to its neighbour.
                    // Possible injected motion inside the capped envelope does not count.
                    distance > 0.0
                        && (put.display != observed.display
                            || distance > DIVERGENCE
                            || projection.outside_injection_cap(put.position, layout))
                }
            } else {
                put.display != observed.display
                    || (put.position.x - observed.position.x)
                        .hypot(put.position.y - observed.position.y)
                        > DIVERGENCE
            }
        });
        let settled = timings.since_injection.is_none_or(|age| age > SETTLE);
        let spaced = timings
            .since_report
            .is_none_or(|age| age >= REPORT_INTERVAL);
        moved && diverged && settled && spaced
    }

    fn moved_since(previous: Option<Reading>, observed: Reading) -> bool {
        previous.is_some_and(|before| match (before.layout, observed.layout) {
            (Some(before), Some(now)) => before != now,
            _ => before.display != observed.display || before.position != observed.position,
        })
    }

    /// Reads the pointer; an `Err` is skipped (module docs).
    #[derive(Clone)]
    pub(super) struct Observation {
        reading: Reading,
        projections: Vec<CursorProjection>,
    }
    impl From<Reading> for Observation {
        fn from(reading: Reading) -> Self {
            Self {
                reading,
                projections: Vec::new(),
            }
        }
    }
    impl From<CursorSample> for Observation {
        fn from(sample: CursorSample) -> Self {
            Self {
                reading: Reading {
                    display: sample.display,
                    position: sample.position,
                    layout: Some(sample.layout),
                    projection: None,
                },
                projections: sample.projections,
            }
        }
    }
    pub(super) type Reader = Box<dyn FnMut() -> Result<Observation, PlatformError> + Send>;
    /// Whether the original compositor is still running; asked before every reading.
    pub(super) type Alive = Box<dyn Fn() -> bool + Send>;

    /// The compositor's pointer over `ipc` (whose timeout bounds each request).
    pub(super) fn cursor_reader(ipc: HyprIpc) -> Reader {
        Box::new(move || cursor_sample(&ipc).map(Observation::from))
    }

    /// Virtual time and waits let tests exercise the real enable path and production cadence.
    pub(super) trait Clock: Send + Sync {
        fn now(&self) -> Instant;
        fn wait(&self, stopped: &AtomicBool, duration: Duration) -> bool;
        fn wake(&self);
    }
    #[derive(Default)]
    pub(super) struct SystemClock {
        wait: Mutex<()>,
        ready: Condvar,
    }
    impl Clock for SystemClock {
        fn now(&self) -> Instant {
            Instant::now()
        }
        fn wait(&self, stopped: &AtomicBool, duration: Duration) -> bool {
            let lock = self.wait.lock().unwrap_or_else(PoisonError::into_inner);
            let _waited = self
                .ready
                .wait_timeout_while(lock, duration, |_| !stopped.load(Ordering::Acquire))
                .unwrap_or_else(PoisonError::into_inner);
            !stopped.load(Ordering::Acquire)
        }
        fn wake(&self) {
            let _lock = self.wait.lock().unwrap_or_else(PoisonError::into_inner);
            self.ready.notify_all();
        }
    }
    pub(super) struct CursorSource {
        pub(super) reader: Reader,
        pub(super) alive: Alive,
        pub(super) clock: Arc<dyn Clock>,
    }
    #[cfg(test)]
    pub(super) type SourceFactory = Box<dyn FnMut() -> CursorSource + Send>;

    /// An allocation identifies one monitor generation. Delivery and invalidation share a short
    /// lock: off cannot return while a send for that generation is in progress. EventSink's
    /// frozen contract requires send to be nonblocking.
    pub(in crate::hyprland) struct Generation(Mutex<bool>);
    impl Generation {
        fn new() -> Arc<Self> {
            Arc::new(Self(Mutex::new(true)))
        }
        fn invalidate(&self) {
            *self.0.lock().unwrap_or_else(PoisonError::into_inner) = false;
        }
        pub(super) fn deliver(
            &self,
            at: MonoTime,
            sink: &Option<Arc<dyn EventSink<CaptureEvent>>>,
        ) {
            let live = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            if *live && let Some(sink) = sink {
                sink.send(CaptureEvent::LocalActivity { at });
            }
        }
    }
    struct InvalidateOnExit(Arc<Generation>);
    impl Drop for InvalidateOnExit {
        fn drop(&mut self) {
            self.0.invalidate();
        }
    }
    pub(super) type Report = Box<dyn Fn(Arc<Generation>, MonoTime) + Send>;

    /// Whether the Hyprland process named by `lock` still runs.
    pub(super) fn compositor_alive(lock: PathBuf) -> Alive {
        let original = compositor_process(&lock, Path::new("/proc"));
        Box::new(move || {
            original.is_some() && compositor_process(&lock, Path::new("/proc")) == original
        })
    }

    /// The first line of `lock` is the compositor's pid; `proc_root/<pid>/comm` says what that
    /// process is. Pin its process start time too, so a reused pid cannot keep an old monitor
    /// running. Missing or unreadable files mean the instance is gone.
    fn compositor_process(lock: &Path, proc_root: &Path) -> Option<(String, String)> {
        let text = std::fs::read_to_string(lock).ok()?;
        let pid = text
            .lines()
            .next()
            .map(str::trim)
            .filter(|pid| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()))?;
        let process = proc_root.join(pid);
        if std::fs::read_to_string(process.join("comm")).ok()?.trim() != COMPOSITOR {
            return None;
        }
        let stat = std::fs::read_to_string(process.join("stat")).ok()?;
        // After the closing parenthesis, field 3 (state) starts the suffix; start time is field 22.
        let started = stat.rsplit_once(") ")?.1.split_whitespace().nth(19)?;
        Some((pid.to_owned(), started.to_owned()))
    }

    /// The node's monotonic clock, which stamps every capture event.
    fn mono_now() -> MonoTime {
        let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
        MonoTime::from_nanos(
            (t.tv_sec.max(0) as u64)
                .saturating_mul(1_000_000_000)
                .saturating_add(t.tv_nsec.max(0) as u64),
        )
    }

    /// A running monitor thread. Stopped, and joined, by [`Monitor::stop`] and on drop.
    pub(super) struct Monitor {
        stop: Arc<AtomicBool>,
        clock: Arc<dyn Clock>,
        generation: Arc<Generation>,
        thread: Option<JoinHandle<()>>,
    }

    impl std::fmt::Debug for Monitor {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Monitor")
                .field("running", &self.is_running())
                .finish_non_exhaustive()
        }
    }

    impl Monitor {
        /// Start the production 100 ms cadence, reading immediately. Reports carry this run's
        /// generation into the capture delivery queue.
        pub(super) fn start_source(
            source: CursorSource,
            injected: Arc<InjectedPosition>,
            report: Report,
        ) -> Result<Monitor, PlatformError> {
            let stop = Arc::new(AtomicBool::new(false));
            let stopped = stop.clone();
            let clock = source.clock.clone();
            let generation = Generation::new();
            let active = generation.clone();
            let thread = std::thread::Builder::new()
                .name("hypr-local-activity".into())
                .spawn(move || {
                    let _invalidate = InvalidateOnExit(active.clone());
                    run(source, &injected, &*report, &active, &stopped);
                })
                .map_err(backend)?;
            Ok(Monitor {
                stop,
                clock,
                generation,
                thread: Some(thread),
            })
        }

        /// False once the thread has ended, by `stop` or on its own (its compositor went away).
        pub(super) fn is_running(&self) -> bool {
            self.thread.as_ref().is_some_and(|t| !t.is_finished())
        }

        /// Stop the thread and wait for it: it ends at its next wait, or when the reading in
        /// progress does (two requests of [`IPC_TIMEOUT`] at most). Idempotent.
        pub(super) fn stop(&mut self) {
            self.stop.store(true, Ordering::Release);
            self.clock.wake();
            self.generation.invalidate();
            if let Some(thread) = self.thread.take()
                && thread.join().is_err()
            {
                tracing::warn!("the Hyprland local-activity monitor panicked");
            }
        }
    }

    impl Drop for Monitor {
        fn drop(&mut self) {
            self.stop();
        }
    }

    fn run(
        mut source: CursorSource,
        injected: &InjectedPosition,
        report: &dyn Fn(Arc<Generation>, MonoTime),
        generation: &Arc<Generation>,
        stopped: &AtomicBool,
    ) {
        let mut previous: Option<Reading> = None;
        let mut reported: Option<Instant> = None;
        let mut baseline: Option<Reading> = None;
        let mut baseline_injection: Option<Injection> = None;
        let mut geometry: Vec<CursorProjection> = Vec::new();
        // A divergent move suppressed during settling remains evidence of movement even if the
        // cursor holds still until the first settled poll.
        let mut pending = false;
        loop {
            if stopped.load(Ordering::Acquire) {
                return;
            }
            let poll_started = source.clock.now();
            if !(source.alive)() {
                tracing::debug!("the Hyprland instance is gone: local-activity monitor ends");
                return;
            }
            match (source.reader)() {
                Ok(sample) => {
                    let observed = sample.reading;
                    let at = source.clock.now();
                    // Read after the pointer, so that every injection the reading can reflect is
                    // recorded already (the injector records before it moves the pointer).
                    let injection = injected.last();
                    if injection != baseline_injection || sample.projections != geometry {
                        baseline = None;
                        pending = false;
                        baseline_injection = injection;
                        geometry.clone_from(&sample.projections);
                    }
                    let timings = Timings {
                        since_injection: injection.map(|i| at.saturating_duration_since(i.at)),
                        since_report: reported.map(|r| at.saturating_duration_since(r)),
                    };
                    let expected = injection.map(|injection| {
                        let mut expected = Reading::from(injection);
                        expected.projection = sample
                            .projections
                            .iter()
                            .copied()
                            .find(|projection| projection.display() == injection.display);
                        expected
                    });
                    // No geometry for a formerly injected display means the output disappeared.
                    // Skip rather than compare a lossy reconstruction against exact coordinates.
                    let known = observed.layout.is_none()
                        || expected.is_none()
                        || expected.is_some_and(|put| put.projection.is_some());
                    let outside_cap = expected.is_some_and(|put| {
                        put.projection
                            .zip(observed.layout)
                            .is_some_and(|(projection, layout)| {
                                projection.outside_injection_cap(put.position, layout)
                            })
                    });
                    if timings.since_injection.is_some_and(|age| age <= SETTLE)
                        && outside_cap
                        && moved_since(previous, observed)
                    {
                        pending = true;
                    }
                    if baseline.is_none()
                        && timings.since_injection.is_some_and(|age| age > SETTLE)
                        && let Some(put) = expected
                        && let Some(projection) = put.projection
                        && let Some(layout) = observed.layout
                        && projection.injection_distance(put.position, layout) == 0.0
                    {
                        let mut settled = observed;
                        settled.projection = Some(projection);
                        baseline = Some(settled);
                    }
                    let activity = known
                        && if pending && outside_cap {
                            activity_with_movement(baseline.or(expected), observed, true, timings)
                        } else {
                            is_local_activity(baseline.or(expected), observed, previous, timings)
                        };
                    previous = Some(observed);
                    if activity {
                        // Nothing is reported once a stop is pending.
                        if stopped.load(Ordering::Acquire) {
                            return;
                        }
                        pending = false;
                        reported = Some(at);
                        report(generation.clone(), mono_now());
                    }
                }
                Err(error) => {
                    tracing::debug!(%error, "Hyprland pointer reading failed; skipped");
                }
            }
            let remaining =
                INTERVAL.saturating_sub(source.clock.now().saturating_duration_since(poll_started));
            if !source.clock.wait(stopped, remaining) {
                return;
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crosspane_platform::InputCapture;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::mpsc;

        const OLD: Duration = Duration::from_secs(1);

        fn at(display: u32, x: f64, y: f64) -> Reading {
            Reading {
                display: DisplayId(display),
                position: PointDevice::new(x, y),
                layout: None,
                projection: None,
            }
        }

        /// A reading judged with the injection `age` old and the last report `since` ago.
        fn judge(
            injected: Option<Reading>,
            observed: Reading,
            previous: Option<Reading>,
            age: Option<Duration>,
            since: Option<Duration>,
        ) -> bool {
            is_local_activity(
                injected,
                observed,
                previous,
                Timings {
                    since_injection: age,
                    since_report: since,
                },
            )
        }

        #[test]
        fn injected_and_matching_gives_no_report() {
            let put = at(0, 500.0, 300.0);
            // The pointer arrived (it moved) exactly where the injector put it.
            assert!(!judge(
                Some(put),
                put,
                Some(at(0, 100.0, 100.0)),
                Some(OLD),
                None
            ));
            // Within the tolerance of the compositor's whole-pixel coordinates.
            assert!(!judge(
                Some(put),
                at(0, 502.0, 301.5),
                Some(at(0, 100.0, 100.0)),
                Some(OLD),
                None
            ));
            // Exactly three device pixels is not "more than" three.
            assert!(!judge(
                Some(put),
                at(0, 503.0, 300.0),
                Some(at(0, 100.0, 100.0)),
                Some(OLD),
                None
            ));
        }

        #[test]
        fn a_divergence_over_three_pixels_after_the_injection_settled_is_reported() {
            let put = at(0, 500.0, 300.0);
            let before = Some(at(0, 500.0, 300.0));
            // A pixel past three, along either axis and diagonally (3 px hypot is 4.24).
            for observed in [
                at(0, 503.01, 300.0),
                at(0, 500.0, 296.9),
                at(0, 503.0, 303.0),
                at(0, 900.0, 700.0),
            ] {
                assert!(
                    judge(Some(put), observed, before, Some(OLD), None),
                    "{observed:?}"
                );
            }
            // Never injected: any movement is the owner's.
            assert!(judge(None, at(0, 10.0, 10.0), before, None, None));
        }

        #[test]
        fn a_divergence_during_an_injection_in_flight_is_not_reported() {
            let put = at(0, 500.0, 300.0);
            let far = at(0, 900.0, 700.0);
            let before = Some(at(0, 500.0, 300.0));
            for age in [0, 40, 100, 150] {
                assert!(
                    !judge(
                        Some(put),
                        far,
                        before,
                        Some(Duration::from_millis(age)),
                        None
                    ),
                    "{age} ms"
                );
            }
            // Older than 150 ms: settled.
            assert!(judge(
                Some(put),
                far,
                before,
                Some(Duration::from_millis(151)),
                None
            ));
        }

        #[test]
        fn a_stale_divergence_is_reported_once() {
            let put = at(0, 500.0, 300.0);
            let there = at(0, 900.0, 700.0);
            // The reading in which the pointer arrives there is the one report...
            assert!(judge(Some(put), there, Some(put), Some(OLD), None));
            // ...and while it stays there, no later reading repeats it, however long it takes.
            for since in [0, 500, 5_000, 60_000] {
                assert!(
                    !judge(
                        Some(put),
                        there,
                        Some(there),
                        Some(OLD),
                        Some(Duration::from_millis(since))
                    ),
                    "{since} ms"
                );
            }
            // The first reading is only a baseline, however far it is from the injection.
            assert!(!judge(Some(put), there, None, Some(OLD), None));
        }

        #[test]
        fn at_most_one_report_per_500_ms() {
            let put = at(0, 500.0, 300.0);
            let (there, further) = (at(0, 900.0, 700.0), at(0, 910.0, 710.0));
            for since in [0, 100, 499] {
                assert!(
                    !judge(
                        Some(put),
                        further,
                        Some(there),
                        Some(OLD),
                        Some(Duration::from_millis(since))
                    ),
                    "{since} ms"
                );
            }
            for since in [500, 501, 10_000] {
                assert!(
                    judge(
                        Some(put),
                        further,
                        Some(there),
                        Some(OLD),
                        Some(Duration::from_millis(since))
                    ),
                    "{since} ms"
                );
            }
        }

        #[test]
        fn another_display_is_a_divergence() {
            let put = at(0, 500.0, 300.0);
            // The same device pixels, on another display: the owner moved the pointer across.
            assert!(judge(
                Some(put),
                at(1, 500.0, 300.0),
                Some(at(0, 500.0, 300.0)),
                Some(OLD),
                None
            ));
            // Not while an injection onto that display settles.
            assert!(!judge(
                Some(put),
                at(1, 500.0, 300.0),
                Some(at(0, 500.0, 300.0)),
                Some(Duration::from_millis(20)),
                None
            ));
        }

        #[test]
        fn instance_liveness_pins_the_lock_process_and_start_time() {
            let dir =
                std::env::temp_dir().join(format!("cp-local-activity-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("proc/4242")).unwrap();
            let lock = dir.join("hyprland.lock");
            let proc_root = dir.join("proc");
            // No lock: the instance directory is gone.
            assert!(compositor_process(&lock, &proc_root).is_none());
            std::fs::write(&lock, "4242\nwayland-9\n").unwrap();
            // A pid with no process.
            assert!(compositor_process(&lock, &proc_root).is_none());
            // A process that is not Hyprland (the pid was reused).
            std::fs::write(proc_root.join("4242/comm"), "bash\n").unwrap();
            assert!(compositor_process(&lock, &proc_root).is_none());
            std::fs::write(proc_root.join("4242/comm"), "Hyprland\n").unwrap();
            assert!(
                compositor_process(&lock, &proc_root).is_none(),
                "missing stat"
            );
            let stat = |started| format!("4242 (Hyprland) S {} {started} 0\n", "0 ".repeat(18));
            std::fs::write(proc_root.join("4242/stat"), stat(100)).unwrap();
            let original = compositor_process(&lock, &proc_root);
            assert_eq!(original, Some(("4242".into(), "100".into())));
            // Another Hyprland with a reused pid is a different instance.
            std::fs::write(proc_root.join("4242/stat"), stat(200)).unwrap();
            assert_ne!(compositor_process(&lock, &proc_root), original);
            // A lock that does not name a pid (including one that tries to leave the root).
            for garbage in ["", "\n", "x\n", "../4242\n", "-1\n"] {
                std::fs::write(&lock, garbage).unwrap();
                assert!(
                    compositor_process(&lock, &proc_root).is_none(),
                    "{garbage:?}"
                );
            }
            std::fs::remove_dir_all(&dir).unwrap();
        }

        /// Collects what the monitor reports.
        #[derive(Clone, Default)]
        struct Reports(Arc<Mutex<Vec<CaptureEvent>>>);
        impl Reports {
            fn sink(&self) -> Arc<dyn EventSink<CaptureEvent>> {
                let events = self.0.clone();
                Arc::new(move |event: CaptureEvent| events.lock().unwrap().push(event))
            }
            fn count(&self) -> usize {
                self.0
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|e| matches!(e, CaptureEvent::LocalActivity { .. }))
                    .count()
            }
        }

        /// A controllable monotonic clock. Each wait announces the production delay and blocks
        /// until the test advances it or shutdown wakes it; no test waits for wall-clock cadence.
        struct FakeClock {
            state: Mutex<(Instant, usize)>,
            ready: Condvar,
            waited: mpsc::Sender<Duration>,
            wake_notice: Mutex<Option<mpsc::Sender<()>>>,
        }
        impl FakeClock {
            fn new() -> (Arc<Self>, mpsc::Receiver<Duration>) {
                let (waited, waits) = mpsc::channel();
                (
                    Arc::new(Self {
                        state: Mutex::new((Instant::now(), 0)),
                        ready: Condvar::new(),
                        waited,
                        wake_notice: Mutex::new(None),
                    }),
                    waits,
                )
            }
            fn advance(&self, duration: Duration) {
                self.state.lock().unwrap().0 += duration;
            }
            fn step(&self) {
                self.state.lock().unwrap().1 += 1;
                self.ready.notify_all();
            }
        }
        impl Clock for FakeClock {
            fn now(&self) -> Instant {
                self.state.lock().unwrap().0
            }
            fn wait(&self, stopped: &AtomicBool, duration: Duration) -> bool {
                self.waited.send(duration).unwrap();
                let mut state = self.state.lock().unwrap();
                while state.1 == 0 && !stopped.load(Ordering::Acquire) {
                    state = self.ready.wait(state).unwrap();
                }
                if stopped.load(Ordering::Acquire) {
                    return false;
                }
                state.1 -= 1;
                state.0 += duration;
                true
            }
            fn wake(&self) {
                let _state = self.state.lock().unwrap();
                if let Some(notice) = self.wake_notice.lock().unwrap().as_ref() {
                    let _ = notice.send(());
                }
                self.ready.notify_all();
            }
        }

        struct Harness {
            capture: super::super::HyprlandCapture,
            // Keep the detached worker's receiver and shared injection allocation alive.
            _commands: mpsc::Receiver<super::super::Command>,
            injected: Arc<InjectedPosition>,
            clock: Arc<FakeClock>,
            waits: mpsc::Receiver<Duration>,
            queued: mpsc::Receiver<super::super::Delivery>,
            reports: Reports,
            calls: Arc<AtomicUsize>,
            starts: Arc<Mutex<Vec<Instant>>>,
            alive: Arc<AtomicBool>,
        }
        impl Harness {
            fn new(script: Vec<Option<Observation>>, read_cost: Duration) -> Self {
                let (mut capture, commands) = super::super::tests::detached(None);
                let injected = crate::hyprland::inject::register(&capture.gate);
                let (clock, waits) = FakeClock::new();
                let clock_source = clock.clone();
                let calls = Arc::new(AtomicUsize::new(0));
                let called = calls.clone();
                let starts = Arc::new(Mutex::new(Vec::new()));
                let started = starts.clone();
                let alive = Arc::new(AtomicBool::new(true));
                let live = alive.clone();
                // Factory, rather than an installed monitor, exercises the actual false->true
                // branch, shared-record lookup, queue wiring and production cadence.
                capture.monitor_source = Some(Box::new(move || {
                    let script = script.clone();
                    let clock = clock_source.clone();
                    let reader_clock = clock.clone();
                    let calls = called.clone();
                    let starts = started.clone();
                    let alive = live.clone();
                    let mut n = 0;
                    CursorSource {
                        reader: Box::new(move || {
                            calls.fetch_add(1, Ordering::SeqCst);
                            starts.lock().unwrap().push(reader_clock.now());
                            reader_clock.advance(read_cost);
                            let sample = script.get(n).or(script.last()).cloned().flatten();
                            n += 1;
                            sample.ok_or_else(|| match n % 3 {
                                0 => PlatformError::Timeout,
                                1 => PlatformError::NotFound,
                                _ => PlatformError::Backend("bad json".into()),
                            })
                        }),
                        alive: Box::new(move || alive.load(Ordering::Acquire)),
                        clock,
                    }
                }));
                let (delivery, queued) = mpsc::channel();
                capture.activity_delivery = delivery;
                let reports = Reports::default();
                capture.sink = Some(reports.sink());
                Self {
                    capture,
                    _commands: commands,
                    injected,
                    clock,
                    waits,
                    queued,
                    reports,
                    calls,
                    starts,
                    alive,
                }
            }
            fn readings(script: Vec<Option<Reading>>, cost: Duration) -> Self {
                Self::new(
                    script
                        .into_iter()
                        .map(|r| r.map(Observation::from))
                        .collect(),
                    cost,
                )
            }
            fn quiet(&self, position: Reading) {
                self.injected.record_at(
                    position.display,
                    position.position,
                    self.clock.now() - OLD,
                );
            }
            fn enable(&mut self) {
                self.capture.set_monitor_local_activity(true).unwrap();
            }
            fn waited(&self) -> Duration {
                self.waits.recv_timeout(Duration::from_secs(2)).unwrap()
            }
            fn step(&self) -> Duration {
                self.clock.step();
                self.waited()
            }
            fn drain(&self) {
                while let Ok(delivery) = self.queued.try_recv() {
                    match delivery {
                        super::super::Delivery::LocalActivity { at, generation } => {
                            generation.deliver(at, &self.capture.sink)
                        }
                        _ => panic!("unexpected non-activity delivery"),
                    }
                }
            }
            fn off(&mut self) {
                self.capture.set_monitor_local_activity(false).unwrap();
            }
        }

        #[test]
        fn the_thread_reports_a_pointer_the_injector_did_not_put_there_once() {
            let mut h = Harness::readings(
                vec![
                    Some(at(0, 500.0, 300.0)),
                    Some(at(0, 500.0, 300.0)),
                    Some(at(0, 700.0, 300.0)),
                    Some(at(0, 710.0, 300.0)),
                    Some(at(0, 720.0, 300.0)),
                ],
                Duration::ZERO,
            );
            h.quiet(at(0, 500.0, 300.0));
            h.enable();
            assert_eq!(h.waited(), INTERVAL);
            for _ in 0..15 {
                assert_eq!(h.step(), INTERVAL);
                h.drain();
            }
            assert_eq!(h.reports.count(), 1);
            h.off();
            h.drain();
            assert_eq!(h.reports.count(), 1);
        }

        #[test]
        fn injected_motion_is_never_reported() {
            let mut h = Harness::readings(
                (0..32)
                    .map(|n| Some(at(0, n as f64 * 10.0, 50.0)))
                    .collect(),
                Duration::ZERO,
            );
            h.quiet(at(0, 0.0, 50.0));
            h.enable();
            h.waited();
            for n in 1..32 {
                h.injected.record_at(
                    DisplayId(0),
                    PointDevice::new(n as f64 * 10.0, 50.0),
                    h.clock.now(),
                );
                h.step();
                h.drain();
            }
            h.off();
            assert_eq!(h.reports.count(), 0);
        }

        #[test]
        fn a_failed_reading_is_skipped_and_never_reports() {
            let mut h = Harness::readings(
                vec![
                    Some(at(0, 500.0, 300.0)),
                    None,
                    None,
                    None,
                    Some(at(0, 500.0, 300.0)),
                ],
                Duration::ZERO,
            );
            h.quiet(at(0, 500.0, 300.0));
            h.enable();
            h.waited();
            for _ in 0..12 {
                h.step();
                h.drain();
            }
            h.off();
            assert_eq!(h.reports.count(), 0);
        }

        fn compositor_observation(scale: f64, point: PointDevice) -> Observation {
            compositor_observation_on(scale, point, (1920.0, 1080.0))
        }

        fn compositor_observation_on(
            scale: f64,
            point: PointDevice,
            extent: (f64, f64),
        ) -> Observation {
            use serde_json::json;
            let reported: f64 = format!("{scale:.2}").parse().unwrap();
            let (width, height) = extent;
            let monitors = json!([{"id":0,"width":width as u32,"height":height as u32,"x":0,"y":0,
                "scale":reported,"transform":0}]);
            let raw = json!({
                "x": ((width / scale).round() * point.x / width).floor(),
                "y": ((height / scale).round() * point.y / height).floor(),
            });
            crate::hyprland::cursor::locate_sample(&raw, &monitors)
                .unwrap()
                .into()
        }

        #[test]
        fn fractional_scale_injection_after_a_skipped_poll_is_never_local_activity() {
            for scale in [4.0 / 3.0, 1.5, 1.25, 4.25, 16.0] {
                for point in [
                    PointDevice::new(1500.0, 500.0),
                    PointDevice::new(0.0, 0.0),
                    PointDevice::new(1919.99609375, 1079.99609375),
                ] {
                    let baseline = compositor_observation(scale, PointDevice::new(100.0, 100.0));
                    let arrived = compositor_observation(scale, point);
                    if scale == 4.0 / 3.0 && point.x == 1500.0 {
                        assert_eq!(arrived.reading.position, PointDevice::new(1496.25, 498.75));
                        assert!(
                            (arrived.reading.position.x - point.x)
                                .hypot(arrived.reading.position.y - point.y)
                                > DIVERGENCE
                        );
                    }
                    let mut h =
                        Harness::new(vec![Some(baseline), None, Some(arrived)], Duration::ZERO);
                    h.enable();
                    h.waited();
                    h.injected.record_at(DisplayId(0), point, h.clock.now());
                    h.step(); // Failed 100 ms poll; successful poll is older than settle time.
                    h.step();
                    h.drain();
                    assert_eq!(h.reports.count(), 0, "scale {scale}, injection {point:?}");
                    h.off();
                }
            }
        }

        #[test]
        fn settled_baseline_detects_a_hundred_device_pixels_at_quarter_scale_on_a_large_display() {
            let point = PointDevice::new(7000.0, 2000.0);
            let arrived = compositor_observation_on(0.25, point, (7680.0, 4320.0));
            let local =
                compositor_observation_on(0.25, PointDevice::new(7100.0, 2000.0), (7680.0, 4320.0));
            assert_eq!(arrived.reading.layout, Some((28000.0, 8000.0)));
            assert_eq!(local.reading.layout, Some((28400.0, 8000.0)));
            let mut h = Harness::new(
                vec![Some(arrived.clone()), None, Some(arrived), Some(local)],
                Duration::ZERO,
            );
            h.enable();
            h.waited();
            h.injected.record_at(DisplayId(0), point, h.clock.now());
            h.step(); // 100 ms: failed reading.
            h.step(); // 200 ms: settled baseline.
            h.drain();
            assert_eq!(h.reports.count(), 0);
            h.step();
            h.drain();
            assert_eq!(
                h.reports.count(),
                1,
                "100 device pixels disappeared in scale uncertainty"
            );
            for _ in 0..10 {
                h.step();
                h.drain();
            }
            assert_eq!(h.reports.count(), 1, "stale divergence repeated");
            h.off();
        }

        #[test]
        fn settled_baseline_at_two_and_three_decimal_scales_reports_later_local_motion() {
            for scale in [1.33, 1.333, 4.0 / 3.0] {
                let point = PointDevice::new(1500.0, 500.0);
                let arrived = compositor_observation(scale, point);
                let local = compositor_observation(scale, PointDevice::new(1506.0, 500.0));
                let projection = arrived.projections[0];
                assert!(
                    projection.observation_distance(
                        arrived.reading.layout.unwrap(),
                        local.reading.layout.unwrap()
                    ) >= 4.0
                );
                let mut h = Harness::new(
                    vec![
                        Some(compositor_observation(
                            scale,
                            PointDevice::new(100.0, 100.0),
                        )),
                        None,
                        Some(arrived.clone()),
                        Some(arrived),
                        Some(local),
                    ],
                    Duration::ZERO,
                );
                h.enable();
                h.waited();
                h.injected.record_at(DisplayId(0), point, h.clock.now());
                for _ in 0..3 {
                    h.step();
                    h.drain();
                    assert_eq!(h.reports.count(), 0, "injected-only at scale {scale}");
                }
                h.step();
                h.drain();
                assert_eq!(
                    h.reports.count(),
                    1,
                    "local motion after baseline at scale {scale}"
                );
                h.off();
            }
        }

        #[test]
        fn local_motion_inside_the_first_settled_envelope_becomes_baseline_then_next_motion_reports()
         {
            let point = PointDevice::new(7000.0, 2000.0);
            let sample =
                |x| compositor_observation_on(0.25, PointDevice::new(x, 2000.0), (7680.0, 4320.0));
            let mut h = Harness::new(
                vec![
                    Some(sample(7000.0)),
                    Some(sample(7000.0)),
                    Some(sample(7002.0)),
                    Some(sample(7006.0)),
                ],
                Duration::ZERO,
            );
            h.enable();
            h.waited();
            h.injected.record_at(DisplayId(0), point, h.clock.now());
            h.step(); // In flight: no settled baseline.
            h.step();
            h.drain(); // First settled position includes two local device pixels.
            assert_eq!(
                h.reports.count(),
                0,
                "ambiguous pre-baseline motion must establish the baseline"
            );
            h.step();
            h.drain();
            assert_eq!(
                h.reports.count(),
                1,
                "next four device pixels must be detected"
            );
            h.off();
        }

        #[test]
        fn a_new_injection_even_at_the_same_point_and_timestamp_clears_the_settled_baseline() {
            let point = PointDevice::new(7000.0, 2000.0);
            let sample =
                |x| compositor_observation_on(0.25, PointDevice::new(x, 2000.0), (7680.0, 4320.0));
            let mut h = Harness::new(
                vec![
                    Some(sample(7000.0)),
                    Some(sample(7002.0)),
                    Some(sample(7007.0)),
                    Some(sample(7011.0)),
                ],
                Duration::ZERO,
            );
            h.quiet(at(0, point.x, point.y));
            h.enable();
            h.waited(); // Establish the first settled baseline.
            h.step();
            h.drain();
            assert_eq!(h.reports.count(), 0);
            let old = h.injected.last().unwrap();
            h.injected.record_at(old.display, old.position, old.at);
            h.step();
            h.drain(); // New baseline at +7, rather than reporting relative to old one.
            assert_eq!(
                h.reports.count(),
                0,
                "new injection did not clear old baseline"
            );
            h.step();
            h.drain();
            assert_eq!(
                h.reports.count(),
                1,
                "next motion from fresh baseline was missed"
            );
            h.off();
        }

        #[test]
        fn output_geometry_change_clears_the_settled_baseline() {
            let point = PointDevice::new(1500.0, 500.0);
            let mut h = Harness::new(
                vec![
                    Some(compositor_observation(1.333, point)),
                    Some(compositor_observation(1.25, point)),
                    Some(compositor_observation(
                        1.25,
                        PointDevice::new(1506.0, 500.0),
                    )),
                ],
                Duration::ZERO,
            );
            h.quiet(at(0, point.x, point.y));
            h.enable();
            h.waited();
            h.step();
            h.drain();
            assert_eq!(
                h.reports.count(),
                0,
                "geometry change was compared with an obsolete baseline"
            );
            h.step();
            h.drain();
            assert_eq!(h.reports.count(), 1);
            h.off();
        }

        #[test]
        fn pre_baseline_local_motion_beyond_eight_device_pixels_reports_without_extra_slack() {
            let point = PointDevice::new(7000.0, 2000.0);
            let sample =
                |x| compositor_observation_on(0.25, PointDevice::new(x, 2000.0), (7680.0, 4320.0));
            let mut h = Harness::new(
                vec![
                    Some(sample(7000.0)),
                    Some(sample(7009.0)),
                    Some(sample(7009.0)),
                ],
                Duration::ZERO,
            );
            h.enable();
            h.waited();
            h.injected.record_at(DisplayId(0), point, h.clock.now());
            h.step();
            h.drain();
            assert_eq!(h.reports.count(), 0, "reported before injection settled");
            h.step();
            h.drain();
            assert_eq!(
                h.reports.count(),
                1,
                "8-pixel cap acquired another 3 pixels of slack"
            );
            for _ in 0..10 {
                h.step();
                h.drain();
                assert_eq!(
                    h.reports.count(),
                    1,
                    "pending settling move reported repeatedly"
                );
            }
            h.off();
        }

        #[test]
        fn pending_settling_divergence_is_cleared_on_new_injection_and_geometry_change() {
            for new_injection in [true, false] {
                let point = PointDevice::new(7000.0, 2000.0);
                let first = compositor_observation_on(0.25, point, (7680.0, 4320.0));
                let local = compositor_observation_on(
                    0.25,
                    PointDevice::new(7009.0, 2000.0),
                    (7680.0, 4320.0),
                );
                let after_reset = if new_injection {
                    local.clone()
                } else {
                    compositor_observation_on(
                        0.25,
                        PointDevice::new(7009.0, 2000.0),
                        (8000.0, 4320.0),
                    )
                };
                let mut h = Harness::new(
                    vec![Some(first), Some(local), Some(after_reset)],
                    Duration::ZERO,
                );
                h.enable();
                h.waited();
                h.injected.record_at(DisplayId(0), point, h.clock.now());
                h.step(); // +9px physical movement at 100ms: pending, not yet reported.
                h.drain();
                assert_eq!(h.reports.count(), 0);
                if new_injection {
                    h.injected.record_at(DisplayId(0), point, h.clock.now());
                }
                for _ in 0..10 {
                    h.step();
                    h.drain();
                }
                assert_eq!(
                    h.reports.count(),
                    0,
                    "pending movement survived reset; injection={new_injection}"
                );
                h.off();
            }
        }

        #[test]
        fn the_lifecycle_polls_only_while_on_and_joins_at_the_production_cadence() {
            let mut h = Harness::readings(vec![Some(at(0, 1.0, 1.0))], Duration::from_millis(30));
            h.quiet(at(0, 1.0, 1.0));
            assert_eq!(h.calls.load(Ordering::SeqCst), 0, "polled while off");
            h.off();
            assert_eq!(h.calls.load(Ordering::SeqCst), 0);
            h.enable();
            assert_eq!(h.waited(), Duration::from_millis(70));
            h.enable(); // Already on: no new source, baseline or thread.
            for _ in 0..3 {
                assert_eq!(h.step(), Duration::from_millis(70));
            }
            assert_eq!(h.calls.load(Ordering::SeqCst), 4);
            for starts in h.starts.lock().unwrap().windows(2) {
                assert_eq!(
                    starts[1].duration_since(starts[0]),
                    Duration::from_millis(100)
                );
            }
            h.off();
            h.off();
            assert!(h.capture.monitor.is_none());
            h.clock.advance(Duration::from_secs(10));
            assert_eq!(h.calls.load(Ordering::SeqCst), 4, "polled while off");
            h.enable();
            assert_eq!(h.waited(), Duration::from_millis(70));
            assert_eq!(h.step(), Duration::from_millis(70));
            assert_eq!(h.calls.load(Ordering::SeqCst), 6);
            drop(h.capture);
            h.clock.advance(Duration::from_secs(10));
            assert_eq!(h.calls.load(Ordering::SeqCst), 6, "polled after drop");
        }

        #[test]
        fn queued_activity_is_invalidated_on_off_reenable_drop_and_restart() {
            let mut h = Harness::readings(
                vec![Some(at(0, 0.0, 0.0)), Some(at(0, 20.0, 0.0))],
                Duration::ZERO,
            );
            h.quiet(at(0, 0.0, 0.0));
            h.enable();
            h.waited();
            h.step(); // Queue activity without delivering it.
            let stale = h.queued.try_recv().unwrap();
            h.off();
            h.enable();
            h.waited();
            if let super::super::Delivery::LocalActivity { at, generation } = stale {
                generation.deliver(at, &h.capture.sink);
            } else {
                panic!("unexpected delivery");
            }
            assert_eq!(h.reports.count(), 0, "old session delivered after reenable");
            h.step();
            h.drain();
            assert_eq!(h.reports.count(), 1, "fresh generation did not deliver");
            h.off();
            h.enable();
            h.waited();
            h.step();
            // Simulate compositor restart. Join the worker without an off call, exercising its
            // exit guard; the pending generation must be invalid before a new target begins.
            h.alive.store(false, Ordering::Release);
            h.clock.step();
            h.capture
                .monitor
                .as_mut()
                .unwrap()
                .thread
                .take()
                .unwrap()
                .join()
                .unwrap();
            h.drain();
            assert_eq!(
                h.reports.count(),
                1,
                "restarted compositor's queue delivered"
            );
            h.alive.store(true, Ordering::Release);
            h.enable();
            h.waited();
            h.step();
            let sink = h.capture.sink.clone();
            drop(h.capture);
            while let Ok(super::super::Delivery::LocalActivity { at, generation }) =
                h.queued.try_recv()
            {
                generation.deliver(at, &sink);
            }
            assert_eq!(h.reports.count(), 1, "queued activity delivered after drop");
        }

        #[test]
        fn invalidation_waits_for_an_in_progress_delivery() {
            let generation = Generation::new();
            let (entered, started) = mpsc::channel();
            let (release, released) = mpsc::channel();
            let released = Mutex::new(released);
            // A deliberately stalled fixture lets the test exercise synchronization with a send
            // already in progress. Production EventSink is required to be nonblocking.
            let sink: Arc<dyn EventSink<CaptureEvent>> = Arc::new(move |_| {
                entered.send(()).unwrap();
                released.lock().unwrap().recv().unwrap();
            });
            let delivering = generation.clone();
            let sender = std::thread::spawn(move || {
                delivering.deliver(MonoTime::from_nanos(1), &Some(sink))
            });
            started.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(
                generation.0.try_lock().is_err(),
                "delivery did not hold the invalidation lock"
            );
            let invalidating = generation.clone();
            let stopped = std::thread::spawn(move || invalidating.invalidate());
            release.send(()).unwrap();
            sender.join().unwrap();
            stopped.join().unwrap();
            assert!(!*generation.0.lock().unwrap());
        }

        #[test]
        fn a_stop_during_a_reading_waits_for_it_and_reports_nothing() {
            let mut h = Harness::readings(vec![Some(at(0, 0.0, 0.0))], Duration::ZERO);
            h.quiet(at(0, 0.0, 0.0));
            let (entered, started) = mpsc::channel();
            let (release, released) = mpsc::channel();
            let clock = h.clock.clone();
            let mut released = Some(released);
            h.capture.monitor_source = Some(Box::new(move || {
                let entered = entered.clone();
                let released = released.take().unwrap();
                let mut baseline = true;
                CursorSource {
                    reader: Box::new(move || {
                        if std::mem::take(&mut baseline) {
                            return Ok(at(0, 0.0, 0.0).into());
                        }
                        entered.send(()).unwrap();
                        released.recv().unwrap();
                        Ok(at(0, 20.0, 0.0).into())
                    }),
                    alive: Box::new(|| true),
                    clock: clock.clone(),
                }
            }));
            h.enable();
            h.waited();
            h.clock.step();
            started.recv_timeout(Duration::from_secs(2)).unwrap();
            let mut monitor = h.capture.monitor.take().unwrap();
            let flag = monitor.stop.clone();
            let generation = monitor.generation.clone();
            let (notice, woken) = mpsc::channel();
            *h.clock.wake_notice.lock().unwrap() = Some(notice);
            let stopping = std::thread::spawn(move || monitor.stop());
            // Release the pending read only after stop publishes its flag, without a sleep.
            woken.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(flag.load(Ordering::Acquire));
            assert!(generation.0.try_lock().is_ok());
            release.send(()).unwrap();
            stopping.join().unwrap();
            h.drain();
            assert_eq!(h.reports.count(), 0);
        }

        #[test]
        fn the_thread_ends_by_itself_when_the_compositor_is_gone() {
            let mut h = Harness::readings(vec![Some(at(0, 1.0, 1.0))], Duration::ZERO);
            h.quiet(at(0, 1.0, 1.0));
            h.enable();
            h.waited();
            for _ in 0..4 {
                h.step();
            }
            h.alive.store(false, Ordering::Release);
            h.clock.step();
            h.capture
                .monitor
                .as_mut()
                .unwrap()
                .thread
                .take()
                .unwrap()
                .join()
                .unwrap();
            assert_eq!(h.calls.load(Ordering::SeqCst), 5);
            h.off();
            h.drain();
            assert_eq!(h.reports.count(), 0);
        }

        // Actual compositor acceptance needs bounded wall-clock waiting for IPC/event delivery.
        fn wait_for(limit: Duration, condition: impl Fn() -> bool) -> bool {
            let end = Instant::now() + limit;
            while Instant::now() < end {
                if condition() {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            condition()
        }

        /// The acceptance check uses only the named, verified nested instance. With the default
        /// cleared environment this is skipped before any compositor connection is attempted.
        #[test]
        fn nested_real_injection_and_ipc_motion() {
            if std::env::var("CROSSPANE_NESTED_HYPR").as_deref() != Ok("1") {
                eprintln!("skipped: WP-1.43 local activity needs CROSSPANE_NESTED_HYPR=1");
                return;
            }
            use crate::hyprland::capture::{HyprlandCapture, NestEndpoints, verify_nest};
            use crate::hyprland::cursor::cursor_position;
            use crate::hyprland::inject::{connect, injected_position_for};
            use crosspane_platform::{InputCapture, IoGate, PointerInjector};

            let proof = verify_nest(&NestEndpoints::from_process())
                .expect("refusing local activity test outside a helper-owned nest");
            // Existing capture conformance tests take this lock around nest/output changes,
            // which can cause the parent to resize every other nested output.
            let topology_lock = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(
                    proof
                        .endpoint
                        .runtime_dir
                        .join("crosspane-capture-topology.lock"),
                )
                .unwrap();
            rustix::fs::flock(&topology_lock, rustix::fs::FlockOperation::LockExclusive).unwrap();
            let ipc = HyprIpc::from_env().unwrap();
            // The parent's tiling can resize a newly created nest just after its first IPC
            // reply. Connect the output-bound injector only after startup topology settles.
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut layout = ipc.json("monitors").unwrap();
            let mut stable_since = Instant::now();
            loop {
                std::thread::sleep(INTERVAL);
                let next = ipc.json("monitors").unwrap();
                if next != layout {
                    layout = next;
                    stable_since = Instant::now();
                }
                if stable_since.elapsed() >= Duration::from_millis(500) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "nested startup topology did not settle"
                );
            }
            let gate = IoGate::new();
            gate.set_session_permits(true);
            gate.set_engine_permits(true);
            let (keys, mut pointer) = connect(gate.clone(), ipc.clone()).unwrap();
            let injected = injected_position_for(&gate).unwrap();
            let mut capture = HyprlandCapture::new_for_nest_test(gate.clone(), &proof).unwrap();
            let reports = Reports::default();
            capture.subscribe(reports.sink()).unwrap();
            let (display, _) = cursor_position(&ipc).unwrap();
            pointer
                .move_to(display, PointDevice::new(30.0, 30.0))
                .unwrap();
            assert_eq!(
                injected.last().unwrap().position,
                PointDevice::new(30.0, 30.0)
            );
            capture.set_monitor_local_activity(true).unwrap();
            // Leave over 150 ms between motions so the settled decision is exercised too.
            let mut longest = Duration::ZERO;
            for x in [60.0, 90.0, 120.0, 150.0] {
                pointer.move_to(display, PointDevice::new(x, 40.0)).unwrap();
                std::thread::sleep(Duration::from_millis(250));
                let start = Instant::now();
                let (seen_display, seen) = cursor_position(&ipc).unwrap();
                longest = longest.max(start.elapsed());
                assert_eq!(seen_display, display);
                assert!(
                    (seen.x - x).hypot(seen.y - 40.0) <= DIVERGENCE,
                    "cursor {seen:?}, expected ({x}, 40), monitors {:?}",
                    ipc.json("monitors").unwrap()
                );
                assert_eq!(reports.count(), 0, "injected motion caused local activity");
            }
            assert!(
                longest < INTERVAL,
                "nested compositor reading stalled: {longest:?}"
            );
            // Rejected and gated motions must not replace the position actually submitted.
            let last = injected.last();
            assert!(
                pointer
                    .move_to(display, PointDevice::new(-1.0, 40.0))
                    .is_err()
            );
            assert_eq!(injected.last(), last);
            gate.set_engine_permits(false);
            assert!(
                pointer
                    .move_to(display, PointDevice::new(70.0, 40.0))
                    .is_err()
            );
            gate.set_engine_permits(true);
            assert_eq!(injected.last(), last);

            let raw = ipc.json("cursorpos").unwrap();
            let (x, y) = (raw["x"].as_f64().unwrap(), raw["y"].as_f64().unwrap());
            // Hyprland's cursor dispatcher uses layout coordinates, unlike the injector.
            ipc.dispatch(&format!(
                "hl.dsp.cursor.move({{ x = {}, y = {} }})",
                x + 20.0,
                y + 20.0
            ))
            .unwrap();
            assert!(wait_for(Duration::from_secs(2), || reports.count() == 1));
            std::thread::sleep(Duration::from_millis(650));
            assert_eq!(reports.count(), 1, "stale IPC divergence repeated");
            let stop_started = Instant::now();
            capture.set_monitor_local_activity(false).unwrap();
            let stopped_in = stop_started.elapsed();
            assert!(stopped_in < Duration::from_millis(100));
            ipc.dispatch(&format!(
                "hl.dsp.cursor.move({{ x = {}, y = {} }})",
                x + 40.0,
                y + 20.0
            ))
            .unwrap();
            std::thread::sleep(Duration::from_millis(250));
            assert_eq!(
                reports.count(),
                1,
                "activity reported while monitor was off"
            );
            eprintln!(
                "WP-1.43 nested: 0 injected-only reports; 1 IPC motion report; longest read {longest:?}; stop joined in {stopped_in:?}"
            );
            drop(capture);
            drop(pointer);
            drop(keys);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A self-deleting stand-in for `$XDG_RUNTIME_DIR`.
    struct FakeRuntime(PathBuf);
    impl FakeRuntime {
        fn new(tag: &str) -> FakeRuntime {
            let dir =
                std::env::temp_dir().join(format!("cp-capture-nest-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            FakeRuntime(dir)
        }
        fn write(&self, relative: &str, text: &str) {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        /// A running nest: Hyprland's lock and the helper's state directory, the pid of this
        /// test process standing in for the compositor.
        fn nest(&self, signature: &str, display: &str) {
            let pid = std::process::id();
            self.write(
                &format!("hypr/{signature}/hyprland.lock"),
                &format!("{pid}\n{display}\n"),
            );
            self.write(
                "crosspane-hypr-t/env",
                &format!(
                    "unset WAYLAND_SOCKET\nexport HYPRLAND_INSTANCE_SIGNATURE={signature}\n\
                     export WAYLAND_DISPLAY={display}\nexport CROSSPANE_NESTED_HYPR=1\n"
                ),
            );
            self.write("crosspane-hypr-t/pid", &format!("{pid}\n"));
        }
        fn endpoints(&self, display: Option<&str>, signature: Option<&str>) -> NestEndpoints {
            NestEndpoints {
                runtime_dir: self.0.clone(),
                wayland_socket: None,
                wayland_display: display.map(str::to_owned),
                signature: signature.map(str::to_owned),
            }
        }
    }
    impl Drop for FakeRuntime {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// An environment that names `endpoint` exactly and has the nested flag.
    fn naming(endpoint: &Endpoint) -> HookEnvironment {
        HookEnvironment {
            flag: true,
            wayland_socket: false,
            runtime_dir: Some(endpoint.runtime_dir.clone()),
            display: Some(endpoint.display.clone()),
            signature: Some(endpoint.signature.clone()),
        }
    }

    fn endpoint(runtime: &str) -> Endpoint {
        Endpoint {
            runtime_dir: PathBuf::from(runtime),
            display: "wayland-9".into(),
            signature: "sigNEST".into(),
        }
    }

    /// A handle that was never connected to anything: the commands it sends land in the returned
    /// receiver, which must stay empty when a hook refuses.
    pub(super) fn detached(nest: Option<Endpoint>) -> (HyprlandCapture, mpsc::Receiver<Command>) {
        let (wake, _receive) = UnixDatagram::pair().unwrap();
        let (commands, requests) = mpsc::channel();
        let capture = HyprlandCapture {
            commands,
            abort: Arc::new(Abort {
                epoch: AtomicU64::new(0),
                next_capture: AtomicU64::new(0),
                acknowledged: AtomicU64::new(0),
                next_barrier: AtomicU64::new(0),
                barrier_reached: AtomicU64::new(0),
                wake,
            }),
            nest,
            gate: IoGate::new(),
            source: Source::Environment,
            sink: None,
            activity_delivery: mpsc::channel().0,
            monitor: None,
            monitor_source: None,
        };
        (capture, requests)
    }

    /// Every public hook, called; each must refuse and none may reach the worker.
    fn assert_every_hook_refuses(
        capture: &mut HyprlandCapture,
        requests: &mpsc::Receiver<Command>,
    ) {
        let refused = |result: Result<(), PlatformError>| {
            assert!(
                matches!(result, Err(PlatformError::Unsupported(_))),
                "{result:?}"
            );
        };
        refused(capture.inject_worker_error_for_test());
        refused(capture.set_portals_for_test(&[], CALL_BUDGET, PortalsTestHooks::default()));
        refused(capture.end_causes_for_test().map(|_| ()));
        refused(capture.output_removal_for_test("WAYLAND-1"));
        assert!(
            requests.try_recv().is_err(),
            "a refused hook reached the worker"
        );
    }

    #[test]
    fn a_production_backend_authorises_no_hook() {
        // Whatever the environment says (the nested flag, the endpoint it is connected to, no
        // inherited socket), a backend without a proof is refused.
        let nest = endpoint("/run/user/1000");
        assert!(authorise(None, &naming(&nest)).is_err());
        // The same through the public hooks, on a handle built the way `HyprlandCapture::new`
        // builds one (`nest: None`).
        let (mut capture, requests) = detached(None);
        assert_every_hook_refuses(&mut capture, &requests);
    }

    #[test]
    fn a_proof_authorises_only_its_own_endpoint() {
        let nest = endpoint("/run/user/1000");
        assert!(authorise(Some(&nest), &naming(&nest)).is_ok());
        // The environment must still name exactly it, carry the flag and no inherited socket.
        let mut env = naming(&nest);
        env.flag = false;
        assert!(authorise(Some(&nest), &env).is_err());
        let mut env = naming(&nest);
        env.wayland_socket = true;
        assert!(authorise(Some(&nest), &env).is_err());
        for change in [
            |e: &mut HookEnvironment| e.display = Some("wayland-1".into()),
            |e: &mut HookEnvironment| e.display = None,
            |e: &mut HookEnvironment| e.signature = Some("sigLIVE".into()),
            |e: &mut HookEnvironment| e.signature = None,
            |e: &mut HookEnvironment| e.runtime_dir = Some(PathBuf::from("/run/user/1001")),
            |e: &mut HookEnvironment| e.runtime_dir = None,
        ] {
            let mut env = naming(&nest);
            change(&mut env);
            assert!(authorise(Some(&nest), &env).is_err());
        }
        // Through the public hooks: a backend with a proof still refuses when the environment of
        // this process does not name its endpoint (a fake endpoint is never the live session).
        let (mut capture, requests) = detached(Some(nest));
        assert_every_hook_refuses(&mut capture, &requests);
    }

    #[test]
    fn a_proof_for_one_runtime_directory_does_not_authorise_another() {
        // The proof records the runtime directory it verified; a backend connected under another
        // directory (same display, same signature) is not authorised by it.
        let (one, two) = (FakeRuntime::new("one"), FakeRuntime::new("two"));
        one.nest("sigNEST", "wayland-9");
        two.nest("sigNEST", "wayland-9");
        let proof = verify_nest(&one.endpoints(Some("wayland-9"), Some("sigNEST"))).unwrap();
        assert_eq!(proof.endpoint.runtime_dir, one.0);
        let other = verify_nest(&two.endpoints(Some("wayland-9"), Some("sigNEST"))).unwrap();
        assert!(authorise(Some(&proof.endpoint), &naming(&proof.endpoint)).is_ok());
        assert!(authorise(Some(&proof.endpoint), &naming(&other.endpoint)).is_err());
        assert!(authorise(Some(&other.endpoint), &naming(&proof.endpoint)).is_err());
        // The proof's endpoint is the one a backend built with it connects to.
        assert_eq!(
            Source::Pinned(proof.endpoint.clone())
                .socket_path()
                .unwrap(),
            one.0.join("wayland-9")
        );
    }

    /// No compositor needed: the guard only reads files.
    #[test]
    fn verify_nest_refuses_inherited_sockets_and_foreign_endpoints() {
        let rt = FakeRuntime::new("guard");
        rt.nest("sigNEST", "wayland-9");
        // The "live session": an instance with its own lock and no nest state directory.
        rt.write("hypr/sigLIVE/hyprland.lock", "1\nwayland-1\n");
        assert!(verify_nest(&rt.endpoints(Some("wayland-9"), Some("sigNEST"))).is_ok());
        // An inherited WAYLAND_SOCKET is refused even when everything else is the nest's.
        let mut inherited = rt.endpoints(Some("wayland-9"), Some("sigNEST"));
        inherited.wayland_socket = Some("7".into());
        assert!(
            verify_nest(&inherited)
                .unwrap_err()
                .contains("WAYLAND_SOCKET")
        );
        inherited.wayland_socket = Some("".into());
        assert!(verify_nest(&inherited).is_err(), "an empty one counts too");
        // Missing or implausible endpoints.
        for (display, signature) in [
            (None, Some("sigNEST")),
            (Some(""), Some("sigNEST")),
            (Some("wayland-9"), None),
            (Some("wayland-9"), Some("")),
            (Some("wayland-9"), Some("../sigNEST")),
            (Some("wayland-9"), Some("sigMISSING")),
        ] {
            assert!(
                verify_nest(&rt.endpoints(display, signature)).is_err(),
                "{display:?} {signature:?}"
            );
        }
        let mut no_runtime = rt.endpoints(Some("wayland-9"), Some("sigNEST"));
        no_runtime.runtime_dir = PathBuf::new();
        assert!(verify_nest(&no_runtime).is_err(), "no runtime directory");
        // The live session, and mismatched pairs, are not a nest.
        for (display, signature) in [
            ("wayland-1", "sigLIVE"),
            ("wayland-9", "sigLIVE"),
            ("wayland-1", "sigNEST"),
        ] {
            assert!(
                verify_nest(&rt.endpoints(Some(display), Some(signature))).is_err(),
                "{display} {signature}"
            );
        }
        // The nest's state must agree with Hyprland's lock: the same, still running compositor.
        let good = rt.endpoints(Some("wayland-9"), Some("sigNEST"));
        rt.write("crosspane-hypr-t/pid", "999999998\n");
        assert!(verify_nest(&good).is_err(), "the state names another pid");
        rt.nest("sigNEST", "wayland-9");
        rt.write(
            "crosspane-hypr-t/env",
            "export HYPRLAND_INSTANCE_SIGNATURE=sigNEST\nexport WAYLAND_DISPLAY=wayland-8\n",
        );
        assert!(
            verify_nest(&good).is_err(),
            "the state names another socket"
        );
        rt.nest("sigNEST", "wayland-9");
        assert!(verify_nest(&good).is_ok());
        // Only `crosspane-hypr-*` directories count as nest state.
        std::fs::rename(rt.0.join("crosspane-hypr-t"), rt.0.join("other-t")).unwrap();
        assert!(verify_nest(&good).is_err(), "not a nest state directory");
    }

    #[test]
    fn only_marked_backend_errors_are_rejections() {
        // The worker's own reports, whatever they were made from, are rejections: that includes a
        // worker-reported timeout, which must not be mistaken for the caller's.
        for error in [
            backend("invalid capture portal"),
            PlatformError::Timeout,
            PlatformError::NotFound,
        ] {
            let marked = rejected(error);
            assert!(
                matches!(&marked, PlatformError::Backend(m) if m.starts_with(PORTALS_REJECTED)),
                "{marked:?}"
            );
            assert_eq!(set_portals_failure(&marked), SetPortalsFailure::Rejected);
        }
        // The caller's receive timeout, a worker that is gone, an abort, and anything unknown
        // leave the fate of the previous set and the capture unknown.
        for error in [
            PlatformError::Timeout,
            backend("capture worker unavailable"),
            backend("capture cancelled"),
            backend("rejected"),
            PlatformError::NotFound,
            PlatformError::Locked,
            PlatformError::Unsupported("x"),
        ] {
            assert_eq!(
                set_portals_failure(&error),
                SetPortalsFailure::Uncertain,
                "{error:?}"
            );
        }
    }

    #[test]
    fn a_rejection_reads_once_and_unmarks_to_uncertain() {
        let marked = rejected(backend("portal outside output"));
        assert_eq!(
            marked.to_string(),
            format!("{PORTALS_REJECTED}portal outside output")
        );
        // Marking twice changes nothing.
        assert_eq!(
            rejected(PlatformError::Backend(marked.to_string())).to_string(),
            marked.to_string()
        );
        let unmarked = unmark(marked);
        assert_eq!(set_portals_failure(&unmarked), SetPortalsFailure::Uncertain);
        assert!(unmarked.to_string().contains("portal outside output"));
        // Errors that are not rejections pass through untouched.
        assert!(matches!(
            unmark(PlatformError::Timeout),
            PlatformError::Timeout
        ));
    }
}
