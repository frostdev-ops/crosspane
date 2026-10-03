use super::*;
fn identity(s: &rfs::Stat) -> [u64; 10] {
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
