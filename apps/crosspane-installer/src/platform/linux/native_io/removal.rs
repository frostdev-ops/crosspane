//! Closed installed-agent execution and exit observation. Neither API infers clean shutdown.
use super::super::payload::{MAX_MEMBER_BYTES, sha256};
use super::*;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::process::{Pid, PidfdFlags, pidfd_open};

type Identity = (u64, u64, u32, u32, u64, i64, u64, i64, u64);
fn identity(s: &rfs::Stat) -> Identity {
    (
        s.st_dev,
        s.st_ino,
        s.st_uid,
        s.st_mode,
        s.st_size as u64,
        s.st_mtime,
        s.st_mtime_nsec,
        s.st_ctime,
        s.st_ctime_nsec,
    )
}
#[derive(Debug)]
pub(super) struct InstalledExecutable {
    nonce: u64,
    digest: [u8; 32],
    identity: Identity,
}
impl InstalledExecutable {
    pub(super) fn cleanup_matches(&self, target: &LinuxTarget, digest: [u8; 32]) -> bool {
        self.nonce == target.nonce && self.digest == digest
    }

    fn open(
        target: &LinuxTarget,
        digest: [u8; 32],
        deadline: &Deadline,
    ) -> Result<(Self, OwnedFd)> {
        deadline.check()?;
        let path = target.agent_path();
        let parent = walk_dir(target, path.parent().ok_or(NativeError::Invalid)?, None)?
            .ok_or(NativeError::Unavailable)?;
        let name = path.file_name().ok_or(NativeError::Invalid)?;
        let fd = native(rfs::openat(
            &parent,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        ))?;
        let before = native(rfs::fstat(&fd))?;
        if before.st_uid != target.paths.uid
            || before.st_nlink != 1
            || before.st_mode & 0o177777 != 0o100755
            || before.st_size <= 0
            || before.st_size as u64 > MAX_MEMBER_BYTES as u64
        {
            return Err(NativeError::Foreign);
        }
        let mut file = File::from(fd);
        let mut bytes = Vec::new();
        native(
            (&mut file)
                .take(MAX_MEMBER_BYTES as u64 + 1)
                .read_to_end(&mut bytes),
        )?;
        deadline.check()?;
        if bytes.len() != before.st_size as usize
            || sha256(&bytes) != digest
            || identity(&native(rfs::fstat(&file))?) != identity(&before)
            || identity(&native(rfs::statat(
                &parent,
                name,
                AtFlags::SYMLINK_NOFOLLOW,
            ))?) != identity(&before)
        {
            return Err(NativeError::Foreign);
        }
        Ok((
            Self {
                nonce: target.nonce,
                digest,
                identity: identity(&before),
            },
            file.into(),
        ))
    }
    pub(super) fn revalidate(&self, target: &LinuxTarget, deadline: &Deadline) -> Result<OwnedFd> {
        let (current, fd) = Self::open(target, self.digest, deadline)?;
        if current.nonce != self.nonce || current.identity != self.identity {
            return Err(NativeError::Foreign);
        }
        Ok(fd)
    }
}
#[cfg(test)]
mod cleanup_target_tests {
    use super::*;
    #[test]
    fn cleanup_erase_same_paths_different_target_nonce_and_digest_are_independent_refusals() {
        let home = PathBuf::from("/tmp/cp419-pure-target-no-io");
        let paths = TargetPaths {
            uid: rustix::process::geteuid().as_raw(),
            prefix: home.join("prefix"),
            config_home: home.join("config"),
            state_home: home.join("state"),
            data_home: home.join("data"),
            runtime_home: home.join("run"),
            runtime_override: None,
            home,
        };
        let selected = LinuxTarget::make(paths.clone(), true).unwrap();
        let unrelated = LinuxTarget::make(paths, true).unwrap();
        assert_eq!(selected.agent_path(), unrelated.agent_path());
        let proof = InstalledExecutable {
            nonce: selected.nonce,
            digest: [7; 32],
            identity: (0, 0, 0, 0, 0, 0, 0, 0, 0),
        };
        assert!(proof.cleanup_matches(&selected, [7; 32]));
        assert!(!proof.cleanup_matches(&unrelated, [7; 32]));
        assert!(!proof.cleanup_matches(&selected, [8; 32]));
    }
}
impl CommandSpec {
    /// The digest comes from the validated package/installation inventory, never current bytes
    /// guessed to be trusted. This constructor allows no flags, ctl, or caller-selected executable.
    /// Clean-exit authority and explicit identity consent remain the removal caller's obligation.
    pub fn erase_identity(
        io: &LinuxNativeIo,
        digest: [u8; 32],
        environment: ChildEnvironment,
        deadline: &Deadline,
    ) -> Result<Self> {
        if environment.nonce != io.target.nonce {
            return Err(NativeError::Foreign);
        }
        let target = io.target.clone();
        let worker_deadline = deadline.clone();
        bounded_launch(&READ_WORKERS, deadline, move || {
            validate_target(&target)?;
            let (agent, _) = InstalledExecutable::open(&target, digest, &worker_deadline)?;
            Ok(Self {
                executable: target.agent_path(),
                argv: vec!["erase-identity".into()],
                environment,
                output_limit: 4096,
                admission: None,
                lease: None,
                spawn_attempt: None,
                agent: Some(Arc::new(agent)),
                cleanup: None,
            })
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessExit {
    Exited,
    Running,
    Unknown,
}
/// Injected only for an exclusively owned scratch target; never used for production observations.
pub trait ExitReader: Send + Sync {
    /// None means established process absence, never an unavailable/inaccessible read.
    fn snapshot(&self, pid: u32, deadline: &Deadline) -> Result<Option<ProcessFacts>>;
}
#[derive(Clone)]
pub struct ProcessWatch {
    nonce: u64,
    original: ProcessIdentity,
    pidfd: Option<Arc<OwnedFd>>,
    reader: Option<Arc<dyn ExitReader>>,
}
impl std::fmt::Debug for ProcessWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProcessWatch { .. }")
    }
}
impl LinuxNativeIo {
    /// Capture before stopping. Opening a pidfd after an uncorrelated exit cannot admit history.
    pub fn track_process(
        self: &Arc<Self>,
        original: &ProcessIdentity,
        deadline: &Deadline,
    ) -> Result<ProcessWatch> {
        if self.target.scratch {
            return Err(NativeError::Foreign);
        }
        self.track(original, None, deadline)
    }
    pub fn scratch_track_process(
        self: &Arc<Self>,
        original: &ProcessIdentity,
        reader: Arc<dyn ExitReader>,
        deadline: &Deadline,
    ) -> Result<ProcessWatch> {
        if !self.target.scratch {
            return Err(NativeError::Foreign);
        }
        self.track(original, Some(reader), deadline)
    }
    fn track(
        self: &Arc<Self>,
        original: &ProcessIdentity,
        reader: Option<Arc<dyn ExitReader>>,
        deadline: &Deadline,
    ) -> Result<ProcessWatch> {
        let (io, original, d) = (self.clone(), original.clone(), deadline.clone());
        bounded_launch(&READ_WORKERS, deadline, move || {
            io.track_direct(&original, reader, &d)
        })
    }
    /// The complete bootstrap/process capture is bounded; timed-out workers retain all resources.
    pub fn track_bootstrap(
        self: &Arc<Self>,
        reader: Option<Arc<dyn ExitReader>>,
        deadline: &Deadline,
    ) -> Result<(BootstrapV1, ProcessWatch)> {
        if self.target.scratch != reader.is_some() {
            return Err(NativeError::Foreign);
        }
        let (io, d) = (self.clone(), deadline.clone());
        bounded_launch(&READ_WORKERS, deadline, move || {
            let (bootstrap, original) = io.bootstrap(&d)?;
            let watch = io.track_direct(&original, reader, &d)?;
            if io.bootstrap(&d)? != (bootstrap.clone(), original) {
                return Err(NativeError::Foreign);
            }
            Ok((bootstrap, watch))
        })
    }
    fn track_direct(
        &self,
        original: &ProcessIdentity,
        reader: Option<Arc<dyn ExitReader>>,
        deadline: &Deadline,
    ) -> Result<ProcessWatch> {
        if self.process_identity(original.pid, deadline)? != *original {
            return Err(NativeError::Foreign);
        }
        if let Some(reader) = &reader
            && !reader
                .snapshot(original.pid, deadline)?
                .as_ref()
                .is_some_and(|f| matches_original(f, original))
        {
            return Err(NativeError::Foreign);
        }
        let pidfd = if reader.is_some() {
            None
        } else {
            match pidfd_open(
                Pid::from_raw(original.pid.try_into().map_err(|_| NativeError::Invalid)?)
                    .ok_or(NativeError::Invalid)?,
                PidfdFlags::empty(),
            ) {
                Ok(fd) => Some(Arc::new(fd)),
                Err(rustix::io::Errno::NOSYS | rustix::io::Errno::INVAL) => None,
                Err(_) => return Err(NativeError::Unavailable),
            }
        };
        if self.process_identity(original.pid, deadline)? != *original {
            return Err(NativeError::Foreign);
        }
        Ok(ProcessWatch {
            nonce: self.target.nonce,
            original: original.clone(),
            pidfd,
            reader,
        })
    }
}
impl ProcessWatch {
    pub fn original(&self) -> &ProcessIdentity {
        &self.original
    }
    pub fn observe(&self, io: &Arc<LinuxNativeIo>, deadline: &Deadline) -> Result<ProcessExit> {
        let (watch, io, d) = (self.clone(), io.clone(), deadline.clone());
        bounded_launch(&READ_WORKERS, deadline, move || {
            watch.observe_direct(&io, &d)
        })
    }
    pub(crate) fn exit_observation(
        &self,
        io: &Arc<LinuxNativeIo>,
        deadline: &Deadline,
    ) -> Result<(Vec<u8>, Vec<u8>, ProcessExit)> {
        let (watch, io, d) = (self.clone(), io.clone(), deadline.clone());
        bounded_launch(&READ_WORKERS, deadline, move || {
            let bootstrap_path = io.target().runtime_dir().join("bootstrap.json");
            let exit_path = io
                .target()
                .paths()
                .state_home
                .join("crosspane/last_exit.json");
            let bootstrap = io.read(&bootstrap_path, 4096, true)?;
            let receipt = io.read(&exit_path, 4096, true)?;
            let exit = watch.observe_direct(&io, &d)?;
            if io.read(&bootstrap_path, 4096, true)? != bootstrap
                || io.read(&exit_path, 4096, true)? != receipt
            {
                return Err(NativeError::Foreign);
            }
            d.check()?;
            Ok((bootstrap, receipt, exit))
        })
    }
    fn observe_direct(&self, io: &LinuxNativeIo, deadline: &Deadline) -> Result<ProcessExit> {
        deadline.check()?;
        if io.target.nonce != self.nonce {
            return Err(NativeError::Foreign);
        }
        io.validate_target()?;
        let current = if let Some(reader) = &self.reader {
            reader.snapshot(self.original.pid, deadline)
        } else {
            proc_current(&self.original, deadline)
        };
        let observed = match current {
            Ok(None) => ProcessExit::Exited,
            Ok(Some(facts)) if matches_original(&facts, &self.original) => {
                if let Some(fd) = &self.pidfd {
                    let mut fds = [PollFd::new(fd.as_ref(), PollFlags::IN)];
                    match poll(
                        &mut fds,
                        Some(&Timespec {
                            tv_sec: 0,
                            tv_nsec: 0,
                        }),
                    ) {
                        Ok(_) if fds[0].revents().contains(PollFlags::IN) => ProcessExit::Exited,
                        Ok(_) if fds[0].revents().is_empty() => ProcessExit::Running,
                        _ => ProcessExit::Unknown,
                    }
                } else {
                    ProcessExit::Running
                }
            }
            _ => ProcessExit::Unknown,
        };
        deadline.check()?;
        Ok(observed)
    }
}
fn matches_original(facts: &ProcessFacts, original: &ProcessIdentity) -> bool {
    facts.uid == original.uid
        && facts.executable == original.executable
        && facts.generation == original.generation
}
fn proc_current(original: &ProcessIdentity, deadline: &Deadline) -> Result<Option<ProcessFacts>> {
    deadline.check()?;
    let root = PathBuf::from(format!("/proc/{}", original.pid));
    match fs::metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Ok(_) => {
            let first = ProcProbe.snapshot(original.pid, deadline)?;
            if first != ProcProbe.snapshot(original.pid, deadline)? {
                return Err(NativeError::Foreign);
            }
            Ok(Some(first))
        }
        Err(_) => Err(NativeError::Unavailable),
    }
}
