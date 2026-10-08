//! Twin display client (WP-W3.1b): open the IddCx control interface, add/resize/remove twins, heartbeat.
#![allow(unsafe_code)]

mod device;
mod lane;
mod topology;

use self::device::{ControlDevice, locate};
use self::lane::Lane;
use crate::model::{
    cpd::HEARTBEAT_INTERVAL_MS,
    journal::JournalFile,
    twin::OwnPath,
    twin_ledger::{TwinBackend, TwinLedger, recover_ledger},
};
use crosspane_platform::PlatformError;
use std::{
    fmt,
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub use crate::model::twin::{Refusal, TwinDisplay, TwinError, TwinKey, TwinMode};
pub use crate::model::twin_ledger::TwinStartup;

/// Client options. `heartbeat: false` is only for the lease-expiry probe row: without beats, every
/// lane loses its lease after `LEASE_MS` and the driver retires its twin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TwinConfig {
    pub heartbeat: bool,
}

impl Default for TwinConfig {
    fn default() -> Self {
        Self { heartbeat: true }
    }
}

/// The twin client: one lane per twin, a heartbeat thread that renews their leases, and the
/// journal-ordered ledger. Every method runs on the caller's thread. The heartbeat thread only
/// beats lanes that are registered with it.
pub struct TwinClient {
    ledger: TwinLedger<NativeBackend>,
    beats: Arc<Beats>,
    heartbeat: Option<JoinHandle<()>>,
}

impl fmt::Debug for TwinClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TwinClient").finish_non_exhaustive()
    }
}

impl TwinClient {
    /// Opens the client with heartbeats on. `Absent` means the driver is not installed.
    pub fn open() -> Result<Self, TwinError> {
        Self::open_with(TwinConfig::default())
    }

    /// Finds the control interface and, when `config.heartbeat` is set, starts the heartbeat
    /// thread. Nothing is added, so the driver holds no twin for this client yet.
    pub fn open_with(config: TwinConfig) -> Result<Self, TwinError> {
        let beats = Arc::new(Beats::default());
        let backend = NativeBackend::new(locate()?, beats.clone());
        let heartbeat = config
            .heartbeat
            .then(|| spawn_heartbeat(beats.clone()))
            .transpose()?;
        Ok(Self {
            ledger: TwinLedger::new(backend),
            beats,
            heartbeat,
        })
    }

    /// Adds a twin in `mode`. The ledger is saved before each IOCTL that changes a twin.
    pub fn add(
        &mut self,
        journal: &mut JournalFile,
        mode: TwinMode,
    ) -> Result<TwinDisplay, TwinError> {
        self.ledger.add(journal, mode)
    }

    /// Changes a twin's mode by REMOVE then ADD on its lane. The monitor id changes.
    pub fn resize(
        &mut self,
        journal: &mut JournalFile,
        key: TwinKey,
        mode: TwinMode,
    ) -> Result<TwinDisplay, TwinError> {
        self.ledger.resize(journal, key, mode)
    }

    /// Removes a twin and closes its lane.
    pub fn remove(&mut self, journal: &mut JournalFile, key: TwinKey) -> Result<(), TwinError> {
        self.ledger.remove(journal, key)
    }

    /// The fast rollback: closes the lane and forgets the twin's record.
    pub fn discard(&mut self, journal: &mut JournalFile, key: TwinKey) -> Result<(), TwinError> {
        self.ledger.discard(journal, key)
    }

    /// Closes and forgets every twin whose lease was lost, and returns their keys.
    pub fn reap_lost(&mut self, journal: &mut JournalFile) -> Result<Vec<TwinKey>, TwinError> {
        self.ledger.reap_lost(journal)
    }

    /// The Windows display of a live twin.
    pub fn display(&self, key: TwinKey) -> Option<&TwinDisplay> {
        self.ledger.display(key)
    }

    /// How many of our own active display paths exist right now.
    pub fn own_path_count(&mut self) -> Result<usize, TwinError> {
        Ok(self.ledger.backend_mut().own_paths()?.len())
    }

    /// The interface path of our own display adapter. It is the only identity used to tell our
    /// paths from other adapters'.
    pub fn adapter_interface(&self) -> &str {
        self.ledger.backend().adapter()
    }
}

impl Drop for TwinClient {
    fn drop(&mut self) {
        self.beats.request_stop();
        // Closing a lane makes the driver retire its twin. The journal keeps its records, so the
        // next startup clears any that outlive this process.
        self.ledger.close_all();
        if let Some(heartbeat) = self.heartbeat.take() {
            let _ = heartbeat.join();
        }
    }
}

/// Startup recovery (see `recover_ledger`). Without a control interface no twin can exist, so the
/// ledger is cleared and saved, and `driver_present` is false. Otherwise the ledger waits for stale
/// twins to vanish, confirms a fresh handle lists none, and then clears it.
pub fn recover_startup(journal: &mut JournalFile) -> Result<TwinStartup, TwinError> {
    let device = match locate() {
        Ok(device) => device,
        Err(TwinError::Absent) => {
            let journaled = journal.twins().count();
            journal.clear_twins();
            journal.save().map_err(journal_error)?;
            return Ok(TwinStartup {
                driver_present: false,
                journaled,
                ..TwinStartup::default()
            });
        }
        Err(error) => return Err(error),
    };
    let mut backend = NativeBackend::new(device, Arc::new(Beats::default()));
    recover_ledger(&mut backend, journal)
}

/// Our own display adapter's interface path, when the driver is present.
pub fn own_adapter_interface() -> Option<String> {
    locate().ok().map(|device| device.adapter)
}

/// The OS side of the ledger. A lane is shared with the heartbeat thread, so it is an `Arc`.
struct NativeBackend {
    interface: String,
    adapter: String,
    beats: Arc<Beats>,
    epoch: Instant,
}

impl NativeBackend {
    fn new(device: ControlDevice, beats: Arc<Beats>) -> Self {
        Self {
            interface: device.interface,
            adapter: device.adapter,
            beats,
            epoch: Instant::now(),
        }
    }

    fn adapter(&self) -> &str {
        &self.adapter
    }
}

impl TwinBackend for NativeBackend {
    type Lane = Arc<Lane>;

    fn open_lane(&mut self) -> Result<Self::Lane, TwinError> {
        let lane = Arc::new(Lane::open(&self.interface)?);
        self.beats.register(lane.clone());
        Ok(lane)
    }

    fn add(
        &mut self,
        lane: &Self::Lane,
        mode: TwinMode,
        timeout_ms: u32,
    ) -> Result<u32, TwinError> {
        lane.add(mode, timeout_ms)
    }

    fn remove(
        &mut self,
        lane: &Self::Lane,
        monitor_id: u32,
        timeout_ms: u32,
    ) -> Result<(), TwinError> {
        lane.remove(monitor_id, timeout_ms)
    }

    fn list(&mut self, lane: &Self::Lane, timeout_ms: u32) -> Result<Option<u32>, TwinError> {
        lane.list(timeout_ms)
    }

    fn close_lane(&mut self, lane: Self::Lane) {
        // The last Arc closes the handle, which retires the lane's twin. A beat in flight keeps
        // its own clone, so the close waits for that beat to finish.
        self.beats.unregister(&lane);
    }

    fn lane_lost(&self, lane: &Self::Lane) -> bool {
        lane.lost()
    }

    fn own_paths(&mut self) -> Result<Vec<OwnPath>, TwinError> {
        topology::own_paths(&self.adapter)
    }

    fn dpi(&mut self, rect: [i32; 4]) -> Result<u32, TwinError> {
        topology::monitor_dpi(rect)
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn sleep_ms(&mut self, ms: u32) {
        thread::sleep(Duration::from_millis(u64::from(ms)));
    }
}

/// The lanes the heartbeat thread renews, and the stop flag it waits on.
#[derive(Default)]
struct Beats {
    lanes: Mutex<Vec<Arc<Lane>>>,
    stop: Mutex<bool>,
    wake: Condvar,
}

impl Beats {
    fn register(&self, lane: Arc<Lane>) {
        lock(&self.lanes).push(lane);
    }

    fn unregister(&self, lane: &Arc<Lane>) {
        lock(&self.lanes).retain(|held| !Arc::ptr_eq(held, lane));
    }

    /// Registered lanes that have not lost their lease. Each is cloned, so a beat in flight keeps
    /// its lane alive even if the owner closes it meanwhile.
    fn live(&self) -> Vec<Arc<Lane>> {
        lock(&self.lanes)
            .iter()
            .filter(|lane| !lane.lost())
            .cloned()
            .collect()
    }

    fn stopped(&self) -> bool {
        *lock(&self.stop)
    }

    fn request_stop(&self) {
        *lock(&self.stop) = true;
        self.wake.notify_all();
    }

    /// Waits one heartbeat interval. Returns false once stop is requested, which also covers a
    /// stop that came before the wait began.
    fn pause(&self) -> bool {
        let stop = lock(&self.stop);
        if *stop {
            return false;
        }
        let interval = Duration::from_millis(u64::from(HEARTBEAT_INTERVAL_MS));
        let (stop, _) = self
            .wake
            .wait_timeout(stop, interval)
            .unwrap_or_else(PoisonError::into_inner);
        !*stop
    }
}

/// Locks through poisoning. The guarded values are plain lists and flags, so a panic elsewhere
/// never leaves them half-written.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn spawn_heartbeat(beats: Arc<Beats>) -> Result<JoinHandle<()>, TwinError> {
    thread::Builder::new()
        .name("crosspane-twin-heartbeat".into())
        .spawn(move || heartbeat_loop(&beats))
        .map_err(|error| {
            let code = error
                .raw_os_error()
                .and_then(|code| u32::try_from(code).ok())
                .unwrap_or(0);
            TwinError::Native("heartbeat thread", code)
        })
}

/// Beats every live lane once per interval, until stop. With no lanes it issues no IOCTL.
fn heartbeat_loop(beats: &Beats) {
    while beats.pause() {
        for lane in beats.live() {
            if beats.stopped() {
                return;
            }
            lane.beat();
        }
    }
}

fn journal_error(error: PlatformError) -> TwinError {
    TwinError::Journal(error.to_string())
}
