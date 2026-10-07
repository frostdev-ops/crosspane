//! Separate strict removal journal; metadata never grants image, tree, task or file authority.
#[path = "removal/executor.rs"]
pub(crate) mod executor;
#[path = "removal/inventory.rs"]
pub(crate) mod inventory;
#[path = "removal/plan.rs"]
pub(crate) mod plan;
use super::{
    native_io::{NativeError, NativeResult, records},
    payload::recovery::OuterContextCorrelation,
    service::supervisor::Generation,
};
use plan::RemovalPlan;
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemovalOptions {
    pub erase_identity: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoppedTreeFacts {
    pub generation: Generation,
    pub started_unix_ms: u64,
}
impl StoppedTreeFacts {
    pub(crate) fn validate(&self) -> NativeResult<()> {
        if self.generation.pid == 0
            || self.generation.creation == 0
            || self.generation.instance == 0
            || self.started_unix_ms == 0
        {
            Err(NativeError::Invalid)
        } else {
            Ok(())
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum RemovalCursor {
    Selected,
    Committed,
    StopIntent,
    Stopped,
    EraseIntent,
    EraseDone,
    EraseSkipped,
    TaskDeleteIntent,
    TaskAbsent,
    DeleteIntent { index: u16 },
    NodeAbsent { index: u16 },
    RootDeleteIntent,
    InstallAbsent,
    CopyDeleteIntent { index: u8 },
    CopyAbsent { index: u8 },
    Complete { retained_copy: Option<u8> },
    FinalCopyCleanupIntent { index: u8 },
    FinalCopyAbsent { index: u8 },
    Retired,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum RemovalHandoffStage {
    None,
    CopyPrepareIntent { index: u8 },
    CopyPrepared { index: u8 },
    CreateIntent { index: u8 },
    Created { index: u8 },
    ResumeIntent { index: u8 },
    Ready { index: u8 },
    RemovalCommitIntent { index: u8 },
    Committed { index: u8 },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemovalRecord {
    schema_version: u32,
    operation: [u8; 16],
    context: OuterContextCorrelation,
    options: RemovalOptions,
    plan: RemovalPlan,
    stopped: Option<StoppedTreeFacts>,
    cursor: RemovalCursor,
    handoff: RemovalHandoffStage,
}
impl RemovalRecord {
    pub(crate) fn new(
        operation: [u8; 16],
        context: OuterContextCorrelation,
        options: RemovalOptions,
        plan: RemovalPlan,
    ) -> NativeResult<Self> {
        let r = Self {
            schema_version: 1,
            operation,
            context,
            options,
            plan,
            stopped: None,
            cursor: RemovalCursor::Selected,
            handoff: RemovalHandoffStage::None,
        };
        r.validate()?;
        Ok(r)
    }
    pub(crate) fn operation(&self) -> [u8; 16] {
        self.operation
    }
    pub(crate) fn context(&self) -> &OuterContextCorrelation {
        &self.context
    }
    pub(crate) fn options(&self) -> RemovalOptions {
        self.options
    }
    pub(crate) fn plan(&self) -> &RemovalPlan {
        &self.plan
    }
    pub(crate) fn stopped(&self) -> Option<StoppedTreeFacts> {
        self.stopped
    }
    pub(crate) fn cursor(&self) -> RemovalCursor {
        self.cursor
    }
    pub(crate) fn handoff_stage(&self) -> RemovalHandoffStage {
        self.handoff
    }
    pub(crate) fn bind_copy_image(
        &mut self,
        index: u8,
        id: super::payload::recovery::FileStamp,
        image: super::payload::inventory::PeFacts,
    ) -> NativeResult<()> {
        self.validate()?;
        if self.handoff != (RemovalHandoffStage::CopyPrepareIntent { index }) {
            return Err(NativeError::Foreign);
        }
        self.plan.copy_mut(index)?.bind_image(id, image)
    }
    pub(crate) fn bind_copy_child(
        &mut self,
        index: u8,
        pid: u32,
        creation: u64,
        parent: u64,
    ) -> NativeResult<()> {
        self.validate()?;
        if self.handoff != (RemovalHandoffStage::CreateIntent { index }) {
            return Err(NativeError::Foreign);
        }
        self.plan.copy_mut(index)?.bind_child(pid, creation, parent)
    }
    pub(crate) fn advance_handoff(&mut self, next: RemovalHandoffStage) -> NativeResult<()> {
        use RemovalHandoffStage::*;
        use inventory::CopyStage;
        self.validate()?;
        if self.cursor != RemovalCursor::Selected {
            return Err(NativeError::Foreign);
        }
        let (index, stage) = match (self.handoff, next) {
            (None, CopyPrepareIntent { index }) if index == 0 => (index, CopyStage::PrepareIntent),
            (Ready { index: old }, CopyPrepareIntent { index }) if index == old + 1 => {
                (index, CopyStage::PrepareIntent)
            }
            (CopyPrepareIntent { index: a }, CopyPrepared { index }) if a == index => {
                (index, CopyStage::Prepared)
            }
            (CopyPrepared { index: a }, CreateIntent { index }) if a == index => {
                (index, CopyStage::CreateIntent)
            }
            (CreateIntent { index: a }, Created { index }) if a == index => {
                (index, CopyStage::Created)
            }
            (Created { index: a }, ResumeIntent { index }) if a == index => {
                (index, CopyStage::ResumeIntent)
            }
            (ResumeIntent { index: a }, Ready { index }) if a == index => (index, CopyStage::Ready),
            (Ready { index: a }, RemovalCommitIntent { index })
                if a == index
                    && self
                        .plan
                        .copies()
                        .iter()
                        .all(|c| c.stage() == CopyStage::Ready) =>
            {
                (index, CopyStage::Ready)
            }
            (RemovalCommitIntent { index: a }, Committed { index }) if a == index => {
                (index, CopyStage::Ready)
            }
            _ => return Err(NativeError::Foreign),
        };
        self.plan.copy_mut(index)?.stage = stage;
        self.handoff = next;
        if matches!(next, Committed { .. }) {
            self.cursor = RemovalCursor::Committed;
        }
        self.validate()
    }
    pub(crate) fn set_stopped(&mut self, facts: StoppedTreeFacts) -> NativeResult<()> {
        facts.validate()?;
        if self.cursor != RemovalCursor::StopIntent || self.stopped.is_some() {
            return Err(NativeError::Foreign);
        }
        self.stopped = Some(facts);
        Ok(())
    }
    pub(crate) fn validate(&self) -> NativeResult<()> {
        if self.schema_version != 1 || self.operation == [0; 16] {
            return Err(NativeError::Invalid);
        }
        self.plan.validate()?;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ContextFields {
            user: Vec<u8>,
            logon: Vec<u8>,
            authentication_id: u64,
            session: u32,
        }
        let f: ContextFields = serde_json::from_value(
            serde_json::to_value(&self.context).map_err(|_| NativeError::Invalid)?,
        )
        .map_err(|_| NativeError::Invalid)?;
        let facts = super::native_io::identity::TokenFacts {
            user: super::native_io::identity::Sid::from_bytes(f.user)?,
            logon: super::native_io::identity::Sid::from_bytes(f.logon)?,
            authentication_id: f.authentication_id,
            session: f.session,
            elevated: false,
            integrity: 0x2000,
            impersonating: false,
        };
        self.context.matches(&facts)?;
        if let Some(s) = self.stopped {
            s.validate()?;
        }
        if self.rank(self.cursor)? >= 3 && self.stopped.is_none() {
            return Err(NativeError::Invalid);
        }
        if self.cursor == RemovalCursor::EraseSkipped && self.options.erase_identity
            || matches!(
                self.cursor,
                RemovalCursor::EraseIntent | RemovalCursor::EraseDone
            ) && !self.options.erase_identity
        {
            return Err(NativeError::Invalid);
        }
        if self.cursor != RemovalCursor::Selected
            && !matches!(self.handoff, RemovalHandoffStage::Committed { .. })
        {
            return Err(NativeError::Invalid);
        }
        match self.handoff {
            RemovalHandoffStage::None => {
                if self
                    .plan
                    .copies()
                    .iter()
                    .any(|c| c.stage() != inventory::CopyStage::Planned)
                {
                    return Err(NativeError::Invalid);
                }
            }
            RemovalHandoffStage::CopyPrepareIntent { index }
            | RemovalHandoffStage::CopyPrepared { index }
            | RemovalHandoffStage::CreateIntent { index }
            | RemovalHandoffStage::Created { index }
            | RemovalHandoffStage::ResumeIntent { index }
            | RemovalHandoffStage::Ready { index }
            | RemovalHandoffStage::RemovalCommitIntent { index }
            | RemovalHandoffStage::Committed { index } => {
                use inventory::CopyStage;
                let expected = match self.handoff {
                    RemovalHandoffStage::CopyPrepareIntent { .. } => CopyStage::PrepareIntent,
                    RemovalHandoffStage::CopyPrepared { .. } => CopyStage::Prepared,
                    RemovalHandoffStage::CreateIntent { .. } => CopyStage::CreateIntent,
                    RemovalHandoffStage::Created { .. } => CopyStage::Created,
                    RemovalHandoffStage::ResumeIntent { .. } => CopyStage::ResumeIntent,
                    _ => CopyStage::Ready,
                };
                if usize::from(index) >= self.plan.copies().len() {
                    return Err(NativeError::Invalid);
                }
                for (i, copy) in self.plan.copies().iter().enumerate() {
                    let target = if i < usize::from(index) {
                        CopyStage::Ready
                    } else if i == usize::from(index) {
                        expected
                    } else {
                        CopyStage::Planned
                    };
                    if copy.stage() != target {
                        return Err(NativeError::Invalid);
                    }
                }
            }
        }
        self.rank(self.cursor)?;
        Ok(())
    }
    pub(crate) fn rank(&self, cursor: RemovalCursor) -> NativeResult<u32> {
        use RemovalCursor::*;
        let n = self.plan.order().len() as u32;
        let c = self.plan.copies().len() as u32;
        Ok(match cursor {
            Selected => 0,
            Committed => 1,
            StopIntent => 2,
            Stopped => 3,
            EraseIntent => 4,
            EraseDone | EraseSkipped => 5,
            TaskDeleteIntent => 6,
            TaskAbsent => 7,
            DeleteIntent { index } | NodeAbsent { index } => {
                let pos = self
                    .plan
                    .order()
                    .iter()
                    .position(|n| *n == index)
                    .ok_or(NativeError::Invalid)? as u32;
                8 + pos * 2 + u32::from(matches!(cursor, NodeAbsent { .. }))
            }
            RootDeleteIntent => 8 + n * 2,
            InstallAbsent => 9 + n * 2,
            CopyDeleteIntent { index } | CopyAbsent { index } => {
                if u32::from(index) >= c {
                    return Err(NativeError::Invalid);
                }
                10 + n * 2 + u32::from(index) * 2 + u32::from(matches!(cursor, CopyAbsent { .. }))
            }
            Complete { retained_copy } => {
                if retained_copy.is_some_and(|i| usize::from(i) >= self.plan.copies().len()) {
                    return Err(NativeError::Invalid);
                }
                10 + n * 2 + c * 2
            }
            FinalCopyCleanupIntent { index } | FinalCopyAbsent { index } => {
                if usize::from(index) >= self.plan.copies().len() {
                    return Err(NativeError::Invalid);
                }
                11 + n * 2 + c * 2 + u32::from(matches!(cursor, FinalCopyAbsent { .. }))
            }
            Retired => 13 + n * 2 + c * 2,
        })
    }
    pub(crate) fn advance(&mut self, cursor: RemovalCursor) -> NativeResult<()> {
        self.validate()?;
        if self.rank(cursor)? <= self.rank(self.cursor)? {
            return Err(NativeError::Foreign);
        }
        if matches!(cursor, RemovalCursor::FinalCopyCleanupIntent { .. })
            && !matches!(
                self.cursor,
                RemovalCursor::Complete {
                    retained_copy: Some(_)
                }
            )
        {
            return Err(NativeError::Foreign);
        }
        if let (
            RemovalCursor::Complete {
                retained_copy: Some(old),
            },
            RemovalCursor::FinalCopyCleanupIntent { index },
        ) = (self.cursor, cursor)
            && old != index
        {
            return Err(NativeError::Foreign);
        }
        self.cursor = cursor;
        self.validate()
    }
    pub(crate) fn same_selection(&self, other: &Self) -> NativeResult<()> {
        self.validate()?;
        other.validate()?;
        if self.operation != other.operation
            || self.context != other.context
            || self.options != other.options
            || self.stopped.is_some() && self.stopped != other.stopped
        {
            return Err(NativeError::Foreign);
        }
        self.plan.same_files(&other.plan)
    }
    /// Fresh publication may fill exact-once observations but may never discard a later intent.
    pub(crate) fn publication_successor(&self, next: &Self) -> NativeResult<()> {
        self.same_selection(next)?;
        if next.rank(next.cursor)? < self.rank(self.cursor)? {
            return Err(NativeError::Foreign);
        }
        use RemovalHandoffStage::*;
        let order = |stage| match stage {
            None => 0,
            CopyPrepareIntent { index } => 1 + u32::from(index) * 7,
            CopyPrepared { index } => 2 + u32::from(index) * 7,
            CreateIntent { index } => 3 + u32::from(index) * 7,
            Created { index } => 4 + u32::from(index) * 7,
            ResumeIntent { index } => 5 + u32::from(index) * 7,
            Ready { index } => 6 + u32::from(index) * 7,
            RemovalCommitIntent { .. } => 1 + self.plan.copies().len() as u32 * 7,
            Committed { .. } => 2 + self.plan.copies().len() as u32 * 7,
        };
        if order(next.handoff) < order(self.handoff) {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
    pub(crate) fn encode(&self) -> NativeResult<Vec<u8>> {
        self.validate()?;
        records::encode_record(
            &records::RecordName::Removal,
            serde_json::to_value(self).map_err(|_| NativeError::Invalid)?,
        )
    }
    pub(crate) fn decode(bytes: &[u8]) -> NativeResult<Self> {
        let r: Self = records::record_data(&records::RecordName::Removal, bytes)?;
        r.validate()?;
        Ok(r)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemovalResult {
    Retained,
    Removed,
    RemovedWithRetainedCopy { index: u8 },
}
/// Freshness only. A native caller still needs unchanged genuine retained-tree completion.
pub(crate) fn erase_receipt_fresh(
    stopped: StoppedTreeFacts,
    receipt: &crate::agent_contract::LastExitV1,
) -> NativeResult<()> {
    use crate::agent_contract::ParkingExit;
    stopped.validate()?;
    if receipt.schema_version != 1
        || receipt.instance_id != stopped.generation.instance
        || receipt.stopped_unix_ms < stopped.started_unix_ms
        || !receipt.clean
        || !receipt.input_journals_empty
        || !receipt.audio_stopped
        || receipt.parking == ParkingExit::Failed
    {
        return Err(NativeError::Foreign);
    }
    Ok(())
}

/// Selection policy only: native callers supply actual locked operation observations.
pub(crate) fn admit_removal_selection(upgrade_unresolved: bool) -> NativeResult<()> {
    if upgrade_unresolved {
        Err(NativeError::Busy)
    } else {
        Ok(())
    }
}
pub(crate) fn admit_upgrade_selection(removal: Option<&RemovalRecord>) -> NativeResult<()> {
    if let Some(record) = removal {
        record.validate()?;
        if record.cursor() != RemovalCursor::Retired {
            return Err(NativeError::Busy);
        }
    }
    Ok(())
}
