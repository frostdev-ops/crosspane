//! Recovery of exact private runtime leaves after both process death and socket refusal.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Leaf {
    device: u64,
    inode: u64,
    uid: u32,
    mode: u32,
    links: u64,
    size: i64,
    modified: (i64, u64),
    changed: (i64, u64),
}
impl From<&rfs::Stat> for Leaf {
    fn from(s: &rfs::Stat) -> Self {
        Self {
            device: s.st_dev,
            inode: s.st_ino,
            uid: s.st_uid,
            mode: s.st_mode,
            links: s.st_nlink,
            size: s.st_size,
            modified: (s.st_mtime, s.st_mtime_nsec),
            changed: (s.st_ctime, s.st_ctime_nsec),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeadRuntime {
    directory: (u64, u64),
    bootstrap: Leaf,
    socket: Leaf,
    bytes: Vec<u8>,
    pid: u32,
}
fn dead(pid: u32) -> Result<()> {
    let pid = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .ok_or(NativeError::Foreign)?;
    match rustix::process::test_kill_process(pid) {
        Err(rustix::io::Errno::SRCH) => Ok(()),
        // Permission errors, zombies and a reused PID all remain live or unknown.
        _ => Err(NativeError::Foreign),
    }
}
impl LinuxNativeIo {
    /// Read-only recognition; never removes a file or claims a clean agent exit.
    pub(crate) fn dead_runtime(&self) -> Result<Option<DeadRuntime>> {
        self.validate_target()?;
        if self.metadata(self.target.runtime_dir())?.is_none() {
            return Ok(None);
        }
        let path = self.target.runtime_dir().join("bootstrap.json");
        let bootstrap = self.metadata(&path)?;
        let socket = self.metadata(&self.target.socket_path())?;
        if bootstrap.is_none() && socket.is_none() {
            return Ok(None);
        }
        let (bootstrap, socket) = bootstrap.zip(socket).ok_or(NativeError::Foreign)?;
        if bootstrap.st_mode & 0o777 != 0o600
            || bootstrap.st_nlink != 1
            || socket.st_mode & 0o022 != 0
            || socket.st_nlink != 1
        {
            return Err(NativeError::Foreign);
        }
        let bytes = self.read(&path, 4096, true)?;
        let record = parse_bootstrap(&bytes).map_err(|_| NativeError::Foreign)?;
        if Path::new(&record.runtime_dir) != self.target.runtime_dir() {
            return Err(NativeError::Foreign);
        }
        dead(record.pid)?;
        let (dir, name, inode) = self.socket_parent()?;
        let directory = native(rfs::fstat(&dir))?;
        if inode != (socket.st_dev, socket.st_ino) {
            return Err(NativeError::Foreign);
        }
        let mut entries = 0usize;
        for entry in native(rfs::Dir::read_from(&dir))? {
            let entry = native(entry)?;
            match entry.file_name().to_bytes() {
                b"." | b".." => {}
                b"bootstrap.json" | b"agent.sock" => entries += 1,
                _ => return Err(NativeError::Foreign),
            }
            if entries > 2 {
                return Err(NativeError::Foreign);
            }
        }
        if entries != 2 {
            return Err(NativeError::Foreign);
        }
        let address = native(SocketAddrUnix::new(format!(
            "/proc/self/fd/{}/{name}",
            dir.as_raw_fd()
        )))?;
        let fd = native(rnet::socket_with(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::NONBLOCK | SocketFlags::CLOEXEC,
            None,
        ))?;
        if rnet::connect(&fd, &address) != Err(rustix::io::Errno::CONNREFUSED) {
            return Err(NativeError::Foreign);
        }
        let current = native(rfs::statat(&dir, &name, AtFlags::SYMLINK_NOFOLLOW))?;
        if Leaf::from(&current) != Leaf::from(&socket)
            || self.metadata(&path)?.as_ref().map(Leaf::from) != Some(Leaf::from(&bootstrap))
            || self.read(&path, 4096, true)? != bytes
        {
            return Err(NativeError::Foreign);
        }
        dead(record.pid)?;
        Ok(Some(DeadRuntime {
            directory: (directory.st_dev, directory.st_ino),
            bootstrap: Leaf::from(&bootstrap),
            socket: Leaf::from(&socket),
            bytes,
            pid: record.pid,
        }))
    }

    /// The caller holds the selected install lock. All recognition is repeated under it,
    /// including PID absence, refusal to connect and exact file identities.
    pub(crate) fn clean_dead_runtime(
        &self,
        proof: &SupportProof,
        expected: &DeadRuntime,
        lease: &InstallLease,
    ) -> Result<()> {
        proof.check(self)?;
        if lease.nonce != self.target.nonce || self.dead_runtime()?.as_ref() != Some(expected) {
            return Err(NativeError::Foreign);
        }
        let (dir, socket, inode) = self.socket_parent()?;
        let parent = native(rfs::fstat(&dir))?;
        if (parent.st_dev, parent.st_ino) != expected.directory
            || inode != (expected.socket.device, expected.socket.inode)
        {
            return Err(NativeError::Foreign);
        }
        dead(expected.pid)?;
        proof.check(self)?;
        let current = native(rfs::statat(&dir, &socket, AtFlags::SYMLINK_NOFOLLOW))?;
        if Leaf::from(&current) != expected.socket {
            return Err(NativeError::Foreign);
        }
        let cleanup = (|| {
            native(rfs::unlinkat(&dir, socket, AtFlags::empty()))?;
            let path = self.target.runtime_dir().join("bootstrap.json");
            if self.metadata(&path)?.as_ref().map(Leaf::from) != Some(expected.bootstrap.clone())
                || self.read(&path, 4096, true)? != expected.bytes
            {
                return Err(NativeError::Foreign);
            }
            dead(expected.pid)?;
            proof.check(self)?;
            native(rfs::unlinkat(&dir, "bootstrap.json", AtFlags::empty()))?;
            native(rfs::fsync(&dir))?;
            // The runtime directory is retained, and an interrupted cleanup remains an error.
            if self.metadata(&path)?.is_some()
                || self.metadata(&self.target.socket_path())?.is_some()
            {
                return Err(NativeError::Foreign);
            }
            Ok(())
        })();
        cleanup.map_err(|_: NativeError| NativeError::OutcomeUnknown)
    }
}
