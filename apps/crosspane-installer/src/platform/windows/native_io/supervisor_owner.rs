//! Private original-supervisor rendezvous. Metadata correlates genuine retained kernel owners;
#![cfg(any(windows, test))]
//! it never constructs a process/job capability or approves an old image for launch.
use super::super::service::supervisor::Generation;
use super::{NativeError, NativeResult};
use serde::{Deserialize, Serialize};

pub(crate) const MAX_OWNER_FRAME: usize = 2048;
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Method {
    Hello,
    Arm,
    Empty,
    Ack,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    schema_version: u32,
    nonce: [u8; 16],
    method: Method,
    operation: Option<[u8; 16]>,
    generation: Generation,
}
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ReplyStatus {
    Ready,
    ReadyPending,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    status: ReplyStatus,
    schema_version: u32,
    nonce: [u8; 16],
    method: Method,
    operation: Option<[u8; 16]>,
    generation: Generation,
    process: Option<u64>,
    child: Option<u64>,
    job: Option<u64>,
}
fn valid_generation(g: Generation) -> bool {
    g.pid != 0 && g.creation != 0 && g.instance != 0
}
fn check_request(r: &Request, selected: Generation) -> NativeResult<()> {
    if r.schema_version != 1
        || r.nonce == [0; 16]
        || !valid_generation(r.generation)
        || r.generation != selected
        || r.operation == Some([0; 16])
        || (r.method == Method::Hello) != r.operation.is_none()
    {
        return Err(NativeError::Foreign);
    }
    Ok(())
}
fn check_reply(r: &Reply, request: &Request) -> NativeResult<()> {
    if r.schema_version != 1
        || r.nonce != request.nonce
        || r.method != request.method
        || r.operation != request.operation
        || r.generation != request.generation
    {
        return Err(NativeError::Foreign);
    }
    if r.status == ReplyStatus::ReadyPending {
        return if r.method == Method::Hello
            && r.process.is_none()
            && r.child.is_none()
            && r.job.is_none()
        {
            Ok(())
        } else {
            Err(NativeError::Foreign)
        };
    }
    let shape = match r.method {
        Method::Hello => r.process.is_some() && r.child.is_some() && r.job.is_none(),
        Method::Empty => r.process.is_none() && r.child.is_none(),
        Method::Arm | Method::Ack => r.process.is_none() && r.child.is_none() && r.job.is_none(),
    };
    if !shape {
        return Err(NativeError::Foreign);
    }
    for handle in [r.process, r.child, r.job].into_iter().flatten() {
        if handle < 4 || handle > usize::MAX as u64 || handle >= (usize::MAX - 3) as u64 {
            return Err(NativeError::Foreign);
        }
    }
    Ok(())
}

/// Correlation AFTER both genuine retained peers have been freshly revalidated. Equality never
/// selects a process or creates authority, and a changed peer cannot extend a pending retry.
fn same_fixed_role(
    old_id: super::files::FileIdentity,
    old_path: &str,
    new_id: super::files::FileIdentity,
    new_path: &str,
) -> bool {
    old_id == new_id && old_path == new_path
}
fn same_connected_peer<T: PartialEq>(old: (u32, u64, &str, &T), new: (u32, u64, &str, &T)) -> bool {
    old == new
}
fn decode_message<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> NativeResult<T> {
    if bytes.is_empty() || bytes.len() > MAX_OWNER_FRAME {
        return Err(NativeError::Oversize);
    }
    serde_json::from_slice(bytes).map_err(|_| NativeError::Invalid)
}

/// Pure order gate USED by the production stop path; no native owner is minted here.
#[derive(Default)]
pub(crate) struct StopSequence {
    attempted: bool,
    armed: bool,
    submitted: bool,
}
pub(crate) trait TerminalStopPort {
    fn persist(&mut self) -> NativeResult<()>;
    fn arm(&mut self) -> NativeResult<()>;
    fn submit(&mut self) -> NativeResult<()>;
}
impl StopSequence {
    pub(crate) fn run_once<P: TerminalStopPort>(&mut self, port: &mut P) -> NativeResult<()> {
        if self.attempted {
            return Err(NativeError::OutcomeUnknown);
        }
        // Claim before any effect: persistence/arm/RPC ambiguity never permits replay.
        self.attempted = true;
        port.persist()?;
        port.arm()?;
        self.armed = true;
        self.submitted = true;
        port.submit()
    }
    pub(crate) fn submitted(&self) -> bool {
        self.armed && self.submitted
    }
}

/// Portable completion predicate used before the native sealed proof factory. These are
/// observations only; callers cannot turn this comparison into RetainedTreeCompletion.
pub(crate) fn completion_matches(
    selected: Generation,
    actual: Generation,
    terminal: bool,
    original_supervisor_exited: bool,
    original_agent_exited: bool,
    job_active: u32,
    clean: bool,
) -> NativeResult<()> {
    if !valid_generation(selected)
        || selected != actual
        || !terminal
        || !original_supervisor_exited
        || !original_agent_exited
        || job_active != 0
        || !clean
    {
        return Err(NativeError::Foreign);
    }
    Ok(())
}

#[cfg(windows)]
mod native {
    use super::super::{
        AgentObservation, BrokerAdmission, Cancellation, Deadline, ExitObservation, SupportProof,
        WindowsNativeIo, identity, jobs,
    };
    use super::*;
    use crate::agent_contract::BootstrapPhase;
    use std::{
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
        sync::{
            Arc, Mutex, OnceLock,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };
    use tokio::{
        io::{AsyncRead, AsyncWrite, ReadBuf},
        net::windows::named_pipe::{
            ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
        },
    };
    use windows_sys::Win32::{
        Foundation::*,
        Security::Authorization::*,
        Security::*,
        System::{JobObjects::*, Pipes::*, SystemServices::JOB_OBJECT_QUERY, Threading::*},
    };

    struct Peer {
        process: Arc<OwnedHandle>,
        pid: u32,
        created: u64,
        image: String,
        token: identity::TokenFacts,
        role: Mutex<Option<Arc<super::super::PeerRolePin>>>,
    }
    impl Peer {
        fn admit(
            raw_pipe: std::os::windows::io::RawHandle,
            server: bool,
            root: &BrokerAdmission,
            deadline: &Deadline,
        ) -> NativeResult<Self> {
            root.reverify(&root.io().admit_support(deadline)?, deadline)?;
            let mut pid = 0;
            // SAFETY: exact connected local pipe stays owned by the caller throughout this query.
            let ok = unsafe {
                if server {
                    GetNamedPipeServerProcessId(raw_pipe, &mut pid)
                } else {
                    GetNamedPipeClientProcessId(raw_pipe, &mut pid)
                }
            };
            // SAFETY: query of our own process id; no foreign enumeration.
            if ok == 0 || pid == 0 || pid == unsafe { GetCurrentProcessId() } {
                return Err(NativeError::Foreign);
            }
            // SAFETY: kernel pipe peer is the ONLY live selection source. No record/PID/name lookup.
            let raw = unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_DUP_HANDLE,
                    0,
                    pid,
                )
            };
            if raw.is_null() {
                return Err(NativeError::Unavailable);
            }
            // SAFETY: successful OpenProcess returned one real non-inheritable owned process handle.
            let process = Arc::new(unsafe { OwnedHandle::from_raw_handle(raw) });
            let (created, image) = process_facts(&process, true)?;
            let token = identity::native::observe_process(&process)?;
            identity::LimitedIdentity::admit(token.clone()).map_err(|_| NativeError::Foreign)?;
            if &token != root.token() {
                return Err(NativeError::Foreign);
            }
            let proof = root.io().admit_support(deadline)?;
            let role = root.peer_role(&image, &proof, deadline)?;
            if server && role.identity() != root.installer_identity() {
                return Err(NativeError::Foreign);
            }
            // Bind peer object, fixed old-role file identity, and pipe peer AGAIN before publication.
            let peer = Self {
                process,
                pid,
                created,
                image,
                token,
                role: Mutex::new(Some(Arc::new(role))),
            };
            peer.reverify(root, deadline)?;
            let mut repeated = 0;
            // SAFETY: same still-owned connected pipe, complete writable PID output.
            let ok = unsafe {
                if server {
                    GetNamedPipeServerProcessId(raw_pipe, &mut repeated)
                } else {
                    GetNamedPipeClientProcessId(raw_pipe, &mut repeated)
                }
            };
            if ok == 0 || repeated != pid {
                return Err(NativeError::Foreign);
            }
            Ok(peer)
        }
        fn reverify(&self, root: &BrokerAdmission, deadline: &Deadline) -> NativeResult<()> {
            deadline.check()?;
            let (created, image) = process_facts(&self.process, true)?;
            let token = identity::native::observe_process(&self.process)?;
            if created != self.created
                || image != self.image
                || token != self.token
                || &token != root.token()
            {
                return Err(NativeError::Foreign);
            }
            // SAFETY: retained exact object, no reopened PID after exit.
            if unsafe { GetProcessId(self.process.as_raw_handle()) } != self.pid {
                return Err(NativeError::Foreign);
            }
            let role = self
                .role
                .lock()
                .map_err(|_| NativeError::Unavailable)?
                .as_ref()
                .cloned()
                .ok_or(NativeError::Unsupported)?;
            role.reverify(deadline)?;
            let proof = root.io().admit_support(deadline)?;
            // Re-select the genuine fixed role NOW. In particular, an old helper-copy pin
            // cannot remain admitted after StageCatalog.active changes to another operation.
            let fresh = root.peer_role(&self.image, &proof, deadline)?;
            if !same_fixed_role(role.identity(), role.path(), fresh.identity(), fresh.path()) {
                return Err(NativeError::Foreign);
            }
            root.reverify(&proof, deadline)
        }
        fn release_image_pin(&self) -> NativeResult<()> {
            self.role
                .lock()
                .map_err(|_| NativeError::Unavailable)?
                .take();
            Ok(())
        }
        fn exited(&self) -> NativeResult<bool> {
            let (created, _) = process_facts(&self.process, false)?;
            if created != self.created {
                return Err(NativeError::Foreign);
            }
            // SAFETY: retained original peer process object, nonblocking observation only.
            match unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) } {
                WAIT_OBJECT_0 => Ok(true),
                WAIT_TIMEOUT => Ok(false),
                _ => Err(NativeError::Unavailable),
            }
        }
        fn duplicate(
            &self,
            value: u64,
            rights: u32,
            deadline: &Deadline,
        ) -> NativeResult<OwnedHandle> {
            deadline.check()?;
            if value < 4 || value > usize::MAX as u64 || value >= (usize::MAX - 3) as u64 {
                return Err(NativeError::Foreign);
            }
            let mut raw = std::ptr::null_mut();
            // SAFETY: retained authenticated ORIGINAL source process; source value came only from
            // its sealed own-handle export on the same admitted pipe. Create our local noninherit
            // duplicate with restricted rights; never a recorded target or blanket SAME_ACCESS.
            if unsafe {
                DuplicateHandle(
                    self.process.as_raw_handle(),
                    value as usize as _,
                    GetCurrentProcess(),
                    &mut raw,
                    rights,
                    0,
                    0,
                )
            } == 0
            {
                return Err(NativeError::Unavailable);
            }
            // SAFETY: successful local duplication transferred one real owned handle.
            Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
        }
    }
    fn process_facts(process: &OwnedHandle, require_live: bool) -> NativeResult<(u64, String)> {
        // SAFETY: retained process handle; zero-timeout query cannot cause or select an exit.
        let wait = unsafe { WaitForSingleObject(process.as_raw_handle(), 0) };
        if wait != WAIT_OBJECT_0 && wait != WAIT_TIMEOUT || require_live && wait != WAIT_TIMEOUT {
            return Err(NativeError::Foreign);
        }
        let (mut created, mut exit, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        // SAFETY: complete distinct writable FILETIME outputs, retained query-only process object.
        if unsafe {
            GetProcessTimes(
                process.as_raw_handle(),
                &mut created,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        } == 0
        {
            return Err(NativeError::Unavailable);
        }
        let created = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
        if created == 0 {
            return Err(NativeError::Foreign);
        }
        if !require_live {
            return Ok((created, String::new()));
        }
        let mut path = vec![0u16; 32768];
        let mut length = path.len() as u32;
        // SAFETY: retained live process query handle and complete bounded writable UTF-16 buffer.
        if unsafe {
            QueryFullProcessImageNameW(process.as_raw_handle(), 0, path.as_mut_ptr(), &mut length)
        } == 0
            || length as usize >= path.len()
        {
            return Err(NativeError::Unavailable);
        }
        let image =
            String::from_utf16(&path[..length as usize]).map_err(|_| NativeError::Foreign)?;
        Ok((created, super::super::process::literal_path(&image)?))
    }
    fn active(job: &OwnedHandle) -> NativeResult<u32> {
        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: exact retained query-only original job, complete SDK accounting output.
        if unsafe {
            QueryInformationJobObject(
                job.as_raw_handle(),
                JobObjectBasicAccountingInformation,
                (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                std::mem::size_of_val(&accounting) as u32,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(NativeError::Unavailable);
        }
        Ok(accounting.ActiveProcesses)
    }
    fn runtime() -> NativeResult<tokio::runtime::Runtime> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| NativeError::Unavailable)
    }
    // Tokio core polling traits are already enabled; io-util convenience extensions are not.
    // The entire frame remains under the caller's one existing timeout. Partial EOF/zero writes
    // fail explicitly and are never interpreted as a retryable ReadyPending response.
    async fn write_bytes<W: AsyncWrite + Unpin>(pipe: &mut W, bytes: &[u8]) -> std::io::Result<()> {
        let mut offset = 0;
        while offset < bytes.len() {
            let written = std::future::poll_fn(|cx| {
                std::pin::Pin::new(&mut *pipe).poll_write(cx, &bytes[offset..])
            })
            .await?;
            if written == 0 {
                return Err(std::io::ErrorKind::WriteZero.into());
            }
            offset += written;
        }
        Ok(())
    }
    async fn read_bytes<R: AsyncRead + Unpin>(
        pipe: &mut R,
        bytes: &mut [u8],
    ) -> std::io::Result<()> {
        let mut offset = 0;
        while offset < bytes.len() {
            let mut buffer = ReadBuf::new(&mut bytes[offset..]);
            std::future::poll_fn(|cx| std::pin::Pin::new(&mut *pipe).poll_read(cx, &mut buffer))
                .await?;
            let read = buffer.filled().len();
            if read == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            offset += read;
        }
        Ok(())
    }
    async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(
        pipe: &mut W,
        value: &T,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        let bytes = serde_json::to_vec(value).map_err(|_| NativeError::Invalid)?;
        if bytes.is_empty() || bytes.len() > MAX_OWNER_FRAME {
            return Err(NativeError::Oversize);
        }
        tokio::time::timeout(Duration::from_millis(deadline.remaining_ms()?), async {
            write_bytes(pipe, &(bytes.len() as u32).to_le_bytes()).await?;
            write_bytes(pipe, &bytes).await
        })
        .await
        .map_err(|_| NativeError::OutcomeUnknown)?
        .map_err(|_| NativeError::OutcomeUnknown)
    }
    async fn read_frame<R: AsyncRead + Unpin, T: for<'de> Deserialize<'de>>(
        pipe: &mut R,
        deadline: &Deadline,
    ) -> NativeResult<T> {
        let bytes = tokio::time::timeout(Duration::from_millis(deadline.remaining_ms()?), async {
            let mut header = [0; 4];
            read_bytes(pipe, &mut header)
                .await
                .map_err(|_| NativeError::Unavailable)?;
            let length = u32::from_le_bytes(header) as usize;
            if length == 0 || length > MAX_OWNER_FRAME {
                return Err(NativeError::Oversize);
            }
            let mut bytes = vec![0; length];
            read_bytes(pipe, &mut bytes)
                .await
                .map_err(|_| NativeError::Unavailable)?;
            Ok::<_, NativeError>(bytes)
        })
        .await
        .map_err(|_| NativeError::Timeout)??;
        decode_message(&bytes)
    }

    struct Descriptor(PSECURITY_DESCRIPTOR);
    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: successful SDDL conversion owns exactly this LocalAlloc allocation.
            unsafe {
                LocalFree(self.0);
            }
        }
    }
    fn create_pipe(root: &BrokerAdmission) -> NativeResult<NamedPipeServer> {
        let user = root.token().user.sddl();
        let logon = root.token().logon.sddl();
        let text = format!("O:{user}G:{user}D:P(A;;GA;;;{user})(A;;GRGW;;;{logon})");
        let text = super::super::files::native::wide(&text)?;
        let mut raw = std::ptr::null_mut();
        // SAFETY: complete NUL terminated SDDL, writable descriptor output, current owner only.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                SDDL_REVISION_1,
                &mut raw,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(NativeError::Unavailable);
        }
        let descriptor = Descriptor(raw);
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        // SAFETY: descriptor/attributes remain live until CreateNamedPipe returns; non-inheritable,
        // protected owner/logon DACL, sole first instance, local clients only, Tokio owns the pipe.
        unsafe {
            ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .max_instances(1)
                .create_with_security_attributes_raw(
                    root.endpoint(),
                    (&attributes as *const SECURITY_ATTRIBUTES)
                        .cast_mut()
                        .cast(),
                )
        }
        .map_err(|_| NativeError::Unavailable)
    }

    /// The SAME real kernel first-instance namespace, retained independently of listener lifetime.
    /// There is no serialized/test constructor; its source is successful own CreateNamedPipe.
    pub(crate) struct ExclusiveSupervisorLease {
        root: Arc<BrokerAdmission>,
        namespace: Arc<OwnedHandle>,
        own: super::super::process::own::OwnProcessIdentity,
        live: Arc<AtomicBool>,
    }
    impl ExclusiveSupervisorLease {
        pub(crate) fn reverify(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if !std::ptr::eq(io, self.root.io().as_ref()) || !self.live.load(Ordering::Acquire) {
                return Err(NativeError::Foreign);
            }
            self.root.reverify(proof, deadline)?;
            self.own.reverify(deadline)?;
            let mut flags = 0;
            // SAFETY: retained original server-pipe duplicate, complete writable type flags;
            // this does not depend on a client existing before the first child is created.
            if unsafe {
                GetNamedPipeInfo(
                    self.namespace.as_raw_handle(),
                    &mut flags,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            } == 0
                || flags & PIPE_SERVER_END == 0
            {
                return Err(NativeError::Unavailable);
            }
            deadline.check()
        }
    }
    struct ServerState {
        root: Arc<BrokerAdmission>,
        owner: Arc<jobs::SupervisorOwner>,
        agent: Mutex<Option<Arc<AgentObservation>>>,
        exclusive: Mutex<Option<Arc<ExclusiveSupervisorLease>>>,
        live: Arc<AtomicBool>,
        stopped: AtomicBool,
        finish: AtomicBool,
        finished: AtomicBool,
        export: Mutex<Option<([u8; 16], Generation)>>,
    }
    pub(crate) struct OwnerServer {
        state: Arc<ServerState>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl OwnerServer {
        pub(crate) fn reserve(
            io: Arc<WindowsNativeIo>,
            owner: Arc<jobs::SupervisorOwner>,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<Self> {
            let root = io.broker_admission(proof, deadline)?;
            let state = Arc::new(ServerState {
                root,
                owner,
                agent: Mutex::new(None),
                exclusive: Mutex::new(None),
                live: Arc::new(AtomicBool::new(false)),
                stopped: AtomicBool::new(false),
                finish: AtomicBool::new(false),
                finished: AtomicBool::new(false),
                export: Mutex::new(None),
            });
            let held = state.clone();
            let initial = deadline.clone();
            let (send, recv) = std::sync::mpsc::sync_channel(1);
            let thread = std::thread::Builder::new().name("crosspane-owner-rendezvous".into()).spawn(move || {
                let result = (|| {
                    let rt = runtime()?;
                    rt.block_on(async {
                        let mut pipe = create_pipe(&held.root)?;
                        let proof = held.root.io().admit_support(&initial)?;
                        let own = held.root.io().own_process_identity(&proof, &initial)?;
                        let mut raw = std::ptr::null_mut();
                        // SAFETY: duplicate only our actual FIRST_INSTANCE server object locally,
                        // noninherit. The retained duplicate keeps exclusivity through quarantine.
                        if unsafe { DuplicateHandle(GetCurrentProcess(), pipe.as_raw_handle(), GetCurrentProcess(), &mut raw, 0, 0, DUPLICATE_SAME_ACCESS) } == 0 { return Err(NativeError::Unavailable); }
                        // SAFETY: successful duplication transferred one real local pipe handle.
                        let namespace = Arc::new(unsafe { OwnedHandle::from_raw_handle(raw) });
                        held.live.store(true, Ordering::Release);
                        let lease = Arc::new(ExclusiveSupervisorLease { root:held.root.clone(), namespace, own, live:held.live.clone() });
                        *held.exclusive.lock().map_err(|_| NativeError::Unavailable)? = Some(lease.clone());
                        let proof = held.root.io().admit_support(&initial)?;
                        held.owner.bind_exclusive(lease, &proof, &initial)?;
                        if initial.check().is_err() || held.stopped.load(Ordering::Acquire) { return Err(NativeError::Timeout); }
                        send.send(Ok(())).map_err(|_| NativeError::Unavailable)?;
                        loop {
                            if held.stopped.load(Ordering::Acquire) { return Ok(()); }
                            tokio::select! {
                                connected = pipe.connect() => { connected.map_err(|_| NativeError::Unavailable)?; },
                                _ = tokio::time::sleep(Duration::from_millis(20)) => {
                                    if held.finish.load(Ordering::Acquire) { return Ok(()); }
                                    continue;
                                }
                            }
                            let deadline = Deadline::new(30_000, held.root.io().bound_clock(), Cancellation::default())?;
                            let result = serve(&held, &mut pipe, &deadline).await;
                            pipe.disconnect().map_err(|_| NativeError::Unavailable)?;
                            if result.is_err() && held.export.lock().map_err(|_| NativeError::Unavailable)?.is_some() {
                                // An armed/exported original owner is never silently reused after loss.
                                held.stopped.store(true, Ordering::Release);
                                return result;
                            }
                            if held.finish.load(Ordering::Acquire) { return Ok(()); }
                        }
                    })
                })();
                if result.is_err() { let _ = send.try_send(Err(NativeError::Unavailable)); }
                held.live.store(false, Ordering::Release);
                held.finished.store(true, Ordering::Release);
            }).map_err(|_| NativeError::Unavailable)?;
            match recv.recv_timeout(Duration::from_millis(deadline.remaining_ms()?)) {
                Ok(Ok(())) => Ok(Self {
                    state,
                    thread: Some(thread),
                }),
                _ => {
                    state.stopped.store(true, Ordering::Release);
                    Err(NativeError::OutcomeUnknown)
                }
            }
        }
        pub(crate) fn update_generation(
            &self,
            agent: AgentObservation,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.state.root.reverify(proof, deadline)?;
            if self.state.owner.terminal_operation().is_some() {
                return Err(NativeError::Foreign);
            }
            self.state.root.bind_runtime(&agent, proof, deadline)?;
            self.state.owner.child_for_peer(&agent, proof, deadline)?;
            *self
                .state
                .agent
                .lock()
                .map_err(|_| NativeError::Unavailable)? = Some(Arc::new(agent));
            Ok(())
        }
        pub(crate) fn finish(&self, proof: &SupportProof, deadline: &Deadline) -> NativeResult<()> {
            self.state.root.reverify(proof, deadline)?;
            self.state.finish.store(true, Ordering::Release);
            while !self.state.finished.load(Ordering::Acquire) {
                deadline.check()?;
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(())
        }
    }
    impl Drop for OwnerServer {
        fn drop(&mut self) {
            self.state.stopped.store(true, Ordering::Release);
            // Drop never joins a listener: its thread retains the actual owners independently,
            // and the global job owner retains the namespace duplicate even after listener exit.
            drop(self.thread.take());
            // A bounded worker that has not settled keeps its actual root/owner until it exits; no
            // other process's PID is killed and no late raw export is forgotten or retried.
        }
    }
    async fn serve(
        state: &ServerState,
        pipe: &mut NamedPipeServer,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        let peer = Peer::admit(pipe.as_raw_handle(), false, &state.root, deadline)?;
        let original = state
            .agent
            .lock()
            .map_err(|_| NativeError::Unavailable)?
            .as_ref()
            .cloned();
        let hello: Request = read_frame(pipe, deadline).await?;
        let Some(original) = original else {
            if hello.schema_version != 1
                || hello.nonce == [0; 16]
                || hello.method != Method::Hello
                || hello.operation.is_some()
                || !valid_generation(hello.generation)
            {
                return Err(NativeError::Foreign);
            }
            let pending = Reply {
                status: ReplyStatus::ReadyPending,
                schema_version: 1,
                nonce: hello.nonce,
                method: Method::Hello,
                operation: None,
                generation: hello.generation,
                process: None,
                child: None,
                job: None,
            };
            write_frame(pipe, &pending, deadline).await?;
            return Ok(());
        };
        let io = state.root.io();
        let proof = io.admit_support(deadline)?;
        let selected = io.agent_generation(&original, &proof, deadline)?;
        check_request(&hello, selected)?;
        if hello.method != Method::Hello {
            return Err(NativeError::Foreign);
        }
        let process = state.owner.process_for_peer(&proof, deadline)?;
        let child = state.owner.child_for_peer(&original, &proof, deadline)?;
        let mut reply = Reply {
            status: ReplyStatus::Ready,
            schema_version: 1,
            nonce: hello.nonce,
            method: Method::Hello,
            operation: None,
            generation: selected,
            process: Some(process.as_raw_handle() as usize as u64),
            child: Some(child.as_raw_handle() as usize as u64),
            job: None,
        };
        // These exact source handles stay retained until this connection/ack finishes. The outer
        // pulls restricted duplicates into its pre-reserved owner; no orphan target handles exist.
        write_frame(pipe, &reply, deadline).await?;
        loop {
            let request: Request = read_frame(pipe, deadline).await?;
            check_request(&request, selected)?;
            if request.nonce != hello.nonce {
                return Err(NativeError::Foreign);
            }
            peer.reverify(&state.root, deadline)?;
            let proof = io.admit_support(deadline)?;
            let operation = request.operation.ok_or(NativeError::Foreign)?;
            reply.method = request.method;
            reply.operation = Some(operation);
            reply.process = None;
            reply.child = None;
            reply.job = None;
            match request.method {
                Method::Arm => {
                    validate_stop_intent(io, &proof, operation, selected, deadline)?;
                    state
                        .owner
                        .arm_terminal(selected, operation, &proof, deadline)?;
                    *state.export.lock().map_err(|_| NativeError::Unavailable)? =
                        Some((operation, selected));
                }
                Method::Empty => {
                    if state.owner.terminal_operation() != Some(operation) {
                        return Err(NativeError::Foreign);
                    }
                    if let Some(empty) =
                        state.owner.settled_for_stop(&proof, &original, deadline)?
                    {
                        let job = state.owner.job_for_empty(&empty, &proof, deadline)?;
                        if active(&job)? != 0 {
                            return Err(NativeError::Foreign);
                        }
                        reply.job = Some(job.as_raw_handle() as usize as u64);
                        write_frame(pipe, &reply, deadline).await?;
                        let ack: Request = read_frame(pipe, deadline).await?;
                        check_request(&ack, selected)?;
                        if ack.method != Method::Ack
                            || ack.operation != Some(operation)
                            || ack.nonce != hello.nonce
                        {
                            return Err(NativeError::Foreign);
                        }
                        peer.reverify(&state.root, deadline)?;
                        reply.method = Method::Ack;
                        reply.job = None;
                        write_frame(pipe, &reply, deadline).await?;
                        return Ok(());
                    }
                }
                _ => return Err(NativeError::Foreign),
            }
            write_frame(pipe, &reply, deadline).await?;
        }
    }
    #[derive(Default)]
    struct Handles {
        supervisor: Option<Arc<OwnedHandle>>,
        child: Option<Arc<OwnedHandle>>,
        job: Option<Arc<OwnedHandle>>,
    }
    struct Transfer {
        root: Arc<BrokerAdmission>,
        original: Mutex<Option<Arc<AgentObservation>>>,
        selected: Generation,
        nonce: [u8; 16],
        peer: Mutex<Option<Arc<Peer>>>,
        handles: Mutex<Handles>,
        lease: Mutex<Option<Arc<super::super::StopLockLease>>>,
        retired: AtomicBool,
        thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    }
    // One outstanding/uncertain original handoff per process. Dropping a caller never releases
    // the sole original handles/stop-lock lease or permits a second owner to evade the bound.
    static TRANSFER: OnceLock<Mutex<Option<Arc<Transfer>>>> = OnceLock::new();
    enum Command {
        Arm(
            [u8; 16],
            Deadline,
            std::sync::mpsc::SyncSender<NativeResult<()>>,
        ),
        Complete(
            [u8; 16],
            Deadline,
            std::sync::mpsc::SyncSender<NativeResult<RetainedTreeCompletion>>,
        ),
        ReadyClose(Deadline, std::sync::mpsc::SyncSender<NativeResult<()>>),
    }
    pub(crate) struct AdmittedSupervisorOwner {
        state: Arc<Transfer>,
        send: std::sync::mpsc::SyncSender<Command>,
    }
    /// ONLY actual original exited objects + original empty job can construct this type.
    pub(crate) struct RetainedTreeCompletion {
        operation: [u8; 16],
        selected: Generation,
        supervisor: Arc<OwnedHandle>,
        child: Arc<OwnedHandle>,
        job: Arc<OwnedHandle>,
    }
    impl RetainedTreeCompletion {
        pub(crate) fn operation(&self) -> [u8; 16] {
            self.operation
        }
        pub(crate) fn original_instance(&self) -> u64 {
            self.selected.instance
        }
        pub(crate) fn reverify(&self, deadline: &Deadline) -> NativeResult<()> {
            deadline.check()?;
            // SAFETY: these exact original restricted process objects stay retained.
            let supervisor =
                unsafe { WaitForSingleObject(self.supervisor.as_raw_handle(), 0) } == WAIT_OBJECT_0;
            // SAFETY: original CreateProcess/admitted clean successor object, no PID reopen.
            let child =
                unsafe { WaitForSingleObject(self.child.as_raw_handle(), 0) } == WAIT_OBJECT_0;
            completion_matches(
                self.selected,
                self.selected,
                true,
                supervisor,
                child,
                active(&self.job)?,
                true,
            )
        }
    }
    impl AdmittedSupervisorOwner {
        pub(crate) fn admit(
            io: Arc<WindowsNativeIo>,
            original: AgentObservation,
            lease: Option<Arc<super::super::StopLockLease>>,
            deadline: &Deadline,
        ) -> NativeResult<Self> {
            let proof = io.admit_support(deadline)?;
            let root = io.broker_admission(&proof, deadline)?;
            // Refuse the external downloaded caller BEFORE connecting or sampling a server.
            let module = io.self_image(&proof, deadline)?;
            let role = root.peer_role(module.canonical_dos_path(), &proof, deadline)?;
            if role.identity() != module.identity() {
                return Err(NativeError::Foreign);
            }
            drop(role);
            drop(module);
            root.bind_runtime(&original, &proof, deadline)?;
            let selected = io.agent_generation(&original, &proof, deadline)?;
            if original.bootstrap().phase != BootstrapPhase::Ready {
                return Err(NativeError::Unavailable);
            }
            let nonce = io.bridge_nonce(&proof, deadline)?;
            let state = Arc::new(Transfer {
                root,
                original: Mutex::new(Some(Arc::new(original))),
                selected,
                nonce,
                peer: Mutex::new(None),
                handles: Mutex::new(Handles::default()),
                lease: Mutex::new(lease),
                retired: AtomicBool::new(false),
                thread: Mutex::new(None),
            });
            {
                let mut slot = TRANSFER
                    .get_or_init(|| Mutex::new(None))
                    .lock()
                    .map_err(|_| NativeError::Unavailable)?;
                if slot.is_some() {
                    return Err(NativeError::Busy);
                }
                *slot = Some(state.clone());
            }
            let (send, commands) = std::sync::mpsc::sync_channel(1);
            let (ready, receive) = std::sync::mpsc::sync_channel(1);
            let held = state.clone();
            let initial = deadline.clone();
            let thread = std::thread::Builder::new()
                .name("crosspane-original-owner".into())
                .spawn(move || {
                    let result = (|| {
                        let rt = runtime()?;
                        let pipe = rt.block_on(connect(&held, &initial))?;
                        ready.send(Ok(())).map_err(|_| NativeError::Unavailable)?;
                        command_loop(&held, &rt, pipe, commands)
                    })();
                    if result.is_err() {
                        held.retired.store(true, Ordering::Release);
                        let _ = ready.try_send(Err(NativeError::OutcomeUnknown));
                    }
                })
                .map_err(|_| NativeError::OutcomeUnknown)?;
            *state.thread.lock().map_err(|_| NativeError::Unavailable)? = Some(thread);
            match receive.recv_timeout(Duration::from_millis(deadline.remaining_ms()?)) {
                Ok(Ok(())) => Ok(Self { state, send }),
                _ => {
                    state.retired.store(true, Ordering::Release);
                    Err(NativeError::OutcomeUnknown)
                }
            }
        }
        pub(crate) fn arm_terminal(
            &self,
            operation: [u8; 16],
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if self.state.retired.load(Ordering::Acquire) {
                return Err(NativeError::OutcomeUnknown);
            }
            let (send, recv) = std::sync::mpsc::sync_channel(1);
            self.send
                .try_send(Command::Arm(operation, deadline.clone(), send))
                .map_err(|_| NativeError::Busy)?;
            self.receive(recv, deadline)
        }
        pub(crate) fn observe_completion(
            &self,
            operation: [u8; 16],
            deadline: &Deadline,
        ) -> NativeResult<RetainedTreeCompletion> {
            let (send, recv) = std::sync::mpsc::sync_channel(1);
            self.send
                .try_send(Command::Complete(operation, deadline.clone(), send))
                .map_err(|_| NativeError::OutcomeUnknown)?;
            let completed = self.receive(recv, deadline)?;
            completed.reverify(deadline)?;
            self.settle(deadline)?;
            Ok(completed)
        }
        pub(crate) fn close_ready(&self, deadline: &Deadline) -> NativeResult<()> {
            let (send, recv) = std::sync::mpsc::sync_channel(1);
            self.send
                .try_send(Command::ReadyClose(deadline.clone(), send))
                .map_err(|_| NativeError::OutcomeUnknown)?;
            self.receive(recv, deadline)?;
            self.settle(deadline)
        }
        fn receive<T>(
            &self,
            recv: std::sync::mpsc::Receiver<NativeResult<T>>,
            deadline: &Deadline,
        ) -> NativeResult<T> {
            match recv.recv_timeout(Duration::from_millis(deadline.remaining_ms()?)) {
                Ok(result) if deadline.check().is_ok() => result,
                _ => {
                    self.state.retired.store(true, Ordering::Release);
                    Err(NativeError::OutcomeUnknown)
                }
            }
        }
        fn settle(&self, deadline: &Deadline) -> NativeResult<()> {
            loop {
                let finished = self
                    .state
                    .thread
                    .lock()
                    .map_err(|_| NativeError::Unavailable)?
                    .as_ref()
                    .is_some_and(|thread| {
                        // SAFETY: exact owned worker thread, zero-time native exit observation;
                        // Rust is_finished alone may precede its native TLS cleanup.
                        (unsafe { WaitForSingleObject(thread.as_raw_handle(), 0) }) == WAIT_OBJECT_0
                    });
                if finished {
                    break;
                }
                deadline.check()?;
                std::thread::sleep(Duration::from_millis(5));
            }
            if self.state.retired.load(Ordering::Acquire) {
                return Err(NativeError::OutcomeUnknown);
            }
            // After exact worker exit: every connected pipe and local pin alias is settled.
            self.state.root.release_image_pin()?;
            if let Some(peer) = self
                .state
                .peer
                .lock()
                .map_err(|_| NativeError::Unavailable)?
                .as_ref()
            {
                peer.release_image_pin()?;
            }
            self.state
                .original
                .lock()
                .map_err(|_| NativeError::Unavailable)?
                .take();
            self.state
                .lease
                .lock()
                .map_err(|_| NativeError::Unavailable)?
                .take();
            let mut slot = TRANSFER
                .get_or_init(|| Mutex::new(None))
                .lock()
                .map_err(|_| NativeError::Unavailable)?;
            if slot
                .as_ref()
                .is_some_and(|held| Arc::ptr_eq(held, &self.state))
            {
                *slot = None;
            }
            Ok(())
        }
    }
    async fn connect(state: &Transfer, deadline: &Deadline) -> NativeResult<NamedPipeClient> {
        let (pipe, peer, reply) = loop {
            let mut pipe = loop {
                deadline.check()?;
                match ClientOptions::new().open(state.root.endpoint()) {
                    Ok(pipe) => break pipe,
                    Err(error) if matches!(error.raw_os_error(), Some(2 | 231)) => {
                        tokio::time::sleep(Duration::from_millis(5)).await
                    }
                    Err(_) => return Err(NativeError::Unavailable),
                }
            };
            let peer = Arc::new(Peer::admit(
                pipe.as_raw_handle(),
                true,
                &state.root,
                deadline,
            )?);
            let previous = state
                .peer
                .lock()
                .map_err(|_| NativeError::Unavailable)?
                .as_ref()
                .cloned();
            if let Some(previous) = previous {
                previous.reverify(&state.root, deadline)?;
                if !same_connected_peer(
                    (
                        previous.pid,
                        previous.created,
                        &previous.image,
                        &previous.token,
                    ),
                    (peer.pid, peer.created, &peer.image, &peer.token),
                ) {
                    return Err(NativeError::Foreign);
                }
            } else {
                *state.peer.lock().map_err(|_| NativeError::Unavailable)? = Some(peer.clone());
            }
            let request = Request {
                schema_version: 1,
                nonce: state.nonce,
                method: Method::Hello,
                operation: None,
                generation: state.selected,
            };
            write_frame(&mut pipe, &request, deadline).await?;
            let reply: Reply = read_frame(&mut pipe, deadline).await?;
            check_reply(&reply, &request)?;
            if reply.status == ReplyStatus::Ready {
                break (pipe, peer, reply);
            }
            // ONLY explicit authenticated ReadyPending, and ZERO copies, permits this retry.
            let proof = state.root.io().admit_support(deadline)?;
            let original = state
                .original
                .lock()
                .map_err(|_| NativeError::Unavailable)?
                .as_ref()
                .cloned()
                .ok_or(NativeError::Foreign)?;
            original.revalidate(state.root.io(), &proof, deadline)?;
            peer.reverify(&state.root, deadline)?;
            drop(pipe);
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let supervisor = Arc::new(peer.duplicate(
            reply.process.ok_or(NativeError::Foreign)?,
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            deadline,
        )?);
        state
            .handles
            .lock()
            .map_err(|_| NativeError::Unavailable)?
            .supervisor = Some(supervisor.clone());
        let child = Arc::new(peer.duplicate(
            reply.child.ok_or(NativeError::Foreign)?,
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            deadline,
        )?);
        state
            .handles
            .lock()
            .map_err(|_| NativeError::Unavailable)?
            .child = Some(child.clone());
        let (created, _) = process_facts(&supervisor, true)?;
        // SAFETY: locally retained exact authenticated source process object, query only.
        let supervisor_pid = unsafe { GetProcessId(supervisor.as_raw_handle()) };
        // SAFETY: locally retained exact original CreateProcess/admitted successor object.
        let child_pid = unsafe { GetProcessId(child.as_raw_handle()) };
        if created != peer.created || supervisor_pid != peer.pid || child_pid != state.selected.pid
        {
            return Err(NativeError::Foreign);
        }
        let (created, image) = process_facts(&child, true)?;
        let original = state
            .original
            .lock()
            .map_err(|_| NativeError::Unavailable)?
            .as_ref()
            .cloned()
            .ok_or(NativeError::Foreign)?;
        if created != state.selected.creation
            || image != super::super::process::literal_path(original.image_canonical())?
            || identity::native::observe_process(&child)? != *state.root.token()
        {
            return Err(NativeError::Foreign);
        }
        let proof = state.root.io().admit_support(deadline)?;
        original.revalidate(state.root.io(), &proof, deadline)?;
        peer.reverify(&state.root, deadline)?;
        Ok(pipe)
    }
    fn command_loop(
        state: &Transfer,
        rt: &tokio::runtime::Runtime,
        pipe: NamedPipeClient,
        commands: std::sync::mpsc::Receiver<Command>,
    ) -> NativeResult<()> {
        let mut pipe = Some(pipe);
        loop {
            let command = commands.recv().map_err(|_| NativeError::Unavailable)?;
            match command {
                Command::Arm(operation, deadline, send) => {
                    let result = (|| {
                        if state.retired.load(Ordering::Acquire) {
                            return Err(NativeError::OutcomeUnknown);
                        }
                        let peer = state
                            .peer
                            .lock()
                            .map_err(|_| NativeError::Unavailable)?
                            .as_ref()
                            .cloned()
                            .ok_or(NativeError::Foreign)?;
                        peer.reverify(&state.root, &deadline)?;
                        let request = Request {
                            schema_version: 1,
                            nonce: state.nonce,
                            method: Method::Arm,
                            operation: Some(operation),
                            generation: state.selected,
                        };
                        rt.block_on(exchange(
                            pipe.as_mut().ok_or(NativeError::Foreign)?,
                            &request,
                            &deadline,
                        ))?;
                        Ok(())
                    })();
                    if result.is_err() {
                        state.retired.store(true, Ordering::Release);
                    }
                    let _ = send.send(result);
                }
                Command::Complete(operation, deadline, send) => {
                    let result = (|| {
                        let peer = state
                            .peer
                            .lock()
                            .map_err(|_| NativeError::Unavailable)?
                            .as_ref()
                            .cloned()
                            .ok_or(NativeError::Foreign)?;
                        let request = Request {
                            schema_version: 1,
                            nonce: state.nonce,
                            method: Method::Empty,
                            operation: Some(operation),
                            generation: state.selected,
                        };
                        let job = loop {
                            deadline.check()?;
                            let reply = rt.block_on(exchange(
                                pipe.as_mut().ok_or(NativeError::Foreign)?,
                                &request,
                                &deadline,
                            ))?;
                            if let Some(value) = reply.job {
                                let job =
                                    Arc::new(peer.duplicate(value, JOB_OBJECT_QUERY, &deadline)?);
                                // Store BEFORE validation or delivery; malformed/late copies quarantine.
                                state
                                    .handles
                                    .lock()
                                    .map_err(|_| NativeError::Unavailable)?
                                    .job = Some(job.clone());
                                if active(&job)? != 0 {
                                    return Err(NativeError::Foreign);
                                }
                                let ack = Request {
                                    method: Method::Ack,
                                    ..request
                                };
                                rt.block_on(exchange(
                                    pipe.as_mut().ok_or(NativeError::Foreign)?,
                                    &ack,
                                    &deadline,
                                ))?;
                                break job;
                            }
                            std::thread::sleep(Duration::from_millis(5));
                        };
                        // Settle EVERY old connected pipe alias before a future first-instance reserve.
                        drop(pipe.take());
                        loop {
                            deadline.check()?;
                            if peer.exited()? {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        let original = state
                            .original
                            .lock()
                            .map_err(|_| NativeError::Unavailable)?
                            .as_ref()
                            .cloned()
                            .ok_or(NativeError::Foreign)?;
                        let proof = state.root.io().admit_support(&deadline)?;
                        match original.observe_exit(state.root.io(), &proof, &deadline)? {
                            ExitObservation::Exited {
                                creation,
                                code: 0,
                                receipt: Some(receipt),
                            } if creation == state.selected.creation
                                && receipt.instance_id == state.selected.instance
                                && receipt.clean
                                && receipt.input_journals_empty
                                && receipt.audio_stopped => {}
                            _ => return Err(NativeError::Foreign),
                        }
                        let (supervisor, child) = {
                            let handles =
                                state.handles.lock().map_err(|_| NativeError::Unavailable)?;
                            (
                                handles
                                    .supervisor
                                    .as_ref()
                                    .cloned()
                                    .ok_or(NativeError::Foreign)?,
                                handles
                                    .child
                                    .as_ref()
                                    .cloned()
                                    .ok_or(NativeError::Foreign)?,
                            )
                        };
                        let (created, _) = process_facts(&child, false)?;
                        if created != state.selected.creation {
                            return Err(NativeError::Foreign);
                        }
                        let completed = RetainedTreeCompletion {
                            operation,
                            selected: state.selected,
                            supervisor,
                            child,
                            job,
                        };
                        completed.reverify(&deadline)?;
                        Ok(completed)
                    })();
                    if result.is_err() {
                        state.retired.store(true, Ordering::Release);
                    }
                    let _ = send.send(result);
                    return if state.retired.load(Ordering::Acquire) {
                        Err(NativeError::OutcomeUnknown)
                    } else {
                        Ok(())
                    };
                }
                Command::ReadyClose(deadline, send) => {
                    let result = (|| {
                        deadline.check()?;
                        let proof = state.root.io().admit_support(&deadline)?;
                        let original = state
                            .original
                            .lock()
                            .map_err(|_| NativeError::Unavailable)?
                            .as_ref()
                            .cloned()
                            .ok_or(NativeError::Foreign)?;
                        original.revalidate(state.root.io(), &proof, &deadline)?;
                        let peer = state
                            .peer
                            .lock()
                            .map_err(|_| NativeError::Unavailable)?
                            .as_ref()
                            .cloned()
                            .ok_or(NativeError::Foreign)?;
                        peer.reverify(&state.root, &deadline)?;
                        drop(pipe.take());
                        Ok(())
                    })();
                    if result.is_err() {
                        state.retired.store(true, Ordering::Release);
                    }
                    let _ = send.send(result);
                    return if state.retired.load(Ordering::Acquire) {
                        Err(NativeError::OutcomeUnknown)
                    } else {
                        Ok(())
                    };
                }
            }
        }
    }
    async fn exchange(
        pipe: &mut NamedPipeClient,
        request: &Request,
        deadline: &Deadline,
    ) -> NativeResult<Reply> {
        write_frame(pipe, request, deadline).await?;
        let reply: Reply = read_frame(pipe, deadline).await?;
        check_reply(&reply, request)?;
        Ok(reply)
    }
    fn validate_stop_intent(
        io: &WindowsNativeIo,
        proof: &SupportProof,
        operation: [u8; 16],
        selected: Generation,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        use super::super::super::payload::recovery::{OperationRecord, Phase, StageCatalog};
        use super::super::records::{RecordName, record_data};
        let catalog = io
            .read_record(
                proof,
                RecordName::StageCatalog,
                super::super::files::MAX_RECORD_BYTES,
                deadline,
            )?
            .ok_or(NativeError::Missing)?;
        let catalog: StageCatalog = record_data(&RecordName::StageCatalog, catalog.bytes())?;
        catalog.validate()?;
        if catalog.active != Some(operation) {
            return Err(NativeError::Foreign);
        }
        let observed = io
            .read_record(
                proof,
                RecordName::Operation(operation),
                super::super::files::MAX_RECORD_BYTES,
                deadline,
            )?
            .ok_or(NativeError::Missing)?;
        let record: OperationRecord =
            record_data(&RecordName::Operation(operation), observed.bytes())?;
        record.validate()?;
        if record.operation() != operation || record.phase() != Phase::StopIntent {
            return Err(NativeError::Foreign);
        }
        let journal = super::super::super::service::journal::Journal::read(io, proof, deadline)?
            .ok_or(NativeError::Missing)?;
        if journal.phase != super::super::super::service::journal::Phase::StopIntent
            || journal.current != Some(selected)
            || journal.stop_instance != Some(selected.instance)
        {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
    #[cfg(test)]
    mod polling_tests {
        use super::*;
        use std::{
            future::Future,
            pin::Pin,
            task::{Context, Poll},
        };
        fn finish<F: Future>(future: F) -> F::Output {
            let mut future = std::pin::pin!(future);
            let mut context = Context::from_waker(std::task::Waker::noop());
            match future.as_mut().poll(&mut context) {
                Poll::Ready(value) => value,
                Poll::Pending => panic!("own memory-only fixture must be immediately ready"),
            }
        }
        struct Bytes {
            input: Vec<u8>,
            offset: usize,
            output: Vec<u8>,
            zero_write: bool,
        }
        impl AsyncRead for Bytes {
            fn poll_read(
                mut self: Pin<&mut Self>,
                _: &mut Context<'_>,
                out: &mut ReadBuf<'_>,
            ) -> Poll<std::io::Result<()>> {
                let length = (self.input.len() - self.offset).min(out.remaining()).min(2);
                out.put_slice(&self.input[self.offset..self.offset + length]);
                self.offset += length;
                Poll::Ready(Ok(()))
            }
        }
        impl AsyncWrite for Bytes {
            fn poll_write(
                mut self: Pin<&mut Self>,
                _: &mut Context<'_>,
                bytes: &[u8],
            ) -> Poll<std::io::Result<usize>> {
                if self.zero_write {
                    return Poll::Ready(Ok(0));
                }
                let length = bytes.len().min(2);
                self.output.extend_from_slice(&bytes[..length]);
                Poll::Ready(Ok(length))
            }
            fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }
            fn poll_shutdown(
                self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }
        }
        #[test]
        fn owner_core_polling_reassembles_partial_memory_reads_and_writes() {
            let mut bytes = Bytes {
                input: vec![1, 2, 3, 4, 5],
                offset: 0,
                output: vec![],
                zero_write: false,
            };
            let mut out = [0; 5];
            finish(read_bytes(&mut bytes, &mut out)).unwrap();
            assert_eq!(out, [1, 2, 3, 4, 5]);
            finish(write_bytes(&mut bytes, &out)).unwrap();
            assert_eq!(bytes.output, out);
        }
        #[test]
        fn owner_core_polling_truncated_or_zero_write_is_an_error() {
            let mut bytes = Bytes {
                input: vec![1, 2],
                offset: 0,
                output: vec![],
                zero_write: true,
            };
            let mut out = [0; 4];
            assert_eq!(
                finish(read_bytes(&mut bytes, &mut out)).unwrap_err().kind(),
                std::io::ErrorKind::UnexpectedEof
            );
            assert_eq!(
                finish(write_bytes(&mut bytes, &[1])).unwrap_err().kind(),
                std::io::ErrorKind::WriteZero
            );
        }
    }
}
#[cfg(windows)]
pub(crate) use native::{
    AdmittedSupervisorOwner, ExclusiveSupervisorLease, OwnerServer, RetainedTreeCompletion,
};

#[cfg(test)]
mod tests {
    use super::*;
    fn generation() -> Generation {
        Generation {
            pid: 31,
            creation: 41,
            instance: u64::MAX - 1,
        }
    }
    fn hello() -> Request {
        Request {
            schema_version: 1,
            nonce: [7; 16],
            method: Method::Hello,
            operation: None,
            generation: generation(),
        }
    }
    fn reply(status: ReplyStatus) -> Reply {
        Reply {
            status,
            schema_version: 1,
            nonce: [7; 16],
            method: Method::Hello,
            operation: None,
            generation: generation(),
            process: None,
            child: None,
            job: None,
        }
    }
    #[test]
    fn owner_ready_pending_is_only_positive_zero_handle_hello_refusal() {
        let h = hello();
        let mut r = reply(ReplyStatus::ReadyPending);
        assert_eq!(check_reply(&r, &h), Ok(()));
        for field in 0..3 {
            let mut r = reply(ReplyStatus::ReadyPending);
            match field {
                0 => r.process = Some(64),
                1 => r.child = Some(68),
                _ => r.job = Some(72),
            };
            assert_eq!(check_reply(&r, &h), Err(NativeError::Foreign));
        }
        r.method = Method::Arm;
        r.operation = Some([8; 16]);
        let request = Request {
            method: Method::Arm,
            operation: Some([8; 16]),
            ..hello()
        };
        assert_eq!(check_reply(&r, &request), Err(NativeError::Foreign));
    }
    #[test]
    fn owner_pending_retry_refuses_changed_creation_image_or_context() {
        let context = ("own-user", "own-logon", 3u32, true);
        assert!(same_connected_peer(
            (31, 41, "fixed-installer", &context),
            (31, 41, "fixed-installer", &context)
        ));
        assert!(!same_connected_peer(
            (31, 41, "fixed-installer", &context),
            (32, 41, "fixed-installer", &context)
        ));
        assert!(!same_connected_peer(
            (31, 41, "fixed-installer", &context),
            (31, 42, "fixed-installer", &context)
        ));
        assert!(!same_connected_peer(
            (31, 41, "fixed-installer", &context),
            (31, 41, "other-image", &context)
        ));
        for changed in [
            ("other-user", "own-logon", 3, true),
            ("own-user", "other-logon", 3, true),
            ("own-user", "own-logon", 4, true),
            ("own-user", "own-logon", 3, false),
        ] {
            assert!(!same_connected_peer(
                (31, 41, "fixed-installer", &context),
                (31, 41, "fixed-installer", &changed)
            ));
        }
    }
    #[test]
    fn owner_helper_role_requires_fresh_active_operation_path_and_same_fileid() {
        let id = super::super::files::FileIdentity {
            volume: 1,
            file: [7; 16],
        };
        let old = "fixed/payload-stage/old-operation/helper-copy.exe";
        assert!(same_fixed_role(id, old, id, old));
        assert!(!same_fixed_role(
            id,
            old,
            id,
            "fixed/payload-stage/new-operation/helper-copy.exe"
        ));
        assert!(!same_fixed_role(
            id,
            old,
            super::super::files::FileIdentity {
                volume: 1,
                file: [8; 16]
            },
            old
        ));
        // The native predicate gets its NEW tuple only from rooted peer_role re-selection,
        // never from peer assertions, retained path alone or serialized membership.
    }
    #[test]
    fn owner_unknown_truncated_foreign_and_partial_messages_never_become_pending() {
        let mut value = serde_json::to_value(reply(ReplyStatus::ReadyPending)).unwrap();
        value["status"] = serde_json::json!("unknown");
        assert!(decode_message::<Reply>(&serde_json::to_vec(&value).unwrap()).is_err());
        value["status"] = serde_json::json!("ready_pending");
        value["unexpected"] = serde_json::json!(1);
        assert!(decode_message::<Reply>(&serde_json::to_vec(&value).unwrap()).is_err());
        for bytes in [b"{".as_slice(), b"".as_slice(), b"null".as_slice()] {
            assert!(decode_message::<Reply>(bytes).is_err());
        }
        assert!(decode_message::<Reply>(&vec![b' '; MAX_OWNER_FRAME + 1]).is_err());
        let mut foreign = reply(ReplyStatus::ReadyPending);
        foreign.nonce = [8; 16];
        assert_eq!(check_reply(&foreign, &hello()), Err(NativeError::Foreign));
        let mut partial = reply(ReplyStatus::Ready);
        partial.process = Some(64);
        assert_eq!(check_reply(&partial, &hello()), Err(NativeError::Foreign));
    }
    #[test]
    fn owner_exact_u64_instance_and_generation_are_not_float_correlation() {
        let r: Request = decode_message(&serde_json::to_vec(&hello()).unwrap()).unwrap();
        assert_eq!(r.generation.instance, u64::MAX - 1);
        assert_eq!(check_request(&r, generation()), Ok(()));
        for n in 0..3 {
            let mut actual = generation();
            match n {
                0 => actual.pid += 1,
                1 => actual.creation += 1,
                _ => actual.instance -= 1,
            };
            assert_eq!(check_request(&r, actual), Err(NativeError::Foreign));
        }
    }
    #[test]
    fn owner_null_pseudo_or_missing_transfer_handles_are_refused() {
        for handle in [0, 1, usize::MAX as u64, (usize::MAX - 3) as u64] {
            let mut r = reply(ReplyStatus::Ready);
            r.process = Some(handle);
            r.child = Some(68);
            assert_eq!(check_reply(&r, &hello()), Err(NativeError::Foreign));
        }
        let mut r = reply(ReplyStatus::Ready);
        r.process = Some(64);
        r.child = Some(68);
        assert_eq!(check_reply(&r, &hello()), Ok(()));
        r.job = Some(72);
        assert_eq!(check_reply(&r, &hello()), Err(NativeError::Foreign));
    }
    struct StopFake {
        events: Vec<&'static str>,
        fail: Option<&'static str>,
    }
    impl StopFake {
        fn step(&mut self, name: &'static str) -> NativeResult<()> {
            self.events.push(name);
            if self.fail == Some(name) {
                Err(NativeError::OutcomeUnknown)
            } else {
                Ok(())
            }
        }
    }
    impl TerminalStopPort for StopFake {
        fn persist(&mut self) -> NativeResult<()> {
            self.step("durable-stop-intent")
        }
        fn arm(&mut self) -> NativeResult<()> {
            self.step("original-terminal-arm")
        }
        fn submit(&mut self) -> NativeResult<()> {
            self.step("one-a2-stop")
        }
    }
    #[test]
    fn owner_actual_stop_order_persists_then_arms_before_consuming_rpc() {
        let mut f = StopFake {
            events: vec![],
            fail: None,
        };
        let mut sequence = StopSequence::default();
        sequence.run_once(&mut f).unwrap();
        assert!(sequence.submitted());
        assert_eq!(
            f.events,
            [
                "durable-stop-intent",
                "original-terminal-arm",
                "one-a2-stop"
            ]
        );
        assert_eq!(sequence.run_once(&mut f), Err(NativeError::OutcomeUnknown));
        assert_eq!(f.events.len(), 3);
    }
    #[test]
    fn owner_every_stop_interruption_is_sticky_with_no_rpc_or_arm_replay() {
        for fail in [
            "durable-stop-intent",
            "original-terminal-arm",
            "one-a2-stop",
        ] {
            let mut f = StopFake {
                events: vec![],
                fail: Some(fail),
            };
            let mut sequence = StopSequence::default();
            assert!(sequence.run_once(&mut f).is_err());
            let events = f.events.clone();
            assert_eq!(sequence.run_once(&mut f), Err(NativeError::OutcomeUnknown));
            assert_eq!(f.events, events);
            assert_eq!(sequence.submitted(), fail == "one-a2-stop");
        }
    }
    #[test]
    fn owner_tree_completion_needs_both_original_exits_empty_job_terminal_and_clean() {
        let selected = generation();
        assert_eq!(
            completion_matches(selected, selected, true, true, true, 0, true),
            Ok(())
        );
        for field in 0..7 {
            let mut actual = selected;
            let (mut terminal, mut supervisor, mut agent, mut job, mut clean) =
                (true, true, true, 0, true);
            match field {
                0 => actual.creation += 1,
                1 => actual.instance -= 1,
                2 => terminal = false,
                3 => supervisor = false,
                4 => agent = false,
                5 => job = 1,
                _ => clean = false,
            };
            assert_eq!(
                completion_matches(selected, actual, terminal, supervisor, agent, job, clean),
                Err(NativeError::Foreign)
            );
        }
    }
}
