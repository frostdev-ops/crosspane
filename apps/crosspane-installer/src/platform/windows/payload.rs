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

#[cfg(windows)]
mod native {
    use super::super::native_io::{
        Deadline, InstallerLock, NativeError, NativeResult, OpenedPe, PayloadRoot, PruneOutcome,
        SelfImagePin, StagedPe, SupportProof, WindowsNativeIo,
    };
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
    pub(crate) struct PayloadInput {
        pub role: PayloadRole,
        pub content: Box<dyn std::io::Read + Send>,
    }
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
                lock,
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
                lock,
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
        lock: &'a InstallerLock,
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
                self.lock,
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
                self.lock,
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
                self.lock,
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
                self.lock,
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
                self.lock,
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
            if self.permit()?.phase() != Phase::StartIntent {
                return Err(NativeError::Foreign);
            }
            self.service.start_once(operation, verified)
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
                    self.lock,
                    self.permit()?,
                    old,
                    self.deadline,
                )? {
                    PruneOutcome::Removed(pruned) => {
                        let proof = self.proof()?;
                        self.payload.io.retire_pruned(
                            &proof,
                            self.lock,
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
                self.lock,
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
                self.lock,
                self.permit()?,
                &self.payload.inventory,
                &self.payload.installer,
                self.deadline,
            )
        }
    }
}
#[cfg(windows)]
#[allow(unused_imports)]
// A5 supplies bounded untrusted content through the retained approved staging facade.
pub(crate) use native::PayloadInput;
#[cfg(windows)]
pub(crate) use native::WindowsPayload;
#[cfg(windows)]
pub(crate) use recovery::resume_helper;
