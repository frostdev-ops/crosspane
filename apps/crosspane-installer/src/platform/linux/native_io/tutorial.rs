//! Tutorial-only admissions; existing command and agent identity rules remain separate.
use super::*;
use aws_lc_rs::digest::{Context, SHA256};
use crosspane_platform_linux::hyprland::ipc::HyprIpc;
use std::os::unix::process::CommandExt;
use std::process::{ChildStdin, ChildStdout};

#[derive(Clone)]
pub struct TutorialCommand {
    file: Arc<File>,
    stat: rfs::Stat,
    path: PathBuf,
    font: PathBuf,
    environment: ChildEnvironment,
    target: LinuxTarget,
    proof: SupportProof,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TutorialIdentity {
    pub pid: u32,
    pub uid: u32,
    /// Linux procfs start ticks, not a wall-clock timestamp or an agent instance id.
    pub start_ticks: u64,
    pub executable: PathBuf,
}
pub struct TutorialChild {
    child: Option<Child>,
    input: Option<ChildStdin>,
    output: Option<ChildStdout>,
    identity: TutorialIdentity,
    cleanup: SyncSender<Child>,
}
impl std::fmt::Debug for TutorialCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TutorialCommand(admitted executable and private pipes)")
    }
}
impl std::fmt::Debug for TutorialChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TutorialChild(owned process and private pipes)")
    }
}
impl std::fmt::Debug for TutorialIpc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TutorialIpc(selected read-only endpoint)")
    }
}
impl TutorialChild {
    pub fn identity(&self) -> &TutorialIdentity {
        &self.identity
    }
    pub fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.output
            .as_mut()
            .ok_or(std::io::ErrorKind::BrokenPipe)?
            .read(bytes)
    }
    pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.input
            .as_mut()
            .ok_or(std::io::ErrorKind::BrokenPipe)?
            .write(bytes)
    }
    pub fn reaped(&mut self) -> Result<bool> {
        if let Some(child) = &mut self.child {
            if native(child.try_wait())?.is_none() {
                return Ok(false);
            }
            self.child = None;
        }
        Ok(true)
    }
    pub fn retire(&mut self) {
        self.input = None;
        self.output = None;
        if let Some(child) = self.child.take() {
            let _ = self.cleanup.try_send(child);
        }
    }
}
impl Drop for TutorialChild {
    fn drop(&mut self) {
        self.retire();
    }
}
fn same(a: &rfs::Stat, b: &rfs::Stat) -> bool {
    (a.st_dev, a.st_ino, a.st_size, a.st_mtime, a.st_mtime_nsec)
        == (b.st_dev, b.st_ino, b.st_size, b.st_mtime, b.st_mtime_nsec)
}
impl TutorialCommand {
    fn check(&self, io: &LinuxNativeIo) -> Result<()> {
        if self.target.nonce != io.target.nonce
            || !same(&self.stat, &native(rfs::fstat(self.file.as_ref()))?)
            || !same(
                &self.stat,
                &io.metadata(&self.path)?.ok_or(NativeError::Foreign)?,
            )
        {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
}
impl LinuxNativeIo {
    /// Exact selected-prefix tutorial, pinned SHA-256 and held no-follow executable descriptor.
    /// Scratch admission stays Demo and can read only the exact checked-in review font.
    pub fn admit_tutorial(
        &self,
        proof: &SupportProof,
        environment: &ChildEnvironment,
        expected_sha256: [u8; 32],
        font: &Path,
        deadline: &Deadline,
    ) -> Result<TutorialCommand> {
        deadline.check()?;
        proof.check(self)?;
        proof.check_agent_compatibility()?;
        self.validate_target()?;
        if environment.nonce != self.target.nonce
            || environment.manager.is_some()
            || environment.values.get("XDG_SESSION_ID") != Some(&proof.facts.session_id)
            || environment.values.get("XDG_SESSION_TYPE") != Some(&proof.facts.session_type)
            || !["WAYLAND_DISPLAY", "HYPRLAND_INSTANCE_SIGNATURE"]
                .iter()
                .all(|key| {
                    environment
                        .values
                        .get(*key)
                        .is_some_and(|v| !v.contains('/') && v != "." && v != "..")
                })
        {
            return Err(NativeError::Foreign);
        }
        let bytes = if self.target.scratch {
            let exact = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/fonts/LiberationSans-Regular.ttf");
            if font.as_os_str() != exact.as_os_str() {
                return Err(NativeError::Foreign);
            }
            let fd = native(rfs::open(
                font,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ))?;
            let stat = native(rfs::fstat(&fd))?;
            if stat.st_uid != self.target.paths.uid || stat.st_mode & 0o170022 != 0o100000 {
                return Err(NativeError::Foreign);
            }
            let mut bytes = Vec::new();
            native(
                File::from(fd)
                    .take(MAX_FONT_BYTES as u64 + 1)
                    .read_to_end(&mut bytes),
            )?;
            bytes
        } else {
            self.read_system(SystemRead::Font(font.into()), deadline)?
                .bytes
        };
        crate::platform::linux::detect::fonts::definitions(bytes)
            .map_err(|_| NativeError::Invalid)?;
        let path = self.target.paths.prefix.join("bin/crosspane-tutorial");
        let (parent, name) = self.parent(&path)?;
        let mut file = File::from(native(rfs::openat(
            &parent,
            &name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ))?);
        let stat = native(rfs::fstat(&file))?;
        if stat.st_uid != self.target.paths.uid
            || stat.st_nlink != 1
            || stat.st_mode & 0o177022 != 0o100000
            || stat.st_mode & 0o111 == 0
            || stat.st_size <= 0
            || stat.st_size as u64 > MAX_FILE_BYTES as u64
        {
            return Err(NativeError::Foreign);
        }
        let mut digest = Context::new(&SHA256);
        let mut buffer = [0; 64 * 1024];
        let mut size = 0u64;
        loop {
            deadline.check()?;
            let n = native(file.read(&mut buffer))?;
            if n == 0 {
                break;
            }
            size = size.checked_add(n as u64).ok_or(NativeError::Oversize)?;
            if size > MAX_FILE_BYTES as u64 {
                return Err(NativeError::Oversize);
            }
            digest.update(&buffer[..n]);
        }
        if size != stat.st_size as u64 || digest.finish().as_ref() != expected_sha256 {
            return Err(NativeError::Foreign);
        }
        let command = TutorialCommand {
            file: Arc::new(file),
            stat,
            path,
            font: font.into(),
            environment: environment.clone(),
            target: self.target.clone(),
            proof: proof.clone(),
        };
        command.check(self)?;
        deadline.check()?;
        Ok(command)
    }
    /// Separate verifier: exact tutorial PID/UID/executable and stable procfs start ticks.
    pub fn tutorial_identity(
        &self,
        command: &TutorialCommand,
        pid: u32,
        deadline: &Deadline,
    ) -> Result<TutorialIdentity> {
        if pid == 0 {
            return Err(NativeError::Foreign);
        }
        command.check(self)?;
        let before = self.probe.snapshot(pid, deadline)?;
        if before.uid != self.target.paths.uid
            || before.executable != command.path
            || before.generation == 0
            || before != self.probe.snapshot(pid, deadline)?
        {
            return Err(NativeError::Foreign);
        }
        command.check(self)?;
        deadline.check()?;
        Ok(TutorialIdentity {
            pid,
            uid: before.uid,
            start_ticks: before.generation,
            executable: before.executable,
        })
    }
    /// Called on a startup worker. Admission/cleanup slots survive stalled spawn and late results.
    pub fn spawn_tutorial(
        self: &Arc<Self>,
        command: TutorialCommand,
        deadline: &Deadline,
    ) -> Result<TutorialChild> {
        let io = self.clone();
        let worker_deadline = deadline.clone();
        bounded_launch(&PROCESS_LAUNCHES, deadline, move || {
            let cleanup = cleanup_admission::<Child>(&PROCESS_CLEANUPS)?;
            command.proof.check(&io)?;
            io.validate_target()?;
            command.check(&io)?;
            if let Some(bus) = &command.environment.bus {
                bus.revalidate(&io.target)?;
            }
            worker_deadline.check()?;
            let mut process = Command::new(format!("/proc/self/fd/{}", command.file.as_raw_fd()));
            child_environment(&mut process, &command.environment);
            let child = native(
                process
                    .arg0(&command.path)
                    .arg("--controlled")
                    .arg("--font")
                    .arg(&command.font)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn(),
            )?;
            let pid = child.id();
            let mut owned = TutorialChild {
                child: Some(child),
                input: None,
                output: None,
                identity: TutorialIdentity {
                    pid,
                    uid: io.target.paths.uid,
                    start_ticks: 0,
                    executable: command.path.clone(),
                },
                cleanup,
            };
            let child = owned.child.as_mut().ok_or(NativeError::Unavailable)?;
            owned.input = child.stdin.take();
            owned.output = child.stdout.take();
            native(rfs::fcntl_setfl(
                owned.input.as_ref().ok_or(NativeError::Unavailable)?,
                OFlags::NONBLOCK,
            ))?;
            native(rfs::fcntl_setfl(
                owned.output.as_ref().ok_or(NativeError::Unavailable)?,
                OFlags::NONBLOCK,
            ))?;
            owned.identity = io.tutorial_identity(&command, pid, &worker_deadline)?;
            command.proof.check(&io)?;
            worker_deadline.check()?;
            Ok(owned)
        })
    }
}

#[derive(Clone)]
pub struct TutorialIpc {
    ipc: HyprIpc,
    path: PathBuf,
    parent: Arc<ReadParent>,
    socket: (u64, u64),
}
fn ipc_parent(path: &Path) -> Result<ReadParent> {
    if !clean(path) {
        return Err(NativeError::Foreign);
    }
    let mut directories = vec![read_anchor(Path::new("/"))?];
    let mut current = PathBuf::from("/");
    let uid = rustix::process::geteuid().as_raw();
    for component in path.parent().ok_or(NativeError::Invalid)?.components() {
        if let Component::Normal(name) = component {
            current.push(name);
            let fd = native(rfs::openat(
                directories.last().ok_or(NativeError::Invalid)?,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ))?;
            let stat = native(rfs::fstat(&fd))?;
            let tmp =
                current == Path::new("/tmp") && stat.st_uid == 0 && stat.st_mode & 0o1777 == 0o1777;
            if ![0, uid].contains(&stat.st_uid) || (stat.st_mode & 0o022 != 0 && !tmp) {
                return Err(NativeError::Foreign);
            }
            directories.push(fd);
        }
    }
    Ok(ReadParent {
        directories,
        name: ".socket.sock".into(),
    })
}
impl TutorialIpc {
    /// Explicit selected endpoint, never from_env. Only clients/monitors, 250-ms whole requests.
    pub fn new(signature: &str, runtime: &Path) -> Result<Self> {
        if signature.is_empty()
            || signature.len() > 160
            || signature == "."
            || signature == ".."
            || !signature
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        {
            return Err(NativeError::Foreign);
        }
        let path = runtime.join("hypr").join(signature).join(".socket.sock");
        let parent = ipc_parent(&path)?;
        let stat = library_snapshot(&parent, &parent.name)?;
        socket_stat(&stat, rustix::process::geteuid().as_raw())?;
        Ok(Self {
            ipc: HyprIpc::new(signature, runtime, Duration::from_millis(250)),
            path,
            parent: Arc::new(parent),
            socket: (stat.st_dev, stat.st_ino),
        })
    }
    fn check(&self) -> Result<()> {
        let parent = ipc_parent(&self.path)?;
        if parent.directories.len() != self.parent.directories.len() {
            return Err(NativeError::Foreign);
        }
        for (old, now) in self.parent.directories.iter().zip(&parent.directories) {
            let a = native(rfs::fstat(old))?;
            let b = native(rfs::fstat(now))?;
            if (a.st_dev, a.st_ino) != (b.st_dev, b.st_ino) {
                return Err(NativeError::Foreign);
            }
        }
        let stat = library_snapshot(&parent, &parent.name)?;
        socket_stat(&stat, rustix::process::geteuid().as_raw())?;
        if self.socket != (stat.st_dev, stat.st_ino) {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
    pub fn json(&self, command: &str) -> Result<serde_json::Value> {
        if !matches!(command, "clients" | "monitors") {
            return Err(NativeError::Invalid);
        }
        self.check()?;
        let value = self
            .ipc
            .json(command)
            .map_err(|_| NativeError::Unavailable)?;
        self.check()?;
        Ok(value)
    }
}
