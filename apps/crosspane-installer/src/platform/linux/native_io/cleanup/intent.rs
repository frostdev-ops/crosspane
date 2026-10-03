use super::*;
use mutation::State;

struct Temp<'a> {
    state: &'a State,
    name: String,
    file: Option<File>,
    created: Option<(u64, u64, u32, u32)>,
}
impl Drop for Temp<'_> {
    fn drop(&mut self) {
        let Some(file) = &self.file else {
            return;
        };
        let dir = &self.state.proof.0.ledger.parent;
        let Ok(d) = Deadline::new(2_000, Cancellation::default()) else {
            return;
        };
        // Cleanup retains captured authority, independently of operation cancellation/retirement.
        let proof = &self.state.proof.0;
        if proof.ledger.parent_revalidate(&proof.io, &d).is_err() {
            return;
        }
        if let (Ok(fd), Ok(named)) = (
            rfs::fstat(file),
            rfs::statat(dir, &self.name, AtFlags::SYMLINK_NOFOLLOW),
        ) && self
            .created
            .is_none_or(|id| id == (fd.st_dev, fd.st_ino, fd.st_uid, fd.st_mode))
            && fd.st_uid == proof.io.target.paths.uid
            && fd.st_mode & 0o177777 == 0o100600
            && fd.st_nlink == 1
            && snapshot::identity(&fd) == snapshot::identity(&named)
            && d.check().is_ok()
        {
            let _ = rfs::unlinkat(dir, &self.name, AtFlags::empty());
        }
    }
}
struct Backup<'a> {
    anchor: &'a Snapshot,
    io: &'a LinuxNativeIo,
    name: String,
    verified: Option<(File, [u64; 10])>,
    removed: bool,
}
impl Backup<'_> {
    fn remove(&mut self, d: &Deadline) -> Result<()> {
        self.anchor.parent_revalidate(self.io, d)?;
        let (file, id) = self.verified.as_ref().ok_or(NativeError::Foreign)?;
        self.anchor.stable_displaced(file, &self.name, *id, d)?;
        native(rfs::unlinkat(
            &self.anchor.parent,
            &self.name,
            AtFlags::empty(),
        ))?;
        self.removed = true;
        Ok(())
    }
}
impl Drop for Backup<'_> {
    fn drop(&mut self) {
        if !self.removed {
            let _ = self.anchor.restore(
                self.io,
                &self.name,
                std::ffi::OsStr::new("cleanup-intent.json"),
            );
        }
    }
}
impl CleanupLease {
    /// Opaque bounded bytes, at one fixed private path; policy/schema belongs to b1b.
    pub fn write_intent(&self, bytes: &[u8], d: &Deadline) -> Result<()> {
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(NativeError::Oversize);
        }
        let bytes = bytes.to_vec();
        self.mutate(d, move |state, d, started| {
            let io = &state.proof.0.io;
            let dir = &state.proof.0.ledger.parent;
            let mut nonce = [0u8; 16];
            aws_lc_rs::rand::fill(&mut nonce).map_err(|_| NativeError::Unavailable)?;
            let name = format!(
                ".cleanup-intent-{}",
                nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
            );
            state.before(d, started)?;
            let file = File::from(native(rfs::openat(
                dir,
                &name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            ))?);
            let mut temp = Temp {
                state,
                name,
                file: Some(file),
                created: None,
            };
            let s = state.created_stat(temp.file.as_ref().ok_or(NativeError::Invalid)?)?;
            temp.created = Some((s.st_dev, s.st_ino, s.st_uid, s.st_mode));
            if s.st_uid != io.target.paths.uid
                || s.st_mode & 0o177777 != 0o100600
                || s.st_nlink != 1
            {
                return Err(NativeError::Foreign);
            }
            state.before(d, started)?;
            let file = temp.file.as_mut().ok_or(NativeError::Invalid)?;
            io.files.apply(FileOperation::Write(file, &bytes))?;
            state.before(d, started)?;
            io.files.apply(FileOperation::FileSync(file))?;
            state.before(d, started)?;
            let mut intent = state.intent.lock().map_err(|_| NativeError::Unavailable)?;
            let mut backup = None;
            if intent.hash().is_some() {
                intent.revalidate(io, d)?;
                d.check()?;
                let name = temp.name.replace("intent", "prior");
                native(rfs::renameat_with(
                    dir,
                    "cleanup-intent.json",
                    dir,
                    &name,
                    rfs::RenameFlags::NOREPLACE,
                ))?;
                backup = Some(Backup {
                    anchor: &state.proof.0.ledger,
                    io,
                    name,
                    verified: None,
                    removed: false,
                });
                let old = backup.as_mut().ok_or(NativeError::Invalid)?;
                #[cfg(test)]
                state.hook(&state.after_intent_displace);
                old.verified = Some(intent.displaced(io, &old.name, d)?);
            }
            let named = native(rfs::statat(dir, &temp.name, AtFlags::SYMLINK_NOFOLLOW))?;
            if named.st_uid != io.target.paths.uid
                || named.st_mode & 0o177777 != 0o100600
                || named.st_nlink != 1
                || snapshot::identity(&named) != snapshot::identity(&native(rfs::fstat(file))?)
            {
                return Err(NativeError::Foreign);
            }
            state.proof.0.ledger.parent_revalidate(io, d)?;
            io.files.apply(FileOperation::IntentRename(
                dir,
                &temp.name,
                "cleanup-intent.json",
            ))?;
            intent.recorded_write(
                temp.file.take().ok_or(NativeError::Invalid)?,
                sha256(&bytes),
            )?;
            drop(intent);
            state.before(d, started)?;
            io.files.apply(FileOperation::ParentSync(dir))?;
            if let Some(mut backup) = backup {
                state.before(d, started)?;
                backup.remove(d)?;
                state.before(d, started)?;
                io.files.apply(FileOperation::ParentSync(dir))?;
            }
            Ok(())
        })
    }
}
