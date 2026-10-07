//! Repair decisions never supply image, process, task or file authority.
#[cfg(any(windows, test))]
#[path = "repair/payload.rs"]
pub(crate) mod payload;
#[cfg(any(windows, test))]
#[path = "repair/payload_record.rs"]
pub(crate) mod payload_record;
#[path = "repair/record.rs"]
pub(crate) mod record;
use super::native_io::{NativeError, NativeResult, files::MAX_RECORD_BYTES};
use super::payload::recovery::FileStamp;
use record::{RepairCursor, RepairRecord};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RepairDiagnostic {
    Healthy,
    Missing,
    AccessDenied,
    Mismatch,
    UnsafeForeign,
    Disabled,
    Unavailable,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceKind {
    OuterUpgrade,
    FileRecovery,
    Removal,
}
impl SourceKind {
    pub(crate) fn leaf(self) -> &'static str {
        match self {
            Self::OuterUpgrade => "outer-upgrade.json",
            Self::FileRecovery => "file-recovery.json",
            Self::Removal => "removal.json",
        }
    }
}
/// Exact metadata correlation; this value can never authorize an archive by itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceSnapshot {
    kind: SourceKind,
    source_operation: [u8; 16],
    stamp: FileStamp,
    sha256: [u8; 32],
    len: u64,
}
impl SourceSnapshot {
    pub(crate) fn new(
        kind: SourceKind,
        source_operation: [u8; 16],
        stamp: FileStamp,
        sha256: [u8; 32],
        len: u64,
    ) -> NativeResult<Self> {
        let s = Self {
            kind,
            source_operation,
            stamp,
            sha256,
            len,
        };
        s.validate()?;
        Ok(s)
    }
    pub(crate) fn validate(&self) -> NativeResult<()> {
        if self.source_operation == [0; 16]
            || !self.stamp.valid()
            || self.sha256 == [0; 32]
            || self.len == 0
            || self.len > MAX_RECORD_BYTES as u64
        {
            return Err(NativeError::Invalid);
        }
        Ok(())
    }
    pub(crate) fn kind(&self) -> SourceKind {
        self.kind
    }
    pub(crate) fn source_operation(&self) -> [u8; 16] {
        self.source_operation
    }
    pub(crate) fn stamp(&self) -> FileStamp {
        self.stamp
    }
    pub(crate) fn sha256(&self) -> [u8; 32] {
        self.sha256
    }
    pub(crate) fn len(&self) -> u64 {
        self.len
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum JournalObservation {
    Absent(SourceKind),
    Terminal(SourceSnapshot),
    Retained(SourceKind, RepairDiagnostic),
}
impl JournalObservation {
    pub(crate) fn kind(&self) -> SourceKind {
        match self {
            Self::Absent(k) | Self::Retained(k, _) => *k,
            Self::Terminal(s) => s.kind(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RepairDebtObservation {
    journals: Vec<JournalObservation>,
    publication: RepairDiagnostic,
}
impl RepairDebtObservation {
    pub(crate) fn new(
        journals: Vec<JournalObservation>,
        publication: RepairDiagnostic,
    ) -> NativeResult<Self> {
        validate_journals(&journals)?;
        Ok(Self {
            journals,
            publication,
        })
    }
    pub(crate) fn journals(&self) -> &[JournalObservation] {
        &self.journals
    }
    pub(crate) fn publication(&self) -> RepairDiagnostic {
        self.publication
    }
    pub(crate) fn into_parts(self) -> (Vec<JournalObservation>, RepairDiagnostic) {
        (self.journals, self.publication)
    }
}
fn validate_journals(journals: &[JournalObservation]) -> NativeResult<()> {
    if journals.len() > 3 {
        return Err(NativeError::Oversize);
    }
    for (i, j) in journals.iter().enumerate() {
        if journals[..i].iter().any(|p| p.kind() == j.kind()) {
            return Err(NativeError::Invalid);
        }
        match j {
            JournalObservation::Terminal(s) => s.validate()?,
            JournalObservation::Retained(_, RepairDiagnostic::Healthy) => {
                return Err(NativeError::Invalid);
            }
            _ => {}
        }
    }
    Ok(())
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RepairTaskObservation {
    diagnostic: RepairDiagnostic,
    expected_xml: Option<String>,
}
impl RepairTaskObservation {
    pub(crate) fn new(
        diagnostic: RepairDiagnostic,
        expected_xml: Option<String>,
    ) -> NativeResult<Self> {
        if expected_xml
            .as_ref()
            .is_some_and(|s| s.is_empty() || s.len() > MAX_RECORD_BYTES || s.contains('\0'))
        {
            return Err(NativeError::Invalid);
        }
        Ok(Self {
            diagnostic,
            expected_xml,
        })
    }
    pub(crate) fn diagnostic(&self) -> RepairDiagnostic {
        self.diagnostic
    }
    pub(crate) fn expected_xml(&self) -> Option<&str> {
        self.expected_xml.as_deref()
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RepairObservation {
    payload: RepairDiagnostic,
    task: RepairTaskObservation,
    agent: RepairDiagnostic,
    debt: RepairDebtObservation,
}
impl RepairObservation {
    pub(crate) fn new(
        payload: RepairDiagnostic,
        task: RepairTaskObservation,
        agent: RepairDiagnostic,
        journals: Vec<JournalObservation>,
        publication: RepairDiagnostic,
    ) -> NativeResult<Self> {
        Ok(Self {
            payload,
            task,
            agent,
            debt: RepairDebtObservation::new(journals, publication)?,
        })
    }
    pub(crate) fn payload(&self) -> RepairDiagnostic {
        self.payload
    }
    pub(crate) fn task(&self) -> &RepairTaskObservation {
        &self.task
    }
    pub(crate) fn agent(&self) -> RepairDiagnostic {
        self.agent
    }
    pub(crate) fn journals(&self) -> &[JournalObservation] {
        self.debt.journals()
    }
    pub(crate) fn publication(&self) -> RepairDiagnostic {
        self.debt.publication()
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepairPlan {
    task_xml: Option<String>,
    archives: Vec<SourceSnapshot>,
}
impl RepairPlan {
    /// Metadata correlation only. Native settlement must separately admit the actual
    /// terminal source, lock and keeper-copy absence before this plan can be used.
    #[cfg(all(windows, not(test)))]
    pub(crate) fn outer_history(source: SourceSnapshot) -> NativeResult<Self> {
        if source.kind() != SourceKind::OuterUpgrade {
            return Err(NativeError::Foreign);
        }
        let plan = Self {
            task_xml: None,
            archives: vec![source],
        };
        plan.validate()?;
        Ok(plan)
    }
    pub(crate) fn task_xml(&self) -> Option<&str> {
        self.task_xml.as_deref()
    }
    pub(crate) fn archives(&self) -> &[SourceSnapshot] {
        &self.archives
    }
    pub(crate) fn validate(&self) -> NativeResult<()> {
        if self.task_xml.is_none() && self.archives.is_empty() {
            return Err(NativeError::Invalid);
        }
        RepairTaskObservation::new(RepairDiagnostic::Missing, self.task_xml.clone())?;
        let j: Vec<_> = self
            .archives
            .iter()
            .cloned()
            .map(JournalObservation::Terminal)
            .collect();
        validate_journals(&j)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RepairIssue {
    PayloadDamaged,
    PayloadDamagedDisabled,
    PayloadWithDisabled(RepairDiagnostic),
    DisabledWith(RepairDiagnostic),
    Payload(RepairDiagnostic),
    Task(RepairDiagnostic),
    Agent(RepairDiagnostic),
    Journal(RepairDiagnostic),
    Publication(RepairDiagnostic),
}
impl RepairIssue {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::PayloadDamaged => "payload damaged: repair needs a6b / reinstall",
            Self::PayloadDamagedDisabled => {
                "user-disabled task preserved; payload damaged: repair needs a6b / reinstall"
            }
            Self::Task(RepairDiagnostic::Disabled) => "user-disabled task preserved",
            Self::PayloadWithDisabled(RepairDiagnostic::AccessDenied)
            | Self::DisabledWith(RepairDiagnostic::AccessDenied) => {
                "user-disabled task preserved; access denied: retained"
            }
            Self::PayloadWithDisabled(RepairDiagnostic::UnsafeForeign)
            | Self::DisabledWith(RepairDiagnostic::UnsafeForeign) => {
                "user-disabled task preserved; unsafe or foreign: retained"
            }
            Self::PayloadWithDisabled(RepairDiagnostic::Unavailable)
            | Self::DisabledWith(RepairDiagnostic::Unavailable) => {
                "user-disabled task preserved; unavailable: retained"
            }
            Self::PayloadWithDisabled(RepairDiagnostic::Missing)
            | Self::DisabledWith(RepairDiagnostic::Missing) => {
                "user-disabled task preserved; missing: retained"
            }
            Self::PayloadWithDisabled(RepairDiagnostic::Mismatch)
            | Self::DisabledWith(RepairDiagnostic::Mismatch) => {
                "user-disabled task preserved; mismatch: retained"
            }
            Self::PayloadWithDisabled(_) | Self::DisabledWith(_) => {
                "user-disabled task preserved; unknown: retained"
            }
            Self::Payload(RepairDiagnostic::AccessDenied)
            | Self::Task(RepairDiagnostic::AccessDenied)
            | Self::Agent(RepairDiagnostic::AccessDenied)
            | Self::Journal(RepairDiagnostic::AccessDenied)
            | Self::Publication(RepairDiagnostic::AccessDenied) => "access denied: retained",
            Self::Payload(RepairDiagnostic::Missing)
            | Self::Task(RepairDiagnostic::Missing)
            | Self::Agent(RepairDiagnostic::Missing)
            | Self::Journal(RepairDiagnostic::Missing)
            | Self::Publication(RepairDiagnostic::Missing) => "missing: retained",
            Self::Payload(RepairDiagnostic::Mismatch)
            | Self::Task(RepairDiagnostic::Mismatch)
            | Self::Agent(RepairDiagnostic::Mismatch)
            | Self::Journal(RepairDiagnostic::Mismatch)
            | Self::Publication(RepairDiagnostic::Mismatch) => "mismatch: retained",
            Self::Payload(RepairDiagnostic::UnsafeForeign)
            | Self::Task(RepairDiagnostic::UnsafeForeign)
            | Self::Agent(RepairDiagnostic::UnsafeForeign)
            | Self::Journal(RepairDiagnostic::UnsafeForeign)
            | Self::Publication(RepairDiagnostic::UnsafeForeign) => "unsafe or foreign: retained",
            Self::Payload(RepairDiagnostic::Unavailable)
            | Self::Task(RepairDiagnostic::Unavailable)
            | Self::Agent(RepairDiagnostic::Unavailable)
            | Self::Journal(RepairDiagnostic::Unavailable)
            | Self::Publication(RepairDiagnostic::Unavailable) => "unavailable: retained",
            _ => "unknown: retained",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RepairDecision {
    Healthy,
    Report(RepairIssue),
    Apply(RepairPlan),
}
pub(crate) fn classify(o: &RepairObservation) -> NativeResult<RepairDecision> {
    use RepairDiagnostic::*;
    validate_journals(o.journals())?;
    if o.payload() != Healthy {
        return Ok(RepairDecision::Report(
            if matches!(o.payload(), Missing | Mismatch) {
                if o.task().diagnostic() == Disabled {
                    RepairIssue::PayloadDamagedDisabled
                } else {
                    RepairIssue::PayloadDamaged
                }
            } else {
                if o.task().diagnostic() == Disabled {
                    RepairIssue::PayloadWithDisabled(o.payload())
                } else {
                    RepairIssue::Payload(o.payload())
                }
            },
        ));
    }
    if o.task().diagnostic() == Disabled {
        let extra = if o.publication() != Healthy {
            Some(o.publication())
        } else {
            o.journals()
                .iter()
                .find_map(|j| match j {
                    JournalObservation::Retained(_, d) => Some(*d),
                    _ => None,
                })
                .or_else(|| (o.agent() != Healthy).then_some(o.agent()))
        };
        return Ok(RepairDecision::Report(
            extra.map_or(RepairIssue::Task(Disabled), RepairIssue::DisabledWith),
        ));
    }
    if o.publication() != Healthy {
        return Ok(RepairDecision::Report(RepairIssue::Publication(
            o.publication(),
        )));
    }
    if let Some(d) = o.journals().iter().find_map(|j| match j {
        JournalObservation::Retained(_, d) => Some(*d),
        _ => None,
    }) {
        return Ok(RepairDecision::Report(RepairIssue::Journal(d)));
    }
    if o.agent() != Healthy {
        return Ok(RepairDecision::Report(RepairIssue::Agent(o.agent())));
    }
    let task_xml = match o.task().diagnostic() {
        Healthy => None,
        Missing => match o.task().expected_xml() {
            Some(x) => Some(x.to_owned()),
            None => return Ok(RepairDecision::Report(RepairIssue::Task(Unavailable))),
        },
        d => return Ok(RepairDecision::Report(RepairIssue::Task(d))),
    };
    let archives = o
        .journals()
        .iter()
        .filter_map(|j| match j {
            JournalObservation::Terminal(s) => Some(s.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if task_xml.is_none() && archives.is_empty() {
        return Ok(RepairDecision::Healthy);
    }
    let plan = RepairPlan { task_xml, archives };
    plan.validate()?;
    Ok(RepairDecision::Apply(plan))
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArchiveObservation {
    SourceOnly,
    DestinationOnly,
    Both,
    Neither,
    Changed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RepairOutcome {
    Healthy,
    Report(RepairIssue),
    Repaired,
    Retained,
}
impl RepairOutcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy: no changes",
            Self::Report(i) => i.as_str(),
            Self::Repaired => "repair complete",
            Self::Retained => "repair retained: unknown or pending",
        }
    }
}
/// Every method observes or uses genuine native capabilities held by the adapter. No Stop or Run.
pub(crate) trait RepairPort {
    type Terminal;
    fn renew(&mut self, record: &RepairRecord) -> NativeResult<()>;
    fn persist(&mut self, record: &RepairRecord) -> NativeResult<()>;
    fn task_observe(&mut self, record: &RepairRecord) -> NativeResult<RepairTaskObservation>;
    fn register_task(&mut self, record: &RepairRecord) -> NativeResult<()>;
    fn admit_terminal(&mut self, record: &RepairRecord, index: u8) -> NativeResult<Self::Terminal>;
    fn observe_archive(
        &mut self,
        record: &RepairRecord,
        index: u8,
    ) -> NativeResult<ArchiveObservation>;
    fn archive_exact(
        &mut self,
        record: &RepairRecord,
        index: u8,
        terminal: &Self::Terminal,
    ) -> NativeResult<()>;
    fn finish(&mut self, record: &RepairRecord) -> NativeResult<()>;
}
fn publish<P: RepairPort>(p: &mut P, r: &mut RepairRecord, c: RepairCursor) -> NativeResult<()> {
    let mut next = r.clone();
    next.advance(c)?;
    p.persist(&next)?;
    *r = next;
    Ok(())
}
/// A reopened task intent is observation-only. Exact metadata moves reconcile their actual result.
pub(crate) fn run_repair<P: RepairPort>(
    p: &mut P,
    r: &mut RepairRecord,
) -> NativeResult<RepairOutcome> {
    r.validate()?;
    p.renew(r)?;
    if r.cursor() == RepairCursor::Unknown {
        return Ok(RepairOutcome::Retained);
    }
    if !r.plan().archives().is_empty() && r.slot().is_none() {
        return Ok(RepairOutcome::Retained);
    }
    if r.cursor() == RepairCursor::Selected && r.plan().task_xml().is_some() {
        let task = p.task_observe(r)?;
        let expected = r.plan().task_xml();
        if task.expected_xml() != expected {
            return Ok(RepairOutcome::Retained);
        }
        match task.diagnostic() {
            RepairDiagnostic::Healthy => publish(p, r, RepairCursor::TaskObserved)?,
            RepairDiagnostic::Missing => {
                publish(p, r, RepairCursor::TaskIntent)?;
                p.register_task(r)?;
            }
            _ => return Ok(RepairOutcome::Retained),
        }
    }
    if r.cursor() == RepairCursor::TaskIntent {
        let task = p.task_observe(r)?;
        if task.diagnostic() != RepairDiagnostic::Healthy
            || task.expected_xml() != r.plan().task_xml()
        {
            return Ok(RepairOutcome::Retained);
        }
        publish(p, r, RepairCursor::TaskObserved)?;
    }
    loop {
        p.renew(r)?;
        let index = match r.cursor() {
            RepairCursor::Selected | RepairCursor::TaskObserved => Some(0),
            RepairCursor::Archived { index } => Some(index + 1),
            RepairCursor::ArchiveIntent { index } => {
                let terminal = p.admit_terminal(r, index)?;
                match p.observe_archive(r, index)? {
                    ArchiveObservation::DestinationOnly => {}
                    ArchiveObservation::SourceOnly => {
                        p.archive_exact(r, index, &terminal)?;
                        if p.observe_archive(r, index)? != ArchiveObservation::DestinationOnly {
                            return Ok(RepairOutcome::Retained);
                        }
                    }
                    _ => return Ok(RepairOutcome::Retained),
                }
                publish(p, r, RepairCursor::Archived { index })?;
                continue;
            }
            RepairCursor::Complete => {
                p.finish(r)?;
                return Ok(RepairOutcome::Repaired);
            }
            _ => return Ok(RepairOutcome::Retained),
        };
        if let Some(index) = index {
            if usize::from(index) < r.plan().archives().len() {
                // Admission precedes even the intent; the effect revalidates the actual cap again.
                let _terminal = p.admit_terminal(r, index)?;
                publish(p, r, RepairCursor::ArchiveIntent { index })?;
            } else {
                publish(p, r, RepairCursor::Complete)?;
            }
        }
    }
}
