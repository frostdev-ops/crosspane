use super::*;

impl Tutorial {
    pub(super) fn audio_sample(
        &mut self,
        current: CounterSample,
        effects: &mut Vec<TutorialEffect>,
    ) -> Result<(), TutorialError> {
        let role = self.session()?.binding.attempt.role;
        let peers = &self
            .health
            .as_ref()
            .ok_or(TutorialError::InsufficientContract)?
            .installer()
            .audio
            .active_peers;
        let sole = peers.as_slice() == [self.peer()?];
        let peer = self.peer()?;
        if peers.iter().any(|p| *p != peer) || peers.len() > 1 {
            return self.fail(TutorialFailure::Isolation, effects);
        }
        // An idle cleanup sample never replaces the active proof endpoint.
        if self.session()?.end.is_some() {
            return Ok(());
        }
        if role == TutorialRole::AudioSender && self.session()?.tone.is_none() {
            return Ok(());
        }
        if role == TutorialRole::AudioReceiver
            && !self
                .session()?
                .human
                .contains(&HumanConfirmation::SelectedSourceToneStarted)
        {
            return Ok(());
        }
        if self.session()?.start.is_none() {
            if sole
                && self
                    .session()?
                    .audio_after
                    .is_some_and(|boundary| current.observed_at_ms > boundary)
            {
                self.session_mut()?.start = Some(current);
                self.state = TutorialState::Running;
            }
            return Ok(());
        }
        if !sole {
            return self.fail(TutorialFailure::Isolation, effects);
        }
        let (advance, forbidden) = requirements(role);
        let s = self.session()?;
        if !isolated(
            role,
            s.start.as_ref().ok_or(TutorialError::InvalidObservation)?,
            &current,
        ) {
            return self.fail(TutorialFailure::Isolation, effects);
        }
        if check_activity(
            s.binding.attempt.attempt,
            s.start.as_ref().ok_or(TutorialError::InvalidObservation)?,
            &current,
            dependencies(role),
            &advance,
            &forbidden,
        )
        .is_ok()
        {
            self.session_mut()?.end = Some(current);
            self.state = TutorialState::WaitingUser;
            if role == TutorialRole::AudioSender {
                let s = self.session()?;
                let fixture = s
                    .fixture
                    .as_ref()
                    .ok_or(TutorialError::InvalidObservation)?
                    .id;
                let tone = s.tone.ok_or(TutorialError::InvalidObservation)?;
                self.session_mut()?.stop_sent = true;
                self.fixture_call(TutorialFixtureAction::StopTone { fixture, tone }, effects)?;
            }
        }
        Ok(())
    }
}
