//! Bounded Mac observations and confined ordinary-user primitives, without readiness policy.
//!
//! Lead amendment (WP-4.12a): stable Rust/rustix cannot read Mac Unix peer credentials.
//! Instead the runtime is fd-admitted, selected-UID-owned, exactly 0700, with no symlink in
//! its ancestry; socket owner/type/dev/inode are pinned and rechecked after connect and before
//! any mutation byte. This proves endpoint plus instance, not kernel peer credentials. Transport
//! must additionally correlate bootstrap/status/PID/start/signed executable around exchange.
//! A same-UID impostor is outside this amended threat model. Only the lead's finite /var and
//! /tmp aliases are expanded before the walk; arbitrary symlinks are never canonicalized.
//! macOS ACLs are not evaluated within the approved no-binding boundary; administrators are trusted.
use crate::agent_contract::{BootstrapV1, InstanceStatus, ObservationSource, parse_bootstrap};
use rustix::fs::{self as rfs, AtFlags, FlockOperation, Mode, OFlags};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Write},
    os::fd::{AsFd, OwnedFd},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

pub const MAX_FILE_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_COMMAND_BYTES: usize = 64 * 1024;
pub const MAX_NATIVE_TIMEOUT_MS: u64 = 120_000;
pub const MAX_NATIVE_CALLS: usize = 32;
pub const SUPPORT_LIFETIME_MS: u64 = 5000;
pub const AGENT_LABEL: &str = "io.frostdev.crosspane.agent";
static NONCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NativeError {
    #[error("invalid bounded native value")]
    Invalid,
    #[error("native observation unavailable")]
    Unavailable,
    #[error("foreign or unsafe resource")]
    Foreign,
    #[error("unsupported or unknown target")]
    Unsupported,
    #[error("operation refused")]
    Refused,
    #[error("operation timed out")]
    Timeout,
    #[error("operation cancelled")]
    Cancelled,
    #[error("bounded queue or lock busy")]
    Busy,
    #[error("bounded output exceeded")]
    Oversize,
    #[error("mutation outcome unknown; detect before retry")]
    OutcomeUnknown,
    #[error("monotonic identifier exhausted")]
    IdExhausted,
}
pub type NativeResult<T> = Result<T, NativeError>;
fn native<T>(r: Result<T, impl std::fmt::Debug>) -> NativeResult<T> {
    r.map_err(|_| NativeError::Unavailable)
}
fn next_nonce() -> NativeResult<u64> {
    NONCE
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_add(1))
        .map_err(|_| NativeError::IdExhausted)
}
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}
#[derive(Debug)]
pub struct MonotonicClock(Instant);
impl Default for MonotonicClock {
    fn default() -> Self {
        Self(Instant::now())
    }
}
impl Clock for MonotonicClock {
    fn now_ms(&self) -> u64 {
        self.0.elapsed().as_millis().min(u64::MAX as u128) as u64
    }
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
#[derive(Clone)]
pub struct Deadline {
    clock: Arc<dyn Clock>,
    end: u64,
    cancellation: Cancellation,
    wall: Instant,
    timeout_ms: u64,
}
impl Deadline {
    pub fn new(
        timeout_ms: u64,
        clock: Arc<dyn Clock>,
        cancellation: Cancellation,
    ) -> NativeResult<Self> {
        if !(1..=MAX_NATIVE_TIMEOUT_MS).contains(&timeout_ms) {
            return Err(NativeError::Invalid);
        }
        let end = clock
            .now_ms()
            .checked_add(timeout_ms)
            .ok_or(NativeError::Invalid)?;
        Ok(Self {
            clock,
            end,
            cancellation,
            wall: Instant::now(),
            timeout_ms,
        })
    }
    pub fn check(&self) -> NativeResult<()> {
        if self.cancellation.is_cancelled() {
            return Err(NativeError::Cancelled);
        }
        if self.clock.now_ms() >= self.end
            || self.wall.elapsed() >= Duration::from_millis(self.timeout_ms)
        {
            return Err(NativeError::Timeout);
        }
        Ok(())
    }
    pub fn remaining_ms(&self) -> NativeResult<u64> {
        self.check()?;
        Ok(self.end.saturating_sub(self.clock.now_ms()).min(
            self.timeout_ms
                .saturating_sub(self.wall.elapsed().as_millis() as u64),
        ))
    }
}
fn clean(path: &Path) -> bool {
    path.is_absolute()
        && path.as_os_str().len() <= 4096
        && path != Path::new("/")
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
fn bounded(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}
/// The entire alias allowlist. Syntax is checked before substitution, without canonicalize().
pub fn admitted_spelling(path: &Path) -> NativeResult<PathBuf> {
    if !clean(path) {
        return Err(NativeError::Invalid);
    }
    for alias in ["/var", "/tmp"] {
        if let Ok(suffix) = path.strip_prefix(alias) {
            return Ok(Path::new("/private")
                .join(alias.trim_start_matches('/'))
                .join(suffix));
        }
    }
    Ok(path.to_owned())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetPaths {
    pub uid: u32,
    pub home: PathBuf,
    pub gui_tmpdir: PathBuf,
    pub runtime_override: Option<PathBuf>,
    /// Explicit read-only distribution root containing Crosspane.app; never an owner default.
    pub payload_root: PathBuf,
}
type EraseLatches = Vec<(u64, ProcessIdentity, Arc<AtomicBool>)>;
#[derive(Clone)]
pub struct MacTarget {
    paths: TargetPaths,
    runtime: PathBuf,
    nonce: u64,
    scratch: bool,
    mutation: Arc<Mutex<()>>,
    erase_latches: Arc<Mutex<EraseLatches>>,
    #[cfg(test)]
    pub(crate) test_hook: Option<TestHook>,
    #[cfg(test)]
    pub(crate) test_path: Option<TestPath>,
}
impl std::fmt::Debug for MacTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MacTarget")
    }
}
#[cfg(test)]
pub(crate) type TestHook = Arc<
    dyn Fn(&str, &Path, Option<FileIdentity>) -> NativeResult<Option<FileIdentity>> + Send + Sync,
>;
#[cfg(test)]
pub(crate) type TestPath = Arc<dyn Fn(&Path) -> PathBuf + Send + Sync>;
impl MacTarget {
    pub fn selected(paths: TargetPaths) -> NativeResult<Self> {
        Self::make(paths, false)
    }
    /// Caller creates a private explicit scratch home and GUI TMPDIR; no environment is consulted.
    pub fn scratch(paths: TargetPaths) -> NativeResult<Self> {
        Self::make(paths, true)
    }
    fn make(mut paths: TargetPaths, scratch: bool) -> NativeResult<Self> {
        if paths.uid == 0 || paths.uid != rustix::process::geteuid().as_raw() {
            return Err(NativeError::Foreign);
        }
        paths.home = admitted_spelling(&paths.home)?;
        paths.gui_tmpdir = admitted_spelling(&paths.gui_tmpdir)?;
        paths.payload_root = admitted_spelling(&paths.payload_root)?;
        paths.runtime_override = paths
            .runtime_override
            .as_deref()
            .map(admitted_spelling)
            .transpose()?;
        let runtime = paths
            .runtime_override
            .clone()
            .unwrap_or_else(|| paths.gui_tmpdir.join("crosspane"));
        if !clean(&paths.home)
            || !clean(&paths.gui_tmpdir)
            || !clean(&runtime)
            || (!scratch && !paths.home.starts_with("/Users"))
            || runtime == paths.home
            || runtime == paths.gui_tmpdir
            || !(runtime.starts_with(&paths.gui_tmpdir) || runtime.starts_with(&paths.home))
            || !(paths.payload_root.starts_with(&paths.home)
                || paths.payload_root.starts_with(&paths.gui_tmpdir))
            || runtime.join("agent.sock").as_os_str().len() > 103
        {
            return Err(NativeError::Foreign);
        }
        Ok(Self {
            paths,
            runtime,
            nonce: next_nonce()?,
            scratch,
            mutation: Arc::new(Mutex::new(())),
            erase_latches: Arc::new(Mutex::new(Vec::new())),
            #[cfg(test)]
            test_hook: None,
            #[cfg(test)]
            test_path: None,
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
    pub fn app_path(&self) -> PathBuf {
        self.paths.home.join("Applications/Crosspane.app")
    }
    pub fn agent_path(&self) -> PathBuf {
        self.app_path().join("Contents/MacOS/Crosspane")
    }
    pub fn state_dir(&self) -> PathBuf {
        self.paths
            .home
            .join("Library/Application Support/Crosspane")
    }
    pub fn installer_dir(&self) -> PathBuf {
        self.state_dir().join("Installer")
    }
    /// WP-4.32: where setup saves whatever it moved out of its install paths.
    pub fn backups_dir(&self) -> PathBuf {
        self.state_dir().join("Backups")
    }
    pub fn source(&self) -> ObservationSource {
        if self.scratch {
            ObservationSource::Demo
        } else {
            ObservationSource::Live
        }
    }
    fn roots(&self) -> Vec<PathBuf> {
        [
            "Applications/Crosspane.app",
            "Applications/.Crosspane.app.crosspane-stage",
            "Applications/.Crosspane.app.crosspane-previous",
            ".local/bin/crosspanectl",
            ".local/bin/.crosspanectl.crosspane-stage",
            ".local/bin/.crosspanectl.crosspane-previous",
            "Library/LaunchAgents/io.frostdev.crosspane.agent.plist",
            "Library/LaunchAgents/.io.frostdev.crosspane.agent.plist.crosspane-stage",
            "Library/Logs/Crosspane",
            "Library/Application Support/Crosspane/Installer",
            "Library/Application Support/Crosspane/Backups",
        ]
        .map(|p| self.paths.home.join(p))
        .to_vec()
    }
    fn readable(&self, path: &Path) -> bool {
        clean(path)
            && (path.starts_with(&self.paths.home)
                || path.starts_with(&self.runtime)
                || path.starts_with(&self.paths.payload_root))
    }
    fn writable(&self, path: &Path) -> bool {
        clean(path)
            && self.roots().iter().any(|root| {
                path.starts_with(root)
                    || (path.parent() == root.parent()
                        && root.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                            path.file_name()
                                .and_then(|n| n.to_str())
                                .and_then(|s| s.strip_prefix(&format!(".{n}.crosspane-temp-")))
                                .is_some_and(|s| {
                                    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
                                })
                        }))
            })
    }
    fn creatable(&self, path: &Path) -> bool {
        clean(path)
            && path != self.paths.home
            && self
                .roots()
                .iter()
                .any(|root| path.starts_with(root) || root.starts_with(path))
    }
    fn observe(
        &self,
        stage: &str,
        path: &Path,
        value: Option<FileIdentity>,
    ) -> NativeResult<Option<FileIdentity>> {
        #[cfg(test)]
        if let Some(hook) = &self.test_hook {
            return hook(stage, path, value);
        }
        let _ = (stage, path);
        Ok(value)
    }
}

/// Both public observations must identify the same selected, active interactive GUI session.
/// A launchd domain, process name, SSH environment or claimed UID alone is insufficient.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuiObservation {
    pub console_uid: Option<u32>,
    pub interactive_uid: Option<u32>,
    pub console_session: String,
    pub interactive_session: String,
    pub active: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupportObservation {
    pub macos_major: u16,
    pub apple_silicon: bool,
    pub gui: GuiObservation,
    pub gui_tmpdir: PathBuf,
}
pub trait SupportProbe: Send + Sync {
    fn observe(&self, deadline: &Deadline) -> NativeResult<SupportObservation>;
}
#[derive(Clone)]
pub struct SupportProof {
    nonce: u64,
    issued: u64,
    wall: Instant,
    gui: GuiObservation,
    gui_tmpdir: PathBuf,
    compatibility: CompatibilityReport,
    signing: SignatureProof,
    valid: Arc<AtomicBool>,
}
impl SupportProof {
    pub fn revoke(&self) {
        self.valid.store(false, Ordering::Release);
    }
    pub fn check(&self, io: &MacNativeIo, deadline: &Deadline) -> NativeResult<()> {
        deadline.check()?;
        if self.nonce != io.target.nonce
            || !self.valid.load(Ordering::Acquire)
            || io
                .clock
                .now_ms()
                .checked_sub(self.issued)
                .is_none_or(|age| age > SUPPORT_LIFETIME_MS)
            || self.wall.elapsed() > Duration::from_millis(SUPPORT_LIFETIME_MS)
        {
            return Err(NativeError::Unsupported);
        }
        let current = io.support_observation(deadline)?;
        if current.gui != self.gui || current.gui_tmpdir != self.gui_tmpdir {
            self.revoke();
            return Err(NativeError::Unsupported);
        }
        io.validate_target()?;
        self.signing.revalidate(io)?;
        Ok(())
    }
    pub fn compatibility(&self) -> &CompatibilityReport {
        &self.compatibility
    }
    pub fn check_agent_compatibility(&self) -> NativeResult<()> {
        if self.compatibility.macos_major < 26 || !self.compatibility.apple_silicon {
            Err(NativeError::Unsupported)
        } else {
            Ok(())
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompatibilityReport {
    pub macos_major: u16,
    pub apple_silicon: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactRole {
    Agent,
    Settings,
    Ctl,
    Installer,
    /// Code embedded in a bundle (a dylib or framework), such as `Contents/Frameworks`. Admitted
    /// only by its own approved identifier and designated requirement, with no entitlements; it
    /// is never launched or used as a controlled child.
    EmbeddedCode,
}
#[derive(Clone, Debug, PartialEq, Eq)]
/// Approved role-specific table input, never derived from the payload's own manifest/signature.
pub struct SigningRequirement {
    pub role: ArtifactRole,
    pub identifier: String,
    pub designated_requirement: String,
    pub entitlements: BTreeMap<String, bool>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignatureObservation {
    pub strict_verified: bool,
    pub team_identifier: String,
    pub identifier: String,
    pub designated_requirement: String,
    pub entitlements: BTreeMap<String, bool>,
    pub apple_development: bool,
    pub hardened_runtime: bool,
    pub ad_hoc: bool,
}
impl SignatureObservation {
    pub fn admit(&self, expected: &SigningRequirement) -> NativeResult<()> {
        if self.team_identifier.len() != 10
            || !self
                .team_identifier
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
            || !self.strict_verified
            || self.ad_hoc
            || !self.apple_development
            || !self.hardened_runtime
            || !bounded(&expected.identifier, 256)
            || self.identifier != expected.identifier
            || !bounded(&expected.designated_requirement, 4096)
            || self.designated_requirement != expected.designated_requirement
            || expected.entitlements.len() > 32
            || expected.entitlements.keys().any(|k| !bounded(k, 256))
            || self.entitlements != expected.entitlements
            || (expected.role == ArtifactRole::Agent && expected.identifier != AGENT_LABEL)
            || (expected.role == ArtifactRole::EmbeddedCode && !expected.entitlements.is_empty())
        {
            return Err(NativeError::Unsupported);
        }
        Ok(())
    }
}
/// Injected structured observations expose verification, designated requirement, identifier,
/// entitlements and Team separately. No production success/default or Keychain access is provided.
pub trait SignatureProbe: Send + Sync {
    fn observe(
        &self,
        path: &Path,
        approved: &SigningRequirement,
        deadline: &Deadline,
    ) -> NativeResult<SignatureObservation>;
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
    pub uid: u32,
    pub mode: u32,
    pub length: u64,
    pub modified_ns: i128,
    pub changed_ns: i128,
    pub links: u64,
}
impl FileIdentity {
    fn from_stat(s: &rfs::Stat) -> Self {
        Self {
            device: s.st_dev as u64,
            inode: s.st_ino,
            uid: s.st_uid,
            mode: s.st_mode as u32,
            length: s.st_size.max(0) as u64,
            modified_ns: s.st_mtime as i128 * 1_000_000_000 + s.st_mtime_nsec as i128,
            changed_ns: s.st_ctime as i128 * 1_000_000_000 + s.st_ctime_nsec as i128,
            links: s.st_nlink as u64,
        }
    }
    pub fn regular(&self, uid: u32, private: bool) -> NativeResult<()> {
        if self.uid != uid
            || self.mode & 0o170000 != 0o100000
            || self.links != 1
            || self.mode & 0o7022 != 0
            || (private && self.mode & 0o777 != 0o600)
        {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
    pub fn socket(&self, uid: u32) -> NativeResult<()> {
        if self.uid != uid || self.mode & 0o170000 != 0o140000 || self.mode & 0o022 != 0 {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
}
#[derive(Clone, Debug)]
pub struct SignatureProof {
    nonce: u64,
    path: PathBuf,
    identity: FileIdentity,
    requirement: SigningRequirement,
    observation: SignatureObservation,
}
impl SignatureProof {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn requirement(&self) -> &SigningRequirement {
        &self.requirement
    }
    pub fn observation(&self) -> &SignatureObservation {
        &self.observation
    }
    pub fn revalidate(&self, io: &MacNativeIo) -> NativeResult<()> {
        if self.nonce != io.target.nonce || io.metadata(&self.path)? != Some(self.identity.clone())
        {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
}

macro_rules! opaque_debug {
    ($($ty:ty),+ $(,)?) => { $(impl std::fmt::Debug for $ty { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(stringify!($ty)) } })+ };
}
opaque_debug!(Deadline, SupportProof, MacNativeIo);
#[path = "native_io/commands.rs"]
mod commands;
#[path = "native_io/files.rs"]
mod files;
#[path = "native_io/process.rs"]
mod process;
#[path = "native_io/signature.rs"]
mod signature;
pub use commands::*;
#[cfg(test)]
#[allow(unused_imports)]
pub(crate) use commands::{ChildSpawner, OwnedChild, run_owned_child};
pub(crate) use files::DeadRuntime;
pub use files::{DirectoryAnchor, InstallerLock, SocketEndpoint};
#[cfg(test)]
#[allow(unused_imports)]
pub(crate) use files::{FilesystemOperation, FilesystemOps, SystemFilesystem};
pub use process::{AdmittedInstance, ProcessIdentity, parse_ps_start};

pub struct MacNativeIo {
    target: MacTarget,
    runner: Arc<dyn CommandRunner>,
    support: Arc<dyn SupportProbe>,
    signatures: Arc<dyn SignatureProbe>,
    clock: Arc<dyn Clock>,
    home: DirectoryAnchor,
    temporary: DirectoryAnchor,
    runtime: Option<DirectoryAnchor>,
    payload: DirectoryAnchor,
    mutation: Arc<Mutex<()>>,
    filesystem: Arc<dyn files::FilesystemOps>,
}

static PROBES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static COMMANDS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
struct ProbeSlot(&'static std::sync::atomic::AtomicUsize);
impl Drop for ProbeSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}
fn bounded_result<T: Send + 'static>(
    slots: &'static std::sync::atomic::AtomicUsize,
    deadline: &Deadline,
    work: impl FnOnce() -> NativeResult<T> + Send + 'static,
) -> NativeResult<T> {
    deadline.check()?;
    slots
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < 4).then_some(n + 1)
        })
        .map_err(|_| NativeError::Busy)?;
    let slot = ProbeSlot(slots);
    let (send, receive) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("installer-native-observation".into())
        .spawn(move || {
            let _slot = slot;
            let _ = send.send(work());
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

impl MacNativeIo {
    // No I/O: a slow support observation must not authorize a now-invalid proof.
    fn support_proof_current(&self, proof: &SupportProof) -> NativeResult<()> {
        let age = self.clock.now_ms().checked_sub(proof.issued);
        if !proof.valid.load(Ordering::Acquire)
            || age.is_none_or(|age| age > SUPPORT_LIFETIME_MS)
            || proof.wall.elapsed() > Duration::from_millis(SUPPORT_LIFETIME_MS)
        {
            return Err(NativeError::Unsupported);
        }
        Ok(())
    }
}
// Opaque authority captured while the exact admitted original instance is still live.
pub struct TrackedAgent {
    io: Arc<MacNativeIo>,
    support: SupportProof,
    signature: SignatureProof,
    instance: Arc<AdmittedInstance>,
    erase_attempted: Arc<AtomicBool>,
}
pub struct CleanAgentExit {
    original: Arc<TrackedAgent>,
    receipt: crate::agent_contract::LastExitV1,
}
opaque_debug!(TrackedAgent, CleanAgentExit);
impl TrackedAgent {
    pub fn process(&self) -> &ProcessIdentity {
        self.instance.process()
    }
    pub fn instance_id(&self) -> u64 {
        self.instance.bootstrap().instance_id
    }
}
impl CleanAgentExit {
    pub fn receipt(&self) -> &crate::agent_contract::LastExitV1 {
        &self.receipt
    }
}
static REMOVAL_OBSERVATIONS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
impl MacNativeIo {
    /// The entire support/signature/process observation is bounded, including admission I/O.
    pub fn track_original(
        self: &Arc<Self>,
        support: &SupportProof,
        signature: &SignatureProof,
        instance: Arc<AdmittedInstance>,
        deadline: &Deadline,
    ) -> NativeResult<Arc<TrackedAgent>> {
        let (io, support, signature, limit) = (
            self.clone(),
            support.clone(),
            signature.clone(),
            deadline.clone(),
        );
        bounded_result(&REMOVAL_OBSERVATIONS, deadline, move || {
            instance.revalidate(&io, &support, &limit)?;
            if io.process_identity(instance.process().pid, &signature, &limit)?
                != *instance.process()
            {
                return Err(NativeError::Foreign);
            }
            // Target-lifetime tombstones survive handle drops and never reset after uncertainty.
            let erase_attempted = {
                let mut latches = io
                    .target
                    .erase_latches
                    .lock()
                    .map_err(|_| NativeError::Busy)?;
                let key = (instance.bootstrap().instance_id, instance.process());
                if let Some((_, _, latch)) = latches
                    .iter()
                    .find(|(id, process, _)| (*id, process) == key)
                {
                    latch.clone()
                } else {
                    if latches.len() >= MAX_NATIVE_CALLS {
                        return Err(NativeError::Busy);
                    }
                    let latch = Arc::new(AtomicBool::new(false));
                    latches.push((key.0, key.1.clone(), latch.clone()));
                    latch
                }
            };
            io.support_proof_current(&support)?;
            limit.check()?;
            Ok(Arc::new(TrackedAgent {
                io,
                support,
                signature,
                instance,
                erase_attempted,
            }))
        })
    }
    /// No receipt, already-stopped untracked installation, or ambiguous PID can mint authority.
    /// Fresh support must retain the captured session/target; expiry never renews identity.
    pub fn observe_clean_exit(
        self: &Arc<Self>,
        original: Arc<TrackedAgent>,
        support: &SupportProof,
        deadline: &Deadline,
    ) -> NativeResult<Option<CleanAgentExit>> {
        let (io, support, limit) = (self.clone(), support.clone(), deadline.clone());
        bounded_result(&REMOVAL_OBSERVATIONS, deadline, move || {
            io.checked_original_exit(&original, &support, &limit)
                .map(|receipt| {
                    receipt
                        .filter(|r| r.clean)
                        .map(|receipt| CleanAgentExit { original, receipt })
                })
        })
    }
    fn removal_proof_current(
        &self,
        original: &TrackedAgent,
        support: &SupportProof,
    ) -> NativeResult<()> {
        if !original.support.valid.load(Ordering::Acquire) {
            return Err(NativeError::Unsupported);
        }
        if support.gui != original.support.gui || support.gui_tmpdir != original.support.gui_tmpdir
        {
            return Err(NativeError::Foreign);
        }
        self.support_proof_current(support)
    }
    fn checked_original_exit(
        &self,
        original: &TrackedAgent,
        support: &SupportProof,
        deadline: &Deadline,
    ) -> NativeResult<Option<crate::agent_contract::LastExitV1>> {
        if self.target.nonce != original.io.target.nonce {
            return Err(NativeError::Foreign);
        }
        self.removal_proof_current(original, support)?;
        support.check(self, deadline)?;
        original.signature.revalidate(self)?;
        let bootstrap_path = self.target.runtime.join("bootstrap.json");
        let bootstrap = if self.metadata(&bootstrap_path)?.is_some() {
            let bytes = self.read(&bootstrap_path, 4096, true, deadline)?;
            let b = parse_bootstrap(&bytes).map_err(|_| NativeError::Invalid)?;
            if b.instance_id != original.instance_id()
                || b.pid != original.process().pid
                || b.started_unix_ms != original.instance.bootstrap().started_unix_ms
                || b.phase_seq < original.instance.bootstrap().phase_seq
                || admitted_spelling(Path::new(&b.runtime_dir))? != self.target.runtime
            {
                return Err(NativeError::Foreign);
            }
            Some(bytes)
        } else {
            None
        };
        // Observe the exact original start time, not merely a process name. Reuse is refused.
        for _ in 0..2 {
            let uid = self.execute(
                &CommandSpec::new(
                    &self.target,
                    NativeOperation::Process {
                        pid: original.process().pid,
                        field: PsField::Uid,
                    },
                )?,
                None,
                deadline,
            )?;
            if uid.code == Some(0) {
                if self.process_identity(original.process().pid, &original.signature, deadline)?
                    != *original.process()
                {
                    return Err(NativeError::Foreign);
                }
                return Ok(None);
            }
            if uid.code != Some(1) || !uid.stdout.is_empty() || !uid.stderr.is_empty() {
                return Err(NativeError::Unavailable);
            }
        }
        let receipt_path = self.target.state_dir().join("last_exit.json");
        let identity = self.metadata(&receipt_path)?;
        let bytes = identity
            .as_ref()
            .map(|_| self.read(&receipt_path, 4096, true, deadline))
            .transpose()?;
        let receipt = self.exit_receipt(original.process(), original.instance_id(), deadline)?;
        let fresh = if self.metadata(&bootstrap_path)?.is_some() {
            Some(self.read(&bootstrap_path, 4096, true, deadline)?)
        } else {
            None
        };
        if self.metadata(&receipt_path)? != identity
            || bytes
                .as_ref()
                .map(|_| self.read(&receipt_path, 4096, true, deadline))
                .transpose()?
                != bytes
            || self.metadata(&receipt_path)? != identity
            || bytes
                .as_ref()
                .map(|b| {
                    crate::agent_contract::parse_last_exit(b).map_err(|_| NativeError::Invalid)
                })
                .transpose()?
                != receipt
            || fresh != bootstrap
        {
            return Err(NativeError::Foreign);
        }
        if let Some(r) = &receipt {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| NativeError::Unavailable)?
                .as_millis();
            if u128::from(r.stopped_unix_ms) > now {
                return Err(NativeError::Foreign);
            }
        }
        original.signature.revalidate(self)?;
        self.removal_proof_current(original, support)?;
        deadline.check()?;
        Ok(receipt)
    }
    /// Exactly one attempt per exact original across all handles, including failures.
    /// Fresh support renews session admission only. Unknown requires re-detection;
    /// a new clean-exit observation cannot reset the dispatch latch.
    pub fn erase_installed_identity(
        self: &Arc<Self>,
        clean: CleanAgentExit,
        support: &SupportProof,
        deadline: &Deadline,
    ) -> NativeResult<crate::agent_contract::EraseIdentityV1> {
        let (io, support, limit) = (self.clone(), support.clone(), deadline.clone());
        let attempted = Arc::new(AtomicBool::new(false));
        let dispatched = attempted.clone();
        let result = bounded_result(&REMOVAL_OBSERVATIONS, deadline, move || {
            if io.checked_original_exit(&clean.original, &support, &limit)?
                != Some(clean.receipt.clone())
            {
                return Err(NativeError::Refused);
            }
            let spec = CommandSpec::installed_erase(&io.target, &clean.original.signature)?;
            clean
                .original
                .erase_attempted
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .map_err(|_| NativeError::Refused)?;
            io.removal_proof_current(&clean.original, &support)?;
            limit.check()?;
            let mut spec = spec;
            support.check(&io, &limit)?;
            clean.original.signature.revalidate(&io)?;
            io.validate_target()?;
            spec.authorized = Some(Instant::now());
            io.target.observe("dispatch", spec.program(), None)?;
            limit.check()?;
            io.removal_proof_current(&clean.original, &support)?;
            dispatched.store(true, Ordering::Release);
            let output = io.runner.run(&spec, &limit)?;
            if output.code != Some(0)
                || !output.stderr.is_empty()
                || output.stdout.len() + output.stderr.len() > spec.max_output()
            {
                return Err(NativeError::OutcomeUnknown);
            }
            let receipt = crate::agent_contract::parse_erase_identity(&output.stdout)
                .map_err(|_| NativeError::OutcomeUnknown)?;
            if io.checked_original_exit(&clean.original, &support, &limit)? != Some(clean.receipt) {
                return Err(NativeError::OutcomeUnknown);
            }
            Ok(receipt)
        });
        match result {
            Err(_) if attempted.load(Ordering::Acquire) => Err(NativeError::OutcomeUnknown),
            other => other,
        }
    }
}
