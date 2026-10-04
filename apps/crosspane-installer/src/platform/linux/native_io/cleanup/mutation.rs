use super::*;

/// Retains the original ledger, snapshots and installation flock, never generic support authority.
#[derive(Clone)]
pub struct CleanupLease(pub(super) Arc<State>);
pub(super) struct State {
    pub(super) proof: CleanupProof,
    lock: Snapshot,
    pub(super) intent: Mutex<Snapshot>,
    pub(super) removed: Mutex<[bool; FILES.len()]>,
    busy: AtomicBool,
    pub(super) unknown: AtomicBool,
    #[cfg(test)]
    pub(super) before_delete: Mutex<Option<TestHook>>,
    #[cfg(test)]
    pub(super) before_normalize: Mutex<Option<TestHook>>,
    #[cfg(test)]
    pub(super) after_displace: Mutex<Option<TestHook>>,
    #[cfg(test)]
    pub(super) before_unlink: Mutex<Option<TestHook>>,
    #[cfg(test)]
    pub(super) before_restore: Mutex<Option<TestHook>>,
    #[cfg(test)]
    pub(super) after_intent_validation: Mutex<Option<TestHook>>,
    #[cfg(test)]
    pub(super) fail_create_stat: AtomicBool,
    #[cfg(test)]
    pub(super) after_intent_displace: Mutex<Option<TestHook>>,
}
#[cfg(test)]
type TestHook = Arc<dyn Fn() + Send + Sync>;
#[derive(Debug)]
pub(crate) struct Binding {
    lease: CleanupLease,
    pub(super) finished: Arc<AtomicBool>,
}
impl std::fmt::Debug for CleanupLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CleanupLease(..)")
    }
}
impl Drop for Binding {
    fn drop(&mut self) {
        self.lease.0.busy.store(false, Ordering::Release);
        self.finished.store(true, Ordering::Release);
    }
}
impl Binding {
    pub(crate) fn check(&self, d: &Deadline) -> Result<()> {
        self.lease.0.check(d)
    }
}
impl CleanupProof {
    /// Opens the existing fixed lock; creates no file or directory.
    pub fn lease(&self, deadline: &Deadline) -> Result<CleanupLease> {
        let proof = self.clone();
        let d = deadline.clone();
        bounded_launch(&READ_WORKERS, deadline, move || {
            proof.check(&d)?;
            let io = &proof.0.io;
            let parent = proof.0.ledger.path.parent().ok_or(NativeError::Foreign)?;
            let (lock, _) = Snapshot::open(
                io,
                &parent.join("install.lock"),
                0o600,
                MAX_RECORD_BYTES,
                &d,
            )?;
            rfs::flock(lock.fd()?, FlockOperation::NonBlockingLockExclusive)
                .map_err(|_| NativeError::Busy)?;
            let (intent, _) = Snapshot::open(
                io,
                &parent.join("cleanup-intent.json"),
                0o600,
                MAX_RECORD_BYTES,
                &d,
            )?;
            let lease = CleanupLease(Arc::new(State {
                proof,
                lock,
                intent: Mutex::new(intent),
                removed: Mutex::new([false; FILES.len()]),
                busy: AtomicBool::new(false),
                unknown: AtomicBool::new(false),
                #[cfg(test)]
                before_delete: Mutex::default(),
                #[cfg(test)]
                before_normalize: Mutex::default(),
                #[cfg(test)]
                after_displace: Mutex::default(),
                #[cfg(test)]
                before_unlink: Mutex::default(),
                #[cfg(test)]
                before_restore: Mutex::default(),
                #[cfg(test)]
                after_intent_validation: Mutex::default(),
                #[cfg(test)]
                fail_create_stat: AtomicBool::new(false),
                #[cfg(test)]
                after_intent_displace: Mutex::default(),
            }));
            lease.0.check(&d)?;
            Ok(lease)
        })
    }
}
impl State {
    pub(super) fn created_stat(&self, file: &File) -> Result<rfs::Stat> {
        #[cfg(test)]
        if self.fail_create_stat.swap(false, Ordering::AcqRel) {
            return Err(NativeError::Unavailable);
        }
        native(rfs::fstat(file))
    }
    #[cfg(test)]
    pub(super) fn hook(&self, slot: &Mutex<Option<TestHook>>) {
        let hook = slot.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook();
        }
    }
    pub(super) fn check(&self, d: &Deadline) -> Result<()> {
        d.check()?;
        if self.unknown.load(Ordering::Acquire) {
            return Err(NativeError::OutcomeUnknown);
        }
        let io = &self.proof.0.io;
        io.validate_target()?;
        self.proof.0.ledger.revalidate(io, d)?;
        if io
            .metadata(
                &self
                    .proof
                    .0
                    .ledger
                    .path
                    .with_file_name("payload-intent.json"),
            )?
            .is_some()
        {
            return Err(NativeError::Foreign);
        }
        self.lock.revalidate(io, d)?;
        self.intent
            .lock()
            .map_err(|_| NativeError::Unavailable)?
            .revalidate(io, d)?;
        #[cfg(test)]
        self.hook(&self.after_intent_validation);
        let removed = self.removed.lock().map_err(|_| NativeError::Unavailable)?;
        let entries = self
            .proof
            .0
            .entries
            .lock()
            .map_err(|_| NativeError::Unavailable)?;
        for (row, removed) in entries.iter().zip(removed.iter()) {
            if *removed {
                row.snapshot.revalidate_removed(io, d)?;
            } else {
                row.snapshot.revalidate(io, d)?;
            }
        }
        d.check()
    }
    pub(super) fn before(&self, d: &Deadline, started: &AtomicBool) -> Result<()> {
        self.check(d)?;
        d.check()?;
        started.store(true, Ordering::Release);
        Ok(())
    }
}
impl CleanupLease {
    pub(super) fn binding(&self) -> Result<Arc<Binding>> {
        self.0
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| NativeError::Busy)?;
        Ok(Arc::new(Binding {
            lease: self.clone(),
            finished: Arc::new(AtomicBool::new(false)),
        }))
    }
    pub(super) fn mutate<T: Send + 'static>(
        &self,
        d: &Deadline,
        action: impl FnOnce(&State, &Deadline, &AtomicBool) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let binding = self.binding()?;
        let worker_binding = binding.clone();
        let deadline = d.clone();
        let started = Arc::new(AtomicBool::new(false));
        let worker_started = started.clone();
        let result = bounded_launch(&PROCESS_LAUNCHES, d, move || {
            let result = (|| {
                worker_binding.check(&deadline)?;
                let result = action(&worker_binding.lease.0, &deadline, &worker_started)?;
                deadline.check()?;
                Ok(result)
            })();
            if result.is_err() && worker_started.load(Ordering::Acquire) {
                worker_binding
                    .lease
                    .0
                    .unknown
                    .store(true, Ordering::Release);
            }
            result
        });
        #[cfg(test)]
        self.0.hook(&self.0.before_normalize);
        result.map_err(|e| {
            if started.load(Ordering::Acquire) {
                self.0.unknown.store(true, Ordering::Release);
                NativeError::OutcomeUnknown
            } else {
                e
            }
        })
    }
}

impl CleanupLease {
    /// Reads only the captured intent under the lease's worker exclusion; absence grants no authority.
    pub fn read_intent(&self, d: &Deadline) -> Result<Option<Vec<u8>>> {
        let binding = self.binding()?;
        let deadline = d.clone();
        bounded_launch(&READ_WORKERS, d, move || {
            binding.check(&deadline)?;
            let state = &binding.lease.0;
            state
                .intent
                .lock()
                .map_err(|_| NativeError::Unavailable)?
                .read_captured(&state.proof.0.io, &deadline)
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Condvar;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Step {
        Write,
        FileSync,
        Publish,
        ParentSync,
    }
    struct Files {
        steps: Mutex<Vec<Step>>,
        fail: Option<Step>,
        block: bool,
        entered: AtomicBool,
        release: (Mutex<bool>, Condvar),
        substitute: Mutex<Option<PathBuf>>,
        collision: AtomicBool,
        on_write: Mutex<Option<TestHook>>,
        fail_at: AtomicUsize,
        on_sync: Mutex<Option<TestHook>>,
    }
    impl Files {
        fn new(fail: Option<Step>, block: bool) -> Arc<Self> {
            Arc::new(Self {
                steps: Mutex::new(vec![]),
                fail,
                block,
                entered: AtomicBool::new(false),
                release: (Mutex::new(false), Condvar::new()),
                substitute: Mutex::new(None),
                collision: AtomicBool::new(false),
                on_write: Mutex::default(),
                fail_at: AtomicUsize::new(usize::MAX),
                on_sync: Mutex::default(),
            })
        }
        fn unblock(&self) {
            *self.release.0.lock().unwrap() = true;
            self.release.1.notify_all();
        }
    }
    impl FileIo for Files {
        fn apply(&self, op: FileOperation<'_>) -> Result<()> {
            let step = match &op {
                FileOperation::Write(..) => Step::Write,
                FileOperation::FileSync(..) => Step::FileSync,
                FileOperation::IntentRename(..) => Step::Publish,
                FileOperation::ParentSync(..) => Step::ParentSync,
                _ => panic!("cleanup created an unapproved object"),
            };
            self.steps.lock().unwrap().push(step);
            if self.block && step == Step::Write {
                self.entered.store(true, Ordering::Release);
                let mut release = self.release.0.lock().unwrap();
                while !*release {
                    release = self.release.1.wait(release).unwrap();
                }
            }
            if self.fail == Some(step)
                || self.steps.lock().unwrap().len() == self.fail_at.load(Ordering::Acquire)
            {
                return Err(NativeError::Unavailable);
            }
            if self.collision.load(Ordering::Acquire)
                && let FileOperation::IntentRename(dir, _, name) = &op
            {
                put(dir, *name, b"concurrent foreign intent");
            }
            NativeFiles.apply(op)?;
            if step == Step::Write {
                let hook = self.on_write.lock().unwrap().clone();
                if let Some(hook) = hook {
                    hook();
                }
            }
            if step == Step::ParentSync {
                let hook = self.on_sync.lock().unwrap().clone();
                if let Some(hook) = hook {
                    hook();
                }
            }
            if step == Step::FileSync
                && let Some(parent) = self.substitute.lock().unwrap().take()
            {
                // All enumeration and descriptor-relative replacement stay inside this exclusive root.
                let name = fs::read_dir(&parent)
                    .unwrap()
                    .map(|e| e.unwrap().file_name())
                    .find(|n| n.to_string_lossy().starts_with(".cleanup-intent-"))
                    .unwrap();
                let dir = rfs::open(
                    &parent,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
                    Mode::empty(),
                )
                .unwrap();
                rfs::renameat(&dir, &name, &dir, "saved-owned-temp").unwrap();
                put(&dir, &name, b"foreign temp sentinel");
            }
            Ok(())
        }
    }
    struct Never;
    impl CommandRunner for Never {
        fn run(&self, _: &CommandSpec, _: &Deadline) -> Result<CommandOutput> {
            panic!("filesystem test dispatched command")
        }
    }
    impl ProcessProbe for Never {
        fn snapshot(&self, _: u32, _: &Deadline) -> Result<ProcessFacts> {
            panic!("filesystem test probed process")
        }
    }
    fn deadline() -> Deadline {
        Deadline::new(5000, Cancellation::default()).unwrap()
    }
    fn put(dir: &OwnedFd, name: impl rustix::path::Arg, bytes: &[u8]) {
        let file = rfs::openat(
            dir,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )
        .unwrap();
        File::from(file).write_all(bytes).unwrap();
    }
    pub(crate) struct Fixture {
        root: PathBuf,
        parent: OwnedFd,
        pub(crate) proof: CleanupProof,
        files: Arc<Files>,
    }
    impl Fixture {
        pub(crate) fn new(fail: Option<Step>, block: bool, existing: bool) -> Self {
            let root = PathBuf::from(format!(
                "/tmp/cp419-cleanup-write-{}-{}",
                std::process::id(),
                NONCE.fetch_add(1, Ordering::Relaxed)
            ));
            let files = Files::new(fail, block);
            let mut io = LinuxNativeIo::scratch(&root, Arc::new(Never), Arc::new(Never)).unwrap();
            io.files = files.clone();
            let io = Arc::new(io);
            let anchor = rfs::open(
                &root,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .unwrap();
            rfs::mkdirat(&anchor, "records", Mode::RWXU).unwrap();
            let parent = rfs::openat(
                &anchor,
                "records",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .unwrap();
            put(&parent, "installed.json", b"private seam-only ledger");
            put(&parent, "install.lock", b"");
            if existing {
                put(&parent, "cleanup-intent.json", b"old complete intent");
            }
            let d = deadline();
            let (ledger, _) = Snapshot::open(
                &io,
                &root.join("records/installed.json"),
                0o600,
                MAX_RECORD_BYTES,
                &d,
            )
            .unwrap();
            let mut entries = vec![];
            for index in 0..FILES.len() {
                let name = format!("member-{index}");
                if index != 9 {
                    put(&parent, &name, b"owned inert member");
                }
                let (snapshot, _) = Snapshot::open(
                    &io,
                    &root.join("records").join(name),
                    0o600,
                    MAX_RECORD_BYTES,
                    &d,
                )
                .unwrap();
                entries.push(Resource {
                    snapshot,
                    hash: sha256(b"owned inert member"),
                    owned: index != 8,
                });
            }
            // Only a private filesystem seam fixture. Public integration tests mint proof from genuine producer ledgers.
            let proof = CleanupProof(Arc::new(Admitted {
                io,
                ledger,
                entries: Mutex::new(entries),
                receipt: InstallReceipt {
                    schema_version: 1,
                    operation_id: crosspane_installer_core::OperationId(1),
                    product_version: "inert".into(),
                    manifest_sha256: [0; 32],
                    payload_sha256: [0; 32],
                    resources: vec![],
                    unfinished: vec![],
                },
            }));
            Self {
                root,
                parent,
                proof,
                files,
            }
        }
        fn intent(&self) -> Vec<u8> {
            let fd = rfs::openat(
                &self.parent,
                "cleanup-intent.json",
                OFlags::RDONLY | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .unwrap();
            let mut bytes = vec![];
            File::from(fd).read_to_end(&mut bytes).unwrap();
            bytes
        }
        fn wait(&self, predicate: impl Fn() -> bool) {
            let limit = Instant::now() + Duration::from_secs(2);
            while !predicate() && Instant::now() < limit {
                thread::sleep(Duration::from_millis(1));
            }
            assert!(predicate());
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.files.unblock();
            fs::remove_dir_all(&self.root).unwrap();
        }
    }
    #[test]
    fn cleanup_intent_read_timeout_cancel_retains_exclusion_until_worker_finishes() {
        for cancelled in [false, true] {
            let f = Fixture::new(None, false, true);
            let lease = f.proof.lease(&deadline()).unwrap();
            let held = lease.0.proof.0.entries.lock().unwrap();
            let entered = Arc::new(AtomicBool::new(false));
            let seen = entered.clone();
            *lease.0.after_intent_validation.lock().unwrap() = Some(Arc::new(move || {
                seen.store(true, Ordering::Release);
            }));
            let cancellation = Cancellation::default();
            let d =
                Deadline::new(if cancelled { 5000 } else { 1000 }, cancellation.clone()).unwrap();
            let start = Instant::now();
            thread::scope(|scope| {
                if cancelled {
                    scope.spawn(|| {
                        f.wait(|| entered.load(Ordering::Acquire));
                        cancellation.cancel();
                    });
                }
                assert_eq!(
                    lease.read_intent(&d),
                    Err(if cancelled {
                        NativeError::Cancelled
                    } else {
                        NativeError::Timeout
                    })
                );
            });
            assert!(entered.load(Ordering::Acquire));
            assert!(start.elapsed() < Duration::from_secs(2));
            assert_eq!(READ_WORKERS.load(Ordering::Acquire), 1);
            assert_eq!(
                lease.write_intent(b"must not dispatch", &deadline()),
                Err(NativeError::Busy)
            );
            assert!(f.files.steps.lock().unwrap().is_empty());
            assert!(!lease.0.unknown.load(Ordering::Acquire));
            drop(held);
            f.wait(|| {
                READ_WORKERS.load(Ordering::Acquire) == 0 && !lease.0.busy.load(Ordering::Acquire)
            });
            assert_eq!(
                lease.read_intent(&deadline()).unwrap(),
                Some(b"old complete intent".to_vec())
            );
            assert_eq!(f.intent(), b"old complete intent");
        }
    }
    #[test]
    fn cleanup_intent_records_actual_write_sync_atomic_publish_and_directory_sync() {
        let f = Fixture::new(None, false, true);
        let lease = f.proof.lease(&deadline()).unwrap();
        let root = f.root.clone();
        let syncs = Arc::new(AtomicUsize::new(0));
        let seen = syncs.clone();
        *f.files.on_sync.lock().unwrap() = Some(Arc::new(move || {
            let prior: Vec<_> = fs::read_dir(root.join("records"))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(".cleanup-prior-")
                })
                .collect();
            if seen.fetch_add(1, Ordering::AcqRel) == 0 {
                assert_eq!(
                    prior.len(),
                    1,
                    "old copy retained until replacement directory sync"
                );
                assert_eq!(fs::read(&prior[0]).unwrap(), b"old complete intent");
            } else {
                assert!(prior.is_empty());
            }
            assert_eq!(
                fs::read(root.join("records/cleanup-intent.json")).unwrap(),
                b"new complete intent"
            );
        }));
        lease
            .write_intent(b"new complete intent", &deadline())
            .unwrap();
        assert_eq!(
            *f.files.steps.lock().unwrap(),
            [
                Step::Write,
                Step::FileSync,
                Step::Publish,
                Step::ParentSync,
                Step::ParentSync
            ]
        );
        assert_eq!(f.intent(), b"new complete intent");
        assert_eq!(syncs.load(Ordering::Acquire), 2);
    }
    #[test]
    fn cleanup_intent_each_actual_filesystem_failure_preserves_old_or_new_and_retires_lease() {
        let order = [Step::Write, Step::FileSync, Step::Publish, Step::ParentSync];
        for (index, step) in order.into_iter().enumerate() {
            let f = Fixture::new(Some(step), false, true);
            let lease = f.proof.lease(&deadline()).unwrap();
            assert_eq!(
                lease.write_intent(b"new complete intent", &deadline()),
                Err(NativeError::OutcomeUnknown)
            );
            assert_eq!(*f.files.steps.lock().unwrap(), order[..=index]);
            assert_eq!(
                f.intent(),
                if step == Step::ParentSync {
                    b"new complete intent"
                } else {
                    b"old complete intent"
                }
            );
            assert_eq!(
                lease.write_intent(b"retry forbidden", &deadline()),
                Err(NativeError::OutcomeUnknown)
            );
            assert_eq!(
                lease.delete(0, &deadline()),
                Err(NativeError::OutcomeUnknown)
            );
            assert!(f.root.join("records/member-0").exists());
        }
    }
    #[test]
    fn cleanup_intent_substituted_temp_is_never_published_or_cleaned_as_owned() {
        let f = Fixture::new(None, false, true);
        *f.files.substitute.lock().unwrap() = Some(f.root.join("records"));
        let lease = f.proof.lease(&deadline()).unwrap();
        assert_eq!(
            lease.write_intent(b"new complete intent", &deadline()),
            Err(NativeError::OutcomeUnknown)
        );
        assert_eq!(f.intent(), b"old complete intent");
        assert_eq!(
            *f.files.steps.lock().unwrap(),
            [Step::Write, Step::FileSync]
        );
        let name = fs::read_dir(f.root.join("records"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .find(|n| n.to_string_lossy().starts_with(".cleanup-intent-"))
            .unwrap();
        let fd = rfs::openat(
            &f.parent,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .unwrap();
        let mut bytes = vec![];
        File::from(fd).read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"foreign temp sentinel");
    }
    #[test]
    fn cleanup_absent_intent_publication_is_noreplace_and_retains_concurrent_foreign_file() {
        let f = Fixture::new(None, false, false);
        let lease = f.proof.lease(&deadline()).unwrap();
        f.files.collision.store(true, Ordering::Release);
        assert_eq!(
            lease.write_intent(b"owned intended content", &deadline()),
            Err(NativeError::OutcomeUnknown)
        );
        assert_eq!(f.intent(), b"concurrent foreign intent");
        assert_eq!(
            *f.files.steps.lock().unwrap(),
            [Step::Write, Step::FileSync, Step::Publish]
        );
        assert_eq!(
            lease.delete(0, &deadline()),
            Err(NativeError::OutcomeUnknown)
        );
    }
    #[test]
    fn cleanup_timeout_and_cancel_retain_flock_until_noncooperative_file_worker_finishes() {
        for cancel in [false, true] {
            let f = Fixture::new(None, true, true);
            let lease = f.proof.lease(&deadline()).unwrap();
            let cancellation = Cancellation::default();
            let d = Deadline::new(if cancel { 5000 } else { 500 }, cancellation.clone()).unwrap();
            let start = Instant::now();
            let result = thread::scope(|s| {
                let worker = s.spawn(|| lease.write_intent(b"stalled write", &d));
                f.wait(|| f.files.entered.load(Ordering::Acquire));
                if cancel {
                    cancellation.cancel();
                }
                worker.join().unwrap()
            });
            assert_eq!(result, Err(NativeError::OutcomeUnknown));
            assert!(start.elapsed() < Duration::from_secs(1));
            assert!(matches!(f.proof.lease(&deadline()), Err(NativeError::Busy)));
            assert_eq!(lease.delete(0, &deadline()), Err(NativeError::Busy));
            f.files.unblock();
            f.wait(|| !lease.0.busy.load(Ordering::Acquire));
            assert_eq!(f.intent(), b"old complete intent");
            assert!(
                !fs::read_dir(f.root.join("records")).unwrap().any(|e| e
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".cleanup-intent-")),
                "owned temporary intent must be gone before fixture teardown"
            );
            assert_eq!(
                lease.delete(0, &deadline()),
                Err(NativeError::OutcomeUnknown)
            );
            drop(lease);
            assert!(f.proof.lease(&deadline()).is_ok());
        }
    }
    #[test]
    fn cleanup_delete_retains_nonowned_absent_and_recreated_paths_and_rechecks_ancestry() {
        let f = Fixture::new(None, false, false);
        let lease = f.proof.lease(&deadline()).unwrap();
        assert_eq!(lease.delete(8, &deadline()), Ok(false));
        assert_eq!(lease.delete(9, &deadline()), Ok(false));
        assert_eq!(
            lease.delete(FILES.len(), &deadline()),
            Err(NativeError::Invalid)
        );
        assert_eq!(lease.delete(0, &deadline()), Ok(true));
        put(&f.parent, "member-0", b"owned inert member");
        assert_eq!(lease.delete(0, &deadline()), Err(NativeError::Foreign));
        assert!(f.root.join("records/member-0").exists());
        assert!(f.root.join("records/member-8").exists());
        let g = Fixture::new(None, false, false);
        let lease = g.proof.lease(&deadline()).unwrap();
        rfs::fchmod(&g.parent, Mode::from_bits_truncate(0o755)).unwrap();
        assert_eq!(lease.delete(0, &deadline()), Err(NativeError::Foreign));
        assert!(g.root.join("records/member-0").exists());
    }
    #[test]
    fn cleanup_delete_directory_sync_failure_is_unknown_and_never_repeats_unlink() {
        let f = Fixture::new(Some(Step::ParentSync), false, true);
        let lease = f.proof.lease(&deadline()).unwrap();
        assert_eq!(
            lease.delete(0, &deadline()),
            Err(NativeError::OutcomeUnknown)
        );
        assert!(!f.root.join("records/member-0").exists());
        assert_eq!(
            lease.delete(1, &deadline()),
            Err(NativeError::OutcomeUnknown)
        );
        assert!(f.root.join("records/member-1").exists());
        assert_eq!(*f.files.steps.lock().unwrap(), [Step::ParentSync]);
    }
    #[test]
    fn cleanup_no_deadline_or_oversize_intent_performs_no_filesystem_operation() {
        let f = Fixture::new(None, false, true);
        let lease = f.proof.lease(&deadline()).unwrap();
        let cancellation = Cancellation::default();
        cancellation.cancel();
        assert_eq!(
            lease.write_intent(b"cancelled", &Deadline::new(5000, cancellation).unwrap()),
            Err(NativeError::Cancelled)
        );
        let expired = Deadline::new(1, Cancellation::default()).unwrap();
        thread::sleep(Duration::from_millis(3));
        assert_eq!(
            lease.write_intent(b"expired", &expired),
            Err(NativeError::Timeout)
        );
        assert_eq!(
            lease.write_intent(&vec![0; MAX_RECORD_BYTES + 1], &deadline()),
            Err(NativeError::Oversize)
        );
        assert_eq!(f.intent(), b"old complete intent");
        assert!(f.files.steps.lock().unwrap().is_empty());
    }
    #[test]
    fn cleanup_lock_must_already_exist_and_match_private_single_link_snapshot() {
        for change in 0..3 {
            let f = Fixture::new(None, false, false);
            match change {
                0 => rfs::unlinkat(&f.parent, "install.lock", AtFlags::empty()).unwrap(),
                1 => {
                    rfs::unlinkat(&f.parent, "install.lock", AtFlags::empty()).unwrap();
                    rfs::symlinkat("member-0", &f.parent, "install.lock").unwrap();
                }
                _ => rfs::linkat(
                    &f.parent,
                    "install.lock",
                    &f.parent,
                    "lock-alias",
                    AtFlags::empty(),
                )
                .unwrap(),
            }
            assert!(f.proof.lease(&deadline()).is_err());
            assert!(!f.root.join("records/cleanup-intent.json").exists());
            assert!(f.files.steps.lock().unwrap().is_empty());
        }
    }
    #[test]
    fn cleanup_binding_revalidates_original_snapshots_inside_manager_dispatch_worker() {
        let mut f = Fixture::new(None, false, false);
        let anchor = rfs::open(
            &f.root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .unwrap();
        rfs::mkdirat(&anchor, "run", Mode::RWXU).unwrap();
        let run = rfs::openat(
            &anchor,
            "run",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .unwrap();
        rfs::mkdirat(&run, "systemd", Mode::RWXU).unwrap();
        let _listener =
            std::os::unix::net::UnixListener::bind(f.root.join("run/systemd/private")).unwrap();
        let file = File::from(
            rfs::openat(
                &f.parent,
                "installed.json",
                OFlags::WRONLY | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .unwrap(),
        );
        let file = Mutex::new(file);
        let io = Arc::get_mut(&mut Arc::get_mut(&mut f.proof.0).unwrap().io).unwrap();
        io.command_admission = Some(Arc::new(move |target| {
            validate_target(target)?;
            file.lock()
                .unwrap()
                .write_all(b"drift after environment admission")
                .unwrap();
            Ok(())
        }));
        let io = f.proof.0.io.clone();
        let lease = f.proof.lease(&deadline()).unwrap();
        let mut command = CommandSpec::new(
            "/usr/bin/systemctl".into(),
            vec![
                "--user".into(),
                "disable".into(),
                "crosspane-agent.service".into(),
            ],
            io.manager_environment(BTreeMap::new(), &deadline())
                .unwrap(),
            MAX_COMMAND_BYTES,
        )
        .unwrap();
        command.cleanup = Some(lease.binding().unwrap());
        // Never's runner panics if reached: the worker must reject drift first.
        assert_eq!(
            io.execute(&command, &deadline(), None).unwrap_err(),
            NativeError::Foreign
        );
        assert_eq!(f.proof.revalidate(&deadline()), Err(NativeError::Foreign));
        assert!(f.files.steps.lock().unwrap().is_empty());
    }
    #[test]
    fn cleanup_manager_child_reaper_keeps_cleanup_lease_until_actual_owned_child_reap() {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        struct Child(Arc<AtomicBool>);
        impl Cleanup for Child {
            fn terminate(&mut self) {}
            fn reaped(&mut self) -> bool {
                self.0.load(Ordering::Acquire)
            }
        }
        let f = Fixture::new(None, false, false);
        let lease = f.proof.lease(&deadline()).unwrap();
        let binding = lease.binding().unwrap();
        let pending = PendingOperation(binding.finished.clone());
        let reaped = Arc::new(AtomicBool::new(false));
        let sender = cleanup_admission::<ManagerChild<Child>>(&COUNT).unwrap();
        assert!(
            sender
                .try_send(ManagerChild {
                    child: Child(reaped.clone()),
                    _lease: None,
                    _cleanup: Some(binding)
                })
                .is_ok()
        );
        drop(sender);
        drop(lease);
        assert!(!pending.completed());
        assert!(matches!(f.proof.lease(&deadline()), Err(NativeError::Busy)));
        reaped.store(true, Ordering::Release);
        f.wait(|| pending.completed());
        assert!(f.proof.lease(&deadline()).is_ok());
    }
    struct Gate {
        entered: AtomicBool,
        release: (Mutex<bool>, Condvar),
    }
    impl Gate {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                entered: AtomicBool::new(false),
                release: (Mutex::new(false), Condvar::new()),
            })
        }
        fn pause(&self) {
            if self.entered.swap(true, Ordering::AcqRel) {
                return;
            }
            let mut release = self.release.0.lock().unwrap();
            while !*release {
                release = self.release.1.wait(release).unwrap();
            }
        }
        fn resume(&self) {
            *self.release.0.lock().unwrap() = true;
            self.release.1.notify_all();
        }
    }
    struct ManagerRunner {
        calls: AtomicUsize,
        fail: bool,
    }
    impl CommandRunner for ManagerRunner {
        fn run(&self, spec: &CommandSpec, _: &Deadline) -> Result<CommandOutput> {
            assert_eq!(spec.executable(), Path::new("/usr/bin/systemctl"));
            assert_eq!(
                spec.argv(),
                ["--user", "disable", "crosspane-agent.service"]
            );
            self.calls.fetch_add(1, Ordering::AcqRel);
            if self.fail {
                Err(NativeError::Unavailable)
            } else {
                Ok(CommandOutput {
                    code: Some(0),
                    stdout: vec![],
                    stderr: vec![],
                })
            }
        }
    }
    fn manager_fixture(
        f: &mut Fixture,
        fail: bool,
    ) -> (
        Arc<ManagerRunner>,
        std::os::unix::net::UnixListener,
        OwnedFd,
    ) {
        let root = rfs::open(
            &f.root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .unwrap();
        rfs::mkdirat(&root, "run", Mode::RWXU).unwrap();
        let run = rfs::openat(
            &root,
            "run",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .unwrap();
        rfs::mkdirat(&run, "systemd", Mode::RWXU).unwrap();
        let dir = rfs::openat(
            &run,
            "systemd",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .unwrap();
        let listener =
            std::os::unix::net::UnixListener::bind(f.root.join("run/systemd/private")).unwrap();
        let runner = Arc::new(ManagerRunner {
            calls: AtomicUsize::new(0),
            fail,
        });
        Arc::get_mut(&mut Arc::get_mut(&mut f.proof.0).unwrap().io)
            .unwrap()
            .runner = runner.clone();
        (runner, listener, dir)
    }
    fn observed(f: &Fixture) -> crate::platform::linux::service::ServiceFacts {
        crate::platform::linux::service::ServiceFacts {
            fragment: f.root.join("records/member-5"),
            enabled: false,
            active_state: "inactive".into(),
            sub_state: "dead".into(),
            main_pid: 0,
            needs_reload: false,
            source: f.proof.0.io.target.source(),
        }
    }
    #[test]
    fn cleanup_manager_replacement_after_observation_never_dispatches_to_new_endpoint() {
        let mut f = Fixture::new(None, false, false);
        let (runner, _listener, dir) = manager_fixture(&mut f, false);
        let lease = f.proof.lease(&deadline()).unwrap();
        let facts = observed(&f);
        let path = f.root.join("run/systemd/private");
        let substituted = Arc::new(Mutex::new(None));
        let keep = substituted.clone();
        let result = lease.disable_observing(&deadline(), move |_| {
            // Inject the exact boundary after an admitted observation of A, before dispatch admission.
            rfs::renameat(&dir, "private", &dir, "owned-original-manager").unwrap();
            *keep.lock().unwrap() = Some(std::os::unix::net::UnixListener::bind(path).unwrap());
            Ok(facts)
        });
        assert!(substituted.lock().unwrap().is_some());
        assert_eq!(runner.calls.load(Ordering::Acquire), 0);
        assert_eq!(result.result.unwrap_err(), NativeError::Foreign);
    }
    #[test]
    fn cleanup_swapped_basename_is_never_unlinked_as_the_admitted_file() {
        let f = Fixture::new(None, false, false);
        let lease = f.proof.lease(&deadline()).unwrap();
        let parent = rfs::open(
            f.root.join("records"),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .unwrap();
        let fired = Arc::new(AtomicBool::new(false));
        let mark = fired.clone();
        *lease.0.before_delete.lock().unwrap() = Some(Arc::new(move || {
            mark.store(true, Ordering::Release);
            rfs::renameat(&parent, "member-0", &parent, "saved-admitted-member").unwrap();
            put(&parent, "member-0", b"replacement must survive");
        }));
        let result = lease.delete(0, &deadline());
        assert!(fired.load(Ordering::Acquire));
        assert!(matches!(
            result,
            Err(NativeError::Foreign | NativeError::OutcomeUnknown)
        ));
        assert_eq!(
            fs::read(f.root.join("records/member-0")).unwrap(),
            b"replacement must survive"
        );
        assert_eq!(
            fs::read(f.root.join("records/saved-admitted-member")).unwrap(),
            b"owned inert member"
        );
    }
    #[test]
    fn cleanup_uncertain_file_worker_cannot_release_exclusion_before_caller_retirement() {
        let f = Fixture::new(Some(Step::ParentSync), false, true);
        let lease = f.proof.lease(&deadline()).unwrap();
        let gate = Gate::new();
        let pause = gate.clone();
        *lease.0.before_normalize.lock().unwrap() = Some(Arc::new(move || pause.pause()));
        let (first, second, retired) = thread::scope(|s| {
            let first = s.spawn(|| lease.write_intent(b"published uncertain", &deadline()));
            f.wait(|| gate.entered.load(Ordering::Acquire));
            let retired = lease.0.unknown.load(Ordering::Acquire);
            let second = lease.delete(0, &deadline());
            gate.resume();
            (first.join().unwrap(), second, retired)
        });
        assert!(retired, "worker retires before caller normalization");
        assert_eq!(first, Err(NativeError::OutcomeUnknown));
        assert!(matches!(
            second,
            Err(NativeError::Busy | NativeError::OutcomeUnknown)
        ));
        assert!(
            f.root.join("records/member-0").exists(),
            "second mutation must perform no unlink"
        );
        assert_eq!(
            *f.files.steps.lock().unwrap(),
            [Step::Write, Step::FileSync, Step::Publish, Step::ParentSync]
        );
    }
    #[test]
    fn cleanup_uncertain_manager_worker_cannot_release_exclusion_before_caller_retirement() {
        let mut f = Fixture::new(None, false, false);
        let (runner, _listener, _dir) = manager_fixture(&mut f, true);
        let lease = f.proof.lease(&deadline()).unwrap();
        let facts = observed(&f);
        let gate = Gate::new();
        let pause = gate.clone();
        *lease.0.before_normalize.lock().unwrap() = Some(Arc::new(move || pause.pause()));
        let (first, second, retired) = thread::scope(|s| {
            let first = s.spawn(|| lease.disable_observing(&deadline(), move |_| Ok(facts)));
            f.wait(|| gate.entered.load(Ordering::Acquire));
            let retired = lease.0.unknown.load(Ordering::Acquire);
            let second = lease.delete(0, &deadline());
            gate.resume();
            (first.join().unwrap(), second, retired)
        });
        assert!(retired, "worker retires before caller normalization");
        assert_eq!(first.result.unwrap_err(), NativeError::OutcomeUnknown);
        assert!(matches!(
            second,
            Err(NativeError::Busy | NativeError::OutcomeUnknown)
        ));
        assert!(
            f.root.join("records/member-0").exists(),
            "second mutation must perform no unlink"
        );
        assert_eq!(runner.calls.load(Ordering::Acquire), 1);
        assert!(f.files.steps.lock().unwrap().is_empty());
    }
    fn displaced_name(root: &Path) -> String {
        fs::read_dir(root.join("records"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .find(|name| name.starts_with(".cleanup-del-"))
            .unwrap()
    }
    #[test]
    fn cleanup_own_rename_ctime_change_is_accepted_and_unit_index_is_pinned() {
        assert_eq!(
            FILES[super::super::manager::UNIT_INDEX],
            "resources/crosspane-agent.service"
        );
        let f = Fixture::new(None, false, false);
        let lease = f.proof.lease(&deadline()).unwrap();
        let original = snapshot::identity(
            &rfs::statat(&f.parent, "member-0", AtFlags::SYMLINK_NOFOLLOW).unwrap(),
        );
        *lease.0.before_delete.lock().unwrap() =
            Some(Arc::new(|| thread::sleep(Duration::from_millis(2))));
        let root = f.root.clone();
        let parent = f.parent.try_clone().unwrap();
        let changed = Arc::new(AtomicBool::new(false));
        let fired = changed.clone();
        *lease.0.after_displace.lock().unwrap() = Some(Arc::new(move || {
            let observed = snapshot::identity(
                &rfs::statat(&parent, displaced_name(&root), AtFlags::SYMLINK_NOFOLLOW).unwrap(),
            );
            assert_eq!(&original[..8], &observed[..8]);
            assert_ne!(&original[8..], &observed[8..]);
            fired.store(true, Ordering::Release);
        }));
        assert!(lease.delete(0, &deadline()).unwrap());
        assert!(changed.load(Ordering::Acquire));
        assert!(!f.root.join("records/member-0").exists());
        assert!(fs::read_dir(f.root.join("records")).unwrap().all(|e| {
            !e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".cleanup-del-")
        }));
    }
    #[test]
    fn cleanup_displaced_mode_drift_restores_without_unlinking() {
        let f = Fixture::new(None, false, false);
        let lease = f.proof.lease(&deadline()).unwrap();
        let parent = f.parent.try_clone().unwrap();
        let root = f.root.clone();
        *lease.0.after_displace.lock().unwrap() = Some(Arc::new(move || {
            let file = rfs::openat(
                &parent,
                displaced_name(&root),
                OFlags::RDONLY | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .unwrap();
            rfs::fchmod(&file, Mode::RUSR).unwrap();
        }));
        assert_eq!(
            lease.delete(0, &deadline()),
            Err(NativeError::OutcomeUnknown)
        );
        assert_eq!(
            fs::read(f.root.join("records/member-0")).unwrap(),
            b"owned inert member"
        );
        assert!(fs::read_dir(f.root.join("records")).unwrap().all(|e| {
            !e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".cleanup-del-")
        }));
    }
    #[test]
    fn cleanup_displaced_name_substitution_is_retained_and_never_unlinked() {
        let f = Fixture::new(None, false, false);
        let lease = f.proof.lease(&deadline()).unwrap();
        let parent = f.parent.try_clone().unwrap();
        let root = f.root.clone();
        *lease.0.before_unlink.lock().unwrap() = Some(Arc::new(move || {
            let name = displaced_name(&root);
            rfs::renameat(&parent, &name, &parent, "saved-displaced").unwrap();
            put(&parent, &name, b"foreign displaced name");
        }));
        assert_eq!(
            lease.delete(0, &deadline()),
            Err(NativeError::OutcomeUnknown)
        );
        assert_eq!(
            fs::read(f.root.join("records/member-0")).unwrap(),
            b"foreign displaced name"
        );
        assert_eq!(
            fs::read(f.root.join("records/saved-displaced")).unwrap(),
            b"owned inert member"
        );
    }
    #[test]
    fn cleanup_failed_noreplace_restoration_keeps_both_objects_and_uncertainty() {
        let f = Fixture::new(None, false, false);
        let lease = f.proof.lease(&deadline()).unwrap();
        let parent = f.parent.try_clone().unwrap();
        let root = f.root.clone();
        *lease.0.after_displace.lock().unwrap() = Some(Arc::new(move || {
            let file = rfs::openat(
                &parent,
                displaced_name(&root),
                OFlags::RDONLY | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .unwrap();
            rfs::fchmod(&file, Mode::RUSR).unwrap();
        }));
        let parent = f.parent.try_clone().unwrap();
        let restored = Arc::new(AtomicBool::new(false));
        let fired = restored.clone();
        *lease.0.before_restore.lock().unwrap() = Some(Arc::new(move || {
            fired.store(true, Ordering::Release);
            put(&parent, "member-0", b"new basename must survive");
        }));
        assert_eq!(
            lease.delete(0, &deadline()),
            Err(NativeError::OutcomeUnknown)
        );
        assert!(
            restored.load(Ordering::Acquire),
            "exercise actual NOREPLACE restoration collision"
        );
        assert_eq!(
            fs::read(f.root.join("records/member-0")).unwrap(),
            b"new basename must survive"
        );
        assert_eq!(
            fs::read(f.root.join("records").join(displaced_name(&f.root))).unwrap(),
            b"owned inert member"
        );
        assert_eq!(
            lease.write_intent(b"no retry", &deadline()),
            Err(NativeError::OutcomeUnknown)
        );
    }
    #[test]
    fn cleanup_existing_intent_swapped_during_resource_hashing_is_never_overwritten() {
        let f = Fixture::new(None, false, true);
        let lease = f.proof.lease(&deadline()).unwrap();
        let parent = f.parent.try_clone().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let reached = calls.clone();
        let swapped = Arc::new(AtomicBool::new(false));
        let fired = swapped.clone();
        *lease.0.after_intent_validation.lock().unwrap() = Some(Arc::new(move || {
            // Fifth validation: the old intent has been checked, remaining resources are still hashed.
            if reached.fetch_add(1, Ordering::AcqRel) == 4 {
                rfs::renameat(
                    &parent,
                    "cleanup-intent.json",
                    &parent,
                    "saved-original-intent",
                )
                .unwrap();
                put(
                    &parent,
                    "cleanup-intent.json",
                    b"foreign destination must survive",
                );
                fired.store(true, Ordering::Release);
            }
        }));
        let result = lease.write_intent(b"new complete intent", &deadline());
        assert!(swapped.load(Ordering::Acquire));
        assert_eq!(f.intent(), b"foreign destination must survive");
        assert_eq!(
            fs::read(f.root.join("records/saved-original-intent")).unwrap(),
            b"old complete intent"
        );
        assert_eq!(result, Err(NativeError::OutcomeUnknown));
    }
    #[test]
    fn cleanup_manager_swap_during_cleanup_validation_never_reaches_runner() {
        let mut f = Fixture::new(None, false, false);
        let (runner, _listener, dir) = manager_fixture(&mut f, false);
        let lease = f.proof.lease(&deadline()).unwrap();
        let facts = observed(&f);
        let path = f.root.join("run/systemd/private");
        let calls = Arc::new(AtomicUsize::new(0));
        let reached = calls.clone();
        let substituted = Arc::new(Mutex::new(None));
        let keep = substituted.clone();
        *lease.0.after_intent_validation.lock().unwrap() = Some(Arc::new(move || {
            if reached.fetch_add(1, Ordering::AcqRel) == 1 {
                rfs::renameat(&dir, "private", &dir, "owned-original-manager").unwrap();
                *keep.lock().unwrap() =
                    Some(std::os::unix::net::UnixListener::bind(&path).unwrap());
            }
        }));
        let result = lease.disable_observing(&deadline(), move |_| Ok(facts));
        assert!(substituted.lock().unwrap().is_some());
        assert_eq!(runner.calls.load(Ordering::Acquire), 0);
        assert_eq!(result.result.unwrap_err(), NativeError::Foreign);
    }
    fn assert_no_owned_intent_temp(f: &Fixture) {
        assert!(
            fs::read_dir(f.root.join("records")).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".cleanup-intent-")),
            "no owned temporary intent before fixture teardown"
        );
    }
    #[test]
    fn cleanup_ledger_drift_after_temp_creation_does_not_abandon_owned_temp() {
        let f = Fixture::new(None, false, true);
        let lease = f.proof.lease(&deadline()).unwrap();
        let parent = f.parent.try_clone().unwrap();
        let changed = Arc::new(AtomicBool::new(false));
        let fired = changed.clone();
        *f.files.on_write.lock().unwrap() = Some(Arc::new(move || {
            let file = rfs::openat(
                &parent,
                "installed.json",
                OFlags::WRONLY | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .unwrap();
            File::from(file).write_all(b"changed ledger").unwrap();
            fired.store(true, Ordering::Release);
        }));
        assert_eq!(
            lease.write_intent(b"new complete intent", &deadline()),
            Err(NativeError::OutcomeUnknown)
        );
        assert!(changed.load(Ordering::Acquire));
        f.wait(|| !lease.0.busy.load(Ordering::Acquire));
        assert_no_owned_intent_temp(&f);
        assert_eq!(f.intent(), b"old complete intent");
    }
    #[test]
    fn cleanup_postcreate_stat_failure_has_an_immediately_installed_temp_guard() {
        let f = Fixture::new(None, false, true);
        let lease = f.proof.lease(&deadline()).unwrap();
        lease.0.fail_create_stat.store(true, Ordering::Release);
        assert_eq!(
            lease.write_intent(b"new complete intent", &deadline()),
            Err(NativeError::OutcomeUnknown)
        );
        assert!(!lease.0.fail_create_stat.load(Ordering::Acquire));
        f.wait(|| !lease.0.busy.load(Ordering::Acquire));
        assert_no_owned_intent_temp(&f);
        assert_eq!(f.intent(), b"old complete intent");
        assert!(f.files.steps.lock().unwrap().is_empty());
    }
    #[test]
    fn cleanup_existing_intent_noreplace_collision_retains_prior_and_foreign_canonical() {
        let f = Fixture::new(None, false, true);
        let lease = f.proof.lease(&deadline()).unwrap();
        f.files.collision.store(true, Ordering::Release);
        assert_eq!(
            lease.write_intent(b"new complete intent", &deadline()),
            Err(NativeError::OutcomeUnknown)
        );
        assert_eq!(f.intent(), b"concurrent foreign intent");
        let prior = fs::read_dir(f.root.join("records"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".cleanup-prior-")
            })
            .unwrap();
        assert_eq!(fs::read(prior).unwrap(), b"old complete intent");
        assert_no_owned_intent_temp(&f);
    }
    #[test]
    fn cleanup_existing_intent_second_directory_sync_failure_keeps_complete_new_record() {
        let f = Fixture::new(None, false, true);
        let lease = f.proof.lease(&deadline()).unwrap();
        f.files.fail_at.store(5, Ordering::Release);
        assert_eq!(
            lease.write_intent(b"new complete intent", &deadline()),
            Err(NativeError::OutcomeUnknown)
        );
        assert_eq!(
            *f.files.steps.lock().unwrap(),
            [
                Step::Write,
                Step::FileSync,
                Step::Publish,
                Step::ParentSync,
                Step::ParentSync
            ]
        );
        assert_eq!(f.intent(), b"new complete intent");
        assert!(fs::read_dir(f.root.join("records")).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".cleanup-prior-")
        }));
        assert_eq!(
            lease.write_intent(b"no retry", &deadline()),
            Err(NativeError::OutcomeUnknown)
        );
    }
    #[test]
    fn cleanup_existing_intent_displaced_drift_restores_without_deleting_mismatch() {
        let f = Fixture::new(None, false, true);
        let lease = f.proof.lease(&deadline()).unwrap();
        let parent = f.parent.try_clone().unwrap();
        let root = f.root.clone();
        let reached = Arc::new(AtomicBool::new(false));
        let fired = reached.clone();
        *lease.0.after_intent_displace.lock().unwrap() = Some(Arc::new(move || {
            let name = fs::read_dir(root.join("records"))
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .find(|name| name.to_string_lossy().starts_with(".cleanup-prior-"))
                .unwrap();
            let fd = rfs::openat(
                &parent,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .unwrap();
            rfs::fchmod(fd, Mode::RUSR).unwrap();
            fired.store(true, Ordering::Release);
        }));
        assert_eq!(
            lease.write_intent(b"new complete intent", &deadline()),
            Err(NativeError::OutcomeUnknown)
        );
        assert!(reached.load(Ordering::Acquire));
        assert_eq!(f.intent(), b"old complete intent");
        assert_eq!(
            rfs::statat(&f.parent, "cleanup-intent.json", AtFlags::SYMLINK_NOFOLLOW)
                .unwrap()
                .st_mode
                & 0o777,
            0o400
        );
        assert_eq!(
            *f.files.steps.lock().unwrap(),
            [Step::Write, Step::FileSync]
        );
        assert_no_owned_intent_temp(&f);
    }
    #[test]
    fn cleanup_native_spawn_gate_rechecks_same_manager_after_resource_hashing() {
        let mut f = Fixture::new(None, false, false);
        let (runner, _listener, dir) = manager_fixture(&mut f, false);
        let lease = f.proof.lease(&deadline()).unwrap();
        let io = &f.proof.0.io;
        let mut spec = CommandSpec::new(
            "/usr/bin/systemctl".into(),
            vec![
                "--user".into(),
                "disable".into(),
                "crosspane-agent.service".into(),
            ],
            io.manager_environment(BTreeMap::new(), &deadline())
                .unwrap(),
            MAX_COMMAND_BYTES,
        )
        .unwrap();
        spec.cleanup = Some(lease.binding().unwrap());
        assert!(cleanup_dispatch_check(&io.target, &spec, &deadline()).is_ok());
        let path = f.root.join("run/systemd/private");
        let substituted = Arc::new(Mutex::new(None));
        let keep = substituted.clone();
        *lease.0.after_intent_validation.lock().unwrap() = Some(Arc::new(move || {
            rfs::renameat(&dir, "private", &dir, "owned-original-manager").unwrap();
            *keep.lock().unwrap() = Some(std::os::unix::net::UnixListener::bind(&path).unwrap());
        }));
        assert_eq!(
            cleanup_dispatch_check(&io.target, &spec, &deadline()),
            Err(NativeError::Foreign)
        );
        assert!(substituted.lock().unwrap().is_some());
        assert_eq!(runner.calls.load(Ordering::Acquire), 0);
        assert!(f.files.steps.lock().unwrap().is_empty());
    }

    struct StopRunner {
        calls: AtomicUsize,
        gate: Option<Arc<Gate>>,
    }
    impl CommandRunner for StopRunner {
        fn run(&self, spec: &CommandSpec, _: &Deadline) -> Result<CommandOutput> {
            assert_eq!(spec.executable(), Path::new("/usr/bin/systemctl"));
            assert_eq!(spec.argv(), ["--user", "stop", "crosspane-agent.service"]);
            assert!(spec.cleanup.is_some());
            self.calls.fetch_add(1, Ordering::AcqRel);
            if let Some(gate) = &self.gate {
                gate.pause();
            }
            Ok(CommandOutput {
                code: Some(0),
                stdout: vec![],
                stderr: vec![],
            })
        }
    }
    struct GateRelease(Arc<Gate>);
    impl Drop for GateRelease {
        fn drop(&mut self) {
            self.0.resume();
        }
    }
    fn stop_runner(f: &mut Fixture, gate: Option<Arc<Gate>>) -> Arc<StopRunner> {
        let runner = Arc::new(StopRunner {
            calls: AtomicUsize::new(0),
            gate,
        });
        Arc::get_mut(&mut Arc::get_mut(&mut f.proof.0).unwrap().io)
            .unwrap()
            .runner = runner.clone();
        runner
    }
    #[test]
    fn cleanup_stop_exact_recorded_unit_is_progress_and_keeps_files() {
        let mut f = Fixture::new(None, false, false);
        let (_, _listener, _dir) = manager_fixture(&mut f, false);
        let runner = stop_runner(&mut f, None);
        let facts = observed(&f);
        let lease = f.proof.lease(&deadline()).unwrap();
        let result = lease.manager_observing(
            super::super::manager::ManagerAction::Stop,
            &deadline(),
            move |_| Ok(facts),
        );
        assert_eq!(result.result.unwrap().code, Some(0));
        assert!(result.pending.is_none());
        assert_eq!(runner.calls.load(Ordering::Acquire), 1);
        assert!(f.files.steps.lock().unwrap().is_empty());
        f.proof.revalidate(&deadline()).unwrap();
        assert_eq!(lease.read_intent(&deadline()).unwrap(), None);
    }
    #[test]
    fn cleanup_stop_changed_manager_and_foreign_fragment_never_dispatch() {
        for change in 0..3 {
            let mut f = Fixture::new(None, false, false);
            let (_, _listener, dir) = manager_fixture(&mut f, false);
            let runner = stop_runner(&mut f, None);
            let mut facts = observed(&f);
            if change == 1 {
                facts.fragment = f.root.join("unrelated.service");
            }
            if change == 2 {
                facts.source = crosspane_installer_core::ObservationSource::Live;
            }
            let path = f.root.join("run/systemd/private");
            let replacement = Arc::new(Mutex::new(None));
            let keep = replacement.clone();
            let lease = f.proof.lease(&deadline()).unwrap();
            let result = lease.manager_observing(
                super::super::manager::ManagerAction::Stop,
                &deadline(),
                move |_| {
                    if change == 0 {
                        rfs::renameat(&dir, "private", &dir, "owned-original-manager").unwrap();
                        *keep.lock().unwrap() =
                            Some(std::os::unix::net::UnixListener::bind(path).unwrap());
                    }
                    Ok(facts)
                },
            );
            assert_eq!(result.result.unwrap_err(), NativeError::Foreign);
            assert_eq!(runner.calls.load(Ordering::Acquire), 0);
            assert!(f.files.steps.lock().unwrap().is_empty());
            assert_eq!(replacement.lock().unwrap().is_some(), change == 0);
        }
    }
    #[test]
    fn cleanup_stop_cancelled_worker_retains_flock_and_prevents_second_stop() {
        let mut f = Fixture::new(None, false, false);
        let (_, _listener, _dir) = manager_fixture(&mut f, false);
        let gate = Gate::new();
        let release = GateRelease(gate.clone());
        let runner = stop_runner(&mut f, Some(gate.clone()));
        let facts = observed(&f);
        let lease = f.proof.lease(&deadline()).unwrap();
        let cancellation = Cancellation::default();
        let d = Deadline::new(5000, cancellation.clone()).unwrap();
        let result = thread::scope(|scope| {
            let worker = scope.spawn(|| {
                lease.manager_observing(super::super::manager::ManagerAction::Stop, &d, move |_| {
                    Ok(facts)
                })
            });
            f.wait(|| gate.entered.load(Ordering::Acquire));
            cancellation.cancel();
            worker.join().unwrap()
        });
        assert_eq!(result.result.unwrap_err(), NativeError::OutcomeUnknown);
        let pending = result.pending.unwrap();
        assert!(!pending.completed());
        let facts = observed(&f);
        assert_eq!(
            lease
                .manager_observing(
                    super::super::manager::ManagerAction::Stop,
                    &deadline(),
                    move |_| Ok(facts)
                )
                .result
                .unwrap_err(),
            NativeError::Busy
        );
        assert_eq!(runner.calls.load(Ordering::Acquire), 1);
        drop(lease);
        assert!(matches!(f.proof.lease(&deadline()), Err(NativeError::Busy)));
        gate.resume();
        f.wait(|| pending.completed());
        assert!(f.proof.lease(&deadline()).is_ok());
        drop(release);
    }
}
