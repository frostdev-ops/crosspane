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
