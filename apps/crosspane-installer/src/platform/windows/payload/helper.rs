//! Fixed helper dispatch and journal-before-effect handoff. Catalog fields are correlations,
//! never a pathname, PID, native handle or lifecycle authority factory.
use super::super::native_io::{NativeError, NativeResult, process};
use super::recovery::{OperationRecord, Phase, RecoveryDecision};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;

pub(crate) const REPLACE_HELPER_ARGUMENT: &str = "--windows-replace-helper";

/// The binary handles this before any GUI or supervisor initialization. A helper-looking command
/// with extra arguments is rejected rather than falling through into an ordinary installer.
pub(crate) fn replace_helper_mode(arguments: &[OsString]) -> NativeResult<bool> {
    let helper = std::ffi::OsStr::new(REPLACE_HELPER_ARGUMENT);
    if arguments.iter().any(|argument| argument == helper) {
        if arguments.len() == 1 && arguments[0] == helper {
            Ok(true)
        } else {
            Err(NativeError::Invalid)
        }
    } else {
        Ok(false)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HandoffStage {
    CreateIntent,
    ChildCreated,
    ResumeIntent,
    Dispatched,
}

/// Only a handle explicitly inherited from the parent may satisfy this correlation. Deserializing
/// this record neither opens a PID/path nor creates a ParentExited or UpgradeStopProof.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HandoffRecord {
    pub(crate) inherited_parent_handle: u64,
    pub(crate) parent_pid: u32,
    pub(crate) parent_created: u64,
    pub(crate) parent_image: String,
    pub(crate) installer_size: u64,
    pub(crate) installer_sha256: [u8; 32],
    pub(crate) installer_machine: u16,
    pub(crate) installer_subsystem: u16,
    pub(crate) stage: HandoffStage,
}
impl std::fmt::Debug for HandoffRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HandoffRecord")
            .field("stage", &self.stage)
            .finish_non_exhaustive()
    }
}
impl HandoffRecord {
    pub(crate) fn validate(&self) -> NativeResult<()> {
        if self.inherited_parent_handle == 0
            || self.inherited_parent_handle >= u64::MAX - 15
            || self.inherited_parent_handle > usize::MAX as u64
            || !self.inherited_parent_handle.is_multiple_of(4)
            || self.parent_pid == 0
            || self.parent_created == 0
            || !(128..=super::inventory::MAX_IMAGE_BYTES).contains(&self.installer_size)
            || self.installer_sha256 == [0; 32]
            || !matches!(self.installer_machine, 0x8664 | 0xaa64)
            || !matches!(self.installer_subsystem, 2 | 3)
        {
            return Err(NativeError::Invalid);
        }
        process::literal_path(&self.parent_image)?;
        Ok(())
    }
    pub(crate) fn matches_image(&self, image: &super::inventory::PeFacts) -> NativeResult<()> {
        self.validate()?;
        if !image.valid() {
            return Err(NativeError::Invalid);
        }
        if image.size != self.installer_size
            || image.sha256 != self.installer_sha256
            || image.machine != self.installer_machine
            || image.subsystem != self.installer_subsystem
        {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
}

/// Bounded facts read from the actual inherited object before taking ownership.
pub(crate) struct ParentFacts {
    pub(crate) pid: u32,
    pub(crate) created: u64,
    pub(crate) image: String,
    pub(crate) inheritable: bool,
}
pub(crate) fn parent_matches(
    record: &HandoffRecord,
    current: &ParentFacts,
    own_pid: u32,
) -> NativeResult<()> {
    record.validate()?;
    if own_pid == 0
        || current.pid == 0
        || current.pid == own_pid
        || current.pid != record.parent_pid
        || current.created != record.parent_created
        || !current.inheritable
        || process::literal_path(&current.image)? != process::literal_path(&record.parent_image)?
    {
        return Err(NativeError::Foreign);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HelperExit {
    /// The selected recovery operation completed; helper-copy is still left for proven reopen
    /// cleanup. This result does not claim that the helper removed its own executing image.
    HandoffCompleted,
    RecoveryRetained,
}

/// Keeps the original exited parent object, never a recorded PID. This is deliberately unrelated
/// to UpgradeStopProof: parent exit cannot prove completion of the old supervisor's child/job.
pub(crate) struct ParentExited {
    operation: [u8; 16],
    #[cfg(windows)]
    parent: Option<std::sync::Arc<std::os::windows::io::OwnedHandle>>,
}
impl ParentExited {
    pub(crate) fn operation(&self) -> [u8; 16] {
        self.operation
    }
    pub(crate) fn reverify(
        &self,
        deadline: &super::super::native_io::Deadline,
    ) -> NativeResult<()> {
        deadline.check()?;
        #[cfg(windows)]
        if let Some(parent) = &self.parent {
            native::exited(parent)?;
        }
        Ok(())
    }
    #[cfg(test)]
    #[allow(dead_code)] // Source-included windows_upgrade fakes use this; library unit tests do not.
    pub(crate) fn fixture(operation: [u8; 16]) -> NativeResult<Self> {
        if operation == [0; 16] {
            return Err(NativeError::Invalid);
        }
        Ok(Self {
            operation,
            #[cfg(windows)]
            parent: None,
        })
    }
}

/// These functions are the production journal/effect sequence, also exercised by pure fakes.
/// The adapter prepares a real current-parent handle and a retained approved fixed helper image.
pub(crate) trait HandoffPort {
    type Child;
    fn prepare(&mut self) -> NativeResult<HandoffRecord>;
    fn save(&mut self, record: &HandoffRecord) -> NativeResult<()>;
    fn create_suspended(&mut self, record: &HandoffRecord) -> NativeResult<Self::Child>;
    fn resume(&mut self, child: &mut Self::Child) -> NativeResult<()>;
    /// Only an owned, never-resumed child is eligible. Success means native exit was observed.
    fn cancel_suspended(&mut self, child: &mut Self::Child) -> NativeResult<()>;
    fn retire(&mut self);
}

pub(crate) enum HandoffOutcome<C> {
    Dispatched(C),
    /// The caller retains the exact child owner, and must not retry the same creation/resume.
    RecoveryRetained(C),
}

pub(crate) fn drive_handoff<P: HandoffPort>(
    port: &mut P,
) -> NativeResult<HandoffOutcome<P::Child>> {
    let mut record = port.prepare()?;
    record.validate()?;
    record.stage = HandoffStage::CreateIntent;
    port.save(&record)?;
    let mut child = match port.create_suspended(&record) {
        Ok(child) => child,
        Err(error) => {
            // An uncertain create may already own a child in the adapter's retained owner.
            if error == NativeError::OutcomeUnknown {
                port.retire();
            }
            return Err(error);
        }
    };
    record.stage = HandoffStage::ChildCreated;
    if port.save(&record).is_err() {
        return stopped_before_resume(port, child);
    }
    record.stage = HandoffStage::ResumeIntent;
    if port.save(&record).is_err() {
        return stopped_before_resume(port, child);
    }
    // A dispatched resume is never retried, even when its result is not observed.
    if port.resume(&mut child).is_err() {
        port.retire();
        return Ok(HandoffOutcome::RecoveryRetained(child));
    }
    record.stage = HandoffStage::Dispatched;
    if port.save(&record).is_err() {
        port.retire();
        return Ok(HandoffOutcome::RecoveryRetained(child));
    }
    Ok(HandoffOutcome::Dispatched(child))
}
fn stopped_before_resume<P: HandoffPort>(
    port: &mut P,
    mut child: P::Child,
) -> NativeResult<HandoffOutcome<P::Child>> {
    port.retire();
    if port.cancel_suspended(&mut child).is_ok() {
        Err(NativeError::OutcomeUnknown)
    } else {
        Ok(HandoffOutcome::RecoveryRetained(child))
    }
}

pub(crate) trait HelperEntryPort {
    type Parent;
    fn select(&mut self) -> NativeResult<Option<OperationRecord>>;
    fn verify_self(&mut self, record: &HandoffRecord) -> NativeResult<()>;
    fn inherited_parent(&mut self, record: &HandoffRecord) -> NativeResult<Self::Parent>;
    fn wait_parent(
        &mut self,
        operation: [u8; 16],
        parent: Self::Parent,
    ) -> NativeResult<ParentExited>;
    /// Acquires the installer lock only after the verified original parent exits, then reselects.
    fn lock_and_reselect(&mut self) -> NativeResult<Option<OperationRecord>>;
    fn resume(
        &mut self,
        record: OperationRecord,
        parent: &ParentExited,
    ) -> NativeResult<RecoveryDecision>;
}

pub(crate) fn drive_entry<P: HelperEntryPort>(port: &mut P) -> NativeResult<HelperExit> {
    let Some(record) = port.select()? else {
        return Err(NativeError::Unsupported);
    };
    record.validate()?;
    if !matches!(
        record.phase(),
        Phase::HandoffIntent | Phase::HelperDispatched | Phase::ParentExited
    ) {
        return Err(NativeError::Unsupported);
    }
    let handoff = record.handoff().ok_or(NativeError::Unsupported)?.clone();
    handoff.validate()?;
    // The child may begin before the parent's post-resume result publication. It verifies the
    // original handle immediately, including when the parent has already exited.
    if !matches!(
        handoff.stage,
        HandoffStage::ResumeIntent | HandoffStage::Dispatched
    ) {
        return Err(NativeError::Unsupported);
    }
    port.verify_self(&handoff)?;
    let parent = port.inherited_parent(&handoff)?;
    let parent = port.wait_parent(record.operation(), parent)?;
    if parent.operation() != record.operation() {
        return Err(NativeError::Foreign);
    }
    let Some(current) = port.lock_and_reselect()? else {
        return Ok(HelperExit::RecoveryRetained);
    };
    current.validate()?;
    let Some(current_handoff) = current.handoff() else {
        return Ok(HelperExit::RecoveryRetained);
    };
    let mut expected = handoff;
    expected.stage = HandoffStage::Dispatched;
    if current.operation() != record.operation()
        || !matches!(
            current.phase(),
            Phase::HelperDispatched | Phase::ParentExited
        )
        || current_handoff != &expected
    {
        return Ok(HelperExit::RecoveryRetained);
    }
    // No Stop/StartIntent replay is permitted by the executor. Unsupported production service
    // completion remains retained until a4b supplies a genuine retained-owner completion port.
    match port.resume(current, &parent) {
        Ok(RecoveryDecision::Complete) => Ok(HelperExit::HandoffCompleted),
        Ok(_) | Err(NativeError::Unsupported | NativeError::OutcomeUnknown) => {
            Ok(HelperExit::RecoveryRetained)
        }
        Err(error) => Err(error),
    }
}

// The actual native owner uses this pure mutex-protected controller. Tests exercise these exact
// transitions without creating a process or native handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChildPhase {
    Prepared,
    Creating,
    Suspended,
    ResumeIntent,
    Running,
    Settling,
    Settled,
    Quarantined,
}
pub(crate) struct HandoffControl {
    pub(crate) phase: ChildPhase,
}
impl HandoffControl {
    pub(crate) fn prepare() -> Self {
        Self {
            phase: ChildPhase::Prepared,
        }
    }
    pub(crate) fn claim_create(&mut self, retired: bool) -> NativeResult<()> {
        if retired || self.phase != ChildPhase::Prepared {
            return Err(NativeError::OutcomeUnknown);
        }
        self.phase = ChildPhase::Creating;
        Ok(())
    }
    pub(crate) fn publish_created(&mut self, complete: bool, retired: bool) -> bool {
        if !complete || self.phase != ChildPhase::Creating {
            self.phase = ChildPhase::Quarantined;
            false
        } else if retired {
            self.phase = ChildPhase::Settling;
            true
        } else {
            self.phase = ChildPhase::Suspended;
            false
        }
    }
    pub(crate) fn claim_resume(&mut self, retired: bool) -> NativeResult<()> {
        if retired || self.phase != ChildPhase::Suspended {
            return Err(NativeError::OutcomeUnknown);
        }
        self.phase = ChildPhase::ResumeIntent;
        Ok(())
    }
    pub(crate) fn resume_observed(&mut self, previous: u32, retired: bool) -> NativeResult<()> {
        if previous != 1 || retired || self.phase != ChildPhase::ResumeIntent {
            self.phase = ChildPhase::Quarantined;
            return Err(NativeError::OutcomeUnknown);
        }
        self.phase = ChildPhase::Running;
        Ok(())
    }
    pub(crate) fn retire(&mut self) -> bool {
        match self.phase {
            ChildPhase::Suspended => {
                self.phase = ChildPhase::Settling;
                true
            }
            ChildPhase::Creating | ChildPhase::Settling | ChildPhase::Settled => false,
            _ => {
                self.phase = ChildPhase::Quarantined;
                false
            }
        }
    }
    pub(crate) fn settle_observed(&mut self, exited: bool) {
        if self.phase == ChildPhase::Settling {
            self.phase = if exited {
                ChildPhase::Settled
            } else {
                ChildPhase::Quarantined
            };
        }
    }
    pub(crate) fn quarantine(&mut self) {
        self.phase = ChildPhase::Quarantined;
    }
}

#[cfg(windows)]
pub(crate) fn replace_helper_entry() -> NativeResult<HelperExit> {
    native::entry()
}

/// New pre-Stop keeper wrappers; the old replacement helper/handoff remains unchanged.
#[cfg(all(windows, not(test)))]
pub(crate) fn prepare_keeper(
    io: std::sync::Arc<super::super::native_io::WindowsNativeIo>,
    proof: &super::super::native_io::SupportProof,
    lock: &super::super::native_io::InstallerLock,
    selected: &super::super::native_io::SelectedOuterOperation,
    own: &super::super::native_io::SelfImagePin,
    sources: super::ApprovedOuterSources,
    deadline: &super::super::native_io::Deadline,
) -> NativeResult<super::super::native_io::keeper::PreparedKeeper> {
    selected.reverify(&io, proof, lock, deadline)?;
    own.reverify(&io, proof, deadline)?;
    if own.identity() != selected.module().identity() || own.facts() != selected.module().facts() {
        return Err(NativeError::Foreign);
    }
    super::super::native_io::keeper::PreparedKeeper::prepare(
        io, proof, lock, selected, sources, deadline,
    )
}
#[cfg(all(windows, not(test)))]
pub(crate) fn launch_keeper(
    prepared: super::super::native_io::keeper::PreparedKeeper,
    deadline: &super::super::native_io::Deadline,
) -> NativeResult<super::super::native_io::keeper::KeeperChild> {
    prepared.launch(deadline)
}

// A4b will connect the retained lifecycle executor to this admitted fixed helper launcher.
#[cfg(windows)]
#[allow(unused_imports)] // A4b/A5 connect the fixed native launcher; a4 keeps it uninvoked.
pub(crate) use native::{HelperChild, launch};

#[cfg(windows)]
mod native {
    use super::super::super::native_io::{
        self, Cancellation, Deadline, InstallerLock, MonotonicClock, WindowsNativeIo,
    };
    use super::*;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::sync::Arc;
    use windows_sys::Win32::{Foundation::*, System::Threading::*};

    fn creation(handle: HANDLE) -> NativeResult<u64> {
        let mut created = FILETIME::default();
        let mut exited_at = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        // SAFETY: caller supplies a verified real process handle; all FILETIME outputs are owned.
        if unsafe { GetProcessTimes(handle, &mut created, &mut exited_at, &mut kernel, &mut user) }
            == 0
        {
            return Err(NativeError::Unavailable);
        }
        let value = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
        if value == 0 {
            return Err(NativeError::Foreign);
        }
        Ok(value)
    }
    fn image(handle: HANDLE) -> NativeResult<String> {
        let mut buffer = vec![0u16; 32768];
        let mut length = buffer.len() as u32;
        // SAFETY: query-only retained process handle and bounded initialized writable UTF-16 output.
        if unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut length) } == 0 {
            return Err(NativeError::Unavailable);
        }
        let value = String::from_utf16(buffer.get(..length as usize).ok_or(NativeError::Oversize)?)
            .map_err(|_| NativeError::Unavailable)?;
        process::literal_path(&value)
    }
    pub(super) fn exited(handle: &OwnedHandle) -> NativeResult<()> {
        // SAFETY: query the retained original process object without waiting or reopening its PID.
        match unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 => Ok(()),
            WAIT_TIMEOUT => Err(NativeError::Busy),
            _ => Err(NativeError::Unavailable),
        }
    }
    fn inherited(record: &HandoffRecord, deadline: &Deadline) -> NativeResult<Arc<OwnedHandle>> {
        record.validate()?;
        deadline.check()?;
        let raw = record.inherited_parent_handle as usize as HANDLE;
        let mut flags = 0;
        // SAFETY: read-only query of a candidate slot in this process, not ownership/adoption.
        if unsafe { GetHandleInformation(raw, &mut flags) } == 0 || flags & HANDLE_FLAG_INHERIT == 0
        {
            return Err(NativeError::Foreign);
        }
        // SAFETY: GetProcessId rejects a non-process handle. No PID is opened from the record.
        let pid = unsafe { GetProcessId(raw) };
        // SAFETY: read-only own PID, used solely to reject a self handle.
        let own_pid = unsafe { GetCurrentProcessId() };
        parent_matches(
            record,
            &ParentFacts {
                pid,
                created: creation(raw)?,
                image: image(raw)?,
                inheritable: flags & HANDLE_FLAG_INHERIT != 0,
            },
            own_pid,
        )?;
        deadline.check()?;
        // SAFETY: the exact explicitly inherited slot has now been verified as the original
        // non-self process object by PID, creation and image; ownership is adopted exactly once.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        let facts = native_io::identity::native::observe_process(&handle)?;
        let current = native_io::identity::native::current()?;
        if &facts != current.facts() {
            return Err(NativeError::Foreign);
        }
        Ok(Arc::new(handle))
    }
    use native_io::process::{CallOwner, Dispatch};
    use std::sync::{
        Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, Ordering},
    };

    struct ChildHandles {
        process: Option<Arc<OwnedHandle>>,
        thread: Option<Arc<OwnedHandle>>,
    }
    struct ChildState {
        control: HandoffControl,
        handles: Option<ChildHandles>,
    }
    /// Reserved once per process before dispatch. Native result delivery never owns the only
    /// child handles. Uncertain ownership cannot evade the cap by creating a new owner.
    struct HandoffOwner {
        io: Arc<WindowsNativeIo>,
        lock: Arc<InstallerLock>,
        helper: Arc<native_io::OpenedPe>,
        // Retain the self-measured source module through settlement/quarantine as well as its copy.
        _module: Arc<native_io::SelfImagePin>,
        parent: Arc<OwnedHandle>,
        calls: Arc<CallOwner>,
        retired: AtomicBool,
        state: Mutex<ChildState>,
    }
    static HANDOFF: OnceLock<Arc<HandoffOwner>> = OnceLock::new();
    pub(crate) struct HelperChild(Arc<HandoffOwner>);
    impl HandoffOwner {
        fn state(&self) -> MutexGuard<'_, ChildState> {
            match self.state.lock() {
                Ok(state) => state,
                Err(error) => {
                    // Poison is never treated as positive suspended/exited evidence. Recover the
                    // storage only to keep every actual owned handle alive under the fixed cap.
                    self.retired.store(true, Ordering::Release);
                    let mut state = error.into_inner();
                    state.control.quarantine();
                    state
                }
            }
        }
        fn retire(&self) {
            self.retired.store(true, Ordering::Release);
            let settle = {
                let mut state = self.state();
                state.control.retire()
            };
            if settle {
                self.settle_claimed();
            }
        }
        fn settle_claimed(&self) {
            let process = {
                let state = self.state();
                if state.control.phase != ChildPhase::Settling {
                    return;
                }
                state.handles.as_ref().and_then(|h| h.process.clone())
            };
            let Some(process) = process else {
                self.state().control.quarantine();
                return;
            };
            // SAFETY: this is the exact process returned by our one CreateProcess; the mutex
            // transition proves that no ResumeThread was dispatched. No recorded PID is used.
            let already_exited =
                unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } == WAIT_OBJECT_0;
            if !already_exited {
                // SAFETY: only the claimed positively never-resumed owned child is terminated.
                // TerminateProcess requests asynchronous termination; settlement is checked below.
                unsafe {
                    TerminateProcess(process.as_raw_handle(), 1);
                }
            }
            // SAFETY: retained exact child, independent cleanup bound, no mutex across the wait.
            let settled =
                unsafe { WaitForSingleObject(process.as_raw_handle(), 2000) } == WAIT_OBJECT_0;
            let mut state = self.state();
            state.control.settle_observed(settled);
            // Handles, image pins and lock stay in the owner even after this bounded observation.
        }
        fn create(self: &Arc<Self>, deadline: &Deadline) -> NativeResult<()> {
            {
                let mut state = self.state();
                state
                    .control
                    .claim_create(self.retired.load(Ordering::Acquire))?;
            }
            let owner = self.clone();
            let budget = deadline.clone();
            let result = self.calls.run(Dispatch::Mutation, deadline, move || {
                budget.check()?;
                let proof = owner.io.admit_support(&budget)?;
                owner.helper.reverify(&owner.io, &proof, &budget)?;
                let path = owner.helper.canonical_dos_path();
                if path.contains(['\0', '"']) {
                    return Err(NativeError::Foreign);
                }
                let application: Vec<u16> = path.encode_utf16().chain([0]).collect();
                let mut command: Vec<u16> = format!("\"{path}\" {REPLACE_HELPER_ARGUMENT}")
                    .encode_utf16()
                    .chain([0])
                    .collect();
                let mut attributes = Attributes::new(owner.parent.as_raw_handle())?;
                let mut startup = STARTUPINFOEXW::default();
                startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
                startup.lpAttributeList = attributes.pointer();
                let mut child = PROCESS_INFORMATION::default();
                budget.check()?;
                // SAFETY: non-null admitted fixed helper application; writable exact one-argument
                // command line; the initialized explicit HANDLE_LIST contains only our real own
                // inheritable parent handle. TRUE is required for that list, not blanket inheritance.
                let created = unsafe {
                    CreateProcessW(
                        application.as_ptr(),
                        command.as_mut_ptr(),
                        std::ptr::null(),
                        std::ptr::null(),
                        1,
                        CREATE_SUSPENDED | CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT,
                        std::ptr::null(),
                        std::ptr::null(),
                        &startup.StartupInfo,
                        &mut child,
                    )
                };
                if created == 0 {
                    return Err(NativeError::Unavailable);
                }
                let complete = !child.hProcess.is_null() && !child.hThread.is_null();
                // SAFETY: a successful CreateProcess transfers each non-null returned handle.
                // Optional storage preserves anomalous partial outputs in the same durable owner.
                let process = (!child.hProcess.is_null())
                    .then(|| Arc::new(unsafe { OwnedHandle::from_raw_handle(child.hProcess) }));
                // SAFETY: independently adopts only the distinct non-null returned thread handle.
                let thread = (!child.hThread.is_null())
                    .then(|| Arc::new(unsafe { OwnedHandle::from_raw_handle(child.hThread) }));
                let settle = {
                    let mut state = owner.state();
                    state.handles = Some(ChildHandles { process, thread });
                    let settle = state
                        .control
                        .publish_created(complete, owner.retired.load(Ordering::Acquire));
                    if state.control.phase == ChildPhase::Quarantined {
                        owner.retired.store(true, Ordering::Release);
                    }
                    settle
                };
                if settle {
                    owner.settle_claimed();
                }
                if owner.retired.load(Ordering::Acquire) {
                    return Err(NativeError::OutcomeUnknown);
                }
                budget.check()?;
                Ok(())
            });
            if result.is_err() {
                self.retire();
            }
            result
        }
        fn resume(self: &Arc<Self>, deadline: &Deadline) -> NativeResult<()> {
            let owner = self.clone();
            let budget = deadline.clone();
            let result = self.calls.run(Dispatch::Mutation, deadline, move || {
                budget.check()?;
                let thread = {
                    let mut state = owner.state();
                    let thread = state
                        .handles
                        .as_ref()
                        .and_then(|h| h.thread.clone())
                        .ok_or(NativeError::OutcomeUnknown)?;
                    // Same mutex as retirement. Once this state is entered, exact suspended
                    // cleanup is forbidden even if ResumeThread fails or result delivery is late.
                    state
                        .control
                        .claim_resume(owner.retired.load(Ordering::Acquire))?;
                    thread
                };
                // SAFETY: exact retained primary thread of our one suspended helper child.
                let previous = unsafe { ResumeThread(thread.as_raw_handle()) };
                let mut state = owner.state();
                if let Err(error) = state
                    .control
                    .resume_observed(previous, owner.retired.load(Ordering::Acquire))
                {
                    owner.retired.store(true, Ordering::Release);
                    return Err(error);
                }
                drop(state);
                budget.check()
            });
            if result.is_err() {
                self.retire();
            }
            result
        }
    }
    struct Attributes {
        storage: Vec<usize>,
        listed: Box<[HANDLE; 1]>,
        initialized: bool,
    }
    impl Attributes {
        fn new(parent: HANDLE) -> NativeResult<Self> {
            let mut bytes = 0;
            // SAFETY: documented size query; no buffer is supplied on this first call.
            unsafe {
                InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut bytes);
            }
            if bytes == 0 || bytes > 65536 {
                return Err(NativeError::Unavailable);
            }
            let mut value = Self {
                storage: vec![0; bytes.div_ceil(std::mem::size_of::<usize>())],
                listed: Box::new([parent]),
                initialized: false,
            };
            // SAFETY: aligned storage has the native-reported size and remains alive through CreateProcess.
            if unsafe { InitializeProcThreadAttributeList(value.pointer(), 1, 0, &mut bytes) } == 0
            {
                return Err(NativeError::Unavailable);
            }
            value.initialized = true;
            let listed = value.listed.as_ptr();
            // SAFETY: one real inheritable own parent handle. The attribute value must outlive the
            // list; boxed storage stays at this address until DeleteProcThreadAttributeList.
            if unsafe {
                UpdateProcThreadAttribute(
                    value.pointer(),
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    listed.cast(),
                    std::mem::size_of::<[HANDLE; 1]>(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            } == 0
            {
                return Err(NativeError::Unavailable);
            }
            Ok(value)
        }
        fn pointer(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
            self.storage.as_mut_ptr().cast()
        }
    }
    impl Drop for Attributes {
        fn drop(&mut self) {
            if self.initialized {
                // SAFETY: matches exactly the initialized native attribute list in retained storage.
                unsafe {
                    DeleteProcThreadAttributeList(self.pointer());
                }
            }
        }
    }
    struct NativePort {
        owner: Arc<HandoffOwner>,
        record: OperationRecord,
        handoff: HandoffRecord,
        deadline: Deadline,
    }
    impl HandoffPort for NativePort {
        type Child = HelperChild;
        fn prepare(&mut self) -> NativeResult<HandoffRecord> {
            if self.owner.retired.load(Ordering::Acquire) {
                return Err(NativeError::OutcomeUnknown);
            }
            let proof = self.owner.io.admit_support(&self.deadline)?;
            let selected =
                super::super::recovery::selected_operation(&self.owner.io, &proof, &self.deadline)?
                    .ok_or(NativeError::Unsupported)?;
            if serde_json::to_value(&selected).map_err(|_| NativeError::Invalid)?
                != serde_json::to_value(&self.record).map_err(|_| NativeError::Invalid)?
                || self.record.handoff().is_some()
                || !matches!(
                    self.record.phase(),
                    Phase::Staged | Phase::BackedUp | Phase::Published
                )
            {
                return Err(NativeError::Foreign);
            }
            Ok(self.handoff.clone())
        }
        fn save(&mut self, record: &HandoffRecord) -> NativeResult<()> {
            self.record.set_handoff(record.clone())?;
            self.record
                .set_phase(if record.stage == HandoffStage::Dispatched {
                    Phase::HelperDispatched
                } else {
                    Phase::HandoffIntent
                });
            let proof = self.owner.io.admit_support(&self.deadline)?;
            let permit = super::super::recovery::save_operation(
                self.owner.io.clone(),
                &proof,
                &self.owner.lock,
                &self.record,
                &self.deadline,
            )?;
            if permit.operation() != self.record.operation()
                || permit.phase() != self.record.phase()
            {
                return Err(NativeError::Foreign);
            }
            Ok(())
        }
        fn create_suspended(&mut self, _: &HandoffRecord) -> NativeResult<HelperChild> {
            self.owner.create(&self.deadline)?;
            Ok(HelperChild(self.owner.clone()))
        }
        fn resume(&mut self, child: &mut HelperChild) -> NativeResult<()> {
            if !Arc::ptr_eq(&self.owner, &child.0) {
                return Err(NativeError::Foreign);
            }
            self.owner.resume(&self.deadline)
        }
        fn cancel_suspended(&mut self, child: &mut HelperChild) -> NativeResult<()> {
            if !Arc::ptr_eq(&self.owner, &child.0) {
                return Err(NativeError::Foreign);
            }
            self.owner.retire();
            if self.owner.state().control.phase == ChildPhase::Settled {
                Ok(())
            } else {
                Err(NativeError::OutcomeUnknown)
            }
        }
        fn retire(&mut self) {
            self.owner.retire();
        }
    }
    #[allow(dead_code)] // The a4b retained-owner lifecycle executor will invoke this fixed launcher.
    pub(crate) fn launch(
        io: Arc<WindowsNativeIo>,
        lock: Arc<InstallerLock>,
        operation: OperationRecord,
        helper: native_io::OpenedPe,
        deadline: &Deadline,
    ) -> NativeResult<HandoffOutcome<HelperChild>> {
        operation.validate()?;
        let proof = io.admit_support(deadline)?;
        let own = io.self_image(&proof, deadline)?;
        let proof = io.admit_support(deadline)?;
        own.reverify(&io, &proof, deadline)?;
        helper.reverify(&io, &proof, deadline)?;
        let own_pin = super::super::inventory::ApprovedPe::own_image(&own)?;
        if !helper.approved().matches(&own_pin) {
            return Err(NativeError::Foreign);
        }
        let operation_hex = operation
            .operation()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let suffix = format!("\\payload-stage\\{operation_hex}\\helper-copy.exe");
        if !process::literal_path(helper.canonical_dos_path())?.ends_with(&suffix) {
            return Err(NativeError::Foreign);
        }
        // SAFETY: opens only this process's own real process object, with the query/wait rights
        // required by the child; explicit inheritability is needed by HANDLE_LIST. No recorded PID.
        let raw = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                1,
                GetCurrentProcessId(),
            )
        };
        if raw.is_null() {
            return Err(NativeError::Unavailable);
        }
        // SAFETY: successful OpenProcess returned one own real inheritable handle, not the pseudo handle.
        let parent = Arc::new(unsafe { OwnedHandle::from_raw_handle(raw) });
        let parent_image = image(raw)?;
        if parent_image != process::literal_path(own.canonical_dos_path())? {
            return Err(NativeError::Foreign);
        }
        // SAFETY: read-only exact own process handle, independently correlates actual creation and PID.
        let parent_pid = unsafe { GetProcessId(raw) };
        let facts = own.facts();
        let handoff = HandoffRecord {
            inherited_parent_handle: raw as usize as u64,
            parent_pid,
            parent_created: creation(raw)?,
            parent_image,
            installer_size: facts.size,
            installer_sha256: facts.sha256,
            installer_machine: facts.machine,
            installer_subsystem: facts.subsystem,
            stage: HandoffStage::CreateIntent,
        };
        handoff.validate()?;
        let owner = Arc::new(HandoffOwner {
            io,
            lock,
            helper: Arc::new(helper),
            _module: Arc::new(own),
            parent,
            calls: Arc::new(CallOwner::default()),
            retired: AtomicBool::new(false),
            state: Mutex::new(ChildState {
                control: HandoffControl::prepare(),
                handles: None,
            }),
        });
        HANDOFF.set(owner.clone()).map_err(|_| NativeError::Busy)?;
        let result = drive_handoff(&mut NativePort {
            owner: owner.clone(),
            record: operation,
            handoff,
            deadline: deadline.clone(),
        });
        if result.is_err() {
            owner.retire();
        }
        result
    }

    struct Entry {
        io: Arc<WindowsNativeIo>,
        deadline: Deadline,
        lock: Option<InstallerLock>,
    }
    impl HelperEntryPort for Entry {
        type Parent = Arc<OwnedHandle>;
        fn select(&mut self) -> NativeResult<Option<OperationRecord>> {
            let proof = self.io.admit_support(&self.deadline)?;
            super::super::recovery::active_helper_operation(&self.io, &proof, &self.deadline)
        }
        fn verify_self(&mut self, record: &HandoffRecord) -> NativeResult<()> {
            let proof = self.io.admit_support(&self.deadline)?;
            let own = self.io.self_image(&proof, &self.deadline)?;
            own.reverify(&self.io, &proof, &self.deadline)?;
            record.matches_image(own.facts())
        }
        fn inherited_parent(&mut self, record: &HandoffRecord) -> NativeResult<Self::Parent> {
            inherited(record, &self.deadline)
        }
        fn wait_parent(
            &mut self,
            operation: [u8; 16],
            parent: Self::Parent,
        ) -> NativeResult<ParentExited> {
            loop {
                let remaining = self.deadline.remaining_ms()?;
                // SAFETY: original verified retained parent handle; each wait is independently
                // bounded so cancellation/deadline cannot be hidden behind an indefinite wait.
                match unsafe {
                    WaitForSingleObject(parent.as_raw_handle(), remaining.clamp(1, 20) as u32)
                } {
                    WAIT_OBJECT_0 => {
                        self.deadline.check()?;
                        return Ok(ParentExited {
                            operation,
                            parent: Some(parent),
                        });
                    }
                    WAIT_TIMEOUT => {}
                    _ => return Err(NativeError::Unavailable),
                }
            }
        }
        fn lock_and_reselect(&mut self) -> NativeResult<Option<OperationRecord>> {
            let proof = self.io.admit_support(&self.deadline)?;
            self.lock = Some(self.io.acquire_installer_lock(&proof, &self.deadline)?);
            super::super::recovery::active_helper_operation(&self.io, &proof, &self.deadline)
        }
        fn resume(
            &mut self,
            record: OperationRecord,
            parent: &ParentExited,
        ) -> NativeResult<RecoveryDecision> {
            let proof = self.io.admit_support(&self.deadline)?;
            parent.reverify(&self.deadline)?;
            // Transfer our actual lock; neither the helper trait nor the parent-exit capability
            // changes. The owned coordinator may release this lock before waiting for readiness.
            let lock = self.lock.take().ok_or(NativeError::Foreign)?;
            super::super::recovery::resume_helper_owned(
                self.io.clone(),
                &proof,
                lock,
                record,
                parent,
                &self.deadline,
            )
        }
    }
    pub(super) fn entry() -> NativeResult<HelperExit> {
        // A missing producer manifest cannot be replaced by own current bytes or catalog pins.
        let _inventory = super::super::inventory::ApprovedInventory::embedded()?;
        let clock: Arc<dyn native_io::Clock> = Arc::new(MonotonicClock::default());
        let deadline = Deadline::new(30_000, clock.clone(), Cancellation::default())?;
        let io = Arc::new(WindowsNativeIo::current(clock, &deadline)?);
        // Same-context classification grants no file authority and preserves the original
        // parent-before-lock order. Cold eligibility is fully reselected under a genuine lock.
        #[cfg(not(test))]
        super::super::recover_prior_logon_files_for_entry(&io, &deadline)?;
        drive_entry(&mut Entry {
            io,
            deadline,
            lock: None,
        })
    }
}
