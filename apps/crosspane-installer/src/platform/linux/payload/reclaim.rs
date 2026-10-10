//! WP-4.32: the installer owns its install paths.
//!
//! Whatever stands where Crosspane installs (another build's files, a hand-made copy, a link, a
//! directory, half-written leftovers of an interrupted run, or installer records that can't be
//! continued) is moved into a timestamped backup folder, and the install continues as a fresh
//! one. Nothing outside the fixed install paths, their parents' own installer leftovers and the
//! installer's payload records is touched; links are moved, never followed.
use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

/// At most this many backup folders are kept; the oldest go first.
pub const KEEP_BACKUPS: usize = 3;
/// Deepest directory a pruned backup may contain; anything deeper is left in place.
const PRUNE_DEPTH: usize = 32;

/// UTC `YYYYMMDD-HHMMSS` for `seconds` since the epoch (proleptic Gregorian calendar).
pub(crate) fn backup_stamp(seconds: u64) -> String {
    let days = seconds / 86_400;
    let rest = seconds % 86_400;
    // Howard Hinnant's days-from-civil inverse.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

fn is_record(name: &str) -> bool {
    let base = name.strip_suffix(".retired").unwrap_or(name);
    matches!(
        base,
        "payload-intent.json" | "payload-outcome.json" | "desktop-outcome.json"
    ) || (base.starts_with("quarantine-") && base.ends_with(".json"))
}

fn is_leftover(name: &str, installed: &[&str]) -> bool {
    let sibling = name
        .strip_prefix(".crosspane-stage-")
        .or_else(|| name.strip_prefix(".crosspane-previous-"))
        .map(|rest| rest.strip_suffix(".retired").unwrap_or(rest));
    if let Some(rest) = sibling {
        return rest.split_once('-').is_some_and(|(op, index)| {
            !op.is_empty()
                && op.bytes().all(|b| b.is_ascii_digit())
                && !index.is_empty()
                && index.bytes().all(|b| b.is_ascii_digit())
        });
    }
    installed.iter().any(|base| {
        name.strip_prefix(base).is_some_and(|rest| {
            rest == ".retired"
                || rest
                    .strip_prefix(".partial-")
                    .is_some_and(|tail| !tail.is_empty())
        })
    })
}

impl PayloadInstaller {
    /// The agent instance the new files must not be confirmed by. Normally the admitted running
    /// instance. An agent still running from a file an earlier run moved (WP-4.32) can't be
    /// admitted at the agent path any more; its recorded instance is excluded instead, which
    /// only makes confirmation stricter.
    pub(super) fn previous_instance(&self, deadline: &Deadline) -> Result<Option<u64>> {
        match self.current_instance(deadline) {
            Ok(instance) => Ok(instance),
            Err(error) => {
                let path = self.io.target().runtime_dir().join("bootstrap.json");
                if self.io.metadata(self.io.target().runtime_dir())?.is_none()
                    || self.io.metadata(&path)?.is_none()
                {
                    return Err(error);
                }
                let bytes = self.io.read(&path, 4096, true)?;
                let record = crate::agent_contract::parse_bootstrap(&bytes)
                    .map_err(|_| PayloadError::Native(NativeError::Invalid))?;
                Ok(Some(record.instance_id))
            }
        }
    }

    fn backup_root(&self) -> PathBuf {
        self.io
            .target()
            .paths()
            .state_home
            .join("crosspane/backups")
    }

    /// The backup folder that was used by this installer, if anything was saved.
    pub fn backup_used(&self) -> Option<PathBuf> {
        self.backup.lock().ok().and_then(|guard| guard.clone())
    }

    /// This run's backup folder, created on first use. Older folders beyond [`KEEP_BACKUPS`]
    /// are removed then.
    pub(super) fn backup_folder(&self, proof: &SupportProof) -> Result<PathBuf> {
        let mut guard = self
            .backup
            .lock()
            .map_err(|_| PayloadError::Native(NativeError::Unavailable))?;
        if let Some(path) = guard.as_ref() {
            return Ok(path.clone());
        }
        let root = self.backup_root();
        self.io.create_private_dir(proof, &root)?;
        let stamp = backup_stamp(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        );
        let mut name = stamp.clone();
        let mut attempt = 1;
        while self.io.metadata(&root.join(&name))?.is_some() {
            attempt += 1;
            if attempt > 100 {
                return Err(PayloadError::Native(NativeError::Busy));
            }
            name = format!("{stamp}-{attempt}");
        }
        let path = root.join(&name);
        self.io.create_private_dir(proof, &path)?;
        self.prune_backups(proof, &root, &name)?;
        *guard = Some(path.clone());
        Ok(path)
    }

    fn prune_backups(&self, proof: &SupportProof, root: &Path, current: &str) -> Result<()> {
        let (dir, _) = self.io.parent_for_mutation(proof, &root.join(current))?;
        let mut names = Vec::new();
        for entry in system(rfs::Dir::read_from(&dir))? {
            let entry = system(entry)?;
            let Ok(name) = entry.file_name().to_str() else {
                continue;
            };
            if name == "." || name == ".." || name == current {
                continue;
            }
            let stat = system(rfs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW))?;
            if stat.st_mode & 0o170000 == 0o040000 && stat.st_uid == self.io.target().paths().uid {
                names.push(name.to_owned());
            }
        }
        names.sort();
        let excess = (names.len() + 1).saturating_sub(KEEP_BACKUPS);
        for name in names.into_iter().take(excess) {
            // A folder that can't be removed completely is simply kept.
            let _ = remove_tree(&dir, &name, 0);
        }
        let _ = rfs::fsync(&dir);
        Ok(())
    }

    /// Move whatever is at `path` (any kind of entry, links included, never followed) into
    /// `folder/name`. Returns whether anything was there.
    pub(super) fn move_aside(
        &self,
        proof: &SupportProof,
        path: &Path,
        folder: &Path,
        name: &str,
    ) -> Result<bool> {
        let Some((parent, entry)) = self.parent(proof, path, false)? else {
            return Ok(false);
        };
        let stat = match rfs::statat(&parent, &entry, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(rustix::io::Errno::NOENT) => return Ok(false),
            Err(_) => return Err(PayloadError::Native(NativeError::Unavailable)),
        };
        let (destination, _) = self.io.parent_for_mutation(proof, &folder.join(name))?;
        let mut target = name.to_owned();
        for attempt in 1..=100 {
            proof.check(&self.io)?;
            match rfs::renameat_with(
                &parent,
                &entry,
                &destination,
                &target,
                RenameFlags::NOREPLACE,
            ) {
                Ok(()) => {
                    system(rfs::fsync(&parent))?;
                    system(rfs::fsync(&destination))?;
                    return Ok(true);
                }
                Err(rustix::io::Errno::EXIST) => target = format!("{name}.{attempt}"),
                Err(rustix::io::Errno::XDEV) => {
                    copy_across(&parent, &entry, &stat, &destination, &target)?;
                    system(rfs::fsync(&parent))?;
                    system(rfs::fsync(&destination))?;
                    return Ok(true);
                }
                Err(_) => return Err(PayloadError::Native(NativeError::Unavailable)),
            }
        }
        Err(PayloadError::Native(NativeError::Busy))
    }

    /// Copy the exact current bytes of an install path into the backup folder before the
    /// install replaces them. The original stays in place for the ordinary replacement.
    pub(super) fn save_copy(
        &self,
        proof: &SupportProof,
        index: usize,
        expected: [u8; 32],
    ) -> Result<()> {
        let Some(current) = self.snapshot(
            proof,
            &self.paths[index],
            Self::mode(index),
            MAX_MEMBER_BYTES,
        )?
        else {
            return Ok(());
        };
        if current.hash != expected {
            return Err(PayloadError::Pending);
        }
        let folder = self.backup_folder(proof)?;
        let name = self.paths[index]
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or(PayloadError::Invalid)?
            .to_owned();
        let (destination, _) = self.io.parent_for_mutation(proof, &folder.join(&name))?;
        let mut target = name.clone();
        for attempt in 1..=100 {
            match rfs::openat(
                &destination,
                &target,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            ) {
                Ok(fd) => {
                    let mut file = File::from(fd);
                    system(file.write_all(&current.bytes))?;
                    system(file.sync_all())?;
                    system(rfs::fsync(&destination))?;
                    return Ok(());
                }
                Err(rustix::io::Errno::EXIST) => target = format!("{name}.{attempt}"),
                Err(_) => return Err(PayloadError::Native(NativeError::Unavailable)),
            }
        }
        Err(PayloadError::Native(NativeError::Busy))
    }

    /// Clear the install paths of everything a fresh install can't build on. Holds the install
    /// lock throughout. Returns the backup folder when anything was moved.
    ///
    /// - Payload records (journals and quarantine identities, finished or not) move to
    ///   `installer/` in the backup: the install that follows is a fresh one.
    /// - An install path holding anything but a plain private file of the expected mode (a link,
    ///   a directory, another owner, extra hard links, an oversized file) is moved as it is.
    ///   A plain file stays: the install copies it into the same backup folder before it
    ///   replaces it.
    /// - Installer leftovers next to the install paths (stages, previous copies, retired and
    ///   partial files) move to `leftovers/`.
    pub fn reclaim(&self, proof: &SupportProof) -> Result<Option<PathBuf>> {
        proof.check(&self.io)?;
        self.io.validate_target()?;
        self.parent(proof, &self.state.join("install.lock"), true)?
            .ok_or(PayloadError::Native(NativeError::Unavailable))?;
        let _lease = self.io.install_lease(proof)?;
        let mut moved = false;
        // Installer records.
        if let Some((state, _)) = self.parent(proof, &self.state.join("install.lock"), false)? {
            let mut names = Vec::new();
            for entry in system(rfs::Dir::read_from(&state))? {
                let entry = system(entry)?;
                if let Ok(name) = entry.file_name().to_str()
                    && is_record(name)
                {
                    names.push(name.to_owned());
                }
            }
            names.sort();
            if !names.is_empty() {
                let folder = self.backup_folder(proof)?;
                let records = folder.join("installer");
                self.io.create_private_dir(proof, &records)?;
                for name in names {
                    moved |= self.move_aside(proof, &self.state.join(&name), &records, &name)?;
                }
            }
        }
        // Obsolete V1 leaf: ownership is the same fixed-path WP-4.32 rule, never an archive.
        let obsolete = self.io.target().paths().prefix.join(LEGACY_TUTORIAL);
        if self.io.metadata(&obsolete)?.is_some() {
            let folder = self.backup_folder(proof)?;
            moved |= self.move_aside(proof, &obsolete, &folder, "crosspane-tutorial")?;
        }
        // The install paths themselves.
        for (index, path) in self.paths.iter().enumerate() {
            let Some((parent, entry)) = self.parent(proof, path, false)? else {
                continue;
            };
            let stat = match rfs::statat(&parent, &entry, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => stat,
                Err(rustix::io::Errno::NOENT) => continue,
                Err(_) => return Err(PayloadError::Native(NativeError::Unavailable)),
            };
            let plain = stat.st_uid == self.io.target().paths().uid
                && stat.st_mode & 0o177777 == (0o100000 | Self::mode(index))
                && stat.st_nlink == 1
                && stat.st_size >= 0
                && stat.st_size as u64 <= MAX_MEMBER_BYTES as u64;
            if !plain {
                let folder = self.backup_folder(proof)?;
                moved |= self.move_aside(proof, path, &folder, &entry)?;
            }
        }
        // Installer leftovers next to the install paths.
        let mut parents: BTreeMap<PathBuf, Vec<&str>> = BTreeMap::new();
        for path in &self.paths {
            if let (Some(parent), Some(name)) =
                (path.parent(), path.file_name().and_then(|n| n.to_str()))
            {
                parents.entry(parent.to_owned()).or_default().push(name);
            }
        }
        parents
            .entry(self.io.target().paths().prefix.join("bin"))
            .or_default()
            .push("crosspane-tutorial");
        for (directory, installed) in parents {
            let Some((dir, _)) = self.parent(proof, &directory.join(installed[0]), false)? else {
                continue;
            };
            let mut names = Vec::new();
            for entry in system(rfs::Dir::read_from(&dir))? {
                let entry = system(entry)?;
                if let Ok(name) = entry.file_name().to_str()
                    && is_leftover(name, &installed)
                {
                    names.push(name.to_owned());
                }
            }
            names.sort();
            if names.is_empty() {
                continue;
            }
            let folder = self.backup_folder(proof)?.join("leftovers");
            self.io.create_private_dir(proof, &folder)?;
            for name in names {
                moved |= self.move_aside(proof, &directory.join(&name), &folder, &name)?;
            }
        }
        Ok(if moved { self.backup_used() } else { None })
    }
}

/// Cross-device fallback for [`PayloadInstaller::move_aside`]: a plain file or a link is copied
/// and then unlinked. Anything else stays where it is.
fn copy_across(
    parent: &OwnedFd,
    entry: &str,
    stat: &rfs::Stat,
    destination: &OwnedFd,
    target: &str,
) -> Result<()> {
    match stat.st_mode & 0o170000 {
        0o100000 => {
            let fd = system(rfs::openat(
                parent,
                entry,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            ))?;
            let opened = system(rfs::fstat(&fd))?;
            if (opened.st_dev, opened.st_ino) != (stat.st_dev, stat.st_ino) {
                return Err(PayloadError::Native(NativeError::Unavailable));
            }
            let mut bytes = Vec::new();
            system(
                File::from(fd)
                    .take(super::super::native_io::MAX_FILE_BYTES as u64 + 1)
                    .read_to_end(&mut bytes),
            )?;
            if bytes.len() > super::super::native_io::MAX_FILE_BYTES {
                return Err(PayloadError::Native(NativeError::Oversize));
            }
            let mut copy = File::from(system(rfs::openat(
                destination,
                target,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            ))?);
            system(copy.write_all(&bytes))?;
            system(copy.sync_all())?;
            system(rfs::unlinkat(parent, entry, AtFlags::empty()))
        }
        0o120000 => {
            let link = system(rfs::readlinkat(parent, entry, Vec::new()))?;
            system(rfs::symlinkat(&link, destination, target))?;
            system(rfs::unlinkat(parent, entry, AtFlags::empty()))
        }
        _ => Err(PayloadError::Native(NativeError::Unavailable)),
    }
}

/// Remove one backup folder below `dir` without following any link.
fn remove_tree(dir: &OwnedFd, name: &str, depth: usize) -> Result<()> {
    if depth > PRUNE_DEPTH {
        return Err(PayloadError::Native(NativeError::Oversize));
    }
    let stat = system(rfs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW))?;
    if stat.st_mode & 0o170000 != 0o040000 {
        return system(rfs::unlinkat(dir, name, AtFlags::empty()));
    }
    let child = system(rfs::openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ))?;
    let opened = system(rfs::fstat(&child))?;
    if (opened.st_dev, opened.st_ino) != (stat.st_dev, stat.st_ino) {
        return Err(PayloadError::Native(NativeError::Unavailable));
    }
    let mut names = Vec::new();
    for entry in system(rfs::Dir::read_from(&child))? {
        let entry = system(entry)?;
        if let Ok(entry) = entry.file_name().to_str()
            && entry != "."
            && entry != ".."
        {
            names.push(entry.to_owned());
        }
    }
    for entry in names {
        remove_tree(&child, &entry, depth + 1)?;
    }
    system(rfs::unlinkat(dir, name, AtFlags::REMOVEDIR))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_are_utc_calendar_times() {
        assert_eq!(backup_stamp(0), "19700101-000000");
        assert_eq!(backup_stamp(951_782_400), "20000229-000000");
        assert_eq!(backup_stamp(1_791_201_845), "20261005-120405");
    }

    #[test]
    fn only_installer_leftovers_and_records_are_recognized() {
        let installed = ["crosspane-agent", "crosspanectl"];
        for name in [
            ".crosspane-stage-41-0",
            ".crosspane-previous-7-3",
            ".crosspane-previous-7-3.retired",
            "crosspane-agent.partial-12-0",
            "crosspanectl.retired",
        ] {
            assert!(is_leftover(name, &installed), "{name}");
        }
        for name in [
            "crosspane-agent",
            ".crosspane-stage-",
            ".crosspane-stage-x-1",
            "other.partial-1",
            "crosspane-agent.partial-",
            "notes.txt",
        ] {
            assert!(!is_leftover(name, &installed), "{name}");
        }
        for name in [
            "payload-intent.json",
            "payload-outcome.json",
            "payload-intent.json.retired",
            "quarantine-00ff.json",
            "desktop-outcome.json",
        ] {
            assert!(is_record(name), "{name}");
        }
        for name in [
            "install.lock",
            "firewall.json",
            "resume.json",
            "payload.json",
        ] {
            assert!(!is_record(name), "{name}");
        }
    }
}
