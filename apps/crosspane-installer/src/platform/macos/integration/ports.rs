//! The GUI-thread ports: the agent port, which asks the worker for a fresh admission of the
//! running agent before it sends, and the practice-fixture launcher. Neither blocks a frame.
//!
//! An agent admission (the signature, the support proof and the instance) lives five seconds, so
//! nothing here caches one for long. The worker thread mints admissions on request; the port
//! reuses one only for a moment, and a peer-bound call is bound to the link the last Status
//! showed for that peer.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};

use crosspane_installer_core::{AttemptId, ObservationSource};
use crosspane_types::id::NodeId;

use super::super::native_io::NativeError;
use super::super::transport::{CallerClock, MacAgentPort, SelectedLink};
use super::super::tutorial::fixture_port;
use super::domains::{Admitted, AdmittedInner, FixtureChild, FixtureChildInner};
use crate::agent_contract::{
    AgentCall, AgentPort, AgentReply, CallFailure, ContractError, DecodedReply, InstallerRequest,
    StatusAdmission,
};
use crate::fixture::{
    FixtureCall, FixtureError, FixtureId, FixturePort, FixtureReceipt, PipeFixturePort,
};
use crate::live::{Clock, FixtureReadiness, NativeJob, PracticeFixtures};

/// What the GUI thread can send the worker.
#[derive(Debug)]
pub enum Command {
    Job(NativeJob),
    /// Admit the running agent now, bound to this peer link when one is given.
    Admit {
        ticket: u64,
        link: Option<SelectedLink>,
    },
    /// Launch the practice fixture now.
    Fixture {
        ticket: u64,
        font: PathBuf,
    },
}

type Admission = Result<Admitted, String>;
type Launch = Result<FixtureChild, String>;

/// Hands out admissions and fixture launches that the worker thread performs, one request at a
/// time, by ticket.
pub struct Broker {
    sender: Mutex<Option<SyncSender<Command>>>,
    admissions: Mutex<BTreeMap<u64, Admission>>,
    launches: Mutex<BTreeMap<u64, Launch>>,
    /// Launch tickets nobody will collect: a window launched for one is retired at once.
    abandoned: Mutex<BTreeSet<u64>>,
    next: AtomicU64,
}

impl Broker {
    pub fn new(sender: SyncSender<Command>) -> Arc<Self> {
        Arc::new(Self {
            sender: Mutex::new(Some(sender)),
            admissions: Mutex::new(BTreeMap::new()),
            launches: Mutex::new(BTreeMap::new()),
            abandoned: Mutex::new(BTreeSet::new()),
            next: AtomicU64::new(1),
        })
    }

    fn send(&self, command: Command) -> bool {
        self.sender
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|s| s.try_send(command).is_ok()))
            .unwrap_or(false)
    }

    fn ticket(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    /// Ask for an admission. `None` means the worker is gone or too busy to take the request.
    pub fn request_admission(&self, link: Option<SelectedLink>) -> Option<u64> {
        let ticket = self.ticket();
        self.send(Command::Admit { ticket, link }).then_some(ticket)
    }

    pub fn request_launch(&self, font: PathBuf) -> Option<u64> {
        let ticket = self.ticket();
        self.send(Command::Fixture { ticket, font })
            .then_some(ticket)
    }

    pub fn take_admission(&self, ticket: u64) -> Option<Admission> {
        self.admissions.lock().ok()?.remove(&ticket)
    }

    pub fn take_launch(&self, ticket: u64) -> Option<Launch> {
        self.launches.lock().ok()?.remove(&ticket)
    }

    /// Worker side: record a result. Requests that were never collected can't pile up.
    pub fn put_admission(&self, ticket: u64, result: Admission) {
        if let Ok(mut map) = self.admissions.lock() {
            while map.len() >= 16 {
                let Some(oldest) = map.keys().next().copied() else {
                    break;
                };
                map.remove(&oldest);
            }
            map.insert(ticket, result);
        }
    }

    /// The GUI no longer wants this launch. A window already launched for it is dropped now
    /// (which retires its process); one still being launched is dropped when it arrives.
    pub fn abandon_launch(&self, ticket: u64) {
        let launched = self
            .launches
            .lock()
            .ok()
            .and_then(|mut m| m.remove(&ticket));
        if launched.is_none()
            && let Ok(mut abandoned) = self.abandoned.lock()
        {
            while abandoned.len() >= 16 {
                let Some(oldest) = abandoned.iter().next().copied() else {
                    break;
                };
                abandoned.remove(&oldest);
            }
            abandoned.insert(ticket);
        }
        drop(launched);
    }

    pub fn put_launch(&self, ticket: u64, result: Launch) {
        if self
            .abandoned
            .lock()
            .is_ok_and(|mut abandoned| abandoned.remove(&ticket))
        {
            // Nobody will collect it: dropping the child retires the window and its process.
            drop(result);
            return;
        }
        if let Ok(mut map) = self.launches.lock() {
            while map.len() >= 4 {
                let Some(oldest) = map.keys().next().copied() else {
                    break;
                };
                map.remove(&oldest);
            }
            map.insert(ticket, result);
        }
    }

    pub fn close(&self) {
        if let Ok(mut guard) = self.sender.lock() {
            guard.take();
        }
    }
}

/// An agent port that can be pointed at a fresh admission of the running agent.
pub trait AgentBackend: AgentPort + Send {
    /// No call is in flight, so the next admission can be taken.
    fn idle(&self) -> bool;
    /// Point the port at `admitted`. Only called while `idle`.
    fn adopt(&mut self, admitted: Admitted) -> Result<(), NativeError>;
}

/// The bounded socket port to the real agent.
pub struct NativeBackend {
    port: Option<MacAgentPort>,
    clock: CallerClock,
    outstanding: usize,
}

impl NativeBackend {
    pub fn new(clock: CallerClock) -> Self {
        Self {
            port: None,
            clock,
            outstanding: 0,
        }
    }
}

impl AgentPort for NativeBackend {
    fn submit(&mut self, call: AgentCall) -> Result<(), CallFailure> {
        let port = self.port.as_mut().ok_or(CallFailure::Unavailable)?;
        port.submit(call)?;
        self.outstanding += 1;
        Ok(())
    }

    fn poll(&mut self) -> Vec<AgentReply> {
        let replies = self.port.as_mut().map(AgentPort::poll).unwrap_or_default();
        self.outstanding = self.outstanding.saturating_sub(replies.len());
        replies
    }
}

impl AgentBackend for NativeBackend {
    fn idle(&self) -> bool {
        self.outstanding == 0
    }

    fn adopt(&mut self, admitted: Admitted) -> Result<(), NativeError> {
        let AdmittedInner::Native(selected) = admitted.inner else {
            return Err(NativeError::Unsupported);
        };
        match self.port.as_mut() {
            Some(port) => port.redetect(*selected),
            None => {
                self.port = Some(MacAgentPort::new(*selected, self.clock.clone())?);
                Ok(())
            }
        }
    }
}

fn readonly(request: &InstallerRequest) -> bool {
    matches!(
        request,
        InstallerRequest::Status
            | InstallerRequest::PairStatus
            | InstallerRequest::PairScan
            | InstallerRequest::Windows
    )
}

/// The peer a call is bound to, if any.
fn bound_peer(request: &InstallerRequest) -> Option<NodeId> {
    match request {
        InstallerRequest::Project { peer, .. }
        | InstallerRequest::Pull { peer, .. }
        | InstallerRequest::Allow { peer, .. }
        | InstallerRequest::WindowsFrom { peer } => Some(*peer),
        InstallerRequest::Return { source, .. } => *source,
        _ => None,
    }
}

/// How long one admission may serve calls. The proofs live five seconds, and the transport
/// rechecks them as it sends, so a short reuse leaves it ample margin.
const REUSE_MS: u64 = 1_500;
const MAX_QUEUED: usize = 32;

struct Queued {
    call: AgentCall,
    need: Need,
    expires_at: u64,
}

/// What an admission must be bound to for a call to use it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Need {
    Plain,
    Peer(SelectedLink),
}

impl Need {
    fn link(self) -> Option<SelectedLink> {
        match self {
            Self::Plain => None,
            Self::Peer(link) => Some(link),
        }
    }
}

struct Current {
    /// When the admission was asked for: it was minted no earlier, so its age is at most
    /// `now - at`, and reuse is measured from here.
    at: u64,
    need: Need,
}

/// Calls are forwarded in id order. A call waits for an admission that is fresh and bound to the
/// link it needs; the ones behind it wait too, so the backend always sees increasing ids.
pub struct AgentSlot {
    backend: Box<dyn AgentBackend>,
    broker: Arc<Broker>,
    clock: Clock,
    queue: VecDeque<Queued>,
    current: Option<Current>,
    asked: Option<(u64, Need, u64)>,
    held: Option<(Admitted, Need, u64)>,
    failed: Vec<AgentReply>,
    last_id: u64,
    /// The link generation each connected peer showed in the last Status.
    links: BTreeMap<NodeId, u64>,
}

impl AgentSlot {
    pub fn new(backend: Box<dyn AgentBackend>, broker: Arc<Broker>, clock: Clock) -> Self {
        Self {
            backend,
            broker,
            clock,
            queue: VecDeque::new(),
            current: None,
            asked: None,
            held: None,
            failed: Vec::new(),
            last_id: 0,
            links: BTreeMap::new(),
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

    fn fail_all(&mut self) {
        while let Some(entry) = self.queue.pop_front() {
            self.fail(entry.call.id, CallFailure::Unavailable);
        }
    }

    fn need_for(&self, request: &InstallerRequest) -> Option<Need> {
        match bound_peer(request) {
            None => Some(Need::Plain),
            Some(node) => self.links.get(&node).map(|generation| {
                Need::Peer(SelectedLink {
                    node,
                    generation: *generation,
                })
            }),
        }
    }

    fn pump(&mut self) {
        let now = (self.clock)();
        // Calls that waited past their own deadline are never sent, wherever they wait.
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
        // A ticket's result arrives from the worker.
        if let Some((ticket, need, asked_at)) = self.asked
            && let Some(result) = self.broker.take_admission(ticket)
        {
            self.asked = None;
            match result {
                Ok(admitted) => self.held = Some((admitted, need, asked_at)),
                Err(_) => self.fail_all(),
            }
        }
        // A fresh admission waits for the backend to drain before it is taken.
        if self.held.is_some() && self.backend.idle() {
            self.take_held(now);
        }
        while let Some(front) = self.queue.front() {
            let fresh = self
                .current
                .as_ref()
                .is_some_and(|c| c.need == front.need && now.saturating_sub(c.at) <= REUSE_MS);
            if fresh {
                let Some(mut entry) = self.queue.pop_front() else {
                    break;
                };
                let id = entry.call.id;
                // Only what is left of the call's own budget goes with it.
                entry.call.timeout_ms = entry.expires_at.saturating_sub(now).max(1);
                if let Err(failure) = self.backend.submit(entry.call) {
                    self.fail(id, failure);
                }
                continue;
            }
            // Ask only once the backend has drained, so the admission is adopted as soon as it
            // arrives and its short reuse window isn't spent waiting.
            if self.asked.is_none() && self.held.is_none() && self.backend.idle() {
                let need = front.need;
                match self.broker.request_admission(need.link()) {
                    Some(ticket) => self.asked = Some((ticket, need, now)),
                    None => self.fail_all(),
                }
            }
            break;
        }
    }

    fn take_held(&mut self, now: u64) {
        if let Some((admitted, need, asked_at)) = self.held.take() {
            if now.saturating_sub(asked_at) > REUSE_MS {
                // Too old to serve anything now; a fresh one is asked for.
                self.current = None;
                return;
            }
            if self.backend.adopt(admitted).is_ok() {
                self.current = Some(Current { at: asked_at, need });
            } else {
                self.current = None;
                self.fail_all();
            }
        }
    }

    fn note_links(&mut self, replies: &[AgentReply]) {
        for reply in replies {
            if let Ok(DecodedReply::Status(StatusAdmission::Supported(health))) = &reply.result {
                self.links = health
                    .installer()
                    .peers
                    .iter()
                    .filter(|p| p.connected)
                    .filter_map(|p| p.link_generation.map(|g| (p.node, g)))
                    .collect();
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
            return Err(CallFailure::QueueFull);
        }
        let now = (self.clock)();
        self.last_id = call.id;
        let Some(need) = self.need_for(&call.request) else {
            // The peer isn't connected as far as the last Status showed.
            let id = call.id;
            self.fail(id, CallFailure::Unavailable);
            return Ok(());
        };
        // A mutation never rides a stale admission, even one bound to the right link.
        if !readonly(&call.request)
            && self
                .current
                .as_ref()
                .is_some_and(|c| now.saturating_sub(c.at) > REUSE_MS / 2)
        {
            self.current = None;
        }
        self.queue.push_back(Queued {
            expires_at: now.saturating_add(call.timeout_ms.max(1)),
            call,
            need,
        });
        self.pump();
        Ok(())
    }

    fn poll(&mut self) -> Vec<AgentReply> {
        self.pump();
        let mut out = std::mem::take(&mut self.failed);
        let replies = self.backend.poll();
        self.note_links(&replies);
        out.extend(replies);
        out
    }
}

/// Launches the installed practice fixture once the worker has admitted it. `launch` never
/// blocks, and a window the worker refuses never appears.
pub struct MacPractice {
    broker: Arc<Broker>,
    clock: Clock,
    font: PathBuf,
    ticket: Option<u64>,
    port: Option<PipeFixturePort>,
    failed: Option<FixtureError>,
}

impl MacPractice {
    pub fn new(broker: Arc<Broker>, clock: Clock, font: PathBuf) -> Self {
        Self {
            broker,
            clock,
            font,
            ticket: None,
            port: None,
            failed: None,
        }
    }
}

impl PracticeFixtures for MacPractice {
    fn launch(&mut self, _attempt: AttemptId) -> Result<(), FixtureError> {
        self.retire();
        self.ticket = Some(
            self.broker
                .request_launch(self.font.clone())
                .ok_or(FixtureError::Unavailable)?,
        );
        Ok(())
    }

    fn readiness(&mut self) -> FixtureReadiness {
        if let Some(error) = self.failed {
            return FixtureReadiness::Failed(error);
        }
        if self.port.is_some() {
            return FixtureReadiness::Ready;
        }
        let Some(ticket) = self.ticket else {
            return FixtureReadiness::Idle;
        };
        match self.broker.take_launch(ticket) {
            None => FixtureReadiness::Launching,
            Some(Err(_)) => {
                self.ticket = None;
                self.failed = Some(FixtureError::Unavailable);
                FixtureReadiness::Failed(FixtureError::Unavailable)
            }
            Some(Ok(child)) => {
                self.ticket = None;
                let FixtureChildInner::Native(child) = child.inner;
                match fixture_port(*child, self.clock.clone()) {
                    Ok(port) => {
                        self.port = Some(port);
                        FixtureReadiness::Ready
                    }
                    Err(error) => {
                        self.failed = Some(error);
                        FixtureReadiness::Failed(error)
                    }
                }
            }
        }
    }

    fn submit(&mut self, call: FixtureCall) -> Result<(), FixtureError> {
        FixturePort::submit(self.port.as_mut().ok_or(FixtureError::Unavailable)?, call)
    }

    fn poll(&mut self) -> Vec<FixtureReceipt> {
        self.port
            .as_mut()
            .map(PipeFixturePort::poll_receipts)
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
        if let Some(ticket) = self.ticket.take() {
            self.broker.abandon_launch(ticket);
        }
        self.failed = None;
        if let Some(mut port) = self.port.take() {
            port.cancel();
        }
    }
}

macro_rules! opaque_debug {
    ($($ty:ty),+ $(,)?) => {$(
        impl std::fmt::Debug for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(stringify!($ty))
            }
        }
    )+};
}

opaque_debug!(Broker, AgentSlot, MacPractice, NativeBackend);

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_launch_nobody_will_collect_is_dropped_whether_it_arrives_before_or_after() {
        let (sender, _receiver) = std::sync::mpsc::sync_channel(8);
        let broker = Broker::new(sender);
        // Abandoned while the worker is still launching: the late result is dropped on arrival.
        let early = broker.request_launch(PathBuf::from("/font")).unwrap();
        broker.abandon_launch(early);
        broker.put_launch(early, Err("late".into()));
        assert!(broker.take_launch(early).is_none());
        // Abandoned after it arrived: it is taken and dropped at once.
        let late = broker.request_launch(PathBuf::from("/font")).unwrap();
        broker.put_launch(late, Err("arrived".into()));
        broker.abandon_launch(late);
        assert!(broker.take_launch(late).is_none());
        // A launch that is still wanted is kept for its ticket.
        let kept = broker.request_launch(PathBuf::from("/font")).unwrap();
        broker.put_launch(kept, Err("wanted".into()));
        assert!(broker.take_launch(kept).is_some());
    }

    #[test]
    fn retiring_a_practice_mid_launch_abandons_its_ticket() {
        let (sender, _receiver) = std::sync::mpsc::sync_channel(8);
        let broker = Broker::new(sender);
        let mut practice = MacPractice::new(broker.clone(), Arc::new(|| 0), PathBuf::from("/f"));
        practice.launch(AttemptId(1)).unwrap();
        let ticket = practice.ticket.unwrap();
        practice.retire();
        broker.put_launch(ticket, Err("late".into()));
        assert!(broker.take_launch(ticket).is_none());
    }
}
