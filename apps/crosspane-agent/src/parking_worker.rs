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
use crosspane_types::geom::PixelSize;
use crosspane_types::id::WindowId;

use crate::agent::Event;
use crate::lifecycle::Parking;

pub(crate) const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);
const QUEUE_CAP: usize = 64;

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
    },
    Restore {
        window: WindowId,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Park,
    Resize,
    Restore,
}

impl Command {
    fn window(self) -> WindowId {
        match self {
            Self::Park { window, .. } | Self::Resize { window, .. } | Self::Restore { window } => {
                window
            }
        }
    }
    fn kind(self) -> Kind {
        match self {
            Self::Park { .. } => Kind::Park,
            Self::Resize { .. } => Kind::Resize,
            Self::Restore { .. } => Kind::Restore,
        }
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
}

impl Completion {
    pub(crate) fn unavailable(id: u64, command: Command) -> Self {
        Self {
            id,
            outcome: match command {
                Command::Restore { window } => Outcome::Restored { window, ok: false },
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
    #[cfg(test)]
    next_test_id: u64,
}

impl Worker {
    pub(crate) fn start(
        backend: Box<dyn WindowParking>,
        events: mpsc::Sender<Event>,
    ) -> std::io::Result<Self> {
        Self::start_inner(
            backend,
            events,
            #[cfg(test)]
            None,
        )
    }

    fn start_inner(
        backend: Box<dyn WindowParking>,
        events: mpsc::Sender<Event>,
        #[cfg(test)] before_claim: Option<BeforeClaim>,
    ) -> std::io::Result<Self> {
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
                let result = catch_unwind(AssertUnwindSafe(|| {
                    run(
                        backend.as_mut(),
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

fn operation(backend: &mut dyn WindowParking, job: Job) -> Completion {
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
        } => {
            let result = match job.command {
                Command::Park { .. } => backend.park(window, size, scale),
                _ => backend.resize(window, size, scale),
            }
            .map_err(|error| {
                tracing::warn!(%error, operation = job.id, "parking operation failed");
                match error {
                    PlatformError::Locked => Failure::Locked,
                    PlatformError::SecureInput => Failure::SecureInput,
                    PlatformError::PointerButtonHeld => Failure::PointerButtonHeld,
                    PlatformError::PermissionDenied(_) => Failure::PermissionDenied,
                    _ => Failure::Other,
                }
            });
            Outcome::Parked { window, result }
        }
        Command::Restore { window } => {
            let result = backend
                .restore(window)
                .inspect_err(|error| tracing::error!(%error, "could not restore a parked window"));
            Outcome::Restored {
                window,
                ok: result.is_ok(),
            }
        }
    };
    Completion {
        id: job.id,
        outcome,
    }
}

fn run(
    backend: &mut dyn WindowParking,
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
                    let completion = operation(backend, job);
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
    }

    pub(crate) struct Controls {
        pub(crate) observed: mpsc::Receiver<(Kind, WindowId, u32)>,
        pub(crate) release: mpsc::Sender<()>,
        pub(crate) journal: Arc<AtomicBool>,
    }

    pub(crate) fn fake(block: Option<Kind>) -> (Fake, Controls) {
        let (observed, calls) = mpsc::channel();
        let (release, releases) = mpsc::channel();
        let journal = Arc::new(AtomicBool::new(false));
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
            },
            Controls {
                observed: calls,
                release,
                journal,
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
        });
        controls.release.send(()).unwrap();
        next(&mut worker, &rx);
        assert_eq!(call(&controls), (Kind::Restore, WindowId(1), 0));
        next(&mut worker, &rx);
        assert!(controls.observed.try_recv().is_err());
        worker.shutdown(SHUTDOWN_WAIT);
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
        });
        worker.submit(Command::Restore {
            window: WindowId(2),
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
}
