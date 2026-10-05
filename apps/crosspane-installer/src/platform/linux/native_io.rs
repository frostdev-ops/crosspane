//! Native observations do not imply readiness. Scratch authority stays inside its own target.
mod cleanup;
pub use cleanup::CleanupLease;
pub use cleanup::CleanupProof;
pub(crate) use cleanup::RepairJournalSnapshot;
mod removal;
mod runtime;
pub(crate) use runtime::DeadRuntime;
pub mod tutorial;
use crate::agent_contract::{BootstrapV1, InstanceStatus, ObservationSource, parse_bootstrap};
pub use removal::{ExitReader, ProcessExit, ProcessWatch};
use rustix::fs::{self as rfs, AtFlags, FlockOperation, Mode, OFlags, ResolveFlags};
use rustix::net::{self as rnet, AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::fs::{DirBuilderExt, MetadataExt},
        unix::net::UnixStream,
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
pub const MAX_OS_RELEASE_BYTES: usize = 1024 * 1024;
pub const MAX_FONT_BYTES: usize = 16 * 1024 * 1024;
/// ELF bytes read for dependency metadata. Real images keep PT_DYNAMIC near their end (the release
/// agent at ~25 MiB, libavcodec ~20 MiB, libicudata ~33 MiB), so this equals the payload member
/// bound (`payload::MAX_MEMBER_BYTES`) rather than a small header prefix.
pub const MAX_ELF_PREFIX_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_UFW_BYTES: usize = 1024 * 1024;
pub const MAX_FIREWALL_COMMAND_BYTES: usize = 1024 * 1024;
static READ_WORKERS: AtomicUsize = AtomicUsize::new(0);

/// Read-only system namespaces; ELF returns a prefix, never executes or resolves a program.
#[derive(Clone, Debug)]
pub enum SystemRead {
    UfwConfig,
    UfwRules,
    UfwRules6,
    OsRelease,
    OsReleaseFallback,
    Font(PathBuf),
    ElfPrefix(PathBuf),
    /// Bare SONAME, with at most three same-directory root-owned library links.
    Library(String),
}
#[derive(Debug)]
pub struct SystemBytes {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    /// Full admitted file length; an ELF prefix may be shorter.
    pub file_size: u64,
}
impl SystemRead {
    fn path_and_limit(&self) -> Result<(PathBuf, usize)> {
        let (path, limit) = match self {
            Self::UfwConfig => (PathBuf::from("/etc/ufw/ufw.conf"), MAX_UFW_BYTES),
            Self::UfwRules => (PathBuf::from("/etc/ufw/user.rules"), MAX_UFW_BYTES),
            Self::UfwRules6 => (PathBuf::from("/etc/ufw/user6.rules"), MAX_UFW_BYTES),
            Self::OsRelease => (PathBuf::from("/etc/os-release"), MAX_OS_RELEASE_BYTES),
            Self::OsReleaseFallback => (PathBuf::from("/usr/lib/os-release"), MAX_OS_RELEASE_BYTES),
            Self::Font(path) => (path.clone(), MAX_FONT_BYTES),
            Self::ElfPrefix(path) => (path.clone(), MAX_ELF_PREFIX_BYTES),
            Self::Library(name) if library_name(name) => {
                (Path::new("/usr/lib").join(name), MAX_ELF_PREFIX_BYTES)
            }
            Self::Library(_) => return Err(NativeError::Foreign),
        };
        if !clean(&path)
            || match self {
                Self::Font(_) => {
                    !path.starts_with("/usr/share/fonts") || path == Path::new("/usr/share/fonts")
                }
                Self::ElfPrefix(_) => !["/usr/lib", "/usr/lib64"]
                    .iter()
                    .any(|root| path.starts_with(root) && path != Path::new(root)),
                _ => false,
            }
        {
            return Err(NativeError::Foreign);
        }
        Ok((path, limit))
    }
}

struct ReadParent {
    directories: Vec<OwnedFd>,
    name: String,
}
impl ReadParent {
    fn open_file(&self) -> Result<OwnedFd> {
        self.open_with_permissions(false)
    }
    fn open_with_permissions(&self, ufw: bool) -> Result<OwnedFd> {
        rfs::openat(
            self.directories.last().ok_or(NativeError::Invalid)?,
            &self.name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| read_error(error, ufw))
    }
    fn same_as(&self, other: &Self) -> Result<()> {
        if self.directories.len() != other.directories.len() {
            return Err(NativeError::Foreign);
        }
        for (old, new) in self.directories.iter().zip(&other.directories) {
            let a = native(rfs::fstat(old))?;
            let b = native(rfs::fstat(new))?;
            if (a.st_dev, a.st_ino) != (b.st_dev, b.st_ino) {
                return Err(NativeError::Foreign);
            }
        }
        Ok(())
    }
}
fn read_stat(stat: &rfs::Stat, uid: u32, kind: u32) -> Result<()> {
    if stat.st_uid != uid || stat.st_mode & 0o170000 != kind || stat.st_mode & 0o022 != 0 {
        return Err(NativeError::Foreign);
    }
    Ok(())
}
// The anchor is / for system data, or the admitted private runtime for selected sockets.
fn read_parent(anchor: OwnedFd, relative: &Path, uid: u32) -> Result<ReadParent> {
    read_parent_permissions(anchor, relative, uid, false)
}
fn read_error(error: rustix::io::Errno, ufw: bool) -> NativeError {
    if ufw && matches!(error, rustix::io::Errno::ACCESS | rustix::io::Errno::PERM) {
        NativeError::PermissionDenied
    } else {
        NativeError::Foreign
    }
}
fn final_read_stat(parent: &ReadParent, name: &str, ufw: bool) -> Result<rfs::Stat> {
    rfs::statat(
        parent.directories.last().ok_or(NativeError::Invalid)?,
        name,
        AtFlags::SYMLINK_NOFOLLOW,
    )
    .map_err(|error| match read_error(error, ufw) {
        NativeError::PermissionDenied => NativeError::PermissionDenied,
        _ => NativeError::Unavailable,
    })
}
fn read_parent_permissions(
    anchor: OwnedFd,
    relative: &Path,
    uid: u32,
    ufw: bool,
) -> Result<ReadParent> {
    let name = relative
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or(NativeError::Invalid)?
        .to_owned();
    let mut directories = vec![anchor];
    read_stat(&native(rfs::fstat(&directories[0]))?, uid, 0o040000)?;
    for component in relative.parent().ok_or(NativeError::Invalid)?.components() {
        let Component::Normal(name) = component else {
            return Err(NativeError::Foreign);
        };
        let current = directories.last().ok_or(NativeError::Invalid)?;
        let child = rfs::openat(
            current,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| read_error(error, ufw))?;
        read_stat(&native(rfs::fstat(&child))?, uid, 0o040000)?;
        directories.push(child);
    }
    Ok(ReadParent { directories, name })
}
fn read_anchor(path: &Path) -> Result<OwnedFd> {
    native(rfs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ))
}
fn library_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.contains("..")
        && name != "."
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._+-".contains(&c))
}
fn library_snapshot(parent: &ReadParent, name: &str) -> Result<rfs::Stat> {
    rfs::statat(
        parent.directories.last().ok_or(NativeError::Invalid)?,
        name,
        AtFlags::SYMLINK_NOFOLLOW,
    )
    .map_err(|e| {
        if e == rustix::io::Errno::NOENT {
            NativeError::Unavailable
        } else {
            NativeError::Foreign
        }
    })
}
struct LibraryEntry {
    name: String,
    stat: rfs::Stat,
    fd: OwnedFd,
}
fn library_chain(
    parent: &mut ReadParent,
    deadline: &Deadline,
    mut evidence: impl FnMut(rfs::Stat) -> rfs::Stat,
) -> Result<Vec<LibraryEntry>> {
    let mut chain = Vec::new();
    loop {
        deadline.check()?;
        let fd = rfs::openat(
            parent.directories.last().ok_or(NativeError::Invalid)?,
            &parent.name,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| {
            if e == rustix::io::Errno::NOENT {
                NativeError::Unavailable
            } else {
                NativeError::Foreign
            }
        })?;
        let stat = evidence(native(rfs::fstat(&fd))?);
        if stat.st_uid != 0 {
            return Err(NativeError::Foreign);
        }
        chain.push(LibraryEntry {
            name: parent.name.clone(),
            stat,
            fd,
        });
        if stat.st_mode & 0o170000 == 0o100000 {
            read_stat(&stat, 0, 0o100000)?;
            return Ok(chain);
        }
        // Linux symlink mode bits are always 0777 and nonfunctional; ownership is checked.
        if stat.st_mode & 0o170000 != 0o120000 || chain.len() > 3 {
            return Err(NativeError::Foreign);
        }
        let mut bytes = [0u8; 256];
        // The link's metadata and target come from the same retained no-follow descriptor.
        let count = native(rfs::readlinkat_raw(
            &chain.last().ok_or(NativeError::Invalid)?.fd,
            "",
            &mut bytes[..],
        ))?;
        let target = std::str::from_utf8(&bytes[..count]).map_err(|_| NativeError::Foreign)?;
        if !library_name(target) {
            return Err(NativeError::Foreign);
        }
        parent.name = target.to_owned();
    }
}
fn library_revalidate(
    parent: &ReadParent,
    chain: &[LibraryEntry],
    deadline: &Deadline,
    mut evidence: impl FnMut(rfs::Stat) -> rfs::Stat,
) -> Result<()> {
    for entry in chain {
        deadline.check()?;
        let before = &entry.stat;
        for after in [
            evidence(native(rfs::fstat(&entry.fd))?),
            evidence(library_snapshot(parent, &entry.name)?),
        ] {
            if (before.st_dev, before.st_ino, before.st_uid, before.st_mode)
                != (after.st_dev, after.st_ino, after.st_uid, after.st_mode)
            {
                return Err(NativeError::Foreign);
            }
        }
    }
    Ok(())
}
fn system_contents(
    file: &mut File,
    stat: &rfs::Stat,
    request: &SystemRead,
    deadline: &Deadline,
) -> Result<Vec<u8>> {
    read_stat(stat, 0, 0o100000)?;
    if stat.st_size < 0 {
        return Err(NativeError::Invalid);
    }
    let (_, limit) = request.path_and_limit()?;
    let prefix = matches!(request, SystemRead::ElfPrefix(_) | SystemRead::Library(_));
    if !prefix && stat.st_size > limit as i64 {
        return Err(NativeError::Oversize);
    }
    deadline.check()?;
    let mut bytes = Vec::new();
    file.take((limit + usize::from(!prefix)) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            if matches!(
                request,
                SystemRead::UfwConfig | SystemRead::UfwRules | SystemRead::UfwRules6
            ) && error.kind() == std::io::ErrorKind::PermissionDenied
            {
                NativeError::PermissionDenied
            } else {
                NativeError::Unavailable
            }
        })?;
    deadline.check()?;
    if bytes.len() > limit {
        return Err(NativeError::Oversize);
    }
    if prefix
        && (bytes.len() < 16
            || &bytes[..4] != b"\x7fELF"
            || !matches!(bytes[4], 1 | 2)
            || !matches!(bytes[5], 1 | 2)
            || bytes[6] != 1)
    {
        return Err(NativeError::Invalid);
    }
    if prefix && bytes.len() < if bytes[4] == 1 { 52 } else { 64 } {
        return Err(NativeError::Invalid);
    }
    Ok(bytes)
}

#[derive(Clone, Copy)]
enum ReadSocket {
    SessionBus,
    SystemBus,
    Wayland,
    Hyprland,
}
fn read_socket_stat(stat: &rfs::Stat, uid: u32, public_bus: bool) -> Result<()> {
    if stat.st_uid != uid
        || stat.st_mode & 0o170000 != 0o140000
        || stat.st_nlink != 1
        || (!public_bus && stat.st_mode & 0o022 != 0)
    {
        return Err(NativeError::Foreign);
    }
    Ok(())
}
fn socket_location(
    target: &LinuxTarget,
    environment: Option<&ChildEnvironment>,
    kind: ReadSocket,
) -> Result<(PathBuf, u32)> {
    if matches!(kind, ReadSocket::SystemBus) {
        return Ok(("/run/dbus/system_bus_socket".into(), 0));
    }
    let environment = environment.ok_or(NativeError::Invalid)?;
    if environment.nonce != target.nonce {
        return Err(NativeError::Foreign);
    }
    let root = &target.paths.runtime_home;
    let path = match kind {
        ReadSocket::SessionBus => environment
            .bus
            .as_ref()
            .ok_or(NativeError::Unavailable)?
            .path
            .clone(),
        ReadSocket::Wayland => root.join(
            environment
                .values
                .get("WAYLAND_DISPLAY")
                .ok_or(NativeError::Unavailable)?,
        ),
        ReadSocket::Hyprland => {
            let signature = environment
                .values
                .get("HYPRLAND_INSTANCE_SIGNATURE")
                .ok_or(NativeError::Unavailable)?;
            if signature.is_empty()
                || signature.len() > 256
                || !signature
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-.".contains(&c))
            {
                return Err(NativeError::Foreign);
            }
            root.join("hypr").join(signature).join(".socket.sock")
        }
        ReadSocket::SystemBus => return Err(NativeError::Invalid),
    };
    if !clean(&path) || !path.starts_with(root) || path == *root {
        return Err(NativeError::Foreign);
    }
    Ok((path, target.paths.uid))
}
fn socket_parent(target: &LinuxTarget, path: &Path, uid: u32) -> Result<ReadParent> {
    let root = if uid == 0 {
        Path::new("/")
    } else {
        &target.paths.runtime_home
    };
    let anchor = if uid == 0 {
        read_anchor(root)?
    } else {
        let anchor = walk_dir(target, root, None)?.ok_or(NativeError::Unavailable)?;
        if native(rfs::fstat(&anchor))?.st_mode & 0o777 != 0o700 {
            return Err(NativeError::Foreign);
        }
        anchor
    };
    read_parent(
        anchor,
        path.strip_prefix(root).map_err(|_| NativeError::Foreign)?,
        uid,
    )
}
fn socket_observation(parent: &ReadParent, uid: u32, public_bus: bool) -> Result<rfs::Stat> {
    let dir = parent.directories.last().ok_or(NativeError::Invalid)?;
    let stat = rfs::statat(dir, &parent.name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| NativeError::Foreign)?;
    read_socket_stat(&stat, uid, public_bus)?;
    Ok(stat)
}
fn read_peer(actual: u32, expected: u32) -> Result<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(NativeError::Foreign)
    }
}
fn connect_read_socket(parent: &ReadParent, uid: u32, deadline: &Deadline) -> Result<UnixStream> {
    let dir = parent.directories.last().ok_or(NativeError::Invalid)?;
    let address = native(SocketAddrUnix::new(format!(
        "/proc/self/fd/{}/{}",
        dir.as_raw_fd(),
        parent.name
    )))?;
    let fd = native(rnet::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
        None,
    ))?;
    loop {
        deadline.check()?;
        match rnet::connect(&fd, &address) {
            Ok(()) | Err(rustix::io::Errno::ISCONN) => break,
            Err(
                rustix::io::Errno::AGAIN
                | rustix::io::Errno::INPROGRESS
                | rustix::io::Errno::ALREADY,
            ) => thread::sleep(Duration::from_millis(2)),
            Err(_) => return Err(NativeError::Unavailable),
        }
    }
    read_peer(
        native(rnet::sockopt::socket_peercred(&fd))?.uid.as_raw(),
        uid,
    )?;
    Ok(UnixStream::from(fd))
}
static NONCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NativeError {
    #[error("native read permission denied")]
    PermissionDenied,
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
#[derive(Clone, Debug, PartialEq, Eq)]
struct SessionAuthority {
    uid: u32,
    uwsm_managed: bool,
    graphical_target_active: bool,
    graphical_sessions: usize,
    session_id: String,
    session_type: String,
    seat: String,
    active: bool,
}
impl From<&SupportObservations> for SessionAuthority {
    fn from(facts: &SupportObservations) -> Self {
        Self {
            uid: facts.uid,
            uwsm_managed: facts.uwsm_managed,
            graphical_target_active: facts.graphical_target_active,
            graphical_sessions: facts.graphical_sessions,
            session_id: facts.session_id.clone(),
            session_type: facts.session_type.clone(),
            seat: facts.seat.clone(),
            active: facts.active,
        }
    }
}
#[derive(Clone, Debug)]
pub struct SupportProof {
    nonce: u64,
    issued: Instant,
    facts: SessionAuthority,
    valid: Arc<AtomicBool>,
    advisory: super::detect::CompatibilityReport,
}
impl SupportProof {
    pub(crate) fn admit(io: &LinuxNativeIo, facts: SupportObservations) -> Result<Self> {
        if facts.uid != io.target.paths.uid
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
            facts: SessionAuthority::from(&facts),
            valid: Arc::new(AtomicBool::new(true)),
            advisory: super::detect::CompatibilityReport::default(),
        })
    }
    pub fn revalidate(&self, io: &LinuxNativeIo, current: &SupportObservations) -> Result<()> {
        self.check(io)?;
        if SessionAuthority::from(current) != self.facts {
            self.valid.store(false, Ordering::Release);
            return Err(NativeError::Unsupported);
        }
        io.validate_target()
    }
    pub(crate) fn with_advisory(mut self, report: super::detect::CompatibilityReport) -> Self {
        self.advisory = report;
        self
    }
    pub fn advisory(&self) -> &super::detect::CompatibilityReport {
        &self.advisory
    }
    /// Call only at steps that need the agent's capabilities. An unknown fact remains a note;
    /// a positive incompatibility is still a refusal and is never presented as ready.
    pub fn check_agent_compatibility(&self) -> Result<()> {
        if matches!(
            self.advisory.eligibility,
            super::detect::Eligibility::NotSupported(_)
        ) {
            Err(NativeError::Unsupported)
        } else {
            Ok(())
        }
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
/// Pin the private user manager, including both directory identities. The same-UID window
/// between the final check and systemctl's connect is outside the admitted threat model.
#[derive(Clone, Debug)]
struct ManagerEndpoint {
    runtime: Arc<OwnedFd>,
    directory: Arc<OwnedFd>,
    socket: (u64, u64),
}
fn manager_socket_stat(stat: &rfs::Stat, uid: u32) -> Result<()> {
    if stat.st_uid != uid || stat.st_mode & 0o170000 != 0o140000 || stat.st_nlink != 1 {
        return Err(NativeError::Foreign);
    }
    Ok(())
}
impl ManagerEndpoint {
    fn admit(target: &LinuxTarget) -> Result<Self> {
        let runtime =
            walk_dir(target, &target.paths.runtime_home, None)?.ok_or(NativeError::Unavailable)?;
        let stat = native(rfs::fstat(&runtime))?;
        if stat.st_uid != target.paths.uid || stat.st_mode & 0o777 != 0o700 {
            return Err(NativeError::Foreign);
        }
        let directory = rfs::openat(
            &runtime,
            "systemd",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| NativeError::Foreign)?;
        let stat = native(rfs::fstat(&directory))?;
        if stat.st_uid != target.paths.uid || stat.st_mode & 0o022 != 0 {
            return Err(NativeError::Foreign);
        }
        let stat = rfs::statat(&directory, "private", AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|_| NativeError::Foreign)?;
        manager_socket_stat(&stat, target.paths.uid)?;
        Ok(Self {
            runtime: Arc::new(runtime),
            directory: Arc::new(directory),
            socket: (stat.st_dev, stat.st_ino),
        })
    }
    fn revalidate(&self, target: &LinuxTarget) -> Result<()> {
        let current = Self::admit(target).map_err(|_| NativeError::Foreign)?;
        for (old, new) in [
            (&self.runtime, &current.runtime),
            (&self.directory, &current.directory),
        ] {
            let old = native(rfs::fstat(old.as_ref()))?;
            let new = native(rfs::fstat(new.as_ref()))?;
            if (old.st_dev, old.st_ino) != (new.st_dev, new.st_ino) {
                return Err(NativeError::Foreign);
            }
        }
        if self.socket != current.socket {
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
    manager: Option<ManagerEndpoint>,
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
            manager: None,
        })
    }
    pub fn values(&self) -> &BTreeMap<String, String> {
        &self.values
    }
}
#[derive(Debug)]
pub struct CommandSpec {
    executable: PathBuf,
    argv: Vec<String>,
    environment: ChildEnvironment,
    output_limit: usize,
    admission: Option<(LinuxTarget, Option<SupportProof>)>,
    lease: Option<Arc<LeaseGuard>>,
    spawn_attempt: Option<Arc<AtomicBool>>,
    agent: Option<Arc<removal::InstalledExecutable>>,
    cleanup: Option<Arc<cleanup::Binding>>,
}
impl Clone for CommandSpec {
    fn clone(&self) -> Self {
        Self {
            executable: self.executable.clone(),
            argv: self.argv.clone(),
            environment: self.environment.clone(),
            output_limit: self.output_limit,
            admission: self.admission.clone(),
            lease: None,
            spawn_attempt: self.spawn_attempt.clone(),
            agent: self.agent.clone(),
            cleanup: self.cleanup.clone(),
        }
    }
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
            || output_limit
                > if firewall_read(&executable, &argv) {
                    MAX_FIREWALL_COMMAND_BYTES
                } else if executable == Path::new("/usr/bin/systemctl")
                    && (argv == ["--user", "show-environment"]
                        || argv.get(1).is_some_and(|verb| verb == "show"))
                {
                    super::detect::MAX_PROBE_BYTES
                } else {
                    MAX_COMMAND_BYTES
                }
            || argv
                .iter()
                .any(|s| s.len() > 4096 || s.chars().any(char::is_control))
            || argv.iter().map(String::len).sum::<usize>() > 32 * 1024
        {
            return Err(NativeError::Invalid);
        }
        approved_executable(&executable, &argv)?;
        if firewall_read(&executable, &argv)
            && (environment.manager.is_some() || environment.bus.is_some())
        {
            return Err(NativeError::Invalid);
        }
        if executable == Path::new("/usr/bin/systemctl")
            && !firewall_read(&executable, &argv)
            && environment.manager.is_none()
        {
            return Err(NativeError::Invalid);
        }
        Ok(Self {
            executable,
            argv,
            environment,
            output_limit,
            admission: None,
            lease: None,
            spawn_attempt: None,
            agent: None,
            cleanup: None,
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
// Fixed mappings only: /bin/ps -> /usr/bin/ps; systemctl and fc-match use their literal /usr/bin paths.
/// The sole show query; no caller-selected property, unit, scope or remote target.
pub const MANAGER_PROPERTIES: &str = "Id,LoadState,FragmentPath,DropInPaths,ExecStart,Environment,User,Group,DynamicUser,ActiveState,SubState,UnitFileState,MainPID,PartOf,After,Requisite,WantedBy,KillSignal,TimeoutStopUSec,Restart,RestartUSec,NeedDaemonReload,ExecStartPre,ExecStartPost,ExecStop,ExecStopPost,ExecReload,EnvironmentFiles,RootDirectory,RootImage,StartLimitIntervalUSec,StartLimitBurst,ExecCondition,Type,Requires,Wants,BindsTo,Upholds,OnFailure,Conflicts,Before,DefaultDependencies,KillMode,SendSIGKILL,FinalKillSignal,RestartKillSignal,SendSIGHUP,UnsetEnvironment,PassEnvironment,WorkingDirectory,UMask,BusName,PIDFile,RemainAfterExit,NotifyAccess,ExecSearchPath,StandardInput,StandardOutput,StandardError,TTYPath,OnSuccess,PropagatesStopTo,PropagatesReloadTo,ReloadPropagatedFrom,StopPropagatedFrom,JoinsNamespaceOf,RequiresMountsFor,WantsMountsFor,RequiredBy,RequisiteOf,BoundBy,UpheldBy,ConsistsOf,ConflictedBy,OnSuccessOf,OnFailureOf,Triggers,TriggeredBy,Following,SliceOf,DelegateControllers,DelegateSubgroup,Conditions,Asserts,ExecConditionEx,ExecStartPreEx,ExecStartPostEx,ExecStopEx,ExecStopPostEx,ExecReloadEx,ExecReloadPost,ExecReloadPostEx,RestartPreventExitStatus,RestartForceExitStatus,SuccessExitStatus,OpenFile,ExtraFileDescriptorNames,BindPaths,BindReadOnlyPaths,TemporaryFileSystem,MountImages,ExtensionImages,ExtensionDirectories,PAMName,Slice,Delegate,OOMPolicy,ManagedOOMSwap,ManagedOOMMemoryPressure,ManagedOOMPreference,SuccessAction,FailureAction,StartLimitAction,JobTimeoutAction,OnSuccessJobMode,OnFailureJobMode,StopWhenUnneeded,RefuseManualStart,RefuseManualStop,AllowIsolate,IgnoreOnIsolate,SurviveFinalKillSignal,JobTimeoutUSec,JobRunningTimeoutUSec,CollectMode,RestartMode,RestartSteps,RestartMaxDelayUSec,TimeoutStartFailureMode,TimeoutStopFailureMode,RuntimeMaxUSec,RuntimeRandomizedExtraUSec,WatchdogUSec,ExitType,FileDescriptorStoreMax,NFileDescriptorStore,FileDescriptorStorePreserve,RootDirectoryStartOnly,RootEphemeral,ExecStartEx,RuntimeDirectory,StateDirectory,CacheDirectory,LogsDirectory,ConfigurationDirectory,RuntimeDirectorySymlink,StateDirectorySymlink,CacheDirectorySymlink,LogsDirectorySymlink,RootMStack,RuntimeDirectoryPreserve";
fn manager_mutation(spec: &CommandSpec) -> bool {
    spec.executable == Path::new("/usr/bin/systemctl")
        && matches!(
            spec.argv.get(1).map(String::as_str),
            Some("start" | "stop" | "restart" | "enable" | "disable" | "daemon-reload")
        )
}
fn firewall_read(path: &Path, argv: &[String]) -> bool {
    let a: Vec<_> = argv.iter().map(String::as_str).collect();
    matches!(
        (path.to_str(), a.as_slice()),
        (Some("/usr/bin/systemctl"), ["is-active", "ufw.service"])
            | (Some("/usr/bin/ip"), ["-j", "-d", "addr", "show"])
            | (Some("/usr/bin/ip"), ["-j", "route", "show", "default"])
            | (
                Some("/usr/bin/ip"),
                ["-j", "-6", "route", "show", "default"]
            )
    )
}
fn approved_executable(path: &Path, argv: &[String]) -> Result<&'static str> {
    if firewall_read(path, argv) {
        return Ok(if path == Path::new("/usr/bin/ip") {
            "/usr/bin/ip"
        } else {
            "/usr/bin/systemctl"
        });
    }
    let a: Vec<_> = argv.iter().map(String::as_str).collect();
    match (path.to_str(), a.as_slice()) {
        (Some("/usr/bin/fc-match"), ["-f", "%{file}", "sans-serif"]) => Ok("/usr/bin/fc-match"),
        (Some("/usr/bin/systemctl"), ["--user", "show-environment"]) => Ok("/usr/bin/systemctl"),
        (Some("/bin/ps"), ["-o", "lstart=" | "comm=", "-p", pid])
            if pid
                .parse::<u32>()
                .is_ok_and(|v| v != 0 && v.to_string() == *pid) =>
        {
            Ok("/usr/bin/ps")
        }
        (Some("/usr/bin/systemctl"), ["--user", "daemon-reload"])
        | (
            Some("/usr/bin/systemctl"),
            [
                "--user",
                "start" | "stop" | "restart" | "enable" | "disable" | "is-active" | "is-enabled"
                | "cat",
                "crosspane-agent.service",
            ],
        ) => Ok("/usr/bin/systemctl"),
        (
            Some("/usr/bin/systemctl"),
            [
                "--user",
                "show",
                "--all",
                "crosspane-agent.service",
                "-p",
                properties,
            ],
        ) if *properties == MANAGER_PROPERTIES => Ok("/usr/bin/systemctl"),
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
/// Owns only the admitted payload installation flock; there is no path or unlock API.
#[derive(Debug)]
pub struct InstallLease {
    lock: File,
    nonce: u64,
}
#[derive(Debug)]
struct LeaseGuard {
    lease: Option<InstallLease>,
    finished: Arc<AtomicBool>,
}
impl Drop for LeaseGuard {
    fn drop(&mut self) {
        if let Some(InstallLease { lock, .. }) = self.lease.take() {
            drop(lock);
        }
        self.finished.store(true, Ordering::Release);
    }
}
/// Completion of outstanding launch AND child cleanup, without access to its installation lock.
#[derive(Clone, Debug)]
pub struct PendingOperation(Arc<AtomicBool>);
impl PendingOperation {
    pub fn completed(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}
#[derive(Debug)]
pub struct ManagerMutation {
    pub result: Result<CommandOutput>,
    pub pending: Option<PendingOperation>,
    /// The manager child was spawned (or the injected runner was dispatched).
    pub submitted: bool,
}
pub trait CommandRunner: Send + Sync {
    fn run(&self, command: &CommandSpec, deadline: &Deadline) -> Result<CommandOutput>;
    /// Production reports its successful spawn; injected runners count dispatch as submission.
    fn tracks_submission(&self) -> bool {
        false
    }
}
/// Canonical network only; further LAN interface/range policy belongs to the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LanCidr(String);
impl LanCidr {
    pub fn parse(value: &str) -> Result<Self> {
        let (address, prefix) = value.split_once('/').ok_or(NativeError::Invalid)?;
        let address: std::net::IpAddr = address.parse().map_err(|_| NativeError::Invalid)?;
        let bits: u32 = prefix.parse().map_err(|_| NativeError::Invalid)?;
        if bits.to_string() != prefix
            || address.is_unspecified()
            || address.is_loopback()
            || address.is_multicast()
        {
            return Err(NativeError::Invalid);
        }
        let valid = match address {
            std::net::IpAddr::V4(a) => {
                (8..=32).contains(&bits)
                    && a.octets()[..2] != [169, 254]
                    && u32::from(a) & !(u32::MAX << (32 - bits)) == 0
            }
            std::net::IpAddr::V6(a) => {
                (16..=128).contains(&bits)
                    && a.segments()[0] & 0xffc0 != 0xfe80
                    && u128::from(a) & !(u128::MAX << (128 - bits)) == 0
            }
        };
        if !valid {
            return Err(NativeError::Invalid);
        }
        Ok(Self(format!("{address}/{bits}")))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UfwRule {
    Lan,
    Mdns,
}
#[derive(Clone, Debug)]
pub struct UfwMutation {
    pub delete: bool,
    pub cidr: LanCidr,
    pub rule: UfwRule,
}
#[derive(Debug)]
pub enum PkexecOutcome {
    Exited {
        code: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        stdout_truncated: bool,
        stderr_truncated: bool,
    },
    TimedOut,
    Signalled,
}
/// The only privileged command model; callers cannot provide argv or inherited environment.
#[derive(Clone, Debug)]
pub struct PkexecCommand {
    argv: Vec<String>,
    target: LinuxTarget,
    proof: SupportProof,
}
impl PkexecCommand {
    pub fn executable(&self) -> &Path {
        Path::new("/usr/bin/setsid")
    }
    pub fn argv(&self) -> &[String] {
        &self.argv
    }
    pub fn environment(&self) -> [(&'static str, &'static str); 3] {
        [("PATH", "/usr/bin:/bin"), ("LANG", "C"), ("LC_ALL", "C")]
    }
    pub fn stdin_null(&self) -> bool {
        true
    }
    pub fn new_session(&self) -> bool {
        true
    }
    pub fn process_group(&self) -> Option<i32> {
        None
    }
}
/// Injected only into an admitted scratch target. Production always uses the native runner.
pub trait PkexecRunner: Send + Sync {
    fn spawn(&self, command: &PkexecCommand, deadline: &Deadline) -> Result<Box<dyn PkexecChild>>;
}
pub trait PkexecChild: Send {
    fn poll(&mut self) -> Result<Option<PkexecOutcome>>;
    fn terminate(&mut self);
    fn reaped(&mut self) -> bool;
}
static UFW_DISPATCHES: AtomicUsize = AtomicUsize::new(0);
fn ufw_program_stat(stat: &rfs::Stat, pkexec: bool) -> Result<()> {
    read_stat(stat, 0, 0o100000)?;
    if stat.st_mode & 0o111 == 0 || stat.st_mode & 0o6000 != if pkexec { 0o4000 } else { 0 } {
        return Err(NativeError::Foreign);
    }
    Ok(())
}
fn ufw_program(path: &Path) -> Result<OwnedFd> {
    let parent = read_parent(
        read_anchor(Path::new("/"))?,
        path.strip_prefix("/").map_err(|_| NativeError::Invalid)?,
        0,
    )?;
    let fd = parent.open_file()?;
    ufw_program_stat(
        &native(rfs::fstat(&fd))?,
        path == Path::new("/usr/bin/pkexec"),
    )?;
    Ok(fd)
}
struct NativePkexecRunner;
struct PkexecPipe {
    reader: Box<dyn Read + Send>,
    bytes: Vec<u8>,
    truncated: bool,
}
impl PkexecPipe {
    fn empty() -> Self {
        Self {
            reader: Box::new(std::io::empty()),
            bytes: Vec::new(),
            truncated: false,
        }
    }
    fn drain(&mut self) -> Result<bool> {
        let mut buffer = [0; 4096];
        // Finite work per poll, including discarded overflow; an endless writer cannot starve the deadline.
        for _ in 0..16 {
            match self.reader.read(&mut buffer) {
                Ok(0) => return Ok(true),
                Ok(n) => {
                    let keep = n.min(MAX_COMMAND_BYTES.saturating_sub(self.bytes.len()));
                    self.bytes.extend_from_slice(&buffer[..keep]);
                    self.truncated |= keep != n;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return Err(NativeError::Unavailable),
            }
        }
        Ok(false)
    }
}
struct NativePkexecChild {
    child: Child,
    out: PkexecPipe,
    err: PkexecPipe,
    status: Option<std::process::ExitStatus>,
    setup_error: Option<NativeError>,
}
impl PkexecChild for NativePkexecChild {
    fn poll(&mut self) -> Result<Option<PkexecOutcome>> {
        if let Some(error) = self.setup_error.take() {
            return Err(error);
        }
        let out = self.out.drain();
        let err = self.err.drain();
        let done = (out?, err?);
        if self.status.is_none() {
            self.status = native(self.child.try_wait())?;
        }
        Ok(self
            .status
            .filter(|_| done == (true, true))
            .map(|status| match status.code() {
                Some(code) => PkexecOutcome::Exited {
                    code,
                    stdout: std::mem::take(&mut self.out.bytes),
                    stderr: std::mem::take(&mut self.err.bytes),
                    stdout_truncated: self.out.truncated,
                    stderr_truncated: self.err.truncated,
                },
                None => PkexecOutcome::Signalled,
            }))
    }
    fn terminate(&mut self) {
        if self.status.is_none()
            && let Some(pid) = rustix::process::Pid::from_raw(self.child.id() as i32)
        {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        }
    }
    fn reaped(&mut self) -> bool {
        self.status.is_some() || matches!(self.child.try_wait(), Ok(Some(_)))
    }
}
impl PkexecRunner for NativePkexecRunner {
    fn spawn(&self, spec: &PkexecCommand, deadline: &Deadline) -> Result<Box<dyn PkexecChild>> {
        use std::os::unix::process::CommandExt;
        let paths = ["/usr/bin/setsid", "/usr/bin/pkexec", "/usr/bin/ufw"];
        let mut admitted = Vec::new();
        for path in paths {
            deadline.check()?;
            admitted.push(ufw_program(Path::new(path))?);
        }
        validate_target(&spec.target)?;
        let mut command = Command::new(format!("/proc/self/fd/{}", admitted[0].as_raw_fd()));
        command
            .arg0(spec.executable())
            .args(&spec.argv)
            .env_clear()
            .envs(spec.environment())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (path, fd) in paths.into_iter().zip(&admitted) {
            let now = native(rfs::fstat(&ufw_program(Path::new(path))?))?;
            let old = native(rfs::fstat(fd))?;
            if (now.st_dev, now.st_ino) != (old.st_dev, old.st_ino) {
                return Err(NativeError::Foreign);
            }
        }
        spec.proof.check_target(&spec.target)?;
        deadline.check()?;
        // Never process_group: setsid must exec pkexec in place; --wait preserves status if it forks.
        let mut child = native(command.spawn())?;
        let out = child.stdout.take();
        let err = child.stderr.take();
        let pipes = (|| {
            let out = out.ok_or(NativeError::Unavailable)?;
            let err = err.ok_or(NativeError::Unavailable)?;
            native(rfs::fcntl_setfl(&out, OFlags::NONBLOCK))?;
            native(rfs::fcntl_setfl(&err, OFlags::NONBLOCK))?;
            Ok((
                PkexecPipe {
                    reader: Box::new(out),
                    bytes: Vec::new(),
                    truncated: false,
                },
                PkexecPipe {
                    reader: Box::new(err),
                    bytes: Vec::new(),
                    truncated: false,
                },
            ))
        })();
        let (out, err, setup_error) = match pipes {
            Ok((out, err)) => (out, err, None),
            Err(error) => (PkexecPipe::empty(), PkexecPipe::empty(), Some(error)),
        };
        Ok(Box::new(NativePkexecChild {
            child,
            out,
            err,
            status: None,
            setup_error,
        }))
    }
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
struct ManagerChild<T: Cleanup> {
    child: T,
    _lease: Option<Arc<LeaseGuard>>,
    _cleanup: Option<Arc<cleanup::Binding>>,
}
impl<T: Cleanup> Cleanup for ManagerChild<T> {
    fn terminate(&mut self) {
        self.child.terminate();
    }
    fn reaped(&mut self) -> bool {
        self.child.reaped()
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
    fn tracks_submission(&self) -> bool {
        true
    }
    fn run(&self, spec: &CommandSpec, deadline: &Deadline) -> Result<CommandOutput> {
        // LinuxNativeIo's private executor already owns bounded admission for this whole operation.
        if let Some(attempt) = &spec.spawn_attempt {
            attempt.store(false, Ordering::Release);
        }
        system_command(spec, deadline)
    }
}
fn child_environment(command: &mut Command, environment: &ChildEnvironment) {
    command.env_clear().envs(&environment.values);
}
fn system_command(spec: &CommandSpec, deadline: &Deadline) -> Result<CommandOutput> {
    use std::os::unix::process::CommandExt;
    deadline.check()?;
    let (target, proof) = spec.admission.as_ref().ok_or(NativeError::Unsupported)?;
    let cleanup = cleanup_admission::<ManagerChild<Child>>(&PROCESS_CLEANUPS)?;
    let executable = if let Some(agent) = &spec.agent {
        agent.revalidate(target, deadline)?
    } else {
        executable_fd(approved_executable(&spec.executable, &spec.argv)?)?
    };
    validate_target(target).map_err(|e| {
        if spec.environment.manager.is_some() {
            NativeError::Foreign
        } else {
            e
        }
    })?;
    if let Some(bus) = &spec.environment.bus {
        bus.revalidate(target).map_err(|e| {
            if spec.environment.manager.is_some() {
                NativeError::Foreign
            } else {
                e
            }
        })?;
    }
    if let Some(manager) = &spec.environment.manager {
        manager.revalidate(target)?;
    }
    if let Some(proof) = proof {
        proof.check_target(target)?;
    }
    deadline.check()?;
    let mut command = Command::new(format!("/proc/self/fd/{}", executable.as_raw_fd()));
    child_environment(&mut command, &spec.environment);
    cleanup_dispatch_check(target, spec, deadline)?;
    let mut child = native(
        command
            .arg0(&spec.executable)
            .args(&spec.argv)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn(),
    )?;
    if let Some(attempt) = &spec.spawn_attempt {
        attempt.store(true, Ordering::Release);
    }
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
        let _ = cleanup.try_send(ManagerChild {
            child,
            _lease: spec.lease.clone(),
            _cleanup: spec.cleanup.clone(),
        });
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

fn cleanup_dispatch_check(target: &LinuxTarget, spec: &CommandSpec, d: &Deadline) -> Result<()> {
    if let Some(cleanup) = &spec.cleanup {
        cleanup.check(d)?;
        if let Some(manager) = &spec.environment.manager {
            manager.revalidate(target)?;
        }
        d.check()?;
    }
    Ok(())
}
enum FileOperation<'a> {
    Mkdir(&'a OwnedFd, &'a std::ffi::OsStr),
    Write(&'a mut File, &'a [u8]),
    FileSync(&'a File),
    Rename(&'a OwnedFd, &'a str, &'a str),
    ParentSync(&'a OwnedFd),
    IntentRename(&'a OwnedFd, &'a str, &'a str),
}
trait FileIo: Send + Sync {
    fn apply(&self, operation: FileOperation<'_>) -> Result<()>;
}
struct NativeFiles;
impl FileIo for NativeFiles {
    fn apply(&self, operation: FileOperation<'_>) -> Result<()> {
        match operation {
            FileOperation::IntentRename(dir, from, to) => {
                rfs::renameat_with(dir, from, dir, to, rfs::RenameFlags::NOREPLACE)
                    .map_err(|_| NativeError::OutcomeUnknown)
            }
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
/// Authenticated context equality only; descriptor freshness is checked separately.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct TargetBinding {
    nonce: u64,
    paths: TargetPaths,
    runtime: PathBuf,
}
impl std::fmt::Debug for TargetBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TargetBinding(..)")
    }
}
pub struct LinuxNativeIo {
    pkexec_runner: Option<Arc<dyn PkexecRunner>>,
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
    #[cfg(test)]
    read_interleave: Option<Arc<dyn Fn() + Send + Sync>>,
}
impl std::fmt::Debug for LinuxNativeIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LinuxNativeIo { .. }")
    }
}
impl LinuxNativeIo {
    pub(crate) fn target_binding(&self) -> TargetBinding {
        TargetBinding {
            nonce: self.target.nonce,
            paths: self.target.paths.clone(),
            runtime: self.target.runtime.clone(),
        }
    }
    /// A bounded original-watch check, not a new watch or clean-exit authority.
    pub(crate) fn revalidate_original_running(
        self: &Arc<Self>,
        watch: &ProcessWatch,
        original: &BootstrapV1,
        deadline: &Deadline,
    ) -> Result<()> {
        let (io, watch, original, d) = (
            self.clone(),
            watch.clone(),
            original.clone(),
            deadline.clone(),
        );
        bounded_launch(&READ_WORKERS, deadline, move || {
            if watch.observe(&io, &d)? != ProcessExit::Running {
                return Err(NativeError::Foreign);
            }
            let (current, process) = io.bootstrap(&d)?;
            if current.instance_id != original.instance_id
                || current.pid != original.pid
                || current.started_unix_ms != original.started_unix_ms
                || current.runtime_dir != original.runtime_dir
                || current.phase_seq < original.phase_seq
                || current.phase != crate::agent_contract::BootstrapPhase::Ready
                || process != *watch.original()
                || watch.observe(&io, &d)? != ProcessExit::Running
            {
                return Err(NativeError::Foreign);
            }
            d.check()
        })
    }
    #[cfg(test)]
    pub(crate) fn second_scratch_context(&self, proof: &SupportProof) -> Arc<Self> {
        assert!(self.target.scratch);
        proof.check(self).unwrap();
        let root = self
            .walk_dir(&self.target.paths.home, false)
            .unwrap()
            .unwrap();
        rfs::mkdirat(&root, "second-context", Mode::RWXU).unwrap();
        let context = self.target.paths.home.join("second-context");
        let mut paths = self.target.paths.clone();
        paths.config_home = context.join("config");
        paths.state_home = context.join("state");
        paths.data_home = context.join("data");
        paths.runtime_home = context.join("run");
        paths.runtime_override = None;
        let io = Arc::new(Self {
            target: LinuxTarget::make(paths, true).unwrap(),
            runner: self.runner.clone(),
            probe: self.probe.clone(),
            files: self.files.clone(),
            pkexec_runner: None,
            command_admission: None,
            command_workers: self.command_workers,
            socket_uid: std::sync::Mutex::new(None),
            peer_uid: std::sync::Mutex::new(None),
            read_interleave: None,
        });
        let facts = &proof.facts;
        let proof = io
            .scratch_support(SupportObservations {
                uid: facts.uid,
                architecture: String::new(),
                arch_based: false,
                hyprland_version: [0; 3],
                protocols_ready: false,
                runtime_libraries_ready: false,
                uwsm_managed: facts.uwsm_managed,
                graphical_target_active: facts.graphical_target_active,
                graphical_sessions: facts.graphical_sessions,
                session_id: facts.session_id.clone(),
                session_type: facts.session_type.clone(),
                seat: facts.seat.clone(),
                active: facts.active,
            })
            .unwrap();
        for path in [
            io.target.paths.config_home.join("crosspane"),
            io.target.paths.state_home.join("crosspane/installer"),
            io.target.paths.data_home.join("crosspane"),
            io.target.runtime.clone(),
        ] {
            io.create_private_dir(&proof, &path).unwrap();
        }
        io.validate_target().unwrap();
        io
    }
    /// Root-owned, no-link reads in fixed namespaces. Scratch targets cannot probe the host.
    /// Font candidates stay within /usr/share/fonts and reject every link in the original path.
    /// ELF data is a bounded prefix only.
    pub fn read_system(&self, request: SystemRead, deadline: &Deadline) -> Result<SystemBytes> {
        request.path_and_limit()?;
        if self.target.scratch {
            return Err(NativeError::Foreign);
        }
        let target = self.target.clone();
        let worker_deadline = deadline.clone();
        bounded_launch(&READ_WORKERS, deadline, move || {
            validate_target(&target)?;
            let (mut path, _) = request.path_and_limit()?;
            let relative = path.strip_prefix("/").map_err(|_| NativeError::Foreign)?;
            let ufw = matches!(
                request,
                SystemRead::UfwConfig | SystemRead::UfwRules | SystemRead::UfwRules6
            );
            let mut parent =
                read_parent_permissions(read_anchor(Path::new("/"))?, relative, 0, ufw)?;
            let chain = if let SystemRead::Library(name) = &request {
                match library_chain(&mut parent, &worker_deadline, std::convert::identity) {
                    Err(NativeError::Unavailable) => {
                        path = Path::new("/usr/lib64").join(name);
                        parent = read_parent(
                            read_anchor(Path::new("/"))?,
                            path.strip_prefix("/").map_err(|_| NativeError::Foreign)?,
                            0,
                        )?;
                        library_chain(&mut parent, &worker_deadline, std::convert::identity)?
                    }
                    result => result?,
                }
            } else {
                Vec::new()
            };
            let fd = parent.open_with_permissions(ufw)?;
            let before = native(rfs::fstat(&fd))?;
            if chain.last().is_some_and(|entry| {
                (entry.stat.st_dev, entry.stat.st_ino) != (before.st_dev, before.st_ino)
            }) {
                return Err(NativeError::Foreign);
            }
            let mut file = File::from(fd);
            let bytes = system_contents(&mut file, &before, &request, &worker_deadline)?;
            let relative = path.strip_prefix("/").map_err(|_| NativeError::Foreign)?;
            let current = read_parent_permissions(read_anchor(Path::new("/"))?, relative, 0, ufw)?;
            parent.same_as(&current)?;
            library_revalidate(&current, &chain, &worker_deadline, std::convert::identity)?;
            let after = final_read_stat(&current, &parent.name, ufw)?;
            read_stat(&after, 0, 0o100000)?;
            if (
                before.st_dev,
                before.st_ino,
                before.st_size,
                before.st_mtime,
                before.st_mtime_nsec,
            ) != (
                after.st_dev,
                after.st_ino,
                after.st_size,
                after.st_mtime,
                after.st_mtime_nsec,
            ) {
                return Err(NativeError::Foreign);
            }
            worker_deadline.check()?;
            validate_target(&target)?;
            if !chain.is_empty() {
                path.set_file_name(&parent.name);
            }
            Ok(SystemBytes {
                path,
                bytes,
                file_size: u64::try_from(before.st_size).map_err(|_| NativeError::Invalid)?,
            })
        })
    }
    fn connect_read_endpoint(
        &self,
        environment: Option<&ChildEnvironment>,
        kind: ReadSocket,
        deadline: &Deadline,
    ) -> Result<UnixStream> {
        if matches!(kind, ReadSocket::SystemBus) && self.target.scratch {
            return Err(NativeError::Foreign);
        }
        socket_location(&self.target, environment, kind)?;
        let target = self.target.clone();
        let environment = environment.cloned();
        let worker_deadline = deadline.clone();
        #[cfg(test)]
        let interleave = self.read_interleave.clone();
        bounded_launch(&READ_WORKERS, deadline, move || {
            validate_target(&target)?;
            if let Some(bus) = environment.as_ref().and_then(|e| e.bus.as_ref()) {
                bus.revalidate(&target)?;
            }
            let (path, uid) = socket_location(&target, environment.as_ref(), kind)?;
            let parent = socket_parent(&target, &path, uid)?;
            let public_bus = matches!(kind, ReadSocket::SessionBus | ReadSocket::SystemBus);
            let before = socket_observation(&parent, uid, public_bus)?;
            let stream = connect_read_socket(&parent, uid, &worker_deadline)?;
            #[cfg(test)]
            if let Some(interleave) = interleave {
                interleave();
            }
            let current = socket_parent(&target, &path, uid)?;
            parent.same_as(&current)?;
            let after = socket_observation(&current, uid, public_bus)?;
            if (before.st_dev, before.st_ino) != (after.st_dev, after.st_ino) {
                return Err(NativeError::Foreign);
            }
            if let Some(bus) = environment.as_ref().and_then(|e| e.bus.as_ref()) {
                bus.revalidate(&target)?;
            }
            worker_deadline.check()?;
            validate_target(&target)?;
            Ok(stream)
        })
    }
    /// Connected nonblocking stream, bound to the environment's admitted bus and target.
    /// No protocol bytes are sent; callers own bounded client I/O and must not reconnect by path.
    pub fn connect_session_bus(
        &self,
        environment: &ChildEnvironment,
        deadline: &Deadline,
    ) -> Result<UnixStream> {
        self.connect_read_endpoint(Some(environment), ReadSocket::SessionBus, deadline)
    }
    /// Every ancestor is root-owned/nonwritable; the root-owned socket may be 0666. Peer UID is 0.
    pub fn connect_system_bus(&self, deadline: &Deadline) -> Result<UnixStream> {
        self.connect_read_endpoint(None, ReadSocket::SystemBus, deadline)
    }
    pub fn connect_wayland(
        &self,
        environment: &ChildEnvironment,
        deadline: &Deadline,
    ) -> Result<UnixStream> {
        self.connect_read_endpoint(Some(environment), ReadSocket::Wayland, deadline)
    }
    pub fn connect_hyprland(
        &self,
        environment: &ChildEnvironment,
        deadline: &Deadline,
    ) -> Result<UnixStream> {
        self.connect_read_endpoint(Some(environment), ReadSocket::Hyprland, deadline)
    }
    /// Production construction has no fake runner, probe or scratch-proof override.
    pub fn selected(paths: TargetPaths) -> Result<Self> {
        let io = Self {
            pkexec_runner: None,
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
            #[cfg(test)]
            read_interleave: None,
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
            pkexec_runner: None,
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
            #[cfg(test)]
            read_interleave: None,
        })
    }
    /// Fake launch is allowed only for this newly-created scratch target.
    pub fn set_scratch_pkexec_runner(&mut self, runner: Arc<dyn PkexecRunner>) -> Result<()> {
        if !self.target.scratch {
            return Err(NativeError::Foreign);
        }
        self.pkexec_runner = Some(runner);
        Ok(())
    }
    /// Single dispatch; admission, launch and reap all retain the process-wide lease.
    /// A stalled spawn cannot make the caller wait or free admission for a second mutation.
    pub fn pkexec_ufw(
        &self,
        proof: &SupportProof,
        mutation: UfwMutation,
        deadline: &Deadline,
    ) -> Result<PkexecOutcome> {
        proof.check(self)?;
        deadline.check()?;
        if deadline.end.saturating_duration_since(Instant::now())
            > Duration::from_millis(MAX_NATIVE_TIMEOUT_MS)
        {
            return Err(NativeError::Invalid);
        }
        let runner = match &self.pkexec_runner {
            Some(runner) => runner.clone(),
            None if self.target.scratch => return Err(NativeError::Foreign),
            None => Arc::new(NativePkexecRunner),
        };
        UFW_DISPATCHES
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n == 0).then_some(1)
            })
            .map_err(|_| NativeError::Busy)?;
        let slot = CleanupSlot(&UFW_DISPATCHES);
        let (port, comment) = match mutation.rule {
            UfwRule::Lan => ("47811:47812", "Crosspane (LAN)"),
            UfwRule::Mdns => ("5353", "Crosspane (mDNS)"),
        };
        let mut argv: Vec<String> = ["--wait", "/usr/bin/pkexec", "/usr/bin/ufw"]
            .map(str::to_owned)
            .into();
        if mutation.delete {
            argv.push("delete".into());
        }
        argv.extend(
            [
                "allow",
                "from",
                mutation.cidr.as_str(),
                "to",
                "any",
                "port",
                port,
                "proto",
                "udp",
                "comment",
                comment,
            ]
            .map(str::to_owned),
        );
        let spec = PkexecCommand {
            argv,
            target: self.target.clone(),
            proof: proof.clone(),
        };
        let worker_deadline = deadline.clone();
        let (send, receive) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("installer-ufw".into())
            .spawn(move || {
                let lease = slot;
                let mut child = None;
                let mut result = (|| {
                    worker_deadline.check()?;
                    validate_target(&spec.target)?;
                    spec.proof.check_target(&spec.target)?;
                    child = Some(runner.spawn(&spec, &worker_deadline)?);
                    loop {
                        worker_deadline.check()?;
                        if let Some(outcome) =
                            child.as_mut().ok_or(NativeError::Unavailable)?.poll()?
                        {
                            return Ok(outcome);
                        }
                        thread::sleep(Duration::from_millis(2));
                    }
                })();
                if result.as_ref().is_err_and(|e| *e == NativeError::Timeout) {
                    result = Ok(PkexecOutcome::TimedOut);
                }
                if let Ok(PkexecOutcome::Exited {
                    stdout,
                    stderr,
                    stdout_truncated,
                    stderr_truncated,
                    ..
                }) = &mut result
                {
                    *stdout_truncated |= stdout.len() > MAX_COMMAND_BYTES;
                    *stderr_truncated |= stderr.len() > MAX_COMMAND_BYTES;
                    stdout.truncate(MAX_COMMAND_BYTES);
                    stderr.truncate(MAX_COMMAND_BYTES);
                }
                let cleanup = result.is_err() || matches!(&result, Ok(PkexecOutcome::TimedOut));
                if cleanup && let Some(child) = &mut child {
                    child.terminate();
                }
                let _ = send.send(result);
                if cleanup && let Some(child) = &mut child {
                    while !child.reaped() {
                        let _ = child.poll();
                        thread::sleep(Duration::from_millis(10));
                    }
                }
                drop(lease); // Release only after reap, before the child's destructor can publish completion.
            })
            .map_err(|_| NativeError::Unavailable)?;
        loop {
            match deadline.check() {
                Err(NativeError::Timeout) => return Ok(PkexecOutcome::TimedOut),
                Err(error) => return Err(error),
                Ok(()) => {}
            }
            match receive.recv_timeout(Duration::from_millis(2)) {
                Ok(result) if deadline.check().is_ok() => return result,
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(NativeError::Unavailable),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
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
    /// Read-only absence for diagnose, including a missing parent. Present entries (even
    /// dangling symlinks) are never absence; every existing ancestor uses the safe fd walk.
    pub(crate) fn path_is_absent(&self, path: &Path) -> Result<bool> {
        self.validate_target()?;
        if !clean(path)
            || !(path.starts_with(&self.target.paths.home)
                || path.starts_with(&self.target.paths.runtime_home))
        {
            return Err(NativeError::Foreign);
        }
        let parent = path.parent().ok_or(NativeError::Invalid)?;
        let name = path.file_name().ok_or(NativeError::Invalid)?;
        let Some(dir) = self.walk_dir(parent, false)? else {
            return Ok(true);
        };
        match rfs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => Ok(false),
            Err(rustix::io::Errno::NOENT) => Ok(true),
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
        if manager_mutation(spec) || spec.agent.is_some() {
            return Err(NativeError::Unsupported);
        }
        self.execute(spec, deadline, None)
    }
    /// Bounded admission for the user manager and optional selected session bus. No inherited
    /// SYSTEMD_* or system-bus variable enters the explicit child environment.
    pub fn manager_environment(
        &self,
        session: BTreeMap<String, String>,
        deadline: &Deadline,
    ) -> Result<ChildEnvironment> {
        if session.keys().any(|k| k != "DBUS_SESSION_BUS_ADDRESS") {
            return Err(NativeError::Invalid);
        }
        let target = self.target.clone();
        let worker_deadline = deadline.clone();
        bounded_launch(&PROCESS_LAUNCHES, deadline, move || {
            validate_target(&target)?;
            let mut environment = ChildEnvironment::selected(&target, session)?;
            environment.manager = Some(ManagerEndpoint::admit(&target)?);
            environment.values.remove("DBUS_SYSTEM_BUS_ADDRESS");
            worker_deadline.check()?;
            Ok(environment)
        })
    }
    fn execute(
        &self,
        spec: &CommandSpec,
        deadline: &Deadline,
        proof: Option<&SupportProof>,
    ) -> Result<CommandOutput> {
        deadline.check()?;
        let mut admitted = spec.clone();
        admitted.lease = spec.lease.clone();
        admitted.admission = Some((self.target.clone(), proof.cloned()));
        let target = self.target.clone();
        let runner = self.runner.clone();
        let worker_deadline = deadline.clone();
        let mutation = manager_mutation(spec) || spec.agent.is_some();
        let started = spec
            .spawn_attempt
            .clone()
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        let worker_started = started.clone();
        admitted.spawn_attempt = Some(worker_started.clone());
        #[cfg(test)]
        let admission = self
            .command_admission
            .clone()
            .unwrap_or_else(|| Arc::new(validate_target));
        #[cfg(test)]
        let counter = self.command_workers;
        #[cfg(not(test))]
        let counter = &PROCESS_LAUNCHES;
        let result = bounded_launch(counter, deadline, move || {
            worker_deadline.check()?;
            #[cfg(test)]
            let admitted_target = admission(&target);
            #[cfg(not(test))]
            let admitted_target = validate_target(&target);
            admitted_target.map_err(|e| {
                if admitted.environment.manager.is_some() {
                    NativeError::Foreign
                } else {
                    e
                }
            })?;
            let mut expected = ChildEnvironment::selected(&target, BTreeMap::new())?;
            if admitted.environment.manager.is_some() {
                expected.values.remove("DBUS_SYSTEM_BUS_ADDRESS");
            }
            if admitted.environment.nonce != target.nonce
                || expected
                    .values
                    .iter()
                    .any(|(key, value)| admitted.environment.values.get(key) != Some(value))
            {
                return Err(NativeError::Foreign);
            }
            if let Some(bus) = &admitted.environment.bus {
                bus.revalidate(&target).map_err(|e| {
                    if admitted.environment.manager.is_some() {
                        NativeError::Foreign
                    } else {
                        e
                    }
                })?;
            }
            if let Some(manager) = &admitted.environment.manager {
                manager.revalidate(&target)?;
            }
            worker_deadline.check()?;
            if let Some((_, Some(proof))) = &admitted.admission {
                proof.check_target(&target)?;
            }
            if let Some(agent) = &admitted.agent {
                agent.revalidate(&target, &worker_deadline)?;
                worker_deadline.check()?;
                if let Some((_, Some(proof))) = &admitted.admission {
                    proof.check_target(&target)?;
                }
            }
            cleanup_dispatch_check(&target, &admitted, &worker_deadline)?;
            if !runner.tracks_submission() {
                worker_started.store(true, Ordering::Release);
            }
            let result = runner.run(&admitted, &worker_deadline);
            let drift = if let Some(manager) = &admitted.environment.manager {
                manager.revalidate(&target).and_then(|_| {
                    admitted
                        .environment
                        .bus
                        .as_ref()
                        .map(|b| b.revalidate(&target))
                        .transpose()
                })
            } else {
                Ok(None)
            };
            if drift.is_err() {
                return Err(if mutation && worker_started.load(Ordering::Acquire) {
                    NativeError::OutcomeUnknown
                } else {
                    NativeError::Foreign
                });
            }
            worker_deadline.check()?;
            let result = result?;
            if result.stdout.len() + result.stderr.len() > admitted.output_limit {
                return Err(NativeError::Oversize);
            }
            Ok(result)
        });
        result.map_err(|error| {
            if mutation
                && started.load(Ordering::Acquire)
                && matches!(
                    error,
                    NativeError::Timeout
                        | NativeError::Cancelled
                        | NativeError::Unavailable
                        | NativeError::Oversize
                )
            {
                NativeError::OutcomeUnknown
            } else {
                error
            }
        })
    }
    pub fn run_mutation(
        &self,
        proof: &SupportProof,
        spec: &CommandSpec,
        deadline: &Deadline,
    ) -> Result<CommandOutput> {
        // Manager mutations require the worker-owned lease and pending-operation route.
        if manager_mutation(spec) {
            return Err(NativeError::Unsupported);
        }
        proof.check(self)?;
        self.execute(spec, deadline, Some(proof))
    }
    pub fn install_lease(&self, proof: &SupportProof) -> Result<InstallLease> {
        self.lock(
            proof,
            &self
                .target
                .paths
                .state_home
                .join("crosspane/installer/install.lock"),
        )
        .map(|lock| InstallLease {
            lock,
            nonce: self.target.nonce,
        })
    }
    /// The worker and, when needed, its cleanup thread retain the lease until the child is
    /// reaped. Dropping a pending handle cannot release it; process exit releases flock normally.
    pub fn run_manager_mutation(
        &self,
        proof: &SupportProof,
        spec: &CommandSpec,
        lease: InstallLease,
        deadline: &Deadline,
    ) -> ManagerMutation {
        if !manager_mutation(spec) || lease.nonce != self.target.nonce {
            return ManagerMutation {
                result: Err(NativeError::Invalid),
                pending: None,
                submitted: false,
            };
        }
        let pending = PendingOperation(Arc::new(AtomicBool::new(false)));
        let mut command = spec.clone();
        let submitted = Arc::new(AtomicBool::new(false));
        command.spawn_attempt = Some(submitted.clone());
        command.lease = Some(Arc::new(LeaseGuard {
            lease: Some(lease),
            finished: pending.0.clone(),
        }));
        let result = proof
            .check(self)
            .and_then(|()| self.execute(&command, deadline, Some(proof)));
        drop(command);
        if pending.completed() {
            ManagerMutation {
                result,
                pending: None,
                submitted: submitted.load(Ordering::Acquire),
            }
        } else {
            ManagerMutation {
                result: Err(NativeError::OutcomeUnknown),
                pending: Some(pending),
                submitted: submitted.load(Ordering::Acquire),
            }
        }
    }
    pub fn process_identity(&self, pid: u32, deadline: &Deadline) -> Result<ProcessIdentity> {
        self.process_identity_at(pid, deadline, &self.target.agent_path())
    }
    fn process_identity_at(
        &self,
        pid: u32,
        deadline: &Deadline,
        expected_executable: &Path,
    ) -> Result<ProcessIdentity> {
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
        if before.uid != self.target.paths.uid || before.executable != expected_executable {
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
        self.bootstrap_at(deadline, &self.target.agent_path())
    }
    /// Only restart planning/apply use this after admitting this run's replacement journal.
    pub(super) fn replaced_bootstrap(
        &self,
        deadline: &Deadline,
        previous_instance: u64,
    ) -> Result<(BootstrapV1, ProcessIdentity)> {
        let mut path = self.target.agent_path().into_os_string();
        path.push(" (deleted)");
        let result = self.bootstrap_at(deadline, &PathBuf::from(path))?;
        if result.0.instance_id != previous_instance {
            return Err(NativeError::Foreign);
        }
        Ok(result)
    }
    fn bootstrap_at(
        &self,
        deadline: &Deadline,
        expected_executable: &Path,
    ) -> Result<(BootstrapV1, ProcessIdentity)> {
        let path = self.target.runtime.join("bootstrap.json");
        let first =
            parse_bootstrap(&self.read(&path, 4096, true)?).map_err(|_| NativeError::Invalid)?;
        let identity = self.process_identity_at(first.pid, deadline, expected_executable)?;
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
        if identity != self.process_identity_at(first.pid, deadline, expected_executable)? {
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
    use std::os::unix::fs::{PermissionsExt, symlink};

    /// The scratch anchor substitutes for /; only root-ownership evidence is injected.
    #[test]
    fn ufw_reads_are_fixed_bounded_protected_and_permission_denial_is_distinct() {
        for (request, literal) in [
            (SystemRead::UfwConfig, "/etc/ufw/ufw.conf"),
            (SystemRead::UfwRules, "/etc/ufw/user.rules"),
            (SystemRead::UfwRules6, "/etc/ufw/user6.rules"),
        ] {
            let (io, root) = fixture();
            let relative = Path::new(literal).strip_prefix("/").unwrap();
            let directory = root.join("etc/ufw");
            fs::create_dir_all(&directory).unwrap();
            let path = root.join(relative);
            fs::write(&path, b"inert rules fixture").unwrap();
            assert_eq!(
                request.path_and_limit().unwrap(),
                (literal.into(), MAX_UFW_BYTES)
            );
            let open = || {
                read_parent_permissions(
                    read_anchor(&root).unwrap(),
                    relative,
                    io.target.paths.uid,
                    true,
                )
            };
            let parent = open().unwrap();
            let mut file = File::from(parent.open_with_permissions(true).unwrap());
            let mut stat = rfs::fstat(&file).unwrap();
            stat.st_uid = 0;
            assert_eq!(
                system_contents(&mut file, &stat, &request, &read_deadline()).unwrap(),
                b"inert rules fixture"
            );
            assert!(matches!(
                read_parent_permissions(
                    read_anchor(&root).unwrap(),
                    relative,
                    io.target.paths.uid + 1,
                    true
                ),
                Err(NativeError::Foreign)
            ));
            for mode in [0o100666, 0o100664, 0o040755, 0o120777] {
                let mut foreign = stat;
                foreign.st_mode = mode;
                assert_eq!(
                    system_contents(&mut file, &foreign, &request, &read_deadline()),
                    Err(NativeError::Foreign)
                );
            }
            stat.st_uid = 1;
            assert_eq!(
                system_contents(&mut file, &stat, &request, &read_deadline()),
                Err(NativeError::Foreign)
            );
            stat.st_uid = 0;
            for link in [root.join("etc"), directory.clone(), path.clone()] {
                let saved = root.join("saved");
                fs::rename(&link, &saved).unwrap();
                symlink(&saved, &link).unwrap();
                assert!(
                    open().and_then(|p| p.open_with_permissions(true)).is_err(),
                    "link {link:?}"
                );
                fs::remove_file(&link).unwrap();
                fs::rename(saved, link).unwrap();
            }
            for ancestor in [root.clone(), root.join("etc"), directory.clone()] {
                fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o777)).unwrap();
                assert!(matches!(open(), Err(NativeError::Foreign)));
                fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o700)).unwrap();
            }
            for denied in [directory.clone(), path.clone()] {
                fs::set_permissions(&denied, fs::Permissions::from_mode(0o0)).unwrap();
                assert!(matches!(
                    open().and_then(|p| p.open_with_permissions(true)),
                    Err(NativeError::PermissionDenied)
                ));
                fs::set_permissions(
                    &denied,
                    fs::Permissions::from_mode(if denied == path { 0o600 } else { 0o700 }),
                )
                .unwrap();
            }
            for size in [MAX_UFW_BYTES, MAX_UFW_BYTES + 1] {
                File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_len(size as u64)
                    .unwrap();
                let mut file = File::from(open().unwrap().open_with_permissions(true).unwrap());
                let mut stat = rfs::fstat(&file).unwrap();
                stat.st_uid = 0;
                let result = system_contents(&mut file, &stat, &request, &read_deadline());
                if size == MAX_UFW_BYTES {
                    assert_eq!(result.unwrap().len(), size);
                } else {
                    assert_eq!(result, Err(NativeError::Oversize));
                }
            }
            assert_eq!(
                read_error(rustix::io::Errno::ACCESS, true),
                NativeError::PermissionDenied
            );
            assert_eq!(
                read_error(rustix::io::Errno::PERM, true),
                NativeError::PermissionDenied
            );
            assert_eq!(
                read_error(rustix::io::Errno::ACCESS, false),
                NativeError::Foreign
            );
            assert!(
                matches!(
                    io.read_system(request, &read_deadline()),
                    Err(NativeError::Foreign)
                ),
                "scratch may never probe /etc/ufw"
            );
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn ufw_programs_require_exact_setuid_role_owner_mode_and_unlinked_ancestry() {
        for program in ["setsid", "pkexec", "ufw"] {
            let (io, root) = fixture();
            fs::create_dir_all(root.join("usr/bin")).unwrap();
            let path = root.join("usr/bin").join(program);
            fs::write(&path, b"inert executable fixture, never run").unwrap();
            let relative = Path::new("usr/bin").join(program);
            let open = || {
                read_parent(read_anchor(&root).unwrap(), &relative, io.target.paths.uid)
                    .and_then(|p| p.open_file())
            };
            let fd = open().unwrap();
            let mut stat = rfs::fstat(&fd).unwrap();
            stat.st_uid = 0;
            stat.st_mode = if program == "pkexec" {
                0o104755
            } else {
                0o100755
            };
            assert_eq!(ufw_program_stat(&stat, program == "pkexec"), Ok(()));
            for uid in [1, io.target.paths.uid] {
                let mut bad = stat;
                bad.st_uid = uid;
                assert_eq!(
                    ufw_program_stat(&bad, program == "pkexec"),
                    Err(NativeError::Foreign)
                );
            }
            for mode in [0o104777, 0o104775, 0o106755, 0o100644, 0o040755, 0o120777] {
                let mut bad = stat;
                bad.st_mode = mode;
                assert_eq!(
                    ufw_program_stat(&bad, program == "pkexec"),
                    Err(NativeError::Foreign)
                );
            }
            stat.st_mode = if program == "pkexec" {
                0o100755
            } else {
                0o104755
            };
            assert_eq!(
                ufw_program_stat(&stat, program == "pkexec"),
                Err(NativeError::Foreign)
            );
            for link in [root.join("usr"), root.join("usr/bin"), path] {
                let saved = root.join("saved");
                fs::rename(&link, &saved).unwrap();
                symlink(&saved, &link).unwrap();
                assert!(matches!(open(), Err(NativeError::Foreign)));
                fs::remove_file(&link).unwrap();
                fs::rename(saved, link).unwrap();
            }
            fs::set_permissions(root.join("usr/bin"), fs::Permissions::from_mode(0o777)).unwrap();
            assert!(matches!(open(), Err(NativeError::Foreign)));
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn ufw_final_revalidation_preserves_denial_without_changing_other_reads() {
        let (io, root) = fixture();
        let path = root.join("etc/ufw");
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("user.rules"), b"inert fixture").unwrap();
        let parent = read_parent_permissions(
            read_anchor(&root).unwrap(),
            Path::new("etc/ufw/user.rules"),
            io.target.paths.uid,
            true,
        )
        .unwrap();
        assert!(final_read_stat(&parent, "user.rules", true).is_ok());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        // Readable but no search permission: reopening the protected parent succeeds, statat fails.
        let current = read_parent_permissions(
            read_anchor(&root).unwrap(),
            Path::new("etc/ufw/user.rules"),
            io.target.paths.uid,
            true,
        )
        .unwrap();
        parent.same_as(&current).unwrap();
        let denied = final_read_stat(&current, "user.rules", true);
        let other = final_read_stat(&current, "user.rules", false);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_dir_all(root).unwrap();
        assert_eq!(denied.map(|_| ()), Err(NativeError::PermissionDenied));
        assert_eq!(other.map(|_| ()), Err(NativeError::Unavailable));
    }

    #[test]
    fn pkexec_pipe_caps_each_stream_and_bounds_work_even_for_endless_output() {
        for size in [
            0,
            MAX_COMMAND_BYTES,
            MAX_COMMAND_BYTES + 1,
            MAX_COMMAND_BYTES * 3,
        ] {
            let mut pipe = PkexecPipe {
                reader: Box::new(std::io::Cursor::new(vec![b'x'; size])),
                bytes: Vec::new(),
                truncated: false,
            };
            while !pipe.drain().unwrap() {}
            assert_eq!(pipe.bytes.len(), size.min(MAX_COMMAND_BYTES));
            assert_eq!(pipe.truncated, size > MAX_COMMAND_BYTES);
        }
        struct Endless;
        impl Read for Endless {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                buffer.fill(b'x');
                Ok(buffer.len())
            }
        }
        let mut pipe = PkexecPipe {
            reader: Box::new(Endless),
            bytes: Vec::new(),
            truncated: false,
        };
        assert!(!pipe.drain().unwrap());
        assert_eq!(pipe.bytes.len(), MAX_COMMAND_BYTES);
        assert!(!pipe.truncated);
        assert!(!pipe.drain().unwrap());
        assert!(pipe.truncated);
    }

    #[test]
    fn pkexec_rejects_an_injected_deadline_beyond_120_seconds_before_dispatch() {
        let (io, root) = fixture();
        let proof = io.scratch_support(observations(&io)).unwrap();
        let mutation = UfwMutation {
            delete: false,
            cidr: LanCidr::parse("10.0.0.0/8").unwrap(),
            rule: UfwRule::Lan,
        };
        let deadline = Deadline {
            end: Instant::now() + Duration::from_secs(121),
            cancellation: Cancellation::default(),
        };
        assert!(matches!(
            io.pkexec_ufw(&proof, mutation, &deadline),
            Err(NativeError::Invalid)
        ));
        fs::remove_dir_all(root).unwrap();
    }

    fn injected_library_snapshot(mut stat: rfs::Stat) -> rfs::Stat {
        stat.st_uid = 0;
        stat
    }

    #[test]
    fn library_link_swap_and_restore_reads_only_the_pinned_inode_target() {
        let (io, root) = fixture();
        fs::write(root.join("real1.so"), b"admitted original bytes").unwrap();
        fs::write(root.join("real2.so"), b"substituted bytes").unwrap();
        symlink("real1.so", root.join("link.so")).unwrap();
        let mut parent = read_parent(
            read_anchor(&root).unwrap(),
            Path::new("link.so"),
            io.target.paths.uid,
        )
        .unwrap();
        let mut observations = 0;
        let chain = library_chain(&mut parent, &read_deadline(), |stat| {
            observations += 1;
            if observations == 1 {
                // After link fstat, before target read: the basename temporarily names B.
                fs::rename(root.join("link.so"), root.join("saved.so")).unwrap();
                symlink("real2.so", root.join("link.so")).unwrap();
            } else if observations == 2 {
                // Restore A before the recorded chain is revalidated.
                fs::remove_file(root.join("link.so")).unwrap();
                fs::rename(root.join("saved.so"), root.join("link.so")).unwrap();
            }
            injected_library_snapshot(stat)
        })
        .unwrap();
        assert_eq!(observations, 2);
        assert_eq!(parent.name, "real1.so");
        let mut bytes = Vec::new();
        File::from(parent.open_file().unwrap())
            .read_to_end(&mut bytes)
            .unwrap();
        library_revalidate(&parent, &chain, &read_deadline(), injected_library_snapshot).unwrap();
        assert_eq!(bytes, b"admitted original bytes");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn library_same_directory_chains_pin_root_evidence_and_final_descriptor() {
        for hops in 1..=3 {
            let (io, root) = fixture();
            fs::create_dir(root.join("libraries")).unwrap();
            let mut bytes = vec![0; 64];
            bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
            fs::write(root.join("libraries/real.so"), &bytes).unwrap();
            for i in 0..hops {
                let target = if i + 1 == hops {
                    "real.so".into()
                } else {
                    format!("link{}.so", i + 1)
                };
                symlink(target, root.join(format!("libraries/link{i}.so"))).unwrap();
            }
            let mut parent = read_parent(
                read_anchor(&root).unwrap(),
                Path::new("libraries/link0.so"),
                io.target.paths.uid,
            )
            .unwrap();
            let chain =
                library_chain(&mut parent, &read_deadline(), injected_library_snapshot).unwrap();
            assert_eq!(chain.len(), hops + 1);
            assert_eq!(parent.name, "real.so");
            let fd = parent.open_file().unwrap();
            let mut stat = rfs::fstat(&fd).unwrap();
            stat.st_uid = 0; // root observations injected; the fixture remains ordinary-user owned
            assert_eq!(
                system_contents(
                    &mut File::from(fd),
                    &stat,
                    &SystemRead::Library("link0.so".into()),
                    &read_deadline()
                )
                .unwrap(),
                bytes
            );
            library_revalidate(&parent, &chain, &read_deadline(), injected_library_snapshot)
                .unwrap();
            // Replacement with the same target text must still invalidate the recorded link inode.
            fs::rename(
                root.join("libraries/link0.so"),
                root.join("libraries/old.so"),
            )
            .unwrap();
            symlink(
                if hops == 1 { "real.so" } else { "link1.so" },
                root.join("libraries/link0.so"),
            )
            .unwrap();
            assert_eq!(
                library_revalidate(&parent, &chain, &read_deadline(), injected_library_snapshot),
                Err(NativeError::Foreign)
            );
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn library_links_refuse_escape_four_hops_foreign_owner_and_cancelled_reads() {
        for target in [
            "nested/real.so",
            "../real.so",
            "/usr/lib/real.so",
            "..",
            "bad space",
        ] {
            let (io, root) = fixture();
            symlink(target, root.join("link.so")).unwrap();
            let mut parent = read_parent(
                read_anchor(&root).unwrap(),
                Path::new("link.so"),
                io.target.paths.uid,
            )
            .unwrap();
            assert!(matches!(
                library_chain(&mut parent, &read_deadline(), injected_library_snapshot),
                Err(NativeError::Foreign)
            ));
            fs::remove_dir_all(root).unwrap();
        }
        let (io, root) = fixture();
        fs::write(root.join("real.so"), b"inert").unwrap();
        for i in 0..4 {
            symlink(
                if i == 3 {
                    "real.so".into()
                } else {
                    format!("link{}.so", i + 1)
                },
                root.join(format!("link{i}.so")),
            )
            .unwrap();
        }
        let mut parent = read_parent(
            read_anchor(&root).unwrap(),
            Path::new("link0.so"),
            io.target.paths.uid,
        )
        .unwrap();
        assert!(matches!(
            library_chain(&mut parent, &read_deadline(), injected_library_snapshot),
            Err(NativeError::Foreign)
        ));
        parent.name = "link3.so".into();
        assert!(
            matches!(
                library_chain(&mut parent, &read_deadline(), std::convert::identity),
                Err(NativeError::Foreign)
            ),
            "ordinary-user link must not become root evidence"
        );
        let cancellation = Cancellation::default();
        let deadline = Deadline::new(1000, cancellation.clone()).unwrap();
        cancellation.cancel();
        assert!(matches!(
            library_chain(&mut parent, &deadline, injected_library_snapshot),
            Err(NativeError::Cancelled)
        ));
        for name in [
            "",
            ".",
            "..",
            "../lib.so",
            "/usr/lib/lib.so",
            "lib.so/other",
            "lib..so",
            &"x".repeat(256),
        ] {
            assert!(SystemRead::Library(name.into()).path_and_limit().is_err());
        }
        fs::remove_dir_all(root).unwrap();
    }

    fn read_deadline() -> Deadline {
        Deadline::new(1000, Cancellation::default()).unwrap()
    }

    #[test]
    fn system_read_namespaces_and_bounds_are_closed() {
        assert_eq!(
            SystemRead::OsRelease.path_and_limit().unwrap(),
            ("/etc/os-release".into(), MAX_OS_RELEASE_BYTES)
        );
        assert_eq!(
            SystemRead::OsReleaseFallback.path_and_limit().unwrap(),
            ("/usr/lib/os-release".into(), MAX_OS_RELEASE_BYTES)
        );
        for path in [
            "/usr/share/fonts/fixture.ttf",
            "/usr/share/fonts/nested/fixture.ttf",
        ] {
            assert_eq!(
                SystemRead::Font(path.into()).path_and_limit().unwrap().1,
                16 * 1024 * 1024
            );
        }
        for path in ["/usr/lib/fixture.so", "/usr/lib64/fixture.so"] {
            assert_eq!(
                SystemRead::ElfPrefix(path.into())
                    .path_and_limit()
                    .unwrap()
                    .1,
                64 * 1024 * 1024
            );
        }
        for path in [
            "/usr/share/fonts",
            "/usr/share/fonts-other/a",
            "/usr/share/fonts/../secret",
            "/home/foreign/font",
            "relative",
            "/usr/share/fonts/a\0",
            "/usr/share/fonts/a\n",
        ] {
            assert!(SystemRead::Font(path.into()).path_and_limit().is_err());
        }
        for path in [
            "/usr/lib",
            "/usr/lib64",
            "/usr/lib-other/a",
            "/usr/lib/../secret",
            "/etc/shadow",
        ] {
            assert!(SystemRead::ElfPrefix(path.into()).path_and_limit().is_err());
        }
        let (io, root) = fixture();
        assert_eq!(io.target.source(), ObservationSource::Demo);
        assert!(matches!(
            io.read_system(SystemRead::OsRelease, &read_deadline()),
            Err(NativeError::Foreign)
        ));
        assert!(matches!(
            io.connect_system_bus(&read_deadline()),
            Err(NativeError::Foreign)
        ));
        fs::remove_dir_all(root).unwrap();
    }

    fn open_fixture_font(root: &Path, candidate: &Path, uid: u32) -> Result<OwnedFd> {
        SystemRead::Font(candidate.into()).path_and_limit()?;
        read_parent(
            read_anchor(root)?,
            candidate
                .strip_prefix("/")
                .map_err(|_| NativeError::Foreign)?,
            uid,
        )?
        .open_file()
    }

    /// The fixture anchor substitutes for /; all descriptor operations are production helpers.
    #[test]
    fn font_final_link_within_allowed_root_is_refused() {
        let (io, root) = fixture();
        let directory = root.join("usr/share/fonts");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("real.ttf"), b"inert font fixture").unwrap();
        symlink("real.ttf", directory.join("alias.ttf")).unwrap();
        let candidate = Path::new("/usr/share/fonts/real.ttf");
        let fd = open_fixture_font(&root, candidate, io.target.paths.uid).unwrap();
        let mut stat = rfs::fstat(&fd).unwrap();
        stat.st_uid = 0; // injected root evidence; no elevation or host font inspection
        assert_eq!(
            system_contents(
                &mut File::from(fd),
                &stat,
                &SystemRead::Font(candidate.into()),
                &read_deadline(),
            )
            .unwrap(),
            b"inert font fixture"
        );
        assert!(matches!(
            open_fixture_font(
                &root,
                Path::new("/usr/share/fonts/alias.ttf"),
                io.target.paths.uid,
            ),
            Err(NativeError::Foreign)
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn font_intermediate_link_within_allowed_root_is_refused() {
        let (io, root) = fixture();
        let directory = root.join("usr/share/fonts");
        fs::create_dir_all(directory.join("real")).unwrap();
        fs::write(directory.join("real/font.ttf"), b"inert font fixture").unwrap();
        symlink("real", directory.join("alias")).unwrap();
        assert!(
            open_fixture_font(
                &root,
                Path::new("/usr/share/fonts/real/font.ttf"),
                io.target.paths.uid,
            )
            .is_ok()
        );
        assert!(matches!(
            open_fixture_font(
                &root,
                Path::new("/usr/share/fonts/alias/font.ttf"),
                io.target.paths.uid,
            ),
            Err(NativeError::Foreign)
        ));
        fs::remove_dir_all(root).unwrap();
    }

    /// Root ownership is injected into scratch-file metadata, never acquired by elevation.
    #[test]
    fn system_file_root_evidence_exact_limits_growth_and_elf_prefix_are_bounded() {
        let (_, root) = fixture();
        let path = root.join("inert");
        fs::write(&path, b"NAME=fixture\n").unwrap();
        let mut file = File::open(&path).unwrap();
        let mut stat = rfs::fstat(&file).unwrap();
        assert_eq!(
            system_contents(&mut file, &stat, &SystemRead::OsRelease, &read_deadline()),
            Err(NativeError::Foreign)
        );
        stat.st_uid = 0;
        assert_eq!(
            system_contents(&mut file, &stat, &SystemRead::OsRelease, &read_deadline()).unwrap(),
            b"NAME=fixture\n"
        );
        for (size, request) in [
            (MAX_OS_RELEASE_BYTES, SystemRead::OsRelease),
            (
                MAX_FONT_BYTES,
                SystemRead::Font("/usr/share/fonts/fixture.ttf".into()),
            ),
        ] {
            File::create(&path).unwrap().set_len(size as u64).unwrap();
            let mut file = File::open(&path).unwrap();
            let mut stat = rfs::fstat(&file).unwrap();
            stat.st_uid = 0;
            assert_eq!(
                system_contents(&mut file, &stat, &request, &read_deadline())
                    .unwrap()
                    .len(),
                size
            );
            file.set_len(size as u64 + 1).unwrap_err(); // the admitted reader is read-only
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_len(size as u64 + 1)
                .unwrap();
            let mut file = File::open(&path).unwrap();
            assert_eq!(
                system_contents(&mut file, &stat, &request, &read_deadline()),
                Err(NativeError::Oversize),
                "growth after size observation remains bounded"
            );
            stat.st_size += 1;
            assert_eq!(
                system_contents(
                    &mut File::open(&path).unwrap(),
                    &stat,
                    &request,
                    &read_deadline()
                ),
                Err(NativeError::Oversize)
            );
        }
        let mut bytes = vec![0; MAX_ELF_PREFIX_BYTES + 123];
        bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        fs::write(&path, &bytes).unwrap();
        let mut stat = rfs::fstat(File::open(&path).unwrap()).unwrap();
        stat.st_uid = 0;
        let elf = SystemRead::ElfPrefix("/usr/lib/fixture.so".into());
        assert_eq!(
            system_contents(
                &mut File::open(&path).unwrap(),
                &stat,
                &elf,
                &read_deadline()
            )
            .unwrap()
            .len(),
            MAX_ELF_PREFIX_BYTES
        );
        for bytes in [
            Vec::new(),
            b"not an ELF file".to_vec(),
            b"\x7fELF\x02\x01\x01".to_vec(),
            vec![0; 64],
        ] {
            fs::write(&path, &bytes).unwrap();
            assert_eq!(
                system_contents(
                    &mut File::open(&path).unwrap(),
                    &stat,
                    &elf,
                    &read_deadline()
                ),
                Err(NativeError::Invalid)
            );
        }
        for mode in [0o100666, 0o040700, 0o120777, 0o010600] {
            let mut unsafe_stat = stat;
            unsafe_stat.st_mode = mode;
            assert_eq!(
                read_stat(&unsafe_stat, 0, 0o100000),
                Err(NativeError::Foreign)
            );
        }
        let stop = Cancellation::default();
        let d = Deadline::new(1000, stop.clone()).unwrap();
        stop.cancel();
        stat.st_size = 64;
        assert_eq!(
            system_contents(
                &mut File::open(&path).unwrap(),
                &stat,
                &SystemRead::OsRelease,
                &d
            ),
            Err(NativeError::Cancelled)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn descriptor_read_parents_refuse_every_symlink_level_and_writable_ancestor() {
        let (io, root) = fixture();
        let parent = root.join("a/b");
        fs::create_dir_all(&parent).unwrap();
        for path in [root.join("a"), parent.clone()] {
            let saved = root.join("saved");
            fs::rename(&path, &saved).unwrap();
            symlink(&saved, &path).unwrap();
            assert!(matches!(
                read_parent(
                    read_anchor(&root).unwrap(),
                    Path::new("a/b/file"),
                    io.target.paths.uid
                ),
                Err(NativeError::Foreign)
            ));
            fs::remove_file(&path).unwrap();
            fs::rename(&saved, &path).unwrap();
        }
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(matches!(
            read_parent(
                read_anchor(&root).unwrap(),
                Path::new("a/b/file"),
                io.target.paths.uid
            ),
            Err(NativeError::Foreign)
        ));
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        let admitted = read_parent(
            read_anchor(&root).unwrap(),
            Path::new("a/b/file"),
            io.target.paths.uid,
        )
        .unwrap();
        fs::write(parent.join("real"), b"inert").unwrap();
        symlink(parent.join("real"), parent.join("file")).unwrap();
        assert!(
            rfs::openat(
                admitted.directories.last().unwrap(),
                &admitted.name,
                OFlags::RDONLY | OFlags::NOFOLLOW,
                Mode::empty()
            )
            .is_err()
        );
        fs::rename(root.join("a"), root.join("saved")).unwrap();
        fs::create_dir(root.join("a")).unwrap();
        fs::rename(root.join("saved/b"), root.join("a/b")).unwrap();
        let replaced = read_parent(
            read_anchor(&root).unwrap(),
            Path::new("a/b/file"),
            io.target.paths.uid,
        )
        .unwrap();
        assert_eq!(
            admitted.same_as(&replaced),
            Err(NativeError::Foreign),
            "replacement retaining the final parent inode is still foreign"
        );
        fs::remove_dir_all(root).unwrap();
    }

    fn scratch_read_socket(
        kind: ReadSocket,
    ) -> (
        LinuxNativeIo,
        PathBuf,
        std::os::unix::net::UnixListener,
        ChildEnvironment,
    ) {
        let (io, root) = fixture();
        let runtime = &io.target.paths.runtime_home;
        io.create_private_dir(&io.scratch_support(observations(&io)).unwrap(), runtime)
            .unwrap();
        let (path, session) = match kind {
            ReadSocket::SessionBus => {
                let path = runtime.join("bus");
                (
                    path.clone(),
                    BTreeMap::from([(
                        "DBUS_SESSION_BUS_ADDRESS".into(),
                        format!("unix:path={}", path.display()),
                    )]),
                )
            }
            ReadSocket::Wayland => (
                runtime.join("wayland-fixture"),
                BTreeMap::from([("WAYLAND_DISPLAY".into(), "wayland-fixture".into())]),
            ),
            ReadSocket::Hyprland => {
                let dir = runtime.join("hypr/fixture_1");
                io.create_private_dir(&io.scratch_support(observations(&io)).unwrap(), &dir)
                    .unwrap();
                (
                    dir.join(".socket.sock"),
                    BTreeMap::from([("HYPRLAND_INSTANCE_SIGNATURE".into(), "fixture_1".into())]),
                )
            }
            ReadSocket::SystemBus => unreachable!(),
        };
        let listener = std::os::unix::net::UnixListener::bind(path).unwrap();
        let env = ChildEnvironment::selected(&io.target, session).unwrap();
        (io, root, listener, env)
    }

    #[test]
    fn selected_streams_connect_scratch_endpoints_without_sending_protocol_bytes() {
        for kind in [
            ReadSocket::SessionBus,
            ReadSocket::Wayland,
            ReadSocket::Hyprland,
        ] {
            let (io, root, listener, environment) = scratch_read_socket(kind);
            let stream = match kind {
                ReadSocket::SessionBus => io.connect_session_bus(&environment, &read_deadline()),
                ReadSocket::Wayland => io.connect_wayland(&environment, &read_deadline()),
                _ => io.connect_hyprland(&environment, &read_deadline()),
            }
            .unwrap();
            drop(stream);
            let (mut server, _) = listener.accept().unwrap();
            let mut bytes = Vec::new();
            server.read_to_end(&mut bytes).unwrap();
            assert!(bytes.is_empty());
            let (other, other_root) = fixture();
            assert!(matches!(
                other.connect_read_endpoint(Some(&environment), kind, &read_deadline()),
                Err(NativeError::Foreign)
            ));
            fs::remove_dir_all(other_root).unwrap();
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn connected_socket_and_runtime_ancestry_replacements_are_discarded() {
        for replace_runtime in [false, true] {
            let (mut io, root, listener, environment) = scratch_read_socket(ReadSocket::Wayland);
            let runtime = io.target.paths.runtime_home.clone();
            io.read_interleave = Some(Arc::new(move || {
                if replace_runtime {
                    fs::rename(&runtime, runtime.with_extension("old")).unwrap();
                    fs::create_dir(&runtime).unwrap();
                    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
                    fs::rename(
                        runtime.with_extension("old").join("wayland-fixture"),
                        runtime.join("wayland-fixture"),
                    )
                    .unwrap();
                } else {
                    fs::rename(runtime.join("wayland-fixture"), runtime.join("old.sock")).unwrap();
                    drop(
                        std::os::unix::net::UnixListener::bind(runtime.join("wayland-fixture"))
                            .unwrap(),
                    );
                }
            }));
            assert!(matches!(
                io.connect_wayland(&environment, &read_deadline()),
                Err(NativeError::Foreign)
            ));
            let (mut server, _) = listener.accept().unwrap();
            let mut bytes = Vec::new();
            server.read_to_end(&mut bytes).unwrap();
            assert!(
                bytes.is_empty(),
                "a drifted stream must be closed before protocol writes"
            );
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn socket_links_selection_grammar_and_injected_root_peer_evidence_fail_closed() {
        let (io, root, listener, environment) = scratch_read_socket(ReadSocket::Hyprland);
        let (path, uid) =
            socket_location(io.target(), Some(&environment), ReadSocket::Hyprland).unwrap();
        let parent = socket_parent(io.target(), &path, uid).unwrap();
        let mut stat = socket_observation(&parent, uid, false).unwrap();
        stat.st_uid = 0;
        stat.st_mode = 0o140666;
        assert!(read_socket_stat(&stat, 0, true).is_ok());
        assert_eq!(
            read_socket_stat(&stat, uid, true),
            Err(NativeError::Foreign)
        );
        assert_eq!(read_socket_stat(&stat, 0, false), Err(NativeError::Foreign));
        assert!(read_peer(0, 0).is_ok());
        assert_eq!(read_peer(uid, 0), Err(NativeError::Foreign));
        assert_eq!(read_peer(uid + 1, uid), Err(NativeError::Foreign));
        stat.st_nlink = 2;
        assert_eq!(read_socket_stat(&stat, 0, true), Err(NativeError::Foreign));
        fs::rename(&path, path.with_extension("old")).unwrap();
        symlink(path.with_extension("old"), &path).unwrap();
        assert!(matches!(
            io.connect_hyprland(&environment, &read_deadline()),
            Err(NativeError::Foreign)
        ));
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        for signature in [
            "..",
            "../foreign",
            "foreign/nested",
            "bad space",
            "",
            &"x".repeat(257),
        ] {
            let mut env = environment.clone();
            env.values
                .insert("HYPRLAND_INSTANCE_SIGNATURE".into(), signature.into());
            assert!(socket_location(io.target(), Some(&env), ReadSocket::Hyprland).is_err());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn read_worker_deadlines_cancellation_and_noncooperative_saturation_keep_admission() {
        let (io, root, listener, environment) = scratch_read_socket(ReadSocket::Wayland);
        let (release, gate) = mpsc::channel();
        let gate = Arc::new(std::sync::Mutex::new(gate));
        for _ in 0..4 {
            let gate = gate.clone();
            assert_eq!(
                bounded_launch(
                    &READ_WORKERS,
                    &Deadline::new(10, Cancellation::default()).unwrap(),
                    move || {
                        gate.lock()
                            .map_err(|_| NativeError::Unavailable)?
                            .recv()
                            .map_err(|_| NativeError::Unavailable)?;
                        Ok(())
                    }
                ),
                Err(NativeError::Timeout)
            );
        }
        assert!(matches!(
            io.connect_wayland(&environment, &read_deadline()),
            Err(NativeError::Busy)
        ));
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        for _ in 0..4 {
            release.send(()).unwrap();
        }
        let until = Instant::now() + Duration::from_secs(1);
        while READ_WORKERS.load(Ordering::Acquire) != 0 {
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(2));
        }
        let cancel = Cancellation::default();
        let deadline = Deadline::new(1000, cancel.clone()).unwrap();
        cancel.cancel();
        assert!(matches!(
            io.connect_wayland(&environment, &deadline),
            Err(NativeError::Cancelled)
        ));
        let stream = io.connect_wayland(&environment, &read_deadline()).unwrap();
        drop(stream);
        let (mut server, _) = listener.accept().unwrap();
        assert_eq!(server.read(&mut [0; 1]).unwrap(), 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn actual_child_environment_preparation_clears_contaminated_fake_parent() {
        let (io, root) = fixture();
        let environment = ChildEnvironment::selected(io.target(), BTreeMap::new()).unwrap();
        let mut manager_environment = environment;
        manager_environment.values.remove("DBUS_SYSTEM_BUS_ADDRESS");
        // A fake launch seeds inherited values without changing this process's environment.
        // Production uses this same preparation function immediately before its own spawn.
        let mut command = Command::new("/inert/never-executed");
        command.envs([
            ("DBUS_SYSTEM_BUS_ADDRESS", "unix:path=/foreign/system"),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/foreign/session"),
            ("SYSTEMD_BUS_ADDRESS", "unix:path=/foreign/manager"),
            ("SYSTEMD_UNIT_PATH", "/foreign/units"),
            ("SYSTEMD_HOST", "foreign"),
            ("HOME", "/foreign/home"),
            ("XDG_RUNTIME_DIR", "/foreign/runtime"),
        ]);
        child_environment(&mut command, &manager_environment);
        let actual: BTreeMap<String, String> = command
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_str().unwrap().into(),
                    v.unwrap().to_str().unwrap().into(),
                )
            })
            .collect();
        assert_eq!(actual, manager_environment.values);
        assert!(
            !actual
                .keys()
                .any(|k| k.starts_with("SYSTEMD_") || k.starts_with("DBUS_"))
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manager_pre_spawn_drift_is_foreign_but_attempted_mutation_drift_is_unknown() {
        struct FakeAdmission(bool);
        impl CommandRunner for FakeAdmission {
            fn run(&self, spec: &CommandSpec, _: &Deadline) -> Result<CommandOutput> {
                // Model the real runner's admission versus spawn handshake without executing.
                spec.spawn_attempt
                    .as_ref()
                    .unwrap()
                    .store(self.0, Ordering::Release);
                let path = PathBuf::from(&spec.environment.values["XDG_RUNTIME_DIR"])
                    .join("systemd/private");
                fs::rename(&path, path.with_extension("old")).unwrap();
                let _socket = std::os::unix::net::UnixListener::bind(&path).unwrap();
                Err(NativeError::Foreign)
            }
        }
        for attempted in [false, true] {
            let (mut io, root) = fixture();
            io.runner = Arc::new(FakeAdmission(attempted));
            let proof = io.scratch_support(observations(&io)).unwrap();
            for path in [
                io.target.paths.runtime_home.join("systemd"),
                io.target.paths.state_home.join("crosspane/installer"),
            ] {
                io.create_private_dir(&proof, &path).unwrap();
            }
            let _socket = std::os::unix::net::UnixListener::bind(
                io.target.paths.runtime_home.join("systemd/private"),
            )
            .unwrap();
            let deadline = Deadline::new(5000, Cancellation::default()).unwrap();
            let command = CommandSpec::new(
                "/usr/bin/systemctl".into(),
                vec![
                    "--user".into(),
                    "start".into(),
                    "crosspane-agent.service".into(),
                ],
                io.manager_environment(BTreeMap::new(), &deadline).unwrap(),
                256,
            )
            .unwrap();
            let result = io.run_manager_mutation(
                &proof,
                &command,
                io.install_lease(&proof).unwrap(),
                &deadline,
            );
            assert_eq!(
                result.result.unwrap_err(),
                if attempted {
                    NativeError::OutcomeUnknown
                } else {
                    NativeError::Foreign
                }
            );
            assert!(result.pending.is_none());
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn manager_child_cleanup_retains_install_lease_until_fake_reaping_finishes() {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        struct FakeChild(Arc<AtomicBool>);
        impl Cleanup for FakeChild {
            fn terminate(&mut self) {}
            fn reaped(&mut self) -> bool {
                self.0.load(Ordering::Acquire)
            }
        }
        let (io, root) = fixture();
        let proof = io.scratch_support(observations(&io)).unwrap();
        io.create_private_dir(
            &proof,
            &io.target.paths.state_home.join("crosspane/installer"),
        )
        .unwrap();
        let pending = PendingOperation(Arc::new(AtomicBool::new(false)));
        let reaped = Arc::new(AtomicBool::new(false));
        let cleanup = cleanup_admission::<ManagerChild<FakeChild>>(&COUNT).unwrap();
        assert!(
            cleanup
                .try_send(ManagerChild {
                    child: FakeChild(reaped.clone()),
                    _cleanup: None,
                    _lease: Some(Arc::new(LeaseGuard {
                        lease: Some(io.install_lease(&proof).unwrap()),
                        finished: pending.0.clone(),
                    })),
                })
                .is_ok()
        );
        drop(cleanup);
        assert!(!pending.completed());
        assert!(matches!(io.install_lease(&proof), Err(NativeError::Busy)));
        reaped.store(true, Ordering::Release);
        let until = Instant::now() + Duration::from_secs(1);
        while !pending.completed() {
            assert!(Instant::now() < until);
            thread::yield_now();
        }
        drop(io.install_lease(&proof).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manager_socket_requires_selected_uid_socket_type_and_single_link() {
        let (io, root) = fixture();
        let path = root.join("manager-test-socket");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let socket = rfs::stat(&path).unwrap();
        assert_eq!(manager_socket_stat(&socket, io.target.paths.uid), Ok(()));
        for choice in 0..3 {
            let mut facts = socket;
            match choice {
                0 => facts.st_uid += 1,
                1 => facts.st_mode = 0o100600,
                _ => facts.st_nlink = 2,
            }
            assert_eq!(
                manager_socket_stat(&facts, io.target.paths.uid),
                Err(NativeError::Foreign)
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

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
                FileOperation::IntentRename(..) => FileStep::Rename,
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
