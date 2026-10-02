//! Native observations do not imply readiness. Scratch authority stays inside its own target.
use crate::agent_contract::{BootstrapV1, InstanceStatus, ObservationSource, parse_bootstrap};
use rustix::fs::{self as rfs, AtFlags, FlockOperation, Mode, OFlags, ResolveFlags};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::fs::{DirBuilderExt, MetadataExt},
    },
    path::{Component, Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

pub const MAX_FILE_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_COMMAND_BYTES: usize = 64 * 1024;
pub const MAX_NATIVE_TIMEOUT_MS: u64 = 120_000;
pub const SUPPORT_LIFETIME: Duration = Duration::from_secs(5);
static NONCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NativeError {
    #[error("invalid or unadmitted value")]
    Invalid,
    #[error("unavailable native observation")]
    Unavailable,
    #[error("foreign or unsafe target")]
    Foreign,
    #[error("bounded operation timed out")]
    Timeout,
    #[error("operation cancelled")]
    Cancelled,
    #[error("bounded resource busy")]
    Busy,
    #[error("bounded result too large")]
    Oversize,
    #[error("support proof missing or stale")]
    Unsupported,
    #[error("mutation may have completed; inspect before retry")]
    OutcomeUnknown,
}
type Result<T> = std::result::Result<T, NativeError>;
fn native<T>(r: std::result::Result<T, impl std::fmt::Debug>) -> Result<T> {
    r.map_err(|_| NativeError::Unavailable)
}

#[derive(Clone, Default, Debug)]
pub struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}
/// One absolute monotonic deadline is shared across every part of an operation.
#[derive(Clone, Debug)]
pub struct Deadline {
    end: Instant,
    cancellation: Cancellation,
}
impl Deadline {
    pub fn new(timeout_ms: u64, cancellation: Cancellation) -> Result<Self> {
        if !(1..=MAX_NATIVE_TIMEOUT_MS).contains(&timeout_ms) {
            return Err(NativeError::Invalid);
        }
        Ok(Self {
            end: Instant::now() + Duration::from_millis(timeout_ms),
            cancellation,
        })
    }
    pub fn check(&self) -> Result<()> {
        if self.cancellation.0.load(Ordering::Acquire) {
            return Err(NativeError::Cancelled);
        }
        if Instant::now() >= self.end {
            return Err(NativeError::Timeout);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetPaths {
    pub uid: u32,
    pub home: PathBuf,
    pub prefix: PathBuf,
    pub config_home: PathBuf,
    pub state_home: PathBuf,
    pub data_home: PathBuf,
    /// XDG_RUNTIME_DIR; `runtime_override` is the already-resolved CROSSPANE_RUNTIME_DIR.
    pub runtime_home: PathBuf,
    pub runtime_override: Option<PathBuf>,
}
#[derive(Clone, Debug)]
pub struct LinuxTarget {
    paths: TargetPaths,
    runtime: PathBuf,
    nonce: u64,
    scratch: bool,
}
fn clean(path: &Path) -> bool {
    path.is_absolute()
        && path.as_os_str().len() <= 4096
        && path.to_str().is_some_and(|s| {
            !s.chars().any(char::is_control)
                && s.split('/')
                    .skip(1)
                    .all(|p| !p.is_empty() && p != "." && p != "..")
        })
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}
impl LinuxTarget {
    fn make(paths: TargetPaths, scratch: bool) -> Result<Self> {
        if paths.uid != rustix::process::geteuid().as_raw() || paths.uid == 0 {
            return Err(NativeError::Foreign);
        }
        if !clean(&paths.home) || paths.home == Path::new("/") {
            return Err(NativeError::Invalid);
        }
        for path in [
            &paths.prefix,
            &paths.config_home,
            &paths.state_home,
            &paths.data_home,
        ] {
            if !clean(path) || !path.starts_with(&paths.home) || path == &paths.home {
                return Err(NativeError::Foreign);
            }
        }
        if !scratch && !paths.home.starts_with("/home") {
            return Err(NativeError::Foreign);
        }
        let runtime = paths
            .runtime_override
            .clone()
            .unwrap_or_else(|| paths.runtime_home.join("crosspane"));
        if !clean(&paths.runtime_home)
            || !clean(&runtime)
            || (!scratch && paths.runtime_home != Path::new(&format!("/run/user/{}", paths.uid)))
            || !(runtime.starts_with(&paths.runtime_home) || runtime.starts_with(&paths.home))
            || runtime == paths.runtime_home
            || runtime.join("agent.sock").as_os_str().len() > 107
        {
            return Err(NativeError::Foreign);
        }
        let roots = [
            paths.prefix.join("bin"),
            paths.config_home.join("crosspane"),
            paths.state_home.join("crosspane"),
            paths.data_home.join("crosspane"),
            runtime.clone(),
        ];
        for (i, a) in roots.iter().enumerate() {
            if roots
                .iter()
                .skip(i + 1)
                .any(|b| a.starts_with(b) || b.starts_with(a))
            {
                return Err(NativeError::Foreign);
            }
        }
        Ok(Self {
            paths,
            runtime,
            nonce: NONCE.fetch_add(1, Ordering::Relaxed),
            scratch,
        })
    }
    pub fn paths(&self) -> &TargetPaths {
        &self.paths
    }
    pub fn runtime_dir(&self) -> &Path {
        &self.runtime
    }
    pub fn socket_path(&self) -> PathBuf {
        self.runtime.join("agent.sock")
    }
    pub fn agent_path(&self) -> PathBuf {
        self.paths.prefix.join("bin/crosspane-agent")
    }
    pub fn source(&self) -> ObservationSource {
        if self.scratch {
            ObservationSource::Demo
        } else {
            ObservationSource::Live
        }
    }
}

/// Detector facts are admitted as a complete set, never as an environment-string override.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupportObservations {
    pub uid: u32,
    pub architecture: String,
    pub arch_based: bool,
    pub hyprland_version: [u16; 3],
    pub protocols_ready: bool,
    pub runtime_libraries_ready: bool,
    pub uwsm_managed: bool,
    pub graphical_target_active: bool,
    pub graphical_sessions: usize,
    pub session_id: String,
    pub session_type: String,
    pub seat: String,
    pub active: bool,
}
#[derive(Clone, Debug)]
pub struct SupportProof {
    nonce: u64,
    issued: Instant,
    facts: SupportObservations,
    valid: Arc<AtomicBool>,
}
impl SupportProof {
    pub(crate) fn admit(io: &LinuxNativeIo, facts: SupportObservations) -> Result<Self> {
        if facts.uid != io.target.paths.uid
            || !matches!(facts.architecture.as_str(), "x86_64" | "aarch64")
            || facts.architecture != std::env::consts::ARCH
            || !facts.arch_based
            || facts.hyprland_version < [0, 56, 0]
            || !facts.protocols_ready
            || !facts.runtime_libraries_ready
            || !facts.uwsm_managed
            || !facts.graphical_target_active
            || facts.graphical_sessions != 1
            || facts.session_type != "wayland"
            || !facts.active
            || [&facts.session_id, &facts.seat]
                .iter()
                .any(|s| s.is_empty() || s.len() > 64 || s.chars().any(char::is_control))
        {
            return Err(NativeError::Unsupported);
        }
        io.validate_target()?;
        Ok(Self {
            nonce: io.target.nonce,
            issued: Instant::now(),
            facts,
            valid: Arc::new(AtomicBool::new(true)),
        })
    }
    pub fn revalidate(&self, io: &LinuxNativeIo, current: &SupportObservations) -> Result<()> {
        self.check(io)?;
        if current != &self.facts {
            self.valid.store(false, Ordering::Release);
            return Err(NativeError::Unsupported);
        }
        io.validate_target()
    }
    pub fn check(&self, io: &LinuxNativeIo) -> Result<()> {
        self.check_target(&io.target)
    }
    fn check_target(&self, target: &LinuxTarget) -> Result<()> {
        if !self.valid.load(Ordering::Acquire)
            || self.nonce != target.nonce
            || self.issued.elapsed() > SUPPORT_LIFETIME
        {
            return Err(NativeError::Unsupported);
        }
        Ok(())
    }
}

fn dbus_address(value: &str, runtime: &Path) -> Result<(String, PathBuf)> {
    let encoded = value
        .strip_prefix("unix:path=")
        .ok_or(NativeError::Foreign)?;
    if encoded.contains([';', ',']) {
        return Err(NativeError::Foreign);
    }
    let mut decoded = Vec::new();
    let mut bytes = encoded.bytes();
    while let Some(b) = bytes.next() {
        decoded.push(if b == b'%' {
            let a = bytes.next().ok_or(NativeError::Invalid)?;
            let b = bytes.next().ok_or(NativeError::Invalid)?;
            let hex = |v: u8| char::from(v).to_digit(16).ok_or(NativeError::Invalid);
            (hex(a)? * 16 + hex(b)?) as u8
        } else {
            b
        });
    }
    let path = std::str::from_utf8(&decoded).map_err(|_| NativeError::Invalid)?;
    if !clean(Path::new(path))
        || !Path::new(path).starts_with(runtime)
        || Path::new(path) == runtime
    {
        return Err(NativeError::Foreign);
    }
    let path = PathBuf::from(path);
    let mut admitted = String::from("unix:path=");
    for b in decoded {
        if b.is_ascii_alphanumeric() || b"/_-.".contains(&b) {
            admitted.push(char::from(b));
        } else {
            admitted.push_str(&format!("%{b:02X}"));
        }
    }
    Ok((admitted, path))
}
#[derive(Clone, Debug)]
struct BusEndpoint {
    path: PathBuf,
    parent: Arc<OwnedFd>,
    identity: (u64, u64),
}
impl BusEndpoint {
    fn observe(target: &LinuxTarget, path: &Path) -> Result<(OwnedFd, rfs::Stat)> {
        let parent = walk_dir(target, path.parent().ok_or(NativeError::Invalid)?, None)?
            .ok_or(NativeError::Unavailable)?;
        if native(rfs::fstat(&parent))?.st_mode & 0o777 != 0o700 {
            return Err(NativeError::Foreign);
        }
        let stat = rfs::statat(
            &parent,
            path.file_name().ok_or(NativeError::Invalid)?,
            AtFlags::SYMLINK_NOFOLLOW,
        )
        .map_err(|_| NativeError::Foreign)?;
        // A session bus may have mode 0666 inside its admitted private runtime directory.
        if stat.st_uid != target.paths.uid || stat.st_mode & 0o170000 != 0o140000 {
            return Err(NativeError::Foreign);
        }
        Ok((parent, stat))
    }
    fn admit(target: &LinuxTarget, path: PathBuf) -> Result<Self> {
        let (parent, stat) = Self::observe(target, &path)?;
        Ok(Self {
            path,
            parent: Arc::new(parent),
            identity: (stat.st_dev, stat.st_ino),
        })
    }
    fn revalidate(&self, target: &LinuxTarget) -> Result<()> {
        let (parent, stat) = Self::observe(target, &self.path)?;
        let old = native(rfs::fstat(self.parent.as_ref()))?;
        let now = native(rfs::fstat(&parent))?;
        if self.identity != (stat.st_dev, stat.st_ino)
            || (old.st_dev, old.st_ino) != (now.st_dev, now.st_ino)
        {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
}
#[derive(Clone, Debug)]
pub struct ChildEnvironment {
    nonce: u64,
    values: BTreeMap<String, String>,
    bus: Option<BusEndpoint>,
}
impl ChildEnvironment {
    pub fn selected(target: &LinuxTarget, session: BTreeMap<String, String>) -> Result<Self> {
        let mut values = BTreeMap::from([
            ("PATH".into(), "/usr/bin:/bin".into()),
            (
                "HOME".into(),
                target.paths.home.to_string_lossy().into_owned(),
            ),
            ("LC_ALL".into(), "C".into()),
            ("TZ".into(), "UTC".into()),
        ]);
        for (key, path) in [
            ("XDG_CONFIG_HOME", &target.paths.config_home),
            ("XDG_STATE_HOME", &target.paths.state_home),
            ("XDG_DATA_HOME", &target.paths.data_home),
            ("XDG_RUNTIME_DIR", &target.paths.runtime_home),
            ("CROSSPANE_RUNTIME_DIR", &target.runtime),
        ] {
            values.insert(key.into(), path.to_string_lossy().into_owned());
        }
        let mut bus = None;
        for (key, mut value) in session {
            if !matches!(
                key.as_str(),
                "DBUS_SESSION_BUS_ADDRESS"
                    | "WAYLAND_DISPLAY"
                    | "HYPRLAND_INSTANCE_SIGNATURE"
                    | "XDG_SESSION_ID"
                    | "XDG_SESSION_TYPE"
            ) || value.is_empty()
                || value.len() > 4096
                || value.chars().any(char::is_control)
            {
                return Err(NativeError::Invalid);
            }
            if key == "DBUS_SESSION_BUS_ADDRESS" {
                let (address, path) = dbus_address(&value, &target.paths.runtime_home)?;
                bus = Some(BusEndpoint::admit(target, path)?);
                value = address;
            }
            if key == "WAYLAND_DISPLAY" && (value.contains('/') || value == "." || value == "..") {
                return Err(NativeError::Foreign);
            }
            values.insert(key, value);
        }
        if target.scratch {
            values.insert(
                "DBUS_SYSTEM_BUS_ADDRESS".into(),
                "unix:path=/nonexistent/crosspane-system-bus".into(),
            );
        }
        if values.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>() > 32 * 1024 {
            return Err(NativeError::Oversize);
        }
        Ok(Self {
            nonce: target.nonce,
            values,
            bus,
        })
    }
    pub fn values(&self) -> &BTreeMap<String, String> {
        &self.values
    }
}
#[derive(Clone, Debug)]
pub struct CommandSpec {
    executable: PathBuf,
    argv: Vec<String>,
    environment: ChildEnvironment,
    output_limit: usize,
    admission: Option<(LinuxTarget, Option<SupportProof>)>,
}
impl CommandSpec {
    pub fn new(
        executable: PathBuf,
        argv: Vec<String>,
        environment: ChildEnvironment,
        output_limit: usize,
    ) -> Result<Self> {
        if !clean(&executable)
            || argv.len() > 32
            || output_limit == 0
            || output_limit > MAX_COMMAND_BYTES
            || argv
                .iter()
                .any(|s| s.len() > 4096 || s.chars().any(char::is_control))
            || argv.iter().map(String::len).sum::<usize>() > 32 * 1024
        {
            return Err(NativeError::Invalid);
        }
        approved_executable(&executable, &argv)?;
        Ok(Self {
            executable,
            argv,
            environment,
            output_limit,
            admission: None,
        })
    }
    pub fn executable(&self) -> &Path {
        &self.executable
    }
    pub fn argv(&self) -> &[String] {
        &self.argv
    }
    pub fn environment(&self) -> &ChildEnvironment {
        &self.environment
    }
    pub fn output_limit(&self) -> usize {
        self.output_limit
    }
}
// Only /bin/ps -> /usr/bin/ps is admitted. No generic aliases/canonicalization.
// WP-4.7c must pin systemd/private and every used bus before admitting any systemctl argv.
fn approved_executable(path: &Path, argv: &[String]) -> Result<&'static str> {
    let a: Vec<_> = argv.iter().map(String::as_str).collect();
    match (path.to_str(), a.as_slice()) {
        (Some("/bin/ps"), ["-o", "lstart=" | "comm=", "-p", pid])
            if pid
                .parse::<u32>()
                .is_ok_and(|v| v != 0 && v.to_string() == *pid) =>
        {
            Ok("/usr/bin/ps")
        }
        _ => Err(NativeError::Invalid),
    }
}
fn executable_stat(stat: &rfs::Stat) -> Result<()> {
    if stat.st_uid != 0
        || stat.st_mode & 0o170000 != 0o100000
        || stat.st_mode & 0o6022 != 0
        || stat.st_mode & 0o111 == 0
    {
        Err(NativeError::Foreign)
    } else {
        Ok(())
    }
}
fn socket_stat(socket: &rfs::Stat, uid: u32) -> Result<()> {
    if socket.st_uid != uid || socket.st_mode & 0o170000 != 0o140000 || socket.st_mode & 0o022 != 0
    {
        Err(NativeError::Foreign)
    } else {
        Ok(())
    }
}
fn executable_fd(path: &str) -> Result<OwnedFd> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut dir = native(rfs::open("/", flags, Mode::empty()))?;
    let path = Path::new(path);
    for component in path.parent().ok_or(NativeError::Invalid)?.components() {
        let s = native(rfs::fstat(&dir))?;
        if s.st_uid != 0 || s.st_mode & 0o022 != 0 {
            return Err(NativeError::Foreign);
        }
        if let Component::Normal(name) = component {
            dir =
                rfs::openat(&dir, name, flags, Mode::empty()).map_err(|_| NativeError::Foreign)?;
        }
    }
    let s = native(rfs::fstat(&dir))?;
    if s.st_uid != 0 || s.st_mode & 0o022 != 0 {
        return Err(NativeError::Foreign);
    }
    let fd = rfs::openat(
        &dir,
        path.file_name().ok_or(NativeError::Invalid)?,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| NativeError::Foreign)?;
    executable_stat(&native(rfs::fstat(&fd))?)?;
    Ok(fd)
}
#[derive(Debug)]
pub struct CommandOutput {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}
pub trait CommandRunner: Send + Sync {
    fn run(&self, command: &CommandSpec, deadline: &Deadline) -> Result<CommandOutput>;
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessFacts {
    pub uid: u32,
    pub executable: PathBuf,
    pub generation: u64,
}
pub trait ProcessProbe: Send + Sync {
    fn snapshot(&self, pid: u32, deadline: &Deadline) -> Result<ProcessFacts>;
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub uid: u32,
    pub executable: PathBuf,
    pub started_unix_ms: u64,
    pub generation: u64,
}
struct SystemRunner;
static PROCESS_CLEANUPS: AtomicUsize = AtomicUsize::new(0);
static PROCESS_LAUNCHES: AtomicUsize = AtomicUsize::new(0);
struct CleanupSlot(&'static AtomicUsize);
impl Drop for CleanupSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}
// The caller never waits for a noncooperative open/exec handshake; its worker retains admission.
fn bounded_launch<T: Send + 'static>(
    counter: &'static AtomicUsize,
    deadline: &Deadline,
    launch: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    deadline.check()?;
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < 4).then_some(n + 1)
        })
        .map_err(|_| NativeError::Busy)?;
    let slot = CleanupSlot(counter);
    let (send, receive) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("installer-command".into())
        .spawn(move || {
            let _slot = slot;
            let _ = send.send(launch());
        })
        .map_err(|_| NativeError::Unavailable)?;
    loop {
        deadline.check()?;
        match receive.recv_timeout(Duration::from_millis(2)) {
            Ok(result) => {
                deadline.check()?;
                return result;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(NativeError::Unavailable),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}
trait Cleanup: Send + 'static {
    fn terminate(&mut self);
    fn reaped(&mut self) -> bool;
}
impl Cleanup for Child {
    fn terminate(&mut self) {
        let _ = self.kill();
    }
    fn reaped(&mut self) -> bool {
        matches!(self.try_wait(), Ok(Some(_)))
    }
}
// Admission precedes spawn. A stalled termination keeps its slot until actually reaped.
fn cleanup_admission<T: Cleanup>(counter: &'static AtomicUsize) -> Result<SyncSender<T>> {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < 4).then_some(n + 1)
        })
        .map_err(|_| NativeError::Busy)?;
    let slot = CleanupSlot(counter);
    let (send, receive) = mpsc::sync_channel::<T>(1);
    thread::Builder::new()
        .name("installer-child-cleanup".into())
        .spawn(move || {
            let _slot = slot;
            if let Ok(mut child) = receive.recv() {
                child.terminate();
                while !child.reaped() {
                    thread::sleep(Duration::from_millis(10));
                }
            }
        })
        .map_err(|_| NativeError::Unavailable)?;
    Ok(send)
}
fn drain(pipe: &mut impl Read, bytes: &mut Vec<u8>, limit: usize) -> Result<bool> {
    let mut buffer = [0; 4096];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(n) => {
                if bytes.len() + n > limit {
                    return Err(NativeError::Oversize);
                }
                bytes.extend_from_slice(&buffer[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err(NativeError::Unavailable),
        }
    }
}
impl CommandRunner for SystemRunner {
    fn run(&self, spec: &CommandSpec, deadline: &Deadline) -> Result<CommandOutput> {
        // LinuxNativeIo's private executor already owns bounded admission for this whole operation.
        system_command(spec, deadline)
    }
}
fn system_command(spec: &CommandSpec, deadline: &Deadline) -> Result<CommandOutput> {
    use std::os::unix::process::CommandExt;
    deadline.check()?;
    let (target, proof) = spec.admission.as_ref().ok_or(NativeError::Unsupported)?;
    let cleanup = cleanup_admission::<Child>(&PROCESS_CLEANUPS)?;
    let executable = executable_fd(approved_executable(&spec.executable, &spec.argv)?)?;
    validate_target(target)?;
    if let Some(bus) = &spec.environment.bus {
        bus.revalidate(target)?;
    }
    if let Some(proof) = proof {
        proof.check_target(target)?;
    }
    deadline.check()?;
    let mut child = native(
        Command::new(format!("/proc/self/fd/{}", executable.as_raw_fd()))
            .arg0(&spec.executable)
            .args(&spec.argv)
            .env_clear()
            .envs(&spec.environment.values)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn(),
    )?;
    let result = (|| {
        let mut stdout = child.stdout.take().ok_or(NativeError::Unavailable)?;
        let mut stderr = child.stderr.take().ok_or(NativeError::Unavailable)?;
        native(rfs::fcntl_setfl(&stdout, OFlags::NONBLOCK))?;
        native(rfs::fcntl_setfl(&stderr, OFlags::NONBLOCK))?;
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut status = None;
        loop {
            deadline.check()?;
            let out_done = drain(&mut stdout, &mut out, spec.output_limit)?;
            let err_done = drain(
                &mut stderr,
                &mut err,
                spec.output_limit.saturating_sub(out.len()),
            )?;
            if status.is_none() {
                status = native(child.try_wait())?;
            }
            if let Some(status) = status.filter(|_| out_done && err_done) {
                return Ok(CommandOutput {
                    code: status.code(),
                    stdout: out,
                    stderr: err,
                });
            }
            thread::sleep(Duration::from_millis(2));
        }
    })();
    if result.is_err() {
        let _ = cleanup.try_send(child);
    }
    result
}
struct ProcProbe;
fn proc_generation(bytes: &[u8]) -> Result<u64> {
    if bytes.len() > 4096 {
        return Err(NativeError::Oversize);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| NativeError::Invalid)?;
    text.rsplit_once(')')
        .and_then(|(_, s)| s.split_whitespace().nth(19))
        .and_then(|s| s.parse().ok())
        .ok_or(NativeError::Invalid)
}
impl ProcessProbe for ProcProbe {
    fn snapshot(&self, pid: u32, deadline: &Deadline) -> Result<ProcessFacts> {
        deadline.check()?;
        if pid == 0 {
            return Err(NativeError::Invalid);
        }
        let root = PathBuf::from(format!("/proc/{pid}"));
        let uid = native(fs::metadata(&root))?.uid();
        let executable = native(fs::read_link(root.join("exe")))?;
        let mut bytes = Vec::new();
        native(
            native(File::open(root.join("stat")))?
                .take(4097)
                .read_to_end(&mut bytes),
        )?;
        let generation = proc_generation(&bytes)?;
        deadline.check()?;
        Ok(ProcessFacts {
            uid,
            executable,
            generation,
        })
    }
}

enum FileOperation<'a> {
    Mkdir(&'a OwnedFd, &'a std::ffi::OsStr),
    Write(&'a mut File, &'a [u8]),
    FileSync(&'a File),
    Rename(&'a OwnedFd, &'a str, &'a str),
    ParentSync(&'a OwnedFd),
}
trait FileIo: Send + Sync {
    fn apply(&self, operation: FileOperation<'_>) -> Result<()>;
}
struct NativeFiles;
impl FileIo for NativeFiles {
    fn apply(&self, operation: FileOperation<'_>) -> Result<()> {
        match operation {
            FileOperation::Mkdir(dir, name) => native(rfs::mkdirat(dir, name, Mode::RWXU)),
            FileOperation::Write(file, bytes) => native(file.write_all(bytes)),
            FileOperation::FileSync(file) => native(file.sync_all()),
            FileOperation::Rename(dir, from, to) => {
                rfs::renameat(dir, from, dir, to).map_err(|_| NativeError::OutcomeUnknown)
            }
            FileOperation::ParentSync(dir) => {
                rfs::fsync(dir).map_err(|_| NativeError::OutcomeUnknown)
            }
        }
    }
}
// Walk from / through opened fds: O_NOFOLLOW on just the selected root is insufficient.
fn walk_dir(
    target: &LinuxTarget,
    path: &Path,
    files: Option<&dyn FileIo>,
) -> Result<Option<OwnedFd>> {
    if !clean(path) {
        return Err(NativeError::Foreign);
    }
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let mut dir = native(rfs::open("/", flags, Mode::empty()))?;
    let mut current = PathBuf::from("/");
    let check = |fd: &OwnedFd, path: &Path| {
        let s = native(rfs::fstat(fd))?;
        let selected =
            path.starts_with(&target.paths.home) || path.starts_with(&target.paths.runtime_home);
        let trusted_tmp = target.scratch
            && path == Path::new("/tmp")
            && s.st_uid == 0
            && s.st_mode & 0o1777 == 0o1777;
        if (!trusted_tmp && s.st_mode & 0o022 != 0)
            || (selected && s.st_uid != target.paths.uid)
            || (!selected && s.st_uid != 0 && s.st_uid != target.paths.uid)
        {
            Err(NativeError::Foreign)
        } else {
            Ok(())
        }
    };
    check(&dir, &current)?;
    for component in path.components().skip(1) {
        let Component::Normal(name) = component else {
            return Err(NativeError::Foreign);
        };
        current.push(name);
        let child = match rfs::openat(&dir, name, flags, Mode::empty()) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => {
                let Some(files) = files else {
                    return Ok(None);
                };
                files.apply(FileOperation::Mkdir(&dir, name))?;
                files.apply(FileOperation::ParentSync(&dir))?;
                native(rfs::openat(&dir, name, flags, Mode::empty()))?
            }
            Err(_) => return Err(NativeError::Foreign),
        };
        check(&child, &current)?;
        dir = child;
    }
    Ok(Some(dir))
}
fn validate_target(target: &LinuxTarget) -> Result<()> {
    walk_dir(target, &target.paths.home, None)?.ok_or(NativeError::Unavailable)?;
    for path in [
        &target.paths.home,
        &target.paths.prefix,
        &target.paths.config_home,
        &target.paths.state_home,
        &target.paths.data_home,
        &target.paths.runtime_home,
        &target.runtime,
    ] {
        walk_dir(target, path, None)?;
    }
    Ok(())
}
#[cfg(test)]
type CommandAdmission = dyn Fn(&LinuxTarget) -> Result<()> + Send + Sync;
pub struct LinuxNativeIo {
    target: LinuxTarget,
    runner: Arc<dyn CommandRunner>,
    probe: Arc<dyn ProcessProbe>,
    files: Arc<dyn FileIo>,
    #[cfg(test)]
    command_admission: Option<Arc<CommandAdmission>>,
    #[cfg(test)]
    command_workers: &'static AtomicUsize,
    #[cfg(test)]
    pub(crate) socket_uid: std::sync::Mutex<Option<u32>>,
    #[cfg(test)]
    pub(crate) peer_uid: std::sync::Mutex<Option<u32>>,
}
impl std::fmt::Debug for LinuxNativeIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LinuxNativeIo { .. }")
    }
}
impl LinuxNativeIo {
    /// Production construction has no fake runner, probe or scratch-proof override.
    pub fn selected(paths: TargetPaths) -> Result<Self> {
        let io = Self {
            target: LinuxTarget::make(paths, false)?,
            runner: Arc::new(SystemRunner),
            probe: Arc::new(ProcProbe),
            files: Arc::new(NativeFiles),
            #[cfg(test)]
            command_admission: None,
            #[cfg(test)]
            command_workers: &PROCESS_LAUNCHES,
            #[cfg(test)]
            socket_uid: std::sync::Mutex::new(None),
            #[cfg(test)]
            peer_uid: std::sync::Mutex::new(None),
        };
        io.validate_target()?;
        Ok(io)
    }
    /// Creates a new private directory under /tmp. Existing directories and user homes are refused.
    pub fn scratch(
        root: &Path,
        runner: Arc<dyn CommandRunner>,
        probe: Arc<dyn ProcessProbe>,
    ) -> Result<Self> {
        if !clean(root) || root.parent() != Some(Path::new("/tmp")) {
            return Err(NativeError::Foreign);
        }
        native(fs::DirBuilder::new().mode(0o700).create(root))?;
        let paths = TargetPaths {
            uid: rustix::process::geteuid().as_raw(),
            home: root.into(),
            prefix: root.join(".local"),
            config_home: root.join(".config"),
            state_home: root.join(".local/state"),
            data_home: root.join(".local/share"),
            runtime_home: root.join("run"),
            runtime_override: None,
        };
        Ok(Self {
            target: LinuxTarget::make(paths, true)?,
            runner,
            probe,
            files: Arc::new(NativeFiles),
            #[cfg(test)]
            command_admission: None,
            #[cfg(test)]
            command_workers: &PROCESS_LAUNCHES,
            #[cfg(test)]
            socket_uid: std::sync::Mutex::new(None),
            #[cfg(test)]
            peer_uid: std::sync::Mutex::new(None),
        })
    }
    pub fn target(&self) -> &LinuxTarget {
        &self.target
    }
    /// External harnesses can admit facts only for a target created by `scratch`.
    pub fn scratch_support(&self, facts: SupportObservations) -> Result<SupportProof> {
        if !self.target.scratch {
            return Err(NativeError::Foreign);
        }
        SupportProof::admit(self, facts)
    }
    pub fn validate_target(&self) -> Result<()> {
        validate_target(&self.target)
    }
    fn walk_dir(&self, path: &Path, create: bool) -> Result<Option<OwnedFd>> {
        walk_dir(&self.target, path, create.then_some(self.files.as_ref()))
    }
    /// Requires a current target-bound proof and full target validation; follows no symlinks.
    /// The returned fd anchors mutations relative to the returned basename.
    pub(crate) fn parent_for_mutation(
        &self,
        proof: &SupportProof,
        path: &Path,
    ) -> Result<(OwnedFd, String)> {
        proof.check(self)?;
        self.validate_target()?;
        self.parent(path)
    }
    fn parent(&self, path: &Path) -> Result<(OwnedFd, String)> {
        if !clean(path) {
            return Err(NativeError::Foreign);
        }
        let root = if path.starts_with(&self.target.paths.home) {
            &self.target.paths.home
        } else if path.starts_with(&self.target.paths.runtime_home) {
            &self.target.paths.runtime_home
        } else {
            return Err(NativeError::Foreign);
        };
        let relative = path.strip_prefix(root).map_err(|_| NativeError::Foreign)?;
        let name = relative
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or(NativeError::Invalid)?
            .to_owned();
        let anchor = self
            .walk_dir(root, false)?
            .ok_or(NativeError::Unavailable)?;
        let dir = rfs::openat2(
            &anchor,
            relative
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS,
        )
        .map_err(|_| NativeError::Foreign)?;
        let stat = native(rfs::fstat(&dir))?;
        if stat.st_uid != self.target.paths.uid || stat.st_mode & 0o022 != 0 {
            return Err(NativeError::Foreign);
        }
        for ancestor in relative
            .ancestors()
            .skip(1)
            .filter(|p| !p.as_os_str().is_empty())
        {
            let fd = rfs::openat2(
                &anchor,
                ancestor,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
                ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS,
            )
            .map_err(|_| NativeError::Foreign)?;
            let s = native(rfs::fstat(&fd))?;
            if s.st_uid != self.target.paths.uid || s.st_mode & 0o022 != 0 {
                return Err(NativeError::Foreign);
            }
        }
        Ok((dir, name))
    }
    pub fn metadata(&self, path: &Path) -> Result<Option<rfs::Stat>> {
        let (dir, name) = self.parent(path)?;
        match rfs::statat(&dir, &name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(s)
                if s.st_uid == self.target.paths.uid
                    && s.st_mode & 0o022 == 0
                    && s.st_mode & 0o170000 != 0o120000 =>
            {
                Ok(Some(s))
            }
            Ok(_) => Err(NativeError::Foreign),
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(_) => Err(NativeError::Unavailable),
        }
    }
    pub fn create_private_dir(&self, proof: &SupportProof, path: &Path) -> Result<()> {
        proof.check(self)?;
        self.validate_target()?;
        if !clean(path) {
            return Err(NativeError::Foreign);
        }
        let root = if path.starts_with(&self.target.paths.home) {
            &self.target.paths.home
        } else {
            &self.target.paths.runtime_home
        };
        let relative = path.strip_prefix(root).map_err(|_| NativeError::Foreign)?;
        if relative.as_os_str().is_empty() {
            return Err(NativeError::Foreign);
        }
        let dir = self.walk_dir(path, true)?.ok_or(NativeError::Unavailable)?;
        if native(rfs::fstat(&dir))?.st_mode & 0o777 != 0o700 {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
    pub fn read(&self, path: &Path, limit: usize, private: bool) -> Result<Vec<u8>> {
        if limit == 0 || limit > MAX_FILE_BYTES {
            return Err(NativeError::Invalid);
        }
        let (dir, name) = self.parent(path)?;
        if private && native(rfs::fstat(&dir))?.st_mode & 0o777 != 0o700 {
            return Err(NativeError::Foreign);
        }
        let fd = rfs::openat(
            &dir,
            &name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| NativeError::Foreign)?;
        let stat = native(rfs::fstat(&fd))?;
        if stat.st_uid != self.target.paths.uid
            || stat.st_nlink != 1
            || stat.st_mode & 0o170000 != 0o100000
            || stat.st_mode & 0o022 != 0
            || (private && stat.st_mode & 0o777 != 0o600)
        {
            return Err(NativeError::Foreign);
        }
        if stat.st_size < 0 || stat.st_size as u64 > limit as u64 {
            return Err(NativeError::Oversize);
        }
        let mut bytes = Vec::new();
        native(
            File::from(fd)
                .take(limit as u64 + 1)
                .read_to_end(&mut bytes),
        )?;
        if bytes.len() > limit {
            return Err(NativeError::Oversize);
        }
        Ok(bytes)
    }
    pub fn atomic_write(&self, proof: &SupportProof, path: &Path, bytes: &[u8]) -> Result<()> {
        let (dir, name) = self.parent_for_mutation(proof, path)?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err(NativeError::Oversize);
        }
        if native(rfs::fstat(&dir))?.st_mode & 0o777 != 0o700 {
            return Err(NativeError::Foreign);
        }
        match rfs::statat(&dir, &name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(s)
                if s.st_uid == self.target.paths.uid
                    && s.st_nlink == 1
                    && s.st_mode & 0o170777 == 0o100600 => {}
            Ok(_) => return Err(NativeError::Foreign),
            Err(rustix::io::Errno::NOENT) => {}
            Err(_) => return Err(NativeError::Unavailable),
        }
        let temp = format!(".crosspane-{}", NONCE.fetch_add(1, Ordering::Relaxed));
        let mut file = File::from(native(rfs::openat(
            &dir,
            &temp,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        ))?);
        let result = (|| {
            self.files.apply(FileOperation::Write(&mut file, bytes))?;
            self.files.apply(FileOperation::FileSync(&file))?;
            self.files
                .apply(FileOperation::Rename(&dir, &temp, &name))?;
            self.files.apply(FileOperation::ParentSync(&dir))
        })();
        if result.is_err() {
            let _ = rfs::unlinkat(&dir, &temp, AtFlags::empty());
        }
        result
    }
    pub fn lock(&self, proof: &SupportProof, path: &Path) -> Result<File> {
        let (dir, name) = self.parent_for_mutation(proof, path)?;
        if native(rfs::fstat(&dir))?.st_mode & 0o777 != 0o700 {
            return Err(NativeError::Foreign);
        }
        let fd = native(rfs::openat(
            &dir,
            &name,
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        ))?;
        let stat = native(rfs::fstat(&fd))?;
        if stat.st_uid != self.target.paths.uid
            || stat.st_nlink != 1
            || stat.st_mode & 0o170777 != 0o100600
        {
            return Err(NativeError::Foreign);
        }
        rfs::flock(&fd, FlockOperation::NonBlockingLockExclusive).map_err(|_| NativeError::Busy)?;
        Ok(File::from(fd))
    }
    pub fn run(&self, spec: &CommandSpec, deadline: &Deadline) -> Result<CommandOutput> {
        self.execute(spec, deadline, None)
    }
    fn execute(
        &self,
        spec: &CommandSpec,
        deadline: &Deadline,
        proof: Option<&SupportProof>,
    ) -> Result<CommandOutput> {
        deadline.check()?;
        let mut admitted = spec.clone();
        admitted.admission = Some((self.target.clone(), proof.cloned()));
        let target = self.target.clone();
        let runner = self.runner.clone();
        let worker_deadline = deadline.clone();
        #[cfg(test)]
        let admission = self
            .command_admission
            .clone()
            .unwrap_or_else(|| Arc::new(validate_target));
        #[cfg(test)]
        let counter = self.command_workers;
        #[cfg(not(test))]
        let counter = &PROCESS_LAUNCHES;
        bounded_launch(counter, deadline, move || {
            worker_deadline.check()?;
            #[cfg(test)]
            admission(&target)?;
            #[cfg(not(test))]
            validate_target(&target)?;
            let expected = ChildEnvironment::selected(&target, BTreeMap::new())?;
            if admitted.environment.nonce != target.nonce
                || expected
                    .values
                    .iter()
                    .any(|(key, value)| admitted.environment.values.get(key) != Some(value))
            {
                return Err(NativeError::Foreign);
            }
            if let Some(bus) = &admitted.environment.bus {
                bus.revalidate(&target)?;
            }
            worker_deadline.check()?;
            if let Some((_, Some(proof))) = &admitted.admission {
                proof.check_target(&target)?;
            }
            let result = runner.run(&admitted, &worker_deadline)?;
            worker_deadline.check()?;
            if result.stdout.len() + result.stderr.len() > admitted.output_limit {
                return Err(NativeError::Oversize);
            }
            Ok(result)
        })
    }
    pub fn run_mutation(
        &self,
        proof: &SupportProof,
        spec: &CommandSpec,
        deadline: &Deadline,
    ) -> Result<CommandOutput> {
        proof.check(self)?;
        self.execute(spec, deadline, Some(proof))
    }
    pub fn process_identity(&self, pid: u32, deadline: &Deadline) -> Result<ProcessIdentity> {
        if pid == 0 {
            return Err(NativeError::Invalid);
        }
        let executable = self
            .metadata(&self.target.agent_path())?
            .ok_or(NativeError::Unavailable)?;
        if executable.st_mode & 0o170000 != 0o100000
            || executable.st_mode & 0o7000 != 0
            || executable.st_mode & 0o111 == 0
            || executable.st_nlink != 1
        {
            return Err(NativeError::Foreign);
        }
        let before = self.probe.snapshot(pid, deadline)?;
        if before.uid != self.target.paths.uid || before.executable != self.target.agent_path() {
            return Err(NativeError::Foreign);
        }
        let environment = ChildEnvironment::selected(&self.target, BTreeMap::new())?;
        let query = |field: &str| {
            self.run(
                &CommandSpec::new(
                    "/bin/ps".into(),
                    vec!["-o".into(), field.into(), "-p".into(), pid.to_string()],
                    environment.clone(),
                    256,
                )?,
                deadline,
            )
        };
        let start = query("lstart=")?;
        let name = query("comm=")?;
        if start.code != Some(0)
            || name.code != Some(0)
            || !start.stderr.is_empty()
            || !name.stderr.is_empty()
            || !matches!(
                name.stdout.as_slice(),
                b"crosspane-agent" | b"crosspane-agent\n"
            )
        {
            return Err(NativeError::Invalid);
        }
        let started_unix_ms = parse_ps_start(&start.stdout)?;
        if before != self.probe.snapshot(pid, deadline)? {
            return Err(NativeError::Foreign);
        }
        deadline.check()?;
        Ok(ProcessIdentity {
            pid,
            uid: before.uid,
            executable: before.executable,
            started_unix_ms,
            generation: before.generation,
        })
    }
    pub fn bootstrap(&self, deadline: &Deadline) -> Result<(BootstrapV1, ProcessIdentity)> {
        let path = self.target.runtime.join("bootstrap.json");
        let first =
            parse_bootstrap(&self.read(&path, 4096, true)?).map_err(|_| NativeError::Invalid)?;
        let identity = self.process_identity(first.pid, deadline)?;
        let second =
            parse_bootstrap(&self.read(&path, 4096, true)?).map_err(|_| NativeError::Invalid)?;
        if first.instance_id != second.instance_id
            || first.pid != second.pid
            || first.started_unix_ms != second.started_unix_ms
            || second.phase_seq < first.phase_seq
            || first.runtime_dir != self.target.runtime.to_string_lossy()
            || second.runtime_dir != first.runtime_dir
            || identity.started_unix_ms.abs_diff(first.started_unix_ms) > 2000
        {
            return Err(NativeError::Foreign);
        }
        if identity != self.process_identity(first.pid, deadline)? {
            return Err(NativeError::Foreign);
        }
        Ok((second, identity))
    }
    pub fn admit_instance(
        &self,
        instance: &InstanceStatus,
        bootstrap: &BootstrapV1,
        identity: &ProcessIdentity,
    ) -> Result<()> {
        if instance.id != bootstrap.instance_id
            || instance.pid != identity.pid
            || instance.uid != identity.uid
            || Path::new(&instance.exe) != identity.executable
            || Path::new(&instance.runtime_dir) != self.target.runtime
            || instance.started_unix_ms != bootstrap.started_unix_ms
            || identity.started_unix_ms.abs_diff(instance.started_unix_ms) > 2000
        {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
    pub(crate) fn socket_parent(&self) -> Result<(OwnedFd, String, (u64, u64))> {
        self.validate_target()?;
        let (dir, name) = self.parent(&self.target.socket_path())?;
        let parent = native(rfs::fstat(&dir))?;
        let socket = native(rfs::statat(&dir, &name, AtFlags::SYMLINK_NOFOLLOW))?;
        #[cfg(test)]
        let socket = {
            let mut s = socket;
            if let Some(uid) = *self
                .socket_uid
                .lock()
                .map_err(|_| NativeError::Unavailable)?
            {
                s.st_uid = uid;
            }
            s
        };
        socket_stat(&socket, self.target.paths.uid)?;
        if parent.st_mode & 0o777 != 0o700 {
            return Err(NativeError::Foreign);
        }
        Ok((dir, name, (socket.st_dev, socket.st_ino)))
    }
}

/// Strict one-result UTC ps format, including calendar and weekday consistency.
pub fn parse_ps_start(bytes: &[u8]) -> Result<u64> {
    if bytes.len() > 128 {
        return Err(NativeError::Oversize);
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| NativeError::Invalid)?
        .strip_suffix('\n')
        .unwrap_or(std::str::from_utf8(bytes).map_err(|_| NativeError::Invalid)?)
        .trim_matches(' ');
    if text.contains(['\n', '\r']) {
        return Err(NativeError::Invalid);
    }
    let fields: Vec<_> = text.split_whitespace().collect();
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
    let number = |s: &str| {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            Err(NativeError::Invalid)
        } else {
            s.parse::<u64>().map_err(|_| NativeError::Invalid)
        }
    };
    let (day, year) = (number(fields[2])?, number(fields[4])?);
    let clock: Vec<_> = fields[3].split(':').collect();
    if clock.len() != 3 || clock.iter().any(|v| v.len() != 2) || !(1970..=9999).contains(&year) {
        return Err(NativeError::Invalid);
    }
    let (hour, minute, second) = (number(clock[0])?, number(clock[1])?, number(clock[2])?);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum FileStep {
        Mkdir,
        Write,
        FileSync,
        Rename,
        ParentSync,
    }
    #[derive(Default)]
    struct RecordedFiles(std::sync::Mutex<(Vec<FileStep>, Option<FileStep>)>);
    impl FileIo for RecordedFiles {
        fn apply(&self, operation: FileOperation<'_>) -> Result<()> {
            let step = match &operation {
                FileOperation::Mkdir(..) => FileStep::Mkdir,
                FileOperation::Write(..) => FileStep::Write,
                FileOperation::FileSync(..) => FileStep::FileSync,
                FileOperation::Rename(..) => FileStep::Rename,
                FileOperation::ParentSync(..) => FileStep::ParentSync,
            };
            let mut fault = self.0.lock().unwrap();
            fault.0.push(step);
            if fault.1 == Some(step) {
                return Err(if matches!(step, FileStep::Rename | FileStep::ParentSync) {
                    NativeError::OutcomeUnknown
                } else {
                    NativeError::Unavailable
                });
            }
            NativeFiles.apply(operation)
        }
    }

    struct NoNativeCalls;
    impl CommandRunner for NoNativeCalls {
        fn run(&self, _: &CommandSpec, _: &Deadline) -> Result<CommandOutput> {
            Err(NativeError::Unavailable)
        }
    }
    impl ProcessProbe for NoNativeCalls {
        fn snapshot(&self, _: u32, _: &Deadline) -> Result<ProcessFacts> {
            Err(NativeError::Unavailable)
        }
    }
    fn fixture() -> (LinuxNativeIo, PathBuf) {
        let root = PathBuf::from(format!(
            "/tmp/cp47n-review-{}-{}",
            std::process::id(),
            NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        (
            LinuxNativeIo::scratch(&root, Arc::new(NoNativeCalls), Arc::new(NoNativeCalls))
                .unwrap(),
            root,
        )
    }
    fn observations(io: &LinuxNativeIo) -> SupportObservations {
        SupportObservations {
            uid: io.target.paths.uid,
            architecture: std::env::consts::ARCH.into(),
            arch_based: true,
            hyprland_version: [0, 56, 0],
            protocols_ready: true,
            runtime_libraries_ready: true,
            uwsm_managed: true,
            graphical_target_active: true,
            graphical_sessions: 1,
            session_id: "scratch".into(),
            session_type: "wayland".into(),
            seat: "seat0".into(),
            active: true,
        }
    }
    #[test]
    fn root_ancestry_symlink_at_each_level_refuses_every_mutation() {
        let (mut io, root) = fixture();
        let home = root.join("a/b/c");
        fs::create_dir_all(&home).unwrap();
        let paths = TargetPaths {
            uid: io.target.paths.uid,
            home: home.clone(),
            prefix: home.join(".local"),
            config_home: home.join(".config"),
            state_home: home.join(".local/state"),
            data_home: home.join(".local/share"),
            runtime_home: home.join("run"),
            runtime_override: None,
        };
        io.target = LinuxTarget::make(paths, true).unwrap();
        for path in [root.join("a"), root.join("a/b"), home.clone()] {
            let proof = io.scratch_support(observations(&io)).unwrap();
            let saved = root.join("saved");
            fs::rename(&path, &saved).unwrap();
            symlink(&saved, &path).unwrap();
            assert_eq!(io.validate_target(), Err(NativeError::Foreign));
            assert_eq!(
                io.create_private_dir(&proof, &home.join("new")),
                Err(NativeError::Foreign)
            );
            assert_eq!(
                io.atomic_write(&proof, &home.join("file"), b"inert"),
                Err(NativeError::Foreign)
            );
            assert!(matches!(
                io.lock(&proof, &home.join("lock")),
                Err(NativeError::Foreign)
            ));
            fs::remove_file(&path).unwrap();
            fs::rename(&saved, &path).unwrap();
        }
        assert!(!home.join("new").exists());
        assert!(!home.join("file").exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn atomic_failures_preserve_prior_data_or_report_inspectable_unknown() {
        let (mut io, root) = fixture();
        let files = Arc::new(RecordedFiles::default());
        io.files = files.clone();
        let proof = io.scratch_support(observations(&io)).unwrap();
        let path = root.join("record");
        for stage in [
            FileStep::Write,
            FileStep::FileSync,
            FileStep::Rename,
            FileStep::ParentSync,
        ] {
            *files.0.lock().unwrap() = (Vec::new(), None);
            io.atomic_write(&proof, &path, b"old").unwrap();
            *files.0.lock().unwrap() = (Vec::new(), Some(stage));
            let result = io.atomic_write(&proof, &path, b"new");
            assert_eq!(
                result,
                Err(
                    if matches!(stage, FileStep::Rename | FileStep::ParentSync) {
                        NativeError::OutcomeUnknown
                    } else {
                        NativeError::Unavailable
                    }
                )
            );
            assert_eq!(
                io.read(&path, 3, true).unwrap(),
                if stage == FileStep::ParentSync {
                    b"new"
                } else {
                    b"old"
                }
            );
            let order = [
                FileStep::Write,
                FileStep::FileSync,
                FileStep::Rename,
                FileStep::ParentSync,
            ];
            let count = order.iter().position(|s| *s == stage).unwrap() + 1;
            assert_eq!(files.0.lock().unwrap().0, order[..count]);
            assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        }
        *files.0.lock().unwrap() = (Vec::new(), None);
        io.create_private_dir(&proof, &root.join("durable/child"))
            .unwrap();
        assert_eq!(
            files.0.lock().unwrap().0,
            [
                FileStep::Mkdir,
                FileStep::ParentSync,
                FileStep::Mkdir,
                FileStep::ParentSync
            ]
        );
        *files.0.lock().unwrap() = (Vec::new(), Some(FileStep::ParentSync));
        assert_eq!(
            io.create_private_dir(&proof, &root.join("uncertain/child")),
            Err(NativeError::OutcomeUnknown)
        );
        assert!(root.join("uncertain").is_dir());
        assert!(!root.join("uncertain/child").exists());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn exact_executable_mapping_rejects_aliases_shells_and_privilege_bits() {
        let (io, root) = fixture();
        let env = ChildEnvironment::selected(io.target(), BTreeMap::new()).unwrap();
        let argv = vec!["-o".into(), "lstart=".into(), "-p".into(), "123".into()];
        assert_eq!(
            approved_executable(Path::new("/bin/ps"), &argv),
            Ok("/usr/bin/ps")
        );
        for executable in [
            "/usr/bin/dash",
            "/usr/bin/busybox",
            "/usr/bin/ps",
            "/usr/bin/sudo",
            "/usr/bin/pkexec",
        ] {
            assert!(CommandSpec::new(executable.into(), argv.clone(), env.clone(), 256).is_err());
        }
        let alias = root.join("harmless");
        for target in ["/usr/bin/sudo", "/usr/bin/pkexec"] {
            symlink(target, &alias).unwrap();
            assert!(CommandSpec::new(alias.clone(), argv.clone(), env.clone(), 256).is_err());
            fs::remove_file(&alias).unwrap();
        }
        fs::write(&alias, b"inert").unwrap();
        let mut stat = rfs::statat(rfs::CWD, &alias, AtFlags::SYMLINK_NOFOLLOW).unwrap();
        stat.st_uid = 0;
        stat.st_mode = 0o100755;
        assert_eq!(executable_stat(&stat), Ok(()));
        for mode in [0o104755, 0o102755, 0o100777, 0o120755] {
            stat.st_mode = mode;
            assert_eq!(executable_stat(&stat), Err(NativeError::Foreign));
        }
        stat.st_mode = 0o100755;
        stat.st_uid = io.target.paths.uid;
        assert_eq!(executable_stat(&stat), Err(NativeError::Foreign));
        stat.st_mode = 0o140600;
        assert_eq!(socket_stat(&stat, io.target.paths.uid), Ok(()));
        stat.st_uid += 1;
        assert_eq!(
            socket_stat(&stat, io.target.paths.uid),
            Err(NativeError::Foreign)
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn dbus_decoding_and_complete_environment_target_binding_fail_closed() {
        let (mut io, root) = fixture();
        let runtime = io.target.paths.runtime_home.clone();
        for value in [
            format!("unix:path={}/%2e%2e/foreign", runtime.display()),
            format!("unix:path={}/bus;unix:path=/tmp/foreign", runtime.display()),
            format!("unix:path={}/bus,guid=other", runtime.display()),
            format!("unix:path={}/%00", runtime.display()),
            format!("unix:path={}/%zz", runtime.display()),
        ] {
            assert!(dbus_address(&value, &runtime).is_err());
        }
        assert_eq!(
            dbus_address(
                &format!("unix:path={}/b%75s%20name", runtime.display()),
                &runtime
            )
            .unwrap(),
            (
                format!("unix:path={}/bus%20name", runtime.display()),
                runtime.join("bus name")
            )
        );
        let old = ChildEnvironment::selected(io.target(), BTreeMap::new()).unwrap();
        let mut paths = io.target.paths.clone();
        paths.config_home = root.join("other-config");
        io.target = LinuxTarget::make(paths, true).unwrap();
        let proof = io.scratch_support(observations(&io)).unwrap();
        let mut spec = CommandSpec::new(
            "/bin/ps".into(),
            vec!["-o".into(), "comm=".into(), "-p".into(), "1".into()],
            old,
            256,
        )
        .unwrap();
        let deadline = Deadline::new(1000, Cancellation::default()).unwrap();
        assert!(matches!(
            io.run_mutation(&proof, &spec, &deadline),
            Err(NativeError::Foreign)
        ));
        spec.environment.nonce = io.target.nonce;
        assert!(matches!(
            io.run(&spec, &deadline),
            Err(NativeError::Foreign)
        ));
        let baseline = ChildEnvironment::selected(io.target(), BTreeMap::new()).unwrap();
        let used: usize = baseline.values.iter().map(|(k, v)| k.len() + v.len()).sum();
        io.target.paths.data_home = PathBuf::from(format!(
            "{}{}",
            io.target.paths.data_home.display(),
            "x".repeat(32768 - used)
        ));
        assert!(ChildEnvironment::selected(io.target(), BTreeMap::new()).is_ok());
        io.target.paths.data_home.as_mut_os_string().push("x");
        assert!(matches!(
            ChildEnvironment::selected(io.target(), BTreeMap::new()),
            Err(NativeError::Oversize)
        ));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn bus_endpoint_admission_rejects_escaping_symlinks_and_replaced_identity_at_use() {
        use std::os::unix::net::UnixListener;
        struct Count(AtomicUsize);
        impl CommandRunner for Count {
            fn run(&self, _: &CommandSpec, _: &Deadline) -> Result<CommandOutput> {
                self.0.fetch_add(1, Ordering::Relaxed);
                Ok(CommandOutput {
                    code: Some(0),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }
        let (mut io, root) = fixture();
        let runner = Arc::new(Count(AtomicUsize::new(0)));
        io.runner = runner.clone();
        let proof = io.scratch_support(observations(&io)).unwrap();
        let runtime = io.target.paths.runtime_home.clone();
        io.create_private_dir(&proof, &runtime).unwrap();
        let outside = root.join("outside");
        io.create_private_dir(&proof, &outside).unwrap();
        let foreign = UnixListener::bind(outside.join("bus")).unwrap();
        foreign.set_nonblocking(true).unwrap();
        symlink(&outside, runtime.join("alias")).unwrap();
        let selected = |path: &Path| {
            BTreeMap::from([(
                "DBUS_SESSION_BUS_ADDRESS".into(),
                format!("unix:path={}", path.display()),
            )])
        };
        assert!(matches!(
            ChildEnvironment::selected(io.target(), selected(&runtime.join("alias/bus"))),
            Err(NativeError::Foreign)
        ));
        symlink(outside.join("bus"), runtime.join("bus")).unwrap();
        assert!(matches!(
            ChildEnvironment::selected(io.target(), selected(&runtime.join("bus"))),
            Err(NativeError::Foreign)
        ));
        fs::remove_file(runtime.join("bus")).unwrap();
        let first = UnixListener::bind(runtime.join("bus")).unwrap();
        let environment =
            ChildEnvironment::selected(io.target(), selected(&runtime.join("bus"))).unwrap();
        let command = CommandSpec::new(
            "/bin/ps".into(),
            vec!["-o".into(), "comm=".into(), "-p".into(), "1".into()],
            environment,
            256,
        )
        .unwrap();
        let deadline = Deadline::new(1000, Cancellation::default()).unwrap();
        io.run(&command, &deadline).unwrap();
        assert_eq!(runner.0.load(Ordering::Relaxed), 1);
        fs::rename(runtime.join("bus"), runtime.join("old-bus")).unwrap();
        let second = UnixListener::bind(runtime.join("bus")).unwrap();
        assert!(matches!(
            io.run(&command, &deadline),
            Err(NativeError::Foreign)
        ));
        assert!(matches!(
            io.run_mutation(&proof, &command, &deadline),
            Err(NativeError::Foreign)
        ));
        assert_eq!(runner.0.load(Ordering::Relaxed), 1);
        let current =
            ChildEnvironment::selected(io.target(), selected(&runtime.join("bus"))).unwrap();
        fs::rename(&runtime, root.join("saved-runtime")).unwrap();
        io.create_private_dir(&proof, &runtime).unwrap();
        let third = UnixListener::bind(runtime.join("bus")).unwrap();
        assert_eq!(
            current.bus.unwrap().revalidate(io.target()),
            Err(NativeError::Foreign)
        );
        fs::remove_dir_all(&runtime).unwrap();
        symlink(root.join("saved-runtime"), &runtime).unwrap();
        assert!(matches!(
            ChildEnvironment::selected(io.target(), selected(&runtime.join("bus"))),
            Err(NativeError::Foreign)
        ));
        assert!(matches!(foreign.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
        drop((first, second, third, foreign));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn noncooperative_launch_returns_at_deadline_and_retains_admission_until_completion() {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let release = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(AtomicUsize::new(0));
        for index in 0..4 {
            let cancellation = Cancellation::default();
            let deadline =
                Deadline::new(if index == 3 { 1000 } else { 20 }, cancellation.clone()).unwrap();
            let release = release.clone();
            let entered = entered.clone();
            let start = Instant::now();
            assert_eq!(
                bounded_launch(&COUNT, &deadline, move || {
                    entered.fetch_add(1, Ordering::Release);
                    if index == 3 {
                        cancellation.cancel();
                    }
                    while !release.load(Ordering::Acquire) {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Ok(99)
                }),
                Err(if index == 3 {
                    NativeError::Cancelled
                } else {
                    NativeError::Timeout
                })
            );
            assert!(start.elapsed() < Duration::from_millis(500));
        }
        assert_eq!(entered.load(Ordering::Acquire), 4);
        assert_eq!(COUNT.load(Ordering::Acquire), 4);
        let deadline = Deadline::new(1000, Cancellation::default()).unwrap();
        assert_eq!(
            bounded_launch(&COUNT, &deadline, || Ok(1)),
            Err(NativeError::Busy)
        );
        release.store(true, Ordering::Release);
        let until = Instant::now() + Duration::from_secs(1);
        while COUNT.load(Ordering::Acquire) != 0 {
            assert!(Instant::now() < until);
            thread::yield_now();
        }
        assert_eq!(bounded_launch(&COUNT, &deadline, || Ok(7)), Ok(7));
    }
    #[test]
    fn public_command_entries_bound_noncooperative_admission_and_retain_occupied_slots() {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        struct CountRunner(AtomicUsize);
        impl CommandRunner for CountRunner {
            fn run(&self, _: &CommandSpec, _: &Deadline) -> Result<CommandOutput> {
                self.0.fetch_add(1, Ordering::Release);
                Ok(CommandOutput {
                    code: Some(0),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }
        let (mut io, root) = fixture();
        let runner = Arc::new(CountRunner(AtomicUsize::new(0)));
        io.runner = runner.clone();
        io.command_workers = &COUNT;
        let proof = io.scratch_support(observations(&io)).unwrap();
        let command = CommandSpec::new(
            "/bin/ps".into(),
            vec!["-o".into(), "comm=".into(), "-p".into(), "1".into()],
            ChildEnvironment::selected(io.target(), BTreeMap::new()).unwrap(),
            256,
        )
        .unwrap();
        let release = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(AtomicUsize::new(0));
        let safety_release = release.clone();
        let releaser = thread::spawn(move || {
            // A broken public boundary must fail the time assertion instead of hanging this test.
            thread::sleep(Duration::from_secs(1));
            safety_release.store(true, Ordering::Release);
        });
        for (mutation, cancel) in [(false, false), (true, false), (false, true), (true, true)] {
            let cancellation = Cancellation::default();
            let deadline =
                Deadline::new(if cancel { 1000 } else { 20 }, cancellation.clone()).unwrap();
            let release = release.clone();
            let entered = entered.clone();
            io.command_admission = Some(Arc::new(move |target| {
                entered.fetch_add(1, Ordering::Release);
                if cancel {
                    cancellation.cancel();
                }
                while !release.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
                validate_target(target)
            }));
            let start = Instant::now();
            let result = if mutation {
                io.run_mutation(&proof, &command, &deadline)
            } else {
                io.run(&command, &deadline)
            };
            assert!(
                matches!(result, Err(e) if e == if cancel { NativeError::Cancelled } else { NativeError::Timeout })
            );
            assert!(start.elapsed() < Duration::from_millis(500));
        }
        assert_eq!(entered.load(Ordering::Acquire), 4);
        assert_eq!(COUNT.load(Ordering::Acquire), 4);
        assert_eq!(runner.0.load(Ordering::Acquire), 0);
        let deadline = Deadline::new(1000, Cancellation::default()).unwrap();
        assert!(matches!(
            io.run(&command, &deadline),
            Err(NativeError::Busy)
        ));
        assert!(matches!(
            io.run_mutation(&proof, &command, &deadline),
            Err(NativeError::Busy)
        ));
        release.store(true, Ordering::Release);
        let until = Instant::now() + Duration::from_secs(1);
        while COUNT.load(Ordering::Acquire) != 0 {
            assert!(Instant::now() < until);
            thread::yield_now();
        }
        assert_eq!(runner.0.load(Ordering::Acquire), 0);
        io.command_admission = None;
        io.run(&command, &deadline).unwrap();
        io.run_mutation(&proof, &command, &deadline).unwrap();
        assert_eq!(runner.0.load(Ordering::Acquire), 2);
        releaser.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn proc_generation_uses_field_22_and_bounds_injected_bytes() {
        let fields = (3..=52)
            .map(|n| {
                if n == 3 {
                    "S".into()
                } else {
                    (n * 100).to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        let bytes = format!("4242 (cross ) pane) {fields}\n");
        assert_eq!(proc_generation(bytes.as_bytes()), Ok(2200));
        assert_eq!(
            proc_generation(b"4242 (agent) S 1"),
            Err(NativeError::Invalid)
        );
        assert_eq!(proc_generation(&vec![0; 4097]), Err(NativeError::Oversize));
    }
    #[test]
    fn stalled_child_cleanup_is_off_deadline_path_and_retains_four_slots() {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        #[derive(Debug)]
        struct Stalled {
            terminated: Arc<AtomicUsize>,
            release: Arc<AtomicBool>,
        }
        impl Cleanup for Stalled {
            fn terminate(&mut self) {
                self.terminated.fetch_add(1, Ordering::Release);
            }
            fn reaped(&mut self) -> bool {
                self.release.load(Ordering::Acquire)
            }
        }
        let terminated = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(AtomicBool::new(false));
        let start = Instant::now();
        for _ in 0..4 {
            cleanup_admission::<Stalled>(&COUNT)
                .unwrap()
                .try_send(Stalled {
                    terminated: terminated.clone(),
                    release: release.clone(),
                })
                .unwrap();
        }
        assert!(start.elapsed() < Duration::from_millis(100));
        let until = Instant::now() + Duration::from_secs(1);
        while terminated.load(Ordering::Acquire) != 4 {
            assert!(Instant::now() < until);
            thread::yield_now();
        }
        assert!(matches!(
            cleanup_admission::<Stalled>(&COUNT),
            Err(NativeError::Busy)
        ));
        release.store(true, Ordering::Release);
        while COUNT.load(Ordering::Acquire) != 0 {
            assert!(Instant::now() < until);
            thread::yield_now();
        }
        drop(cleanup_admission::<Stalled>(&COUNT).unwrap());
    }

    #[test]
    fn mutation_parent_is_proof_checked_and_stays_anchored_after_path_replacement() {
        let root = PathBuf::from(format!(
            "/tmp/cp47n-fd-{}-{}",
            std::process::id(),
            NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        let io = LinuxNativeIo::scratch(&root, Arc::new(NoNativeCalls), Arc::new(NoNativeCalls))
            .unwrap();
        let facts = SupportObservations {
            uid: io.target.paths.uid,
            architecture: std::env::consts::ARCH.into(),
            arch_based: true,
            hyprland_version: [0, 56, 0],
            protocols_ready: true,
            runtime_libraries_ready: true,
            uwsm_managed: true,
            graphical_target_active: true,
            graphical_sessions: 1,
            session_id: "scratch".into(),
            session_type: "wayland".into(),
            seat: "seat0".into(),
            active: true,
        };
        let proof = io.scratch_support(facts.clone()).unwrap();
        let parent = root.join("mutation");
        io.create_private_dir(&proof, &parent).unwrap();
        let destination = parent.join("payload");
        let (fd, name) = io.parent_for_mutation(&proof, &destination).unwrap();
        let pinned = root.join("pinned");
        fs::rename(&parent, &pinned).unwrap();
        io.create_private_dir(&proof, &parent).unwrap();
        let mut file = File::from(
            rfs::openat(
                &fd,
                &name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
                Mode::RUSR | Mode::WUSR,
            )
            .unwrap(),
        );
        file.write_all(b"inert payload").unwrap();
        assert_eq!(fs::read(pinned.join("payload")).unwrap(), b"inert payload");
        assert!(!destination.exists());
        let link = root.join("link");
        symlink(&pinned, &link).unwrap();
        assert!(matches!(
            io.parent_for_mutation(&proof, &link.join("payload")),
            Err(NativeError::Foreign)
        ));
        fs::create_dir_all(io.target.paths.config_home.parent().unwrap()).unwrap();
        symlink(&pinned, &io.target.paths.config_home).unwrap();
        assert!(matches!(
            io.parent_for_mutation(&proof, &destination),
            Err(NativeError::Foreign)
        ));
        fs::remove_file(&io.target.paths.config_home).unwrap();
        let mut changed = facts;
        changed.active = false;
        assert_eq!(
            proof.revalidate(&io, &changed),
            Err(NativeError::Unsupported)
        );
        assert!(matches!(
            io.parent_for_mutation(&proof, &destination),
            Err(NativeError::Unsupported)
        ));
        fs::remove_dir_all(root).unwrap();
    }
}
