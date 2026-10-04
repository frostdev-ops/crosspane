use super::*;
pub(super) fn identity(s: &rfs::Stat) -> [u64; 10] {
    [
        s.st_dev,
        s.st_ino,
        s.st_uid as u64,
        s.st_mode as u64,
        s.st_nlink,
        s.st_size as u64,
        s.st_mtime as u64,
        s.st_mtime_nsec,
        s.st_ctime as u64,
        s.st_ctime_nsec,
    ]
}

fn directory_identity(s: &rfs::Stat) -> [u64; 4] {
    [s.st_dev, s.st_ino, s.st_uid as u64, s.st_mode as u64]
}
pub(super) struct Snapshot {
    pub path: PathBuf,
    pub parent: OwnedFd,
    directories: Vec<(OwnedFd, [u64; 4])>,
    file: Option<(File, [u64; 10], [u8; 32])>,
    mode: u32,
    limit: usize,
}
impl Snapshot {
    pub(super) fn parent_revalidate(&self, io: &LinuxNativeIo, d: &Deadline) -> Result<()> {
        d.check()?;
        io.validate_target()?;
        let ancestors: Vec<_> = self
            .path
            .parent()
            .ok_or(NativeError::Foreign)?
            .ancestors()
            .collect();
        if ancestors.len() != self.directories.len() {
            return Err(NativeError::Foreign);
        }
        for (path, (fd, expected)) in ancestors.into_iter().rev().zip(&self.directories) {
            d.check()?;
            let current = if path == Path::new("/") {
                native(rfs::open(
                    path,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                ))?
            } else {
                io.walk_dir(path, false)?.ok_or(NativeError::Foreign)?
            };
            if *expected != directory_identity(&native(rfs::fstat(fd))?)
                || *expected != directory_identity(&native(rfs::fstat(&current))?)
            {
                return Err(NativeError::Foreign);
            }
        }
        if self.directories.last().map(|(_, id)| *id)
            != Some(directory_identity(&native(rfs::fstat(&self.parent))?))
        {
            return Err(NativeError::Foreign);
        }
        d.check()
    }
    pub(super) fn restore(
        &self,
        io: &LinuxNativeIo,
        displaced: &str,
        name: &std::ffi::OsStr,
    ) -> Result<()> {
        let d = Deadline::new(2_000, Cancellation::default())?;
        self.parent_revalidate(io, &d)?;
        native(rfs::renameat_with(
            &self.parent,
            displaced,
            &self.parent,
            name,
            rfs::RenameFlags::NOREPLACE,
        ))
    }
    pub(super) fn fd(&self) -> Result<&File> {
        self.file
            .as_ref()
            .map(|(f, _, _)| f)
            .ok_or(NativeError::Unavailable)
    }
    pub(super) fn recorded_write(&mut self, file: File, hash: [u8; 32]) -> Result<()> {
        let s = native(rfs::fstat(&file))?;
        let named = native(rfs::statat(
            &self.parent,
            self.path.file_name().ok_or(NativeError::Invalid)?,
            AtFlags::SYMLINK_NOFOLLOW,
        ))?;
        if identity(&s) != identity(&named) {
            return Err(NativeError::Foreign);
        }
        self.file = Some((file, identity(&s), hash));
        Ok(())
    }
    pub(super) fn revalidate_removed(&self, io: &LinuxNativeIo, d: &Deadline) -> Result<()> {
        let (current, _) = Self::open(io, &self.path, self.mode, self.limit, d)?;
        if current.hash().is_some() || self.directories.len() != current.directories.len() {
            return Err(NativeError::Foreign);
        }
        for ((fd, admitted), (_, observed)) in self.directories.iter().zip(&current.directories) {
            if admitted != observed || *admitted != directory_identity(&native(rfs::fstat(fd))?) {
                return Err(NativeError::Foreign);
            }
        }
        Ok(())
    }
    pub(super) fn displaced(
        &self,
        io: &LinuxNativeIo,
        name: &str,
        d: &Deadline,
    ) -> Result<(File, [u64; 10])> {
        self.revalidate_removed(io, d)?;
        let mut file = File::from(native(rfs::openat(
            &self.parent,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ))?);
        let observed = identity(&native(rfs::fstat(&file))?);
        let (_, original, hash) = self.file.as_ref().ok_or(NativeError::Foreign)?;
        // Our rename changes ctime only; every other original identity field remains mandatory.
        if observed[..8] != original[..8] {
            return Err(NativeError::Foreign);
        }
        let mut bytes = Vec::new();
        native(
            (&mut file)
                .take(self.limit as u64 + 1)
                .read_to_end(&mut bytes),
        )?;
        if bytes.len() > self.limit || sha256(&bytes) != *hash {
            return Err(NativeError::Foreign);
        }
        self.stable_displaced(&file, name, observed, d)?;
        Ok((file, observed))
    }
    pub(super) fn stable_displaced(
        &self,
        file: &File,
        name: &str,
        expected: [u64; 10],
        d: &Deadline,
    ) -> Result<()> {
        let named = native(rfs::statat(&self.parent, name, AtFlags::SYMLINK_NOFOLLOW))?;
        if expected != identity(&native(rfs::fstat(file))?) || expected != identity(&named) {
            return Err(NativeError::Foreign);
        }
        d.check()
    }
    pub fn parent_pair(&self) -> Result<(u64, u64)> {
        let p = native(rfs::fstat(&self.parent))?;
        Ok((p.st_dev, p.st_ino))
    }
    pub fn file_pair(&self) -> Option<(u64, u64)> {
        self.file.as_ref().map(|(_, s, _)| (s[0], s[1]))
    }
    pub fn hash(&self) -> Option<[u8; 32]> {
        self.file.as_ref().map(|(_, _, h)| *h)
    }
    pub fn open(
        io: &LinuxNativeIo,
        path: &Path,
        mode: u32,
        limit: usize,
        d: &Deadline,
    ) -> Result<(Self, Vec<u8>)> {
        d.check()?;
        let (parent, name) = io.parent(path)?;
        let mut directories = Vec::new();
        let ancestors: Vec<_> = path
            .parent()
            .ok_or(NativeError::Foreign)?
            .ancestors()
            .collect();
        for ancestor in ancestors.into_iter().rev() {
            d.check()?;
            let fd = if ancestor == Path::new("/") {
                native(rfs::open(
                    ancestor,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                ))?
            } else {
                io.walk_dir(ancestor, false)?.ok_or(NativeError::Foreign)?
            };
            let captured = directory_identity(&native(rfs::fstat(&fd))?);
            directories.push((fd, captured));
        }
        if directories.last().map(|(_, s)| *s)
            != Some(directory_identity(&native(rfs::fstat(&parent))?))
        {
            return Err(NativeError::Foreign);
        }
        if mode == 0o600 && native(rfs::fstat(&parent))?.st_mode & 0o777 != 0o700 {
            return Err(NativeError::Foreign);
        }
        let fd = rfs::openat(
            &parent,
            &name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        );
        let mut bytes = Vec::new();
        let file = match fd {
            Err(rustix::io::Errno::NOENT) => None,
            Err(_) => return Err(NativeError::Foreign),
            Ok(fd) => {
                let before = native(rfs::fstat(&fd))?;
                if before.st_uid != io.target.paths.uid
                    || before.st_nlink != 1
                    || before.st_mode & 0o177777 != (0o100000 | mode)
                    || before.st_size < 0
                    || before.st_size as u64 > limit as u64
                {
                    return Err(NativeError::Foreign);
                }
                let mut file = File::from(fd);
                native((&mut file).take(limit as u64 + 1).read_to_end(&mut bytes))?;
                d.check()?;
                let after = native(rfs::fstat(&file))?;
                let named = native(rfs::statat(&parent, &name, AtFlags::SYMLINK_NOFOLLOW))?;
                if bytes.len() > limit
                    || identity(&before) != identity(&after)
                    || identity(&named) != identity(&after)
                {
                    return Err(NativeError::Foreign);
                }
                Some((file, identity(&after), sha256(&bytes)))
            }
        };
        d.check()?;
        Ok((
            Self {
                path: path.into(),
                parent,
                directories,
                file,
                mode,
                limit,
            },
            bytes,
        ))
    }
    pub fn revalidate(&self, io: &LinuxNativeIo, d: &Deadline) -> Result<()> {
        let (current, _) = Self::open(io, &self.path, self.mode, self.limit, d)?;
        if self.directories.len() != current.directories.len() {
            return Err(NativeError::Foreign);
        }
        for ((fd, admitted), (_, observed)) in self.directories.iter().zip(&current.directories) {
            if admitted != observed || *admitted != directory_identity(&native(rfs::fstat(fd))?) {
                return Err(NativeError::Foreign);
            }
        }
        match (&self.file, &current.file) {
            (None, None) => Ok(()),
            (Some((fd, a, h)), Some((_, b, k)))
                if a == b && h == k && *a == identity(&native(rfs::fstat(fd))?) =>
            {
                Ok(())
            }
            _ => Err(NativeError::Foreign),
        }
    }
}

impl Snapshot {
    pub(super) fn read_captured(
        &self,
        io: &LinuxNativeIo,
        d: &Deadline,
    ) -> Result<Option<Vec<u8>>> {
        self.parent_revalidate(io, d)?;
        let (current, bytes) = Self::open(io, &self.path, self.mode, self.limit, d)?;
        if self.directories.len() != current.directories.len() {
            return Err(NativeError::Foreign);
        }
        for ((fd, admitted), (_, observed)) in self.directories.iter().zip(&current.directories) {
            if admitted != observed || *admitted != directory_identity(&native(rfs::fstat(fd))?) {
                return Err(NativeError::Foreign);
            }
        }
        match (&self.file, &current.file) {
            (None, None) => {
                self.parent_revalidate(io, d)?;
                Ok(None)
            }
            (Some((fd, a, h)), Some((current_fd, b, k)))
                if a == b && h == k && *a == identity(&native(rfs::fstat(fd))?) =>
            {
                self.stable_displaced(
                    current_fd,
                    self.path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .ok_or(NativeError::Invalid)?,
                    *a,
                    d,
                )?;
                self.parent_revalidate(io, d)?;
                Ok(Some(bytes))
            }
            _ => Err(NativeError::Foreign),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mutation::tests::Fixture;

    #[test]
    fn cleanup_displacement_relaxes_only_own_rename_ctime_and_no_other_field() {
        for field in 0..10 {
            let f = Fixture::new(None, false, false);
            let mut entries = f.proof.0.entries.lock().unwrap();
            let snapshot = &mut entries[0].snapshot;
            let original = snapshot.file.as_ref().unwrap().1;
            rfs::renameat_with(
                &snapshot.parent,
                "member-0",
                &snapshot.parent,
                ".cleanup-del-test",
                rfs::RenameFlags::NOREPLACE,
            )
            .unwrap();
            // Private comparison seam: isolate each field independently, even those a user cannot edit.
            snapshot.file.as_mut().unwrap().1[field] ^= 1;
            let d = Deadline::new(5000, Cancellation::default()).unwrap();
            let result = snapshot.displaced(&f.proof.0.io, ".cleanup-del-test", &d);
            if field < 8 {
                assert_eq!(
                    result.unwrap_err(),
                    NativeError::Foreign,
                    "identity field {field}"
                );
            } else {
                let (file, observed) = result.unwrap();
                assert_eq!(&observed[..8], &original[..8]);
                assert_eq!(observed, identity(&rfs::fstat(&file).unwrap()));
            }
            assert!(
                rfs::statat(
                    &snapshot.parent,
                    ".cleanup-del-test",
                    AtFlags::SYMLINK_NOFOLLOW
                )
                .is_ok()
            );
        }
    }
    #[test]
    fn cleanup_displacement_requires_bounded_original_hash_even_with_exact_identity() {
        let f = Fixture::new(None, false, false);
        let mut entries = f.proof.0.entries.lock().unwrap();
        let snapshot = &mut entries[0].snapshot;
        rfs::renameat_with(
            &snapshot.parent,
            "member-0",
            &snapshot.parent,
            ".cleanup-del-test",
            rfs::RenameFlags::NOREPLACE,
        )
        .unwrap();
        snapshot.file.as_mut().unwrap().2[0] ^= 1;
        let d = Deadline::new(5000, Cancellation::default()).unwrap();
        assert_eq!(
            snapshot
                .displaced(&f.proof.0.io, ".cleanup-del-test", &d)
                .unwrap_err(),
            NativeError::Foreign
        );
        assert!(
            rfs::statat(
                &snapshot.parent,
                ".cleanup-del-test",
                AtFlags::SYMLINK_NOFOLLOW
            )
            .is_ok()
        );
    }
}
