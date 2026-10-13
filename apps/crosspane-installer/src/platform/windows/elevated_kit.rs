//! Elevated setup kit verification: the placed kit is re-hashed and held before each launch.
//! The kit is the helper image plus the four-file driver package, pinned by the embedded
//! inventory (WP-W4.1c2 N1). Nothing here elevates or writes.

use super::error::{NativeError, NativeResult};
use aws_lc_rs::digest::{SHA256, digest};
use crosspane_installer_core::elevated::{
    DRIVER_DIRECTORY, HELPER_IMAGE,
    kit::{KitDocument, KitFile, KitManifest, KitPin, MAX_KIT_INVENTORY_BYTES},
};
use std::{
    fs::{self, Metadata, OpenOptions},
    io::{self, Read},
    os::windows::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
};

/// The four placed kit files, each open for reading only (WP-W4.1c2 L17). Every handle shares READ
/// and nothing else, so while the hold lives none of the files can be replaced, renamed or deleted.
/// Dropping the hold closes the handles.
pub(crate) struct KitHold {
    /// Held for their lifetime only; nothing reads them.
    #[allow(dead_code)]
    files: Vec<fs::File>,
}

impl std::fmt::Debug for KitHold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KitHold(..)")
    }
}

/// The embedded kit inventory. `None` when it is absent, longer than `MAX_KIT_INVENTORY_BYTES`,
/// unparseable, or refused by `KitManifest::admit`.
pub(crate) fn embedded() -> Option<KitManifest> {
    let value = option_env!("CROSSPANE_WINDOWS_ELEVATED_INVENTORY")?;
    if value.len() > MAX_KIT_INVENTORY_BYTES {
        return None;
    }
    let document: KitDocument = serde_json::from_str(value).ok()?;
    KitManifest::admit(document).ok()
}

/// Verifies the placed kit under `install` and holds it. `install` and `install\driver` must be
/// non-reparse directories. Each of the four files is opened with share READ only and
/// `FILE_FLAG_OPEN_REPARSE_POINT`, must be a plain non-reparse file of its pinned size, and is
/// hashed through that same handle. Returns the helper image path with the hold. The caller keeps
/// the hold until the helper has run. A missing file is `Missing`; a directory or reparse point, a
/// size mismatch or a digest mismatch is `Foreign`.
pub(crate) fn hold_placed(
    install: &Path,
    manifest: &KitManifest,
) -> NativeResult<(PathBuf, KitHold)> {
    plain_dir(install)?;
    plain_dir(&install.join(DRIVER_DIRECTORY))?;
    let mut files = Vec::with_capacity(KitFile::ALL.len());
    for file in KitFile::ALL {
        files.push(hold_pinned(&kit_path(install, file), manifest.pin(file))?);
    }
    Ok((install.join(HELPER_IMAGE), KitHold { files }))
}

/// The on-disk path of one kit file under `root`, which is the install directory.
fn kit_path(root: &Path, file: KitFile) -> PathBuf {
    file.components()
        .iter()
        .fold(root.to_path_buf(), |path, part| path.join(part))
}

/// Opens one kit file for reading only, sharing READ, and hashes it through that same handle. The
/// path check and the handle check both require a plain file of exactly the pinned size.
fn hold_pinned(path: &Path, pin: &KitPin) -> NativeResult<fs::File> {
    let metadata = fs::symlink_metadata(path).map_err(io_error)?;
    if !plain_file(&metadata) || metadata.len() != pin.size {
        return Err(NativeError::Foreign);
    }
    // Only READ is shared, so no later open can write to or delete the file. The link itself is
    // opened, not its target, so a reparse point swapped in after the check above is seen below.
    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(io_error)?;
    let opened = file.metadata().map_err(io_error)?;
    if !plain_file(&opened) || opened.len() != pin.size {
        return Err(NativeError::Foreign);
    }
    hash_pinned(&file, pin)?;
    Ok(file)
}

/// Reads the whole of `file` through its handle, bounded to `pin.size + 1` bytes, and checks the
/// size and digest against `pin`.
fn hash_pinned(file: &fs::File, pin: &KitPin) -> NativeResult<()> {
    let capacity = usize::try_from(pin.size).map_err(|_| NativeError::Oversize)?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(pin.size.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 != pin.size {
        return Err(NativeError::Foreign);
    }
    let actual = digest(&SHA256, &bytes);
    if actual.as_ref() != pin.sha256.as_slice() {
        return Err(NativeError::Foreign);
    }
    Ok(())
}

/// A non-reparse directory. Anything else, including absence, is an error.
fn plain_dir(path: &Path) -> NativeResult<()> {
    let metadata = fs::symlink_metadata(path).map_err(io_error)?;
    if plain_dir_metadata(&metadata) {
        Ok(())
    } else {
        Err(NativeError::Foreign)
    }
}

fn plain_file(metadata: &Metadata) -> bool {
    metadata.file_type().is_file() && !is_reparse(metadata)
}

fn plain_dir_metadata(metadata: &Metadata) -> bool {
    metadata.file_type().is_dir() && !is_reparse(metadata)
}

fn is_reparse(metadata: &Metadata) -> bool {
    (metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
}

/// Maps an I/O failure as `ElevatedHelper::locate` does: absence is `Missing`, anything else
/// `Unavailable`.
fn io_error(error: io::Error) -> NativeError {
    if error.kind() == io::ErrorKind::NotFound {
        NativeError::Missing
    } else {
        NativeError::Unavailable
    }
}
