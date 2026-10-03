//! Bounded selected-agent exchanges. No mutation is automatically retried or rebound.
//! Admission proves endpoint plus instance, not kernel peer credentials; the approved macOS
//! ACL residual and same-UID threat boundary are documented in the frozen native foundation.
use super::native_io::{
    AdmittedInstance, Cancellation, Clock, Deadline, MacNativeIo, NativeError, NativeResult,
    SupportProof,
};
use crate::agent_contract::{
    AgentCall, AgentPlatform, AgentPort, AgentQueue, AgentReply, CallFailure, ContractError,
    DecodedReply, HealthSnapshot, InstallerRequest, MAX_QUEUE, MAX_RESPONSE_BYTES, MAX_TIMEOUT_MS,
    ObservationSource, StatusAdmission, decode_reply, encode_request,
};
use crosspane_types::id::NodeId;
use rustix::{
    io as rio,
    net::{self, AddressFamily, SocketAddrUnix, SocketType},
};
use std::{
    io::{Read, Write},
    net::{Shutdown, SocketAddr, ToSocketAddrs},
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::Duration,
};

pub type CallerClock = Arc<dyn Fn() -> u64 + Send + Sync>;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectedLink {
    pub node: NodeId,
    pub generation: u64,
}
/// Detector-owned proofs; construction performs no I/O and confers no additional authority.
#[derive(Clone)]
pub struct SelectedAgent {
    pub io: Arc<MacNativeIo>,
    pub support: SupportProof,
    pub instance: Arc<AdmittedInstance>,
    pub link: Option<SelectedLink>,
}
impl std::fmt::Debug for SelectedAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SelectedAgent { .. }")
    }
}
static TRANSPORT_WORKERS: AtomicUsize = AtomicUsize::new(0);
static RESOLVER_WORKERS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
type TestHook = Arc<dyn Fn(&std::path::Path, &str) + Send + Sync>;
#[cfg(test)]
pub(crate) static HOOK: std::sync::Mutex<Option<TestHook>> = std::sync::Mutex::new(None);
#[cfg(test)]
fn event(selected: &SelectedAgent, stage: &str) {
    if let Some(hook) = HOOK.lock().ok().and_then(|hook| hook.clone()) {
        hook(&selected.io.target().paths().home, stage);
    }
}
struct Slot(&'static AtomicUsize);
impl Slot {
    fn take(counter: &'static AtomicUsize) -> NativeResult<Self> {
        counter
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 4).then_some(n + 1)
            })
            .map_err(|_| NativeError::Busy)?;
        Ok(Self(counter))
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}
fn failure(error: NativeError) -> CallFailure {
    match error {
        NativeError::Timeout | NativeError::Cancelled | NativeError::OutcomeUnknown => {
            CallFailure::TimeoutOutcomeUnknown
        }
        _ => CallFailure::Unavailable,
    }
}
fn pause(deadline: &Deadline) -> Result<(), CallFailure> {
    deadline.check().map_err(failure)?;
    thread::sleep(Duration::from_millis(2));
    deadline.check().map_err(failure)
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
pub(crate) fn reply_source(admitted: bool, selected: ObservationSource) -> ObservationSource {
    if admitted {
        selected
    } else {
        ObservationSource::Demo
    }
}
struct Work {
    call: AgentCall,
    deadline: Deadline,
    selected: Arc<SelectedAgent>,
    generation: u64,
    source: ObservationSource,
}
pub struct MacAgentPort {
    queue: AgentQueue,
    sender: Option<SyncSender<Work>>,
    receiver: Receiver<AgentReply>,
    selected: Arc<SelectedAgent>,
    cancellation: Cancellation,
    clock: CallerClock,
    generation: u64,
    outstanding: usize,
    source: ObservationSource,
}
impl std::fmt::Debug for MacAgentPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MacAgentPort { .. }")
    }
}
impl MacAgentPort {
    /// Starts a bounded worker; native admission occurs there, never in submit/poll/new.
    pub fn new(selected: SelectedAgent, clock: CallerClock) -> NativeResult<Self> {
        let slot = Slot::take(&TRANSPORT_WORKERS)?;
        let (sender, calls) = mpsc::sync_channel::<Work>(MAX_QUEUE);
        let (results, receiver) = mpsc::sync_channel(MAX_QUEUE);
        let cancellation = Cancellation::default();
        let stop = cancellation.clone();
        let receipts = clock.clone();
        thread::Builder::new()
            .name("installer-mac-agent".into())
            .spawn(move || {
                let _slot = slot;
                let mut uncertain = None;
                loop {
                    let work = match calls.recv_timeout(Duration::from_millis(2)) {
                        Ok(work) => work,
                        Err(mpsc::RecvTimeoutError::Timeout) if !stop.is_cancelled() => continue,
                        Err(_) => break,
                    };
                    let mutation = !readonly(&work.call.request);
                    let mut observed = receipts();
                    let (result, admitted) = if mutation && uncertain == Some(work.generation) {
                        (Err(CallFailure::Unavailable), false)
                    } else {
                        run(
                            &work.selected,
                            &work.call.request,
                            &work.deadline,
                            &receipts,
                            &mut observed,
                        )
                    };
                    if mutation
                        && result
                            .as_ref()
                            .is_err_and(|e| !matches!(e, CallFailure::Refused(_)))
                    {
                        uncertain = Some(work.generation);
                    }
                    if results
                        .try_send(AgentReply {
                            id: work.call.id,
                            observed_at_ms: observed,
                            source: reply_source(admitted, work.source),
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .map_err(|_| NativeError::Unavailable)?;
        let source = selected.io.target().source();
        Ok(Self {
            queue: AgentQueue::default(),
            sender: Some(sender),
            receiver,
            selected: Arc::new(selected),
            cancellation,
            clock,
            generation: 1,
            outstanding: 0,
            source,
        })
    }
    /// Explicit detector handoff, only when all old replies have been drained. Call IDs persist.
    /// The next exchange rechecks these proofs and status before transmitting a mutation.
    pub fn redetect(&mut self, selected: SelectedAgent) -> NativeResult<()> {
        if self.sender.is_none() {
            return Err(NativeError::Unavailable);
        }
        if self.outstanding != 0 {
            return Err(NativeError::Busy);
        }
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(NativeError::IdExhausted)?;
        self.source = selected.io.target().source();
        self.selected = Arc::new(selected);
        Ok(())
    }
    pub fn shutdown(&mut self) {
        self.cancellation.cancel();
        self.sender.take();
    }
    #[cfg(test)]
    #[allow(dead_code)] // Used by the integration suite's privately compiled transport module.
    pub(crate) fn emulate_live(&mut self) {
        self.source = ObservationSource::Live;
    }
}
impl Drop for MacAgentPort {
    fn drop(&mut self) {
        self.shutdown();
    }
}
impl AgentPort for MacAgentPort {
    fn submit(&mut self, call: AgentCall) -> Result<(), CallFailure> {
        let sender = self.sender.as_ref().ok_or(CallFailure::Unavailable)?;
        if self.outstanding == MAX_QUEUE {
            return Err(CallFailure::QueueFull);
        }
        self.queue.submit(call)?;
        self.outstanding += 1;
        for call in self.queue.take_calls() {
            let deadline = Deadline::new(
                call.timeout_ms,
                self.selected.io.clock(),
                self.cancellation.clone(),
            );
            let submitted = match deadline {
                Ok(deadline) => sender
                    .try_send(Work {
                        call: call.clone(),
                        deadline,
                        selected: self.selected.clone(),
                        generation: self.generation,
                        source: self.source,
                    })
                    .is_ok(),
                Err(error) => {
                    self.queue
                        .push_reply(AgentReply {
                            id: call.id,
                            observed_at_ms: (self.clock)(),
                            source: ObservationSource::Demo,
                            result: Err(if error == NativeError::Invalid {
                                CallFailure::InvalidCall(ContractError::InvalidDeadline)
                            } else {
                                failure(error)
                            }),
                        })
                        .map_err(CallFailure::InvalidCall)?;
                    continue;
                }
            };
            if !submitted {
                self.queue
                    .push_reply(AgentReply {
                        id: call.id,
                        observed_at_ms: (self.clock)(),
                        source: ObservationSource::Demo,
                        result: Err(CallFailure::Unavailable),
                    })
                    .map_err(CallFailure::InvalidCall)?;
            }
        }
        Ok(())
    }
    fn poll(&mut self) -> Vec<AgentReply> {
        let mut failures = Vec::new();
        for reply in self.receiver.try_iter().take(MAX_QUEUE) {
            let fallback = reply.clone();
            if let Err(error) = self.queue.push_reply(reply) {
                failures.push(AgentReply {
                    result: Err(CallFailure::InvalidCall(error)),
                    source: ObservationSource::Demo,
                    ..fallback
                });
            }
        }
        if !failures.is_empty() {
            self.shutdown();
        }
        let mut replies = self.queue.poll();
        replies.extend(failures);
        self.outstanding = self.outstanding.saturating_sub(replies.len());
        replies
    }
}

fn health<'a>(
    selected: &SelectedAgent,
    reply: &'a DecodedReply,
    check_link: bool,
) -> Result<&'a HealthSnapshot, CallFailure> {
    let DecodedReply::Status(StatusAdmission::Supported(snapshot)) = reply else {
        return Err(CallFailure::Unavailable);
    };
    selected
        .instance
        .admit_status(&snapshot.installer().instance)
        .map_err(failure)?;
    if check_link
        && selected.link.is_some_and(|link| {
            !snapshot.installer().peers.iter().any(|p| {
                p.node == link.node && p.connected && p.link_generation == Some(link.generation)
            })
        })
    {
        return Err(CallFailure::Unavailable);
    }
    Ok(snapshot)
}
fn run(
    selected: &SelectedAgent,
    request: &InstallerRequest,
    deadline: &Deadline,
    clock: &CallerClock,
    observed: &mut u64,
) -> (Result<DecodedReply, CallFailure>, bool) {
    let mut attempt_admitted = false;
    let result = (|| {
        let peer = match request {
            InstallerRequest::Project { peer, .. }
            | InstallerRequest::Pull { peer, .. }
            | InstallerRequest::Allow { peer, .. }
            | InstallerRequest::WindowsFrom { peer } => Some(*peer),
            InstallerRequest::Return { source, .. } => *source,
            _ => None,
        };
        if peer.is_some_and(|peer| selected.link.is_none_or(|link| link.node != peer)) {
            return Err(CallFailure::Unavailable);
        }
        let before = frame(
            selected,
            &InstallerRequest::Status,
            deadline,
            clock,
            observed,
            &mut attempt_admitted,
        )?;
        let before = decode_reply(&InstallerRequest::Status, &before, AgentPlatform::Macos)?;
        if matches!(request, InstallerRequest::Status) {
            let admitted = match &before {
                DecodedReply::Status(StatusAdmission::PendingHealthContract(_)) => false,
                _ => {
                    health(selected, &before, false)?;
                    true
                }
            };
            return Ok((Ok(before), admitted));
        }
        health(selected, &before, true)?;
        *observed = clock();
        let bytes = frame(
            selected,
            request,
            deadline,
            clock,
            observed,
            &mut attempt_admitted,
        )?;
        let decoded = decode_reply(request, &bytes, AgentPlatform::Macos);
        let mut ignored = 0;
        let after = frame(
            selected,
            &InstallerRequest::Status,
            deadline,
            clock,
            &mut ignored,
            &mut attempt_admitted,
        )
        .and_then(|bytes| decode_reply(&InstallerRequest::Status, &bytes, AgentPlatform::Macos))
        .and_then(|reply| health(selected, &reply, true).map(|_| ()));
        // A complete, admitted refusal is evidence even if later detection fails.
        if matches!(decoded, Err(CallFailure::Refused(_))) {
            return Ok((decoded, true));
        }
        if after.is_err() {
            return Err(if readonly(request) {
                CallFailure::Unavailable
            } else {
                CallFailure::TimeoutOutcomeUnknown
            });
        }
        Ok((decoded, true))
    })();
    match result {
        Ok(value) => value,
        Err(error) => (Err(error), attempt_admitted),
    }
}
#[cfg(test)]
pub(crate) static BACKOFF: std::sync::Mutex<Vec<(std::path::PathBuf, u64)>> =
    std::sync::Mutex::new(Vec::new());
fn connect(selected: &SelectedAgent, deadline: &Deadline) -> Result<UnixStream, CallFailure> {
    let endpoint = selected.instance.endpoint();
    let address = SocketAddrUnix::new(endpoint.path()).map_err(|_| CallFailure::Unavailable)?;
    for attempt in 0..3 {
        // Fresh descriptors, original proofs, one shared deadline; no request exists here.
        selected
            .instance
            .revalidate(&selected.io, &selected.support, deadline)
            .map_err(failure)?;
        let fd = net::socket(AddressFamily::UNIX, SocketType::STREAM, None)
            .map_err(|_| CallFailure::Unavailable)?;
        rio::fcntl_setfd(&fd, rio::FdFlags::CLOEXEC).map_err(|_| CallFailure::Unavailable)?;
        net::sockopt::set_socket_nosigpipe(&fd, true).map_err(|_| CallFailure::Unavailable)?;
        let stream = UnixStream::from(fd);
        stream
            .set_nonblocking(true)
            .map_err(|_| CallFailure::Unavailable)?;
        let connected = loop {
            deadline.check().map_err(failure)?;
            endpoint.revalidate(&selected.io).map_err(failure)?;
            match net::connect(&stream, &address) {
                Ok(()) | Err(rustix::io::Errno::ISCONN) => break true,
                Err(
                    rustix::io::Errno::AGAIN
                    | rustix::io::Errno::INPROGRESS
                    | rustix::io::Errno::ALREADY,
                ) => pause(deadline)?,
                Err(_) => break false,
            }
        };
        if connected {
            #[cfg(test)]
            event(selected, "connected");
            endpoint.revalidate(&selected.io).map_err(failure)?;
            selected
                .instance
                .revalidate(&selected.io, &selected.support, deadline)
                .map_err(failure)?;
            return Ok(stream);
        }
        drop(stream);
        if attempt < 2 {
            let delay = 2 << attempt;
            #[cfg(test)]
            BACKOFF
                .lock()
                .map_err(|_| CallFailure::Unavailable)?
                .push((selected.io.target().paths().home.clone(), delay));
            deadline.check().map_err(failure)?;
            thread::sleep(Duration::from_millis(delay));
            deadline.check().map_err(failure)?;
        }
    }
    Err(CallFailure::Unavailable)
}
fn frame(
    selected: &SelectedAgent,
    request: &InstallerRequest,
    deadline: &Deadline,
    clock: &CallerClock,
    observed: &mut u64,
    attempt_admitted: &mut bool,
) -> Result<Vec<u8>, CallFailure> {
    let validate = || {
        selected
            .instance
            .revalidate(&selected.io, &selected.support, deadline)
            .map_err(failure)
    };
    let endpoint = selected.instance.endpoint();
    let mut stream = connect(selected, deadline)?;
    *attempt_admitted = true; // Selected endpoint/instance admitted, independent of response success.
    net::sockopt::set_socket_send_buffer_size(&stream, 4096)
        .map_err(|_| CallFailure::Unavailable)?;
    let bytes = encode_request(request).map_err(CallFailure::InvalidCall)?;
    let mut written = 0;
    while written < bytes.len() {
        deadline.check().map_err(failure)?;
        // Every mutation write retains the original endpoint, support and opaque instance.
        if !readonly(request) {
            validate().map_err(|e| {
                if written == 0 {
                    e
                } else {
                    CallFailure::TimeoutOutcomeUnknown
                }
            })?;
        }
        #[cfg(test)]
        event(selected, "write_validation");
        endpoint.revalidate(&selected.io).map_err(|e| {
            if written == 0 {
                failure(e)
            } else {
                CallFailure::TimeoutOutcomeUnknown
            }
        })?;
        deadline.check().map_err(failure)?;
        match stream.write(&bytes[written..]) {
            Ok(0) => return Err(CallFailure::TimeoutOutcomeUnknown),
            Ok(n) => {
                written += n;
                #[cfg(test)]
                event(selected, "written");
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                pause(deadline)?
            }
            Err(_) => return Err(CallFailure::TimeoutOutcomeUnknown),
        }
    }
    stream
        .shutdown(Shutdown::Write)
        .map_err(|_| CallFailure::TimeoutOutcomeUnknown)?;
    let mut response = Vec::new();
    let mut received = false;
    loop {
        deadline.check().map_err(failure)?;
        let mut buffer = [0; 8192];
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                if response.len() + n > MAX_RESPONSE_BYTES {
                    return Err(CallFailure::InvalidResponse);
                }
                response.extend_from_slice(&buffer[..n]);
                if let Some(end) = response.iter().position(|b| *b == b'\n') {
                    if !received {
                        *observed = clock();
                        received = true;
                        #[cfg(test)]
                        event(selected, "receipt");
                    }
                    if end + 1 != response.len() {
                        return Err(CallFailure::InvalidResponse);
                    }
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                pause(deadline)?
            }
            Err(_) => return Err(CallFailure::TimeoutOutcomeUnknown),
        }
    }
    if !received {
        return Err(CallFailure::InvalidResponse);
    }
    validate().map_err(|e| {
        if readonly(request) {
            e
        } else {
            CallFailure::TimeoutOutcomeUnknown
        }
    })?;
    deadline.check().map_err(failure)?;
    Ok(response)
}

/// Blocking lookups retain a global slot until they actually return. Answers are capped at 16.
pub trait HostLookup: Send + Sync {
    fn lookup(&self, host: &str, port: u16, deadline: &Deadline) -> NativeResult<Vec<SocketAddr>>;
}
struct SystemLookup;
impl HostLookup for SystemLookup {
    fn lookup(&self, host: &str, port: u16, deadline: &Deadline) -> NativeResult<Vec<SocketAddr>> {
        deadline.check()?;
        let answers = (host, port)
            .to_socket_addrs()
            .map_err(|_| NativeError::Unavailable)?
            .take(17)
            .collect();
        deadline.check()?;
        Ok(answers)
    }
}
fn address(input: &str) -> NativeResult<(String, u16)> {
    if input.len() > 260 || input.chars().any(char::is_control) {
        return Err(NativeError::Invalid);
    }
    let (host, port) = input.rsplit_once(':').ok_or(NativeError::Invalid)?;
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|p| *p != 0)
        .ok_or(NativeError::Invalid)?;
    if host.len() > 253
        || host.is_empty()
        || host.split('.').any(|part| {
            part.is_empty()
                || part.len() > 63
                || part.starts_with('-')
                || part.ends_with('-')
                || !part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
    {
        return Err(NativeError::Invalid);
    }
    Ok((host.to_ascii_lowercase(), port))
}
type ResolverPending = (Deadline, Cancellation, Receiver<NativeResult<SocketAddr>>);
pub struct BoundedResolver {
    lookup: Arc<dyn HostLookup>,
    clock: Arc<dyn Clock>,
    pending: Option<ResolverPending>,
}
impl std::fmt::Debug for BoundedResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BoundedResolver { .. }")
    }
}
impl BoundedResolver {
    pub fn system(clock: Arc<dyn Clock>) -> Self {
        Self::injected(Arc::new(SystemLookup), clock)
    }
    pub fn injected(lookup: Arc<dyn HostLookup>, clock: Arc<dyn Clock>) -> Self {
        Self {
            lookup,
            clock,
            pending: None,
        }
    }
    pub fn submit(&mut self, input: &str, timeout_ms: u64) -> NativeResult<()> {
        if self.pending.is_some() {
            return Err(NativeError::Busy);
        }
        if !(1..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
            return Err(NativeError::Invalid);
        }
        let cancel = Cancellation::default();
        let deadline = Deadline::new(timeout_ms, self.clock.clone(), cancel.clone())?;
        let (send, receive) = mpsc::sync_channel(1);
        if let Ok(literal) = input.parse::<SocketAddr>() {
            if literal.port() == 0 {
                return Err(NativeError::Invalid);
            }
            send.try_send(Ok(literal))
                .map_err(|_| NativeError::Unavailable)?;
        } else {
            let (host, port) = address(input)?;
            let slot = Slot::take(&RESOLVER_WORKERS)?;
            let lookup = self.lookup.clone();
            let limit = deadline.clone();
            thread::Builder::new()
                .name("installer-mac-resolver".into())
                .spawn(move || {
                    let _slot = slot;
                    let result = (|| {
                        limit.check()?;
                        let mut answers = lookup.lookup(&host, port, &limit)?;
                        limit.check()?;
                        if answers.is_empty() {
                            return Err(NativeError::Unavailable);
                        }
                        if answers.len() > 16 || answers.iter().any(|a| a.port() != port) {
                            return Err(NativeError::Oversize);
                        }
                        answers.sort_unstable();
                        answers.dedup();
                        answers.into_iter().next().ok_or(NativeError::Unavailable)
                    })();
                    let _ = send.try_send(result);
                })
                .map_err(|_| NativeError::Unavailable)?;
        }
        self.pending = Some((deadline, cancel, receive));
        Ok(())
    }
    pub fn poll(&mut self) -> Option<NativeResult<SocketAddr>> {
        let (deadline, cancel, receive) = self.pending.as_ref()?;
        let result = match deadline.check() {
            Err(error) => {
                cancel.cancel();
                Err(error)
            }
            Ok(()) => match receive.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => return None,
                Err(_) => Err(NativeError::Unavailable),
            },
        };
        self.pending.take();
        Some(result)
    }
    pub fn cancel(&mut self) {
        if let Some((_, cancel, _)) = &self.pending {
            cancel.cancel();
        }
    }
}
impl Drop for BoundedResolver {
    fn drop(&mut self) {
        self.cancel();
    }
}
