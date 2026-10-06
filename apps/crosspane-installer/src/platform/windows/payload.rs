//! Fixed-role Windows payload policy. Observed catalog facts never confer native authority.
#[path = "payload/health.rs"]
pub(crate) mod health;
#[path = "payload/helper.rs"]
pub(crate) mod helper;
#[path = "payload/inventory.rs"]
pub(crate) mod inventory;
#[path = "payload/recovery.rs"]
pub(crate) mod recovery;
#[path = "payload/staging.rs"]
pub(crate) mod staging;

/// Untrusted source content; approval still comes only from the executing build.
#[cfg(any(windows, test))]
pub(crate) struct PayloadInput {
    pub role: inventory::PayloadRole,
    pub content: Box<dyn std::io::Read + Send>,
}
/// Immutable bounded bytes survive the outer's process without granting path/native authority.
#[cfg(any(windows, test))]
#[cfg_attr(test, allow(dead_code))]
pub(crate) struct ApprovedOuterSources {
    roles: Vec<(inventory::PayloadRole, std::sync::Arc<[u8]>)>,
    facts: [inventory::PeFacts; 3],
}
#[cfg(any(windows, test))]
#[cfg_attr(test, allow(dead_code))]
impl ApprovedOuterSources {
    pub(crate) fn facts(&self) -> &[inventory::PeFacts; 3] {
        &self.facts
    }
    pub(crate) fn roles(&self) -> &[(inventory::PayloadRole, std::sync::Arc<[u8]>)] {
        &self.roles
    }
    pub(crate) fn bytes(
        &self,
        role: inventory::PayloadRole,
    ) -> super::native_io::NativeResult<&[u8]> {
        self.roles
            .iter()
            .find(|(r, _)| *r == role)
            .map(|(_, b)| b.as_ref())
            .ok_or(super::native_io::NativeError::Invalid)
    }
    pub(crate) fn inputs(&self) -> Vec<PayloadInput> {
        self.roles
            .iter()
            .map(|(role, bytes)| PayloadInput {
                role: *role,
                content: Box::new(std::io::Cursor::new(bytes.clone())),
            })
            .collect()
    }
    /// Receiver verifies AGAIN using its own embedded inventory, not sender-supplied approvals.
    pub(crate) fn receive(
        bytes: [Vec<u8>; 3],
        inventory: &inventory::ApprovedInventory,
        installer: &inventory::ApprovedPe,
        deadline: &super::native_io::Deadline,
    ) -> super::native_io::NativeResult<Self> {
        use inventory::PayloadRole;
        Self::receive_roles(
            [PayloadRole::Agent, PayloadRole::Ui, PayloadRole::Ctl]
                .into_iter()
                .zip(bytes)
                .collect(),
            inventory,
            installer,
            deadline,
        )
    }
    fn receive_roles(
        roles: Vec<(inventory::PayloadRole, Vec<u8>)>,
        inventory: &inventory::ApprovedInventory,
        installer: &inventory::ApprovedPe,
        deadline: &super::native_io::Deadline,
    ) -> super::native_io::NativeResult<Self> {
        use super::native_io::NativeError;
        use inventory::PayloadRole;
        deadline.check()?;
        inventory.check_staging_budget(installer, true)?;
        if roles.len() != 3 {
            return Err(NativeError::Invalid);
        }
        let mut result = Vec::with_capacity(3);
        let ordered = [PayloadRole::Agent, PayloadRole::Ui, PayloadRole::Ctl];
        let mut facts = Vec::with_capacity(3);
        for role in ordered {
            let mut matches = roles.iter().filter(|(r, _)| *r == role);
            let (_, bytes) = matches.next().ok_or(NativeError::Invalid)?;
            if matches.next().is_some() {
                return Err(NativeError::Invalid);
            }
            verify_outer_source(bytes, inventory.role(role)?, deadline)?;
            facts.push(inventory.role(role)?.facts().clone());
        }
        for role in ordered {
            let (_, bytes) = roles
                .iter()
                .find(|(r, _)| *r == role)
                .ok_or(NativeError::Invalid)?;
            // Move each validated allocation below; no caller can mutate the sealed bundle.
            if bytes.len() as u64 != inventory.role(role)?.size() {
                return Err(NativeError::Foreign);
            }
        }
        for (role, bytes) in roles {
            result.push((role, std::sync::Arc::from(bytes)));
        }
        Ok(Self {
            roles: result,
            facts: facts.try_into().map_err(|_| NativeError::Invalid)?,
        })
    }
}
#[cfg(any(windows, test))]
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn verify_outer_source(
    bytes: &[u8],
    approved: &inventory::ApprovedPe,
    deadline: &super::native_io::Deadline,
) -> super::native_io::NativeResult<()> {
    use super::native_io::NativeError;
    use aws_lc_rs::digest::{SHA256, digest};
    deadline.check()?;
    if bytes.len() as u64 != approved.size() {
        return Err(NativeError::Foreign);
    }
    let (machine, subsystem) =
        inventory::pe_header(&bytes[..bytes.len().min(1024 * 1024)], approved.size())?;
    if machine != approved.machine()
        || subsystem != approved.subsystem()
        || digest(&SHA256, bytes).as_ref() != approved.sha256()
    {
        return Err(NativeError::Foreign);
    }
    deadline.check()
}
/// The only pre-Stop source read: exact expected length plus one overflow-detection byte.
/// A blocking supplied Read is never allowed to make Stop eligible; every completion is rechecked.
#[cfg(any(windows, test))]
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn buffer_outer_sources(
    mut inputs: Vec<PayloadInput>,
    inventory: &inventory::ApprovedInventory,
    installer: &inventory::ApprovedPe,
    deadline: &super::native_io::Deadline,
) -> super::native_io::NativeResult<ApprovedOuterSources> {
    use super::native_io::NativeError;
    use inventory::PayloadRole;
    deadline.check()?;
    inventory.check_staging_budget(installer, true)?;
    if inputs.len() != 3 {
        return Err(NativeError::Invalid);
    }
    let mut roles = Vec::with_capacity(3);
    for role in [PayloadRole::Agent, PayloadRole::Ui, PayloadRole::Ctl] {
        if inputs.iter().filter(|input| input.role == role).count() != 1 {
            return Err(NativeError::Invalid);
        }
        let index = inputs
            .iter()
            .position(|input| input.role == role)
            .ok_or(NativeError::Invalid)?;
        let mut input = inputs.remove(index);
        let cap = usize::try_from(inventory.role(role)?.size())
            .map_err(|_| NativeError::Oversize)?
            .checked_add(1)
            .ok_or(NativeError::Oversize)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(cap)
            .map_err(|_| NativeError::Oversize)?;
        let mut chunk = [0u8; 64 * 1024];
        while bytes.len() < cap {
            deadline.check()?;
            let room = (cap - bytes.len()).min(chunk.len());
            let n = input
                .content
                .read(&mut chunk[..room])
                .map_err(|_| NativeError::Unavailable)?;
            deadline.check()?;
            if n == 0 {
                break;
            }
            if n > room {
                return Err(NativeError::Invalid);
            }
            bytes.extend_from_slice(&chunk[..n]);
        }
        verify_outer_source(&bytes, inventory.role(role)?, deadline)?;
        roles.push((role, bytes));
    }
    ApprovedOuterSources::receive_roles(roles, inventory, installer, deadline)
}

/// Actual owned-lock boundary, shared with focused fakes. It has no native authority factory.
#[cfg(any(windows, test))]
#[allow(dead_code)] // Older source-included test roots compile but do not execute this new boundary.
pub(crate) enum LockHandoff<'a, L> {
    Borrowed(&'a L),
    Owned(Option<L>),
}
#[cfg(any(windows, test))]
#[allow(dead_code)]
impl<L> LockHandoff<'_, L> {
    pub(crate) fn get(&self) -> super::native_io::NativeResult<&L> {
        match self {
            Self::Borrowed(lock) => Ok(lock),
            Self::Owned(lock) => lock
                .as_ref()
                .ok_or(super::native_io::NativeError::OutcomeUnknown),
        }
    }
    /// Caller validates its durable selection and native idle state before entering. A failed
    /// effect/reentry cannot restore an old permit or run again through this consumed boundary.
    pub(crate) fn run_once<P, S>(
        &mut self,
        permit: &mut Option<P>,
        native_idle: bool,
        effect: impl FnOnce() -> super::native_io::NativeResult<S>,
        reenter: impl FnOnce() -> super::native_io::NativeResult<(L, P)>,
    ) -> super::native_io::NativeResult<S> {
        use super::native_io::NativeError;
        if !native_idle {
            return Err(NativeError::OutcomeUnknown);
        }
        let Self::Owned(lock) = self else {
            return Err(NativeError::Unsupported);
        };
        if permit.is_none() {
            return Err(NativeError::Foreign);
        }
        let owned = lock.take().ok_or(NativeError::OutcomeUnknown)?;
        *permit = None;
        drop(owned);
        let started = effect()?;
        let (fresh_lock, fresh_permit) = reenter()?;
        *self = Self::Owned(Some(fresh_lock));
        *permit = Some(fresh_permit);
        Ok(started)
    }
}

#[cfg(windows)]
mod native {
    use super::super::native_io::{
        Deadline, InstallerLock, NativeError, NativeResult, OpenedPe, PayloadRoot, PruneOutcome,
        SelfImagePin, StagedPe, SupportProof, WindowsNativeIo,
    };
    use super::{LockHandoff, PayloadInput};
    use super::{
        health::{ServicePort, VerifiedPayload},
        inventory::{ApprovedInventory, ApprovedPe, PayloadRole},
        recovery::{
            self, FileStamp, ImageObservation, MutationPermit, OperationRecord, OriginalLeaf, Phase,
        },
        staging::PayloadPort,
    };
    use std::sync::Arc;
    /// Content is untrusted and grants no path or approval capability. The embedded inventory
    /// pins and the opened stage's whole size/SHA/PE decide whether it may be published.
    pub(crate) struct WindowsPayload {
        io: Arc<WindowsNativeIo>,
        inventory: ApprovedInventory,
        module: SelfImagePin,
        installer: ApprovedPe,
        root: PayloadRoot,
    }
    impl WindowsPayload {
        pub(crate) fn new(
            io: Arc<WindowsNativeIo>,
            proof: &SupportProof,
            lock: &InstallerLock,
            deadline: &Deadline,
        ) -> NativeResult<Self> {
            let inventory = ApprovedInventory::embedded()?;
            let module = io.self_image(proof, deadline)?;
            let installer = ApprovedPe::own_image(&module)?;
            inventory.check_staging_budget(&installer, true)?;
            let root = io.payload_root(proof, lock, deadline)?;
            Ok(Self {
                io,
                inventory,
                module,
                installer,
                root,
            })
        }
        // A4b/A5 will call the complete install sequence after genuine native owner completion.
        #[allow(dead_code)]
        pub(crate) fn apply<S: ServicePort>(
            &self,
            service: &mut S,
            lock: &InstallerLock,
            record: &mut OperationRecord,
            inputs: Vec<PayloadInput>,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if inputs.len() != 3
                || [PayloadRole::Agent, PayloadRole::Ui, PayloadRole::Ctl]
                    .iter()
                    .any(|role| inputs.iter().filter(|input| input.role == *role).count() != 1)
            {
                return Err(NativeError::Invalid);
            }
            let mut port = NativePayloadPort {
                payload: self,
                service,
                lock: LockHandoff::Borrowed(lock),
                deadline,
                inputs,
                permit: None,
                staged: Vec::new(),
                published: Vec::new(),
                stop: None,
            };
            super::staging::apply(&mut port, record)
        }
        pub(crate) fn recover<S: ServicePort>(
            &self,
            service: &mut S,
            lock: &InstallerLock,
            record: &mut OperationRecord,
            inputs: Vec<PayloadInput>,
            deadline: &Deadline,
        ) -> NativeResult<recovery::RecoveryDecision> {
            if inputs.iter().enumerate().any(|(i, input)| {
                input.role == PayloadRole::Installer
                    || inputs[..i].iter().any(|old| old.role == input.role)
            }) {
                return Err(NativeError::Invalid);
            }
            let mut port = NativePayloadPort {
                payload: self,
                service,
                lock: LockHandoff::Borrowed(lock),
                deadline,
                inputs,
                permit: None,
                staged: Vec::new(),
                published: Vec::new(),
                stop: None,
            };
            recovery::resume(&mut port, record)
        }
        /// The caller transfers the actual installer lock. No borrowed-lock path may Run.
        #[allow(dead_code)] // The installer apply UI is supplied by the next integration package.
        pub(crate) fn apply_owned(
            &self,
            service: &mut super::super::service::NativeUpgradePort,
            lock: InstallerLock,
            record: &mut OperationRecord,
            inputs: Vec<PayloadInput>,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if inputs.len() != 3
                || [PayloadRole::Agent, PayloadRole::Ui, PayloadRole::Ctl]
                    .iter()
                    .any(|role| inputs.iter().filter(|input| input.role == *role).count() != 1)
            {
                return Err(NativeError::Invalid);
            }
            service.bind_io(self.io.clone(), deadline)?;
            service.bind_stop_lock(&lock, deadline)?;
            let mut port = NativePayloadPort {
                payload: self,
                service,
                lock: LockHandoff::Owned(Some(lock)),
                deadline,
                inputs,
                permit: None,
                staged: Vec::new(),
                published: Vec::new(),
                stop: None,
            };
            super::staging::apply(&mut port, record)
        }
        /// Recovery keeps Stop and Run observational and owns any lock handoff at StartIntent.
        pub(crate) fn recover_owned(
            &self,
            service: &mut super::super::service::NativeUpgradePort,
            lock: InstallerLock,
            record: &mut OperationRecord,
            inputs: Vec<PayloadInput>,
            deadline: &Deadline,
        ) -> NativeResult<recovery::RecoveryDecision> {
            if inputs.iter().enumerate().any(|(i, input)| {
                input.role == PayloadRole::Installer
                    || inputs[..i].iter().any(|old| old.role == input.role)
            }) {
                return Err(NativeError::Invalid);
            }
            service.bind_io(self.io.clone(), deadline)?;
            let mut port = NativePayloadPort {
                payload: self,
                service,
                lock: LockHandoff::Owned(Some(lock)),
                deadline,
                inputs,
                permit: None,
                staged: Vec::new(),
                published: Vec::new(),
                stop: None,
            };
            recovery::resume(&mut port, record)
        }
        // A4b/A5 will stage a helper once genuine upgrade completion is available.
        #[allow(dead_code)]
        pub(crate) fn stage_helper(
            &self,
            lock: &InstallerLock,
            permit: &MutationPermit,
            deadline: &Deadline,
        ) -> NativeResult<OpenedPe> {
            let proof = self.io.admit_support(deadline)?;
            self.root
                .stage_helper(&self.io, &proof, lock, permit, &self.module, deadline)
        }
    }
    struct NativePayloadPort<'a, S> {
        payload: &'a WindowsPayload,
        service: &'a mut S,
        lock: LockHandoff<'a, InstallerLock>,
        deadline: &'a Deadline,
        inputs: Vec<PayloadInput>,
        permit: Option<MutationPermit>,
        staged: Vec<StagedPe>,
        published: Vec<OpenedPe>,
        stop: Option<(Option<u64>, [u8; 16])>,
    }
    impl<S: ServicePort> NativePayloadPort<'_, S> {
        fn permit(&self) -> NativeResult<&MutationPermit> {
            self.permit.as_ref().ok_or(NativeError::Foreign)
        }
        fn proof(&self) -> NativeResult<SupportProof> {
            self.payload.io.admit_support(self.deadline)
        }
    }
    impl<S: ServicePort> PayloadPort for NativePayloadPort<'_, S> {
        type Stop = super::super::service::UpgradeStopProof;
        type Verified = VerifiedPayload;
        type Started = super::super::service::NewInstanceEvidence;
        fn journal(&mut self, record: &OperationRecord) -> NativeResult<()> {
            let proof = self.proof()?;
            self.permit = Some(recovery::save_operation(
                self.payload.io.clone(),
                &proof,
                self.lock.get()?,
                record,
                self.deadline,
            )?);
            Ok(())
        }
        fn stop(&mut self, operation: [u8; 16]) -> NativeResult<Self::Stop> {
            // Production returns Unsupported in a4. A4b must supply genuine old-owner/job proof.
            let proof = self.service.stop_for_replace(operation)?;
            if proof.operation() != operation {
                return Err(NativeError::Foreign);
            }
            self.stop = Some((proof.original_instance(), operation));
            Ok(proof)
        }
        fn original_instance(&self, stop: &Self::Stop) -> Option<u64> {
            stop.original_instance()
        }
        fn released(&mut self, stop: &Self::Stop) -> NativeResult<()> {
            if self.stop != Some((stop.original_instance(), stop.operation())) {
                return Err(NativeError::Foreign);
            }
            let proof = self.proof()?;
            let release = self.payload.root.prove_released(
                &self.payload.io,
                &proof,
                self.lock.get()?,
                stop.operation(),
                self.deadline,
            )?;
            release.binding(&self.payload.io, stop.operation())
        }
        fn stage(
            &mut self,
            operation: [u8; 16],
            role: PayloadRole,
        ) -> NativeResult<ImageObservation> {
            let proof = self.proof()?;
            let expected = if role == PayloadRole::Installer {
                &self.payload.installer
            } else {
                self.payload.inventory.role(role)?
            };
            let content: Box<dyn std::io::Read + Send> = if role == PayloadRole::Installer {
                self.payload
                    .io
                    .self_image_reader(&self.payload.module, &proof, self.deadline)?
            } else {
                let at = self
                    .inputs
                    .iter()
                    .position(|input| input.role == role)
                    .ok_or(NativeError::Invalid)?;
                self.inputs.swap_remove(at).content
            };
            if self.permit()?.operation() != operation {
                return Err(NativeError::Foreign);
            }
            let staged = self.payload.root.stage(
                &self.payload.io,
                &proof,
                self.lock.get()?,
                self.permit()?,
                role,
                content,
                expected,
                self.deadline,
            )?;
            let observed = staged.observation();
            self.staged.push(staged);
            Ok(observed)
        }
        fn observe_original(&mut self, role: PayloadRole) -> NativeResult<OriginalLeaf> {
            let proof = self.proof()?;
            Ok(
                match self.payload.root.observe_opaque(
                    &self.payload.io,
                    &proof,
                    role,
                    self.deadline,
                )? {
                    Some(leaf) => OriginalLeaf::Present(leaf.identity().into()),
                    None => OriginalLeaf::Missing,
                },
            )
        }
        fn backup(
            &mut self,
            operation: [u8; 16],
            role: PayloadRole,
        ) -> NativeResult<Option<FileStamp>> {
            let proof = self.proof()?;
            let Some(observed) =
                self.payload
                    .root
                    .observe_opaque(&self.payload.io, &proof, role, self.deadline)?
            else {
                return Ok(None);
            };
            if self.permit()?.operation() != operation {
                return Err(NativeError::Foreign);
            }
            let id = self.payload.root.backup(
                &self.payload.io,
                &proof,
                self.lock.get()?,
                self.permit()?,
                observed,
                self.deadline,
            )?;
            Ok(Some(id.into()))
        }
        fn publish(
            &mut self,
            operation: [u8; 16],
            role: PayloadRole,
        ) -> NativeResult<ImageObservation> {
            let at = self
                .staged
                .iter()
                .position(|image| image.role() == role)
                .ok_or(NativeError::Invalid)?;
            let staged = self.staged.swap_remove(at);
            let proof = self.proof()?;
            if self.permit()?.operation() != operation || self.permit()?.role() != Some(role) {
                return Err(NativeError::Foreign);
            }
            let image = self.payload.root.publish(
                &self.payload.io,
                &proof,
                self.lock.get()?,
                self.permit()?,
                staged,
                self.deadline,
            )?;
            let observed = ImageObservation {
                identity: image.identity().into(),
                facts: image.approved().facts().clone(),
            };
            self.published.push(image);
            Ok(observed)
        }
        fn verify(&mut self, operation: [u8; 16]) -> NativeResult<Self::Verified> {
            let proof = self.proof()?;
            VerifiedPayload::open(
                operation,
                &self.payload.io,
                &proof,
                &self.payload.root,
                &self.payload.inventory,
                &self.payload.installer,
                self.deadline,
            )
        }
        fn start(
            &mut self,
            operation: [u8; 16],
            verified: &Self::Verified,
        ) -> NativeResult<Self::Started> {
            let proof = self.proof()?;
            verified.reverify(&self.payload.io, &proof, self.deadline)?;
            if self.permit()?.phase() != Phase::StartIntent
                || self.permit()?.operation() != operation
                || verified.operation() != operation
            {
                return Err(NativeError::Foreign);
            }
            let before = recovery::selected_operation(&self.payload.io, &proof, self.deadline)?
                .ok_or(NativeError::Missing)?;
            if before.operation() != operation || before.phase() != Phase::StartIntent {
                return Err(NativeError::Foreign);
            }
            let selected = serde_json::to_value(&before).map_err(|_| NativeError::Invalid)?;
            let encoded = super::super::native_io::records::encode_record(
                &super::super::native_io::records::RecordName::Operation(operation),
                selected.clone(),
            )?;
            if self.permit()?.bytes() != encoded {
                return Err(NativeError::Foreign);
            }
            // The fixed-image constructor and the new supervisor acquire the same lock. Release
            // only our actual owned lock, with no outstanding IO call or mutation permit.
            let payload = self.payload;
            let deadline = self.deadline;
            let service = &mut *self.service;
            self.lock.run_once(
                &mut self.permit,
                payload.io.native_idle(),
                || service.start_once(operation, verified),
                || {
                    // A timeout or an unknown Run leaves StartIntent durable; reentry is not run.
                    // Fresh support/lock/record/image admission precedes later publications.
                    let proof = payload.io.admit_support(deadline)?;
                    let acquired = payload.io.acquire_installer_lock(&proof, deadline)?;
                    let proof = payload.io.admit_support(deadline)?;
                    let after = recovery::selected_operation(&payload.io, &proof, deadline)?
                        .ok_or(NativeError::Missing)?;
                    if serde_json::to_value(&after).map_err(|_| NativeError::Invalid)? != selected {
                        return Err(NativeError::Foreign);
                    }
                    let _root = payload.io.payload_root(&proof, &acquired, deadline)?;
                    verified.reverify(&payload.io, &proof, deadline)?;
                    let proof = payload.io.admit_support(deadline)?;
                    let fresh_permit = recovery::save_operation(
                        payload.io.clone(),
                        &proof,
                        &acquired,
                        &after,
                        deadline,
                    )?;
                    Ok((acquired, fresh_permit))
                },
            )
        }
        fn health(
            &mut self,
            operation: [u8; 16],
            started: &Self::Started,
            verified: &Self::Verified,
        ) -> NativeResult<String> {
            let proof = self.proof()?;
            verified.reverify(&self.payload.io, &proof, self.deadline)?;
            if started.operation() != operation
                || started.image_identity() != verified.agent_identity()?
                || started.instance() == 0
                || self
                    .stop
                    .is_some_and(|(old, op)| op != operation || old == Some(started.instance()))
            {
                return Err(NativeError::Foreign);
            }
            Ok(format!("{:032x}", started.instance()))
        }
        fn prune(&mut self, _operation: [u8; 16]) -> NativeResult<bool> {
            let proof = self.proof()?;
            let catalog = recovery::catalog(&self.payload.io, &proof, self.deadline)?;
            let mut complete = true;
            for old in recovery::prune_candidates(&catalog)? {
                let proof = self.proof()?;
                match self.payload.root.prune(
                    &self.payload.io,
                    &proof,
                    self.lock.get()?,
                    self.permit()?,
                    old,
                    self.deadline,
                )? {
                    PruneOutcome::Removed(pruned) => {
                        let proof = self.proof()?;
                        self.payload.io.retire_pruned(
                            &proof,
                            self.lock.get()?,
                            self.permit()?,
                            pruned,
                            self.deadline,
                        )?;
                    }
                    PruneOutcome::Retained => complete = false,
                }
            }
            let proof = self.proof()?;
            let retained = recovery::catalog(&self.payload.io, &proof, self.deadline)?;
            Ok(complete && retained.generations.iter().filter(|g| g.completed).count() <= 3)
        }
    }
    impl<S: ServicePort> recovery::RecoveryPort for NativePayloadPort<'_, S> {
        fn recover_stop(&mut self, record: &OperationRecord) -> NativeResult<Self::Stop> {
            let stop = self.service.recover_stop(record)?;
            if stop.operation() != record.operation() {
                return Err(NativeError::Foreign);
            }
            self.stop = Some((stop.original_instance(), stop.operation()));
            Ok(stop)
        }
        fn observe_role(
            &mut self,
            record: &OperationRecord,
            role: PayloadRole,
        ) -> NativeResult<recovery::ReopenedRole> {
            use recovery::{FixedObservation, ReopenedRole, StageObservation};
            let pin = if role == PayloadRole::Installer {
                &self.payload.installer
            } else {
                self.payload.inventory.role(role)?
            };
            let proof = self.proof()?;
            let staged = match self.payload.root.open_staged(
                &self.payload.io,
                &proof,
                record.operation(),
                role,
                pin,
                self.deadline,
            ) {
                Ok(Some(image)) => {
                    let facts = image.observation();
                    self.staged.push(image);
                    StageObservation::Ready(facts)
                }
                Ok(None) => StageObservation::Missing,
                Err(
                    NativeError::Unsupported
                    | NativeError::Foreign
                    | NativeError::Unavailable
                    | NativeError::Busy,
                ) => StageObservation::Unknown,
                Err(error) => return Err(error),
            };
            let proof = self.proof()?;
            let observed =
                self.payload
                    .root
                    .observe_opaque(&self.payload.io, &proof, role, self.deadline)?;
            let fixed = match observed {
                None => FixedObservation::Missing,
                Some(leaf) => {
                    let stamp: FileStamp = leaf.identity().into();
                    if record
                        .role(role)?
                        .staged
                        .as_ref()
                        .is_some_and(|image| image.identity == stamp)
                    {
                        drop(leaf);
                        let proof = self.proof()?;
                        match self.payload.root.open_approved(
                            &self.payload.io,
                            &proof,
                            role,
                            pin,
                            self.deadline,
                        ) {
                            Ok(image) => {
                                let observed = ImageObservation {
                                    identity: image.identity().into(),
                                    facts: image.approved().facts().clone(),
                                };
                                self.published.push(image);
                                FixedObservation::Published(observed)
                            }
                            Err(
                                NativeError::Unsupported
                                | NativeError::Foreign
                                | NativeError::Unavailable
                                | NativeError::Busy,
                            ) => FixedObservation::Unknown,
                            Err(error) => return Err(error),
                        }
                    } else if matches!(
                        record.role(role)?.original,
                        OriginalLeaf::Unobserved | OriginalLeaf::Present(_)
                    ) {
                        FixedObservation::Original(stamp)
                    } else {
                        FixedObservation::Unknown
                    }
                }
            };
            let proof = self.proof()?;
            let (backup, unknown_backup) = match self.payload.root.observe_backup(
                &self.payload.io,
                &proof,
                record.operation(),
                role,
                self.deadline,
            ) {
                Ok(value) => (value.map(Into::into), false),
                Err(NativeError::Foreign | NativeError::Unavailable | NativeError::Busy) => {
                    (None, true)
                }
                Err(error) => return Err(error),
            };
            Ok(ReopenedRole {
                staged,
                fixed,
                backup,
                unknown_backup,
            })
        }
        fn recover_started(
            &mut self,
            record: &OperationRecord,
            verified: &Self::Verified,
        ) -> NativeResult<Option<Self::Started>> {
            let proof = self.proof()?;
            verified.reverify(&self.payload.io, &proof, self.deadline)?;
            let started = self.service.recover_started(record, verified)?;
            if started.as_ref().is_some_and(|new| {
                new.operation() != record.operation()
                    || record
                        .new_instance()
                        .is_some_and(|old| old != format!("{:032x}", new.instance()))
            }) {
                return Err(NativeError::Foreign);
            }
            // Correlation comparison only. The service factory, not this journal field, must
            // supply genuine retained-owner/current-instance authority in a4b.
            let old = record
                .original_instance()
                .map(|id| u64::from_str_radix(id, 16).map_err(|_| NativeError::Invalid))
                .transpose()?;
            self.stop = Some((old, record.operation()));
            Ok(started)
        }
        fn settle_stage(
            &mut self,
            record: &OperationRecord,
            role: PayloadRole,
        ) -> NativeResult<ImageObservation> {
            let stage = self
                .staged
                .iter()
                .find(|image| image.role() == role)
                .ok_or(NativeError::Missing)?;
            if record.operation() != self.permit()?.operation() {
                return Err(NativeError::Foreign);
            }
            let proof = self.proof()?;
            self.payload.root.settle_stage(
                &self.payload.io,
                &proof,
                self.lock.get()?,
                self.permit()?,
                stage,
                self.deadline,
            )
        }
        fn rollback_stage(
            &mut self,
            _record: &OperationRecord,
        ) -> NativeResult<recovery::RollbackOutcome> {
            self.staged.clear();
            let proof = self.proof()?;
            self.payload.root.rollback_stage(
                &self.payload.io,
                &proof,
                self.lock.get()?,
                self.permit()?,
                &self.payload.inventory,
                &self.payload.installer,
                self.deadline,
            )
        }
    }
}
#[cfg(windows)]
pub(crate) use native::WindowsPayload;
#[cfg(windows)]
#[allow(unused_imports)]
// Keep the frozen borrowed helper re-export; native entry transfers its owned lock.
pub(crate) use recovery::resume_helper;
