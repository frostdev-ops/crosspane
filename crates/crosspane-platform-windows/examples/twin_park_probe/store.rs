//! File-backed mirror journal store for the M2 probe. Each publish writes a unique temp file in the
//! same directory, syncs it, and renames it over the target. Commit publishes pending, then
//! committed, as `MirrorJournalStore` requires.
#![allow(unsafe_code)]

use crosspane_platform::PlatformError;
use crosspane_platform_windows::{
    model::parking::MAX_BYTES,
    parking::{MirrorJournalImages, MirrorJournalStore},
};
use std::{
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

const COMMITTED: &str = "probe-committed.journal";
const PENDING: &str = "probe-pending.journal";

/// Journal images kept as files named by the caller, in one directory.
#[derive(Debug)]
pub struct FileStore {
    dir: PathBuf,
    committed: &'static str,
    pending: &'static str,
}

impl FileStore {
    pub fn new(dir: PathBuf, committed: &'static str, pending: &'static str) -> Self {
        Self {
            dir,
            committed,
            pending,
        }
    }

    fn load(&self, name: &str) -> Result<Option<Vec<u8>>, PlatformError> {
        let file = match File::open(self.dir.join(name)) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error(error)),
        };
        let mut bytes = Vec::new();
        // One byte past the limit proves an oversize file without reading all of it.
        file.take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        if bytes.len() > MAX_BYTES {
            return Err(backend("mirror journal too large; retained"));
        }
        Ok(Some(bytes))
    }

    /// Writes `bytes` to a unique temp file, syncs it, and renames it over `name`.
    fn publish(&self, name: &str, bytes: &[u8]) -> Result<(), PlatformError> {
        let target = self.dir.join(name);
        let (temp, mut file) = self.temporary(name)?;
        let result = (|| {
            file.write_all(bytes).map_err(io_error)?;
            file.sync_all().map_err(io_error)?;
            drop(file);
            fs::rename(&temp, &target).map_err(io_error)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    fn temporary(&self, name: &str) -> Result<(PathBuf, File), PlatformError> {
        let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = self
            .dir
            .join(format!("{name}.{}.{serial}.tmp", std::process::id()));
        // create_new refuses an existing file, so a stale temp is never reused.
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(io_error)?;
        Ok((path, file))
    }
}

impl MirrorJournalStore for FileStore {
    fn read(&mut self) -> Result<MirrorJournalImages, PlatformError> {
        Ok(MirrorJournalImages {
            committed: self.load(self.committed)?,
            pending: self.load(self.pending)?,
        })
    }

    fn commit(&mut self, document: &[u8]) -> Result<(), PlatformError> {
        if document.len() > MAX_BYTES {
            return Err(backend("mirror journal too large; retained"));
        }
        self.publish(self.pending, document)?;
        self.publish(self.committed, document)
    }
}

/// Round-trips the store in a fresh directory: an empty read, a commit, a read from a second store
/// on the same directory, an overwrite, then a check that no temp file was left behind.
pub fn selftest() -> Result<(), PlatformError> {
    let scratch = Scratch::create()?;
    let mut store = FileStore::new(scratch.0.clone(), COMMITTED, PENDING);
    let empty = store.read()?;
    if empty.committed.is_some() || empty.pending.is_some() {
        return Err(backend("store selftest: fresh directory is not empty"));
    }
    store.commit(b"first document")?;
    let mut reopened = FileStore::new(scratch.0.clone(), COMMITTED, PENDING);
    expect_document(reopened.read()?, b"first document")?;
    reopened.commit(b"second document, longer than the first")?;
    expect_document(store.read()?, b"second document, longer than the first")?;
    if temp_files_left(&scratch.0)? {
        return Err(backend("store selftest: temp file left behind"));
    }
    Ok(())
}

fn expect_document(images: MirrorJournalImages, expected: &[u8]) -> Result<(), PlatformError> {
    if images.committed.as_deref() == Some(expected) && images.pending.as_deref() == Some(expected)
    {
        Ok(())
    } else {
        Err(backend("store selftest: read-back mismatch"))
    }
}

fn temp_files_left(dir: &Path) -> Result<bool, PlatformError> {
    for entry in fs::read_dir(dir).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        if entry.file_name().to_string_lossy().ends_with(".tmp") {
            return Ok(true);
        }
    }
    Ok(false)
}

/// A directory this probe created. `create_dir` refuses an existing path, so the directory
/// removed on drop is always one this run made.
struct Scratch(PathBuf);

impl Scratch {
    fn create() -> Result<Self, PlatformError> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |age| age.as_nanos());
        let path = std::env::temp_dir().join(format!(
            "crosspane-twin-store-selftest-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir(&path).map_err(io_error)?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn backend(text: &str) -> PlatformError {
    PlatformError::Backend(text.to_owned())
}

fn io_error(error: std::io::Error) -> PlatformError {
    PlatformError::Backend(format!("mirror journal I/O: {error}"))
}
