//! Payload files (L9): the approved inventory, source verification, hash-checked placement.

use super::{
    elevated_kit,
    engine::text,
    error::{SetupError, SetupResult, note_io},
    legacy::Backup,
    paths::{self, Paths, PayloadFile},
    payload::inventory::{ApprovedInventory, PayloadRole, pe_header},
};
use aws_lc_rs::digest::{Context, SHA256};
use crosspane_installer_core::elevated::kit::{KitFile, KitManifest};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// The `machine` field of an x64 PE image.
const MACHINE_X64: u16 = 0x8664;
/// How much of an image is read to check its PE header (`pe_header` looks up to 1 MiB in).
const HEADER_BYTES: u64 = 1024 * 1024;
/// Size of the buffer that hashing and copying read through.
const CHUNK: usize = 1024 * 1024;
/// How long one file's copy, hash and rename keep retrying while another program holds it open.
const BUSY_BUDGET: Duration = Duration::from_secs(5);
/// The pause between two such retries.
const BUSY_PAUSE: Duration = Duration::from_millis(100);
/// Shown when the running installer's own file cannot be read.
const OWN_FILE_UNREADABLE: &str = "Setup couldn't read its own file. Try again.";

/// The pinned size and digest of one payload file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pin {
    pub file: PayloadFile,
    pub size: u64,
    pub sha256: [u8; 32],
    pub pe: bool,
}

/// Stand-in returned by `Inventory::pin` for a file the inventory does not list.
static UNLISTED: Pin = Pin {
    file: PayloadFile::Agent,
    size: 0,
    sha256: [0; 32],
    pe: false,
};

/// Every payload file's pin, the agent's version, and the elevated setup kit's manifest.
#[derive(Debug)]
pub(crate) struct Inventory {
    pins: Vec<Pin>,
    version: String,
    kit: Option<KitManifest>,
}

impl Inventory {
    /// `ApprovedInventory::embedded()` (3 PE pins) + `elevated_kit::embedded()` (4 kit pins) +
    /// sha256/size of `std::env::current_exe()` for `Installer`. `Err(before(text::NO_INVENTORY))`.
    pub(crate) fn embedded() -> SetupResult<Self> {
        let approved = ApprovedInventory::embedded().map_err(|_| no_inventory())?;
        let kit = elevated_kit::embedded().ok_or_else(no_inventory)?;
        let mut pins = Vec::with_capacity(PayloadFile::ALL.len());
        for (file, role) in [
            (PayloadFile::Agent, PayloadRole::Agent),
            (PayloadFile::Ui, PayloadRole::Ui),
            (PayloadFile::Ctl, PayloadRole::Ctl),
        ] {
            let approved_pe = approved.role(role).map_err(|_| no_inventory())?;
            pins.push(Pin {
                file,
                size: approved_pe.size(),
                sha256: approved_pe.sha256(),
                pe: true,
            });
        }
        let version = approved
            .role(PayloadRole::Agent)
            .map_err(|_| no_inventory())?
            .version()
            .to_owned();
        for (file, kit_file) in [
            (PayloadFile::Helper, KitFile::Helper),
            (PayloadFile::DriverInf, KitFile::DriverInf),
            (PayloadFile::DriverCat, KitFile::DriverCatalog),
            (PayloadFile::DriverDll, KitFile::DriverBinary),
        ] {
            let pin = kit.pin(kit_file);
            pins.push(Pin {
                file,
                size: pin.size,
                sha256: pin.sha256,
                pe: false,
            });
        }
        let (sha256, size) = std::env::current_exe()
            .and_then(|exe| sha256_file(&exe))
            .map_err(|error| {
                note_io("hash the running installer", &error);
                SetupError::before(OWN_FILE_UNREADABLE)
            })?;
        pins.push(Pin {
            file: PayloadFile::Installer,
            size,
            sha256,
            pe: false,
        });
        Ok(Self {
            pins,
            version,
            kit: Some(kit),
        })
    }

    #[cfg(test)]
    pub(crate) fn for_tests(pins: Vec<Pin>, version: &str) -> Self {
        Self {
            pins,
            version: version.to_owned(),
            kit: None,
        }
    }

    pub(crate) fn pin(&self, file: PayloadFile) -> &Pin {
        self.pins
            .iter()
            .find(|pin| pin.file == file)
            .unwrap_or(&UNLISTED)
    }

    /// The agent's `PeFacts.version`.
    pub(crate) fn version(&self) -> &str {
        &self.version
    }

    pub(crate) fn kit(&self) -> Option<&KitManifest> {
        self.kit.as_ref()
    }

    /// Whether the inventory pins every payload file.
    fn lists_all(&self) -> bool {
        PayloadFile::ALL
            .iter()
            .all(|file| self.pins.iter().any(|pin| pin.file == *file))
    }
}

/// The refusal for a build whose inventories are missing or unusable.
fn no_inventory() -> SetupError {
    SetupError::before(text::NO_INVENTORY)
}

/// Writes one setup-log line. Unit tests skip it, so they never write the real log.
fn log(args: std::fmt::Arguments<'_>) {
    #[cfg(not(test))]
    super::setup_log::note(args);
    #[cfg(test)]
    let _ = args;
}

/// What one installed file looks like against its pin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FileState {
    Ok,
    Missing,
    Different,
}

/// Each payload file's state in the install dir.
pub(crate) fn inspect(paths: &Paths, inventory: &Inventory) -> Vec<(PayloadFile, FileState)> {
    PayloadFile::ALL
        .iter()
        .map(|file| (*file, state_of(&paths.file(*file), inventory.pin(*file))))
        .collect()
}

/// Missing if `path` does not exist, Ok if it is a plain file of the pinned size and digest,
/// Different otherwise (including a folder or a link in its place, or a file that cannot be read).
fn state_of(path: &Path, pin: &Pin) -> FileState {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return FileState::Missing,
        Err(_) => return FileState::Different,
    };
    if !metadata.file_type().is_file() || metadata.len() != pin.size {
        return FileState::Different;
    }
    match sha256_file(path) {
        Ok((digest, size)) if digest == pin.sha256 && size == pin.size => FileState::Ok,
        _ => FileState::Different,
    }
}

/// A verified payload folder (or none) plus the running installer's own image.
#[derive(Debug)]
pub(crate) struct Sources {
    payload: Option<PathBuf>,
    installer: PathBuf,
}

impl Sources {
    pub(crate) fn has_payload(&self) -> bool {
        self.payload.is_some()
    }
}

/// Verifies EVERY source in `payload` (size, sha256, `pe_header` x64 for PE pins) before returning.
/// A missing folder = `Sources` without a payload. Any mismatch = `Err(before(text::SOURCES_DAMAGED))`.
pub(crate) fn sources(payload: Option<&Path>, inventory: &Inventory) -> SetupResult<Sources> {
    if !inventory.lists_all() {
        return Err(no_inventory());
    }
    let installer = std::env::current_exe().map_err(|error| {
        note_io("locate the running installer", &error);
        SetupError::before(OWN_FILE_UNREADABLE)
    })?;
    let Some(folder) = payload.filter(|folder| folder.is_dir()) else {
        return Ok(Sources {
            payload: None,
            installer,
        });
    };
    for file in PayloadFile::ALL {
        if file != PayloadFile::Installer {
            verify_source(folder, file, inventory.pin(file))?;
        }
    }
    Ok(Sources {
        payload: Some(folder.to_path_buf()),
        installer,
    })
}

/// Checks one file of the payload folder against its pin: a plain file of the pinned size and
/// digest and, for a PE pin, an x64 image. Reads only; the log line names the file.
fn verify_source(folder: &Path, file: PayloadFile, pin: &Pin) -> SetupResult<()> {
    let name = file.relative();
    let path = folder.join(name);
    let damaged = |why: &str| {
        log(format_args!("payload file {name} {why}"));
        SetupError::before(text::SOURCES_DAMAGED)
    };
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(damaged("is missing"));
        }
        Err(error) => {
            note_io(&format!("read payload file {name}"), &error);
            return Err(damaged("can't be read"));
        }
    };
    if !metadata.file_type().is_file() {
        return Err(damaged("is not a plain file"));
    }
    if metadata.len() != pin.size {
        return Err(damaged("has the wrong size"));
    }
    let deadline = Instant::now() + BUSY_BUDGET;
    match retrying(deadline, || sha256_file(&path)) {
        Ok((digest, size)) if digest == pin.sha256 && size == pin.size => {}
        Ok(_) => return Err(damaged("doesn't match the approved file")),
        Err(error) => {
            note_io(&format!("hash payload file {name}"), &error);
            return Err(damaged("can't be read"));
        }
    }
    if pin.pe {
        match retrying(deadline, || is_x64_pe(&path, pin.size)) {
            Ok(true) => {}
            Ok(false) => return Err(damaged("is not an x64 program")),
            Err(error) => {
                note_io(&format!("read payload file {name}"), &error);
                return Err(damaged("can't be read"));
            }
        }
    }
    Ok(())
}

/// Whether the first megabyte of `path` is the header of an x64 PE image of total size `size`.
fn is_x64_pe(path: &Path, size: u64) -> io::Result<bool> {
    let mut head = Vec::new();
    File::open(path)?
        .take(HEADER_BYTES)
        .read_to_end(&mut head)?;
    Ok(matches!(pe_header(&head, size), Ok((MACHINE_X64, _))))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FilesOutcome {
    Unchanged,
    Replaced(Vec<PayloadFile>),
    NeedsDownloadedInstaller(Vec<PayloadFile>),
}

/// Per file: skip if Ok && !complete; else copy → `<name>.new`, `sync_all`, re-hash, rename the
/// existing file into the backup, rename .new into place (roll the backup rename back if the last
/// rename fails). Source == target (installed installer running Repair) is skipped. Retries
/// errors 32/5/33 for ≤5 s.
pub(crate) fn place(
    paths: &Paths,
    inventory: &Inventory,
    sources: &Sources,
    complete: bool,
    backup: &mut Backup,
) -> SetupResult<FilesOutcome> {
    if !inventory.lists_all() {
        return Err(no_inventory());
    }
    let driver = paths.install.join("driver");
    if let Err(error) = fs::create_dir_all(&driver) {
        note_io("create the install folder", &error);
        return Err(SetupError::after(
            "Setup couldn't create Crosspane's folder.",
        ));
    }
    let mut replaced = Vec::new();
    let mut unrestorable = Vec::new();
    for file in PayloadFile::ALL {
        let pin = inventory.pin(file);
        let target = paths.file(file);
        let source = if file == PayloadFile::Installer {
            if paths::same_path(&sources.installer, &target) {
                continue;
            }
            sources.installer.clone()
        } else if let Some(folder) = &sources.payload {
            folder.join(file.relative())
        } else {
            if state_of(&target, pin) != FileState::Ok {
                unrestorable.push(file);
            }
            continue;
        };
        if !complete && state_of(&target, pin) == FileState::Ok {
            continue;
        }
        replace(file, &source, &target, pin, backup)?;
        replaced.push(file);
    }
    Ok(if !unrestorable.is_empty() {
        FilesOutcome::NeedsDownloadedInstaller(unrestorable)
    } else if !replaced.is_empty() {
        FilesOutcome::Replaced(replaced)
    } else {
        FilesOutcome::Unchanged
    })
}

/// Puts the bytes of `source` at `target`: copy beside it as `<name>.new`, flush and re-hash the
/// copy, move the existing file into the backup, rename the copy into place and re-hash the result.
/// If the last rename fails, the backed-up file is moved back.
fn replace(
    file: PayloadFile,
    source: &Path,
    target: &Path,
    pin: &Pin,
    backup: &mut Backup,
) -> SetupResult<()> {
    let name = file.relative();
    let failed = || SetupError::after(format!("Setup couldn't update {name}. Try again."));
    let fresh = with_suffix(target, ".new");
    let deadline = Instant::now() + BUSY_BUDGET;

    if let Err(error) = retrying(deadline, || copy_synced(source, &fresh)) {
        note_io(&format!("copy {name}"), &error);
        let _ = fs::remove_file(&fresh);
        return Err(failed());
    }
    match retrying(deadline, || sha256_file(&fresh)) {
        Ok((digest, size)) if digest == pin.sha256 && size == pin.size => {}
        Ok(_) => {
            log(format_args!(
                "copy of {name} doesn't match the approved file"
            ));
            let _ = fs::remove_file(&fresh);
            return Err(SetupError::after(text::SOURCES_DAMAGED));
        }
        Err(error) => {
            note_io(&format!("hash the copy of {name}"), &error);
            let _ = fs::remove_file(&fresh);
            return Err(failed());
        }
    }

    let stashed = match fs::symlink_metadata(target) {
        Ok(_) => match retrying(deadline, || backup.stash(target, "files")) {
            Ok(moved) => Some(moved),
            Err(error) => {
                note_io(&format!("back up {name}"), &error);
                let _ = fs::remove_file(&fresh);
                return Err(failed());
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            note_io(&format!("look at {name}"), &error);
            let _ = fs::remove_file(&fresh);
            return Err(failed());
        }
    };

    if let Err(error) = retrying(deadline, || fs::rename(&fresh, target)) {
        note_io(&format!("put {name} in place"), &error);
        if let Some(moved) = &stashed {
            let restore = Instant::now() + BUSY_BUDGET;
            if let Err(back) = retrying(restore, || fs::rename(moved, target)) {
                note_io(&format!("restore {name}"), &back);
            }
        }
        let _ = fs::remove_file(&fresh);
        return Err(failed());
    }

    match retrying(deadline, || sha256_file(target)) {
        Ok((digest, size)) if digest == pin.sha256 && size == pin.size => Ok(()),
        Ok(_) => {
            log(format_args!(
                "{name} doesn't match the approved file after placing it"
            ));
            Err(failed())
        }
        Err(error) => {
            note_io(&format!("hash {name} after placing it"), &error);
            Err(failed())
        }
    }
}

/// Copies `source` to `fresh` (created or truncated) and flushes it to disk.
fn copy_synced(source: &Path, fresh: &Path) -> io::Result<()> {
    let mut input = File::open(source)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(fresh)?;
    let mut buffer = vec![0_u8; CHUNK];
    loop {
        let read = match input.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        output.write_all(&buffer[..read])?;
    }
    output.sync_all()
}

/// `path` with `suffix` appended to its last component.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Runs `op`, and runs it again after a pause while it fails because another program holds the
/// file (access denied, sharing violation or lock violation: OS errors 5, 32 and 33) and
/// `deadline` has not passed. Virus scanners cause these briefly after a file is written.
fn retrying<T>(deadline: Instant, mut op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    loop {
        match op() {
            Err(error)
                if matches!(error.raw_os_error(), Some(5 | 32 | 33))
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(BUSY_PAUSE);
            }
            other => return other,
        }
    }
}

/// Streams `path` through SHA-256 in 1 MiB chunks. Returns the digest and the byte count.
pub(crate) fn sha256_file(path: &Path) -> io::Result<([u8; 32], u64)> {
    let mut file = File::open(path)?;
    let mut context = Context::new(&SHA256);
    let mut buffer = vec![0_u8; CHUNK];
    let mut size = 0_u64;
    loop {
        let read = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        context.update(&buffer[..read]);
        size += read as u64;
    }
    let digest: [u8; 32] = context
        .finish()
        .as_ref()
        .try_into()
        .map_err(|_| io::Error::other("unexpected digest length"))?;
    Ok((digest, size))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aws_lc_rs::digest::digest;

    /// A fresh folder under the worktree's `target\`, never anywhere a real install lives.
    fn scratch(name: &str) -> PathBuf {
        let target = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap()
            .join("target")
            .join("w41e-scratch");
        let dir = target.join(format!("files-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sha(bytes: &[u8]) -> [u8; 32] {
        digest(&SHA256, bytes).as_ref().try_into().unwrap()
    }

    /// The fake bytes of `file` in generation `generation`.
    fn bytes_of(file: PayloadFile, generation: u32) -> Vec<u8> {
        format!("{file:?} generation {generation}").into_bytes()
    }

    fn pin_for(file: PayloadFile, bytes: &[u8], pe: bool) -> Pin {
        Pin {
            file,
            size: bytes.len() as u64,
            sha256: sha(bytes),
            pe,
        }
    }

    struct Fixture {
        root: PathBuf,
        paths: Paths,
        payload: PathBuf,
        installer: PathBuf,
        inventory: Inventory,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = scratch(name);
            let paths = Paths::at(
                &root.join("local"),
                &root.join("roaming"),
                &root.join("temp"),
            )
            .unwrap();
            let payload = root.join("payload");
            let installer = root.join("downloaded").join("crosspane-installer.exe");
            fs::create_dir_all(payload.join("driver")).unwrap();
            fs::create_dir_all(installer.parent().unwrap()).unwrap();
            let mut pins = Vec::new();
            for file in PayloadFile::ALL {
                let bytes = bytes_of(file, 1);
                let path = if file == PayloadFile::Installer {
                    installer.clone()
                } else {
                    payload.join(file.relative())
                };
                fs::write(path, &bytes).unwrap();
                pins.push(pin_for(file, &bytes, false));
            }
            let inventory = Inventory::for_tests(pins, "1.2.3");
            Self {
                root,
                paths,
                payload,
                installer,
                inventory,
            }
        }

        /// The payload folder, verified, with the downloaded fake as the running installer.
        fn sources(&self) -> Sources {
            let verified = sources(Some(&self.payload), &self.inventory).unwrap();
            Sources {
                payload: verified.payload,
                installer: self.installer.clone(),
            }
        }

        /// A `Sources` without a payload folder whose installer is the downloaded fake.
        fn bare(&self) -> Sources {
            Sources {
                payload: None,
                installer: self.installer.clone(),
            }
        }

        fn place(&self, sources: &Sources, complete: bool) -> SetupResult<FilesOutcome> {
            let mut backup = Backup::new(&self.paths);
            place(&self.paths, &self.inventory, sources, complete, &mut backup)
        }

        fn state(&self, file: PayloadFile) -> FileState {
            inspect(&self.paths, &self.inventory)
                .into_iter()
                .find(|(listed, _)| *listed == file)
                .unwrap()
                .1
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn backup_dirs(paths: &Paths) -> Vec<PathBuf> {
        match fs::read_dir(&paths.backups) {
            Ok(read) => read.map(|entry| entry.unwrap().path()).collect(),
            Err(_) => Vec::new(),
        }
    }

    #[test]
    fn sha256_file_matches_a_known_vector_and_a_chunk_crossing_file() {
        let dir = scratch("sha");
        let abc = dir.join("abc");
        fs::write(&abc, b"abc").unwrap();
        let (digest, size) = sha256_file(&abc).unwrap();
        assert_eq!(size, 3);
        assert_eq!(
            digest,
            [
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
                0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
                0xf2, 0x00, 0x15, 0xad
            ]
        );
        let big = dir.join("big");
        let bytes: Vec<u8> = (0..(CHUNK * 2 + 4321)).map(|n| (n % 251) as u8).collect();
        fs::write(&big, &bytes).unwrap();
        assert_eq!(
            sha256_file(&big).unwrap(),
            (sha(&bytes), bytes.len() as u64)
        );
        assert!(sha256_file(&dir.join("missing")).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn first_placement_replaces_everything_and_then_nothing_changes() {
        let fixture = Fixture::new("unchanged");
        let sources = fixture.sources();
        assert!(sources.has_payload());
        assert!(
            inspect(&fixture.paths, &fixture.inventory)
                .iter()
                .all(|(_, state)| *state == FileState::Missing)
        );
        let outcome = fixture.place(&sources, false);
        assert_eq!(
            outcome.unwrap(),
            FilesOutcome::Replaced(PayloadFile::ALL.to_vec())
        );
        assert!(
            inspect(&fixture.paths, &fixture.inventory)
                .iter()
                .all(|(_, state)| *state == FileState::Ok)
        );
        // Nothing was replaced, so nothing was backed up.
        assert!(backup_dirs(&fixture.paths).is_empty());
        let again = fixture.place(&sources, false);
        assert_eq!(again.unwrap(), FilesOutcome::Unchanged);
        assert!(backup_dirs(&fixture.paths).is_empty());
        // The `.new` copies never linger.
        for file in PayloadFile::ALL {
            assert!(!with_suffix(&fixture.paths.file(file), ".new").exists());
        }
    }

    #[test]
    fn a_changed_target_is_replaced_and_its_old_bytes_are_backed_up() {
        let fixture = Fixture::new("changed");
        let sources = fixture.sources();
        fixture.place(&sources, false).unwrap();
        let agent = fixture.paths.file(PayloadFile::Agent);
        fs::write(&agent, b"old bytes").unwrap();
        assert_eq!(fixture.state(PayloadFile::Agent), FileState::Different);

        let outcome = fixture.place(&sources, false);
        assert_eq!(
            outcome.unwrap(),
            FilesOutcome::Replaced(vec![PayloadFile::Agent])
        );
        assert_eq!(fixture.state(PayloadFile::Agent), FileState::Ok);
        let folders = backup_dirs(&fixture.paths);
        assert_eq!(folders.len(), 1);
        let kept = folders[0].join("files").join("crosspane-agent.exe");
        assert_eq!(fs::read(kept).unwrap(), b"old bytes");
        assert!(!with_suffix(&agent, ".new").exists());
    }

    #[test]
    fn complete_recopies_a_file_that_already_matches() {
        let fixture = Fixture::new("complete");
        let sources = fixture.sources();
        fixture.place(&sources, false).unwrap();
        let outcome = fixture.place(&sources, true);
        assert_eq!(
            outcome.unwrap(),
            FilesOutcome::Replaced(PayloadFile::ALL.to_vec())
        );
        // Every previous copy went into the backup, and every file is intact.
        let folders = backup_dirs(&fixture.paths);
        assert_eq!(folders.len(), 1);
        let kept = folders[0].join("files");
        assert_eq!(fs::read_dir(&kept).unwrap().count(), 8);
        assert_eq!(
            fs::read(kept.join("crosspane-agent.exe")).unwrap(),
            bytes_of(PayloadFile::Agent, 1)
        );
        assert!(
            inspect(&fixture.paths, &fixture.inventory)
                .iter()
                .all(|(_, state)| *state == FileState::Ok)
        );
    }

    #[test]
    fn the_installed_installer_is_never_copied_over_itself() {
        let fixture = Fixture::new("selfcopy");
        fixture.place(&fixture.sources(), false).unwrap();
        let installed = fixture.paths.file(PayloadFile::Installer);
        // Repair from the installed copy, without a payload folder.
        let bare = Sources {
            payload: None,
            installer: installed.clone(),
        };
        assert_eq!(fixture.place(&bare, true).unwrap(), FilesOutcome::Unchanged);
        assert!(backup_dirs(&fixture.paths).is_empty());
        // Complete reinstall from a payload folder, with the installed copy running.
        let running = Sources {
            payload: Some(fixture.payload.clone()),
            installer: installed,
        };
        let expected: Vec<PayloadFile> = PayloadFile::ALL
            .into_iter()
            .filter(|file| *file != PayloadFile::Installer)
            .collect();
        assert_eq!(
            fixture.place(&running, true).unwrap(),
            FilesOutcome::Replaced(expected)
        );
        assert_eq!(fixture.state(PayloadFile::Installer), FileState::Ok);
    }

    #[test]
    fn a_damaged_source_is_refused_before_anything_is_written() {
        let fixture = Fixture::new("damaged");
        // Same length, different bytes.
        let mut bytes = bytes_of(PayloadFile::Ui, 1);
        bytes[0] ^= 0xff;
        fs::write(fixture.payload.join("crosspane-ui.exe"), bytes).unwrap();
        let error = sources(Some(&fixture.payload), &fixture.inventory).unwrap_err();
        assert!(!error.changed);
        assert_eq!(error.text, text::SOURCES_DAMAGED);
        assert!(!fixture.paths.install.exists());

        // Wrong length.
        fs::write(fixture.payload.join("crosspane-ui.exe"), b"short").unwrap();
        let error = sources(Some(&fixture.payload), &fixture.inventory).unwrap_err();
        assert!(!error.changed);

        // Restored, then one file removed.
        fs::write(
            fixture.payload.join("crosspane-ui.exe"),
            bytes_of(PayloadFile::Ui, 1),
        )
        .unwrap();
        assert!(sources(Some(&fixture.payload), &fixture.inventory).is_ok());
        fs::remove_file(fixture.payload.join(r"driver\CrosspaneIdd.cat")).unwrap();
        let error = sources(Some(&fixture.payload), &fixture.inventory).unwrap_err();
        assert!(!error.changed);
        assert_eq!(error.text, text::SOURCES_DAMAGED);
        assert!(!fixture.paths.install.exists());
    }

    #[test]
    fn a_missing_or_plain_file_payload_means_no_payload() {
        let fixture = Fixture::new("nopayload");
        let none = sources(None, &fixture.inventory).unwrap();
        assert!(!none.has_payload());
        let absent = sources(Some(&fixture.root.join("nowhere")), &fixture.inventory).unwrap();
        assert!(!absent.has_payload());
        let file = fixture.root.join("a-file");
        fs::write(&file, b"x").unwrap();
        let not_a_folder = sources(Some(&file), &fixture.inventory).unwrap();
        assert!(!not_a_folder.has_payload());
        assert_eq!(not_a_folder.installer, std::env::current_exe().unwrap());
    }

    #[test]
    fn without_a_payload_missing_files_need_the_downloaded_installer() {
        let fixture = Fixture::new("needs");
        let bare = fixture.bare();
        let outcome = fixture.place(&bare, false);
        let expected: Vec<PayloadFile> = PayloadFile::ALL
            .into_iter()
            .filter(|file| *file != PayloadFile::Installer)
            .collect();
        assert_eq!(
            outcome.unwrap(),
            FilesOutcome::NeedsDownloadedInstaller(expected)
        );
        // The installer itself could be placed from the downloaded copy.
        assert_eq!(fixture.state(PayloadFile::Installer), FileState::Ok);
        assert_eq!(fixture.state(PayloadFile::Agent), FileState::Missing);

        // A damaged installed file is also unrestorable without a payload; Ok files are not.
        fixture.place(&fixture.sources(), false).unwrap();
        fs::write(fixture.paths.file(PayloadFile::Ctl), b"broken").unwrap();
        let outcome = fixture.place(&bare, false);
        assert_eq!(
            outcome.unwrap(),
            FilesOutcome::NeedsDownloadedInstaller(vec![PayloadFile::Ctl])
        );
        assert_eq!(
            fs::read(fixture.paths.file(PayloadFile::Ctl)).unwrap(),
            b"broken"
        );

        // Once everything matches, no payload is needed.
        fixture.place(&fixture.sources(), false).unwrap();
        let outcome = fixture.place(&bare, false);
        assert_eq!(outcome.unwrap(), FilesOutcome::Unchanged);
    }

    #[test]
    fn a_copy_that_does_not_match_its_pin_is_removed_and_the_target_is_kept() {
        let fixture = Fixture::new("rehash");
        let sources = fixture.sources();
        fixture.place(&sources, false).unwrap();
        // The payload changes after it was verified, and the installed file is out of date.
        fs::write(
            fixture.payload.join("crosspane-agent.exe"),
            b"swapped bytes",
        )
        .unwrap();
        let agent = fixture.paths.file(PayloadFile::Agent);
        fs::write(&agent, b"installed bytes").unwrap();
        let error = fixture.place(&sources, false).unwrap_err();
        assert!(error.changed);
        assert_eq!(error.text, text::SOURCES_DAMAGED);
        assert_eq!(fs::read(&agent).unwrap(), b"installed bytes");
        assert!(!with_suffix(&agent, ".new").exists());
        assert!(backup_dirs(&fixture.paths).is_empty());
    }

    /// A 256-byte image that `pe_header` accepts, for `machine`.
    fn fake_pe(machine: u16) -> Vec<u8> {
        let mut image = vec![0_u8; 256];
        image[..2].copy_from_slice(b"MZ");
        image[60..64].copy_from_slice(&64_u32.to_le_bytes());
        image[64..68].copy_from_slice(b"PE\0\0");
        image[68..70].copy_from_slice(&machine.to_le_bytes());
        image[84..86].copy_from_slice(&112_u16.to_le_bytes());
        image[88..90].copy_from_slice(&0x20b_u16.to_le_bytes());
        image[156..158].copy_from_slice(&2_u16.to_le_bytes());
        image
    }

    #[test]
    fn pe_pins_require_an_x64_image() {
        let root = scratch("pe");
        let payload = root.join("payload");
        fs::create_dir_all(payload.join("driver")).unwrap();
        let mut pins = Vec::new();
        for file in PayloadFile::ALL {
            let bytes = if file == PayloadFile::Agent {
                fake_pe(MACHINE_X64)
            } else {
                bytes_of(file, 1)
            };
            if file != PayloadFile::Installer {
                fs::write(payload.join(file.relative()), &bytes).unwrap();
            }
            pins.push(pin_for(file, &bytes, file == PayloadFile::Agent));
        }
        let inventory = Inventory::for_tests(pins, "1.2.3");
        assert!(sources(Some(&payload), &inventory).is_ok());

        // An ARM64 image with a matching pin is still not an x64 program.
        let arm = fake_pe(0xaa64);
        fs::write(payload.join("crosspane-agent.exe"), &arm).unwrap();
        let mut arm_pins = Vec::new();
        for file in PayloadFile::ALL {
            let pin = inventory.pin(file).clone();
            arm_pins.push(if file == PayloadFile::Agent {
                pin_for(file, &arm, true)
            } else {
                pin
            });
        }
        let arm_inventory = Inventory::for_tests(arm_pins, "1.2.3");
        let error = sources(Some(&payload), &arm_inventory).unwrap_err();
        assert!(!error.changed);
        assert_eq!(error.text, text::SOURCES_DAMAGED);

        // Bytes that are not an image at all.
        let junk = vec![7_u8; 300];
        fs::write(payload.join("crosspane-agent.exe"), &junk).unwrap();
        let mut junk_pins = Vec::new();
        for file in PayloadFile::ALL {
            let pin = inventory.pin(file).clone();
            junk_pins.push(if file == PayloadFile::Agent {
                pin_for(file, &junk, true)
            } else {
                pin
            });
        }
        let junk_inventory = Inventory::for_tests(junk_pins, "1.2.3");
        assert!(sources(Some(&payload), &junk_inventory).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_inventory_that_leaves_a_file_unlisted_is_refused() {
        let fixture = Fixture::new("unlisted");
        let pins: Vec<Pin> = PayloadFile::ALL
            .into_iter()
            .filter(|file| *file != PayloadFile::Helper)
            .map(|file| fixture.inventory.pin(file).clone())
            .collect();
        let partial = Inventory::for_tests(pins, "1.2.3");
        let error = sources(Some(&fixture.payload), &partial).unwrap_err();
        assert!(!error.changed);
        assert_eq!(error.text, text::NO_INVENTORY);
        let bare = fixture.bare();
        let mut backup = Backup::new(&fixture.paths);
        let error = place(&fixture.paths, &partial, &bare, false, &mut backup).unwrap_err();
        assert!(!error.changed);
        assert!(!fixture.paths.install.exists());
    }
}
