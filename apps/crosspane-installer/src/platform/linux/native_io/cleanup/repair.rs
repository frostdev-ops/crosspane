//! Fixed repair-journal snapshots. Neither bytes nor stored paths grant generic delete authority.
use super::*;

pub(crate) struct RepairJournalSnapshot {
    io: Arc<LinuxNativeIo>,
    snapshot: Snapshot,
    bytes: Vec<u8>,
}
impl std::fmt::Debug for RepairJournalSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RepairJournalSnapshot(..)")
    }
}
impl LinuxNativeIo {
    pub(crate) fn capture_repair_journal(
        self: &Arc<Self>,
        proof: &SupportProof,
        d: &Deadline,
    ) -> Result<RepairJournalSnapshot> {
        let (io, proof, d2) = (self.clone(), proof.clone(), d.clone());
        bounded_launch(&READ_WORKERS, d, move || {
            proof.check(&io)?;
            let (snapshot, bytes) =
                Snapshot::open(&io, &fixed_path(&io), 0o600, MAX_RECORD_BYTES, &d2)?;
            proof.check(&io)?;
            snapshot.revalidate(&io, &d2)?;
            Ok(RepairJournalSnapshot {
                io,
                snapshot,
                bytes,
            })
        })
    }
}
pub(super) fn fixed_path(io: &LinuxNativeIo) -> PathBuf {
    io.target
        .paths
        .state_home
        .join("crosspane/installer/repair-intent.json")
}
impl RepairJournalSnapshot {
    pub(crate) fn bytes(&self) -> Option<&[u8]> {
        self.snapshot.hash().map(|_| self.bytes.as_slice())
    }
    /// Consumes the offered snapshot and retains the installation flock through any late worker.
    pub(crate) fn remove(
        self,
        proof: &SupportProof,
        lease: InstallLease,
        d: &Deadline,
    ) -> Result<()> {
        let (proof, d2) = (proof.clone(), d.clone());
        bounded_launch(&PROCESS_LAUNCHES, d, move || {
            let _lease = lease;
            if _lease.nonce != self.io.target.nonce || self.snapshot.hash().is_none() {
                return Err(NativeError::Foreign);
            }
            proof.check(&self.io)?;
            self.snapshot.revalidate(&self.io, &d2)?;
            delete_snapshot(&self.io, &self.snapshot, &d2, || proof.check(&self.io))
        })
    }
}
/// Same displace/verify/restore protocol as captured payload deletion; fixed-path callers only.
fn delete_snapshot(
    io: &LinuxNativeIo,
    snapshot: &Snapshot,
    d: &Deadline,
    check: impl Fn() -> Result<()>,
) -> Result<()> {
    let mut nonce = [0u8; 16];
    aws_lc_rs::rand::fill(&mut nonce).map_err(|_| NativeError::Unavailable)?;
    let displaced = format!(
        ".repair-retire-{}",
        nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
    );
    let name = snapshot.path.file_name().ok_or(NativeError::Invalid)?;
    check()?;
    snapshot.revalidate(io, d)?;
    snapshot.parent_revalidate(io, d)?;
    d.check()?;
    native(rfs::renameat_with(
        &snapshot.parent,
        name,
        &snapshot.parent,
        &displaced,
        rfs::RenameFlags::NOREPLACE,
    ))?;
    let result = (|| {
        let (file, observed) = snapshot.displaced(io, &displaced, d)?;
        check()?;
        snapshot.parent_revalidate(io, d)?;
        snapshot.stable_displaced(&file, &displaced, observed, d)?;
        native(rfs::unlinkat(
            &snapshot.parent,
            &displaced,
            AtFlags::empty(),
        ))
    })();
    if let Err(error) = result {
        return match snapshot.restore(io, &displaced, name) {
            Ok(()) => Err(error),
            Err(_) => Err(NativeError::OutcomeUnknown),
        };
    }
    // The record is unlinked: a durability failure now can't be reported as "nothing changed".
    io.files
        .apply(FileOperation::ParentSync(&snapshot.parent))
        .map_err(|_| NativeError::OutcomeUnknown)
}
impl CleanupProof {
    pub(crate) fn repair_target_matches(
        &self,
        target: &super::super::TargetPaths,
        scratch: bool,
    ) -> bool {
        *target == self.0.io.target.paths && scratch == self.0.io.target.scratch
    }
    pub(crate) fn repair_journal(&self, d: &Deadline) -> Result<Option<Vec<u8>>> {
        self.revalidate(d)?;
        if self.0.repair_foreign {
            return Err(NativeError::Foreign);
        }
        self.0
            .repair
            .as_ref()
            .map(|s| s.read_captured(&self.0.io, d))
            .transpose()
            .map(Option::flatten)
    }
}
impl CleanupLease {
    pub(crate) fn delete_repair_journal(&self, d: &Deadline) -> Result<bool> {
        self.mutate(d, |state, d, started| {
            if state.proof.0.repair_foreign {
                return Err(NativeError::Foreign);
            }
            let Some(snapshot) = &state.proof.0.repair else {
                return Ok(false);
            };
            if state.repair_removed.load(Ordering::Acquire) || snapshot.hash().is_none() {
                return Ok(false);
            }
            state.before(d, started)?;
            delete_snapshot(&state.proof.0.io, snapshot, d, || {
                // The ledger and all retained payload snapshots remain the authority. The
                // journal is now displaced, so only its own absence/parent is checked here.
                state.check_except_repair(d)
            })?;
            state.repair_removed.store(true, Ordering::Release);
            state.before(d, started)?;
            Ok(true)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::mutation::tests::Fixture;
    use super::*;
    use std::cell::Cell;

    #[test]
    fn r2_journal_displacement_restores_on_revocation_cancellation_or_inode_edit() {
        for kind in 0..3 {
            let f = Fixture::new(None, false, false);
            let entries = f.proof.0.entries.lock().unwrap();
            let snapshot = &entries[0].snapshot;
            let io = &f.proof.0.io;
            let cancel = Cancellation::default();
            let d = Deadline::new(5000, cancel.clone()).unwrap();
            let calls = Cell::new(0);
            let result = delete_snapshot(io, snapshot, &d, || {
                calls.set(calls.get() + 1);
                if calls.get() == 2 {
                    match kind {
                        0 => return Err(NativeError::Foreign),
                        1 => {
                            cancel.cancel();
                            return d.check();
                        }
                        _ => {
                            let displaced = std::fs::read_dir(snapshot.path.parent().unwrap())
                                .unwrap()
                                .map(|e| e.unwrap().path())
                                .find(|p| {
                                    p.file_name()
                                        .unwrap()
                                        .to_string_lossy()
                                        .starts_with(".repair-retire-")
                                })
                                .unwrap();
                            std::fs::write(displaced, b"changed while displaced").unwrap();
                        }
                    }
                }
                Ok(())
            });
            assert!(result.is_err(), "case {kind}");
            assert!(snapshot.path.exists());
            assert_eq!(
                std::fs::read(&snapshot.path).unwrap(),
                if kind == 2 {
                    b"changed while displaced".as_slice()
                } else {
                    b"owned inert member".as_slice()
                }
            );
            assert!(
                !std::fs::read_dir(snapshot.path.parent().unwrap())
                    .unwrap()
                    .any(|e| e
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".repair-retire-"))
            );
        }
    }

    #[test]
    fn r2_journal_replaced_after_capture_is_not_displaced() {
        let f = Fixture::new(None, false, false);
        let entries = f.proof.0.entries.lock().unwrap();
        let snapshot = &entries[0].snapshot;
        let replacement = snapshot.path.with_file_name("replacement");
        std::fs::write(&replacement, b"owned inert member").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::rename(&replacement, &snapshot.path).unwrap();
        let d = Deadline::new(5000, Cancellation::default()).unwrap();
        assert_eq!(
            delete_snapshot(&f.proof.0.io, snapshot, &d, || Ok(())).unwrap_err(),
            NativeError::Foreign
        );
        assert_eq!(
            std::fs::read(&snapshot.path).unwrap(),
            b"owned inert member"
        );
    }
}
