//! Bounded current-process observations and native dispatch; no arbitrary process selection.

use super::{NativeError, NativeResult};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

pub const MAX_NATIVE_TIMEOUT_MS: u64 = 120_000;

/// Portable comparisons of observations, never constructors for native target authority.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ProcessFacts {
    pub token: super::identity::TokenFacts,
    pub pid: u32,
    pub created: u64,
    pub alive: bool,
    pub image: String,
    /// The separately pinned fixed leaf, not the mapped image section's queried identity.
    pub file: super::files::FileIdentity,
}
impl std::fmt::Debug for ProcessFacts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProcessFacts")
    }
}
/// Literal equivalent DOS spellings only; never opens an observed pathname or follows aliases.
pub(crate) fn literal_path(value: &str) -> NativeResult<String> {
    if value.len() > crate::agent_contract::MAX_STRING_BYTES || value.contains('\0') {
        return Err(NativeError::Foreign);
    }
    let path = value.replace('/', "\\");
    let path = path.strip_prefix(r"\\?\").unwrap_or(&path);
    if path.len() < 3
        || !path.as_bytes()[0].is_ascii_uppercase()
        || &path.as_bytes()[1..3] != b":\\"
    {
        return Err(NativeError::Foreign);
    }
    if path[3..].split('\\').any(|part| {
        part.is_empty()
            || part == "."
            || part == ".."
            || part.contains(':')
            || part.ends_with(['.', ' '])
    }) {
        return Err(NativeError::Foreign);
    }
    Ok(path.to_owned())
}
pub(crate) fn process_matches(expected: &ProcessFacts, current: &ProcessFacts) -> NativeResult<()> {
    super::identity::LimitedIdentity::admit(current.token.clone())
        .map_err(|_| NativeError::Foreign)?;
    if expected.token != current.token
        || current.pid == 0
        || current.pid != expected.pid
        || !current.alive
        || !expected.alive
        || current.created == 0
        || current.created != expected.created
        || current.file != expected.file
        || current.file.volume == 0
        || current.file.file == [0; 16]
        || literal_path(&current.image)? != literal_path(&expected.image)?
    {
        return Err(NativeError::Foreign);
    }
    Ok(())
}
pub(crate) fn bootstrap_matches(
    expected: &crate::agent_contract::BootstrapV1,
    current: &crate::agent_contract::BootstrapV1,
) -> NativeResult<()> {
    use crate::agent_contract::BootstrapPhase;
    if expected.schema_version != 1
        || current.schema_version != 1
        || expected.instance_id == 0
        || expected.pid == 0
        || current.instance_id != expected.instance_id
        || current.pid != expected.pid
        || current.started_unix_ms != expected.started_unix_ms
        || current.phase_seq < expected.phase_seq
        || current.phase == BootstrapPhase::Failed
        || expected.phase == BootstrapPhase::Failed
        || (expected.phase == BootstrapPhase::Ready && current.phase != BootstrapPhase::Ready)
        || (current.phase_seq == expected.phase_seq && current.phase != expected.phase)
        || literal_path(&current.runtime_dir)? != literal_path(&expected.runtime_dir)?
    {
        return Err(NativeError::Foreign);
    }
    Ok(())
}
pub(crate) fn status_matches(
    bootstrap: &crate::agent_contract::BootstrapV1,
    image: &str,
    status: &crate::agent_contract::InstanceStatus,
) -> NativeResult<()> {
    use crate::agent_contract::BootstrapPhase;
    bootstrap_matches(bootstrap, bootstrap)?;
    if bootstrap.phase != BootstrapPhase::Ready
        || status.uid.is_some()
        || status.id != bootstrap.instance_id
        || status.pid != bootstrap.pid
        || status.started_unix_ms != bootstrap.started_unix_ms
        || literal_path(&status.exe)? != literal_path(image)?
        || literal_path(&status.runtime_dir)? != literal_path(&bootstrap.runtime_dir)?
    {
        return Err(NativeError::Foreign);
    }
    Ok(())
}
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}
#[derive(Debug)]
pub struct MonotonicClock(Instant);
impl Default for MonotonicClock {
    fn default() -> Self {
        Self(Instant::now())
    }
}
impl Clock for MonotonicClock {
    fn now_ms(&self) -> u64 {
        self.0.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    }
}
#[derive(Clone, Default, Debug)]
pub struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}
#[derive(Clone)]
pub struct Deadline {
    clock: Arc<dyn Clock>,
    end: u64,
    wall: Instant,
    timeout_ms: u64,
    cancel: Cancellation,
}
impl std::fmt::Debug for Deadline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Deadline")
    }
}
impl Deadline {
    pub fn new(timeout_ms: u64, clock: Arc<dyn Clock>, cancel: Cancellation) -> NativeResult<Self> {
        if timeout_ms == 0 || timeout_ms > MAX_NATIVE_TIMEOUT_MS {
            return Err(NativeError::Invalid);
        }
        let end = clock
            .now_ms()
            .checked_add(timeout_ms)
            .ok_or(NativeError::Invalid)?;
        Ok(Self {
            clock,
            end,
            wall: Instant::now(),
            timeout_ms,
            cancel,
        })
    }
    pub fn check(&self) -> NativeResult<()> {
        if self.cancel.is_cancelled() {
            Err(NativeError::Cancelled)
        } else if self.clock.now_ms() >= self.end
            || self.wall.elapsed() >= Duration::from_millis(self.timeout_ms)
        {
            Err(NativeError::Timeout)
        } else {
            Ok(())
        }
    }
    pub fn remaining_ms(&self) -> NativeResult<u64> {
        self.check()?;
        Ok(self.end.saturating_sub(self.clock.now_ms()).min(
            self.timeout_ms
                .saturating_sub(self.wall.elapsed().as_millis() as u64),
        ))
    }
    pub(crate) fn shorten(&self, maximum_ms: u64) -> NativeResult<Self> {
        self.check()?;
        if maximum_ms == 0 {
            return Err(NativeError::Timeout);
        }
        let mut value = self.clone();
        value.end = value.end.min(
            self.clock
                .now_ms()
                .checked_add(maximum_ms)
                .ok_or(NativeError::Invalid)?,
        );
        let wall_end = (self.wall.elapsed().as_millis() as u64)
            .checked_add(maximum_ms)
            .ok_or(NativeError::Invalid)?;
        value.timeout_ms = value.timeout_ms.min(wall_end);
        Ok(value)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dispatch {
    Observation,
    Mutation,
}
/// At most one submitted call. Timing out does not cancel a Win32 call or release its handles.
/// After uncertain mutation this owner refuses any further mutation for its entire lifetime.
/// Reopening requires fresh native admission and acquisition of the still-retained installer lock.
#[derive(Default)]
pub(crate) struct CallOwner {
    busy: AtomicBool,
    uncertain: AtomicBool,
    #[cfg(test)]
    delivery_hook: std::sync::Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}
struct Flight(Arc<CallOwner>);
impl Drop for Flight {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::Release);
    }
}
impl CallOwner {
    pub(crate) fn idle(&self) -> bool {
        !self.busy.load(Ordering::Acquire)
    }
    pub(crate) fn retire_mutations(&self) {
        self.uncertain.store(true, Ordering::Release);
    }
    #[cfg(test)]
    #[allow(dead_code)] // Exercised by the source-included integration seam, not library tests.
    pub(crate) fn before_delivery(&self, hook: Arc<dyn Fn() + Send + Sync>) -> NativeResult<()> {
        *self
            .delivery_hook
            .lock()
            .map_err(|_| NativeError::Unavailable)? = Some(hook);
        Ok(())
    }
    pub(crate) fn run<T: Send + 'static>(
        self: &Arc<Self>,
        kind: Dispatch,
        deadline: &Deadline,
        work: impl FnOnce() -> NativeResult<T> + Send + 'static,
    ) -> NativeResult<T> {
        deadline.check()?;
        if kind == Dispatch::Mutation && self.uncertain.load(Ordering::Acquire) {
            return Err(NativeError::OutcomeUnknown);
        }
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(NativeError::Busy);
        }
        let flight = Flight(self.clone());
        // A previous worker may have retired mutations between our first check and the CAS.
        if kind == Dispatch::Mutation && self.uncertain.load(Ordering::Acquire) {
            return Err(NativeError::OutcomeUnknown);
        }
        let (send, receive) = mpsc::sync_channel(1);
        let dispatched_deadline = deadline.clone();
        let delivery_owner = self.clone();
        let thread = std::thread::Builder::new()
            .name("crosspane-installer-native".into())
            .spawn(move || {
                let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    dispatched_deadline.check().and_then(|()| work())
                })) {
                    Ok(result) => result,
                    Err(_) => Err(NativeError::OutcomeUnknown),
                };
                // Retire BEFORE releasing the busy slot or delivering the result. Otherwise
                // another caller could enter while this caller is still waiting to wake.
                if kind == Dispatch::Mutation && matches!(result, Err(NativeError::OutcomeUnknown))
                {
                    delivery_owner.retire_mutations();
                }
                drop(flight);
                #[cfg(test)]
                {
                    let hook = delivery_owner
                        .delivery_hook
                        .lock()
                        .ok()
                        .and_then(|slot| slot.clone());
                    if let Some(hook) = hook {
                        hook();
                    }
                }
                let _ = send.send(result);
            });
        if thread.is_err() {
            return Err(NativeError::Unavailable);
        }
        loop {
            let remaining = match deadline.remaining_ms() {
                Ok(v) => v,
                Err(e) => return self.abandoned(kind, e),
            };
            match receive.recv_timeout(Duration::from_millis(remaining.clamp(1, 10))) {
                Ok(result) => {
                    if let Err(error) = deadline.check() {
                        return self.abandoned(kind, error);
                    }
                    if kind == Dispatch::Mutation
                        && matches!(result, Err(NativeError::OutcomeUnknown))
                    {
                        self.uncertain.store(true, Ordering::Release);
                    }
                    return result;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return self.abandoned(kind, NativeError::Unavailable);
                }
            }
        }
    }
    fn abandoned<T>(&self, kind: Dispatch, error: NativeError) -> NativeResult<T> {
        if kind == Dispatch::Mutation {
            self.uncertain.store(true, Ordering::Release);
            Err(NativeError::OutcomeUnknown)
        } else {
            Err(error)
        }
    }
}

#[cfg(windows)]
pub(crate) mod selected {
    use super::super::{
        files::FileIdentity,
        identity::{self, LimitedIdentity, TokenFacts},
    };
    use super::*;
    use crate::agent_contract::BootstrapV1;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{Foundation::*, System::Threading::*};

    /// Retains the original kernel process object and raw creation FILETIME, not just its PID.
    pub(crate) struct SelectedProcess {
        handle: OwnedHandle,
        facts: ProcessFacts,
    }
    pub(crate) enum ProcessExit {
        Running,
        Exited { creation: u64, code: u32 },
    }
    impl SelectedProcess {
        /// Original raw creation FILETIME of the retained, admitted kernel process.
        // A4b native supervisor generation consumes this newly admitted read-only fact.
        #[allow(dead_code)]
        pub(crate) fn creation_time(&self) -> u64 {
            self.facts.created
        }
        /// Duplicates ONLY the already-admitted live original object, never a PID or record.
        /// Lead f912268c: this readonly capability supplies same-job clean-successor retention;
        /// it confers no execution/image/assignment/termination authority.
        // Test builds exclude the native supervisor consumers of this new retained capability.
        #[cfg_attr(test, allow(dead_code, unused_imports))]
        pub(crate) fn duplicate_retained(
            &self,
            deadline: &Deadline,
        ) -> NativeResult<Arc<OwnedHandle>> {
            self.revalidate(deadline)?;
            let mut raw = std::ptr::null_mut();
            // SAFETY: source is the retained original process; target is this process, minimal
            // query/synchronize rights and no inheritance. No SAME_ACCESS or new PID lookup.
            let duplicated = unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    self.handle.as_raw_handle(),
                    GetCurrentProcess(),
                    &mut raw,
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                    0,
                    0,
                )
            };
            if duplicated == 0 || raw.is_null() {
                return Err(NativeError::Unavailable);
            }
            // SAFETY: successful non-null duplication transfers this one real readonly handle.
            let handle = Arc::new(unsafe { OwnedHandle::from_raw_handle(raw) });
            self.revalidate(deadline)?;
            deadline.check()?;
            Ok(handle)
        }
        /// Called only inside the fixed AgentObservation factory's bounded native owner.
        pub(crate) fn admit(
            bootstrap: &BootstrapV1,
            token: &TokenFacts,
            image: &str,
            file: FileIdentity,
            deadline: &Deadline,
        ) -> NativeResult<Self> {
            deadline.check()?;
            if bootstrap.pid == 0 {
                return Err(NativeError::Foreign);
            }
            // SAFETY: query/synchronize-only non-inheritable handle for the fixed bootstrap PID.
            let raw = unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                    0,
                    bootstrap.pid,
                )
            };
            if raw.is_null() {
                #[cfg(not(test))]
                // SAFETY: read thread-local last-error immediately after this failed OpenProcess;
                // capture is inert outside the actual worker's scoped read-only repair diagnostic.
                super::super::files::native::repair_capture_error(unsafe { GetLastError() });
                return Err(NativeError::Unavailable);
            }
            // SAFETY: successful OpenProcess transferred one owned non-pseudo process handle.
            let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
            let current = Self::observe(&handle, bootstrap.pid, file, deadline)?;
            let expected = ProcessFacts {
                token: token.clone(),
                image: super::literal_path(image)?,
                ..current.clone()
            };
            process_matches(&expected, &current)?;
            deadline.check()?;
            Ok(Self {
                handle,
                facts: current,
            })
        }
        pub(crate) fn revalidate(&self, deadline: &Deadline) -> NativeResult<()> {
            let current = Self::observe(&self.handle, self.facts.pid, self.facts.file, deadline)?;
            process_matches(&self.facts, &current)
        }
        /// Query the retained original kernel object. An exited PID is never reopened.
        pub(crate) fn observe_exit(&self, deadline: &Deadline) -> NativeResult<ProcessExit> {
            deadline.check()?;
            // SAFETY: original retained query/synchronize handle; zero wait, no mutation.
            let state = unsafe { WaitForSingleObject(self.handle.as_raw_handle(), 0) };
            if state == WAIT_TIMEOUT {
                self.revalidate(deadline)?;
                return Ok(ProcessExit::Running);
            }
            if state != WAIT_OBJECT_0 {
                return Err(NativeError::Unavailable);
            }
            let (mut creation, mut exit, mut kernel, mut user) = (
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
            );
            // SAFETY: retained original query handle and complete distinct writable outputs.
            if unsafe {
                GetProcessTimes(
                    self.handle.as_raw_handle(),
                    &mut creation,
                    &mut exit,
                    &mut kernel,
                    &mut user,
                )
            } == 0
            {
                #[cfg(not(test))]
                // SAFETY: immediate last-error read after this actual failed readonly SDK query.
                super::super::files::native::repair_capture_error(unsafe { GetLastError() });
                return Err(NativeError::Unavailable);
            }
            let creation =
                (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
            if creation == 0 || creation != self.facts.created {
                return Err(NativeError::Foreign);
            }
            let mut code = 0;
            // SAFETY: signalled retained original process and complete exit-code output.
            if unsafe { GetExitCodeProcess(self.handle.as_raw_handle(), &mut code) } == 0 {
                return Err(NativeError::Unavailable);
            }
            deadline.check()?;
            Ok(ProcessExit::Exited { creation, code })
        }
        /// Query only the explicitly retained service-owned job; never a null/current job.
        #[allow(dead_code)] // Lead750c0da2 holds the actual owned-job caller until A4.
        pub(crate) fn in_job(&self, job: &OwnedHandle, deadline: &Deadline) -> NativeResult<bool> {
            if job.as_raw_handle().is_null() {
                return Err(NativeError::Foreign);
            }
            self.observe_exit(deadline)?;
            let mut result = 0;
            // SAFETY: both original process and actual non-null own job stay retained by the
            // enclosing observation owner until this query completes, including caller timeout.
            if unsafe {
                windows_sys::Win32::System::JobObjects::IsProcessInJob(
                    self.handle.as_raw_handle(),
                    job.as_raw_handle(),
                    &mut result,
                )
            } == 0
            {
                return Err(NativeError::Unavailable);
            }
            self.observe_exit(deadline)?;
            deadline.check()?;
            Ok(result != 0)
        }
        fn observe(
            handle: &OwnedHandle,
            pid: u32,
            file: FileIdentity,
            deadline: &Deadline,
        ) -> NativeResult<ProcessFacts> {
            deadline.check()?;
            // SAFETY: retained original process handle, zero wait, no signal or termination.
            if unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } != WAIT_TIMEOUT {
                return Err(NativeError::Foreign);
            }
            let (mut creation, mut exit, mut kernel, mut user) = (
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
            );
            // SAFETY: retained query handle and four distinct complete writable FILETIME values.
            if unsafe {
                GetProcessTimes(
                    handle.as_raw_handle(),
                    &mut creation,
                    &mut exit,
                    &mut kernel,
                    &mut user,
                )
            } == 0
            {
                #[cfg(not(test))]
                // SAFETY: immediate last-error read after this actual failed readonly SDK query.
                super::super::files::native::repair_capture_error(unsafe { GetLastError() });
                return Err(NativeError::Unavailable);
            }
            let created =
                (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
            if created == 0 {
                return Err(NativeError::Foreign);
            }
            let token = identity::native::observe_process(handle)?;
            LimitedIdentity::admit(token.clone()).map_err(|_| NativeError::Foreign)?;
            let mut path = vec![0u16; 32768];
            let mut length = path.len() as u32;
            // SAFETY: query-only original process and complete bounded writable path buffer.
            if unsafe {
                QueryFullProcessImageNameW(
                    handle.as_raw_handle(),
                    0,
                    path.as_mut_ptr(),
                    &mut length,
                )
            } == 0
                || length == 0
                || length as usize >= path.len()
            {
                return Err(NativeError::Unavailable);
            }
            let image = super::literal_path(
                &String::from_utf16(&path[..length as usize]).map_err(|_| NativeError::Foreign)?,
            )?;
            deadline.check()?;
            // SAFETY: same retained process, rechecked after all observations to reject an exit.
            if unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } != WAIT_TIMEOUT {
                return Err(NativeError::Foreign);
            }
            Ok(ProcessFacts {
                token,
                pid,
                created,
                alive: true,
                image,
                file,
            })
        }
    }
}

/// A real retained object for THIS Limited process only; never a PID/journal-selection factory.
#[cfg(windows)]
// Test builds exclude the native activation/supervisor graph that owns these new capabilities.
#[cfg_attr(test, allow(dead_code, unused_imports))]
pub(crate) mod own {
    use super::super::identity::{self, LimitedIdentity, TokenFacts};
    use super::*;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{Foundation::*, System::Threading::*};

    pub(crate) struct OwnProcessIdentity {
        handle: Arc<OwnedHandle>,
        expected: TokenFacts,
        pid: u32,
        creation: u64,
    }
    impl OwnProcessIdentity {
        pub(crate) fn current(expected: &TokenFacts, deadline: &Deadline) -> NativeResult<Self> {
            deadline.check()?;
            identity::native::refuse_impersonation()?;
            LimitedIdentity::admit(expected.clone()).map_err(|_| NativeError::Foreign)?;
            let mut raw = std::ptr::null_mut();
            // SAFETY: duplicate only our own process pseudo-handle into THIS process; minimal
            // query/synchronize rights, no inheritance, and complete owned-handle output.
            let duplicated = unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    GetCurrentProcess(),
                    GetCurrentProcess(),
                    &mut raw,
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                    0,
                    0,
                )
            };
            if duplicated == 0 || raw.is_null() {
                return Err(NativeError::Unavailable);
            }
            // SAFETY: successful non-null duplication transfers one real owned handle.
            let handle = Arc::new(unsafe { OwnedHandle::from_raw_handle(raw) });
            let current = identity::native::observe_process(&handle)?;
            LimitedIdentity::admit(current.clone()).map_err(|_| NativeError::Foreign)?;
            if &current != expected {
                return Err(NativeError::Foreign);
            }
            // SAFETY: retained query handle for our actual original process.
            let pid = unsafe { GetProcessId(handle.as_raw_handle()) };
            let creation = creation(&handle)?;
            if pid == 0 || creation == 0 {
                return Err(NativeError::Foreign);
            }
            let owned = Self {
                handle,
                expected: expected.clone(),
                pid,
                creation,
            };
            owned.reverify(deadline)?;
            Ok(owned)
        }
        pub(crate) fn pid(&self) -> u32 {
            self.pid
        }
        pub(crate) fn creation(&self) -> u64 {
            self.creation
        }
        /// Handle view is only available on this sealed current-process object; no raw borrow.
        pub(crate) fn handle(&self) -> Arc<OwnedHandle> {
            self.handle.clone()
        }
        pub(crate) fn reverify(&self, deadline: &Deadline) -> NativeResult<()> {
            deadline.check()?;
            // SAFETY: actual retained original query/synchronize process, nonblocking wait.
            if unsafe { WaitForSingleObject(self.handle.as_raw_handle(), 0) } != WAIT_TIMEOUT {
                return Err(NativeError::Foreign);
            }
            // SAFETY: original retained query handle, never OpenProcess by a stored PID.
            if unsafe { GetProcessId(self.handle.as_raw_handle()) } != self.pid
                || creation(&self.handle)? != self.creation
            {
                return Err(NativeError::Foreign);
            }
            let facts = identity::native::observe_process(&self.handle)?;
            LimitedIdentity::admit(facts.clone()).map_err(|_| NativeError::Foreign)?;
            if facts != self.expected {
                return Err(NativeError::Foreign);
            }
            deadline.check()
        }
    }
    fn creation(handle: &OwnedHandle) -> NativeResult<u64> {
        let (mut created, mut exited, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        // SAFETY: retained query process and four complete, distinct writable FILETIME outputs.
        if unsafe {
            GetProcessTimes(
                handle.as_raw_handle(),
                &mut created,
                &mut exited,
                &mut kernel,
                &mut user,
            )
        } == 0
        {
            return Err(NativeError::Unavailable);
        }
        Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }
}
