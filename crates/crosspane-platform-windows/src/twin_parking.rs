//! Windows M2: parks a window alone on a native twin display (WP-W3.2 T18). One owner thread runs
//! every native call under a bounded deadline, as the M1 facade does. The orchestration and its
//! rollbacks live in `model::twin_parking`; this file supplies the Win32-backed [`TwinOps`].
#![allow(unsafe_code)]

pub use crate::model::twin_parking::{WINDOW_COMMITTED_NAME, WINDOW_PENDING_NAME};
use crate::{
    model::{
        geometry::DisplayIds,
        journal::JournalFile,
        parking::{Controller, MirrorJournalStore, MirrorRecovery},
        twin::{TWIN_ABSENT_REASON, TWIN_UNAVAILABLE_REASON},
        twin_parking::{
            LOCATE_TIMEOUT_MS, TWIN_DISABLED_REASON, TwinOps, TwinParking, find_twin_probe,
        },
    },
    parking::{Binding, Deadline, DpiScope, Port, Shared},
    twin::{TwinClient, TwinDisplay, TwinError, TwinKey, TwinMode, TwinStartup},
    window::{MonitorReader, WindowResolver},
};
use crosspane_platform::{Parked, PlatformError, WindowParking};
use crosspane_types::{
    geom::PixelSize,
    id::{DisplayId, WindowId},
};
use std::{
    fmt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// How long a failed twin add, or a failed ledger recovery, refuses new twins.
const TWIN_RETRY: Duration = Duration::from_secs(30);
const PARK_BOUND: Duration = Duration::from_millis(7_500);
const RESIZE_BOUND: Duration = Duration::from_secs(12);
const RESTORE_BOUND: Duration = Duration::from_secs(6);
const RECOVER_BOUND: Duration = Duration::from_secs(30);
const QUERY_BOUND: Duration = Duration::from_secs(2);
/// How long the owner waits for a call before it checks for a stop request.
const POLL: Duration = Duration::from_millis(25);
/// How often the owner reaps lost twins while no call arrives.
const IDLE_REAP: Duration = Duration::from_secs(1);
const LOCATE_POLL: Duration = Duration::from_millis(50);
/// How long `Drop` waits for the owner to finish its current call.
const DROP_BOUND: Duration = Duration::from_secs(2);

fn backend(text: &'static str) -> PlatformError {
    PlatformError::Backend(text.into())
}

/// What startup recovery found. `unavailable` is the reason twin parking is off for this run, when
/// it is off. An absent driver counts as off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TwinParkingRecovery {
    pub windows: MirrorRecovery,
    pub driver_present: bool,
    pub twins_journaled: usize,
    pub unavailable: Option<&'static str>,
}

/// One call's bounds, checked by the native operations before each change.
struct Bounds {
    shared: Arc<Shared>,
    until: Instant,
    abandoned: Arc<AtomicBool>,
}

impl Bounds {
    fn check(&self) -> Result<(), PlatformError> {
        if !self.shared.alive.load(Ordering::Acquire) || self.shared.fault.load(Ordering::Acquire) {
            return Err(backend("twin owner unavailable"));
        }
        if self.abandoned.load(Ordering::Acquire) || Instant::now() >= self.until {
            return Err(PlatformError::Timeout);
        }
        Ok(())
    }
}

/// Why new twins are refused, and until when. `until: None` refuses for the rest of the run.
struct Retry {
    until: Option<Instant>,
    cause: TwinError,
}

impl Retry {
    /// Refuses new twins for `TWIN_RETRY`. A clock that cannot hold that instant refuses for the
    /// run.
    fn timed(cause: TwinError) -> Self {
        Self {
            until: Instant::now().checked_add(TWIN_RETRY),
            cause,
        }
    }

    fn for_run(cause: TwinError) -> Self {
        Self { until: None, cause }
    }

    /// The refusal after a failed ledger recovery. Stale twins keep M2 off for the run (D1). A
    /// journal failure is replayed as a protocol failure, which maps to `Unsupported`, so the agent
    /// falls back to M1. A `Journal` cause would map to `Backend`, which the agent does not fall
    /// back from.
    fn for_recovery(error: TwinError) -> Self {
        match error {
            TwinError::Stale(_) => Self::for_run(error),
            TwinError::Journal(_) => {
                Self::timed(TwinError::Protocol("twin ledger could not be saved"))
            }
            other => Self::timed(other),
        }
    }

    fn active(&self, now: Instant) -> bool {
        self.until.is_none_or(|until| now < until)
    }
}

/// The source's monitor reader and display ids. Locating a twin needs both.
struct Locator {
    monitors: MonitorReader,
    ids: Arc<Mutex<DisplayIds>>,
}

/// The owner thread's native state. The client opens on the first park, and the ledger is read by
/// startup recovery.
struct Natives {
    enabled: bool,
    ledger_path: PathBuf,
    client: Option<TwinClient>,
    ledger: Option<JournalFile>,
    retry: Option<Retry>,
    locator: Option<Locator>,
}

impl Natives {
    fn ops<'a>(&'a mut self, bounds: &'a Bounds) -> NativeTwinOps<'a> {
        NativeTwinOps {
            client: &mut self.client,
            ledger: &mut self.ledger,
            ledger_path: &self.ledger_path,
            retry: &mut self.retry,
            locator: self.locator.as_ref(),
            bounds,
        }
    }
}

/// Opens the ledger and recovers the twins it records. The ledger is returned only when recovery
/// succeeds.
fn recover_ledger(path: &Path) -> Result<(JournalFile, TwinStartup), TwinError> {
    let mut journal = JournalFile::open(path)
        .map_err(|_| TwinError::Protocol("twin ledger could not be read"))?;
    let startup = crate::twin::recover_startup(&mut journal)?;
    Ok((journal, startup))
}

/// The native [`TwinOps`]: the client and ledger on the owner thread, with the twin lookup.
struct NativeTwinOps<'a> {
    client: &'a mut Option<TwinClient>,
    ledger: &'a mut Option<JournalFile>,
    ledger_path: &'a Path,
    retry: &'a mut Option<Retry>,
    locator: Option<&'a Locator>,
    bounds: &'a Bounds,
}

impl NativeTwinOps<'_> {
    fn bounded(&self) -> Result<(), TwinError> {
        self.bounds
            .check()
            .map_err(|_| TwinError::Timeout("the twin facade bound"))
    }

    /// The refusal in force, if any. An expired refusal is cleared.
    fn refusal(&mut self) -> Option<TwinError> {
        let retry = self.retry.as_ref()?;
        let (active, cause) = (retry.active(Instant::now()), retry.cause.clone());
        if active {
            return Some(cause);
        }
        *self.retry = None;
        None
    }

    /// The open ledger and the client. Requires the ledger, so no driver is touched without one.
    fn live(&mut self) -> Result<(&mut TwinClient, &mut JournalFile), TwinError> {
        let journal = self
            .ledger
            .as_mut()
            .ok_or(TwinError::Protocol("twin ledger is not open"))?;
        let client = self.client.as_mut().ok_or(TwinError::UnknownKey)?;
        Ok((client, journal))
    }

    /// As [`Self::live`], but the client opens on first use.
    fn open(&mut self) -> Result<(&mut TwinClient, &mut JournalFile), TwinError> {
        if self.ledger.is_some() && self.client.is_none() {
            *self.client = Some(TwinClient::open()?);
        }
        self.live()
    }
}

impl TwinOps for NativeTwinOps<'_> {
    fn ready(&mut self) -> Result<(), TwinError> {
        self.bounded()?;
        if let Some(cause) = self.refusal() {
            return Err(cause);
        }
        if self.ledger.is_none() {
            match recover_ledger(self.ledger_path) {
                Ok((journal, _)) => *self.ledger = Some(journal),
                Err(error) => {
                    // Return the cause the refusal replays. A journal failure must not become `Backend`.
                    let retry = Retry::for_recovery(error);
                    let cause = retry.cause.clone();
                    *self.retry = Some(retry);
                    return Err(cause);
                }
            }
        }
        self.open().map(|_| ())
    }

    fn add(&mut self, mode: TwinMode) -> Result<TwinDisplay, TwinError> {
        self.bounded()?;
        let added = self
            .open()
            .and_then(|(client, journal)| client.add(journal, mode));
        match added {
            Ok(display) => Ok(display),
            Err(error) => {
                if !matches!(error, TwinError::Journal(_)) {
                    *self.retry = Some(Retry::timed(error.clone()));
                }
                Err(error)
            }
        }
    }

    fn resize(&mut self, key: TwinKey, mode: TwinMode) -> Result<TwinDisplay, TwinError> {
        self.bounded()?;
        let (client, journal) = self.live()?;
        client.resize(journal, key, mode)
    }

    fn remove(&mut self, key: TwinKey) -> Result<(), TwinError> {
        if let Err(error) = self.bounded() {
            // Out of time: the lane must still close, so the twin is not left registered.
            self.discard(key);
            return Err(error);
        }
        let (client, journal) = self.live()?;
        client.remove(journal, key)
    }

    /// Never gated on the bound: it only closes a local lane, and the driver retires the twin.
    fn discard(&mut self, key: TwinKey) {
        if let Ok((client, journal)) = self.live() {
            // A failed save leaves the record for the next startup to clear. The lane closes anyway.
            let _ = client.discard(journal, key);
        }
    }

    fn locate(&mut self, display: &TwinDisplay) -> Result<(DisplayId, [i32; 4]), PlatformError> {
        let locator = self
            .locator
            .ok_or(PlatformError::Unsupported("unbound Windows mirror parking"))?;
        let until = Instant::now() + Duration::from_millis(u64::from(LOCATE_TIMEOUT_MS));
        loop {
            self.bounds.check()?;
            // A topology change can fail one coherent read; it is retried until the bound.
            if let Ok(probes) = (locator.monitors)()
                && let Some(probe) = find_twin_probe(&probes, &display.gdi_name, display.rect)?
            {
                let mut ids = locator
                    .ids
                    .lock()
                    .map_err(|_| backend("twin display identities"))?;
                let id = ids
                    .assign(&probe.device_path)
                    .map_err(|_| backend("twin display identity"))?;
                return Ok((id, probe.rc_monitor));
            }
            let now = Instant::now();
            if now >= until {
                return Err(PlatformError::Timeout);
            }
            thread::sleep(LOCATE_POLL.min(until - now));
        }
    }

    fn lost(&mut self) -> Vec<TwinKey> {
        if self.bounded().is_err() {
            return Vec::new();
        }
        let reaped = match self.live() {
            Ok((client, journal)) => client.reap_lost(journal),
            Err(_) => return Vec::new(),
        };
        match reaped {
            Ok(keys) => keys,
            Err(_) => {
                // The lanes closed but their records are unsaved, so the lost keys are unknown.
                // The stream stops, as after a failed journal save. The next start clears the
                // records.
                self.bounds.shared.fault.store(true, Ordering::Release);
                Vec::new()
            }
        }
    }
}

enum Operation {
    Bind(Binding),
    Startup,
    Park(WindowId, PixelSize, f64),
    Resize(WindowId, PixelSize, f64),
    Geometry(WindowId),
    Fullscreen(WindowId, bool),
    Restore(WindowId),
    Recover,
}

impl Operation {
    fn bound(&self) -> Duration {
        match self {
            Self::Park(..) => PARK_BOUND,
            Self::Resize(..) => RESIZE_BOUND,
            Self::Restore(_) => RESTORE_BOUND,
            Self::Startup | Self::Recover => RECOVER_BOUND,
            Self::Bind(_) | Self::Geometry(_) | Self::Fullscreen(..) => QUERY_BOUND,
        }
    }
}

enum Reply {
    Unit,
    Geometry(Parked),
    Recovery(TwinParkingRecovery),
    Windows(Vec<WindowId>),
}

struct Call {
    deadline: Deadline,
    reply: mpsc::SyncSender<Result<Reply, PlatformError>>,
    operation: Operation,
}

/// The owner thread's state. Only this thread touches the window originals, twins and ledger.
struct Owner {
    twin: TwinParking<Port>,
    natives: Natives,
}

impl Owner {
    fn serve(&mut self, receive: &mpsc::Receiver<Call>, shared: &Arc<Shared>) {
        let mut reaped = Instant::now();
        while shared.alive.load(Ordering::Acquire) && !shared.fault.load(Ordering::Acquire) {
            let call = match receive.recv_timeout(POLL) {
                Ok(call) => call,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if reaped.elapsed() >= IDLE_REAP {
                        self.idle_reap(shared);
                        reaped = Instant::now();
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            let Call {
                deadline,
                reply,
                operation,
            } = call;
            let bounds = Bounds {
                shared: shared.clone(),
                until: deadline.until,
                abandoned: deadline.abandoned.clone(),
            };
            self.twin.windows.port.deadline = Some(deadline);
            let result = bounds.check().and_then(|()| self.run(operation, &bounds));
            if result.is_err() && Instant::now() >= bounds.until {
                // A call that fails past its deadline may have left a change half-made. The
                // stream stops before the reply, so the owner drops its client and no lane
                // outlives the call.
                shared.fault.store(true, Ordering::Release);
            }
            let _ = reply.send(result);
            self.twin.windows.port.deadline = None;
        }
    }

    /// Reaps lost twins while no call is queued. Nothing is reaped before a client exists.
    fn idle_reap(&mut self, shared: &Arc<Shared>) {
        if self.natives.client.is_none() {
            return;
        }
        let until = Instant::now() + RECOVER_BOUND;
        let abandoned = Arc::new(AtomicBool::new(false));
        let bounds = Bounds {
            shared: shared.clone(),
            until,
            abandoned: abandoned.clone(),
        };
        self.twin.windows.port.deadline = Some(Deadline { until, abandoned });
        if let Ok(_dpi) = DpiScope::new() {
            self.reap(&bounds);
        }
        self.twin.windows.port.deadline = None;
    }

    fn run(&mut self, operation: Operation, bounds: &Bounds) -> Result<Reply, PlatformError> {
        let _dpi = DpiScope::new()?;
        self.reap(bounds);
        match operation {
            Operation::Bind(binding) => self.bind(binding).map(|()| Reply::Unit),
            Operation::Startup => self.startup().map(Reply::Recovery),
            Operation::Park(id, size, scale) => {
                if !self.natives.enabled {
                    return Err(PlatformError::Unsupported(TWIN_DISABLED_REASON));
                }
                let mut ops = self.natives.ops(bounds);
                self.twin
                    .park(&mut ops, id, size, scale)
                    .map(Reply::Geometry)
            }
            Operation::Resize(id, size, scale) => {
                let mut ops = self.natives.ops(bounds);
                self.twin
                    .resize(&mut ops, id, size, scale)
                    .map(Reply::Geometry)
            }
            Operation::Geometry(id) => self.twin.geometry(id).map(Reply::Geometry),
            Operation::Fullscreen(id, on) => self.twin.set_fullscreen(id, on).map(|()| Reply::Unit),
            Operation::Restore(id) => {
                let mut ops = self.natives.ops(bounds);
                self.twin.restore(&mut ops, id).map(|()| Reply::Unit)
            }
            Operation::Recover => {
                let mut ops = self.natives.ops(bounds);
                self.twin.recover(&mut ops).map(Reply::Windows)
            }
        }
    }

    fn reap(&mut self, bounds: &Bounds) {
        let mut ops = self.natives.ops(bounds);
        self.twin.reap_lost(&mut ops);
    }

    fn bind(&mut self, binding: Binding) -> Result<(), PlatformError> {
        if self.twin.windows.port.binding.is_some() {
            return Err(backend("twin source already bound"));
        }
        self.natives.locator = Some(Locator {
            monitors: binding.monitors.clone(),
            ids: binding.ids.clone(),
        });
        self.twin.windows.port.binding = Some(binding);
        self.twin.windows.bind();
        Ok(())
    }

    /// Window originals first, as in M1. Then the ledger. A ledger failure never fails startup: it
    /// sets `unavailable` and refuses new twins, so the run uses M1.
    fn startup(&mut self) -> Result<TwinParkingRecovery, PlatformError> {
        let windows = self.twin.windows.recover_startup()?;
        let mut report = TwinParkingRecovery {
            windows,
            ..TwinParkingRecovery::default()
        };
        if !self.natives.enabled {
            report.unavailable = Some(TWIN_DISABLED_REASON);
            return Ok(report);
        }
        match recover_ledger(&self.natives.ledger_path) {
            Ok((journal, startup)) => {
                self.natives.ledger = Some(journal);
                self.natives.retry = None;
                report.driver_present = startup.driver_present;
                report.twins_journaled = startup.journaled;
                if !startup.driver_present {
                    report.unavailable = Some(TWIN_ABSENT_REASON);
                }
            }
            Err(error) => {
                report.unavailable = Some(TWIN_UNAVAILABLE_REASON);
                self.natives.retry = Some(Retry::for_recovery(error));
            }
        }
        Ok(report)
    }
}

/// The M2 facade: a send handle to one serial owner thread. Native handles never leave that thread.
pub struct WindowsTwinParking {
    shared: Arc<Shared>,
    commands: mpsc::SyncSender<Call>,
    done: mpsc::Receiver<()>,
    worker: Option<JoinHandle<()>>,
}

impl fmt::Debug for WindowsTwinParking {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WindowsTwinParking").finish_non_exhaustive()
    }
}

impl WindowsTwinParking {
    /// `store` holds the window originals, in the M1 journal format. `ledger` is the twin ledger
    /// file. With `twins == false` (scratch modes) no park reaches the driver.
    pub fn new(
        store: Box<dyn MirrorJournalStore>,
        ledger: PathBuf,
        twins: bool,
    ) -> Result<Self, PlatformError> {
        let shared = Arc::new(Shared {
            alive: AtomicBool::new(true),
            fault: AtomicBool::new(false),
        });
        let port = Port {
            shared: shared.clone(),
            binding: None,
            deadline: None,
        };
        // Journal reads and republishing are private file I/O, as in M1. No native state exists.
        let windows = Controller::new(store, port)?;
        let mut owner = Owner {
            twin: TwinParking::new(windows),
            natives: Natives {
                enabled: twins,
                ledger_path: ledger,
                client: None,
                ledger: None,
                retry: None,
                locator: None,
            },
        };
        let (commands, receive) = mpsc::sync_channel::<Call>(1);
        let (finished, done) = mpsc::sync_channel(1);
        let worker_shared = shared.clone();
        let worker = thread::Builder::new()
            .name("crosspane-twin-owner".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    owner.serve(&receive, &worker_shared);
                }));
                if result.is_err() {
                    worker_shared.fault.store(true, Ordering::Release);
                }
                worker_shared.alive.store(false, Ordering::Release);
                let _ = finished.send(());
            })
            .map_err(|_| backend("twin owner thread"))?;
        Ok(Self {
            shared,
            commands,
            done,
            worker: Some(worker),
        })
    }

    /// Restores the window originals, then recovers the twin ledger. Call before anything else.
    pub fn recover_startup(&mut self) -> Result<TwinParkingRecovery, PlatformError> {
        match self.call(Operation::Startup)? {
            Reply::Recovery(report) => Ok(report),
            _ => Err(backend("twin reply")),
        }
    }

    /// Binds the source's window resolver, display ids and monitor reader. Once only.
    pub fn bind_source(
        &mut self,
        resolver: WindowResolver,
        ids: Arc<Mutex<DisplayIds>>,
        monitors: MonitorReader,
    ) -> Result<(), PlatformError> {
        match self.call(Operation::Bind(Binding {
            resolver,
            ids,
            monitors,
        }))? {
            Reply::Unit => Ok(()),
            _ => Err(backend("twin reply")),
        }
    }

    fn call(&self, operation: Operation) -> Result<Reply, PlatformError> {
        if !self.shared.alive.load(Ordering::Acquire) || self.shared.fault.load(Ordering::Acquire) {
            return Err(backend("twin owner unavailable"));
        }
        let until = Instant::now() + operation.bound();
        let abandoned = Arc::new(AtomicBool::new(false));
        let (reply, receive) = mpsc::sync_channel(1);
        self.commands
            .try_send(Call {
                deadline: Deadline {
                    until,
                    abandoned: abandoned.clone(),
                },
                reply,
                operation,
            })
            .map_err(|_| backend("twin owner busy"))?;
        match receive.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok(result) => result,
            Err(_) => {
                // The owner finishes its current native call but starts no more. The stream faults.
                abandoned.store(true, Ordering::Release);
                self.shared.fault.store(true, Ordering::Release);
                Err(PlatformError::Timeout)
            }
        }
    }

    fn geometry_reply(&self, operation: Operation) -> Result<Parked, PlatformError> {
        match self.call(operation)? {
            Reply::Geometry(parked) => Ok(parked),
            _ => Err(backend("twin reply")),
        }
    }
}

impl WindowParking for WindowsTwinParking {
    fn park(
        &mut self,
        window: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError> {
        self.geometry_reply(Operation::Park(window, size, scale))
    }

    fn resize(
        &mut self,
        window: WindowId,
        size: PixelSize,
        scale: f64,
    ) -> Result<Parked, PlatformError> {
        self.geometry_reply(Operation::Resize(window, size, scale))
    }

    fn geometry(&self, window: WindowId) -> Result<Parked, PlatformError> {
        self.geometry_reply(Operation::Geometry(window))
    }

    fn set_fullscreen(&mut self, window: WindowId, on: bool) -> Result<(), PlatformError> {
        match self.call(Operation::Fullscreen(window, on))? {
            Reply::Unit => Ok(()),
            _ => Err(backend("twin reply")),
        }
    }

    fn restore(&mut self, window: WindowId) -> Result<(), PlatformError> {
        match self.call(Operation::Restore(window))? {
            Reply::Unit => Ok(()),
            _ => Err(backend("twin reply")),
        }
    }

    fn recover(&mut self) -> Result<Vec<WindowId>, PlatformError> {
        match self.call(Operation::Recover)? {
            Reply::Windows(windows) => Ok(windows),
            _ => Err(backend("twin reply")),
        }
    }
}

impl Drop for WindowsTwinParking {
    fn drop(&mut self) {
        self.shared.alive.store(false, Ordering::Release);
        if let Some(worker) = self.worker.take()
            && self.done.recv_timeout(DROP_BOUND).is_ok()
        {
            let _ = worker.join();
        }
    }
}
