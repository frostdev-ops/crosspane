//! The actual child/job owner and its bounded launch state. Correlation is never handle authority.

use super::super::service::supervisor::Generation;
use super::{NativeError, NativeResult};

/// Error-exit scheduling observation only, never an upgrade/empty-tree authority proof.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ErrorRetentionObservation {
    Settled,
    Pending,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ChildPhase {
    #[default]
    Idle,
    Creating,
    Suspended,
    Assigning,
    Assigned,
    Resuming,
    Running,
    Settling,
    Settled,
    Unknown,
}

/// The native mutex owns this state AND every actual handle. Retirement never assumes a timeout
/// cancelled a dispatched OS operation. The same gate arbitrates terminal arm and every resume.
#[derive(Default, Debug)]
pub(crate) struct ChildControl {
    phase: ChildPhase,
    retired: bool,
    terminal: Option<([u8; 16], Generation)>,
    generation: Option<Generation>,
    create_dispatched: bool,
    create_refused_without_outputs: bool,
}
impl ChildControl {
    pub(crate) fn claim_create(&mut self) -> NativeResult<()> {
        if self.retired || self.terminal.is_some() {
            return Err(NativeError::OutcomeUnknown);
        }
        if self.phase != ChildPhase::Idle {
            return Err(NativeError::Busy);
        }
        self.phase = ChildPhase::Creating;
        self.create_dispatched = false;
        self.create_refused_without_outputs = false;
        Ok(())
    }
    pub(crate) fn claim_native_create(&mut self) -> NativeResult<()> {
        if self.retired
            || self.terminal.is_some()
            || self.phase != ChildPhase::Creating
            || self.create_dispatched
        {
            return Err(NativeError::OutcomeUnknown);
        }
        self.create_dispatched = true;
        Ok(())
    }
    pub(crate) fn create_refused(&mut self, unexpected_outputs: bool) {
        self.retired = true;
        if self.phase == ChildPhase::Creating && self.create_dispatched && !unexpected_outputs {
            self.create_refused_without_outputs = true;
            self.phase = ChildPhase::Settled;
        } else {
            self.phase = ChildPhase::Unknown;
        }
    }
    /// This scheduling result is positive only after the actual call owner is idle. A timeout
    /// while it is live cannot imply that the CreateProcess entry point was never called.
    pub(crate) fn no_child_after_settled_call(&self, call_live: bool, owns_output: bool) -> bool {
        !call_live
            && !owns_output
            && (!self.create_dispatched || self.create_refused_without_outputs)
    }
    pub(crate) fn created(&mut self, complete: bool) -> bool {
        if self.phase != ChildPhase::Creating || !self.create_dispatched || !complete {
            self.retired = true;
            self.phase = ChildPhase::Unknown;
            return false;
        }
        self.phase = ChildPhase::Suspended;
        self.claim_cleanup()
    }
    pub(crate) fn claim_assign(&mut self) -> NativeResult<()> {
        if self.retired || self.terminal.is_some() {
            return Err(NativeError::OutcomeUnknown);
        }
        if self.phase != ChildPhase::Suspended {
            return Err(NativeError::Foreign);
        }
        self.phase = ChildPhase::Assigning;
        Ok(())
    }
    pub(crate) fn assigned(&mut self, success: bool) -> bool {
        if self.phase != ChildPhase::Assigning {
            self.retired = true;
            self.phase = ChildPhase::Unknown;
            return false;
        }
        self.phase = if success {
            ChildPhase::Assigned
        } else {
            ChildPhase::Suspended
        };
        if !success {
            self.retired = true;
        }
        self.claim_cleanup()
    }
    pub(crate) fn claim_resume(&mut self) -> NativeResult<()> {
        if self.retired || self.terminal.is_some() {
            return Err(NativeError::OutcomeUnknown);
        }
        if self.phase != ChildPhase::Assigned {
            return Err(NativeError::Foreign);
        }
        // From this point a never-resumed cleanup is forbidden, even on an error or late result.
        self.phase = ChildPhase::Resuming;
        Ok(())
    }
    pub(crate) fn resumed(&mut self, previous: u32) -> NativeResult<()> {
        if self.phase != ChildPhase::Resuming || previous != 1 || self.retired {
            self.retired = true;
            self.phase = ChildPhase::Unknown;
            return Err(NativeError::OutcomeUnknown);
        }
        self.phase = ChildPhase::Running;
        Ok(())
    }
    pub(crate) fn retire(&mut self) -> bool {
        self.retired = true;
        self.claim_cleanup()
    }
    fn claim_cleanup(&mut self) -> bool {
        if (self.retired || self.terminal.is_some())
            && matches!(self.phase, ChildPhase::Suspended | ChildPhase::Assigned)
        {
            self.phase = ChildPhase::Settling;
            true
        } else {
            false
        }
    }
    pub(crate) fn cleanup_observed(&mut self, exited_and_empty: bool) {
        if self.phase != ChildPhase::Settling || !exited_and_empty {
            self.phase = ChildPhase::Unknown;
        } else {
            self.phase = ChildPhase::Settled;
        }
        self.retired = true;
    }
    pub(crate) fn bind_ready(&mut self, generation: Generation) -> NativeResult<()> {
        if self.retired
            || self.phase != ChildPhase::Running
            || self.terminal.is_some()
            || generation.pid == 0
            || generation.creation == 0
            || generation.instance == 0
        {
            return Err(NativeError::Foreign);
        }
        if self.generation.is_some_and(|old| old != generation) {
            return Err(NativeError::Foreign);
        }
        self.generation = Some(generation);
        Ok(())
    }
    pub(crate) fn arm_terminal(
        &mut self,
        operation: [u8; 16],
        generation: Generation,
    ) -> NativeResult<()> {
        if operation == [0; 16] || self.generation != Some(generation) {
            return Err(NativeError::Foreign);
        }
        if let Some(old) = self.terminal {
            return if old == (operation, generation) {
                Ok(())
            } else {
                Err(NativeError::Foreign)
            };
        }
        if self.retired {
            return Err(NativeError::OutcomeUnknown);
        }
        self.terminal = Some((operation, generation));
        Ok(())
    }
    pub(crate) fn follow_ready(
        &mut self,
        previous: Generation,
        next: Generation,
    ) -> NativeResult<()> {
        if self.retired
            || self.terminal.is_some()
            || self.phase != ChildPhase::Running
            || self.generation != Some(previous)
            || next.pid == 0
            || next.creation == 0
            || next.instance == 0
            || next.pid == previous.pid
            || next.creation == previous.creation
            || next.instance == previous.instance
        {
            return Err(NativeError::Foreign);
        }
        self.generation = Some(next);
        Ok(())
    }
    pub(crate) fn terminal_operation(&self) -> Option<[u8; 16]> {
        self.terminal.map(|(operation, _)| operation)
    }
    /// Only the native adapter's actual original exit + job-zero observation calls this.
    pub(crate) fn reset_after_exit_and_empty(&mut self) -> NativeResult<()> {
        if self.retired || self.terminal.is_some() || self.phase != ChildPhase::Running {
            return Err(NativeError::OutcomeUnknown);
        }
        self.phase = ChildPhase::Idle;
        self.generation = None;
        Ok(())
    }
}

/// The same production ordering is exercised by the portable fakes. An ambiguous resume can never
/// be sent to the positive never-resumed cleanup path.
pub(crate) trait ChildJobPort {
    type Child;
    fn create_suspended(&mut self) -> NativeResult<Self::Child>;
    fn assign(&mut self, child: &Self::Child) -> NativeResult<()>;
    fn resume(&mut self, child: &Self::Child) -> NativeResult<()>;
    fn cleanup_never_resumed(&mut self, child: &Self::Child) -> NativeResult<()>;
    fn retain_unknown(&mut self);
}
pub(crate) fn launch<P: ChildJobPort>(port: &mut P) -> NativeResult<P::Child> {
    let child = match port.create_suspended() {
        Ok(child) => child,
        Err(error) => {
            port.retain_unknown();
            return Err(error);
        }
    };
    if let Err(error) = port.assign(&child) {
        if port.cleanup_never_resumed(&child).is_err() {
            port.retain_unknown();
            return Err(NativeError::OutcomeUnknown);
        }
        return Err(error);
    }
    if let Err(error) = port.resume(&child) {
        port.retain_unknown();
        return Err(error);
    }
    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn generation() -> Generation {
        Generation {
            pid: 11,
            creation: 12,
            instance: 13,
        }
    }
    fn running() -> ChildControl {
        let mut c = ChildControl::default();
        c.claim_create().unwrap();
        c.claim_native_create().unwrap();
        assert!(!c.created(true));
        c.claim_assign().unwrap();
        assert!(!c.assigned(true));
        c.claim_resume().unwrap();
        c.resumed(1).unwrap();
        c.bind_ready(generation()).unwrap();
        c
    }
    #[test]
    fn native_launch_control_requires_create_assign_before_one_resume() {
        let mut c = ChildControl::default();
        assert_eq!(c.claim_resume(), Err(NativeError::Foreign));
        c.claim_create().unwrap();
        c.claim_native_create().unwrap();
        assert_eq!(c.claim_create(), Err(NativeError::Busy));
        assert!(!c.created(true));
        assert_eq!(c.claim_resume(), Err(NativeError::Foreign));
        c.claim_assign().unwrap();
        assert!(!c.assigned(true));
        c.claim_resume().unwrap();
        assert_eq!(c.claim_resume(), Err(NativeError::Foreign));
        c.resumed(1).unwrap();
        assert_eq!(c.phase, ChildPhase::Running);
    }
    #[test]
    fn native_no_dispatch_is_positive_only_after_call_owner_settles() {
        let mut c = ChildControl::default();
        c.claim_create().unwrap();
        c.retire();
        assert!(!c.no_child_after_settled_call(true, false));
        assert!(c.no_child_after_settled_call(false, false));
        assert!(!c.no_child_after_settled_call(false, true));
        assert_eq!(c.claim_native_create(), Err(NativeError::OutcomeUnknown));
        let mut c = ChildControl::default();
        c.claim_create().unwrap();
        c.claim_native_create().unwrap();
        c.retire();
        assert!(!c.no_child_after_settled_call(false, false));
    }
    #[test]
    fn native_conclusive_create_refusal_and_anomalous_outputs_are_distinct() {
        let mut c = ChildControl::default();
        c.claim_create().unwrap();
        c.claim_native_create().unwrap();
        c.create_refused(false);
        assert!(c.no_child_after_settled_call(false, false));
        assert!(!c.no_child_after_settled_call(true, false));
        assert_eq!(c.phase, ChildPhase::Settled);
        let mut c = ChildControl::default();
        c.claim_create().unwrap();
        c.claim_native_create().unwrap();
        c.create_refused(true);
        assert!(!c.no_child_after_settled_call(false, false));
        assert!(!c.no_child_after_settled_call(false, true));
        assert_eq!(c.phase, ChildPhase::Unknown);
    }
    #[test]
    fn native_late_create_retirement_claims_only_never_resumed_cleanup() {
        let mut c = ChildControl::default();
        c.claim_create().unwrap();
        c.claim_native_create().unwrap();
        assert!(!c.retire());
        assert!(c.created(true));
        assert_eq!(c.phase, ChildPhase::Settling);
        assert_eq!(c.claim_resume(), Err(NativeError::OutcomeUnknown));
        c.cleanup_observed(true);
        assert_eq!(c.phase, ChildPhase::Settled);
        assert!(!c.retire());
    }
    #[test]
    fn native_late_assign_retirement_cannot_resume() {
        let mut c = ChildControl::default();
        c.claim_create().unwrap();
        c.claim_native_create().unwrap();
        c.created(true);
        c.claim_assign().unwrap();
        assert!(!c.retire());
        assert!(c.assigned(true));
        assert_eq!(c.phase, ChildPhase::Settling);
        assert_eq!(c.claim_resume(), Err(NativeError::OutcomeUnknown));
    }
    #[test]
    fn native_resume_timeout_and_every_unexpected_suspend_count_retain() {
        for previous in [0, 2, u32::MAX] {
            let mut c = ChildControl::default();
            c.claim_create().unwrap();
            c.claim_native_create().unwrap();
            c.created(true);
            c.claim_assign().unwrap();
            c.assigned(true);
            c.claim_resume().unwrap();
            assert_eq!(c.resumed(previous), Err(NativeError::OutcomeUnknown));
            assert!(!c.retire());
            assert_eq!(c.phase, ChildPhase::Unknown);
        }
        let mut c = ChildControl::default();
        c.claim_create().unwrap();
        c.claim_native_create().unwrap();
        c.created(true);
        c.claim_assign().unwrap();
        c.assigned(true);
        c.claim_resume().unwrap();
        assert!(!c.retire());
        assert_eq!(c.resumed(1), Err(NativeError::OutcomeUnknown));
        assert_eq!(c.phase, ChildPhase::Unknown);
    }
    #[test]
    fn native_terminal_arm_is_exact_sticky_and_excludes_restart() {
        let mut c = running();
        c.arm_terminal([1; 16], generation()).unwrap();
        assert_eq!(c.arm_terminal([1; 16], generation()), Ok(()));
        assert_eq!(
            c.arm_terminal([2; 16], generation()),
            Err(NativeError::Foreign)
        );
        assert_eq!(
            c.reset_after_exit_and_empty(),
            Err(NativeError::OutcomeUnknown)
        );
        assert_eq!(c.claim_create(), Err(NativeError::OutcomeUnknown));
    }
    #[test]
    fn native_clean_successor_requires_three_distinct_original_facts_and_live_gate() {
        let next = Generation {
            pid: 21,
            creation: 22,
            instance: 23,
        };
        for next in [
            Generation {
                pid: generation().pid,
                ..next
            },
            Generation {
                creation: generation().creation,
                ..next
            },
            Generation {
                instance: generation().instance,
                ..next
            },
        ] {
            let mut c = running();
            assert_eq!(
                c.follow_ready(generation(), next),
                Err(NativeError::Foreign)
            );
            assert_eq!(c.generation, Some(generation()));
        }
        let mut c = running();
        c.follow_ready(generation(), next).unwrap();
        assert_eq!(c.generation, Some(next));
        let mut c = running();
        c.arm_terminal([1; 16], generation()).unwrap();
        assert_eq!(
            c.follow_ready(generation(), next),
            Err(NativeError::Foreign)
        );
        assert_eq!(c.generation, Some(generation()));
    }
    #[test]
    fn native_real_empty_exit_rearms_only_nonterminal_running_generation() {
        let mut c = running();
        c.reset_after_exit_and_empty().unwrap();
        c.claim_create().unwrap();
        c.claim_native_create().unwrap();
        assert_eq!(c.generation, None);
        assert_eq!(c.phase, ChildPhase::Creating);
        let mut c = running();
        c.retire();
        assert_eq!(
            c.reset_after_exit_and_empty(),
            Err(NativeError::OutcomeUnknown)
        );
    }
    #[test]
    fn native_partial_handles_and_unsettled_cleanup_never_become_empty_proof() {
        let mut c = ChildControl::default();
        c.claim_create().unwrap();
        c.claim_native_create().unwrap();
        assert!(!c.created(false));
        assert_eq!(c.phase, ChildPhase::Unknown);
        assert_eq!(c.claim_create(), Err(NativeError::OutcomeUnknown));
        let mut c = ChildControl::default();
        c.claim_create().unwrap();
        c.claim_native_create().unwrap();
        c.created(true);
        assert!(c.retire());
        c.cleanup_observed(false);
        assert_eq!(c.phase, ChildPhase::Unknown);
    }
    struct Fake {
        steps: Vec<&'static str>,
        fail: Option<&'static str>,
    }
    impl ChildJobPort for Fake {
        type Child = ();
        fn create_suspended(&mut self) -> NativeResult<()> {
            self.steps.push("create");
            if self.fail == Some("create") {
                Err(NativeError::OutcomeUnknown)
            } else {
                Ok(())
            }
        }
        fn assign(&mut self, _: &()) -> NativeResult<()> {
            self.steps.push("assign");
            if self.fail == Some("assign") {
                Err(NativeError::Unavailable)
            } else {
                Ok(())
            }
        }
        fn resume(&mut self, _: &()) -> NativeResult<()> {
            self.steps.push("resume");
            if self.fail == Some("resume") {
                Err(NativeError::OutcomeUnknown)
            } else {
                Ok(())
            }
        }
        fn cleanup_never_resumed(&mut self, _: &()) -> NativeResult<()> {
            self.steps.push("cleanup");
            Ok(())
        }
        fn retain_unknown(&mut self) {
            self.steps.push("retain");
        }
    }
    #[test]
    fn native_same_launch_coordinator_covers_effect_order_and_failure_cleanup() {
        for (fail, expected) in [
            (None, vec!["create", "assign", "resume"]),
            (Some("create"), vec!["create", "retain"]),
            (Some("assign"), vec!["create", "assign", "cleanup"]),
            (Some("resume"), vec!["create", "assign", "resume", "retain"]),
        ] {
            let mut p = Fake {
                steps: vec![],
                fail,
            };
            let _ = launch(&mut p);
            assert_eq!(p.steps, expected);
        }
    }
}

#[cfg(windows)]
mod native {
    use super::super::super::service::journal::{Journal, Phase as JournalPhase};
    use super::super::{
        AgentObservation, Clock, Deadline, ExitObservation, JobAdmission, SupportProof,
        WindowsNativeIo,
        process::{CallOwner, Dispatch, own::OwnProcessIdentity},
    };
    use super::*;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
    use windows_sys::Win32::{
        Foundation::*,
        System::{JobObjects::*, SystemInformation::GetTickCount64, Threading::*},
    };

    pub(crate) struct NativeSupervisorClock {
        original: Arc<OwnProcessIdentity>,
    }
    impl Clock for NativeSupervisorClock {
        fn now_ms(&self) -> u64 {
            // SAFETY: read-only process-independent uptime; no wall-clock or native object change.
            unsafe { GetTickCount64() }
        }
    }
    impl NativeSupervisorClock {
        pub(crate) fn epoch(&self) -> u64 {
            self.original.creation()
        }
    }
    struct Handles {
        process: Arc<OwnedHandle>,
        thread: Option<Arc<OwnedHandle>>,
        pid: u32,
        creation: u64,
        created_suspended: bool,
        never_resumed: bool,
    }
    // Failed-call output bits are untrusted correlation only; never wrapped, queried, closed,
    // logged or used to select a process when Win32 did not transfer successful ownership.
    struct UnknownOutputs {
        _process: usize,
        _thread: usize,
        _pid: u32,
        _thread_id: u32,
    }
    #[derive(Default)]
    struct State {
        control: ChildControl,
        job: Option<Arc<OwnedHandle>>,
        child: Option<Handles>,
        orphan_thread: Option<Arc<OwnedHandle>>,
        sequence: u64,
        cleanup_attempted: bool,
        cleanup_live: bool,
        current_output_received: bool,
        unknown_outputs: Option<UnknownOutputs>,
    }
    pub(crate) struct SupervisorOwner {
        admission: Arc<JobAdmission>,
        original: Arc<OwnProcessIdentity>,
        clock: Arc<NativeSupervisorClock>,
        calls: Arc<CallOwner>,
        state: Mutex<State>,
        exclusive: Mutex<Option<Arc<super::super::supervisor_owner::ExclusiveSupervisorLease>>>,
    }
    // One runtime per process. Lost delivery cannot drop a created process/job or free a retry slot.
    static OWNER: OnceLock<Arc<SupervisorOwner>> = OnceLock::new();
    pub(crate) struct CreatedChild {
        owner: Arc<SupervisorOwner>,
        sequence: u64,
        pid: u32,
        creation: u64,
    }
    impl CreatedChild {
        pub(crate) fn pid(&self) -> u32 {
            self.pid
        }
    }
    pub(crate) struct EmptyOwnedTree {
        owner: Arc<SupervisorOwner>,
        operation: [u8; 16],
        generation: Generation,
    }
    pub(crate) struct ChildStartPermit {
        owner: Arc<SupervisorOwner>,
        source: PermitSource,
    }
    enum PermitSource {
        #[cfg(not(test))]
        Initial(Arc<super::super::super::service::task::TaskRunPermit>),
        #[cfg(not(test))]
        Logon(Arc<super::super::super::service::SupervisorLogonPermit>),
        Restart(Journal),
    }
    impl ChildStartPermit {
        #[cfg(not(test))]
        pub(crate) fn initial(
            owner: Arc<SupervisorOwner>,
            permit: Arc<super::super::super::service::task::TaskRunPermit>,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<Arc<Self>> {
            if permit.owner_identity().pid() != owner.original.pid()
                || permit.owner_identity().creation() != owner.original.creation()
            {
                return Err(NativeError::Foreign);
            }
            let result = Arc::new(Self {
                owner,
                source: PermitSource::Initial(permit),
            });
            result.reverify(proof, deadline)?;
            Ok(result)
        }
        /// Separate genuine logon admission; never converts or resets an installer task claim.
        #[cfg(not(test))]
        pub(crate) fn initial_logon(
            owner: Arc<SupervisorOwner>,
            permit: Arc<super::super::super::service::SupervisorLogonPermit>,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<Arc<Self>> {
            if permit.owner_identity().pid() != owner.original.pid()
                || permit.owner_identity().creation() != owner.original.creation()
            {
                return Err(NativeError::Foreign);
            }
            let result = Arc::new(Self {
                owner,
                source: PermitSource::Logon(permit),
            });
            result.reverify(proof, deadline)?;
            Ok(result)
        }
        fn reverify(&self, proof: &SupportProof, deadline: &Deadline) -> NativeResult<()> {
            self.owner.reverify(proof, deadline)?;
            match &self.source {
                #[cfg(not(test))]
                PermitSource::Initial(permit) => {
                    permit.reverify(self.owner.admission.io(), proof, deadline)
                }
                #[cfg(not(test))]
                PermitSource::Logon(permit) => {
                    permit.reverify(self.owner.admission.io(), proof, deadline)?;
                    // Only the actual completed archive/Preparing admission can mint this gate.
                    // Retain it through the permit and renew before each native dispatch.
                    permit.reservation().ensure_epoch_prepared(
                        self.owner.admission.io(),
                        proof,
                        deadline,
                    )
                }
                PermitSource::Restart(record) => {
                    let current = Journal::read(self.owner.admission.io(), proof, deadline)?
                        .ok_or(NativeError::Missing)?;
                    if current != *record
                        || current.phase != JournalPhase::StartRequested
                        || current.clock_epoch != self.owner.clock.epoch()
                    {
                        return Err(NativeError::Foreign);
                    }
                    deadline.check()
                }
            }
        }
    }
    impl SupervisorOwner {
        pub(crate) fn prepare(
            admission: JobAdmission,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<Arc<Self>> {
            admission.reverify(proof, deadline)?;
            let original = Arc::new(admission.io().own_process_identity(proof, deadline)?);
            let clock = Arc::new(NativeSupervisorClock {
                original: original.clone(),
            });
            let owner = Arc::new(Self {
                admission: Arc::new(admission),
                original,
                clock,
                calls: Arc::new(CallOwner::default()),
                state: Mutex::new(State::default()),
                exclusive: Mutex::new(None),
            });
            OWNER.set(owner.clone()).map_err(|_| NativeError::Busy)?;
            let held = owner.clone();
            let budget = deadline.clone();
            let result = owner.calls.run(Dispatch::Mutation, deadline, move || {
                let proof = held.admission.io().admit_support(&budget)?;
                held.reverify(&proof, &budget)?;
                // SAFETY: no name and no inherited job handle; this creates only our own empty job.
                let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
                if raw.is_null() {
                    return Err(NativeError::Unavailable);
                }
                // SAFETY: successful non-null create transfers this real job handle.
                let job = Arc::new(unsafe { OwnedHandle::from_raw_handle(raw) });
                held.state().job = Some(job.clone()); // publish ownership BEFORE any late/failing result
                let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                // SAFETY: exact newly owned empty job and complete initialized limits. No breakaway flags.
                if unsafe {
                    SetInformationJobObject(
                        job.as_raw_handle(),
                        JobObjectExtendedLimitInformation,
                        (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                        std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                    )
                } == 0
                {
                    return Err(NativeError::Unavailable);
                }
                budget.check()
            });
            if result.is_err() {
                owner.retire();
            }
            result?;
            Ok(owner)
        }
        fn state(&self) -> MutexGuard<'_, State> {
            match self.state.lock() {
                Ok(state) => state,
                Err(error) => {
                    let mut state = error.into_inner();
                    state.control.retire();
                    state.control.phase = ChildPhase::Unknown;
                    state
                }
            }
        }
        fn reverify(&self, proof: &SupportProof, deadline: &Deadline) -> NativeResult<()> {
            // The own-process object cannot change, and this code cannot execute after its exit.
            // Fresh context and image revalidation run through the original IO's bounded owner.
            self.admission.reverify(proof, deadline)
        }
        pub(crate) fn bind_exclusive(
            &self,
            lease: Arc<super::super::supervisor_owner::ExclusiveSupervisorLease>,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.reverify(proof, deadline)?;
            lease.reverify(self.io(), proof, deadline)?;
            let mut slot = self
                .exclusive
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?;
            if let Some(old) = &*slot {
                return if Arc::ptr_eq(old, &lease) {
                    Ok(())
                } else {
                    Err(NativeError::Foreign)
                };
            }
            *slot = Some(lease);
            Ok(())
        }
        pub(crate) fn exclusive_lease(
            &self,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<Arc<super::super::supervisor_owner::ExclusiveSupervisorLease>> {
            self.ensure_exclusive(proof, deadline)?;
            self.exclusive
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?
                .clone()
                .ok_or(NativeError::Foreign)
        }
        fn ensure_exclusive(&self, proof: &SupportProof, deadline: &Deadline) -> NativeResult<()> {
            let lease = self
                .exclusive
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?
                .clone()
                .ok_or(NativeError::Foreign)?;
            lease.reverify(self.io(), proof, deadline)
        }
        pub(crate) fn clock(&self) -> Arc<NativeSupervisorClock> {
            self.clock.clone()
        }
        pub(crate) fn io(&self) -> &Arc<WindowsNativeIo> {
            self.admission.io()
        }
        pub(crate) fn terminal_operation(&self) -> Option<[u8; 16]> {
            self.state().control.terminal_operation()
        }
        pub(crate) fn arm_terminal(
            &self,
            generation: Generation,
            operation: [u8; 16],
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.reverify(proof, deadline)?;
            // Caller is the authenticated owner bridge, after freshly matching durable StopIntent.
            // This same native mutex excludes every future create/assign/resume.
            self.state().control.arm_terminal(operation, generation)
        }
        fn retire(self: &Arc<Self>) {
            let settle = self.state().control.retire();
            self.calls.retire_mutations();
            if settle {
                self.schedule_cleanup();
            }
        }
        fn schedule_cleanup(self: &Arc<Self>) {
            {
                let mut state = self.state();
                if state.cleanup_attempted || state.cleanup_live {
                    return;
                }
                let known = state
                    .child
                    .as_ref()
                    .is_some_and(|h| h.created_suspended && h.never_resumed);
                if !known {
                    state.control.phase = ChildPhase::Unknown;
                    return;
                }
                state.cleanup_attempted = true;
                state.cleanup_live = true;
                state.control.phase = ChildPhase::Settling;
            }
            // A caller timeout never executes or waits for OS cleanup. The already reserved
            // singleton and this worker retain the exact never-resumed child's real handles.
            let owner = self.clone();
            if std::thread::Builder::new()
                .name("crosspane-owned-child-cleanup".into())
                .spawn(move || owner.cleanup_claimed())
                .is_err()
            {
                let mut state = self.state();
                state.cleanup_live = false;
                state.control.cleanup_observed(false);
            }
        }
        fn job(&self) -> NativeResult<Arc<OwnedHandle>> {
            self.state().job.clone().ok_or(NativeError::OutcomeUnknown)
        }
        fn child(
            &self,
            child: &CreatedChild,
        ) -> NativeResult<(Arc<OwnedHandle>, Option<Arc<OwnedHandle>>)> {
            if !std::ptr::eq(self, Arc::as_ptr(&child.owner)) {
                return Err(NativeError::Foreign);
            }
            let state = self.state();
            let handles = state.child.as_ref().ok_or(NativeError::OutcomeUnknown)?;
            if state.sequence != child.sequence
                || handles.pid != child.pid
                || handles.creation != child.creation
            {
                return Err(NativeError::Foreign);
            }
            Ok((handles.process.clone(), handles.thread.clone()))
        }
        fn cleanup_claimed(&self) {
            let (process, job) = {
                let state = self.state();
                if state.control.phase != ChildPhase::Settling || !state.cleanup_live {
                    return;
                }
                (
                    state
                        .child
                        .as_ref()
                        .filter(|h| h.created_suspended && h.never_resumed)
                        .map(|h| h.process.clone()),
                    state.job.clone(),
                )
            };
            let (Some(process), Some(job)) = (process, job) else {
                let mut state = self.state();
                state.cleanup_live = false;
                state.control.cleanup_observed(false);
                return;
            };
            // SAFETY: only exact returned process positively never resumed under the native mutex.
            if unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } != WAIT_OBJECT_0 {
                // SAFETY: termination restricted to that owned never-resumed process; not an old/foreign tree.
                unsafe {
                    TerminateProcess(process.as_raw_handle(), 1);
                }
            }
            // SAFETY: actual owned process, independent cleanup bound, no mutex held across wait.
            let exited =
                unsafe { WaitForSingleObject(process.as_raw_handle(), 2000) } == WAIT_OBJECT_0;
            let empty = job_count(&job).is_ok_and(|n| n == 0);
            let mut state = self.state();
            state.cleanup_live = false;
            state.control.cleanup_observed(exited && empty);
        }
        fn create(
            self: &Arc<Self>,
            permit: Arc<ChildStartPermit>,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<CreatedChild> {
            if !Arc::ptr_eq(self, &permit.owner) {
                return Err(NativeError::Foreign);
            }
            permit.reverify(proof, deadline)?;
            self.ensure_exclusive(proof, deadline)?;
            {
                let mut state = self.state();
                state.control.claim_create()?;
                state.current_output_received = false;
            }
            let owner = self.clone();
            let budget = deadline.clone();
            let result = self.calls.run(Dispatch::Mutation, deadline, move || {
                let proof = owner.io().admit_support(&budget)?;
                permit.reverify(&proof, &budget)?;
                owner.ensure_exclusive(&proof, &budget)?;
                let path = owner.admission.application();
                let cwd = owner.admission.cwd();
                if path.contains(['\0', '"']) || cwd.contains('\0') {
                    return Err(NativeError::Foreign);
                }
                let application: Vec<u16> = path.encode_utf16().chain([0]).collect();
                let mut command: Vec<u16> =
                    format!("\"{path}\"").encode_utf16().chain([0]).collect();
                let directory: Vec<u16> = cwd.encode_utf16().chain([0]).collect();
                let startup = STARTUPINFOW {
                    cb: std::mem::size_of::<STARTUPINFOW>() as u32,
                    ..Default::default()
                };
                let mut info = PROCESS_INFORMATION::default();
                budget.check()?;
                owner.state().control.claim_native_create()?;
                // SAFETY: exact approved pinned full application, writable executable-only command;
                // no handle inheritance, own admitted fixed cwd, and complete native outputs.
                let ok = unsafe {
                    CreateProcessW(
                        application.as_ptr(),
                        command.as_mut_ptr(),
                        std::ptr::null(),
                        std::ptr::null(),
                        0,
                        CREATE_SUSPENDED | CREATE_NO_WINDOW,
                        std::ptr::null(),
                        directory.as_ptr(),
                        &startup,
                        &mut info,
                    )
                };
                if ok == 0 {
                    let unexpected = !info.hProcess.is_null()
                        || !info.hThread.is_null()
                        || info.dwProcessId != 0
                        || info.dwThreadId != 0;
                    let mut state = owner.state();
                    if unexpected {
                        state.unknown_outputs = Some(UnknownOutputs {
                            _process: info.hProcess as usize,
                            _thread: info.hThread as usize,
                            _pid: info.dwProcessId,
                            _thread_id: info.dwThreadId,
                        });
                    }
                    state.control.create_refused(unexpected);
                    return Err(if unexpected {
                        NativeError::OutcomeUnknown
                    } else {
                        NativeError::Unavailable
                    });
                }
                // SAFETY: each independently non-null successful CreateProcess output transfers ownership.
                let process = (!info.hProcess.is_null())
                    .then(|| Arc::new(unsafe { OwnedHandle::from_raw_handle(info.hProcess) }));
                // SAFETY: primary thread is a distinct independently owned returned handle.
                let thread = (!info.hThread.is_null())
                    .then(|| Arc::new(unsafe { OwnedHandle::from_raw_handle(info.hThread) }));
                let complete_outputs =
                    process.is_some() && thread.is_some() && info.dwProcessId != 0;
                let settle = {
                    let mut state = owner.state();
                    // Save every actual output BEFORE querying metadata or checking a late result.
                    // The mutex only stores memory; no process/job query runs under it.
                    if let Some(process) = &process {
                        state.current_output_received = true;
                        state.child = Some(Handles {
                            process: process.clone(),
                            thread: thread.clone(),
                            pid: info.dwProcessId,
                            creation: 0,
                            created_suspended: true,
                            never_resumed: true,
                        });
                    } else if let Some(thread) = &thread {
                        state.orphan_thread = Some(thread.clone());
                    }
                    state.control.created(complete_outputs)
                };
                if settle {
                    owner.schedule_cleanup();
                }
                let creation = process
                    .as_ref()
                    .map_or(0, |p| process_creation(p).unwrap_or(0));
                let actual_pid = process.as_ref().map_or(0, |p| {
                    // SAFETY: query on the exact returned process, never a recorded PID selection.
                    unsafe { GetProcessId(p.as_raw_handle()) }
                });
                let complete = complete_outputs && creation != 0 && actual_pid == info.dwProcessId;
                let mut state = owner.state();
                if let Some(child) = &mut state.child {
                    child.creation = creation;
                }
                if complete {
                    state.sequence = state
                        .sequence
                        .checked_add(1)
                        .ok_or(NativeError::OutcomeUnknown)?;
                }
                let value = state
                    .child
                    .as_ref()
                    .map(|h| (state.sequence, h.pid, h.creation));
                drop(state);
                if !complete {
                    owner.retire();
                    return Err(NativeError::OutcomeUnknown);
                }
                if owner.state().control.retired {
                    return Err(NativeError::OutcomeUnknown);
                }
                budget.check()?;
                let (sequence, pid, creation) = value.ok_or(NativeError::OutcomeUnknown)?;
                Ok(CreatedChild {
                    owner: owner.clone(),
                    sequence,
                    pid,
                    creation,
                })
            });
            if result.is_err() {
                self.retire();
            }
            result
        }
        fn assign(
            self: &Arc<Self>,
            child: &CreatedChild,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.reverify(proof, deadline)?;
            let (process, _) = self.child(child)?;
            self.state().control.claim_assign()?;
            let job = self.job()?;
            let owner = self.clone();
            let budget = deadline.clone();
            let result = self.calls.run(Dispatch::Mutation, deadline, move || {
                let proof = owner.io().admit_support(&budget)?;
                owner.reverify(&proof, &budget)?;
                budget.check()?;
                // SAFETY: actual returned suspended process with assignment rights and exact own job.
                let ok = unsafe {
                    AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle())
                } != 0;
                let settle = owner.state().control.assigned(ok);
                if settle {
                    owner.schedule_cleanup();
                }
                if !ok {
                    return Err(NativeError::Unavailable);
                }
                if owner.state().control.retired {
                    return Err(NativeError::OutcomeUnknown);
                }
                budget.check()
            });
            if result.is_err() {
                self.retire();
            }
            result
        }
        fn resume(
            self: &Arc<Self>,
            child: &CreatedChild,
            permit: Arc<ChildStartPermit>,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if !Arc::ptr_eq(self, &permit.owner) {
                return Err(NativeError::Foreign);
            }
            permit.reverify(proof, deadline)?;
            let (_, thread) = self.child(child)?;
            let thread = thread.ok_or(NativeError::OutcomeUnknown)?;
            let owner = self.clone();
            let budget = deadline.clone();
            let result = self.calls.run(Dispatch::Mutation, deadline, move || {
                let proof = owner.io().admit_support(&budget)?;
                permit.reverify(&proof, &budget)?;
                owner.ensure_exclusive(&proof, &budget)?;
                budget.check()?;
                // The same mutex as terminal retirement makes resume admission irrevocable.
                {
                    let mut state = owner.state();
                    state.control.claim_resume()?;
                    // This irreversible claim precedes the OS call under the same terminal gate.
                    // Neither an error nor a timeout may classify this child as never resumed.
                    state
                        .child
                        .as_mut()
                        .ok_or(NativeError::OutcomeUnknown)?
                        .never_resumed = false;
                }
                // SAFETY: exact retained primary thread of our one assigned suspended child, once only.
                let previous = unsafe { ResumeThread(thread.as_raw_handle()) };
                owner.state().control.resumed(previous)?;
                budget.check()
            });
            if result.is_err() {
                self.retire();
            }
            result
        }
        pub(crate) fn launch(
            self: &Arc<Self>,
            permit: Arc<ChildStartPermit>,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<CreatedChild> {
            struct Port<'a> {
                owner: Arc<SupervisorOwner>,
                permit: Arc<ChildStartPermit>,
                proof: &'a SupportProof,
                deadline: &'a Deadline,
            }
            impl ChildJobPort for Port<'_> {
                type Child = CreatedChild;
                fn create_suspended(&mut self) -> NativeResult<CreatedChild> {
                    self.owner
                        .create(self.permit.clone(), self.proof, self.deadline)
                }
                fn assign(&mut self, child: &CreatedChild) -> NativeResult<()> {
                    self.owner.assign(child, self.proof, self.deadline)
                }
                fn resume(&mut self, child: &CreatedChild) -> NativeResult<()> {
                    self.owner
                        .resume(child, self.permit.clone(), self.proof, self.deadline)
                }
                fn cleanup_never_resumed(&mut self, _: &CreatedChild) -> NativeResult<()> {
                    self.owner.retire();
                    if self.owner.state().control.phase == ChildPhase::Settled {
                        Ok(())
                    } else {
                        Err(NativeError::OutcomeUnknown)
                    }
                }
                fn retain_unknown(&mut self) {
                    self.owner.retire();
                }
            }
            super::launch(&mut Port {
                owner: self.clone(),
                permit,
                proof,
                deadline,
            })
        }
        /// Exact CreatedChild observation only: stale bootstrap data never selects another PID.
        pub(crate) fn created_exit(
            &self,
            child: &CreatedChild,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<Option<u32>> {
            self.reverify(proof, deadline)?;
            let (process, _) = self.child(child)?;
            let creation = child.creation;
            let budget = deadline.clone();
            self.calls.run(Dispatch::Observation, deadline, move || {
                if process_creation(&process)? != creation {
                    return Err(NativeError::Foreign);
                }
                let code = process_exit(&process)?;
                budget.check()?;
                Ok(code)
            })
        }
        pub(crate) fn admit_ready(
            &self,
            child: &CreatedChild,
            original: &AgentObservation,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<Generation> {
            self.reverify(proof, deadline)?;
            self.admission.agent_matches(original, proof, deadline)?;
            let (process, _) = self.child(child)?;
            let bootstrap = original.bootstrap();
            let creation = original.creation_time(self.io(), proof, deadline)?;
            if bootstrap.pid != child.pid
                || creation != child.creation
                || !original.in_job(self.io(), proof, self.job()?, deadline)?
            {
                return Err(NativeError::Foreign);
            }
            let budget = deadline.clone();
            self.calls.run(Dispatch::Observation, deadline, move || {
                if process_creation(&process)? != creation || process_exit(&process)?.is_some() {
                    return Err(NativeError::Foreign);
                }
                budget.check()
            })?;
            let generation = Generation {
                pid: bootstrap.pid,
                creation,
                instance: bootstrap.instance_id,
            };
            self.state().control.bind_ready(generation)?;
            Ok(generation)
        }
        /// The actual known-crash and empty native tree, plus the durable StartRequested record,
        /// select a restart. A serialized Generation alone can never mint this permit.
        pub(crate) fn restart_permit(
            self: &Arc<Self>,
            record: &Journal,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<Arc<ChildStartPermit>> {
            self.reverify(proof, deadline)?;
            self.ensure_exclusive(proof, deadline)?;
            if record.phase != JournalPhase::StartRequested
                || record.clock_epoch != self.clock.epoch()
                || Journal::read(self.io(), proof, deadline)?.as_ref() != Some(record)
            {
                return Err(NativeError::Foreign);
            }
            let (generation, process) = {
                let state = self.state();
                if state.control.retired
                    || state.control.terminal.is_some()
                    || state.control.phase != ChildPhase::Running
                {
                    return Err(NativeError::OutcomeUnknown);
                }
                let generation = state.control.generation.ok_or(NativeError::Missing)?;
                if record.current != Some(generation) {
                    return Err(NativeError::Foreign);
                }
                let child = state.child.as_ref().ok_or(NativeError::Missing)?;
                (generation, child.process.clone())
            };
            let budget = deadline.clone();
            let exited = self.calls.run(Dispatch::Observation, deadline, move || {
                budget.check()?;
                if process_creation(&process)? != generation.creation {
                    return Err(NativeError::Foreign);
                }
                let code = process_exit(&process)?;
                budget.check()?;
                Ok(code)
            })?;
            if exited.is_none_or(|code| code == 0) || self.job_active(proof, deadline)? != 0 {
                return Err(NativeError::Foreign);
            }
            self.state().control.reset_after_exit_and_empty()?;
            Ok(Arc::new(ChildStartPermit {
                owner: self.clone(),
                source: PermitSource::Restart(record.clone()),
            }))
        }
        /// Lead f912268c: adopt an actual same-job, same-approved-image clean successor through its
        /// retained A2 object. No PID reopen, execution pin or mutable tree authority is synthesized.
        pub(crate) fn adopt_successor(
            self: &Arc<Self>,
            previous: &AgentObservation,
            next: &AgentObservation,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<Generation> {
            self.reverify(proof, deadline)?;
            self.ensure_exclusive(proof, deadline)?;
            self.admission.agent_matches(previous, proof, deadline)?;
            self.admission.agent_matches(next, proof, deadline)?;
            let old = {
                let state = self.state();
                if state.control.retired || state.control.terminal.is_some() {
                    return Err(NativeError::OutcomeUnknown);
                }
                state.control.generation.ok_or(NativeError::Missing)?
            };
            if previous.bootstrap().pid != old.pid
                || previous.bootstrap().instance_id != old.instance
            {
                return Err(NativeError::Foreign);
            }
            match previous.observe_exit(self.io(), proof, deadline)? {
                ExitObservation::Exited {
                    creation,
                    code,
                    receipt,
                } => {
                    if creation != old.creation
                        || code != 0
                        || receipt.is_none_or(|r| !r.clean || r.instance_id != old.instance)
                    {
                        return Err(NativeError::Foreign);
                    }
                }
                ExitObservation::Running => return Err(NativeError::Busy),
            }
            if !next.in_job(self.io(), proof, self.job()?, deadline)? {
                return Err(NativeError::Foreign);
            }
            let generation = Generation {
                pid: next.bootstrap().pid,
                creation: next.creation_time(self.io(), proof, deadline)?,
                instance: next.bootstrap().instance_id,
            };
            let process = self.admission.retained_agent(next, proof, deadline)?;
            let checked = process.clone();
            let budget = deadline.clone();
            self.calls.run(Dispatch::Observation, deadline, move || {
                if process_creation(&checked)? != generation.creation
                    || process_exit(&checked)?.is_some()
                {
                    return Err(NativeError::Foreign);
                }
                budget.check()
            })?;
            let mut state = self.state();
            state.control.follow_ready(old, generation)?;
            // Native handle is stored before return/publication. The original A2 object remains
            // separately retained by the caller/server through its own completion checks.
            state.child = Some(Handles {
                process,
                thread: None,
                pid: generation.pid,
                creation: generation.creation,
                created_suspended: false,
                never_resumed: false,
            });
            state.sequence = state
                .sequence
                .checked_add(1)
                .ok_or(NativeError::OutcomeUnknown)?;
            Ok(generation)
        }
        pub(crate) fn process_for_peer(
            &self,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<Arc<OwnedHandle>> {
            self.reverify(proof, deadline)?;
            Ok(self.original.handle())
        }
        pub(crate) fn child_for_peer(
            &self,
            original: &AgentObservation,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<Arc<OwnedHandle>> {
            self.reverify(proof, deadline)?;
            self.admission.agent_matches(original, proof, deadline)?;
            let creation = original.creation_time(self.io(), proof, deadline)?;
            let state = self.state();
            let generation = state.control.generation.ok_or(NativeError::Missing)?;
            let child = state.child.as_ref().ok_or(NativeError::Missing)?;
            if state.control.phase != ChildPhase::Running
                || generation.pid != original.bootstrap().pid
                || generation.creation != creation
                || generation.instance != original.bootstrap().instance_id
                || child.pid != generation.pid
                || child.creation != generation.creation
            {
                return Err(NativeError::Foreign);
            }
            let process = child.process.clone();
            drop(state);
            if !original.in_job(self.io(), proof, self.job()?, deadline)? {
                return Err(NativeError::Foreign);
            }
            let checked = process.clone();
            let budget = deadline.clone();
            self.calls.run(Dispatch::Observation, deadline, move || {
                if process_creation(&checked)? != creation || process_exit(&checked)?.is_some() {
                    return Err(NativeError::Foreign);
                }
                budget.check()
            })?;
            Ok(process)
        }
        pub(crate) fn job_active(
            &self,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<u32> {
            self.reverify(proof, deadline)?;
            let job = self.job()?;
            let budget = deadline.clone();
            self.calls.run(Dispatch::Observation, deadline, move || {
                budget.check()?;
                let result = job_count(&job)?;
                budget.check()?;
                Ok(result)
            })
        }
        pub(crate) fn settled_for_stop(
            self: &Arc<Self>,
            proof: &SupportProof,
            original: &AgentObservation,
            deadline: &Deadline,
        ) -> NativeResult<Option<EmptyOwnedTree>> {
            self.reverify(proof, deadline)?;
            self.admission.agent_matches(original, proof, deadline)?;
            let (operation, generation) = {
                let state = self.state();
                if state.control.retired {
                    return Err(NativeError::OutcomeUnknown);
                }
                state.control.terminal.ok_or(NativeError::Foreign)?
            };
            if generation.pid != original.bootstrap().pid
                || generation.instance != original.bootstrap().instance_id
            {
                return Err(NativeError::Foreign);
            }
            match original.observe_exit(self.io(), proof, deadline)? {
                ExitObservation::Running => return Ok(None),
                ExitObservation::Exited {
                    creation,
                    code,
                    receipt,
                } => {
                    if creation != generation.creation
                        || code != 0
                        || receipt.is_none_or(|r| !r.clean || r.instance_id != generation.instance)
                    {
                        return Err(NativeError::Foreign);
                    }
                }
            }
            if self.job_active(proof, deadline)? != 0 {
                return Ok(None);
            }
            let state = self.state();
            if state.control.terminal != Some((operation, generation)) {
                return Err(NativeError::Foreign);
            }
            deadline.check()?;
            Ok(Some(EmptyOwnedTree {
                owner: self.clone(),
                operation,
                generation,
            }))
        }
        pub(crate) fn job_for_empty(
            &self,
            empty: &EmptyOwnedTree,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<Arc<OwnedHandle>> {
            if !std::ptr::eq(self, Arc::as_ptr(&empty.owner)) {
                return Err(NativeError::Foreign);
            }
            self.reverify(proof, deadline)?;
            {
                let state = self.state();
                if state.control.retired
                    || state.control.terminal != Some((empty.operation, empty.generation))
                {
                    return Err(NativeError::Foreign);
                }
            }
            if self.job_active(proof, deadline)? != 0 {
                return Err(NativeError::Busy);
            }
            deadline.check()?;
            self.job()
        }
    }
    /// Lead d776409a: the main process must not disappear while a submitted Create/Assign or
    /// unassigned cleanup can still deliver an original handle. This path dispatches no launch,
    /// resume, broker arm or RPC, and never mints RetainedTreeCompletion.
    pub(crate) fn observe_failed_entry(
        deadline: &Deadline,
    ) -> NativeResult<ErrorRetentionObservation> {
        deadline.check()?;
        let Some(owner) = OWNER.get() else {
            return Ok(ErrorRetentionObservation::Settled);
        };
        owner.calls.retire_mutations();
        if !owner.calls.idle() {
            return Ok(ErrorRetentionObservation::Pending);
        }
        let (process, job, cleanup_live, known_cleanup, no_child, unaccounted) = {
            let state = owner.state();
            let known_cleanup = state
                .child
                .as_ref()
                .is_some_and(|h| h.created_suspended && h.never_resumed)
                && !state.cleanup_attempted;
            let owns_output = state.child.is_some()
                || state.orphan_thread.is_some()
                || state.unknown_outputs.is_some();
            let no_child = state
                .control
                .no_child_after_settled_call(false, owns_output);
            let unaccounted = state.unknown_outputs.is_some()
                || state.orphan_thread.is_some()
                || (state.control.create_dispatched
                    && !state.control.create_refused_without_outputs
                    && !state.current_output_received);
            (
                state.child.as_ref().map(|h| h.process.clone()),
                state.job.clone(),
                state.cleanup_live,
                known_cleanup,
                no_child,
                unaccounted,
            )
        };
        if unaccounted {
            return Ok(ErrorRetentionObservation::Pending);
        }
        if cleanup_live {
            return Ok(ErrorRetentionObservation::Pending);
        }
        if known_cleanup {
            owner.schedule_cleanup();
            return Ok(ErrorRetentionObservation::Pending);
        }
        if no_child {
            return Ok(ErrorRetentionObservation::Settled);
        }
        let (Some(process), Some(job)) = (process, job) else {
            return Ok(ErrorRetentionObservation::Pending);
        };
        let budget = deadline.clone();
        let settled = owner.calls.run(Dispatch::Observation, deadline, move || {
            // Only actual retained kernel objects are queried, regardless of obsolete record/path.
            let exited = process_exit(&process)?.is_some();
            let empty = job_count(&job)? == 0;
            budget.check()?;
            Ok(exited && empty)
        })?;
        // A late callback or cleanup still owns its objects until the busy/live state is settled.
        if settled && owner.calls.idle() && !owner.state().cleanup_live {
            Ok(ErrorRetentionObservation::Settled)
        } else {
            Ok(ErrorRetentionObservation::Pending)
        }
    }
    fn process_creation(process: &OwnedHandle) -> NativeResult<u64> {
        let (mut creation, mut exit, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        // SAFETY: exact owned query process and complete distinct writable FILETIME outputs.
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
        let result = (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
        if result == 0 {
            return Err(NativeError::Foreign);
        }
        Ok(result)
    }
    fn process_exit(process: &OwnedHandle) -> NativeResult<Option<u32>> {
        // SAFETY: exact retained query/synchronize process, zero wait.
        match unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                // SAFETY: signalled retained process and complete writable code output.
                if unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) } == 0 {
                    return Err(NativeError::Unavailable);
                }
                Ok(Some(code))
            }
            _ => Err(NativeError::Unavailable),
        }
    }
    fn job_count(job: &OwnedHandle) -> NativeResult<u32> {
        if job.as_raw_handle().is_null() {
            return Err(NativeError::Foreign);
        }
        let mut value = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        let mut written = 0;
        // SAFETY: non-null actual own job, fixed complete accounting output, no PID enumeration.
        if unsafe {
            QueryInformationJobObject(
                job.as_raw_handle(),
                JobObjectBasicAccountingInformation,
                (&mut value as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                &mut written,
            )
        } == 0
            || written != std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32
        {
            return Err(NativeError::Unavailable);
        }
        Ok(value.ActiveProcesses)
    }
}
#[cfg(windows)]
pub(crate) use native::{ChildStartPermit, CreatedChild, SupervisorOwner, observe_failed_entry};
