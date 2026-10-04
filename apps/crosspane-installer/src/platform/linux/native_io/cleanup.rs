//! Read-only ledger authority never implies readiness or clean exit.
//! A lost original process watch remains NotClean; recovery material and identity are retained.
mod delete;
mod intent;
mod ledger;
mod manager;
mod mutation;
mod snapshot;
use super::*;
use crate::platform::linux::payload::{
    FILES, MAX_MEMBER_BYTES, MAX_RECORD_BYTES, PayloadInstaller, sha256,
};
use crosspane_installer_core::{
    InstallReceipt, MutationOutcome, ResourceObservation, ResourceOwnership,
};
pub(super) use mutation::Binding;
pub use mutation::CleanupLease;
use snapshot::Snapshot;
use std::sync::Mutex;

/// Private native snapshots accompany the ledger; receipt bytes alone never grant authority.
#[derive(Clone)]
pub struct CleanupProof(Arc<Admitted>);
struct Admitted {
    io: Arc<LinuxNativeIo>,
    ledger: Snapshot,
    receipt: InstallReceipt,
    entries: Mutex<Vec<Resource>>,
}
struct Resource {
    snapshot: Snapshot,
    hash: [u8; 32],
    owned: bool,
}
impl std::fmt::Debug for CleanupProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CleanupProof(..)")
    }
}
impl CleanupProof {
    pub(crate) fn target_binding(&self) -> TargetBinding {
        self.0.io.target_binding()
    }
    pub fn receipt(&self) -> &InstallReceipt {
        &self.0.receipt
    }
    /// Captured compatibility/absence, never a refreshed native observation.
    pub fn observation(&self, index: usize) -> Result<ResourceObservation> {
        let entries = self.0.entries.try_lock().map_err(|_| NativeError::Busy)?;
        let row = entries.get(index).ok_or(NativeError::Invalid)?;
        Ok(match row.snapshot.hash() {
            None => ResourceObservation::Absent,
            Some(h) if h == row.hash => ResourceObservation::Matching,
            Some(_) => ResourceObservation::Different,
        })
    }
    pub fn owned(&self, index: usize) -> Result<bool> {
        self.0
            .entries
            .try_lock()
            .map_err(|_| NativeError::Busy)?
            .get(index)
            .map(|r| r.owned)
            .ok_or(NativeError::Invalid)
    }
    pub fn revalidate(&self, deadline: &Deadline) -> Result<()> {
        let proof = self.clone();
        let d = deadline.clone();
        bounded_launch(&READ_WORKERS, deadline, move || proof.check(&d))
    }
    fn check(&self, d: &Deadline) -> Result<()> {
        let entries = self
            .0
            .entries
            .lock()
            .map_err(|_| NativeError::Unavailable)?;
        d.check()?;
        self.0.io.validate_target()?;
        self.0.ledger.revalidate(&self.0.io, d)?;
        let intent = self.0.ledger.path.with_file_name("payload-intent.json");
        if self.0.io.metadata(&intent)?.is_some() {
            return Err(NativeError::Foreign);
        }
        for row in entries.iter() {
            row.snapshot.revalidate(&self.0.io, d)?;
        }
        d.check()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Never;
    impl CommandRunner for Never {
        fn run(&self, _: &CommandSpec, _: &Deadline) -> Result<CommandOutput> {
            panic!("read-only proof dispatched a command")
        }
    }
    impl ProcessProbe for Never {
        fn snapshot(&self, _: u32, _: &Deadline) -> Result<ProcessFacts> {
            panic!("read-only proof probed a process")
        }
    }
    #[test]
    fn blocked_cleanup_revalidation_is_capped_cancellable_and_retains_snapshots() {
        let root = PathBuf::from(format!("/tmp/cp419-cleanup-{}", std::process::id()));
        let io = Arc::new(LinuxNativeIo::scratch(&root, Arc::new(Never), Arc::new(Never)).unwrap());
        let path = root.join("pinned-record");
        let fd = rfs::open(
            &path,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .unwrap();
        File::from(fd).write_all(b"private inert record").unwrap();
        let d = Deadline::new(5000, Cancellation::default()).unwrap();
        let (ledger, _) = Snapshot::open(&io, &path, 0o600, MAX_RECORD_BYTES, &d).unwrap();
        let proof = CleanupProof(Arc::new(Admitted {
            io,
            ledger,
            receipt: InstallReceipt {
                schema_version: 1,
                operation_id: crosspane_installer_core::OperationId(1),
                product_version: "inert".into(),
                manifest_sha256: [0; 32],
                payload_sha256: [0; 32],
                resources: vec![],
                unfinished: vec![],
            },
            entries: Mutex::new(vec![]),
        }));
        let held = proof.0.entries.lock().unwrap();
        let start = Instant::now();
        for _ in 0..3 {
            let d = Deadline::new(20, Cancellation::default()).unwrap();
            assert_eq!(proof.revalidate(&d), Err(NativeError::Timeout));
        }
        let cancellation = Cancellation::default();
        let d = Deadline::new(5000, cancellation.clone()).unwrap();
        thread::scope(|s| {
            s.spawn(|| {
                thread::sleep(Duration::from_millis(5));
                cancellation.cancel();
            });
            assert_eq!(proof.revalidate(&d), Err(NativeError::Cancelled));
        });
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(READ_WORKERS.load(Ordering::Acquire), 4);
        assert_eq!(proof.revalidate(&d), Err(NativeError::Cancelled));
        assert_eq!(
            proof.revalidate(&Deadline::new(5000, Cancellation::default()).unwrap()),
            Err(NativeError::Busy)
        );
        assert_eq!(proof.observation(0), Err(NativeError::Busy));
        assert_eq!(proof.owned(0), Err(NativeError::Busy));
        assert!(Arc::strong_count(&proof.0) >= 5);
        drop(held);
        while READ_WORKERS.load(Ordering::Acquire) != 0 && start.elapsed() < Duration::from_secs(1)
        {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(READ_WORKERS.load(Ordering::Acquire), 0);
        assert_eq!(fs::read(&path).unwrap(), b"private inert record");
        drop(proof);
        fs::remove_dir_all(root).unwrap();
    }
}
