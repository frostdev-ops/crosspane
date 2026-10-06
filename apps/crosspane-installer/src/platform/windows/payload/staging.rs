//! The same journal-before-action sequence drives the sealed native port and interruption fakes.
use super::super::native_io::{NativeError, NativeResult};
use super::{
    inventory::PayloadRole,
    recovery::{FileStamp, ImageObservation, OperationRecord, OriginalLeaf, Phase},
};
pub(crate) trait PayloadPort {
    type Stop;
    type Verified;
    type Started;
    fn journal(&mut self, record: &OperationRecord) -> NativeResult<()>;
    // A4b/A5 activates initial install only after genuine old-owner completion; named fakes cover this sequence.
    #[allow(dead_code)]
    fn stop(&mut self, operation: [u8; 16]) -> NativeResult<Self::Stop>;
    fn original_instance(&self, stop: &Self::Stop) -> Option<u64>;
    fn released(&mut self, stop: &Self::Stop) -> NativeResult<()>;
    fn stage(&mut self, operation: [u8; 16], role: PayloadRole) -> NativeResult<ImageObservation>;
    // A4b/A5 activates initial install only after genuine old-owner completion; named fakes cover this sequence.
    #[allow(dead_code)]
    fn observe_original(&mut self, role: PayloadRole) -> NativeResult<OriginalLeaf>;
    fn backup(&mut self, operation: [u8; 16], role: PayloadRole)
    -> NativeResult<Option<FileStamp>>;
    fn publish(&mut self, operation: [u8; 16], role: PayloadRole)
    -> NativeResult<ImageObservation>;
    fn verify(&mut self, operation: [u8; 16]) -> NativeResult<Self::Verified>;
    fn start(
        &mut self,
        operation: [u8; 16],
        verified: &Self::Verified,
    ) -> NativeResult<Self::Started>;
    fn health(
        &mut self,
        operation: [u8; 16],
        started: &Self::Started,
        verified: &Self::Verified,
    ) -> NativeResult<String>;
    fn prune(&mut self, operation: [u8; 16]) -> NativeResult<bool>;
}
pub(crate) fn persist<P: PayloadPort>(
    port: &mut P,
    record: &mut OperationRecord,
    phase: Phase,
    role: Option<PayloadRole>,
) -> NativeResult<()> {
    let mut next = record.clone();
    next.advance(phase, role);
    next.validate()?;
    port.journal(&next)?;
    *record = next;
    Ok(())
}
/// Returned failures preserve the last durable intent; they do not dispatch a retry or another start.
// A4b/A5 activates initial install only after genuine old-owner completion; named fakes cover this sequence.
#[allow(dead_code)]
pub(crate) fn apply<P: PayloadPort>(
    port: &mut P,
    record: &mut OperationRecord,
) -> NativeResult<()> {
    record.validate()?;
    if record.phase() != Phase::Intent {
        return Err(NativeError::OutcomeUnknown);
    }
    persist(port, record, Phase::Intent, None)?;
    persist(port, record, Phase::StopIntent, None)?;
    let stop = port.stop(record.operation())?;
    let mut next = record.clone();
    if let Some(instance) = port.original_instance(&stop) {
        next.set_original_instance(format!("{instance:032x}"));
    }
    port.released(&stop)?;
    persist(port, &mut next, Phase::ImageReleased, None)?;
    *record = next;
    for role in PayloadRole::ALL {
        persist(port, record, Phase::StageIntent, Some(role))?;
        let observed = port.stage(record.operation(), role)?;
        let mut next = record.clone();
        next.role_mut(role)?.staged = Some(observed);
        persist(port, &mut next, Phase::Staged, Some(role))?;
        *record = next;
    }
    for role in PayloadRole::ALL {
        let original = port.observe_original(role)?;
        let mut next = record.clone();
        next.role_mut(role)?.original = original;
        persist(port, &mut next, Phase::BackupIntent, Some(role))?;
        *record = next;
        let observed = port.backup(record.operation(), role)?;
        let mut next = record.clone();
        next.role_mut(role)?.backup = observed;
        persist(port, &mut next, Phase::BackedUp, Some(role))?;
        *record = next;
        persist(port, record, Phase::PublishIntent, Some(role))?;
        let observed = port.publish(record.operation(), role)?;
        let mut next = record.clone();
        next.role_mut(role)?.published = Some(observed);
        persist(port, &mut next, Phase::Published, Some(role))?;
        *record = next;
    }
    let verified = port.verify(record.operation())?;
    finish(port, record, verified, None)
}
/// `already_started` comes only from a fresh actual observer; no start is replayed on reopen.
pub(crate) fn finish<P: PayloadPort>(
    port: &mut P,
    record: &mut OperationRecord,
    verified: P::Verified,
    already_started: Option<P::Started>,
) -> NativeResult<()> {
    let started = match already_started {
        Some(started) => started,
        None => {
            persist(port, record, Phase::StartIntent, None)?;
            port.start(record.operation(), &verified)?
        }
    };
    let instance = port.health(record.operation(), &started, &verified)?;
    if record.original_instance() == Some(instance.as_str()) {
        return Err(NativeError::Foreign);
    }
    let mut next = record.clone();
    next.set_new_instance(instance);
    persist(port, &mut next, Phase::NewInstanceObserved, None)?;
    *record = next;
    persist(port, record, Phase::Verified, None)?;
    persist(port, record, Phase::PruneIntent, None)?;
    let complete = port.prune(record.operation())?;
    let mut next = record.clone();
    next.set_retention_incomplete(!complete);
    persist(port, &mut next, Phase::Complete, None)?;
    *record = next;
    Ok(())
}
