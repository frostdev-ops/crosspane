//! Intent-before-effect first install. This port has no Stop or keeper authority.
use super::{
    super::{
        native_io::{NativeError, NativeResult},
        payload::{
            inventory::PayloadRole,
            recovery::{FileStamp, ImageObservation, OriginalLeaf},
        },
    },
    record::{FirstInstallRecord, Phase},
};

pub(crate) trait FirstInstallPort {
    fn renew(&mut self, record: &FirstInstallRecord) -> NativeResult<()>;
    fn persist(&mut self, record: &FirstInstallRecord) -> NativeResult<()>;
    fn stage(
        &mut self,
        record: &FirstInstallRecord,
        role: PayloadRole,
    ) -> NativeResult<ImageObservation>;
    fn observe_stage(
        &mut self,
        record: &FirstInstallRecord,
        role: PayloadRole,
    ) -> NativeResult<Option<ImageObservation>>;
    fn original(
        &mut self,
        record: &FirstInstallRecord,
        role: PayloadRole,
    ) -> NativeResult<OriginalLeaf>;
    fn backup(
        &mut self,
        record: &FirstInstallRecord,
        role: PayloadRole,
    ) -> NativeResult<Option<FileStamp>>;
    fn observe_backup(
        &mut self,
        record: &FirstInstallRecord,
        role: PayloadRole,
    ) -> NativeResult<Option<FileStamp>>;
    fn publish(
        &mut self,
        record: &FirstInstallRecord,
        role: PayloadRole,
    ) -> NativeResult<ImageObservation>;
    fn observe_fixed(
        &mut self,
        record: &FirstInstallRecord,
        role: PayloadRole,
    ) -> NativeResult<Option<ImageObservation>>;
    fn verify_files(&mut self, record: &FirstInstallRecord) -> NativeResult<()>;
    /// The actual a3 adapter publishes TaskRegistered and RunIntent before their corresponding
    /// effects. This returns its freshly reread first record, including the genuine Run result.
    fn activate(&mut self, record: &FirstInstallRecord) -> NativeResult<FirstInstallRecord>;
    fn observe_running(&mut self, record: &FirstInstallRecord) -> NativeResult<Option<u64>>;
    fn prune(&mut self, record: &FirstInstallRecord) -> NativeResult<bool>;
}
fn save<P: FirstInstallPort>(
    port: &mut P,
    record: &mut FirstInstallRecord,
    next: FirstInstallRecord,
) -> NativeResult<()> {
    next.validate()?;
    if !next.same_selection(record) {
        return Err(NativeError::Foreign);
    }
    port.renew(record)?;
    port.persist(&next)?;
    *record = next;
    Ok(())
}
fn phase<P: FirstInstallPort>(
    port: &mut P,
    record: &mut FirstInstallRecord,
    value: Phase,
) -> NativeResult<()> {
    let mut next = record.clone();
    next.advance(value)?;
    save(port, record, next)
}
pub(crate) fn apply<P: FirstInstallPort>(
    port: &mut P,
    record: &mut FirstInstallRecord,
) -> NativeResult<()> {
    if record.phase() != Phase::Intent {
        return Err(NativeError::OutcomeUnknown);
    }
    port.persist(record)?;
    drive(port, record, false)
}
pub(crate) fn resume<P: FirstInstallPort>(
    port: &mut P,
    record: &mut FirstInstallRecord,
) -> NativeResult<()> {
    drive(port, record, true)
}
fn drive<P: FirstInstallPort>(
    port: &mut P,
    record: &mut FirstInstallRecord,
    reopening: bool,
) -> NativeResult<()> {
    record.validate()?;
    port.renew(record)?;
    if record.phase() == Phase::Unknown {
        return Err(NativeError::OutcomeUnknown);
    }
    if record.phase() == Phase::Complete {
        return Ok(());
    }
    for role in PayloadRole::ALL {
        if record.phase().rank() < Phase::Staged(role).rank() {
            let observed = if reopening && record.phase() == Phase::StageIntent(role) {
                port.observe_stage(record, role)?
                    .ok_or(NativeError::OutcomeUnknown)?
            } else {
                phase(port, record, Phase::StageIntent(role))?;
                port.stage(record, role)?
            };
            let mut next = record.clone();
            next.role_mut(role)?.staged = Some(observed);
            next.advance(Phase::Staged(role))?;
            save(port, record, next)?;
        }
    }
    for role in PayloadRole::ALL {
        if record.phase().rank() < Phase::BackedUp(role).rank() {
            let backup = if reopening && record.phase() == Phase::BackupIntent(role) {
                let original = record.role(role)?.original;
                let actual = port.observe_backup(record, role)?;
                match (original, actual, port.original(record, role)?) {
                    (OriginalLeaf::Missing, None, OriginalLeaf::Missing) => None,
                    (OriginalLeaf::Present(id), Some(actual), OriginalLeaf::Missing)
                        if id == actual =>
                    {
                        Some(actual)
                    }
                    _ => return Err(NativeError::OutcomeUnknown),
                }
            } else {
                let original = port.original(record, role)?;
                let mut next = record.clone();
                next.role_mut(role)?.original = original;
                next.advance(Phase::BackupIntent(role))?;
                save(port, record, next)?;
                port.backup(record, role)?
            };
            let mut next = record.clone();
            next.role_mut(role)?.backup = backup;
            next.advance(Phase::BackedUp(role))?;
            save(port, record, next)?;
        }
        if record.phase().rank() < Phase::Published(role).rank() {
            let published = if reopening && record.phase() == Phase::PublishIntent(role) {
                if port.observe_stage(record, role)?.is_some() {
                    return Err(NativeError::OutcomeUnknown);
                }
                port.observe_fixed(record, role)?
                    .ok_or(NativeError::OutcomeUnknown)?
            } else {
                phase(port, record, Phase::PublishIntent(role))?;
                port.publish(record, role)?
            };
            let mut next = record.clone();
            next.role_mut(role)?.published = Some(published);
            next.advance(Phase::Published(role))?;
            save(port, record, next)?;
        }
    }
    port.verify_files(record)?;
    if record.phase().rank() < Phase::FilesVerified.rank() {
        phase(port, record, Phase::FilesVerified)?;
    }
    if record.phase().rank() < Phase::RunObserved.rank() {
        if reopening && record.phase().rank() >= Phase::TaskIntent.rank() {
            // A registration/Run may have been consumed. Genuine readiness can settle it below,
            // but an absent observer never grants another dispatch.
            let instance = port
                .observe_running(record)?
                .ok_or(NativeError::OutcomeUnknown)?;
            let mut next = record.clone();
            next.ready(instance)?;
            save(port, record, next)?;
        } else {
            phase(port, record, Phase::TaskIntent)?;
            let actual = port.activate(record)?;
            if !actual.same_selection(record) || actual.phase() != Phase::RunObserved {
                return Err(NativeError::OutcomeUnknown);
            }
            actual.validate()?;
            *record = actual;
        }
    }
    if record.phase().rank() < Phase::Ready.rank() {
        let instance = port
            .observe_running(record)?
            .ok_or(NativeError::OutcomeUnknown)?;
        let mut next = record.clone();
        next.ready(instance)?;
        save(port, record, next)?;
    }
    if record.phase().rank() < Phase::PruneIntent.rank() {
        phase(port, record, Phase::PruneIntent)?;
    }
    let complete = port.prune(record)?;
    let mut next = record.clone();
    next.retention(!complete);
    next.advance(Phase::Complete)?;
    save(port, record, next)
}
