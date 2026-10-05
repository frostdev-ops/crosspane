use crate::agent_contract::*;
use crosspane_installer_core::{CounterSample, FlowEvent, StepId};
use crosspane_types::id::NodeId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingsTransitionState {
    NeedsDetection,
    NeedsConsent,
    Updating,
    NeedsRestartConsent,
    NeedsRecoveryRestartConsent,
    Restarting,
    WaitingNewInstance,
    Complete,
    Failed(CallFailure),
}
/// Reduce observations before using the setting fact or dispatching detection/retry consent.
#[derive(Clone, Debug, PartialEq)]
pub struct SettingsOutcome {
    pub observations: Vec<FlowEvent>,
    pub detect_after_unknown: bool,
}
/// Application uses a single monotonic call allocator across settings runs.
/// Native disk detection and consent are separate from the loaded revision reported by status.
pub struct SettingsTransition {
    local: NodeId,
    view: u64,
    instance: Option<u64>,
    expected: Option<String>,
    returned: Option<String>,
    pending: Option<AgentCall>,
    last_call: u64,
    at: u64,
    state: SettingsTransitionState,
    tracked_peers: Vec<NodeId>,
    samples: Vec<CounterSample>,
    reload_needed: bool,
    restart_uncertain: bool,
    last_error: Option<CallFailure>,
}
impl std::fmt::Debug for SettingsTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SettingsTransition")
    }
}
impl SettingsTransition {
    pub fn new(local: NodeId, view_revision: u64) -> Self {
        Self {
            local,
            view: view_revision,
            instance: None,
            expected: None,
            returned: None,
            pending: None,
            last_call: 0,
            at: 0,
            state: SettingsTransitionState::NeedsDetection,
            tracked_peers: Vec::new(),
            samples: Vec::new(),
            reload_needed: false,
            restart_uncertain: false,
            last_error: None,
        }
    }
    pub fn state(&self) -> &SettingsTransitionState {
        &self.state
    }
    pub fn returned_revision(&self) -> Option<&str> {
        self.returned.as_deref()
    }
    pub fn view_revision(&self) -> u64 {
        self.view
    }
    pub fn last_error(&self) -> Option<&CallFailure> {
        self.last_error.as_ref()
    }
    pub fn current_samples(&self) -> &[CounterSample] {
        &self.samples
    }
    pub fn last_call_id(&self) -> u64 {
        self.last_call
    }
    /// Pass the selected scopes still tracked by core; never infer them from the trust set.
    pub fn track_peers(&mut self, peers: &[NodeId]) -> Result<(), CallFailure> {
        if peers.len() > MAX_ITEMS || peers.contains(&self.local) {
            return Err(CallFailure::InvalidResponse);
        }
        self.tracked_peers = peers.to_vec();
        self.tracked_peers.sort();
        self.tracked_peers.dedup();
        Ok(())
    }
    fn observation(
        &self,
        health: &HealthSnapshot,
        source: ObservationSource,
        at: u64,
    ) -> Result<Vec<FlowEvent>, CallFailure> {
        let mut samples = vec![
            counter_sample(health, None, source, at)
                .map_err(CallFailure::InvalidCall)?
                .sample,
        ];
        for peer in &self.tracked_peers {
            match counter_sample(health, Some(*peer), source, at) {
                Ok(normalized) => samples.push(normalized.sample),
                Err(ContractError::UnknownPeer | ContractError::DisconnectedPeer) => {}
                Err(error) => return Err(CallFailure::InvalidCall(error)),
            }
        }
        Ok(vec![FlowEvent::Observe { samples }])
    }
    /// After conflict/unknown outcome, re-detect and give the replacement plan a new view.
    /// This is loaded-instance detection; an unloaded disk edit requires an ordinary restart
    /// before consent to a plan based on the newly loaded revision.
    /// Fresh facts still emit Observe when a same-view replacement retires the consent plan;
    /// NeedsDetection and last_error then require a new view before any mutation.
    pub fn detected(
        &mut self,
        health: &HealthSnapshot,
        source: ObservationSource,
        at: u64,
        now: u64,
        view_revision: u64,
    ) -> Result<Vec<FlowEvent>, CallFailure> {
        if self.pending.is_some()
            || source != ObservationSource::Live
            || at > now
            || at < self.at
            || health.installer().node != self.local
            || view_revision < self.view
            || !matches!(
                self.state,
                SettingsTransitionState::NeedsDetection
                    | SettingsTransitionState::NeedsConsent
                    | SettingsTransitionState::NeedsRecoveryRestartConsent
            )
        {
            return Err(CallFailure::InvalidResponse);
        }
        let observations = self.observation(health, source, at)?;
        self.samples = match &observations[0] {
            FlowEvent::Observe { samples } => samples.clone(),
            _ => Vec::new(),
        };
        self.at = at;
        if self.expected.is_some()
            && view_revision == self.view
            && (self.instance != Some(health.installer().instance.id)
                || self.expected.as_deref() != Some(health.installer().config_revision.as_str()))
        {
            self.expected = None;
            self.state = SettingsTransitionState::NeedsDetection;
            self.view = self
                .view
                .checked_add(1)
                .ok_or(CallFailure::InvalidResponse)?;
            self.last_error = Some(CallFailure::InvalidResponse);
            return Ok(observations);
        }
        self.view = view_revision;
        self.instance = Some(health.installer().instance.id);
        self.expected = Some(health.installer().config_revision.clone());
        self.returned = None;
        self.state = if self.reload_needed {
            SettingsTransitionState::NeedsRecoveryRestartConsent
        } else {
            SettingsTransitionState::NeedsConsent
        };
        Ok(observations)
    }
    pub fn consent_update(
        &mut self,
        call_id: u64,
        view_revision: u64,
        mac_virtual_display: bool,
    ) -> Result<AgentCall, CallFailure> {
        if self.state != SettingsTransitionState::NeedsConsent || view_revision != self.view {
            return Err(CallFailure::InvalidResponse);
        }
        let expected_revision = self.expected.clone().ok_or(CallFailure::InvalidResponse)?;
        let call = self.call(
            call_id,
            InstallerRequest::SettingsUpdate {
                expected_revision,
                mac_virtual_display,
            },
        )?;
        self.state = SettingsTransitionState::Updating;
        Ok(call)
    }
    /// The application reduces Observe, then these invalidations, before sending Restart.
    /// Supply every still-tracked step; these are not persisted receipt identifiers.
    pub fn consent_restart(
        &mut self,
        call_id: u64,
        view_revision: u64,
        tracked_steps: &[StepId],
        observations: &[CounterSample],
    ) -> Result<(Vec<FlowEvent>, AgentCall), CallFailure> {
        if !matches!(
            self.state,
            SettingsTransitionState::NeedsRestartConsent
                | SettingsTransitionState::NeedsRecoveryRestartConsent
        ) || view_revision != self.view
            || observations != self.samples
        {
            return Err(CallFailure::InvalidResponse);
        }
        let call = self.call(call_id, InstallerRequest::Restart)?;
        let mut events = vec![FlowEvent::Observe {
            samples: observations.to_vec(),
        }];
        events.extend(
            tracked_steps
                .iter()
                .map(|step| FlowEvent::Invalidate { step: *step }),
        );
        self.state = SettingsTransitionState::Restarting;
        self.restart_uncertain = false;
        Ok((events, call))
    }
    pub fn poll_new_instance(&mut self, call_id: u64) -> Result<AgentCall, CallFailure> {
        if self.state != SettingsTransitionState::WaitingNewInstance {
            return Err(CallFailure::InvalidResponse);
        }
        self.call(call_id, InstallerRequest::Status)
    }
    fn call(&mut self, id: u64, request: InstallerRequest) -> Result<AgentCall, CallFailure> {
        if self.pending.is_some() || id <= self.last_call || id == u64::MAX {
            return Err(CallFailure::InvalidResponse);
        }
        let call = AgentCall {
            id,
            request,
            timeout_ms: MAX_TIMEOUT_MS,
        };
        self.last_call = id;
        self.pending = Some(call.clone());
        Ok(call)
    }
    /// Returns detection intent after conflict/unknown mutation. Never resends a mutation.
    /// A Complete state is only the new instance's loaded-setting fact, not WorkspaceReady.
    pub fn reply(&mut self, reply: AgentReply, now: u64) -> Result<SettingsOutcome, CallFailure> {
        let observations = match &reply.result {
            Ok(DecodedReply::Status(StatusAdmission::Supported(health))) => {
                self.observation(health, reply.source, reply.observed_at_ms)?
            }
            Ok(DecodedReply::Status(StatusAdmission::PendingHealthContract(_))) => {
                vec![FlowEvent::Observe {
                    samples: Vec::new(),
                }]
            }
            _ => Vec::new(),
        };
        let detect_after_unknown = self.reply_inner(reply, now)?;
        if let Some(FlowEvent::Observe { samples }) = observations.first() {
            self.samples = samples.clone();
        }
        Ok(SettingsOutcome {
            observations,
            detect_after_unknown,
        })
    }
    fn reply_inner(&mut self, reply: AgentReply, now: u64) -> Result<bool, CallFailure> {
        let pending = self.pending.as_ref().ok_or(CallFailure::InvalidResponse)?;
        if reply.id != pending.id
            || reply.observed_at_ms > now
            || reply.observed_at_ms < self.at
            || reply.source != ObservationSource::Live
            || matches!(&reply.result, Ok(DecodedReply::Status(StatusAdmission::Supported(h))) if h.installer().node != self.local)
        {
            return Err(CallFailure::InvalidResponse);
        }
        let request = pending.request.clone();
        self.pending = None;
        self.at = reply.observed_at_ms;
        match reply.result {
            Err(error) => {
                self.last_error = Some(error.clone());
                if request == InstallerRequest::Restart || request == InstallerRequest::Status {
                    self.restart_uncertain |= request == InstallerRequest::Restart;
                    self.state = SettingsTransitionState::WaitingNewInstance;
                    return Ok(true);
                }
                let detect = matches!(
                    error,
                    CallFailure::Refused(AgentRefusal::RevisionConflict)
                        | CallFailure::TimeoutOutcomeUnknown
                );
                self.state = if detect {
                    SettingsTransitionState::NeedsDetection
                } else {
                    SettingsTransitionState::Failed(error.clone())
                };
                self.expected = None;
                self.returned = None;
                self.reload_needed = error == CallFailure::Refused(AgentRefusal::RevisionConflict);
                // Retire consent even when the caller supplied the old revision again.
                if detect {
                    self.view = self
                        .view
                        .checked_add(1)
                        .ok_or(CallFailure::InvalidResponse)?;
                }
                Ok(detect)
            }
            Ok(DecodedReply::SettingsUpdated(update))
                if matches!(request, InstallerRequest::SettingsUpdate { .. }) =>
            {
                if !update.restart_required
                    || update.revision.len() != 16
                    || !update
                        .revision
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    self.state = SettingsTransitionState::Failed(CallFailure::InvalidResponse);
                    return Err(CallFailure::InvalidResponse);
                }
                self.returned = Some(update.revision);
                self.state = SettingsTransitionState::NeedsRestartConsent;
                Ok(false)
            }
            Ok(DecodedReply::Acknowledged) if request == InstallerRequest::Restart => {
                self.state = SettingsTransitionState::WaitingNewInstance;
                Ok(false)
            }
            Ok(DecodedReply::Status(StatusAdmission::Supported(health)))
                if request == InstallerRequest::Status =>
            {
                if health.installer().node != self.local {
                    return Err(CallFailure::InvalidResponse);
                }
                if self.instance != Some(health.installer().instance.id)
                    && self.returned.as_deref() == Some(health.installer().config_revision.as_str())
                {
                    self.state = SettingsTransitionState::Complete;
                } else if self.instance != Some(health.installer().instance.id)
                    && self.reload_needed
                {
                    self.reload_needed = false;
                    self.expected = None;
                    self.view = self
                        .view
                        .checked_add(1)
                        .ok_or(CallFailure::InvalidResponse)?;
                    self.state = SettingsTransitionState::NeedsDetection;
                } else if self.instance == Some(health.installer().instance.id)
                    && self.restart_uncertain
                {
                    self.view = self
                        .view
                        .checked_add(1)
                        .ok_or(CallFailure::InvalidResponse)?;
                    self.state = if self.reload_needed {
                        SettingsTransitionState::NeedsRecoveryRestartConsent
                    } else {
                        SettingsTransitionState::NeedsRestartConsent
                    };
                    self.restart_uncertain = false;
                }
                Ok(false)
            }
            Ok(DecodedReply::Status(StatusAdmission::PendingHealthContract(_)))
                if request == InstallerRequest::Status =>
            {
                Ok(false)
            }
            _ => {
                self.state = SettingsTransitionState::Failed(CallFailure::InvalidResponse);
                Err(CallFailure::InvalidResponse)
            }
        }
    }
}
