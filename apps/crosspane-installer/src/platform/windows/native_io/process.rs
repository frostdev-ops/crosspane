//! Bounded current-process observations and native dispatch; no arbitrary process selection.

use super::{NativeError, NativeResult};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

pub const MAX_NATIVE_TIMEOUT_MS: u64 = 120_000;
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}
#[derive(Debug)]
pub struct MonotonicClock(Instant);
impl Default for MonotonicClock {
    fn default() -> Self {
        Self(Instant::now())
    }
}
impl Clock for MonotonicClock {
    fn now_ms(&self) -> u64 {
        self.0.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    }
}
#[derive(Clone, Default, Debug)]
pub struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}
#[derive(Clone)]
pub struct Deadline {
    clock: Arc<dyn Clock>,
    end: u64,
    wall: Instant,
    timeout_ms: u64,
    cancel: Cancellation,
}
impl std::fmt::Debug for Deadline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Deadline")
    }
}
impl Deadline {
    pub fn new(timeout_ms: u64, clock: Arc<dyn Clock>, cancel: Cancellation) -> NativeResult<Self> {
        if timeout_ms == 0 || timeout_ms > MAX_NATIVE_TIMEOUT_MS {
            return Err(NativeError::Invalid);
        }
        let end = clock
            .now_ms()
            .checked_add(timeout_ms)
            .ok_or(NativeError::Invalid)?;
        Ok(Self {
            clock,
            end,
            wall: Instant::now(),
            timeout_ms,
            cancel,
        })
    }
    pub fn check(&self) -> NativeResult<()> {
        if self.cancel.is_cancelled() {
            Err(NativeError::Cancelled)
        } else if self.clock.now_ms() >= self.end
            || self.wall.elapsed() >= Duration::from_millis(self.timeout_ms)
        {
            Err(NativeError::Timeout)
        } else {
            Ok(())
        }
    }
    pub fn remaining_ms(&self) -> NativeResult<u64> {
        self.check()?;
        Ok(self.end.saturating_sub(self.clock.now_ms()).min(
            self.timeout_ms
                .saturating_sub(self.wall.elapsed().as_millis() as u64),
        ))
    }
    pub(crate) fn shorten(&self, maximum_ms: u64) -> NativeResult<Self> {
        self.check()?;
        if maximum_ms == 0 {
            return Err(NativeError::Timeout);
        }
        let mut value = self.clone();
        value.end = value.end.min(
            self.clock
                .now_ms()
                .checked_add(maximum_ms)
                .ok_or(NativeError::Invalid)?,
        );
        let wall_end = (self.wall.elapsed().as_millis() as u64)
            .checked_add(maximum_ms)
            .ok_or(NativeError::Invalid)?;
        value.timeout_ms = value.timeout_ms.min(wall_end);
        Ok(value)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dispatch {
    Observation,
    Mutation,
}
/// At most one submitted call. Timing out does not cancel a Win32 call or release its handles.
/// After uncertain mutation this owner refuses any further mutation for its entire lifetime.
/// Reopening requires fresh native admission and acquisition of the still-retained installer lock.
#[derive(Default)]
pub(crate) struct CallOwner {
    busy: AtomicBool,
    uncertain: AtomicBool,
    #[cfg(test)]
    delivery_hook: std::sync::Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}
struct Flight(Arc<CallOwner>);
impl Drop for Flight {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::Release);
    }
}
impl CallOwner {
    pub(crate) fn idle(&self) -> bool {
        !self.busy.load(Ordering::Acquire)
    }
    pub(crate) fn retire_mutations(&self) {
        self.uncertain.store(true, Ordering::Release);
    }
    #[cfg(test)]
    #[allow(dead_code)] // Exercised by the source-included integration seam, not library tests.
    pub(crate) fn before_delivery(&self, hook: Arc<dyn Fn() + Send + Sync>) -> NativeResult<()> {
        *self
            .delivery_hook
            .lock()
            .map_err(|_| NativeError::Unavailable)? = Some(hook);
        Ok(())
    }
    pub(crate) fn run<T: Send + 'static>(
        self: &Arc<Self>,
        kind: Dispatch,
        deadline: &Deadline,
        work: impl FnOnce() -> NativeResult<T> + Send + 'static,
    ) -> NativeResult<T> {
        deadline.check()?;
        if kind == Dispatch::Mutation && self.uncertain.load(Ordering::Acquire) {
            return Err(NativeError::OutcomeUnknown);
        }
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(NativeError::Busy);
        }
        let flight = Flight(self.clone());
        // A previous worker may have retired mutations between our first check and the CAS.
        if kind == Dispatch::Mutation && self.uncertain.load(Ordering::Acquire) {
            return Err(NativeError::OutcomeUnknown);
        }
        let (send, receive) = mpsc::sync_channel(1);
        let dispatched_deadline = deadline.clone();
        let delivery_owner = self.clone();
        let thread = std::thread::Builder::new()
            .name("crosspane-installer-native".into())
            .spawn(move || {
                let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    dispatched_deadline.check().and_then(|()| work())
                })) {
                    Ok(result) => result,
                    Err(_) => Err(NativeError::OutcomeUnknown),
                };
                // Retire BEFORE releasing the busy slot or delivering the result. Otherwise
                // another caller could enter while this caller is still waiting to wake.
                if kind == Dispatch::Mutation && matches!(result, Err(NativeError::OutcomeUnknown))
                {
                    delivery_owner.retire_mutations();
                }
                drop(flight);
                #[cfg(test)]
                {
                    let hook = delivery_owner
                        .delivery_hook
                        .lock()
                        .ok()
                        .and_then(|slot| slot.clone());
                    if let Some(hook) = hook {
                        hook();
                    }
                }
                let _ = send.send(result);
            });
        if thread.is_err() {
            return Err(NativeError::Unavailable);
        }
        loop {
            let remaining = match deadline.remaining_ms() {
                Ok(v) => v,
                Err(e) => return self.abandoned(kind, e),
            };
            match receive.recv_timeout(Duration::from_millis(remaining.clamp(1, 10))) {
                Ok(result) => {
                    if let Err(error) = deadline.check() {
                        return self.abandoned(kind, error);
                    }
                    if kind == Dispatch::Mutation
                        && matches!(result, Err(NativeError::OutcomeUnknown))
                    {
                        self.uncertain.store(true, Ordering::Release);
                    }
                    return result;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return self.abandoned(kind, NativeError::Unavailable);
                }
            }
        }
    }
    fn abandoned<T>(&self, kind: Dispatch, error: NativeError) -> NativeResult<T> {
        if kind == Dispatch::Mutation {
            self.uncertain.store(true, Ordering::Release);
            Err(NativeError::OutcomeUnknown)
        } else {
            Err(error)
        }
    }
}
