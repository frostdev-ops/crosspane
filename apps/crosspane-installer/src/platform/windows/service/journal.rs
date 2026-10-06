//! Fixed protected supervisor record. Persistence contains intent/facts, never executable authority.
//! An external record reader cannot prove another process's in-memory latch or owned-job emptiness.

use super::super::native_io::{
    NativeError, NativeResult,
    records::{self, RecordName},
};
use super::{
    supervisor::{Generation, Supervisor},
    task::MAX_XML_BYTES,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Planned,
    StartRequested,
    Running,
    Backoff,
    StopIntent,
    Finished,
    Unknown,
}
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Journal {
    pub schema_version: u8,
    pub registration: [u8; 16],
    pub operation: [u8; 16],
    pub user: String,
    pub phase: Phase,
    pub current: Option<Generation>,
    pub stop_instance: Option<u64>,
    pub original_xml: Option<String>,
    pub restart_times: Vec<u64>,
    pub last_tick_ms: u64,
    /// Native monotonic domain observation, not a path, process or launch capability.
    pub clock_epoch: u64,
}
impl std::fmt::Debug for Journal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SupervisorJournal")
    }
}
impl Journal {
    pub fn encode(&self) -> NativeResult<Vec<u8>> {
        let data = serde_json::to_value(self).map_err(|_| NativeError::Invalid)?;
        records::encode_record(&RecordName::Supervisor, data)
    }
    pub fn decode(bytes: &[u8]) -> NativeResult<Self> {
        let record: Self = records::record_data(&RecordName::Supervisor, bytes)?;
        record.validate()?;
        Ok(record)
    }
    pub fn bind(
        &self,
        registration: [u8; 16],
        operation: [u8; 16],
        user: &str,
        clock_epoch: u64,
    ) -> NativeResult<()> {
        self.validate()?;
        if self.registration != registration
            || self.operation != operation
            || self.user != user
            || self.clock_epoch != clock_epoch
        {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
    /// Fresh native admission must supply `fresh`; disk state alone never selects a process.
    pub fn restore_model(&self, fresh: Generation, now_ms: u64) -> NativeResult<Supervisor> {
        self.validate()?;
        if self.current != Some(fresh) || now_ms < self.last_tick_ms {
            return Err(NativeError::Foreign);
        }
        let mut model = Supervisor::new(fresh, now_ms, self.restart_times.clone())?;
        match self.phase {
            Phase::StartRequested => {
                model.start_once();
            }
            Phase::StopIntent => {
                model.stop(fresh.instance)?;
            }
            // Reopening cannot prove an earlier start dispatch or reproduce a backoff clock
            // observation. Preserve uncertainty instead of resetting its delay or replaying.
            Phase::Backoff | Phase::Finished | Phase::Unknown => model.ambiguous(),
            Phase::Running => model.running(),
            Phase::Planned => {}
        }
        Ok(model)
    }
    fn validate(&self) -> NativeResult<()> {
        if self.schema_version != 1
            || self.registration == [0; 16]
            || self.operation == [0; 16]
            || self.user.is_empty()
            || self.user.len() > 256
            || self.user.contains('\0')
            || self.clock_epoch == 0
            || self.restart_times.len() > super::supervisor::MAX_RESTARTS
            || self
                .restart_times
                .iter()
                .any(|time| *time > self.last_tick_ms)
            || self.restart_times.windows(2).any(|pair| pair[0] > pair[1])
            || self
                .original_xml
                .as_ref()
                .is_some_and(|xml| xml.len() > MAX_XML_BYTES || xml.is_empty())
            || self.current.is_some_and(|current| {
                current.pid == 0 || current.creation == 0 || current.instance == 0
            })
            || (self.phase == Phase::StopIntent) != self.stop_instance.is_some()
            || self.stop_instance.is_some_and(|instance| {
                instance == 0
                    || self
                        .current
                        .is_none_or(|current| current.instance != instance)
            })
            || (self.phase != Phase::Planned && self.current.is_none())
        {
            return Err(NativeError::Invalid);
        }
        Ok(())
    }
}

#[cfg(windows)]
#[allow(dead_code)] // Lead750c0da2: selected native operation integration is held until A4.
impl Journal {
    /// Protected record observations never prove another supervisor's latch or job accounting.
    pub(crate) fn read(
        io: &super::super::native_io::WindowsNativeIo,
        proof: &super::super::native_io::SupportProof,
        deadline: &super::super::native_io::Deadline,
    ) -> NativeResult<Option<Self>> {
        io.read_record(
            proof,
            RecordName::Supervisor,
            super::super::native_io::files::MAX_RECORD_BYTES,
            deadline,
        )?
        .map(|record| Self::decode(record.bytes()))
        .transpose()
    }
    pub(crate) fn publish(
        &self,
        io: &super::super::native_io::WindowsNativeIo,
        proof: &super::super::native_io::SupportProof,
        lock: &super::super::native_io::InstallerLock,
        deadline: &super::super::native_io::Deadline,
    ) -> NativeResult<()> {
        self.validate()?;
        let publication = io.publish_record(
            proof,
            lock,
            RecordName::Supervisor,
            &self.encode()?,
            deadline,
        )?;
        if publication.native_failure.is_some()
            || publication.state != records::PublicationRecovery::NewPublished
        {
            return Err(NativeError::OutcomeUnknown);
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Journal {
    /// Native source transitions are locked and exact-current. A cold journal is never an owner
    /// factory: the caller retains its genuine job/process and exclusive namespace separately.
    // Test builds exclude the native supervisor's new locked transition wiring.
    #[cfg_attr(test, allow(dead_code, unused_imports))]
    pub(crate) fn publish_owned_transition(
        &self,
        io: &super::super::native_io::WindowsNativeIo,
        proof: &super::super::native_io::SupportProof,
        lock: &super::super::native_io::InstallerLock,
        expected: Option<&Self>,
        deadline: &super::super::native_io::Deadline,
    ) -> NativeResult<()> {
        let current = Self::read(io, proof, deadline)?;
        if current.as_ref() != expected {
            return Err(NativeError::Foreign);
        }
        self.publish(io, proof, lock, deadline)
    }
}
