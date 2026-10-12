//! Serial parking calls with a bounded ready queue and newest queued resize per window.
//! Startup recovery finishes before this worker is built. Jobs stay cancellable until the
//! worker claims them under the shared scheduler lock; the channel carries wakeups only.
//! Restores awaiting a full ready queue retain FIFO order and cannot be overtaken by new
//! parks/resizes. A shutdown timeout leaves recovery to the next start.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use crosspane_engine::Failure;
use crosspane_platform::{Parked, PlatformError, WindowParking};
use crosspane_types::geom::{PixelSize, PointDevice};
use crosspane_types::id::{DisplayId, WindowId};

use crate::agent::Event;
use crate::lifecycle::Parking;

pub(crate) const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);
const QUEUE_CAP: usize = 64;

/// Gives a window the keyboard focus (`WindowSource::activate`). It runs on the worker's thread, in
/// the same FIFO as the parking calls: the Windows source waits up to two seconds when the OS
/// denies the foreground request, which must not stall the agent loop, and a later park or return
/// of the same window must not overtake the activation.
pub(crate) type Activator = Box<dyn FnMut(WindowId) -> Result<(), PlatformError> + Send>;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Command {
    Park {
        window: WindowId,
        size: PixelSize,
        scale: f64,
    },
    Resize {
        window: WindowId,
        size: PixelSize,
        scale: f64,
        fullscreen: bool,
    },
    Restore {
        window: WindowId,
        place: Option<(DisplayId, PointDevice)>,
    },
    Activate {
        window: WindowId,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Park,
    Resize,
    Restore,
    Activate,
}

impl Command {
    fn window(self) -> WindowId {
        match self {
            Self::Park { window, .. }
            | Self::Resize { window, .. }
            | Self::Restore { window, .. }
            | Self::Activate { window } => window,
        }
    }
    fn kind(self) -> Kind {
        match self {
            Self::Park { .. } => Kind::Park,
            Self::Resize { .. } => Kind::Resize,
            Self::Restore { .. } => Kind::Restore,
            Self::Activate { .. } => Kind::Activate,
        }
    }
}

/// What the engine is told about a failed platform call.
fn failure_of(error: &PlatformError) -> Failure {
    match error {
        PlatformError::Locked => Failure::Locked,
        PlatformError::SecureInput => Failure::SecureInput,
        PlatformError::PointerButtonHeld => Failure::PointerButtonHeld,
        PlatformError::PermissionDenied(_) => Failure::PermissionDenied,
        _ => Failure::Other,
    }
}

#[derive(Clone, Copy)]
struct Job {
    id: u64,
    command: Command,
}

#[derive(Clone, Debug)]
pub(crate) struct Completion {
    pub(crate) id: u64,
    pub(crate) outcome: Outcome,
    #[cfg(windows)]
    pub(crate) notice: Option<&'static str>,
}

impl Completion {
    pub(crate) fn unavailable(id: u64, command: Command) -> Self {
        Self {
            id,
            #[cfg(windows)]
            notice: None,
            outcome: match command {
                Command::Restore { window, .. } => Outcome::Restored {
                    window,
                    ok: false,
                    failure: Some(Failure::Other),
                },
                Command::Activate { window } => Outcome::Activated {
                    window,
                    result: Err(Failure::Other),
                },
                _ => Outcome::Parked {
                    window: command.window(),
                    result: Err(Failure::Other),
                },
            },
        }
    }
    fn started(&self) -> bool {
        matches!(self.outcome, Outcome::Started { .. })
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Outcome {
    Started {
        window: WindowId,
        kind: Kind,
    },
    Parked {
        window: WindowId,
        result: Result<Parked, Failure>,
    },
    Restored {
        window: WindowId,
        ok: bool,
        /// Why a return failed (`None` when it succeeded). `Locked` and `SecureInput` mean the
        /// session was closed or the OS could not prove it open: the agent retries those when the
        /// session opens again (no window is lost), and leaves every other failure to the
        /// journal's startup recovery.
        failure: Option<Failure>,
    },
    /// The focus request ran (or was refused) on the worker; the focus change itself arrives as a
    /// window event.
    Activated {
        window: WindowId,
        result: Result<(), Failure>,
    },
    Panicked,
}

#[derive(Default)]
struct State {
    ready: VecDeque<Job>,
    waiting_restores: VecDeque<Job>,
    claimed: Option<Job>,
    failed: bool,
    stopping: bool,
    unhandled: VecDeque<Completion>,
}

impl State {
    fn promote(&mut self) {
        while self.ready.len() < QUEUE_CAP {
            let Some(job) = self.waiting_restores.pop_front() else {
                break;
            };
            self.ready.push_back(job);
        }
    }
    fn emit(&mut self, completion: Completion, events: &mpsc::Sender<Event>) {
        self.unhandled.push_back(completion.clone());
        let _ = events.send(Event::Parking(completion));
    }
    fn refuse(&mut self, job: Job, events: &mpsc::Sender<Event>) {
        tracing::warn!(
            window = job.command.window().0,
            operation = job.id,
            "parking command refused: queue full or worker stopped"
        );
        self.emit(Completion::unavailable(job.id, job.command), events);
    }
    fn fail(&mut self, events: &mpsc::Sender<Event>) {
        self.failed = true;
        if let Some(job) = self.claimed.take() {
            self.refuse(job, events);
        }
        while let Some(job) = self
            .ready
            .pop_front()
            .or_else(|| self.waiting_restores.pop_front())
        {
            self.refuse(job, events);
        }
    }
}

type Shared = Arc<Mutex<State>>;

#[cfg(test)]
struct BeforeClaim {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}

/// Only the worker claims jobs and calls the backend; the loop edits unclaimed jobs under lock.
pub(crate) struct Worker {
    wake: mpsc::SyncSender<()>,
    events: mpsc::Sender<Event>,
    state: Shared,
    stopped: mpsc::Receiver<Parking>,
    abandoned: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    can_activate: bool,
    #[cfg(test)]
    next_test_id: u64,
}

impl Worker {
    /// A worker that parks and restores only (the agent starts its own with an activator).
    #[cfg(test)]
    pub(crate) fn start(
        backend: Box<dyn WindowParking>,
        events: mpsc::Sender<Event>,
    ) -> std::io::Result<Self> {
        Self::start_with_activator(backend, None, events)
    }

    /// As `start`, and the worker also carries out `Command::Activate` through `activator`.
    pub(crate) fn start_with_activator(
        backend: Box<dyn WindowParking>,
        activator: Option<Activator>,
        events: mpsc::Sender<Event>,
    ) -> std::io::Result<Self> {
        Self::start_inner(
            backend,
            activator,
            events,
            #[cfg(test)]
            None,
        )
    }

    /// Whether `Command::Activate` can run here. Without an activator the agent focuses windows
    /// itself.
    pub(crate) fn can_activate(&self) -> bool {
        self.can_activate
    }

    fn start_inner(
        backend: Box<dyn WindowParking>,
        activator: Option<Activator>,
        events: mpsc::Sender<Event>,
        #[cfg(test)] before_claim: Option<BeforeClaim>,
    ) -> std::io::Result<Self> {
        let can_activate = activator.is_some();
        let (wake, rx) = mpsc::sync_channel(1);
        let (stopped_tx, stopped) = mpsc::sync_channel(1);
        let state = Arc::new(Mutex::new(State::default()));
        let shared = state.clone();
        let sink = events.clone();
        let abandoned = Arc::new(AtomicBool::new(false));
        let expired = abandoned.clone();
        let thread = std::thread::Builder::new()
            .name("crosspane-parking".into())
            .spawn(move || {
                let mut backend = backend;
                let mut activator = activator;
                let result = catch_unwind(AssertUnwindSafe(|| {
                    run(
                        backend.as_mut(),
                        &mut activator,
                        rx,
                        &sink,
                        &shared,
                        &expired,
                        #[cfg(test)]
                        before_claim,
                    )
                }));
                let outcome = match result {
                    Ok(outcome) => {
                        if expired.load(Ordering::Acquire) || shared.is_poisoned() {
                            std::mem::forget(backend);
                            let _ = stopped_tx.send(Parking::Failed);
                            return;
                        }
                        // Backend Drop belongs to the bounded wait too.
                        if catch_unwind(AssertUnwindSafe(|| drop(backend))).is_err() {
                            Parking::Failed
                        } else {
                            outcome
                        }
                    }
                    Err(_) => {
                        // Mac twin Drop restores windows. Do not mutate a panicked backend or
                        // erase its journal's recovery evidence on this failure path.
                        std::mem::forget(backend);
                        tracing::error!("parking worker panicked; recovery remains pending");
                        if let Ok(mut state) = shared.lock() {
                            state.emit(
                                Completion {
                                    id: 0,
                                    outcome: Outcome::Panicked,
                                    #[cfg(windows)]
                                    notice: None,
                                },
                                &sink,
                            );
                            state.fail(&sink);
                        }
                        Parking::Failed
                    }
                };
                let _ = stopped_tx.send(outcome);
            })?;
        Ok(Self {
            wake,
            events,
            state,
            stopped,
            abandoned,
            thread: Some(thread),
            can_activate,
            #[cfg(test)]
            next_test_id: 1,
        })
    }

    /// IDs are allocated on the agent loop before admission, including refused requests.
    pub(crate) fn enqueue(&mut self, id: u64, command: Command) {
        let job = Job { id, command };
        let Ok(mut state) = self.state.lock() else {
            let _ = self
                .events
                .send(Event::Parking(Completion::unavailable(id, command)));
            return;
        };
        if state.failed || state.stopping {
            state.refuse(job, &self.events);
            return;
        }
        if !matches!(command, Command::Restore { .. }) && !state.waiting_restores.is_empty() {
            state.refuse(job, &self.events);
            return;
        }
        if matches!(command, Command::Resize { .. } | Command::Restore { .. }) {
            // Superseded geometry never runs, even if a Park/Restore separates the two
            // resize requests. The replacement enters at its own arrival position.
            state.ready.retain(|queued| !matches!(queued.command, Command::Resize { window, .. } if window == command.window()));
        }
        if matches!(command, Command::Activate { .. } | Command::Restore { .. }) {
            // A newer focus request replaces a queued one, and a window that is being returned
            // is not focused after the fact. A claimed activation is never cancelled: it is the
            // call in flight, and the return queues behind it.
            state.ready.retain(|queued| !matches!(queued.command, Command::Activate { window } if window == command.window()));
        }
        if matches!(command, Command::Restore { .. }) {
            if state.waiting_restores.is_empty() && state.ready.len() < QUEUE_CAP {
                state.ready.push_back(job);
            } else {
                state.waiting_restores.push_back(job);
            }
        } else if !state.waiting_restores.is_empty() || state.ready.len() >= QUEUE_CAP {
            state.refuse(job, &self.events);
        } else {
            state.ready.push_back(job);
        }
        state.promote();
        drop(state);
        self.wake();
    }

    fn wake(&self) {
        match self.wake.try_send(()) {
            Ok(()) | Err(mpsc::TrySendError::Full(())) => {}
            Err(mpsc::TrySendError::Disconnected(())) => {
                if let Ok(mut state) = self.state.lock() {
                    state.fail(&self.events);
                }
            }
        }
    }

    /// Started and completed notifications have the same operation ID, but distinct identities.
    pub(crate) fn acknowledge(&mut self, completion: &Completion) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        state
            .unhandled
            .iter()
            .position(|c| c.id == completion.id && c.started() == completion.started())
            .and_then(|index| state.unhandled.remove(index))
            .is_some()
    }

    pub(crate) fn shutdown(&mut self, wait: Duration) -> (Parking, Vec<Completion>) {
        let failed = match self.state.lock() {
            Ok(mut state) => {
                state.stopping = true;
                // The claimed job is the only in-flight call. Even a job whose wakeup has
                // been received remains cancellable until claim() takes it under this lock.
                state
                    .ready
                    .retain(|job| matches!(job.command, Command::Restore { .. }));
                state.promote();
                state.failed
            }
            Err(_) => true,
        };
        self.wake();
        let parking = if failed {
            Parking::Failed
        } else {
            match self.stopped.recv_timeout(wait) {
                Ok(outcome) => outcome,
                Err(_) => {
                    self.abandoned.store(true, Ordering::Release);
                    Parking::Failed
                }
            }
        };
        let completions = match self.state.lock() {
            Ok(state) => state.unhandled.iter().cloned().collect(),
            Err(_) => Vec::new(),
        };
        if self.thread.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(thread) = self.thread.take()
        {
            let _ = thread.join();
        }
        (parking, completions)
    }

    #[cfg(test)]
    fn submit(&mut self, command: Command) {
        let id = self.next_test_id;
        self.next_test_id += 1;
        self.enqueue(id, command);
    }

    #[cfg(test)]
    pub(crate) fn pending(&self) -> bool {
        self.state.lock().is_ok_and(|state| {
            state.claimed.is_some()
                || !state.ready.is_empty()
                || !state.waiting_restores.is_empty()
                || !state.unhandled.is_empty()
        })
    }
}

enum Claim {
    Job(Job),
    Recover,
    Wait,
    Failed,
}

fn claim(shared: &Shared, events: &mpsc::Sender<Event>, abandoned: &AtomicBool) -> Claim {
    let Ok(mut state) = shared.lock() else {
        return Claim::Failed;
    };
    if state.failed || abandoned.load(Ordering::Acquire) {
        return Claim::Failed;
    }
    state.promote();
    if let Some(job) = state.ready.pop_front() {
        state.claimed = Some(job);
        state.promote();
        // Publish under the scheduler lock: a later enqueue/claim cannot overtake this start.
        state.emit(
            Completion {
                id: job.id,
                #[cfg(windows)]
                notice: None,
                outcome: Outcome::Started {
                    window: job.command.window(),
                    kind: job.command.kind(),
                },
            },
            events,
        );
        Claim::Job(job)
    } else if state.stopping {
        Claim::Recover
    } else {
        Claim::Wait
    }
}

fn operation(
    backend: &mut dyn WindowParking,
    activator: &mut Option<Activator>,
    job: Job,
) -> Completion {
    #[cfg(windows)]
    let mut notice = None;
    let outcome = match job.command {
        Command::Park {
            window,
            size,
            scale,
        }
        | Command::Resize {
            window,
            size,
            scale,
            ..
        } => {
            let result = match job.command {
                Command::Park { .. } => backend.park(window, size, scale),
                Command::Resize { fullscreen, .. } => {
                    tracing::debug!(
                        operation = job.id,
                        window = window.0,
                        fullscreen,
                        ?size,
                        scale,
                        cause = "ResizeParked",
                        "ensuring parked fullscreen before resize"
                    );
                    match backend.set_fullscreen(window, fullscreen) {
                        Ok(()) => backend.resize(window, size, scale),
                        Err(error @ (PlatformError::Unsupported(_) | PlatformError::Timeout)) => {
                            tracing::debug!(%error, "parked fullscreen request was not confirmed");
                            backend.resize(window, size, scale)
                        }
                        Err(error) => Err(error),
                    }
                }
                _ => unreachable!(),
            }
            .map_err(|error| {
                tracing::warn!(
                    %error,
                    operation = job.id,
                    window = window.0,
                    operation_kind = match job.command {
                        Command::Park { .. } => "park",
                        Command::Resize { .. } => "move",
                        Command::Restore { .. } => "restore",
                        Command::Activate { .. } => "activate",
                    },
                    "parking operation failed"
                );
                #[cfg(windows)]
                if matches!(job.command, Command::Park { .. }) && matches!(&error,
                    PlatformError::Unsupported(reason) if *reason == crosspane_platform_windows::model::parking::PENDING_REPARK_REASON)
                {
                    notice = Some(crosspane_platform_windows::model::parking::PENDING_REPARK_REASON);
                }
                failure_of(&error)
            });
            Outcome::Parked { window, result }
        }
        Command::Activate { window } => {
            // A panic here is the focus call's, not the parking backend's: the worker, and the
            // journal recovery it owns, carry on.
            let result = match activator.as_mut() {
                Some(activate) => catch_unwind(AssertUnwindSafe(|| activate(window)))
                    .unwrap_or_else(|_| {
                        Err(PlatformError::Backend("window activation panicked".into()))
                    }),
                None => Err(PlatformError::Unsupported("no window activator")),
            }
            .map_err(|error| {
                tracing::warn!(
                    %error,
                    operation = job.id,
                    window = window.0,
                    "could not focus a projected window: its keys are held back"
                );
                failure_of(&error)
            });
            Outcome::Activated { window, result }
        }
        Command::Restore { window, place } => {
            let result = match place {
                Some((display, at)) => backend.restore_at(window, display, at),
                None => backend.restore(window),
            }
            .inspect_err(|error| {
                tracing::error!(
                    %error,
                    operation = job.id,
                    window = window.0,
                    operation_kind = "restore",
                    "could not restore a parked window"
                );
            });
            Outcome::Restored {
                window,
                ok: result.is_ok(),
                failure: result.as_ref().err().map(failure_of),
            }
        }
    };
    Completion {
        id: job.id,
        outcome,
        #[cfg(windows)]
        notice,
    }
}

fn run(
    backend: &mut dyn WindowParking,
    activator: &mut Option<Activator>,
    rx: mpsc::Receiver<()>,
    events: &mpsc::Sender<Event>,
    shared: &Shared,
    abandoned: &AtomicBool,
    #[cfg(test)] mut before_claim: Option<BeforeClaim>,
) -> Parking {
    while rx.recv().is_ok() {
        #[cfg(test)]
        if let Some(gate) = before_claim.take() {
            gate.entered.send(()).unwrap();
            gate.release.recv().unwrap();
        }
        loop {
            match claim(shared, events, abandoned) {
                Claim::Job(job) => {
                    let completion = operation(backend, activator, job);
                    let Ok(mut state) = shared.lock() else {
                        return Parking::Failed;
                    };
                    state.claimed = None;
                    state.emit(completion, events);
                }
                Claim::Recover => {
                    return match backend.recover() {
                        Ok(windows) => {
                            tracing::info!(restored = windows.len(), "parked windows restored");
                            if windows.is_empty() {
                                Parking::NothingParked
                            } else {
                                Parking::Restored
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%error, "parked windows not restored; the next start restores them");
                            Parking::Failed
                        }
                    };
                }
                Claim::Wait => break,
                Claim::Failed => return Parking::Failed,
            }
        }
    }
    Parking::Failed
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crosspane_platform::ParkingKind;
    use crosspane_types::geom::{PixelRect, euclid::Point2D};
    use crosspane_types::id::DisplayId;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Kind {
        Park,
        Resize,
        Restore,
        Recover,
    }

    type RestorePlacements = Arc<Mutex<Vec<Option<(DisplayId, PointDevice)>>>>;

    pub(crate) struct Fake {
        observed: mpsc::Sender<(Kind, WindowId, u32)>,
        release: mpsc::Receiver<()>,
        pub(crate) block: Option<Kind>,
        pub(crate) block_count: u32,
        pub(crate) increment_display: bool,
        pub(crate) panic: Option<Kind>,
        pub(crate) park_fails: bool,
        pub(crate) restore_fails: bool,
        pub(crate) display: DisplayId,
        pub(crate) journal: Arc<AtomicBool>,
        pub(crate) restored_at: RestorePlacements,
    }

    pub(crate) struct Controls {
        pub(crate) observed: mpsc::Receiver<(Kind, WindowId, u32)>,
        pub(crate) release: mpsc::Sender<()>,
        pub(crate) journal: Arc<AtomicBool>,
        pub(crate) restored_at: RestorePlacements,
    }

    pub(crate) fn fake(block: Option<Kind>) -> (Fake, Controls) {
        let (observed, calls) = mpsc::channel();
        let (release, releases) = mpsc::channel();
        let journal = Arc::new(AtomicBool::new(false));
        let restored_at = Arc::new(Mutex::new(Vec::new()));
        (
            Fake {
                observed,
                release: releases,
                block,
                block_count: u32::from(block.is_some()),
                increment_display: false,
                panic: None,
                park_fails: false,
                restore_fails: false,
                display: DisplayId(37),
                journal: journal.clone(),
                restored_at: restored_at.clone(),
            },
            Controls {
                observed: calls,
                release,
                journal,
                restored_at,
            },
        )
    }

    impl Fake {
        fn called(&mut self, kind: Kind, window: WindowId, width: u32) {
            if matches!(kind, Kind::Park | Kind::Resize) {
                self.journal.store(true, Ordering::SeqCst);
            }
            self.observed.send((kind, window, width)).unwrap();
            assert_ne!(self.panic, Some(kind), "fixture: parking panic");
            if self.block == Some(kind) {
                self.block_count -= 1;
                if self.block_count == 0 {
                    self.block = None;
                }
                self.release.recv().unwrap();
            }
        }

        fn parked(&self, window: WindowId, size: PixelSize) -> Result<Parked, PlatformError> {
            if self.park_fails {
                return Err(PlatformError::NotFound);
            }
            Ok(Parked {
                fullscreen: false,
                window,
                kind: ParkingKind::Twin,
                display: self.display,
                content: PixelRect::new(
                    Point2D::new(0, 0),
                    Point2D::new(size.width as i32, size.height as i32),
                ),
            })
        }
    }

    impl WindowParking for Fake {
        fn set_fullscreen(&mut self, _: WindowId, _: bool) -> Result<(), PlatformError> {
            Err(PlatformError::Unsupported("fullscreen is not implemented"))
        }

        fn park(
            &mut self,
            window: WindowId,
            size: PixelSize,
            _scale: f64,
        ) -> Result<Parked, PlatformError> {
            self.called(Kind::Park, window, size.width);
            let parked = self.parked(window, size);
            if self.increment_display {
                self.display = DisplayId(self.display.0 + 1);
            }
            parked
        }
        fn resize(
            &mut self,
            window: WindowId,
            size: PixelSize,
            _scale: f64,
        ) -> Result<Parked, PlatformError> {
            self.called(Kind::Resize, window, size.width);
            self.parked(window, size)
        }
        fn geometry(&self, window: WindowId) -> Result<Parked, PlatformError> {
            self.parked(window, PixelSize::new(1, 1))
        }
        fn restore(&mut self, window: WindowId) -> Result<(), PlatformError> {
            self.restored_at.lock().unwrap().push(None);
            self.called(Kind::Restore, window, 0);
            if self.restore_fails {
                return Err(PlatformError::NotFound);
            }
            self.journal.store(false, Ordering::SeqCst);
            Ok(())
        }
        fn restore_at(
            &mut self,
            window: WindowId,
            display: DisplayId,
            at: PointDevice,
        ) -> Result<(), PlatformError> {
            self.restored_at.lock().unwrap().push(Some((display, at)));
            self.called(Kind::Restore, window, 0);
            if self.restore_fails {
                return Err(PlatformError::NotFound);
            }
            self.journal.store(false, Ordering::SeqCst);
            Ok(())
        }
        fn recover(&mut self) -> Result<Vec<WindowId>, PlatformError> {
            self.called(Kind::Recover, WindowId(0), 0);
            Ok(if self.journal.swap(false, Ordering::SeqCst) {
                vec![WindowId(1)]
            } else {
                Vec::new()
            })
        }
    }

    fn resize(window: u64, width: u32) -> Command {
        Command::Resize {
            fullscreen: false,
            window: WindowId(window),
            size: PixelSize::new(width, 100),
            scale: 1.0,
        }
    }
    fn park(window: u64) -> Command {
        Command::Park {
            window: WindowId(window),
            size: PixelSize::new(100, 100),
            scale: 1.0,
        }
    }
    fn next(worker: &mut Worker, events: &mpsc::Receiver<Event>) -> Outcome {
        loop {
            let Event::Parking(completion) = events.recv_timeout(Duration::from_secs(1)).unwrap()
            else {
                panic!("parking completion expected")
            };
            assert!(worker.acknowledge(&completion));
            if !completion.started() {
                return completion.outcome;
            }
        }
    }
    fn call(controls: &Controls) -> (Kind, WindowId, u32) {
        controls
            .observed
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
    }

    fn delayed(
        backend: Fake,
        events: mpsc::Sender<Event>,
    ) -> (Worker, mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered, waiting) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let worker = Worker::start_inner(
            Box::new(backend),
            None,
            events,
            Some(BeforeClaim {
                entered,
                release: gate,
            }),
        )
        .unwrap();
        (worker, waiting, release)
    }

    #[test]
    fn shutdown_cancels_park_after_wakeup_received_but_before_worker_claim() {
        let (backend, controls) = fake(None);
        let (events, _rx) = mpsc::channel();
        let (mut worker, waiting, release) = delayed(backend, events);
        worker.submit(park(1));
        waiting.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(worker.state.lock().unwrap().claimed.is_none());
        let scheduler = worker.state.clone();
        let stopped = std::thread::spawn(move || worker.shutdown(SHUTDOWN_WAIT));
        let deadline = Instant::now() + Duration::from_secs(1);
        while !scheduler.lock().unwrap().stopping {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        release.send(()).unwrap();
        let (outcome, notifications) = stopped.join().unwrap();
        assert_eq!(outcome, Parking::NothingParked);
        assert!(notifications.is_empty());
        assert_eq!(call(&controls).0, Kind::Recover);
        assert!(controls.observed.try_recv().is_err());
    }

    #[test]
    fn restore_cancels_resize_after_wakeup_received_but_before_worker_claim() {
        let (backend, controls) = fake(None);
        let (events, rx) = mpsc::channel();
        let (mut worker, waiting, release) = delayed(backend, events);
        worker.submit(resize(1, 100));
        waiting.recv_timeout(Duration::from_secs(1)).unwrap();
        worker.submit(Command::Restore {
            window: WindowId(1),
            place: None,
        });
        release.send(()).unwrap();
        assert_eq!(call(&controls).0, Kind::Restore);
        assert!(matches!(
            next(&mut worker, &rx),
            Outcome::Restored { ok: true, .. }
        ));
        assert!(controls.observed.try_recv().is_err());
        assert_eq!(worker.shutdown(SHUTDOWN_WAIT).0, Parking::NothingParked);
    }

    #[test]
    fn newest_resize_replaces_job_after_wakeup_received_but_before_worker_claim() {
        let (backend, controls) = fake(None);
        let (events, rx) = mpsc::channel();
        let (mut worker, waiting, release) = delayed(backend, events);
        worker.submit(resize(1, 100));
        waiting.recv_timeout(Duration::from_secs(1)).unwrap();
        worker.submit(resize(1, 200));
        release.send(()).unwrap();
        assert_eq!(call(&controls), (Kind::Resize, WindowId(1), 200));
        assert!(matches!(
            next(&mut worker, &rx),
            Outcome::Parked { result: Ok(_), .. }
        ));
        assert!(controls.observed.try_recv().is_err());
        worker.shutdown(SHUTDOWN_WAIT);
    }

    #[test]
    fn resize_storm_runs_one_call_and_only_newest_queued_geometry() {
        let (backend, controls) = fake(Some(Kind::Resize));
        let (events, rx) = mpsc::channel();
        let mut worker = Worker::start(Box::new(backend), events).unwrap();
        worker.submit(resize(1, 100));
        assert_eq!(call(&controls), (Kind::Resize, WindowId(1), 100));
        for width in 200..500 {
            worker.submit(resize(1, width));
        }
        assert_eq!(worker.state.lock().unwrap().ready.len(), 1);
        assert!(controls.observed.try_recv().is_err());
        controls.release.send(()).unwrap();
        next(&mut worker, &rx);
        assert_eq!(call(&controls), (Kind::Resize, WindowId(1), 499));
        next(&mut worker, &rx);
        assert!(controls.observed.try_recv().is_err());
        assert_eq!(worker.shutdown(SHUTDOWN_WAIT).0, Parking::Restored);
    }

    #[test]
    fn park_resize_restore_order_and_restore_cancels_queued_resize() {
        let (backend, controls) = fake(None);
        let (events, rx) = mpsc::channel();
        let mut worker = Worker::start(Box::new(backend), events).unwrap();
        worker.submit(park(1));
        assert_eq!(call(&controls).0, Kind::Park);
        next(&mut worker, &rx);
        worker.submit(resize(1, 200));
        assert_eq!(call(&controls).0, Kind::Resize);
        next(&mut worker, &rx);
        worker.submit(Command::Restore {
            window: WindowId(1),
            place: None,
        });
        assert_eq!(call(&controls).0, Kind::Restore);
        next(&mut worker, &rx);
        worker.shutdown(SHUTDOWN_WAIT);

        let (backend, controls) = fake(Some(Kind::Park));
        let (events, rx) = mpsc::channel();
        let mut worker = Worker::start(Box::new(backend), events).unwrap();
        worker.submit(park(1));
        assert_eq!(call(&controls).0, Kind::Park);
        worker.submit(resize(1, 200));
        worker.submit(Command::Restore {
            window: WindowId(1),
            place: None,
        });
        controls.release.send(()).unwrap();
        next(&mut worker, &rx);
        assert_eq!(call(&controls), (Kind::Restore, WindowId(1), 0));
        next(&mut worker, &rx);
        assert!(controls.observed.try_recv().is_err());
        worker.shutdown(SHUTDOWN_WAIT);
    }

    #[test]
    fn placed_restore_keeps_serial_order_cancels_resize_and_failure_retains_recovery() {
        for fails in [false, true] {
            let (mut backend, controls) = fake(Some(Kind::Park));
            backend.restore_fails = fails;
            let (events, rx) = mpsc::channel();
            let mut worker = Worker::start(Box::new(backend), events).unwrap();
            worker.submit(park(1));
            assert_eq!(call(&controls), (Kind::Park, WindowId(1), 100));
            worker.submit(resize(1, 300));
            let place = Some((DisplayId(2), PointDevice::new(31.0, 44.0)));
            worker.submit(Command::Restore {
                window: WindowId(1),
                place,
            });
            assert!(controls.restored_at.lock().unwrap().is_empty());
            controls.release.send(()).unwrap();
            assert!(matches!(next(&mut worker, &rx), Outcome::Parked { .. }));
            assert_eq!(call(&controls), (Kind::Restore, WindowId(1), 0));
            assert!(matches!(next(&mut worker, &rx), Outcome::Restored { ok, .. } if ok == !fails));
            assert_eq!(*controls.restored_at.lock().unwrap(), [place]);
            assert_eq!(controls.journal.load(Ordering::SeqCst), fails);
            assert!(controls.observed.try_recv().is_err());
            worker.shutdown(SHUTDOWN_WAIT);
        }
    }

    #[test]
    fn resize_coalescing_replaces_every_unclaimed_same_window_resize_at_new_arrival_position() {
        let (backend, controls) = fake(Some(Kind::Resize));
        let (events, rx) = mpsc::channel();
        let mut worker = Worker::start(Box::new(backend), events).unwrap();
        worker.submit(resize(0, 10));
        call(&controls);
        worker.submit(resize(1, 100));
        worker.submit(park(2));
        worker.submit(resize(1, 200));
        assert_eq!(
            worker
                .state
                .lock()
                .unwrap()
                .ready
                .iter()
                .map(|job| job.command)
                .collect::<Vec<_>>(),
            vec![park(2), resize(1, 200)]
        );
        worker.submit(Command::Restore {
            window: WindowId(2),
            place: None,
        });
        worker.submit(resize(1, 300));
        controls.release.send(()).unwrap();
        next(&mut worker, &rx);
        for expected in [
            (Kind::Park, WindowId(2), 100),
            (Kind::Restore, WindowId(2), 0),
            (Kind::Resize, WindowId(1), 300),
        ] {
            assert_eq!(call(&controls), expected);
            next(&mut worker, &rx);
        }
        worker.shutdown(SHUTDOWN_WAIT);
    }

    #[test]
    fn ready_queue_is_bounded_and_waiting_restores_are_never_lost_or_overtaken() {
        let (backend, controls) = fake(Some(Kind::Resize));
        let (events, rx) = mpsc::channel();
        let mut worker = Worker::start(Box::new(backend), events).unwrap();
        worker.submit(resize(0, 10));
        call(&controls);
        for window in 1..=64 {
            worker.submit(park(window));
        }
        worker.submit(park(65));
        assert!(matches!(
            next(&mut worker, &rx),
            Outcome::Parked {
                window: WindowId(65),
                result: Err(Failure::Other)
            }
        ));
        worker.submit(resize(66, 100));
        assert!(matches!(
            next(&mut worker, &rx),
            Outcome::Parked {
                window: WindowId(66),
                result: Err(Failure::Other)
            }
        ));
        worker.submit(Command::Restore {
            window: WindowId(1),
            place: None,
        });
        worker.submit(Command::Restore {
            window: WindowId(2),
            place: None,
        });
        assert_eq!(worker.state.lock().unwrap().ready.len(), QUEUE_CAP);
        assert_eq!(worker.state.lock().unwrap().waiting_restores.len(), 2);
        worker.submit(park(67));
        assert!(matches!(
            next(&mut worker, &rx),
            Outcome::Parked {
                window: WindowId(67),
                result: Err(Failure::Other)
            }
        ));
        assert!(controls.observed.try_recv().is_err());
        controls.release.send(()).unwrap();
        next(&mut worker, &rx);
        for window in 1..=64 {
            assert_eq!(call(&controls), (Kind::Park, WindowId(window), 100));
            next(&mut worker, &rx);
        }
        for window in [1, 2] {
            assert_eq!(call(&controls), (Kind::Restore, WindowId(window), 0));
            next(&mut worker, &rx);
        }
        worker.shutdown(SHUTDOWN_WAIT);
    }

    #[test]
    fn shutdown_finishes_inflight_discards_queued_park_resize_then_restores_and_recovers() {
        let (backend, controls) = fake(Some(Kind::Resize));
        let (events, _rx) = mpsc::channel();
        let mut worker = Worker::start(Box::new(backend), events).unwrap();
        worker.submit(resize(1, 100));
        call(&controls);
        worker.submit(park(2));
        worker.submit(resize(3, 200));
        worker.submit(Command::Restore {
            window: WindowId(1),
            place: None,
        });
        let scheduler = worker.state.clone();
        let shutdown = std::thread::spawn(move || worker.shutdown(SHUTDOWN_WAIT));
        let deadline = Instant::now() + Duration::from_secs(1);
        while !scheduler.lock().unwrap().stopping {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        controls.release.send(()).unwrap();
        let (outcome, completed) = shutdown.join().unwrap();
        assert_eq!(outcome, Parking::NothingParked);
        assert_eq!(completed.iter().filter(|c| !c.started()).count(), 2);
        assert_eq!(call(&controls), (Kind::Restore, WindowId(1), 0));
        assert_eq!(call(&controls), (Kind::Recover, WindowId(0), 0));
        assert!(controls.observed.try_recv().is_err());
    }

    #[test]
    fn hung_shutdown_times_out_and_leaves_journal_pending() {
        let (backend, controls) = fake(Some(Kind::Resize));
        let (events, _rx) = mpsc::channel();
        let mut worker = Worker::start(Box::new(backend), events).unwrap();
        worker.submit(resize(1, 100));
        call(&controls);
        let started = Instant::now();
        assert_eq!(
            worker.shutdown(Duration::from_millis(20)).0,
            Parking::Failed
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(controls.journal.load(Ordering::SeqCst));
        controls.release.send(()).unwrap();
        assert_eq!(
            worker.stopped.recv_timeout(Duration::from_secs(1)).unwrap(),
            Parking::Failed
        );
        assert!(controls.journal.load(Ordering::SeqCst));
        assert!(controls.observed.try_recv().is_err());
    }

    #[test]
    fn panicked_worker_shutdown_is_unclean_without_recovering_or_hanging() {
        let (mut backend, controls) = fake(None);
        backend.panic = Some(Kind::Resize);
        let (events, rx) = mpsc::channel();
        let mut worker = Worker::start(Box::new(backend), events).unwrap();
        worker.submit(resize(1, 100));
        call(&controls);
        assert!(matches!(next(&mut worker, &rx), Outcome::Panicked));
        assert!(matches!(
            next(&mut worker, &rx),
            Outcome::Parked {
                result: Err(Failure::Other),
                ..
            }
        ));
        let started = Instant::now();
        assert_eq!(worker.shutdown(SHUTDOWN_WAIT).0, Parking::Failed);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(controls.journal.load(Ordering::SeqCst));
        assert!(controls.observed.try_recv().is_err());
    }

    type ActivationLog = Arc<Mutex<Vec<WindowId>>>;

    /// An activator that records the window, announces it, and then waits for one token on `gate`.
    fn activator(
        log: &ActivationLog,
        entered: mpsc::Sender<WindowId>,
        gate: mpsc::Receiver<()>,
    ) -> Activator {
        let log = log.clone();
        Box::new(move |window| {
            log.lock().unwrap().push(window);
            let _ = entered.send(window);
            gate.recv().unwrap();
            Ok(())
        })
    }

    fn activate(window: u64) -> Command {
        Command::Activate {
            window: WindowId(window),
        }
    }

    #[test]
    fn activation_runs_on_the_worker_in_order_with_a_return_of_the_same_window() {
        let (backend, controls) = fake(None);
        let (events, rx) = mpsc::channel();
        let log = ActivationLog::default();
        let (entered_tx, entered) = mpsc::channel();
        let (open, gate) = mpsc::channel();
        let mut worker = Worker::start_with_activator(
            Box::new(backend),
            Some(activator(&log, entered_tx, gate)),
            events,
        )
        .unwrap();
        assert!(worker.can_activate());
        // `submit` returns while the first activation is blocked: the caller never waits for it.
        worker.submit(activate(1));
        assert_eq!(
            entered.recv_timeout(Duration::from_secs(1)).unwrap(),
            WindowId(1)
        );
        worker.submit(activate(1));
        worker.submit(activate(2));
        worker.submit(Command::Restore {
            window: WindowId(1),
            place: None,
        });
        // The return of window 1 dropped its queued (not yet claimed) activation, kept window 2's,
        // and waits behind the call in flight: the worker is serial.
        assert_eq!(
            worker
                .state
                .lock()
                .unwrap()
                .ready
                .iter()
                .map(|job| job.command)
                .collect::<Vec<_>>(),
            vec![
                activate(2),
                Command::Restore {
                    window: WindowId(1),
                    place: None
                }
            ]
        );
        assert!(controls.observed.try_recv().is_err());
        open.send(()).unwrap();
        open.send(()).unwrap();
        assert!(matches!(
            next(&mut worker, &rx),
            Outcome::Activated {
                window: WindowId(1),
                result: Ok(())
            }
        ));
        assert!(matches!(
            next(&mut worker, &rx),
            Outcome::Activated {
                window: WindowId(2),
                result: Ok(())
            }
        ));
        assert_eq!(call(&controls), (Kind::Restore, WindowId(1), 0));
        assert!(matches!(
            next(&mut worker, &rx),
            Outcome::Restored { ok: true, .. }
        ));
        assert_eq!(*log.lock().unwrap(), [WindowId(1), WindowId(2)]);
        worker.shutdown(SHUTDOWN_WAIT);
    }

    #[test]
    fn a_refused_or_panicking_activation_reports_failure_and_leaves_parking_running() {
        let (backend, _controls) = fake(None);
        let (events, rx) = mpsc::channel();
        let mut worker = Worker::start(Box::new(backend), events).unwrap();
        assert!(!worker.can_activate());
        worker.submit(activate(1));
        assert!(matches!(
            next(&mut worker, &rx),
            Outcome::Activated {
                result: Err(Failure::Other),
                ..
            }
        ));
        worker.shutdown(SHUTDOWN_WAIT);

        let (backend, controls_after) = fake(None);
        let (events, rx) = mpsc::channel();
        let mut worker = Worker::start_with_activator(
            Box::new(backend),
            Some(Box::new(|_| panic!("fixture: activation panic"))),
            events,
        )
        .unwrap();
        worker.submit(activate(1));
        assert!(matches!(
            next(&mut worker, &rx),
            Outcome::Activated {
                result: Err(Failure::Other),
                ..
            }
        ));
        worker.submit(park(2));
        assert_eq!(call(&controls_after).0, Kind::Park);
        assert!(matches!(
            next(&mut worker, &rx),
            Outcome::Parked { result: Ok(_), .. }
        ));
        assert_eq!(worker.shutdown(SHUTDOWN_WAIT).0, Parking::Restored);
    }

    #[test]
    fn shutdown_discards_a_queued_activation() {
        let (backend, controls) = fake(Some(Kind::Resize));
        let (events, _rx) = mpsc::channel();
        let log = ActivationLog::default();
        let (entered_tx, _entered) = mpsc::channel();
        let (_open, gate) = mpsc::channel();
        let mut worker = Worker::start_with_activator(
            Box::new(backend),
            Some(activator(&log, entered_tx, gate)),
            events,
        )
        .unwrap();
        worker.submit(resize(1, 100));
        call(&controls);
        worker.submit(activate(3));
        let scheduler = worker.state.clone();
        let shutdown = std::thread::spawn(move || worker.shutdown(SHUTDOWN_WAIT));
        let deadline = Instant::now() + Duration::from_secs(1);
        while !scheduler.lock().unwrap().stopping {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        controls.release.send(()).unwrap();
        shutdown.join().unwrap();
        assert!(log.lock().unwrap().is_empty());
    }
}

#[cfg(all(test, windows))]
mod windows_pending_notice_tests {
    use super::*;
    struct Refused(&'static str);
    impl WindowParking for Refused {
        fn park(&mut self, _: WindowId, _: PixelSize, _: f64) -> Result<Parked, PlatformError> {
            Err(PlatformError::Unsupported(self.0))
        }
        fn resize(&mut self, _: WindowId, _: PixelSize, _: f64) -> Result<Parked, PlatformError> {
            Err(PlatformError::Unsupported(self.0))
        }
        fn geometry(&self, _: WindowId) -> Result<Parked, PlatformError> {
            Err(PlatformError::Unsupported("owned fake geometry"))
        }
        fn set_fullscreen(&mut self, _: WindowId, _: bool) -> Result<(), PlatformError> {
            Ok(())
        }
        fn restore(&mut self, _: WindowId) -> Result<(), PlatformError> {
            Ok(())
        }
        fn recover(&mut self) -> Result<Vec<WindowId>, PlatformError> {
            Ok(Vec::new())
        }
    }
    #[test]
    fn only_exact_pending_park_refusal_carries_local_notice_and_engine_failure_is_unchanged() {
        let reason = crosspane_platform_windows::model::parking::PENDING_REPARK_REASON;
        let park = Command::Park {
            window: WindowId(7),
            size: PixelSize::new(400, 300),
            scale: 1.0,
        };
        let done = operation(
            &mut Refused(reason),
            &mut None,
            Job {
                id: 1,
                command: park,
            },
        );
        assert_eq!(done.notice, Some(reason));
        assert!(matches!(
            done.outcome,
            Outcome::Parked {
                result: Err(Failure::Other),
                ..
            }
        ));
        assert_eq!(
            operation(
                &mut Refused("different unsupported reason"),
                &mut None,
                Job {
                    id: 2,
                    command: park
                }
            )
            .notice,
            None
        );
        let resize = Command::Resize {
            window: WindowId(7),
            size: PixelSize::new(400, 300),
            scale: 1.0,
            fullscreen: false,
        };
        assert_eq!(
            operation(
                &mut Refused(reason),
                &mut None,
                Job {
                    id: 3,
                    command: resize
                }
            )
            .notice,
            None
        );
        assert_eq!(Completion::unavailable(4, park).notice, None);
    }
}
