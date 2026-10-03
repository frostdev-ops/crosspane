//! Exact scoped actions, real C1 clean-stop/payload flow, and correlated startup facts.
use super::*;
impl MacLaunchAgent {
    pub fn execute(
        &self,
        plan: LaunchPlan,
        consent: LaunchConsent,
        deadline: &Deadline,
    ) -> NativeResult<PendingLaunch> {
        if !Arc::ptr_eq(&self.owner, &plan.owner)
            || !Arc::ptr_eq(&plan.binding, &consent.binding)
            || (plan.revision, plan.operation) != self.last
            || self.snapshot(&self.io, deadline)? != plan.snapshot
        {
            return Err(NativeError::Foreign);
        }
        let (payload, support) = self.refreshed(&plan, plan.installed_main.as_ref(), deadline)?;
        self.preflight(deadline)?;
        self.parents(&self.io.target().installer_dir(), &support, deadline)?;
        let mut pending = PendingLaunch {
            plan,
            consent,
            phase: LaunchPhase::Intent,
            error: None,
            payload: None,
            requested: false,
            stop_attempted: false,
            requested_at: 0,
            admission_refused: false,
            health_call: None,
            last_health: 0,
            prior: None,
        };
        let lock = self.io.lock(&support, deadline)?;
        self.persist(&pending, &self.io, &support, deadline)?;
        drop(lock);
        self.advance(&mut pending, &payload, &support, deadline);
        Ok(pending)
    }
    /// Rechecks actual original-process exit and the real C1 receipt; never reissues bootout.
    pub fn resume_clean_stop(
        &self,
        pending: &mut PendingLaunch,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        if (pending.plan.revision, pending.plan.operation) != self.last {
            return Err(NativeError::Foreign);
        }
        if (pending.phase != LaunchPhase::WaitingForCleanStop
            && !(pending.phase == LaunchPhase::Unknown && pending.stop_attempted))
            || !Arc::ptr_eq(&self.owner, &pending.plan.owner)
        {
            return Err(NativeError::Invalid);
        }
        let (payload, support) = self.refreshed(
            &pending.plan,
            pending.plan.installed_main.as_ref(),
            deadline,
        )?;
        if pending.phase == LaunchPhase::Unknown {
            let original = pending.plan.original.as_ref().ok_or(NativeError::Invalid)?;
            if CleanStopGate::observe(original.clone(), deadline)?.is_none() {
                return Err(NativeError::Unavailable);
            }
            pending.phase = LaunchPhase::WaitingForCleanStop;
        }
        self.advance(pending, &payload, &support, deadline);
        Ok(())
    }
    fn advance(
        &self,
        pending: &mut PendingLaunch,
        payload: &MacPayload,
        support: &SupportProof,
        deadline: &Deadline,
    ) {
        if let Err(error) = self.apply(pending, payload, support, deadline) {
            pending.phase = LaunchPhase::Unknown;
            pending.error = Some(error);
            if !pending.admission_refused {
                let _ = self.persist(pending, &self.io, support, deadline);
            }
        } else {
            pending.error = None;
        }
    }
    fn apply(
        &self,
        pending: &mut PendingLaunch,
        payload: &MacPayload,
        support: &SupportProof,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        support.check(&self.io, deadline)?;
        self.preflight(deadline)?;
        if pending.phase == LaunchPhase::Intent
            && self.snapshot(&self.io, deadline)? != pending.plan.snapshot
        {
            return Err(NativeError::Foreign);
        }
        if let Some(original) = &pending.plan.original {
            if pending.phase == LaunchPhase::Intent {
                let selected = pending.plan.selected.as_ref().ok_or(NativeError::Invalid)?;
                pending.admission_refused = true;
                let main = selected.io.admit_main_signature(
                    &selected.io.target().agent_path(),
                    &self.requirement,
                    deadline,
                )?;
                let expected = pending
                    .plan
                    .installed_main
                    .as_ref()
                    .ok_or(NativeError::Invalid)?;
                if main.observation() != expected.observation() {
                    return Err(NativeError::Foreign);
                }
                let selected_support =
                    Self::bound_support(&selected.io, &main, &pending.plan.session, deadline)?;
                selected
                    .instance
                    .revalidate(&selected.io, &selected_support, deadline)?;
                pending.admission_refused = false;
                pending.stop_attempted = true;
                let output = Self::command(
                    &self.io,
                    NativeOperation::Launchctl(LaunchctlAction::Bootout),
                    Some(support),
                    deadline,
                )?;
                if output.code != Some(0) {
                    return Err(NativeError::Refused);
                }
                pending.phase = LaunchPhase::WaitingForCleanStop;
                self.persist(pending, &self.io, support, deadline)?;
            }
            if CleanStopGate::observe(original.clone(), deadline)?.is_none() {
                return Ok(());
            }
        }
        let gate = pending
            .plan
            .original
            .as_ref()
            .map(|o| CleanStopGate::observe(o.clone(), deadline))
            .transpose()?
            .flatten();
        pending.stop_attempted = false;
        pending.payload = payload.install(
            pending.plan.payload.take().ok_or(NativeError::Invalid)?,
            pending.consent.payload.take().ok_or(NativeError::Invalid)?,
            gate.as_ref(),
            deadline,
        )?;
        pending.admission_refused = true;
        let main = self.io.admit_main_signature(
            &self.io.target().agent_path(),
            &self.requirement,
            deadline,
        )?;
        let support = Self::bound_support(&self.io, &main, &pending.plan.session, deadline)?;
        pending.admission_refused = false;
        self.parents(
            Self::plist(&self.io).parent().ok_or(NativeError::Invalid)?,
            &support,
            deadline,
        )?;
        self.parents(
            &self.io.target().paths().home.join("Library/Logs/Crosspane"),
            &support,
            deadline,
        )?;
        let lock = self.io.lock(&support, deadline)?;
        let current = self.snapshot(&self.io, deadline)?;
        if current.identity != pending.plan.snapshot.identity
            || current.bytes != pending.plan.snapshot.bytes
            || current.disabled != Disabled::No
            || current.job != Job::Absent
        {
            return Err(NativeError::Foreign);
        }
        if current.identity.is_some() {
            let prior = self.io.target().installer_dir().join(format!(
                "launch-agent-prior-{}.plist",
                pending.plan.operation
            ));
            if self.io.metadata(&prior)?.is_some() {
                return Err(NativeError::Foreign);
            }
            self.io
                .atomic_write(&support, &prior, &current.bytes, None, deadline)?;
            pending.prior = Some(prior);
            self.persist(pending, &self.io, &support, deadline)?;
        }
        self.io.atomic_write(
            &support,
            &Self::plist(&self.io),
            &self.xml,
            current.identity.as_ref(),
            deadline,
        )?;
        let identity = self
            .io
            .metadata(&Self::plist(&self.io))?
            .ok_or(NativeError::Foreign)?;
        let lint = Self::command(
            &self.io,
            NativeOperation::ValidatePlist {
                path: Self::plist(&self.io),
            },
            None,
            deadline,
        )?;
        if lint.code != Some(0)
            || self.io.metadata(&Self::plist(&self.io))? != Some(identity.clone())
        {
            return Err(NativeError::Foreign);
        }
        pending.phase = LaunchPhase::Published;
        self.persist(pending, &self.io, &support, deadline)?;
        pending.admission_refused = true;
        if let Some(payload) = &pending.payload {
            if payload.phase() != PayloadPhase::Published {
                return Err(NativeError::Refused);
            }
        } else {
            let current = MacPayload::admit(self.io.clone(), self.inventory.clone(), deadline)?;
            if !pending.plan.matching
                || current.manifest_sha256() != self.payload.manifest_sha256()
                || current
                    .plan(
                        pending.plan.revision,
                        pending.plan.operation,
                        pending.plan.original.clone(),
                        deadline,
                    )?
                    .state()
                    != PayloadState::Matching
            {
                return Err(NativeError::Refused);
            }
        }
        // Genuine C1 Published token belongs to this object's install flow, not a disk receipt.
        let (_, support) = self.refreshed(&pending.plan, Some(&main), deadline)?;
        if self.io.metadata(&Self::plist(&self.io))? != Some(identity)
            || self.snapshot(&self.io, deadline)?.disabled != Disabled::No
        {
            return Err(NativeError::Foreign);
        }
        pending.admission_refused = false;
        pending.phase = LaunchPhase::BootstrapRequested;
        pending.requested = true;
        pending.requested_at = self.io.clock().now_ms();
        self.persist(pending, &self.io, &support, deadline)?;
        drop(lock);
        let result = Self::command(
            &self.io,
            NativeOperation::Launchctl(LaunchctlAction::Bootstrap),
            Some(&support),
            deadline,
        )?;
        if result.code != Some(0) {
            return Err(NativeError::Refused);
        }
        Ok(())
    }
    pub fn observe(
        &self,
        pending: &mut PendingLaunch,
        selected: &SelectedAgent,
        reply: AgentReply,
        returned_revision: Option<&str>,
        deadline: &Deadline,
    ) -> NativeResult<StartupFacts> {
        if !pending.requested
            || pending.health_call != Some(reply.id)
            || reply.observed_at_ms < pending.requested_at
            || !Arc::ptr_eq(&self.owner, &pending.plan.owner)
        {
            return Err(NativeError::Invalid);
        }
        checked_reply(&self.io, selected, &reply, deadline)?;
        if pending.plan.baseline == Some(selected.instance.bootstrap().instance_id) {
            return Err(NativeError::Refused);
        }
        let snapshot = self.snapshot(&selected.io, deadline)?;
        if snapshot.bytes != self.xml
            || snapshot.job != Job::Running(selected.instance.process().pid)
        {
            return Err(NativeError::Foreign);
        }
        let approval = self.approval.observe(selected.io.target(), deadline)?;
        selected
            .instance
            .revalidate(&selected.io, &selected.support, deadline)?;
        let session = selected
            .io
            .support_observation(deadline)?
            .gui
            .console_session;
        let verified = if let Some(payload) = &mut pending.payload {
            Some(
                self.payload
                    .verify(payload, selected, &reply, returned_revision, deadline)?,
            )
        } else {
            None
        };
        if verified.is_some() {
            pending.payload = None;
        }
        pending.health_call = None;
        pending.phase = LaunchPhase::Observed;
        pending.error = None;
        self.persist(pending, &selected.io, &selected.support, deadline)?;
        Ok(StartupFacts {
            reply,
            approval,
            disabled: snapshot.disabled,
            login: if session == pending.plan.session {
                LoginEvidence::SameSession
            } else {
                LoginEvidence::DifferentInteractiveSession
            },
            payload_verified: verified,
            retained_prior: pending.prior.clone(),
        })
    }
}
