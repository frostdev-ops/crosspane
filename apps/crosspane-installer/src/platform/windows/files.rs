//! Payload files (L9): the approved inventory, source verification, hash-checked placement.

use super::{
    error::{SetupError, SetupResult},
    legacy::Backup,
    paths::{Paths, PayloadFile},
};
use crosspane_installer_core::elevated::kit::KitManifest;
use std::path::{Path, PathBuf};

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
        Err(SetupError::before("not built"))
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
    let _ = (paths, inventory);
    Vec::new()
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
    let _ = (payload, inventory);
    Err(SetupError::before("not built"))
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
    let _ = (paths, inventory, sources, complete, backup);
    Err(SetupError::before("not built"))
}

pub(crate) fn sha256_file(path: &Path) -> std::io::Result<([u8; 32], u64)> {
    let _ = path;
    Err(std::io::Error::other("not built"))
}
