//! Bounded detached calls: one outstanding worker per lane, a polled wait that observes a stop
//! flag, and an admission that stays occupied until the worker really returns.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

/// The longest a caller sleeps between two checks of the stop flag while it waits for a worker.
pub(crate) const POLL: Duration = Duration::from_millis(25);

/// At most one detached worker at a time. The admission stays occupied until a worker returns.
#[derive(Debug, Default)]
pub(crate) struct Lane {
    busy: Arc<AtomicBool>,
}

/// How one [`Lane::run`] call settled.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Settled<T> {
    /// The worker returned this value.
    Done(T),
    /// The wait ran out first. The worker may still be running, and the lane stays busy until it
    /// returns. Its eventual value is discarded.
    TimedOut,
    /// `stop` was set before the call or during the wait. A call with `stop` already set spawns
    /// nothing.
    Stopped,
    /// An earlier worker is still outstanding. Nothing was spawned and `work` never ran.
    Busy,
    /// The thread could not be spawned, or the worker panicked.
    Failed,
}

/// Holds the lane's admission for one worker. Dropping it clears `busy`, including on unwind.
struct Lease(Arc<AtomicBool>);

impl Drop for Lease {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Lane {
    /// Returns within `wait` + POLL, or within POLL of `stop`. While an earlier worker is still
    /// running: `Busy`, and no thread is spawned. A failed spawn or a panicking worker: `Failed`.
    /// `busy` is released before `Done` is observable (lease dropped before send; also on unwind).
    pub(crate) fn run<T: Send + 'static>(
        &self,
        name: &str,
        stop: &AtomicBool,
        wait: Duration,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Settled<T> {
        if stop.load(Ordering::Acquire) {
            return Settled::Stopped;
        }
        if self.busy.swap(true, Ordering::AcqRel) {
            return Settled::Busy;
        }
        let lease = Lease(Arc::clone(&self.busy));
        let (tx, rx) = mpsc::sync_channel(1);
        let spawned = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                // Locals drop in reverse declaration order. On a panic the lease clears `busy`
                // before `tx` disconnects, so `Failed` is never seen while `busy` is still set.
                let tx = tx;
                let lease = lease;
                let value = work();
                // On success the lease is released before the value is sent, so `busy` is already
                // false when `Done` is observable.
                drop(lease);
                let _ = tx.send(value);
            });
        if spawned.is_err() {
            // The failed spawn dropped the closure, and with it the lease, so `busy` is clear.
            return Settled::Failed;
        }
        let deadline = Instant::now() + wait;
        loop {
            if stop.load(Ordering::Acquire) {
                return Settled::Stopped;
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return Settled::TimedOut;
            };
            match rx.recv_timeout(left.min(POLL)) {
                Ok(value) => return Settled::Done(value),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Settled::Failed,
            }
        }
    }

    /// Whether a worker admitted by this lane has not returned yet.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};

    use super::{Lane, POLL, Settled};

    /// Headroom for scheduler jitter on a loaded machine. Every timing assertion stays bounded.
    const SLACK: Duration = Duration::from_millis(500);

    /// Waits until the lane is free, for at most `limit`. Returns whether it became free.
    fn wait_until_free(lane: &Lane, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while lane.busy() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(POLL);
        }
        true
    }

    #[test]
    fn done_returns_the_value_and_frees_the_lane() {
        let lane = Lane::default();
        let stop = AtomicBool::new(false);
        let settled = lane.run("bounded-done", &stop, Duration::from_secs(5), || 42_u32);
        assert_eq!(settled, Settled::Done(42));
        assert!(!lane.busy());
    }

    #[test]
    fn timeout_keeps_the_lane_busy_until_the_worker_returns() {
        let lane = Lane::default();
        let stop = AtomicBool::new(false);
        let (release, gate) = mpsc::channel::<()>();
        let wait = Duration::from_millis(50);
        let started = Instant::now();
        let settled = lane.run("bounded-timeout", &stop, wait, move || {
            let _ = gate.recv();
            7_u32
        });
        let elapsed = started.elapsed();
        assert_eq!(settled, Settled::TimedOut);
        assert!(
            elapsed >= wait && elapsed < wait + SLACK,
            "elapsed {elapsed:?} for wait {wait:?}"
        );
        assert!(
            lane.busy(),
            "busy must stay set while the worker is still running"
        );

        let _ = release.send(());
        assert!(
            wait_until_free(&lane, SLACK),
            "the worker never released the lane"
        );
        let after = lane.run("bounded-after", &stop, Duration::from_secs(5), || 9_u32);
        assert_eq!(after, Settled::Done(9));
    }

    #[test]
    fn busy_while_a_worker_is_outstanding_spawns_nothing() {
        let lane = Lane::default();
        let stop = AtomicBool::new(false);
        let (release, gate) = mpsc::channel::<()>();
        let first = lane.run(
            "bounded-busy-first",
            &stop,
            Duration::from_millis(25),
            move || {
                let _ = gate.recv();
                1_u32
            },
        );
        assert_eq!(first, Settled::TimedOut);

        let spawns = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&spawns);
        let second = lane.run(
            "bounded-busy-second",
            &stop,
            Duration::from_secs(5),
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
                2_u32
            },
        );
        assert_eq!(second, Settled::Busy);
        assert_eq!(
            spawns.load(Ordering::SeqCst),
            0,
            "work must not run while busy"
        );

        let _ = release.send(());
        assert!(
            wait_until_free(&lane, SLACK),
            "the first worker never released the lane"
        );
        let after = lane.run("bounded-busy-after", &stop, Duration::from_secs(5), || {
            3_u32
        });
        assert_eq!(after, Settled::Done(3));
    }

    #[test]
    fn stop_already_set_is_stopped_without_a_spawn() {
        let lane = Lane::default();
        let stop = AtomicBool::new(true);
        let spawns = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&spawns);
        let settled = lane.run(
            "bounded-stop-before",
            &stop,
            Duration::from_secs(5),
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
                1_u32
            },
        );
        assert_eq!(settled, Settled::Stopped);
        std::thread::sleep(POLL * 4);
        assert_eq!(
            spawns.load(Ordering::SeqCst),
            0,
            "no worker may run after a stop"
        );
        assert!(
            !lane.busy(),
            "a call that never spawned must not hold the lane"
        );
    }

    #[test]
    fn stop_during_the_wait_is_stopped_within_two_polls() {
        let lane = Lane::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (release, gate) = mpsc::channel::<()>();
        let setter = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                let set_at = Instant::now();
                stop.store(true, Ordering::Release);
                set_at
            })
        };
        let settled = lane.run(
            "bounded-stop-during",
            &stop,
            Duration::from_secs(5),
            move || {
                let _ = gate.recv();
                1_u32
            },
        );
        let returned = Instant::now();
        let set_at = setter.join().expect("stop setter panicked");
        assert_eq!(settled, Settled::Stopped);
        let latency = returned.duration_since(set_at);
        assert!(
            latency < POLL * 2 + Duration::from_millis(100),
            "stop observed after {latency:?}"
        );
        assert!(lane.busy(), "the worker is still running after a stop");

        let _ = release.send(());
        assert!(
            wait_until_free(&lane, SLACK),
            "the worker never released the lane"
        );
    }

    #[test]
    fn panicking_worker_is_failed_and_frees_the_lane() {
        let lane = Lane::default();
        let stop = AtomicBool::new(false);
        let settled = lane.run("bounded-panic", &stop, Duration::from_secs(5), || -> u32 {
            panic!("bounded_call test panic")
        });
        assert_eq!(settled, Settled::Failed);
        assert!(!lane.busy(), "the lease must be released on unwind");
        let after = lane.run("bounded-panic-after", &stop, Duration::from_secs(5), || {
            4_u32
        });
        assert_eq!(after, Settled::Done(4));
    }
}
