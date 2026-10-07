//! Operation-bound task and supervisor decisions. Model facts never grant native authority.
//! Genuine fixed-role inventory supplies image approval. Native A3 activation and whole-tree
//! completion remain Unsupported until WP-W4.1a4b; image release is not tree completion.

#[path = "service/journal.rs"]
pub mod journal;
#[path = "service/supervisor.rs"]
pub mod supervisor;
#[path = "service/task.rs"]
pub mod task;

use super::native_io::{NativeError, NativeResult};

/// Only genuine fixed-role approval can construct the production image capability.
struct TrustedImages {
    _sealed: (),
    #[cfg(all(windows, not(test)))]
    _native: std::sync::Arc<NativeImages>,
}
impl TrustedImages {
    fn current() -> NativeResult<Self> {
        #[cfg(all(windows, not(test)))]
        {
            Self::current_native()
        }
        #[cfg(any(not(windows), test))]
        {
            // The unchanged A3 fake roots never inspect native folders or gain launch authority.
            Err(NativeError::Unsupported)
        }
    }
    #[cfg(test)]
    fn fixture() -> Self {
        Self { _sealed: () }
    }
}

#[cfg(all(windows, not(test)))]
struct NativeImages {
    io: std::sync::Arc<super::native_io::WindowsNativeIo>,
    _root: super::native_io::PayloadRoot,
    // A self-pin is an own opened module, not a hash adopted from the stage catalog.
    module: super::native_io::SelfImagePin,
    installer: super::native_io::OpenedPe,
    agent: super::native_io::OpenedPe,
    ui: super::native_io::OpenedPe,
}

#[cfg(all(windows, not(test)))]
impl TrustedImages {
    fn current_native() -> NativeResult<Self> {
        use super::native_io::{Cancellation, Deadline, MonotonicClock, WindowsNativeIo};
        use super::payload::inventory::{ApprovedInventory, ApprovedPe, PayloadRole};
        use std::sync::Arc;

        // Missing or invalid build-time provenance refuses BEFORE any native context is opened.
        let inventory = ApprovedInventory::embedded()?;
        let clock: Arc<dyn super::native_io::Clock> = Arc::new(MonotonicClock::default());
        let deadline = Deadline::new(30_000, clock.clone(), Cancellation::default())?;
        let io = Arc::new(WindowsNativeIo::current(clock.clone(), &deadline)?);
        let proof = io.admit_support(&deadline)?;
        let module = io.self_image(&proof, &deadline)?;
        let installer_pin = ApprovedPe::own_image(&module)?;
        let proof = io.admit_support(&deadline)?;
        let lock = io.acquire_installer_lock(&proof, &deadline)?;
        let proof = io.admit_support(&deadline)?;
        let root = io.payload_root(&proof, &lock, &deadline)?;
        let proof = io.admit_support(&deadline)?;
        let installer = root.open_approved(
            &io,
            &proof,
            PayloadRole::Installer,
            &installer_pin,
            &deadline,
        )?;
        let proof = io.admit_support(&deadline)?;
        let agent = root.open_approved(
            &io,
            &proof,
            PayloadRole::Agent,
            inventory.role(PayloadRole::Agent)?,
            &deadline,
        )?;
        let proof = io.admit_support(&deadline)?;
        let ui = root.open_approved(
            &io,
            &proof,
            PayloadRole::Ui,
            inventory.role(PayloadRole::Ui)?,
            &deadline,
        )?;
        // Read-only root/image pins outlive this constructor; the operation lock MUST NOT.
        drop(lock);
        let native = Arc::new(NativeImages {
            io,
            _root: root,
            module,
            installer,
            agent,
            ui,
        });
        native.reverify(&deadline)?;
        Ok(Self {
            _sealed: (),
            _native: native,
        })
    }
}

#[cfg(all(windows, not(test)))]
impl NativeImages {
    fn reverify(&self, deadline: &super::native_io::Deadline) -> NativeResult<()> {
        let proof = self.io.admit_support(deadline)?;
        self.module.reverify(&self.io, &proof, deadline)?;
        for image in [&self.installer, &self.agent, &self.ui] {
            let proof = self.io.admit_support(deadline)?;
            image.reverify(&self.io, &proof, deadline)?;
        }
        deadline.check()
    }
}

/// Complete old-origin/tree settlement, not an image-open or record observation.
/// WP-W4.1a4b must supply the production native factory; only fakes can mint this in a4.
pub(crate) struct UpgradeStopProof {
    operation: [u8; 16],
    original_instance: Option<u64>,
    _sealed: (),
}
impl UpgradeStopProof {
    pub(crate) fn operation(&self) -> [u8; 16] {
        self.operation
    }
    pub(crate) fn original_instance(&self) -> Option<u64> {
        self.original_instance
    }
    #[cfg(test)]
    #[allow(dead_code)] // Used by the source-included windows_payload/windows_upgrade integration roots.
    pub(crate) fn fixture(
        operation: [u8; 16],
        original_instance: Option<u64>,
    ) -> NativeResult<Self> {
        if operation == [0; 16] || original_instance == Some(0) {
            return Err(NativeError::Invalid);
        }
        Ok(Self {
            operation,
            original_instance,
            _sealed: (),
        })
    }
}

/// Exact new image and authenticated u64 instance evidence. Never deserialized as authority.
pub(crate) struct NewInstanceEvidence {
    operation: [u8; 16],
    instance: u64,
    image_identity: super::native_io::files::FileIdentity,
    _sealed: (),
}
impl NewInstanceEvidence {
    pub(crate) fn operation(&self) -> [u8; 16] {
        self.operation
    }
    pub(crate) fn instance(&self) -> u64 {
        self.instance
    }
    pub(crate) fn image_identity(&self) -> super::native_io::files::FileIdentity {
        self.image_identity
    }
    #[cfg(test)]
    #[allow(dead_code)] // Used by the source-included windows_payload/windows_upgrade integration roots.
    pub(crate) fn fixture(
        operation: [u8; 16],
        instance: u64,
        image_identity: super::native_io::files::FileIdentity,
    ) -> NativeResult<Self> {
        if operation == [0; 16]
            || instance == 0
            || image_identity.volume == 0
            || image_identity.file == [0; 16]
        {
            return Err(NativeError::Invalid);
        }
        Ok(Self {
            operation,
            instance,
            image_identity,
            _sealed: (),
        })
    }
}

#[cfg(any(windows, test))]
// Source-included focused integration tests consume this; unit-test native entry is absent.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn record_instance(value: &str) -> NativeResult<u64> {
    if value.len() != 32
        || value
            .bytes()
            .any(|b| !b.is_ascii_digit() && !(b'a'..=b'f').contains(&b))
    {
        return Err(NativeError::Invalid);
    }
    let instance = u64::from_str_radix(value, 16).map_err(|_| NativeError::Invalid)?;
    if instance == 0 || format!("{instance:032x}") != value {
        return Err(NativeError::Invalid);
    }
    Ok(instance)
}

/// Only the bounded initial post-start observation treats a stale predecessor as pending.
/// Once a NEW candidate exists, FileId, Status, context and owned-child failures remain strict.
#[cfg(any(windows, test))]
// Source-included focused integration tests consume this; unit-test native entry is absent.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn pending_start_observation(error: NativeError) -> bool {
    matches!(
        error,
        NativeError::Missing | NativeError::Busy | NativeError::Unavailable | NativeError::Foreign
    )
}
#[cfg(any(windows, test))]
// Source-included focused integration tests consume this; unit-test native entry is absent.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn new_ready_candidate(
    phase: &crate::agent_contract::BootstrapPhase,
    instance: u64,
    previous: Option<u64>,
) -> NativeResult<bool> {
    if instance == 0 {
        return Err(NativeError::Invalid);
    }
    Ok(*phase == crate::agent_contract::BootstrapPhase::Ready && Some(instance) != previous)
}

/// The constructor remains inert. Native use requires the caller's ORIGINAL nonce-bound IO and
/// (for stop only) an alias of the actual already-held installer lock; no fresh current context.
#[cfg(any(windows, test))]
pub(crate) struct NativeUpgradePort {
    #[cfg(all(windows, not(test)))]
    native: Option<upgrade::Bound>,
    _sealed: (),
}
#[cfg(any(windows, test))]
impl NativeUpgradePort {
    pub(crate) fn new() -> Self {
        Self {
            #[cfg(all(windows, not(test)))]
            native: None,
            _sealed: (),
        }
    }
    #[cfg(windows)]
    pub(crate) fn bind_io(
        &mut self,
        io: std::sync::Arc<super::native_io::WindowsNativeIo>,
        deadline: &super::native_io::Deadline,
    ) -> NativeResult<()> {
        #[cfg(not(test))]
        {
            if let Some(bound) = &self.native {
                if !std::sync::Arc::ptr_eq(&io, &bound.io) {
                    return Err(NativeError::Foreign);
                }
                return deadline.check();
            }
            let proof = io.admit_support(deadline)?;
            proof.check(&io, deadline)?;
            self.native = Some(upgrade::Bound::new(io, deadline.clone()));
            Ok(())
        }
        #[cfg(test)]
        {
            let _ = (io, deadline);
            Err(NativeError::Unsupported)
        }
    }
    #[cfg(all(windows, not(test)))]
    pub(crate) fn bind_outer_completion(
        &mut self,
        admission: super::native_io::OuterCompletionAdmission,
        deadline: &super::native_io::Deadline,
    ) -> NativeResult<()> {
        let bound = self.native.as_mut().ok_or(NativeError::Unsupported)?;
        admission.reverify(&bound.io, &bound.io.admit_support(deadline)?, deadline)?;
        if bound.stop.attempted || bound.outer.is_some() {
            return Err(NativeError::OutcomeUnknown);
        }
        bound.outer = Some(admission);
        Ok(())
    }
    #[cfg(all(windows, not(test)))]
    fn bind_keeper_source(
        &mut self,
        selected: &super::native_io::SelectedOuterOperation,
        lock: &super::native_io::InstallerLock,
        deadline: &super::native_io::Deadline,
    ) -> NativeResult<()> {
        self.bind_io(selected.io().clone(), deadline)?;
        let bound = self.native.as_mut().ok_or(NativeError::Unsupported)?;
        selected.reverify(
            &bound.io,
            &bound.io.admit_support(deadline)?,
            lock,
            deadline,
        )?;
        selected.record().peer_matches(
            selected.owner_identity().pid(),
            selected.owner_identity().creation(),
            super::payload::recovery::FileStamp {
                volume: selected.module().identity().volume,
                file: selected.module().identity().file,
            },
            selected.module().facts(),
            bound.io.target().identity(),
        )?;
        if bound.stop.attempted || bound.keeper_operation.is_some() {
            return Err(NativeError::OutcomeUnknown);
        }
        bound.keeper_operation = Some(selected.operation());
        Ok(())
    }
    #[cfg(all(windows, not(test)))]
    fn renew_keeper_observation(
        &mut self,
        deadline: &super::native_io::Deadline,
    ) -> NativeResult<()> {
        let bound = self.native.as_mut().ok_or(NativeError::Unsupported)?;
        if bound.keeper_operation.is_none() {
            return Err(NativeError::Unsupported);
        }
        bound
            .io
            .admit_support(deadline)?
            .check(&bound.io, deadline)?;
        // Renew only a bounded observation/recovery call on the same resident source. None of
        // the consumed Stop/Run/native mutation flags, original handles or IO nonce is reset.
        bound.deadline = deadline.clone();
        Ok(())
    }
    #[cfg(windows)]
    pub(crate) fn bind_stop_lock(
        &mut self,
        lock: &super::native_io::InstallerLock,
        deadline: &super::native_io::Deadline,
    ) -> NativeResult<()> {
        #[cfg(not(test))]
        {
            let bound = self.native.as_mut().ok_or(NativeError::Unsupported)?;
            if bound.stop.attempted || bound.lease.is_some() {
                return Err(NativeError::OutcomeUnknown);
            }
            let proof = bound.io.admit_support(deadline)?;
            bound.lease = Some(bound.io.lease_stop_lock(&proof, lock, deadline)?);
            Ok(())
        }
        #[cfg(test)]
        {
            let _ = (lock, deadline);
            Err(NativeError::Unsupported)
        }
    }
}
#[cfg(any(windows, test))]
impl super::payload::health::ServicePort for NativeUpgradePort {
    fn stop_for_replace(&mut self, operation: [u8; 16]) -> NativeResult<UpgradeStopProof> {
        #[cfg(all(windows, not(test)))]
        {
            let bound = self.native.as_ref().ok_or(NativeError::Unsupported)?;
            if bound.keeper_operation == Some(operation)
                && bound.outer.is_none()
                && !bound.stop.attempted
            {
                let lease = bound.lease.as_ref().ok_or(NativeError::Unsupported)?;
                let deadline = bound.deadline.clone();
                let proof = bound.io.admit_support(&deadline)?;
                let admission = bound.io.prepare_outer_completion(
                    &proof,
                    lease.lock(),
                    operation,
                    &deadline,
                )?;
                self.bind_outer_completion(admission, &deadline)?;
            }
            self.native
                .as_mut()
                .ok_or(NativeError::Unsupported)?
                .stop(operation)
        }
        #[cfg(any(not(windows), test))]
        {
            let _ = operation;
            Err(NativeError::Unsupported)
        }
    }
    fn start_once(
        &mut self,
        operation: [u8; 16],
        payload: &super::payload::health::VerifiedPayload,
    ) -> NativeResult<NewInstanceEvidence> {
        #[cfg(all(windows, not(test)))]
        {
            self.native
                .as_mut()
                .ok_or(NativeError::Unsupported)?
                .start(operation, payload)
        }
        #[cfg(any(not(windows), test))]
        {
            let _ = (operation, payload);
            Err(NativeError::Unsupported)
        }
    }
    fn recover_stop(
        &mut self,
        record: &super::payload::recovery::OperationRecord,
    ) -> NativeResult<UpgradeStopProof> {
        #[cfg(all(windows, not(test)))]
        {
            self.native
                .as_mut()
                .ok_or(NativeError::Unsupported)?
                .recover_stop(record)
        }
        #[cfg(any(not(windows), test))]
        {
            let _ = record;
            Err(NativeError::Unsupported)
        }
    }
    fn recover_started(
        &mut self,
        record: &super::payload::recovery::OperationRecord,
        payload: &super::payload::health::VerifiedPayload,
    ) -> NativeResult<Option<NewInstanceEvidence>> {
        #[cfg(all(windows, not(test)))]
        {
            self.native
                .as_mut()
                .ok_or(NativeError::Unsupported)?
                .recover_started(record, payload)
                .map(Some)
        }
        #[cfg(any(not(windows), test))]
        {
            let _ = (record, payload);
            Err(NativeError::Unsupported)
        }
    }
}

#[cfg(all(windows, not(test)))]
mod upgrade {
    use super::super::{
        native_io::{
            self, AgentObservation, Deadline, WindowsNativeIo,
            supervisor_owner::{
                AdmittedSupervisorOwner, RetainedTreeCompletion, StopSequence, TerminalStopPort,
            },
        },
        payload::{health::VerifiedPayload, recovery},
        transport::WindowsAgentPort,
    };
    use super::*;
    use crate::agent_contract::{
        AgentCall, AgentPort, DecodedReply, InstallerRequest, ObservationSource, StatusAdmission,
    };
    use std::sync::Arc;

    pub(super) struct Bound {
        pub(super) io: Arc<WindowsNativeIo>,
        pub(super) deadline: Deadline,
        pub(super) lease: Option<Arc<native_io::StopLockLease>>,
        pub(super) stop: Attempt,
        sequence: StopSequence,
        owner: Option<AdmittedSupervisorOwner>,
        completion: Option<Arc<RetainedTreeCompletion>>,
        start_attempted: bool,
        pub(super) keeper_operation: Option<[u8; 16]>,
        pub(super) outer: Option<native_io::OuterCompletionAdmission>,
        stop_journal: Option<journal::Journal>,
        stop_settlement: Option<super::super::transport::StopSettlement>,
        new_ready: Option<Arc<NewInstanceEvidence>>,
    }
    #[derive(Default)]
    pub(super) struct Attempt {
        pub(super) attempted: bool,
    }
    impl Bound {
        pub(super) fn new(io: Arc<WindowsNativeIo>, deadline: Deadline) -> Self {
            Self {
                io,
                deadline,
                lease: None,
                stop: Attempt::default(),
                sequence: StopSequence::default(),
                owner: None,
                completion: None,
                start_attempted: false,
                keeper_operation: None,
                outer: None,
                stop_journal: None,
                stop_settlement: None,
                new_ready: None,
            }
        }
        fn selected(
            &self,
            operation: [u8; 16],
            phase: recovery::Phase,
        ) -> NativeResult<recovery::OperationRecord> {
            if operation == [0; 16] {
                return Err(NativeError::Invalid);
            }
            let proof = self.io.admit_support(&self.deadline)?;
            let record = recovery::selected_operation(&self.io, &proof, &self.deadline)?
                .ok_or(NativeError::Missing)?;
            if record.operation() != operation || record.phase() != phase {
                return Err(NativeError::Foreign);
            }
            Ok(record)
        }
        pub(super) fn stop(&mut self, operation: [u8; 16]) -> NativeResult<UpgradeStopProof> {
            if self.stop.attempted {
                return Err(NativeError::OutcomeUnknown);
            }
            self.selected(operation, recovery::Phase::StopIntent)?;
            let lease = self
                .lease
                .as_ref()
                .cloned()
                .ok_or(NativeError::Unsupported)?;
            let proof = self.io.admit_support(&self.deadline)?;
            lease.reverify(&proof, &self.deadline)?;
            // Reserve before any persistence or RPC. No error can make a second attempt legal.
            self.stop.attempted = true;
            let original = self.io.observe_agent(&proof, &self.deadline)?;
            let selected = self
                .io
                .agent_generation(&original, &proof, &self.deadline)?;
            let journal = journal::Journal::read(&self.io, &proof, &self.deadline)?
                .ok_or(NativeError::Missing)?;
            if journal.current != Some(selected) {
                return Err(NativeError::Foreign);
            }
            let retained = self.io.clone_agent(&original, &proof, &self.deadline)?;
            self.owner = Some(if let Some(outer) = self.outer.take() {
                AdmittedSupervisorOwner::admit_outer(
                    self.io.clone(),
                    retained,
                    Some(lease.clone()),
                    outer,
                    &self.deadline,
                )?
            } else {
                AdmittedSupervisorOwner::admit(
                    self.io.clone(),
                    retained,
                    Some(lease.clone()),
                    &self.deadline,
                )?
            });
            let port = WindowsAgentPort::new(
                self.io.clone(),
                self.io.admit_support(&self.deadline)?,
                self.io.bound_clock(),
                &self.deadline,
            )?;
            self.stop_settlement = Some(port.stop_settlement());
            // Both journal and transport settlement survive a bounded return/panic after RPC.
            self.stop_journal = Some(journal);
            let timeout = self
                .deadline
                .remaining_ms()?
                .min(crate::agent_contract::MAX_TIMEOUT_MS);
            let native = supervisor::NativeStop {
                io: &self.io,
                proof: &proof,
                lock: lease.lock(),
                journal: self.stop_journal.as_mut().ok_or(NativeError::Foreign)?,
                port: Some(port),
                deadline: &self.deadline,
                timeout_ms: timeout,
            };
            let mut ordered = OrderedStop {
                native,
                owner: self.owner.as_ref().ok_or(NativeError::Foreign)?,
                operation,
                selected,
                deadline: &self.deadline,
            };
            let result = self.sequence.run_once(&mut ordered);
            drop(ordered);
            if !self.sequence.submitted() {
                return Err(result.err().unwrap_or(NativeError::OutcomeUnknown));
            }
            // An ACK is never completion. Even an uncertain RPC may be resolved ONLY by the
            // actual original supervisor exit + empty original job + exact clean agent receipt.
            let completed = self
                .owner
                .as_ref()
                .ok_or(NativeError::Foreign)?
                .observe_completion(operation, &self.deadline)?;
            completed.reverify(&self.deadline)?;
            // Reserve the successful actual proof BEFORE any subsequent deadline/endpoint check.
            self.completion = Some(Arc::new(completed));
            self.stop_settlement
                .as_ref()
                .ok_or(NativeError::Foreign)?
                .wait(&self.deadline)?;
            drop(original);
            let journal = self.stop_journal.as_mut().ok_or(NativeError::Foreign)?;
            journal.phase = journal::Phase::Finished;
            journal.stop_instance = None;
            let proof = self.io.admit_support(&self.deadline)?;
            journal.publish(&self.io, &proof, lease.lock(), &self.deadline)?;
            // Broker settlement already closed every old connected pipe and old image pin.
            // ALL local stop-lease aliases are released before the caller's owned-lock handoff.
            drop(lease);
            self.lease.take();
            self.owner.take();
            let record = self.selected(operation, recovery::Phase::StopIntent)?;
            self.recover_stop(&record)
        }
        pub(super) fn recover_stop(
            &mut self,
            record: &recovery::OperationRecord,
        ) -> NativeResult<UpgradeStopProof> {
            if self.keeper_operation == Some(record.operation())
                && self.sequence.submitted()
                && (self.lease.is_some() || self.owner.is_some())
            {
                self.settle_keeper_stop(record.operation())?;
            }
            if self.lease.is_some() || self.owner.is_some() {
                return Err(NativeError::OutcomeUnknown);
            }
            let completed = self.completion.as_ref().ok_or(NativeError::Unsupported)?;
            if record.operation() != completed.operation() {
                return Err(NativeError::Foreign);
            }
            completed.reverify(&self.deadline)?;
            let proof = self.io.admit_support(&self.deadline)?;
            let current = recovery::selected_operation(&self.io, &proof, &self.deadline)?
                .ok_or(NativeError::Foreign)?;
            if serde_json::to_vec(&current).map_err(|_| NativeError::Invalid)?
                != serde_json::to_vec(record).map_err(|_| NativeError::Invalid)?
            {
                return Err(NativeError::Foreign);
            }
            Ok(UpgradeStopProof {
                operation: completed.operation(),
                original_instance: Some(completed.original_instance()),
                _sealed: (),
            })
        }
        fn settle_keeper_stop(&mut self, operation: [u8; 16]) -> NativeResult<()> {
            // This is only the SAME resident source's observation. It never sends Arm/Complete/
            // Stop again and cannot construct completion from journal/ACK/process numbers.
            let owner = self.owner.as_ref().ok_or(NativeError::OutcomeUnknown)?;
            let completed = owner
                .observe_retained_completion(&self.deadline)?
                .ok_or(NativeError::Busy)?;
            if completed.operation() != operation {
                return Err(NativeError::Foreign);
            }
            completed.reverify(&self.deadline)?;
            self.completion = Some(completed);
            self.stop_settlement
                .as_ref()
                .ok_or(NativeError::OutcomeUnknown)?
                .wait(&self.deadline)?;
            let lease = self.lease.as_ref().ok_or(NativeError::OutcomeUnknown)?;
            let journal = self
                .stop_journal
                .as_mut()
                .ok_or(NativeError::OutcomeUnknown)?;
            journal.phase = journal::Phase::Finished;
            journal.stop_instance = None;
            let proof = self.io.admit_support(&self.deadline)?;
            let current = journal::Journal::read(&self.io, &proof, &self.deadline)?
                .ok_or(NativeError::Foreign)?;
            if current.encode()? != journal.encode()? {
                journal.publish(&self.io, &proof, lease.lock(), &self.deadline)?;
            }
            self.owner.take();
            self.lease.take();
            self.stop_settlement.take();
            Ok(())
        }
        pub(super) fn verify_keeper_completion(
            &self,
            record: &recovery::OperationRecord,
        ) -> NativeResult<()> {
            let evidence = self.new_ready.as_ref().ok_or(NativeError::OutcomeUnknown)?;
            if self.keeper_operation != Some(record.operation())
                || record.phase() != recovery::Phase::Complete
                || evidence.operation() != record.operation()
                || record.new_instance().map(record_instance).transpose()?
                    != Some(evidence.instance())
                || self.lease.is_some()
                || self.owner.is_some()
            {
                return Err(NativeError::OutcomeUnknown);
            }
            let proof = self.io.admit_support(&self.deadline)?;
            let agent = self.io.observe_agent(&proof, &self.deadline)?;
            if agent.bootstrap().instance_id != evidence.instance()
                || !new_ready_candidate(&agent.bootstrap().phase, evidence.instance(), None)?
                || self.io.agent_identity(&agent, &proof, &self.deadline)?
                    != evidence.image_identity()
            {
                return Err(NativeError::Foreign);
            }
            ready_status(&self.io, &agent, &self.deadline)?;
            let original = self.io.clone_agent(&agent, &proof, &self.deadline)?;
            let source =
                AdmittedSupervisorOwner::admit(self.io.clone(), original, None, &self.deadline)?;
            source.close_ready(&self.deadline)?;
            agent.revalidate(&self.io, &proof, &self.deadline)?;
            Ok(())
        }
        pub(super) fn start(
            &mut self,
            operation: [u8; 16],
            payload: &VerifiedPayload,
        ) -> NativeResult<NewInstanceEvidence> {
            if self.start_attempted || self.lease.is_some() || self.owner.is_some() {
                return Err(NativeError::OutcomeUnknown);
            }
            self.selected(operation, recovery::Phase::StartIntent)?;
            if operation != payload.operation() {
                return Err(NativeError::Foreign);
            }
            let proof = self.io.admit_support(&self.deadline)?;
            payload.reverify(&self.io, &proof, &self.deadline)?;
            let selection = task::prepare_upgrade(self.io.clone(), payload, &self.deadline)?;
            // The outer coordinator has dropped its actual lock. No alias is held here.
            let trusted = TrustedImages::current()?;
            self.start_attempted = true;
            let evidence = task::start_upgrade(&trusted, &selection, &self.deadline)?;
            if evidence.operation() != operation {
                return Err(NativeError::Foreign);
            }
            drop(trusted);
            drop(selection);
            let record = self.selected(operation, recovery::Phase::StartIntent)?;
            self.recover_started(&record, payload)
        }
        pub(super) fn recover_started(
            &mut self,
            record: &recovery::OperationRecord,
            payload: &VerifiedPayload,
        ) -> NativeResult<NewInstanceEvidence> {
            if self.lease.is_some()
                || self.owner.is_some()
                || record.operation() != payload.operation()
            {
                return Err(NativeError::Unsupported);
            }
            let proof = self.io.admit_support(&self.deadline)?;
            payload.reverify(&self.io, &proof, &self.deadline)?;
            let current = recovery::selected_operation(&self.io, &proof, &self.deadline)?
                .ok_or(NativeError::Foreign)?;
            if serde_json::to_vec(&current).map_err(|_| NativeError::Invalid)?
                != serde_json::to_vec(record).map_err(|_| NativeError::Invalid)?
            {
                return Err(NativeError::Foreign);
            }
            let previous = record
                .original_instance()
                .map(record_instance)
                .transpose()?;
            loop {
                self.deadline.check()?;
                let proof = self.io.admit_support(&self.deadline)?;
                match self.io.observe_agent(&proof, &self.deadline) {
                    Ok(agent) => {
                        let instance = agent.bootstrap().instance_id;
                        if !new_ready_candidate(&agent.bootstrap().phase, instance, previous)? {
                            std::thread::sleep(std::time::Duration::from_millis(20));
                            continue;
                        }
                        let image = self.io.agent_identity(&agent, &proof, &self.deadline)?;
                        if image != payload.agent_identity()? {
                            return Err(NativeError::Foreign);
                        }
                        ready_status(&self.io, &agent, &self.deadline)?;
                        let retained = self.io.clone_agent(&agent, &proof, &self.deadline)?;
                        // Fresh source handshake validates actual SAME own-job child lineage;
                        // task EnginePID, claim and scheduler GUID are never readiness authority.
                        let owner = AdmittedSupervisorOwner::admit(
                            self.io.clone(),
                            retained,
                            None,
                            &self.deadline,
                        )?;
                        agent.revalidate(&self.io, &proof, &self.deadline)?;
                        payload.reverify(&self.io, &proof, &self.deadline)?;
                        owner.close_ready(&self.deadline)?;
                        let evidence = Arc::new(NewInstanceEvidence {
                            operation: record.operation(),
                            instance,
                            image_identity: image,
                            _sealed: (),
                        });
                        self.new_ready = Some(evidence.clone());
                        // Same actual authenticated readiness result is retained before delivery;
                        // neither a Complete record nor a caller result can reconstruct it.
                        return Ok(NewInstanceEvidence {
                            operation: evidence.operation,
                            instance: evidence.instance,
                            image_identity: evidence.image_identity,
                            _sealed: (),
                        });
                    }
                    Err(error) if pending_start_observation(error) => {
                        std::thread::sleep(std::time::Duration::from_millis(20))
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    }
    struct OrderedStop<'a> {
        native: supervisor::NativeStop<'a>,
        owner: &'a AdmittedSupervisorOwner,
        operation: [u8; 16],
        selected: supervisor::Generation,
        deadline: &'a Deadline,
    }
    impl TerminalStopPort for OrderedStop<'_> {
        fn persist(&mut self) -> NativeResult<()> {
            supervisor::StopPort::persist_stop(&mut self.native, self.selected.instance)
        }
        fn arm(&mut self) -> NativeResult<()> {
            self.owner.arm_terminal(self.operation, self.deadline)
        }
        fn submit(&mut self) -> NativeResult<()> {
            supervisor::StopPort::submit_stop(&mut self.native, self.selected.instance)
        }
    }
    fn ready_status(
        io: &Arc<WindowsNativeIo>,
        original: &AgentObservation,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        let proof = io.admit_support(deadline)?;
        let mut port = WindowsAgentPort::new(io.clone(), proof, io.bound_clock(), deadline)?;
        port.submit(AgentCall {
            id: 1,
            request: InstallerRequest::Status,
            timeout_ms: deadline
                .remaining_ms()?
                .min(crate::agent_contract::MAX_TIMEOUT_MS),
        })
        .map_err(|_| NativeError::Unavailable)?;
        loop {
            deadline.check()?;
            let mut replies = port.poll();
            if replies.len() > 1 {
                return Err(NativeError::Foreign);
            }
            if let Some(reply) = replies.pop() {
                if reply.id != 1 || reply.source != ObservationSource::Live {
                    return Err(NativeError::Foreign);
                }
                match reply.result {
                    Ok(DecodedReply::Status(StatusAdmission::Supported(health))) => {
                        let instance = &health.installer().instance;
                        if instance.id != original.bootstrap().instance_id
                            || instance.pid != original.bootstrap().pid
                            || instance.uid.is_some()
                        {
                            return Err(NativeError::Foreign);
                        }
                        original.revalidate(io, &io.admit_support(deadline)?, deadline)?;
                        return Ok(());
                    }
                    _ => return Err(NativeError::Unavailable),
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

/// Separate sole-argument keeper entry; it never falls through to GUI or supervisor mode.
pub fn upgrade_keeper_mode(arguments: &[std::ffi::OsString]) -> NativeResult<bool> {
    const FLAG: &str = "--windows-upgrade-keeper";
    let requested = arguments.iter().any(|arg| {
        arg == FLAG
            || arg
                .to_str()
                .is_some_and(|value| value.starts_with("--windows-upgrade-keeper="))
    });
    if !requested {
        return Ok(false);
    }
    if arguments.len() != 1 || arguments[0] != FLAG {
        return Err(NativeError::Invalid);
    }
    Ok(true)
}
/// Only the genuine fixed-copy/native entry can admit a keeper. No argv path/PID/op selects it.
pub fn upgrade_keeper_entry() -> NativeResult<()> {
    #[cfg(all(windows, not(test)))]
    {
        keeper_runtime::entry()
    }
    #[cfg(any(not(windows), test))]
    {
        Err(NativeError::Unsupported)
    }
}

#[cfg(all(windows, not(test)))]
mod keeper_runtime {
    use super::super::{
        native_io::{
            self, Cancellation, Deadline, MonotonicClock, WindowsNativeIo,
            activation::{
                KeeperApplyAttempt, KeeperControl, KeeperPort, KeeperProgress, KeeperStage,
            },
            keeper::{KeeperContinuation, KeeperServer},
        },
        payload::{
            self, ApprovedOuterSources, WindowsPayload,
            recovery::{self, OperationRecord, Phase, RecoveryDecision},
        },
    };
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    fn budget(io: &WindowsNativeIo, ms: u64) -> NativeResult<Deadline> {
        Deadline::new(ms, io.bound_clock(), Cancellation::default())
    }
    type Session = Arc<Mutex<KeeperServer>>;
    struct ResidentPort {
        io: Arc<WindowsNativeIo>,
        pre_deadline: Deadline,
        initial: Option<KeeperServer>,
        sources: Option<ApprovedOuterSources>,
        payload: Option<WindowsPayload>,
        operation: Option<OperationRecord>,
        service: NativeUpgradePort,
        apply_attempt: KeeperApplyAttempt,
    }
    impl ResidentPort {
        fn record(&mut self, deadline: &Deadline) -> NativeResult<OperationRecord> {
            let proof = self.io.admit_support(deadline)?;
            if let Some(record) = recovery::selected_operation(&self.io, &proof, deadline)? {
                return Ok(record);
            }
            let operation = self
                .operation
                .as_ref()
                .ok_or(NativeError::Foreign)?
                .operation();
            let observed = self
                .io
                .read_record(
                    &proof,
                    native_io::records::RecordName::Operation(operation),
                    native_io::files::MAX_RECORD_BYTES,
                    deadline,
                )?
                .ok_or(NativeError::Foreign)?;
            let record: OperationRecord = native_io::records::record_data(
                &native_io::records::RecordName::Operation(operation),
                observed.bytes(),
            )?;
            record.validate()?;
            if record.operation() != operation
                || !matches!(record.phase(), Phase::Complete | Phase::RolledBack)
            {
                return Err(NativeError::Foreign);
            }
            Ok(record)
        }
    }
    impl KeeperPort for ResidentPort {
        type Child = Session;
        fn prepare(&mut self) -> NativeResult<Session> {
            self.pre_deadline.check()?;
            let mut server = self.initial.take().ok_or(NativeError::OutcomeUnknown)?;
            let result = (|| {
                let sources = server.receive_sources(&self.pre_deadline)?;
                // Legacy development servers are refused BEFORE readiness/commit, without creating
                // a terminal latch or leaving an unsupported post-Stop resident operation.
                if let Err(error) = super::super::native_io::supervisor_owner::probe_outer_support(
                    self.io.clone(),
                    &self.io.admit_support(&self.pre_deadline)?,
                    server.selected(),
                    &self.pre_deadline,
                ) {
                    if error == NativeError::Unsupported {
                        let _ = server.refuse_reinstall_required(&self.pre_deadline);
                    }
                    return Err(error);
                }
                let proof = self.io.admit_support(&self.pre_deadline)?;
                let lock = self.io.acquire_installer_lock(&proof, &self.pre_deadline)?;
                let proof = self.io.admit_support(&self.pre_deadline)?;
                server
                    .selected()
                    .reverify(&self.io, &proof, &lock, &self.pre_deadline)?;
                self.payload = Some(WindowsPayload::new(
                    self.io.clone(),
                    &proof,
                    &lock,
                    &self.pre_deadline,
                )?);
                self.service
                    .bind_keeper_source(server.selected(), &lock, &self.pre_deadline)?;
                self.operation = Some(self.record(&self.pre_deadline.clone())?);
                self.sources = Some(sources);
                drop(lock);
                Ok(())
            })();
            if let Err(error) = result {
                // This source has not accepted Commit and has no service Stop. Retain the loaded
                // copy until exit; only publish cancellation through this actual keeper selection.
                let _ = cancel_selected(&self.io, server.selected());
                return Err(error);
            }
            Ok(Arc::new(Mutex::new(server)))
        }
        fn mark_ready(&mut self, child: &Session) -> NativeResult<()> {
            self.pre_deadline.check()?;
            let mut server = child.lock().map_err(|_| NativeError::OutcomeUnknown)?;
            let proof = self.io.admit_support(&self.pre_deadline)?;
            let lock = self.io.acquire_installer_lock(&proof, &self.pre_deadline)?;
            let proof = self.io.admit_support(&self.pre_deadline)?;
            server
                .selected()
                .mark_ready(&proof, &lock, &self.pre_deadline)?;
            drop(lock);
            server.announce_ready(&self.pre_deadline)
        }
        fn commit_intent(&mut self, child: &Session) -> NativeResult<()> {
            // The absolute pre-Stop source/commit window is not renewed by late transport work.
            self.pre_deadline.check()?;
            let mut server = child.lock().map_err(|_| NativeError::OutcomeUnknown)?;
            let proof = self.io.admit_support(&self.pre_deadline)?;
            let lock = self.io.acquire_installer_lock(&proof, &self.pre_deadline)?;
            let proof = self.io.admit_support(&self.pre_deadline)?;
            server
                .selected()
                .mark_committed(&proof, &lock, &self.pre_deadline)?;
            drop(lock);
            server.acknowledge_commit(&self.pre_deadline);
            Ok(())
        }
        fn apply_once(&mut self, _: &Session) -> NativeResult<KeeperProgress> {
            let deadline = budget(&self.io, 30_000)?;
            self.service.renew_keeper_observation(&deadline)?;
            let proof = self.io.admit_support(&deadline)?;
            let lock = self.io.acquire_installer_lock(&proof, &deadline)?;
            if !self.io.native_idle() {
                return Err(NativeError::Busy);
            }
            let payload = self.payload.as_ref().ok_or(NativeError::Foreign)?;
            let record = self.operation.as_mut().ok_or(NativeError::Foreign)?;
            if record.phase() != Phase::Intent {
                return Err(NativeError::OutcomeUnknown);
            }
            self.apply_attempt.reserve()?;
            // Actual owned-lock apply persists A4 StopIntent immediately before the service stop
            // callback, where lazy genuine .outer completion admission is constructed.
            payload.apply_owned(
                &mut self.service,
                lock,
                record,
                self.sources.as_ref().ok_or(NativeError::Foreign)?.inputs(),
                &deadline,
            )?;
            if record.phase() != Phase::Complete {
                return Err(NativeError::OutcomeUnknown);
            }
            Ok(KeeperProgress::Complete)
        }
        fn recover_same_owner(&mut self, child: &Session) -> NativeResult<KeeperProgress> {
            let deadline = budget(&self.io, 30_000)?;
            self.service.renew_keeper_observation(&deadline)?;
            if !self.apply_attempt.started() {
                // Only this actual resident retained a live Commit request and the independently
                // prepared source bundle. A cold record or merely Ready state cannot enter here.
                if !self.io.native_idle() || self.sources.is_none() || self.payload.is_none() {
                    return Err(NativeError::Busy);
                }
                let proof = self.io.admit_support(&deadline)?;
                let lock = self.io.acquire_installer_lock(&proof, &deadline)?;
                let proof = self.io.admit_support(&deadline)?;
                let server = child.lock().map_err(|_| NativeError::OutcomeUnknown)?;
                server
                    .selected()
                    .reverify(&self.io, &proof, &lock, &deadline)?;
                let selected = self.io.observe_outer_operation(&proof, &lock, &deadline)?;
                if selected.phase() != recovery::OuterPhase::Committed {
                    return Err(NativeError::OutcomeUnknown);
                }
                let record = self.record(&deadline)?;
                if record.operation() != server.selected().operation()
                    || record.phase() != Phase::Intent
                {
                    return Err(NativeError::OutcomeUnknown);
                }
                drop(server);
                drop(lock);
                return self.apply_once(child);
            }
            // Resolve any late actual original completion before trying another coordinator
            // lock: the resident service may still retain the original stop lease.
            let record = self.record(&deadline)?;
            if !matches!(
                record.phase(),
                Phase::StartIntent
                    | Phase::NewInstanceObserved
                    | Phase::Verified
                    | Phase::PruneIntent
                    | Phase::Complete
            ) {
                use payload::health::ServicePort;
                self.service.recover_stop(&record)?;
            }
            let proof = self.io.admit_support(&deadline)?;
            let lock = self.io.acquire_installer_lock(&proof, &deadline)?;
            let mut record = self.record(&deadline)?;
            let result = self
                .payload
                .as_ref()
                .ok_or(NativeError::Foreign)?
                .recover_owned(
                    &mut self.service,
                    lock,
                    &mut record,
                    self.sources.as_ref().ok_or(NativeError::Foreign)?.inputs(),
                    &deadline,
                )?;
            self.operation = Some(record);
            match result {
                RecoveryDecision::Complete => Ok(KeeperProgress::Complete),
                _ => Ok(KeeperProgress::Pending),
            }
        }
        fn settle(&mut self, child: &Session) -> NativeResult<()> {
            let deadline = budget(&self.io, 30_000)?;
            if !self.io.native_idle() {
                return Err(NativeError::Busy);
            }
            let proof = self.io.admit_support(&deadline)?;
            let lock = self.io.acquire_installer_lock(&proof, &deadline)?;
            let server = child.lock().map_err(|_| NativeError::OutcomeUnknown)?;
            let proof = self.io.admit_support(&deadline)?;
            let selected = server.selected();
            let record = self
                .io
                .read_record(
                    &proof,
                    native_io::records::RecordName::Operation(selected.operation()),
                    native_io::files::MAX_RECORD_BYTES,
                    &deadline,
                )?
                .ok_or(NativeError::Foreign)?;
            let operation: OperationRecord = native_io::records::record_data(
                &native_io::records::RecordName::Operation(selected.operation()),
                record.bytes(),
            )?;
            operation.validate()?;
            if operation.phase() != Phase::Complete {
                return Err(NativeError::OutcomeUnknown);
            }
            // Release coordinator lock before source handshake; namespace/selected cap remain held.
            drop(lock);
            self.service.renew_keeper_observation(&deadline)?;
            self.service
                .native
                .as_ref()
                .ok_or(NativeError::OutcomeUnknown)?
                .verify_keeper_completion(&operation)?;
            let proof = self.io.admit_support(&deadline)?;
            let lock = self.io.acquire_installer_lock(&proof, &deadline)?;
            let proof = self.io.admit_support(&deadline)?;
            selected.mark_complete(&proof, &lock, &deadline)?;
            drop(lock);
            Ok(())
        }
        fn cancel_before_stop(&mut self, child: &Session) -> NativeResult<()> {
            let deadline = budget(&self.io, 30_000)?;
            let proof = self.io.admit_support(&deadline)?;
            let lock = self.io.acquire_installer_lock(&proof, &deadline)?;
            let proof = self.io.admit_support(&deadline)?;
            child
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?
                .selected()
                .mark_cancelled(&proof, &lock, &deadline)?;
            // Executing copy stays retained. Never invoke old Stage rollback while it is loaded;
            // a later actual settled-copy cleanup reopens only this creator FileId under a lock.
            Ok(())
        }
    }
    fn cancel_selected(
        io: &Arc<WindowsNativeIo>,
        selected: &native_io::SelectedOuterOperation,
    ) -> NativeResult<()> {
        let deadline = budget(io, 30_000)?;
        let proof = io.admit_support(&deadline)?;
        let lock = io.acquire_installer_lock(&proof, &deadline)?;
        let proof = io.admit_support(&deadline)?;
        selected.reverify(io, &proof, &lock, &deadline)?;
        let record =
            recovery::selected_operation(io, &proof, &deadline)?.ok_or(NativeError::Foreign)?;
        if record.operation() != selected.operation() || record.phase() != Phase::Intent {
            return Err(NativeError::OutcomeUnknown);
        }
        selected.mark_cancelled(&proof, &lock, &deadline)
    }
    pub(super) fn entry() -> NativeResult<()> {
        let clock: Arc<dyn native_io::Clock> = Arc::new(MonotonicClock::default());
        let initial = Deadline::new(30_000, clock.clone(), Cancellation::default())?;
        // Build approval must exist before any native keeper context/connection is admitted.
        payload::inventory::ApprovedInventory::embedded()?;
        let io = Arc::new(WindowsNativeIo::current(clock, &initial)?);
        payload::recover_prior_logon_files_for_entry(&io, &initial)?;
        if super::removal_native::maybe_entry(
            io.clone(),
            &initial,
            super::super::removal::inventory::RemovalCopyKind::Keeper,
        )? {
            return Ok(());
        }
        let proof = io.admit_support(&initial)?;
        let lock = io.acquire_installer_lock(&proof, &initial)?;
        let proof = io.admit_support(&initial)?;
        payload::recover_prior_logon_files(&io, &proof, &lock, &initial)?;
        // File-only retirement never substitutes for this keeper's actual source admission.
        let proof = io.admit_support(&initial)?;
        let selected = io.keeper_selection(&proof, &lock, &initial)?;
        let server = KeeperServer::reserve(io.clone(), selected, &initial)?;
        drop(lock);
        let mut port = ResidentPort {
            io: io.clone(),
            pre_deadline: initial.clone(),
            initial: Some(server),
            sources: None,
            payload: None,
            operation: None,
            service: NativeUpgradePort::new(),
            apply_attempt: KeeperApplyAttempt::default(),
        };
        let mut control = KeeperControl::default();
        if let Err(error) = control.prepare(&mut port) {
            if let Ok(child) = control.child()
                && let Ok(server) = child.lock()
            {
                let _ = cancel_selected(&io, server.selected());
            }
            return Err(error);
        }
        let commit = control
            .child()?
            .lock()
            .map_err(|_| NativeError::OutcomeUnknown)?
            .wait_commit(&initial);
        match commit {
            Ok(true) => {}
            Ok(false) => {
                control.cancel(&mut port)?;
                return Ok(());
            }
            Err(error) => {
                let _ = control.cancel(&mut port);
                return Err(error);
            }
        }
        let mut previous = control.stage();
        let _ = control.commit(&mut port);
        if !control.committed() {
            return Err(NativeError::OutcomeUnknown);
        }
        loop {
            let stage = control.stage();
            if stage != previous {
                eprintln!("Crosspane upgrade keeper state: {stage:?}");
                previous = stage;
            }
            let observation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let observe = budget(&io, 100)?;
                control
                    .child()?
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?
                    .poll_observer(stage, &observe)
            }));
            let _ = observation;
            if stage == KeeperStage::Complete {
                return Ok(());
            }
            // Even poison/cancellation/panic in one bounded observation preserves this actual
            // resident port, source namespace and retained stop result. No post-commit wall cap.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                control.recover(&mut port)
            }));
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    pub(super) fn begin(inputs: Vec<payload::PayloadInput>) -> NativeResult<KeeperContinuation> {
        let inventory = payload::inventory::ApprovedInventory::embedded()?;
        let clock: Arc<dyn native_io::Clock> = Arc::new(MonotonicClock::default());
        let deadline = Deadline::new(30_000, clock.clone(), Cancellation::default())?;
        let io = Arc::new(WindowsNativeIo::current(clock, &deadline)?);
        let proof = io.admit_support(&deadline)?;
        let lock = io.acquire_installer_lock(&proof, &deadline)?;
        let proof = io.admit_support(&deadline)?;
        payload::recover_prior_logon_files(&io, &proof, &lock, &deadline)?;
        // A same-logon resident/readonly continuation is still admitted by the old live path.
        let proof = io.admit_support(&deadline)?;
        if let Some(prior) = recovery::OuterUpgradeRecord::read(&io, &proof, &deadline)? {
            if matches!(
                prior.phase(),
                recovery::OuterPhase::Complete | recovery::OuterPhase::Cancelled
            ) {
                // Terminal history never proves an old process exited. The genuine FS-only
                // vacancy/exact-file/positive-absence capability is required before replacing it.
                let absent = io.cleanup_cold_keeper(&proof, &lock, &deadline)?;
                let proof = io.admit_support(&deadline)?;
                recovery::retire_outer_terminal(&io, &proof, &lock, &absent, &deadline)?;
                drop(absent);
            } else {
                if prior.phase() == recovery::OuterPhase::Unknown {
                    return Err(NativeError::OutcomeUnknown);
                }
                let selected = io.select_outer_operation(&proof, &lock, &deadline)?;
                drop(lock);
                return super::admit_keeper_continuation(io, &proof, &selected, &deadline);
            }
        }
        drop(lock);
        let proof = io.admit_support(&deadline)?;
        let module = io.self_image(&proof, &deadline)?;
        let pin = payload::inventory::ApprovedPe::own_image(&module)?;
        let sources = payload::buffer_outer_sources(inputs, &inventory, &pin, &deadline)?;
        let proof = io.admit_support(&deadline)?;
        let lock = io.acquire_installer_lock(&proof, &deadline)?;
        let mut op = [0; 16];
        aws_lc_rs::rand::fill(&mut op).map_err(|_| NativeError::Unavailable)?;
        let proof = io.admit_support(&deadline)?;
        let selected = io.begin_outer_operation(&proof, &lock, op, &sources, &deadline)?;
        let proof = io.admit_support(&deadline)?;
        let prepared = payload::helper::prepare_keeper(
            io.clone(),
            &proof,
            &lock,
            &selected,
            &module,
            sources,
            &deadline,
        )?;
        // The prepared actual owner retains the ONLY remaining lock alias through Create; it
        // drops that alias after ResumeIntent before Resume, so child fresh admission cannot deadlock.
        drop(lock);
        let child = payload::helper::launch_keeper(prepared, &deadline)?;
        child.await_ready(&deadline)?.commit(&deadline)
    }
}

/// An opaque actual keeper observer; it confers no native/image approval.
#[cfg(all(windows, not(test)))]
#[doc(hidden)]
pub use super::native_io::keeper::KeeperContinuation;

/// Installer operation facade. Streams are exactly Agent, UI, then Ctl; executing-build approval
/// verifies every byte independently. Calling this is the actual same-operation commit request.
/// Normal Windows GUI wiring is deferred to WP-W4.1a7.
#[cfg(all(windows, not(test)))]
#[doc(hidden)]
pub fn begin_outer_upgrade(
    inputs: [Box<dyn std::io::Read + Send>; 3],
) -> NativeResult<KeeperContinuation> {
    use super::payload::{PayloadInput, inventory::PayloadRole};
    let inputs = [PayloadRole::Agent, PayloadRole::Ui, PayloadRole::Ctl]
        .into_iter()
        .zip(inputs)
        .map(|(role, content)| PayloadInput { role, content })
        .collect();
    keeper_runtime::begin(inputs)
}
#[cfg(all(windows, not(test)))]
pub(crate) fn admit_keeper_continuation(
    io: std::sync::Arc<super::native_io::WindowsNativeIo>,
    proof: &super::native_io::SupportProof,
    selected: &super::native_io::SelectedOuterOperation,
    deadline: &super::native_io::Deadline,
) -> NativeResult<super::native_io::keeper::KeeperContinuation> {
    super::native_io::keeper::KeeperContinuation::admit(io, proof, selected, deadline)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupervisorExit {
    NormalQuit,
    InstallerStopped,
}

/// File recovery runs before fixed images are opened. Its result grants no startup authority.
#[cfg(all(windows, not(test)))]
fn recover_prior_logon_files_before_images() -> NativeResult<()> {
    use super::native_io::{Cancellation, Deadline, MonotonicClock, WindowsNativeIo};
    use std::sync::Arc;

    // Executing-build approval is independent of whichever fixed images need repair.
    super::payload::inventory::ApprovedInventory::embedded()?;
    let clock: Arc<dyn super::native_io::Clock> = Arc::new(MonotonicClock::default());
    let deadline = Deadline::new(30_000, clock.clone(), Cancellation::default())?;
    let io = Arc::new(WindowsNativeIo::current(clock, &deadline)?);
    super::payload::recover_prior_logon_files_for_entry(&io, &deadline)?;
    // The preflight releases any own lease before unchanged TrustedImages takes a fresh lease.
    Ok(())
}

/// Windows-only early entry; it cannot start a GUI or manufacture image approval.
#[cfg(windows)]
pub fn supervisor_entry() -> NativeResult<SupervisorExit> {
    #[cfg(not(test))]
    recover_prior_logon_files_before_images()?;
    let trusted = TrustedImages::current()?;
    supervisor::native_entry(&trusted)
}

/// A4 must supply genuine fixed installer/agent role and PE/hash approval before this can start.
pub fn start_supported() -> NativeResult<()> {
    #[cfg(all(windows, not(test)))]
    recover_prior_logon_files_before_images()?;
    let trusted = TrustedImages::current()?;
    task::native_start(&trusted)
}

/// This fixed mode accepts no additional argv and confers no execution authority.
pub fn supervisor_mode(arguments: &[std::ffi::OsString]) -> NativeResult<bool> {
    if !arguments
        .iter()
        .any(|argument| argument == task::SUPERVISOR_ARGUMENT)
    {
        return Ok(false);
    }
    if arguments.len() != 1 {
        return Err(NativeError::Invalid);
    }
    Ok(true)
}

/// Opaque observation of the genuine planned-removal keeper. No native authority is public.
#[doc(hidden)]
pub struct RemovalContinuation {
    #[cfg(all(windows, not(test)))]
    state: removal_native::ContinuationState,
}
impl std::fmt::Debug for RemovalContinuation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RemovalContinuation")
    }
}
impl RemovalContinuation {
    /// Bounded genuine same-owner observation; this method never submits Stop, erase or deletion.
    pub fn status(&self) -> NativeResult<&'static str> {
        #[cfg(all(windows, not(test)))]
        {
            self.state.status()
        }
        #[cfg(any(not(windows), test))]
        {
            Err(NativeError::Unsupported)
        }
    }
}
/// Begin planned removal through the genuine surviving keeper. `false` preserves identity,
/// configuration, logs and state. Normal GUI activation is a separate work package.
#[doc(hidden)]
pub fn begin_removal(erase_identity: bool) -> NativeResult<RemovalContinuation> {
    #[cfg(all(windows, not(test)))]
    {
        removal_native::begin(erase_identity).map(|state| RemovalContinuation { state })
    }
    #[cfg(any(not(windows), test))]
    {
        let _ = erase_identity;
        Err(NativeError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_start_has_no_current_byte_or_receipt_approval_fallback() {
        let _fixture = TrustedImages::fixture();
        assert_eq!(start_supported(), Err(NativeError::Unsupported));
    }
}

/// Private genuine logon admission; records and route observations never construct these seals.
#[cfg(all(windows, not(test)))]
mod logon {
    use super::super::native_io::{
        Deadline, LogonReservation, NativeError, NativeResult, PriorLogonDisposition, SupportProof,
        WindowsNativeIo,
        activation::{self, EntrySelection, TaskActivationRecord},
        identity::TokenFacts,
        process::own::OwnProcessIdentity,
    };
    use super::{NativeImages, TrustedImages};
    use std::sync::Arc;

    pub(crate) struct SupervisorLogonCandidate {
        images: Arc<NativeImages>,
        owner: OwnProcessIdentity,
        context: TokenFacts,
        user: String,
        epoch: [u8; 16],
    }
    impl std::fmt::Debug for SupervisorLogonCandidate {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("SupervisorLogonCandidate")
        }
    }
    fn checked_images(
        trusted: &TrustedImages,
        deadline: &Deadline,
    ) -> NativeResult<Arc<NativeImages>> {
        let images = trusted._native.clone();
        images.reverify(deadline)?;
        if images.module.identity() != images.installer.identity()
            || images.module.canonical_dos_path() != images.installer.canonical_dos_path()
        {
            return Err(NativeError::Foreign);
        }
        Ok(images)
    }
    fn selected(images: &Arc<NativeImages>, deadline: &Deadline) -> NativeResult<EntrySelection> {
        images.reverify(deadline)?;
        let proof = images.io.admit_support(deadline)?;
        let record = TaskActivationRecord::read(&images.io, &proof, deadline)?;
        if let Some(record) = &record {
            record.bind(
                record.operation(),
                &images.io.target().identity().user.sddl(),
                images.installer.identity(),
            )?;
        }
        activation::select_entry(record.as_ref())
    }
    pub(super) fn entry_selection(
        trusted: &TrustedImages,
        deadline: &Deadline,
    ) -> NativeResult<EntrySelection> {
        selected(&checked_images(trusted, deadline)?, deadline)
    }
    pub(super) fn prepare_logon(
        trusted: &TrustedImages,
        deadline: &Deadline,
    ) -> NativeResult<SupervisorLogonCandidate> {
        let images = checked_images(trusted, deadline)?;
        if selected(&images, deadline)? != EntrySelection::Logon {
            return Err(NativeError::Foreign);
        }
        let proof = images.io.admit_support(deadline)?;
        let owner = images.io.own_process_identity(&proof, deadline)?;
        let context = images.io.target().identity().clone();
        let user = context.user.sddl();
        let mut epoch = [0; 16];
        aws_lc_rs::rand::fill(&mut epoch).map_err(|_| NativeError::Unavailable)?;
        if epoch == [0; 16] {
            return Err(NativeError::Unavailable);
        }
        // One native ID source for candidate, reservation, permit, Preparing record and Journal.
        let candidate = SupervisorLogonCandidate {
            images,
            owner,
            context,
            user,
            epoch,
        };
        let proof = candidate.images.io.admit_support(deadline)?;
        candidate.reverify(&candidate.images.io, &proof, deadline)?;
        Ok(candidate)
    }
    impl SupervisorLogonCandidate {
        pub(crate) fn io(&self) -> &Arc<WindowsNativeIo> {
            &self.images.io
        }
        pub(crate) fn operation(&self) -> [u8; 16] {
            self.epoch
        }
        pub(crate) fn registration(&self) -> [u8; 16] {
            self.epoch
        }
        pub(crate) fn user(&self) -> &str {
            &self.user
        }
        pub(crate) fn context(&self) -> &TokenFacts {
            &self.context
        }
        pub(crate) fn owner_identity(&self) -> &OwnProcessIdentity {
            &self.owner
        }
        pub(crate) fn reverify(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if !std::ptr::eq(io, self.images.io.as_ref()) {
                return Err(NativeError::Foreign);
            }
            proof.check(io, deadline)?;
            self.images.reverify(deadline)?;
            self.owner.reverify(deadline)?;
            if io.target().identity() != &self.context
                || self.user != self.context.user.sddl()
                || self.images.module.identity() != self.images.installer.identity()
                || self.images.module.canonical_dos_path()
                    != self.images.installer.canonical_dos_path()
                || selected(&self.images, deadline)? != EntrySelection::Logon
            {
                return Err(NativeError::Foreign);
            }
            deadline.check()
        }
    }
    pub(crate) struct SupervisorLogonPermit {
        candidate: SupervisorLogonCandidate,
        reservation: LogonReservation,
        disposition: PriorLogonDisposition,
    }
    impl std::fmt::Debug for SupervisorLogonPermit {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("SupervisorLogonPermit")
        }
    }
    pub(in super::super) fn admit_logon(
        candidate: SupervisorLogonCandidate,
        reservation: LogonReservation,
        disposition: PriorLogonDisposition,
        deadline: &Deadline,
    ) -> NativeResult<SupervisorLogonPermit> {
        let proof = candidate.io().admit_support(deadline)?;
        candidate.reverify(candidate.io(), &proof, deadline)?;
        reservation.reverify(candidate.io(), &proof, deadline)?;
        if reservation.context() != candidate.context()
            || reservation.owner_pid() != candidate.owner_identity().pid()
            || reservation.owner_creation() != candidate.owner_identity().creation()
        {
            return Err(NativeError::Foreign);
        }
        // Correlation binds exactly once after genuine owner/context matching; reservation makes no IDs.
        reservation.bind_epoch(
            candidate.io(),
            &proof,
            candidate.registration(),
            candidate.operation(),
            deadline,
        )?;
        disposition.reverify(candidate.io(), &proof, deadline)?;
        let permit = SupervisorLogonPermit {
            candidate,
            reservation,
            disposition,
        };
        let proof = permit.io().admit_support(deadline)?;
        permit.reverify(permit.io(), &proof, deadline)?;
        Ok(permit)
    }
    impl SupervisorLogonPermit {
        pub(crate) fn io(&self) -> &Arc<WindowsNativeIo> {
            self.candidate.io()
        }
        pub(crate) fn operation(&self) -> [u8; 16] {
            self.candidate.operation()
        }
        pub(crate) fn registration(&self) -> [u8; 16] {
            self.candidate.registration()
        }
        pub(crate) fn user(&self) -> &str {
            self.candidate.user()
        }
        pub(crate) fn context(&self) -> &TokenFacts {
            self.candidate.context()
        }
        pub(crate) fn owner_identity(&self) -> &OwnProcessIdentity {
            self.candidate.owner_identity()
        }
        pub(crate) fn reservation(&self) -> &LogonReservation {
            &self.reservation
        }
        pub(crate) fn disposition(&self) -> &PriorLogonDisposition {
            &self.disposition
        }
        pub(crate) fn reverify(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.candidate.reverify(io, proof, deadline)?;
            self.reservation.reverify(io, proof, deadline)?;
            if self.reservation.context() != self.context()
                || self.reservation.owner_pid() != self.owner_identity().pid()
                || self.reservation.owner_creation() != self.owner_identity().creation()
                || self.reservation.registration()? != self.registration()
                || self.reservation.operation()? != self.operation()
            {
                return Err(NativeError::Foreign);
            }
            self.disposition.reverify(io, proof, deadline)?;
            deadline.check()
        }
    }
}
#[cfg(all(windows, not(test)))]
pub(super) use logon::admit_logon;
#[cfg(all(windows, not(test)))]
pub(crate) use logon::{SupervisorLogonCandidate, SupervisorLogonPermit};
#[cfg(all(windows, not(test)))]
use logon::{entry_selection, prepare_logon};

// Native-only because legacy source-included fake roots intentionally contain no removal graph.
#[cfg(all(windows, not(test)))]
pub(crate) mod removal_runtime {
    use super::super::{
        native_io::{
            Deadline, InstallerLock, NativeError, NativeResult, SupportProof, WindowsNativeIo,
            supervisor_owner::RetainedTreeCompletion,
        },
        removal::{RemovalRecord, StoppedTreeFacts},
    };
    use super::supervisor::Generation;
    use std::sync::Arc;

    /// The actual tree remains retained after every image, broker, observer and install-root
    /// alias has physically settled and been dropped. No persisted fact constructs this type.
    pub(crate) struct RemovalCompletion {
        io: Arc<WindowsNativeIo>,
        operation: [u8; 16],
        generation: Generation,
        started_unix_ms: u64,
        tree: Arc<RetainedTreeCompletion>,
    }
    impl RemovalCompletion {
        fn from_settled(
            io: Arc<WindowsNativeIo>,
            generation: Generation,
            started_unix_ms: u64,
            tree: Arc<RetainedTreeCompletion>,
            deadline: &Deadline,
        ) -> NativeResult<Self> {
            tree.reverify(deadline)?;
            if generation.pid == 0 || generation.instance != tree.original_instance() {
                return Err(NativeError::Foreign);
            }
            // Preserve the exact lifecycle start from the originally admitted bootstrap.
            // The native creation stamp still belongs to genuine Generation, not receipt authority.
            StoppedTreeFacts {
                generation,
                started_unix_ms,
            }
            .validate()?;
            Ok(Self {
                io,
                operation: tree.operation(),
                generation,
                started_unix_ms,
                tree,
            })
        }
        pub(crate) fn tree(&self) -> &Arc<RetainedTreeCompletion> {
            &self.tree
        }
        pub(crate) fn generation(&self) -> Generation {
            self.generation
        }
        pub(crate) fn started_unix_ms(&self) -> u64 {
            self.started_unix_ms
        }
        pub(crate) fn reverify(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            lock: &InstallerLock,
            operation: [u8; 16],
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if !std::ptr::eq(io, self.io.as_ref()) || operation != self.operation {
                return Err(NativeError::Foreign);
            }
            proof.check(io, deadline)?;
            let permit = self.io.admit_removal_permit(proof, lock, deadline)?;
            permit.reverify(io, proof, lock, deadline)?;
            let record = RemovalRecord::decode(permit.bytes())?;
            if record.operation() != operation {
                return Err(NativeError::Foreign);
            }
            record.context().matches(io.target().identity())?;
            if let Some(stopped) = record.stopped()
                && (stopped.generation != self.generation
                    || stopped.started_unix_ms != self.started_unix_ms)
            {
                return Err(NativeError::Foreign);
            }
            self.tree.reverify(deadline)
        }
    }

    pub(super) struct StopRuntime {
        io: Arc<WindowsNativeIo>,
        operation: [u8; 16],
        sequence: super::super::native_io::supervisor_owner::StopSequence,
        attempted: bool,
        owner: Option<super::super::native_io::supervisor_owner::AdmittedRemovalOwner>,
        settlement: Option<super::super::transport::StopSettlement>,
        lease: Option<Arc<super::super::native_io::StopLockLease>>,
        journal: Option<super::journal::Journal>,
        generation: Option<Generation>,
        started_unix_ms: Option<u64>,
        completed: Option<Arc<RetainedTreeCompletion>>,
    }
    impl StopRuntime {
        pub(super) fn new(io: Arc<WindowsNativeIo>, operation: [u8; 16]) -> Self {
            Self {
                io,
                operation,
                sequence: Default::default(),
                attempted: false,
                owner: None,
                settlement: None,
                lease: None,
                journal: None,
                generation: None,
                started_unix_ms: None,
                completed: None,
            }
        }
        fn selected(
            &self,
            proof: &SupportProof,
            lock: &InstallerLock,
            deadline: &Deadline,
        ) -> NativeResult<RemovalRecord> {
            let permit = self.io.admit_removal_permit(proof, lock, deadline)?;
            permit.reverify(&self.io, proof, lock, deadline)?;
            let record = RemovalRecord::decode(permit.bytes())?;
            if record.operation() != self.operation
                || record.cursor() != super::super::removal::RemovalCursor::StopIntent
            {
                return Err(NativeError::Foreign);
            }
            record.context().matches(self.io.target().identity())?;
            Ok(record)
        }
        pub(super) fn stop_once(
            &mut self,
            lock: &InstallerLock,
            deadline: &Deadline,
        ) -> NativeResult<RemovalCompletion> {
            use super::super::{
                native_io::supervisor_owner::{AdmittedRemovalOwner, TerminalStopPort},
                transport::WindowsAgentPort,
            };
            if self.attempted {
                return Err(NativeError::OutcomeUnknown);
            }
            let proof = self.io.admit_support(deadline)?;
            self.selected(&proof, lock, deadline)?;
            // Reserve before owner/RPC dispatch; any late/error result uses observations only.
            self.attempted = true;
            let original = self.io.observe_agent(&proof, deadline)?;
            let generation = self.io.agent_generation(&original, &proof, deadline)?;
            let journal = super::journal::Journal::read(&self.io, &proof, deadline)?
                .ok_or(NativeError::Missing)?;
            if journal.current != Some(generation) {
                return Err(NativeError::Foreign);
            }
            let original_clone = self.io.clone_agent(&original, &proof, deadline)?;
            let lease = self.io.lease_stop_lock(&proof, lock, deadline)?;
            let admission =
                self.io
                    .prepare_removal_completion(&proof, &lease, self.operation, deadline)?;
            self.owner = Some(AdmittedRemovalOwner::admit(
                self.io.clone(),
                original_clone,
                lease.clone(),
                admission,
                deadline,
            )?);
            self.lease = Some(lease);
            self.generation = Some(generation);
            self.started_unix_ms = Some(original.bootstrap().started_unix_ms);
            if self.started_unix_ms == Some(0) {
                return Err(NativeError::Foreign);
            }
            self.journal = Some(journal);
            let agent = WindowsAgentPort::new(
                self.io.clone(),
                self.io.admit_support(deadline)?,
                self.io.bound_clock(),
                deadline,
            )?;
            self.settlement = Some(agent.stop_settlement());
            let io = self.io.clone();
            let lease = self.lease.as_ref().cloned().ok_or(NativeError::Foreign)?;
            let timeout = deadline
                .remaining_ms()?
                .min(crate::agent_contract::MAX_TIMEOUT_MS);
            let native = super::supervisor::NativeStop {
                io: &io,
                proof: &proof,
                lock: lease.lock(),
                journal: self.journal.as_mut().ok_or(NativeError::Foreign)?,
                port: Some(agent),
                deadline,
                timeout_ms: timeout,
            };
            struct Ordered<'a> {
                native: super::supervisor::NativeStop<'a>,
                owner: &'a AdmittedRemovalOwner,
                operation: [u8; 16],
                generation: Generation,
                deadline: &'a Deadline,
            }
            impl TerminalStopPort for Ordered<'_> {
                fn persist(&mut self) -> NativeResult<()> {
                    super::supervisor::StopPort::persist_stop(
                        &mut self.native,
                        self.generation.instance,
                    )
                }
                fn arm(&mut self) -> NativeResult<()> {
                    self.owner.arm_terminal(self.operation, self.deadline)
                }
                fn submit(&mut self) -> NativeResult<()> {
                    super::supervisor::StopPort::submit_stop(
                        &mut self.native,
                        self.generation.instance,
                    )
                }
            }
            let mut ordered = Ordered {
                native,
                owner: self.owner.as_ref().ok_or(NativeError::Foreign)?,
                operation: self.operation,
                generation,
                deadline,
            };
            let submitted = self.sequence.run_once(&mut ordered);
            drop(ordered);
            drop(original);
            if !self.sequence.submitted() {
                return submitted.and(Err(NativeError::OutcomeUnknown));
            }
            // One completion command only. Result is retained by the actual removal sidecar.
            let _ = self
                .owner
                .as_ref()
                .ok_or(NativeError::Foreign)?
                .observe_completion(self.operation, deadline);
            self.finish(lock, deadline)?
                .ok_or(NativeError::OutcomeUnknown)
        }
        pub(super) fn finish(
            &mut self,
            lock: &InstallerLock,
            deadline: &Deadline,
        ) -> NativeResult<Option<RemovalCompletion>> {
            if !self.attempted || !self.sequence.submitted() {
                return Ok(None);
            }
            let proof = self.io.admit_support(deadline)?;
            self.selected(&proof, lock, deadline)?;
            // This physical transport barrier is BEFORE consuming the owner sidecar/broker.
            self.settlement
                .as_ref()
                .ok_or(NativeError::OutcomeUnknown)?
                .wait(deadline)?;
            if self.completed.is_none() {
                let Some(owner) = self.owner.as_mut() else {
                    return Ok(None);
                };
                let Some(tree) = owner.finish_settled(deadline)? else {
                    return Ok(None);
                };
                // Preserve the unchanged actual proof before a later Finished publication can fail.
                self.completed = Some(tree);
            }
            let tree = self
                .completed
                .as_ref()
                .cloned()
                .ok_or(NativeError::OutcomeUnknown)?;
            tree.reverify(deadline)?;
            let generation = self.generation.ok_or(NativeError::OutcomeUnknown)?;
            let mut finished = self
                .journal
                .as_ref()
                .ok_or(NativeError::OutcomeUnknown)?
                .clone();
            finished.phase = super::journal::Phase::Finished;
            finished.stop_instance = Some(generation.instance);
            let observed = super::journal::Journal::read(
                &self.io,
                &self.io.admit_support(deadline)?,
                deadline,
            )?
            .ok_or(NativeError::OutcomeUnknown)?;
            if observed.encode()? != finished.encode()? {
                finished.publish(&self.io, &self.io.admit_support(deadline)?, lock, deadline)?;
            }
            // Source worker, connected pipes, old image pins and both global/client alias slots
            // have settled before this bare tree can reach any task/file deletion callback.
            // Finish every fallible admission while the resident slot still retains the actual
            // proof. A deadline/error here cannot discard the sole completed recovery capability.
            let completion = RemovalCompletion::from_settled(
                self.io.clone(),
                generation,
                self.started_unix_ms.ok_or(NativeError::OutcomeUnknown)?,
                tree,
                deadline,
            )?;
            self.owner.take();
            self.settlement.take();
            self.lease.take();
            self.journal.take();
            self.completed.take();
            Ok(Some(completion))
        }
    }
}

// APPEND-ONLY TEMPORARY OWNERSHIP BOUNDARY: windows_agent_audit owns new removal_port module.

#[cfg(all(windows, not(test)))]
pub(super) mod removal_port {
    use super::super::{
        native_io::{
            self, Deadline, InstallerLock, NativeError, NativeResult, OpenedPe, PayloadRoot,
            RemovalMutationPermit, RemovalRoot, StopLockLease, SupportProof, WindowsNativeIo,
        },
        payload::{
            helper::removal::{RemovalCommit, RemovalCopyExit, RemovalKeeperLease},
            inventory::{ApprovedInventory, PayloadRole},
        },
        removal::{
            RemovalCursor as Cursor, RemovalRecord, RemovalResult, StoppedTreeFacts,
            erase_receipt_fresh,
            executor::{NodePresence, RemovalController, RemovalPort},
        },
    };
    use super::removal_runtime::{RemovalCompletion, StopRuntime};
    use crate::agent_contract::{EraseIdentityV1, LastExitV1};
    use std::{
        io::Read,
        os::windows::{
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
            process::CommandExt,
        },
        process::{Child, Command, Stdio},
        sync::{Arc, Mutex, OnceLock},
        thread::JoinHandle,
        time::Duration,
    };
    use windows_sys::Win32::{
        Foundation::{DuplicateHandle, WAIT_OBJECT_0},
        System::Threading::*,
    };

    struct ErasePins {
        agent: Arc<OpenedPe>,
        _payload: Arc<PayloadRoot>,
        lock: Arc<StopLockLease>,
    }
    #[derive(Default)]
    struct EraseState {
        child: Option<Arc<Mutex<Child>>>,
        process: Option<Arc<OwnedHandle>>,
        result: Option<NativeResult<EraseIdentityV1>>,
        worker_settled: bool,
        settled: bool,
    }
    struct EraseOwner {
        io: Arc<WindowsNativeIo>,
        operation: [u8; 16],
        pins: Mutex<Option<ErasePins>>,
        state: Mutex<EraseState>,
        worker: Mutex<Option<JoinHandle<()>>>,
    }
    static ERASE: OnceLock<Arc<EraseOwner>> = OnceLock::new();
    fn exited(handle: &OwnedHandle) -> bool {
        // SAFETY: nonblocking observation of an actual retained process or worker-thread handle.
        (unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) }) == WAIT_OBJECT_0
    }
    impl EraseOwner {
        fn start(
            io: Arc<WindowsNativeIo>,
            root: Arc<RemovalRoot>,
            lock: &InstallerLock,
            permit: &RemovalMutationPermit,
            record: &RemovalRecord,
            tree: Arc<RemovalCompletion>,
            deadline: &Deadline,
        ) -> NativeResult<Arc<Self>> {
            if record.cursor() != Cursor::EraseIntent || !record.options().erase_identity {
                return Err(NativeError::Foreign);
            }
            let proof = io.admit_support(deadline)?;
            tree.reverify(&io, &proof, lock, record.operation(), deadline)?;
            let receipt = io
                .read_removal_receipt(&proof, lock, permit, &tree, deadline)?
                .ok_or(NativeError::Missing)?;
            erase_receipt_fresh(
                StoppedTreeFacts {
                    generation: tree.generation(),
                    started_unix_ms: tree.started_unix_ms(),
                },
                &receipt,
            )?;
            let index = record
                .plan()
                .order()
                .iter()
                .copied()
                .find(|index| {
                    record
                        .plan()
                        .node(*index)
                        .is_ok_and(|n| n.components() == [PayloadRole::Agent.leaf().to_owned()])
                })
                .ok_or(NativeError::Missing)?;
            if root.observe_node(&io, &proof, lock, permit, index, deadline)? != NodePresence::Same
            {
                return Err(NativeError::Foreign);
            }
            let inventory = ApprovedInventory::embedded()?;
            let payload = Arc::new(io.payload_root(&proof, lock, deadline)?);
            let agent = Arc::new(payload.open_approved(
                &io,
                &proof,
                PayloadRole::Agent,
                inventory.role(PayloadRole::Agent)?,
                deadline,
            )?);
            let expected = record.plan().node(index)?.identity();
            if agent.identity().volume != expected.volume
                || agent.identity().file != expected.file
                || agent.canonical_dos_path().is_empty()
            {
                return Err(NativeError::Foreign);
            }
            let lease = io.lease_stop_lock(&proof, lock, deadline)?;
            let owner = Arc::new(Self {
                io: io.clone(),
                operation: record.operation(),
                pins: Mutex::new(Some(ErasePins {
                    agent,
                    _payload: payload,
                    lock: lease,
                })),
                state: Mutex::new(EraseState::default()),
                worker: Mutex::new(None),
            });
            // Reserve globally BEFORE any native creation; neither timeout nor a new resident
            // can create another erase process. The actual pins already belong to this owner.
            ERASE
                .set(owner.clone())
                .map_err(|_| NativeError::OutcomeUnknown)?;
            let held = owner.clone();
            let budget = deadline.clone();
            let expected_bytes = permit.bytes().to_vec();
            let worker = std::thread::Builder::new()
                .name("crosspane-removal-erase".into())
                .spawn(move || {
                    let result = (|| {
                        let (agent, lease) = {
                            let pins = held.pins.lock().map_err(|_| NativeError::OutcomeUnknown)?;
                            let pins = pins.as_ref().ok_or(NativeError::OutcomeUnknown)?;
                            (pins.agent.clone(), pins.lock.clone())
                        };
                        let proof = held.io.admit_support(&budget)?;
                        let current =
                            held.io
                                .admit_removal_permit(&proof, lease.lock(), &budget)?;
                        if current.bytes() != expected_bytes {
                            return Err(NativeError::Foreign);
                        }
                        tree.reverify(&held.io, &proof, lease.lock(), held.operation, &budget)?;
                        let receipt = held
                            .io
                            .read_removal_receipt(&proof, lease.lock(), &current, &tree, &budget)?
                            .ok_or(NativeError::Missing)?;
                        erase_receipt_fresh(
                            StoppedTreeFacts {
                                generation: tree.generation(),
                                started_unix_ms: tree.started_unix_ms(),
                            },
                            &receipt,
                        )?;
                        if root.observe_node(
                            &held.io,
                            &proof,
                            lease.lock(),
                            &current,
                            index,
                            &budget,
                        )? != NodePresence::Same
                        {
                            return Err(NativeError::Foreign);
                        }
                        agent.reverify(&held.io, &held.io.admit_support(&budget)?, &budget)?;
                        budget.check()?;
                        // Absolute approved same-FileId image only; one literal argument, no shell,
                        // PATH lookup, borrowed stdio, input, credentials probe or flag substitute.
                        let child = Command::new(agent.canonical_dos_path())
                            .arg("erase-identity")
                            .stdin(Stdio::null())
                            .stdout(Stdio::piped())
                            .stderr(Stdio::null())
                            .creation_flags(CREATE_NO_WINDOW)
                            .spawn()
                            .map_err(|_| NativeError::OutcomeUnknown)?;
                        let child = Arc::new(Mutex::new(child));
                        // Store the real Child before any output/process observation or result delivery.
                        held.state
                            .lock()
                            .map_err(|_| NativeError::OutcomeUnknown)?
                            .child = Some(child.clone());
                        let (process, mut output) = {
                            let mut actual =
                                child.lock().map_err(|_| NativeError::OutcomeUnknown)?;
                            let mut raw = std::ptr::null_mut();
                            // SAFETY: duplicate only this actually spawned Child object, query/sync,
                            // noninheritable. No serialized PID, name, path or reconstructed handle.
                            if unsafe {
                                DuplicateHandle(
                                    GetCurrentProcess(),
                                    actual.as_raw_handle(),
                                    GetCurrentProcess(),
                                    &mut raw,
                                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                                    0,
                                    0,
                                )
                            } == 0
                            {
                                return Err(NativeError::OutcomeUnknown);
                            }
                            // SAFETY: successful duplication transferred the actual local process handle.
                            let process = Arc::new(unsafe { OwnedHandle::from_raw_handle(raw) });
                            let output = actual.stdout.take().ok_or(NativeError::OutcomeUnknown)?;
                            (process, output)
                        };
                        held.state
                            .lock()
                            .map_err(|_| NativeError::OutcomeUnknown)?
                            .process = Some(process.clone());
                        let mut bytes = Vec::new();
                        output
                            .by_ref()
                            .take((crate::agent_contract::MAX_RESPONSE_BYTES + 1) as u64)
                            .read_to_end(&mut bytes)
                            .map_err(|_| NativeError::OutcomeUnknown)?;
                        if bytes.len() > crate::agent_contract::MAX_RESPONSE_BYTES {
                            return Err(NativeError::OutcomeUnknown);
                        }
                        drop(output);
                        let status = child
                            .lock()
                            .map_err(|_| NativeError::OutcomeUnknown)?
                            .wait()
                            .map_err(|_| NativeError::OutcomeUnknown)?;
                        if !status.success() || !exited(&process) {
                            return Err(NativeError::OutcomeUnknown);
                        }
                        crate::agent_contract::parse_erase_identity(&bytes)
                            .map_err(|_| NativeError::OutcomeUnknown)
                    })();
                    // Retain the same actual process, worker/output and pins on any ambiguous result.
                    // A late receiver observes this slot; no sole capability is sent to a lost channel.
                    if let Ok(mut state) = held.state.lock() {
                        state.result = Some(result);
                    }
                })
                .map_err(|_| NativeError::OutcomeUnknown)?;
            *owner
                .worker
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)? = Some(worker);
            Ok(owner)
        }
        fn observe(
            &self,
            io: &WindowsNativeIo,
            operation: [u8; 16],
            deadline: &Deadline,
        ) -> NativeResult<Option<EraseIdentityV1>> {
            if !std::ptr::eq(io, self.io.as_ref()) || operation != self.operation {
                return Err(NativeError::Foreign);
            }
            deadline.check()?;
            let mut worker = self
                .worker
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?;
            let mut state = self.state.lock().map_err(|_| NativeError::OutcomeUnknown)?;
            if !state.worker_settled {
                let Some(thread) = worker.as_ref() else {
                    return Ok(None);
                };
                // SAFETY: actual retained erase worker thread, native exit includes output/TLS drops.
                if (unsafe { WaitForSingleObject(thread.as_raw_handle(), 0) }) != WAIT_OBJECT_0 {
                    return Ok(None);
                }
                let joined = worker.take().ok_or(NativeError::OutcomeUnknown)?.join();
                // Positive OS-thread exit is remembered independently of the process. A later
                // poll still observes that SAME actual process after an oversize/error result.
                state.worker_settled = true;
                if joined.is_err() {
                    state.result = Some(Err(NativeError::OutcomeUnknown));
                }
            }
            if !state.settled {
                let actual_exit = if let Some(process) = state.process.as_ref() {
                    exited(process)
                } else if let Some(child) = state.child.as_ref() {
                    // The worker has physically exited before this lock/observation. Even
                    // a failed duplicate cannot cause PID reopening or lost Child authority.
                    let child = child.lock().map_err(|_| NativeError::OutcomeUnknown)?;
                    // SAFETY: nonblocking wait on the actual retained Child object itself.
                    (unsafe { WaitForSingleObject(child.as_raw_handle(), 0) }) == WAIT_OBJECT_0
                } else {
                    false
                };
                if !actual_exit {
                    return Ok(None);
                }
                // This ledger is set only after actual process AND worker/output exit. Retain only
                // result metadata; all image/root/lock and Child aliases are now physically retired.
                state.child.take();
                state.process.take();
                self.pins
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?
                    .take();
                state.settled = true;
            }
            deadline.check()?;
            match state.result.as_ref() {
                Some(Ok(value)) => Ok(Some(value.clone())),
                Some(Err(error)) => Err(*error),
                None => Err(NativeError::OutcomeUnknown),
            }
        }
        fn settled(&self) -> NativeResult<bool> {
            Ok(self
                .state
                .lock()
                .map_err(|_| NativeError::OutcomeUnknown)?
                .settled)
        }
    }

    pub(super) struct Resident {
        io: Arc<WindowsNativeIo>,
        root: Arc<RemovalRoot>,
        lock: InstallerLock,
        record: RemovalRecord,
        permit: RemovalMutationPermit,
        commit: Option<Arc<RemovalCommit>>,
        namespace: Arc<RemovalKeeperLease>,
        controller: RemovalController<Arc<RemovalCompletion>>,
        stop: StopRuntime,
        erase: Option<Arc<EraseOwner>>,
        exits: Vec<Option<RemovalCopyExit>>,
        deadline: Deadline,
    }
    impl Resident {
        pub(super) fn new(
            io: Arc<WindowsNativeIo>,
            root: RemovalRoot,
            lock: InstallerLock,
            record: RemovalRecord,
            commit: Arc<RemovalCommit>,
        ) -> NativeResult<Self> {
            let deadline =
                Deadline::new(5000, io.bound_clock(), native_io::Cancellation::default())?;
            let proof = io.admit_support(&deadline)?;
            commit.reverify(&io, &proof, record.operation(), &deadline)?;
            let namespace = commit.lease().clone();
            Self::construct(io, root, lock, record, Some(commit), namespace, deadline)
        }
        pub(super) fn reopen_terminal(
            io: Arc<WindowsNativeIo>,
            root: RemovalRoot,
            lock: InstallerLock,
            record: RemovalRecord,
            namespace: Arc<RemovalKeeperLease>,
        ) -> NativeResult<Self> {
            if !matches!(
                record.cursor(),
                Cursor::Complete { .. }
                    | Cursor::FinalCopyCleanupIntent { .. }
                    | Cursor::FinalCopyAbsent { .. }
                    | Cursor::Retired
            ) {
                return Err(NativeError::Foreign);
            }
            let deadline =
                Deadline::new(5000, io.bound_clock(), native_io::Cancellation::default())?;
            namespace.reverify(
                &io,
                &io.admit_support(&deadline)?,
                record.operation(),
                &deadline,
            )?;
            Self::construct(io, root, lock, record, None, namespace, deadline)
        }
        fn construct(
            io: Arc<WindowsNativeIo>,
            root: RemovalRoot,
            lock: InstallerLock,
            record: RemovalRecord,
            commit: Option<Arc<RemovalCommit>>,
            namespace: Arc<RemovalKeeperLease>,
            deadline: Deadline,
        ) -> NativeResult<Self> {
            let proof = io.admit_support(&deadline)?;
            let permit = io.admit_removal_permit(&proof, &lock, &deadline)?;
            if permit.bytes() != record.encode()? {
                return Err(NativeError::Foreign);
            }
            root.reverify(&io, &proof, &lock, &permit, &deadline)?;
            let exits = record.plan().copies().iter().map(|_| None).collect();
            let operation = record.operation();
            Ok(Self {
                io: io.clone(),
                root: Arc::new(root),
                lock,
                record,
                permit,
                commit,
                namespace,
                controller: Default::default(),
                stop: StopRuntime::new(io, operation),
                erase: None,
                exits,
                deadline,
            })
        }
        pub(super) fn run(&mut self, deadline: &Deadline) -> NativeResult<RemovalResult> {
            self.deadline = deadline.clone();
            let proof = self.io.admit_support(deadline)?;
            let permit = self.io.admit_removal_permit(&proof, &self.lock, deadline)?;
            let fresh = RemovalRecord::decode(permit.bytes())?;
            self.record.publication_successor(&fresh)?;
            self.record = fresh;
            self.permit = permit;
            let mut controller = std::mem::take(&mut self.controller);
            let mut record = self.record.clone();
            let result = controller.run(self, &mut record);
            self.record = record;
            self.controller = controller;
            result
        }
        fn proof(&self) -> NativeResult<SupportProof> {
            self.io.admit_support(&self.deadline)
        }
        fn terminal(&self) -> bool {
            matches!(
                self.record.cursor(),
                Cursor::Complete { .. }
                    | Cursor::FinalCopyCleanupIntent { .. }
                    | Cursor::FinalCopyAbsent { .. }
                    | Cursor::Retired
            )
        }
    }
    impl RemovalPort for Resident {
        type Tree = Arc<RemovalCompletion>;
        fn renew(&mut self, record: &RemovalRecord) -> NativeResult<()> {
            if record.encode()? != self.permit.bytes() {
                return Err(NativeError::Foreign);
            }
            self.root.reverify(
                &self.io,
                &self.proof()?,
                &self.lock,
                &self.permit,
                &self.deadline,
            )
        }
        fn persist(&mut self, record: &RemovalRecord) -> NativeResult<()> {
            let permit =
                self.io
                    .publish_removal(&self.proof()?, &self.lock, record, &self.deadline)?;
            self.record = record.clone();
            self.permit = permit;
            Ok(())
        }
        fn commit_authorized(&mut self, record: &RemovalRecord) -> NativeResult<()> {
            let commit = self.commit.as_ref().ok_or(NativeError::Foreign)?;
            commit.reverify(&self.io, &self.proof()?, record.operation(), &self.deadline)?;
            commit.wait_parent_if_installed(record, &self.deadline)
        }
        fn stop_once(&mut self, _: &RemovalRecord) -> NativeResult<Self::Tree> {
            self.stop
                .stop_once(&self.lock, &self.deadline)
                .map(Arc::new)
        }
        fn recover_stop(&mut self, _: &RemovalRecord) -> NativeResult<Option<Self::Tree>> {
            self.stop
                .finish(&self.lock, &self.deadline)
                .map(|tree| tree.map(Arc::new))
        }
        fn stopped_facts(&self, tree: &Self::Tree) -> NativeResult<StoppedTreeFacts> {
            Ok(StoppedTreeFacts {
                generation: tree.generation(),
                started_unix_ms: tree.started_unix_ms(),
            })
        }
        fn renew_completion(
            &mut self,
            record: &RemovalRecord,
            tree: &Self::Tree,
        ) -> NativeResult<()> {
            tree.reverify(
                &self.io,
                &self.proof()?,
                &self.lock,
                record.operation(),
                &self.deadline,
            )
        }
        fn receipt(
            &mut self,
            _: &RemovalRecord,
            tree: &Self::Tree,
        ) -> NativeResult<Option<LastExitV1>> {
            self.io.read_removal_receipt(
                &self.proof()?,
                &self.lock,
                &self.permit,
                tree,
                &self.deadline,
            )
        }
        fn erase_once(
            &mut self,
            record: &RemovalRecord,
            tree: &Self::Tree,
        ) -> NativeResult<EraseIdentityV1> {
            if self.erase.is_some() {
                return Err(NativeError::OutcomeUnknown);
            }
            self.erase = Some(EraseOwner::start(
                self.io.clone(),
                self.root.clone(),
                &self.lock,
                &self.permit,
                record,
                tree.clone(),
                &self.deadline,
            )?);
            loop {
                if let Some(value) = self
                    .erase
                    .as_ref()
                    .ok_or(NativeError::OutcomeUnknown)?
                    .observe(&self.io, record.operation(), &self.deadline)?
                {
                    return Ok(value);
                }
                self.deadline.check()?;
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        fn recover_erase(
            &mut self,
            record: &RemovalRecord,
            _: &Self::Tree,
        ) -> NativeResult<Option<EraseIdentityV1>> {
            if self.erase.is_none()
                && let Some(owner) = ERASE.get()
            {
                // This is the actual process-global reservation, never a record/fact constructor.
                // Check original IO identity before the matching operation correlation.
                if !std::ptr::eq(self.io.as_ref(), owner.io.as_ref())
                    || owner.operation != record.operation()
                {
                    return Err(NativeError::Foreign);
                }
                self.erase = Some(owner.clone());
            }
            match &self.erase {
                Some(owner) => owner.observe(&self.io, record.operation(), &self.deadline),
                None => Ok(None),
            }
        }
        fn task_absent(&mut self, _: &RemovalRecord) -> NativeResult<bool> {
            self.root.task_absent(
                &self.io,
                &self.proof()?,
                &self.lock,
                &self.permit,
                &self.deadline,
            )
        }
        fn task_delete(&mut self, _: &RemovalRecord, tree: &Self::Tree) -> NativeResult<()> {
            self.root.delete_task(
                &self.io,
                &self.proof()?,
                &self.lock,
                &self.permit,
                tree,
                &self.deadline,
            )
        }
        fn observe_node(&mut self, _: &RemovalRecord, index: u16) -> NativeResult<NodePresence> {
            self.root.observe_node(
                &self.io,
                &self.proof()?,
                &self.lock,
                &self.permit,
                index,
                &self.deadline,
            )
        }
        fn delete_node(
            &mut self,
            _: &RemovalRecord,
            index: u16,
            tree: &Self::Tree,
        ) -> NativeResult<()> {
            self.settle_install_aliases(&self.record.clone(), tree)?;
            self.root.delete_node(
                &self.io,
                &self.proof()?,
                &self.lock,
                &self.permit,
                index,
                tree,
                &self.deadline,
            )
        }
        fn root_absent(&mut self, _: &RemovalRecord) -> NativeResult<bool> {
            self.root.root_absent(
                &self.io,
                &self.proof()?,
                &self.lock,
                &self.permit,
                &self.deadline,
            )
        }
        fn settle_install_aliases(
            &mut self,
            record: &RemovalRecord,
            tree: &Self::Tree,
        ) -> NativeResult<()> {
            tree.reverify(
                &self.io,
                &self.proof()?,
                &self.lock,
                record.operation(),
                &self.deadline,
            )?;
            if let Some(owner) = &self.erase {
                let _ = owner.observe(&self.io, record.operation(), &self.deadline)?;
                if !owner.settled()? {
                    return Err(NativeError::OutcomeUnknown);
                }
            }
            Ok(())
        }
        fn delete_root(&mut self, _: &RemovalRecord, tree: &Self::Tree) -> NativeResult<()> {
            self.settle_install_aliases(&self.record.clone(), tree)?;
            self.root.delete_root(
                &self.io,
                &self.proof()?,
                &self.lock,
                &self.permit,
                tree,
                &self.deadline,
            )
        }
        fn executing_copy(&mut self, _: &RemovalRecord) -> NativeResult<Option<u8>> {
            self.io
                .executing_removal_copy(&self.proof()?, &self.permit, &self.deadline)
        }
        fn copy_absent(&mut self, _: &RemovalRecord, index: u8) -> NativeResult<bool> {
            if self.terminal() {
                return self.io.final_removal_copy_absent(
                    &self.proof()?,
                    &self.lock,
                    &self.permit,
                    index,
                    &self.namespace,
                    &self.deadline,
                );
            }
            let Some(exit) = self.exits.get(usize::from(index)).and_then(Option::as_ref) else {
                return Ok(false);
            };
            self.io.removal_copy_absent(
                &self.proof()?,
                &self.lock,
                &self.permit,
                index,
                exit,
                &self.deadline,
            )
        }
        fn retire_copy(&mut self, _: &RemovalRecord, index: u8) -> NativeResult<()> {
            let exit = self
                .exits
                .get(usize::from(index))
                .and_then(Option::as_ref)
                .ok_or(NativeError::OutcomeUnknown)?;
            self.io.retire_removal_copy(
                &self.proof()?,
                &self.lock,
                &self.permit,
                index,
                exit,
                &self.deadline,
            )
        }
        fn cleanup_final_copy(&mut self, _: &RemovalRecord, index: u8) -> NativeResult<()> {
            self.io.cleanup_final_removal_copy(
                &self.proof()?,
                &self.lock,
                &self.permit,
                index,
                &self.namespace,
                &self.deadline,
            )
        }
    }
}

#[cfg(all(windows, not(test)))]
mod removal_native {
    use super::super::{
        native_io::{
            self, Cancellation, Deadline, InstallerLock, MonotonicClock, NativeError, NativeResult,
            RemovalRoot, WindowsNativeIo,
            activation::{KeeperControl, KeeperPort, KeeperProgress},
        },
        payload::{
            self,
            helper::removal::{
                self as copy, ControlMethod, ControlState, RemovalChild, RemovalServer,
            },
        },
        removal::{
            RemovalCursor, RemovalHandoffStage, RemovalOptions, RemovalRecord, RemovalResult,
            inventory::RemovalCopyKind,
        },
    };
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };
    fn budget(io: &WindowsNativeIo, ms: u64) -> NativeResult<Deadline> {
        Deadline::new(ms, io.bound_clock(), Cancellation::default())
    }

    struct SourcePort {
        io: Arc<WindowsNativeIo>,
        root: RemovalRoot,
        lock: Option<InstallerLock>,
        record: RemovalRecord,
        initial: Deadline,
    }
    impl SourcePort {
        fn observed(
            &self,
            child: &Arc<RemovalChild>,
            deadline: &Deadline,
        ) -> NativeResult<KeeperProgress> {
            let proof = self.io.admit_support(deadline)?;
            let current = self
                .io
                .read_removal(&proof, deadline)?
                .ok_or(NativeError::Missing)?;
            self.record.same_selection(&current)?;
            if matches!(
                current.cursor(),
                RemovalCursor::Complete { .. }
                    | RemovalCursor::FinalCopyAbsent { .. }
                    | RemovalCursor::Retired
            ) && child.actual_exit(&proof, deadline)?.is_some()
            {
                let lock = self
                    .io
                    .acquire_installer_lock(&self.io.admit_support(deadline)?, deadline)?;
                let proof = self.io.admit_support(deadline)?;
                let permit = self.io.admit_removal_permit(&proof, &lock, deadline)?;
                if !self
                    .root
                    .root_absent(&self.io, &proof, &lock, &permit, deadline)?
                    || !self
                        .root
                        .task_absent(&self.io, &proof, &lock, &permit, deadline)?
                {
                    return Err(NativeError::OutcomeUnknown);
                }
                return Ok(KeeperProgress::Complete);
            }
            // Reconnect only for bounded metadata. Commit/Stop/erase is never sent by recovery.
            let _ = child.exchange(ControlMethod::Status, deadline);
            Ok(KeeperProgress::Pending)
        }
    }
    impl KeeperPort for SourcePort {
        type Child = Arc<RemovalChild>;
        fn prepare(&mut self) -> NativeResult<Self::Child> {
            let (child, record) = copy::launch(
                self.io.clone(),
                self.lock.take(),
                self.record.clone(),
                &self.initial,
            )?;
            self.record = record;
            Ok(Arc::new(child))
        }
        fn mark_ready(&mut self, child: &Self::Child) -> NativeResult<()> {
            match child.exchange(ControlMethod::Ready, &self.initial)? {
                ControlState::Ready => {}
                ControlState::ReinstallRequired => return Err(NativeError::Unsupported),
                _ => return Err(NativeError::OutcomeUnknown),
            }
            let proof = self.io.admit_support(&self.initial)?;
            let lock = self.io.acquire_installer_lock(&proof, &self.initial)?;
            let proof = self.io.admit_support(&self.initial)?;
            let permit = self.io.admit_removal_permit(&proof, &lock, &self.initial)?;
            let mut record = RemovalRecord::decode(permit.bytes())?;
            self.record.same_selection(&record)?;
            if record.handoff_stage()
                != (RemovalHandoffStage::ResumeIntent {
                    index: child.index(),
                })
            {
                return Err(NativeError::OutcomeUnknown);
            }
            record.advance_handoff(RemovalHandoffStage::Ready {
                index: child.index(),
            })?;
            self.io
                .publish_removal(&proof, &lock, &record, &self.initial)?;
            self.record = record;
            Ok(())
        }
        fn commit_intent(&mut self, child: &Self::Child) -> NativeResult<()> {
            let proof = self.io.admit_support(&self.initial)?;
            let lock = self.io.acquire_installer_lock(&proof, &self.initial)?;
            let proof = self.io.admit_support(&self.initial)?;
            let permit = self.io.admit_removal_permit(&proof, &lock, &self.initial)?;
            let mut record = RemovalRecord::decode(permit.bytes())?;
            self.record.same_selection(&record)?;
            if record.handoff_stage()
                != (RemovalHandoffStage::Ready {
                    index: child.index(),
                })
            {
                return Err(NativeError::OutcomeUnknown);
            }
            record.advance_handoff(RemovalHandoffStage::RemovalCommitIntent {
                index: child.index(),
            })?;
            self.io
                .publish_removal(&proof, &lock, &record, &self.initial)?;
            record.advance_handoff(RemovalHandoffStage::Committed {
                index: child.index(),
            })?;
            self.io.publish_removal(
                &self.io.admit_support(&self.initial)?,
                &lock,
                &record,
                &self.initial,
            )?;
            self.record = record;
            drop(lock);
            Ok(())
        }
        fn apply_once(&mut self, child: &Self::Child) -> NativeResult<KeeperProgress> {
            // One actual Commit message, following the durable intent/result and lock release.
            match child.exchange(ControlMethod::Commit, &self.initial)? {
                ControlState::Committed => Ok(KeeperProgress::Pending),
                _ => Err(NativeError::OutcomeUnknown),
            }
        }
        fn recover_same_owner(&mut self, child: &Self::Child) -> NativeResult<KeeperProgress> {
            self.observed(child, &budget(&self.io, 5000)?)
        }
        fn settle(&mut self, child: &Self::Child) -> NativeResult<()> {
            let deadline = budget(&self.io, 5000)?;
            child
                .actual_exit(&self.io.admit_support(&deadline)?, &deadline)?
                .ok_or(NativeError::OutcomeUnknown)?;
            Ok(())
        }
        fn cancel_before_stop(&mut self, child: &Self::Child) -> NativeResult<()> {
            let _ = child.exchange(ControlMethod::Cancel, &self.initial)?;
            // This acknowledgement is not file removal or a terminal journal clear.
            Ok(())
        }
    }
    pub(super) struct ContinuationState {
        live: Option<Mutex<(KeeperControl<Arc<RemovalChild>>, SourcePort)>>,
        terminal: &'static str,
    }
    impl ContinuationState {
        pub(super) fn status(&self) -> NativeResult<&'static str> {
            let Some(live) = &self.live else {
                return Ok(self.terminal);
            };
            let mut live = live.try_lock().map_err(|error| match error {
                std::sync::TryLockError::WouldBlock => NativeError::Busy,
                std::sync::TryLockError::Poisoned(_) => NativeError::OutcomeUnknown,
            })?;
            let (control, port) = &mut *live;
            if !control.committed() {
                return Ok("retained_uncommitted");
            }
            let _ = control.recover(port);
            if control.stage() == native_io::activation::KeeperStage::Complete {
                Ok("removed_with_retained_copy")
            } else {
                Ok("retained")
            }
        }
    }
    pub(super) fn begin(erase_identity: bool) -> NativeResult<ContinuationState> {
        payload::inventory::ApprovedInventory::embedded()?;
        let clock: Arc<dyn native_io::Clock> = Arc::new(MonotonicClock::default());
        let deadline = Deadline::new(30_000, clock.clone(), Cancellation::default())?;
        let io = Arc::new(WindowsNativeIo::current(clock, &deadline)?);
        // Same-context warm flow is unchanged; different-ended-logon file-only recovery must
        // finish separately before selecting any images or a new removal operation.
        payload::recover_prior_logon_files_for_entry(&io, &deadline)?;
        let proof = io.admit_support(&deadline)?;
        let lock = io.acquire_installer_lock(&proof, &deadline)?;
        let proof = io.admit_support(&deadline)?;
        let own = io.self_image(&proof, &deadline)?;
        own.reverify(&io, &proof, &deadline)?;
        if let Some(record) = io.read_removal(&proof, &deadline)?
            && matches!(
                record.cursor(),
                RemovalCursor::Complete { .. }
                    | RemovalCursor::FinalCopyCleanupIntent { .. }
                    | RemovalCursor::FinalCopyAbsent { .. }
                    | RemovalCursor::Retired
            )
        {
            let rt = copy::runtime()?;
            let (server, namespace) = rt.block_on(async {
                copy::RemovalKeeperLease::reserve(io.clone(), &proof, record.operation(), &deadline)
            })?;
            let permit = io.admit_removal_permit(&proof, &lock, &deadline)?;
            let root = io.reopen_removal_root(&proof, &lock, &permit, &deadline)?;
            let mut resident = super::removal_port::Resident::reopen_terminal(
                io.clone(),
                root,
                lock,
                record,
                namespace,
            )?;
            let result = resident.run(&deadline)?;
            drop(resident);
            drop(server);
            drop(rt);
            return Ok(ContinuationState {
                live: None,
                terminal: match result {
                    RemovalResult::Removed => "removed",
                    RemovalResult::RemovedWithRetainedCopy { .. } => "removed_with_retained_copy",
                    RemovalResult::Retained => "retained",
                },
            });
        }
        let (root, record) =
            io.prepare_removal(&proof, &lock, RemovalOptions { erase_identity }, &deadline)?;
        if record.cursor() != RemovalCursor::Selected
            || record.handoff_stage() != RemovalHandoffStage::None
        {
            return Err(NativeError::OutcomeUnknown);
        }
        io.publish_removal(&proof, &lock, &record, &deadline)?;
        drop(own);
        let mut port = SourcePort {
            io,
            root,
            lock: Some(lock),
            record,
            initial: deadline,
        };
        let mut control = KeeperControl::default();
        if let Err(error) = control.prepare(&mut port) {
            let _ = control.cancel(&mut port);
            return Err(error);
        }
        let _ = control.commit(&mut port);
        Ok(ContinuationState {
            live: Some(Mutex::new((control, port))),
            terminal: "retained",
        })
    }

    pub(super) fn maybe_entry(
        io: Arc<WindowsNativeIo>,
        initial: &Deadline,
        kind: RemovalCopyKind,
    ) -> NativeResult<bool> {
        let proof = io.admit_support(initial)?;
        let Some(record) = io.read_removal(&proof, initial)? else {
            return Ok(false);
        };
        if record.cursor() == RemovalCursor::Retired {
            return Ok(false);
        }
        let mut server = RemovalServer::new(io.clone(), kind, initial)?;
        let Some(commit) = server.await_commit(initial)? else {
            return Ok(true);
        };
        let commit = Arc::new(commit);
        let mut resident = None;
        let mut previous = None;
        loop {
            // A committed actual owner is resident while unresolved. Every call is bounded,
            // state/calls/queue remain finite, and no error replaces an invoked controller.
            let deadline = match budget(&io, 30_000) {
                Ok(deadline) => deadline,
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(250));
                    continue;
                }
            };
            if resident.is_none() {
                let prepared = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let proof = io.admit_support(&deadline)?;
                    commit.reverify(&io, &proof, record.operation(), &deadline)?;
                    let lock = io.acquire_installer_lock(&proof, &deadline)?;
                    let proof = io.admit_support(&deadline)?;
                    let current = io
                        .read_removal(&proof, &deadline)?
                        .ok_or(NativeError::Missing)?;
                    record.same_selection(&current)?;
                    let (root, selected) =
                        io.prepare_removal(&proof, &lock, current.options(), &deadline)?;
                    if selected.encode()? != current.encode()? {
                        return Err(NativeError::Foreign);
                    }
                    super::removal_port::Resident::new(
                        io.clone(),
                        root,
                        lock,
                        current,
                        commit.clone(),
                    )
                }));
                if let Ok(Ok(actual)) = prepared {
                    resident = Some(actual)
                }
            }
            let result = resident.as_mut().map(|actual| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| actual.run(&deadline)))
            });
            let phase = if let Ok(proof) = io.admit_support(&deadline) {
                io.read_removal(&proof, &deadline)
                    .ok()
                    .flatten()
                    .map(|record| record.cursor())
            } else {
                None
            };
            if phase != previous {
                eprintln!("Crosspane removal keeper phase: {phase:?}");
                previous = phase;
            }
            let done = matches!(
                result,
                Some(Ok(Ok(
                    RemovalResult::Removed | RemovalResult::RemovedWithRetainedCopy { .. }
                )))
            );
            let state = if done {
                ControlState::Complete
            } else {
                ControlState::Retained
            };
            if let Ok(observe) = budget(&io, 100) {
                let _ = server.poll_status(state, &observe);
            }
            if done {
                return Ok(true);
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

#[cfg(all(windows, not(test)))]
pub(crate) fn removal_entry_for_helper(
    io: std::sync::Arc<super::native_io::WindowsNativeIo>,
    deadline: &super::native_io::Deadline,
) -> NativeResult<bool> {
    removal_native::maybe_entry(
        io,
        deadline,
        super::removal::inventory::RemovalCopyKind::Helper,
    )
}
