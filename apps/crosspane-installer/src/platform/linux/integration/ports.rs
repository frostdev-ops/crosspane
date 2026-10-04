//! The GUI-thread ports: the agent port that obtains a fresh support proof before every mutating
//! call, and the practice-fixture launcher. Neither blocks a frame.
//!
//! A `SupportProof` is only valid for five seconds, so nothing caches one. The worker thread
//! mints proofs on request; these ports ask for one immediately before each mutation.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};

use crosspane_installer_core::{AttemptId, ObservationSource};

use super::super::native_io::{ChildEnvironment, LinuxNativeIo, NativeError, SupportProof};
use super::super::transport::LinuxAgentPort;
use super::super::tutorial::{LinuxFixtureLaunch, LinuxFixturePort, launch as launch_fixture};
use crate::agent_contract::{
    AgentCall, AgentPort, AgentReply, CallFailure, ContractError, InstallerRequest,
};
use crate::fixture::{FixtureCall, FixtureError, FixtureId, FixturePort, FixtureReceipt};
use crate::live::{Clock, FixtureReadiness, NativeJob, PracticeFixtures};

/// What the GUI thread can send the worker.
#[derive(Debug)]
pub enum Command {
    Job(NativeJob),
    Proof(u64),
}

type ProofResult = Result<SupportProof, String>;

/// Hands out support proofs minted by the worker thread, one request at a time.
pub struct ProofBroker {
    sender: Mutex<Option<SyncSender<Command>>>,
    results: Arc<Mutex<BTreeMap<u64, ProofResult>>>,
    next: AtomicU64,
}

impl ProofBroker {
    pub fn new(sender: SyncSender<Command>) -> (Arc<Self>, Arc<Mutex<BTreeMap<u64, ProofResult>>>) {
        let results = Arc::new(Mutex::new(BTreeMap::new()));
        (
            Arc::new(Self {
                sender: Mutex::new(Some(sender)),
                results: results.clone(),
                next: AtomicU64::new(1),
            }),
            results,
        )
    }

    /// Ask for a fresh proof. `None` means the worker is gone or too busy to take the request.
    pub fn request(&self) -> Option<u64> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let guard = self.sender.lock().ok()?;
        guard.as_ref()?.try_send(Command::Proof(id)).ok()?;
        Some(id)
    }

    pub fn take(&self, id: u64) -> Option<ProofResult> {
        self.results.lock().ok()?.remove(&id)
    }

    pub fn close(&self) {
        if let Ok(mut guard) = self.sender.lock() {
            guard.take();
        }
    }
}

/// An agent port whose mutating calls can carry a fresh support proof.
pub trait SupportedAgentPort: AgentPort + Send {
    fn refresh_support(&mut self, proof: SupportProof) -> Result<(), NativeError>;
}

impl SupportedAgentPort for LinuxAgentPort {
    fn refresh_support(&mut self, proof: SupportProof) -> Result<(), NativeError> {
        LinuxAgentPort::refresh_support(self, proof)
    }
}

fn readonly(request: &InstallerRequest) -> bool {
    matches!(
        request,
        InstallerRequest::Status
            | InstallerRequest::PairStatus
            | InstallerRequest::PairScan
            | InstallerRequest::Windows
            | InstallerRequest::WindowsFrom { .. }
    )
}

struct Queued {
    call: AgentCall,
    ticket: Option<u64>,
    /// The call's own deadline, from when it was submitted. Waiting for a proof (or behind a
    /// call that waits for one) spends it; it is never restarted.
    expires_at: u64,
}

/// Calls are forwarded in id order; a mutating call waits for its proof and holds the ones
/// behind it, so the inner queue always sees strictly increasing ids.
pub struct AgentSlot {
    inner: Box<dyn SupportedAgentPort>,
    broker: Arc<ProofBroker>,
    clock: Clock,
    queue: VecDeque<Queued>,
    failed: Vec<AgentReply>,
    last_id: u64,
}

const MAX_QUEUED: usize = 32;

impl AgentSlot {
    pub fn new(inner: Box<dyn SupportedAgentPort>, broker: Arc<ProofBroker>, clock: Clock) -> Self {
        Self {
            inner,
            broker,
            clock,
            queue: VecDeque::new(),
            failed: Vec::new(),
            last_id: 0,
        }
    }

    fn fail(&mut self, id: u64, failure: CallFailure) {
        self.failed.push(AgentReply {
            id,
            observed_at_ms: (self.clock)(),
            source: ObservationSource::Live,
            result: Err(failure),
        });
    }

    fn pump(&mut self) {
        // A call whose own time ran out while it waited is never sent: it fails as unavailable,
        // wherever it sits in the queue.
        let now = (self.clock)();
        let mut expired = Vec::new();
        self.queue.retain(|q| {
            let keep = q.expires_at > now;
            if !keep {
                expired.push(q.call.id);
            }
            keep
        });
        for id in expired {
            self.fail(id, CallFailure::Unavailable);
        }
        while let Some(head) = self.queue.front() {
            let id = head.call.id;
            match head.ticket {
                None => {}
                Some(ticket) => match self.broker.take(ticket) {
                    None => break,
                    Some(Ok(proof)) => {
                        if self.inner.refresh_support(proof).is_err() {
                            self.queue.pop_front();
                            self.fail(id, CallFailure::Unavailable);
                            continue;
                        }
                    }
                    Some(Err(_)) => {
                        // Support isn't proved right now, so nothing is sent.
                        self.queue.pop_front();
                        self.fail(id, CallFailure::Unavailable);
                        continue;
                    }
                },
            }
            let Some(mut entry) = self.queue.pop_front() else {
                break;
            };
            // Only what is left of the call's own budget goes with it.
            entry.call.timeout_ms = entry.expires_at.saturating_sub(now).max(1);
            if let Err(failure) = self.inner.submit(entry.call) {
                self.fail(id, failure);
            }
        }
    }
}

impl AgentPort for AgentSlot {
    fn submit(&mut self, call: AgentCall) -> Result<(), CallFailure> {
        if call.id <= self.last_id {
            return Err(CallFailure::InvalidCall(ContractError::InvalidValue));
        }
        if self.queue.len() >= MAX_QUEUED {
            return Err(CallFailure::Unavailable);
        }
        let ticket = if readonly(&call.request) {
            None
        } else {
            Some(self.broker.request().ok_or(CallFailure::Unavailable)?)
        };
        self.last_id = call.id;
        let expires_at = (self.clock)().saturating_add(call.timeout_ms);
        self.queue.push_back(Queued {
            call,
            ticket,
            expires_at,
        });
        self.pump();
        Ok(())
    }

    fn poll(&mut self) -> Vec<AgentReply> {
        self.pump();
        let mut out = std::mem::take(&mut self.failed);
        out.extend(self.inner.poll());
        out
    }
}

/// Launches the installed practice fixture once a fresh proof arrives. `launch` never blocks.
pub struct LinuxPractice {
    io: Arc<LinuxNativeIo>,
    env: ChildEnvironment,
    clock: Clock,
    font: PathBuf,
    broker: Arc<ProofBroker>,
    tutorial_hash: Arc<Mutex<Option<[u8; 32]>>>,
    ticket: Option<u64>,
    launch: Option<LinuxFixtureLaunch>,
    port: Option<LinuxFixturePort>,
    failed: Option<FixtureError>,
}

impl LinuxPractice {
    pub fn new(
        io: Arc<LinuxNativeIo>,
        env: ChildEnvironment,
        clock: Clock,
        font: PathBuf,
        broker: Arc<ProofBroker>,
        tutorial_hash: Arc<Mutex<Option<[u8; 32]>>>,
    ) -> Self {
        Self {
            io,
            env,
            clock,
            font,
            broker,
            tutorial_hash,
            ticket: None,
            launch: None,
            port: None,
            failed: None,
        }
    }
}

impl PracticeFixtures for LinuxPractice {
    fn launch(&mut self, _attempt: AttemptId) -> Result<(), FixtureError> {
        self.retire();
        self.ticket = Some(self.broker.request().ok_or(FixtureError::Unavailable)?);
        Ok(())
    }

    fn readiness(&mut self) -> FixtureReadiness {
        if let Some(error) = self.failed {
            return FixtureReadiness::Failed(error);
        }
        if self.port.is_some() {
            return FixtureReadiness::Ready;
        }
        if let Some(ticket) = self.ticket {
            match self.broker.take(ticket) {
                None => return FixtureReadiness::Launching,
                Some(Err(_)) => {
                    self.ticket = None;
                    self.failed = Some(FixtureError::Unavailable);
                    return FixtureReadiness::Failed(FixtureError::Unavailable);
                }
                Some(Ok(proof)) => {
                    self.ticket = None;
                    let hash = self.tutorial_hash.lock().ok().and_then(|g| *g);
                    let Some(hash) = hash else {
                        self.failed = Some(FixtureError::Unavailable);
                        return FixtureReadiness::Failed(FixtureError::Unavailable);
                    };
                    match launch_fixture(
                        self.io.clone(),
                        proof,
                        self.env.clone(),
                        hash,
                        self.font.clone(),
                        self.clock.clone(),
                    ) {
                        Ok(launch) => self.launch = Some(launch),
                        Err(error) => {
                            self.failed = Some(error);
                            return FixtureReadiness::Failed(error);
                        }
                    }
                }
            }
        }
        match self.launch.as_mut().and_then(LinuxFixtureLaunch::poll) {
            Some(Ok(port)) => {
                self.launch = None;
                self.port = Some(port);
                FixtureReadiness::Ready
            }
            Some(Err(error)) => {
                self.launch = None;
                self.failed = Some(error);
                FixtureReadiness::Failed(error)
            }
            None if self.launch.is_some() || self.ticket.is_some() => FixtureReadiness::Launching,
            None => FixtureReadiness::Idle,
        }
    }

    fn submit(&mut self, call: FixtureCall) -> Result<(), FixtureError> {
        FixturePort::submit(self.port.as_mut().ok_or(FixtureError::Unavailable)?, call)
    }

    fn poll(&mut self) -> Vec<FixtureReceipt> {
        self.port
            .as_mut()
            .map(LinuxFixturePort::poll_receipts)
            .unwrap_or_default()
    }

    fn complete_closed(
        &mut self,
        attempt: AttemptId,
        fixture: FixtureId,
    ) -> Result<(), FixtureError> {
        self.port
            .as_mut()
            .ok_or(FixtureError::Unavailable)?
            .complete_closed(attempt, fixture)
    }

    fn retire(&mut self) {
        self.ticket = None;
        self.failed = None;
        self.launch = None;
        if let Some(mut port) = self.port.take() {
            port.cancel();
        }
    }
}

opaque_debug!(ProofBroker, AgentSlot, LinuxPractice);
