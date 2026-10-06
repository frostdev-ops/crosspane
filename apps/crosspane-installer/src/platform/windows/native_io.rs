//! Windows installer native foundation. Observations and record bytes are never authority.
//! The native adapter is Windows-only; bounded admission decisions remain portable for tests.

#[path = "native_io/files.rs"]
pub mod files;
#[path = "native_io/identity.rs"]
pub mod identity;
#[path = "native_io/process.rs"]
pub mod process;
#[path = "native_io/records.rs"]
pub mod records;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NativeError {
    #[error("invalid bounded native value")]
    Invalid,
    #[error("native observation unavailable")]
    Unavailable,
    #[error("foreign or unsafe resource")]
    Foreign,
    #[error("unsupported or unknown target")]
    Unsupported,
    #[error("positively missing admitted native leaf")]
    Missing,
    #[error("operation timed out before dispatch")]
    Timeout,
    #[error("operation cancelled before dispatch")]
    Cancelled,
    #[error("bounded native owner or installer lock busy")]
    Busy,
    #[error("bounded record exceeded")]
    Oversize,
    #[error("native mutation outcome unknown; inspect before retry")]
    OutcomeUnknown,
}
pub type NativeResult<T> = Result<T, NativeError>;
pub use process::{Cancellation, Clock, Deadline, MonotonicClock};

#[cfg(windows)]
pub(crate) use adapter::AgentObservation;
#[cfg(all(windows, test))]
#[allow(unused_imports)]
// Source-included probe exports; library unit tests do not invoke them.
pub(crate) use adapter::scratch::{FixtureLock, ScratchFixture, scratch_current};
#[cfg(windows)]
pub use adapter::{InstallerLock, SupportProof, WindowsNativeIo, WindowsTarget};

#[cfg(windows)]
mod adapter {
    use super::super::detect::{FixedPaths, FolderFacts};
    use super::*;
    use files::{
        Admission, FileIdentity, PrivateName,
        native::{self, Anchor, Security},
    };
    use identity::TokenFacts;
    use process::{CallOwner, Dispatch};
    use std::{
        fs::File,
        os::windows::io::AsRawHandle,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };
    use windows::Win32::{System::Com::*, UI::Shell::*};
    use windows_sys::Win32::{Storage::FileSystem::*, System::IO::OVERLAPPED};

    struct Apartment;
    impl Apartment {
        fn initialize() -> NativeResult<Self> {
            // SAFETY: this is a newly owned native-call worker thread; no caller apartment is changed.
            unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
                .ok()
                .map_err(|_| NativeError::Unavailable)?;
            Ok(Self)
        }
    }
    impl Drop for Apartment {
        fn drop(&mut self) {
            // SAFETY: exactly balances successful initialization on this same owner thread.
            unsafe {
                CoUninitialize();
            }
        }
    }
    fn folder(id: &windows::core::GUID) -> NativeResult<String> {
        // SAFETY: read-only current-token Shell folder resolution; DONT_VERIFY never creates it.
        let value = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DONT_VERIFY, None) }
            .map_err(|_| NativeError::Unavailable)?;
        if value.0.is_null() {
            return Err(NativeError::Unavailable);
        }
        let result = (|| {
            for length in 0..32768 {
                // SAFETY: Shell returned a NUL-terminated CoTaskMem-owned UTF-16 path.
                if unsafe { value.0.add(length).read() } == 0 {
                    // SAFETY: NUL was found within the bounded native path allocation.
                    return String::from_utf16(unsafe {
                        std::slice::from_raw_parts(value.0, length)
                    })
                    .map_err(|_| NativeError::Unavailable);
                }
            }
            Err(NativeError::Oversize)
        })();
        // SAFETY: frees exactly the string returned by SHGetKnownFolderPath, even on decoding error.
        unsafe {
            CoTaskMemFree(Some(value.0.cast()));
        }
        result
    }
    fn paths() -> NativeResult<FixedPaths> {
        let _apartment = Apartment::initialize()?;
        FixedPaths::admit(FolderFacts {
            local: folder(&FOLDERID_LocalAppData)?,
            roaming: folder(&FOLDERID_RoamingAppData)?,
            programs: folder(&FOLDERID_UserProgramFiles)?,
            environment_local: std::env::var("LOCALAPPDATA")
                .map_err(|_| NativeError::Unsupported)?,
            environment_roaming: std::env::var("APPDATA").map_err(|_| NativeError::Unsupported)?,
        })
    }
    fn nonce() -> NativeResult<[u8; 16]> {
        let mut value = [0; 16];
        aws_lc_rs::rand::fill(&mut value).map_err(|_| NativeError::Unavailable)?;
        Ok(value)
    }
    pub struct WindowsTarget {
        nonce: [u8; 16],
        identity: TokenFacts,
        paths: FixedPaths,
    }
    impl WindowsTarget {
        pub fn identity(&self) -> &TokenFacts {
            &self.identity
        }
        pub fn paths(&self) -> &FixedPaths {
            &self.paths
        }
    }
    impl std::fmt::Debug for WindowsTarget {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("WindowsTarget")
        }
    }
    struct Context {
        target: WindowsTarget,
        security: Security,
        local: Arc<Anchor>,
        roaming: Arc<Anchor>,
        installer: Mutex<Option<Arc<Anchor>>>,
    }
    impl Context {
        fn current(deadline: &Deadline) -> NativeResult<Self> {
            deadline.check()?;
            let identity = identity::native::current()?.facts().clone();
            let security = Security {
                user: identity.user.clone(),
                trusted_installer: identity::native::trusted_installer().ok(),
            };
            let paths = paths()?;
            let local = Arc::new(
                Anchor::open(paths.local(), &security, false, deadline)?
                    .ok_or(NativeError::Unavailable)?,
            );
            let roaming = Arc::new(
                Anchor::open(paths.roaming(), &security, false, deadline)?
                    .ok_or(NativeError::Unavailable)?,
            );
            // Read-only support validates present fixed roots. Missing roots grant no arbitrary-path
            // capability; only future packages may create their explicitly frozen fixed inventory.
            Anchor::open(paths.programs(), &security, false, deadline)?;
            Anchor::open(paths.install(), &security, false, deadline)?;
            let installer =
                Anchor::open(paths.installer(), &security, true, deadline)?.map(Arc::new);
            Ok(Self {
                target: WindowsTarget {
                    nonce: nonce()?,
                    identity,
                    paths,
                },
                security,
                local,
                roaming,
                installer: Mutex::new(installer),
            })
        }
        fn validate(&self, deadline: &Deadline) -> NativeResult<()> {
            deadline.check()?;
            if identity::native::current()?.facts() != &self.target.identity {
                return Err(NativeError::Foreign);
            }
            if paths()? != self.target.paths {
                return Err(NativeError::Foreign);
            }
            self.local.revalidate(&self.security, false, deadline)?;
            self.roaming.revalidate(&self.security, false, deadline)?;
            Anchor::open(
                self.target.paths.programs(),
                &self.security,
                false,
                deadline,
            )?;
            Anchor::open(self.target.paths.install(), &self.security, false, deadline)?;
            if let Some(installer) = &*self
                .installer
                .lock()
                .map_err(|_| NativeError::Unavailable)?
            {
                installer.revalidate(&self.security, true, deadline)?;
            }
            Ok(())
        }
        fn installer(&self, deadline: &Deadline, change: &Change) -> NativeResult<Arc<Anchor>> {
            let mut slot = self
                .installer
                .lock()
                .map_err(|_| NativeError::Unavailable)?;
            if let Some(anchor) = &*slot {
                anchor.revalidate(&self.security, true, deadline)?;
                return Ok(anchor.clone());
            }
            let crosspane_path = format!("{}\\Crosspane", self.target.paths.local());
            let parent = match Anchor::open(&crosspane_path, &self.security, false, deadline)? {
                Some(parent) => parent,
                None => {
                    change.reached();
                    self.local
                        .create_child_directory("Crosspane", &self.security, deadline)?
                }
            };
            let installer = match Anchor::open(
                self.target.paths.installer(),
                &self.security,
                true,
                deadline,
            )? {
                Some(anchor) => anchor,
                None => {
                    change.reached();
                    parent.create_child_directory("Installer", &self.security, deadline)?
                }
            };
            let installer = Arc::new(installer);
            *slot = Some(installer.clone());
            Ok(installer)
        }
    }
    /// Tracks MAY-have-mutated before each native write; no timeout or native error means rollback.
    struct Change(AtomicBool);
    impl Change {
        fn new() -> Self {
            Self(AtomicBool::new(false))
        }
        fn reached(&self) {
            self.0.store(true, Ordering::Release);
        }
        fn finish<T>(&self, result: NativeResult<T>) -> NativeResult<T> {
            result.map_err(|error| {
                if self.0.load(Ordering::Acquire) {
                    NativeError::OutcomeUnknown
                } else {
                    error
                }
            })
        }
    }
    pub struct SupportProof {
        target: [u8; 16],
        issued: u64,
        wall: Instant,
    }
    impl std::fmt::Debug for SupportProof {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("SupportProof")
        }
    }
    impl SupportProof {
        fn binding(&self, io: &WindowsNativeIo, deadline: &Deadline) -> NativeResult<()> {
            identity::native::refuse_impersonation()?;
            deadline.check()?;
            if self.target != io.context.target.nonce {
                return Err(NativeError::Foreign);
            }
            if io
                .clock
                .now_ms()
                .checked_sub(self.issued)
                .is_none_or(|age| age > 5000)
                || self.wall.elapsed() > Duration::from_secs(5)
            {
                return Err(NativeError::Unsupported);
            }
            Ok(())
        }
        fn budget(&self, io: &WindowsNativeIo, deadline: &Deadline) -> NativeResult<Deadline> {
            self.binding(io, deadline)?;
            let logical_age = io
                .clock
                .now_ms()
                .checked_sub(self.issued)
                .ok_or(NativeError::Unsupported)?;
            let wall_age = self.wall.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            deadline.shorten(5000u64.saturating_sub(logical_age.max(wall_age)))
        }
        pub fn check(&self, io: &WindowsNativeIo, deadline: &Deadline) -> NativeResult<()> {
            let budget = self.budget(io, deadline)?;
            let context = io.context.clone();
            io.owner.run(Dispatch::Observation, deadline, move || {
                context.validate(&budget)
            })
        }
    }
    struct LockState {
        target: [u8; 16],
        file: File,
        identity: FileIdentity,
        parent: Arc<Anchor>,
    }
    pub struct InstallerLock(Arc<LockState>);
    impl std::fmt::Debug for InstallerLock {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("InstallerLock")
        }
    }
    pub struct WindowsNativeIo {
        context: Arc<Context>,
        owner: Arc<CallOwner>,
        clock: Arc<dyn Clock>,
    }
    /// Read-only sealed observations, bound to the genuine target context. Non-Clone/non-Copy.
    /// The pinned fixed image leaf is not a queried loaded-section FileId (W4.1a2 ruling).
    pub(crate) struct AgentObservation(Arc<AgentObservationData>);
    struct AgentObservationData {
        target: [u8; 16],
        runtime: Arc<Anchor>,
        install: Arc<Anchor>,
        runtime_canonical: std::ffi::OsString,
        image_canonical: String,
        image: File,
        image_identity: FileIdentity,
        process: process::selected::SelectedProcess,
        bootstrap: crate::agent_contract::BootstrapV1,
        phase_seq: std::sync::atomic::AtomicU64,
    }
    impl std::fmt::Debug for AgentObservation {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("AgentObservation")
        }
    }
    impl AgentObservation {
        pub(crate) fn bootstrap(&self) -> &crate::agent_contract::BootstrapV1 {
            &self.0.bootstrap
        }
        pub(crate) fn runtime_canonical(&self) -> &std::ffi::OsStr {
            &self.0.runtime_canonical
        }
        pub(crate) fn image_canonical(&self) -> &str {
            &self.0.image_canonical
        }
        #[cfg(test)]
        #[allow(dead_code)] // Fixture-only observation view; no production caller.
        pub(crate) fn image_identity(&self) -> FileIdentity {
            self.0.image_identity
        }
        #[cfg(test)]
        #[allow(dead_code)] // Fixture-only observation view; no production caller.
        pub(crate) fn image_handle(&self) -> &File {
            &self.0.image
        }
        pub(crate) fn revalidate(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            let budget = proof.budget(io, deadline)?;
            if self.0.target != io.context.target.nonce {
                return Err(NativeError::Foreign);
            }
            let observed = self.0.clone();
            let context = io.context.clone();
            io.owner.run(Dispatch::Observation, deadline, move || {
                context.validate(&budget)?;
                observed.process.revalidate(&budget)?;
                observed
                    .runtime
                    .revalidate(&context.security, true, &budget)?;
                observed
                    .install
                    .revalidate(&context.security, false, &budget)?;
                if observed
                    .runtime
                    .canonical_dos_path(&context.security, &budget)?
                    != observed.runtime_canonical
                {
                    return Err(NativeError::Foreign);
                }
                let facts =
                    native::observe(&observed.image, "crosspane-agent.exe", &context.security)?;
                files::admit_component(&facts, Admission::PrivateFile)?;
                let (_, current_image) = observed.install.open_file_metadata(
                    &PrivateName::new("crosspane-agent.exe")?,
                    &context.security,
                    &budget,
                )?;
                if facts.identity != observed.image_identity
                    || current_image != observed.image_identity
                {
                    return Err(NativeError::Foreign);
                }
                let (_, bytes) = observed
                    .runtime
                    .read_private(
                        &PrivateName::new("bootstrap.json")?,
                        &context.security,
                        crate::agent_contract::MAX_RESPONSE_BYTES,
                        &budget,
                    )?
                    .ok_or(NativeError::Missing)?;
                let current = crate::agent_contract::parse_bootstrap(&bytes)
                    .map_err(|_| NativeError::Invalid)?;
                process::bootstrap_matches(&observed.bootstrap, &current)?;
                observed
                    .phase_seq
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                        (current.phase_seq >= old).then_some(current.phase_seq)
                    })
                    .map_err(|_| NativeError::Foreign)?;
                observed.process.revalidate(&budget)?;
                budget.check()
            })
        }
    }
    impl std::fmt::Debug for WindowsNativeIo {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("WindowsNativeIo")
        }
    }
    struct NativeStore {
        context: Arc<Context>,
        lease: Arc<LockState>,
        budget: Deadline,
        name: records::RecordName,
        change: Change,
    }
    impl NativeStore {
        fn check(&self) -> NativeResult<()> {
            self.context.validate(&self.budget)?;
            self.lease
                .parent
                .revalidate(&self.context.security, true, &self.budget)?;
            let facts = native::observe(&self.lease.file, "install.lock", &self.context.security)?;
            files::admit_component(&facts, Admission::PrivateFile)?;
            if facts.identity != self.lease.identity
                || self.lease.target != self.context.target.nonce
            {
                return Err(NativeError::Foreign);
            }
            self.budget.check()
        }
        fn read(&self, name: &PrivateName) -> NativeResult<Option<Vec<u8>>> {
            self.check()?;
            self.lease
                .parent
                .read_private(
                    name,
                    &self.context.security,
                    files::MAX_RECORD_BYTES,
                    &self.budget,
                )
                .map(|record| record.map(|(_, bytes)| bytes))
        }
    }
    impl records::RecordStore for NativeStore {
        type Temporary = File;
        fn target(&self) -> records::RecordName {
            self.name.clone()
        }
        fn context(&self) -> Option<[u8; 16]> {
            Some(self.context.target.nonce)
        }
        fn read_final(&mut self) -> NativeResult<Option<Vec<u8>>> {
            let record = self.read(&self.name.file_name()?)?;
            if let Some(bytes) = &record {
                records::validate_for(&self.name, bytes)?;
            }
            Ok(record)
        }
        fn create(&mut self) -> NativeResult<(PrivateName, File)> {
            self.check()?;
            let name = PrivateName::new(&format!("record-temp-{}.json", records::hex(&nonce()?)))?;
            self.change.reached();
            let file =
                self.lease
                    .parent
                    .create_private(&name, &self.context.security, &self.budget)?;
            Ok((name, file))
        }
        fn write(&mut self, file: &mut File, bytes: &[u8]) -> NativeResult<()> {
            use std::io::Write;
            self.check()?;
            files::check_read_size(bytes.len(), files::MAX_RECORD_BYTES)?;
            self.change.reached();
            file.write_all(bytes)
                .map_err(|_| NativeError::OutcomeUnknown)
        }
        fn flush(&mut self, file: &File) -> NativeResult<()> {
            self.check()?;
            self.change.reached();
            // SAFETY: own created complete record, with write access, retained through completion.
            if unsafe { FlushFileBuffers(file.as_raw_handle()) } == 0 {
                return Err(native::last_error());
            }
            self.budget.check()
        }
        fn publish(&mut self, file: &File) -> NativeResult<()> {
            self.check()?;
            self.change.reached();
            self.lease.parent.publish_private(
                file,
                &self.name.file_name()?,
                &self.context.security,
                &self.budget,
            )
        }
        fn read_temporary(&mut self, name: &PrivateName) -> NativeResult<Option<Vec<u8>>> {
            self.read(name)
        }
    }
    impl WindowsNativeIo {
        pub fn current(clock: Arc<dyn Clock>, deadline: &Deadline) -> NativeResult<Self> {
            identity::native::refuse_impersonation()?;
            let owner = Arc::new(CallOwner::default());
            let budget = deadline.clone();
            let context = owner.run(Dispatch::Observation, deadline, move || {
                Context::current(&budget)
            })?;
            Ok(Self {
                context: Arc::new(context),
                owner,
                clock,
            })
        }
        pub fn target(&self) -> &WindowsTarget {
            &self.context.target
        }
        /// Only the fixed agent/runtime leaves beneath Shell-admitted roots; no path arguments.
        pub(crate) fn observe_agent(
            &self,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<AgentObservation> {
            let budget = proof.budget(self, deadline)?;
            let context = self.context.clone();
            self.owner.run(Dispatch::Observation, deadline, move || {
                context.validate(&budget)?;
                let runtime = Arc::new(
                    Anchor::open(
                        &format!("{}\\Crosspane\\runtime", context.target.paths.local()),
                        &context.security,
                        true,
                        &budget,
                    )?
                    .ok_or(NativeError::Missing)?,
                );
                let install = Arc::new(
                    Anchor::open(
                        context.target.paths.install(),
                        &context.security,
                        false,
                        &budget,
                    )?
                    .ok_or(NativeError::Missing)?,
                );
                let runtime_canonical = runtime.canonical_dos_path(&context.security, &budget)?;
                let image_canonical = std::path::PathBuf::from(
                    install.canonical_dos_path(&context.security, &budget)?,
                )
                .join("crosspane-agent.exe")
                .to_str()
                .ok_or(NativeError::Unsupported)?
                .to_owned();
                let (image, image_identity) = install.open_file_metadata(
                    &PrivateName::new("crosspane-agent.exe")?,
                    &context.security,
                    &budget,
                )?;
                let (_, bytes) = runtime
                    .read_private(
                        &PrivateName::new("bootstrap.json")?,
                        &context.security,
                        crate::agent_contract::MAX_RESPONSE_BYTES,
                        &budget,
                    )?
                    .ok_or(NativeError::Missing)?;
                let bootstrap = crate::agent_contract::parse_bootstrap(&bytes)
                    .map_err(|_| NativeError::Invalid)?;
                if process::literal_path(&bootstrap.runtime_dir)?
                    != process::literal_path(
                        runtime_canonical.to_str().ok_or(NativeError::Unsupported)?,
                    )?
                {
                    return Err(NativeError::Foreign);
                }
                process::bootstrap_matches(&bootstrap, &bootstrap)?;
                let process = process::selected::SelectedProcess::admit(
                    &bootstrap,
                    &context.target.identity,
                    &image_canonical,
                    image_identity,
                    &budget,
                )?;
                budget.check()?;
                Ok(AgentObservation(Arc::new(AgentObservationData {
                    target: context.target.nonce,
                    runtime,
                    install,
                    runtime_canonical,
                    image_canonical,
                    image,
                    image_identity,
                    process,
                    phase_seq: std::sync::atomic::AtomicU64::new(bootstrap.phase_seq),
                    bootstrap,
                })))
            })
        }
        pub fn admit_support(&self, deadline: &Deadline) -> NativeResult<SupportProof> {
            identity::native::refuse_impersonation()?;
            let context = self.context.clone();
            let budget = deadline.clone();
            self.owner.run(Dispatch::Observation, deadline, move || {
                context.validate(&budget)
            })?;
            Ok(SupportProof {
                target: self.context.target.nonce,
                issued: self.clock.now_ms(),
                wall: Instant::now(),
            })
        }
        pub fn acquire_installer_lock(
            &self,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<InstallerLock> {
            let budget = proof.budget(self, deadline)?;
            let context = self.context.clone();
            self.owner.run(Dispatch::Mutation, deadline, move || {
                let change = Change::new();
                change.finish((|| {
                    context.validate(&budget)?;
                    let parent = context.installer(&budget, &change)?;
                    let name = PrivateName::new("install.lock")?;
                    let file = match parent.open_lock(&context.security, &budget)? {
                        Some(file) => file,
                        None => {
                            change.reached();
                            parent.create_lock(&context.security, &budget)?
                        }
                    };
                    let facts = native::observe(&file, name.as_str(), &context.security)?;
                    files::admit_component(&facts, Admission::PrivateFile)?;
                    if facts.identity.volume != parent.identity()?.volume {
                        return Err(NativeError::Foreign);
                    }
                    let mut overlap = OVERLAPPED::default();
                    budget.check()?;
                    // SAFETY: owned regular file, retained parent; immediate exclusive byte lock, no PID contents used.
                    if unsafe {
                        LockFileEx(
                            file.as_raw_handle(),
                            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                            0,
                            1,
                            0,
                            &mut overlap,
                        )
                    } == 0
                    {
                        return Err(native::last_error());
                    }
                    Ok(InstallerLock(Arc::new(LockState {
                        target: context.target.nonce,
                        file,
                        identity: facts.identity,
                        parent,
                    })))
                })())
            })
        }
        fn lock_binding(
            &self,
            proof: &SupportProof,
            lock: &InstallerLock,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            proof.binding(self, deadline)?;
            if lock.0.target != self.context.target.nonce {
                return Err(NativeError::Foreign);
            }
            Ok(())
        }
        pub fn read_record(
            &self,
            proof: &SupportProof,
            name: records::RecordName,
            max_bytes: usize,
            deadline: &Deadline,
        ) -> NativeResult<Option<records::ObservedRecord>> {
            files::check_read_size(0, max_bytes)?;
            let file_name = name.file_name()?;
            let budget = proof.budget(self, deadline)?;
            let context = self.context.clone();
            self.owner.run(Dispatch::Observation, deadline, move || {
                context.validate(&budget)?;
                let parent = Anchor::open(
                    context.target.paths.installer(),
                    &context.security,
                    true,
                    &budget,
                )?;
                match parent {
                    None => Ok(None),
                    Some(parent) => parent
                        .read_private(&file_name, &context.security, max_bytes, &budget)?
                        .map(|(identity, bytes)| {
                            records::validate_for(&name, &bytes)?;
                            Ok(records::ObservedRecord::new(identity, bytes))
                        })
                        .transpose(),
                }
            })
        }
        /// Whole-record publication; a native failure remains explicit even when observation
        /// proves the new complete record. No old/partial/unknown outcome is automatically retried.
        pub fn publish_record(
            &self,
            proof: &SupportProof,
            lock: &InstallerLock,
            name: records::RecordName,
            bytes: &[u8],
            deadline: &Deadline,
        ) -> NativeResult<records::Publication> {
            self.lock_binding(proof, lock, deadline)?;
            records::validate_for(&name, bytes)?;
            let budget = proof.budget(self, deadline)?;
            let context = self.context.clone();
            let lease = lock.0.clone();
            let bytes = bytes.to_vec();
            let owner = self.owner.clone();
            self.owner.run(Dispatch::Mutation, deadline, move || {
                let mut store = NativeStore {
                    context,
                    lease,
                    budget,
                    name,
                    change: Change::new(),
                };
                let budget = store.budget.clone();
                let result = records::publish(&mut store, &bytes, &budget);
                let result = store.change.finish(result);
                if result.as_ref().is_ok_and(|publication| {
                    publication.native_failure.is_some()
                        || publication.state != records::PublicationRecovery::NewPublished
                }) {
                    owner.retire_mutations();
                }
                result
            })
        }
        pub fn inspect_publication(
            &self,
            proof: &SupportProof,
            lock: &InstallerLock,
            intent: &records::PublicationIntent,
            deadline: &Deadline,
        ) -> NativeResult<records::PublicationRecovery> {
            self.lock_binding(proof, lock, deadline)?;
            if intent.context != Some(self.context.target.nonce) {
                return Err(NativeError::Foreign);
            }
            let context = self.context.clone();
            let lease = lock.0.clone();
            let intent = intent.clone();
            let budget = proof.budget(self, deadline)?;
            self.owner.run(Dispatch::Observation, deadline, move || {
                let store = NativeStore {
                    context,
                    lease,
                    budget,
                    name: intent.target.clone(),
                    change: Change::new(),
                };
                let record = store.read(&intent.target.file_name()?)?;
                let temporary = intent
                    .temporary
                    .as_ref()
                    .map(|name| store.read(name))
                    .transpose()?
                    .flatten();
                Ok(records::recover(
                    &intent,
                    record.as_deref(),
                    temporary.as_deref(),
                ))
            })
        }
        pub fn create_private_file(
            &self,
            proof: &SupportProof,
            lock: &InstallerLock,
            name: PrivateName,
            bytes: &[u8],
            deadline: &Deadline,
        ) -> NativeResult<FileIdentity> {
            use std::io::Write;
            self.lock_binding(proof, lock, deadline)?;
            files::check_read_size(bytes.len(), files::MAX_RECORD_BYTES)?;
            let budget = proof.budget(self, deadline)?;
            let context = self.context.clone();
            let lease = lock.0.clone();
            let bytes = bytes.to_vec();
            self.owner.run(Dispatch::Mutation, deadline, move || {
                let change = Change::new();
                change.finish((|| {
                    context.validate(&budget)?;
                    lease.parent.revalidate(&context.security, true, &budget)?;
                    if native::observe(&lease.file, "install.lock", &context.security)?.identity
                        != lease.identity
                    {
                        return Err(NativeError::Foreign);
                    }
                    change.reached();
                    let mut file =
                        lease
                            .parent
                            .create_private(&name, &context.security, &budget)?;
                    file.write_all(&bytes)
                        .map_err(|_| NativeError::OutcomeUnknown)?;
                    budget.check()?;
                    // SAFETY: retained own create-new private file, opened with write access.
                    if unsafe { FlushFileBuffers(file.as_raw_handle()) } == 0 {
                        return Err(native::last_error());
                    }
                    budget.check()?;
                    let facts = native::observe(&file, name.as_str(), &context.security)?;
                    files::admit_component(&facts, Admission::PrivateFile)?;
                    Ok(facts.identity)
                })())
            })
        }
        pub fn native_idle(&self) -> bool {
            self.owner.idle()
        }
    }
    impl Drop for WindowsNativeIo {
        fn drop(&mut self) {
            if !self.owner.idle() {
                use std::io::Write;
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "Windows installer native cleanup unverified: call still in flight; retained owner keeps handles and lease"
                );
            }
        }
    }

    /// No production proof, target, lock or arbitrary-path constructor is exposed here.
    /// The fixture realm is sealed by a genuine Limited token and an own CREATE_NEW root.
    #[cfg(test)]
    #[allow(dead_code)] // Source-included Limited fixtures, never reachable from product code.
    pub(crate) mod scratch {
        use super::*;
        use native::CreatedLeaf;
        struct Scope {
            identity: TokenFacts,
            security: Security,
            root: Arc<Anchor>,
            nonce: [u8; 16],
        }
        impl Scope {
            fn check(&self, deadline: &Deadline) -> NativeResult<()> {
                deadline.check()?;
                if identity::native::current()?.facts() != &self.identity {
                    return Err(NativeError::Foreign);
                }
                self.root.revalidate(&self.security, true, deadline)
            }
        }
        pub(crate) struct FixtureLock {
            scope: Arc<Scope>,
            file: Arc<File>,
            identity: FileIdentity,
        }
        pub(crate) struct ScratchFixture {
            scope: Option<Arc<Scope>>,
            parent: Arc<Anchor>,
            name: PrivateName,
            identity: FileIdentity,
            ledger: Arc<Mutex<Vec<CreatedLeaf>>>,
            owner: Arc<CallOwner>,
            clock: Arc<dyn Clock>,
            cleaned: bool,
        }
        fn receipt(stage: &'static str, boundary: &'static str) {
            use std::io::Write;
            let _ = writeln!(
                std::io::stdout().lock(),
                "scratch admission stage={stage} boundary={boundary}"
            );
        }
        pub(crate) fn scratch_current(
            clock: Arc<dyn Clock>,
            deadline: &Deadline,
        ) -> NativeResult<ScratchFixture> {
            identity::native::refuse_impersonation()?;
            let owner = Arc::new(CallOwner::default());
            let budget = deadline.clone();
            let native_owner = owner.clone();
            let native_clock = clock.clone();
            owner.run(Dispatch::Mutation, deadline, move || {
                budget.check()?;
                receipt("token", "before");
                let identity = identity::native::current()?.facts().clone();
                let security = Security {
                    user: identity.user.clone(),
                    trusted_installer: identity::native::trusted_installer().ok(),
                };
                receipt("token", "after");
                // Shell observations are read-only. No real install/state root is opened.
                receipt("folders", "before");
                let real = paths()?;
                receipt("folders", "after");
                receipt("temp-realm", "before");
                let temp = std::env::temp_dir()
                    .to_str()
                    .ok_or(NativeError::Unsupported)?
                    .trim_end_matches('\\')
                    .to_owned();
                let folded = temp.to_ascii_lowercase();
                for forbidden in [real.install(), &format!("{}\\Crosspane", real.local())] {
                    let value = forbidden.to_ascii_lowercase();
                    if folded == value || folded.starts_with(&format!("{value}\\")) {
                        return Err(NativeError::Foreign);
                    }
                }
                receipt("temp-realm", "after");
                receipt("temp-anchor", "before");
                let parent = Arc::new(
                    Anchor::open(&temp, &security, false, &budget)?
                        .ok_or(NativeError::Unsupported)?,
                );
                receipt("temp-anchor", "after");
                let name =
                    PrivateName::new(&format!("crosspane-w4a1-{}", records::hex(&nonce()?)))?;
                // No existing name is adopted; create is strictly FILE_CREATE. If any later
                // admission/native failure occurs, conservatively report unknown cleanup.
                receipt("root-create", "before");
                let root = match parent.create_child_directory(name.as_str(), &security, &budget) {
                    Ok(root) => root,
                    Err(error) => {
                        use std::io::Write;
                        let _ = writeln!(
                            std::io::stderr().lock(),
                            "scratch creation refused; cleanup not verified after create attempt"
                        );
                        return Err(error);
                    }
                };
                receipt("root-create", "after");
                receipt("root-admission", "before");
                let id = root.identity()?;
                let fixture = ScratchFixture {
                    scope: Some(Arc::new(Scope {
                        identity,
                        security,
                        root: Arc::new(root),
                        nonce: nonce()?,
                    })),
                    parent,
                    name,
                    identity: id,
                    ledger: Arc::new(Mutex::new(Vec::new())),
                    owner: native_owner,
                    clock: native_clock,
                    cleaned: false,
                };
                receipt("root-admission", "after");
                Ok(fixture)
            })
        }
        fn remember(
            ledger: &Mutex<Vec<CreatedLeaf>>,
            name: PrivateName,
            identity: FileIdentity,
        ) -> NativeResult<()> {
            let mut entries = ledger.lock().map_err(|_| NativeError::OutcomeUnknown)?;
            if entries.len() >= 32 {
                return Err(NativeError::OutcomeUnknown);
            }
            entries.push(CreatedLeaf {
                name,
                identity,
                directory: false,
            });
            Ok(())
        }
        impl ScratchFixture {
            fn scope(&self) -> NativeResult<Arc<Scope>> {
                identity::native::refuse_impersonation()?;
                self.scope.clone().ok_or(NativeError::Foreign)
            }
            pub(crate) fn clock(&self) -> Arc<dyn Clock> {
                self.clock.clone()
            }
            pub(crate) fn lock(&self, deadline: &Deadline) -> NativeResult<FixtureLock> {
                let scope = self.scope()?;
                let ledger = self.ledger.clone();
                let budget = deadline.clone();
                self.owner.run(Dispatch::Mutation, deadline, move || {
                    scope.check(&budget)?;
                    let name = PrivateName::new("install.lock")?;
                    let file = match scope.root.open_lock(&scope.security, &budget)? {
                        Some(file) => file,
                        None => {
                            let file = scope.root.create_lock(&scope.security, &budget)?;
                            let facts = native::observe(&file, name.as_str(), &scope.security)?;
                            remember(&ledger, name.clone(), facts.identity)?;
                            file
                        }
                    };
                    let facts = native::observe(&file, name.as_str(), &scope.security)?;
                    let mut overlap = OVERLAPPED::default();
                    budget.check()?;
                    // SAFETY: own created/admitted scratch-only file; one immediate exclusive byte lock.
                    if unsafe {
                        LockFileEx(
                            file.as_raw_handle(),
                            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                            0,
                            1,
                            0,
                            &mut overlap,
                        )
                    } == 0
                    {
                        return Err(native::last_error());
                    }
                    Ok(FixtureLock {
                        scope,
                        file: Arc::new(file),
                        identity: facts.identity,
                    })
                })
            }
            pub(crate) fn write(
                &self,
                lock: &FixtureLock,
                name: PrivateName,
                bytes: &[u8],
                deadline: &Deadline,
            ) -> NativeResult<()> {
                let mut store = self.store(lock, records::RecordName::Receipt, deadline)?;
                files::check_read_size(bytes.len(), files::MAX_RECORD_BYTES)?;
                let bytes = bytes.to_vec();
                self.owner.run(Dispatch::Mutation, deadline, move || {
                    store.check()?;
                    let mut file = store.scope.root.create_private(
                        &name,
                        &store.scope.security,
                        &store.budget,
                    )?;
                    let facts = native::observe(&file, name.as_str(), &store.scope.security)?;
                    remember(&store.ledger, name, facts.identity)?;
                    records::RecordStore::write(&mut store, &mut file, &bytes)?;
                    records::RecordStore::flush(&mut store, &file)
                })
            }
            pub(crate) fn read(
                &self,
                name: PrivateName,
                cap: usize,
                deadline: &Deadline,
            ) -> NativeResult<Option<Vec<u8>>> {
                let scope = self.scope()?;
                let budget = deadline.clone();
                self.owner.run(Dispatch::Observation, deadline, move || {
                    scope.check(&budget)?;
                    scope
                        .root
                        .read_private(&name, &scope.security, cap, &budget)
                        .map(|v| v.map(|(_, bytes)| bytes))
                })
            }
            fn store(
                &self,
                lock: &FixtureLock,
                name: records::RecordName,
                deadline: &Deadline,
            ) -> NativeResult<FixtureStore> {
                let scope = self.scope()?;
                if scope.nonce != lock.scope.nonce {
                    return Err(NativeError::Foreign);
                }
                Ok(FixtureStore {
                    scope,
                    file: lock.file.clone(),
                    lock_identity: lock.identity,
                    ledger: self.ledger.clone(),
                    budget: deadline.clone(),
                    name,
                    stop_before_publish: false,
                })
            }
            pub(crate) fn publish(
                &self,
                lock: &FixtureLock,
                name: records::RecordName,
                bytes: &[u8],
                deadline: &Deadline,
            ) -> NativeResult<records::Publication> {
                let mut store = self.store(lock, name, deadline)?;
                records::validate_for(&store.name, bytes)?;
                let bytes = bytes.to_vec();
                let budget = deadline.clone();
                self.owner.run(Dispatch::Mutation, deadline, move || {
                    records::publish(&mut store, &bytes, &budget)
                })
            }
            pub(crate) fn publish_interrupted(
                &self,
                lock: &FixtureLock,
                name: records::RecordName,
                bytes: &[u8],
                deadline: &Deadline,
            ) -> NativeResult<records::Publication> {
                let mut store = self.store(lock, name, deadline)?;
                store.stop_before_publish = true;
                records::validate_for(&store.name, bytes)?;
                let bytes = bytes.to_vec();
                let budget = deadline.clone();
                self.owner.run(Dispatch::Mutation, deadline, move || {
                    records::publish(&mut store, &bytes, &budget)
                })
            }
            pub(crate) fn foreign_acl_refusal(
                &self,
                name: PrivateName,
                deadline: &Deadline,
            ) -> NativeResult<bool> {
                let scope = self.scope()?;
                let expected = self
                    .ledger
                    .lock()
                    .map_err(|_| NativeError::Unavailable)?
                    .iter()
                    .find(|entry| entry.name == name && !entry.directory)
                    .map(|entry| entry.identity)
                    .ok_or(NativeError::Foreign)?;
                let budget = deadline.clone();
                self.owner.run(Dispatch::Mutation, deadline, move || {
                    scope.check(&budget)?;
                    native::fixture_foreign_acl(
                        &scope.root,
                        &name,
                        expected,
                        false,
                        &scope.security,
                        &budget,
                    )?;
                    Ok(matches!(
                        scope.root.read_private(&name, &scope.security, 32, &budget),
                        Err(NativeError::Foreign)
                    ))
                })
            }
            pub(crate) fn ancestor_refusal(&self, deadline: &Deadline) -> NativeResult<bool> {
                let scope = self.scope()?;
                let ledger = self.ledger.clone();
                let budget = deadline.clone();
                self.owner.run(Dispatch::Mutation, deadline, move || {
                    scope.check(&budget)?;
                    let name = PrivateName::new("ancestor-fixture")?;
                    let child = scope.root.create_child_directory(
                        name.as_str(),
                        &scope.security,
                        &budget,
                    )?;
                    let identity = child.identity()?;
                    ledger
                        .lock()
                        .map_err(|_| NativeError::OutcomeUnknown)?
                        .push(CreatedLeaf {
                            name: name.clone(),
                            identity,
                            directory: true,
                        });
                    native::fixture_foreign_acl(
                        &scope.root,
                        &name,
                        identity,
                        true,
                        &scope.security,
                        &budget,
                    )?;
                    Ok(matches!(
                        Anchor::open(child.fixture_path(), &scope.security, false, &budget),
                        Err(NativeError::Foreign)
                    ))
                })
            }
            pub(crate) fn reparse_refusal(&self, deadline: &Deadline) -> NativeResult<bool> {
                let scope = self.scope()?;
                let ledger = self.ledger.clone();
                let budget = deadline.clone();
                self.owner.run(Dispatch::Mutation, deadline, move || {
                    scope.check(&budget)?;
                    let name = PrivateName::new("reparse-fixture")?;
                    let child = scope.root.create_child_directory(
                        name.as_str(),
                        &scope.security,
                        &budget,
                    )?;
                    let created = CreatedLeaf {
                        name,
                        identity: child.identity()?,
                        directory: true,
                    };
                    ledger
                        .lock()
                        .map_err(|_| NativeError::OutcomeUnknown)?
                        .push(created.clone());
                    native::fixture_reparse_refusal(&scope.root, &created, &scope.security, &budget)
                })
            }
            pub(crate) fn case_alias_refusal(
                &self,
                name: PrivateName,
                deadline: &Deadline,
            ) -> NativeResult<bool> {
                let scope = self.scope()?;
                let budget = deadline.clone();
                let expected = self
                    .ledger
                    .lock()
                    .map_err(|_| NativeError::Unavailable)?
                    .iter()
                    .find(|entry| entry.name == name && !entry.directory)
                    .map(|entry| entry.identity)
                    .ok_or(NativeError::Foreign)?;
                self.owner.run(Dispatch::Observation, deadline, move || {
                    scope.check(&budget)?;
                    native::fixture_case_alias(
                        &scope.root,
                        &name,
                        expected,
                        &scope.security,
                        &budget,
                    )
                })
            }
            pub(crate) fn cleanup(mut self, deadline: &Deadline) -> NativeResult<()> {
                identity::native::refuse_impersonation()?;
                if !self.owner.idle() {
                    return Err(NativeError::OutcomeUnknown);
                }
                let scope = self.scope.take().ok_or(NativeError::Foreign)?;
                let scope = Arc::try_unwrap(scope).map_err(|_| NativeError::Busy)?;
                let ledger = self
                    .ledger
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?
                    .clone();
                let Scope { root, security, .. } = scope;
                if Arc::strong_count(&root) != 1 {
                    return Err(NativeError::Busy);
                }
                drop(root); // Release every no-delete pin BEFORE reopening creator leaves with DELETE.
                let parent = self.parent.clone();
                let name = self.name.clone();
                let id = self.identity;
                let budget = deadline.clone();
                let result = self.owner.run(Dispatch::Mutation, deadline, move || {
                    native::cleanup_fixture(&parent, &name, id, &ledger, &security, &budget)
                });
                self.cleaned = result.is_ok();
                result
            }
        }
        impl Drop for ScratchFixture {
            fn drop(&mut self) {
                if !self.cleaned {
                    use std::io::Write;
                    let _ = writeln!(
                        std::io::stderr().lock(),
                        "scratch cleanup unverified; no recursive deletion or adoption of unknown leaves"
                    );
                }
            }
        }
        struct FixtureStore {
            scope: Arc<Scope>,
            file: Arc<File>,
            lock_identity: FileIdentity,
            ledger: Arc<Mutex<Vec<CreatedLeaf>>>,
            budget: Deadline,
            name: records::RecordName,
            stop_before_publish: bool,
        }
        impl FixtureStore {
            fn check(&self) -> NativeResult<()> {
                self.scope.check(&self.budget)?;
                if native::observe(&self.file, "install.lock", &self.scope.security)?.identity
                    != self.lock_identity
                {
                    return Err(NativeError::Foreign);
                }
                Ok(())
            }
            fn read(&self, name: &PrivateName) -> NativeResult<Option<Vec<u8>>> {
                self.check()?;
                self.scope
                    .root
                    .read_private(
                        name,
                        &self.scope.security,
                        files::MAX_RECORD_BYTES,
                        &self.budget,
                    )
                    .map(|v| v.map(|(_, bytes)| bytes))
            }
        }
        impl records::RecordStore for FixtureStore {
            type Temporary = File;
            fn target(&self) -> records::RecordName {
                self.name.clone()
            }
            fn context(&self) -> Option<[u8; 16]> {
                Some(self.scope.nonce)
            }
            fn read_final(&mut self) -> NativeResult<Option<Vec<u8>>> {
                self.read(&self.name.file_name()?)
            }
            fn create(&mut self) -> NativeResult<(PrivateName, File)> {
                self.check()?;
                let name =
                    PrivateName::new(&format!("record-temp-{}.json", records::hex(&nonce()?)))?;
                let file =
                    self.scope
                        .root
                        .create_private(&name, &self.scope.security, &self.budget)?;
                let facts = native::observe(&file, name.as_str(), &self.scope.security)?;
                remember(&self.ledger, name.clone(), facts.identity)?;
                Ok((name, file))
            }
            fn write(&mut self, file: &mut File, bytes: &[u8]) -> NativeResult<()> {
                use std::io::Write;
                self.check()?;
                file.write_all(bytes)
                    .map_err(|_| NativeError::OutcomeUnknown)
            }
            fn flush(&mut self, file: &File) -> NativeResult<()> {
                self.check()?;
                // SAFETY: exact own scratch file created for this fixture, with write access.
                if unsafe { FlushFileBuffers(file.as_raw_handle()) } == 0 {
                    return Err(native::last_error());
                }
                self.budget.check()
            }
            fn publish(&mut self, file: &File) -> NativeResult<()> {
                self.check()?;
                if self.stop_before_publish {
                    return Err(NativeError::Unavailable);
                }
                let identity = native::observe(file, "", &self.scope.security)?.identity;
                self.scope.root.publish_private(
                    file,
                    &self.name.file_name()?,
                    &self.scope.security,
                    &self.budget,
                )?;
                let mut ledger = self
                    .ledger
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?;
                let name = self.name.file_name()?;
                ledger.retain(|entry| entry.identity != identity && entry.name != name);
                ledger.push(CreatedLeaf {
                    name,
                    identity,
                    directory: false,
                });
                Ok(())
            }
            fn read_temporary(&mut self, name: &PrivateName) -> NativeResult<Option<Vec<u8>>> {
                self.read(name)
            }
        }
    }
}
