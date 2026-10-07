//! Strict separate repair/evidence metadata. Decoding is never a native mutation permit.
use super::super::{
    native_io::{NativeError, NativeResult, records},
    payload::recovery::OuterContextCorrelation,
};
use super::{RepairPlan, SourceSnapshot};
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum RepairCursor {
    Selected,
    TaskIntent,
    TaskObserved,
    ArchiveIntent { index: u8 },
    Archived { index: u8 },
    Complete,
    Unknown,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepairRecord {
    schema_version: u32,
    operation: [u8; 16],
    context: OuterContextCorrelation,
    plan: RepairPlan,
    slot: Option<u8>,
    cursor: RepairCursor,
}
impl RepairRecord {
    pub(crate) fn new(
        operation: [u8; 16],
        context: OuterContextCorrelation,
        plan: RepairPlan,
        slot: Option<u8>,
    ) -> NativeResult<Self> {
        let r = Self {
            schema_version: 1,
            operation,
            context,
            plan,
            slot,
            cursor: RepairCursor::Selected,
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
    pub(crate) fn plan(&self) -> &RepairPlan {
        &self.plan
    }
    pub(crate) fn cursor(&self) -> RepairCursor {
        self.cursor
    }
    pub(crate) fn slot(&self) -> Option<u8> {
        self.slot
    }
    pub(crate) fn bind_slot(&mut self, slot: u8) -> NativeResult<()> {
        self.validate()?;
        if slot >= 3
            || self.slot.is_some()
            || self.cursor != RepairCursor::Selected
            || self.plan.archives().is_empty()
        {
            return Err(NativeError::Foreign);
        }
        self.slot = Some(slot);
        self.validate()
    }
    pub(crate) fn validate(&self) -> NativeResult<()> {
        if self.schema_version != 1
            || self.operation == [0; 16]
            || self.slot.is_some_and(|s| s >= 3)
        {
            return Err(NativeError::Invalid);
        }
        self.plan.validate()?;
        validate_context(&self.context)?;
        if self.plan.archives().is_empty() && self.slot.is_some() {
            return Err(NativeError::Invalid);
        }
        if !self.plan.archives().is_empty()
            && self.slot.is_none()
            && !matches!(self.cursor, RepairCursor::Selected | RepairCursor::Unknown)
        {
            return Err(NativeError::Invalid);
        }
        match self.cursor {
            RepairCursor::TaskIntent | RepairCursor::TaskObserved
                if self.plan.task_xml().is_none() =>
            {
                return Err(NativeError::Invalid);
            }
            RepairCursor::ArchiveIntent { index } | RepairCursor::Archived { index }
                if usize::from(index) >= self.plan.archives().len() =>
            {
                return Err(NativeError::Invalid);
            }
            _ => {}
        }
        Ok(())
    }
    pub(crate) fn advance(&mut self, next: RepairCursor) -> NativeResult<()> {
        self.validate()?;
        if !self.allowed(next) {
            return Err(NativeError::Foreign);
        }
        let mut r = self.clone();
        r.cursor = next;
        r.validate()?;
        *self = r;
        Ok(())
    }
    fn allowed(&self, next: RepairCursor) -> bool {
        use RepairCursor::*;
        if next == Unknown {
            return !matches!(self.cursor, Complete | Unknown);
        }
        match (self.cursor, next) {
            (Selected, TaskIntent | TaskObserved) => self.plan.task_xml().is_some(),
            (Selected, ArchiveIntent { index: 0 }) => {
                self.plan.task_xml().is_none() && !self.plan.archives().is_empty()
            }
            (Selected, Complete) => {
                self.plan.task_xml().is_none() && self.plan.archives().is_empty()
            }
            (TaskIntent, TaskObserved) => true,
            (TaskObserved, ArchiveIntent { index: 0 }) => !self.plan.archives().is_empty(),
            (TaskObserved, Complete) => self.plan.archives().is_empty(),
            (ArchiveIntent { index: a }, Archived { index: b }) => a == b,
            (Archived { index: a }, ArchiveIntent { index: b }) => {
                b == a + 1 && usize::from(b) < self.plan.archives().len()
            }
            (Archived { index }, Complete) => usize::from(index) + 1 == self.plan.archives().len(),
            _ => false,
        }
    }
    pub(crate) fn publication_successor(&self, next: &Self) -> NativeResult<()> {
        self.validate()?;
        next.validate()?;
        if self.operation != next.operation
            || self.context != next.context
            || self.plan != next.plan
        {
            return Err(NativeError::Foreign);
        }
        if self == next {
            return Ok(());
        }
        if self.cursor == RepairCursor::Selected
            && next.cursor == self.cursor
            && self.slot.is_none()
            && next.slot.is_some()
            && !self.plan.archives().is_empty()
        {
            return Ok(());
        }
        if self.slot != next.slot || !self.allowed(next.cursor) {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
    pub(crate) fn encode(&self) -> NativeResult<Vec<u8>> {
        self.validate()?;
        records::encode_record(
            &records::RecordName::Repair,
            serde_json::to_value(self).map_err(|_| NativeError::Invalid)?,
        )
    }
    pub(crate) fn decode(bytes: &[u8]) -> NativeResult<Self> {
        let r: Self = records::record_data(&records::RecordName::Repair, bytes)?;
        r.validate()?;
        Ok(r)
    }
}
fn validate_context(context: &OuterContextCorrelation) -> NativeResult<()> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Fields {
        user: Vec<u8>,
        logon: Vec<u8>,
        authentication_id: u64,
        session: u32,
    }
    let f: Fields =
        serde_json::from_value(serde_json::to_value(context).map_err(|_| NativeError::Invalid)?)
            .map_err(|_| NativeError::Invalid)?;
    let facts = super::super::native_io::identity::TokenFacts {
        user: super::super::native_io::identity::Sid::from_bytes(f.user)?,
        logon: super::super::native_io::identity::Sid::from_bytes(f.logon)?,
        authentication_id: f.authentication_id,
        session: f.session,
        elevated: false,
        integrity: 0x2000,
        impersonating: false,
    };
    context.matches(&facts)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EvidencePhase {
    Reserved,
    Complete,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvidenceSlot {
    repair: [u8; 16],
    sources: Vec<SourceSnapshot>,
    phase: EvidencePhase,
}
impl EvidenceSlot {
    pub(crate) fn repair(&self) -> [u8; 16] {
        self.repair
    }
    pub(crate) fn sources(&self) -> &[SourceSnapshot] {
        &self.sources
    }
    pub(crate) fn phase(&self) -> EvidencePhase {
        self.phase
    }
    fn validate(&self) -> NativeResult<()> {
        if self.repair == [0; 16] || self.sources.is_empty() || self.sources.len() > 3 {
            return Err(NativeError::Invalid);
        }
        for (i, s) in self.sources.iter().enumerate() {
            s.validate()?;
            if self.sources[..i].iter().any(|p| p.kind() == s.kind()) {
                return Err(NativeError::Invalid);
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvidenceIndex {
    schema_version: u32,
    slots: [Option<EvidenceSlot>; 3],
}
impl Default for EvidenceIndex {
    fn default() -> Self {
        Self {
            schema_version: 1,
            slots: [None, None, None],
        }
    }
}
impl EvidenceIndex {
    pub(crate) fn get(&self, slot: u8) -> Option<&EvidenceSlot> {
        self.slots.get(usize::from(slot)).and_then(Option::as_ref)
    }
    pub(crate) fn reserve(
        &mut self,
        repair: [u8; 16],
        sources: &[SourceSnapshot],
    ) -> NativeResult<u8> {
        self.validate()?;
        let s = EvidenceSlot {
            repair,
            sources: sources.to_vec(),
            phase: EvidencePhase::Reserved,
        };
        s.validate()?;
        for (i, old) in self.slots.iter().enumerate() {
            if let Some(old) = old
                && old.repair == repair
            {
                if old.sources != s.sources {
                    return Err(NativeError::Foreign);
                }
                return Ok(i as u8);
            }
        }
        let index = self
            .slots
            .iter()
            .position(Option::is_none)
            .ok_or(NativeError::Busy)?;
        self.slots[index] = Some(s);
        Ok(index as u8)
    }
    pub(crate) fn complete(&mut self, slot: u8, repair: [u8; 16]) -> NativeResult<()> {
        self.validate()?;
        let s = self
            .slots
            .get_mut(usize::from(slot))
            .and_then(Option::as_mut)
            .ok_or(NativeError::Foreign)?;
        if s.repair != repair {
            return Err(NativeError::Foreign);
        }
        s.phase = EvidencePhase::Complete;
        Ok(())
    }
    pub(crate) fn validate(&self) -> NativeResult<()> {
        if self.schema_version != 1 {
            return Err(NativeError::Invalid);
        }
        for (i, s) in self.slots.iter().enumerate() {
            if let Some(s) = s {
                s.validate()?;
                if self.slots[..i]
                    .iter()
                    .flatten()
                    .any(|p| p.repair == s.repair)
                {
                    return Err(NativeError::Invalid);
                }
            }
        }
        Ok(())
    }
    pub(crate) fn publication_successor(&self, next: &Self) -> NativeResult<()> {
        self.validate()?;
        next.validate()?;
        for (old, new) in self.slots.iter().zip(&next.slots) {
            match (old, new) {
                (None, _) => {}
                (Some(old), Some(new))
                    if old.repair == new.repair
                        && old.sources == new.sources
                        && (old.phase == new.phase
                            || old.phase == EvidencePhase::Reserved
                                && new.phase == EvidencePhase::Complete) => {}
                _ => return Err(NativeError::Foreign),
            }
        }
        Ok(())
    }
    pub(crate) fn encode(&self) -> NativeResult<Vec<u8>> {
        self.validate()?;
        records::encode_record(
            &records::RecordName::RepairEvidence,
            serde_json::to_value(self).map_err(|_| NativeError::Invalid)?,
        )
    }
    pub(crate) fn decode(bytes: &[u8]) -> NativeResult<Self> {
        let r: Self = records::record_data(&records::RecordName::RepairEvidence, bytes)?;
        r.validate()?;
        Ok(r)
    }
}
