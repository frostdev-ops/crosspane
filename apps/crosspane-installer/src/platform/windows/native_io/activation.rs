//! Fixed task intent facts. Decoding never grants a Run or process-owner capability.
use super::super::service::task::{Definition, MAX_XML_BYTES, Plan, TaskPort, reconcile};
use super::{
    NativeError, NativeResult,
    files::FileIdentity,
    records::{self, RecordName},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Phase {
    RegistrationIntent,
    Registered,
    RunIntent,
    RunObserved,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum RegistrationStep {
    Intent,
    FolderIntent,
    FolderCreated,
    DefinitionIntent,
    DefinitionRegistered,
    Kept,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SupervisorClaim {
    pub(crate) pid: u32,
    pub(crate) creation: u64,
}

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskActivationRecord {
    schema_version: u8,
    operation: [u8; 16],
    user: String,
    installer_volume: u64,
    installer_file: [u8; 16],
    phase: Phase,
    registration_step: RegistrationStep,
    original_xml: Option<String>,
    submission: Option<String>,
    claim: Option<SupervisorClaim>,
}
impl std::fmt::Debug for TaskActivationRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TaskActivationRecord")
    }
}
impl TaskActivationRecord {
    pub(crate) fn new(
        operation: [u8; 16],
        user: String,
        installer: FileIdentity,
        original_xml: Option<String>,
    ) -> NativeResult<Self> {
        let value = Self {
            schema_version: 1,
            operation,
            user,
            installer_volume: installer.volume,
            installer_file: installer.file,
            phase: Phase::RegistrationIntent,
            registration_step: RegistrationStep::Intent,
            original_xml,
            submission: None,
            claim: None,
        };
        value.validate()?;
        Ok(value)
    }
    pub(crate) fn operation(&self) -> [u8; 16] {
        self.operation
    }
    pub(crate) fn phase(&self) -> Phase {
        self.phase
    }
    pub(crate) fn claim(&self) -> Option<SupervisorClaim> {
        self.claim
    }
    pub(crate) fn bind(
        &self,
        operation: [u8; 16],
        user: &str,
        installer: FileIdentity,
    ) -> NativeResult<()> {
        self.validate()?;
        if self.operation != operation
            || self.user != user
            || self.installer_volume != installer.volume
            || self.installer_file != installer.file
        {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
    pub(crate) fn registration_step(&mut self, next: RegistrationStep) -> NativeResult<()> {
        use RegistrationStep::*;
        if self.phase != Phase::RegistrationIntent
            || !matches!(
                (self.registration_step, next),
                (Intent, FolderIntent)
                    | (Intent, DefinitionIntent)
                    | (FolderIntent, FolderCreated)
                    | (FolderCreated, DefinitionIntent)
                    | (DefinitionIntent, DefinitionRegistered)
            )
        {
            return Err(NativeError::OutcomeUnknown);
        }
        self.registration_step = next;
        Ok(())
    }
    pub(crate) fn registered(&mut self) -> NativeResult<()> {
        if self.phase != Phase::RegistrationIntent {
            return Err(NativeError::OutcomeUnknown);
        }
        if self.registration_step == RegistrationStep::Intent {
            self.registration_step = RegistrationStep::Kept;
        }
        if !matches!(
            self.registration_step,
            RegistrationStep::Kept | RegistrationStep::DefinitionRegistered
        ) {
            return Err(NativeError::OutcomeUnknown);
        }
        self.phase = Phase::Registered;
        Ok(())
    }
    pub(crate) fn run_intent(&mut self) -> NativeResult<()> {
        if self.phase != Phase::Registered {
            return Err(NativeError::OutcomeUnknown);
        }
        self.phase = Phase::RunIntent;
        Ok(())
    }
    pub(crate) fn run_observed(&mut self, submission: String) -> NativeResult<()> {
        if self.phase != Phase::RunIntent || !valid_submission(&submission) {
            return Err(NativeError::OutcomeUnknown);
        }
        self.phase = Phase::RunObserved;
        self.submission = Some(submission);
        Ok(())
    }
    pub(crate) fn claim_supervisor(&mut self, claim: SupervisorClaim) -> NativeResult<()> {
        self.validate()?;
        if !matches!(self.phase, Phase::RunIntent | Phase::RunObserved)
            || self.claim.is_some()
            || claim.pid == 0
            || claim.creation == 0
        {
            return Err(NativeError::Foreign);
        }
        self.claim = Some(claim);
        Ok(())
    }
    pub(crate) fn encode(&self) -> NativeResult<Vec<u8>> {
        self.validate()?;
        records::encode_record(
            &RecordName::TaskActivation,
            serde_json::to_value(self).map_err(|_| NativeError::Invalid)?,
        )
    }
    pub(crate) fn decode(bytes: &[u8]) -> NativeResult<Self> {
        let value: Self = records::record_data(&RecordName::TaskActivation, bytes)?;
        value.validate()?;
        Ok(value)
    }
    fn validate(&self) -> NativeResult<()> {
        if self.schema_version != 1
            || self.operation == [0; 16]
            || !self.user.starts_with("S-1-")
            || self.user.len() > 256
            || !self
                .user
                .bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b'S' | b'-'))
            || self.installer_volume == 0
            || self.installer_file == [0; 16]
            || self
                .original_xml
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > MAX_XML_BYTES || s.contains('\0'))
            || (self.phase != Phase::RegistrationIntent
                && !matches!(
                    self.registration_step,
                    RegistrationStep::Kept | RegistrationStep::DefinitionRegistered
                ))
            || (self.phase == Phase::RunObserved) != self.submission.is_some()
            || self
                .submission
                .as_ref()
                .is_some_and(|s| !valid_submission(s))
            || self.claim.is_some_and(|c| {
                c.pid == 0
                    || c.creation == 0
                    || !matches!(self.phase, Phase::RunIntent | Phase::RunObserved)
            })
        {
            return Err(NativeError::Invalid);
        }
        Ok(())
    }
}
fn valid_submission(value: &str) -> bool {
    value.len() == 38
        && value.starts_with('{')
        && value.ends_with('}')
        && value[1..37].bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

/// Only submission correlation. A task GUID never proves an agent image, instance or job.
pub(crate) struct TaskSubmission {
    guid: String,
}
impl TaskSubmission {
    pub(crate) fn new(guid: String) -> NativeResult<Self> {
        if !valid_submission(&guid) {
            return Err(NativeError::Invalid);
        }
        Ok(Self { guid })
    }
    pub(crate) fn guid(&self) -> &str {
        &self.guid
    }
}

/// Production consumes this same ordering seam; implementations retain actual native authority.
pub(crate) trait ActivationPort: TaskPort {
    fn prepare(&mut self) -> NativeResult<()>;
    fn record_run_intent(&mut self) -> NativeResult<()>;
    fn release_lock(&mut self) -> NativeResult<()>;
    fn run_once(&mut self) -> NativeResult<TaskSubmission>;
    fn record_run_result(&mut self, submission: &TaskSubmission) -> NativeResult<()>;
}
pub(crate) fn activate(port: &mut impl ActivationPort, desired: &Definition) -> NativeResult<Plan> {
    port.prepare()?;
    let decision = reconcile(port, desired)?;
    if decision == Plan::PreserveDisabled {
        return Ok(decision);
    }
    port.record_run_intent()?;
    port.release_lock()?;
    let submission =
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| port.run_once())) {
            Ok(Ok(value)) => value,
            _ => {
                port.retire();
                return Err(NativeError::OutcomeUnknown);
            }
        };
    if !matches!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(
            || port.record_run_result(&submission)
        )),
        Ok(Ok(()))
    ) {
        port.retire();
        return Err(NativeError::OutcomeUnknown);
    }
    Ok(decision)
}

#[cfg(windows)]
impl TaskActivationRecord {
    pub(crate) fn read(
        io: &super::WindowsNativeIo,
        proof: &super::SupportProof,
        deadline: &super::Deadline,
    ) -> NativeResult<Option<Self>> {
        io.read_record(
            proof,
            RecordName::TaskActivation,
            super::files::MAX_RECORD_BYTES,
            deadline,
        )?
        .map(|r| Self::decode(r.bytes()))
        .transpose()
    }
    pub(crate) fn publish(
        &self,
        io: &super::WindowsNativeIo,
        proof: &super::SupportProof,
        lock: &super::InstallerLock,
        deadline: &super::Deadline,
    ) -> NativeResult<()> {
        let value = io.publish_record(
            proof,
            lock,
            RecordName::TaskActivation,
            &self.encode()?,
            deadline,
        )?;
        if value.native_failure.is_some()
            || value.state != records::PublicationRecovery::NewPublished
        {
            return Err(NativeError::OutcomeUnknown);
        }
        Ok(())
    }
}

/// Immutable predecessor correlation, NEVER an exclusive-owner, tree-completion or launch proof.
/// Only the genuine native TaskRunPermit may bind this observation to a current claim.
pub(crate) struct UpgradeLineage {
    operation: [u8; 16],
    // The focused lineage fake asserts this exact u64 predecessor instance.
    #[allow(dead_code)]
    original_instance: u64,
    // The production archive gate compares the full predecessor; fixtures inspect this exact tuple.
    #[allow(dead_code)]
    generation: super::super::service::supervisor::Generation,
    predecessor: super::super::service::journal::Journal,
}
impl std::fmt::Debug for UpgradeLineage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UpgradeLineageObservation")
    }
}
impl UpgradeLineage {
    pub(crate) fn operation(&self) -> [u8; 16] {
        self.operation
    }
    // The focused lineage fake asserts this exact observation; it grants no native authority.
    #[allow(dead_code)]
    pub(crate) fn original_instance(&self) -> u64 {
        self.original_instance
    }
    // The focused lineage fake asserts this exact observation; it grants no native authority.
    #[allow(dead_code)]
    pub(crate) fn predecessor_registration(&self) -> [u8; 16] {
        self.predecessor.registration
    }
    // The focused lineage fake asserts this exact observation; it grants no native authority.
    #[allow(dead_code)]
    pub(crate) fn predecessor_operation(&self) -> [u8; 16] {
        self.predecessor.operation
    }
    // The focused lineage fake asserts this exact observation; it grants no native authority.
    #[allow(dead_code)]
    pub(crate) fn predecessor_generation(&self) -> super::super::service::supervisor::Generation {
        self.generation
    }
    // The focused lineage fake asserts this exact observation; it grants no native authority.
    #[allow(dead_code)]
    pub(crate) fn predecessor_clock_epoch(&self) -> u64 {
        self.predecessor.clock_epoch
    }
    pub(crate) fn matches_predecessor(
        &self,
        other: &super::super::service::journal::Journal,
    ) -> bool {
        &self.predecessor == other
    }
}
/// Pure matching only. Native ownership is supplied separately, never reconstructed from these facts.
pub(crate) fn correlate_upgrade(
    operation: [u8; 16],
    user: &str,
    selected: Option<&super::super::payload::recovery::OperationRecord>,
    predecessor: Option<&super::super::service::journal::Journal>,
) -> NativeResult<Option<UpgradeLineage>> {
    use super::super::{payload::recovery, service::journal};
    let (selected, predecessor) = match (selected, predecessor) {
        (None, None) => return Ok(None),
        (Some(selected), Some(predecessor)) => (selected, predecessor),
        _ => return Err(NativeError::Foreign),
    };
    selected.validate()?;
    // Encode is serialization only. Decode invokes the SAME frozen journal validation; this
    // metadata check neither changes the schema nor republishes/rewrites the original history.
    let validated = journal::Journal::decode(&predecessor.encode()?)?;
    let predecessor = &validated;
    if operation == [0; 16]
        || selected.operation() != operation
        || selected.phase() != recovery::Phase::StartIntent
        || predecessor.user != user
        || predecessor.operation == operation
        || !matches!(
            predecessor.phase,
            journal::Phase::Finished | journal::Phase::StopIntent
        )
    {
        return Err(NativeError::Foreign);
    }
    let original = selected.original_instance().ok_or(NativeError::Foreign)?;
    let original_instance = u64::from_str_radix(original, 16).map_err(|_| NativeError::Invalid)?;
    let generation = predecessor.current.ok_or(NativeError::Foreign)?;
    if original_instance == 0
        || original != format!("{original_instance:032x}")
        || original_instance != generation.instance
        || (predecessor.phase == journal::Phase::StopIntent
            && predecessor.stop_instance != Some(generation.instance))
    {
        return Err(NativeError::Foreign);
    }
    Ok(Some(UpgradeLineage {
        operation,
        original_instance,
        generation,
        predecessor: predecessor.clone(),
    }))
}

/// A route decision only. It never claims an activation or constructs native launch authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EntrySelection {
    Installer,
    Logon,
}
pub(crate) fn select_entry(task: Option<&TaskActivationRecord>) -> NativeResult<EntrySelection> {
    let Some(task) = task else {
        return Ok(EntrySelection::Logon);
    };
    task.validate()?;
    match task.phase {
        Phase::RunIntent | Phase::RunObserved if task.claim.is_none() => {
            Ok(EntrySelection::Installer)
        }
        Phase::Registered | Phase::RunIntent | Phase::RunObserved => Ok(EntrySelection::Logon),
        Phase::RegistrationIntent | Phase::Unknown => Err(NativeError::OutcomeUnknown),
    }
}

/// Immutable epoch/context observations. Neither decoding nor matching supplies owner authority.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EpochProvenance {
    registration: [u8; 16],
    operation: [u8; 16],
    user: String,
    user_sid: Vec<u8>,
    logon_sid: Vec<u8>,
    authentication_id: u64,
    session: u32,
    owner_pid: u32,
    owner_creation: u64,
    clock_epoch: u64,
}
impl std::fmt::Debug for EpochProvenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EpochProvenance")
    }
}
impl EpochProvenance {
    pub(crate) fn new(
        registration: [u8; 16],
        operation: [u8; 16],
        facts: &super::identity::TokenFacts,
        owner_pid: u32,
        owner_creation: u64,
    ) -> NativeResult<Self> {
        super::identity::LimitedIdentity::admit(facts.clone())?;
        let value = Self {
            registration,
            operation,
            user: facts.user.sddl(),
            user_sid: facts.user.bytes().to_vec(),
            logon_sid: facts.logon.bytes().to_vec(),
            authentication_id: facts.authentication_id,
            session: facts.session,
            owner_pid,
            owner_creation,
            clock_epoch: owner_creation,
        };
        value.validate()?;
        Ok(value)
    }
    pub(crate) fn registration(&self) -> [u8; 16] {
        self.registration
    }
    pub(crate) fn operation(&self) -> [u8; 16] {
        self.operation
    }
    pub(crate) fn user(&self) -> &str {
        &self.user
    }
    pub(crate) fn logon_sid(&self) -> &[u8] {
        &self.logon_sid
    }
    pub(crate) fn authentication_id(&self) -> u64 {
        self.authentication_id
    }
    pub(crate) fn owner_creation(&self) -> u64 {
        self.owner_creation
    }
    // The focused logon fakes construct the frozen Journal using this exact epoch.
    #[cfg(test)]
    pub(crate) fn clock_epoch(&self) -> u64 {
        self.clock_epoch
    }
    // The focused logon fakes assert exact same-context and different-logon observations.
    #[cfg(test)]
    pub(crate) fn matches_context(&self, facts: &super::identity::TokenFacts) -> bool {
        self.user_sid == facts.user.bytes()
            && self.logon_sid == facts.logon.bytes()
            && self.authentication_id == facts.authentication_id
            && self.session == facts.session
    }
    pub(crate) fn validate(&self) -> NativeResult<()> {
        let user = super::identity::Sid::from_bytes(self.user_sid.clone())?;
        let logon = super::identity::Sid::from_bytes(self.logon_sid.clone())?;
        // Structural checked observations only: this does not call a native token factory.
        super::identity::LimitedIdentity::admit(super::identity::TokenFacts {
            user: user.clone(),
            logon,
            session: self.session,
            elevated: false,
            integrity: 0x2000,
            authentication_id: self.authentication_id,
            impersonating: false,
        })?;
        if self.registration == [0; 16]
            || self.operation == [0; 16]
            || self.user != user.sddl()
            || self.authentication_id == 0
            || self.owner_pid == 0
            || self.owner_creation == 0
            || self.clock_epoch != self.owner_creation
        {
            return Err(NativeError::Invalid);
        }
        Ok(())
    }
    pub(crate) fn matches_journal(
        &self,
        journal: &super::super::service::journal::Journal,
    ) -> NativeResult<()> {
        use super::super::service::journal::{Journal, Phase};
        self.validate()?;
        let journal = Journal::decode(&journal.encode()?)?;
        if journal.user != self.user
            || journal.registration != self.registration
            || journal.operation != self.operation
            || journal.clock_epoch != self.clock_epoch
            || journal.current.is_none()
            || !matches!(
                journal.phase,
                Phase::Running
                    | Phase::Backoff
                    | Phase::StartRequested
                    | Phase::StopIntent
                    | Phase::Finished
            )
        {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
}

/// Immutable archive correlation. The native adapter independently verifies the actual file.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HistoryCorrelation {
    slot: u8,
    epoch: EpochProvenance,
    source: super::super::payload::recovery::FileStamp,
    sha256: [u8; 32],
}
impl std::fmt::Debug for HistoryCorrelation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HistoryCorrelation")
    }
}
impl HistoryCorrelation {
    pub(crate) fn new(
        slot: u8,
        epoch: EpochProvenance,
        source: super::super::payload::recovery::FileStamp,
        sha256: [u8; 32],
    ) -> NativeResult<Self> {
        let value = Self {
            slot,
            epoch,
            source,
            sha256,
        };
        value.validate()?;
        Ok(value)
    }
    pub(crate) fn slot(&self) -> u8 {
        self.slot
    }
    fn validate(&self) -> NativeResult<()> {
        self.epoch.validate()?;
        if self.slot > 2
            || self.source.volume == 0
            || self.source.file == [0; 16]
            || self.sha256 == [0; 32]
        {
            return Err(NativeError::Invalid);
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum LogonRecordPhase {
    Preparing,
    Bound,
    Unknown,
}

/// One fixed private record, with at most the three admitted immutable archive correlations.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SupervisorLogonRecord {
    schema_version: u8,
    phase: LogonRecordPhase,
    current: EpochProvenance,
    history: Vec<HistoryCorrelation>,
}
impl std::fmt::Debug for SupervisorLogonRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SupervisorLogonRecord")
    }
}
impl SupervisorLogonRecord {
    pub(crate) fn new(current: EpochProvenance) -> NativeResult<Self> {
        let value = Self {
            schema_version: 1,
            phase: LogonRecordPhase::Preparing,
            current,
            history: Vec::new(),
        };
        value.validate()?;
        Ok(value)
    }
    pub(crate) fn current(&self) -> &EpochProvenance {
        &self.current
    }
    pub(crate) fn phase(&self) -> LogonRecordPhase {
        self.phase
    }
    pub(crate) fn history(&self) -> &[HistoryCorrelation] {
        &self.history
    }
    pub(crate) fn rotate(
        &self,
        next: EpochProvenance,
        archived: HistoryCorrelation,
    ) -> NativeResult<Self> {
        self.validate()?;
        next.validate()?;
        archived.validate()?;
        if self.phase != LogonRecordPhase::Bound
            || archived.epoch != self.current
            || next.operation == self.current.operation
            || next.user != self.current.user
        {
            return Err(NativeError::Foreign);
        }
        let mut history = self
            .history
            .iter()
            .filter(|entry| entry.slot != archived.slot)
            .cloned()
            .collect::<Vec<_>>();
        history.push(archived);
        history.sort_by_key(|entry| entry.slot);
        let value = Self {
            schema_version: 1,
            phase: LogonRecordPhase::Preparing,
            current: next,
            history,
        };
        value.validate()?;
        Ok(value)
    }
    pub(crate) fn bind(
        &mut self,
        journal: &super::super::service::journal::Journal,
    ) -> NativeResult<()> {
        self.validate()?;
        if self.phase != LogonRecordPhase::Preparing
            || journal.phase != super::super::service::journal::Phase::Running
        {
            return Err(NativeError::Foreign);
        }
        self.current.matches_journal(journal)?;
        self.phase = LogonRecordPhase::Bound;
        Ok(())
    }
    pub(crate) fn matches_current(
        &self,
        journal: &super::super::service::journal::Journal,
    ) -> NativeResult<&EpochProvenance> {
        self.validate()?;
        if self.phase != LogonRecordPhase::Bound {
            return Err(NativeError::OutcomeUnknown);
        }
        self.current.matches_journal(journal)?;
        Ok(&self.current)
    }
    pub(crate) fn matches_history(
        &self,
        slot: u8,
        journal: &super::super::service::journal::Journal,
        source: super::super::payload::recovery::FileStamp,
        sha256: [u8; 32],
    ) -> NativeResult<&EpochProvenance> {
        self.validate()?;
        let found = self
            .history
            .iter()
            .find(|entry| entry.slot == slot)
            .ok_or(NativeError::Foreign)?;
        if found.source != source || found.sha256 != sha256 {
            return Err(NativeError::Foreign);
        }
        found.epoch.matches_journal(journal)?;
        Ok(&found.epoch)
    }
    pub(crate) fn encode(&self) -> NativeResult<Vec<u8>> {
        self.validate()?;
        records::encode_record(
            &RecordName::SupervisorLogon,
            serde_json::to_value(self).map_err(|_| NativeError::Invalid)?,
        )
    }
    pub(crate) fn decode(bytes: &[u8]) -> NativeResult<Self> {
        let value: Self = records::record_data(&RecordName::SupervisorLogon, bytes)?;
        value.validate()?;
        Ok(value)
    }
    fn validate(&self) -> NativeResult<()> {
        self.current.validate()?;
        if self.schema_version != 1
            || self.history.len() > 3
            || self.phase == LogonRecordPhase::Unknown
        {
            return Err(NativeError::Invalid);
        }
        for (index, entry) in self.history.iter().enumerate() {
            entry.validate()?;
            if entry.epoch.user != self.current.user
                || entry.epoch.operation == self.current.operation
                || self.history[..index].iter().any(|older| {
                    older.slot == entry.slot || older.epoch.operation == entry.epoch.operation
                })
            {
                return Err(NativeError::Invalid);
            }
        }
        Ok(())
    }
}
#[cfg(windows)]
impl SupervisorLogonRecord {
    pub(crate) fn read(
        io: &super::WindowsNativeIo,
        proof: &super::SupportProof,
        deadline: &super::Deadline,
    ) -> NativeResult<Option<Self>> {
        io.read_record(
            proof,
            RecordName::SupervisorLogon,
            super::files::MAX_RECORD_BYTES,
            deadline,
        )?
        .map(|record| Self::decode(record.bytes()))
        .transpose()
    }
    pub(crate) fn publish(
        &self,
        io: &super::WindowsNativeIo,
        proof: &super::SupportProof,
        lock: &super::InstallerLock,
        deadline: &super::Deadline,
    ) -> NativeResult<()> {
        let value = io.publish_record(
            proof,
            lock,
            RecordName::SupervisorLogon,
            &self.encode()?,
            deadline,
        )?;
        if value.native_failure.is_some()
            || value.state != records::PublicationRecovery::NewPublished
        {
            return Err(NativeError::OutcomeUnknown);
        }
        Ok(())
    }
}

// WP-W4.1a4d keeper lifecycle. These states are observations, never native authority.
#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeeperStage {
    Preparing,
    Ready,
    Committed,
    Retained,
    Complete,
    Cancelled,
}
#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeeperProgress {
    Pending,
    Complete,
}

/// The native port retains actual process/image/namespace/copy authority before delivery. Fakes
/// exercise this same sequence but cannot manufacture any of its native capability types.
#[cfg(any(windows, test))]
pub(crate) trait KeeperPort {
    type Child;
    fn prepare(&mut self) -> NativeResult<Self::Child>;
    fn mark_ready(&mut self, child: &Self::Child) -> NativeResult<()>;
    fn commit_intent(&mut self, child: &Self::Child) -> NativeResult<()>;
    fn apply_once(&mut self, child: &Self::Child) -> NativeResult<KeeperProgress>;
    fn recover_same_owner(&mut self, child: &Self::Child) -> NativeResult<KeeperProgress>;
    fn settle(&mut self, child: &Self::Child) -> NativeResult<()>;
    fn cancel_before_stop(&mut self, child: &Self::Child) -> NativeResult<()>;
}

#[cfg(any(windows, test))]
pub(crate) struct KeeperControl<C> {
    stage: KeeperStage,
    child: Option<C>,
    preparing: bool,
    committed: bool,
    cancelled: bool,
}
#[cfg(any(windows, test))]
impl<C> Default for KeeperControl<C> {
    fn default() -> Self {
        Self {
            stage: KeeperStage::Preparing,
            child: None,
            preparing: false,
            committed: false,
            cancelled: false,
        }
    }
}
#[cfg(any(windows, test))]
impl<C> KeeperControl<C> {
    pub(crate) fn stage(&self) -> KeeperStage {
        self.stage
    }
    pub(crate) fn committed(&self) -> bool {
        self.committed
    }
    pub(crate) fn child(&self) -> NativeResult<&C> {
        self.child.as_ref().ok_or(NativeError::OutcomeUnknown)
    }
    pub(crate) fn prepare<P: KeeperPort<Child = C>>(&mut self, port: &mut P) -> NativeResult<()> {
        if self.preparing || self.cancelled || self.committed {
            return Err(NativeError::OutcomeUnknown);
        }
        self.preparing = true;
        // No error/late delivery makes another launch or source allocation eligible.
        let result = keeper_call(|| port.prepare());
        match result {
            Ok(child) => self.child = Some(child),
            Err(error) => {
                self.stage = KeeperStage::Retained;
                return Err(error);
            }
        }
        if let Err(error) = keeper_call(|| port.mark_ready(self.child()?)) {
            self.stage = KeeperStage::Retained;
            return Err(error);
        }
        self.stage = KeeperStage::Ready;
        Ok(())
    }
    pub(crate) fn commit<P: KeeperPort<Child = C>>(
        &mut self,
        port: &mut P,
    ) -> NativeResult<KeeperProgress> {
        if self.stage != KeeperStage::Ready || self.committed || self.cancelled {
            return Err(NativeError::OutcomeUnknown);
        }
        // The actual resident owner consumes commit BEFORE intent/effect. A lost acknowledgement
        // or outer exit can only observe/recover this owner; neither may repeat Stop or apply.
        self.committed = true;
        self.stage = KeeperStage::Committed;
        let result = keeper_call(|| {
            port.commit_intent(self.child()?)?;
            port.apply_once(self.child()?)
        });
        self.finish_progress(port, result)
    }
    pub(crate) fn recover<P: KeeperPort<Child = C>>(
        &mut self,
        port: &mut P,
    ) -> NativeResult<KeeperProgress> {
        if !self.committed || self.stage == KeeperStage::Cancelled {
            return Err(NativeError::Foreign);
        }
        if self.stage == KeeperStage::Complete {
            return Ok(KeeperProgress::Complete);
        }
        // Fresh per-call deadline is valid only for observations and the SAME retained recovery
        // owner. The native port never resets its one-Stop/one-Run or unknown mutation fences.
        let result = keeper_call(|| port.recover_same_owner(self.child()?));
        self.finish_progress(port, result)
    }
    fn finish_progress<P: KeeperPort<Child = C>>(
        &mut self,
        port: &mut P,
        result: NativeResult<KeeperProgress>,
    ) -> NativeResult<KeeperProgress> {
        match result {
            Ok(KeeperProgress::Complete) => {
                if let Err(error) = keeper_call(|| port.settle(self.child()?)) {
                    self.stage = KeeperStage::Retained;
                    return Err(error);
                }
                self.stage = KeeperStage::Complete;
                Ok(KeeperProgress::Complete)
            }
            Ok(KeeperProgress::Pending) => {
                self.stage = KeeperStage::Retained;
                Ok(KeeperProgress::Pending)
            }
            Err(error) => {
                self.stage = KeeperStage::Retained;
                Err(error)
            }
        }
    }
    pub(crate) fn cancel<P: KeeperPort<Child = C>>(&mut self, port: &mut P) -> NativeResult<()> {
        if self.committed || self.cancelled {
            return Err(NativeError::OutcomeUnknown);
        }
        self.cancelled = true;
        if let Some(child) = self.child.as_ref()
            && let Err(error) = keeper_call(|| port.cancel_before_stop(child))
        {
            self.stage = KeeperStage::Retained;
            return Err(error);
        }
        self.stage = KeeperStage::Cancelled;
        Ok(())
    }
}
#[cfg(any(windows, test))]
fn keeper_call<T>(work: impl FnOnce() -> NativeResult<T>) -> NativeResult<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
        .unwrap_or(Err(NativeError::OutcomeUnknown))
}

/// In-memory cardinality only, never native/image/commit authority. The resident owner validates
/// its live commit, prepared sources and actual selected native capability before reserving it.
#[cfg(any(windows, test))]
#[derive(Default)]
pub(crate) struct KeeperApplyAttempt {
    started: bool,
}
#[cfg(any(windows, test))]
impl KeeperApplyAttempt {
    pub(crate) fn started(&self) -> bool {
        self.started
    }
    pub(crate) fn reserve(&mut self) -> NativeResult<()> {
        if self.started {
            return Err(NativeError::OutcomeUnknown);
        }
        self.started = true;
        Ok(())
    }
}

/// Native repair lineage facade delegates to the same pure model exercised by the new
/// focused fakes. Older source-included test graphs need no new repair module.
#[cfg(all(windows, not(test)))]
pub(crate) use super::super::repair::payload_record::PayloadRepairLineage as RepairLineage;
/// Matches only the strict repair selection and its exact completed predecessor. No upgrade
/// OperationRecord is fabricated and no native ownership is inferred from these bytes.
#[cfg(all(windows, not(test)))]
pub(crate) fn correlate_repair(
    operation: [u8; 16],
    user: &str,
    selected: &super::super::repair::payload_record::PayloadRepairRecord,
    predecessor: &super::super::service::journal::Journal,
) -> NativeResult<RepairLineage> {
    super::super::repair::payload_record::correlate_repair(operation, user, selected, predecessor)
}

/// Correlates the actual retained task Run result with its strict durable submission. Reading
/// this string never constructs TaskRunEvidence, a started child, or any native authority.
#[cfg(all(windows, not(test)))]
pub(crate) fn repair_submission(
    io: &super::WindowsNativeIo,
    proof: &super::SupportProof,
    operation: [u8; 16],
    deadline: &super::Deadline,
) -> NativeResult<String> {
    proof.check(io, deadline)?;
    let record = TaskActivationRecord::read(io, proof, deadline)?.ok_or(NativeError::Missing)?;
    if record.operation != operation || record.phase != Phase::RunObserved {
        return Err(NativeError::Foreign);
    }
    let submission = record
        .submission
        .as_ref()
        .filter(|value| valid_submission(value))
        .cloned()
        .ok_or(NativeError::Foreign)?;
    proof.check(io, deadline)?;
    Ok(submission)
}
