//! Backups and cleanup (L3): the old backend's leftovers and replaced files move into `backups\`.

use super::{
    error::note_io,
    paths::{Paths, PayloadFile},
};
use std::{
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
};
use windows_sys::Win32::{
    Foundation::{ERROR_NOT_SAME_DEVICE, SYSTEMTIME},
    System::SystemInformation::GetSystemTime,
};

/// Where each kind of leftover goes inside a backup folder.
const UNDER_INSTALLER: &str = r"legacy\Installer";
const UNDER_RUNTIME: &str = r"legacy\runtime";
const UNDER_STATE: &str = r"legacy\state";
const UNDER_PROGRAMS: &str = r"legacy\programs";
const UNDER_INSTALL: &str = r"legacy\install";
const UNDER_DRIVER: &str = r"legacy\install\driver";

/// The dated folder under `backups\` that one converge moves things into, created lazily.
#[derive(Debug)]
pub(crate) struct Backup {
    root: PathBuf,
    made: bool,
}

impl Backup {
    /// `backups\<UTC yyyymmddThhmmssZ>[-n]`, created on the first `stash`.
    pub(crate) fn new(paths: &Paths) -> Self {
        let stamp = utc_stamp();
        let mut root = paths.backups.join(&stamp);
        let mut n = 0_u32;
        while fs::symlink_metadata(&root).is_ok() && n < 1000 {
            n += 1;
            root = paths.backups.join(format!("{stamp}-{n}"));
        }
        Self { root, made: false }
    }

    /// Rename `item` to `root\<under>\<leaf>` (`<leaf>-n` if that name is taken).
    /// `ERROR_NOT_SAME_DEVICE` → rename to `<item>.old` in place instead. Returns where it went.
    pub(crate) fn stash(&mut self, item: &Path, under: &str) -> io::Result<PathBuf> {
        let leaf = item.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "the item has no file name")
        })?;
        if !self.made {
            fs::create_dir_all(&self.root)?;
            self.made = true;
        }
        let folder = self.root.join(under);
        fs::create_dir_all(&folder)?;
        let target = free_name(&folder.join(leaf), "-");
        match fs::rename(item, &target) {
            Ok(()) => Ok(target),
            Err(error) if error.raw_os_error() == Some(ERROR_NOT_SAME_DEVICE as i32) => {
                let aside = free_name(&with_suffix(item, ".old"), "-");
                fs::rename(item, &aside)?;
                Ok(aside)
            }
            Err(error) => Err(error),
        }
    }
}

/// `path` if nothing is there, else `path` with `separator` and the first free number appended.
fn free_name(path: &Path, separator: &str) -> PathBuf {
    if fs::symlink_metadata(path).is_err() {
        return path.to_path_buf();
    }
    let mut n = 1_u32;
    loop {
        let candidate = with_suffix(path, &format!("{separator}{n}"));
        if fs::symlink_metadata(&candidate).is_err() || n >= 1000 {
            return candidate;
        }
        n += 1;
    }
}

/// `path` with `suffix` appended to its last component.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// `GetSystemTime` → "20261013T004000Z"
pub(crate) fn utc_stamp() -> String {
    let mut now = SYSTEMTIME {
        wYear: 0,
        wMonth: 0,
        wDayOfWeek: 0,
        wDay: 0,
        wHour: 0,
        wMinute: 0,
        wSecond: 0,
        wMilliseconds: 0,
    };
    // SAFETY: `now` is a valid, writable SYSTEMTIME that lives for the whole call.
    unsafe { GetSystemTime(&mut now) };
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        now.wYear, now.wMonth, now.wDay, now.wHour, now.wMinute, now.wSecond
    )
}

/// `Installer\*` except `installed.json`; `state\runtime\reinstall`, `runtime\removal`;
/// `state\reinstall-*`, `state\reinstall.lock`; `Programs\crosspane-reinstall-*`; install-dir
/// strays (anything not in `PayloadFile::ALL`, including `first-install-stage\`, `payload-stage\`,
/// `payload-backups\`, `*.new`, `*.old`). Nothing else is ever listed: the agent's own files in
/// `state` are never legacy.
pub(crate) fn items(paths: &Paths) -> Vec<PathBuf> {
    entries(paths).into_iter().map(|(_, path)| path).collect()
}

/// Every legacy item with the backup sub-folder it moves into.
fn entries(paths: &Paths) -> Vec<(&'static str, PathBuf)> {
    let mut found = Vec::new();

    for (path, _) in children(&paths.installer_state) {
        if lower_leaf(&path) != "installed.json" {
            found.push((UNDER_INSTALLER, path));
        }
    }

    for leaf in ["reinstall", "removal"] {
        let path = paths.runtime.join(leaf);
        if fs::symlink_metadata(&path).is_ok() {
            found.push((UNDER_RUNTIME, path));
        }
    }

    for (path, is_dir) in children(&paths.state) {
        let name = lower_leaf(&path);
        if (is_dir && name.starts_with("reinstall-")) || name == "reinstall.lock" {
            found.push((UNDER_STATE, path));
        }
    }

    for (path, is_dir) in children(&paths.programs) {
        if is_dir && lower_leaf(&path).starts_with("crosspane-reinstall-") {
            found.push((UNDER_PROGRAMS, path));
        }
    }

    // The payload files at the top of the install dir, and the three inside `driver`.
    let mut top = Vec::new();
    let mut driver = Vec::new();
    for file in PayloadFile::ALL {
        match file.relative().split_once('\\') {
            Some((folder, leaf)) => {
                top.push(folder.to_ascii_lowercase());
                driver.push(leaf.to_ascii_lowercase());
            }
            None => top.push(file.relative().to_ascii_lowercase()),
        }
    }
    for (path, is_dir) in children(&paths.install) {
        let name = lower_leaf(&path);
        if !top.contains(&name) {
            found.push((UNDER_INSTALL, path));
        } else if name == "driver" {
            if is_dir {
                for (inner, _) in children(&path) {
                    if !driver.contains(&lower_leaf(&inner)) {
                        found.push((UNDER_DRIVER, inner));
                    }
                }
            } else {
                // A file or link where the driver folder belongs is in the way.
                found.push((UNDER_INSTALL, path));
            }
        }
    }
    found
}

/// The entries of `dir`, sorted by name, each with whether it is a real folder (not a link).
/// A folder that cannot be listed is treated as empty.
fn children(dir: &Path) -> Vec<(PathBuf, bool)> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(PathBuf, bool)> = read
        .flatten()
        .map(|entry| {
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            (entry.path(), is_dir)
        })
        .collect();
    found.sort();
    found
}

/// The last component of `path`, lower-cased, for case-insensitive comparison.
fn lower_leaf(path: &Path) -> String {
    path.file_name()
        .map(OsStr::to_string_lossy)
        .map(|name| name.to_ascii_lowercase())
        .unwrap_or_default()
}

/// Moves every item into `backup`; returns what could not be moved (plain names).
///
/// An item that cannot be moved is deleted instead. Only an item that is still there afterwards
/// is reported.
pub(crate) fn clean(paths: &Paths, backup: &mut Backup) -> Vec<String> {
    let mut leftovers = Vec::new();
    for (under, item) in entries(paths) {
        let Err(error) = backup.stash(&item, under) else {
            continue;
        };
        note_io("move a leftover aside", &error);
        if let Err(error) = remove_item(&item) {
            note_io("remove a leftover", &error);
        }
        if fs::symlink_metadata(&item).is_ok() {
            leftovers.push(plain_leaf(&item));
        }
    }
    leftovers
}

/// The last component of `path` as written, for messages.
fn plain_leaf(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Deletes a file, a link or a folder (a folder with everything in it; links are not followed).
fn remove_item(item: &Path) -> io::Result<()> {
    if fs::symlink_metadata(item)?.file_type().is_dir() {
        fs::remove_dir_all(item)
    } else {
        fs::remove_file(item).or_else(|_| fs::remove_dir(item))
    }
}

/// Keeps the `keep` newest backup folders; best effort.
pub(crate) fn prune(paths: &Paths, keep: usize) {
    let mut folders: Vec<(String, PathBuf)> = children(&paths.backups)
        .into_iter()
        .filter(|(_, is_dir)| *is_dir)
        .map(|(path, _)| (plain_leaf(&path), path))
        .filter(|(name, _)| is_backup_name(name))
        .collect();
    // Timestamps sort in time order, and a `-n` copy sorts after its own second.
    folders.sort();
    let excess = folders.len().saturating_sub(keep);
    for (_, folder) in folders.into_iter().take(excess) {
        if let Err(error) = fs::remove_dir_all(&folder) {
            note_io("remove an old backup", &error);
        }
    }
}

/// Whether `name` is `yyyymmddThhmmssZ` or `yyyymmddThhmmssZ-n`: a folder this installer made.
fn is_backup_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() >= 16
        && bytes[..8].iter().all(u8::is_ascii_digit)
        && bytes[8] == b'T'
        && bytes[9..15].iter().all(u8::is_ascii_digit)
        && bytes[15] == b'Z'
        && (bytes.len() == 16
            || (bytes[16] == b'-'
                && bytes.len() > 17
                && bytes[17..].iter().all(u8::is_ascii_digit)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A fresh folder under the worktree's `target\`, never anywhere a real install lives.
    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap()
            .join("target")
            .join("w41e-scratch")
            .join(format!("legacy-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn paths_in(root: &Path) -> Paths {
        Paths::at(
            &root.join("local"),
            &root.join("roaming"),
            &root.join("temp"),
        )
        .unwrap()
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x").unwrap();
    }

    /// Everything a machine upgraded from the old backend has: legacy leftovers, the agent's own
    /// files, and the payload. Returns the legacy paths.
    fn populate(paths: &Paths) -> Vec<PathBuf> {
        // (a file to create, the top-level legacy item that makes it one)
        let table: Vec<(PathBuf, PathBuf)> = vec![
            (
                paths.installer_state.join("first-install.json"),
                paths.installer_state.join("first-install.json"),
            ),
            (
                paths.installer_state.join("supervisor-1.json"),
                paths.installer_state.join("supervisor-1.json"),
            ),
            (
                paths.installer_state.join("task-activation.json"),
                paths.installer_state.join("task-activation.json"),
            ),
            (
                paths.installer_state.join("elevated-setup.json"),
                paths.installer_state.join("elevated-setup.json"),
            ),
            (
                paths.installer_state.join("install.lock"),
                paths.installer_state.join("install.lock"),
            ),
            (
                paths.installer_state.join("history").join("one.json"),
                paths.installer_state.join("history"),
            ),
            (
                paths.runtime.join("reinstall").join("record.json"),
                paths.runtime.join("reinstall"),
            ),
            (
                paths.runtime.join("removal").join("record.json"),
                paths.runtime.join("removal"),
            ),
            (
                paths.state.join("reinstall-ab12").join("step.json"),
                paths.state.join("reinstall-ab12"),
            ),
            (
                paths.state.join("reinstall.lock"),
                paths.state.join("reinstall.lock"),
            ),
            (
                paths
                    .programs
                    .join("crosspane-reinstall-deadbeef")
                    .join("a.exe"),
                paths.programs.join("crosspane-reinstall-deadbeef"),
            ),
            (
                paths.install.join(r"first-install-stage\one.exe"),
                paths.install.join("first-install-stage"),
            ),
            (
                paths.install.join(r"payload-stage\two.exe"),
                paths.install.join("payload-stage"),
            ),
            (
                paths.install.join(r"payload-backups\three.exe"),
                paths.install.join("payload-backups"),
            ),
            (
                paths.install.join("crosspane-agent.exe.new"),
                paths.install.join("crosspane-agent.exe.new"),
            ),
            (
                paths.install.join("crosspane-ui.exe.old"),
                paths.install.join("crosspane-ui.exe.old"),
            ),
            (
                paths.install.join("notes.txt"),
                paths.install.join("notes.txt"),
            ),
            (
                paths.install.join(r"driver\stray.dll"),
                paths.install.join(r"driver\stray.dll"),
            ),
            (
                paths.install.join(r"driver\CrosspaneIdd.dll.new"),
                paths.install.join(r"driver\CrosspaneIdd.dll.new"),
            ),
        ];
        let mut legacy = Vec::new();
        for (file, item) in table {
            touch(&file);
            if !legacy.contains(&item) {
                legacy.push(item);
            }
        }
        legacy
    }

    /// What the new backend, the agent and the payload own. None of it may ever be listed.
    fn kept(paths: &Paths) -> Vec<PathBuf> {
        let mut kept = vec![
            paths.installed_json(),
            paths.runtime.join("bootstrap.json"),
            paths.state.join("agent.lock"),
            paths.state.join("identity.lock"),
            paths.state.join("input.journal"),
            paths.state.join("parking-1.journal"),
            paths.state.join("projection-input.journal"),
            paths.state.join("window.pending"),
            paths.state.join("last_exit.json"),
            paths.state.join("logs").join("agent.log"),
            paths.state.join("reinstall-notes.txt"),
            paths.config.join("config.toml"),
            paths.config.join("trust.json"),
            paths.programs.join("OtherApp").join("app.exe"),
            paths.backups.join("20200101T000000Z").join("old.txt"),
        ];
        kept.extend(PayloadFile::ALL.map(|file| paths.file(file)));
        kept
    }

    fn sorted(mut list: Vec<PathBuf>) -> Vec<PathBuf> {
        list.sort();
        list
    }

    #[test]
    fn utc_stamp_has_the_documented_shape() {
        let stamp = utc_stamp();
        assert_eq!(stamp.len(), 16);
        assert!(is_backup_name(&stamp));
        assert_eq!(stamp.as_bytes()[8], b'T');
        assert!(stamp.ends_with('Z'));
        assert!(stamp.starts_with("20"));
        assert!(
            stamp
                .chars()
                .enumerate()
                .all(|(i, c)| i == 8 || i == 15 || c.is_ascii_digit())
        );
    }

    #[test]
    fn items_lists_exactly_the_legacy_set() {
        let root = scratch("items");
        let paths = paths_in(&root);
        let legacy = populate(&paths);
        let kept = kept(&paths);
        for path in &kept {
            touch(path);
        }
        // `reinstall-notes.txt` is a file, not a quarantine folder, so it is not legacy.
        assert_eq!(sorted(items(&paths)), sorted(legacy));
        for path in &kept {
            assert!(!items(&paths).contains(path));
        }
        // A missing tree lists nothing.
        let empty = paths_in(&root.join("elsewhere"));
        assert!(items(&empty).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_where_the_driver_folder_belongs_is_a_stray() {
        let root = scratch("driverfile");
        let paths = paths_in(&root);
        touch(&paths.install.join("driver"));
        assert_eq!(items(&paths), vec![paths.install.join("driver")]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn clean_moves_the_legacy_set_into_the_backup_and_nothing_else() {
        let root = scratch("clean");
        let paths = paths_in(&root);
        let legacy = populate(&paths);
        let kept = kept(&paths);
        for path in &kept {
            touch(path);
        }
        let mut backup = Backup::new(&paths);
        let leftovers = clean(&paths, &mut backup);
        assert!(leftovers.is_empty());
        assert!(items(&paths).is_empty());
        for path in &legacy {
            assert!(!path.exists(), "{} should have moved", path.display());
        }
        for path in &kept {
            assert!(path.exists(), "{} must stay", path.display());
        }
        assert!(
            backup
                .root
                .join(r"legacy\Installer\first-install.json")
                .is_file()
        );
        assert!(
            backup
                .root
                .join(r"legacy\Installer\history\one.json")
                .is_file()
        );
        assert!(
            backup
                .root
                .join(r"legacy\runtime\reinstall\record.json")
                .is_file()
        );
        assert!(backup.root.join(r"legacy\state\reinstall.lock").is_file());
        assert!(
            backup
                .root
                .join(r"legacy\programs\crosspane-reinstall-deadbeef\a.exe")
                .is_file()
        );
        assert!(
            backup
                .root
                .join(r"legacy\install\first-install-stage\one.exe")
                .is_file()
        );
        assert!(
            backup
                .root
                .join(r"legacy\install\crosspane-ui.exe.old")
                .is_file()
        );
        assert!(
            backup
                .root
                .join(r"legacy\install\driver\stray.dll")
                .is_file()
        );
        // A second pass has nothing to do and makes no new backup folder.
        let mut again = Backup::new(&paths);
        assert!(clean(&paths, &mut again).is_empty());
        assert!(!again.root.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn stash_never_overwrites_and_each_backup_gets_its_own_folder() {
        let root = scratch("stash");
        let paths = paths_in(&root);
        let first = root.join("a").join("same.txt");
        let second = root.join("b").join("same.txt");
        touch(&first);
        touch(&second);
        fs::write(&second, b"second").unwrap();

        let mut backup = Backup::new(&paths);
        assert!(!backup.root.exists());
        let one = backup.stash(&first, "files").unwrap();
        let two = backup.stash(&second, "files").unwrap();
        assert_eq!(one, backup.root.join("files").join("same.txt"));
        assert_eq!(two, backup.root.join("files").join("same.txt-1"));
        assert_eq!(fs::read(two).unwrap(), b"second");
        assert!(!first.exists());
        assert!(!second.exists());

        // The folder exists now, so a second backup in the same second takes the next name.
        let other = Backup::new(&paths);
        assert_ne!(other.root, backup.root);
        assert!(other.root.starts_with(&paths.backups));
        assert!(backup.stash(&root.join("nothing-here"), "files").is_err());
        let _ = fs::remove_dir_all(&root);
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = children(dir)
            .into_iter()
            .map(|(path, _)| plain_leaf(&path))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn prune_keeps_the_newest_five_of_seven() {
        let root = scratch("prune");
        let paths = paths_in(&root);
        for n in 1..=7 {
            touch(
                &paths
                    .backups
                    .join(format!("2026101{n}T000000Z"))
                    .join("f.txt"),
            );
        }
        // Things that are not this installer's backup folders are never pruned.
        touch(&paths.backups.join("notes").join("f.txt"));
        touch(&paths.backups.join("20261010T000000Z.txt"));
        prune(&paths, 5);
        assert_eq!(
            names_in(&paths.backups),
            vec![
                "20261010T000000Z.txt",
                "20261013T000000Z",
                "20261014T000000Z",
                "20261015T000000Z",
                "20261016T000000Z",
                "20261017T000000Z",
                "notes",
            ]
        );
        // Nothing beyond `keep` is left to remove, and a missing folder is fine.
        prune(&paths, 5);
        assert_eq!(names_in(&paths.backups).len(), 7);
        prune(&paths_in(&root.join("elsewhere")), 5);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn prune_sorts_a_dash_copy_after_its_own_second() {
        let root = scratch("prune-dash");
        let paths = paths_in(&root);
        for name in ["20261016T000000Z", "20261016T000000Z-1", "20261017T000000Z"] {
            touch(&paths.backups.join(name).join("f.txt"));
        }
        prune(&paths, 2);
        assert_eq!(
            names_in(&paths.backups),
            vec!["20261016T000000Z-1", "20261017T000000Z"]
        );
        prune(&paths, 0);
        assert!(names_in(&paths.backups).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn backup_names_are_recognised_exactly() {
        assert!(is_backup_name("20261013T004000Z"));
        assert!(is_backup_name("20261013T004000Z-3"));
        assert!(!is_backup_name("20261013T004000Z-"));
        assert!(!is_backup_name("20261013T004000Z-x"));
        assert!(!is_backup_name("20261013T004000"));
        assert!(!is_backup_name("20261013t004000Z"));
        assert!(!is_backup_name("notes"));
        assert!(!is_backup_name(""));
    }
}
