//! Durable elevated journal store: the on-disk Intent and Outcome record for elevated setup.
//!
//! The record is `Installer\elevated-setup.json` (`RecordName::ElevatedSetup`), published only
//! through `publish_record`. A write re-reads the record first and must find the bytes this store
//! last read or wrote (compare-and-publish), so a changed record is never overwritten. The schema,
//! bound and Intent/Outcome protocol are checked by the OS-free `ElevatedRecord`.

use super::{
    elevated_launch::ElevatedHelper,
    first_install,
    native_io::{
        Cancellation, Deadline, InstallerLock, MonotonicClock, NativeError, NativeResult,
        WindowsNativeIo,
        files::MAX_RECORD_BYTES,
        records::{Publication, PublicationRecovery, RecordName, encode_record, record_data},
    },
};
use aws_lc_rs::digest::{SHA256, digest};
use crosspane_installer_core::elevated::{
    ElevatedError, RuleScope,
    journal::ElevatedRecord,
    status::{ElevatedJournal, JournalEntry, outcome_entry},
};
use std::sync::Arc;

/// Longest one journal read or write may take, lock wait included. Every `record` call and every
/// `open` gets a fresh budget of this length.
pub(crate) const JOURNAL_WRITE_MS: u64 = 30_000;

/// Where the installer lock for a journal write comes from. `Held` is the lock the caller already
/// retains. `PerWrite` takes it for one write and releases it when that write returns.
pub(crate) enum LockSource<'a> {
    Held(&'a InstallerLock),
    PerWrite,
}

/// The journal as this process sees it: the validated record, the SHA-256 of its bytes as last read
/// or written (`None` while absent), and a sticky `failed` flag. `failed` means a write may have
/// landed without its success being proven, so every later write is refused.
pub(crate) struct StoreJournal<'a> {
    io: &'a WindowsNativeIo,
    lock: LockSource<'a>,
    record: ElevatedRecord,
    known: Option<[u8; 32]>,
    failed: bool,
}

/// Observation only: the record and the SHA-256 of its bytes, or `None` when the record is absent.
/// A record that fails validation is `Invalid`.
pub(crate) fn read(
    io: &WindowsNativeIo,
    deadline: &Deadline,
) -> NativeResult<Option<(ElevatedRecord, [u8; 32])>> {
    let Some(bytes) = observe(io, deadline)? else {
        return Ok(None);
    };
    // The typed body, not a `serde_json::Value`: a `Value` would silently keep one of two duplicate
    // keys, and the typed decode rejects them through `deny_unknown_fields` (records.rs:278-279).
    let record: ElevatedRecord = record_data(&RecordName::ElevatedSetup, &bytes)?;
    record.validate().map_err(|_| NativeError::Invalid)?;
    Ok(Some((record, fingerprint(&bytes))))
}

impl<'a> StoreJournal<'a> {
    /// Missing record → empty for `scope`, published by the first entry. A record with another
    /// scope → `Foreign`.
    pub(crate) fn open(
        io: &'a WindowsNativeIo,
        lock: LockSource<'a>,
        scope: &RuleScope,
    ) -> NativeResult<Self> {
        let deadline = fresh_deadline()?;
        let (record, known) = match read(io, &deadline)? {
            Some((record, sha)) => (record, Some(sha)),
            None => (ElevatedRecord::new(scope), None),
        };
        if record.scope() != *scope {
            return Err(NativeError::Foreign);
        }
        Ok(Self {
            io,
            lock,
            record,
            known,
            failed: false,
        })
    }

    /// Settles each pending verb with an Outcome that has no exit code, observed by the read-only
    /// unelevated `status`. Never elevates. Returns the number of verbs settled.
    pub(crate) fn settle_pending(
        &mut self,
        helper: &ElevatedHelper,
    ) -> Result<usize, ElevatedError> {
        let scope = self.record.scope();
        let mut settled = 0;
        for verb in self.record.pending_verbs() {
            let after = helper.status(Some(&scope)).ok();
            self.record(&outcome_entry(&verb, None, after.as_ref()))?;
            settled += 1;
        }
        Ok(settled)
    }

    /// Whether a write may have landed without its success being proven. Once true, every later
    /// write is refused and the caller must treat the journal as unknown.
    pub(crate) fn failed(&self) -> bool {
        self.failed
    }
}

impl ElevatedJournal for StoreJournal<'_> {
    /// Appends `entry` and publishes the whole record. A refusal before the publish leaves the
    /// journal unchanged and is not `failed`. Only a `NewPublished` outcome with no native failure
    /// counts as written; anything else sets `failed` and is `Journal`.
    fn record(&mut self, entry: &JournalEntry) -> Result<(), ElevatedError> {
        // Deviation (fail-closed): once an uncertain write has happened, nothing more is written
        // through this journal. The caller must re-open it from the disk.
        if self.failed {
            return Err(ElevatedError::Journal);
        }
        let mut next = self.record.clone();
        next.append(entry)?;
        let deadline = fresh_deadline().map_err(|_| ElevatedError::Journal)?;
        let held = match self.lock {
            LockSource::Held(lock) => Some(lock),
            LockSource::PerWrite => None,
        };
        let taken: InstallerLock;
        let lock = match held {
            Some(lock) => lock,
            None => {
                taken =
                    acquire_per_write(self.io, &deadline).map_err(|_| ElevatedError::Journal)?;
                &taken
            }
        };
        let current = observe(self.io, &deadline).map_err(|_| ElevatedError::Journal)?;
        if current.as_deref().map(fingerprint) != self.known {
            return Err(ElevatedError::Journal);
        }
        let bytes = serde_json::to_value(&next)
            .map_err(|_| NativeError::Invalid)
            .and_then(|data| encode_record(&RecordName::ElevatedSetup, data))
            .map_err(|_| ElevatedError::Journal)?;
        let written = fingerprint(&bytes);
        let proof = self
            .io
            .admit_support(&deadline)
            .map_err(|_| ElevatedError::Journal)?;
        let publication =
            self.io
                .publish_record(&proof, lock, RecordName::ElevatedSetup, &bytes, &deadline);
        match publication {
            Ok(Publication {
                state: PublicationRecovery::NewPublished,
                native_failure: None,
                ..
            }) => {
                self.record = next;
                self.known = Some(written);
                Ok(())
            }
            _ => {
                self.failed = true;
                Err(ElevatedError::Journal)
            }
        }
    }
}

/// A fresh `JOURNAL_WRITE_MS` budget on the monotonic clock with no cancellation.
fn fresh_deadline() -> NativeResult<Deadline> {
    Deadline::new(
        JOURNAL_WRITE_MS,
        Arc::new(MonotonicClock::default()),
        Cancellation::default(),
    )
}

/// The record's bytes as observed, bounded by `MAX_RECORD_BYTES`. `None` when absent.
fn observe(io: &WindowsNativeIo, deadline: &Deadline) -> NativeResult<Option<Vec<u8>>> {
    let proof = io.admit_support(deadline)?;
    Ok(io
        .read_record(
            &proof,
            RecordName::ElevatedSetup,
            MAX_RECORD_BYTES,
            deadline,
        )?
        .map(|observed| observed.bytes().to_vec()))
}

/// Takes the installer lock for one write. Only a positively busy lock is retried, within
/// `deadline`; no other failure is replayed.
fn acquire_per_write(io: &WindowsNativeIo, deadline: &Deadline) -> NativeResult<InstallerLock> {
    first_install::retry_busy(
        deadline,
        || {
            let proof = io.admit_support(deadline)?;
            io.acquire_installer_lock(&proof, deadline)
        },
        |ms| std::thread::sleep(std::time::Duration::from_millis(ms)),
    )
}

/// SHA-256 of `bytes`, the correlation the record store uses for complete bytes.
fn fingerprint(bytes: &[u8]) -> [u8; 32] {
    let hash = digest(&SHA256, bytes);
    let mut value = [0; 32];
    value.copy_from_slice(hash.as_ref());
    value
}
