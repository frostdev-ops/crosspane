use super::*;

// Private operation seam: the same dispatch performs real I/O and injected durability failures.
pub(crate) enum FilesystemOperation<'a> {
    Write(&'a mut File, &'a [u8]),
    FileSync(std::os::fd::BorrowedFd<'a>),
    Rename {
        old_dir: std::os::fd::BorrowedFd<'a>,
        old: &'a str,
        new_dir: std::os::fd::BorrowedFd<'a>,
        new: &'a str,
        flags: rfs::RenameFlags,
    },
    DirectorySync(std::os::fd::BorrowedFd<'a>),
}
pub(crate) trait FilesystemOps: Send + Sync {
    fn execute(&self, operation: FilesystemOperation<'_>) -> NativeResult<()>;
}
pub(crate) struct SystemFilesystem;
impl FilesystemOps for SystemFilesystem {
    fn execute(&self, operation: FilesystemOperation<'_>) -> NativeResult<()> {
        match operation {
            FilesystemOperation::Write(file, bytes) => native(file.write_all(bytes)),
            FilesystemOperation::FileSync(file) => native(rfs::fsync(file)),
            FilesystemOperation::Rename {
                old_dir,
                old,
                new_dir,
                new,
                flags,
            } => native(rfs::renameat_with(old_dir, old, new_dir, new, flags)),
            FilesystemOperation::DirectorySync(directory) => native(rfs::fsync(directory)),
        }
    }
}

/// Keeps the admitted leaf fd and every ancestor identity; rewalking only the leaf is insufficient.
#[derive(Debug)]
pub struct DirectoryAnchor {
    fd: OwnedFd,
    path: PathBuf,
    chain: Vec<(u64, u64)>,
}
impl DirectoryAnchor {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn identity(&self) -> (u64, u64) {
        self.chain[self.chain.len() - 1]
    }
    pub fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.fd.as_fd()
    }
    pub fn revalidate(&self, io: &MacNativeIo) -> NativeResult<()> {
        let fresh = walk(&io.target, &self.path)?.ok_or(NativeError::Unavailable)?;
        if fresh.chain != self.chain {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
}
fn walk(target: &MacTarget, path: &Path) -> NativeResult<Option<DirectoryAnchor>> {
    // Approved residual: BSD ownership/mode only; macOS ACLs are not evaluated. Admins are trusted.
    let path = admitted_spelling(path)?;
    #[cfg(test)]
    let physical = target
        .test_path
        .as_ref()
        .map(|map| map(&path))
        .unwrap_or_else(|| path.clone());
    #[cfg(not(test))]
    let physical = path.clone();
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let mut fd = native(rfs::open("/", flags, Mode::empty()))?;
    let mut current = PathBuf::from("/");
    let mut chain = Vec::new();
    let check = |fd: &OwnedFd, path: &Path| -> NativeResult<(u64, u64)> {
        let s = target
            .observe(
                "walk",
                path,
                Some(FileIdentity::from_stat(&native(rfs::fstat(fd))?)),
            )?
            .ok_or(NativeError::Invalid)?;
        let selected = path.starts_with(&target.paths.home)
            || path.starts_with(&target.paths.gui_tmpdir)
            || path.starts_with(&target.runtime);
        // Exactly the merged Linux scratch exception: root-owned sticky tmp is allowed only
        // for an explicitly constructed scratch target, never for production admission.
        let scratch_tmp = target.scratch
            && path == Path::new("/private/tmp")
            && s.uid == 0
            && s.mode & 0o1777 == 0o1777;
        if s.mode & 0o170000 != 0o040000
            || (!scratch_tmp && s.mode & 0o022 != 0)
            || (selected && s.uid != target.paths.uid)
            || (!selected && s.uid != 0 && s.uid != target.paths.uid)
        {
            return Err(NativeError::Foreign);
        }
        Ok((s.device, s.inode))
    };
    chain.push(check(&fd, &current)?);
    for component in physical.components().skip(1) {
        let Component::Normal(name) = component else {
            return Err(NativeError::Foreign);
        };
        current.push(name);
        fd = match rfs::openat(&fd, name, flags, Mode::empty()) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(_) => return Err(NativeError::Foreign),
        };
        chain.push(check(&fd, &current)?);
    }
    Ok(Some(DirectoryAnchor { fd, path, chain }))
}
#[derive(Debug)]
pub struct SocketEndpoint {
    parent: DirectoryAnchor,
    path: PathBuf,
    identity: FileIdentity,
}
impl SocketEndpoint {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn identity(&self) -> &FileIdentity {
        &self.identity
    }
    /// Transport MUST call after connect and again before each mutation write, not just at submit.
    pub fn revalidate(&self, io: &MacNativeIo) -> NativeResult<()> {
        io.validate_target()?;
        self.parent.revalidate(io)?;
        let current = io.socket_endpoint()?;
        if current.identity.device != self.identity.device
            || current.identity.inode != self.identity.inode
        {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
}
/// File close releases the lock. It carries no authority to delete/adopt the corresponding path.
#[derive(Debug)]
pub struct InstallerLock {
    _file: File,
}

impl MacNativeIo {
    pub fn new(
        target: MacTarget,
        runner: Arc<dyn CommandRunner>,
        support: Arc<dyn SupportProbe>,
        signatures: Arc<dyn SignatureProbe>,
        clock: Arc<dyn Clock>,
    ) -> NativeResult<Self> {
        let home = walk(&target, &target.paths.home)?.ok_or(NativeError::Unavailable)?;
        let temporary = walk(&target, &target.paths.gui_tmpdir)?.ok_or(NativeError::Unavailable)?;
        let runtime = walk(&target, &target.runtime)?;
        let payload = walk(&target, &target.paths.payload_root)?.ok_or(NativeError::Unavailable)?;
        if let Some(dir) = &runtime {
            private_directory(&dir.fd, target.paths.uid)?;
        }
        let mutation = target.mutation.clone();
        Ok(Self {
            target,
            runner,
            support,
            signatures,
            clock,
            home,
            temporary,
            runtime,
            payload,
            mutation,
            filesystem: Arc::new(SystemFilesystem),
        })
    }
    #[cfg(test)]
    #[allow(dead_code)] // Exercised by the privately source-included integration suite.
    pub(crate) fn with_filesystem(mut self, filesystem: Arc<dyn FilesystemOps>) -> Self {
        self.filesystem = filesystem;
        self
    }
    pub fn target(&self) -> &MacTarget {
        &self.target
    }
    pub fn clock(&self) -> Arc<dyn Clock> {
        self.clock.clone()
    }
    pub fn validate_target(&self) -> NativeResult<()> {
        self.home.revalidate(self)?;
        self.temporary.revalidate(self)?;
        self.payload.revalidate(self)?;
        match (&self.runtime, walk(&self.target, &self.target.runtime)?) {
            (Some(old), Some(fresh)) => {
                if old.chain != fresh.chain {
                    return Err(NativeError::Foreign);
                }
                private_directory(&fresh.fd, self.target.paths.uid)?;
            }
            (None, None) => {}
            _ => return Err(NativeError::Foreign), // Re-detect an intentionally created/restarted runtime.
        }
        Ok(())
    }
    fn parent(&self, path: &Path) -> NativeResult<(DirectoryAnchor, String)> {
        let path = admitted_spelling(path)?;
        if !self.target.readable(&path) {
            return Err(NativeError::Foreign);
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| bounded(n, 255))
            .ok_or(NativeError::Invalid)?
            .to_owned();
        let parent = walk(&self.target, path.parent().ok_or(NativeError::Invalid)?)?
            .ok_or(NativeError::Unavailable)?;
        Ok((parent, name))
    }
    pub fn metadata(&self, path: &Path) -> NativeResult<Option<FileIdentity>> {
        self.validate_target()?;
        let (parent, name) = match self.parent(path) {
            Ok(p) => p,
            Err(NativeError::Unavailable) => return Ok(None),
            Err(e) => return Err(e),
        };
        let stat = match rfs::statat(&parent.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(s) => s,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(_) => return Err(NativeError::Unavailable),
        };
        parent.revalidate(self)?;
        self.target
            .observe("metadata", path, Some(FileIdentity::from_stat(&stat)))
    }
    fn open_regular(&self, path: &Path, private: bool, flags: OFlags) -> NativeResult<File> {
        let (parent, name) = self.parent(path)?;
        self.target.observe("open-before", path, None)?;
        let first = FileIdentity::from_stat(&native(rfs::statat(
            &parent.fd,
            &name,
            AtFlags::SYMLINK_NOFOLLOW,
        ))?);
        first.regular(self.target.paths.uid, private)?;
        let fd = rfs::openat(
            &parent.fd,
            &name,
            flags | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|_| NativeError::Foreign)?;
        if first != FileIdentity::from_stat(&native(rfs::fstat(&fd))?) {
            return Err(NativeError::Foreign);
        }
        parent.revalidate(self)?;
        Ok(File::from(fd))
    }
    pub fn read(
        &self,
        path: &Path,
        limit: usize,
        private: bool,
        deadline: &Deadline,
    ) -> NativeResult<Vec<u8>> {
        deadline.check()?;
        self.validate_target()?;
        if limit == 0 || limit > MAX_FILE_BYTES {
            return Err(NativeError::Invalid);
        }
        let mut file = self.open_regular(path, private, OFlags::RDONLY)?;
        let before = FileIdentity::from_stat(&native(rfs::fstat(&file))?);
        if before.length > limit as u64 {
            return Err(NativeError::Oversize);
        }
        let mut bytes = Vec::new();
        let mut buffer = [0; 16384];
        loop {
            deadline.check()?;
            let n = native(file.read(&mut buffer))?;
            if n == 0 {
                break;
            }
            if bytes.len() + n > limit {
                return Err(NativeError::Oversize);
            }
            bytes.extend_from_slice(&buffer[..n]);
        }
        if before != FileIdentity::from_stat(&native(rfs::fstat(&file))?)
            || self.metadata(path)? != Some(before)
        {
            return Err(NativeError::Foreign);
        }
        deadline.check()?;
        Ok(bytes)
    }
    pub fn entries(
        &self,
        path: &Path,
        limit: usize,
        deadline: &Deadline,
    ) -> NativeResult<Vec<(String, FileIdentity)>> {
        deadline.check()?;
        self.validate_target()?;
        if !self.target.readable(&admitted_spelling(path)?) || !(1..=4096).contains(&limit) {
            return Err(NativeError::Invalid);
        }
        let anchor = walk(&self.target, path)?.ok_or(NativeError::Unavailable)?;
        let mut result = Vec::new();
        for entry in native(rfs::Dir::read_from(&anchor.fd))? {
            deadline.check()?;
            let entry = native(entry)?;
            let name = entry
                .file_name()
                .to_str()
                .map_err(|_| NativeError::Invalid)?;
            if matches!(name, "." | "..") {
                continue;
            }
            if result.len() == limit {
                return Err(NativeError::Oversize);
            }
            if !bounded(name, 255) {
                return Err(NativeError::Invalid);
            }
            let identity = FileIdentity::from_stat(&native(rfs::statat(
                &anchor.fd,
                name,
                AtFlags::SYMLINK_NOFOLLOW,
            ))?);
            result.push((name.to_owned(), identity));
        }
        anchor.revalidate(self)?;
        result.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(result)
    }
    pub fn socket_endpoint(&self) -> NativeResult<SocketEndpoint> {
        self.validate_target()?;
        let parent = walk(&self.target, &self.target.runtime)?.ok_or(NativeError::Unavailable)?;
        private_directory(&parent.fd, self.target.paths.uid)?;
        let identity = FileIdentity::from_stat(&native(rfs::statat(
            &parent.fd,
            "agent.sock",
            AtFlags::SYMLINK_NOFOLLOW,
        ))?);
        identity.socket(self.target.paths.uid)?;
        parent.revalidate(self)?;
        Ok(SocketEndpoint {
            parent,
            path: self.target.socket_path(),
            identity,
        })
    }
    pub fn create_directory(
        &self,
        proof: &SupportProof,
        path: &Path,
        mode: u32,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        let _serial = self.mutation.try_lock().map_err(|_| NativeError::Busy)?;
        proof.check(self, deadline)?;
        let path = admitted_spelling(path)?;
        if !self.target.creatable(&path) || !matches!(mode, 0o700 | 0o755) {
            return Err(NativeError::Foreign);
        }
        let (parent, name) = self.parent(&path)?;
        if self.metadata(&path)?.is_some() {
            return Err(NativeError::Foreign);
        }
        self.boundary("mkdir", &path, deadline)?;
        let result = (|| {
            native(rfs::mkdirat(
                &parent.fd,
                name,
                Mode::from_raw_mode(mode as _),
            ))?;
            self.boundary("parent-sync", &path, deadline)?;
            self.filesystem
                .execute(FilesystemOperation::DirectorySync(parent.fd.as_fd()))?;
            parent.revalidate(self)?;
            self.boundary("complete", &path, deadline)
        })();
        result.map_err(|_| NativeError::OutcomeUnknown)
    }
    /// Only an observed exact prior identity, or observed absence, authorizes replacement.
    /// Writes and fsyncs a new private file, then renames and flushes its directory before return.
    pub fn atomic_write(
        &self,
        proof: &SupportProof,
        path: &Path,
        bytes: &[u8],
        expected: Option<&FileIdentity>,
        deadline: &Deadline,
    ) -> NativeResult<FileIdentity> {
        let _serial = self.mutation.try_lock().map_err(|_| NativeError::Busy)?;
        proof.check(self, deadline)?;
        let path = admitted_spelling(path)?;
        if !self.target.writable(&path) {
            return Err(NativeError::Foreign);
        }
        if bytes.len() > MAX_FILE_BYTES {
            return Err(NativeError::Oversize);
        }
        let (parent, name) = self.parent(&path)?;
        self.expected_at(&parent, &name, expected)?;
        let temporary = self.temporary_name(&parent, &name)?;
        self.boundary("create-temp", &path, deadline)?;
        let fd = rfs::openat(
            &parent.fd,
            &temporary,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|_| NativeError::OutcomeUnknown)?;
        let mut file = File::from(fd);
        let result: NativeResult<FileIdentity> = (|| {
            for chunk in bytes.chunks(16384) {
                self.boundary("write", &path, deadline)?;
                self.filesystem
                    .execute(FilesystemOperation::Write(&mut file, chunk))?;
            }
            self.boundary("file-sync", &path, deadline)?;
            self.filesystem
                .execute(FilesystemOperation::FileSync(file.as_fd()))?;
            proof.check(self, deadline)?;
            parent.revalidate(self)?;
            self.expected_at(&parent, &name, expected)?;
            let prior = expected
                .map(|identity| {
                    let old = self.open_regular(&path, false, OFlags::RDONLY)?;
                    if self.fd_identity(&old, &path)? != *identity {
                        return Err(NativeError::Foreign);
                    }
                    Ok(old)
                })
                .transpose()?;
            self.boundary("publish", &path, deadline)?;
            // One atomic publication: canonical receipt is always old or new, never absent.
            self.filesystem.execute(FilesystemOperation::Rename {
                old_dir: parent.fd.as_fd(),
                old: &temporary,
                new_dir: parent.fd.as_fd(),
                new: &name,
                flags: if prior.is_some() {
                    rfs::RenameFlags::EXCHANGE
                } else {
                    rfs::RenameFlags::NOREPLACE
                },
            })?;
            self.boundary("parent-sync", &path, deadline)?;
            self.filesystem
                .execute(FilesystemOperation::DirectorySync(parent.fd.as_fd()))?;
            if let Some(old) = prior {
                // A concurrent substitute stays retained; only the opened displaced object is ours.
                self.unlink_quarantined(&parent, &temporary, &old, &path, deadline)?;
            }
            let identity = self.fd_identity(&file, &path)?;
            identity.regular(self.target.paths.uid, true)?;
            self.expected_at(&parent, &name, Some(&identity))?;
            parent.revalidate(self)?;
            self.boundary("complete", &path, deadline)?;
            Ok(identity)
        })();
        // A failed write deliberately leaves its owned temporary for detection, never broad cleanup.
        result.map_err(|_| NativeError::OutcomeUnknown)
    }
    fn expected_at(
        &self,
        parent: &DirectoryAnchor,
        name: &str,
        expected: Option<&FileIdentity>,
    ) -> NativeResult<()> {
        let observed = match rfs::statat(&parent.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(s) => Some(FileIdentity::from_stat(&s)),
            Err(rustix::io::Errno::NOENT) => None,
            Err(_) => return Err(NativeError::Unavailable),
        };
        if observed.as_ref() != expected {
            return Err(NativeError::Foreign);
        }
        if let Some(s) = observed {
            if s.uid != self.target.paths.uid || s.mode & 0o7022 != 0 {
                return Err(NativeError::Foreign);
            }
            if s.mode & 0o170000 == 0o100000 && s.links != 1 {
                return Err(NativeError::Foreign);
            }
        }
        Ok(())
    }
    fn boundary(&self, stage: &str, path: &Path, deadline: &Deadline) -> NativeResult<()> {
        deadline.check()?;
        self.target.observe(stage, path, None)?;
        deadline.check()
    }
    fn fd_identity(&self, fd: &impl AsFd, path: &Path) -> NativeResult<FileIdentity> {
        self.target
            .observe(
                "fd-stat",
                path,
                Some(FileIdentity::from_stat(&native(rfs::fstat(fd))?)),
            )?
            .ok_or(NativeError::Invalid)
    }
    fn temporary_name(&self, parent: &DirectoryAnchor, name: &str) -> NativeResult<String> {
        let original = name
            .strip_prefix('.')
            .and_then(|s| s.split_once(".crosspane-temp-"))
            .filter(|(_, suffix)| {
                suffix.len() == 32 && suffix.bytes().all(|b| b.is_ascii_hexdigit())
            });
        let name = original.map(|(base, _)| base).unwrap_or(name);
        let prefix = format!(".{name}.crosspane-temp-");
        if prefix.len() + 32 > 255 {
            return Err(NativeError::Invalid);
        }
        let directory = native(rfs::Dir::read_from(&parent.fd))?;
        let mut count = 0;
        for (index, entry) in directory.enumerate() {
            if index == 4096 {
                return Err(NativeError::Oversize);
            }
            if native(entry)?
                .file_name()
                .to_bytes()
                .starts_with(prefix.as_bytes())
            {
                count += 1;
            }
            if count >= 8 && original.is_none() {
                return Err(NativeError::Busy);
            }
        }
        let mut random = [0; 16];
        native(aws_lc_rs::rand::fill(&mut random))?;
        Ok(format!(
            "{prefix}{}",
            random
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ))
    }
    /// Bind the opened object to expected, then isolate the pathname before destructive work.
    fn quarantine(
        &self,
        parent: &DirectoryAnchor,
        name: &str,
        expected: &FileIdentity,
        path: &Path,
        deadline: &Deadline,
    ) -> NativeResult<(String, File)> {
        self.expected_at(parent, name, Some(expected))?;
        let fd = native(rfs::openat(
            &parent.fd,
            name,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ))?;
        if FileIdentity::from_stat(&native(rfs::fstat(&fd))?) != *expected {
            return Err(NativeError::Foreign);
        }
        let temporary = self.temporary_name(parent, name)?;
        self.boundary("quarantine", path, deadline)?;
        let result = (|| {
            self.filesystem.execute(FilesystemOperation::Rename {
                old_dir: parent.fd.as_fd(),
                old: name,
                new_dir: parent.fd.as_fd(),
                new: &temporary,
                flags: rfs::RenameFlags::NOREPLACE,
            })?;
            let now = FileIdentity::from_stat(&native(rfs::fstat(&fd))?);
            if self.expected_at(parent, &temporary, Some(&now)).is_err() {
                // Never unlink a substituted object. Restore only into still-absent original.
                self.boundary("restore-quarantine", path, deadline)?;
                let _ = self.filesystem.execute(FilesystemOperation::Rename {
                    old_dir: parent.fd.as_fd(),
                    old: &temporary,
                    new_dir: parent.fd.as_fd(),
                    new: name,
                    flags: rfs::RenameFlags::NOREPLACE,
                });
                return Err(NativeError::Foreign);
            }
            parent.revalidate(self)?;
            Ok((temporary, File::from(fd)))
        })();
        result.map_err(|_| NativeError::OutcomeUnknown)
    }
    fn unlink_quarantined(
        &self,
        parent: &DirectoryAnchor,
        name: &str,
        file: &File,
        path: &Path,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        let expected = FileIdentity::from_stat(&native(rfs::fstat(file))?);
        self.expected_at(parent, name, Some(&expected))?;
        self.boundary("unlink", path, deadline)?;
        self.expected_at(
            parent,
            name,
            Some(&FileIdentity::from_stat(&native(rfs::fstat(file))?)),
        )?;
        let flags = if expected.mode & 0o170000 == 0o040000 {
            AtFlags::REMOVEDIR
        } else {
            AtFlags::empty()
        };
        native(rfs::unlinkat(&parent.fd, name, flags))?;
        self.boundary("parent-sync", path, deadline)?;
        self.filesystem
            .execute(FilesystemOperation::DirectorySync(parent.fd.as_fd()))
    }
    /// Finalizes only new staged payload/ctl files; never chmods receipts or broad directories.
    pub fn finalize_staged_mode(
        &self,
        proof: &SupportProof,
        path: &Path,
        expected: &FileIdentity,
        mode: u32,
        deadline: &Deadline,
    ) -> NativeResult<FileIdentity> {
        let _serial = self.mutation.try_lock().map_err(|_| NativeError::Busy)?;
        proof.check(self, deadline)?;
        let path = admitted_spelling(path)?;
        let app = self
            .target
            .paths
            .home
            .join("Applications/.Crosspane.app.crosspane-stage");
        let ctl = self
            .target
            .paths
            .home
            .join(".local/bin/.crosspanectl.crosspane-stage");
        if !(path.starts_with(app) || path == ctl) || !matches!(mode, 0o600 | 0o644 | 0o755) {
            return Err(NativeError::Foreign);
        }
        let file = self.open_regular(&path, false, OFlags::RDONLY)?;
        if FileIdentity::from_stat(&native(rfs::fstat(&file))?) != *expected {
            return Err(NativeError::Foreign);
        }
        self.boundary("chmod", &path, deadline)?;
        let result = (|| {
            native(rfs::fchmod(&file, Mode::from_raw_mode(mode as _)))?;
            self.boundary("file-sync", &path, deadline)?;
            self.filesystem
                .execute(FilesystemOperation::FileSync(file.as_fd()))?;
            let identity = self.fd_identity(&file, &path)?;
            if self.metadata(&path)? != Some(identity.clone()) {
                return Err(NativeError::Foreign);
            }
            self.boundary("complete", &path, deadline)?;
            Ok(identity)
        })();
        result.map_err(|_| NativeError::OutcomeUnknown)
    }
    pub fn rename_owned(
        &self,
        proof: &SupportProof,
        from: &Path,
        to: &Path,
        expected: &FileIdentity,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        let _serial = self.mutation.try_lock().map_err(|_| NativeError::Busy)?;
        proof.check(self, deadline)?;
        let (from, to) = (admitted_spelling(from)?, admitted_spelling(to)?);
        if !self.target.writable(&from) || !self.target.writable(&to) {
            return Err(NativeError::Foreign);
        }
        let (source, name) = self.parent(&from)?;
        let (destination, new_name) = self.parent(&to)?;
        self.expected_at(&source, &name, Some(expected))?;
        self.expected_at(&destination, &new_name, None)?;
        if source.identity().0 != destination.identity().0 {
            return Err(NativeError::Foreign);
        }
        source.revalidate(self)?;
        destination.revalidate(self)?;
        let (temporary, file) = self.quarantine(&source, &name, expected, &from, deadline)?;
        let result = (|| {
            self.boundary("publish", &to, deadline)?;
            self.filesystem.execute(FilesystemOperation::Rename {
                old_dir: source.fd.as_fd(),
                old: &temporary,
                new_dir: destination.fd.as_fd(),
                new: &new_name,
                flags: rfs::RenameFlags::NOREPLACE,
            })?;
            self.boundary("parent-sync", &from, deadline)?;
            self.filesystem
                .execute(FilesystemOperation::DirectorySync(source.fd.as_fd()))?;
            self.boundary("parent-sync", &to, deadline)?;
            self.filesystem
                .execute(FilesystemOperation::DirectorySync(destination.fd.as_fd()))?;
            self.expected_at(
                &destination,
                &new_name,
                Some(&FileIdentity::from_stat(&native(rfs::fstat(&file))?)),
            )?;
            source.revalidate(self)?;
            destination.revalidate(self)?;
            self.boundary("complete", &to, deadline)
        })();
        result.map_err(|_| NativeError::OutcomeUnknown)
    }
    pub fn remove_owned_leaf(
        &self,
        proof: &SupportProof,
        path: &Path,
        expected: &FileIdentity,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        let _serial = self.mutation.try_lock().map_err(|_| NativeError::Busy)?;
        proof.check(self, deadline)?;
        let path = admitted_spelling(path)?;
        if !self.target.writable(&path) || !matches!(expected.mode & 0o170000, 0o100000 | 0o040000)
        {
            return Err(NativeError::Foreign);
        }
        let (parent, name) = self.parent(&path)?;
        if expected.mode & 0o170000 == 0o040000 && !self.entries(&path, 1, deadline)?.is_empty() {
            return Err(NativeError::Foreign);
        }
        let (temporary, file) = self.quarantine(&parent, &name, expected, &path, deadline)?;
        let result = self
            .unlink_quarantined(&parent, &temporary, &file, &path, deadline)
            .and_then(|_| parent.revalidate(self))
            .and_then(|_| self.boundary("complete", &path, deadline));
        result.map_err(|_| NativeError::OutcomeUnknown)
    }
    pub fn lock(&self, proof: &SupportProof, deadline: &Deadline) -> NativeResult<InstallerLock> {
        let _serial = self.mutation.try_lock().map_err(|_| NativeError::Busy)?;
        proof.check(self, deadline)?;
        let path = self.target.installer_dir().join("lock");
        let (parent, name) = self.parent(&path)?;
        private_directory(&parent.fd, self.target.paths.uid)?;
        let prior = self.metadata(&path)?;
        if let Some(identity) = &prior {
            identity.regular(self.target.paths.uid, true)?;
        }
        let created = prior.is_none();
        self.boundary("lock-open", &path, deadline)?;
        let fd = rfs::openat(
            &parent.fd,
            name,
            OFlags::RDWR
                | OFlags::NOFOLLOW
                | OFlags::CLOEXEC
                | OFlags::NONBLOCK
                | if created {
                    OFlags::CREATE | OFlags::EXCL
                } else {
                    OFlags::empty()
                },
            Mode::from_raw_mode(0o600),
        )
        .map_err(|_| {
            if created {
                NativeError::OutcomeUnknown
            } else {
                NativeError::Unavailable
            }
        })?;
        let mut acquired = false;
        let result = (|| {
            let identity = FileIdentity::from_stat(&native(rfs::fstat(&fd))?);
            identity.regular(self.target.paths.uid, true)?;
            if prior.as_ref().is_some_and(|s| *s != identity) {
                return Err(NativeError::Foreign);
            }
            self.boundary("lock", &path, deadline)?;
            rfs::flock(&fd, FlockOperation::NonBlockingLockExclusive)
                .map_err(|_| NativeError::Busy)?;
            acquired = true;
            if created {
                self.boundary("file-sync", &path, deadline)?;
                self.filesystem
                    .execute(FilesystemOperation::FileSync(fd.as_fd()))?;
                self.boundary("parent-sync", &path, deadline)?;
                self.filesystem
                    .execute(FilesystemOperation::DirectorySync(parent.fd.as_fd()))?;
            }
            parent.revalidate(self)?;
            if self.metadata(&path)? != Some(identity) {
                return Err(NativeError::Foreign);
            }
            self.boundary("complete", &path, deadline)?;
            Ok(InstallerLock {
                _file: File::from(fd),
            })
        })();
        if created || acquired {
            result.map_err(|_| NativeError::OutcomeUnknown)
        } else {
            result
        }
    }
}
fn private_directory(fd: &OwnedFd, uid: u32) -> NativeResult<()> {
    let s = native(rfs::fstat(fd))?;
    if s.st_uid != uid || s.st_mode & 0o7777 != 0o700 {
        return Err(NativeError::Foreign);
    }
    Ok(())
}

type AudioOutcomeParent = (OwnedFd, Vec<(u64, u64)>);

impl MacNativeIo {
    /// Fixed advisory metadata only: no directory listing, bundle identity, or content read.
    /// The tuple is (file kind bits, uid, permission bits, device). Only NOENT means Absent.
    pub fn audio_metadata(
        &self,
        path: &Path,
        deadline: &Deadline,
    ) -> NativeResult<Option<(u32, u32, u32, u64)>> {
        deadline.check()?;
        if ![
            "/Library/Audio/Plug-Ins/HAL",
            "/Library/Audio/Plug-Ins/HAL/CrosspaneAudio.driver",
            "/Library/Application Support",
            "/Library/Application Support/Crosspane",
            "/Library/Application Support/Crosspane/Installer",
            "/Library/Application Support/Crosspane/Installer/previous",
        ]
        .iter()
        .any(|p| path.as_os_str() == std::ffi::OsStr::new(p))
        {
            return Err(NativeError::Invalid);
        }
        #[cfg(test)]
        let physical = self
            .target
            .test_path
            .as_ref()
            .map(|map| map(path))
            .unwrap_or_else(|| path.to_owned());
        #[cfg(not(test))]
        let physical = path;
        let identity = match rfs::statat(rfs::CWD, physical, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(s) => Some(FileIdentity::from_stat(&s)),
            Err(rustix::io::Errno::NOENT) => None,
            Err(_) => return Err(NativeError::Unavailable),
        };
        let identity = self.target.observe("audio-metadata", path, identity)?;
        deadline.check()?;
        Ok(identity.map(|s| (s.mode & 0o170000, s.uid, s.mode & 0o7777, s.device)))
    }
    /// Exactly the two root outcomes. ACLs are the lead-accepted residual; these are advisory.
    pub fn read_audio_outcome(
        &self,
        removal: bool,
        deadline: &Deadline,
    ) -> NativeResult<Option<(Vec<u8>, FileIdentity)>> {
        deadline.check()?;
        self.validate_target()?;
        let parent = self.audio_outcome_parent()?;
        let Some((fd, chain)) = parent else {
            return Ok(None);
        };
        let name = if removal {
            "audio-removal-outcome.json"
        } else {
            "audio-outcome.json"
        };
        let path = Path::new("/Library/Application Support/Crosspane/Installer").join(name);
        if ![
            "/Library/Application Support/Crosspane/Installer/audio-outcome.json",
            "/Library/Application Support/Crosspane/Installer/audio-removal-outcome.json",
        ]
        .iter()
        .any(|p| path.as_os_str() == std::ffi::OsStr::new(p))
        {
            return Err(NativeError::Invalid);
        }
        let leaf = match rfs::openat(
            &fd,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(_) => return Err(NativeError::Foreign),
        };
        let observe = |s: rfs::Stat| {
            self.target
                .observe("audio-outcome", &path, Some(FileIdentity::from_stat(&s)))?
                .ok_or(NativeError::Foreign)
        };
        let identity = observe(native(rfs::fstat(&leaf))?)?;
        if identity.uid != 0
            || identity.mode & 0o170000 != 0o100000
            || identity.mode & 0o7777 != 0o644
        {
            return Err(NativeError::Foreign);
        }
        if identity.length > 256 {
            return Err(NativeError::Oversize);
        }
        let mut bytes = Vec::new();
        let mut file = File::from(leaf);
        let mut buffer = [0; 257];
        loop {
            deadline.check()?;
            let n = native(file.read(&mut buffer))?;
            if n == 0 {
                break;
            }
            if bytes.len() + n > 256 {
                return Err(NativeError::Oversize);
            }
            bytes.extend_from_slice(&buffer[..n]);
        }
        if identity != observe(native(rfs::fstat(&file))?)?
            || identity != observe(native(rfs::statat(&fd, name, AtFlags::SYMLINK_NOFOLLOW))?)?
            || self
                .audio_outcome_parent()?
                .is_none_or(|(_, fresh)| fresh != chain)
        {
            return Err(NativeError::Foreign);
        }
        deadline.check()?;
        Ok(Some((bytes, identity)))
    }
    fn audio_outcome_parent(&self) -> NativeResult<Option<AudioOutcomeParent>> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let mut path = PathBuf::from("/Library");
        #[cfg(test)]
        let physical = self
            .target
            .test_path
            .as_ref()
            .map(|map| map(&path))
            .unwrap_or_else(|| path.clone());
        #[cfg(not(test))]
        let physical = &path;
        let mut fd = rfs::open(physical, flags, Mode::empty()).map_err(|_| NativeError::Foreign)?;
        let mut chain = Vec::new();
        for name in ["", "Application Support", "Crosspane", "Installer"] {
            if !name.is_empty() {
                path.push(name);
                fd = match rfs::openat(&fd, name, flags, Mode::empty()) {
                    Ok(fd) => fd,
                    Err(rustix::io::Errno::NOENT) if matches!(name, "Crosspane" | "Installer") => {
                        return Ok(None);
                    }
                    Err(_) => return Err(NativeError::Foreign),
                };
            }
            let s = self
                .target
                .observe(
                    "audio-ancestor",
                    &path,
                    Some(FileIdentity::from_stat(&native(rfs::fstat(&fd))?)),
                )?
                .ok_or(NativeError::Foreign)?;
            if s.uid != 0 || s.mode & 0o170000 != 0o040000 || s.mode & 0o022 != 0 {
                return Err(NativeError::Foreign);
            }
            chain.push((s.device, s.inode));
        }
        Ok(Some((fd, chain)))
    }
}
