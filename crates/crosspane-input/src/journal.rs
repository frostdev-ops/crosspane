//! Records injected downs before submission and retains them until a successful release.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crosspane_types::hid::{HidUsage, MouseButton};

use crate::Held;

const DOWN: u8 = 0xD1;
const UP: u8 = 0x0F;
const RECORD_BYTES: u64 = 8;
const COMPACT_BYTES: u64 = 4 * 1024;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("journal I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// A record of injected downs that survives a crash of the agent process (04 §8 invariant 2).
pub trait Journal: Send {
    /// Record that `item` is about to be pressed. Returns only once the record has reached the OS.
    fn record_down(&mut self, item: Held) -> Result<(), JournalError>;
    /// Record that `item` was released.
    fn record_up(&mut self, item: Held) -> Result<(), JournalError>;
    /// Items recorded down and not up, sorted.
    fn held(&self) -> Result<Vec<Held>, JournalError>;
}

/// For tests and simulations.
#[derive(Debug, Default)]
pub struct MemoryJournal {
    held: BTreeSet<Held>,
}

impl Journal for MemoryJournal {
    fn record_down(&mut self, item: Held) -> Result<(), JournalError> {
        self.held.insert(item);
        Ok(())
    }

    fn record_up(&mut self, item: Held) -> Result<(), JournalError> {
        self.held.remove(&item);
        Ok(())
    }

    fn held(&self) -> Result<Vec<Held>, JournalError> {
        Ok(self.held.iter().copied().collect())
    }
}

/// An append-only file of fixed 8-byte records.
#[derive(Debug)]
pub struct FileJournal {
    path: PathBuf,
    file: File,
    bytes: u64,
    held: BTreeSet<Held>,
}

impl FileJournal {
    /// Open or create the journal at `path` and replay it.
    pub fn open(path: &Path) -> Result<FileJournal, JournalError> {
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)?;
        let file_bytes = file.metadata()?.len();
        let mut bytes = 0;
        let mut held = BTreeSet::new();
        while file_bytes - bytes >= RECORD_BYTES {
            let mut record = [0; RECORD_BYTES as usize];
            file.read_exact(&mut record)?;
            let Some((op, item)) = decode(record) else {
                break;
            };
            if op == DOWN {
                held.insert(item);
            } else {
                held.remove(&item);
            }
            bytes += RECORD_BYTES;
        }
        // Only the valid prefix is replayable; discard a torn or corrupt suffix before appending.
        if bytes != file_bytes {
            file.set_len(bytes)?;
        }
        let mut journal = FileJournal {
            path: path.to_path_buf(),
            file,
            bytes,
            held,
        };
        journal.compact()?;
        Ok(journal)
    }

    fn record(&mut self, op: u8, item: Held) -> Result<(), JournalError> {
        // File has no process-side buffer. No disk sync is needed for process-crash recovery.
        if let Err(error) = self.file.write_all(&encode(op, item)) {
            // A failed write may have left a partial record. Keep later records replayable.
            self.file.set_len(self.bytes)?;
            return Err(error.into());
        }
        self.bytes += RECORD_BYTES;
        if op == DOWN {
            self.held.insert(item);
        } else {
            self.held.remove(&item);
        }
        self.compact()
    }

    fn compact(&mut self) -> Result<(), JournalError> {
        if self.bytes <= COMPACT_BYTES {
            return Ok(());
        }
        if self.held.is_empty() {
            self.file.set_len(0)?;
            self.bytes = 0;
            return Ok(());
        }

        let (temp_path, mut temp) = self.temporary_file()?;
        let result = (|| {
            for &item in &self.held {
                temp.write_all(&encode(DOWN, item))?;
            }
            fs::rename(&temp_path, &self.path)?;
            Ok::<(), std::io::Error>(())
        })();
        if let Err(error) = result {
            drop(temp);
            let _ = fs::remove_file(temp_path);
            return Err(error.into());
        }
        self.file = temp;
        self.bytes = self.held.len() as u64 * RECORD_BYTES;
        Ok(())
    }

    fn temporary_file(&self) -> Result<(PathBuf, File), JournalError> {
        loop {
            let mut name = self.path.as_os_str().to_os_string();
            let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            name.push(format!(".{}.{}.tmp", std::process::id(), serial));
            let path = PathBuf::from(name);
            match OpenOptions::new()
                .read(true)
                .append(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => return Ok((path, file)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }
}

impl Journal for FileJournal {
    fn record_down(&mut self, item: Held) -> Result<(), JournalError> {
        self.record(DOWN, item)
    }

    fn record_up(&mut self, item: Held) -> Result<(), JournalError> {
        self.record(UP, item)
    }

    fn held(&self) -> Result<Vec<Held>, JournalError> {
        Ok(self.held.iter().copied().collect())
    }
}

fn encode(op: u8, item: Held) -> [u8; RECORD_BYTES as usize] {
    let (kind, a, b) = match item {
        Held::Key(usage) => (0, usage.page, usage.id),
        Held::Button(button) => (1, 0, u16::from(button.0)),
    };
    let check = (u16::from(op) ^ u16::from(kind) ^ a ^ b) ^ 0xC0DE;
    let a = a.to_le_bytes();
    let b = b.to_le_bytes();
    let check = check.to_le_bytes();
    [op, kind, a[0], a[1], b[0], b[1], check[0], check[1]]
}

fn decode(record: [u8; RECORD_BYTES as usize]) -> Option<(u8, Held)> {
    let [op, kind, a0, a1, b0, b1, c0, c1] = record;
    let a = u16::from_le_bytes([a0, a1]);
    let b = u16::from_le_bytes([b0, b1]);
    let check = u16::from_le_bytes([c0, c1]);
    if !matches!(op, DOWN | UP) || check != (u16::from(op) ^ u16::from(kind) ^ a ^ b) ^ 0xC0DE {
        return None;
    }
    let item = match kind {
        0 => Held::Key(HidUsage { page: a, id: b }),
        1 if a == 0 => Held::Button(MouseButton(u8::try_from(b).ok()?)),
        _ => return None,
    };
    Some((op, item))
}

/// Lets the engine hold any journal behind a `Box<dyn Journal>`.
impl<J: Journal + ?Sized> Journal for Box<J> {
    fn record_down(&mut self, item: Held) -> Result<(), JournalError> {
        (**self).record_down(item)
    }

    fn record_up(&mut self, item: Held) -> Result<(), JournalError> {
        (**self).record_up(item)
    }

    fn held(&self) -> Result<Vec<Held>, JournalError> {
        (**self).held()
    }
}
