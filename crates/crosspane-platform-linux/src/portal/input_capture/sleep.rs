//! Timers for the worker's single-threaded executor, which has none of its own.

use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

/// Completes once its duration has elapsed. A short-lived helper thread sleeps until the deadline
/// and wakes the task, so the executor thread can park meanwhile.
#[derive(Debug)]
pub(super) struct Sleep {
    deadline: Instant,
    waker: Arc<Mutex<Option<Waker>>>,
    spawned: bool,
}

impl Sleep {
    pub(super) fn new(duration: Duration) -> Sleep {
        Sleep {
            deadline: Instant::now() + duration,
            waker: Arc::new(Mutex::new(None)),
            spawned: false,
        }
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = &mut *self;
        if Instant::now() >= this.deadline {
            return Poll::Ready(());
        }
        *this.waker.lock().unwrap_or_else(PoisonError::into_inner) = Some(cx.waker().clone());
        if !this.spawned {
            this.spawned = true;
            let waker = Arc::clone(&this.waker);
            let deadline = this.deadline;
            let spawned = thread::Builder::new()
                .name("crosspane-capture-timer".to_owned())
                .spawn(move || {
                    loop {
                        let left = deadline.saturating_duration_since(Instant::now());
                        if left.is_zero() {
                            break;
                        }
                        thread::sleep(left);
                    }
                    let waker = waker.lock().unwrap_or_else(PoisonError::into_inner).take();
                    if let Some(waker) = waker {
                        waker.wake();
                    }
                });
            if let Err(error) = spawned {
                // Degrade to not waiting rather than hanging the worker.
                tracing::warn!(%error, "cannot start a timer thread; not waiting");
                return Poll::Ready(());
            }
        }
        Poll::Pending
    }
}

/// `future`'s output, or `None` if `duration` passes first.
pub(super) async fn timeout<F: Future>(future: F, duration: Duration) -> Option<F::Output> {
    let mut future = std::pin::pin!(future);
    let mut sleep = Sleep::new(duration);
    poll_fn(|cx| {
        if let Poll::Ready(value) = future.as_mut().poll(cx) {
            return Poll::Ready(Some(value));
        }
        if Pin::new(&mut sleep).poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        Poll::Pending
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::{pending, ready};

    #[test]
    fn sleep_waits_for_its_duration() {
        let start = Instant::now();
        zbus::block_on(Sleep::new(Duration::from_millis(40)));
        assert!(start.elapsed() >= Duration::from_millis(40));
    }

    #[test]
    fn timeout_returns_the_output_when_the_future_wins() {
        let value = zbus::block_on(timeout(ready(7), Duration::from_secs(5)));
        assert_eq!(value, Some(7));
    }

    #[test]
    fn timeout_gives_up_on_a_future_that_never_completes() {
        let start = Instant::now();
        let value = zbus::block_on(timeout(pending::<()>(), Duration::from_millis(40)));
        assert_eq!(value, None);
        assert!(start.elapsed() >= Duration::from_millis(40));
    }
}
