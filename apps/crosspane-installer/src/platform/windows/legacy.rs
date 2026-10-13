//! Backups and cleanup (L3): the old backend's leftovers and replaced files move into `backups\`.

use super::paths::Paths;
use std::path::{Path, PathBuf};

/// The dated folder under `backups\` that one converge moves things into, created lazily.
#[derive(Debug)]
pub(crate) struct Backup {
    root: PathBuf,
    made: bool,
}

impl Backup {
    /// `backups\<UTC yyyymmddThhmmssZ>[-n]`, created on the first `stash`.
    pub(crate) fn new(paths: &Paths) -> Self {
        Self {
            root: paths.backups.clone(),
            made: false,
        }
    }

    /// Rename `item` to `root\<under>\<leaf>`. `ERROR_NOT_SAME_DEVICE` → rename to `<item>.old`
    /// in place.
    pub(crate) fn stash(&mut self, item: &Path, under: &str) -> std::io::Result<PathBuf> {
        let _ = (item, under);
        Err(std::io::Error::other("not built"))
    }
}

/// `GetSystemTime` → "20261013T004000Z"
pub(crate) fn utc_stamp() -> String {
    String::new()
}

/// `Installer\*` except `installed.json`; `state\runtime\reinstall`, `runtime\removal`;
/// `state\reinstall-*`, `state\reinstall.lock`; `Programs\crosspane-reinstall-*`; install-dir
/// strays (anything not in `PayloadFile::ALL`, including `first-install-stage\`, `payload-stage\`,
/// `payload-backups\`, `*.new`, `*.old`).
pub(crate) fn items(paths: &Paths) -> Vec<PathBuf> {
    let _ = paths;
    Vec::new()
}

/// Moves every item into `backup`; returns what could not be moved (plain names).
pub(crate) fn clean(paths: &Paths, backup: &mut Backup) -> Vec<String> {
    let _ = (paths, backup);
    Vec::new()
}

/// Keeps the `keep` newest backup folders; best effort.
pub(crate) fn prune(paths: &Paths, keep: usize) {
    let _ = (paths, keep);
}
