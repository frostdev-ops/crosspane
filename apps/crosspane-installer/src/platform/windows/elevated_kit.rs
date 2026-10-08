//! Elevated setup kit placement and verification: staged, then re-hashed before each launch.
//! The kit is the helper image plus the four-file driver package, pinned by the embedded
//! inventory (WP-W4.1c2 N1). Nothing here elevates. Every write uses bytes that already matched
//! their pin, and nothing is ever deleted recursively.

use super::native_io::{NativeError, NativeResult};
use aws_lc_rs::digest::{SHA256, digest};
use crosspane_installer_core::elevated::{
    DRIVER_DIRECTORY, HELPER_IMAGE,
    kit::{KitDocument, KitFile, KitManifest, KitPin, MAX_KIT_INVENTORY_BYTES},
};
use std::{
    fs::{self, Metadata, OpenOptions},
    io::{self, Read, Write},
    os::windows::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf, Prefix},
    sync::Arc,
};
use windows_sys::Win32::{
    Foundation::ERROR_DIR_NOT_EMPTY,
    Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
    },
};

/// Suffix of the staging sibling that each kit file is written under before the rename.
const TEMP_SUFFIX: &str = ".crosspane-new";

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

/// Kit file bytes read from an explicit local folder. Each entry matched its pin when read.
pub(crate) struct KitSources {
    files: Vec<(KitFile, Arc<[u8]>)>,
}

impl std::fmt::Debug for KitSources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KitSources(..)")
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

/// Reads the four kit files from an explicit local folder. Each must be a regular, non-reparse
/// file of exactly its pinned size whose digest matches. A missing file is `Missing`; a directory
/// or reparse point, a size mismatch or a digest mismatch is `Foreign`.
pub(crate) fn read_sources(folder: &Path, manifest: &KitManifest) -> NativeResult<KitSources> {
    // Explicit local input only; no PATH, sibling, UNC or backup fallback (domains.rs:328-333).
    if !folder.is_absolute()
        || !matches!(folder.components().next(),
        Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
    {
        return Err(NativeError::Invalid);
    }
    let mut files: Vec<(KitFile, Arc<[u8]>)> = Vec::with_capacity(KitFile::ALL.len());
    for file in KitFile::ALL {
        let bytes = read_pinned(&kit_path(folder, file), manifest.pin(file))?;
        files.push((file, Arc::from(bytes)));
    }
    Ok(KitSources { files })
}

/// Verifies the placed kit under `install` without changing anything, and returns the helper
/// image path. `install` and `install\driver` must be non-reparse directories. Each of the four
/// files must be a non-reparse regular file whose size and digest match its pin.
pub(crate) fn verify_placed(install: &Path, manifest: &KitManifest) -> NativeResult<PathBuf> {
    plain_dir(install)?;
    plain_dir(&install.join(DRIVER_DIRECTORY))?;
    for file in KitFile::ALL {
        read_pinned(&kit_path(install, file), manifest.pin(file))?;
    }
    Ok(install.join(HELPER_IMAGE))
}

/// Verifies the placed kit under `install` like `verify_placed`, and holds it. Each of the four
/// files is opened with share READ only and `FILE_FLAG_OPEN_REPARSE_POINT`, must be a plain
/// non-reparse file of its pinned size, and is hashed through that same handle. Returns the helper
/// image path with the hold. The caller keeps the hold until the helper has run. A missing file is
/// `Missing`; a directory or reparse point, a size mismatch or a digest mismatch is `Foreign`.
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

/// Makes the kit under `install` match its pins and returns the helper image path. A placed,
/// verified kit is used as it is. Otherwise, with `sources`, the verified bytes are staged beside
/// each target and renamed over it, and the result is verified again. Without `sources`, a
/// missing kit is `Missing`. Any other verification failure is returned unchanged.
pub(crate) fn ensure(
    install: &Path,
    manifest: &KitManifest,
    sources: Option<&KitSources>,
) -> NativeResult<PathBuf> {
    let placed = match verify_placed(install, manifest) {
        Ok(image) => return Ok(image),
        Err(error) => error,
    };
    let Some(sources) = sources else {
        return Err(placed);
    };
    plain_dir(install)?;
    let driver = install.join(DRIVER_DIRECTORY);
    let driver_present = optional_plain_dir(&driver)?;
    // Refuse before the first write. Every target and staging leaf must be absent or a plain file,
    // and every source must match its pin, so a refusal leaves no partial kit behind.
    let mut staged: Vec<(PathBuf, PathBuf, &[u8])> = Vec::with_capacity(KitFile::ALL.len());
    for file in KitFile::ALL {
        let bytes = source_bytes(sources, manifest, file)?;
        let target = kit_path(install, file);
        let temp = temp_path(&target)?;
        replaceable(&target)?;
        replaceable(&temp)?;
        staged.push((target, temp, bytes));
    }
    if !driver_present {
        fs::create_dir(&driver).map_err(io_error)?;
    }
    for (target, temp, bytes) in &staged {
        publish(target, temp, bytes)?;
    }
    verify_placed(install, manifest)
}

/// Removes the placed kit from `install`: each kit file and its `.crosspane-new` sibling when it is
/// a plain non-reparse file, then the `driver` directory when it is a plain non-reparse directory
/// that is empty by then. Absent entries are fine, and `install` itself stays. Everything is
/// checked before the first removal, so a refusal leaves the kit as it was. A directory or reparse
/// point where a file belongs is `Foreign`, and so is any entry in `driver` that is not one of the
/// kit's own files. Nothing is removed recursively.
pub(crate) fn remove_placed(install: &Path) -> NativeResult<()> {
    if !optional_plain_dir(install)? {
        return Ok(());
    }
    // `driver` is checked before any path under it is looked up, so a junction is never followed.
    let driver = install.join(DRIVER_DIRECTORY);
    let driver_present = optional_plain_dir(&driver)?;
    let mut paths: Vec<PathBuf> = Vec::with_capacity(2 * KitFile::ALL.len());
    for file in KitFile::ALL {
        let target = kit_path(install, file);
        let temp = temp_path(&target)?;
        replaceable(&target)?;
        replaceable(&temp)?;
        paths.push(target);
        paths.push(temp);
    }
    if driver_present {
        driver_holds_only_kit(&driver, &paths)?;
    }
    for path in &paths {
        remove_plain_file(path)?;
    }
    remove_empty_dir(&driver)
}

/// The on-disk path of one kit file under `root`, which is `install` or the `--payload` folder.
fn kit_path(root: &Path, file: KitFile) -> PathBuf {
    file.components()
        .iter()
        .fold(root.to_path_buf(), |path, part| path.join(part))
}

/// The staging sibling of `target`: `<leaf>.crosspane-new` in the same directory.
fn temp_path(target: &Path) -> NativeResult<PathBuf> {
    let leaf = target.file_name().ok_or(NativeError::Invalid)?;
    let mut name = leaf.to_os_string();
    name.push(TEMP_SUFFIX);
    Ok(target.with_file_name(name))
}

/// The verified bytes of one kit file. They are checked against `manifest` again, so sources read
/// under a different inventory are never published.
fn source_bytes<'a>(
    sources: &'a KitSources,
    manifest: &KitManifest,
    file: KitFile,
) -> NativeResult<&'a [u8]> {
    let (_, bytes) = sources
        .files
        .iter()
        .find(|(kit_file, _)| *kit_file == file)
        .ok_or(NativeError::Invalid)?;
    let actual = digest(&SHA256, bytes);
    let sha256: [u8; 32] = actual
        .as_ref()
        .try_into()
        .map_err(|_| NativeError::Invalid)?;
    if !manifest.matches(file, bytes.len() as u64, &sha256) {
        return Err(NativeError::Foreign);
    }
    Ok(&bytes[..])
}

/// Stages one verified file beside its target, then renames it over the target. The staging file
/// is removed, best effort, on any failure after this call created it. Nothing else is removed.
fn publish(target: &Path, temp: &Path, bytes: &[u8]) -> NativeResult<()> {
    remove_plain_file(temp)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp)
        .map_err(io_error)?;
    let written = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(io_error);
    // The handle is closed before the rename, which needs the staging file unopened.
    drop(file);
    let published = written.and_then(|()| fs::rename(temp, target).map_err(io_error));
    if published.is_err() {
        let _ = fs::remove_file(temp);
    }
    published
}

/// Removes one plain file if it is present: a leftover staging file or a placed kit file. Absent is
/// fine. A directory or reparse point is `Foreign`.
fn remove_plain_file(path: &Path) -> NativeResult<()> {
    replaceable(path)?;
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(error)),
    }
}

/// Reads one kit file whose pin is known. It must be a regular, non-reparse file of exactly the
/// pinned size. The read is bounded to `pin.size + 1` bytes, and it must yield exactly that size
/// and digest.
fn read_pinned(path: &Path, pin: &KitPin) -> NativeResult<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).map_err(io_error)?;
    if !plain_file(&metadata) || metadata.len() != pin.size {
        return Err(NativeError::Foreign);
    }
    // The link itself is opened, not its target, so a reparse point swapped in after the check
    // above is seen on the handle below.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(io_error)?;
    let opened = file.metadata().map_err(io_error)?;
    if !plain_file(&opened) || opened.len() != pin.size {
        return Err(NativeError::Foreign);
    }
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
    Ok(bytes)
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

/// Absent, or a plain regular file that a rename may replace. A directory or reparse point is
/// `Foreign`.
fn replaceable(path: &Path) -> NativeResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if plain_file(&metadata) => Ok(()),
        Ok(_) => Err(NativeError::Foreign),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(error)),
    }
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

/// `Ok(true)` for a non-reparse directory, `Ok(false)` when absent, and `Foreign` otherwise.
fn optional_plain_dir(path: &Path) -> NativeResult<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if plain_dir_metadata(&metadata) => Ok(true),
        Ok(_) => Err(NativeError::Foreign),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error(error)),
    }
}

/// Refuses unless every entry of `driver` is one of the kit's own driver files or staging siblings
/// (`paths`, whose parent is `driver`). Any other entry, including a subdirectory, is `Foreign`.
fn driver_holds_only_kit(driver: &Path, paths: &[PathBuf]) -> NativeResult<()> {
    for entry in fs::read_dir(driver).map_err(io_error)? {
        let name = entry.map_err(io_error)?.file_name();
        let owned = paths.iter().any(|path| {
            path.parent() == Some(driver) && path.file_name() == Some(name.as_os_str())
        });
        if !owned {
            return Err(NativeError::Foreign);
        }
    }
    Ok(())
}

/// Removes an empty plain directory with a non-recursive `remove_dir`. Absent is fine. A directory
/// that still holds something is `Foreign`, as is a file or reparse point.
fn remove_empty_dir(path: &Path) -> NativeResult<()> {
    if !optional_plain_dir(path)? {
        return Ok(());
    }
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) if dir_not_empty(&error) => Err(NativeError::Foreign),
        Err(error) => Err(io_error(error)),
    }
}

/// Whether `error` is `ERROR_DIR_NOT_EMPTY`: a directory that still holds an entry.
fn dir_not_empty(error: &io::Error) -> bool {
    let code = error.raw_os_error();
    code.and_then(|code| u32::try_from(code).ok()) == Some(ERROR_DIR_NOT_EMPTY)
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
