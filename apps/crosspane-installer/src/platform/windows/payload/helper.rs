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
        #[cfg(not(test))]
        if super::super::super::service::removal_entry_for_helper(io.clone(), &deadline)? {
            return Ok(HelperExit::RecoveryRetained);
        }
        drive_entry(&mut Entry {
            io,
            deadline,
            lock: None,
        })
    }
}

/// Distinct planned-removal copy/entry. None of the old helper/upgrade constructors is widened.
#[cfg(all(windows, not(test)))]
pub(crate) mod removal {
    use super::super::super::{
        native_io::{
            self, Deadline, InstallerLock, NativeError, NativeResult, OpenedPe, SupportProof,
            WindowsNativeIo, files::FileIdentity,
        },
        removal::{RemovalHandoffStage, RemovalRecord, inventory::RemovalCopyKind},
    };
    use std::{
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
        sync::{Arc, Mutex, OnceLock},
        time::Duration,
    };
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use windows_sys::Win32::{
        Foundation::*,
        Security::{Authorization::*, *},
        System::{JobObjects::IsProcessInJob, Pipes::*, Threading::*},
    };

    fn process_tuple(process: &OwnedHandle) -> NativeResult<(u32, u64)> {
        let (mut creation, mut exit, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        // SAFETY: exact originally retained process object and complete writable time outputs.
        if unsafe {
            GetProcessTimes(
                process.as_raw_handle(),
                &mut creation,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        } == 0
        {
            return Err(NativeError::Unavailable);
        }
        // SAFETY: query only the same retained original object; never a PID reopen.
        let pid = unsafe { GetProcessId(process.as_raw_handle()) };
        let created =
            (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
        if pid == 0 || created == 0 {
            return Err(NativeError::Foreign);
        }
        Ok((pid, created))
    }
    fn exited(process: &OwnedHandle) -> bool {
        // SAFETY: nonblocking observation of the exact retained process object.
        (unsafe { WaitForSingleObject(process.as_raw_handle(), 0) }) == WAIT_OBJECT_0
    }
    /// Actual explicitly inherited original parent, no serialized PID/path selection.
    pub(crate) struct RemovalParent {
        io: Arc<WindowsNativeIo>,
        process: Arc<OwnedHandle>,
        pid: u32,
        created: u64,
        operation: [u8; 16],
    }
    impl RemovalParent {
        fn inherited(
            io: Arc<WindowsNativeIo>,
            proof: &SupportProof,
            record: &RemovalRecord,
            index: u8,
            deadline: &Deadline,
        ) -> NativeResult<Self> {
            proof.check(&io, deadline)?;
            let copy = record
                .plan()
                .copies()
                .get(usize::from(index))
                .ok_or(NativeError::Foreign)?;
            let value = copy.inherited_parent_handle().ok_or(NativeError::Foreign)?;
            if value < 4
                || value > usize::MAX as u64
                || value >= usize::MAX as u64 - 3
                || !value.is_multiple_of(4)
            {
                return Err(NativeError::Foreign);
            }
            let raw = value as usize as std::os::windows::io::RawHandle;
            let mut flags = 0;
            // SAFETY: query the bounded inherited handle value before adopting ownership.
            if unsafe { GetHandleInformation(raw, &mut flags) } == 0
                || flags & HANDLE_FLAG_INHERIT == 0
            {
                return Err(NativeError::Foreign);
            }
            let mut owned = std::ptr::null_mut();
            // SAFETY: source is the actual inherited process handle in THIS process. Restrict the
            // local noninheritable duplicate to query/synchronize; no serialized PID is opened.
            if unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    raw,
                    GetCurrentProcess(),
                    &mut owned,
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                    0,
                    0,
                )
            } == 0
            {
                return Err(NativeError::Foreign);
            }
            // SAFETY: successful DuplicateHandle transferred one real local owned handle.
            let process = Arc::new(unsafe { OwnedHandle::from_raw_handle(owned) });
            let (pid, created) = process_tuple(&process)?;
            // SAFETY: this process's ID only, not a selection/scan.
            if pid == unsafe { GetCurrentProcessId() }
                || native_io::identity::native::observe_process(&process)?
                    != *io.target().identity()
            {
                return Err(NativeError::Foreign);
            }
            // SAFETY: the validated original inherited handle is now represented by our restricted
            // duplicate. No future code uses the raw correlation as authority.
            if unsafe { CloseHandle(raw) } == 0 {
                return Err(NativeError::OutcomeUnknown);
            }
            let parent = Self {
                io,
                process,
                pid,
                created,
                operation: record.operation(),
            };
            parent.reverify(&parent.io, proof, record.operation(), deadline)?;
            Ok(parent)
        }
        pub(crate) fn retained_process(&self) -> &Arc<OwnedHandle> {
            &self.process
        }
        pub(crate) fn reverify(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            operation: [u8; 16],
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if !std::ptr::eq(io, self.io.as_ref()) || operation != self.operation {
                return Err(NativeError::Foreign);
            }
            proof.check(io, deadline)?;
            if process_tuple(&self.process)? != (self.pid, self.created) {
                return Err(NativeError::Foreign);
            }
            deadline.check()
        }
        pub(crate) fn has_exited(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<bool> {
            self.reverify(io, proof, self.operation, deadline)?;
            Ok(exited(&self.process))
        }
    }
    /// Positive exit of a SAME actual created/admitted copy object. No native fact constructor.
    pub(crate) struct RemovalCopyExit {
        io: Arc<WindowsNativeIo>,
        process: Arc<OwnedHandle>,
        pid: u32,
        created: u64,
        operation: [u8; 16],
        index: u8,
        identity: FileIdentity,
    }
    impl RemovalCopyExit {
        fn from_child(
            io: Arc<WindowsNativeIo>,
            process: Arc<OwnedHandle>,
            operation: [u8; 16],
            index: u8,
            identity: FileIdentity,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<Self> {
            let (pid, created) = process_tuple(&process)?;
            let cap = Self {
                io,
                process,
                pid,
                created,
                operation,
                index,
                identity,
            };
            cap.reverify(&cap.io, proof, operation, index, identity, deadline)?;
            Ok(cap)
        }
        pub(crate) fn retained_process(&self) -> &Arc<OwnedHandle> {
            &self.process
        }
        pub(crate) fn reverify(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            operation: [u8; 16],
            index: u8,
            identity: FileIdentity,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if !std::ptr::eq(io, self.io.as_ref())
                || operation != self.operation
                || index != self.index
                || identity != self.identity
            {
                return Err(NativeError::Foreign);
            }
            proof.check(io, deadline)?;
            if process_tuple(&self.process)? != (self.pid, self.created) || !exited(&self.process) {
                return Err(NativeError::OutcomeUnknown);
            }
            let record = io
                .read_removal(proof, deadline)?
                .ok_or(NativeError::Missing)?;
            let copy = record
                .plan()
                .copies()
                .get(usize::from(index))
                .ok_or(NativeError::Foreign)?;
            if record.operation() != operation
                || copy.identity()
                    != Some(super::super::recovery::FileStamp {
                        volume: identity.volume,
                        file: identity.file,
                    })
                || copy.pid() != Some(self.pid)
                || copy.creation() != Some(self.created)
            {
                return Err(NativeError::Foreign);
            }
            deadline.check()
        }
    }
    // Actual SDDL allocation owner; keep the pointer, never fabricate an owning descriptor.
    struct SecurityDescriptor(*mut std::ffi::c_void);
    impl Drop for SecurityDescriptor {
        fn drop(&mut self) {
            // SAFETY: only the successful ConvertStringSecurityDescriptor output is held here.
            unsafe {
                LocalFree(self.0);
            }
        }
    }
    fn endpoint(io: &WindowsNativeIo) -> String {
        // Exclusivity intentionally excludes logon/LUID. Authentication still checks ALL context.
        format!(
            r"\\.\pipe\Crosspane-removal-{}-{}",
            io.target().identity().user.sddl(),
            io.target().identity().session
        )
    }
    /// FILE-only vacancy/exclusivity capability: NEVER a tree, Stop, erase or launch proof.
    pub(crate) struct RemovalKeeperLease {
        io: Arc<WindowsNativeIo>,
        namespace: Arc<OwnedHandle>,
        own: native_io::process::own::OwnProcessIdentity,
        operation: [u8; 16],
    }
    impl RemovalKeeperLease {
        pub(crate) fn reserve(
            io: Arc<WindowsNativeIo>,
            proof: &SupportProof,
            operation: [u8; 16],
            deadline: &Deadline,
        ) -> NativeResult<(NamedPipeServer, Arc<Self>)> {
            proof.check(&io, deadline)?;
            if operation == [0; 16] {
                return Err(NativeError::Invalid);
            }
            let own = io.own_process_identity(proof, deadline)?;
            let token = io.target().identity();
            let user = token.user.sddl();
            let logon = token.logon.sddl();
            let text = native_io::files::native::wide(&format!(
                "O:{user}G:{user}D:P(A;;GA;;;{user})(A;;GRGW;;;{logon})"
            ))?;
            let mut raw = std::ptr::null_mut();
            // SAFETY: complete bounded NUL SDDL and writable descriptor output.
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
            let descriptor = SecurityDescriptor(raw);
            let attributes = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor.0,
                bInheritHandle: 0,
            };
            // SAFETY: descriptor remains live for creation; actual sole first instance, local-only,
            // protected current user/logon DACL. No record or Boolean grants namespace vacancy.
            let server = unsafe {
                ServerOptions::new()
                    .first_pipe_instance(true)
                    .reject_remote_clients(true)
                    .max_instances(1)
                    .create_with_security_attributes_raw(
                        endpoint(&io),
                        (&attributes as *const SECURITY_ATTRIBUTES)
                            .cast_mut()
                            .cast(),
                    )
            }
            .map_err(|_| NativeError::Busy)?;
            let mut raw = std::ptr::null_mut();
            // SAFETY: duplicate only OUR actual first-instance server object, noninheritable.
            if unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    server.as_raw_handle(),
                    GetCurrentProcess(),
                    &mut raw,
                    0,
                    0,
                    DUPLICATE_SAME_ACCESS,
                )
            } == 0
            {
                return Err(NativeError::Unavailable);
            }
            // SAFETY: successful duplication transferred one real namespace handle.
            let namespace = Arc::new(unsafe { OwnedHandle::from_raw_handle(raw) });
            let lease = Arc::new(Self {
                io,
                namespace,
                own,
                operation,
            });
            lease.reverify(&lease.io, proof, operation, deadline)?;
            Ok((server, lease))
        }
        pub(crate) fn retained_namespace(&self) -> &Arc<OwnedHandle> {
            &self.namespace
        }
        pub(crate) fn reverify(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            operation: [u8; 16],
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if !std::ptr::eq(io, self.io.as_ref()) || operation != self.operation {
                return Err(NativeError::Foreign);
            }
            proof.check(io, deadline)?;
            self.own.reverify(deadline)?;
            let mut flags = 0;
            // SAFETY: exact original namespace duplicate, complete writable flags output.
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
                return Err(NativeError::Foreign);
            }
            deadline.check()
        }
    }

    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    pub(crate) enum ControlMethod {
        Ready,
        Commit,
        Cancel,
        Status,
    }
    #[derive(serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(crate) struct ControlFrame {
        pub schema_version: u32,
        pub operation: [u8; 16],
        pub nonce: [u8; 16],
        pub method: ControlMethod,
    }
    #[derive(serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(crate) struct ControlReply {
        pub schema_version: u32,
        pub operation: [u8; 16],
        pub nonce: [u8; 16],
        pub state: ControlState,
    }
    #[derive(Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    pub(crate) enum ControlState {
        Ready,
        Committed,
        Retained,
        Complete,
        ReinstallRequired,
    }
    const MAX_CONTROL: usize = 2048;
    pub(crate) async fn read_frame<T: serde::de::DeserializeOwned>(
        pipe: &mut (impl tokio::io::AsyncRead + Unpin),
        deadline: &Deadline,
    ) -> NativeResult<T> {
        use std::{future::poll_fn, pin::Pin};
        async fn exact(
            pipe: &mut (impl tokio::io::AsyncRead + Unpin),
            bytes: &mut [u8],
            deadline: &Deadline,
        ) -> NativeResult<()> {
            let mut position = 0;
            while position < bytes.len() {
                deadline.check()?;
                let mut output = tokio::io::ReadBuf::new(&mut bytes[position..]);
                poll_fn(|context| Pin::new(&mut *pipe).poll_read(context, &mut output))
                    .await
                    .map_err(|_| NativeError::Unavailable)?;
                let count = output.filled().len();
                if count == 0 {
                    return Err(NativeError::Unavailable);
                }
                position += count;
            }
            Ok(())
        }
        let result = tokio::time::timeout(Duration::from_millis(deadline.remaining_ms()?), async {
            let mut prefix = [0u8; 4];
            exact(pipe, &mut prefix, deadline).await?;
            let length = u32::from_le_bytes(prefix) as usize;
            if length == 0 || length > MAX_CONTROL {
                return Err(NativeError::Oversize);
            }
            let mut bytes = vec![0; length];
            exact(pipe, &mut bytes, deadline).await?;
            serde_json::from_slice(&bytes).map_err(|_| NativeError::Invalid)
        })
        .await
        .map_err(|_| NativeError::OutcomeUnknown)?;
        deadline.check()?;
        result
    }
    pub(crate) async fn write_frame<T: serde::Serialize>(
        pipe: &mut (impl tokio::io::AsyncWrite + Unpin),
        value: &T,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        use std::{future::poll_fn, pin::Pin};
        let bytes = serde_json::to_vec(value).map_err(|_| NativeError::Invalid)?;
        if bytes.is_empty() || bytes.len() > MAX_CONTROL {
            return Err(NativeError::Oversize);
        }
        tokio::time::timeout(Duration::from_millis(deadline.remaining_ms()?), async {
            let frame = [
                (bytes.len() as u32).to_le_bytes().as_slice(),
                bytes.as_slice(),
            ]
            .concat();
            let mut position = 0;
            while position < frame.len() {
                deadline.check()?;
                let count =
                    poll_fn(|context| Pin::new(&mut *pipe).poll_write(context, &frame[position..]))
                        .await
                        .map_err(|_| NativeError::OutcomeUnknown)?;
                if count == 0 {
                    return Err(NativeError::OutcomeUnknown);
                }
                position += count;
            }
            poll_fn(|context| Pin::new(&mut *pipe).poll_flush(context))
                .await
                .map_err(|_| NativeError::OutcomeUnknown)
        })
        .await
        .map_err(|_| NativeError::OutcomeUnknown)??;
        deadline.check()
    }
    pub(crate) fn runtime() -> NativeResult<tokio::runtime::Runtime> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| NativeError::Unavailable)
    }

    /// A first Commit observed through the authenticated inherited source, with the actual
    /// namespace and copied-image selection retained. Journal Committed alone cannot construct it.
    pub(crate) struct RemovalCommit {
        io: Arc<WindowsNativeIo>,
        selection: Arc<native_io::RemovalKeeperSelection>,
        namespace: Arc<RemovalKeeperLease>,
        parent: Arc<RemovalParent>,
        parent_image: FileIdentity,
    }
    impl RemovalCommit {
        pub(crate) fn lease(&self) -> &Arc<RemovalKeeperLease> {
            &self.namespace
        }
        pub(crate) fn reverify(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            operation: [u8; 16],
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if !std::ptr::eq(io, self.io.as_ref()) || operation != self.selection.operation() {
                return Err(NativeError::Foreign);
            }
            self.namespace.reverify(io, proof, operation, deadline)?;
            self.parent.reverify(io, proof, operation, deadline)?;
            self.selection.reverify(io, proof, deadline)?;
            let current = io
                .read_removal(proof, deadline)?
                .ok_or(NativeError::Missing)?;
            if current.operation() != operation
                || current.cursor() == super::super::super::removal::RemovalCursor::Selected
                || current.handoff_stage()
                    != (RemovalHandoffStage::Committed {
                        index: self.selection.index(),
                    })
            {
                return Err(NativeError::OutcomeUnknown);
            }
            deadline.check()
        }
        pub(crate) fn wait_parent_if_installed(
            &self,
            record: &RemovalRecord,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.selection.record().same_selection(record)?;
            if !record.plan().order().iter().any(|index| {
                record.plan().node(*index).is_ok_and(|node| {
                    node.identity().volume == self.parent_image.volume
                        && node.identity().file == self.parent_image.file
                })
            }) {
                return deadline.check();
            }
            let proof = self.io.admit_support(deadline)?;
            // Installed outer images are mapped by this SAME retained parent object. Keep the
            // original resident/tree while pending; never reopen PID, force exit, or bypass sharing.
            if !self.parent.has_exited(&self.io, &proof, deadline)? {
                return Err(NativeError::Busy);
            }
            Ok(())
        }
    }

    /// One bounded metadata connection at a time; no source payload bytes or waiting queue.
    pub(crate) struct RemovalServer {
        io: Arc<WindowsNativeIo>,
        selection: Arc<native_io::RemovalKeeperSelection>,
        namespace: Arc<RemovalKeeperLease>,
        parent: Arc<RemovalParent>,
        pipe: NamedPipeServer,
        rt: tokio::runtime::Runtime,
        committed: bool,
    }
    impl RemovalServer {
        pub(crate) fn new(
            io: Arc<WindowsNativeIo>,
            kind: RemovalCopyKind,
            deadline: &Deadline,
        ) -> NativeResult<Self> {
            let proof = io.admit_support(deadline)?;
            let selection = io.select_removal_keeper(&proof, deadline)?;
            if selection
                .record()
                .plan()
                .copies()
                .get(usize::from(selection.index()))
                .map(|copy| copy.kind())
                != Some(kind)
                || selection.record().handoff_stage()
                    != (RemovalHandoffStage::ResumeIntent {
                        index: selection.index(),
                    })
            {
                return Err(NativeError::Foreign);
            }
            let parent = RemovalParent::inherited(
                io.clone(),
                &proof,
                selection.record(),
                selection.index(),
                deadline,
            )?;
            let rt = runtime()?;
            let (pipe, namespace) = rt.block_on(async {
                RemovalKeeperLease::reserve(io.clone(), &proof, selection.operation(), deadline)
            })?;
            Ok(Self {
                io,
                selection: Arc::new(selection),
                namespace,
                parent: Arc::new(parent),
                pipe,
                rt,
                committed: false,
            })
        }
        pub(crate) fn operation(&self) -> [u8; 16] {
            self.namespace.operation
        }
        pub(crate) fn poll_status(
            &mut self,
            state: ControlState,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if !self.committed {
                return Err(NativeError::Foreign);
            }
            let (frame, peer) = match self.receive(deadline) {
                Ok(value) => value,
                Err(error) => {
                    let _ = self.pipe.disconnect();
                    return Err(error);
                }
            };
            peer.reverify(&self.io, &self.io.admit_support(deadline)?, deadline)?;
            let state = match frame.method {
                ControlMethod::Status => state,
                _ => ControlState::Retained,
            };
            self.reply(&frame, state, deadline)
        }
        fn receive(
            &mut self,
            deadline: &Deadline,
        ) -> NativeResult<(
            ControlFrame,
            native_io::supervisor_owner::RemovalControlPeer,
        )> {
            let io = self.io.clone();
            let operation = self.operation();
            let selection = &self.selection;
            let parent = &self.parent;
            self.rt.block_on(async {
                tokio::time::timeout(
                    Duration::from_millis(deadline.remaining_ms()?),
                    self.pipe.connect(),
                )
                .await
                .map_err(|_| NativeError::OutcomeUnknown)?
                .map_err(|_| NativeError::Unavailable)?;
                let proof = io.admit_support(deadline)?;
                let peer = native_io::supervisor_owner::admit_removal_source_peer(
                    self.pipe.as_raw_handle(),
                    io.clone(),
                    &proof,
                    parent,
                    io.self_image(&proof, deadline)?,
                    selection.clone(),
                    operation,
                    deadline,
                )?;
                // Kernel peer/full context/actual inherited parent precede all claimed frame fields.
                let frame: ControlFrame = read_frame(&mut self.pipe, deadline).await?;
                if frame.schema_version != 1
                    || frame.operation != operation
                    || frame.nonce == [0; 16]
                {
                    return Err(NativeError::Foreign);
                }
                peer.reverify(&io, &io.admit_support(deadline)?, deadline)?;
                Ok((frame, peer))
            })
        }
        fn reply(
            &mut self,
            frame: &ControlFrame,
            state: ControlState,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            let result = self.rt.block_on(write_frame(
                &mut self.pipe,
                &ControlReply {
                    schema_version: 1,
                    operation: frame.operation,
                    nonce: frame.nonce,
                    state,
                },
                deadline,
            ));
            let disconnected = self
                .pipe
                .disconnect()
                .map_err(|_| NativeError::OutcomeUnknown);
            result?;
            disconnected
        }
        pub(crate) fn await_commit(
            &mut self,
            deadline: &Deadline,
        ) -> NativeResult<Option<RemovalCommit>> {
            loop {
                let (frame, peer) = match self.receive(deadline) {
                    Ok(value) => value,
                    Err(error) => {
                        let _ = self.pipe.disconnect();
                        return Err(error);
                    }
                };
                match frame.method {
                    ControlMethod::Ready => {
                        let proof = self.io.admit_support(deadline)?;
                        let selected = &self.selection;
                        if let Err(error) = native_io::supervisor_owner::probe_removal_support(
                            self.io.clone(),
                            &proof,
                            selected.operation(),
                            deadline,
                        ) {
                            if error == NativeError::Unsupported {
                                let _ =
                                    self.reply(&frame, ControlState::ReinstallRequired, deadline);
                            }
                            return Err(error);
                        }
                        peer.reverify(&self.io, &self.io.admit_support(deadline)?, deadline)?;
                        self.reply(&frame, ControlState::Ready, deadline)?;
                    }
                    ControlMethod::Commit => {
                        if self.committed {
                            return Err(NativeError::OutcomeUnknown);
                        }
                        let proof = self.io.admit_support(deadline)?;
                        let current = self
                            .io
                            .read_removal(&proof, deadline)?
                            .ok_or(NativeError::Missing)?;
                        let selected = &self.selection;
                        selected.reverify(&self.io, &proof, deadline)?;
                        if current.cursor()
                            != super::super::super::removal::RemovalCursor::Committed
                            || current.handoff_stage()
                                != (RemovalHandoffStage::Committed {
                                    index: selected.index(),
                                })
                        {
                            return Err(NativeError::Foreign);
                        }
                        // Reserve resident ownership BEFORE ACK. A dropped ACK cannot undo Commit,
                        // replay Stop, or reconstruct this cap from the now-Committed record.
                        let parent_image = peer.image_identity();
                        self.committed = true;
                        let selection = self.selection.clone();
                        let parent = self.parent.clone();
                        drop(peer);
                        let commit = RemovalCommit {
                            io: self.io.clone(),
                            selection,
                            namespace: self.namespace.clone(),
                            parent,
                            parent_image,
                        };
                        // An abandoned ACK is observation loss only: actual committed cap is retained
                        // in the executing caller even if the metadata connection is gone.
                        let _ = self.reply(&frame, ControlState::Committed, deadline);
                        return Ok(Some(commit));
                    }
                    ControlMethod::Cancel => {
                        // No Stop/start/file effect has occurred. Leave the selection/copy facts for
                        // later genuine cleanup; exiting this actual owner is not removal success.
                        self.reply(&frame, ControlState::Retained, deadline)?;
                        return Ok(None);
                    }
                    ControlMethod::Status => self.reply(&frame, ControlState::Ready, deadline)?,
                }
            }
        }
    }

    #[derive(Default)]
    struct LaunchState {
        process: Option<Arc<OwnedHandle>>,
        thread: Option<Arc<OwnedHandle>>,
        complete: bool,
        call_live: bool,
        abandoned: bool,
        resume_attempted: bool,
        cleanup_attempted: bool,
    }
    struct LaunchOwner {
        io: Arc<WindowsNativeIo>,
        operation: [u8; 16],
        index: u8,
        image: Mutex<Option<Arc<OpenedPe>>>,
        identity: FileIdentity,
        state: Mutex<LaunchState>,
        workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
        exit: Mutex<Option<Arc<RemovalCopyExit>>>,
    }
    static LAUNCH: OnceLock<Mutex<Option<Arc<LaunchOwner>>>> = OnceLock::new();
    pub(crate) struct RemovalChild {
        owner: Arc<LaunchOwner>,
    }
    impl RemovalChild {
        pub(crate) fn process(&self) -> NativeResult<Arc<OwnedHandle>> {
            let state = self
                .owner
                .state
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?;
            if !state.complete {
                return Err(NativeError::OutcomeUnknown);
            }
            state
                .process
                .as_ref()
                .cloned()
                .ok_or(NativeError::OutcomeUnknown)
        }
        fn image(&self) -> NativeResult<Arc<OpenedPe>> {
            self.owner
                .image
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?
                .as_ref()
                .cloned()
                .ok_or(NativeError::OutcomeUnknown)
        }
        pub(crate) fn index(&self) -> u8 {
            self.owner.index
        }
        pub(crate) fn actual_exit(
            &self,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<Option<Arc<RemovalCopyExit>>> {
            let mut stored = self
                .owner
                .exit
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?;
            if let Some(cap) = stored.as_ref() {
                cap.reverify(
                    &self.owner.io,
                    proof,
                    self.owner.operation,
                    self.owner.index,
                    self.owner.identity,
                    deadline,
                )?;
                return Ok(Some(cap.clone()));
            }
            let process = self.process()?;
            if !exited(&process) {
                return Ok(None);
            }
            let mut workers = self
                .owner
                .workers
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?;
            if workers.is_empty()
                || workers.iter().any(|worker| {
                    // SAFETY: observe each actual retained native worker, including its TLS teardown.
                    (unsafe { WaitForSingleObject(worker.as_raw_handle(), 0) }) != WAIT_OBJECT_0
                })
            {
                return Err(NativeError::OutcomeUnknown);
            }
            let state = self
                .owner
                .state
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?;
            if state.call_live {
                return Err(NativeError::Busy);
            }
            let mut image = self
                .owner
                .image
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?;
            let mut slot = LAUNCH
                .get_or_init(|| Mutex::new(None))
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?;
            if !slot
                .as_ref()
                .is_some_and(|owner| Arc::ptr_eq(owner, &self.owner))
            {
                return Err(NativeError::OutcomeUnknown);
            }
            // Complete every fallible check while the global owner still retains the actual outputs.
            let cap = Arc::new(RemovalCopyExit::from_child(
                self.owner.io.clone(),
                process,
                self.owner.operation,
                self.owner.index,
                self.owner.identity,
                proof,
                deadline,
            )?);
            *stored = Some(cap.clone());
            image.take();
            workers.clear();
            slot.take();
            drop(state);
            // No fallible work after alias retirement: the returned cap owns only the actual exited
            // process, never the executing-image read pin needed by the same-handle DELETE adapter.
            Ok(Some(cap))
        }
        fn cancel_never_resumed(&self) -> NativeResult<()> {
            request_cleanup(self.owner.clone())
        }
        pub(crate) fn exchange(
            &self,
            method: ControlMethod,
            deadline: &Deadline,
        ) -> NativeResult<ControlState> {
            use native_io::supervisor_owner::admit_removal_keeper_peer;
            let io = &self.owner.io;
            let proof = io.admit_support(deadline)?;
            let process = self.process()?;
            let nonce = io.bridge_nonce(&proof, deadline)?;
            runtime()?.block_on(async {
                let mut pipe = loop {
                    deadline.check()?;
                    match tokio::net::windows::named_pipe::ClientOptions::new().open(endpoint(io)) {
                        Ok(pipe) => break pipe,
                        Err(error) if matches!(error.raw_os_error(), Some(2 | 231)) => {
                            tokio::time::sleep(Duration::from_millis(5)).await
                        }
                        Err(_) => return Err(NativeError::Unavailable),
                    }
                };
                let image = self.image()?;
                let peer = admit_removal_keeper_peer(
                    pipe.as_raw_handle(),
                    true,
                    io.clone(),
                    &io.admit_support(deadline)?,
                    &process,
                    image.as_ref(),
                    self.owner.operation,
                    deadline,
                )?;
                write_frame(
                    &mut pipe,
                    &ControlFrame {
                        schema_version: 1,
                        operation: self.owner.operation,
                        nonce,
                        method,
                    },
                    deadline,
                )
                .await?;
                let reply: ControlReply = read_frame(&mut pipe, deadline).await?;
                if reply.schema_version != 1
                    || reply.operation != self.owner.operation
                    || reply.nonce != nonce
                {
                    return Err(NativeError::Foreign);
                }
                peer.reverify(io, &io.admit_support(deadline)?, deadline)?;
                Ok(reply.state)
            })
        }
    }

    impl Drop for RemovalChild {
        fn drop(&mut self) {
            let never_resumed = self
                .owner
                .state
                .lock()
                .map(|state| !state.resume_attempted)
                .unwrap_or(false);
            if never_resumed {
                let _ = request_cleanup(self.owner.clone());
            }
        }
    }
    fn request_cleanup(owner: Arc<LaunchOwner>) -> NativeResult<()> {
        let cleanup = Deadline::new(
            5_000,
            owner.io.bound_clock(),
            native_io::Cancellation::default(),
        )?;
        let process = {
            let mut state = owner
                .state
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?;
            state.abandoned = true;
            if state.call_live {
                return Err(NativeError::Busy);
            }
            if !state.complete || state.resume_attempted {
                return Err(NativeError::OutcomeUnknown);
            }
            let process = state
                .process
                .as_ref()
                .cloned()
                .ok_or(NativeError::OutcomeUnknown)?;
            if exited(&process) {
                return Ok(());
            }
            if state.cleanup_attempted {
                return Err(NativeError::OutcomeUnknown);
            }
            state.cleanup_attempted = true;
            state.call_live = true;
            process
        };
        // Cleanup has its own bounded observation/effect owner; never reset Create/Resume's
        // absolute pre-Stop deadline. The source still owns the exact suspended child afterward.
        let captured = owner.clone();
        let worker = std::thread::Builder::new()
            .name("crosspane-removal-suspended-cleanup".into())
            .spawn(move || {
                let _result = (|| {
                    captured.io.admit_support(&cleanup)?;
                    cleanup.check()?;
                    // SAFETY: THIS creator returned a complete SUSPENDED child, never attempted Resume;
                    // only that actual retained handle is terminated, outside every state mutex.
                    if unsafe { TerminateProcess(process.as_raw_handle(), 1) } == 0 {
                        return Err(NativeError::OutcomeUnknown);
                    }
                    // SAFETY: bounded settlement of that SAME known original child object.
                    if unsafe {
                        WaitForSingleObject(
                            process.as_raw_handle(),
                            cleanup.remaining_ms()?.min(u32::MAX as u64) as u32,
                        )
                    } != WAIT_OBJECT_0
                    {
                        return Err(NativeError::OutcomeUnknown);
                    }
                    cleanup.check()
                })();
                if let Ok(mut state) = captured.state.lock() {
                    state.call_live = false;
                }
            });
        match worker {
            Ok(worker) => {
                owner
                    .workers
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?
                    .push(worker);
                Ok(())
            }
            Err(_) => {
                owner
                    .state
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?
                    .call_live = false;
                Err(NativeError::OutcomeUnknown)
            }
        }
    }
    struct Attributes {
        storage: Vec<usize>,
        handles: Vec<std::os::windows::io::RawHandle>,
    }
    impl Drop for Attributes {
        fn drop(&mut self) {
            // SAFETY: only a successfully initialized attribute list is wrapped by this owner.
            unsafe {
                DeleteProcThreadAttributeList(self.storage.as_mut_ptr().cast());
            }
        }
    }
    fn attributes(parent: &OwnedHandle) -> NativeResult<Attributes> {
        let mut bytes = 0;
        // SAFETY: sizing pass with null list and complete required-size output.
        unsafe {
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut bytes);
        }
        if bytes == 0 || bytes > 65_536 {
            return Err(NativeError::Unavailable);
        }
        let mut storage = vec![0usize; bytes.div_ceil(std::mem::size_of::<usize>())];
        // SAFETY: correctly aligned storage of the exact required bounded size.
        if unsafe {
            InitializeProcThreadAttributeList(storage.as_mut_ptr().cast(), 1, 0, &mut bytes)
        } == 0
        {
            return Err(NativeError::Unavailable);
        }
        let mut owned = Attributes {
            storage,
            handles: vec![parent.as_raw_handle()],
        };
        // SAFETY: HANDLE_LIST contains ONLY the explicitly duplicated inheritable original parent,
        // whose handle value stays valid through actual CreateProcess. No other handle inherits.
        if unsafe {
            UpdateProcThreadAttribute(
                owned.storage.as_mut_ptr().cast(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                owned.handles.as_mut_ptr().cast(),
                std::mem::size_of_val(owned.handles.as_slice()),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(NativeError::Unavailable);
        }
        Ok(owned)
    }
    pub(crate) fn launch(
        io: Arc<WindowsNativeIo>,
        mut lock: Option<InstallerLock>,
        mut record: RemovalRecord,
        deadline: &Deadline,
    ) -> NativeResult<(RemovalChild, RemovalRecord)> {
        use native_io::files::native::wide;
        let actual = lock.as_ref().ok_or(NativeError::Foreign)?;
        let proof = io.admit_support(deadline)?;
        let own = io.self_image(&proof, deadline)?;
        let index = 0u8;
        let kind = record
            .plan()
            .copies()
            .get(usize::from(index))
            .ok_or(NativeError::Foreign)?
            .kind();
        if record.handoff_stage() != RemovalHandoffStage::None {
            return Err(NativeError::OutcomeUnknown);
        }
        record.advance_handoff(RemovalHandoffStage::CopyPrepareIntent { index })?;
        let permit = io.publish_removal(&proof, actual, &record, deadline)?;
        let copy = Arc::new(io.prepare_removal_copy(
            &io.admit_support(deadline)?,
            actual,
            &permit,
            index,
            &own,
            deadline,
        )?);
        record.bind_copy_image(
            index,
            super::super::recovery::FileStamp {
                volume: copy.identity().volume,
                file: copy.identity().file,
            },
            copy.approved().facts().clone(),
        )?;
        record.advance_handoff(RemovalHandoffStage::CopyPrepared { index })?;
        io.publish_removal(&io.admit_support(deadline)?, actual, &record, deadline)?;
        let parent = io.own_process_identity(&io.admit_support(deadline)?, deadline)?;
        let mut raw = std::ptr::null_mut();
        // SAFETY: duplicate only the actual originally admitted current process, query/sync,
        // inheritable for the sole explicit HANDLE_LIST; no PID is selected from metadata.
        if unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                parent.handle().as_raw_handle(),
                GetCurrentProcess(),
                &mut raw,
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                1,
                0,
            )
        } == 0
        {
            return Err(NativeError::Unavailable);
        }
        // SAFETY: successful duplication transfers one real owned inheritable parent handle.
        let inherited = Arc::new(unsafe { OwnedHandle::from_raw_handle(raw) });
        let owner = Arc::new(LaunchOwner {
            io: io.clone(),
            operation: record.operation(),
            index,
            image: Mutex::new(Some(copy.clone())),
            identity: copy.identity(),
            state: Mutex::new(LaunchState::default()),
            workers: Mutex::new(Vec::new()),
            exit: Mutex::new(None),
        });
        {
            let mut slot = LAUNCH
                .get_or_init(|| Mutex::new(None))
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?;
            if slot.is_some() {
                return Err(NativeError::Busy);
            }
            *slot = Some(owner.clone());
        }
        record.advance_handoff(RemovalHandoffStage::CreateIntent { index })?;
        io.publish_removal(&io.admit_support(deadline)?, actual, &record, deadline)?;
        let captured = owner.clone();
        let original = deadline.clone();
        let parent_handle = inherited.clone();
        let (send, recv) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("crosspane-removal-copy-create".into())
            .spawn(move || {
                let result = (|| {
                    let proof = captured.io.admit_support(&original)?;
                    let image = captured
                        .image
                        .lock()
                        .map_err(|_| NativeError::OutcomeUnknown)?
                        .as_ref()
                        .cloned()
                        .ok_or(NativeError::OutcomeUnknown)?;
                    image.reverify(&captured.io, &proof, &original)?;
                    let mut attrs = attributes(&parent_handle)?;
                    let application = wide(image.canonical_dos_path())?;
                    let (directory, _) = image
                        .canonical_dos_path()
                        .rsplit_once('\\')
                        .ok_or(NativeError::Foreign)?;
                    // The keeper must never inherit an install-root working-directory handle.
                    let directory = wide(directory)?;
                    let flag = match kind {
                        RemovalCopyKind::Keeper => "--windows-upgrade-keeper",
                        RemovalCopyKind::Helper => super::REPLACE_HELPER_ARGUMENT,
                    };
                    let mut command = wide(&format!("\"{}\" {flag}", image.canonical_dos_path()))?;
                    let startup = STARTUPINFOEXW {
                        StartupInfo: STARTUPINFOW {
                            cb: std::mem::size_of::<STARTUPINFOEXW>() as u32,
                            ..Default::default()
                        },
                        lpAttributeList: attrs.storage.as_mut_ptr().cast(),
                    };
                    let mut output = PROCESS_INFORMATION::default();
                    {
                        let mut state = captured
                            .state
                            .lock()
                            .map_err(|_| NativeError::OutcomeUnknown)?;
                        if state.abandoned {
                            return Err(NativeError::Cancelled);
                        }
                        original.check()?;
                        state.call_live = true;
                    }
                    // SAFETY: pinned actual fixed copied image, exact sole flag, valid explicit inherited
                    // parent list; create suspended and request breakaway, never alter any containing job.
                    let ok = unsafe {
                        CreateProcessW(
                            application.as_ptr(),
                            command.as_mut_ptr(),
                            std::ptr::null(),
                            std::ptr::null(),
                            1,
                            CREATE_SUSPENDED
                                | CREATE_BREAKAWAY_FROM_JOB
                                | EXTENDED_STARTUPINFO_PRESENT
                                | CREATE_NO_WINDOW,
                            std::ptr::null(),
                            directory.as_ptr(),
                            &startup.StartupInfo,
                            &mut output,
                        )
                    };
                    // SAFETY: read this thread's CreateProcess error before any other native call.
                    let failure = if ok == 0 {
                        // SAFETY: read this worker's CreateProcess failure before another native call.
                        unsafe { GetLastError() }
                    } else {
                        0
                    };
                    let mut state = captured
                        .state
                        .lock()
                        .map_err(|_| NativeError::OutcomeUnknown)?;
                    state.call_live = false;
                    if !output.hProcess.is_null() {
                        // SAFETY: actual CreateProcess output is retained before any validation/delivery.
                        state.process = Some(Arc::new(unsafe {
                            OwnedHandle::from_raw_handle(output.hProcess)
                        }));
                    }
                    if !output.hThread.is_null() {
                        // SAFETY: actual CreateProcess thread output, retained even for partial failure.
                        state.thread = Some(Arc::new(unsafe {
                            OwnedHandle::from_raw_handle(output.hThread)
                        }));
                    }
                    state.complete = ok != 0 && state.process.is_some() && state.thread.is_some();
                    if !state.complete {
                        if ok == 0 && state.process.is_none() && state.thread.is_none() {
                            return Err(if failure == ERROR_ACCESS_DENIED {
                                NativeError::Unsupported
                            } else {
                                NativeError::Unavailable
                            });
                        }
                        return Err(NativeError::OutcomeUnknown);
                    }
                    if state.abandoned {
                        drop(state);
                        let _ = request_cleanup(captured.clone());
                        return Err(NativeError::OutcomeUnknown);
                    }
                    let process = state
                        .process
                        .as_ref()
                        .cloned()
                        .ok_or(NativeError::OutcomeUnknown)?;
                    // Claim only the native observation; timeout/cancel can still latch abandonment
                    // immediately without waiting on job/process queries under this state mutex.
                    state.call_live = true;
                    drop(state);
                    let observed = (|| {
                        let mut in_job = 0;
                        // SAFETY: the actual returned child is still suspended; NULL asks membership
                        // in ANY containing job. Refuse unsupported breakaway BEFORE Resume or Stop.
                        if unsafe {
                            IsProcessInJob(
                                process.as_raw_handle(),
                                std::ptr::null_mut(),
                                &mut in_job,
                            )
                        } == 0
                            || in_job != 0
                        {
                            return Err(NativeError::Unsupported);
                        }
                        let tuple = process_tuple(&process)?;
                        original.check()?;
                        Ok(tuple)
                    })();
                    let abandoned = {
                        let mut state = captured
                            .state
                            .lock()
                            .map_err(|_| NativeError::OutcomeUnknown)?;
                        state.call_live = false;
                        if observed.is_err() {
                            state.abandoned = true;
                        }
                        state.abandoned
                    };
                    if abandoned {
                        let _ = request_cleanup(captured.clone());
                        return Err(observed.err().unwrap_or(NativeError::OutcomeUnknown));
                    }
                    observed
                })();
                if result.is_err() {
                    let _ = request_cleanup(captured.clone());
                }
                let _ = send.send(result);
            })
            .map_err(|_| NativeError::OutcomeUnknown)?;
        owner
            .workers
            .lock()
            .map_err(|_| NativeError::OutcomeUnknown)?
            .push(worker);
        let child = RemovalChild { owner };
        let (pid, created) =
            match recv.recv_timeout(Duration::from_millis(deadline.remaining_ms()?)) {
                Ok(Ok(tuple)) if deadline.check().is_ok() => tuple,
                Ok(Err(error)) => {
                    let _ = child.cancel_never_resumed();
                    return Err(error);
                }
                _ => {
                    let _ = child.cancel_never_resumed();
                    return Err(NativeError::OutcomeUnknown);
                }
            };
        // Physical worker settlement BEFORE source record/resume, retaining no late output owner.
        loop {
            // SAFETY: exact owned creator thread; native exit includes TLS/attribute cleanup.
            let workers = child
                .owner
                .workers
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?;
            if workers.first().is_some_and(|worker| {
                // SAFETY: exact retained creator thread, no guessed absent-thread settlement.
                (unsafe { WaitForSingleObject(worker.as_raw_handle(), 0) }) == WAIT_OBJECT_0
            }) {
                break;
            }
            drop(workers);
            if deadline.check().is_err() {
                let _ = child.cancel_never_resumed();
                return Err(NativeError::OutcomeUnknown);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        record.bind_copy_child(
            index,
            pid,
            created,
            inherited.as_raw_handle() as usize as u64,
        )?;
        record.advance_handoff(RemovalHandoffStage::Created { index })?;
        io.publish_removal(&io.admit_support(deadline)?, actual, &record, deadline)?;
        record.advance_handoff(RemovalHandoffStage::ResumeIntent { index })?;
        io.publish_removal(&io.admit_support(deadline)?, actual, &record, deadline)?;
        // ALL original mutation-lock aliases are released before the copied entry can act.
        drop(lock.take());

        deadline.check()?;
        let thread = {
            let mut state = child
                .owner
                .state
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?;
            if !state.complete || state.abandoned || state.resume_attempted || state.call_live {
                return Err(NativeError::OutcomeUnknown);
            }
            state.resume_attempted = true;
            state.call_live = true;
            state
                .thread
                .as_ref()
                .cloned()
                .ok_or(NativeError::OutcomeUnknown)?
        };
        let captured = child.owner.clone();
        let resume_bound = deadline.clone();
        let (send, recv) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("crosspane-removal-copy-resume".into())
            .spawn(move || {
                let result = (|| {
                    let proof = captured.io.admit_support(&resume_bound)?;
                    let image = captured
                        .image
                        .lock()
                        .map_err(|_| NativeError::OutcomeUnknown)?
                        .as_ref()
                        .cloned()
                        .ok_or(NativeError::OutcomeUnknown)?;
                    image.reverify(&captured.io, &proof, &resume_bound)?;
                    resume_bound.check()?;
                    // SAFETY: only the actual thread from our complete suspended output; invocation
                    // claimed once before dispatch, outside every state mutex, never a Resume retry.
                    if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                        return Err(NativeError::OutcomeUnknown);
                    }
                    resume_bound.check()
                })();
                if let Ok(mut state) = captured.state.lock() {
                    state.call_live = false;
                }
                let _ = send.send(result);
            });
        let worker = match worker {
            Ok(worker) => worker,
            Err(_) => {
                // Definite local thread-spawn refusal: no Resume call ran. The one-way attempt
                // remains consumed, so uncertain/resume-attempted outputs can never be terminated.
                child
                    .owner
                    .state
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?
                    .call_live = false;
                return Err(NativeError::OutcomeUnknown);
            }
        };
        child
            .owner
            .workers
            .lock()
            .map_err(|_| NativeError::OutcomeUnknown)?
            .push(worker);
        match recv.recv_timeout(Duration::from_millis(deadline.remaining_ms()?)) {
            Ok(Ok(())) if deadline.check().is_ok() => {}
            _ => return Err(NativeError::OutcomeUnknown),
        }
        drop(own);
        drop(parent);
        drop(inherited);
        Ok((child, record))
    }
}
