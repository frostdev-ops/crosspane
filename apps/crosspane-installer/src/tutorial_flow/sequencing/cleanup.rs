use super::*;

impl Tutorial {
    pub(super) fn finish(
        &mut self,
        effects: &mut Vec<TutorialEffect>,
    ) -> Result<(), TutorialError> {
        let s = self.session()?;
        if s.retired || self.state == TutorialState::Verified || s.end.is_none() {
            return Ok(());
        }
        let role = s.binding.attempt.role;
        if !confirmations(role).iter().all(|c| s.human.contains(c)) {
            return Ok(());
        }
        if role == TutorialRole::AudioSender && s.tone.is_some() {
            return Ok(());
        }
        if role == TutorialRole::E1Target
            && !s.fixture.as_ref().is_some_and(|f| f.clicked)
            && !s.fixture_proved
        {
            return Ok(());
        }
        if source_role(role)
            && !s.fixture.as_ref().is_some_and(|f| f.home_at.is_some())
            && !s.fixture_proved
        {
            return Ok(());
        }
        if source_role(role)
            && (self.detail.parking == ParkingResult::Unknown
                || (s.context.source_policy == TutorialSourcePolicy::MacPrivateDisplay
                    && (self.detail.parking != ParkingResult::Twin
                        || !s.human.contains(&HumanConfirmation::PrivateDisplayObserved))))
        {
            self.state = TutorialState::PendingContract;
            return Ok(());
        }
        self.session_mut()?.fixture_proved = true;
        self.session_mut()?.stage = Stage::Cleaning;
        self.cleanup(effects)?;
        if self.session()?.fixture.is_some()
            || self.session()?.tone.is_some()
            || self.session()?.projection.is_some()
        {
            return Ok(());
        }
        let s = self.session()?;
        let start = s
            .start
            .as_ref()
            .ok_or(TutorialError::InsufficientContract)?;
        let end = s.end.as_ref().ok_or(TutorialError::InsufficientContract)?;
        let (advance, forbidden) = requirements(role);
        let proof = check_activity(
            s.binding.attempt.attempt,
            start,
            end,
            dependencies(role),
            &advance,
            &forbidden,
        )
        .map_err(|_| TutorialError::InvalidObservation)?;
        effects.push(self.effect(TutorialEffectKind::Core(FlowEvent::Verified {
            step: s.binding.attempt.step,
            operation: s.binding.attempt.operation,
            verification: Verification {
                source: end.source,
                observed_at_ms: end.observed_at_ms,
                binding: Some(end.binding.clone()),
                activity: Some(proof),
                human_attempt: Some(s.binding.attempt.attempt),
                fixture_attempt: uses_fixture(role).then_some(s.binding.attempt.attempt),
            },
        }))?);
        self.detail.local_cleanup_settled = true;
        if !source_role(role) && e2_role(role) {
            self.detail.remote_restoration = RemoteRestoration::HumanConfirmed;
        }
        self.detail.cleanup_settled = true;
        self.session_mut()?.projection_cleanup = None;
        self.detail.private_display_verified = source_role(role)
            && self.session()?.context.source_policy == TutorialSourcePolicy::MacPrivateDisplay
            && self.detail.parking == ParkingResult::Twin;
        self.state = TutorialState::Verified;
        Ok(())
    }
    pub(super) fn fail(
        &mut self,
        failure: TutorialFailure,
        effects: &mut Vec<TutorialEffect>,
    ) -> Result<(), TutorialError> {
        if failure == TutorialFailure::Fixture(TutorialFixtureError::NotOwned) {
            self.session_mut()?.ownership_lost = true;
        }
        self.detail.failure.get_or_insert(failure);
        self.retire(TutorialState::Failed, effects)
    }
    pub(super) fn retire(
        &mut self,
        state: TutorialState,
        effects: &mut Vec<TutorialEffect>,
    ) -> Result<(), TutorialError> {
        let step = self.session()?.binding.attempt.step;
        self.session_mut()?.retired = true;
        if self.session()?.retired_at == 0 {
            self.session_mut()?.retired_at = self.now;
        }
        self.session_mut()?.start = None;
        self.session_mut()?.end = None;
        self.state = state;
        self.tracked.remove(&step);
        effects.push(self.effect(TutorialEffectKind::Core(FlowEvent::Invalidate { step }))?);
        self.cleanup(effects)
    }
    pub(super) fn detect(
        &mut self,
        effects: &mut Vec<TutorialEffect>,
    ) -> Result<(), TutorialError> {
        if !self
            .agent_calls
            .values()
            .any(|(r, _)| *r == InstallerRequest::Status)
        {
            self.call(InstallerRequest::Status, effects)?;
        }
        Ok(())
    }
    pub(super) fn prepare_projection_cleanup(&mut self) -> Result<(), TutorialError> {
        let s = self.session()?;
        if s.projection_cleanup.is_none() {
            let start = s.start.as_ref().ok_or(TutorialError::InvalidObservation)?;
            let source = source_role(s.binding.attempt.role);
            self.session_mut()?.projection_cleanup = Some(ProjectionCleanup {
                started: start.values[&CounterId(if source {
                    Metric::SourceStarted
                } else {
                    Metric::DestStarted
                } as u16)],
                returned: start.values[&CounterId(if source {
                    Metric::SourceReturned
                } else {
                    Metric::DestReturned
                } as u16)],
                failed: start.values[&CounterId(Metric::ReturnsFailed as u16)],
                terminal_at: None,
            });
            if !source {
                self.detail.remote_restoration = RemoteRestoration::Unknown;
            }
        }
        Ok(())
    }
    pub(super) fn discard_unsubmitted(&mut self) -> Result<(), TutorialError> {
        let agents: Vec<_> = self
            .agent_calls
            .keys()
            .filter(|id| !self.submitted.contains(id))
            .copied()
            .collect();
        for id in agents {
            if let Some((request, _)) = self.agent_calls.remove(&id) {
                match request {
                    InstallerRequest::Release => self.session_mut()?.release_sent = false,
                    InstallerRequest::Return { .. } => self.session_mut()?.return_sent = false,
                    InstallerRequest::Project { .. } | InstallerRequest::Pull { .. } => {
                        self.session_mut()?.projection_unknown = false;
                        self.session_mut()?.projection_cleanup = None;
                        self.detail.remote_restoration = RemoteRestoration::NotApplicable;
                    }
                    _ => {}
                }
            }
        }
        let fixtures: Vec<_> = self
            .fixture_calls
            .keys()
            .filter(|id| !self.submitted.contains(id))
            .copied()
            .collect();
        for id in fixtures {
            self.fixture_sent.remove(&id);
            match self.fixture_calls.remove(&id) {
                Some(TutorialFixtureAction::PlayTone { .. }) => {
                    self.session_mut()?.tone_unknown = false
                }
                Some(TutorialFixtureAction::StopTone { .. }) => {
                    self.session_mut()?.stop_sent = false
                }
                Some(TutorialFixtureAction::Close { .. }) => self.session_mut()?.close_sent = false,
                _ => {}
            }
        }
        Ok(())
    }
    pub(super) fn return_owned(
        &mut self,
        effects: &mut Vec<TutorialEffect>,
    ) -> Result<(), TutorialError> {
        if !self.session()?.return_sent
            && self.cleanup_correlated() == Some(true)
            && let Some(p) = self.session()?.projection.clone()
        {
            self.session_mut()?.return_sent = true;
            self.call(
                InstallerRequest::Return {
                    projection: p.projection,
                    source: (p.source != self.session()?.binding.attempt.local).then_some(p.source),
                },
                effects,
            )?;
        }
        Ok(())
    }
    pub(super) fn cleanup(
        &mut self,
        effects: &mut Vec<TutorialEffect>,
    ) -> Result<(), TutorialError> {
        let correlation = self.cleanup_correlated();
        if self.session()?.projection_cleanup.is_some() && correlation != Some(true) {
            if correlation == Some(false) {
                self.session_mut()?.ownership_lost = true;
                self.state = TutorialState::Failed;
                self.detail
                    .failure
                    .get_or_insert(TutorialFailure::BindingChanged);
                if self.detail.remote_restoration == RemoteRestoration::HumanConfirmed {
                    self.detail.remote_restoration = RemoteRestoration::Unknown;
                }
            }
            self.detail.local_cleanup_settled = false;
            self.detail.cleanup_settled = false;
            return Ok(());
        }
        if self.session()?.retired
            && (self.session()?.projection_unknown || self.session()?.tone_unknown)
        {
            effects.push(self.effect(TutorialEffectKind::DetectAfterUnknown)?);
            self.detect(effects)?;
        }
        if self.session()?.retired && !self.session()?.release_sent {
            self.session_mut()?.release_sent = true;
            self.call(InstallerRequest::Release, effects)?;
        }
        self.return_owned(effects)?;
        if self.session()?.ownership_lost {
            return Ok(());
        }
        if self.session()?.tone_unknown
            && !self.fixture_calls.values().any(|a| {
                matches!(
                    a,
                    TutorialFixtureAction::PlayTone { .. }
                        | TutorialFixtureAction::ObserveWindow { .. }
                )
            })
            && let Some(fixture) = self.session()?.fixture.as_ref().map(|f| f.id)
        {
            self.fixture_call(TutorialFixtureAction::ObserveWindow { fixture }, effects)?;
        }
        if let Some(tone) = self.session()?.tone {
            if !self.session()?.stop_sent {
                let fixture = self
                    .session()?
                    .fixture
                    .as_ref()
                    .ok_or(TutorialError::InvalidObservation)?
                    .id;
                self.session_mut()?.stop_sent = true;
                self.fixture_call(TutorialFixtureAction::StopTone { fixture, tone }, effects)?;
            }
        } else if let Some(fixture) = self.session()?.fixture.as_ref().map(|f| f.id)
            && !self.session()?.close_sent
            && self.session()?.projection.is_none()
            && !self.session()?.tone_unknown
            && !self.session()?.projection_unknown
            && !self.fixture_calls.values().any(|a| {
                matches!(
                    a,
                    TutorialFixtureAction::Open { .. }
                        | TutorialFixtureAction::ArmTarget { .. }
                        | TutorialFixtureAction::PlayTone { .. }
                )
            })
            && (self.session()?.projection_cleanup.is_none() || self.session()?.fixture_proved)
        {
            self.session_mut()?.close_sent = true;
            self.fixture_call(TutorialFixtureAction::Close { fixture }, effects)?;
        }
        Ok(())
    }
    pub(super) fn settle_cleanup(
        &mut self,
        effects: &mut Vec<TutorialEffect>,
    ) -> Result<(), TutorialError> {
        if self.session()?.retired && self.terminal() && !self.session()?.ownership_lost {
            if self.samples.first().is_none_or(|s| {
                s.observed_at_ms < self.session.as_ref().map_or(u64::MAX, |s| s.retired_at)
            }) {
                return Ok(());
            }
            self.session_mut()?.projection = None;
            if self.session()?.projection_cleanup.is_some() {
                let s = self.session()?;
                let owned = s
                    .projection_cleanup
                    .as_ref()
                    .ok_or(TutorialError::InvalidObservation)?;
                let i = self
                    .health
                    .as_ref()
                    .ok_or(TutorialError::InsufficientContract)?
                    .installer();
                let current = i
                    .peers
                    .iter()
                    .find(|p| Some(p.node) == s.binding.attempt.peer);
                let at = self
                    .samples
                    .first()
                    .ok_or(TutorialError::InvalidObservation)?
                    .observed_at_ms;
                if at < s.retired_at
                    || s.unowned_projection.is_some()
                    || self.agent_calls.values().any(|(r, _)| {
                        matches!(
                            r,
                            InstallerRequest::Project { .. } | InstallerRequest::Pull { .. }
                        )
                    })
                {
                    return Ok(());
                }
                if current.is_none_or(|p| p.counters.e2_returns_failed != owned.failed) {
                    self.state = TutorialState::Failed;
                    self.detail.failure.get_or_insert(TutorialFailure::Fixture(
                        TutorialFixtureError::CleanupFailed,
                    ));
                    return Ok(());
                }
                let source = source_role(s.binding.attempt.role);
                let started = current.map(|p| {
                    if source {
                        p.counters.e2_source_started
                    } else {
                        p.counters.e2_dest_started
                    }
                });
                let returned = current.map(|p| {
                    if source {
                        p.counters.e2_source_returned
                    } else {
                        p.counters.e2_dest_returned
                    }
                });
                if started != owned.started.checked_add(1)
                    || returned.is_none_or(|r| r <= owned.returned)
                {
                    return Ok(());
                }
                self.session_mut()?.projection_unknown = false;
                if !source {
                    self.session_mut()?.fixture_proved = true;
                    self.session_mut()?
                        .projection_cleanup
                        .as_mut()
                        .ok_or(TutorialError::InvalidObservation)?
                        .terminal_at
                        .get_or_insert(at);
                    self.cleanup(effects)?;
                } else {
                    let owned = self
                        .session()?
                        .projection_cleanup
                        .as_ref()
                        .ok_or(TutorialError::InvalidObservation)?;
                    let terminal_at = owned.terminal_at.unwrap_or(at);
                    self.session_mut()?
                        .projection_cleanup
                        .as_mut()
                        .ok_or(TutorialError::InvalidObservation)?
                        .terminal_at = Some(terminal_at);
                    if !self.session()?.fixture_proved
                        && !self
                            .session()?
                            .fixture
                            .as_ref()
                            .is_some_and(|f| f.home_at.is_some_and(|home| home >= terminal_at))
                    {
                        if let Some(fixture) = self.session()?.fixture.as_ref().map(|f| f.id)
                            && !self
                                .fixture_calls
                                .values()
                                .any(|a| matches!(a, TutorialFixtureAction::ObserveWindow { .. }))
                        {
                            self.fixture_call(
                                TutorialFixtureAction::ObserveWindow { fixture },
                                effects,
                            )?;
                        }
                        return Ok(());
                    }
                    self.session_mut()?.fixture_proved = true;
                }
            }
            self.cleanup(effects)?;
            self.detail.local_cleanup_settled = self.session()?.fixture.is_none()
                && self.session()?.tone.is_none()
                && self.fixture_calls.is_empty()
                && !self.session()?.tone_unknown
                && !self.session()?.projection_unknown
                && !self.session()?.ownership_lost;
            self.detail.cleanup_settled = self.detail.local_cleanup_settled
                && self.detail.remote_restoration != RemoteRestoration::Unknown;
            if self.detail.cleanup_settled {
                self.session_mut()?.projection_cleanup = None;
            }
        }
        Ok(())
    }
    pub(super) fn cleanup_correlated(&self) -> Option<bool> {
        let s = self.session.as_ref()?;
        let bound = s.binding.evidence.as_ref()?;
        let current = self.samples.iter().find(|p| p.binding.peer == bound.peer)?;
        Some(
            !s.ownership_lost
                && current.binding.local_node == bound.local_node
                && current.binding.epochs.instance_id == bound.epochs.instance_id
                && current.binding.link_generation == bound.link_generation,
        )
    }
    pub(super) fn confirm_remote_restoration(
        &mut self,
        effects: &mut Vec<TutorialEffect>,
    ) -> Result<(), TutorialError> {
        let s = self.session()?;
        let current = self
            .samples
            .iter()
            .find(|sample| sample.binding.peer == s.binding.attempt.peer);
        if !self.detail.local_cleanup_settled
            || self.detail.remote_restoration != RemoteRestoration::Unknown
            || !self.fresh()
            || !self.terminal()
            || s.projection_cleanup
                .as_ref()
                .and_then(|c| c.terminal_at)
                .is_none_or(|at| self.now <= at)
            || current.is_none_or(|sample| {
                s.binding.evidence.as_ref().is_none_or(|binding| {
                    !same_binding(
                        binding,
                        &sample.binding,
                        dependencies(s.binding.attempt.role),
                    )
                })
            })
        {
            return Err(TutorialError::InvalidObservation);
        }
        self.detail.remote_restoration = RemoteRestoration::HumanConfirmed;
        self.settle_cleanup(effects)
    }
}
