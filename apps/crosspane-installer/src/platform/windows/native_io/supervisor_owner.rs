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

/// Observation validation only: never constructs a peer, image, job or completion capability.
pub(crate) fn outer_peer_observations_match(
    expected_token: &super::identity::TokenFacts,
    actual_token: &super::identity::TokenFacts,
    expected_process: (u32, u64),
    actual_process: (u32, u64),
    expected_module: (super::files::FileIdentity, &str),
    actual_module: (super::files::FileIdentity, &str),
) -> NativeResult<()> {
    super::identity::LimitedIdentity::admit(actual_token.clone())
        .map_err(|_| NativeError::Foreign)?;
    super::identity::LimitedIdentity::admit(expected_token.clone())
        .map_err(|_| NativeError::Foreign)?;
    if expected_token != actual_token
        || expected_process.0 == 0
        || expected_process.1 == 0
        || expected_process != actual_process
        || expected_module.0.volume == 0
        || expected_module.0.file == [0; 16]
        || expected_module.0 != actual_module.0
        || super::process::literal_path(expected_module.1)?
            != super::process::literal_path(actual_module.1)?
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

    /// Created only from the PID returned by the connected pipe's kernel query.
    #[cfg(not(test))]
    pub(crate) struct KernelOuterPeer {
        io: Arc<WindowsNativeIo>,
        process: Arc<OwnedHandle>,
        pid: u32,
        created: u64,
        image: String,
        token: identity::TokenFacts,
    }
    #[cfg(not(test))]
    impl KernelOuterPeer {
        fn admit(
            pipe: std::os::windows::io::RawHandle,
            server: bool,
            io: Arc<WindowsNativeIo>,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<Self> {
            proof.check(&io, deadline)?;
            let mut pid = 0;
            // SAFETY: caller owns this connected local pipe throughout the kernel peer query.
            let ok = unsafe {
                if server {
                    GetNamedPipeServerProcessId(pipe, &mut pid)
                } else {
                    GetNamedPipeClientProcessId(pipe, &mut pid)
                }
            };
            // SAFETY: this process's ID only; no process enumeration or claimed PID selection.
            if ok == 0 || pid == 0 || pid == unsafe { GetCurrentProcessId() } {
                return Err(NativeError::Foreign);
            }
            // SAFETY: selection is solely the actual connected pipe's kernel PID. Query/sync only.
            let raw = unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                    0,
                    pid,
                )
            };
            if raw.is_null() {
                return Err(NativeError::Unavailable);
            }
            // SAFETY: successful OpenProcess transferred one real non-inheritable process handle.
            let process = Arc::new(unsafe { OwnedHandle::from_raw_handle(raw) });
            let (created, image) = process_facts(&process, true)?;
            let token = identity::native::observe_process(&process)?;
            identity::LimitedIdentity::admit(token.clone()).map_err(|_| NativeError::Foreign)?;
            if &token != io.target().identity() {
                return Err(NativeError::Foreign);
            }
            let peer = Self {
                io,
                process,
                pid,
                created,
                image,
                token,
            };
            peer.reverify(&peer.io, proof, deadline)?;
            let mut repeated = 0;
            // SAFETY: same owned connected pipe; complete writable peer PID output.
            let ok = unsafe {
                if server {
                    GetNamedPipeServerProcessId(pipe, &mut repeated)
                } else {
                    GetNamedPipeClientProcessId(pipe, &mut repeated)
                }
            };
            if ok == 0 || repeated != pid {
                return Err(NativeError::Foreign);
            }
            Ok(peer)
        }
        pub(crate) fn image(&self) -> &str {
            &self.image
        }
        pub(crate) fn pid(&self) -> u32 {
            self.pid
        }
        pub(crate) fn creation(&self) -> u64 {
            self.created
        }
        pub(crate) fn token(&self) -> &identity::TokenFacts {
            &self.token
        }
        pub(crate) fn reverify(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if !std::ptr::eq(io, self.io.as_ref()) {
                return Err(NativeError::Foreign);
            }
            proof.check(io, deadline)?;
            let (created, image) = process_facts(&self.process, true)?;
            let token = identity::native::observe_process(&self.process)?;
            // SAFETY: already-retained exact kernel-selected process object, never PID reopening.
            if unsafe { GetProcessId(self.process.as_raw_handle()) } != self.pid
                || created != self.created
                || image != self.image
                || token != self.token
                || &token != io.target().identity()
            {
                return Err(NativeError::Foreign);
            }
            deadline.check()
        }
    }
    #[cfg(not(test))]
    enum OuterPeerBinding {
        Keeper { expected: Arc<OwnedHandle> },
        Source { parent: Arc<OwnedHandle> },
        Completion,
        Support,
        KeeperObserver,
        Observer,
    }
    /// Retained actual kernel peer and opened images; no Deserialize/Clone/facts constructor.
    #[cfg(not(test))]
    pub(crate) struct OuterPeerPin {
        io: Arc<WindowsNativeIo>,
        peer: KernelOuterPeer,
        image: super::super::OuterPeerImage,
        keeper: Option<super::super::OuterPeerImage>,
        operation: [u8; 16],
        binding: OuterPeerBinding,
    }
    #[cfg(not(test))]
    impl OuterPeerPin {
        pub(crate) fn pid(&self) -> u32 {
            self.peer.pid
        }
        pub(crate) fn creation(&self) -> u64 {
            self.peer.created
        }
        pub(crate) fn retained_process(&self) -> Arc<OwnedHandle> {
            self.peer.process.clone()
        }
        pub(crate) fn image_identity(&self) -> super::super::files::FileIdentity {
            self.image.identity()
        }
        /// Cleanup-only observation of the SAME previously admitted fixed keeper after exit.
        /// The caller must drop this non-Clone read-pin wrapper before deleting its exact copy.
        pub(crate) fn reverify_exited_keeper(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            operation: [u8; 16],
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if !std::ptr::eq(io, self.io.as_ref())
                || operation != self.operation
                || !matches!(
                    self.binding,
                    OuterPeerBinding::Keeper { .. } | OuterPeerBinding::KeeperObserver
                )
            {
                return Err(NativeError::Foreign);
            }
            proof.check(io, deadline)?;
            let fresh = io.admit_support(deadline)?;
            if &self.peer.token != io.target().identity() {
                return Err(NativeError::Foreign);
            }
            let (created, _) = process_facts(&self.peer.process, false)?;
            // SAFETY: retained exact kernel-selected original object; never PID reopening.
            if unsafe {GetProcessId(self.peer.process.as_raw_handle())}!=self.peer.pid || created!=self.peer.created
                // SAFETY: nonblocking exit observation on that SAME retained object only.
                || unsafe {WaitForSingleObject(self.peer.process.as_raw_handle(),0)}!=WAIT_OBJECT_0
            {
                return Err(NativeError::OutcomeUnknown);
            }
            self.image.reverify(io, &fresh, deadline)?;
            let keeper = self.keeper.as_ref().ok_or(NativeError::Foreign)?;
            keeper.reverify(io, &io.admit_support(deadline)?, deadline)?;
            let record = super::super::super::payload::recovery::OuterUpgradeRecord::read(
                io,
                &io.admit_support(deadline)?,
                deadline,
            )?
            .ok_or(NativeError::Missing)?;
            if record.operation() != operation
                || record.keeper_image() != Some(stamp(self.image.identity()))
                || keeper.identity() != self.image.identity()
                || keeper.facts() != self.image.facts()
            {
                return Err(NativeError::Foreign);
            }
            record.peer_matches(
                self.peer.pid,
                self.peer.created,
                stamp(self.image.identity()),
                self.image.facts(),
                &self.peer.token,
            )?;
            deadline.check()
        }

        pub(crate) fn reverify(
            &self,
            io: &WindowsNativeIo,
            original: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if !std::ptr::eq(io, self.io.as_ref()) {
                return Err(NativeError::Foreign);
            }
            original.check(io, deadline)?;
            let proof = self.io.admit_support(deadline)?;
            self.peer.reverify(&self.io, &proof, deadline)?;
            self.image.reverify(&self.io, &proof, deadline)?;
            use super::super::super::payload::recovery::{OuterPhase, OuterUpgradeRecord};
            let record = OuterUpgradeRecord::read(&self.io, &proof, deadline)?
                .ok_or(NativeError::Missing)?;
            if record.operation() != self.operation {
                return Err(NativeError::Foreign);
            }
            record.context().matches(&self.peer.token)?;
            if let Some(keeper) = &self.keeper {
                keeper.reverify(&self.io, &proof, deadline)?;
                let fresh = self.io.pin_outer_keeper(&proof, self.operation, deadline)?;
                if fresh.identity() != keeper.identity() || fresh.facts() != keeper.facts() {
                    return Err(NativeError::Foreign);
                }
            }
            let actual = (self.peer.pid, self.peer.created);
            let observed = (self.image.identity(), self.peer.image.as_str());
            match &self.binding {
                OuterPeerBinding::Keeper { expected } => {
                    let (creation, path) = process_facts(expected, true)?;
                    // SAFETY: actual reserved child handle, not a serialized PID selection.
                    let pid = unsafe { GetProcessId(expected.as_raw_handle()) };
                    let keeper = self.keeper.as_ref().ok_or(NativeError::Foreign)?;
                    outer_peer_observations_match(
                        self.io.target().identity(),
                        &self.peer.token,
                        (pid, creation),
                        actual,
                        (keeper.identity(), keeper.canonical_dos_path()),
                        observed,
                    )?;
                    if path != self.peer.image {
                        return Err(NativeError::Foreign);
                    }
                    record.peer_matches(
                        actual.0,
                        actual.1,
                        stamp(self.image.identity()),
                        self.image.facts(),
                        &self.peer.token,
                    )?;
                }
                OuterPeerBinding::Source { parent } => {
                    let (creation, path) = process_facts(parent, true)?;
                    // SAFETY: genuine inherited original-parent object, never a PID lookup.
                    let pid = unsafe { GetProcessId(parent.as_raw_handle()) };
                    outer_peer_observations_match(
                        self.io.target().identity(),
                        &self.peer.token,
                        (pid, creation),
                        actual,
                        (self.image.identity(), &path),
                        observed,
                    )?;
                    record.outer().matches(
                        actual.0,
                        actual.1,
                        stamp(self.image.identity()),
                        self.image.facts(),
                    )?;
                    let keeper = self.keeper.as_ref().ok_or(NativeError::Foreign)?;
                    if keeper.facts() != self.image.facts()
                        || !matches!(record.phase(), OuterPhase::Prepared | OuterPhase::Ready)
                    {
                        return Err(NativeError::Foreign);
                    }
                }
                OuterPeerBinding::Completion => {
                    self.io
                        .outer_stop_selection(&proof, self.operation, deadline)?;
                    let expected = record.keeper().ok_or(NativeError::Foreign)?;
                    let keeper = self.keeper.as_ref().ok_or(NativeError::Foreign)?;
                    outer_peer_observations_match(
                        self.io.target().identity(),
                        &self.peer.token,
                        (expected.pid(), expected.creation()),
                        actual,
                        (keeper.identity(), keeper.canonical_dos_path()),
                        observed,
                    )?;
                    record.peer_matches(
                        actual.0,
                        actual.1,
                        stamp(self.image.identity()),
                        self.image.facts(),
                        &self.peer.token,
                    )?;
                }
                OuterPeerBinding::Support => {
                    self.io
                        .outer_support_selection(&proof, self.operation, deadline)?;
                    let expected = record.keeper().ok_or(NativeError::Foreign)?;
                    let keeper = self.keeper.as_ref().ok_or(NativeError::Foreign)?;
                    outer_peer_observations_match(
                        self.io.target().identity(),
                        &self.peer.token,
                        (expected.pid(), expected.creation()),
                        actual,
                        (keeper.identity(), keeper.canonical_dos_path()),
                        observed,
                    )?;
                    record.peer_matches(
                        actual.0,
                        actual.1,
                        stamp(self.image.identity()),
                        self.image.facts(),
                        &self.peer.token,
                    )?;
                }
                OuterPeerBinding::KeeperObserver => {
                    let expected = record.keeper().ok_or(NativeError::Foreign)?;
                    let keeper = self.keeper.as_ref().ok_or(NativeError::Foreign)?;
                    outer_peer_observations_match(
                        self.io.target().identity(),
                        &self.peer.token,
                        (expected.pid(), expected.creation()),
                        actual,
                        (keeper.identity(), keeper.canonical_dos_path()),
                        observed,
                    )?;
                    record.peer_matches(
                        actual.0,
                        actual.1,
                        stamp(self.image.identity()),
                        self.image.facts(),
                        &self.peer.token,
                    )?;
                }
                OuterPeerBinding::Observer => {
                    if !matches!(
                        record.phase(),
                        OuterPhase::Ready | OuterPhase::Committed | OuterPhase::Complete
                    ) {
                        return Err(NativeError::Foreign);
                    }
                }
            }
            deadline.check()
        }
    }
    #[cfg(not(test))]
    fn stamp(
        id: super::super::files::FileIdentity,
    ) -> super::super::super::payload::recovery::FileStamp {
        super::super::super::payload::recovery::FileStamp {
            volume: id.volume,
            file: id.file,
        }
    }
    #[cfg(not(test))]
    pub(crate) fn admit_keeper_peer(
        pipe: std::os::windows::io::RawHandle,
        server: bool,
        io: Arc<WindowsNativeIo>,
        proof: &SupportProof,
        expected_process: &Arc<OwnedHandle>,
        expected_image: &super::super::OpenedPe,
        deadline: &Deadline,
    ) -> NativeResult<OuterPeerPin> {
        expected_image.reverify(&io, proof, deadline)?;
        let record =
            super::super::super::payload::recovery::OuterUpgradeRecord::read(&io, proof, deadline)?
                .ok_or(NativeError::Missing)?;
        let peer = KernelOuterPeer::admit(pipe, server, io.clone(), proof, deadline)?;
        let keeper = io.pin_outer_keeper(proof, record.operation(), deadline)?;
        if keeper.identity() != expected_image.identity()
            || keeper.facts() != expected_image.approved().facts()
        {
            return Err(NativeError::Foreign);
        }
        let image = io.pin_outer_kernel_image(proof, &peer, &keeper.facts().version, deadline)?;
        let pin = OuterPeerPin {
            io,
            peer,
            image,
            keeper: Some(keeper),
            operation: record.operation(),
            binding: OuterPeerBinding::Keeper {
                expected: expected_process.clone(),
            },
        };
        pin.reverify(&pin.io, proof, deadline)?;
        Ok(pin)
    }
    #[cfg(not(test))]
    pub(crate) fn admit_keeper_observer_server(
        pipe: std::os::windows::io::RawHandle,
        io: Arc<WindowsNativeIo>,
        proof: &SupportProof,
        operation: [u8; 16],
        deadline: &Deadline,
    ) -> NativeResult<OuterPeerPin> {
        let peer = KernelOuterPeer::admit(pipe, true, io.clone(), proof, deadline)?;
        let keeper = io.pin_outer_keeper(proof, operation, deadline)?;
        let image = io.pin_outer_kernel_image(proof, &peer, &keeper.facts().version, deadline)?;
        let pin = OuterPeerPin {
            io,
            peer,
            image,
            keeper: Some(keeper),
            operation,
            binding: OuterPeerBinding::KeeperObserver,
        };
        pin.reverify(&pin.io, proof, deadline)?;
        Ok(pin)
    }
    #[cfg(not(test))]
    pub(crate) fn admit_outer_source_peer(
        pipe: std::os::windows::io::RawHandle,
        io: Arc<WindowsNativeIo>,
        proof: &SupportProof,
        parent: &super::super::keeper::KeeperParent,
        own: &super::super::SelfImagePin,
        operation: [u8; 16],
        deadline: &Deadline,
    ) -> NativeResult<OuterPeerPin> {
        parent.reverify(deadline)?;
        own.reverify(&io, proof, deadline)?;
        let peer = KernelOuterPeer::admit(pipe, false, io.clone(), proof, deadline)?;
        let keeper = io.pin_outer_keeper(proof, operation, deadline)?;
        if own.identity() != keeper.identity() || own.facts() != keeper.facts() {
            return Err(NativeError::Foreign);
        }
        let image = io.pin_outer_kernel_image(proof, &peer, &own.facts().version, deadline)?;
        let pin = OuterPeerPin {
            io,
            peer,
            image,
            keeper: Some(keeper),
            operation,
            binding: OuterPeerBinding::Source {
                parent: parent.handle().clone(),
            },
        };
        pin.reverify(&pin.io, proof, deadline)?;
        Ok(pin)
    }
    #[cfg(not(test))]
    pub(crate) fn admit_outer_observer_peer(
        pipe: std::os::windows::io::RawHandle,
        io: Arc<WindowsNativeIo>,
        proof: &SupportProof,
        operation: [u8; 16],
        deadline: &Deadline,
    ) -> NativeResult<OuterPeerPin> {
        let peer = KernelOuterPeer::admit(pipe, false, io.clone(), proof, deadline)?;
        let image = io.pin_outer_kernel_image(proof, &peer, env!("CARGO_PKG_VERSION"), deadline)?;
        let pin = OuterPeerPin {
            io,
            peer,
            image,
            keeper: None,
            operation,
            binding: OuterPeerBinding::Observer,
        };
        pin.reverify(&pin.io, proof, deadline)?;
        Ok(pin)
    }

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
    #[cfg(not(test))]
    fn create_outer_pipe(root: &BrokerAdmission) -> NativeResult<NamedPipeServer> {
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
                    format!("{}.outer", root.endpoint()),
                    (&attributes as *const SECURITY_ATTRIBUTES)
                        .cast_mut()
                        .cast(),
                )
        }
        .map_err(|_| NativeError::Unavailable)
    }

    #[cfg(not(test))]
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct OuterHello {
        request: Request,
        operation: [u8; 16],
        mode: OuterMode,
    }
    #[cfg(not(test))]
    #[derive(Serialize, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    enum OuterMode {
        Probe,
        Complete,
    }
    #[cfg(not(test))]
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct OuterSupportReply {
        schema_version: u32,
        nonce: [u8; 16],
        operation: [u8; 16],
        generation: Generation,
    }
    #[cfg(not(test))]
    struct OuterListener {
        source: Arc<ServerState>,
        closing: AtomicBool,
        thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    }
    /// Companion of the SAME owner/current-agent/terminal/export state, never another job owner.
    #[cfg(not(test))]
    pub(crate) struct OuterOwnerServer {
        state: Arc<OuterListener>,
    }
    #[cfg(not(test))]
    impl OuterOwnerServer {
        pub(crate) fn reserve(
            source: &OwnerServer,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<Self> {
            source.state.root.reverify(proof, deadline)?;
            source
                .state
                .owner
                .exclusive_lease(proof, deadline)?
                .reverify(source.state.root.io(), proof, deadline)?;
            let state = Arc::new(OuterListener {
                source: source.state.clone(),
                closing: AtomicBool::new(false),
                thread: Mutex::new(None),
            });
            let held = state.clone();
            let initial = deadline.clone();
            let (send, recv) = std::sync::mpsc::sync_channel(1);
            let thread=std::thread::Builder::new().name("crosspane-outer-rendezvous".into()).spawn(move || {
                let result=(|| {
                    let rt=runtime()?;
                    rt.block_on(async {
                        let mut pipe=create_outer_pipe(&held.source.root)?;
                        held.source.root.reverify(&held.source.root.io().admit_support(&initial)?,&initial)?;
                        initial.check()?;
                        if held.closing.load(Ordering::Acquire) {return Err(NativeError::Timeout);}
                        send.send(Ok(())).map_err(|_|NativeError::Unavailable)?;
                        loop {
                            if held.closing.load(Ordering::Acquire) || held.source.stopped.load(Ordering::Acquire) {return Ok(());}
                            tokio::select! {
                                connected=pipe.connect()=>{connected.map_err(|_|NativeError::Unavailable)?;},
                                _=tokio::time::sleep(Duration::from_millis(20))=>{continue;}
                            }
                            let deadline=Deadline::new(30_000,held.source.root.io().bound_clock(),Cancellation::default())?;
                            let result=serve_outer(&held.source,&mut pipe,&deadline).await;
                            pipe.disconnect().map_err(|_|NativeError::Unavailable)?;
                            if result.is_err() && held.source.export.lock().map_err(|_|NativeError::Unavailable)?.is_some() {
                                // One shared armed/export ledger: an uncertain session is never reused.
                                held.source.stopped.store(true,Ordering::Release); return result;
                            }
                        }
                    })
                })();
                if result.is_err() {let _=send.try_send(Err(NativeError::OutcomeUnknown));}
                // pipe, peers, handle exports and this captured source alias drop before native thread exit.
            }).map_err(|_|NativeError::Unavailable)?;
            *state
                .thread
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)? = Some(thread);
            match recv.recv_timeout(Duration::from_millis(deadline.remaining_ms()?)) {
                Ok(Ok(())) if deadline.check().is_ok() => Ok(Self { state }),
                _ => {
                    state.closing.store(true, Ordering::Release);
                    Err(NativeError::OutcomeUnknown)
                }
            }
        }
        pub(crate) fn finish_and_settle(
            self,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.state.source.root.reverify(proof, deadline)?;
            self.state.closing.store(true, Ordering::Release);
            loop {
                let finished = self
                    .state
                    .thread
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?
                    .as_ref()
                    .is_some_and(|thread| {
                        // SAFETY: exact owned worker thread; zero-time native exit includes TLS cleanup.
                        (unsafe { WaitForSingleObject(thread.as_raw_handle(), 0) }) == WAIT_OBJECT_0
                    });
                if finished {
                    break;
                }
                deadline.check()?;
                std::thread::sleep(Duration::from_millis(5));
            }
            // Consume this companion and its captured source/endpoint aliases before positive return.
            self.state
                .thread
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?
                .take();
            Ok(())
        }
    }
    #[cfg(not(test))]
    impl Drop for OuterOwnerServer {
        fn drop(&mut self) {
            self.state.closing.store(true, Ordering::Release);
        }
    }
    #[cfg(not(test))]
    async fn serve_outer(
        state: &ServerState,
        pipe: &mut NamedPipeServer,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        let io = state.root.io();
        let proof = io.admit_support(deadline)?;
        if let Some(record) = io.read_payload_repair(&proof, deadline)?
            && !matches!(
                record.phase(),
                super::super::super::repair::payload_record::PayloadRepairPhase::Complete
                    | super::super::super::repair::payload_record::PayloadRepairPhase::Cancelled
                    | super::super::super::repair::payload_record::PayloadRepairPhase::Retired
            )
        {
            return repair_owner::serve(state, pipe, &record, deadline).await;
        }
        if let Some(record) = io.read_removal(&proof, deadline)?
            && record.cursor() != super::super::super::removal::RemovalCursor::Retired
        {
            return removal_owner::serve(state, pipe, &record, deadline).await;
        }
        let peer =
            KernelOuterPeer::admit(pipe.as_raw_handle(), false, io.clone(), &proof, deadline)?;
        // Kernel context is authenticated BEFORE any frame or supplied operation is decoded.
        let record =
            super::super::super::payload::recovery::OuterUpgradeRecord::read(io, &proof, deadline)?
                .ok_or(NativeError::Missing)?;
        use super::super::super::payload::recovery::OuterPhase;
        let binding = match record.phase() {
            OuterPhase::Prepared | OuterPhase::Ready => {
                io.outer_support_selection(&proof, record.operation(), deadline)?;
                OuterPeerBinding::Support
            }
            OuterPhase::Committed => {
                io.outer_stop_selection(&proof, record.operation(), deadline)?;
                OuterPeerBinding::Completion
            }
            _ => return Err(NativeError::Foreign),
        };
        let keeper = io.pin_outer_keeper(&proof, record.operation(), deadline)?;
        let image = io.pin_outer_kernel_image(&proof, &peer, &keeper.facts().version, deadline)?;
        let peer = OuterPeerPin {
            io: io.clone(),
            peer,
            image,
            keeper: Some(keeper),
            operation: record.operation(),
            binding,
        };
        peer.reverify(io, &proof, deadline)?;
        let outer: OuterHello = read_frame(pipe, deadline).await?;
        if outer.operation != peer.operation {
            return Err(NativeError::Foreign);
        }
        let support = matches!(&peer.binding, OuterPeerBinding::Support);
        if (outer.mode == OuterMode::Probe) != support {
            return Err(NativeError::Foreign);
        }
        let hello = outer.request;
        let original = state
            .agent
            .lock()
            .map_err(|_| NativeError::Unavailable)?
            .as_ref()
            .cloned();
        let Some(original) = original else {
            if support {
                return Err(NativeError::Unavailable);
            }
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
        let selected = io.agent_generation(&original, &io.admit_support(deadline)?, deadline)?;
        check_request(&hello, selected)?;
        if hello.method != Method::Hello {
            return Err(NativeError::Foreign);
        }
        let process = state.owner.process_for_peer(&proof, deadline)?;
        let child = state.owner.child_for_peer(&original, &proof, deadline)?;
        if support {
            // Both handles are actual SAME-owner observations; neither is exported by Probe.
            process_facts(&process, true)?;
            let (created, _) = process_facts(&child, true)?;
            // SAFETY: retained original CreateProcess/successor object supplied by the SAME owner.
            if unsafe { GetProcessId(child.as_raw_handle()) } != selected.pid
                || created != selected.creation
            {
                return Err(NativeError::Foreign);
            }
            original.revalidate(io, &io.admit_support(deadline)?, deadline)?;
            peer.reverify(io, &io.admit_support(deadline)?, deadline)?;
            let reply = OuterSupportReply {
                schema_version: 1,
                nonce: hello.nonce,
                operation: peer.operation,
                generation: selected,
            };
            write_frame(pipe, &reply, deadline).await?;
            return Ok(());
        }
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
        write_frame(pipe, &reply, deadline).await?;
        loop {
            let request: Request = read_frame(pipe, deadline).await?;
            check_request(&request, selected)?;
            if request.nonce != hello.nonce || request.operation != Some(peer.operation) {
                return Err(NativeError::Foreign);
            }
            let proof = io.admit_support(deadline)?;
            peer.reverify(io, &proof, deadline)?;
            reply.method = request.method;
            reply.operation = request.operation;
            reply.process = None;
            reply.child = None;
            reply.job = None;
            match request.method {
                Method::Arm => {
                    validate_stop_intent(io, &proof, peer.operation, selected, deadline)?;
                    state
                        .owner
                        .arm_terminal(selected, peer.operation, &proof, deadline)?;
                    *state.export.lock().map_err(|_| NativeError::Unavailable)? =
                        Some((peer.operation, selected));
                }
                Method::Empty => {
                    if state.owner.terminal_operation() != Some(peer.operation) {
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
                            || ack.nonce != hello.nonce
                            || ack.operation != Some(peer.operation)
                        {
                            return Err(NativeError::Foreign);
                        }
                        peer.reverify(io, &io.admit_support(deadline)?, deadline)?;
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
    #[cfg(not(test))]
    struct OuterTransfer {
        original: Arc<Transfer>,
        admission: super::super::OuterCompletionAdmission,
        completion: Mutex<Option<Arc<RetainedTreeCompletion>>>,
        job_export: Mutex<Option<Arc<OuterJobExport>>>,
    }
    /// Actual restricted handles imported only from the authenticated terminal/empty export.
    /// No fact, record, ACK or count can construct this private observation precursor.
    #[cfg(not(test))]
    struct OuterJobExport {
        operation: [u8; 16],
        selected: Generation,
        supervisor: Arc<OwnedHandle>,
        child: Arc<OwnedHandle>,
        job: Arc<OwnedHandle>,
    }
    #[cfg(not(test))]
    static OUTER_TRANSFER: OnceLock<Mutex<Option<Arc<OuterTransfer>>>> = OnceLock::new();

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
    /// Read-only companion support observation BEFORE KeeperReady; no terminal/export command.
    #[cfg(not(test))]
    pub(crate) fn probe_outer_support(
        io: Arc<WindowsNativeIo>,
        proof: &SupportProof,
        selected: &super::super::SelectedOuterOperation,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        if !Arc::ptr_eq(&io, selected.io()) {
            return Err(NativeError::Foreign);
        }
        proof.check(&io, deadline)?;
        selected.module().reverify(&io, proof, deadline)?;
        selected.owner_identity().reverify(deadline)?;
        let record = io.outer_support_selection(proof, selected.operation(), deadline)?;
        record.same_selection(selected.record())?;
        record.peer_matches(
            selected.owner_identity().pid(),
            selected.owner_identity().creation(),
            stamp(selected.module().identity()),
            selected.module().facts(),
            io.target().identity(),
        )?;
        let root = io.broker_admission(proof, deadline)?;
        let original = io.observe_agent(proof, deadline)?;
        root.bind_runtime(&original, proof, deadline)?;
        let generation = io.agent_generation(&original, proof, deadline)?;
        let nonce = io.bridge_nonce(proof, deadline)?;
        let rt = runtime()?;
        rt.block_on(async {
            let mut pipe = loop {
                deadline.check()?;
                match ClientOptions::new().open(format!("{}.outer", root.endpoint())) {
                    Ok(pipe) => break pipe,
                    Err(error) if error.raw_os_error() == Some(231) => {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    Err(error) if error.raw_os_error() == Some(2) => {
                        eprintln!("Windows outer completion unavailable; reinstall required");
                        return Err(NativeError::Unsupported);
                    }
                    Err(_) => return Err(NativeError::Unavailable),
                }
            };
            let peer = Peer::admit(pipe.as_raw_handle(), true, &root, deadline)?;
            let request = Request {
                schema_version: 1,
                nonce,
                method: Method::Hello,
                operation: None,
                generation,
            };
            let hello = OuterHello {
                request,
                operation: selected.operation(),
                mode: OuterMode::Probe,
            };
            write_frame(&mut pipe, &hello, deadline).await?;
            let reply: OuterSupportReply = read_frame(&mut pipe, deadline).await?;
            if reply.schema_version != 1
                || reply.nonce != nonce
                || reply.operation != selected.operation()
                || reply.generation != generation
            {
                return Err(NativeError::Foreign);
            }
            original.revalidate(&io, &io.admit_support(deadline)?, deadline)?;
            peer.reverify(&root, deadline)?;
            selected
                .module()
                .reverify(&io, &io.admit_support(deadline)?, deadline)?;
            selected.owner_identity().reverify(deadline)?;
            io.outer_support_selection(
                &io.admit_support(deadline)?,
                selected.operation(),
                deadline,
            )?;
            deadline.check()
        })
    }
    #[cfg(not(test))]
    impl AdmittedSupervisorOwner {
        pub(crate) fn admit_outer(
            io: Arc<WindowsNativeIo>,
            original: AgentObservation,
            lease: Option<Arc<super::super::StopLockLease>>,
            outer: super::super::OuterCompletionAdmission,
            deadline: &Deadline,
        ) -> NativeResult<Self> {
            let proof = io.admit_support(deadline)?;
            outer.reverify(&io, &proof, deadline)?;
            let root = io.broker_admission(&proof, deadline)?;
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
            let retained = Arc::new(OuterTransfer {
                original: state.clone(),
                admission: outer,
                completion: Mutex::new(None),
                job_export: Mutex::new(None),
            });
            {
                let mut transfer = TRANSFER
                    .get_or_init(|| Mutex::new(None))
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?;
                let mut owner = OUTER_TRANSFER
                    .get_or_init(|| Mutex::new(None))
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?;
                if transfer.is_some() || owner.is_some() {
                    return Err(NativeError::Busy);
                }
                *transfer = Some(state.clone());
                *owner = Some(retained.clone());
            }
            let (send, commands) = std::sync::mpsc::sync_channel(1);
            let (ready, receive) = std::sync::mpsc::sync_channel(1);
            let held = state.clone();
            let captured = retained;
            let initial = deadline.clone();
            let thread = std::thread::Builder::new()
                .name("crosspane-outer-original-owner".into())
                .spawn(move || {
                    let result = (|| {
                        let rt = runtime()?;
                        let pipe = rt.block_on(connect_outer(&held, &captured, &initial))?;
                        ready.send(Ok(())).map_err(|_| NativeError::Unavailable)?;
                        command_loop_outer(&held, &captured, &rt, pipe, commands)
                    })();
                    if result.is_err() {
                        held.retired.store(true, Ordering::Release);
                        let _ = ready.try_send(Err(NativeError::OutcomeUnknown));
                    }
                })
                .map_err(|_| NativeError::OutcomeUnknown)?;
            *state
                .thread
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)? = Some(thread);
            match receive.recv_timeout(Duration::from_millis(deadline.remaining_ms()?)) {
                Ok(Ok(())) if deadline.check().is_ok() => Ok(Self { state, send }),
                _ => {
                    state.retired.store(true, Ordering::Release);
                    Err(NativeError::OutcomeUnknown)
                }
            }
        }
        /// Pure observation of already-produced native completion, never another command or Stop.
        pub(crate) fn observe_retained_completion(
            &self,
            deadline: &Deadline,
        ) -> NativeResult<Option<Arc<RetainedTreeCompletion>>> {
            let outer = OUTER_TRANSFER
                .get()
                .ok_or(NativeError::Unsupported)?
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?
                .as_ref()
                .cloned()
                .ok_or(NativeError::Unsupported)?;
            if !Arc::ptr_eq(&outer.original, &self.state) {
                return Err(NativeError::Foreign);
            }
            let finished = self
                .state
                .thread
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?
                .as_ref()
                .is_some_and(|thread| {
                    // SAFETY: exact retained worker thread; native exit proves queued capabilities/TLS settled.
                    (unsafe { WaitForSingleObject(thread.as_raw_handle(), 0) }) == WAIT_OBJECT_0
                });
            if !finished {
                deadline.check()?;
                return Ok(None);
            }
            let existing = outer
                .completion
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?
                .as_ref()
                .cloned();
            let completed = if let Some(completed) = existing {
                completed
            } else {
                let export = outer
                    .job_export
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?
                    .as_ref()
                    .cloned();
                let Some(export) = export else {
                    return Ok(None);
                };
                if export.operation != outer.admission.operation()
                    || export.selected != self.state.selected
                {
                    return Err(NativeError::Foreign);
                }
                let peer = self
                    .state
                    .peer
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?
                    .as_ref()
                    .cloned()
                    .ok_or(NativeError::Foreign)?;
                if !peer.exited()? {
                    return Ok(None);
                }
                let original = self
                    .state
                    .original
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?
                    .as_ref()
                    .cloned()
                    .ok_or(NativeError::Foreign)?;
                let proof = self.state.root.io().admit_support(deadline)?;
                match original.observe_exit(self.state.root.io(), &proof, deadline)? {
                    ExitObservation::Exited {
                        creation,
                        code: 0,
                        receipt: Some(receipt),
                    } if creation == export.selected.creation
                        && receipt.instance_id == export.selected.instance
                        && receipt.clean
                        && receipt.input_journals_empty
                        && receipt.audio_stopped => {}
                    ExitObservation::Running => return Ok(None),
                    _ => return Err(NativeError::OutcomeUnknown),
                }
                let (creation, _) = process_facts(&export.child, false)?;
                // SAFETY: SAME actual authenticated import, never a PID reopen or new selection.
                if unsafe { GetProcessId(export.child.as_raw_handle()) } != export.selected.pid
                    || creation != export.selected.creation
                {
                    return Err(NativeError::Foreign);
                }
                let completed = Arc::new(RetainedTreeCompletion {
                    operation: export.operation,
                    selected: export.selected,
                    supervisor: export.supervisor.clone(),
                    child: export.child.clone(),
                    job: export.job.clone(),
                });
                completed.reverify(deadline)?;
                // Seal actual retained-object observation before publishing it to a late caller.
                let mut slot = outer
                    .completion
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?;
                if let Some(previous) = slot.as_ref() {
                    previous.clone()
                } else {
                    *slot = Some(completed.clone());
                    completed
                }
            };
            if completed.operation() != outer.admission.operation() {
                return Err(NativeError::Foreign);
            }
            completed.reverify(deadline)?;
            // Native worker exit + actual original exits/clean receipt/empty same job; never reset
            // retired or resend Arm/Complete/Empty/ACK/Stop. All local aliases settle before backup.
            self.state.root.release_image_pin()?;
            if let Some(peer) = self
                .state
                .peer
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?
                .as_ref()
            {
                peer.release_image_pin()?;
            }
            self.state
                .original
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?
                .take();
            self.state
                .lease
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?
                .take();
            let mut slot = TRANSFER
                .get_or_init(|| Mutex::new(None))
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?;
            if slot
                .as_ref()
                .is_some_and(|state| Arc::ptr_eq(state, &self.state))
            {
                *slot = None;
            }
            completed.reverify(deadline)?;
            Ok(Some(completed))
        }
    }

    #[cfg(not(test))]
    async fn connect_outer(
        state: &Transfer,
        outer: &OuterTransfer,
        deadline: &Deadline,
    ) -> NativeResult<NamedPipeClient> {
        let (pipe, peer, reply) = loop {
            let mut pipe = loop {
                deadline.check()?;
                match ClientOptions::new().open(format!("{}.outer", state.root.endpoint())) {
                    Ok(pipe) => break pipe,
                    Err(error) if error.raw_os_error() == Some(2) => {
                        use std::io::Write;
                        let _ = writeln!(
                            std::io::stderr().lock(),
                            "Windows outer completion unavailable; reinstall required"
                        );
                        return Err(NativeError::Unsupported);
                    }
                    Err(error) if error.raw_os_error() == Some(231) => {
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
            outer.admission.reverify(
                state.root.io(),
                &state.root.io().admit_support(deadline)?,
                deadline,
            )?;
            let hello = OuterHello {
                request,
                operation: outer.admission.operation(),
                mode: OuterMode::Complete,
            };
            write_frame(&mut pipe, &hello, deadline).await?;
            let request = hello.request;
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
    #[cfg(not(test))]
    fn command_loop_outer(
        state: &Transfer,
        outer: &OuterTransfer,
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
                        outer.admission.reverify(
                            state.root.io(),
                            &state.root.io().admit_support(&deadline)?,
                            &deadline,
                        )?;
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
                                // Store the actual terminal/empty handle export before ACK/delivery.
                                let (supervisor, child) = {
                                    let handles = state
                                        .handles
                                        .lock()
                                        .map_err(|_| NativeError::OutcomeUnknown)?;
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
                                let exported = Arc::new(OuterJobExport {
                                    operation,
                                    selected: state.selected,
                                    supervisor,
                                    child,
                                    job: job.clone(),
                                });
                                let mut export = match outer.job_export.lock() {
                                    Ok(slot) => slot,
                                    Err(poison) => poison.into_inner(),
                                };
                                if export.is_some() {
                                    return Err(NativeError::OutcomeUnknown);
                                }
                                *export = Some(exported);
                                drop(export);
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
                        // Retain the actual native proof BEFORE any result delivery can be lost.
                        let retained = Arc::new(RetainedTreeCompletion {
                            operation: completed.operation,
                            selected: completed.selected,
                            supervisor: completed.supervisor.clone(),
                            child: completed.child.clone(),
                            job: completed.job.clone(),
                        });
                        let mut slot = match outer.completion.lock() {
                            Ok(slot) => slot,
                            Err(poison) => poison.into_inner(),
                        };
                        if slot.is_some() {
                            return Err(NativeError::OutcomeUnknown);
                        }
                        *slot = Some(retained);
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
        #[cfg(not(test))]
        if let Some(record) = io.read_payload_repair(proof, deadline)?
            && !matches!(
                record.phase(),
                super::super::super::repair::payload_record::PayloadRepairPhase::Complete
                    | super::super::super::repair::payload_record::PayloadRepairPhase::Cancelled
                    | super::super::super::repair::payload_record::PayloadRepairPhase::Retired
            )
        {
            return repair_owner::validate_stop(io, proof, &record, operation, selected, deadline);
        }
        #[cfg(not(test))]
        if let Some(record) = io.read_removal(proof, deadline)?
            && record.cursor() != super::super::super::removal::RemovalCursor::Retired
        {
            return removal_owner::validate_stop(io, proof, &record, operation, selected, deadline);
        }
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

    /// Separate removal role and transfer. Old outer admission/sidecar/constructors are untouched.
    #[cfg(not(test))]
    mod removal_owner {
        use super::super::super::super::{
            payload::recovery::StageCatalog,
            removal::{RemovalCursor, RemovalHandoffStage, RemovalRecord},
            service::journal::{Journal, Phase as JournalPhase},
        };
        use super::super::super::{RemovalCompletionAdmission, RemovalPeerImage};
        use super::*;

        #[derive(Serialize, Deserialize, PartialEq, Eq)]
        #[serde(rename_all = "snake_case")]
        enum RemovalMode {
            ProbeRemoval,
            CompleteRemoval,
        }
        #[derive(Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RemovalHello {
            request: Request,
            operation: [u8; 16],
            mode: RemovalMode,
        }
        #[derive(Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct SupportReply {
            schema_version: u32,
            nonce: [u8; 16],
            operation: [u8; 16],
            generation: Generation,
        }

        fn fresh(
            io: &WindowsNativeIo,
            proof: &SupportProof,
            operation: [u8; 16],
            deadline: &Deadline,
        ) -> NativeResult<RemovalRecord> {
            let record = io
                .read_removal(proof, deadline)?
                .ok_or(NativeError::Missing)?;
            if record.operation() != operation {
                return Err(NativeError::Foreign);
            }
            record.context().matches(io.target().identity())?;
            use super::super::super::records::{RecordName, record_data};
            if let Some(raw) = io.read_record(
                proof,
                RecordName::StageCatalog,
                super::super::super::files::MAX_RECORD_BYTES,
                deadline,
            )? {
                let catalog: StageCatalog = record_data(&RecordName::StageCatalog, raw.bytes())?;
                catalog.validate()?;
                if catalog.active.is_some() {
                    return Err(NativeError::Foreign);
                }
            }
            Ok(record)
        }
        pub(super) fn validate_stop(
            io: &WindowsNativeIo,
            proof: &SupportProof,
            record: &RemovalRecord,
            operation: [u8; 16],
            selected: Generation,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            let actual = fresh(io, proof, operation, deadline)?;
            record.same_selection(&actual)?;
            if actual.cursor() != RemovalCursor::StopIntent
                || !matches!(
                    actual.handoff_stage(),
                    RemovalHandoffStage::Committed { .. }
                )
            {
                return Err(NativeError::Foreign);
            }
            let journal = Journal::read(io, proof, deadline)?.ok_or(NativeError::Missing)?;
            if journal.phase != JournalPhase::StopIntent
                || journal.current != Some(selected)
                || journal.stop_instance != Some(selected.instance)
            {
                return Err(NativeError::Foreign);
            }
            Ok(())
        }
        fn support(record: &RemovalRecord) -> bool {
            record.cursor() == RemovalCursor::Selected
                && matches!(
                    record.handoff_stage(),
                    RemovalHandoffStage::Created { .. }
                        | RemovalHandoffStage::ResumeIntent { .. }
                        | RemovalHandoffStage::Ready { .. }
                )
        }
        pub(super) async fn serve(
            state: &ServerState,
            pipe: &mut NamedPipeServer,
            record: &RemovalRecord,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            let io = state.root.io();
            let proof = io.admit_support(deadline)?;
            // The actual connected kernel process/context is selected before any claimed frame.
            let peer =
                KernelOuterPeer::admit(pipe.as_raw_handle(), false, io.clone(), &proof, deadline)?;
            let image: RemovalPeerImage =
                io.pin_removal_peer(&proof, record.operation(), &peer, deadline)?;
            peer.reverify(io, &proof, deadline)?;
            image.reverify(io, &proof, deadline)?;
            let actual = fresh(io, &proof, record.operation(), deadline)?;
            let probe = support(&actual);
            if !probe && actual.cursor() != RemovalCursor::StopIntent {
                return Err(NativeError::Foreign);
            }
            let hello: RemovalHello = read_frame(pipe, deadline).await?;
            if hello.operation != actual.operation()
                || (hello.mode == RemovalMode::ProbeRemoval) != probe
            {
                return Err(NativeError::Foreign);
            }
            let original = state
                .agent
                .lock()
                .map_err(|_| NativeError::Unavailable)?
                .as_ref()
                .cloned()
                .ok_or(NativeError::Unavailable)?;
            let selected =
                io.agent_generation(&original, &io.admit_support(deadline)?, deadline)?;
            check_request(&hello.request, selected)?;
            if hello.request.method != Method::Hello {
                return Err(NativeError::Foreign);
            }
            let process = state.owner.process_for_peer(&proof, deadline)?;
            let child = state.owner.child_for_peer(&original, &proof, deadline)?;
            if probe {
                original.revalidate(io, &io.admit_support(deadline)?, deadline)?;
                peer.reverify(io, &io.admit_support(deadline)?, deadline)?;
                image.reverify(io, &io.admit_support(deadline)?, deadline)?;
                return write_frame(
                    pipe,
                    &SupportReply {
                        schema_version: 1,
                        nonce: hello.request.nonce,
                        operation: actual.operation(),
                        generation: selected,
                    },
                    deadline,
                )
                .await;
            }
            let mut reply = Reply {
                status: ReplyStatus::Ready,
                schema_version: 1,
                nonce: hello.request.nonce,
                method: Method::Hello,
                operation: None,
                generation: selected,
                process: Some(process.as_raw_handle() as usize as u64),
                child: Some(child.as_raw_handle() as usize as u64),
                job: None,
            };
            write_frame(pipe, &reply, deadline).await?;
            loop {
                let request: Request = read_frame(pipe, deadline).await?;
                check_request(&request, selected)?;
                if request.nonce != hello.request.nonce
                    || request.operation != Some(actual.operation())
                {
                    return Err(NativeError::Foreign);
                }
                let proof = io.admit_support(deadline)?;
                peer.reverify(io, &proof, deadline)?;
                image.reverify(io, &proof, deadline)?;
                let current = fresh(io, &proof, actual.operation(), deadline)?;
                validate_stop(io, &proof, &current, actual.operation(), selected, deadline)?;
                reply.method = request.method;
                reply.operation = request.operation;
                reply.process = None;
                reply.child = None;
                reply.job = None;
                match request.method {
                    Method::Arm => {
                        state
                            .owner
                            .arm_terminal(selected, actual.operation(), &proof, deadline)?;
                        *state.export.lock().map_err(|_| NativeError::Unavailable)? =
                            Some((actual.operation(), selected));
                    }
                    Method::Empty => {
                        if state.owner.terminal_operation() != Some(actual.operation()) {
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
                                || ack.nonce != hello.request.nonce
                                || ack.operation != Some(actual.operation())
                            {
                                return Err(NativeError::Foreign);
                            }
                            peer.reverify(io, &io.admit_support(deadline)?, deadline)?;
                            image.reverify(io, &io.admit_support(deadline)?, deadline)?;
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

        pub(crate) fn probe_removal_support(
            io: Arc<WindowsNativeIo>,
            proof: &SupportProof,
            operation: [u8; 16],
            deadline: &Deadline,
        ) -> NativeResult<()> {
            let selected = io.select_removal_keeper(proof, deadline)?;
            selected.reverify(&io, proof, deadline)?;
            if selected.operation() != operation || !support(selected.record()) {
                return Err(NativeError::Foreign);
            }
            let original = io.observe_agent(proof, deadline)?;
            if original.bootstrap().phase != BootstrapPhase::Ready {
                return Err(NativeError::Unavailable);
            }
            let generation = io.agent_generation(&original, proof, deadline)?;
            let root = io.broker_admission(proof, deadline)?;
            root.bind_runtime(&original, proof, deadline)?;
            let nonce = io.bridge_nonce(proof, deadline)?;
            runtime()?.block_on(async {
                let mut pipe = loop {
                    deadline.check()?;
                    match ClientOptions::new().open(format!("{}.outer", root.endpoint())) {
                        Ok(pipe) => break pipe,
                        Err(error) if error.raw_os_error() == Some(231) => {
                            tokio::time::sleep(Duration::from_millis(5)).await
                        }
                        Err(error) if error.raw_os_error() == Some(2) => {
                            return Err(NativeError::Unsupported);
                        }
                        Err(_) => return Err(NativeError::Unavailable),
                    }
                };
                // The original fixed source/kernel peer is authenticated BEFORE reply decoding.
                let peer = Peer::admit(pipe.as_raw_handle(), true, &root, deadline)?;
                selected.reverify(&io, &io.admit_support(deadline)?, deadline)?;
                let hello = RemovalHello {
                    request: Request {
                        schema_version: 1,
                        nonce,
                        method: Method::Hello,
                        operation: None,
                        generation,
                    },
                    operation,
                    mode: RemovalMode::ProbeRemoval,
                };
                write_frame(&mut pipe, &hello, deadline).await?;
                let reply: SupportReply = read_frame(&mut pipe, deadline).await?;
                if reply.schema_version != 1
                    || reply.nonce != nonce
                    || reply.operation != operation
                    || reply.generation != generation
                {
                    return Err(NativeError::Foreign);
                }
                peer.reverify(&root, deadline)?;
                original.revalidate(&io, &io.admit_support(deadline)?, deadline)?;
                selected.reverify(&io, &io.admit_support(deadline)?, deadline)?;
                Ok(())
            })
        }

        enum ControlRole {
            Keeper {
                process: Arc<OwnedHandle>,
                identity: super::super::super::files::FileIdentity,
            },
            Source {
                parent: Arc<OwnedHandle>,
                own: super::super::super::SelfImagePin,
                selection: Arc<super::super::super::RemovalKeeperSelection>,
            },
        }
        /// Authentication of the real preparation/commit endpoint, never a serialized permit.
        pub(crate) struct RemovalControlPeer {
            io: Arc<WindowsNativeIo>,
            peer: KernelOuterPeer,
            image: super::super::super::OuterPeerImage,
            operation: [u8; 16],
            role: ControlRole,
        }
        impl RemovalControlPeer {
            pub(crate) fn reverify(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                if !std::ptr::eq(io, self.io.as_ref()) {
                    return Err(NativeError::Foreign);
                }
                self.peer.reverify(io, proof, deadline)?;
                self.image.reverify(io, proof, deadline)?;
                let record = fresh(io, proof, self.operation, deadline)?;
                // This is control-channel authentication only. Later same-owner Status has no
                // Arm/Stop/Run or completion authority; every destructive method has its own gate.
                if record.cursor() == RemovalCursor::Retired {
                    return Err(NativeError::Foreign);
                }
                match &self.role {
                    ControlRole::Keeper { process, identity } => {
                        let (creation, path) = process_facts(process, true)?;
                        // SAFETY: originally created child object retained by the actual creator.
                        let pid = unsafe { GetProcessId(process.as_raw_handle()) };
                        if self.peer.pid != pid
                            || self.peer.created != creation
                            || self.peer.image != path
                            || self.image.identity() != *identity
                        {
                            return Err(NativeError::Foreign);
                        }
                        let copy = record
                            .plan()
                            .copies()
                            .iter()
                            .find(|copy| copy.identity() == Some(stamp(*identity)))
                            .ok_or(NativeError::Foreign)?;
                        if copy.pid() != Some(pid)
                            || copy.creation() != Some(creation)
                            || copy.image() != Some(self.image.facts())
                        {
                            return Err(NativeError::Foreign);
                        }
                    }
                    ControlRole::Source {
                        parent,
                        own,
                        selection,
                    } => {
                        own.reverify(io, proof, deadline)?;
                        selection.reverify(io, proof, deadline)?;
                        if selection.operation() != self.operation
                            || selection.module().identity() != own.identity()
                            || selection.module().facts() != own.facts()
                        {
                            return Err(NativeError::Foreign);
                        }
                        let (creation, path) = process_facts(parent, true)?;
                        // SAFETY: the exact explicitly inherited original parent handle only.
                        let pid = unsafe { GetProcessId(parent.as_raw_handle()) };
                        if self.peer.pid != pid
                            || self.peer.created != creation
                            || self.peer.image != path
                            || self.image.facts() != own.facts()
                        {
                            return Err(NativeError::Foreign);
                        }
                    }
                }
                deadline.check()
            }
            pub(crate) fn image_identity(&self) -> super::super::super::files::FileIdentity {
                self.image.identity()
            }
        }
        // Signed additive admission keeps original IO, context, actual object and deadline
        // separate; restructuring would obscure the sealed authority inputs.
        #[allow(clippy::too_many_arguments)]
        pub(crate) fn admit_removal_keeper_peer(
            pipe: std::os::windows::io::RawHandle,
            is_server: bool,
            io: Arc<WindowsNativeIo>,
            proof: &SupportProof,
            process: &Arc<OwnedHandle>,
            image: &super::super::super::OpenedPe,
            operation: [u8; 16],
            deadline: &Deadline,
        ) -> NativeResult<RemovalControlPeer> {
            image.reverify(&io, proof, deadline)?;
            let peer = KernelOuterPeer::admit(pipe, is_server, io.clone(), proof, deadline)?;
            let opened = io.pin_outer_kernel_image(
                proof,
                &peer,
                &image.approved().facts().version,
                deadline,
            )?;
            if opened.identity() != image.identity()
                || opened.facts() != image.approved().facts()
                || opened.canonical_dos_path() != image.canonical_dos_path()
            {
                return Err(NativeError::Foreign);
            }
            let result = RemovalControlPeer {
                io,
                peer,
                image: opened,
                operation,
                role: ControlRole::Keeper {
                    process: process.clone(),
                    identity: image.identity(),
                },
            };
            result.reverify(&result.io, proof, deadline)?;
            Ok(result)
        }
        // Signed additive admission keeps original IO, context, actual object and deadline
        // separate; restructuring would obscure the sealed authority inputs.
        #[allow(clippy::too_many_arguments)]
        pub(crate) fn admit_removal_source_peer(
            pipe: std::os::windows::io::RawHandle,
            io: Arc<WindowsNativeIo>,
            proof: &SupportProof,
            parent: &super::super::super::super::payload::helper::removal::RemovalParent,
            own: super::super::super::SelfImagePin,
            selection: Arc<super::super::super::RemovalKeeperSelection>,
            operation: [u8; 16],
            deadline: &Deadline,
        ) -> NativeResult<RemovalControlPeer> {
            parent.reverify(&io, proof, operation, deadline)?;
            own.reverify(&io, proof, deadline)?;
            let peer = KernelOuterPeer::admit(pipe, false, io.clone(), proof, deadline)?;
            let image = io.pin_outer_kernel_image(proof, &peer, &own.facts().version, deadline)?;
            let result = RemovalControlPeer {
                io,
                peer,
                image,
                operation,
                role: ControlRole::Source {
                    parent: parent.retained_process().clone(),
                    own,
                    selection,
                },
            };
            result.reverify(&result.io, proof, deadline)?;
            Ok(result)
        }

        struct RemovalTransfer {
            original: Arc<Transfer>,
            admission: RemovalCompletionAdmission,
            completion: Mutex<Option<Arc<RetainedTreeCompletion>>>,
            arm_attempted: AtomicBool,
            complete_attempted: AtomicBool,
        }
        static REMOVAL_TRANSFER: OnceLock<Mutex<Option<Arc<RemovalTransfer>>>> = OnceLock::new();
        pub(crate) struct AdmittedRemovalOwner {
            inner: Option<AdmittedSupervisorOwner>,
            sidecar: Option<Arc<RemovalTransfer>>,
        }
        impl AdmittedRemovalOwner {
            pub(crate) fn admit(
                io: Arc<WindowsNativeIo>,
                original: AgentObservation,
                lease: Arc<super::super::super::StopLockLease>,
                admission: RemovalCompletionAdmission,
                deadline: &Deadline,
            ) -> NativeResult<Self> {
                let proof = io.admit_support(deadline)?;
                admission.reverify(&io, &proof, deadline)?;
                lease.reverify(&proof, deadline)?;
                let root = io.broker_admission(&proof, deadline)?;
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
                    lease: Mutex::new(Some(lease)),
                    retired: AtomicBool::new(false),
                    thread: Mutex::new(None),
                });
                let held = Arc::new(RemovalTransfer {
                    original: state.clone(),
                    admission,
                    completion: Mutex::new(None),
                    arm_attempted: AtomicBool::new(false),
                    complete_attempted: AtomicBool::new(false),
                });
                {
                    let mut current = TRANSFER
                        .get_or_init(|| Mutex::new(None))
                        .lock()
                        .map_err(|_| NativeError::OutcomeUnknown)?;
                    let mut removal = REMOVAL_TRANSFER
                        .get_or_init(|| Mutex::new(None))
                        .lock()
                        .map_err(|_| NativeError::OutcomeUnknown)?;
                    if current.is_some() || removal.is_some() {
                        return Err(NativeError::Busy);
                    }
                    *current = Some(state.clone());
                    *removal = Some(held.clone());
                }
                let (send, commands) = std::sync::mpsc::sync_channel(1);
                let (ready, receive) = std::sync::mpsc::sync_channel(1);
                let captured = held.clone();
                let initial = deadline.clone();
                let thread = std::thread::Builder::new()
                    .name("crosspane-removal-original-owner".into())
                    .spawn(move || {
                        let state = &captured.original;
                        let result = (|| {
                            let rt = runtime()?;
                            let pipe =
                                rt.block_on(connect(state, &captured.admission, &initial))?;
                            ready
                                .send(Ok(()))
                                .map_err(|_| NativeError::OutcomeUnknown)?;
                            // Shared actual original-handle/job import mechanics; no upgrade selection.
                            command_loop(state, &rt, pipe, commands)
                        })();
                        if result.is_err() {
                            state.retired.store(true, Ordering::Release);
                            let _ = ready.try_send(Err(NativeError::OutcomeUnknown));
                        }
                    })
                    .map_err(|_| NativeError::OutcomeUnknown)?;
                *state
                    .thread
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)? = Some(thread);
                match receive.recv_timeout(Duration::from_millis(deadline.remaining_ms()?)) {
                    Ok(Ok(())) if deadline.check().is_ok() => Ok(Self {
                        inner: Some(AdmittedSupervisorOwner { state, send }),
                        sidecar: Some(held),
                    }),
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
                let held = self.sidecar.as_ref().ok_or(NativeError::Foreign)?;
                if operation != held.admission.operation() {
                    return Err(NativeError::Foreign);
                }
                if held.arm_attempted.swap(true, Ordering::AcqRel) {
                    return Err(NativeError::OutcomeUnknown);
                }
                held.admission.reverify(
                    held.original.root.io(),
                    &held.original.root.io().admit_support(deadline)?,
                    deadline,
                )?;
                self.inner
                    .as_ref()
                    .ok_or(NativeError::Foreign)?
                    .arm_terminal(operation, deadline)
            }
            pub(crate) fn observe_completion(
                &self,
                operation: [u8; 16],
                deadline: &Deadline,
            ) -> NativeResult<()> {
                let held = self.sidecar.as_ref().ok_or(NativeError::Foreign)?;
                if operation != held.admission.operation() {
                    return Err(NativeError::Foreign);
                }
                if held.complete_attempted.swap(true, Ordering::AcqRel) {
                    return Err(NativeError::OutcomeUnknown);
                }
                let inner = self.inner.as_ref().ok_or(NativeError::Foreign)?;
                let (send, recv) = std::sync::mpsc::sync_channel(1);
                inner
                    .send
                    .try_send(Command::Complete(operation, deadline.clone(), send))
                    .map_err(|_| NativeError::OutcomeUnknown)?;
                let tree = Arc::new(inner.receive(recv, deadline)?);
                tree.reverify(deadline)?;
                // Preserve the actual native result BEFORE any later settlement failure.
                *held
                    .completion
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)? = Some(tree);
                inner.settle(deadline)
            }
            pub(crate) fn finish_settled(
                &mut self,
                deadline: &Deadline,
            ) -> NativeResult<Option<Arc<RetainedTreeCompletion>>> {
                let held = self.sidecar.as_ref().ok_or(NativeError::Foreign)?;
                let state = &held.original;
                let finished = state
                    .thread
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?
                    .as_ref()
                    .is_some_and(|thread| {
                        // SAFETY: exact actual owned worker thread; native exit includes TLS cleanup.
                        (unsafe { WaitForSingleObject(thread.as_raw_handle(), 0) }) == WAIT_OBJECT_0
                    });
                if !finished {
                    deadline.check()?;
                    return Ok(None);
                }
                let existing = held
                    .completion
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?
                    .as_ref()
                    .cloned();
                let tree = if let Some(tree) = existing {
                    tree
                } else {
                    held.admission.reverify(
                        state.root.io(),
                        &state.root.io().admit_support(deadline)?,
                        deadline,
                    )?;
                    let peer = state
                        .peer
                        .lock()
                        .map_err(|_| NativeError::OutcomeUnknown)?
                        .as_ref()
                        .cloned()
                        .ok_or(NativeError::OutcomeUnknown)?;
                    if !peer.exited()? {
                        return Ok(None);
                    }
                    let original = state
                        .original
                        .lock()
                        .map_err(|_| NativeError::OutcomeUnknown)?
                        .as_ref()
                        .cloned()
                        .ok_or(NativeError::OutcomeUnknown)?;
                    match original.observe_exit(
                        state.root.io(),
                        &state.root.io().admit_support(deadline)?,
                        deadline,
                    )? {
                        ExitObservation::Exited {
                            creation,
                            code: 0,
                            receipt: Some(receipt),
                        } if creation == state.selected.creation
                            && receipt.instance_id == state.selected.instance
                            && receipt.clean
                            && receipt.input_journals_empty
                            && receipt.audio_stopped => {}
                        ExitObservation::Running => return Ok(None),
                        _ => return Err(NativeError::OutcomeUnknown),
                    }
                    let handles = state
                        .handles
                        .lock()
                        .map_err(|_| NativeError::OutcomeUnknown)?;
                    let tree = Arc::new(RetainedTreeCompletion {
                        operation: held.admission.operation(),
                        selected: state.selected,
                        supervisor: handles
                            .supervisor
                            .as_ref()
                            .cloned()
                            .ok_or(NativeError::OutcomeUnknown)?,
                        child: handles
                            .child
                            .as_ref()
                            .cloned()
                            .ok_or(NativeError::OutcomeUnknown)?,
                        job: handles
                            .job
                            .as_ref()
                            .cloned()
                            .ok_or(NativeError::OutcomeUnknown)?,
                    });
                    tree.reverify(deadline)?;
                    *held
                        .completion
                        .lock()
                        .map_err(|_| NativeError::OutcomeUnknown)? = Some(tree.clone());
                    tree
                };
                tree.reverify(deadline)?;
                // The service MUST physically settle its ONE agent transport before calling this.
                // Acquire every fallible guard and verify the final proof BEFORE irreversible
                // slot consumption. Errors leave both the worker handle and actual proof retained.
                let mut slot = TRANSFER
                    .get_or_init(|| Mutex::new(None))
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?;
                if slot
                    .as_ref()
                    .is_some_and(|value| !Arc::ptr_eq(value, state))
                {
                    return Err(NativeError::Foreign);
                }
                let mut side = REMOVAL_TRANSFER
                    .get_or_init(|| Mutex::new(None))
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?;
                if !side.as_ref().is_some_and(|value| Arc::ptr_eq(value, held))
                    || Arc::strong_count(held) != 2
                {
                    return Err(NativeError::OutcomeUnknown);
                }
                let mut peer = state.peer.lock().map_err(|_| NativeError::OutcomeUnknown)?;
                let mut original = state
                    .original
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?;
                let mut lease = state
                    .lease
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?;
                let mut thread = state
                    .thread
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?;
                state.root.release_image_pin()?;
                tree.reverify(deadline)?;
                // No fallible work follows this same-object alias drop barrier.
                peer.take();
                original.take();
                lease.take();
                thread.take();
                slot.take();
                side.take();
                drop(peer);
                drop(original);
                drop(lease);
                drop(thread);
                drop(slot);
                drop(side);
                self.inner.take();
                self.sidecar.take();
                // No Transfer/Broker/image/install anchor is contained in the returned bare proof.
                Ok(Some(tree))
            }
        }

        async fn connect(
            state: &Transfer,
            admission: &RemovalCompletionAdmission,
            deadline: &Deadline,
        ) -> NativeResult<NamedPipeClient> {
            let mut pipe = loop {
                deadline.check()?;
                match ClientOptions::new().open(format!("{}.outer", state.root.endpoint())) {
                    Ok(pipe) => break pipe,
                    Err(error) if error.raw_os_error() == Some(231) => {
                        tokio::time::sleep(Duration::from_millis(5)).await
                    }
                    Err(error) if error.raw_os_error() == Some(2) => {
                        return Err(NativeError::Unsupported);
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
            *state.peer.lock().map_err(|_| NativeError::OutcomeUnknown)? = Some(peer.clone());
            admission.reverify(
                state.root.io(),
                &state.root.io().admit_support(deadline)?,
                deadline,
            )?;
            let request = Request {
                schema_version: 1,
                nonce: state.nonce,
                method: Method::Hello,
                operation: None,
                generation: state.selected,
            };
            let hello = RemovalHello {
                request,
                operation: admission.operation(),
                mode: RemovalMode::CompleteRemoval,
            };
            write_frame(&mut pipe, &hello, deadline).await?;
            let reply: Reply = read_frame(&mut pipe, deadline).await?;
            check_reply(&reply, &hello.request)?;
            if reply.status != ReplyStatus::Ready {
                return Err(NativeError::Unavailable);
            }
            let supervisor = Arc::new(peer.duplicate(
                reply.process.ok_or(NativeError::Foreign)?,
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                deadline,
            )?);
            state
                .handles
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?
                .supervisor = Some(supervisor.clone());
            let child = Arc::new(peer.duplicate(
                reply.child.ok_or(NativeError::Foreign)?,
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                deadline,
            )?);
            state
                .handles
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?
                .child = Some(child.clone());
            let (creation, _) = process_facts(&supervisor, true)?;
            // SAFETY: query only SAME locally retained authenticated source/child objects.
            if unsafe{GetProcessId(supervisor.as_raw_handle())}!=peer.pid || creation!=peer.created
                // SAFETY: SAME actual original child imported from its genuine owning supervisor.
                || unsafe{GetProcessId(child.as_raw_handle())}!=state.selected.pid
            {
                return Err(NativeError::Foreign);
            }
            let (created, image) = process_facts(&child, true)?;
            let original = state
                .original
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?
                .as_ref()
                .cloned()
                .ok_or(NativeError::Foreign)?;
            if created != state.selected.creation
                || image != super::super::super::process::literal_path(original.image_canonical())?
                || identity::native::observe_process(&child)? != *state.root.token()
            {
                return Err(NativeError::Foreign);
            }
            original.revalidate(
                state.root.io(),
                &state.root.io().admit_support(deadline)?,
                deadline,
            )?;
            peer.reverify(&state.root, deadline)?;
            Ok(pipe)
        }
    }
    /// Third distinct role shares the original Transfer, server latch and export ledger.
    #[cfg(not(test))]
    mod repair_owner {
        use super::super::super::super::{
            payload::{
                inventory::PayloadRole,
                recovery::{OriginalLeaf, StageCatalog},
            },
            repair::payload_record::{PayloadRepairPhase, PayloadRepairRecord},
            service::journal::{Journal, Phase as JournalPhase},
        };
        use super::super::super::{RepairCompletionAdmission, RepairFixedPayload, RepairPeerImage};
        use super::*;

        #[derive(Serialize, Deserialize, PartialEq, Eq)]
        #[serde(rename_all = "snake_case")]
        enum RepairMode {
            ProbeRepair,
            CompleteRepair,
            ProveReady,
        }
        #[derive(Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RepairHello {
            request: Request,
            operation: [u8; 16],
            mode: RepairMode,
        }
        #[derive(Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct SupportReply {
            schema_version: u32,
            nonce: [u8; 16],
            operation: [u8; 16],
            generation: Generation,
        }

        fn fresh(
            io: &WindowsNativeIo,
            proof: &SupportProof,
            operation: [u8; 16],
            deadline: &Deadline,
        ) -> NativeResult<PayloadRepairRecord> {
            let record = io
                .read_payload_repair(proof, deadline)?
                .ok_or(NativeError::Missing)?;
            if record.operation() != operation {
                return Err(NativeError::Foreign);
            }
            record.context().matches(io.target().identity())?;
            if let Some(removal) = io.read_removal(proof, deadline)?
                && removal.cursor() != super::super::super::super::removal::RemovalCursor::Retired
            {
                return Err(NativeError::Foreign);
            }
            if let Some(outer) =
                super::super::super::super::payload::recovery::OuterUpgradeRecord::read(
                    io, proof, deadline,
                )?
            {
                use super::super::super::super::payload::recovery::{OuterCopyCleanup, OuterPhase};
                if !matches!(outer.phase(), OuterPhase::Complete | OuterPhase::Cancelled)
                    || outer.copy_cleanup() != OuterCopyCleanup::Absent
                {
                    return Err(NativeError::Foreign);
                }
            }
            use super::super::super::records::{RecordName, record_data};
            if let Some(raw) = io.read_record(
                proof,
                RecordName::StageCatalog,
                super::super::super::files::MAX_RECORD_BYTES,
                deadline,
            )? {
                let catalog: StageCatalog = record_data(&RecordName::StageCatalog, raw.bytes())?;
                catalog.validate()?;
                if catalog.active.is_some() {
                    return Err(NativeError::Foreign);
                }
            }
            Ok(record)
        }
        pub(super) fn validate_stop(
            io: &WindowsNativeIo,
            proof: &SupportProof,
            record: &PayloadRepairRecord,
            operation: [u8; 16],
            selected: Generation,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            let actual = fresh(io, proof, operation, deadline)?;
            if !record.same_selection(&actual)
                || actual.original_generation() != selected
                || actual.phase() != PayloadRepairPhase::StopIntent
            {
                return Err(NativeError::Foreign);
            }
            let journal = Journal::read(io, proof, deadline)?.ok_or(NativeError::Missing)?;
            if journal.phase != JournalPhase::StopIntent
                || journal.current != Some(selected)
                || journal.stop_instance != Some(selected.instance)
            {
                return Err(NativeError::Foreign);
            }
            Ok(())
        }
        fn support(record: &PayloadRepairRecord) -> bool {
            matches!(
                record.phase(),
                PayloadRepairPhase::Created
                    | PayloadRepairPhase::ResumeIntent
                    | PayloadRepairPhase::Ready
            )
        }
        fn start_ready(record: &PayloadRepairRecord) -> bool {
            matches!(
                record.phase(),
                PayloadRepairPhase::StartIntent
                    | PayloadRepairPhase::StartSubmitted
                    | PayloadRepairPhase::ReadyObserved
            )
        }
        /// Matches a freshly admitted observation to the published new Agent. These facts never
        /// construct an owner, approve an image, or grant terminal/export authority.
        fn ready_generation(
            io: &WindowsNativeIo,
            proof: &SupportProof,
            record: &PayloadRepairRecord,
            agent: &AgentObservation,
            deadline: &Deadline,
        ) -> NativeResult<Generation> {
            if !start_ready(record) || agent.bootstrap().phase != BootstrapPhase::Ready {
                return Err(NativeError::Foreign);
            }
            agent.revalidate(io, proof, deadline)?;
            let generation = io.agent_generation(agent, proof, deadline)?;
            let original = record.original_generation();
            let published = record
                .role(PayloadRole::Agent)
                .published()
                .ok_or(NativeError::Foreign)?;
            let identity = io.agent_identity(agent, proof, deadline)?;
            let old_identity = match record.selection().fixed(PayloadRole::Agent) {
                OriginalLeaf::Present(identity) => identity,
                _ => return Err(NativeError::Foreign),
            };
            if generation == original
                || generation.instance == original.instance
                || generation.creation == original.creation
                || record.phase() == PayloadRepairPhase::ReadyObserved
                    && record.ready() != Some(generation)
                || published.identity != identity.into()
                || published.identity == old_identity
            {
                return Err(NativeError::Foreign);
            }
            deadline.check()?;
            Ok(generation)
        }
        pub(super) async fn serve(
            state: &ServerState,
            pipe: &mut NamedPipeServer,
            record: &PayloadRepairRecord,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            let io = state.root.io();
            let proof = io.admit_support(deadline)?;
            // The actual connected kernel process/context is selected before any claimed frame.
            let peer =
                KernelOuterPeer::admit(pipe.as_raw_handle(), false, io.clone(), &proof, deadline)?;
            let image: RepairPeerImage =
                io.pin_repair_peer(&proof, record.operation(), &peer, deadline)?;
            peer.reverify(io, &proof, deadline)?;
            image.reverify(io, &proof, deadline)?;
            let actual = fresh(io, &proof, record.operation(), deadline)?;
            let probe = support(&actual);
            if !probe && !start_ready(&actual) && actual.phase() != PayloadRepairPhase::StopIntent {
                return Err(NativeError::Foreign);
            }
            let hello: RepairHello = read_frame(pipe, deadline).await?;
            if hello.operation != actual.operation() {
                return Err(NativeError::Foreign);
            }
            if hello.mode == RepairMode::ProveReady {
                let agent = state
                    .agent
                    .lock()
                    .map_err(|_| NativeError::Unavailable)?
                    .as_ref()
                    .cloned()
                    .ok_or(NativeError::Unavailable)?;
                let proof = io.admit_support(deadline)?;
                let generation = ready_generation(io, &proof, &actual, &agent, deadline)?;
                check_request(&hello.request, generation)?;
                if hello.request.method != Method::Hello {
                    return Err(NativeError::Foreign);
                }
                // Only the original native owner can supply these exact retained objects. Its
                // child accessor positively checks the new child's membership in its own job.
                let process = state.owner.process_for_peer(&proof, deadline)?;
                let child = state.owner.child_for_peer(&agent, &proof, deadline)?;
                peer.reverify(io, &proof, deadline)?;
                image.reverify(io, &proof, deadline)?;
                let mut reply = Reply {
                    status: ReplyStatus::Ready,
                    schema_version: 1,
                    nonce: hello.request.nonce,
                    method: Method::Hello,
                    operation: None,
                    generation,
                    process: Some(process.as_raw_handle() as usize as u64),
                    child: Some(child.as_raw_handle() as usize as u64),
                    job: None,
                };
                write_frame(pipe, &reply, deadline).await?;
                // This separate mode accepts only ready-only Acks. Arm/Empty cannot enter
                // the terminal loop, state.export, or job export from this connection.
                let ack: Request = read_frame(pipe, deadline).await?;
                check_request(&ack, generation)?;
                if ack.method != Method::Ack
                    || ack.nonce != hello.request.nonce
                    || ack.operation != Some(actual.operation())
                {
                    return Err(NativeError::Foreign);
                }
                let proof = io.admit_support(deadline)?;
                let current = fresh(io, &proof, actual.operation(), deadline)?;
                if !current.same_selection(&actual)
                    || ready_generation(io, &proof, &current, &agent, deadline)? != generation
                {
                    return Err(NativeError::Foreign);
                }
                let current_agent = state
                    .agent
                    .lock()
                    .map_err(|_| NativeError::Unavailable)?
                    .as_ref()
                    .cloned()
                    .ok_or(NativeError::Unavailable)?;
                if ready_generation(io, &proof, &current, &current_agent, deadline)? != generation
                    || !Arc::ptr_eq(&process, &state.owner.process_for_peer(&proof, deadline)?)
                    || !Arc::ptr_eq(
                        &child,
                        &state
                            .owner
                            .child_for_peer(&current_agent, &proof, deadline)?,
                    )
                {
                    return Err(NativeError::Foreign);
                }
                peer.reverify(io, &proof, deadline)?;
                image.reverify(io, &proof, deadline)?;
                reply.method = Method::Ack;
                reply.operation = ack.operation;
                reply.process = None;
                reply.child = None;
                write_frame(pipe, &reply, deadline).await?;
                // Do not Disconnect while that reply may still be queued. The client sends
                // this final ready-only Ack after consuming it, then positively observes EOF.
                let close: Request = read_frame(pipe, deadline).await?;
                check_request(&close, generation)?;
                if close.method != Method::Ack
                    || close.nonce != hello.request.nonce
                    || close.operation != Some(actual.operation())
                {
                    return Err(NativeError::Foreign);
                }
                let proof = io.admit_support(deadline)?;
                let current = fresh(io, &proof, actual.operation(), deadline)?;
                let current_agent = state
                    .agent
                    .lock()
                    .map_err(|_| NativeError::Unavailable)?
                    .as_ref()
                    .cloned()
                    .ok_or(NativeError::Unavailable)?;
                if !current.same_selection(&actual)
                    || ready_generation(io, &proof, &current, &agent, deadline)? != generation
                    || ready_generation(io, &proof, &current, &current_agent, deadline)?
                        != generation
                    || !Arc::ptr_eq(&process, &state.owner.process_for_peer(&proof, deadline)?)
                    || !Arc::ptr_eq(
                        &child,
                        &state
                            .owner
                            .child_for_peer(&current_agent, &proof, deadline)?,
                    )
                {
                    return Err(NativeError::Foreign);
                }
                peer.reverify(io, &proof, deadline)?;
                image.reverify(io, &proof, deadline)?;
                // The existing listener disconnects the actual server pipe after this return.
                return Ok(());
            }
            if start_ready(&actual) || (hello.mode == RepairMode::ProbeRepair) != probe {
                return Err(NativeError::Foreign);
            }
            let original = state
                .agent
                .lock()
                .map_err(|_| NativeError::Unavailable)?
                .as_ref()
                .cloned()
                .ok_or(NativeError::Unavailable)?;
            let selected =
                io.agent_generation(&original, &io.admit_support(deadline)?, deadline)?;
            if selected != actual.original_generation() {
                return Err(NativeError::Foreign);
            }
            check_request(&hello.request, selected)?;
            if hello.request.method != Method::Hello {
                return Err(NativeError::Foreign);
            }
            let process = state.owner.process_for_peer(&proof, deadline)?;
            let child = state.owner.child_for_peer(&original, &proof, deadline)?;
            if probe {
                original.revalidate(io, &io.admit_support(deadline)?, deadline)?;
                peer.reverify(io, &io.admit_support(deadline)?, deadline)?;
                image.reverify(io, &io.admit_support(deadline)?, deadline)?;
                return write_frame(
                    pipe,
                    &SupportReply {
                        schema_version: 1,
                        nonce: hello.request.nonce,
                        operation: actual.operation(),
                        generation: selected,
                    },
                    deadline,
                )
                .await;
            }
            let mut reply = Reply {
                status: ReplyStatus::Ready,
                schema_version: 1,
                nonce: hello.request.nonce,
                method: Method::Hello,
                operation: None,
                generation: selected,
                process: Some(process.as_raw_handle() as usize as u64),
                child: Some(child.as_raw_handle() as usize as u64),
                job: None,
            };
            write_frame(pipe, &reply, deadline).await?;
            loop {
                let request: Request = read_frame(pipe, deadline).await?;
                check_request(&request, selected)?;
                if request.nonce != hello.request.nonce
                    || request.operation != Some(actual.operation())
                {
                    return Err(NativeError::Foreign);
                }
                let proof = io.admit_support(deadline)?;
                peer.reverify(io, &proof, deadline)?;
                image.reverify(io, &proof, deadline)?;
                let current = fresh(io, &proof, actual.operation(), deadline)?;
                validate_stop(io, &proof, &current, actual.operation(), selected, deadline)?;
                reply.method = request.method;
                reply.operation = request.operation;
                reply.process = None;
                reply.child = None;
                reply.job = None;
                match request.method {
                    Method::Arm => {
                        state
                            .owner
                            .arm_terminal(selected, actual.operation(), &proof, deadline)?;
                        *state.export.lock().map_err(|_| NativeError::Unavailable)? =
                            Some((actual.operation(), selected));
                    }
                    Method::Empty => {
                        if state.owner.terminal_operation() != Some(actual.operation()) {
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
                                || ack.nonce != hello.request.nonce
                                || ack.operation != Some(actual.operation())
                            {
                                return Err(NativeError::Foreign);
                            }
                            peer.reverify(io, &io.admit_support(deadline)?, deadline)?;
                            image.reverify(io, &io.admit_support(deadline)?, deadline)?;
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

        /// Pre-reservation, read-only support admission. The actual live pipe peer must
        /// run the fixed Installer image freshly measured against THIS build's own module.
        /// No record, message, terminal latch or effect is needed to refuse an older owner.
        pub(crate) fn probe_source_repair_support(
            io: Arc<WindowsNativeIo>,
            lock: &super::super::super::InstallerLock,
            module: &super::super::super::SelfImagePin,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            use super::super::super::super::payload::inventory::{ApprovedPe, PayloadRole};
            let proof = io.admit_support(deadline)?;
            let expected = ApprovedPe::own_image(module)?;
            let image = io.payload_root(&proof, lock, deadline)?.open_approved(
                &io,
                &proof,
                PayloadRole::Installer,
                &expected,
                deadline,
            )?;
            let agent = io.observe_agent(&io.admit_support(deadline)?, deadline)?;
            if agent.bootstrap().phase != BootstrapPhase::Ready {
                return Err(NativeError::Unsupported);
            }
            let root = io.broker_admission(&io.admit_support(deadline)?, deadline)?;
            root.bind_runtime(&agent, &io.admit_support(deadline)?, deadline)?;
            runtime()?.block_on(async {
                let pipe = loop {
                    deadline.check()?;
                    match ClientOptions::new().open(format!("{}.outer", root.endpoint())) {
                        Ok(pipe) => break pipe,
                        Err(error) if error.raw_os_error() == Some(231) => {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                        Err(_) => return Err(NativeError::Unsupported),
                    }
                };
                let peer = Peer::admit(pipe.as_raw_handle(), true, &root, deadline)?;
                if image.identity() != root.installer_identity() {
                    return Err(NativeError::Foreign);
                }
                peer.reverify(&root, deadline)?;
                image.reverify(&io, &io.admit_support(deadline)?, deadline)?;
                module.reverify(&io, &io.admit_support(deadline)?, deadline)?;
                agent.revalidate(&io, &io.admit_support(deadline)?, deadline)?;
                // Connection closes without any request. This helper grants no effect authority.
                deadline.check()
            })
        }

        pub(crate) fn probe_repair_support(
            io: Arc<WindowsNativeIo>,
            proof: &SupportProof,
            operation: [u8; 16],
            deadline: &Deadline,
        ) -> NativeResult<()> {
            let selected = io.select_repair_keeper(proof, deadline)?;
            selected.reverify(&io, proof, deadline)?;
            if selected.operation() != operation || !support(selected.record()) {
                return Err(NativeError::Foreign);
            }
            let original = io.observe_agent(proof, deadline)?;
            if original.bootstrap().phase != BootstrapPhase::Ready {
                return Err(NativeError::Unavailable);
            }
            let generation = io.agent_generation(&original, proof, deadline)?;
            if generation != selected.record().original_generation() {
                return Err(NativeError::Foreign);
            }
            let root = io.broker_admission(proof, deadline)?;
            root.bind_runtime(&original, proof, deadline)?;
            let nonce = io.bridge_nonce(proof, deadline)?;
            runtime()?.block_on(async {
                let mut pipe = loop {
                    deadline.check()?;
                    match ClientOptions::new().open(format!("{}.outer", root.endpoint())) {
                        Ok(pipe) => break pipe,
                        Err(error) if error.raw_os_error() == Some(231) => {
                            tokio::time::sleep(Duration::from_millis(5)).await
                        }
                        Err(error) if error.raw_os_error() == Some(2) => {
                            return Err(NativeError::Unsupported);
                        }
                        Err(_) => return Err(NativeError::Unavailable),
                    }
                };
                // The original fixed source/kernel peer is authenticated BEFORE reply decoding.
                let peer = Peer::admit(pipe.as_raw_handle(), true, &root, deadline)?;
                selected.reverify(&io, &io.admit_support(deadline)?, deadline)?;
                let hello = RepairHello {
                    request: Request {
                        schema_version: 1,
                        nonce,
                        method: Method::Hello,
                        operation: None,
                        generation,
                    },
                    operation,
                    mode: RepairMode::ProbeRepair,
                };
                write_frame(&mut pipe, &hello, deadline).await?;
                let reply: SupportReply = read_frame(&mut pipe, deadline).await?;
                if reply.schema_version != 1
                    || reply.nonce != nonce
                    || reply.operation != operation
                    || reply.generation != generation
                {
                    return Err(NativeError::Foreign);
                }
                peer.reverify(&root, deadline)?;
                original.revalidate(&io, &io.admit_support(deadline)?, deadline)?;
                selected.reverify(&io, &io.admit_support(deadline)?, deadline)?;
                Ok(())
            })
        }

        fn ready_handles_match(
            peer: &Peer,
            supervisor: &OwnedHandle,
            child: &OwnedHandle,
            root: &BrokerAdmission,
            agent: &AgentObservation,
            generation: Generation,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            peer.reverify(root, deadline)?;
            let (supervisor_creation, supervisor_image) = process_facts(supervisor, true)?;
            let (child_creation, child_image) = process_facts(child, true)?;
            // SAFETY: this exact query/sync handle was duplicated from the authenticated
            // native owner's own retained supervisor object, never selected by a recorded PID.
            let supervisor_pid = unsafe { GetProcessId(supervisor.as_raw_handle()) };
            // SAFETY: this exact query/sync handle came from owner.child_for_peer, which
            // proves actual native child lineage and job membership before the reply.
            let child_pid = unsafe { GetProcessId(child.as_raw_handle()) };
            if supervisor_pid != peer.pid
                || supervisor_creation != peer.created
                || supervisor_image != peer.image
                || child_pid != generation.pid
                || child_creation != generation.creation
                || child_image
                    != super::super::super::process::literal_path(agent.image_canonical())?
                || identity::native::observe_process(supervisor)? != *root.token()
                || identity::native::observe_process(child)? != *root.token()
            {
                return Err(NativeError::Foreign);
            }
            deadline.check()
        }
        /// Ready-only proof of the newly admitted child through its actual native supervisor.
        /// No terminal latch, job export, Stop request, or completion capability is available.
        pub(crate) fn prove_repair_ready(
            io: Arc<WindowsNativeIo>,
            agent: &AgentObservation,
            payload: &RepairFixedPayload,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if !Arc::ptr_eq(&io, payload.io()) {
                return Err(NativeError::Foreign);
            }
            let proof = io.admit_support(deadline)?;
            let selected = io.select_repair_keeper(&proof, deadline)?;
            selected.reverify(&io, &proof, deadline)?;
            payload.reverify(&io, &proof, deadline)?;
            let record = fresh(&io, &proof, payload.operation(), deadline)?;
            if selected.operation() != payload.operation()
                || !selected.record().same_selection(&record)
            {
                return Err(NativeError::Foreign);
            }
            let generation = ready_generation(&io, &proof, &record, agent, deadline)?;
            if io.agent_identity(agent, &proof, deadline)? != payload.identity(PayloadRole::Agent) {
                return Err(NativeError::Foreign);
            }
            let root = io.broker_admission(&proof, deadline)?;
            root.bind_runtime(agent, &proof, deadline)?;
            let nonce = io.bridge_nonce(&proof, deadline)?;
            runtime()?.block_on(async {
                let mut pipe = loop {
                    deadline.check()?;
                    match ClientOptions::new().open(format!("{}.outer", root.endpoint())) {
                        Ok(pipe) => break pipe,
                        Err(error) if error.raw_os_error() == Some(231) => {
                            tokio::time::sleep(Duration::from_millis(5)).await
                        }
                        Err(error) if error.raw_os_error() == Some(2) => {
                            return Err(NativeError::Unsupported);
                        }
                        Err(_) => return Err(NativeError::Unavailable),
                    }
                };
                // The actual pipe server must be the freshly pinned installed installer role.
                // The old admitted caller-role factory is unchanged; only this private mode
                // authenticates the repair keeper at the server's fixed repair-role gate.
                let peer = Peer::admit(pipe.as_raw_handle(), true, &root, deadline)?;
                let hello = RepairHello {
                    request: Request {
                        schema_version: 1,
                        nonce,
                        method: Method::Hello,
                        operation: None,
                        generation,
                    },
                    operation: payload.operation(),
                    mode: RepairMode::ProveReady,
                };
                write_frame(&mut pipe, &hello, deadline).await?;
                let reply: Reply = read_frame(&mut pipe, deadline).await?;
                check_reply(&reply, &hello.request)?;
                if reply.status != ReplyStatus::Ready {
                    return Err(NativeError::Unavailable);
                }
                peer.reverify(&root, deadline)?;
                let supervisor = peer.duplicate(
                    reply.process.ok_or(NativeError::Foreign)?,
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                    deadline,
                )?;
                let child = peer.duplicate(
                    reply.child.ok_or(NativeError::Foreign)?,
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                    deadline,
                )?;
                ready_handles_match(
                    &peer,
                    &supervisor,
                    &child,
                    &root,
                    agent,
                    generation,
                    deadline,
                )?;
                let proof = io.admit_support(deadline)?;
                selected.reverify(&io, &proof, deadline)?;
                payload.reverify(&io, &proof, deadline)?;
                let current = fresh(&io, &proof, payload.operation(), deadline)?;
                if !current.same_selection(&record)
                    || ready_generation(&io, &proof, &current, agent, deadline)? != generation
                {
                    return Err(NativeError::Foreign);
                }
                let ack = Request {
                    schema_version: 1,
                    nonce,
                    method: Method::Ack,
                    operation: Some(payload.operation()),
                    generation,
                };
                let ack_reply = exchange(&mut pipe, &ack, deadline).await?;
                if ack_reply.status != ReplyStatus::Ready {
                    return Err(NativeError::Foreign);
                }
                ready_handles_match(
                    &peer,
                    &supervisor,
                    &child,
                    &root,
                    agent,
                    generation,
                    deadline,
                )?;
                write_frame(&mut pipe, &ack, deadline).await?;
                // An Ack alone is insufficient: positively observe the actual server-side
                // Disconnect before dropping our endpoint and reporting ready-only success.
                let mut byte = [0u8; 1];
                let mut buffer = ReadBuf::new(&mut byte);
                let closed = tokio::time::timeout(
                    Duration::from_millis(deadline.remaining_ms()?),
                    std::future::poll_fn(|cx| {
                        std::pin::Pin::new(&mut pipe).poll_read(cx, &mut buffer)
                    }),
                )
                .await
                .map_err(|_| NativeError::OutcomeUnknown)?;
                match closed {
                    Ok(()) if buffer.filled().is_empty() => {}
                    Err(error)
                        if matches!(error.raw_os_error(),
                        Some(code) if code == ERROR_BROKEN_PIPE as i32
                            || code == ERROR_PIPE_NOT_CONNECTED as i32) => {}
                    _ => return Err(NativeError::OutcomeUnknown),
                }
                drop(pipe);
                ready_handles_match(
                    &peer,
                    &supervisor,
                    &child,
                    &root,
                    agent,
                    generation,
                    deadline,
                )?;
                let proof = io.admit_support(deadline)?;
                selected.reverify(&io, &proof, deadline)?;
                payload.reverify(&io, &proof, deadline)?;
                let current = fresh(&io, &proof, payload.operation(), deadline)?;
                if !current.same_selection(&record)
                    || ready_generation(&io, &proof, &current, agent, deadline)? != generation
                {
                    return Err(NativeError::Foreign);
                }
                // All imported objects are read-only and released locally; no global owner,
                // lease, terminal bit, or job handle was created by this readiness exchange.
                drop(child);
                drop(supervisor);
                deadline.check()
            })
        }

        enum ControlRole {
            Keeper {
                process: Arc<OwnedHandle>,
                identity: super::super::super::files::FileIdentity,
            },
            Source {
                parent: Arc<OwnedHandle>,
                own: super::super::super::SelfImagePin,
                selection: Arc<super::super::super::RepairKeeperSelection>,
            },
        }
        /// Authentication of the real preparation/commit endpoint, never a serialized permit.
        pub(crate) struct RepairControlPeer {
            io: Arc<WindowsNativeIo>,
            peer: KernelOuterPeer,
            image: super::super::super::OuterPeerImage,
            operation: [u8; 16],
            role: ControlRole,
        }
        impl RepairControlPeer {
            pub(crate) fn reverify(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                if !std::ptr::eq(io, self.io.as_ref()) {
                    return Err(NativeError::Foreign);
                }
                self.peer.reverify(io, proof, deadline)?;
                self.image.reverify(io, proof, deadline)?;
                let record = fresh(io, proof, self.operation, deadline)?;
                // This is control-channel authentication only. Later same-owner Status has no
                // Arm/Stop/Run or completion authority; every destructive method has its own gate.
                if record.phase() == PayloadRepairPhase::Retired {
                    return Err(NativeError::Foreign);
                }
                match &self.role {
                    ControlRole::Keeper { process, identity } => {
                        let (creation, path) = process_facts(process, true)?;
                        // SAFETY: originally created child object retained by the actual creator.
                        let pid = unsafe { GetProcessId(process.as_raw_handle()) };
                        if self.peer.pid != pid
                            || self.peer.created != creation
                            || self.peer.image != path
                            || self.image.identity() != *identity
                        {
                            return Err(NativeError::Foreign);
                        }
                        let copy = record.keeper().image().ok_or(NativeError::Foreign)?;
                        let child = record.keeper().child().ok_or(NativeError::Foreign)?;
                        if copy.identity != stamp(*identity)
                            || copy.facts != *self.image.facts()
                            || child.pid() != pid
                            || child.creation() != creation
                        {
                            return Err(NativeError::Foreign);
                        }
                    }
                    ControlRole::Source {
                        parent,
                        own,
                        selection,
                    } => {
                        own.reverify(io, proof, deadline)?;
                        selection.reverify(io, proof, deadline)?;
                        if selection.operation() != self.operation
                            || selection.module().identity() != own.identity()
                            || selection.module().facts() != own.facts()
                        {
                            return Err(NativeError::Foreign);
                        }
                        let (creation, path) = process_facts(parent, true)?;
                        // SAFETY: the exact explicitly inherited original parent handle only.
                        let pid = unsafe { GetProcessId(parent.as_raw_handle()) };
                        if self.peer.pid != pid
                            || self.peer.created != creation
                            || self.peer.image != path
                            || self.image.facts() != own.facts()
                            || stamp(self.image.identity())
                                != record.selection().own_module().identity
                            || pid != record.selection().source_process().pid()
                            || creation != record.selection().source_process().creation()
                        {
                            return Err(NativeError::Foreign);
                        }
                    }
                }
                deadline.check()
            }
        }
        // Signed additive admission keeps original IO, context, actual object and deadline
        // separate; restructuring would obscure the sealed authority inputs.
        #[allow(clippy::too_many_arguments)]
        pub(crate) fn admit_repair_keeper_peer(
            pipe: std::os::windows::io::RawHandle,
            is_server: bool,
            io: Arc<WindowsNativeIo>,
            proof: &SupportProof,
            process: &Arc<OwnedHandle>,
            image: &super::super::super::OpenedPe,
            operation: [u8; 16],
            deadline: &Deadline,
        ) -> NativeResult<RepairControlPeer> {
            image.reverify(&io, proof, deadline)?;
            let peer = KernelOuterPeer::admit(pipe, is_server, io.clone(), proof, deadline)?;
            let opened = io.pin_outer_kernel_image(
                proof,
                &peer,
                &image.approved().facts().version,
                deadline,
            )?;
            if opened.identity() != image.identity()
                || opened.facts() != image.approved().facts()
                || opened.canonical_dos_path() != image.canonical_dos_path()
            {
                return Err(NativeError::Foreign);
            }
            let result = RepairControlPeer {
                io,
                peer,
                image: opened,
                operation,
                role: ControlRole::Keeper {
                    process: process.clone(),
                    identity: image.identity(),
                },
            };
            result.reverify(&result.io, proof, deadline)?;
            Ok(result)
        }
        // Signed additive admission keeps original IO, context, actual object and deadline
        // separate; restructuring would obscure the sealed authority inputs.
        #[allow(clippy::too_many_arguments)]
        pub(crate) fn admit_repair_source_peer(
            pipe: std::os::windows::io::RawHandle,
            io: Arc<WindowsNativeIo>,
            proof: &SupportProof,
            parent: &super::super::super::super::payload::helper::repair::RepairParent,
            own: super::super::super::SelfImagePin,
            selection: Arc<super::super::super::RepairKeeperSelection>,
            operation: [u8; 16],
            deadline: &Deadline,
        ) -> NativeResult<RepairControlPeer> {
            parent.reverify(&io, proof, operation, deadline)?;
            own.reverify(&io, proof, deadline)?;
            let peer = KernelOuterPeer::admit(pipe, false, io.clone(), proof, deadline)?;
            let image = io.pin_outer_kernel_image(proof, &peer, &own.facts().version, deadline)?;
            let result = RepairControlPeer {
                io,
                peer,
                image,
                operation,
                role: ControlRole::Source {
                    parent: parent.retained_process().clone(),
                    own,
                    selection,
                },
            };
            result.reverify(&result.io, proof, deadline)?;
            Ok(result)
        }

        struct RepairTransfer {
            original: Arc<Transfer>,
            admission: RepairCompletionAdmission,
            completion: Mutex<Option<Arc<RetainedTreeCompletion>>>,
            arm_attempted: AtomicBool,
            complete_attempted: AtomicBool,
        }
        static REPAIR_TRANSFER: OnceLock<Mutex<Option<Arc<RepairTransfer>>>> = OnceLock::new();
        pub(crate) struct AdmittedRepairOwner {
            inner: Option<AdmittedSupervisorOwner>,
            sidecar: Option<Arc<RepairTransfer>>,
        }
        impl AdmittedRepairOwner {
            pub(crate) fn admit(
                io: Arc<WindowsNativeIo>,
                original: AgentObservation,
                lease: Arc<super::super::super::StopLockLease>,
                admission: RepairCompletionAdmission,
                deadline: &Deadline,
            ) -> NativeResult<Self> {
                let proof = io.admit_support(deadline)?;
                admission.reverify(&io, &proof, deadline)?;
                lease.reverify(&proof, deadline)?;
                let root = io.broker_admission(&proof, deadline)?;
                root.bind_runtime(&original, &proof, deadline)?;
                let selected = io.agent_generation(&original, &proof, deadline)?;
                let record = fresh(&io, &proof, admission.operation(), deadline)?;
                if selected != record.original_generation() {
                    return Err(NativeError::Foreign);
                }
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
                    lease: Mutex::new(Some(lease)),
                    retired: AtomicBool::new(false),
                    thread: Mutex::new(None),
                });
                let held = Arc::new(RepairTransfer {
                    original: state.clone(),
                    admission,
                    completion: Mutex::new(None),
                    arm_attempted: AtomicBool::new(false),
                    complete_attempted: AtomicBool::new(false),
                });
                {
                    let mut current = TRANSFER
                        .get_or_init(|| Mutex::new(None))
                        .lock()
                        .map_err(|_| NativeError::OutcomeUnknown)?;
                    let mut repair = REPAIR_TRANSFER
                        .get_or_init(|| Mutex::new(None))
                        .lock()
                        .map_err(|_| NativeError::OutcomeUnknown)?;
                    if current.is_some() || repair.is_some() {
                        return Err(NativeError::Busy);
                    }
                    *current = Some(state.clone());
                    *repair = Some(held.clone());
                }
                let (send, commands) = std::sync::mpsc::sync_channel(1);
                let (ready, receive) = std::sync::mpsc::sync_channel(1);
                let captured = held.clone();
                let initial = deadline.clone();
                let thread = std::thread::Builder::new()
                    .name("crosspane-repair-original-owner".into())
                    .spawn(move || {
                        let state = &captured.original;
                        let result = (|| {
                            let rt = runtime()?;
                            let pipe =
                                rt.block_on(connect(state, &captured.admission, &initial))?;
                            ready
                                .send(Ok(()))
                                .map_err(|_| NativeError::OutcomeUnknown)?;
                            // Shared actual original-handle/job import mechanics; no upgrade selection.
                            command_loop(state, &rt, pipe, commands)
                        })();
                        if result.is_err() {
                            state.retired.store(true, Ordering::Release);
                            let _ = ready.try_send(Err(NativeError::OutcomeUnknown));
                        }
                    })
                    .map_err(|_| NativeError::OutcomeUnknown)?;
                *state
                    .thread
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)? = Some(thread);
                match receive.recv_timeout(Duration::from_millis(deadline.remaining_ms()?)) {
                    Ok(Ok(())) if deadline.check().is_ok() => Ok(Self {
                        inner: Some(AdmittedSupervisorOwner { state, send }),
                        sidecar: Some(held),
                    }),
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
                let held = self.sidecar.as_ref().ok_or(NativeError::Foreign)?;
                if operation != held.admission.operation() {
                    return Err(NativeError::Foreign);
                }
                if held.arm_attempted.swap(true, Ordering::AcqRel) {
                    return Err(NativeError::OutcomeUnknown);
                }
                held.admission.reverify(
                    held.original.root.io(),
                    &held.original.root.io().admit_support(deadline)?,
                    deadline,
                )?;
                self.inner
                    .as_ref()
                    .ok_or(NativeError::Foreign)?
                    .arm_terminal(operation, deadline)
            }
            pub(crate) fn observe_completion(
                &self,
                operation: [u8; 16],
                deadline: &Deadline,
            ) -> NativeResult<()> {
                let held = self.sidecar.as_ref().ok_or(NativeError::Foreign)?;
                if operation != held.admission.operation() {
                    return Err(NativeError::Foreign);
                }
                if held.complete_attempted.swap(true, Ordering::AcqRel) {
                    return Err(NativeError::OutcomeUnknown);
                }
                let inner = self.inner.as_ref().ok_or(NativeError::Foreign)?;
                let (send, recv) = std::sync::mpsc::sync_channel(1);
                inner
                    .send
                    .try_send(Command::Complete(operation, deadline.clone(), send))
                    .map_err(|_| NativeError::OutcomeUnknown)?;
                let tree = Arc::new(inner.receive(recv, deadline)?);
                tree.reverify(deadline)?;
                // Preserve the actual native result BEFORE any later settlement failure.
                *held
                    .completion
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)? = Some(tree);
                inner.settle(deadline)
            }
            pub(crate) fn finish_settled(
                &mut self,
                deadline: &Deadline,
            ) -> NativeResult<Option<Arc<RetainedTreeCompletion>>> {
                let held = self.sidecar.as_ref().ok_or(NativeError::Foreign)?;
                let state = &held.original;
                let finished = state
                    .thread
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?
                    .as_ref()
                    .is_some_and(|thread| {
                        // SAFETY: exact actual owned worker thread; native exit includes TLS cleanup.
                        (unsafe { WaitForSingleObject(thread.as_raw_handle(), 0) }) == WAIT_OBJECT_0
                    });
                if !finished {
                    deadline.check()?;
                    return Ok(None);
                }
                let existing = held
                    .completion
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?
                    .as_ref()
                    .cloned();
                let tree = if let Some(tree) = existing {
                    tree
                } else {
                    held.admission.reverify(
                        state.root.io(),
                        &state.root.io().admit_support(deadline)?,
                        deadline,
                    )?;
                    let peer = state
                        .peer
                        .lock()
                        .map_err(|_| NativeError::OutcomeUnknown)?
                        .as_ref()
                        .cloned()
                        .ok_or(NativeError::OutcomeUnknown)?;
                    if !peer.exited()? {
                        return Ok(None);
                    }
                    let original = state
                        .original
                        .lock()
                        .map_err(|_| NativeError::OutcomeUnknown)?
                        .as_ref()
                        .cloned()
                        .ok_or(NativeError::OutcomeUnknown)?;
                    match original.observe_exit(
                        state.root.io(),
                        &state.root.io().admit_support(deadline)?,
                        deadline,
                    )? {
                        ExitObservation::Exited {
                            creation,
                            code: 0,
                            receipt: Some(receipt),
                        } if creation == state.selected.creation
                            && receipt.instance_id == state.selected.instance
                            && receipt.stopped_unix_ms >= original.bootstrap().started_unix_ms
                            && receipt.clean
                            && receipt.input_journals_empty
                            && receipt.audio_stopped => {}
                        ExitObservation::Running => return Ok(None),
                        _ => return Err(NativeError::OutcomeUnknown),
                    }
                    let handles = state
                        .handles
                        .lock()
                        .map_err(|_| NativeError::OutcomeUnknown)?;
                    let tree = Arc::new(RetainedTreeCompletion {
                        operation: held.admission.operation(),
                        selected: state.selected,
                        supervisor: handles
                            .supervisor
                            .as_ref()
                            .cloned()
                            .ok_or(NativeError::OutcomeUnknown)?,
                        child: handles
                            .child
                            .as_ref()
                            .cloned()
                            .ok_or(NativeError::OutcomeUnknown)?,
                        job: handles
                            .job
                            .as_ref()
                            .cloned()
                            .ok_or(NativeError::OutcomeUnknown)?,
                    });
                    tree.reverify(deadline)?;
                    *held
                        .completion
                        .lock()
                        .map_err(|_| NativeError::OutcomeUnknown)? = Some(tree.clone());
                    tree
                };
                tree.reverify(deadline)?;
                // The service MUST physically settle its ONE agent transport before calling this.
                // Acquire every fallible guard and verify the final proof BEFORE irreversible
                // slot consumption. Errors leave both the worker handle and actual proof retained.
                let mut slot = TRANSFER
                    .get_or_init(|| Mutex::new(None))
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?;
                if slot
                    .as_ref()
                    .is_some_and(|value| !Arc::ptr_eq(value, state))
                {
                    return Err(NativeError::Foreign);
                }
                let mut side = REPAIR_TRANSFER
                    .get_or_init(|| Mutex::new(None))
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?;
                if !side.as_ref().is_some_and(|value| Arc::ptr_eq(value, held))
                    || Arc::strong_count(held) != 2
                {
                    return Err(NativeError::OutcomeUnknown);
                }
                let mut peer = state.peer.lock().map_err(|_| NativeError::OutcomeUnknown)?;
                let mut original = state
                    .original
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?;
                let mut lease = state
                    .lease
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?;
                let mut thread = state
                    .thread
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?;
                state.root.release_image_pin()?;
                tree.reverify(deadline)?;
                // No fallible work follows this same-object alias drop barrier.
                peer.take();
                original.take();
                lease.take();
                thread.take();
                slot.take();
                side.take();
                drop(peer);
                drop(original);
                drop(lease);
                drop(thread);
                drop(slot);
                drop(side);
                self.inner.take();
                self.sidecar.take();
                // No Transfer/Broker/image/install anchor is contained in the returned bare proof.
                Ok(Some(tree))
            }
        }

        async fn connect(
            state: &Transfer,
            admission: &RepairCompletionAdmission,
            deadline: &Deadline,
        ) -> NativeResult<NamedPipeClient> {
            let mut pipe = loop {
                deadline.check()?;
                match ClientOptions::new().open(format!("{}.outer", state.root.endpoint())) {
                    Ok(pipe) => break pipe,
                    Err(error) if error.raw_os_error() == Some(231) => {
                        tokio::time::sleep(Duration::from_millis(5)).await
                    }
                    Err(error) if error.raw_os_error() == Some(2) => {
                        return Err(NativeError::Unsupported);
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
            *state.peer.lock().map_err(|_| NativeError::OutcomeUnknown)? = Some(peer.clone());
            admission.reverify(
                state.root.io(),
                &state.root.io().admit_support(deadline)?,
                deadline,
            )?;
            let request = Request {
                schema_version: 1,
                nonce: state.nonce,
                method: Method::Hello,
                operation: None,
                generation: state.selected,
            };
            let hello = RepairHello {
                request,
                operation: admission.operation(),
                mode: RepairMode::CompleteRepair,
            };
            write_frame(&mut pipe, &hello, deadline).await?;
            let reply: Reply = read_frame(&mut pipe, deadline).await?;
            check_reply(&reply, &hello.request)?;
            if reply.status != ReplyStatus::Ready {
                return Err(NativeError::Unavailable);
            }
            let supervisor = Arc::new(peer.duplicate(
                reply.process.ok_or(NativeError::Foreign)?,
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                deadline,
            )?);
            state
                .handles
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?
                .supervisor = Some(supervisor.clone());
            let child = Arc::new(peer.duplicate(
                reply.child.ok_or(NativeError::Foreign)?,
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                deadline,
            )?);
            state
                .handles
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?
                .child = Some(child.clone());
            let (creation, _) = process_facts(&supervisor, true)?;
            // SAFETY: query only the SAME retained authenticated supervisor kernel object.
            let supervisor_pid = unsafe { GetProcessId(supervisor.as_raw_handle()) };
            // SAFETY: query only the SAME exact original child imported from its genuine owner.
            let child_pid = unsafe { GetProcessId(child.as_raw_handle()) };
            if supervisor_pid != peer.pid
                || creation != peer.created
                || child_pid != state.selected.pid
            {
                return Err(NativeError::Foreign);
            }
            let (created, image) = process_facts(&child, true)?;
            let original = state
                .original
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?
                .as_ref()
                .cloned()
                .ok_or(NativeError::Foreign)?;
            if created != state.selected.creation
                || image != super::super::super::process::literal_path(original.image_canonical())?
                || identity::native::observe_process(&child)? != *state.root.token()
            {
                return Err(NativeError::Foreign);
            }
            original.revalidate(
                state.root.io(),
                &state.root.io().admit_support(deadline)?,
                deadline,
            )?;
            peer.reverify(&state.root, deadline)?;
            Ok(pipe)
        }
    }
    #[cfg(not(test))]
    pub(crate) use repair_owner::{
        AdmittedRepairOwner, RepairControlPeer, admit_repair_keeper_peer, admit_repair_source_peer,
        probe_repair_support, probe_source_repair_support, prove_repair_ready,
    };

    #[cfg(not(test))]
    pub(crate) use removal_owner::{
        AdmittedRemovalOwner, RemovalControlPeer, admit_removal_keeper_peer,
        admit_removal_source_peer, probe_removal_support,
    };

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
#[cfg(all(windows, not(test)))]
pub(crate) use native::{
    AdmittedRemovalOwner, AdmittedRepairOwner, KernelOuterPeer, OuterOwnerServer, OuterPeerPin,
    RemovalControlPeer, RepairControlPeer, admit_keeper_observer_server, admit_keeper_peer,
    admit_outer_observer_peer, admit_outer_source_peer, admit_removal_keeper_peer,
    admit_removal_source_peer, admit_repair_keeper_peer, admit_repair_source_peer,
    probe_outer_support, probe_removal_support, probe_repair_support, probe_source_repair_support,
    prove_repair_ready,
};

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
