use super::*;

impl Tutorial {
    pub(super) fn fixture(
        &mut self,
        event: TutorialEvent,
        effects: &mut Vec<TutorialEffect>,
    ) -> Result<(), TutorialError> {
        let TutorialEvent::Fixture {
            attempt,
            call_id,
            sequence,
            observed_at_ms: at,
            result,
        } = event
        else {
            return Err(TutorialError::InvalidObservation);
        };
        self.attempt(attempt)?;
        let s = self.session()?;
        if sequence <= s.sequence
            || at < s.admitted_at
            || at < s.fixture_time
            || at > self.now
            || s.start
                .as_ref()
                .is_some_and(|start| at < start.observed_at_ms)
        {
            return Err(TutorialError::InvalidObservation);
        }
        let action = match call_id {
            Some(id) => {
                if self.fixture_sent.get(&id).is_none_or(|sent| at < *sent) {
                    return Err(TutorialError::InvalidObservation);
                }
                Some(
                    self.fixture_calls
                        .get(&id)
                        .ok_or(TutorialError::InvalidObservation)?
                        .clone(),
                )
            }
            None => None,
        };
        if let Ok(observation) = &result {
            let owned = s.fixture.as_ref().map(|f| f.id);
            if !fixture_matches(action.as_ref(), observation, owned) {
                return Err(TutorialError::InvalidObservation);
            }
        }
        if let Some(id) = call_id
            && !matches!(
                result,
                Ok(TutorialFixtureObservation::CloseRequested { .. })
            )
        {
            self.fixture_calls.remove(&id);
            self.submitted.remove(&id);
            self.fixture_sent.remove(&id);
        }
        self.session_mut()?.sequence = sequence;
        self.session_mut()?.fixture_time = at;
        match result {
            Err(error) => {
                if matches!(action, Some(TutorialFixtureAction::PlayTone { .. }))
                    && error != TutorialFixtureError::TimedOut
                {
                    self.session_mut()?.tone_unknown = false;
                }
                self.fail(TutorialFailure::Fixture(error), effects)?;
            }
            Ok(TutorialFixtureObservation::Lost { reason, .. }) => {
                self.fail(TutorialFailure::Fixture(reason), effects)?
            }
            Ok(TutorialFixtureObservation::Opened {
                fixture,
                window,
                label,
                ..
            }) => {
                if fixture == 0
                    || label != self.session()?.context.machine_label
                    || self.session()?.fixture.is_some()
                {
                    return Err(TutorialError::InvalidObservation);
                }
                self.session_mut()?.fixture = Some(OwnedFixture {
                    id: fixture,
                    window,
                    armed: false,
                    clicks: 0,
                    clicked: false,
                    home_at: None,
                });
                if self.session()?.retired {
                    self.cleanup(effects)?;
                } else {
                    match self.session()?.binding.attempt.role {
                        TutorialRole::E1Target => self.fixture_call(
                            TutorialFixtureAction::ArmTarget {
                                fixture,
                                phase: attempt.0,
                            },
                            effects,
                        )?,
                        TutorialRole::E2SourcePush => {
                            self.call(InstallerRequest::Windows, effects)?
                        }
                        _ => {
                            self.state = TutorialState::WaitingUser;
                            effects.push(self.effect(TutorialEffectKind::WaitForUser)?);
                        }
                    }
                }
            }
            Ok(TutorialFixtureObservation::TargetArmed { phase, .. }) => {
                if phase != attempt.0 {
                    return Err(TutorialError::InvalidObservation);
                }
                let f = self
                    .session_mut()?
                    .fixture
                    .as_mut()
                    .ok_or(TutorialError::InvalidObservation)?;
                f.armed = true;
                if self.session()?.retired {
                    self.cleanup(effects)?;
                } else {
                    self.state = TutorialState::WaitingUser;
                }
            }
            Ok(TutorialFixtureObservation::Snapshot {
                window,
                phase,
                target_clicks,
                window_facts,
                tone,
                ..
            }) => {
                if window_facts == TutorialWindowFacts::Missing {
                    return self.fail(
                        TutorialFailure::Fixture(TutorialFixtureError::UnknownWindow),
                        effects,
                    );
                }
                let end_at = self
                    .session()?
                    .end
                    .as_ref()
                    .map(|s| s.observed_at_ms)
                    .or_else(|| {
                        self.session()
                            .ok()?
                            .projection_cleanup
                            .as_ref()?
                            .terminal_at
                    });
                let f = self
                    .session_mut()?
                    .fixture
                    .as_mut()
                    .ok_or(TutorialError::InvalidObservation)?;
                if window != f.window || target_clicks < f.clicks {
                    return Err(TutorialError::InvalidObservation);
                }
                f.clicked |= f.armed && phase == Some(attempt.0) && target_clicks > f.clicks;
                f.clicks = target_clicks;
                if end_at.is_some_and(|end| at >= end)
                    && window_facts
                        == (TutorialWindowFacts::Present {
                            visible_on_user_workspace: Some(true),
                            on_initial_display: Some(true),
                        })
                {
                    f.home_at = Some(at);
                } else if end_at.is_some() {
                    f.home_at = None;
                }
                if self.session()?.tone_unknown && at >= self.session()?.retired_at {
                    self.session_mut()?.tone_unknown = false;
                    match tone {
                        TutorialToneState::Running { tone } => {
                            self.session_mut()?.tone = Some(tone)
                        }
                        TutorialToneState::Stopped => {}
                        TutorialToneState::StopUnconfirmed { tone } => {
                            self.session_mut()?.tone = Some(tone);
                            self.session_mut()?.tone_unknown = true;
                        }
                    }
                }
                if matches!(tone, TutorialToneState::StopUnconfirmed { .. }) {
                    self.fail(
                        TutorialFailure::Fixture(TutorialFixtureError::CleanupFailed),
                        effects,
                    )?;
                }
                if self.session()?.retired {
                    self.cleanup(effects)?;
                }
                self.settle_cleanup(effects)?;
            }
            Ok(TutorialFixtureObservation::ToneStarted { tone, .. }) => {
                if self.session()?.tone.is_some() {
                    return Err(TutorialError::InvalidObservation);
                }
                self.session_mut()?.tone = Some(tone);
                self.session_mut()?.tone_unknown = false;
                self.session_mut()?.audio_after = Some(at);
                if self.session()?.retired {
                    self.cleanup(effects)?;
                } else {
                    self.detect(effects)?;
                }
            }
            Ok(TutorialFixtureObservation::ToneStopped { tone, .. }) => {
                if self.session()?.tone != Some(tone) {
                    return Err(TutorialError::InvalidObservation);
                }
                self.session_mut()?.tone = None;
                self.session_mut()?.tone_unknown = false;
                let stops: Vec<_> = self
                    .fixture_calls
                    .iter()
                    .filter(|(_, a)| {
                        matches!(a,
                    TutorialFixtureAction::StopTone { tone: t, .. } if *t == tone)
                    })
                    .map(|(id, _)| *id)
                    .collect();
                for id in stops {
                    self.fixture_calls.remove(&id);
                    self.submitted.remove(&id);
                    self.fixture_sent.remove(&id);
                }
                if !self.session()?.retired
                    && self
                        .session()?
                        .end
                        .as_ref()
                        .is_none_or(|end| at <= end.observed_at_ms)
                {
                    self.fail(
                        TutorialFailure::Fixture(TutorialFixtureError::CleanupFailed),
                        effects,
                    )?;
                } else {
                    self.settle_cleanup(effects)?;
                }
            }
            Ok(TutorialFixtureObservation::CloseRequested { .. }) => {
                if self.session()?.close_ack {
                    return Err(TutorialError::InvalidObservation);
                }
                self.session_mut()?.close_ack = true;
                if call_id.is_none() {
                    self.retire(TutorialState::Cancelled, effects)?;
                }
            }
            Ok(TutorialFixtureObservation::Closed { .. }) => {
                if !self.session()?.close_sent {
                    return Err(TutorialError::InvalidObservation);
                }
                self.session_mut()?.fixture = None;
                self.settle_cleanup(effects)?;
            }
        }
        Ok(())
    }
}

pub(super) fn fixture_matches(
    action: Option<&TutorialFixtureAction>,
    observation: &TutorialFixtureObservation,
    owned: Option<u64>,
) -> bool {
    use TutorialFixtureAction as A;
    use TutorialFixtureObservation as O;
    let id = match observation {
        O::Opened { .. } => return matches!(action, Some(A::Open { .. })),
        O::TargetArmed { fixture, .. }
        | O::Snapshot { fixture, .. }
        | O::ToneStarted { fixture, .. }
        | O::ToneStopped { fixture, .. }
        | O::CloseRequested { fixture }
        | O::Closed { fixture }
        | O::Lost { fixture, .. } => *fixture,
    };
    owned == Some(id)
        && matches!(
            (action, observation),
            (Some(A::ArmTarget { .. }), O::TargetArmed { .. })
                | (Some(A::ObserveWindow { .. }), O::Snapshot { .. })
                | (Some(A::PlayTone { .. }), O::ToneStarted { .. })
                | (Some(A::StopTone { .. }), O::ToneStopped { .. })
                | (
                    Some(A::Close { .. }),
                    O::CloseRequested { .. } | O::Closed { .. }
                )
                | (
                    None,
                    O::Snapshot { .. }
                        | O::ToneStopped { .. }
                        | O::CloseRequested { .. }
                        | O::Closed { .. }
                        | O::Lost { .. },
                )
        )
}
