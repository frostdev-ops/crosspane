//! One private connection per request; no mutation is retried after an uncertain outcome.
use super::native_io::{
    Cancellation, Deadline, LinuxNativeIo, NativeError, ProcessIdentity, SupportProof,
};
use crate::agent_contract::{
    AgentCall, AgentPlatform, AgentPort, AgentQueue, AgentReply, CallFailure, DecodedReply,
    InstallerRequest, InstanceStatus, MAX_QUEUE, MAX_RESPONSE_BYTES, ObservationSource,
    StatusAdmission, decode_reply, encode_request,
};
use rustix::{
    fs::{self as rfs, AtFlags},
    net::{self, AddressFamily, SocketAddrUnix, SocketFlags, SocketType},
};
use std::{
    io::{Read, Write},
    net::{Shutdown, SocketAddr, ToSocketAddrs},
    os::{fd::AsRawFd, unix::net::UnixStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::Duration,
};

/// The supplied clock is the consumer's clock, not queue delivery time.
pub type CallerClock = Arc<dyn Fn() -> u64 + Send + Sync>;
static TRANSPORT_WORKERS: AtomicUsize = AtomicUsize::new(0);
static RESOLVER_WORKERS: AtomicUsize = AtomicUsize::new(0);
struct Slot(&'static AtomicUsize);
impl Slot {
    fn take(counter: &'static AtomicUsize) -> Result<Self, NativeError> {
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
    Ok(())
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

struct Work {
    call: AgentCall,
    deadline: Deadline,
}
pub struct LinuxAgentPort {
    queue: AgentQueue,
    sender: Option<SyncSender<Work>>,
    receiver: Receiver<AgentReply>,
    cancellation: Cancellation,
    support: Arc<Mutex<Option<SupportProof>>>,
    io: Arc<LinuxNativeIo>,
}
impl std::fmt::Debug for LinuxAgentPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LinuxAgentPort { .. }")
    }
}
impl LinuxAgentPort {
    pub fn new(
        io: Arc<LinuxNativeIo>,
        proof: Option<SupportProof>,
        clock: CallerClock,
    ) -> Result<Self, NativeError> {
        io.validate_target()?;
        if let Some(proof) = &proof {
            proof.check(&io)?;
        }
        let slot = Slot::take(&TRANSPORT_WORKERS)?;
        let (sender, calls) = mpsc::sync_channel::<Work>(MAX_QUEUE);
        let (results, receiver) = mpsc::sync_channel(MAX_QUEUE);
        let cancellation = Cancellation::default();
        let stop = cancellation.clone();
        let support = Arc::new(Mutex::new(proof));
        let worker_support = support.clone();
        let worker_io = io.clone();
        thread::Builder::new()
            .name("installer-agent".into())
            .spawn(move || {
                let _slot = slot;
                loop {
                    let work = match calls.recv_timeout(Duration::from_millis(10)) {
                        Ok(work) => work,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if stop.is_cancelled() {
                                break;
                            }
                            continue;
                        }
                        Err(_) => break,
                    };
                    let mut observed_at_ms = clock();
                    let mut source = ObservationSource::Demo;
                    let result = (|| {
                        work.deadline.check().map_err(failure)?;
                        if !readonly(&work.call.request) {
                            worker_support
                                .lock()
                                .map_err(|_| CallFailure::Unavailable)?
                                .as_ref()
                                .ok_or(CallFailure::Unavailable)?
                                .check(&worker_io)
                                .map_err(failure)?;
                            worker_io.validate_target().map_err(failure)?;
                        }
                        let expected = if matches!(work.call.request, InstallerRequest::Status) {
                            None
                        } else {
                            let (status, _, admission) = exchange(
                                &worker_io,
                                &InstallerRequest::Status,
                                None,
                                None,
                                &work.deadline,
                                &clock,
                                &mut observed_at_ms,
                            )?;
                            let status = status?;
                            let DecodedReply::Status(StatusAdmission::Supported(health)) = status
                            else {
                                return Err(CallFailure::Unavailable);
                            };
                            let _ = health;
                            Some(admission.ok_or(CallFailure::Unavailable)?)
                        };
                        let proof = if !readonly(&work.call.request) {
                            let proof = worker_support
                                .lock()
                                .map_err(|_| CallFailure::Unavailable)?
                                .as_ref()
                                .ok_or(CallFailure::Unavailable)?
                                .clone();
                            proof.check(&worker_io).map_err(failure)?;
                            Some(proof)
                        } else {
                            None
                        };
                        let (reply, time, admitted) = exchange(
                            &worker_io,
                            &work.call.request,
                            expected.as_ref(),
                            proof.as_ref(),
                            &work.deadline,
                            &clock,
                            &mut observed_at_ms,
                        )?;
                        observed_at_ms = time;
                        if admitted.is_some() {
                            source = worker_io.target().source();
                        }
                        reply
                    })();
                    // Every accepted but unpolled call remains pending in AgentQueue, bounding all
                    // queued/working/completed requests together to 32. This channel cannot overflow.
                    if results
                        .try_send(AgentReply {
                            id: work.call.id,
                            observed_at_ms,
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
        Ok(Self {
            queue: AgentQueue::default(),
            sender: Some(sender),
            receiver,
            cancellation,
            support,
            io,
        })
    }
    /// The detector supplies a newly admitted proof; this operation performs no I/O.
    pub fn refresh_support(&mut self, proof: SupportProof) -> Result<(), NativeError> {
        proof.check(&self.io)?;
        *self.support.try_lock().map_err(|_| NativeError::Busy)? = Some(proof);
        Ok(())
    }
    /// Cancels owned work without joining a worker on the caller's thread.
    pub fn shutdown(&mut self) {
        self.cancellation.cancel();
        self.sender.take();
    }
}
impl Drop for LinuxAgentPort {
    fn drop(&mut self) {
        self.shutdown();
    }
}
impl AgentPort for LinuxAgentPort {
    fn submit(&mut self, call: AgentCall) -> Result<(), CallFailure> {
        let sender = self.sender.as_ref().ok_or(CallFailure::Unavailable)?;
        self.queue.submit(call)?;
        for call in self.queue.take_calls() {
            let deadline =
                Deadline::new(call.timeout_ms, self.cancellation.clone()).map_err(failure)?;
            if let Err(error) = sender.try_send(Work { call, deadline }) {
                let work = match error {
                    mpsc::TrySendError::Full(work) | mpsc::TrySendError::Disconnected(work) => work,
                };
                self.queue
                    .push_reply(AgentReply {
                        id: work.call.id,
                        observed_at_ms: 0,
                        source: ObservationSource::Demo,
                        result: Err(CallFailure::Unavailable),
                    })
                    .map_err(CallFailure::InvalidCall)?;
            }
        }
        Ok(())
    }
    fn poll(&mut self) -> Vec<AgentReply> {
        for reply in self.receiver.try_iter().take(MAX_QUEUE) {
            let _ = self.queue.push_reply(reply);
        }
        self.queue.poll()
    }
}

struct Admission {
    instance: InstanceStatus,
    identity: ProcessIdentity,
    socket: (u64, u64),
}
type Exchange = (Result<DecodedReply, CallFailure>, u64, Option<Admission>);
fn admit_peer(
    io: &LinuxNativeIo,
    uid: u32,
    pid: u32,
    identity: &ProcessIdentity,
) -> Result<(), CallFailure> {
    #[cfg(test)]
    let uid = io
        .peer_uid
        .lock()
        .map_err(|_| CallFailure::Unavailable)?
        .unwrap_or(uid);
    if uid != io.target().paths().uid || uid != identity.uid || pid != identity.pid {
        Err(CallFailure::Unavailable)
    } else {
        Ok(())
    }
}
fn exchange(
    io: &LinuxNativeIo,
    request: &InstallerRequest,
    expected: Option<&Admission>,
    proof: Option<&SupportProof>,
    deadline: &Deadline,
    clock: &CallerClock,
    observed_at_ms: &mut u64,
) -> Result<Exchange, CallFailure> {
    deadline.check().map_err(failure)?;
    let (bootstrap, identity) = io.bootstrap(deadline).map_err(failure)?;
    if let Some(admitted) = expected {
        io.admit_instance(&admitted.instance, &bootstrap, &identity)
            .map_err(failure)?;
        if identity != admitted.identity {
            return Err(CallFailure::Unavailable);
        }
    }
    let (dir, name, fingerprint) = io.socket_parent().map_err(failure)?;
    if expected.is_some_and(|a| a.socket != fingerprint) {
        return Err(CallFailure::Unavailable);
    }
    let address = SocketAddrUnix::new(format!("/proc/self/fd/{}/{}", dir.as_raw_fd(), name))
        .map_err(|_| CallFailure::Unavailable)?;
    let fd = net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
        None,
    )
    .map_err(|_| CallFailure::Unavailable)?;
    loop {
        deadline.check().map_err(failure)?;
        match net::connect(&fd, &address) {
            Ok(()) | Err(rustix::io::Errno::ISCONN) => break,
            Err(
                rustix::io::Errno::AGAIN
                | rustix::io::Errno::INPROGRESS
                | rustix::io::Errno::ALREADY,
            ) => pause(deadline)?,
            Err(_) => return Err(CallFailure::Unavailable),
        }
    }
    net::sockopt::set_socket_send_buffer_size(&fd, 4096).map_err(|_| CallFailure::Unavailable)?;
    let peer = net::sockopt::socket_peercred(&fd).map_err(|_| CallFailure::Unavailable)?;
    let socket = rfs::statat(&dir, &name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| CallFailure::Unavailable)?;
    admit_peer(
        io,
        peer.uid.as_raw(),
        peer.pid.as_raw_nonzero().get() as u32,
        &identity,
    )?;
    if (socket.st_dev, socket.st_ino) != fingerprint {
        return Err(CallFailure::Unavailable);
    }
    let (connected, process) = io.bootstrap(deadline).map_err(failure)?;
    if identity != process
        || bootstrap.instance_id != connected.instance_id
        || io.socket_parent().map_err(failure)?.2 != fingerprint
    {
        return Err(CallFailure::Unavailable);
    }
    if let Some(admitted) = expected {
        io.admit_instance(&admitted.instance, &connected, &process)
            .map_err(failure)?;
    }
    let mut stream = UnixStream::from(fd);
    let bytes = encode_request(request).map_err(CallFailure::InvalidCall)?;
    let mut written = 0;
    while written < bytes.len() {
        deadline.check().map_err(failure)?;
        if let Some(proof) = proof
            && proof.check(io).is_err()
        {
            return Err(if written == 0 {
                CallFailure::Unavailable
            } else {
                CallFailure::TimeoutOutcomeUnknown
            });
        }
        match stream.write(&bytes[written..]) {
            Ok(0) => return Err(CallFailure::TimeoutOutcomeUnknown),
            Ok(n) => written += n,
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
    let mut receipt = None;
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
                if receipt.is_none() && response.contains(&b'\n') {
                    let time = clock();
                    receipt = Some(time);
                    *observed_at_ms = time;
                }
                if let Some(end) = response.iter().position(|b| *b == b'\n')
                    && end + 1 != response.len()
                {
                    return Err(CallFailure::InvalidResponse);
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
    let time = receipt.ok_or(CallFailure::InvalidResponse)?;
    let (current, after) = io.bootstrap(deadline).map_err(failure)?;
    if identity != after
        || bootstrap.instance_id != current.instance_id
        || io.socket_parent().map_err(failure)?.2 != fingerprint
    {
        return Err(CallFailure::Unavailable);
    }
    let decoded = decode_reply(request, &response, AgentPlatform::Linux);
    let admitted = if let Ok(DecodedReply::Status(StatusAdmission::Supported(health))) = &decoded {
        io.admit_instance(&health.installer().instance, &current, &after)
            .map_err(failure)?;
        Some(Admission {
            instance: health.installer().instance.clone(),
            identity: after,
            socket: fingerprint,
        })
    } else if let Some(expected) = expected {
        io.admit_instance(&expected.instance, &current, &after)
            .map_err(failure)?;
        Some(Admission {
            instance: expected.instance.clone(),
            identity: after,
            socket: fingerprint,
        })
    } else {
        None
    };
    Ok((decoded, time, admitted))
}

/// Host lookup implementations must bound their answers to 16 and honor cancellation when
/// possible. A blocking libc lookup retains its global slot until it actually finishes.
pub trait HostLookup: Send + Sync {
    fn lookup(
        &self,
        host: &str,
        port: u16,
        deadline: &Deadline,
    ) -> Result<Vec<SocketAddr>, NativeError>;
}
struct SystemLookup;
impl HostLookup for SystemLookup {
    fn lookup(
        &self,
        host: &str,
        port: u16,
        deadline: &Deadline,
    ) -> Result<Vec<SocketAddr>, NativeError> {
        deadline.check()?;
        let answers: Vec<_> = (host, port)
            .to_socket_addrs()
            .map_err(|_| NativeError::Unavailable)?
            .take(17)
            .collect();
        deadline.check()?;
        Ok(answers)
    }
}
fn address(input: &str) -> Result<(String, u16), NativeError> {
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
type ResolverPending = (
    Deadline,
    Cancellation,
    Receiver<Result<SocketAddr, NativeError>>,
);
pub struct BoundedResolver {
    lookup: Arc<dyn HostLookup>,
    pending: Option<ResolverPending>,
}
impl std::fmt::Debug for BoundedResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BoundedResolver { .. }")
    }
}
impl BoundedResolver {
    pub fn system() -> Self {
        Self::injected(Arc::new(SystemLookup))
    }
    /// Injected answers have no transport or production-target authority.
    pub fn injected(lookup: Arc<dyn HostLookup>) -> Self {
        Self {
            lookup,
            pending: None,
        }
    }
    pub fn submit(&mut self, input: &str, timeout_ms: u64) -> Result<(), NativeError> {
        if self.pending.is_some() {
            return Err(NativeError::Busy);
        }
        if !(1..=5000).contains(&timeout_ms) {
            return Err(NativeError::Invalid);
        }
        let cancel = Cancellation::default();
        let deadline = Deadline::new(timeout_ms, cancel.clone())?;
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
            let worker_deadline = deadline.clone();
            thread::Builder::new()
                .name("installer-resolver".into())
                .spawn(move || {
                    let _slot = slot;
                    let result = (|| {
                        let mut answers = lookup.lookup(&host, port, &worker_deadline)?;
                        worker_deadline.check()?;
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
    pub fn poll(&mut self) -> Option<Result<SocketAddr, NativeError>> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::linux::native_io::{
        CommandOutput, CommandRunner, CommandSpec, ProcessFacts, ProcessProbe, SupportObservations,
        parse_ps_start,
    };
    use std::{
        fs,
        os::unix::{fs::PermissionsExt, net::UnixListener},
        path::PathBuf,
    };
    struct Fake(PathBuf);
    impl CommandRunner for Fake {
        fn run(&self, command: &CommandSpec, _: &Deadline) -> Result<CommandOutput, NativeError> {
            Ok(CommandOutput {
                code: Some(0),
                stderr: Vec::new(),
                stdout: if command.argv()[1] == "lstart=" {
                    b"Fri Oct  2 12:00:00 2026\n".to_vec()
                } else {
                    b"crosspane-agent\n".to_vec()
                },
            })
        }
    }
    impl ProcessProbe for Fake {
        fn snapshot(&self, _: u32, _: &Deadline) -> Result<ProcessFacts, NativeError> {
            Ok(ProcessFacts {
                uid: rustix::process::geteuid().as_raw(),
                executable: self.0.join(".local/bin/crosspane-agent"),
                generation: 77,
            })
        }
    }
    #[test]
    fn injected_foreign_socket_owner_and_connected_peer_reject_before_request_bytes() {
        for kind in 0..3 {
            let root = PathBuf::from(format!("/tmp/cp47-peer-{}-{kind}", std::process::id()));
            let fake = Arc::new(Fake(root.clone()));
            let io = Arc::new(LinuxNativeIo::scratch(&root, fake.clone(), fake).unwrap());
            let uid = io.target().paths().uid;
            let proof = io
                .scratch_support(SupportObservations {
                    uid,
                    architecture: std::env::consts::ARCH.into(),
                    arch_based: true,
                    hyprland_version: [0, 56, 0],
                    protocols_ready: true,
                    runtime_libraries_ready: true,
                    uwsm_managed: true,
                    graphical_target_active: true,
                    graphical_sessions: 1,
                    session_id: "scratch".into(),
                    session_type: "wayland".into(),
                    seat: "seat0".into(),
                    active: true,
                })
                .unwrap();
            io.create_private_dir(&proof, io.target().runtime_dir())
                .unwrap();
            io.create_private_dir(&proof, &root.join(".local/bin"))
                .unwrap();
            fs::write(io.target().agent_path(), b"inert").unwrap();
            fs::set_permissions(io.target().agent_path(), fs::Permissions::from_mode(0o755))
                .unwrap();
            let pid = std::process::id() + u32::from(kind == 2);
            let bytes=serde_json::to_vec(&serde_json::json!({"schema_version":1,"instance_id":9,"pid":pid,
                "started_unix_ms":parse_ps_start(b"Fri Oct  2 12:00:00 2026\n").unwrap(),"phase":"ready","phase_seq":2,
                "keystore":"os_store","reason":null,"runtime_dir":io.target().runtime_dir()})).unwrap();
            io.atomic_write(
                &proof,
                &io.target().runtime_dir().join("bootstrap.json"),
                &bytes,
            )
            .unwrap();
            if kind == 0 {
                *io.socket_uid.lock().unwrap() = Some(uid + 1);
            }
            if kind == 1 {
                *io.peer_uid.lock().unwrap() = Some(uid + 1);
            }
            let listener = UnixListener::bind(io.target().socket_path()).unwrap();
            listener.set_nonblocking(true).unwrap();
            fs::set_permissions(io.target().socket_path(), fs::Permissions::from_mode(0o600))
                .unwrap();
            let mut port = LinuxAgentPort::new(io.clone(), None, Arc::new(|| 123)).unwrap();
            port.submit(AgentCall {
                id: 1,
                request: InstallerRequest::Status,
                timeout_ms: 1000,
            })
            .unwrap();
            let until = std::time::Instant::now() + Duration::from_secs(2);
            let reply = loop {
                if let Some(reply) = port.poll().pop() {
                    break reply;
                }
                assert!(std::time::Instant::now() < until);
                thread::sleep(Duration::from_millis(1));
            };
            assert_eq!(reply.result, Err(CallFailure::Unavailable));
            if kind == 0 {
                assert!(
                    matches!(listener.accept(),Err(e) if e.kind()==std::io::ErrorKind::WouldBlock)
                );
            } else {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut received = Vec::new();
                stream.read_to_end(&mut received).unwrap();
                assert!(received.is_empty());
            }
            port.shutdown();
            drop(port);
            drop(listener);
            fs::remove_dir_all(root).unwrap();
        }
    }
}
