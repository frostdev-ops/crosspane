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
        deadline: Deadline,
        pub(super) lease: Option<Arc<native_io::StopLockLease>>,
        pub(super) stop: Attempt,
        sequence: StopSequence,
        owner: Option<AdmittedSupervisorOwner>,
        completion: Option<Arc<RetainedTreeCompletion>>,
        start_attempted: bool,
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
            let mut journal = journal::Journal::read(&self.io, &proof, &self.deadline)?
                .ok_or(NativeError::Missing)?;
            if journal.current != Some(selected) {
                return Err(NativeError::Foreign);
            }
            let retained = self.io.clone_agent(&original, &proof, &self.deadline)?;
            self.owner = Some(AdmittedSupervisorOwner::admit(
                self.io.clone(),
                retained,
                Some(lease.clone()),
                &self.deadline,
            )?);
            let port = WindowsAgentPort::new(
                self.io.clone(),
                self.io.admit_support(&self.deadline)?,
                self.io.bound_clock(),
                &self.deadline,
            )?;
            let settlement = port.stop_settlement();
            let timeout = self
                .deadline
                .remaining_ms()?
                .min(crate::agent_contract::MAX_TIMEOUT_MS);
            let native = supervisor::NativeStop {
                io: &self.io,
                proof: &proof,
                lock: lease.lock(),
                journal: &mut journal,
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
            settlement.wait(&self.deadline)?;
            drop(original);
            self.completion = Some(Arc::new(completed));
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
                        return Ok(NewInstanceEvidence {
                            operation: record.operation(),
                            instance,
                            image_identity: image,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupervisorExit {
    NormalQuit,
    InstallerStopped,
}

/// Windows-only early entry; it cannot start a GUI or manufacture image approval.
#[cfg(windows)]
pub fn supervisor_entry() -> NativeResult<SupervisorExit> {
    let trusted = TrustedImages::current()?;
    supervisor::native_entry(&trusted)
}

/// A4 must supply genuine fixed installer/agent role and PE/hash approval before this can start.
pub fn start_supported() -> NativeResult<()> {
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
