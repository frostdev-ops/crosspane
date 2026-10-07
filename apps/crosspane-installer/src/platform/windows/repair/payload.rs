//! Payload repair uses live capabilities; persisted facts never replay native ownership.
use super::super::{
    native_io::{NativeError, NativeResult},
    payload::{
        inventory::PayloadRole,
        recovery::{FileStamp, ImageObservation, OriginalLeaf},
    },
    service::supervisor::Generation,
};
use super::payload_record::{
    PayloadRepairPhase as Phase, PayloadRepairProcess, PayloadRepairRecord,
};
use super::{JournalObservation, RepairDiagnostic, RepairObservation, RepairTaskObservation};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PayloadRepairDecision {
    Healthy,
    Eligible,
    Disabled,
    ReinstallRequired,
}
pub(crate) fn classify(observed: &RepairObservation) -> NativeResult<PayloadRepairDecision> {
    if observed.task().diagnostic() == RepairDiagnostic::Disabled {
        return Ok(PayloadRepairDecision::Disabled);
    }
    if observed.publication() != RepairDiagnostic::Healthy
        || observed
            .journals()
            .iter()
            .any(|j| !matches!(j, JournalObservation::Absent(_)))
        || !(observed.agent() == RepairDiagnostic::Healthy
            || matches!(
                observed.payload(),
                RepairDiagnostic::Missing | RepairDiagnostic::Mismatch
            ) && observed.agent() == RepairDiagnostic::Unknown)
        || !matches!(
            observed.task().diagnostic(),
            RepairDiagnostic::Healthy | RepairDiagnostic::Missing
        )
        || observed.task().expected_xml().is_none()
    {
        return Ok(PayloadRepairDecision::ReinstallRequired);
    }
    Ok(match observed.payload() {
        RepairDiagnostic::Healthy => PayloadRepairDecision::Healthy,
        RepairDiagnostic::Missing | RepairDiagnostic::Mismatch => PayloadRepairDecision::Eligible,
        _ => PayloadRepairDecision::ReinstallRequired,
    })
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PayloadRepairOutcome {
    Complete,
    Retired,
    Retained,
    Cancelled,
}
/// Associated capabilities are genuine adapter-owned objects. Every recovery callback is
/// read-only and selects only this same live owner, never an id/path from the record.
pub(crate) trait PayloadRepairPort: Sized {
    type Sources;
    type Owner;
    type Completion;
    type Verified;
    type Started;
    fn take_state(&mut self) -> NativeResult<PayloadRepairState<Self>>;
    fn retain_state(&mut self, state: PayloadRepairState<Self>);
    fn renew(&mut self, record: &PayloadRepairRecord) -> NativeResult<()>;
    fn persist(&mut self, record: &PayloadRepairRecord) -> NativeResult<()>;
    fn task_observe(&mut self, record: &PayloadRepairRecord)
    -> NativeResult<RepairTaskObservation>;
    fn verify_sources(&mut self, record: &PayloadRepairRecord) -> NativeResult<Self::Sources>;
    fn recover_sources(
        &mut self,
        record: &PayloadRepairRecord,
    ) -> NativeResult<Option<Self::Sources>>;
    fn keeper_copy(
        &mut self,
        record: &PayloadRepairRecord,
        sources: &Self::Sources,
    ) -> NativeResult<ImageObservation>;
    fn recover_copy(
        &mut self,
        record: &PayloadRepairRecord,
    ) -> NativeResult<Option<ImageObservation>>;
    fn keeper_prepare(
        &mut self,
        record: &PayloadRepairRecord,
        sources: &Self::Sources,
    ) -> NativeResult<Self::Owner>;
    fn recover_owner(&mut self, record: &PayloadRepairRecord) -> NativeResult<Option<Self::Owner>>;
    fn keeper_facts(
        &self,
        owner: &Self::Owner,
    ) -> NativeResult<(PayloadRepairProcess, PayloadRepairProcess, u64)>;
    fn keeper_resume(
        &mut self,
        record: &PayloadRepairRecord,
        owner: &Self::Owner,
    ) -> NativeResult<()>;
    fn keeper_ready(
        &mut self,
        record: &PayloadRepairRecord,
        owner: &Self::Owner,
    ) -> NativeResult<bool>;
    fn keeper_commit(
        &mut self,
        record: &PayloadRepairRecord,
        owner: &Self::Owner,
    ) -> NativeResult<()>;
    fn commit_authorized(
        &mut self,
        record: &PayloadRepairRecord,
        owner: &Self::Owner,
    ) -> NativeResult<bool>;
    fn stop_once(
        &mut self,
        record: &PayloadRepairRecord,
        owner: &Self::Owner,
    ) -> NativeResult<Self::Completion>;
    fn recover_completion(
        &mut self,
        record: &PayloadRepairRecord,
        owner: &Self::Owner,
    ) -> NativeResult<Option<Self::Completion>>;
    /// Exact original supervisor/child exit, job zero, clean receipt and all file/worker/parent
    /// aliases must physically settle before this returns. Completion metadata is insufficient.
    fn settled(
        &mut self,
        record: &PayloadRepairRecord,
        owner: &Self::Owner,
        completion: &Self::Completion,
    ) -> NativeResult<()>;
    fn stage(
        &mut self,
        record: &PayloadRepairRecord,
        role: PayloadRole,
        sources: &Self::Sources,
        completion: &Self::Completion,
    ) -> NativeResult<ImageObservation>;
    fn recover_stage(
        &mut self,
        record: &PayloadRepairRecord,
        role: PayloadRole,
        completion: &Self::Completion,
    ) -> NativeResult<Option<ImageObservation>>;
    fn observe_original(
        &mut self,
        record: &PayloadRepairRecord,
        role: PayloadRole,
        completion: &Self::Completion,
    ) -> NativeResult<OriginalLeaf>;
    fn backup(
        &mut self,
        record: &PayloadRepairRecord,
        role: PayloadRole,
        completion: &Self::Completion,
    ) -> NativeResult<Option<FileStamp>>;
    fn recover_backup(
        &mut self,
        record: &PayloadRepairRecord,
        role: PayloadRole,
        completion: &Self::Completion,
    ) -> NativeResult<Option<Option<FileStamp>>>;
    fn publish(
        &mut self,
        record: &PayloadRepairRecord,
        role: PayloadRole,
        completion: &Self::Completion,
    ) -> NativeResult<ImageObservation>;
    fn recover_publish(
        &mut self,
        record: &PayloadRepairRecord,
        role: PayloadRole,
        completion: &Self::Completion,
    ) -> NativeResult<Option<ImageObservation>>;
    fn verify_fixed(
        &mut self,
        record: &PayloadRepairRecord,
        completion: &Self::Completion,
    ) -> NativeResult<Self::Verified>;
    fn recover_verified(
        &mut self,
        record: &PayloadRepairRecord,
        completion: &Self::Completion,
    ) -> NativeResult<Option<Self::Verified>>;
    fn start_once(
        &mut self,
        record: &PayloadRepairRecord,
        verified: &Self::Verified,
        completion: &Self::Completion,
    ) -> NativeResult<Self::Started>;
    fn recover_started(
        &mut self,
        record: &PayloadRepairRecord,
        verified: &Self::Verified,
    ) -> NativeResult<Option<Self::Started>>;
    fn submission(&self, started: &Self::Started) -> NativeResult<String>;
    /// Fresh exact images/claim/repair epoch, actual Ready BEFORE Running, distinct u64 instance.
    fn health(
        &mut self,
        record: &PayloadRepairRecord,
        verified: &Self::Verified,
        started: &Self::Started,
    ) -> NativeResult<Option<Generation>>;
    /// Returns true only for genuine settled final-copy cleanup/positive absence. The currently
    /// executing keeper cannot make this true; terminal phase alone never authorizes DELETE.
    fn retire_settled(&mut self, record: &PayloadRepairRecord) -> NativeResult<bool>;
}
/// This same runtime state is retained on every Err. Sticky attempts precede publication as
/// well as dispatch, so an unknown intent delivery cannot silently enable another effect.
pub(crate) struct PayloadRepairState<P: PayloadRepairPort> {
    sources: Option<P::Sources>,
    owner: Option<P::Owner>,
    completion: Option<P::Completion>,
    verified: Option<P::Verified>,
    started: Option<P::Started>,
    source_attempted: bool,
    attempted: Vec<Phase>,
}
impl<P: PayloadRepairPort> Default for PayloadRepairState<P> {
    fn default() -> Self {
        Self {
            sources: None,
            owner: None,
            completion: None,
            verified: None,
            started: None,
            source_attempted: false,
            attempted: Vec::new(),
        }
    }
}
impl<P: PayloadRepairPort> PayloadRepairState<P> {
    fn reserve(&mut self, phase: Phase) -> bool {
        if self.attempted.contains(&phase) {
            return false;
        }
        self.attempted.push(phase);
        true
    }
    #[cfg(test)]
    pub(crate) fn retains_owner(&self) -> bool {
        self.owner.is_some()
    }
}
fn publish<P: PayloadRepairPort>(
    port: &mut P,
    record: &mut PayloadRepairRecord,
    phase: Phase,
) -> NativeResult<()> {
    let mut next = record.clone();
    next.advance(phase)?;
    port.renew(record)?;
    port.persist(&next)?;
    *record = next;
    Ok(())
}
fn publish_bound<P: PayloadRepairPort>(
    port: &mut P,
    record: &mut PayloadRepairRecord,
    next: PayloadRepairRecord,
    phase: Phase,
) -> NativeResult<()> {
    let mut next = next;
    next.advance(phase)?;
    record.publication_successor(&next)?;
    port.renew(record)?;
    port.persist(&next)?;
    *record = next;
    Ok(())
}
fn task_allowed(task: &RepairTaskObservation, record: &PayloadRepairRecord) -> bool {
    matches!(
        task.diagnostic(),
        RepairDiagnostic::Healthy | RepairDiagnostic::Missing
    ) && task.expected_xml() == Some(record.task().xml())
}
/// A resident checkout is always returned, including failed publication/native delivery.
/// Ports reserve and retain each actual native worker/child before delivering observations.
pub(crate) fn drive<P: PayloadRepairPort>(
    port: &mut P,
    record: &mut PayloadRepairRecord,
) -> NativeResult<PayloadRepairOutcome> {
    let mut state = port.take_state()?;
    let result = step(&mut state, port, record);
    port.retain_state(state);
    result
}
fn step<P: PayloadRepairPort>(
    state: &mut PayloadRepairState<P>,
    port: &mut P,
    record: &mut PayloadRepairRecord,
) -> NativeResult<PayloadRepairOutcome> {
    record.validate()?;
    port.renew(record)?;
    if record.phase() == Phase::Unknown {
        return Ok(PayloadRepairOutcome::Retained);
    }
    if record.phase() == Phase::Cancelled {
        return Ok(PayloadRepairOutcome::Cancelled);
    }
    if record.phase() == Phase::Retired {
        return Ok(PayloadRepairOutcome::Retired);
    }
    if record.slot().is_none() {
        return Ok(PayloadRepairOutcome::Retained);
    }
    if record.phase() == Phase::Complete {
        if port.retire_settled(record)? {
            publish(port, record, Phase::Retired)?;
            return Ok(PayloadRepairOutcome::Retired);
        }
        return Ok(PayloadRepairOutcome::Complete);
    }
    // A cold journal never reaches an effect; the port must return actual retained capabilities.
    if state.sources.is_none() {
        if record.phase() == Phase::Selected && !state.source_attempted {
            if !task_allowed(&port.task_observe(record)?, record) {
                return Ok(PayloadRepairOutcome::Retained);
            }
            state.source_attempted = true;
            state.sources = Some(port.verify_sources(record)?);
        } else {
            state.sources = port.recover_sources(record)?;
        }
    }
    if state.sources.is_none() {
        return Ok(PayloadRepairOutcome::Retained);
    }
    let mut first_dispatch = None;
    loop {
        port.renew(record)?;
        if record.phase().rank() >= Phase::HandoffIntent.rank() && state.owner.is_none() {
            state.owner = port.recover_owner(record)?;
            if record.phase() != Phase::HandoffIntent && state.owner.is_none() {
                return Ok(PayloadRepairOutcome::Retained);
            }
        }
        match record.phase() {
            Phase::Selected => {
                if !task_allowed(&port.task_observe(record)?, record)
                    || !state.reserve(Phase::CopyIntent)
                {
                    return Ok(PayloadRepairOutcome::Retained);
                }
                publish(port, record, Phase::CopyIntent)?;
                first_dispatch = Some(Phase::CopyIntent);
            }
            Phase::CopyIntent => {
                let image = if first_dispatch.take() == Some(Phase::CopyIntent) {
                    Some(port.keeper_copy(
                        record,
                        state.sources.as_ref().ok_or(NativeError::OutcomeUnknown)?,
                    )?)
                } else {
                    port.recover_copy(record)?
                };
                let Some(image) = image else {
                    return Ok(PayloadRepairOutcome::Retained);
                };
                let mut next = record.clone();
                next.bind_keeper_copy(image)?;
                publish_bound(port, record, next, Phase::CopyReady)?;
            }
            Phase::CopyReady => {
                if !state.reserve(Phase::HandoffIntent) {
                    return Ok(PayloadRepairOutcome::Retained);
                }
                publish(port, record, Phase::HandoffIntent)?;
                first_dispatch = Some(Phase::HandoffIntent);
            }
            Phase::HandoffIntent => {
                if first_dispatch.take() == Some(Phase::HandoffIntent) {
                    state.owner = Some(port.keeper_prepare(
                        record,
                        state.sources.as_ref().ok_or(NativeError::OutcomeUnknown)?,
                    )?);
                }
                let Some(owner) = state.owner.as_ref() else {
                    return Ok(PayloadRepairOutcome::Retained);
                };
                let (parent, child, handle) = port.keeper_facts(owner)?;
                let mut next = record.clone();
                next.bind_keeper_child(parent, child, handle)?;
                publish_bound(port, record, next, Phase::Created)?;
            }
            Phase::Created => {
                if !state.reserve(Phase::ResumeIntent) {
                    return Ok(PayloadRepairOutcome::Retained);
                }
                publish(port, record, Phase::ResumeIntent)?;
                first_dispatch = Some(Phase::ResumeIntent);
            }
            Phase::ResumeIntent => {
                let owner = state.owner.as_ref().ok_or(NativeError::OutcomeUnknown)?;
                if first_dispatch.take() == Some(Phase::ResumeIntent) {
                    port.keeper_resume(record, owner)?;
                }
                if !port.keeper_ready(record, owner)? {
                    return Ok(PayloadRepairOutcome::Retained);
                }
                publish(port, record, Phase::Ready)?;
            }
            Phase::Ready => {
                if !task_allowed(&port.task_observe(record)?, record)
                    || !state.reserve(Phase::CommitIntent)
                {
                    return Ok(PayloadRepairOutcome::Retained);
                }
                publish(port, record, Phase::CommitIntent)?;
                first_dispatch = Some(Phase::CommitIntent);
            }
            Phase::CommitIntent => {
                let owner = state.owner.as_ref().ok_or(NativeError::OutcomeUnknown)?;
                if first_dispatch.take() == Some(Phase::CommitIntent) {
                    port.keeper_commit(record, owner)?;
                }
                if !port.commit_authorized(record, owner)? {
                    return Ok(PayloadRepairOutcome::Retained);
                }
                publish(port, record, Phase::Committed)?;
            }
            Phase::Committed => {
                let owner = state.owner.as_ref().ok_or(NativeError::OutcomeUnknown)?;
                if !task_allowed(&port.task_observe(record)?, record)
                    || !port.commit_authorized(record, owner)?
                    || !state.reserve(Phase::StopIntent)
                {
                    return Ok(PayloadRepairOutcome::Retained);
                }
                publish(port, record, Phase::StopIntent)?;
                first_dispatch = Some(Phase::StopIntent);
            }
            Phase::StopIntent => {
                let owner = state.owner.as_ref().ok_or(NativeError::OutcomeUnknown)?;
                if first_dispatch.take() == Some(Phase::StopIntent) {
                    state.completion = Some(port.stop_once(record, owner)?);
                } else if state.completion.is_none() {
                    state.completion = port.recover_completion(record, owner)?;
                }
                let Some(completion) = state.completion.as_ref() else {
                    return Ok(PayloadRepairOutcome::Retained);
                };
                port.settled(record, owner, completion)?;
                publish(port, record, Phase::TreeCompleted)?;
            }
            Phase::TreeCompleted | Phase::Published { .. } => {
                let role = match record.phase() {
                    Phase::TreeCompleted => Some(PayloadRole::Installer),
                    Phase::Published { role } => PayloadRole::ALL
                        .iter()
                        .position(|r| *r == role)
                        .and_then(|i| PayloadRole::ALL.get(i + 1))
                        .copied(),
                    _ => None,
                };
                let owner = state.owner.as_ref().ok_or(NativeError::OutcomeUnknown)?;
                if state.completion.is_none() {
                    state.completion = port.recover_completion(record, owner)?;
                }
                let Some(completion) = state.completion.as_ref() else {
                    return Ok(PayloadRepairOutcome::Retained);
                };
                port.settled(record, owner, completion)?;
                if let Some(role) = role {
                    let intent = Phase::StageIntent { role };
                    if !state.reserve(intent) {
                        return Ok(PayloadRepairOutcome::Retained);
                    }
                    publish(port, record, intent)?;
                    first_dispatch = Some(intent);
                } else {
                    if state.verified.is_none() {
                        state.verified = Some(port.verify_fixed(record, completion)?);
                    }
                    publish(port, record, Phase::FixedVerified)?;
                }
            }
            Phase::StageIntent { role } => {
                let owner = state.owner.as_ref().ok_or(NativeError::OutcomeUnknown)?;
                if state.completion.is_none() {
                    state.completion = port.recover_completion(record, owner)?;
                }
                let Some(completion) = state.completion.as_ref() else {
                    return Ok(PayloadRepairOutcome::Retained);
                };
                port.settled(record, owner, completion)?;
                let image = if first_dispatch.take() == Some(Phase::StageIntent { role }) {
                    Some(port.stage(
                        record,
                        role,
                        state.sources.as_ref().ok_or(NativeError::OutcomeUnknown)?,
                        completion,
                    )?)
                } else {
                    port.recover_stage(record, role, completion)?
                };
                let Some(image) = image else {
                    return Ok(PayloadRepairOutcome::Retained);
                };
                let mut next = record.clone();
                next.bind_staged(role, image)?;
                publish_bound(port, record, next, Phase::Staged { role })?;
            }
            Phase::Staged { role } => {
                let owner = state.owner.as_ref().ok_or(NativeError::OutcomeUnknown)?;
                if state.completion.is_none() {
                    state.completion = port.recover_completion(record, owner)?;
                }
                let Some(completion) = state.completion.as_ref() else {
                    return Ok(PayloadRepairOutcome::Retained);
                };
                port.settled(record, owner, completion)?;
                let original = port.observe_original(record, role, completion)?;
                let mut next = record.clone();
                next.bind_original(role, original)?;
                let intent = Phase::BackupIntent { role };
                if !state.reserve(intent) {
                    return Ok(PayloadRepairOutcome::Retained);
                }
                publish_bound(port, record, next, intent)?;
                first_dispatch = Some(intent);
            }
            Phase::BackupIntent { role } => {
                let owner = state.owner.as_ref().ok_or(NativeError::OutcomeUnknown)?;
                if state.completion.is_none() {
                    state.completion = port.recover_completion(record, owner)?;
                }
                let Some(completion) = state.completion.as_ref() else {
                    return Ok(PayloadRepairOutcome::Retained);
                };
                port.settled(record, owner, completion)?;
                let backup = if first_dispatch.take() == Some(Phase::BackupIntent { role }) {
                    Some(port.backup(record, role, completion)?)
                } else {
                    port.recover_backup(record, role, completion)?
                };
                let Some(backup) = backup else {
                    return Ok(PayloadRepairOutcome::Retained);
                };
                let mut next = record.clone();
                next.bind_backup(role, backup)?;
                publish_bound(port, record, next, Phase::BackedUp { role })?;
            }
            Phase::BackedUp { role } => {
                let intent = Phase::PublishIntent { role };
                if !state.reserve(intent) {
                    return Ok(PayloadRepairOutcome::Retained);
                }
                publish(port, record, intent)?;
                first_dispatch = Some(intent);
            }
            Phase::PublishIntent { role } => {
                let owner = state.owner.as_ref().ok_or(NativeError::OutcomeUnknown)?;
                if state.completion.is_none() {
                    state.completion = port.recover_completion(record, owner)?;
                }
                let Some(completion) = state.completion.as_ref() else {
                    return Ok(PayloadRepairOutcome::Retained);
                };
                port.settled(record, owner, completion)?;
                let image = if first_dispatch.take() == Some(Phase::PublishIntent { role }) {
                    Some(port.publish(record, role, completion)?)
                } else {
                    port.recover_publish(record, role, completion)?
                };
                let Some(image) = image else {
                    return Ok(PayloadRepairOutcome::Retained);
                };
                let mut next = record.clone();
                next.bind_published(role, image)?;
                publish_bound(port, record, next, Phase::Published { role })?;
            }
            Phase::FixedVerified => {
                let owner = state.owner.as_ref().ok_or(NativeError::OutcomeUnknown)?;
                if state.completion.is_none() {
                    state.completion = port.recover_completion(record, owner)?;
                }
                let Some(completion) = state.completion.as_ref() else {
                    return Ok(PayloadRepairOutcome::Retained);
                };
                port.settled(record, owner, completion)?;
                if state.verified.is_none() {
                    state.verified = port.recover_verified(record, completion)?;
                }
                if state.verified.is_none()
                    || !task_allowed(&port.task_observe(record)?, record)
                    || !state.reserve(Phase::StartIntent)
                {
                    return Ok(PayloadRepairOutcome::Retained);
                }
                publish(port, record, Phase::StartIntent)?;
                first_dispatch = Some(Phase::StartIntent);
            }
            Phase::StartIntent => {
                let owner = state.owner.as_ref().ok_or(NativeError::OutcomeUnknown)?;
                if state.completion.is_none() {
                    state.completion = port.recover_completion(record, owner)?;
                }
                let Some(completion) = state.completion.as_ref() else {
                    return Ok(PayloadRepairOutcome::Retained);
                };
                port.settled(record, owner, completion)?;
                if state.verified.is_none() {
                    state.verified = port.recover_verified(record, completion)?;
                }
                let Some(verified) = state.verified.as_ref() else {
                    return Ok(PayloadRepairOutcome::Retained);
                };
                if first_dispatch.take() == Some(Phase::StartIntent) {
                    if !task_allowed(&port.task_observe(record)?, record) {
                        return Ok(PayloadRepairOutcome::Retained);
                    }
                    state.started = Some(port.start_once(record, verified, completion)?);
                } else if state.started.is_none() {
                    state.started = port.recover_started(record, verified)?;
                }
                let Some(started) = state.started.as_ref() else {
                    return Ok(PayloadRepairOutcome::Retained);
                };
                let mut next = record.clone();
                next.bind_submission(port.submission(started)?)?;
                publish_bound(port, record, next, Phase::StartSubmitted)?;
            }
            Phase::StartSubmitted | Phase::ReadyObserved => {
                let owner = state.owner.as_ref().ok_or(NativeError::OutcomeUnknown)?;
                if state.completion.is_none() {
                    state.completion = port.recover_completion(record, owner)?;
                }
                let Some(completion) = state.completion.as_ref() else {
                    return Ok(PayloadRepairOutcome::Retained);
                };
                if state.verified.is_none() {
                    state.verified = port.recover_verified(record, completion)?;
                }
                let Some(verified) = state.verified.as_ref() else {
                    return Ok(PayloadRepairOutcome::Retained);
                };
                if state.started.is_none() {
                    state.started = port.recover_started(record, verified)?;
                }
                let Some(started) = state.started.as_ref() else {
                    return Ok(PayloadRepairOutcome::Retained);
                };
                let Some(generation) = port.health(record, verified, started)? else {
                    return Ok(PayloadRepairOutcome::Retained);
                };
                if record.phase() == Phase::StartSubmitted {
                    let mut next = record.clone();
                    next.bind_ready(generation)?;
                    publish_bound(port, record, next, Phase::ReadyObserved)?;
                } else {
                    if record.ready() != Some(generation) {
                        return Err(NativeError::Foreign);
                    }
                    publish(port, record, Phase::Complete)?;
                }
            }
            Phase::Complete => {
                if port.retire_settled(record)? {
                    publish(port, record, Phase::Retired)?;
                    return Ok(PayloadRepairOutcome::Retired);
                }
                return Ok(PayloadRepairOutcome::Complete);
            }
            Phase::Retired => return Ok(PayloadRepairOutcome::Retired),
            Phase::Unknown => return Ok(PayloadRepairOutcome::Retained),
            Phase::Cancelled => return Ok(PayloadRepairOutcome::Cancelled),
        }
    }
}
