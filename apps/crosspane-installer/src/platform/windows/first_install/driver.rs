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
    /// W4.1c2. After FilesVerified is durable and before TaskIntent; at most once per forward
    /// Apply; never on a reopen. Ok: the install continues whatever the elevated result. Err:
    /// install Unknown.
    fn elevated(&mut self, _record: &FirstInstallRecord) -> NativeResult<()> {
        Ok(())
    }
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
    // Forward Apply only. A reopen never repeats the elevated step.
    if !reopening && record.phase() == Phase::FilesVerified {
        port.elevated(record)?;
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

/// One pair observation; content is bounded admitted metadata, never executable approval.
pub(crate) type HistoryObservation = (FileStamp, Vec<u8>);
pub(crate) trait FirstHistoryPort {
    fn renew(&mut self, intent: &super::record::FirstHistoryIntent) -> NativeResult<()>;
    fn persist_intent(&mut self, intent: &super::record::FirstHistoryIntent) -> NativeResult<()>;
    fn observe_pair(
        &mut self,
        intent: &super::record::FirstHistoryIntent,
        leaf: &super::record::HistoryLeaf,
    ) -> NativeResult<(Option<HistoryObservation>, Option<HistoryObservation>)>;
    fn move_exact(
        &mut self,
        intent: &super::record::FirstHistoryIntent,
        leaf: &super::record::HistoryLeaf,
    ) -> NativeResult<()>;
    fn persist_index(&mut self, index: &super::record::FirstHistoryIndex) -> NativeResult<()>;
}
/// On reopen, settled moves are observed, never repeated. An ambiguous pair never admits launch.
pub(crate) fn archive_history<P: FirstHistoryPort>(
    port: &mut P,
    intent: &mut super::record::FirstHistoryIntent,
    index: &mut super::record::FirstHistoryIndex,
) -> NativeResult<()> {
    intent.validate()?;
    index.validate()?;
    let slot = index.slots[usize::from(intent.slot)].as_ref();
    if slot.is_some_and(|slot| slot != &intent.selected) {
        return Err(NativeError::Foreign);
    }
    if intent.complete && slot != Some(&intent.selected) {
        return Err(NativeError::OutcomeUnknown);
    }
    port.renew(intent)?;
    port.persist_intent(intent)?;
    for (position, leaf) in intent.selected.leaves.clone().iter().enumerate() {
        port.renew(intent)?;
        let (source, destination) = port.observe_pair(intent, leaf)?;
        match (source, destination) {
            (Some((id, bytes)), None)
                if leaf.matches(id, &bytes) && position >= intent.moved && !intent.complete =>
            {
                port.move_exact(intent, leaf)?;
                let (source, destination) = port.observe_pair(intent, leaf)?;
                if source.is_some()
                    || destination
                        .as_ref()
                        .is_none_or(|(id, bytes)| !leaf.matches(*id, bytes))
                {
                    return Err(NativeError::OutcomeUnknown);
                }
            }
            (None, Some((id, bytes))) if leaf.matches(id, &bytes) => {}
            _ => return Err(NativeError::OutcomeUnknown),
        }
        if position >= intent.moved {
            let mut next = intent.clone();
            next.moved = position + 1;
            port.persist_intent(&next)?;
            *intent = next;
        }
    }
    let mut next = index.clone();
    next.commit(intent)?;
    port.persist_index(&next)?;
    *index = next;
    let mut complete = intent.clone();
    complete.complete = true;
    port.persist_intent(&complete)?;
    *intent = complete;
    port.renew(intent)
}

pub(crate) trait FirstRecoveryPort {
    fn renew_recovery(&mut self, recovery: &super::record::FirstRecoveryRecord)
    -> NativeResult<()>;
    fn persist_recovery(
        &mut self,
        recovery: &super::record::FirstRecoveryRecord,
    ) -> NativeResult<()>;
    fn task_absent(&mut self, recovery: &super::record::FirstRecoveryRecord) -> NativeResult<bool>;
    fn delete_task(&mut self, recovery: &super::record::FirstRecoveryRecord) -> NativeResult<()>;
    fn triad(
        &mut self,
        recovery: &super::record::FirstRecoveryRecord,
        role: PayloadRole,
    ) -> NativeResult<super::record::RoleTriad>;
    fn unpublish(
        &mut self,
        recovery: &super::record::FirstRecoveryRecord,
        role: PayloadRole,
    ) -> NativeResult<()>;
    fn restore_original(
        &mut self,
        recovery: &super::record::FirstRecoveryRecord,
        role: PayloadRole,
    ) -> NativeResult<()>;
    fn delete_stage(
        &mut self,
        recovery: &super::record::FirstRecoveryRecord,
        role: PayloadRole,
    ) -> NativeResult<()>;
    fn settle_scaffold(
        &mut self,
        recovery: &super::record::FirstRecoveryRecord,
    ) -> NativeResult<bool>;
    fn retire_first(&mut self, recovery: &super::record::FirstRecoveryRecord) -> NativeResult<()>;
}
fn recovery_save<P: FirstRecoveryPort>(
    port: &mut P,
    record: &mut super::record::FirstRecoveryRecord,
    cursor: super::record::FirstRecoveryCursor,
) -> NativeResult<()> {
    let mut next = record.clone();
    next.advance(cursor)?;
    port.renew_recovery(record)?;
    port.persist_recovery(&next)?;
    *record = next;
    Ok(())
}
/// File-only rollback. An already completed NO_REPLACE move is observed rather than repeated.
pub(crate) fn recover_first<P: FirstRecoveryPort>(
    port: &mut P,
    record: &mut super::record::FirstRecoveryRecord,
) -> NativeResult<super::record::FirstRecoveryOutcome> {
    use super::record::{
        FirstRecoveryCursor as C, FirstRecoveryMode as M, FirstRecoveryOutcome as O,
    };
    record.validate()?;
    if record.mode != M::Rollback {
        return Err(NativeError::Foreign);
    }
    port.renew_recovery(record)?;
    if record.cursor == C::Retired {
        return Ok(O::RolledBack);
    }
    if record.cursor == C::Selected {
        port.persist_recovery(record)?;
        recovery_save(port, record, C::TaskDeleteIntent)?;
    }
    if record.cursor == C::TaskDeleteIntent {
        if !port.task_absent(record)? {
            port.delete_task(record)?;
        }
        if !port.task_absent(record)? {
            return Err(NativeError::OutcomeUnknown);
        }
        recovery_save(port, record, C::TaskAbsent)?;
    }
    if !port.task_absent(record)? {
        return Err(NativeError::OutcomeUnknown);
    }
    for (index, role) in PayloadRole::ALL.into_iter().rev().enumerate() {
        let index = index as u8;
        let source = record.document.role(role)?.clone();
        // The persisted staged identity, or the recorded stray leaf for this role only.
        let identity = record.stage_identity(role)?;
        let point = |step| C::Role { index, step };
        let mut stage = 0;
        match record.cursor {
            C::TaskAbsent => {}
            C::Role { index: old, step } if old == index => stage = step,
            C::Role { index: old, .. } if old > index => continue,
            C::Role {
                index: old,
                step: 5,
            } if old + 1 == index => {}
            C::RolledBack | C::CleanupIntent | C::CleanupDone | C::RetireIntent | C::Retired => {
                break;
            }
            _ => return Err(NativeError::OutcomeUnknown),
        }
        if stage == 0 {
            recovery_save(port, record, point(0))?;
            let observed = port.triad(record, role)?;
            if observed.fixed.is_some() && observed.fixed == identity {
                if observed.stage.is_some() {
                    return Err(NativeError::OutcomeUnknown);
                }
                port.unpublish(record, role)?;
                let after = port.triad(record, role)?;
                if after.fixed.is_some() || after.stage != identity {
                    return Err(NativeError::OutcomeUnknown);
                }
            } else if observed.stage.is_some() && observed.stage != identity {
                return Err(NativeError::OutcomeUnknown);
            }
            recovery_save(port, record, point(1))?;
            stage = 1;
        }
        if stage <= 2 {
            recovery_save(port, record, point(2))?;
            let observed = port.triad(record, role)?;
            match source.original {
                OriginalLeaf::Present(id)
                    if observed.fixed.is_none() && observed.backup == Some(id) =>
                {
                    port.restore_original(record, role)?;
                    let after = port.triad(record, role)?;
                    if after.fixed != Some(id) || after.backup.is_some() {
                        return Err(NativeError::OutcomeUnknown);
                    }
                }
                OriginalLeaf::Present(id)
                    if observed.fixed == Some(id) && observed.backup.is_none() => {}
                OriginalLeaf::Missing if observed.fixed.is_none() && observed.backup.is_none() => {}
                OriginalLeaf::Unobserved
                    if observed.backup.is_none()
                        && (observed.fixed.is_none() || observed.fixed != identity) => {}
                _ => return Err(NativeError::OutcomeUnknown),
            }
            recovery_save(port, record, point(3))?;
            stage = 3;
        }
        if stage <= 4 {
            recovery_save(port, record, point(4))?;
            let observed = port.triad(record, role)?;
            match observed.stage {
                Some(id) if Some(id) == identity => {
                    port.delete_stage(record, role)?;
                }
                None => {}
                _ => return Err(NativeError::OutcomeUnknown),
            }
            if port.triad(record, role)?.stage.is_some() {
                return Err(NativeError::OutcomeUnknown);
            }
            recovery_save(port, record, point(5))?;
        }
    }
    if !matches!(
        record.cursor,
        C::RolledBack | C::CleanupIntent | C::CleanupDone | C::RetireIntent
    ) {
        recovery_save(port, record, C::RolledBack)?;
    }
    if record.cursor == C::RolledBack {
        recovery_save(port, record, C::CleanupIntent)?;
    }
    if record.cursor == C::CleanupIntent {
        if !port.settle_scaffold(record)? {
            return Err(NativeError::OutcomeUnknown);
        }
        recovery_save(port, record, C::CleanupDone)?;
    }
    if record.cursor == C::CleanupDone {
        recovery_save(port, record, C::RetireIntent)?;
    }
    port.retire_first(record)?;
    recovery_save(port, record, C::Retired)?;
    Ok(O::RolledBack)
}

/// A read-only live observation is supplied by the native sealed sibling. Absence must be
/// independently admitted; this port has no file/task/launch/Stop operations.
pub(crate) trait FirstStalePort {
    fn renew_stale(&mut self, record: &super::record::FirstRecoveryRecord) -> NativeResult<()>;
    fn observe_supersession(
        &mut self,
        record: &super::record::FirstRecoveryRecord,
    ) -> NativeResult<Option<u64>>;
    fn reserve_absent(&mut self, record: &super::record::FirstRecoveryRecord) -> NativeResult<()>;
    fn persist_stale(&mut self, record: &super::record::FirstRecoveryRecord) -> NativeResult<()>;
    fn retire_stale(&mut self, record: &super::record::FirstRecoveryRecord) -> NativeResult<()>;
}
pub(crate) fn settle_stale_first<P: FirstStalePort>(
    port: &mut P,
    record: &mut super::record::FirstRecoveryRecord,
) -> NativeResult<super::record::FirstRecoveryOutcome> {
    use super::record::{
        FirstRecoveryCursor as C, FirstRecoveryMode as M, FirstRecoveryOutcome as O,
    };
    record.validate()?;
    super::recovery_mode_allowed(&record.document, record.mode)?;
    if !matches!(record.mode, M::Supersede | M::RetireStale) {
        return Err(NativeError::Foreign);
    }
    port.renew_stale(record)?;
    if record.mode == M::Supersede {
        if port.observe_supersession(record)?.is_none_or(|i| i == 0) {
            return Err(NativeError::OutcomeUnknown);
        }
        if record.cursor != C::Superseded {
            let mut next = record.clone();
            next.advance(C::Superseded)?;
            port.persist_stale(&next)?;
            *record = next;
        }
        return Ok(O::Superseded);
    }
    port.reserve_absent(record)?;
    if record.cursor == C::Retired {
        return Ok(O::Retired);
    }
    if record.cursor == C::Selected {
        port.persist_stale(record)?;
        let mut next = record.clone();
        next.advance(C::RetireIntent)?;
        port.persist_stale(&next)?;
        *record = next;
    }
    if record.cursor != C::RetireIntent {
        return Err(NativeError::Foreign);
    }
    port.retire_stale(record)?;
    let mut next = record.clone();
    next.advance(C::Retired)?;
    port.persist_stale(&next)?;
    *record = next;
    Ok(O::Retired)
}
