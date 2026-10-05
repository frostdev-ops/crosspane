//! Bounded public launchd observations and immutable plans; unknown syntax stays unobservable.
use super::*;
impl MacLaunchAgent {
    pub fn admit(
        io: Arc<MacNativeIo>,
        inventory: ApprovedInventory,
        approval: Arc<dyn ApprovalProbe>,
        deadline: &Deadline,
    ) -> NativeResult<Self> {
        let payload = MacPayload::admit(io.clone(), inventory.clone(), deadline)?;
        let rule = inventory
            .files
            .iter()
            .find(|f| f.path == "Crosspane.app/Contents/MacOS/Crosspane")
            .and_then(|f| f.signing.as_ref())
            .ok_or(NativeError::Invalid)?;
        let requirement = SigningRequirement {
            role: ArtifactRole::Agent,
            identifier: rule.identifier.clone(),
            designated_requirement: rule.designated_requirement.clone(),
            entitlements: rule.entitlements.clone(),
        };
        let main = io.admit_main_signature(
            &io.target()
                .paths()
                .payload_root
                .join("Crosspane.app/Contents/MacOS/Crosspane"),
            &requirement,
            deadline,
        )?;
        let support = io.admit_support(&main, deadline)?;
        support.check_agent_compatibility()?;
        let xml = render_plist(io.target())?;
        Ok(Self {
            io,
            payload,
            requirement,
            main,
            support,
            approval,
            owner: Arc::new(()),
            last: (0, 0),
            version: inventory.product_version.clone(),
            inventory,
            xml,
        })
    }
    pub(super) fn command(
        io: &MacNativeIo,
        action: NativeOperation,
        proof: Option<&SupportProof>,
        deadline: &Deadline,
    ) -> NativeResult<CommandOutput> {
        io.execute(&CommandSpec::new(io.target(), action)?, proof, deadline)
    }
    pub(super) fn snapshot(&self, io: &MacNativeIo, deadline: &Deadline) -> NativeResult<Snapshot> {
        let path = Self::plist(io);
        let identity = io.metadata(&path)?;
        let bytes = if let Some(id) = &identity {
            id.regular(io.target().paths().uid, false)?;
            io.read(&path, LIMIT, false, deadline)?
        } else {
            vec![]
        };
        let job = query_job(
            io,
            &Self::command(
                io,
                NativeOperation::Launchctl(LaunchctlAction::Print),
                None,
                deadline,
            )?,
        )?;
        let disabled = query_disabled(&Self::command(
            io,
            NativeOperation::Launchctl(LaunchctlAction::PrintDisabled),
            None,
            deadline,
        )?)?;
        if io.metadata(&path)? != identity {
            return Err(NativeError::Foreign);
        }
        Ok(Snapshot {
            identity,
            bytes,
            job,
            disabled,
        })
    }
    pub fn plan(
        &mut self,
        revision: u64,
        operation: u64,
        current: Option<(&SelectedAgent, &AgentReply)>,
        deadline: &Deadline,
    ) -> NativeResult<LaunchPlan> {
        if self.last.0 == u64::MAX || self.last.1 == u64::MAX {
            return Err(NativeError::IdExhausted);
        }
        if revision <= self.last.0 || operation <= self.last.1 {
            return Err(NativeError::Invalid);
        }
        self.support.check(&self.io, deadline)?;
        self.preflight(deadline)?;
        let snapshot = self.snapshot(&self.io, deadline)?;
        let session = self.io.support_observation(deadline)?.gui.console_session;
        let path = self.io.target().runtime_dir().join("bootstrap.json");
        let baseline = if self.io.metadata(&path)?.is_some() {
            Some(
                parse_bootstrap(&self.io.read(&path, 4096, true, deadline)?)
                    .map_err(|_| NativeError::Invalid)?
                    .instance_id,
            )
        } else {
            None
        };
        let mut original = None;
        let mut selected = None;
        if let Job::Running(pid) = snapshot.job {
            let (agent, reply) = current.ok_or(NativeError::Unavailable)?;
            checked_reply(&self.io, agent, reply, deadline)?;
            if agent.instance.process().pid != pid {
                return Err(NativeError::Foreign);
            }
            let DecodedReply::Status(StatusAdmission::Supported(health)) = reply
                .result
                .as_ref()
                .map_err(|_| NativeError::Unavailable)?
            else {
                return Err(NativeError::Unavailable);
            };
            original = Some(Arc::new(OriginalAgent::capture(
                agent.clone(),
                health,
                deadline,
            )?));
            selected = Some(agent.clone());
        }
        let record_origin = self.record_origin(&snapshot, deadline)?;
        let owned = record_origin.is_some();
        let state = if snapshot.disabled == Disabled::Yes {
            LaunchState::UserDisabled
        } else if snapshot.disabled == Disabled::Unknown || snapshot.job == Job::Unknown {
            LaunchState::Unobservable
        } else if snapshot.job == Job::LoadedStopped && !owned {
            LaunchState::Conflict
        } else if snapshot.job == Job::LoadedStopped {
            LaunchState::LoadedStopped
        } else if snapshot.identity.is_none() {
            LaunchState::Absent
        } else if owned {
            LaunchState::Owned
        } else {
            LaunchState::AdoptionRequired
        };
        let payload = self
            .payload
            .plan(revision, operation, original.clone(), deadline)?;
        if snapshot.job == Job::LoadedStopped && payload.state() != PayloadState::Matching {
            // Loaded-but-stopped does not prove a clean exit of the previously installed app.
            return Err(NativeError::Refused);
        }
        let installed_main = self
            .io
            .metadata(&self.io.target().agent_path())?
            .map(|_| {
                self.io.admit_main_signature(
                    &self.io.target().agent_path(),
                    &self.requirement,
                    deadline,
                )
            })
            .transpose()?;
        self.last = (revision, operation);
        Ok(LaunchPlan {
            owner: self.owner.clone(),
            binding: Arc::new(()),
            revision,
            operation,
            state,
            snapshot,
            record_origin,
            original,
            selected,
            installed_main,
            matching: payload.state() == PayloadState::Matching,
            payload: Some(payload),
            session,
            baseline,
        })
    }
    // Always refresh at action boundaries: the frozen proof deliberately exposes no age getter.
    pub(super) fn refreshed(
        &self,
        plan: &LaunchPlan,
        installed: Option<&SignatureProof>,
        deadline: &Deadline,
    ) -> NativeResult<(MacPayload, SupportProof)> {
        self.main.revalidate(&self.io)?;
        let expected = installed.unwrap_or(&self.main);
        expected.revalidate(&self.io)?;
        let payload = MacPayload::admit(self.io.clone(), self.inventory.clone(), deadline)?;
        let main =
            self.io
                .admit_main_signature(expected.path(), expected.requirement(), deadline)?;
        if main.observation() != expected.observation()
            || payload.manifest_sha256() != self.payload.manifest_sha256()
        {
            return Err(NativeError::Foreign);
        }
        let support = Self::bound_support(&self.io, &main, &plan.session, deadline)?;
        expected.revalidate(&self.io)?;
        if installed.is_none() && self.io.metadata(&self.io.target().agent_path())?.is_some() {
            return Err(NativeError::Foreign);
        }
        Ok((payload, support))
    }
    pub(super) fn bound_support(
        io: &MacNativeIo,
        main: &SignatureProof,
        session: &str,
        deadline: &Deadline,
    ) -> NativeResult<SupportProof> {
        let support = io.admit_support(main, deadline)?;
        if io.support_observation(deadline)?.gui.console_session != session {
            return Err(NativeError::Foreign);
        }
        support.check(io, deadline)?;
        Ok(support)
    }
    fn record_origin(
        &self,
        snapshot: &Snapshot,
        deadline: &Deadline,
    ) -> NativeResult<Option<ReceiptOrigin>> {
        let path = Self::record(&self.io);
        let Some(identity) = self.io.metadata(&path)? else {
            return Ok(None);
        };
        if snapshot.bytes != self.xml {
            return Ok(None);
        }
        let bytes = self.io.read(&path, 512 * 1024, true, deadline)?;
        let record: Record = serde_json::from_slice(&bytes).map_err(|_| NativeError::Invalid)?;
        if record.phase == LaunchPhase::Unknown {
            // A timed-out command's phase is not a bootstrap receipt; its uncertain dispatch
            // remains retained. Published/BootstrapRequested can be freshly reassessed below.
            return Err(NativeError::OutcomeUnknown);
        }
        let receipt = &record.receipt;
        let current = receipt.product_version == self.version
            && receipt.manifest_sha256 == self.payload.manifest_sha256();
        if (!current
            && !self.payload.owns_receipt_identity(
                &receipt.product_version,
                receipt.manifest_sha256,
                deadline,
            )?)
            || receipt.schema_version != 1
            || receipt.operation_id.0 == 0
            || receipt.payload_sha256 != digest(&self.xml)
            || receipt.resources.len() != 1
            || receipt.unfinished != vec![StepId(12)]
            || !matches!(
                record.phase,
                LaunchPhase::Published | LaunchPhase::BootstrapRequested | LaunchPhase::Observed
            )
            || record.stop_attempted
            || record.session != self.io.support_observation(deadline)?.gui.console_session
        {
            return Ok(None);
        }
        let row = &receipt.resources[0];
        if row.resource_id != "mac.launch-agent"
            || row.resolved_path != Self::plist(&self.io).to_string_lossy()
            || !matches!(
                (row.ownership, row.before),
                (ResourceOwnership::Created, ResourceObservation::Absent)
                    | (ResourceOwnership::Adopted, ResourceObservation::Different)
            )
            || row.after != ResourceObservation::Matching
            || row.outcome != MutationOutcome::Unknown
            || self.io.metadata(&path)? != Some(identity.clone())
        {
            return Ok(None);
        }
        let (ownership, before) = (row.ownership, row.before);
        Ok(Some((identity, bytes, ownership, before)))
    }
}
pub(super) fn checked_reply(
    io: &MacNativeIo,
    selected: &SelectedAgent,
    reply: &AgentReply,
    deadline: &Deadline,
) -> NativeResult<()> {
    let now = selected.io.clock().now_ms();
    if selected.io.target().paths() != io.target().paths()
        || reply.source != selected.io.target().source()
        || reply.observed_at_ms > now
        || now - reply.observed_at_ms > SUPPORT_LIFETIME_MS
    {
        return Err(NativeError::Foreign);
    }
    selected
        .instance
        .revalidate(&selected.io, &selected.support, deadline)?;
    let DecodedReply::Status(StatusAdmission::Supported(health)) = reply
        .result
        .as_ref()
        .map_err(|_| NativeError::Unavailable)?
    else {
        return Err(NativeError::Unavailable);
    };
    selected.instance.admit_status(&health.installer().instance)
}
fn query_job(io: &MacNativeIo, output: &CommandOutput) -> NativeResult<Job> {
    let plist = MacLaunchAgent::plist(io);
    let program = io.target().agent_path();
    Ok(super::super::launchd_observation::job(
        output.code,
        &output.stdout,
        &output.stderr,
        super::super::launchd_observation::SelectedJob {
            uid: io.target().paths().uid,
            label: AGENT_LABEL,
            plist: &plist,
            program: &program,
        },
    ))
}
fn query_disabled(output: &CommandOutput) -> NativeResult<Disabled> {
    Ok(
        match super::super::launchd_observation::disabled(
            output.code,
            &output.stdout,
            &output.stderr,
            AGENT_LABEL,
        ) {
            Some(true) => Disabled::Yes,
            Some(false) => Disabled::No,
            None => Disabled::Unknown,
        },
    )
}

impl MacLaunchAgent {
    /// Receipt-bound full reinstall; run on a detached worker with the caller's shared deadline.
    pub fn plan_repair(
        &mut self,
        revision: u64,
        operation: u64,
        current: Option<(&SelectedAgent, &AgentReply)>,
        deadline: &Deadline,
    ) -> NativeResult<LaunchPlan> {
        // A present app needs the unchanged executor's tracked, live original clean-stop gate.
        if self.io.metadata(&self.io.target().app_path())?.is_some()
            && (current.is_none() || self.io.metadata(&self.io.target().agent_path())?.is_none())
        {
            return Err(NativeError::Refused);
        }
        let mut plan = self.plan(revision, operation, current, deadline)?;
        match plan.state {
            LaunchState::UserDisabled => return Err(NativeError::Unsupported),
            LaunchState::Unobservable | LaunchState::Conflict => {
                return Err(NativeError::Unavailable);
            }
            _ => {}
        }
        if let Some((_, reply)) = current {
            let DecodedReply::Status(StatusAdmission::Supported(health)) = reply
                .result
                .as_ref()
                .map_err(|_| NativeError::Unavailable)?
            else {
                return Err(NativeError::Unavailable);
            };
            let mut actual = health.installer().build.features.clone();
            let mut expected = self.inventory.features.clone();
            actual.sort();
            expected.sort();
            if health.installer().build.version != self.version || actual != expected {
                return Err(NativeError::Refused);
            }
        }
        let path = Self::record(&self.io);
        let identity = self.io.metadata(&path)?.ok_or(NativeError::Refused)?;
        let bytes = self.io.read(&path, 512 * 1024, true, deadline)?;
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| NativeError::Invalid)?;
        let record: Record =
            serde_json::from_value(value.clone()).map_err(|_| NativeError::Invalid)?;
        let operation = record.receipt.operation_id;
        let expected = InstallReceipt {
            schema_version: 1,
            operation_id: operation,
            product_version: self.version.clone(),
            manifest_sha256: self.payload.manifest_sha256(),
            payload_sha256: digest(&self.xml),
            resources: vec![ResourceReceipt {
                resource_id: "mac.launch-agent".into(),
                resolved_path: Self::plist(&self.io).to_string_lossy().into_owned(),
                ownership: ResourceOwnership::Created,
                before: ResourceObservation::Absent,
                after: ResourceObservation::Matching,
                outcome: MutationOutcome::Unknown,
            }],
            unfinished: vec![StepId(12)],
        };
        let prior = self
            .io
            .target()
            .installer_dir()
            .join(format!("launch-agent-prior-{}.plist", operation.0));
        if operation.0 == 0
            || record.phase != LaunchPhase::Observed
            || record.stop_attempted
            || record.session != plan.session
            || record
                .prior
                .as_ref()
                .is_some_and(|p| Some(p.as_str()) != prior.to_str())
            || record.receipt != expected
            || value != serde_json::to_value(&record).map_err(|_| NativeError::Invalid)?
            || (plan.snapshot.identity.is_some() && plan.snapshot.bytes != self.xml)
            || self.io.metadata(&path)? != Some(identity.clone())
        {
            return Err(NativeError::Foreign);
        }
        let origin = (
            identity,
            bytes,
            plan.snapshot.identity.clone(),
            plan.snapshot.bytes.clone(),
        );
        plan.payload = Some(self.payload.plan_repair(
            revision,
            plan.operation,
            plan.original.clone(),
            origin,
            deadline,
        )?);
        plan.matching = false;
        plan.state = if plan.snapshot.identity.is_some() {
            LaunchState::Owned
        } else {
            LaunchState::Absent
        };
        Ok(plan)
    }
    pub(crate) fn revalidate_repair(
        &self,
        plan: &LaunchPlan,
        current: Option<(&SelectedAgent, &AgentReply)>,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        if !Arc::ptr_eq(&self.owner, &plan.owner)
            || (plan.revision, plan.operation) != self.last
            || self.snapshot(&self.io, deadline)? != plan.snapshot
        {
            return Err(NativeError::Foreign);
        }
        if let Some(expected) = &plan.selected {
            let (selected, reply) = current.ok_or(NativeError::Unavailable)?;
            checked_reply(&self.io, selected, reply, deadline)?;
            if selected.instance.bootstrap().instance_id
                != expected.instance.bootstrap().instance_id
                || selected.instance.process() != expected.instance.process()
            {
                return Err(NativeError::Foreign);
            }
        } else if current.is_some() {
            return Err(NativeError::Foreign);
        }
        self.payload
            .check_repair_plan(plan.payload.as_ref().ok_or(NativeError::Invalid)?, deadline)?;
        self.refreshed(plan, plan.installed_main.as_ref(), deadline)?;
        deadline.check()
    }
}
