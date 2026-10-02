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
mod events;
mod wayland;
use wayland::Refresh;

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
    /// Hyprland's IPC for the same compositor.
    fn ipc(&self, timeout: Duration) -> Result<HyprIpc, PlatformError> {
        match self {
            Source::Environment => {
                let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
                    .map_err(|_| PlatformError::Unsupported("not under Hyprland"))?;
                let runtime = std::env::var_os("XDG_RUNTIME_DIR")
                    .ok_or(PlatformError::Unsupported("no runtime directory"))?;
                Ok(HyprIpc::new(&signature, &PathBuf::from(runtime), timeout))
            }
            Source::Pinned(endpoint) => Ok(HyprIpc::new(
                &endpoint.signature,
                &endpoint.runtime_dir,
                timeout,
            )),
        }
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
        std::thread::Builder::new()
            .name("hypr-capture".into())
            .spawn(move || worker(requests, delivery, gate, control, ready, source))
            .map_err(backend)?;
        let handle = Self {
            commands,
            abort,
            nest,
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
        self.call(Operation::Subscribe(sink)).map(|_| ())
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
    fn set_monitor_local_activity(&mut self, _: bool) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported(
            "Wayland local activity monitoring",
        ))
    }
}
impl Drop for HyprlandCapture {
    fn drop(&mut self) {
        self.abort.abort();
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
    fn detached(nest: Option<Endpoint>) -> (HyprlandCapture, mpsc::Receiver<Command>) {
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
