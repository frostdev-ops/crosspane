//! Three fixed immutable supervisor-epoch archives. Record data never creates native ownership.
#![cfg(any(windows, test))]
use super::super::{payload::recovery::FileStamp, service::journal::Journal};
use super::{NativeError, NativeResult};
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ArchivePhase {
    PruneIntent,
    MoveIntent,
    Complete,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArchiveIntent {
    pub(super) schema_version: u32,
    pub(super) operation: [u8; 16],
    pub(super) owner_creation: u64,
    pub(super) slot: u8,
    pub(super) source: FileStamp,
    pub(super) sha256: [u8; 32],
    pub(super) victim: Option<FileStamp>,
    pub(super) phase: ArchivePhase,
}
impl ArchiveIntent {
    pub(crate) fn require_complete(&self) -> NativeResult<()> {
        self.validate()?;
        if self.phase != ArchivePhase::Complete {
            return Err(NativeError::OutcomeUnknown);
        }
        Ok(())
    }
    pub(crate) fn validate(&self) -> NativeResult<()> {
        if self.schema_version != 1
            || self.operation == [0; 16]
            || self.owner_creation == 0
            || self.slot >= 3
            || self.source.volume == 0
            || self.source.file == [0; 16]
            || self.sha256 == [0; 32]
            || self
                .victim
                .is_some_and(|s| s.volume == 0 || s.file == [0; 16] || s == self.source)
            || (self.phase == ArchivePhase::PruneIntent && self.victim.is_none())
        {
            return Err(NativeError::Invalid);
        }
        Ok(())
    }
}
/// Status policy only. The native caller supplies the actual bounded LSA observation and
/// separately renews its original context/deadline before constructing any private seal.
pub(crate) fn classify_prior_logon_status(status: i32, has_data: bool) -> NativeResult<()> {
    // Exact SDK STATUS_NO_SUCH_LOGON_SESSION; named native binding is checked at the call site.
    const NO_SUCH_LOGON_SESSION: i32 = 0xC000005Fu32 as i32;
    if status == NO_SUCH_LOGON_SESSION && !has_data {
        Ok(())
    } else {
        Err(NativeError::Foreign)
    }
}

/// Positive first-epoch absence policy, used only after fresh locked fixed-leaf observations.
/// An old consumed task claim without provenance is not a first logon, even without a Journal.
pub(crate) fn first_logon_absence(
    task: Option<&super::activation::TaskActivationRecord>,
    journal_present: bool,
    provenance_present: bool,
    archive_intent_present: bool,
    archive_history_present: bool,
) -> NativeResult<()> {
    if journal_present || provenance_present {
        return Err(NativeError::Foreign);
    }
    if archive_intent_present {
        return Err(NativeError::OutcomeUnknown);
    }
    if archive_history_present {
        return Err(NativeError::Unsupported);
    }
    if task.is_some_and(|record| record.claim().is_some()) {
        return Err(NativeError::Unsupported);
    }
    if super::activation::select_entry(task)? != super::activation::EntrySelection::Logon {
        return Err(NativeError::Foreign);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HistoryRequirement {
    Terminal,
    PriorLogonDisposition,
}
/// A decoded phase determines which additional evidence is required, never whether an owner
/// died. Nonterminal history needs exact correlated provenance and a fresh native disposition.
pub(crate) fn history_requirement(journal: &Journal) -> NativeResult<HistoryRequirement> {
    use super::super::service::journal::Phase;
    let admitted = Journal::decode(&journal.encode()?)?;
    match admitted.phase {
        Phase::Finished | Phase::StopIntent => Ok(HistoryRequirement::Terminal),
        Phase::Running | Phase::Backoff | Phase::StartRequested => {
            Ok(HistoryRequirement::PriorLogonDisposition)
        }
        Phase::Planned | Phase::Unknown => Err(NativeError::Foreign),
    }
}
/// Quota selection only. Production first validates every populated slot with its required
/// actual admission. This function cannot construct a disposition or archive capability.
pub(crate) fn select_correlated_slots(slots: &[Option<Journal>; 3]) -> NativeResult<(u8, bool)> {
    for journal in slots.iter().flatten() {
        history_requirement(journal)?;
    }
    if let Some(slot) = slots.iter().position(Option::is_none) {
        return Ok((slot as u8, false));
    }
    let slot = slots
        .iter()
        .enumerate()
        .min_by_key(|(_, journal)| journal.as_ref().map(|journal| journal.clock_epoch))
        .map(|(slot, _)| slot as u8)
        .ok_or(NativeError::Invalid)?;
    Ok((slot, true))
}

/// Preserved legacy terminal-only selection for the existing fakes. Production uses
/// `select_correlated_slots` after native evidence validation; timestamps order history only.
#[cfg(test)]
pub(crate) fn select_slot(slots: &[Option<Journal>; 3]) -> NativeResult<(u8, bool)> {
    for journal in slots.iter().flatten() {
        let encoded = journal.encode()?;
        let admitted = Journal::decode(&encoded)?;
        if !matches!(
            admitted.phase,
            super::super::service::journal::Phase::Finished
                | super::super::service::journal::Phase::StopIntent
        ) {
            return Err(NativeError::Foreign);
        }
    }
    if let Some(slot) = slots.iter().position(Option::is_none) {
        return Ok((slot as u8, false));
    }
    let slot = slots
        .iter()
        .enumerate()
        .min_by_key(|(_, j)| j.as_ref().map(|j| j.clock_epoch))
        .map(|(slot, _)| slot as u8)
        .ok_or(NativeError::Invalid)?;
    Ok((slot, true))
}

/// Actual production order seam. Effects are never replayed after a caller's interrupted attempt.
pub(crate) trait ArchivePort {
    fn persist(&mut self, phase: ArchivePhase) -> NativeResult<()>;
    fn prune(&mut self) -> NativeResult<()>;
    fn move_current(&mut self) -> NativeResult<()>;
    fn observe_complete(&mut self) -> NativeResult<()>;
}
#[derive(Default)]
pub(crate) struct ArchiveSequence {
    attempted: bool,
}
impl ArchiveSequence {
    pub(crate) fn run_once(
        &mut self,
        port: &mut impl ArchivePort,
        prune: bool,
    ) -> NativeResult<()> {
        if self.attempted {
            return Err(NativeError::OutcomeUnknown);
        }
        self.attempted = true;
        port.persist(if prune {
            ArchivePhase::PruneIntent
        } else {
            ArchivePhase::MoveIntent
        })?;
        if prune {
            port.prune()?;
            port.persist(ArchivePhase::MoveIntent)?;
        }
        port.move_current()?;
        port.observe_complete()?;
        port.persist(ArchivePhase::Complete)
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::service::{journal::Phase, supervisor::Generation};
    use super::*;
    fn journal(epoch: u64) -> Journal {
        Journal {
            schema_version: 1,
            registration: [1; 16],
            operation: [2; 16],
            user: "fixture-user".into(),
            phase: Phase::StopIntent,
            current: Some(Generation {
                pid: 8,
                creation: 9,
                instance: u64::MAX,
            }),
            stop_instance: Some(u64::MAX),
            original_xml: None,
            restart_times: vec![],
            last_tick_ms: 10,
            clock_epoch: epoch,
        }
    }
    fn intent(phase: ArchivePhase) -> ArchiveIntent {
        ArchiveIntent {
            schema_version: 1,
            operation: [7; 16],
            owner_creation: 9,
            slot: 1,
            source: FileStamp {
                volume: 1,
                file: [3; 16],
            },
            sha256: [4; 32],
            victim: Some(FileStamp {
                volume: 1,
                file: [5; 16],
            }),
            phase,
        }
    }
    #[test]
    fn epoch_three_fixed_slots_choose_vacancy_then_lowest_history_epoch_only() {
        assert_eq!(
            select_slot(&[Some(journal(9)), None, Some(journal(2))]),
            Ok((1, false))
        );
        assert_eq!(
            select_slot(&[Some(journal(9)), Some(journal(4)), Some(journal(8))]),
            Ok((1, true))
        );
        assert_eq!(select_slot(&[None, None, None]), Ok((0, false)));
        for slot in 0..3 {
            assert!(
                super::super::records::RecordName::SupervisorEpoch(slot)
                    .file_name()
                    .is_ok()
            );
        }
        assert!(
            super::super::records::RecordName::SupervisorEpoch(3)
                .file_name()
                .is_err()
        );
    }
    #[test]
    fn epoch_history_preserves_exact_supervisor_bytes_and_precise_u64() {
        let old = journal(11);
        let bytes = old.encode().unwrap();
        for slot in 0..3 {
            super::super::records::validate_for(
                &super::super::records::RecordName::SupervisorEpoch(slot),
                &bytes,
            )
            .unwrap();
        }
        assert_eq!(Journal::decode(&bytes).unwrap(), old);
        assert_eq!(old.current.unwrap().instance, u64::MAX);
    }
    #[test]
    fn epoch_invalid_or_nonterminal_history_refuses_before_any_prune() {
        let mut malformed = journal(1);
        malformed.current = None;
        assert!(select_slot(&[Some(malformed), None, None]).is_err());
        let mut live = journal(1);
        live.phase = Phase::Running;
        live.stop_instance = None;
        assert_eq!(
            select_slot(&[Some(live), None, None]),
            Err(NativeError::Foreign)
        );
    }
    #[test]
    fn epoch_reopened_incomplete_intent_never_authorizes_retry_or_create() {
        for phase in [ArchivePhase::PruneIntent, ArchivePhase::MoveIntent] {
            let original = intent(phase);
            let parsed: ArchiveIntent =
                serde_json::from_slice(&serde_json::to_vec(&original).unwrap()).unwrap();
            assert_eq!(parsed.require_complete(), Err(NativeError::OutcomeUnknown));
        }
        assert_eq!(intent(ArchivePhase::Complete).require_complete(), Ok(()));
        // This remains metadata: actual source absence/target FileId/bytes/exclusive/task caps
        // are all revalidated separately by the native sealed result factory.
    }
    #[test]
    fn epoch_strict_intent_rejects_unknown_identity_and_aliasing_victim() {
        let mut value = serde_json::to_value(intent(ArchivePhase::MoveIntent)).unwrap();
        value["authority"] = serde_json::json!(true);
        assert!(serde_json::from_value::<ArchiveIntent>(value).is_err());
        for field in 0..6 {
            let mut i = intent(ArchivePhase::MoveIntent);
            match field {
                0 => i.operation = [0; 16],
                1 => i.owner_creation = 0,
                2 => i.slot = 3,
                3 => i.sha256 = [0; 32],
                4 => i.source.file = [0; 16],
                _ => i.victim = Some(i.source),
            };
            assert!(i.validate().is_err());
        }
    }
}
