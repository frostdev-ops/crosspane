//! Frame-facing queues contain requests and observations, never native capabilities.
use super::worker::Fence;
use crate::{
    agent_contract::{AgentCall, AgentPort, AgentReply, CallFailure},
    live::NativeJob,
};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::AtomicBool,
        mpsc::{Receiver, SyncSender},
    },
};

pub(crate) enum Command {
    Job { job: NativeJob, fence: Fence },
    Call { call: AgentCall, queued_at: u64 },
}
#[derive(Default)]
pub(crate) struct CloseState {
    pub handed_off: AtomicBool,
}
pub(crate) struct AgentSlot {
    pub sender: SyncSender<Command>,
    pub receiver: Receiver<AgentReply>,
    pub clock: crate::live::Clock,
    pending: BTreeMap<u64, u64>,
    last: u64,
}
impl AgentSlot {
    pub fn new(
        sender: SyncSender<Command>,
        receiver: Receiver<AgentReply>,
        clock: crate::live::Clock,
    ) -> Self {
        Self {
            sender,
            receiver,
            clock,
            pending: BTreeMap::new(),
            last: 0,
        }
    }
}
impl AgentPort for AgentSlot {
    fn submit(&mut self, call: AgentCall) -> Result<(), CallFailure> {
        #[cfg(all(windows, not(test)))]
        super::super::native_io::identity::native::refuse_impersonation()
            .map_err(|_| CallFailure::Unavailable)?;
        if !(1..=crate::agent_contract::MAX_TIMEOUT_MS).contains(&call.timeout_ms) {
            return Err(CallFailure::Unavailable);
        }
        if call.id <= self.last || self.pending.len() >= 32 {
            return Err(CallFailure::QueueFull);
        }
        let now = (self.clock)();
        let id = call.id;
        let expiry = now.saturating_add(call.timeout_ms);
        self.sender
            .try_send(Command::Call {
                call,
                queued_at: now,
            })
            .map_err(|_| CallFailure::QueueFull)?;
        self.pending.insert(id, expiry);
        self.last = id;
        Ok(())
    }
    fn poll(&mut self) -> Vec<AgentReply> {
        let now = (self.clock)();
        let mut out = Vec::new();
        for reply in self.receiver.try_iter().take(32) {
            if self.pending.remove(&reply.id).is_some() {
                out.push(reply);
            }
        }
        let expired: Vec<_> = self
            .pending
            .iter()
            .filter_map(|(id, end)| (now >= *end).then_some(*id))
            .collect();
        for id in expired {
            self.pending.remove(&id);
            out.push(AgentReply {
                id,
                observed_at_ms: now,
                source: crosspane_installer_core::ObservationSource::Demo,
                result: Err(CallFailure::TimeoutOutcomeUnknown),
            });
        }
        out
    }
}
