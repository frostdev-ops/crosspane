//! One-shot stop/erase controller and identity-checked file-effect reconciliation.
use super::super::native_io::{NativeError, NativeResult};
use super::{
    RemovalCursor as Cursor, RemovalRecord, RemovalResult, StoppedTreeFacts, erase_receipt_fresh,
};
use crate::agent_contract::{EraseIdentityV1, LastExitV1};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NodePresence {
    Absent,
    Same,
    Changed,
}
/// Production ports hold genuine capabilities. Serializable records and fake observations
/// cannot implement the native capability factories or select arbitrary filesystem/processes.
pub(crate) trait RemovalPort {
    type Tree;
    fn renew(&mut self, record: &RemovalRecord) -> NativeResult<()>;
    fn persist(&mut self, record: &RemovalRecord) -> NativeResult<()>;
    fn commit_authorized(&mut self, record: &RemovalRecord) -> NativeResult<()>;
    fn stop_once(&mut self, record: &RemovalRecord) -> NativeResult<Self::Tree>;
    fn recover_stop(&mut self, record: &RemovalRecord) -> NativeResult<Option<Self::Tree>>;
    fn stopped_facts(&self, tree: &Self::Tree) -> NativeResult<StoppedTreeFacts>;
    fn renew_completion(&mut self, record: &RemovalRecord, tree: &Self::Tree) -> NativeResult<()>;
    fn receipt(
        &mut self,
        record: &RemovalRecord,
        tree: &Self::Tree,
    ) -> NativeResult<Option<LastExitV1>>;
    fn erase_once(
        &mut self,
        record: &RemovalRecord,
        tree: &Self::Tree,
    ) -> NativeResult<EraseIdentityV1>;
    fn recover_erase(
        &mut self,
        record: &RemovalRecord,
        tree: &Self::Tree,
    ) -> NativeResult<Option<EraseIdentityV1>>;
    fn task_absent(&mut self, record: &RemovalRecord) -> NativeResult<bool>;
    fn task_delete(&mut self, record: &RemovalRecord, tree: &Self::Tree) -> NativeResult<()>;
    fn observe_node(&mut self, record: &RemovalRecord, index: u16) -> NativeResult<NodePresence>;
    fn delete_node(
        &mut self,
        record: &RemovalRecord,
        index: u16,
        tree: &Self::Tree,
    ) -> NativeResult<()>;
    fn root_absent(&mut self, record: &RemovalRecord) -> NativeResult<bool>;
    fn settle_install_aliases(
        &mut self,
        record: &RemovalRecord,
        tree: &Self::Tree,
    ) -> NativeResult<()>;
    fn delete_root(&mut self, record: &RemovalRecord, tree: &Self::Tree) -> NativeResult<()>;
    fn executing_copy(&mut self, record: &RemovalRecord) -> NativeResult<Option<u8>>;
    fn copy_absent(&mut self, record: &RemovalRecord, index: u8) -> NativeResult<bool>;
    fn retire_copy(&mut self, record: &RemovalRecord, index: u8) -> NativeResult<()>;
    fn cleanup_final_copy(&mut self, record: &RemovalRecord, index: u8) -> NativeResult<()>;
}
/// Runtime attempts/caps are never serialized; cold intents can only observe retained owners.
pub(crate) struct RemovalController<T> {
    stop_attempted: bool,
    erase_attempted: bool,
    tree: Option<T>,
}
impl<T> Default for RemovalController<T> {
    fn default() -> Self {
        Self {
            stop_attempted: false,
            erase_attempted: false,
            tree: None,
        }
    }
}
fn persist<P: RemovalPort>(
    port: &mut P,
    record: &mut RemovalRecord,
    cursor: Cursor,
) -> NativeResult<()> {
    if record.rank(record.cursor())? >= record.rank(cursor)? {
        return Ok(());
    }
    let mut next = record.clone();
    next.advance(cursor)?;
    port.renew(record)?;
    port.persist(&next)?;
    *record = next;
    Ok(())
}
impl<T> RemovalController<T> {
    pub(crate) fn run<P: RemovalPort<Tree = T>>(
        &mut self,
        port: &mut P,
        record: &mut RemovalRecord,
    ) -> NativeResult<RemovalResult> {
        record.validate()?;
        port.renew(record)?;
        if matches!(
            record.cursor(),
            Cursor::Complete { .. }
                | Cursor::FinalCopyCleanupIntent { .. }
                | Cursor::FinalCopyAbsent { .. }
                | Cursor::Retired
        ) {
            return self.terminal(port, record);
        }
        if record.cursor() == Cursor::Committed {
            port.commit_authorized(record)?;
            if self.stop_attempted {
                return Ok(RemovalResult::Retained);
            }
            self.stop_attempted = true;
            persist(port, record, Cursor::StopIntent)?;
            self.tree = Some(port.stop_once(record)?);
        } else if record.rank(record.cursor())? < record.rank(Cursor::StopIntent)? {
            // Preparation/Ready alone never commits a destructive removal.
            return Ok(RemovalResult::Retained);
        }
        if self.tree.is_none() {
            self.tree = port.recover_stop(record)?;
        }
        let Some(tree) = self.tree.as_ref() else {
            return Ok(RemovalResult::Retained);
        };
        port.renew_completion(record, tree)?;
        let actual = port.stopped_facts(tree)?;
        if record.cursor() == Cursor::StopIntent {
            let mut next = record.clone();
            next.set_stopped(actual)?;
            next.advance(Cursor::Stopped)?;
            port.persist(&next)?;
            *record = next;
        } else if record.stopped() != Some(actual) {
            return Err(NativeError::Foreign);
        }
        if record.rank(record.cursor())? < record.rank(Cursor::TaskDeleteIntent)? {
            if record.options().erase_identity {
                let receipt = port.receipt(record, tree)?.ok_or(NativeError::Missing)?;
                erase_receipt_fresh(actual, &receipt)?;
                let result = if record.cursor() == Cursor::Stopped {
                    if self.erase_attempted {
                        return Ok(RemovalResult::Retained);
                    }
                    self.erase_attempted = true;
                    persist(port, record, Cursor::EraseIntent)?;
                    port.renew_completion(record, tree)?;
                    Some(port.erase_once(record, tree)?)
                } else if record.cursor() == Cursor::EraseIntent {
                    port.recover_erase(record, tree)?
                } else {
                    None
                };
                if record.cursor() == Cursor::EraseIntent {
                    let Some(result) = result else {
                        return Ok(RemovalResult::Retained);
                    };
                    if !result.identity_and_pairings_removed() {
                        return Ok(RemovalResult::Retained);
                    }
                    persist(port, record, Cursor::EraseDone)?;
                }
            } else {
                persist(port, record, Cursor::EraseSkipped)?;
            }
        }
        port.renew_completion(record, tree)?;
        if !port.task_absent(record)? {
            if record.rank(record.cursor())? > record.rank(Cursor::TaskDeleteIntent)? {
                return Ok(RemovalResult::Retained);
            }
            persist(port, record, Cursor::TaskDeleteIntent)?;
            port.renew_completion(record, tree)?;
            port.task_delete(record, tree)?;
            if !port.task_absent(record)? {
                return Ok(RemovalResult::Retained);
            }
        }
        persist(port, record, Cursor::TaskAbsent)?;
        for index in record.plan().order().to_vec() {
            let intent = Cursor::DeleteIntent { index };
            let done = Cursor::NodeAbsent { index };
            match port.observe_node(record, index)? {
                NodePresence::Changed => return Ok(RemovalResult::Retained),
                NodePresence::Absent => {}
                NodePresence::Same => {
                    if record.rank(record.cursor())? > record.rank(intent)? {
                        return Ok(RemovalResult::Retained);
                    }
                    persist(port, record, intent)?;
                    port.renew_completion(record, tree)?;
                    port.delete_node(record, index, tree)?;
                    if port.observe_node(record, index)? != NodePresence::Absent {
                        return Ok(RemovalResult::Retained);
                    }
                }
            }
            persist(port, record, done)?;
        }
        if !port.root_absent(record)? {
            if record.rank(record.cursor())? > record.rank(Cursor::RootDeleteIntent)? {
                return Ok(RemovalResult::Retained);
            }
            port.settle_install_aliases(record, tree)?;
            persist(port, record, Cursor::RootDeleteIntent)?;
            port.renew_completion(record, tree)?;
            port.delete_root(record, tree)?;
            if !port.root_absent(record)? {
                return Ok(RemovalResult::Retained);
            }
        }
        persist(port, record, Cursor::InstallAbsent)?;
        let executing = port.executing_copy(record)?;
        for index in 0..record.plan().copies().len() {
            let index = index as u8;
            if executing == Some(index) {
                continue;
            }
            if !port.copy_absent(record, index)? {
                let intent = Cursor::CopyDeleteIntent { index };
                if record.rank(record.cursor())? > record.rank(intent)? {
                    return Ok(RemovalResult::Retained);
                }
                persist(port, record, intent)?;
                port.retire_copy(record, index)?;
                if !port.copy_absent(record, index)? {
                    return Ok(RemovalResult::Retained);
                }
            }
            persist(port, record, Cursor::CopyAbsent { index })?;
        }
        persist(
            port,
            record,
            Cursor::Complete {
                retained_copy: executing,
            },
        )?;
        self.terminal(port, record)
    }
    fn terminal<P: RemovalPort<Tree = T>>(
        &mut self,
        port: &mut P,
        record: &mut RemovalRecord,
    ) -> NativeResult<RemovalResult> {
        if !port.task_absent(record)? || !port.root_absent(record)? {
            return Ok(RemovalResult::Retained);
        }
        let index = match record.cursor() {
            Cursor::Complete { retained_copy } => retained_copy,
            Cursor::FinalCopyCleanupIntent { index } | Cursor::FinalCopyAbsent { index } => {
                Some(index)
            }
            Cursor::Retired => None,
            _ => return Err(NativeError::Foreign),
        };
        for i in 0..record.plan().copies().len() {
            if index != Some(i as u8) && !port.copy_absent(record, i as u8)? {
                return Ok(RemovalResult::Retained);
            }
        }
        if let Some(index) = index {
            if port.executing_copy(record)? == Some(index) {
                return Ok(RemovalResult::RemovedWithRetainedCopy { index });
            }
            if !port.copy_absent(record, index)? {
                persist(port, record, Cursor::FinalCopyCleanupIntent { index })?;
                // Distinct actual-exit or FILE-only reopen cleanup, never reconstructed tree proof.
                port.cleanup_final_copy(record, index)?;
                if !port.copy_absent(record, index)? {
                    return Ok(RemovalResult::Retained);
                }
            }
            persist(port, record, Cursor::FinalCopyAbsent { index })?;
        }
        persist(port, record, Cursor::Retired)?;
        Ok(RemovalResult::Removed)
    }
}
