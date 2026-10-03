use super::*;
use crate::agent_contract::{LastExitV1, parse_last_exit};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub uid: u32,
    pub executable: PathBuf,
    pub started_unix_ms: u64,
}
#[derive(Debug)]
pub struct AdmittedInstance {
    bootstrap: BootstrapV1,
    process: ProcessIdentity,
    signature: SignatureProof,
    endpoint: SocketEndpoint,
    observed_at_ms: u64,
    source: ObservationSource,
}
impl AdmittedInstance {
    pub fn bootstrap(&self) -> &BootstrapV1 {
        &self.bootstrap
    }
    pub fn process(&self) -> &ProcessIdentity {
        &self.process
    }
    pub fn endpoint(&self) -> &SocketEndpoint {
        &self.endpoint
    }
    pub fn observed_at_ms(&self) -> u64 {
        self.observed_at_ms
    }
    pub fn source(&self) -> ObservationSource {
        self.source
    }
    /// Re-run around an exchange. A new opaque instance never silently inherits an operation.
    pub fn revalidate(
        &self,
        io: &MacNativeIo,
        support: &SupportProof,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        support.check(io, deadline)?;
        self.endpoint.revalidate(io)?;
        let (bootstrap, process) = io.bootstrap(&self.signature, deadline)?;
        if bootstrap.instance_id != self.bootstrap.instance_id
            || process != self.process
            || bootstrap.started_unix_ms != self.bootstrap.started_unix_ms
            || bootstrap.phase_seq < self.bootstrap.phase_seq
        {
            return Err(NativeError::Foreign);
        }
        self.endpoint.revalidate(io)
    }
    pub fn admit_status(&self, status: &InstanceStatus) -> NativeResult<()> {
        if status.id != self.bootstrap.instance_id
            || status.pid != self.process.pid
            || status.uid != self.process.uid
            || admitted_spelling(Path::new(&status.exe))? != self.process.executable
            || admitted_spelling(Path::new(&status.runtime_dir))?
                != admitted_spelling(Path::new(&self.bootstrap.runtime_dir))?
            || status.started_unix_ms != self.bootstrap.started_unix_ms
        {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
}
impl MacNativeIo {
    fn ps(&self, pid: u32, field: PsField, deadline: &Deadline) -> NativeResult<CommandOutput> {
        let command = CommandSpec::new(&self.target, NativeOperation::Process { pid, field })?;
        self.execute(&command, None, deadline)
    }
    pub fn process_identity(
        &self,
        pid: u32,
        signature: &SignatureProof,
        deadline: &Deadline,
    ) -> NativeResult<ProcessIdentity> {
        if signature.requirement.role != ArtifactRole::Agent
            || signature.path != self.target.agent_path()
        {
            return Err(NativeError::Foreign);
        }
        signature.revalidate(self)?;
        let capture = || -> NativeResult<ProcessIdentity> {
            let uid = self.ps(pid, PsField::Uid, deadline)?;
            let start = self.ps(pid, PsField::Started, deadline)?;
            let exe = self.ps(pid, PsField::Executable, deadline)?;
            for reply in [&uid, &start, &exe] {
                if reply.code != Some(0) || !reply.stderr.is_empty() {
                    return Err(NativeError::Unavailable);
                }
            }
            let uid_text = one_line(&uid.stdout)?;
            if !uid_text.bytes().all(|b| b.is_ascii_digit()) {
                return Err(NativeError::Invalid);
            }
            let uid = uid_text.parse::<u32>().map_err(|_| NativeError::Invalid)?;
            let executable = admitted_spelling(Path::new(one_line(&exe.stdout)?))?;
            if uid != self.target.paths.uid || executable != signature.path {
                return Err(NativeError::Foreign);
            }
            Ok(ProcessIdentity {
                pid,
                uid,
                executable,
                started_unix_ms: parse_ps_start(&start.stdout)?,
            })
        };
        let first = capture()?;
        if capture()? != first {
            return Err(NativeError::Foreign);
        }
        signature.revalidate(self)?;
        Ok(first)
    }
    pub fn bootstrap(
        &self,
        signature: &SignatureProof,
        deadline: &Deadline,
    ) -> NativeResult<(BootstrapV1, ProcessIdentity)> {
        let path = self.target.runtime.join("bootstrap.json");
        let read = || {
            parse_bootstrap(&self.read(&path, 4096, true, deadline)?)
                .map_err(|_| NativeError::Invalid)
        };
        let first = read()?;
        let identity = self.process_identity(first.pid, signature, deadline)?;
        let second = read()?;
        if first.instance_id != second.instance_id
            || first.pid != second.pid
            || first.started_unix_ms != second.started_unix_ms
            || second.phase_seq < first.phase_seq
            || admitted_spelling(Path::new(&first.runtime_dir))? != self.target.runtime
            || second.runtime_dir != first.runtime_dir
            || identity.started_unix_ms.abs_diff(first.started_unix_ms) > 2000
        {
            return Err(NativeError::Foreign);
        }
        if self.process_identity(first.pid, signature, deadline)? != identity {
            return Err(NativeError::Foreign);
        }
        Ok((second, identity))
    }
    pub fn admit_instance(
        &self,
        support: &SupportProof,
        signature: &SignatureProof,
        deadline: &Deadline,
    ) -> NativeResult<AdmittedInstance> {
        support.check(self, deadline)?;
        if signature.requirement != support.signing.requirement
            || signature.observation.team_identifier != support.signing.observation.team_identifier
        {
            return Err(NativeError::Unsupported);
        }
        let endpoint = self.socket_endpoint()?;
        let (bootstrap, process) = self.bootstrap(signature, deadline)?;
        let observed_at_ms = self.clock.now_ms();
        endpoint.revalidate(self)?;
        Ok(AdmittedInstance {
            bootstrap,
            process,
            signature: signature.clone(),
            endpoint,
            observed_at_ms,
            source: self.target.source(),
        })
    }
    /// Two bounded absence observations. A reused/ambiguous PID is never a stopped-process proof.
    pub fn process_exited(
        &self,
        identity: &ProcessIdentity,
        deadline: &Deadline,
    ) -> NativeResult<bool> {
        for _ in 0..2 {
            let result = self.ps(identity.pid, PsField::Uid, deadline)?;
            if result.code == Some(0) {
                return Ok(false);
            }
            if result.code != Some(1) || !result.stdout.is_empty() || !result.stderr.is_empty() {
                return Err(NativeError::Unavailable);
            }
        }
        Ok(true)
    }
    /// Lifecycle truth only: clean=false is returned unchanged; callers gate replacement separately.
    pub fn exit_receipt(
        &self,
        identity: &ProcessIdentity,
        instance: u64,
        deadline: &Deadline,
    ) -> NativeResult<Option<LastExitV1>> {
        let path = self.target.state_dir().join("last_exit.json");
        if self.metadata(&path)?.is_none() {
            return Ok(None);
        }
        let receipt = parse_last_exit(&self.read(&path, 4096, true, deadline)?)
            .map_err(|_| NativeError::Invalid)?;
        if receipt.instance_id != instance
            || receipt.stopped_unix_ms < identity.started_unix_ms
            || !self.process_exited(identity, deadline)?
        {
            return Err(NativeError::Foreign);
        }
        Ok(Some(receipt))
    }
}
fn one_line(bytes: &[u8]) -> NativeResult<&str> {
    if bytes.len() > 4096 {
        return Err(NativeError::Oversize);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| NativeError::Invalid)?;
    let text = text.strip_suffix('\n').unwrap_or(text).trim_matches(' ');
    if text.is_empty() || text.chars().any(char::is_control) {
        return Err(NativeError::Invalid);
    }
    Ok(text)
}
/// Strict C-format UTC ps date, including weekday/calendar consistency, without new OS bindings.
pub fn parse_ps_start(bytes: &[u8]) -> NativeResult<u64> {
    if bytes.len() > 128 {
        return Err(NativeError::Oversize);
    }
    let text = one_line(bytes)?;
    let fields: Vec<_> = text.split(' ').filter(|f| !f.is_empty()).collect();
    if fields.len() != 5 {
        return Err(NativeError::Invalid);
    }
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| *m == fields[1])
    .ok_or(NativeError::Invalid)?
        + 1;
    let number = |s: &str| -> NativeResult<u64> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return Err(NativeError::Invalid);
        }
        s.parse().map_err(|_| NativeError::Invalid)
    };
    let (day, year) = (number(fields[2])?, number(fields[4])?);
    let time: Vec<_> = fields[3].split(':').collect();
    if fields[2].len() > 2
        || fields[2].starts_with('0')
        || fields[4].len() != 4
        || time.len() != 3
        || time.iter().any(|v| v.len() != 2)
        || !(1970..=9999).contains(&year)
    {
        return Err(NativeError::Invalid);
    }
    let (hour, minute, second) = (number(time[0])?, number(time[1])?, number(time[2])?);
    let leap = |y: u64| y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400));
    let lengths = [
        31,
        if leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if day == 0 || day > lengths[month - 1] || hour > 23 || minute > 59 || second > 59 {
        return Err(NativeError::Invalid);
    }
    let days = (1970..year)
        .map(|y| if leap(y) { 366 } else { 365 })
        .sum::<u64>()
        + lengths[..month - 1].iter().sum::<u64>()
        + day
        - 1;
    if fields[0] != ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][((days + 4) % 7) as usize] {
        return Err(NativeError::Invalid);
    }
    Ok((days * 86400 + hour * 3600 + minute * 60 + second) * 1000)
}
