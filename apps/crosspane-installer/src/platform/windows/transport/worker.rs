use super::super::native_io::{Cancellation, Clock, NativeError, NativeResult, process::Deadline};
use super::{Endpoint, StopAcknowledgement, run, run_stop};
use crate::agent_contract::{AgentCall, AgentReply, MAX_QUEUE};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};
static WORKERS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
pub(super) fn count() -> usize {
    WORKERS.load(Ordering::Acquire)
}
struct Slot;
impl Drop for Slot {
    fn drop(&mut self) {
        WORKERS.fetch_sub(1, Ordering::Release);
    }
}
pub(super) enum WorkRequest {
    Ordinary(AgentCall),
    #[allow(dead_code)] // Same private worker branch; production Stop constructor awaits A4.
    Stop {
        expected_instance: u64,
        reply: mpsc::SyncSender<Result<StopAcknowledgement, crate::agent_contract::CallFailure>>,
    },
}
pub(super) struct Work {
    pub request: WorkRequest,
    pub deadline: Deadline,
    pub endpoint: Arc<dyn Endpoint>,
    pub retired: Arc<AtomicBool>,
}
struct Flight(Arc<AtomicBool>);
impl Drop for Flight {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
pub(super) fn start(
    cancel: Cancellation,
    clock: Arc<dyn Clock>,
) -> NativeResult<(
    mpsc::SyncSender<Work>,
    mpsc::Receiver<AgentReply>,
    Arc<AtomicBool>,
)> {
    WORKERS
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < MAX_QUEUE).then_some(n + 1)
        })
        .map_err(|_| NativeError::Busy)?;
    let slot = Slot;
    let (send, calls) = mpsc::sync_channel::<Work>(MAX_QUEUE);
    let (results, receive) = mpsc::sync_channel(MAX_QUEUE);
    let active = Arc::new(AtomicBool::new(false));
    let current = active.clone();
    std::thread::Builder::new()
        .name("installer-windows-agent".into())
        .spawn(move || {
            let _slot = slot;
            loop {
                let work = match calls.recv_timeout(Duration::from_millis(10)) {
                    Ok(work) => work,
                    Err(mpsc::RecvTimeoutError::Timeout) if !cancel.is_cancelled() => continue,
                    Err(_) => break,
                };
                current.store(true, Ordering::Release);
                let flight = Flight(current.clone());
                if let WorkRequest::Stop {
                    expected_instance,
                    reply,
                } = &work.request
                {
                    let result =
                        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            if work.retired.load(Ordering::Acquire) {
                                Err(crate::agent_contract::CallFailure::Unavailable)
                            } else {
                                run_stop(work.endpoint.as_ref(), *expected_instance, &work.deadline)
                            }
                        })) {
                            Ok(result) => result,
                            Err(_) => {
                                Err(crate::agent_contract::CallFailure::TimeoutOutcomeUnknown)
                            }
                        };
                    // A consumed Stop selection always terminates. Retirement precedes active
                    // release/reply, and native completion still precedes dropping this Work/slot.
                    work.retired.store(true, Ordering::Release);
                    drop(flight);
                    let _ = reply.try_send(result);
                    break;
                }
                let WorkRequest::Ordinary(call) = &work.request else {
                    break;
                };
                let (result, source) =
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        if work.retired.load(Ordering::Acquire) {
                            (
                                Err(crate::agent_contract::CallFailure::Unavailable),
                                crate::agent_contract::ObservationSource::Demo,
                            )
                        } else {
                            run(work.endpoint.as_ref(), &call.request, &work.deadline)
                        }
                    })) {
                        Ok(result) => result,
                        Err(_) => (
                            Err(crate::agent_contract::CallFailure::TimeoutOutcomeUnknown),
                            crate::agent_contract::ObservationSource::Demo,
                        ),
                    };
                if matches!(
                    &result,
                    Err(crate::agent_contract::CallFailure::TimeoutOutcomeUnknown
                        | crate::agent_contract::CallFailure::Unavailable
                        | crate::agent_contract::CallFailure::InvalidResponse)
                ) {
                    // Retire before releasing active or delivering a reply. Old queued calls retain
                    // this same flag and cannot invoke native I/O; only explicit fresh admission resets.
                    work.retired.store(true, Ordering::Release);
                }
                drop(flight);
                if results
                    .try_send(AgentReply {
                        id: call.id,
                        observed_at_ms: clock.now_ms(),
                        source,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .map_err(|_| NativeError::Unavailable)?;
    Ok((send, receive, active))
}
