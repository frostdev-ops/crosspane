//! The agent's named-pipe client: `call`, `installer_stop`, and the GUI's `AgentPort`.
//!
//! One connection per call. The server is verified before a byte is sent: its process image must
//! be the installed agent and its token must carry this user and logon. Nothing here records
//! anything, and no earlier call or failure gates a later one.

use super::{
    identity::{self, Sid},
    paths::{self, Paths, PayloadFile},
    setup_log::setup_note,
};
use crate::agent_contract::{
    AgentCall, AgentPlatform, AgentPort, AgentReply, CallFailure, ContractError, DecodedReply,
    InstallerRequest, MAX_QUEUE, MAX_RESPONSE_BYTES, MAX_TIMEOUT_MS, ObservationSource,
    decode_installer_stop, decode_reply, encode_installer_stop, encode_request,
};
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{
        ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, ERROR_PIPE_BUSY, GetLastError, INVALID_HANDLE_VALUE,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ, FILE_WRITE_DATA, OPEN_EXISTING,
        ReadFile, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, SYNCHRONIZE, WriteFile,
    },
    System::{
        IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
        Pipes::{GetNamedPipeServerProcessId, WaitNamedPipeW},
        Threading::{
            CreateEventW, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW, WaitForSingleObject,
        },
    },
};

/// How often `wait_until_answering` asks.
const POLL_EVERY: Duration = Duration::from_millis(500);
/// Longest single wait on a busy pipe or a pending transfer before the deadline is looked at again.
const SLICE: Duration = Duration::from_millis(250);
/// The room one read takes from the response.
const READ_CHUNK: usize = 8192;
/// An image path can be up to 32 767 UTF-16 units (long path support).
const IMAGE_UNITS: usize = 32_768;

/// One decoded reply with its raw bytes and the pid of the server that sent it.
#[derive(Debug)]
pub(crate) struct Answer {
    pub reply: DecodedReply,
    pub raw: Vec<u8>,
    pub server_pid: u32,
}

impl Answer {
    /// `raw["result"]["installer"]["instance"]["id"]` (works for Pending replies too).
    pub(crate) fn instance(&self) -> Option<u64> {
        let value: serde_json::Value = serde_json::from_slice(&self.raw).ok()?;
        value
            .get("result")?
            .get("installer")?
            .get("instance")?
            .get("id")?
            .as_u64()
    }
}

/// The located pipe of this user's installed agent.
#[derive(Debug)]
pub(crate) struct AgentPipe {
    name: Vec<u16>,
    agent: PathBuf,
    user: Sid,
    logon: Sid,
}

impl AgentPipe {
    /// `None` if the runtime dir is missing or the identity is unreadable. Name = xxh3_128(user
    /// SDDL, logon SDDL, `fs::canonicalize(runtime)`) exactly as the agent's `security.rs:763-785`.
    pub(crate) fn locate(paths: &Paths) -> Option<Self> {
        let identity = identity::native::current().ok()?;
        let facts = identity.facts();
        // The agent hashes `fs::canonicalize(runtime_dir)` as it comes back, `\\?\` prefix and all.
        let runtime = std::fs::canonicalize(&paths.runtime).ok()?;
        let name = endpoint_name(&facts.user.sddl(), &facts.logon.sddl(), runtime.as_os_str());
        Some(Self {
            name: name.encode_utf16().chain(std::iter::once(0)).collect(),
            agent: paths.file(PayloadFile::Agent),
            user: facts.user.clone(),
            logon: facts.logon.clone(),
        })
    }

    /// Connect (SQOS identification), verify the server PID's image == `paths.file(Agent)` and its
    /// token user + logon == ours, send one request line, read one response line; timeout ≤
    /// `MAX_TIMEOUT_MS`.
    pub(crate) fn call(
        &self,
        request: &InstallerRequest,
        timeout: Duration,
    ) -> Result<Answer, CallFailure> {
        let deadline = Deadline::after(timeout)?;
        self.call_by(request, &deadline)
    }

    /// Status → instance → `encode_installer_stop` → `decode_installer_stop`. Returns the server PID.
    pub(crate) fn installer_stop(&self, timeout: Duration) -> Result<u32, CallFailure> {
        let deadline = Deadline::after(timeout)?;
        // The agent only stops the instance the caller names, so ask which one is running.
        let status = self.call_by(&InstallerRequest::Status, &deadline)?;
        let instance = status.instance().ok_or(CallFailure::InvalidResponse)?;
        let request = encode_installer_stop(instance).map_err(CallFailure::InvalidCall)?;
        let (reply, server_pid) = self.exchange(&request, &deadline)?;
        decode_installer_stop(&reply)?;
        Ok(server_pid)
    }

    fn call_by(
        &self,
        request: &InstallerRequest,
        deadline: &Deadline,
    ) -> Result<Answer, CallFailure> {
        let bytes = encode_request(request).map_err(CallFailure::InvalidCall)?;
        let (raw, server_pid) = self.exchange(&bytes, deadline)?;
        let reply = decode_reply(request, &raw, AgentPlatform::Windows)?;
        Ok(Answer {
            reply,
            raw,
            server_pid,
        })
    }

    /// One verified connection: write `request`, read one line. Returns the line and the server pid.
    fn exchange(&self, request: &[u8], deadline: &Deadline) -> Result<(Vec<u8>, u32), CallFailure> {
        // Nothing has been sent yet, so every failure up to here is a plain "not available".
        let connection = self.connect(deadline)?;
        let pipe = &connection.pipe;
        // From the first byte on, a failure leaves it unknown whether the agent acted.
        let mut sent = 0;
        while sent < request.len() {
            let mut operation = Operation::new(pipe, request[sent..].to_vec())
                .map_err(|_| CallFailure::TimeoutOutcomeUnknown)?;
            let count = operation
                .transfer(true, deadline)
                .map_err(|_| CallFailure::TimeoutOutcomeUnknown)?;
            if count == 0 || count > request.len() - sent {
                return Err(CallFailure::TimeoutOutcomeUnknown);
            }
            sent += count;
        }
        let mut response = Response::new();
        loop {
            let mut operation = Operation::new(pipe, vec![0; READ_CHUNK])
                .map_err(|_| CallFailure::TimeoutOutcomeUnknown)?;
            let count = operation
                .transfer(false, deadline)
                .map_err(|_| CallFailure::TimeoutOutcomeUnknown)?;
            if count == 0 {
                return Err(CallFailure::InvalidResponse);
            }
            match response.append(&operation.bytes[..count]) {
                Ok(Some(line)) => return Ok((line, connection.pid)),
                Ok(None) => {}
                Err(_) => return Err(CallFailure::InvalidResponse),
            }
        }
    }

    /// Open the pipe (waiting out `ERROR_PIPE_BUSY` within the deadline) and verify the server.
    fn connect(&self, deadline: &Deadline) -> Result<Connection, CallFailure> {
        loop {
            let remaining = deadline.remaining().ok_or(CallFailure::Unavailable)?;
            // SAFETY: `name` is NUL-terminated and lives through the call. The handle is
            // overlapped, non-inheritable (null security attributes) and opened with
            // identification-only SQOS, so the server cannot impersonate this process. The
            // individual write right avoids FILE_CREATE_PIPE_INSTANCE from GENERIC_WRITE.
            let raw = unsafe {
                CreateFileW(
                    self.name.as_ptr(),
                    FILE_GENERIC_READ | FILE_WRITE_DATA | SYNCHRONIZE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    std::ptr::null_mut(),
                )
            };
            if raw == INVALID_HANDLE_VALUE {
                // SAFETY: reads the thread-local error of the CreateFileW that just failed.
                if unsafe { GetLastError() } != ERROR_PIPE_BUSY {
                    return Err(CallFailure::Unavailable);
                }
                let wait = remaining.min(SLICE).as_millis().max(1) as u32;
                // SAFETY: `name` is NUL-terminated and live. The result is not needed: the loop
                // opens the pipe again and re-checks the deadline either way.
                unsafe { WaitNamedPipeW(self.name.as_ptr(), wait) };
                continue;
            }
            // SAFETY: the successful CreateFileW transferred exactly one owned pipe handle.
            let pipe = unsafe { OwnedHandle::from_raw_handle(raw) };
            let (process, pid) = self.verify_server(&pipe)?;
            return Ok(Connection {
                pipe,
                _process: process,
                pid,
            });
        }
    }

    /// The server must be the installed agent, running as this user and logon. Returns its open
    /// process handle, which keeps the pid from being reused while the exchange runs.
    fn verify_server(&self, pipe: &OwnedHandle) -> Result<(OwnedHandle, u32), CallFailure> {
        let mut pid = 0;
        // SAFETY: `pipe` is a live connected client handle and `pid` is a writable output.
        if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } == 0 || pid == 0 {
            return Err(CallFailure::Unavailable);
        }
        // SAFETY: query-only, non-inheritable open of the pid the connected pipe reported.
        let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if raw.is_null() {
            return Err(CallFailure::Unavailable);
        }
        // SAFETY: the successful OpenProcess transferred exactly one owned handle.
        let process = unsafe { OwnedHandle::from_raw_handle(raw) };
        let is_agent = process_image(&process).is_some_and(|image| is_agent(&image, &self.agent));
        let ours = identity::native::observe_process(&process)
            .is_ok_and(|facts| facts.user == self.user && facts.logon == self.logon);
        if !is_agent || !ours {
            setup_note!("agent pipe: the server is not this user's installed agent");
            return Err(CallFailure::Unavailable);
        }
        Ok((process, pid))
    }
}

/// Poll Status every 500 ms until `Ok(Status(_))` or `within`; never errors.
pub(crate) fn wait_until_answering(paths: &Paths, within: Duration) -> Option<Answer> {
    let end = Instant::now().checked_add(within)?;
    loop {
        let attempt = Instant::now();
        // Out of time: the attempt below would have no room to run.
        let remaining = end
            .checked_duration_since(attempt)
            .filter(|left| !left.is_zero())?;
        if let Some(pipe) = AgentPipe::locate(paths)
            && let Ok(answer) = pipe.call(&InstallerRequest::Status, remaining)
            && matches!(answer.reply, DecodedReply::Status(_))
        {
            return Some(answer);
        }
        let left = end.saturating_duration_since(Instant::now());
        std::thread::sleep(POLL_EVERY.saturating_sub(attempt.elapsed()).min(left));
    }
}

/// The agent's pipe name: three NUL-terminated UTF-16LE fields hashed with xxh3-128. Byte for
/// byte the agent's `endpoint_for` (`crosspane-agent/src/windows/security.rs`).
fn endpoint_name(user: &str, logon: &str, runtime: &OsStr) -> String {
    let mut bytes = Vec::new();
    for text in [OsStr::new(user), OsStr::new(logon), runtime] {
        for unit in text.encode_wide() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.extend_from_slice(&0u16.to_le_bytes());
    }
    format!(
        r"\\.\pipe\Crosspane-{:032x}",
        xxhash_rust::xxh3::xxh3_128(&bytes)
    )
}

/// Whether `image` (as the process reports it) is the installed agent, by spelling or, failing
/// that, by what both resolve to.
fn is_agent(image: &Path, agent: &Path) -> bool {
    paths::same_path(image, agent)
        || matches!(
            (std::fs::canonicalize(image), std::fs::canonicalize(agent)),
            (Ok(a), Ok(b)) if paths::same_path(&a, &b)
        )
}

/// The Win32 image path of an open process (`QueryFullProcessImageNameW`).
fn process_image(process: &OwnedHandle) -> Option<PathBuf> {
    let mut units = vec![0u16; IMAGE_UNITS];
    let mut length = units.len() as u32;
    // SAFETY: `process` is a live handle with query rights; `units` is writable for `length`
    // units, and `length` is its in/out capacity.
    let ok = unsafe {
        QueryFullProcessImageNameW(process.as_raw_handle(), 0, units.as_mut_ptr(), &mut length)
    };
    if ok == 0 || length == 0 || length as usize >= units.len() {
        return None;
    }
    Some(PathBuf::from(OsString::from_wide(
        &units[..length as usize],
    )))
}

/// A connected, verified pipe plus the server process handle that pins its pid.
#[derive(Debug)]
struct Connection {
    pipe: OwnedHandle,
    /// Held only so the pid cannot be reused while the exchange runs.
    _process: OwnedHandle,
    pid: u32,
}

/// The time one call may take: the caller's timeout, never more than `MAX_TIMEOUT_MS`.
#[derive(Clone, Copy, Debug)]
struct Deadline {
    end: Instant,
}

impl Deadline {
    fn after(timeout: Duration) -> Result<Self, CallFailure> {
        let capped = timeout.min(Duration::from_millis(MAX_TIMEOUT_MS));
        let end = Instant::now()
            .checked_add(capped)
            .filter(|_| !capped.is_zero())
            .ok_or(CallFailure::InvalidCall(ContractError::InvalidDeadline))?;
        Ok(Self { end })
    }

    /// The time left, or `None` once it has run out.
    fn remaining(&self) -> Option<Duration> {
        self.end
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
    }
}

/// One response line, assembled from reads.
#[derive(Debug)]
struct Response {
    bytes: Vec<u8>,
}

impl Response {
    fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    /// Adds a chunk. The whole line (with its newline) comes back once it is complete; bytes after
    /// the newline, or a response over the size limit, are an error.
    fn append(&mut self, bytes: &[u8]) -> Result<Option<Vec<u8>>, IoFailed> {
        if self
            .bytes
            .len()
            .checked_add(bytes.len())
            .is_none_or(|n| n > MAX_RESPONSE_BYTES)
        {
            return Err(IoFailed);
        }
        self.bytes.extend_from_slice(bytes);
        if let Some(end) = self.bytes.iter().position(|b| *b == b'\n') {
            if end + 1 != self.bytes.len() {
                return Err(IoFailed);
            }
            return Ok(Some(std::mem::take(&mut self.bytes)));
        }
        Ok(None)
    }
}

/// A pipe transfer that did not complete (timeout, cancellation or an OS error).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IoFailed;

/// Every in-flight buffer/OVERLAPPED/event remains owned until actual kernel completion.
/// Cancelling a caller is not cancelling/freeing kernel I/O. Drop also drains before freeing.
struct Operation<'a> {
    pipe: &'a OwnedHandle,
    event: OwnedHandle,
    overlap: Box<OVERLAPPED>,
    bytes: Vec<u8>,
    pending: bool,
}

impl<'a> Operation<'a> {
    fn new(pipe: &'a OwnedHandle, bytes: Vec<u8>) -> Result<Self, IoFailed> {
        // SAFETY: unnamed manual-reset event, non-inheritable, owned only by this operation.
        let raw = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if raw.is_null() {
            return Err(IoFailed);
        }
        // SAFETY: successful event creation transferred exactly one owned handle.
        let event = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut overlap = Box::new(OVERLAPPED::default());
        overlap.hEvent = event.as_raw_handle();
        Ok(Self {
            pipe,
            event,
            overlap,
            bytes,
            pending: false,
        })
    }

    /// One write (all of `bytes`) or one read (up to `bytes.len()`), bounded by the deadline.
    fn transfer(&mut self, write: bool, deadline: &Deadline) -> Result<usize, IoFailed> {
        deadline.remaining().ok_or(IoFailed)?;
        let mut count = 0;
        // SAFETY: pipe, event, stable boxed OVERLAPPED and complete owned buffer stay live
        // through synchronous completion or cancellation followed by actual completion.
        let ok = unsafe {
            if write {
                WriteFile(
                    self.pipe.as_raw_handle(),
                    self.bytes.as_ptr(),
                    self.bytes.len() as u32,
                    &mut count,
                    self.overlap.as_mut(),
                )
            } else {
                ReadFile(
                    self.pipe.as_raw_handle(),
                    self.bytes.as_mut_ptr(),
                    self.bytes.len() as u32,
                    &mut count,
                    self.overlap.as_mut(),
                )
            }
        };
        if ok == 0 {
            // SAFETY: immediately observes only the submission result on this owner thread.
            if unsafe { GetLastError() } != ERROR_IO_PENDING {
                return Err(IoFailed);
            }
            self.pending = true;
            loop {
                let Some(remaining) = deadline.remaining() else {
                    self.cancel_and_drain();
                    return Err(IoFailed);
                };
                let wait = remaining.min(SLICE).as_millis().max(1) as u32;
                // SAFETY: retained own event; short bounded wait, no message pumping or GUI.
                match unsafe { WaitForSingleObject(self.event.as_raw_handle(), wait) } {
                    WAIT_OBJECT_0 => break,
                    WAIT_TIMEOUT => continue,
                    _ => {
                        self.cancel_and_drain();
                        return Err(IoFailed);
                    }
                }
            }
        }
        // SAFETY: complete/still-owned operation; event reported completion or synchronous success.
        let completed = unsafe {
            GetOverlappedResult(
                self.pipe.as_raw_handle(),
                self.overlap.as_ref(),
                &mut count,
                0,
            )
        };
        if completed == 0 {
            self.cancel_and_drain();
            return Err(IoFailed);
        }
        self.pending = false;
        let count = count as usize;
        if count > self.bytes.len() {
            return Err(IoFailed);
        }
        Ok(count)
    }

    fn cancel_and_drain(&mut self) {
        if !self.pending {
            return;
        }
        // SAFETY: cancel only this exact owned OVERLAPPED; cancellation is not completion.
        unsafe {
            CancelIoEx(self.pipe.as_raw_handle(), self.overlap.as_ref());
        }
        loop {
            let mut count = 0;
            // SAFETY: all operation resources retained; this owned worker may outlive caller.
            let ok = unsafe {
                GetOverlappedResult(
                    self.pipe.as_raw_handle(),
                    self.overlap.as_ref(),
                    &mut count,
                    1,
                )
            };
            // SAFETY: immediately reads only the completion result on this thread.
            let error = unsafe { GetLastError() };
            if ok != 0 || error != ERROR_IO_INCOMPLETE {
                self.pending = false;
                break;
            }
        }
    }
}

impl Drop for Operation<'_> {
    fn drop(&mut self) {
        self.cancel_and_drain();
    }
}

/// How the worker runs one call: a pipe call in production, a fake in the unit tests.
type Runner = Box<dyn FnMut(&AgentCall, Duration) -> Result<DecodedReply, CallFailure> + Send>;

/// A queued call and the clock reading at which it stops being worth running.
struct Work {
    call: AgentCall,
    expires_at_ms: u64,
}

/// The GUI's `AgentPort`: its own worker thread, with the expiry semantics of
/// `integration/ports.rs:45-93`. Calls run one after another, each against a freshly located
/// pipe, so an agent that restarts between calls is found again.
pub(crate) struct PipePort {
    clock: crate::live::Clock,
    /// Dropping this ends the worker after its current call (at most `MAX_TIMEOUT_MS`). The
    /// thread is never joined.
    work: mpsc::SyncSender<Work>,
    replies: mpsc::Receiver<AgentReply>,
    /// Call id → clock reading at which it expires.
    pending: BTreeMap<u64, u64>,
    last: u64,
}

impl std::fmt::Debug for PipePort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipePort")
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

impl PipePort {
    pub(crate) fn start(paths: Paths, clock: crate::live::Clock) -> std::io::Result<Self> {
        Self::with_runner(
            Box::new(move |call, timeout| {
                let pipe = AgentPipe::locate(&paths).ok_or(CallFailure::Unavailable)?;
                pipe.call(&call.request, timeout).map(|answer| answer.reply)
            }),
            clock,
        )
    }

    fn with_runner(mut run: Runner, clock: crate::live::Clock) -> std::io::Result<Self> {
        let (work, queue) = mpsc::sync_channel::<Work>(MAX_QUEUE);
        let (reply, replies) = mpsc::channel::<AgentReply>();
        let worker_clock = clock.clone();
        std::thread::Builder::new()
            .name("installer-agent-pipe".into())
            .spawn(move || {
                while let Ok(job) = queue.recv() {
                    // Already expired while it waited: the port has answered it, so don't run it.
                    let Some(left) = job
                        .expires_at_ms
                        .checked_sub(worker_clock())
                        .filter(|ms| *ms > 0)
                    else {
                        continue;
                    };
                    let timeout = Duration::from_millis(left.min(MAX_TIMEOUT_MS));
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        run(&job.call, timeout)
                    }))
                    .unwrap_or(Err(CallFailure::TimeoutOutcomeUnknown));
                    let answer = AgentReply {
                        id: job.call.id,
                        observed_at_ms: worker_clock(),
                        source: ObservationSource::Live,
                        result,
                    };
                    if reply.send(answer).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            clock,
            work,
            replies,
            pending: BTreeMap::new(),
            last: 0,
        })
    }
}

impl AgentPort for PipePort {
    fn submit(&mut self, call: AgentCall) -> Result<(), CallFailure> {
        // The worker is a fresh thread and cannot see an impersonation token on this one.
        #[cfg(not(test))]
        identity::native::refuse_impersonation().map_err(|_| CallFailure::Unavailable)?;
        if !(1..=MAX_TIMEOUT_MS).contains(&call.timeout_ms) {
            return Err(CallFailure::Unavailable);
        }
        if call.id <= self.last || self.pending.len() >= MAX_QUEUE {
            return Err(CallFailure::QueueFull);
        }
        let now = (self.clock)();
        let id = call.id;
        let expires_at_ms = now.saturating_add(call.timeout_ms);
        self.work
            .try_send(Work {
                call,
                expires_at_ms,
            })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => CallFailure::QueueFull,
                mpsc::TrySendError::Disconnected(_) => CallFailure::Unavailable,
            })?;
        self.pending.insert(id, expires_at_ms);
        self.last = id;
        Ok(())
    }

    fn poll(&mut self) -> Vec<AgentReply> {
        let now = (self.clock)();
        let mut out = Vec::new();
        for reply in self.replies.try_iter().take(MAX_QUEUE) {
            // A reply for a call that already expired (or was never submitted) is dropped.
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
                source: ObservationSource::Demo,
                result: Err(CallFailure::TimeoutOutcomeUnknown),
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    /// The producer-shaped Status reply of `tests/windows_contract.rs` (`STATUS`), byte for byte.
    const STATUS: &[u8] = br#"{"ok":true,"result":{
 "controlling":null,"controlled_by":null,"projections":[],
 "displays":[{"id":1,"name":"local","pixels":[1920,1080],"scale":1.25,"mm":[500.5,300.5],"origin":[-10.5,0]}],
 "peers":[{"node":"2222222222222222222222222222222222222222222222222222222222222222","name":"remote",
 "connected":true,"displays":[{"id":2,"name":"remote","pixels":[3840,2160],"scale":2,"mm":[600,400],"origin":[0,0]}]}],
 "layout":[{"node":"1111111111111111","display":1,"origin_mm":[0,0],"version":17},
 {"node":"2222222222222222","display":2,"origin_mm":[500.5,-1.5],"version":19}],
 "installer":{"schema_version":1,"build":{"version":"0.0.0","features":["video"]},
 "instance":{"id":18446744073709551615,"pid":4242,"uid":null,"exe":"C:\\fixture\\crosspane-agent.exe",
 "runtime_dir":"C:\\fixture\\runtime","started_unix_ms":1790950000000},
 "config_revision":"9f86d081884c7d65","node":"1111111111111111111111111111111111111111111111111111111111111111",
 "recovery_pending":0,"startup_recovery":"restored",
 "gate":{"open":true,"session":"unlocked","active":true,"armed":true,"panic":false},
 "epochs":{"gate":7,"grants":3,"layout":2,"backends":1},
 "backends":[
 {"name":"capture","state":"ready","reason":null},{"name":"keys","state":"ready","reason":null},
 {"name":"pointer","state":"ready","reason":null},{"name":"overlay","state":"ready","reason":null},
 {"name":"hotkeys","state":"ready","reason":null},{"name":"keystore","state":"ready","reason":null},
 {"name":"windows","state":"ready","reason":null},{"name":"parking","state":"ready","reason":null},
 {"name":"frames","state":"ready","reason":null},{"name":"tray","state":"ready","reason":null},
 {"name":"links","state":"ready","reason":null},{"name":"gpu","state":"ready","reason":null},
 {"name":"home","state":"ready","reason":null},{"name":"audio","state":"ready","reason":null},
 {"name":"discovery","state":"ready","reason":null}],
 "keystore":"os_store","permissions":[],
 "discovery":{"enabled":true,"running":true,"candidates":1,"error":null},"tray":{"created":true},
 "audio":{"enabled":true,"active_peers":[],"frames_sent":14,"frames_played":15},"settings_opened":16,
 "peers":[{"node":"2222222222222222222222222222222222222222222222222222222222222222","name":"remote",
 "connected":true,"link_generation":3,"features":["e1"],"grants_given":["browse","input"],
 "last_source_parking":null,"counters":{
 "e1_controller_started":0,"e1_controller_ended":1,"e1_target_started":2,"e1_target_ended":3,
 "e1_injections_ok":4,"e1_hud_shows":5,"e1_chord_releases":6,"e1_command_releases":7,
 "e2_source_started":8,"e2_source_returned":9,"e2_dest_started":10,"e2_dest_returned":11,
 "e2_frames_presented":null,"e2_returns_failed":13}}]}}}"#;

    fn answer(raw: &[u8]) -> Answer {
        Answer {
            reply: DecodedReply::Acknowledged,
            raw: raw.to_vec(),
            server_pid: 1,
        }
    }

    #[test]
    fn pipe_name_matches_the_agents_algorithm_vector() {
        // The inputs of the agent's own endpoint test (`security.rs` `control_endpoint_...`).
        let name = endpoint_name(
            "S-1-5-21-1",
            "S-1-5-5-1-2",
            OsStr::new(r"C:\scratch\runtime"),
        );
        assert_eq!(name, r"\\.\pipe\Crosspane-d03a985e570cadefee427dfbe37c48e9");
        // A canonicalized runtime directory comes back with the verbatim prefix; it is hashed
        // whole (checked against the live agent's pipe on 2026-10-13).
        let verbatim = endpoint_name(
            "S-1-5-21-1",
            "S-1-5-5-1-2",
            OsStr::new(r"\\?\C:\scratch\runtime"),
        );
        assert_eq!(
            verbatim,
            r"\\.\pipe\Crosspane-8cc77ffe4e892f033611768d3c015e51"
        );
    }

    #[test]
    fn pipe_name_separates_users_logons_and_runtime_directories() {
        let runtime = OsStr::new(r"C:\scratch\runtime");
        let base = endpoint_name("S-1-5-21-1", "S-1-5-5-1-2", runtime);
        assert_eq!(base, endpoint_name("S-1-5-21-1", "S-1-5-5-1-2", runtime));
        assert_ne!(
            base,
            endpoint_name("S-1-5-21-1", "S-1-5-5-1-2", OsStr::new(r"C:\scratch\other"))
        );
        assert_ne!(base, endpoint_name("S-1-5-21-1", "S-1-5-5-1-23", runtime));
        assert_ne!(base, endpoint_name("S-1-5-21-13", "S-1-5-5-1-2", runtime));
        // The fields are delimited: moving a character between them changes the name.
        assert_ne!(
            endpoint_name("ab", "c", runtime),
            endpoint_name("a", "bc", runtime)
        );
    }

    #[test]
    fn answer_instance_reads_the_status_path() {
        assert_eq!(answer(STATUS).instance(), Some(u64::MAX));
        // The typed decoder agrees with the raw path on the same bytes.
        let reply = decode_reply(&InstallerRequest::Status, STATUS, AgentPlatform::Windows);
        let Ok(DecodedReply::Status(crate::agent_contract::StatusAdmission::Supported(health))) =
            reply
        else {
            panic!("the contract fixture must decode as a supported status");
        };
        assert_eq!(health.installer().instance.id, u64::MAX);
        assert_eq!(
            answer(br#"{"ok":true,"result":{"installer":{"instance":{"id":7}}}}"#).instance(),
            Some(7)
        );
    }

    #[test]
    fn answer_instance_is_none_when_the_path_is_missing_or_not_a_u64() {
        for raw in [
            &b""[..],
            b"not json",
            br#"{"ok":true,"result":{}}"#,
            br#"{"ok":true,"result":{"installer":{}}}"#,
            br#"{"ok":true,"result":{"installer":{"instance":{}}}}"#,
            br#"{"ok":true,"result":{"installer":{"instance":{"id":"7"}}}}"#,
            br#"{"ok":true,"result":{"installer":{"instance":{"id":-1}}}}"#,
            br#"{"ok":true,"result":{"installer":{"instance":{"id":1.5}}}}"#,
        ] {
            assert_eq!(
                answer(raw).instance(),
                None,
                "{}",
                String::from_utf8_lossy(raw)
            );
        }
    }

    #[test]
    fn answer_instance_works_for_a_pending_health_reply() {
        // No `schema_version`: the decoder calls this Pending, but the instance is still there.
        let raw = br#"{"ok":true,"result":{"installer":{"instance":{"id":9}}}}"#;
        let request = InstallerRequest::Status;
        let reply = decode_reply(&request, raw, AgentPlatform::Windows).unwrap();
        assert!(matches!(
            reply,
            DecodedReply::Status(crate::agent_contract::StatusAdmission::PendingHealthContract(_))
        ));
        let pending = Answer {
            reply,
            raw: raw.to_vec(),
            server_pid: 1,
        };
        assert_eq!(pending.instance(), Some(9));
    }

    #[test]
    fn response_assembles_one_line_across_reads() {
        let mut response = Response::new();
        assert_eq!(response.append(b"{\"ok\":").unwrap(), None);
        assert_eq!(response.append(b"true}").unwrap(), None);
        assert_eq!(
            response.append(b"\n").unwrap(),
            Some(b"{\"ok\":true}\n".to_vec())
        );
    }

    #[test]
    fn response_refuses_bytes_after_the_newline_and_oversize_lines() {
        assert_eq!(Response::new().append(b"{}\nx"), Err(IoFailed));
        let mut response = Response::new();
        assert_eq!(
            response.append(&vec![b'a'; MAX_RESPONSE_BYTES]).unwrap(),
            None
        );
        assert_eq!(response.append(b"b"), Err(IoFailed));
    }

    #[test]
    fn deadline_rejects_zero_and_caps_at_the_contract_maximum() {
        assert!(matches!(
            Deadline::after(Duration::ZERO),
            Err(CallFailure::InvalidCall(ContractError::InvalidDeadline))
        ));
        let long = Deadline::after(Duration::from_secs(3600)).unwrap();
        assert!(long.remaining().unwrap() <= Duration::from_millis(MAX_TIMEOUT_MS));
        let short = Deadline::after(Duration::from_millis(1)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(short.remaining(), None);
    }

    /// A manual clock the tests move forward.
    fn manual_clock() -> (Arc<AtomicU64>, crate::live::Clock) {
        let now = Arc::new(AtomicU64::new(1_000));
        let reader = now.clone();
        (now, Arc::new(move || reader.load(Ordering::Acquire)))
    }

    fn call(id: u64, timeout_ms: u64) -> AgentCall {
        AgentCall {
            id,
            request: InstallerRequest::Status,
            timeout_ms,
        }
    }

    /// Polls until `want` replies arrive (the fake worker answers on another thread).
    fn collect(port: &mut PipePort, want: usize) -> Vec<AgentReply> {
        let mut out = Vec::new();
        for _ in 0..400 {
            out.extend(port.poll());
            if out.len() >= want {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        out
    }

    #[test]
    fn port_delivers_a_live_reply_once() {
        let (_now, clock) = manual_clock();
        let mut port =
            PipePort::with_runner(Box::new(|_, _| Ok(DecodedReply::Acknowledged)), clock).unwrap();
        port.submit(call(1, 1_000)).unwrap();
        let replies = collect(&mut port, 1);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].id, 1);
        assert_eq!(replies[0].source, ObservationSource::Live);
        assert_eq!(replies[0].result, Ok(DecodedReply::Acknowledged));
        assert!(port.poll().is_empty());
    }

    #[test]
    fn port_requires_increasing_ids_and_a_valid_timeout() {
        let (_now, clock) = manual_clock();
        let mut port =
            PipePort::with_runner(Box::new(|_, _| Ok(DecodedReply::Acknowledged)), clock).unwrap();
        assert_eq!(port.submit(call(1, 0)), Err(CallFailure::Unavailable));
        assert_eq!(
            port.submit(call(1, MAX_TIMEOUT_MS + 1)),
            Err(CallFailure::Unavailable)
        );
        assert_eq!(port.submit(call(0, 1_000)), Err(CallFailure::QueueFull));
        port.submit(call(5, 1_000)).unwrap();
        assert_eq!(port.submit(call(5, 1_000)), Err(CallFailure::QueueFull));
        assert_eq!(port.submit(call(4, 1_000)), Err(CallFailure::QueueFull));
        port.submit(call(6, 1_000)).unwrap();
        assert_eq!(collect(&mut port, 2).len(), 2);
    }

    #[test]
    fn port_holds_at_most_32_pending_calls() {
        let (_now, clock) = manual_clock();
        let (release, gate) = mpsc::channel::<()>();
        let mut port = PipePort::with_runner(
            Box::new(move |_, _| {
                let _ = gate.recv_timeout(Duration::from_secs(10));
                Ok(DecodedReply::Acknowledged)
            }),
            clock,
        )
        .unwrap();
        for id in 1..=32 {
            port.submit(call(id, 5_000)).unwrap();
        }
        assert_eq!(port.submit(call(33, 5_000)), Err(CallFailure::QueueFull));
        for _ in 0..32 {
            release.send(()).unwrap();
        }
        assert_eq!(collect(&mut port, 32).len(), 32);
        // Room again once they are answered.
        port.submit(call(33, 5_000)).unwrap();
        release.send(()).unwrap();
        assert_eq!(collect(&mut port, 1).len(), 1);
    }

    #[test]
    fn port_expires_a_stuck_call_and_drops_its_late_reply() {
        let (now, clock) = manual_clock();
        let (release, gate) = mpsc::channel::<()>();
        let mut port = PipePort::with_runner(
            Box::new(move |_, _| {
                let _ = gate.recv_timeout(Duration::from_secs(10));
                Ok(DecodedReply::Acknowledged)
            }),
            clock,
        )
        .unwrap();
        port.submit(call(1, 1_000)).unwrap();
        assert!(port.poll().is_empty());
        now.fetch_add(1_000, Ordering::AcqRel);
        let replies = port.poll();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].id, 1);
        assert_eq!(replies[0].source, ObservationSource::Demo);
        assert_eq!(replies[0].observed_at_ms, 2_000);
        assert_eq!(replies[0].result, Err(CallFailure::TimeoutOutcomeUnknown));
        // The worker finishes later; that reply belongs to no pending call and is dropped.
        release.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(port.poll().is_empty());
        assert!(port.poll().is_empty());
    }

    #[test]
    fn port_skips_a_queued_call_that_expired_before_it_started() {
        let (now, clock) = manual_clock();
        let (release, gate) = mpsc::channel::<()>();
        let ran = Arc::new(AtomicU64::new(0));
        let counted = ran.clone();
        let mut port = PipePort::with_runner(
            Box::new(move |call, _| {
                counted.fetch_or(1 << call.id, Ordering::AcqRel);
                let _ = gate.recv_timeout(Duration::from_secs(10));
                Ok(DecodedReply::Acknowledged)
            }),
            clock,
        )
        .unwrap();
        port.submit(call(1, 5_000)).unwrap();
        port.submit(call(2, 1_000)).unwrap();
        // Let the worker start call 1, then run call 2's time out while it waits in the queue.
        std::thread::sleep(Duration::from_millis(100));
        now.fetch_add(2_000, Ordering::AcqRel);
        let replies = port.poll();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].id, 2);
        release.send(()).unwrap();
        assert_eq!(collect(&mut port, 1)[0].id, 1);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(ran.load(Ordering::Acquire), 1 << 1, "call 2 must not run");
    }

    #[test]
    fn port_turns_a_panicking_call_into_an_unknown_outcome_and_keeps_serving() {
        let (_now, clock) = manual_clock();
        let mut port = PipePort::with_runner(
            Box::new(|call, _| {
                assert_ne!(call.id, 1, "fixture panic");
                Ok(DecodedReply::Acknowledged)
            }),
            clock,
        )
        .unwrap();
        port.submit(call(1, 1_000)).unwrap();
        let first = collect(&mut port, 1);
        assert_eq!(first[0].result, Err(CallFailure::TimeoutOutcomeUnknown));
        port.submit(call(2, 1_000)).unwrap();
        assert_eq!(
            collect(&mut port, 1)[0].result,
            Ok(DecodedReply::Acknowledged)
        );
    }

    #[test]
    fn port_reports_a_failed_call_as_that_failure() {
        let (_now, clock) = manual_clock();
        let mut port =
            PipePort::with_runner(Box::new(|_, _| Err(CallFailure::Unavailable)), clock).unwrap();
        port.submit(call(1, 1_000)).unwrap();
        assert_eq!(
            collect(&mut port, 1)[0].result,
            Err(CallFailure::Unavailable)
        );
    }

    #[test]
    fn dropping_the_port_stops_the_worker() {
        let (_now, clock) = manual_clock();
        let (alive, watch) = mpsc::channel::<()>();
        let port = PipePort::with_runner(
            Box::new(move |_, _| {
                let _keep = &alive;
                Ok(DecodedReply::Acknowledged)
            }),
            clock,
        )
        .unwrap();
        drop(port);
        // The worker owns the runner (and the sender inside it); it ends and drops both.
        assert!(matches!(
            watch.recv_timeout(Duration::from_secs(5)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
    }
}
