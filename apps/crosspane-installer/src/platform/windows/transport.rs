//! Selected Windows agent transport. Paths and wire observations are never capabilities.

#[path = "transport/security.rs"]
pub(crate) mod security;
#[path = "transport/worker.rs"]
mod worker;

use super::native_io::{Clock, NativeError, NativeResult, process::Deadline};
use crate::agent_contract::{
    AgentCall, AgentPort, AgentQueue, AgentReply, CallFailure, DecodedReply, InstallerRequest,
    MAX_QUEUE, ObservationSource, StatusAdmission,
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

pub(crate) trait Endpoint: Send + Sync {
    fn admit_caller(&self) -> NativeResult<()> {
        Ok(())
    }
    fn check(&self, deadline: &Deadline) -> NativeResult<()>;
    fn frame(
        &self,
        request: &InstallerRequest,
        deadline: &Deadline,
    ) -> Result<DecodedReply, CallFailure>;
    fn admit_status(&self, reply: &DecodedReply) -> Result<(), CallFailure>;
    fn source(&self) -> ObservationSource;
    fn selected_instance(&self) -> NativeResult<u64> {
        Err(NativeError::Unsupported)
    }
    fn stop_frame(&self, _expected_instance: u64, _deadline: &Deadline) -> Result<(), CallFailure> {
        Err(CallFailure::Unavailable)
    }
    fn check_after_stop(&self, deadline: &Deadline) -> NativeResult<()> {
        self.check(deadline)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopAcknowledgement {
    Stopping,
}

pub(crate) fn run_stop(
    endpoint: &dyn Endpoint,
    expected_instance: u64,
    deadline: &Deadline,
) -> Result<StopAcknowledgement, CallFailure> {
    endpoint.check(deadline).map_err(failure)?;
    let status = endpoint.frame(&InstallerRequest::Status, deadline)?;
    endpoint.admit_status(&status)?;
    if endpoint.selected_instance().map_err(failure)? != expected_instance {
        return Err(CallFailure::Unavailable);
    }
    endpoint.check(deadline).map_err(failure)?;
    endpoint.stop_frame(expected_instance, deadline)?;
    // No post-stop Status. The exact connected origin/context can be rechecked after it exits.
    endpoint
        .check_after_stop(deadline)
        .map_err(|_| CallFailure::TimeoutOutcomeUnknown)?;
    deadline.check().map_err(failure)?;
    Ok(StopAcknowledgement::Stopping)
}

pub(crate) fn failure(error: NativeError) -> CallFailure {
    match error {
        NativeError::Timeout | NativeError::Cancelled | NativeError::OutcomeUnknown => {
            CallFailure::TimeoutOutcomeUnknown
        }
        _ => CallFailure::Unavailable,
    }
}
pub(crate) fn readonly(request: &InstallerRequest) -> bool {
    matches!(
        request,
        InstallerRequest::Status
            | InstallerRequest::PairStatus
            | InstallerRequest::PairScan
            | InstallerRequest::Windows
            | InstallerRequest::WindowsFrom { .. }
    )
}

pub(crate) fn run(
    endpoint: &dyn Endpoint,
    request: &InstallerRequest,
    deadline: &Deadline,
) -> (Result<DecodedReply, CallFailure>, ObservationSource) {
    let mut source = ObservationSource::Demo;
    let result = (|| {
        endpoint.check(deadline).map_err(failure)?;
        let status = endpoint.frame(&InstallerRequest::Status, deadline)?;
        if matches!(request, InstallerRequest::Status) {
            let pending = matches!(
                &status,
                DecodedReply::Status(StatusAdmission::PendingHealthContract(_))
            );
            if !pending {
                endpoint.admit_status(&status)?;
            }
            endpoint.check(deadline).map_err(failure)?;
            deadline.check().map_err(failure)?;
            if !pending {
                source = endpoint.source();
            }
            return Ok(status);
        }
        endpoint.admit_status(&status)?;
        drop(status);
        endpoint.check(deadline).map_err(failure)?;
        let result = endpoint.frame(request, deadline);
        if matches!(&result, Err(CallFailure::Refused(_))) {
            source = endpoint.source();
            return result;
        }
        let result = result?;
        let after = endpoint
            .frame(&InstallerRequest::Status, deadline)
            .and_then(|after| endpoint.admit_status(&after));
        if after.is_err() {
            return Err(if readonly(request) {
                CallFailure::Unavailable
            } else {
                CallFailure::TimeoutOutcomeUnknown
            });
        }
        endpoint.check(deadline).map_err(|e| {
            if readonly(request) {
                failure(e)
            } else {
                CallFailure::TimeoutOutcomeUnknown
            }
        })?;
        deadline.check().map_err(failure)?;
        source = endpoint.source();
        Ok(result)
    })();
    (result, source)
}

/// Private per-worker observation only. ACK/active=false never imply old endpoint pins settled.
#[derive(Clone)]
pub(crate) struct StopSettlement(Arc<AtomicBool>);
impl StopSettlement {
    pub(crate) fn wait(&self, deadline: &Deadline) -> NativeResult<()> {
        loop {
            deadline.check()?;
            if self.0.load(Ordering::Acquire) {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

pub struct WindowsAgentPort {
    // Consumed by native upgrade entry, which is deliberately cfg(not(test)).
    #[cfg_attr(test, allow(dead_code))]
    settlement: StopSettlement,
    queue: AgentQueue,
    sender: Option<mpsc::SyncSender<worker::Work>>,
    receiver: mpsc::Receiver<AgentReply>,
    endpoint: Arc<dyn Endpoint>,
    cancellation: super::native_io::Cancellation,
    clock: Arc<dyn Clock>,
    outstanding: usize,
    pending: BTreeMap<u64, Deadline>,
    active: Arc<AtomicBool>,
    retired: Arc<AtomicBool>,
}
impl std::fmt::Debug for WindowsAgentPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WindowsAgentPort")
    }
}
impl WindowsAgentPort {
    // Shipping consuming-stop integration; absent from the lib-test native entry graph.
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn stop_settlement(&self) -> StopSettlement {
        self.settlement.clone()
    }

    #[allow(dead_code)] // Lead750c0da2: selected installer operation integration awaits A4.
    pub(crate) fn installer_stop(
        self,
        expected_instance: u64,
        timeout_ms: u64,
    ) -> Result<StopAcknowledgement, CallFailure> {
        self.endpoint.admit_caller().map_err(failure)?;
        if self.outstanding != 0
            || self.active.load(Ordering::Acquire)
            || self.retired.load(Ordering::Acquire)
        {
            return Err(CallFailure::Unavailable);
        }
        if expected_instance == 0 {
            return Err(CallFailure::InvalidCall(
                crate::agent_contract::ContractError::InvalidValue,
            ));
        }
        if !(1..=crate::agent_contract::MAX_TIMEOUT_MS).contains(&timeout_ms) {
            return Err(CallFailure::InvalidCall(
                crate::agent_contract::ContractError::InvalidDeadline,
            ));
        }
        let sender = self.sender.as_ref().ok_or(CallFailure::Unavailable)?;
        let deadline = Deadline::new(timeout_ms, self.clock.clone(), self.cancellation.clone())
            .map_err(failure)?;
        let (reply, receive) = mpsc::sync_channel(1);
        sender
            .try_send(worker::Work {
                request: worker::WorkRequest::Stop {
                    expected_instance,
                    reply,
                },
                deadline: deadline.clone(),
                endpoint: self.endpoint.clone(),
                retired: self.retired.clone(),
            })
            .map_err(|_| CallFailure::Unavailable)?;
        loop {
            let remaining = match deadline.remaining_ms() {
                Ok(remaining) => remaining,
                Err(_) => {
                    self.retired.store(true, Ordering::Release);
                    return Err(CallFailure::TimeoutOutcomeUnknown);
                }
            };
            match receive.recv_timeout(std::time::Duration::from_millis(remaining.clamp(1, 10))) {
                Ok(result) => {
                    if deadline.check().is_err() {
                        self.retired.store(true, Ordering::Release);
                        return Err(CallFailure::TimeoutOutcomeUnknown);
                    }
                    return result;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.retired.store(true, Ordering::Release);
                    return Err(CallFailure::TimeoutOutcomeUnknown);
                }
            }
        }
    }
    #[cfg(test)]
    #[allow(dead_code)] // Source-included integration seam; primary library unit tests do not call it.
    pub(crate) fn workers() -> usize {
        worker::count()
    }
    #[cfg(windows)]
    pub fn new(
        io: Arc<super::native_io::WindowsNativeIo>,
        support: super::native_io::SupportProof,
        clock: Arc<dyn Clock>,
        deadline: &Deadline,
    ) -> NativeResult<Self> {
        let endpoint = security::native::NativeEndpoint::admit(io, support, deadline)?;
        Self::start(Arc::new(endpoint), clock)
    }
    /// Explicit fresh native detection only; old IDs and outstanding calls are never reset.
    #[cfg(windows)]
    pub fn redetect(
        &mut self,
        io: Arc<super::native_io::WindowsNativeIo>,
        support: super::native_io::SupportProof,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        if self.outstanding != 0 || self.active.load(Ordering::Acquire) {
            return Err(NativeError::Busy);
        }
        if self.sender.is_none() {
            return Err(NativeError::Unavailable);
        }
        let endpoint = security::native::NativeEndpoint::admit(io, support, deadline)?;
        self.replace(Arc::new(endpoint))
    }
    fn start(endpoint: Arc<dyn Endpoint>, clock: Arc<dyn Clock>) -> NativeResult<Self> {
        let cancellation = super::native_io::Cancellation::default();
        let (sender, receiver, active, settlement) =
            worker::start(cancellation.clone(), clock.clone())?;
        Ok(Self {
            settlement,
            queue: AgentQueue::default(),
            sender: Some(sender),
            receiver,
            endpoint,
            cancellation,
            clock,
            outstanding: 0,
            pending: BTreeMap::new(),
            active,
            retired: Arc::new(AtomicBool::new(false)),
        })
    }
    #[cfg(test)]
    #[allow(dead_code)] // This factory is unavailable to product builds.
    pub(crate) fn fake(endpoint: Arc<dyn Endpoint>, clock: Arc<dyn Clock>) -> NativeResult<Self> {
        Self::start(endpoint, clock)
    }
    fn replace(&mut self, endpoint: Arc<dyn Endpoint>) -> NativeResult<()> {
        if self.outstanding != 0 || self.active.load(Ordering::Acquire) {
            return Err(NativeError::Busy);
        }
        if self.sender.is_none() {
            return Err(NativeError::Unavailable);
        }
        self.endpoint = endpoint;
        self.retired = Arc::new(AtomicBool::new(false));
        Ok(())
    }
    #[cfg(test)]
    #[allow(dead_code)] // This comparison-only handoff is unavailable to product builds.
    pub(crate) fn fake_redetect(&mut self, endpoint: Arc<dyn Endpoint>) -> NativeResult<()> {
        self.replace(endpoint)
    }
    pub fn shutdown(&mut self) {
        self.cancellation.cancel();
        self.sender.take();
    }
}
impl Drop for WindowsAgentPort {
    fn drop(&mut self) {
        self.shutdown();
        if self.active.load(Ordering::Acquire) {
            use std::io::Write;
            let _ = writeln!(
                std::io::stderr(),
                "installer pipe worker remains owned; cleanup unverified"
            );
        }
    }
}
impl AgentPort for WindowsAgentPort {
    fn submit(&mut self, call: AgentCall) -> Result<(), CallFailure> {
        let sender = self.sender.as_ref().ok_or(CallFailure::Unavailable)?;
        if self.outstanding == MAX_QUEUE {
            return Err(CallFailure::QueueFull);
        }
        // Lead1a000c0e: this one read-only caller-thread guard precedes queue/ID admission.
        // A fresh native worker cannot observe the submitter's impersonation token.
        self.endpoint.admit_caller().map_err(failure)?;
        let deadline = Deadline::new(
            call.timeout_ms,
            self.clock.clone(),
            self.cancellation.clone(),
        )
        .map_err(|_| {
            CallFailure::InvalidCall(crate::agent_contract::ContractError::InvalidDeadline)
        })?;
        self.queue.submit(call)?;
        self.outstanding += 1;
        for call in self.queue.take_calls() {
            self.pending.insert(call.id, deadline.clone());
            let work = worker::Work {
                request: worker::WorkRequest::Ordinary(call.clone()),
                deadline: deadline.clone(),
                endpoint: self.endpoint.clone(),
                retired: self.retired.clone(),
            };
            if sender.try_send(work).is_err() {
                self.pending.remove(&call.id);
                self.queue
                    .push_reply(AgentReply {
                        id: call.id,
                        observed_at_ms: self.clock.now_ms(),
                        source: ObservationSource::Demo,
                        result: Err(CallFailure::Unavailable),
                    })
                    .map_err(CallFailure::InvalidCall)?;
            }
        }
        Ok(())
    }
    fn poll(&mut self) -> Vec<AgentReply> {
        for mut reply in self.receiver.try_iter().take(MAX_QUEUE) {
            let Some(deadline) = self.pending.remove(&reply.id) else {
                continue;
            };
            if deadline.check().is_err() {
                self.retired.store(true, Ordering::Release);
                reply.source = ObservationSource::Demo;
                reply.result = Err(CallFailure::TimeoutOutcomeUnknown);
            }
            if self.queue.push_reply(reply).is_err() {
                self.shutdown();
                break;
            }
        }
        let expired: Vec<_> = self
            .pending
            .iter()
            .filter_map(|(&id, deadline)| deadline.check().err().map(|_| id))
            .collect();
        for id in expired {
            self.retired.store(true, Ordering::Release);
            self.pending.remove(&id);
            if self
                .queue
                .push_reply(AgentReply {
                    id,
                    observed_at_ms: self.clock.now_ms(),
                    source: ObservationSource::Demo,
                    result: Err(CallFailure::TimeoutOutcomeUnknown),
                })
                .is_err()
            {
                self.shutdown();
                break;
            }
        }
        let replies = self.queue.poll();
        self.outstanding = self.outstanding.saturating_sub(replies.len());
        replies
    }
}
