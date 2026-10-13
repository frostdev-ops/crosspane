//! The sign-in supervisor (L6): keeps the agent running inside a job object.
//!
//! Simple and stateless. It writes no records and takes no installer lock; its only shared state is
//! the named mutex that makes it one per sign-in session. It logs to `setup.log` through
//! `setup_note!`.
//!
//! What the agent is given (checked against the old supervisor and the agent's `main`):
//! - the quoted image path as the whole command line, so the agent runs its default `run` command;
//! - the supervisor's own environment (the user's logon environment), unchanged. The agent's
//!   restart handoff variable is the agent's own: the supervisor never sets it;
//! - no inherited handles and no standard handles (the release agent is a GUI program);
//! - the install folder as the current directory, `NORMAL_PRIORITY_CLASS` and no console window.
//!   Never `CREATE_BREAKAWAY_FROM_JOB`: the agent's self-restart must stay inside the job.
//!
//! What the supervisor watches: only processes whose image is the installed agent. The agent also
//! starts the settings window (`crosspane-ui.exe`) into the same job; that window's exit says
//! nothing about whether the agent should run, so it is not tracked and its handle is not held.
//! A restart decision is taken when the last agent process has ended, not when the job is empty.

use std::{
    collections::VecDeque,
    ffi::{OsStr, OsString, c_void},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::{
        ERROR_ALREADY_EXISTS, FILETIME, GetLastError, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
        WAIT_TIMEOUT,
    },
    System::{
        IO::{CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED},
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_ASSOCIATE_COMPLETION_PORT,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectAssociateCompletionPortInformation,
            JobObjectExtendedLimitInformation, SetInformationJobObject,
        },
        SystemServices::{
            JOB_OBJECT_MSG_ABNORMAL_EXIT_PROCESS, JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO,
            JOB_OBJECT_MSG_EXIT_PROCESS, JOB_OBJECT_MSG_NEW_PROCESS,
        },
        Threading::{
            CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateMutexW,
            CreateProcessW, GetExitCodeProcess, GetProcessTimes, INFINITE, NORMAL_PRIORITY_CLASS,
            OpenProcess, PROCESS_INFORMATION, PROCESS_NAME_WIN32,
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW,
            ResumeThread, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
        },
    },
};

use super::{
    error::note_io,
    lock::{OPERATION_MUTEX, mutex_exists},
    paths::{Paths, PayloadFile, same_path},
    setup_log::setup_note,
};

pub const SUPERVISOR_FLAG: &str = "--windows-supervisor";
pub const SUPERVISOR_ALIAS: &str = "--supervisor";
pub(crate) const SUPERVISOR_MUTEX: &str = r"Local\Crosspane-Supervisor";

/// Failures kept for the give-up rule are the ones this recent.
const FAILURE_WINDOW: Duration = Duration::from_secs(120);
/// The supervisor gives up when this many failures sit inside the window.
const MAX_FAILURES: usize = 5;
/// Seconds to wait before restart number `n` of the current window (`n` starts at 1).
const BACKOFF_SECS: [u64; 6] = [1, 2, 4, 8, 16, 30];
/// The completion key of this supervisor's job; packets with another key are not ours.
const JOB_KEY: usize = 0x5350_5356;
/// How long to wait for an already-ended process to report its exit status.
const EXIT_STATUS_WAIT_MS: u32 = 2_000;
/// How long an agent's clean exit waits for a replacement agent to join the job before it counts
/// as the user quitting. The agent starts its replacement into the job before it exits, so the
/// replacement is normally known already; this only covers a late job message.
const REPLACEMENT_GRACE: Duration = Duration::from_secs(2);
/// How long the job-empty fallback gives each held agent to report that it has ended.
const EMPTY_JOB_WAIT_MS: u32 = 100;

/// Exactly one argument, and it is the flag or its alias.
pub fn is_supervisor(arguments: &[OsString]) -> bool {
    matches!(arguments, [only] if only == SUPERVISOR_FLAG || only == SUPERVISOR_ALIAS)
}

/// Mutex (a second instance → 0); job `KILL_ON_JOB_CLOSE` + completion port associated BEFORE the
/// first assign; `CreateProcessW(agent, CREATE_SUSPENDED | NORMAL_PRIORITY_CLASS, cwd install)` →
/// assign → resume; on `JOB_OBJECT_MSG_NEW_PROCESS` a handle is opened and kept only for a process
/// whose image is the agent (the settings window and anything else in the job are ignored). When
/// the last held agent has ended (`EXIT_PROCESS`, `ABNORMAL_EXIT_PROCESS`, or `ACTIVE_PROCESS_ZERO`
/// as the fallback), the exit code of the one with the latest `ExitTime` decides via
/// `RestartPolicy`. Logs via `setup_note!`. Returns the process exit code.
pub fn run() -> i32 {
    let _instance = match Instance::acquire() {
        Instance::First(held) => held,
        Instance::Second => {
            setup_note!("supervisor: another supervisor already runs in this session; exiting");
            return 0;
        }
        Instance::Failed(code) => {
            setup_note!("supervisor: could not create the single-instance mutex: os error {code}");
            return 1;
        }
    };
    let paths = match Paths::current() {
        Ok(paths) => paths,
        Err(error) => {
            setup_note!("supervisor: folders unavailable: {}", error.text);
            return 1;
        }
    };
    let job = match Job::create() {
        Ok(job) => job,
        Err(error) => {
            note_io("supervisor: create the job", &error);
            return 1;
        }
    };
    setup_note!("supervisor: start");
    let code = supervise(
        &job,
        &paths.file(PayloadFile::Agent),
        &paths.install,
        &mut RestartPolicy::new(),
    );
    setup_note!("supervisor: exit {code}");
    code
}

/// Whether a supervisor is running in this session (`lock::mutex_exists(SUPERVISOR_MUTEX)`).
pub(crate) fn running() -> bool {
    mutex_exists(SUPERVISOR_MUTEX)
}

/// Backoff and give-up bookkeeping for agent restarts.
#[derive(Debug)]
pub(crate) struct RestartPolicy {
    failures: VecDeque<Instant>,
}

/// What the supervisor does when the job becomes empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Next {
    Exit(i32),
    RestartAfter(Duration),
}

impl RestartPolicy {
    pub(crate) fn new() -> Self {
        Self {
            failures: VecDeque::new(),
        }
    }

    /// `Some(0)` → `Exit(0)`; `installer_busy` → `Exit(0)` (F6); else a failure: the 5th within
    /// 120 s → `Exit(1)`, otherwise `RestartAfter([1, 2, 4, 8, 16, 30][min(n - 1, 5)] s)`.
    pub(crate) fn on_empty(
        &mut self,
        now: Instant,
        last_exit: Option<u32>,
        installer_busy: bool,
    ) -> Next {
        if last_exit == Some(0) || installer_busy {
            return Next::Exit(0);
        }
        self.failures.push_back(now);
        while self
            .failures
            .front()
            .is_some_and(|oldest| now.saturating_duration_since(*oldest) > FAILURE_WINDOW)
        {
            self.failures.pop_front();
        }
        let recent = self.failures.len();
        if recent >= MAX_FAILURES {
            return Next::Exit(1);
        }
        let index = recent.saturating_sub(1).min(BACKOFF_SECS.len() - 1);
        Next::RestartAfter(Duration::from_secs(BACKOFF_SECS[index]))
    }
}

/// The exit code of the process that exited last. `exits` holds `(exit time, exit code)` pairs;
/// on an exact tie a failing code wins, so a doubtful stop is restarted rather than missed.
fn latest_exit(exits: impl IntoIterator<Item = (u64, u32)>) -> Option<u32> {
    exits
        .into_iter()
        .max_by_key(|&(time, code)| (time, code != 0))
        .map(|(_, code)| code)
}

/// How a held agent process stands when a job message makes the supervisor look at it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Standing {
    Running,
    /// Ended; `(exit time, exit code)` when they could be read.
    Ended(Option<(u64, u32)>),
}

/// `None` while any agent process still runs. Otherwise every agent has ended, and this is
/// `Some(exit code of the one that ended last)`; the inner `None` means no exit code could be read
/// (which includes there being no agent process at all).
fn settled(standings: impl IntoIterator<Item = Standing>) -> Option<Option<u32>> {
    let mut exits = Vec::new();
    for standing in standings {
        match standing {
            Standing::Running => return None,
            Standing::Ended(facts) => exits.extend(facts),
        }
    }
    Some(latest_exit(exits))
}

/// Whether a process that joined the job is tracked as an agent. `image` is its image path. A path
/// that could not be read counts as the agent: wrongly dropping a replacement agent would end it
/// with the supervisor, while wrongly keeping another process only delays a restart decision.
fn is_agent_image(image: Option<&Path>, agent: &Path) -> bool {
    image.is_none_or(|image| same_path(image, agent))
}

/// The single-instance mutex of this sign-in session.
#[derive(Debug)]
enum Instance {
    /// Created here; held (not released) until the process ends.
    First(OwnedHandle),
    /// Another supervisor already holds it.
    Second,
    /// The mutex could not be created (OS error code).
    Failed(u32),
}

impl Instance {
    fn acquire() -> Self {
        let name = wide(SUPERVISOR_MUTEX);
        // SAFETY: `name` is a NUL-terminated UTF-16 string that outlives the call; the default
        // security attributes are requested with a null pointer.
        let raw = unsafe { CreateMutexW(std::ptr::null(), 1, name.as_ptr()) };
        // SAFETY: reads this thread's last error, set by the CreateMutexW call just above.
        let error = unsafe { GetLastError() };
        if raw.is_null() {
            return Self::Failed(error);
        }
        // SAFETY: CreateMutexW returned a new handle (to the new or the existing mutex) that only
        // this function owns.
        let held = unsafe { OwnedHandle::from_raw_handle(raw) };
        if error == ERROR_ALREADY_EXISTS {
            return Self::Second;
        }
        Self::First(held)
    }
}

/// The job that holds the agent and every process it starts, plus its completion port.
#[derive(Debug)]
struct Job {
    /// Dropped first: `KILL_ON_JOB_CLOSE` ends whatever is still inside.
    job: OwnedHandle,
    port: OwnedHandle,
}

impl Job {
    /// A job with `KILL_ON_JOB_CLOSE`, a completion port, and the port associated with the job
    /// before any process is assigned.
    fn create() -> std::io::Result<Self> {
        // SAFETY: no security attributes and no name: this creates only our own unnamed job.
        let raw_job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw_job.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: CreateJobObjectW returned a new handle that only this function owns.
        let job = unsafe { OwnedHandle::from_raw_handle(raw_job) };

        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `job` is our empty job, and `limits` is a fully initialised structure of exactly
        // the size passed. No breakaway flags are set.
        let limited = unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast::<c_void>(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if limited == 0 {
            return Err(std::io::Error::last_os_error());
        }

        // SAFETY: INVALID_HANDLE_VALUE with no existing port asks for a new completion port that
        // serves no file; one thread reads it.
        let raw_port =
            unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
        if raw_port.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: CreateIoCompletionPort returned a new handle that only this function owns.
        let port = unsafe { OwnedHandle::from_raw_handle(raw_port) };

        let association = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
            CompletionKey: std::ptr::without_provenance_mut(JOB_KEY),
            CompletionPort: port.as_raw_handle(),
        };
        // SAFETY: `job` and `port` are our live handles, and `association` is fully initialised
        // and of exactly the size passed.
        let associated = unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectAssociateCompletionPortInformation,
                (&raw const association).cast::<c_void>(),
                size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>() as u32,
            )
        };
        if associated == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { job, port })
    }
}

/// An agent process of the job whose exit status the supervisor keeps.
#[derive(Debug)]
struct Member {
    pid: u32,
    process: OwnedHandle,
}

/// What starting the agent left behind.
#[derive(Debug)]
enum Spawn {
    /// The agent runs inside the job.
    Running(Member),
    /// The agent joined the job but could not be resumed and was ended; its exit will be reported.
    Ended(Member),
    /// No process was left in the job.
    Failed,
}

/// Starts the agent, waits until no agent process is left, decides, and repeats. Returns the exit
/// code. Other processes in the job (the settings window) neither delay nor trigger a decision.
fn supervise(job: &Job, agent: &Path, directory: &Path, policy: &mut RestartPolicy) -> i32 {
    loop {
        let mut members = Vec::new();
        let waiting = match spawn_agent(job, agent, directory) {
            Spawn::Running(member) => {
                setup_note!("supervisor: agent started pid={}", member.pid);
                members.push(member);
                true
            }
            Spawn::Ended(member) => {
                members.push(member);
                true
            }
            Spawn::Failed => {
                setup_note!("supervisor: agent did not start");
                false
            }
        };
        if waiting && let Err(code) = wait_until_agents_end(job, agent, &mut members) {
            return code;
        }
        let last_exit = latest_exit(members.iter().filter_map(|member| {
            let facts = exit_facts(&member.process);
            match facts {
                Some((_, code)) => setup_note!("supervisor: pid={} exited with {code}", member.pid),
                None => setup_note!("supervisor: pid={} exit status unreadable", member.pid),
            }
            facts
        }));
        // Handles belong to one cycle; closing them here also frees the exited processes.
        members.clear();
        let busy = mutex_exists(OPERATION_MUTEX);
        let next = policy.on_empty(Instant::now(), last_exit, busy);
        setup_note!(
            "supervisor: agent ended last_exit={last_exit:?} installer_busy={busy} {next:?}"
        );
        match next {
            Next::Exit(code) => return code,
            Next::RestartAfter(delay) => std::thread::sleep(delay),
        }
    }
}

/// Creates the agent suspended, puts it in the job, then resumes it.
fn spawn_agent(job: &Job, agent: &Path, directory: &Path) -> Spawn {
    let (Some(application), Some(directory)) = (
        wide_path(agent.as_os_str()),
        wide_path(directory.as_os_str()),
    ) else {
        setup_note!("supervisor: the agent or install folder is not a usable Windows path");
        return Spawn::Failed;
    };
    if application.contains(&u16::from(b'"')) {
        setup_note!("supervisor: the agent path is not usable");
        return Spawn::Failed;
    }
    // The whole command line is the quoted image path: the agent takes no arguments to run.
    let mut command = Vec::with_capacity(application.len() + 2);
    command.push(u16::from(b'"'));
    command.extend(application.iter().take(application.len() - 1));
    command.push(u16::from(b'"'));
    command.push(0);

    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: `application`, `command` and `directory` are NUL-terminated UTF-16 buffers that
    // outlive the call, and `command` is writable as CreateProcessW requires. No security
    // attributes, no handle inheritance, no explicit environment (the supervisor's own is
    // inherited) and no breakaway flag; `startup` and `info` are initialised structures of the
    // right types.
    let created = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_SUSPENDED
                | NORMAL_PRIORITY_CLASS
                | CREATE_UNICODE_ENVIRONMENT
                | CREATE_NO_WINDOW,
            std::ptr::null(),
            directory.as_ptr(),
            &startup,
            &mut info,
        )
    };
    if created == 0 {
        note_io(
            "supervisor: start the agent",
            &std::io::Error::last_os_error(),
        );
        return Spawn::Failed;
    }
    // SAFETY: the successful CreateProcessW returned a new process handle that only this function
    // owns.
    let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess) };
    // SAFETY: the same call returned the primary thread handle, also owned only here.
    let thread = unsafe { OwnedHandle::from_raw_handle(info.hThread) };

    // SAFETY: both handles are live: our job, and the suspended child we just created.
    if unsafe { AssignProcessToJobObject(job.job.as_raw_handle(), process.as_raw_handle()) } == 0 {
        note_io(
            "supervisor: put the agent in the job",
            &std::io::Error::last_os_error(),
        );
        // The child never ran and is not in the job: end it so nothing escapes the supervisor.
        end_child(&process);
        return Spawn::Failed;
    }
    let member = Member {
        pid: info.dwProcessId,
        process,
    };
    // SAFETY: `thread` is the live primary thread of the suspended child; ResumeThread takes only
    // the handle.
    if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
        note_io(
            "supervisor: resume the agent",
            &std::io::Error::last_os_error(),
        );
        end_child(&member.process);
        return Spawn::Ended(member);
    }
    Spawn::Running(member)
}

/// Ends the child this supervisor just created, and waits a moment for it to go.
fn end_child(process: &OwnedHandle) {
    // SAFETY: ends only the child this function's caller just created and still holds a
    // full-access handle to.
    let _ = unsafe { TerminateProcess(process.as_raw_handle(), 1) };
    // SAFETY: waits, for a bounded time, on the same live handle.
    let _ = unsafe { WaitForSingleObject(process.as_raw_handle(), 5_000) };
}

/// What reading the job's completion port produced.
#[derive(Debug)]
enum Dequeued {
    /// A job message: its id and the process id it carries (meaningful for process messages).
    Message { id: u32, pid: u32 },
    /// A packet that is not a job message of ours, or a failed I/O packet.
    Ignored,
    /// The wait ran out with nothing queued.
    TimedOut,
    /// The wait itself failed.
    Failed(std::io::Error),
}

/// Reads one packet from the job's completion port, waiting at most `timeout_ms`.
fn dequeue(job: &Job, timeout_ms: u32) -> Dequeued {
    let mut message = 0u32;
    let mut key = 0usize;
    let mut overlapped: *mut OVERLAPPED = std::ptr::null_mut();
    // SAFETY: `port` is our live completion port, and the three outputs point to writable
    // locals. Job messages carry the message id as the byte count and, for process messages,
    // the process id in the overlapped pointer value; the pointer is never dereferenced.
    let dequeued = unsafe {
        GetQueuedCompletionStatus(
            job.port.as_raw_handle(),
            &mut message,
            &mut key,
            &mut overlapped,
            timeout_ms,
        )
    };
    if dequeued == 0 {
        if !overlapped.is_null() {
            return Dequeued::Ignored;
        }
        let error = std::io::Error::last_os_error();
        let code = error
            .raw_os_error()
            .and_then(|code| u32::try_from(code).ok());
        return if code == Some(WAIT_TIMEOUT) {
            Dequeued::TimedOut
        } else {
            Dequeued::Failed(error)
        };
    }
    if key != JOB_KEY {
        return Dequeued::Ignored;
    }
    Dequeued::Message {
        id: message,
        pid: u32::try_from(overlapped.addr()).unwrap_or(0),
    }
}

/// Reads job messages until no agent process is left, holding a handle to each agent that joins
/// the job (the agent's own restart child among them) and to nothing else. A decision is due when
/// the last held agent has ended: on its `EXIT_PROCESS` / `ABNORMAL_EXIT_PROCESS` message, or, as a
/// fallback, on `ACTIVE_PROCESS_ZERO`.
///
/// The agent restarts itself by starting a replacement into the job and then exiting 0. The job's
/// messages arrive in order, so the replacement's `NEW_PROCESS` is handled (and the replacement
/// held and running) before the old agent's exit is. As a safeguard, a clean exit with no agent
/// left waits `REPLACEMENT_GRACE` for another agent to join before it counts as the user quitting.
///
/// `Err` carries the supervisor's exit code when the wait itself fails.
fn wait_until_agents_end(job: &Job, agent: &Path, members: &mut Vec<Member>) -> Result<(), i32> {
    // Set while a clean exit waits to see whether a replacement agent joins.
    let mut grace_ends: Option<Instant> = None;
    loop {
        let timeout_ms = grace_ends.map_or(INFINITE, |end| {
            let left = end.saturating_duration_since(Instant::now());
            u32::try_from(left.as_millis()).unwrap_or(u32::MAX)
        });
        let (id, pid) = match dequeue(job, timeout_ms) {
            Dequeued::Message { id, pid } => (id, pid),
            Dequeued::Ignored => continue,
            // Only a running grace period has a timeout, and nothing replaced the agent in it.
            Dequeued::TimedOut => return Ok(()),
            Dequeued::Failed(error) => {
                note_io("supervisor: wait for the job", &error);
                return Err(1);
            }
        };
        match id {
            JOB_OBJECT_MSG_NEW_PROCESS => {
                if adopt(job, agent, members, pid) == Joined::Agent {
                    grace_ends = None;
                }
                continue;
            }
            // Only the end of a held agent can settle anything; other processes are not tracked.
            JOB_OBJECT_MSG_EXIT_PROCESS | JOB_OBJECT_MSG_ABNORMAL_EXIT_PROCESS
                if members.iter().any(|member| member.pid == pid) => {}
            // The fallback: the job has no process left. A packet from an earlier cycle can arrive
            // late; the held agents tell it apart, because one of them is still running then.
            JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO => {}
            _ => continue,
        }
        let job_empty = id == JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO;
        let Some(last_exit) = settled(members.iter().map(|member| {
            // The process the job just reported may be a moment from signalling its handle.
            let patience_ms = if job_empty {
                EMPTY_JOB_WAIT_MS
            } else if member.pid == pid {
                EXIT_STATUS_WAIT_MS
            } else {
                0
            };
            standing(member, patience_ms)
        })) else {
            continue;
        };
        if last_exit == Some(0) && !job_empty {
            // A clean exit is the user quitting, unless the agent's replacement is still on its way.
            grace_ends.get_or_insert_with(|| Instant::now() + REPLACEMENT_GRACE);
            continue;
        }
        return Ok(());
    }
}

/// What came of a process that joined the job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Joined {
    /// An agent process, now held.
    Agent,
    /// A process whose handle is already held (the agent this supervisor started).
    Held,
    /// Not tracked: not the agent, not in the job (a reused process id), or already gone.
    Other,
}

/// Opens a process that joined the job and keeps it only if its image is the agent, so its exit
/// status can be read later. Any other process (the settings window) is closed again.
fn adopt(job: &Job, agent: &Path, members: &mut Vec<Member>, pid: u32) -> Joined {
    if pid == 0 {
        return Joined::Other;
    }
    if members.iter().any(|member| member.pid == pid) {
        return Joined::Held;
    }
    // SAFETY: OpenProcess takes plain values and returns a new handle or null; only
    // synchronise and limited-query rights are requested, never terminate.
    let raw = unsafe {
        OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )
    };
    if raw.is_null() {
        note_io(
            "supervisor: open a job process",
            &std::io::Error::last_os_error(),
        );
        return Joined::Other;
    }
    // SAFETY: OpenProcess returned a new handle that only this function owns.
    let process = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut inside = 0;
    // SAFETY: both handles are live, and `inside` is a writable local.
    let known = unsafe {
        IsProcessInJob(
            process.as_raw_handle(),
            job.job.as_raw_handle(),
            &mut inside,
        )
    };
    if known == 0 || inside == 0 {
        setup_note!("supervisor: pid={pid} is not in the job; ignoring it");
        return Joined::Other;
    }
    let image = image_path(&process);
    if !is_agent_image(image.as_deref(), agent) {
        setup_note!(
            "supervisor: pid={pid} joined the job; it is not the agent, so it is not tracked"
        );
        return Joined::Other;
    }
    if image.is_none() {
        setup_note!("supervisor: pid={pid} has no readable image path; tracking it as the agent");
    }
    setup_note!("supervisor: pid={pid} joined the job");
    members.push(Member { pid, process });
    Joined::Agent
}

/// The Win32 path of the program a process runs; `None` when it cannot be read.
fn image_path(process: &OwnedHandle) -> Option<PathBuf> {
    // A Win32 path is at most 32 767 UTF-16 units.
    let mut buffer = vec![0u16; 32_768];
    let mut length = u32::try_from(buffer.len()).ok()?;
    // SAFETY: the handle is live with limited-query rights; `buffer` holds `length` writable
    // units, and `length` is the writable in/out size.
    let read = unsafe {
        QueryFullProcessImageNameW(
            process.as_raw_handle(),
            PROCESS_NAME_WIN32,
            buffer.as_mut_ptr(),
            &mut length,
        )
    };
    if read == 0 {
        return None;
    }
    buffer.truncate(usize::try_from(length).ok()?);
    Some(PathBuf::from(OsString::from_wide(&buffer)))
}

/// Looks at a held agent process: still running, or ended with whatever exit status can be read.
/// `patience_ms` is how long to wait for a process the job has just reported as ended to signal
/// its handle.
fn standing(member: &Member, patience_ms: u32) -> Standing {
    // SAFETY: waits, for a bounded time, on a live handle that has synchronise rights.
    match unsafe { WaitForSingleObject(member.process.as_raw_handle(), patience_ms) } {
        WAIT_OBJECT_0 => Standing::Ended(exit_facts(&member.process)),
        WAIT_TIMEOUT => Standing::Running,
        // The wait itself failed: count the process as ended, with nothing readable, rather than
        // wait on it forever.
        _ => Standing::Ended(None),
    }
}

/// `(exit time, exit code)` of a process that has ended; `None` if it has not, or cannot be read.
fn exit_facts(process: &OwnedHandle) -> Option<(u64, u32)> {
    // SAFETY: waits, for a bounded time, on a live handle that has synchronise rights.
    if unsafe { WaitForSingleObject(process.as_raw_handle(), EXIT_STATUS_WAIT_MS) } != WAIT_OBJECT_0
    {
        return None;
    }
    let mut code = 0u32;
    // SAFETY: the handle is live with query rights, and `code` is a writable local.
    if unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) } == 0 {
        return None;
    }
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: the handle is live with query rights, and all four outputs are writable locals.
    let timed = unsafe {
        GetProcessTimes(
            process.as_raw_handle(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    if timed == 0 {
        return None;
    }
    let time = (u64::from(exited.dwHighDateTime) << 32) | u64::from(exited.dwLowDateTime);
    Some((time, code))
}

/// `text` as NUL-terminated UTF-16.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

/// `text` as NUL-terminated UTF-16, or `None` if it holds a NUL of its own.
fn wide_path(text: &OsStr) -> Option<Vec<u16>> {
    let mut units: Vec<u16> = text.encode_wide().collect();
    if units.contains(&0) {
        return None;
    }
    units.push(0);
    Some(units)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(base: Instant, seconds: u64) -> Instant {
        base + Duration::from_secs(seconds)
    }

    fn restart(seconds: u64) -> Next {
        Next::RestartAfter(Duration::from_secs(seconds))
    }

    #[test]
    fn a_clean_exit_ends_the_supervisor() {
        let mut policy = RestartPolicy::new();
        assert_eq!(
            policy.on_empty(Instant::now(), Some(0), false),
            Next::Exit(0)
        );
    }

    #[test]
    fn a_busy_installer_ends_the_supervisor_even_after_a_failure() {
        let mut policy = RestartPolicy::new();
        let now = Instant::now();
        assert_eq!(policy.on_empty(now, Some(1), true), Next::Exit(0));
        assert_eq!(policy.on_empty(now, None, true), Next::Exit(0));
    }

    #[test]
    fn neither_a_clean_exit_nor_a_busy_installer_counts_as_a_failure() {
        let mut policy = RestartPolicy::new();
        let t0 = Instant::now();
        assert_eq!(policy.on_empty(t0, Some(0), false), Next::Exit(0));
        assert_eq!(policy.on_empty(t0, Some(1), true), Next::Exit(0));
        assert_eq!(policy.on_empty(t0, Some(1), false), restart(1));
    }

    #[test]
    fn failures_back_off_1_2_4_8_seconds() {
        let mut policy = RestartPolicy::new();
        let t0 = Instant::now();
        assert_eq!(policy.on_empty(t0, Some(1), false), restart(1));
        assert_eq!(policy.on_empty(secs(t0, 1), Some(1), false), restart(2));
        assert_eq!(policy.on_empty(secs(t0, 3), Some(1), false), restart(4));
        assert_eq!(policy.on_empty(secs(t0, 7), Some(1), false), restart(8));
    }

    #[test]
    fn the_fifth_failure_inside_the_window_gives_up() {
        let mut policy = RestartPolicy::new();
        let t0 = Instant::now();
        for (at, wait) in [(0, 1), (1, 2), (3, 4), (7, 8)] {
            assert_eq!(policy.on_empty(secs(t0, at), Some(1), false), restart(wait));
        }
        assert_eq!(policy.on_empty(secs(t0, 15), Some(1), false), Next::Exit(1));
    }

    #[test]
    fn failures_without_an_exit_code_count_too() {
        let mut policy = RestartPolicy::new();
        let t0 = Instant::now();
        assert_eq!(policy.on_empty(t0, None, false), restart(1));
        assert_eq!(policy.on_empty(secs(t0, 1), None, false), restart(2));
    }

    #[test]
    fn failures_spaced_apart_keep_restarting_after_one_second() {
        let mut policy = RestartPolicy::new();
        let t0 = Instant::now();
        for step in 0..8 {
            assert_eq!(
                policy.on_empty(secs(t0, step * 121), Some(1), false),
                restart(1)
            );
        }
    }

    #[test]
    fn a_failure_exactly_at_the_window_edge_still_counts() {
        let mut policy = RestartPolicy::new();
        let t0 = Instant::now();
        assert_eq!(policy.on_empty(t0, Some(1), false), restart(1));
        assert_eq!(policy.on_empty(secs(t0, 120), Some(1), false), restart(2));
        // One second later the first failure is older than the window.
        assert_eq!(policy.on_empty(secs(t0, 121), Some(1), false), restart(2));
    }

    #[test]
    fn the_latest_exit_decides() {
        assert_eq!(latest_exit([(10, 0), (20, 1)]), Some(1));
        assert_eq!(latest_exit([(20, 1), (10, 0)]), Some(1));
        assert_eq!(latest_exit([(20, 0), (10, 1)]), Some(0));
        assert_eq!(latest_exit([(7, 3)]), Some(3));
    }

    #[test]
    fn an_exact_tie_prefers_the_failing_exit() {
        assert_eq!(latest_exit([(5, 0), (5, 1)]), Some(1));
        assert_eq!(latest_exit([(5, 1), (5, 0)]), Some(1));
    }

    #[test]
    fn no_exits_means_no_exit_code() {
        assert_eq!(latest_exit([]), None);
    }

    #[test]
    fn a_running_agent_keeps_the_decision_open() {
        use Standing::{Ended, Running};
        assert_eq!(settled([Running]), None);
        assert_eq!(settled([Ended(Some((10, 1))), Running]), None);
        assert_eq!(settled([Running, Ended(Some((10, 0)))]), None);
        assert_eq!(settled([Ended(None), Running]), None);
    }

    #[test]
    fn the_last_agent_to_end_settles_with_the_latest_exit() {
        use Standing::Ended;
        // The agent restarts itself: the parent leaves with 0, the replacement later fails.
        assert_eq!(
            settled([Ended(Some((10, 0))), Ended(Some((20, 1)))]),
            Some(Some(1))
        );
        // The replacement was the user's clean quit.
        assert_eq!(
            settled([Ended(Some((10, 1))), Ended(Some((20, 0)))]),
            Some(Some(0))
        );
        assert_eq!(settled([Ended(Some((7, 3)))]), Some(Some(3)));
    }

    #[test]
    fn unreadable_exits_do_not_hold_the_decision_or_outvote_readable_ones() {
        use Standing::Ended;
        assert_eq!(settled([Ended(None)]), Some(None));
        assert_eq!(settled([Ended(None), Ended(Some((5, 2)))]), Some(Some(2)));
        assert_eq!(settled([]), Some(None));
    }

    #[test]
    fn only_the_installed_agent_image_is_tracked() {
        let agent = Path::new(r"X:\no\such\folder\Programs\Crosspane\crosspane-agent.exe");
        assert!(is_agent_image(Some(agent), agent));
        // The kernel's spelling of the same path: other case, extended-length prefix.
        assert!(is_agent_image(
            Some(Path::new(
                r"\\?\x:\NO\such\FOLDER\programs\crosspane\CROSSPANE-AGENT.EXE"
            )),
            agent
        ));
        // The settings window the agent starts into the same job, and a same-named file elsewhere.
        assert!(!is_agent_image(
            Some(Path::new(
                r"X:\no\such\folder\Programs\Crosspane\crosspane-ui.exe"
            )),
            agent
        ));
        assert!(!is_agent_image(
            Some(Path::new(r"X:\elsewhere\crosspane-agent.exe")),
            agent
        ));
    }

    #[test]
    fn an_unreadable_image_path_is_tracked_as_the_agent() {
        let agent = Path::new(r"X:\no\such\folder\Programs\Crosspane\crosspane-agent.exe");
        assert!(is_agent_image(None, agent));
    }

    #[test]
    fn only_the_flag_or_its_alias_alone_selects_the_supervisor() {
        let args = |items: &[&str]| items.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(is_supervisor(&args(&["--windows-supervisor"])));
        assert!(is_supervisor(&args(&["--supervisor"])));
        assert!(!is_supervisor(&args(&[])));
        assert!(!is_supervisor(&args(&["--install"])));
        assert!(!is_supervisor(&args(&["--supervisor", "--install"])));
        assert!(!is_supervisor(&args(&["--supervisor", "--supervisor"])));
    }

    #[test]
    fn wide_text_is_nul_terminated_and_paths_reject_embedded_nul() {
        assert_eq!(wide("ab"), vec![97, 98, 0]);
        assert_eq!(wide_path(OsStr::new("ab")), Some(vec![97, 98, 0]));
        assert_eq!(wide_path(OsStr::new("a\0b")), None);
    }
}
