use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PsField {
    Uid,
    Started,
    Executable,
}
#[derive(Clone, PartialEq, Eq)]
pub enum SignatureQuery {
    Verify,
    Describe,
    Requirement,
    Entitlements,
    VerifyRequirement(String),
}
opaque_debug!(SignatureQuery);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchctlAction {
    Print,
    PrintDisabled,
    Bootstrap,
    Bootout,
}
#[derive(Clone)]
pub enum NativeOperation {
    Process {
        pid: u32,
        field: PsField,
    },
    Signature {
        path: PathBuf,
        query: SignatureQuery,
    },
    ValidatePlist {
        path: PathBuf,
    },
    Launchctl(LaunchctlAction),
    /// The sole package opener; the adapter verifies the production manifest hash before dispatch.
    OpenAudioPackage {
        path: PathBuf,
    },
    ControlledChild {
        signature: Box<SignatureProof>,
        args: Vec<String>,
    },
}
opaque_debug!(NativeOperation, NativeCall, NativeReply, NativeCommandQueue);
/// Absolute bounded argv; environment is entirely selected-target-specific, never inherited.
#[derive(Clone)]
pub struct CommandSpec {
    pub(super) nonce: u64,
    program: PathBuf,
    args: Vec<String>,
    environment: BTreeMap<String, String>,
    pub(super) max_output: usize,
    pub(super) mutation: bool,
    pub(super) child_signature: Option<SignatureProof>,
    pub(super) authorized: Option<Instant>,
}
impl CommandSpec {
    pub fn new(target: &MacTarget, operation: NativeOperation) -> NativeResult<Self> {
        let domain = format!("gui/{}", target.paths.uid);
        let plist = target
            .paths
            .home
            .join("Library/LaunchAgents/io.frostdev.crosspane.agent.plist");
        let path_string = |path: &Path| -> NativeResult<String> {
            if !target.readable(path) {
                return Err(NativeError::Foreign);
            }
            path.to_str().map(str::to_owned).ok_or(NativeError::Invalid)
        };
        let mut child_signature = None;
        let (program, args, max_output, mutation) = match operation {
            NativeOperation::Process { pid, field } => {
                if pid == 0 {
                    return Err(NativeError::Invalid);
                }
                let field = match field {
                    PsField::Uid => "uid=",
                    PsField::Started => "lstart=",
                    PsField::Executable => "comm=",
                };
                (
                    PathBuf::from("/bin/ps"),
                    vec!["-o".into(), field.into(), "-p".into(), pid.to_string()],
                    4096,
                    false,
                )
            }
            NativeOperation::Signature { path, query } => {
                let flags = match query {
                    SignatureQuery::Verify => vec!["--verify".into(), "--strict".into()],
                    SignatureQuery::Describe => vec!["--display".into(), "--verbose=4".into()],
                    SignatureQuery::Requirement => vec!["--display".into(), "-r-".into()],
                    SignatureQuery::Entitlements => {
                        vec!["--display".into(), "--entitlements".into(), "-".into()]
                    }
                    SignatureQuery::VerifyRequirement(requirement) => {
                        if !bounded(&requirement, 4096) {
                            return Err(NativeError::Invalid);
                        }
                        vec![
                            "--verify".into(),
                            "--strict".into(),
                            format!("-R={requirement}"),
                        ]
                    }
                };
                let mut args = flags;
                args.push(path_string(&path)?);
                (
                    PathBuf::from("/usr/bin/codesign"),
                    args,
                    MAX_COMMAND_BYTES,
                    false,
                )
            }
            NativeOperation::ValidatePlist { path } => (
                PathBuf::from("/usr/bin/plutil"),
                vec!["-lint".into(), "--".into(), path_string(&path)?],
                4096,
                false,
            ),
            NativeOperation::Launchctl(action) => {
                let (args, mutation) = match action {
                    LaunchctlAction::Print => (
                        vec!["print".into(), format!("{domain}/{AGENT_LABEL}")],
                        false,
                    ),
                    LaunchctlAction::PrintDisabled => {
                        (vec!["print-disabled".into(), domain], false)
                    }
                    LaunchctlAction::Bootstrap => {
                        (vec!["bootstrap".into(), domain, path_string(&plist)?], true)
                    }
                    LaunchctlAction::Bootout => (
                        vec!["bootout".into(), format!("{domain}/{AGENT_LABEL}")],
                        true,
                    ),
                };
                (
                    PathBuf::from("/bin/launchctl"),
                    args,
                    MAX_COMMAND_BYTES,
                    mutation,
                )
            }
            NativeOperation::OpenAudioPackage { path } => {
                let directory = target.installer_dir().join("packages");
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or(NativeError::Invalid)?;
                let version = ["CrosspaneAudio-install-", "CrosspaneAudio-remove-"]
                    .iter()
                    .find_map(|prefix| name.strip_prefix(prefix))
                    .and_then(|s| s.strip_suffix(".pkg"))
                    .ok_or(NativeError::Foreign)?;
                let parts: Vec<_> = version.split('.').collect();
                if path.parent() != Some(directory.as_path())
                    || !clean(&path)
                    || parts.len() != 3
                    || parts.iter().any(|p| {
                        p.is_empty() || p.len() > 4 || !p.bytes().all(|b| b.is_ascii_digit())
                    })
                {
                    return Err(NativeError::Foreign);
                }
                (
                    PathBuf::from("/usr/bin/open"),
                    vec![
                        "-b".into(),
                        "com.apple.installer".into(),
                        "--".into(),
                        path_string(&path)?,
                    ],
                    4096,
                    true,
                )
            }
            NativeOperation::ControlledChild { signature, args } => {
                if signature.nonce != target.nonce
                    || !matches!(
                        signature.requirement.role,
                        ArtifactRole::Settings | ArtifactRole::Tutorial
                    )
                    || args.len() > 32
                    || args
                        .iter()
                        .any(|a| a.len() > 1024 || a.chars().any(char::is_control))
                {
                    return Err(NativeError::Invalid);
                }
                let program = signature.path.clone();
                child_signature = Some(*signature);
                (program, args, MAX_COMMAND_BYTES, true)
            }
        };
        let environment = BTreeMap::from([
            ("HOME".into(), path_string(&target.paths.home)?),
            (
                "TMPDIR".into(),
                target.paths.gui_tmpdir.to_string_lossy().into_owned(),
            ),
            (
                "CROSSPANE_RUNTIME_DIR".into(),
                target.runtime.to_string_lossy().into_owned(),
            ),
            ("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into()),
            ("LC_ALL".into(), "C".into()),
            ("TZ".into(), "UTC".into()),
        ]);
        Ok(Self {
            nonce: target.nonce,
            program,
            args,
            environment,
            max_output,
            mutation,
            child_signature,
            authorized: None,
        })
    }
    pub fn program(&self) -> &Path {
        &self.program
    }
    pub fn args(&self) -> &[String] {
        &self.args
    }
    pub fn environment(&self) -> &BTreeMap<String, String> {
        &self.environment
    }
    pub fn max_output(&self) -> usize {
        self.max_output
    }
    pub fn is_mutation(&self) -> bool {
        self.mutation
    }
}
impl std::fmt::Debug for CommandSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandSpec")
            .field("program", &self.program)
            .field("argument_count", &self.args.len())
            .field("mutation", &self.mutation)
            .finish()
    }
}
#[derive(Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}
impl std::fmt::Debug for CommandOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandOutput")
            .field("code", &self.code)
            .field("stdout_bytes", &self.stdout.len())
            .field("stderr_bytes", &self.stderr.len())
            .finish()
    }
}
pub trait CommandRunner: Send + Sync {
    fn run(&self, spec: &CommandSpec, deadline: &Deadline) -> NativeResult<CommandOutput>;
}
#[derive(Debug)]
pub struct SystemCommandRunner;
static CHILDREN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
struct ChildSlot;
impl ChildSlot {
    fn acquire() -> NativeResult<Self> {
        CHILDREN
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < 4).then_some(n + 1)
            })
            .map_err(|_| NativeError::Busy)?;
        Ok(Self)
    }
}
impl Drop for ChildSlot {
    fn drop(&mut self) {
        CHILDREN.fetch_sub(1, Ordering::Release);
    }
}
fn drain(
    reader: &mut (impl Read + ?Sized),
    bytes: &mut Vec<u8>,
    other: usize,
    max: usize,
    deadline: &Deadline,
) -> NativeResult<bool> {
    let mut buf = [0; 4096];
    loop {
        deadline.check()?;
        match reader.read(&mut buf) {
            Ok(0) => return Ok(true),
            Ok(n) => {
                if bytes.len() + other + n > max {
                    return Err(NativeError::Oversize);
                }
                bytes.extend_from_slice(&buf[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(NativeError::Unavailable),
        }
    }
}
impl CommandRunner for SystemCommandRunner {
    fn run(&self, spec: &CommandSpec, deadline: &Deadline) -> NativeResult<CommandOutput> {
        run_owned_child(spec, deadline, &SystemSpawner)
    }
}
type Pipes = (Box<dyn Read + Send>, Box<dyn Read + Send>);
pub(crate) trait OwnedChild: Send {
    fn pipes(&mut self) -> NativeResult<Pipes>;
    fn status(&mut self) -> NativeResult<Option<Option<i32>>>;
    fn reap(&mut self);
}
pub(crate) trait ChildSpawner: Send + Sync {
    fn spawn(&self, spec: &CommandSpec) -> NativeResult<Box<dyn OwnedChild>>;
}
struct SystemSpawner;
struct ProcessChild(std::process::Child);
impl ChildSpawner for SystemSpawner {
    fn spawn(&self, spec: &CommandSpec) -> NativeResult<Box<dyn OwnedChild>> {
        Ok(Box::new(ProcessChild(native(
            Command::new(&spec.program)
                .args(&spec.args)
                .env_clear()
                .envs(&spec.environment)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn(),
        )?)))
    }
}
impl OwnedChild for ProcessChild {
    fn pipes(&mut self) -> NativeResult<Pipes> {
        let out = self.0.stdout.take().ok_or(NativeError::Unavailable)?;
        let err = self.0.stderr.take().ok_or(NativeError::Unavailable)?;
        native(rfs::fcntl_setfl(&out, OFlags::NONBLOCK))?;
        native(rfs::fcntl_setfl(&err, OFlags::NONBLOCK))?;
        Ok((Box::new(out), Box::new(err)))
    }
    fn status(&mut self) -> NativeResult<Option<Option<i32>>> {
        Ok(native(self.0.try_wait())?.map(|s| s.code()))
    }
    fn reap(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
pub(crate) fn run_owned_child(
    spec: &CommandSpec,
    deadline: &Deadline,
    spawner: &dyn ChildSpawner,
) -> NativeResult<CommandOutput> {
    deadline.check()?;
    if spec
        .authorized
        .is_none_or(|issued| issued.elapsed() > Duration::from_millis(SUPPORT_LIFETIME_MS))
    {
        return Err(NativeError::Unsupported);
    }
    let slot = ChildSlot::acquire()?;
    let (cleanup, receive) = mpsc::sync_channel::<Box<dyn OwnedChild>>(1);
    thread::Builder::new()
        .name("installer-owned-child-reaper".into())
        .spawn(move || {
            let _slot = slot;
            if let Ok(mut child) = receive.recv() {
                child.reap();
            }
        })
        .map_err(|_| NativeError::Unavailable)?;
    deadline.check()?;
    let mut child = spawner.spawn(spec)?;
    let result = (|| {
        let (mut out, mut err) = child.pipes()?;
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        loop {
            deadline.check()?;
            let out_done = drain(
                &mut out,
                &mut stdout,
                stderr.len(),
                spec.max_output,
                deadline,
            )?;
            let err_done = drain(
                &mut err,
                &mut stderr,
                stdout.len(),
                spec.max_output,
                deadline,
            )?;
            if let Some(status) = child.status()?
                && out_done
                && err_done
            {
                return Ok(CommandOutput {
                    code: status,
                    stdout,
                    stderr,
                });
            }
            thread::sleep(Duration::from_millis(2));
        }
    })();
    if result.is_err() {
        // Only the child started above is signalled. The bounded global slot remains held
        // until reaping completes, even if the OS delays it; callers and GUI never join it.
        let _ = cleanup.try_send(child);
        if spec.mutation {
            return Err(NativeError::OutcomeUnknown);
        }
    }
    result
}

#[derive(Clone)]
pub struct NativeCall {
    pub id: u64,
    pub command: CommandSpec,
    pub timeout_ms: u64,
    pub proof: Option<SupportProof>,
}
pub struct NativeReply {
    pub id: u64,
    pub observed_at_ms: u64,
    pub source: ObservationSource,
    pub result: NativeResult<CommandOutput>,
}
struct Work {
    call: NativeCall,
    deadline: Deadline,
}
/// Four bounded workers, 32 outstanding calls including undrained replies, no caller I/O/join.
pub struct NativeCommandQueue {
    sender: Option<mpsc::SyncSender<Work>>,
    receiver: mpsc::Receiver<NativeReply>,
    io: Arc<MacNativeIo>,
    pending: BTreeMap<u64, Cancellation>,
    last_id: u64,
}
impl NativeCommandQueue {
    pub fn new(io: Arc<MacNativeIo>) -> NativeResult<Self> {
        let (sender, work) = mpsc::sync_channel::<Work>(MAX_NATIVE_CALLS);
        let work = Arc::new(Mutex::new(work));
        let (reply, receiver) = mpsc::channel();
        for _ in 0..4 {
            let (io, work, reply) = (io.clone(), work.clone(), reply.clone());
            thread::Builder::new()
                .name("installer-native-worker".into())
                .spawn(move || {
                    loop {
                        let job = match work.lock() {
                            Ok(r) => r.recv(),
                            Err(_) => break,
                        };
                        let Ok(job) = job else { break };
                        let result =
                            io.execute(&job.call.command, job.call.proof.as_ref(), &job.deadline);
                        let observed_at_ms = io.clock.now_ms();
                        if reply
                            .send(NativeReply {
                                id: job.call.id,
                                observed_at_ms,
                                source: io.target.source(),
                                result,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .map_err(|_| NativeError::Unavailable)?;
        }
        Ok(Self {
            sender: Some(sender),
            receiver,
            io,
            pending: BTreeMap::new(),
            last_id: 0,
        })
    }
    pub fn submit(&mut self, call: NativeCall) -> NativeResult<Cancellation> {
        if self.last_id == u64::MAX {
            return Err(NativeError::IdExhausted);
        }
        if call.id <= self.last_id || call.command.nonce != self.io.target.nonce {
            return Err(NativeError::Invalid);
        }
        if self.pending.len() == MAX_NATIVE_CALLS {
            return Err(NativeError::Busy);
        }
        let cancellation = Cancellation::default();
        let deadline = Deadline::new(call.timeout_ms, self.io.clock.clone(), cancellation.clone())?;
        let id = call.id;
        self.sender
            .as_ref()
            .ok_or(NativeError::Unavailable)?
            .try_send(Work { call, deadline })
            .map_err(|_| NativeError::Busy)?;
        self.pending.insert(id, cancellation.clone());
        self.last_id = id;
        Ok(cancellation)
    }
    pub fn poll(&mut self) -> Vec<NativeReply> {
        let replies: Vec<_> = self.receiver.try_iter().take(MAX_NATIVE_CALLS).collect();
        for reply in &replies {
            self.pending.remove(&reply.id);
        }
        replies
    }
}
impl Drop for NativeCommandQueue {
    fn drop(&mut self) {
        for cancellation in self.pending.values() {
            cancellation.cancel();
        }
        self.sender.take();
    }
}
impl CommandSpec {
    /// Fixed selected-user disable; no generic launchctl verb or domain is admitted.
    pub fn disable_agent(target: &MacTarget) -> NativeResult<Self> {
        let mut spec = Self::new(target, NativeOperation::Launchctl(LaunchctlAction::Print))?;
        spec.args = vec![
            "disable".into(),
            format!("gui/{}/{AGENT_LABEL}", target.paths.uid),
        ];
        spec.mutation = true;
        Ok(spec)
    }
    // Only the opaque correlated native cleanup path may construct this one-shot.
    pub(super) fn installed_erase(
        target: &MacTarget,
        signature: &SignatureProof,
    ) -> NativeResult<Self> {
        if signature.nonce != target.nonce
            || signature.requirement.role != ArtifactRole::Agent
            || signature.path != target.agent_path()
        {
            return Err(NativeError::Foreign);
        }
        let mut spec = Self::new(target, NativeOperation::Launchctl(LaunchctlAction::Print))?;
        spec.program = signature.path.clone();
        spec.args = vec!["erase-identity".into()];
        spec.max_output = 4096;
        spec.mutation = true;
        spec.child_signature = Some(signature.clone());
        Ok(spec)
    }
}
