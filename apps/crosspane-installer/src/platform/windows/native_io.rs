//! Windows installer native foundation. Observations and record bytes are never authority.
//! The native adapter is Windows-only; bounded admission decisions remain portable for tests.

#[path = "native_io/activation.rs"]
// Only new native graph: production entries are intentionally absent in unit-test roots.
#[cfg_attr(test, allow(dead_code, unused_imports))]
pub(crate) mod activation;
#[path = "native_io/epoch_archive.rs"]
// Only new native graph: production entries are intentionally absent in unit-test roots.
#[cfg_attr(test, allow(dead_code, unused_imports))]
pub(crate) mod epoch_archive;
#[path = "native_io/files.rs"]
pub mod files;
#[path = "native_io/identity.rs"]
pub mod identity;
#[path = "native_io/jobs.rs"]
// Only new native graph: production entries are intentionally absent in unit-test roots.
#[cfg_attr(test, allow(dead_code, unused_imports))]
pub(crate) mod jobs;
#[path = "native_io/process.rs"]
pub mod process;
#[path = "native_io/records.rs"]
pub mod records;
#[path = "native_io/supervisor_owner.rs"]
// Only new native graph: production entries are intentionally absent in unit-test roots.
#[cfg_attr(test, allow(dead_code, unused_imports))]
pub(crate) mod supervisor_owner;
#[path = "native_io/task.rs"]
pub(crate) mod task;

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
/// Select only the state's fixed exit receipt. The callback receives one exact parent/leaf;
/// it cannot fall back to runtime or search another directory after a missing receipt.
pub(crate) fn with_state_exit_receipt<T>(
    local_root: &str,
    read: impl FnOnce(&str, &str) -> NativeResult<T>,
) -> NativeResult<T> {
    let state = format!("{local_root}\\Crosspane");
    read(&state, "last_exit.json")
}
pub use process::{Cancellation, Clock, Deadline, MonotonicClock};

#[cfg(windows)]
pub(crate) use adapter::AgentObservation;
#[cfg(windows)]
#[allow(unused_imports)]
// A4 consumes completion fields; terminal ACK presently only rechecks origin.
pub(crate) use adapter::ExitObservation;
#[cfg(all(windows, not(test)))]
pub(crate) use adapter::SelectedOuterOperation;
#[cfg(all(windows, test))]
#[allow(unused_imports)]
// Source-included probe exports; library unit tests do not invoke them.
pub(crate) use adapter::scratch::{FixtureLock, ScratchFixture, scratch_current};
#[cfg(windows)]
pub(crate) use adapter::{BrokerAdmission, JobAdmission, PeerRolePin, StopLockLease};
#[cfg(windows)]
#[allow(unused_imports)]
// A4b/A5 consumers retain these sealed native return capabilities.
pub(crate) use adapter::{ImageReleased, OpaqueLeaf, PrunedGeneration};
#[cfg(windows)]
pub use adapter::{InstallerLock, SupportProof, WindowsNativeIo, WindowsTarget};
#[cfg(windows)]
pub(crate) use adapter::{OpenedPe, PayloadRoot, PruneOutcome, SelfImagePin, StagedPe};

#[cfg(all(windows, not(test)))]
pub(crate) use adapter::{
    LogonArchiveResult, LogonReservation, OwnedArchiveResult, PriorLogonDisposition,
};

#[cfg(all(windows, not(test)))]
pub(crate) use adapter::{OuterCompletionAdmission, OuterPeerImage};

#[cfg(all(windows, not(test)))]
pub(crate) use adapter::{FileRecoveryKeeperAbsent, FileRecoveryRoot, FileRecoverySeal};

#[cfg(all(windows, not(test)))]
pub(crate) use adapter::{
    RemovalCompletionAdmission, RemovalKeeperSelection, RemovalMutationPermit, RemovalPeerImage,
    RemovalRoot,
};

#[cfg(all(windows, not(test)))]
pub(crate) use adapter::keeper;

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
    use super::super::payload::{
        inventory::{ApprovedPe, PayloadRole, PeFacts},
        recovery::{MutationPermit, Phase},
    };
    /// The only observed bytes allowed to create an Installer approval pin: our own module.
    pub(crate) struct SelfImagePin(Arc<OwnImage>);
    struct OwnImage {
        target: [u8; 16],
        parent: Arc<Anchor>,
        leaf: String,
        image: native::ImageData,
    }
    impl SelfImagePin {
        pub(crate) fn facts(&self) -> &PeFacts {
            &self.0.image.facts
        }
        pub(crate) fn identity(&self) -> FileIdentity {
            self.0.image.identity
        }
        pub(crate) fn canonical_dos_path(&self) -> &str {
            &self.0.image.canonical
        }
        pub(crate) fn reverify(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            let budget = proof.budget(io, deadline)?;
            if self.0.target != io.context.target.nonce {
                return Err(NativeError::Foreign);
            }
            let image = self.0.clone();
            let context = io.context.clone();
            io.owner.run(Dispatch::Observation, deadline, move || {
                context.validate(&budget)?;
                let fresh = image.parent.open_image(
                    &image.leaf,
                    false,
                    env!("CARGO_PKG_VERSION"),
                    &context.security,
                    &budget,
                )?;
                if fresh.identity != image.image.identity || fresh.facts != image.image.facts {
                    return Err(NativeError::Foreign);
                }
                Ok(())
            })
        }
    }
    /// A current original-IO own-process/image selection, never reconstructed from journal facts.
    #[cfg(not(test))]
    pub(crate) struct SelectedOuterOperation {
        io: Arc<WindowsNativeIo>,
        module: SelfImagePin,
        process: super::process::own::OwnProcessIdentity,
        record: super::super::payload::recovery::OuterUpgradeRecord,
    }
    #[cfg(not(test))]
    impl SelectedOuterOperation {
        pub(crate) fn io(&self) -> &Arc<WindowsNativeIo> {
            &self.io
        }
        pub(crate) fn module(&self) -> &SelfImagePin {
            &self.module
        }
        pub(crate) fn owner_identity(&self) -> &super::process::own::OwnProcessIdentity {
            &self.process
        }
        pub(crate) fn record(&self) -> &super::super::payload::recovery::OuterUpgradeRecord {
            &self.record
        }
        pub(crate) fn operation(&self) -> [u8; 16] {
            self.record.operation()
        }
        pub(crate) fn reverify(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            lock: &InstallerLock,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if !std::ptr::eq(io, self.io.as_ref()) {
                return Err(NativeError::Foreign);
            }
            io.verify_stop_lock(proof, lock, deadline)?;
            self.process.reverify(deadline)?;
            self.module.reverify(io, proof, deadline)?;
            let current = io.observe_outer_operation(proof, lock, deadline)?;
            current.same_selection(&self.record)?;
            current.context().matches(&io.context.target.identity)?;
            deadline.check()
        }
        fn change(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            lock: &InstallerLock,
            deadline: &Deadline,
            edit: impl FnOnce(
                &mut super::super::payload::recovery::OuterUpgradeRecord,
            ) -> NativeResult<()>,
        ) -> NativeResult<()> {
            self.reverify(io, proof, lock, deadline)?;
            let mut record = io.observe_outer_operation(proof, lock, deadline)?;
            edit(&mut record)?;
            record.same_selection(&self.record)?;
            io.publish_outer_operation(proof, lock, &record, deadline)
        }
        pub(crate) fn mark_preparing(
            &self,
            proof: &SupportProof,
            lock: &InstallerLock,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.record.outer().matches(
                self.process.pid(),
                self.process.creation(),
                outer_stamp(self.module.identity()),
                self.module.facts(),
            )?;
            self.change(&self.io, proof, lock, deadline, |r| {
                r.advance(super::super::payload::recovery::OuterPhase::Preparing)
            })
        }
        pub(crate) fn record_keeper_image(
            &self,
            image: &OpenedPe,
            proof: &SupportProof,
            lock: &InstallerLock,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            image.reverify(&self.io, proof, deadline)?;
            if image.approved().facts() != self.module.facts() {
                return Err(NativeError::Foreign);
            }
            // The caller's new fixed-copy adapter supplied this actual opened object. It is only
            // recorded here; native keeper admission separately reopens the exact rooted leaf.
            self.change(&self.io, proof, lock, deadline, |r| {
                r.set_keeper_image(outer_stamp(image.identity()))
            })
        }
        pub(crate) fn mark_launch_intent(
            &self,
            proof: &SupportProof,
            lock: &InstallerLock,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.record.outer().matches(
                self.process.pid(),
                self.process.creation(),
                outer_stamp(self.module.identity()),
                self.module.facts(),
            )?;
            self.change(&self.io, proof, lock, deadline, |r| {
                r.advance_launch(super::super::payload::recovery::OuterLaunchPhase::CreateIntent)
            })
        }
        pub(crate) fn record_created_keeper(
            &mut self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            lock: &InstallerLock,
            parent_handle: u64,
            keeper: super::super::payload::recovery::OuterProcessCorrelation,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.change(io, proof, lock, deadline, |r| {
                r.record_created_keeper(parent_handle, keeper)
            })?;
            self.record = io.observe_outer_operation(proof, lock, deadline)?;
            Ok(())
        }
        pub(crate) fn mark_resume_intent(
            &self,
            proof: &SupportProof,
            lock: &InstallerLock,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.change(&self.io, proof, lock, deadline, |r| {
                r.advance_launch(super::super::payload::recovery::OuterLaunchPhase::ResumeIntent)
            })
        }
        pub(crate) fn mark_ready(
            &self,
            proof: &SupportProof,
            lock: &InstallerLock,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.change(&self.io, proof, lock, deadline, |r| {
                r.peer_matches(
                    self.process.pid(),
                    self.process.creation(),
                    outer_stamp(self.module.identity()),
                    self.module.facts(),
                    &self.io.context.target.identity,
                )?;
                if r.launch_stage()
                    == super::super::payload::recovery::OuterLaunchPhase::ResumeIntent
                {
                    r.advance_launch(super::super::payload::recovery::OuterLaunchPhase::Resumed)?;
                }
                r.advance(super::super::payload::recovery::OuterPhase::Ready)
            })
        }
        pub(crate) fn mark_committed(
            &self,
            proof: &SupportProof,
            lock: &InstallerLock,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.change(&self.io, proof, lock, deadline, |r| {
                r.advance(super::super::payload::recovery::OuterPhase::Committed)
            })
        }
        pub(crate) fn mark_complete(
            &self,
            proof: &SupportProof,
            lock: &InstallerLock,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.change(&self.io, proof, lock, deadline, |r| {
                r.advance(super::super::payload::recovery::OuterPhase::Complete)
            })
        }
        pub(crate) fn mark_cancelled(
            &self,
            proof: &SupportProof,
            lock: &InstallerLock,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.change(&self.io, proof, lock, deadline, |r| {
                r.advance(super::super::payload::recovery::OuterPhase::Cancelled)
            })
        }
    }
    #[cfg(not(test))]
    fn outer_stamp(identity: FileIdentity) -> super::super::payload::recovery::FileStamp {
        super::super::payload::recovery::FileStamp {
            volume: identity.volume,
            file: identity.file,
        }
    }

    pub(crate) struct OpenedPe(Arc<ApprovedImage>);
    struct ApprovedImage {
        target: [u8; 16],
        parent: Arc<Anchor>,
        leaf: String,
        image: native::ImageData,
        expected: ApprovedPe,
    }
    impl OpenedPe {
        pub(crate) fn identity(&self) -> FileIdentity {
            self.0.image.identity
        }
        pub(crate) fn canonical_dos_path(&self) -> &str {
            &self.0.image.canonical
        }
        pub(crate) fn approved(&self) -> &ApprovedPe {
            &self.0.expected
        }
        pub(crate) fn reverify(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            let budget = proof.budget(io, deadline)?;
            if self.0.target != io.context.target.nonce {
                return Err(NativeError::Foreign);
            }
            let image = self.0.clone();
            let context = io.context.clone();
            io.owner.run(Dispatch::Observation, deadline, move || {
                context.validate(&budget)?;
                let fresh = image.parent.open_image(
                    &image.leaf,
                    true,
                    image.expected.version(),
                    &context.security,
                    &budget,
                )?;
                if fresh.identity != image.image.identity || fresh.facts != *image.expected.facts()
                {
                    return Err(NativeError::Foreign);
                }
                Ok(())
            })
        }
    }
    pub(crate) struct PayloadRoot(Arc<PayloadRootData>);
    struct PayloadRootData {
        target: [u8; 16],
        install: Mutex<Option<Arc<Anchor>>>,
        // A4b native launch consumers use the fixed retained install working directory.
        #[allow(dead_code)]
        canonical: String,
    }
    pub(crate) struct OpaqueLeaf {
        target: [u8; 16],
        role: PayloadRole,
        observed: native::OpaqueData,
    }
    impl OpaqueLeaf {
        pub(crate) fn identity(&self) -> FileIdentity {
            self.observed.identity
        }
    }
    pub(crate) struct StagedPe {
        target: [u8; 16],
        operation: [u8; 16],
        role: PayloadRole,
        parent: Arc<Anchor>,
        image: native::ImageData,
        expected: ApprovedPe,
    }
    impl StagedPe {
        pub(crate) fn role(&self) -> PayloadRole {
            self.role
        }
        pub(crate) fn observation(&self) -> super::super::payload::recovery::ImageObservation {
            super::super::payload::recovery::ImageObservation {
                identity: self.image.identity.into(),
                facts: self.image.facts.clone(),
            }
        }
    }
    pub(crate) struct PrunedGeneration {
        target: [u8; 16],
        operation: [u8; 16],
        generation: [u8; 16],
    }
    pub(crate) enum PruneOutcome {
        Removed(PrunedGeneration),
        Retained,
    }
    /// Positive exclusive fixed-leaf admission ONLY. It does not prove old supervisor/tree completion.
    pub(crate) struct ImageReleased {
        target: [u8; 16],
        operation: [u8; 16],
    }
    impl ImageReleased {
        // A4b composes this image fence with genuine old owner/tree completion.
        #[allow(dead_code)]
        pub(crate) fn operation(&self) -> [u8; 16] {
            self.operation
        }
        pub(crate) fn binding(
            &self,
            io: &WindowsNativeIo,
            operation: [u8; 16],
        ) -> NativeResult<()> {
            if self.target != io.context.target.nonce || self.operation != operation {
                return Err(NativeError::Foreign);
            }
            Ok(())
        }
    }
    impl PayloadRoot {
        // A4b native launch consumers use this fixed retained working directory.
        #[allow(dead_code)]
        pub(crate) fn canonical_dos_path(&self) -> &str {
            &self.0.canonical
        }
        fn check(&self, context: &Context, budget: &Deadline) -> NativeResult<Option<Arc<Anchor>>> {
            if self.0.target != context.target.nonce {
                return Err(NativeError::Foreign);
            }
            context.validate(budget)?;
            let slot = self
                .0
                .install
                .lock()
                .map_err(|_| NativeError::Unavailable)?;
            if let Some(root) = &*slot {
                root.revalidate(&context.security, true, budget)?;
            }
            Ok(slot.clone())
        }
        fn ensure(
            &self,
            context: &Context,
            budget: &Deadline,
            change: &Change,
        ) -> NativeResult<Arc<Anchor>> {
            if let Some(root) = self.check(context, budget)? {
                return Ok(root);
            }
            let programs = match Anchor::open(
                context.target.paths.programs(),
                &context.security,
                false,
                budget,
            )? {
                Some(root) => root,
                None => {
                    change.reached();
                    context
                        .local
                        .create_child_directory("Programs", &context.security, budget)?
                }
            };
            let root = match programs.child("Crosspane", &context.security, budget)? {
                Some(root) => root,
                None => {
                    change.reached();
                    programs.create_child_directory("Crosspane", &context.security, budget)?
                }
            };
            let root = Arc::new(root);
            *self
                .0
                .install
                .lock()
                .map_err(|_| NativeError::Unavailable)? = Some(root.clone());
            Ok(root)
        }
        pub(crate) fn open_approved(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            role: PayloadRole,
            expected: &ApprovedPe,
            deadline: &Deadline,
        ) -> NativeResult<OpenedPe> {
            if expected.role() != role {
                return Err(NativeError::Foreign);
            }
            let budget = proof.budget(io, deadline)?;
            let context = io.context.clone();
            let root = self.0.clone();
            let expected = expected.clone();
            io.owner.run(Dispatch::Observation, deadline, move || {
                let root = PayloadRoot(root)
                    .check(&context, &budget)?
                    .ok_or(NativeError::Missing)?;
                let image = root.open_image(
                    role.leaf(),
                    true,
                    expected.version(),
                    &context.security,
                    &budget,
                )?;
                if image.facts != *expected.facts() {
                    return Err(NativeError::Unsupported);
                }
                Ok(OpenedPe(Arc::new(ApprovedImage {
                    target: context.target.nonce,
                    parent: root,
                    leaf: role.leaf().into(),
                    image,
                    expected,
                })))
            })
        }
        pub(crate) fn open_staged(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            operation: [u8; 16],
            role: PayloadRole,
            expected: &ApprovedPe,
            deadline: &Deadline,
        ) -> NativeResult<Option<StagedPe>> {
            if operation == [0; 16] || expected.role() != role {
                return Err(NativeError::Invalid);
            }
            let budget = proof.budget(io, deadline)?;
            let context = io.context.clone();
            let root = self.0.clone();
            let expected = expected.clone();
            io.owner.run(Dispatch::Observation, deadline, move || {
                let Some(root) = PayloadRoot(root).check(&context, &budget)? else {
                    return Ok(None);
                };
                let Some(stage) = root.child("payload-stage", &context.security, &budget)? else {
                    return Ok(None);
                };
                let Some(parent) =
                    stage.child(&records::hex(&operation), &context.security, &budget)?
                else {
                    return Ok(None);
                };
                let parent = Arc::new(parent);
                let image = match parent.open_staged_image(
                    role.leaf(),
                    expected.version(),
                    &context.security,
                    &budget,
                ) {
                    Ok(image) => image,
                    Err(NativeError::Missing) => return Ok(None),
                    Err(error) => return Err(error),
                };
                if image.facts != *expected.facts() {
                    return Err(NativeError::Unsupported);
                }
                Ok(Some(StagedPe {
                    target: context.target.nonce,
                    operation,
                    role,
                    parent,
                    image,
                    expected,
                }))
            })
        }
        /// A complete stage observed after an interrupted write is flushed under a fresh intent
        /// before adoption; observation alone cannot claim durable staging.
        pub(crate) fn settle_stage(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            lock: &InstallerLock,
            permit: &MutationPermit,
            staged: &StagedPe,
            deadline: &Deadline,
        ) -> NativeResult<super::super::payload::recovery::ImageObservation> {
            io.lock_binding(proof, lock, deadline)?;
            check_permit(io, permit, Phase::StageIntent, Some(staged.role))?;
            if staged.target != io.context.target.nonce || staged.operation != permit.operation() {
                return Err(NativeError::Foreign);
            }
            let context = io.context.clone();
            let lease = lock.0.clone();
            let budget = proof.budget(io, deadline)?;
            let file = staged.image.file.clone();
            let parent = staged.parent.clone();
            let expected = staged.expected.clone();
            let identity = staged.image.identity;
            let operation = permit.operation();
            let bytes = permit.bytes().to_vec();
            io.owner.run(Dispatch::Mutation, deadline, move || {
                let change = Change::new();
                change.finish((|| {
                    validate_payload_lock(&context, &lease, &budget)?;
                    validate_intent(&context, &lease, operation, &bytes, &budget)?;
                    parent.revalidate(&context.security, true, &budget)?;
                    let image = native::measure_image(file.clone(), expected.version(), &budget)?;
                    if image.identity != identity || image.facts != *expected.facts() {
                        return Err(NativeError::Foreign);
                    }
                    change.reached();
                    // SAFETY: only a freshly hash/PE-admitted own writable stage, under its durable intent.
                    if unsafe { FlushFileBuffers(file.as_raw_handle()) } == 0 {
                        return Err(native::last_error());
                    }
                    budget.check()?;
                    Ok(super::super::payload::recovery::ImageObservation {
                        identity: identity.into(),
                        facts: expected.facts().clone(),
                    })
                })())
            })
        }
        pub(crate) fn observe_backup(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            operation: [u8; 16],
            role: PayloadRole,
            deadline: &Deadline,
        ) -> NativeResult<Option<FileIdentity>> {
            if operation == [0; 16] {
                return Err(NativeError::Invalid);
            }
            let budget = proof.budget(io, deadline)?;
            let context = io.context.clone();
            let root = self.0.clone();
            io.owner.run(Dispatch::Observation, deadline, move || {
                let Some(root) = PayloadRoot(root).check(&context, &budget)? else {
                    return Ok(None);
                };
                let Some(backups) = root.child("payload-backups", &context.security, &budget)?
                else {
                    return Ok(None);
                };
                let Some(generation) =
                    backups.child(&records::hex(&operation), &context.security, &budget)?
                else {
                    return Ok(None);
                };
                Ok(generation
                    .opaque(role.leaf(), false, &context.security, &budget)?
                    .map(|leaf| leaf.identity))
            })
        }
        /// Roll back only a pre-stop staging generation with exactly the approved fixed content.
        /// Unknown/partial/loaded material is retained; no installed or backup image is changed.
        // Separate sealed io/proof/lock/intent/pin/deadline arguments preserve the admitted authority boundaries.
        #[allow(clippy::too_many_arguments)]
        pub(crate) fn rollback_stage(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            lock: &InstallerLock,
            permit: &MutationPermit,
            inventory: &super::super::payload::inventory::ApprovedInventory,
            installer: &ApprovedPe,
            deadline: &Deadline,
        ) -> NativeResult<super::super::payload::recovery::RollbackOutcome> {
            io.lock_binding(proof, lock, deadline)?;
            check_permit(io, permit, Phase::RollbackIntent, None)?;
            let expected = PayloadRole::ALL
                .into_iter()
                .map(|role| {
                    if role == PayloadRole::Installer {
                        Ok(installer.clone())
                    } else {
                        inventory.role(role).cloned()
                    }
                })
                .collect::<NativeResult<Vec<_>>>()?;
            let budget = proof.budget(io, deadline)?;
            let context = io.context.clone();
            let lease = lock.0.clone();
            let root = self.0.clone();
            let operation = permit.operation();
            let bytes = permit.bytes().to_vec();
            io.owner.run(Dispatch::Mutation, deadline, move || {
                use super::super::payload::recovery::RollbackOutcome;
                let change = Change::new();
                change.finish((|| {
                    validate_payload_lock(&context, &lease, &budget)?;
                    validate_intent(&context, &lease, operation, &bytes, &budget)?;
                    let Some(root) = PayloadRoot(root).check(&context, &budget)? else {
                        return Ok(RollbackOutcome::RolledBack);
                    };
                    let Some(stage) = root.child("payload-stage", &context.security, &budget)?
                    else {
                        return Ok(RollbackOutcome::RolledBack);
                    };
                    let Some(generation) =
                        stage.child(&records::hex(&operation), &context.security, &budget)?
                    else {
                        return Ok(RollbackOutcome::RolledBack);
                    };
                    for leaf in generation.entry_names(&context.security, &budget)? {
                        let pin = if leaf == "helper-copy.exe" {
                            expected
                                .iter()
                                .find(|pin| pin.role() == PayloadRole::Installer)
                        } else {
                            expected.iter().find(|pin| pin.role().leaf() == leaf)
                        };
                        let Some(pin) = pin else {
                            return Ok(RollbackOutcome::Retained);
                        };
                        let image = match generation.open_image(
                            &leaf,
                            true,
                            pin.version(),
                            &context.security,
                            &budget,
                        ) {
                            Ok(image) => image,
                            Err(
                                NativeError::Unsupported
                                | NativeError::Unavailable
                                | NativeError::Foreign
                                | NativeError::Busy,
                            ) => return Ok(RollbackOutcome::Retained),
                            Err(error) => return Err(error),
                        };
                        if image.facts != *pin.facts() {
                            return Ok(RollbackOutcome::Retained);
                        }
                        drop(image);
                    }
                    drop(generation);
                    change.reached();
                    match stage.prune_tree(&records::hex(&operation), &context.security, &budget) {
                        Ok(()) => Ok(RollbackOutcome::RolledBack),
                        Err(NativeError::Busy | NativeError::Foreign) => {
                            Ok(RollbackOutcome::Retained)
                        }
                        Err(error) => Err(error),
                    }
                })())
            })
        }
        pub(crate) fn observe_opaque(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            role: PayloadRole,
            deadline: &Deadline,
        ) -> NativeResult<Option<OpaqueLeaf>> {
            let budget = proof.budget(io, deadline)?;
            let context = io.context.clone();
            let root = self.0.clone();
            io.owner.run(Dispatch::Observation, deadline, move || {
                let Some(root) = PayloadRoot(root).check(&context, &budget)? else {
                    return Ok(None);
                };
                root.opaque(role.leaf(), false, &context.security, &budget)?
                    .map(|observed| {
                        Ok(OpaqueLeaf {
                            target: context.target.nonce,
                            role,
                            observed,
                        })
                    })
                    .transpose()
            })
        }
        pub(crate) fn prove_released(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            lock: &InstallerLock,
            operation: [u8; 16],
            deadline: &Deadline,
        ) -> NativeResult<ImageReleased> {
            io.lock_binding(proof, lock, deadline)?;
            if operation == [0; 16] {
                return Err(NativeError::Invalid);
            }
            let budget = proof.budget(io, deadline)?;
            let context = io.context.clone();
            let lease = lock.0.clone();
            let root = self.0.clone();
            io.owner.run(Dispatch::Observation, deadline, move || {
                validate_payload_lock(&context, &lease, &budget)?;
                if let Some(root) = PayloadRoot(root).check(&context, &budget)? {
                    for role in PayloadRole::ALL {
                        drop(root.opaque(role.leaf(), true, &context.security, &budget)?);
                    }
                }
                Ok(ImageReleased {
                    target: context.target.nonce,
                    operation,
                })
            })
        }
        // Separate sealed io/proof/lock/intent/pin/deadline arguments preserve the admitted authority boundaries.
        #[allow(clippy::too_many_arguments)]
        pub(crate) fn stage(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            lock: &InstallerLock,
            permit: &MutationPermit,
            role: PayloadRole,
            input: Box<dyn std::io::Read + Send>,
            expected: &ApprovedPe,
            deadline: &Deadline,
        ) -> NativeResult<StagedPe> {
            self.stage_leaf(
                io,
                proof,
                lock,
                permit,
                role,
                role.leaf(),
                input,
                expected,
                deadline,
            )
        }
        // Separate sealed io/proof/lock/intent/pin/deadline arguments preserve the admitted authority boundaries.
        #[allow(clippy::too_many_arguments)]
        fn stage_leaf(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            lock: &InstallerLock,
            permit: &MutationPermit,
            role: PayloadRole,
            leaf: &'static str,
            input: Box<dyn std::io::Read + Send>,
            expected: &ApprovedPe,
            deadline: &Deadline,
        ) -> NativeResult<StagedPe> {
            io.lock_binding(proof, lock, deadline)?;
            check_permit(io, permit, Phase::StageIntent, Some(role))?;
            if expected.role() != role {
                return Err(NativeError::Foreign);
            }
            let root = self.0.clone();
            let context = io.context.clone();
            let lease = lock.0.clone();
            let budget = proof.budget(io, deadline)?;
            let expected = expected.clone();
            let operation = permit.operation();
            let bytes = permit.bytes().to_vec();
            io.owner.run(Dispatch::Mutation, deadline, move || {
                let change = Change::new();
                change.finish((|| {
                    validate_payload_lock(&context, &lease, &budget)?;
                    validate_intent(&context, &lease, operation, &bytes, &budget)?;
                    let root = PayloadRoot(root).ensure(&context, &budget, &change)?;
                    let parent =
                        ensure_payload_child(&root, "payload-stage", &context, &budget, &change)?;
                    let parent = Arc::new(ensure_payload_child(
                        &parent,
                        &records::hex(&operation),
                        &context,
                        &budget,
                        &change,
                    )?);
                    change.reached();
                    let image =
                        parent.stage_image(leaf, input, &expected, &context.security, &budget)?;
                    Ok(StagedPe {
                        target: context.target.nonce,
                        operation,
                        role,
                        parent,
                        image,
                        expected,
                    })
                })())
            })
        }
        // A4b/A5 calls helper staging only after genuine old-owner completion.
        #[allow(dead_code)]
        pub(crate) fn stage_helper(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            lock: &InstallerLock,
            permit: &MutationPermit,
            source: &SelfImagePin,
            deadline: &Deadline,
        ) -> NativeResult<OpenedPe> {
            source.reverify(io, proof, deadline)?;
            let expected = ApprovedPe::own_image(source)?;
            let input = io.self_image_reader(source, proof, deadline)?;
            let staged = self.stage_leaf(
                io,
                proof,
                lock,
                permit,
                PayloadRole::Installer,
                "helper-copy.exe",
                input,
                &expected,
                deadline,
            )?;
            drop(staged);
            self.open_helper(io, proof, permit.operation(), &expected, deadline)
        }
        pub(crate) fn open_helper(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            operation: [u8; 16],
            expected: &ApprovedPe,
            deadline: &Deadline,
        ) -> NativeResult<OpenedPe> {
            if operation == [0; 16] || expected.role() != PayloadRole::Installer {
                return Err(NativeError::Invalid);
            }
            let budget = proof.budget(io, deadline)?;
            let context = io.context.clone();
            let root = self.0.clone();
            let expected = expected.clone();
            io.owner.run(Dispatch::Observation, deadline, move || {
                let root = PayloadRoot(root)
                    .check(&context, &budget)?
                    .ok_or(NativeError::Missing)?;
                let parent = root
                    .child("payload-stage", &context.security, &budget)?
                    .ok_or(NativeError::Missing)?;
                let parent = Arc::new(
                    parent
                        .child(&records::hex(&operation), &context.security, &budget)?
                        .ok_or(NativeError::Missing)?,
                );
                let image = parent.open_image(
                    "helper-copy.exe",
                    true,
                    expected.version(),
                    &context.security,
                    &budget,
                )?;
                if image.facts != *expected.facts() {
                    return Err(NativeError::Unsupported);
                }
                Ok(OpenedPe(Arc::new(ApprovedImage {
                    target: context.target.nonce,
                    parent,
                    leaf: "helper-copy.exe".into(),
                    image,
                    expected,
                })))
            })
        }
        pub(crate) fn backup(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            lock: &InstallerLock,
            permit: &MutationPermit,
            observed: OpaqueLeaf,
            deadline: &Deadline,
        ) -> NativeResult<FileIdentity> {
            io.lock_binding(proof, lock, deadline)?;
            check_permit(io, permit, Phase::BackupIntent, Some(observed.role))?;
            if observed.target != io.context.target.nonce {
                return Err(NativeError::Foreign);
            }
            let root = self.0.clone();
            let context = io.context.clone();
            let lease = lock.0.clone();
            let budget = proof.budget(io, deadline)?;
            let operation = permit.operation();
            let bytes = permit.bytes().to_vec();
            io.owner.run(Dispatch::Mutation, deadline, move || {
                let change = Change::new();
                change.finish((|| {
                    validate_payload_lock(&context, &lease, &budget)?;
                    validate_intent(&context, &lease, operation, &bytes, &budget)?;
                    let root = PayloadRoot(root)
                        .check(&context, &budget)?
                        .ok_or(NativeError::Missing)?;
                    let journal: super::super::payload::recovery::OperationRecord =
                        records::record_data(&records::RecordName::Operation(operation), &bytes)?;
                    if journal.role(observed.role)?.original
                        != super::super::payload::recovery::OriginalLeaf::Present(
                            observed.observed.identity.into(),
                        )
                    {
                        return Err(NativeError::Foreign);
                    }
                    let parent =
                        ensure_payload_child(&root, "payload-backups", &context, &budget, &change)?;
                    let parent = ensure_payload_child(
                        &parent,
                        &records::hex(&operation),
                        &context,
                        &budget,
                        &change,
                    )?;
                    change.reached();
                    root.move_opaque(
                        observed.observed,
                        &parent,
                        observed.role.leaf(),
                        &context.security,
                        &budget,
                    )
                })())
            })
        }
        pub(crate) fn publish(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            lock: &InstallerLock,
            permit: &MutationPermit,
            staged: StagedPe,
            deadline: &Deadline,
        ) -> NativeResult<OpenedPe> {
            io.lock_binding(proof, lock, deadline)?;
            check_permit(io, permit, Phase::PublishIntent, Some(staged.role))?;
            if staged.target != io.context.target.nonce || staged.operation != permit.operation() {
                return Err(NativeError::Foreign);
            }
            let root = self.0.clone();
            let context = io.context.clone();
            let lease = lock.0.clone();
            let budget = proof.budget(io, deadline)?;
            let operation = permit.operation();
            let bytes = permit.bytes().to_vec();
            io.owner.run(Dispatch::Mutation, deadline, move || {
                let change = Change::new();
                change.finish((|| {
                    validate_payload_lock(&context, &lease, &budget)?;
                    validate_intent(&context, &lease, operation, &bytes, &budget)?;
                    let root = PayloadRoot(root)
                        .check(&context, &budget)?
                        .ok_or(NativeError::Missing)?;
                    let fresh = native::measure_image(
                        staged.image.file.clone(),
                        staged.expected.version(),
                        &budget,
                    )?;
                    if fresh.identity != staged.image.identity
                        || fresh.facts != *staged.expected.facts()
                    {
                        return Err(NativeError::Foreign);
                    }
                    drop(fresh);
                    change.reached();
                    staged.parent.publish_image(
                        &staged.image,
                        &root,
                        staged.role.leaf(),
                        &context.security,
                        &budget,
                    )?;
                    // Drop the writable stage before returning a readonly approved fixed-image pin.
                    let identity = staged.image.identity;
                    let expected = staged.expected;
                    drop(staged.image);
                    let image = root.open_image(
                        staged.role.leaf(),
                        true,
                        expected.version(),
                        &context.security,
                        &budget,
                    )?;
                    if image.identity != identity || image.facts != *expected.facts() {
                        return Err(NativeError::OutcomeUnknown);
                    }
                    Ok(OpenedPe(Arc::new(ApprovedImage {
                        target: context.target.nonce,
                        parent: root,
                        leaf: staged.role.leaf().into(),
                        image,
                        expected,
                    })))
                })())
            })
        }
        pub(crate) fn prune(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            lock: &InstallerLock,
            permit: &MutationPermit,
            completed: [u8; 16],
            deadline: &Deadline,
        ) -> NativeResult<PruneOutcome> {
            io.lock_binding(proof, lock, deadline)?;
            check_permit(io, permit, Phase::PruneIntent, None)?;
            let root = self.0.clone();
            let context = io.context.clone();
            let lease = lock.0.clone();
            let budget = proof.budget(io, deadline)?;
            let operation = permit.operation();
            let bytes = permit.bytes().to_vec();
            io.owner.run(Dispatch::Mutation, deadline, move || {
                let change = Change::new();
                change.finish((|| {
                    validate_payload_lock(&context, &lease, &budget)?;
                    validate_intent(&context, &lease, operation, &bytes, &budget)?;
                    let (_, catalog_bytes) = lease
                        .parent
                        .read_private(
                            &records::RecordName::StageCatalog.file_name()?,
                            &context.security,
                            files::MAX_RECORD_BYTES,
                            &budget,
                        )?
                        .ok_or(NativeError::Missing)?;
                    let catalog: super::super::payload::recovery::StageCatalog =
                        records::record_data(&records::RecordName::StageCatalog, &catalog_bytes)?;
                    if !super::super::payload::recovery::prune_candidates(&catalog)?
                        .contains(&completed)
                    {
                        return Err(NativeError::Foreign);
                    }
                    let removed = || {
                        PruneOutcome::Removed(PrunedGeneration {
                            target: context.target.nonce,
                            operation,
                            generation: completed,
                        })
                    };
                    let Some(root) = PayloadRoot(root).check(&context, &budget)? else {
                        return Ok(removed());
                    };
                    let Some(parent) = root.child("payload-backups", &context.security, &budget)?
                    else {
                        return Ok(removed());
                    };
                    // A failed prune is incomplete retention; the owner must settle before another effect.
                    change.reached();
                    match parent.prune_tree(&records::hex(&completed), &context.security, &budget) {
                        Ok(()) => Ok(removed()),
                        Err(NativeError::Busy | NativeError::Foreign) => Ok(PruneOutcome::Retained),
                        Err(error) => Err(error),
                    }
                })())
            })
        }
    }
    struct OffsetReader {
        file: File,
        offset: u64,
    }
    impl std::io::Read for OffsetReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            use std::os::windows::fs::FileExt;
            let count = self.file.seek_read(buffer, self.offset)?;
            self.offset += count as u64;
            Ok(count)
        }
    }
    fn ensure_payload_child(
        parent: &Anchor,
        name: &str,
        context: &Context,
        budget: &Deadline,
        change: &Change,
    ) -> NativeResult<Anchor> {
        if let Some(child) = parent.child(name, &context.security, budget)? {
            return Ok(child);
        }
        change.reached();
        parent.create_child_directory(name, &context.security, budget)
    }
    fn check_permit(
        io: &WindowsNativeIo,
        permit: &MutationPermit,
        phase: Phase,
        role: Option<PayloadRole>,
    ) -> NativeResult<()> {
        if !std::ptr::eq(io, permit.io().as_ref())
            || permit.phase() != phase
            || permit.role() != role
        {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
    fn validate_payload_lock(
        context: &Context,
        lease: &LockState,
        budget: &Deadline,
    ) -> NativeResult<()> {
        context.validate(budget)?;
        if lease.target != context.target.nonce {
            return Err(NativeError::Foreign);
        }
        lease.parent.revalidate(&context.security, true, budget)?;
        if native::observe(&lease.file, "install.lock", &context.security)?.identity
            != lease.identity
        {
            return Err(NativeError::Foreign);
        }
        budget.check()
    }
    fn validate_intent(
        context: &Context,
        lease: &LockState,
        operation: [u8; 16],
        expected: &[u8],
        budget: &Deadline,
    ) -> NativeResult<()> {
        let name = records::RecordName::Operation(operation);
        let (_, bytes) = lease
            .parent
            .read_private(
                &name.file_name()?,
                &context.security,
                files::MAX_RECORD_BYTES,
                budget,
            )?
            .ok_or(NativeError::Missing)?;
        if bytes != expected {
            return Err(NativeError::Foreign);
        }
        let (_, catalog) = lease
            .parent
            .read_private(
                &records::RecordName::StageCatalog.file_name()?,
                &context.security,
                files::MAX_RECORD_BYTES,
                budget,
            )?
            .ok_or(NativeError::Missing)?;
        let catalog: super::super::payload::recovery::StageCatalog =
            records::record_data(&records::RecordName::StageCatalog, &catalog)?;
        catalog.validate()?;
        if catalog.active != Some(operation) {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
    pub struct WindowsNativeIo {
        context: Arc<Context>,
        owner: Arc<CallOwner>,
        clock: Arc<dyn Clock>,
    }
    /// A checked lease of the caller's SAME installer lock, never a newly acquired lock.
    // Shipping consumers are cfg(not(test)); integration roots cover this exact new bridge.
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) struct StopLockLease {
        io: Arc<WindowsNativeIo>,
        lock: InstallerLock,
    }
    // Shipping consumers are cfg(not(test)); integration roots cover this exact new bridge.
    #[cfg_attr(test, allow(dead_code))]
    impl StopLockLease {
        pub(crate) fn lock(&self) -> &InstallerLock {
            &self.lock
        }
        pub(crate) fn reverify(
            &self,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.io.verify_stop_lock(proof, &self.lock, deadline)
        }
    }
    /// Private, same-context launch admission. No path or journal constructs this capability.
    // Shipping consumers are cfg(not(test)); integration roots cover this exact new bridge.
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) struct JobAdmission {
        io: Arc<WindowsNativeIo>,
        agent: OpenedPe,
        root: PayloadRoot,
    }
    // Shipping consumers are cfg(not(test)); integration roots cover this exact new bridge.
    #[cfg_attr(test, allow(dead_code))]
    impl JobAdmission {
        pub(crate) fn io(&self) -> &Arc<WindowsNativeIo> {
            &self.io
        }
        pub(crate) fn application(&self) -> &str {
            self.agent.canonical_dos_path()
        }
        pub(crate) fn cwd(&self) -> &str {
            self.root.canonical_dos_path()
        }
        pub(crate) fn reverify(
            &self,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.agent.reverify(&self.io, proof, deadline)?;
            let budget = proof.budget(&self.io, deadline)?;
            self.io.context.validate(&budget)?;
            if self.root.0.target != self.io.context.target.nonce {
                return Err(NativeError::Foreign);
            }
            let install = self
                .root
                .0
                .install
                .lock()
                .map_err(|_| NativeError::Unavailable)?;
            install.as_ref().ok_or(NativeError::Missing)?.revalidate(
                &self.io.context.security,
                false,
                &budget,
            )
        }
        pub(crate) fn agent_matches(
            &self,
            agent: &AgentObservation,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.reverify(proof, deadline)?;
            let budget = proof.budget(&self.io, deadline)?;
            if agent.0.target != self.io.context.target.nonce
                || agent.0.image_identity != self.agent.identity()
            {
                return Err(NativeError::Foreign);
            }
            agent.0.validate_pins(&self.io.context, &budget)
        }
        pub(crate) fn retained_agent(
            &self,
            agent: &AgentObservation,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<Arc<std::os::windows::io::OwnedHandle>> {
            self.agent_matches(agent, proof, deadline)?;
            let budget = proof.budget(&self.io, deadline)?;
            let handle = agent.0.process.duplicate_retained(&budget)?;
            self.agent_matches(agent, proof, deadline)?;
            Ok(handle)
        }
    }
    /// Rooted, opaque OLD installer-role evidence for completion only; never image approval.
    // Shipping consumers are cfg(not(test)); integration roots cover this exact new bridge.
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) struct BrokerAdmission {
        io: Arc<WindowsNativeIo>,
        runtime: Mutex<Option<Arc<Anchor>>>,
        parent: Arc<Anchor>,
        expected_runtime: String,
        install: Arc<Anchor>,
        installer: Mutex<Option<Arc<File>>>,
        installer_identity: FileIdentity,
        installer_path: String,
        endpoint: String,
    }
    // Shipping consumers are cfg(not(test)); integration roots cover this exact new bridge.
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) struct PeerRolePin {
        context: Arc<Context>,
        parent: Arc<Anchor>,
        file: File,
        name: String,
        path: String,
        identity: FileIdentity,
    }
    // Shipping consumers are cfg(not(test)); integration roots cover this exact new bridge.
    #[cfg_attr(test, allow(dead_code))]
    impl PeerRolePin {
        pub(crate) fn path(&self) -> &str {
            &self.path
        }
        pub(crate) fn identity(&self) -> FileIdentity {
            self.identity
        }
        pub(crate) fn reverify(&self, deadline: &Deadline) -> NativeResult<()> {
            self.context.validate(deadline)?;
            self.parent
                .revalidate(&self.context.security, false, deadline)?;
            let facts = native::observe(&self.file, &self.name, &self.context.security)?;
            files::admit_component(&facts, Admission::PrivateFile)?;
            let (_, current) = self.parent.open_file_metadata(
                &PrivateName::new(&self.name)?,
                &self.context.security,
                deadline,
            )?;
            if facts.identity != self.identity || current != self.identity {
                return Err(NativeError::Foreign);
            }
            deadline.check()
        }
    }
    // Shipping consumers are cfg(not(test)); integration roots cover this exact new bridge.
    #[cfg_attr(test, allow(dead_code))]
    impl BrokerAdmission {
        pub(crate) fn io(&self) -> &Arc<WindowsNativeIo> {
            &self.io
        }
        pub(crate) fn token(&self) -> &TokenFacts {
            &self.io.context.target.identity
        }
        pub(crate) fn installer_identity(&self) -> FileIdentity {
            self.installer_identity
        }
        pub(crate) fn endpoint(&self) -> &str {
            &self.endpoint
        }
        pub(crate) fn reverify(
            &self,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            let budget = proof.budget(&self.io, deadline)?;
            self.io.context.validate(&budget)?;
            self.parent
                .revalidate(&self.io.context.security, true, &budget)?;
            let runtime = self
                .runtime
                .lock()
                .map_err(|_| NativeError::Unavailable)?
                .as_ref()
                .cloned();
            if let Some(runtime) = runtime {
                runtime.revalidate(&self.io.context.security, true, &budget)?;
            }
            self.install
                .revalidate(&self.io.context.security, false, &budget)?;
            let pin = self
                .installer
                .lock()
                .map_err(|_| NativeError::Unavailable)?
                .as_ref()
                .cloned()
                .ok_or(NativeError::Unsupported)?;
            let facts =
                native::observe(&pin, "crosspane-installer.exe", &self.io.context.security)?;
            files::admit_component(&facts, Admission::PrivateFile)?;
            let (_, current) = self.install.open_file_metadata(
                &PrivateName::new("crosspane-installer.exe")?,
                &self.io.context.security,
                &budget,
            )?;
            if facts.identity != self.installer_identity || current != self.installer_identity {
                return Err(NativeError::Foreign);
            }
            Ok(())
        }
        pub(crate) fn bind_runtime(
            &self,
            agent: &AgentObservation,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.reverify(proof, deadline)?;
            if agent.0.target != self.io.context.target.nonce {
                return Err(NativeError::Foreign);
            }
            agent.revalidate(&self.io, proof, deadline)?;
            let actual = agent
                .0
                .runtime_canonical
                .to_str()
                .ok_or(NativeError::Unsupported)?;
            if process::literal_path(actual)? != process::literal_path(&self.expected_runtime)? {
                return Err(NativeError::Foreign);
            }
            *self.runtime.lock().map_err(|_| NativeError::Unavailable)? =
                Some(agent.0.runtime.clone());
            Ok(())
        }
        pub(crate) fn peer_role(
            &self,
            observed_image: &str,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<PeerRolePin> {
            let budget = proof.budget(&self.io, deadline)?;
            let context = self.io.context.clone();
            context.validate(&budget)?;
            let image = process::literal_path(observed_image)?;
            if image == process::literal_path(&self.installer_path)? {
                let (file, identity) = self.install.open_file_metadata(
                    &PrivateName::new("crosspane-installer.exe")?,
                    &context.security,
                    &budget,
                )?;
                if identity != self.installer_identity {
                    return Err(NativeError::Foreign);
                }
                return Ok(PeerRolePin {
                    context,
                    parent: self.install.clone(),
                    file,
                    name: "crosspane-installer.exe".into(),
                    path: self.installer_path.clone(),
                    identity,
                });
            }
            // ONLY the active fixed helper role can be selected. The observed image is compared,
            // never opened; catalog ids select this one anchored constant internal path.
            let observed = self
                .io
                .read_record(
                    proof,
                    records::RecordName::StageCatalog,
                    files::MAX_RECORD_BYTES,
                    deadline,
                )?
                .ok_or(NativeError::Missing)?;
            let catalog: super::super::payload::recovery::StageCatalog =
                records::record_data(&records::RecordName::StageCatalog, observed.bytes())?;
            catalog.validate()?;
            let operation = catalog.active.ok_or(NativeError::Unsupported)?;
            let op: String = operation.iter().map(|byte| format!("{byte:02x}")).collect();
            let fixed = format!("{}\\payload-stage\\{op}", context.target.paths.install());
            let parent = Arc::new(
                Anchor::open(&fixed, &context.security, true, &budget)?
                    .ok_or(NativeError::Missing)?,
            );
            let path = format!(
                "{}\\helper-copy.exe",
                parent
                    .canonical_dos_path(&context.security, &budget)?
                    .to_str()
                    .ok_or(NativeError::Unsupported)?
            );
            if image != process::literal_path(&path)? {
                return Err(NativeError::Unsupported);
            }
            let (file, identity) = parent.open_file_metadata(
                &PrivateName::new("helper-copy.exe")?,
                &context.security,
                &budget,
            )?;
            Ok(PeerRolePin {
                context,
                parent,
                file,
                name: "helper-copy.exe".into(),
                path,
                identity,
            })
        }
        pub(crate) fn release_image_pin(&self) -> NativeResult<()> {
            self.installer
                .lock()
                .map_err(|_| NativeError::Unavailable)?
                .take();
            Ok(())
        }
    }

    /// Completion-only own-module admission. No approval or facts constructor exists.
    #[cfg(not(test))]
    pub(crate) struct OuterCompletionAdmission {
        io: Arc<WindowsNativeIo>,
        module: SelfImagePin,
        own: process::own::OwnProcessIdentity,
        keeper: OuterPeerImage,
        operation: [u8; 16],
    }
    #[cfg(not(test))]
    impl OuterCompletionAdmission {
        pub(crate) fn operation(&self) -> [u8; 16] {
            self.operation
        }
        pub(crate) fn reverify(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if !std::ptr::eq(io, self.io.as_ref()) {
                return Err(NativeError::Foreign);
            }
            self.module.reverify(io, proof, deadline)?;
            self.own.reverify(deadline)?;
            self.keeper.reverify(io, proof, deadline)?;
            let record = io.outer_stop_selection(proof, self.operation, deadline)?;
            record.peer_matches(
                self.own.pid(),
                self.own.creation(),
                epoch_stamp(self.module.identity()),
                self.module.facts(),
                io.target().identity(),
            )?;
            if self.module.identity() != self.keeper.identity()
                || self.module.facts() != self.keeper.facts()
                || process::literal_path(self.module.canonical_dos_path())?
                    != process::literal_path(self.keeper.canonical_dos_path())?
            {
                return Err(NativeError::Foreign);
            }
            deadline.check()
        }
    }
    /// Measured read pin, deliberately not SelfImagePin/OpenedPe or launch approval.
    #[cfg(not(test))]
    pub(crate) struct OuterPeerImage {
        io: Arc<WindowsNativeIo>,
        parent: Arc<Anchor>,
        leaf: String,
        image: native::ImageData,
        private: bool,
    }
    #[cfg(not(test))]
    impl OuterPeerImage {
        pub(crate) fn identity(&self) -> FileIdentity {
            self.image.identity
        }
        pub(crate) fn facts(&self) -> &PeFacts {
            &self.image.facts
        }
        pub(crate) fn canonical_dos_path(&self) -> &str {
            &self.image.canonical
        }
        pub(crate) fn reverify(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if !std::ptr::eq(io, self.io.as_ref()) {
                return Err(NativeError::Foreign);
            }
            let context = io.context.clone();
            let parent = self.parent.clone();
            let leaf = self.leaf.clone();
            let private = self.private;
            let expected = self.image.identity;
            let facts = self.image.facts.clone();
            let budget = proof.budget(io, deadline)?;
            io.owner.run(Dispatch::Observation, deadline, move || {
                context.validate(&budget)?;
                let fresh = parent.open_image(
                    &leaf,
                    private,
                    &facts.version,
                    &context.security,
                    &budget,
                )?;
                if fresh.identity != expected || fresh.facts != facts {
                    return Err(NativeError::Foreign);
                }
                budget.check()
            })
        }
    }

    /// Genuine current-key reservation; never reconstructed from a logon record.
    #[cfg(not(test))]
    pub(crate) struct LogonReservation {
        io: Arc<WindowsNativeIo>,
        namespace: Arc<super::supervisor_owner::ExclusiveSupervisorLease>,
        own: process::own::OwnProcessIdentity,
        context: TokenFacts,
        epoch_prepared: std::sync::OnceLock<PreparedLogonGate>,
        epoch: std::sync::OnceLock<([u8; 16], [u8; 16])>,
    }
    #[cfg(not(test))]
    impl LogonReservation {
        pub(crate) fn bind_epoch(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            registration: [u8; 16],
            operation: [u8; 16],
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.reverify(io, proof, deadline)?;
            if registration == [0; 16] || operation == [0; 16] {
                return Err(NativeError::Invalid);
            }
            if let Some(bound) = self.epoch.get() {
                return if *bound == (registration, operation) {
                    Ok(())
                } else {
                    Err(NativeError::Foreign)
                };
            }
            self.epoch
                .set((registration, operation))
                .map_err(|_| NativeError::Foreign)
        }
        pub(crate) fn registration(&self) -> NativeResult<[u8; 16]> {
            self.epoch
                .get()
                .map(|value| value.0)
                .ok_or(NativeError::Foreign)
        }
        pub(crate) fn operation(&self) -> NativeResult<[u8; 16]> {
            self.epoch
                .get()
                .map(|value| value.1)
                .ok_or(NativeError::Foreign)
        }
        pub(crate) fn context(&self) -> &TokenFacts {
            &self.context
        }
        pub(crate) fn owner_pid(&self) -> u32 {
            self.own.pid()
        }
        pub(crate) fn owner_creation(&self) -> u64 {
            self.own.creation()
        }
        pub(crate) fn reverify(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if !std::ptr::eq(io, self.io.as_ref()) || self.context != io.context.target.identity {
                return Err(NativeError::Foreign);
            }
            let budget = proof.budget(io, deadline)?;
            io.context.validate(&budget)?;
            self.own.reverify(&budget)?;
            self.namespace.reverify(io, proof, deadline)
        }
        /// This is a live-memory gate set only after the actual archive/preparation factory.
        /// The record's phase cannot set it, and a cold reservation cannot inherit it.
        pub(crate) fn ensure_epoch_prepared(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.reverify(io, proof, deadline)?;
            let prepared = self.epoch_prepared.get().ok_or(NativeError::Foreign)?;
            if prepared.provenance.current().registration() != self.registration()?
                || prepared.provenance.current().operation() != self.operation()?
            {
                return Err(NativeError::Foreign);
            }
            let observed = verify_logon_preparing_files(
                &self.io,
                proof,
                prepared.provenance.current(),
                prepared.archived.as_ref(),
                deadline,
            )?;
            if observed != prepared.provenance {
                return Err(NativeError::Foreign);
            }
            let current = io
                .read_record(
                    proof,
                    records::RecordName::SupervisorLogon,
                    files::MAX_RECORD_BYTES,
                    deadline,
                )?
                .ok_or(NativeError::Foreign)?;
            if current.identity != prepared.preparing_identity
                || current.bytes() != prepared.preparing_bytes
            {
                return Err(NativeError::Foreign);
            }
            self.reverify(io, proof, deadline)
        }
    }
    /// Exact immutable token/epoch correlation obtained only from a fresh fixed record pair.
    /// This does not select a PID, represent a job, or prove that an owner exited.
    #[cfg(not(test))]
    pub(crate) struct MatchedLogonProvenance {
        io: Arc<WindowsNativeIo>,
        epoch: super::activation::EpochProvenance,
    }
    #[cfg(not(test))]
    enum PriorLogonKind {
        FirstAbsent,
        SessionGone(MatchedLogonProvenance),
    }
    /// Narrow logon/archive admission only. NEVER an UpgradeStopProof or job-zero observation.
    #[cfg(not(test))]
    pub(crate) struct PriorLogonDisposition {
        io: Arc<WindowsNativeIo>,
        kind: PriorLogonKind,
    }
    #[cfg(not(test))]
    impl PriorLogonDisposition {
        pub(crate) fn reverify(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if !std::ptr::eq(io, self.io.as_ref()) {
                return Err(NativeError::Foreign);
            }
            let budget = proof.budget(io, deadline)?;
            io.context.validate(&budget)?;
            match &self.kind {
                PriorLogonKind::FirstAbsent => deadline.check(),
                PriorLogonKind::SessionGone(prior) => io.query_prior_logon(proof, prior, deadline),
            }
        }
    }

    #[cfg(not(test))]
    fn legacy_provenance_required() -> NativeError {
        use std::io::Write;
        let _ = writeln!(
            std::io::stderr().lock(),
            "Windows supervisor logon provenance unavailable; reinstall or upgrade required"
        );
        NativeError::Unsupported
    }
    #[cfg(not(test))]
    struct LsaBuffer(
        *mut windows_sys::Win32::Security::Authentication::Identity::SECURITY_LOGON_SESSION_DATA,
    );
    #[cfg(not(test))]
    impl Drop for LsaBuffer {
        fn drop(&mut self) {
            // SAFETY: only STATUS_SUCCESS assigns documented ownership to this RAII buffer.
            // No session names or returned contents are read; the same native worker frees it.
            let _ = unsafe {
                windows_sys::Win32::Security::Authentication::Identity::LsaFreeReturnBuffer(
                    self.0.cast(),
                )
            };
        }
    }
    // A failed call's nonnull output has undocumented ownership. Keep at most one opaque value;
    // never dereference/free it, and refuse all subsequent queries instead of accumulating it.
    #[cfg(not(test))]
    static LOGON_QUERY_BUSY: AtomicBool = AtomicBool::new(false);
    #[cfg(not(test))]
    struct LogonQueryGuard;
    #[cfg(not(test))]
    impl Drop for LogonQueryGuard {
        fn drop(&mut self) {
            LOGON_QUERY_BUSY.store(false, Ordering::Release);
        }
    }
    #[cfg(not(test))]
    static LOGON_QUERY_QUARANTINE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

    #[cfg(not(test))]
    pub(crate) struct ArchivedSupervisorEpoch {
        io: Arc<WindowsNativeIo>,
        namespace: Arc<super::supervisor_owner::ExclusiveSupervisorLease>,
        permit: Arc<super::super::service::task::TaskRunPermit>,
        intent: super::epoch_archive::ArchiveIntent,
        bytes: Vec<u8>,
    }
    #[cfg(not(test))]
    pub(crate) struct FirstSupervisorEpoch {
        io: Arc<WindowsNativeIo>,
        namespace: Arc<super::supervisor_owner::ExclusiveSupervisorLease>,
        permit: Arc<super::super::service::task::TaskRunPermit>,
    }
    /// Neither variant has a record/fact constructor. Both start with a fresh actual own task
    /// claim, original IO and retained FIRST_INSTANCE namespace, before any child can be created.
    #[cfg(not(test))]
    pub(crate) enum OwnedArchiveResult {
        First(FirstSupervisorEpoch),
        Archived(ArchivedSupervisorEpoch),
    }
    #[cfg(not(test))]
    impl OwnedArchiveResult {
        pub(crate) fn reverify(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            lock: &InstallerLock,
            permit: &Arc<super::super::service::task::TaskRunPermit>,
            owner: &Arc<super::jobs::SupervisorOwner>,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            let (original, namespace, claim) = match self {
                Self::First(value) => (&value.io, &value.namespace, &value.permit),
                Self::Archived(value) => (&value.io, &value.namespace, &value.permit),
            };
            if !std::ptr::eq(io, original.as_ref())
                || !Arc::ptr_eq(permit, claim)
                || !Arc::ptr_eq(namespace, &owner.exclusive_lease(proof, deadline)?)
            {
                return Err(NativeError::Foreign);
            }
            io.verify_stop_lock(proof, lock, deadline)?;
            namespace.reverify(io, proof, deadline)?;
            claim.reverify(io, proof, deadline)?;
            preparing_matches(io, proof, &EpochClaim::Task(permit).epoch(io)?, deadline)?;
            let current = io.read_record(
                proof,
                records::RecordName::Supervisor,
                files::MAX_RECORD_BYTES,
                deadline,
            )?;
            if current.is_some() {
                return Err(NativeError::Foreign);
            }
            match self {
                Self::First(_) => {
                    if io
                        .read_record(
                            proof,
                            records::RecordName::SupervisorArchiveIntent,
                            files::MAX_RECORD_BYTES,
                            deadline,
                        )?
                        .is_some()
                        || super::super::payload::recovery::selected_operation(io, proof, deadline)?
                            .is_some()
                    {
                        return Err(NativeError::Foreign);
                    }
                }
                Self::Archived(value) => {
                    let intent =
                        read_archive_intent(io, proof, deadline)?.ok_or(NativeError::Foreign)?;
                    if intent != value.intent
                        || intent.phase != super::epoch_archive::ArchivePhase::Complete
                    {
                        return Err(NativeError::Foreign);
                    }
                    let record = io
                        .read_record(
                            proof,
                            records::RecordName::SupervisorEpoch(intent.slot),
                            files::MAX_RECORD_BYTES,
                            deadline,
                        )?
                        .ok_or(NativeError::Foreign)?;
                    if record.identity != epoch_identity(intent.source)
                        || record.bytes() != value.bytes
                    {
                        return Err(NativeError::Foreign);
                    }
                    let prior = super::super::service::journal::Journal::decode(record.bytes())?;
                    let provenance = preparing_matches(
                        io,
                        proof,
                        &EpochClaim::Task(permit).epoch(io)?,
                        deadline,
                    )?;
                    provenance.matches_history(
                        intent.slot,
                        &prior,
                        intent.source,
                        intent.sha256,
                    )?;
                    let selected =
                        super::super::payload::recovery::selected_operation(io, proof, deadline)?;
                    let lineage = super::activation::correlate_upgrade(
                        claim.operation(),
                        claim.user(),
                        selected.as_ref(),
                        Some(&prior),
                    )?
                    .ok_or(NativeError::Foreign)?;
                    if lineage.operation() != intent.operation
                        || intent.owner_creation != claim.owner_identity().creation()
                    {
                        return Err(NativeError::Foreign);
                    }
                }
            }
            deadline.check()
        }
    }
    #[cfg(not(test))]
    impl OwnedArchiveResult {
        pub(crate) fn publish_bound(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            lock: &InstallerLock,
            permit: &Arc<super::super::service::task::TaskRunPermit>,
            owner: &Arc<super::jobs::SupervisorOwner>,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            let (original, namespace, claim) = match self {
                Self::First(value) => (&value.io, &value.namespace, &value.permit),
                Self::Archived(value) => (&value.io, &value.namespace, &value.permit),
            };
            if !std::ptr::eq(io, original.as_ref())
                || !Arc::ptr_eq(permit, claim)
                || !Arc::ptr_eq(namespace, &owner.exclusive_lease(proof, deadline)?)
            {
                return Err(NativeError::Foreign);
            }
            publish_epoch_bound(
                io,
                proof,
                lock,
                namespace,
                EpochClaim::Task(permit),
                owner,
                deadline,
            )
        }
    }
    #[cfg(not(test))]
    #[derive(Clone)]
    struct ArchivedEpochFiles {
        intent: super::epoch_archive::ArchiveIntent,
        bytes: Vec<u8>,
    }
    /// Minted only by completed native preparation. Retains exact immutable observed files and
    /// correlation, not an Arc to result/permit: no reservation/permit ownership cycle exists.
    #[cfg(not(test))]
    struct PreparedLogonGate {
        provenance: super::activation::SupervisorLogonRecord,
        preparing_identity: FileIdentity,
        preparing_bytes: Vec<u8>,
        archived: Option<ArchivedEpochFiles>,
    }
    #[cfg(not(test))]
    fn verify_logon_preparing_files(
        io: &Arc<WindowsNativeIo>,
        proof: &SupportProof,
        expected: &super::activation::EpochProvenance,
        archived: Option<&ArchivedEpochFiles>,
        deadline: &Deadline,
    ) -> NativeResult<super::activation::SupervisorLogonRecord> {
        let record = preparing_matches(io, proof, expected, deadline)?;
        if io
            .read_record(
                proof,
                records::RecordName::Supervisor,
                files::MAX_RECORD_BYTES,
                deadline,
            )?
            .is_some()
        {
            return Err(NativeError::Foreign);
        }
        match archived {
            None => {
                if read_archive_intent(io, proof, deadline)?.is_some() {
                    return Err(NativeError::Foreign);
                }
            }
            Some(archived) => {
                let intent =
                    read_archive_intent(io, proof, deadline)?.ok_or(NativeError::Foreign)?;
                intent.require_complete()?;
                if intent != archived.intent
                    || intent.operation != expected.operation()
                    || intent.owner_creation != expected.owner_creation()
                {
                    return Err(NativeError::Foreign);
                }
                let source = io
                    .read_record(
                        proof,
                        records::RecordName::SupervisorEpoch(intent.slot),
                        files::MAX_RECORD_BYTES,
                        deadline,
                    )?
                    .ok_or(NativeError::Foreign)?;
                if source.identity != epoch_identity(intent.source)
                    || source.bytes() != archived.bytes
                {
                    return Err(NativeError::Foreign);
                }
                let prior = super::super::service::journal::Journal::decode(source.bytes())?;
                let epoch = record
                    .matches_history(intent.slot, &prior, intent.source, intent.sha256)?
                    .clone();
                io.query_prior_logon(
                    proof,
                    &MatchedLogonProvenance {
                        io: io.clone(),
                        epoch,
                    },
                    deadline,
                )?;
            }
        }
        Ok(record)
    }
    /// Separate opaque result: existing task/upgrade result constructors remain unchanged.
    #[cfg(not(test))]
    pub(crate) struct LogonArchiveResult {
        io: Arc<WindowsNativeIo>,
        namespace: Arc<super::supervisor_owner::ExclusiveSupervisorLease>,
        permit: Arc<super::super::service::SupervisorLogonPermit>,
        archived: Option<ArchivedEpochFiles>,
    }
    #[cfg(not(test))]
    impl LogonArchiveResult {
        fn renew(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            permit: &Arc<super::super::service::SupervisorLogonPermit>,
            owner: &Arc<super::jobs::SupervisorOwner>,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if !std::ptr::eq(io, self.io.as_ref())
                || !Arc::ptr_eq(permit, &self.permit)
                || !Arc::ptr_eq(&self.namespace, &owner.exclusive_lease(proof, deadline)?)
            {
                return Err(NativeError::Foreign);
            }
            permit.reverify(io, proof, deadline)?;
            self.namespace.reverify(io, proof, deadline)
        }
        pub(crate) fn reverify(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            lock: &InstallerLock,
            permit: &Arc<super::super::service::SupervisorLogonPermit>,
            owner: &Arc<super::jobs::SupervisorOwner>,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.renew(io, proof, permit, owner, deadline)?;
            io.verify_stop_lock(proof, lock, deadline)?;
            let expected = EpochClaim::Logon(permit).epoch(io)?;
            let provenance = verify_logon_preparing_files(
                &self.io,
                proof,
                &expected,
                self.archived.as_ref(),
                deadline,
            )?;
            match &self.archived {
                None => {
                    if !matches!(permit.disposition().kind, PriorLogonKind::FirstAbsent) {
                        return Err(NativeError::Foreign);
                    }
                }
                Some(archived) => {
                    let prior = super::super::service::journal::Journal::decode(&archived.bytes)?;
                    let epoch = provenance.matches_history(
                        archived.intent.slot,
                        &prior,
                        archived.intent.source,
                        archived.intent.sha256,
                    )?;
                    let PriorLogonKind::SessionGone(matched) = &permit.disposition().kind else {
                        return Err(NativeError::Foreign);
                    };
                    if epoch != &matched.epoch {
                        return Err(NativeError::Foreign);
                    }
                    permit.disposition().reverify(io, proof, deadline)?;
                }
            }
            deadline.check()
        }
        pub(crate) fn publish_bound(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            lock: &InstallerLock,
            permit: &Arc<super::super::service::SupervisorLogonPermit>,
            owner: &Arc<super::jobs::SupervisorOwner>,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.renew(io, proof, permit, owner, deadline)?;
            publish_epoch_bound(
                io,
                proof,
                lock,
                &self.namespace,
                EpochClaim::Logon(permit),
                owner,
                deadline,
            )
        }
    }

    #[cfg(not(test))]
    fn epoch_identity(stamp: super::super::payload::recovery::FileStamp) -> FileIdentity {
        FileIdentity {
            volume: stamp.volume,
            file: stamp.file,
        }
    }
    #[cfg(not(test))]
    fn epoch_stamp(identity: FileIdentity) -> super::super::payload::recovery::FileStamp {
        super::super::payload::recovery::FileStamp {
            volume: identity.volume,
            file: identity.file,
        }
    }
    #[cfg(not(test))]
    fn epoch_hash(bytes: &[u8]) -> [u8; 32] {
        let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes);
        let mut result = [0; 32];
        result.copy_from_slice(digest.as_ref());
        result
    }
    #[cfg(not(test))]
    fn read_archive_intent(
        io: &WindowsNativeIo,
        proof: &SupportProof,
        deadline: &Deadline,
    ) -> NativeResult<Option<super::epoch_archive::ArchiveIntent>> {
        io.read_record(
            proof,
            records::RecordName::SupervisorArchiveIntent,
            files::MAX_RECORD_BYTES,
            deadline,
        )?
        .map(|record| {
            let value: super::epoch_archive::ArchiveIntent = records::record_data(
                &records::RecordName::SupervisorArchiveIntent,
                record.bytes(),
            )?;
            value.validate()?;
            Ok(value)
        })
        .transpose()
    }
    #[cfg(not(test))]
    fn publish_archive_intent(
        io: &WindowsNativeIo,
        proof: &SupportProof,
        lock: &InstallerLock,
        intent: &super::epoch_archive::ArchiveIntent,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        intent.validate()?;
        let bytes = records::encode_record(
            &records::RecordName::SupervisorArchiveIntent,
            serde_json::to_value(intent).map_err(|_| NativeError::Invalid)?,
        )?;
        let result = io.publish_record(
            proof,
            lock,
            records::RecordName::SupervisorArchiveIntent,
            &bytes,
            deadline,
        )?;
        if result.native_failure.is_some()
            || result.state != records::PublicationRecovery::NewPublished
        {
            return Err(NativeError::OutcomeUnknown);
        }
        Ok(())
    }

    #[cfg(not(test))]
    #[derive(Clone, Copy)]
    enum EpochClaim<'a> {
        Task(&'a Arc<super::super::service::task::TaskRunPermit>),
        Logon(&'a Arc<super::super::service::SupervisorLogonPermit>),
    }
    #[cfg(not(test))]
    impl EpochClaim<'_> {
        fn reverify(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            match self {
                Self::Task(permit) => permit.reverify(io, proof, deadline),
                Self::Logon(permit) => permit.reverify(io, proof, deadline),
            }
        }
        fn epoch(&self, io: &WindowsNativeIo) -> NativeResult<super::activation::EpochProvenance> {
            match self {
                Self::Task(permit) => super::activation::EpochProvenance::new(
                    permit.registration(),
                    permit.operation(),
                    &io.context.target.identity,
                    permit.owner_identity().pid(),
                    permit.owner_identity().creation(),
                ),
                Self::Logon(permit) => super::activation::EpochProvenance::new(
                    permit.registration(),
                    permit.operation(),
                    permit.context(),
                    permit.owner_identity().pid(),
                    permit.owner_identity().creation(),
                ),
            }
        }
        fn operation(&self) -> [u8; 16] {
            match self {
                Self::Task(permit) => permit.operation(),
                Self::Logon(permit) => permit.operation(),
            }
        }
        fn owner_creation(&self) -> u64 {
            match self {
                Self::Task(permit) => permit.owner_identity().creation(),
                Self::Logon(permit) => permit.owner_identity().creation(),
            }
        }
        fn renew_predecessor(
            &self,
            io: &Arc<WindowsNativeIo>,
            proof: &SupportProof,
            prior: &super::super::service::journal::Journal,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            let record = super::activation::SupervisorLogonRecord::read(io, proof, deadline)?
                .ok_or_else(legacy_provenance_required)?;
            let epoch = record.matches_current(prior)?;
            match self {
                Self::Task(permit) => {
                    let selected =
                        super::super::payload::recovery::selected_operation(io, proof, deadline)?;
                    super::activation::correlate_upgrade(
                        permit.operation(),
                        permit.user(),
                        selected.as_ref(),
                        Some(prior),
                    )?
                    .ok_or(NativeError::Foreign)?;
                }
                Self::Logon(permit) => {
                    let PriorLogonKind::SessionGone(matched) = &permit.disposition().kind else {
                        return Err(NativeError::Foreign);
                    };
                    if epoch != &matched.epoch {
                        return Err(NativeError::Foreign);
                    }
                    permit.disposition().reverify(io, proof, deadline)?;
                }
            }
            Ok(())
        }
    }

    #[cfg(not(test))]
    fn preparing_matches(
        io: &WindowsNativeIo,
        proof: &SupportProof,
        expected: &super::activation::EpochProvenance,
        deadline: &Deadline,
    ) -> NativeResult<super::activation::SupervisorLogonRecord> {
        let record = super::activation::SupervisorLogonRecord::read(io, proof, deadline)?
            .ok_or_else(legacy_provenance_required)?;
        if record.phase() != super::activation::LogonRecordPhase::Preparing
            || record.current() != expected
        {
            return Err(NativeError::Foreign);
        }
        Ok(record)
    }
    #[cfg(not(test))]
    fn publish_epoch_preparing(
        io: &Arc<WindowsNativeIo>,
        proof: &SupportProof,
        lock: &InstallerLock,
        next: super::activation::EpochProvenance,
        archived: Option<&super::epoch_archive::ArchiveIntent>,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        io.verify_stop_lock(proof, lock, deadline)?;
        let current = super::activation::SupervisorLogonRecord::read(io, proof, deadline)?;
        let record = match (current, archived) {
            (None, None) => super::activation::SupervisorLogonRecord::new(next)?,
            (Some(current), Some(intent)) => {
                intent.require_complete()?;
                current.rotate(
                    next,
                    super::activation::HistoryCorrelation::new(
                        intent.slot,
                        current.current().clone(),
                        intent.source,
                        intent.sha256,
                    )?,
                )?
            }
            _ => return Err(legacy_provenance_required()),
        };
        record.publish(io, proof, lock, deadline)?;
        preparing_matches(io, proof, record.current(), deadline)?;
        Ok(())
    }
    #[cfg(not(test))]
    fn publish_epoch_bound(
        io: &WindowsNativeIo,
        proof: &SupportProof,
        lock: &InstallerLock,
        namespace: &Arc<super::supervisor_owner::ExclusiveSupervisorLease>,
        claim: EpochClaim<'_>,
        owner: &Arc<super::jobs::SupervisorOwner>,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        io.verify_stop_lock(proof, lock, deadline)?;
        namespace.reverify(io, proof, deadline)?;
        claim.reverify(io, proof, deadline)?;
        let expected = claim.epoch(io)?;
        let mut record = preparing_matches(io, proof, &expected, deadline)?;
        let journal = super::super::service::journal::Journal::read(io, proof, deadline)?
            .ok_or(NativeError::Foreign)?;
        let agent = io.observe_agent(proof, deadline)?;
        let generation = io.agent_generation(&agent, proof, deadline)?;
        owner.child_for_peer(&agent, proof, deadline)?;
        if journal.current != Some(generation) {
            return Err(NativeError::Foreign);
        }
        record.bind(&journal)?;
        record.publish(io, proof, lock, deadline)?;
        let bound = super::activation::SupervisorLogonRecord::read(io, proof, deadline)?
            .ok_or(NativeError::Foreign)?;
        if bound != record {
            return Err(NativeError::Foreign);
        }
        bound.matches_current(&journal)?;
        claim.reverify(io, proof, deadline)?;
        namespace.reverify(io, proof, deadline)
    }
    /// Fixed-name observations only, under the caller's actual installer lock. Every slot is
    /// read positively; any remaining history prevents a missing-provenance "first" admission.
    #[cfg(not(test))]
    fn archive_history_present(
        io: &WindowsNativeIo,
        proof: &SupportProof,
        deadline: &Deadline,
    ) -> NativeResult<bool> {
        let mut present = false;
        for slot in 0..3 {
            present |= io
                .read_record(
                    proof,
                    records::RecordName::SupervisorEpoch(slot),
                    files::MAX_RECORD_BYTES,
                    deadline,
                )?
                .is_some();
        }
        Ok(present)
    }
    /// Shared exact slot observations; a nonterminal phase only requests more evidence.
    /// Actual fixed FileId/full bytes and native LSA disposition remain mandatory.
    #[cfg(not(test))]
    fn archive_slots(
        io: &Arc<WindowsNativeIo>,
        proof: &SupportProof,
        deadline: &Deadline,
    ) -> NativeResult<(u8, Option<super::super::payload::recovery::FileStamp>)> {
        let record = super::activation::SupervisorLogonRecord::read(io, proof, deadline)?;
        let mut slots: [Option<super::super::service::journal::Journal>; 3] = [None, None, None];
        let mut identities = [None, None, None];
        for slot in 0..3 {
            if let Some(observed) = io.read_record(
                proof,
                records::RecordName::SupervisorEpoch(slot as u8),
                files::MAX_RECORD_BYTES,
                deadline,
            )? {
                let journal = super::super::service::journal::Journal::decode(observed.bytes())?;
                let requirement = super::epoch_archive::history_requirement(&journal)?;
                let correlated = record.as_ref().and_then(|record| {
                    record
                        .history()
                        .iter()
                        .find(|entry| usize::from(entry.slot()) == slot)
                });
                if correlated.is_some()
                    || requirement
                        == super::epoch_archive::HistoryRequirement::PriorLogonDisposition
                {
                    let record = record.as_ref().ok_or_else(legacy_provenance_required)?;
                    let epoch = record
                        .matches_history(
                            slot as u8,
                            &journal,
                            epoch_stamp(observed.identity),
                            epoch_hash(observed.bytes()),
                        )?
                        .clone();
                    if requirement
                        == super::epoch_archive::HistoryRequirement::PriorLogonDisposition
                    {
                        let matched = MatchedLogonProvenance {
                            io: io.clone(),
                            epoch,
                        };
                        io.query_prior_logon(proof, &matched, deadline)?;
                    }
                }
                identities[slot] = Some(epoch_stamp(observed.identity));
                slots[slot] = Some(journal);
            }
        }
        let (slot, _) = super::epoch_archive::select_correlated_slots(&slots)?;
        Ok((slot, identities[usize::from(slot)]))
    }

    #[cfg(not(test))]
    struct NativeEpochArchive<'a> {
        io: &'a Arc<WindowsNativeIo>,
        proof: &'a SupportProof,
        lock: &'a InstallerLock,
        namespace: &'a Arc<super::supervisor_owner::ExclusiveSupervisorLease>,
        claim: EpochClaim<'a>,
        prior: &'a super::super::service::journal::Journal,
        source: &'a records::ObservedRecord,
        intent: &'a mut super::epoch_archive::ArchiveIntent,
        deadline: &'a Deadline,
    }
    #[cfg(not(test))]
    impl NativeEpochArchive<'_> {
        fn renew(&self) -> NativeResult<()> {
            self.claim.reverify(self.io, self.proof, self.deadline)?;
            self.namespace
                .reverify(self.io, self.proof, self.deadline)?;
            self.io
                .verify_stop_lock(self.proof, self.lock, self.deadline)
        }
    }
    #[cfg(not(test))]
    impl super::epoch_archive::ArchivePort for NativeEpochArchive<'_> {
        fn persist(&mut self, phase: super::epoch_archive::ArchivePhase) -> NativeResult<()> {
            self.renew()?;
            self.intent.phase = phase;
            publish_archive_intent(self.io, self.proof, self.lock, self.intent, self.deadline)
        }
        fn prune(&mut self) -> NativeResult<()> {
            self.renew()?;
            let context = self.io.context.clone();
            let held = self.lock.0.clone();
            let budget = self.proof.budget(self.io, self.deadline)?;
            let expected = epoch_identity(self.intent.victim.ok_or(NativeError::Foreign)?);
            let name = records::RecordName::SupervisorEpoch(self.intent.slot).file_name()?;
            let owner = self.io.owner.clone();
            self.io
                .owner
                .run(Dispatch::Mutation, self.deadline, move || {
                    let change = Change::new();
                    let result = change.finish((|| {
                        context.validate(&budget)?;
                        validate_payload_lock(&context, &held, &budget)?;
                        change.reached();
                        held.parent.delete_private_record(
                            &name,
                            expected,
                            &context.security,
                            &budget,
                        )
                    })());
                    if result == Err(NativeError::OutcomeUnknown) {
                        owner.retire_mutations();
                    }
                    result
                })
        }
        fn move_current(&mut self) -> NativeResult<()> {
            self.renew()?;
            let fresh = self
                .io
                .read_record(
                    self.proof,
                    records::RecordName::Supervisor,
                    files::MAX_RECORD_BYTES,
                    self.deadline,
                )?
                .ok_or(NativeError::Foreign)?;
            if fresh.identity != self.source.identity || fresh.bytes() != self.source.bytes() {
                return Err(NativeError::Foreign);
            }
            self.claim
                .renew_predecessor(self.io, self.proof, self.prior, self.deadline)?;
            let context = self.io.context.clone();
            let held = self.lock.0.clone();
            let budget = self.proof.budget(self.io, self.deadline)?;
            let expected = self.source.identity;
            let target = records::RecordName::SupervisorEpoch(self.intent.slot).file_name()?;
            let owner = self.io.owner.clone();
            self.io
                .owner
                .run(Dispatch::Mutation, self.deadline, move || {
                    let change = Change::new();
                    let result = change.finish((|| {
                        context.validate(&budget)?;
                        validate_payload_lock(&context, &held, &budget)?;
                        let parent = &held.parent;
                        let source = parent
                            .opaque("supervisor.json", false, &context.security, &budget)?
                            .ok_or(NativeError::Foreign)?;
                        if source.identity != expected {
                            return Err(NativeError::Foreign);
                        }
                        change.reached();
                        let identity = parent.move_opaque(
                            source,
                            parent,
                            target.as_str(),
                            &context.security,
                            &budget,
                        )?;
                        if identity != expected
                            || parent
                                .opaque("supervisor.json", false, &context.security, &budget)?
                                .is_some()
                        {
                            return Err(NativeError::OutcomeUnknown);
                        }
                        Ok(())
                    })());
                    if result == Err(NativeError::OutcomeUnknown) {
                        owner.retire_mutations();
                    }
                    result
                })
        }
        fn observe_complete(&mut self) -> NativeResult<()> {
            self.renew()?;
            let target = self
                .io
                .read_record(
                    self.proof,
                    records::RecordName::SupervisorEpoch(self.intent.slot),
                    files::MAX_RECORD_BYTES,
                    self.deadline,
                )?
                .ok_or(NativeError::OutcomeUnknown)?;
            if target.identity != self.source.identity
                || target.bytes() != self.source.bytes()
                || self
                    .io
                    .read_record(
                        self.proof,
                        records::RecordName::Supervisor,
                        files::MAX_RECORD_BYTES,
                        self.deadline,
                    )?
                    .is_some()
            {
                return Err(NativeError::OutcomeUnknown);
            }
            Ok(())
        }
    }
    #[cfg(test)]
    #[test]
    fn epoch_archive_actual_change_guard_classifies_shortened_budget_after_effect_unknown() {
        // Pure error classification only: no Context, native owner, path or OS call is opened.
        let before = Change::new();
        assert_eq!(
            before.finish::<()>(Err(NativeError::Timeout)),
            Err(NativeError::Timeout)
        );
        let after = Change::new();
        after.reached();
        assert_eq!(
            after.finish::<()>(Err(NativeError::Timeout)),
            Err(NativeError::OutcomeUnknown)
        );
        assert_eq!(
            after.finish::<()>(Err(NativeError::Unavailable)),
            Err(NativeError::OutcomeUnknown)
        );
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
    #[allow(dead_code)] // A4's held job adapter consumes these facts; ACK is submission only.
    pub(crate) enum ExitObservation {
        Running,
        Exited {
            creation: u64,
            code: u32,
            receipt: Option<crate::agent_contract::LastExitV1>,
        },
    }
    impl AgentObservationData {
        fn validate_pins(&self, context: &Context, budget: &Deadline) -> NativeResult<()> {
            self.runtime.revalidate(&context.security, true, budget)?;
            self.install.revalidate(&context.security, false, budget)?;
            if self.runtime.canonical_dos_path(&context.security, budget)? != self.runtime_canonical
            {
                return Err(NativeError::Foreign);
            }
            let facts = native::observe(&self.image, "crosspane-agent.exe", &context.security)?;
            files::admit_component(&facts, Admission::PrivateFile)?;
            let (_, current_image) = self.install.open_file_metadata(
                &PrivateName::new("crosspane-agent.exe")?,
                &context.security,
                budget,
            )?;
            if facts.identity != self.image_identity || current_image != self.image_identity {
                return Err(NativeError::Foreign);
            }
            budget.check()
        }
    }
    impl std::fmt::Debug for AgentObservation {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("AgentObservation")
        }
    }
    impl AgentObservation {
        /// Revalidates the same selected process before exposing its original creation fact.
        // A4b native supervisor generation uses the original retained-process creation fact.
        #[allow(dead_code)]
        pub(crate) fn creation_time(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<u64> {
            self.revalidate(io, proof, deadline)?;
            Ok(self.0.process.creation_time())
        }
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
        /// Fresh context and pins plus the original handle, including after original exit.
        /// A replacement bootstrap never becomes an original-process completion proof.
        pub(crate) fn observe_exit(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<ExitObservation> {
            let budget = proof.budget(io, deadline)?;
            if self.0.target != io.context.target.nonce {
                return Err(NativeError::Foreign);
            }
            let observed = self.0.clone();
            let context = io.context.clone();
            io.owner.run(Dispatch::Observation, deadline, move || {
                context.validate(&budget)?;
                observed.validate_pins(&context, &budget)?;
                match observed.process.observe_exit(&budget)? {
                    process::selected::ProcessExit::Running => Ok(ExitObservation::Running),
                    process::selected::ProcessExit::Exited { creation, code } => {
                        let receipt = with_state_exit_receipt(
                            context.target.paths.local(),
                            |state, leaf| {
                                // The retained runtime pins its ancestors; reopen ONLY its fixed
                                // state parent through the same no-follow private Anchor admission.
                                let state = Anchor::open(state, &context.security, true, &budget)?
                                    .ok_or(NativeError::Missing)?;
                                state.read_private(
                                    &PrivateName::new(leaf)?,
                                    &context.security,
                                    crate::agent_contract::MAX_RESPONSE_BYTES,
                                    &budget,
                                )
                            },
                        )?
                        .map(|(_, bytes)| {
                            let receipt = crate::agent_contract::parse_last_exit(&bytes)
                                .map_err(|_| NativeError::Invalid)?;
                            if receipt.instance_id != observed.bootstrap.instance_id
                                || receipt.stopped_unix_ms < observed.bootstrap.started_unix_ms
                            {
                                return Err(NativeError::Foreign);
                            }
                            Ok(receipt)
                        })
                        .transpose()?;
                        budget.check()?;
                        Ok(ExitObservation::Exited {
                            creation,
                            code,
                            receipt,
                        })
                    }
                }
            })
        }
        #[allow(dead_code)] // Lead750c0da2 holds the production owned-job adapter until A4.
        pub(crate) fn in_job(
            &self,
            io: &WindowsNativeIo,
            proof: &SupportProof,
            job: Arc<std::os::windows::io::OwnedHandle>,
            deadline: &Deadline,
        ) -> NativeResult<bool> {
            let budget = proof.budget(io, deadline)?;
            if self.0.target != io.context.target.nonce || job.as_raw_handle().is_null() {
                return Err(NativeError::Foreign);
            }
            let observed = self.0.clone();
            let context = io.context.clone();
            io.owner.run(Dispatch::Observation, deadline, move || {
                // Owns this exact Arc through actual completion: no duplicate, raw borrow, new
                // rights, name lookup or job authority introduced on the caller thread.
                context.validate(&budget)?;
                observed.validate_pins(&context, &budget)?;
                observed.process.in_job(&job, &budget)
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

    /// A5 private removal wiring. Serialized observations are never completion or mutation seals.
    #[cfg(not(test))]
    mod removal_io {
        use super::super::super::{
            removal::{
                self, RemovalCursor as Cursor, RemovalHandoffStage as Handoff, RemovalOptions,
                RemovalRecord,
                executor::NodePresence,
                inventory::{
                    RemovalCopy, RemovalCopyKind, RemovalInventory, RemovalNode, RemovalNodeKind,
                    RemovalTask,
                },
                plan::RemovalPlan,
            },
            service::{
                removal_runtime::RemovalCompletion,
                task::{Definition, Logon, RunLevel, SUPERVISOR_ARGUMENT, TASK_NAME},
            },
        };
        use super::*;
        use native::RemovalObjectKind as Kind;

        pub(crate) struct RemovalMutationPermit {
            io: Arc<WindowsNativeIo>,
            bytes: Vec<u8>,
            operation: [u8; 16],
        }
        impl RemovalMutationPermit {
            pub(crate) fn operation(&self) -> [u8; 16] {
                self.operation
            }
            pub(crate) fn bytes(&self) -> &[u8] {
                &self.bytes
            }
            fn document(&self) -> NativeResult<RemovalRecord> {
                RemovalRecord::decode(&self.bytes)
            }
            pub(crate) fn reverify(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                lock: &InstallerLock,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                if !std::ptr::eq(io, self.io.as_ref()) {
                    return Err(NativeError::Foreign);
                }
                io.verify_stop_lock(proof, lock, deadline)?;
                let current = io
                    .read_record(
                        proof,
                        records::RecordName::Removal,
                        files::MAX_RECORD_BYTES,
                        deadline,
                    )?
                    .ok_or(NativeError::Missing)?;
                if current.bytes() != self.bytes {
                    return Err(NativeError::Foreign);
                }
                context_matches(&self.document()?, io.target().identity())?;
                deadline.check()
            }
        }
        pub(crate) struct RemovalRoot {
            io: Arc<WindowsNativeIo>,
            programs: Arc<Anchor>,
            original: RemovalRecord,
        }
        fn definition(context: &Context) -> Definition {
            let user = context.target.identity.user.sddl();
            Definition {
                name: TASK_NAME.into(),
                principal: user.clone(),
                trigger_user: user,
                logon: Logon::InteractiveToken,
                run_level: RunLevel::Limited,
                action: format!(
                    "{}\\crosspane-installer.exe",
                    context.target.paths.install()
                ),
                arguments: SUPERVISOR_ARGUMENT.into(),
                working_directory: context.target.paths.install().into(),
                logon_trigger_only: true,
                ignore_new_instance: true,
                manager_restart_count: 0,
                enabled: true,
            }
        }
        fn read(
            context: &Context,
            lease: &LockState,
            budget: &Deadline,
        ) -> NativeResult<Option<(Vec<u8>, RemovalRecord)>> {
            validate_payload_lock(context, lease, budget)?;
            lease
                .parent
                .read_private(
                    &records::RecordName::Removal.file_name()?,
                    &context.security,
                    files::MAX_RECORD_BYTES,
                    budget,
                )?
                .map(|(_, bytes)| {
                    let record = RemovalRecord::decode(&bytes)?;
                    context_matches(&record, &context.target.identity)?;
                    Ok((bytes, record))
                })
                .transpose()
        }
        fn context_matches(record: &RemovalRecord, facts: &TokenFacts) -> NativeResult<()> {
            if matches!(
                record.cursor(),
                Cursor::Complete { .. }
                    | Cursor::FinalCopyCleanupIntent { .. }
                    | Cursor::FinalCopyAbsent { .. }
                    | Cursor::Retired
            ) {
                record.context().same_user(facts)
            } else {
                record.context().matches(facts)
            }
        }
        fn check(
            context: &Context,
            lease: &LockState,
            bytes: &[u8],
            budget: &Deadline,
        ) -> NativeResult<RemovalRecord> {
            let (fresh, record) = read(context, lease, budget)?.ok_or(NativeError::Missing)?;
            if fresh != bytes {
                return Err(NativeError::Foreign);
            }
            Ok(record)
        }
        fn exclusion(context: &Context, lease: &LockState, budget: &Deadline) -> NativeResult<()> {
            validate_payload_lock(context, lease, budget)?;
            let catalog = lease.parent.read_private(
                &records::RecordName::StageCatalog.file_name()?,
                &context.security,
                files::MAX_RECORD_BYTES,
                budget,
            )?;
            let active = if let Some((_, bytes)) = catalog {
                let catalog: super::super::super::payload::recovery::StageCatalog =
                    records::record_data(&records::RecordName::StageCatalog, &bytes)?;
                catalog.validate()?;
                catalog.active.is_some()
            } else {
                false
            };
            let outer = lease.parent.read_private(
                &records::RecordName::OuterUpgrade.file_name()?,
                &context.security,
                files::MAX_RECORD_BYTES,
                budget,
            )?;
            let outer_active = if let Some((_, bytes)) = outer {
                use super::super::super::payload::recovery::{OuterPhase, OuterUpgradeRecord};
                !matches!(
                    OuterUpgradeRecord::decode(&bytes)?.phase(),
                    OuterPhase::Complete | OuterPhase::Cancelled
                )
            } else {
                false
            };
            let progress = lease.parent.read_private(
                &records::RecordName::FileRecovery.file_name()?,
                &context.security,
                files::MAX_RECORD_BYTES,
                budget,
            )?;
            let recovery_active = if let Some((_, bytes)) = progress {
                use super::super::super::payload::recovery::{
                    FileRecoveryCursor, FileRecoveryJournal,
                };
                FileRecoveryJournal::decode(&bytes)?.cursor() != FileRecoveryCursor::Retired
            } else {
                false
            };
            removal::admit_removal_selection(active || outer_active || recovery_active)
        }
        fn plan(
            context: &Context,
            lease: &LockState,
            programs: &Anchor,
            budget: &Deadline,
        ) -> NativeResult<RemovalPlan> {
            exclusion(context, lease, budget)?;
            let observed = programs.observe_removal_install(&context.security, budget)?;
            let nodes = observed
                .nodes
                .into_iter()
                .map(|n| {
                    RemovalNode::new(
                        n.components,
                        n.parent.into(),
                        n.identity.into(),
                        match n.kind {
                            Kind::File => RemovalNodeKind::File,
                            Kind::Directory => RemovalNodeKind::Directory,
                            Kind::ReparseLink => RemovalNodeKind::Reparse,
                        },
                    )
                })
                .collect::<NativeResult<Vec<_>>>()?;
            let scheduler = super::super::task::Scheduler::connect(&|| {
                validate_payload_lock(context, lease, budget)
            })?;
            let (expected, actual) = scheduler.inspect_removal(&definition(context), &|| {
                validate_payload_lock(context, lease, budget)
            })?;
            RemovalPlan::new(RemovalInventory::new(
                observed.root.map(Into::into),
                nodes,
                RemovalTask::new(expected, actual)?,
                vec![RemovalCopy::planned(RemovalCopyKind::Keeper)],
            )?)
        }
        fn identity(stamp: super::super::super::payload::recovery::FileStamp) -> FileIdentity {
            FileIdentity {
                volume: stamp.volume,
                file: stamp.file,
            }
        }
        fn ancestors(record: &RemovalRecord, index: u16) -> NativeResult<Vec<FileIdentity>> {
            let node = record.plan().node(index)?;
            let mut ids = vec![identity(record.plan().root().ok_or(NativeError::Foreign)?)];
            for depth in 1..node.components().len() {
                let parent = record
                    .plan()
                    .order()
                    .iter()
                    .map(|i| record.plan().node(*i))
                    .collect::<NativeResult<Vec<_>>>()?
                    .into_iter()
                    .find(|n| {
                        n.components() == &node.components()[..depth]
                            && n.kind() == RemovalNodeKind::Directory
                    })
                    .ok_or(NativeError::Foreign)?;
                ids.push(identity(parent.identity()));
            }
            Ok(ids)
        }
        fn kind(kind: RemovalNodeKind) -> Kind {
            match kind {
                RemovalNodeKind::File => Kind::File,
                RemovalNodeKind::Directory => Kind::Directory,
                RemovalNodeKind::Reparse => Kind::ReparseLink,
            }
        }
        impl RemovalRoot {
            pub(crate) fn reverify(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                lock: &InstallerLock,
                permit: &RemovalMutationPermit,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                if !std::ptr::eq(io, self.io.as_ref()) {
                    return Err(NativeError::Foreign);
                }
                permit.reverify(io, proof, lock, deadline)?;
                self.original.same_selection(&permit.document()?)?;
                let root = self.programs.clone();
                let context = io.context.clone();
                let budget = proof.budget(io, deadline)?;
                io.owner.run(Dispatch::Observation, deadline, move || {
                    root.revalidate(&context.security, false, &budget)
                })
            }
            pub(crate) fn task_absent(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                lock: &InstallerLock,
                permit: &RemovalMutationPermit,
                deadline: &Deadline,
            ) -> NativeResult<bool> {
                self.reverify(io, proof, lock, permit, deadline)?;
                let context = io.context.clone();
                let lease = lock.0.clone();
                let bytes = permit.bytes.clone();
                let budget = proof.budget(io, deadline)?;
                io.owner.run(Dispatch::Observation, deadline, move || {
                    let record = check(&context, &lease, &bytes, &budget)?;
                    let scheduler = super::super::task::Scheduler::connect(&|| {
                        check(&context, &lease, &bytes, &budget).map(|_| ())
                    })?;
                    let (expected, actual) = scheduler
                        .inspect_removal(&definition(&context), &|| {
                            check(&context, &lease, &bytes, &budget).map(|_| ())
                        })?;
                    if expected != record.plan().task().expected_xml() {
                        return Err(NativeError::Foreign);
                    }
                    Ok(actual.is_none())
                })
            }
            pub(crate) fn delete_task(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                lock: &InstallerLock,
                permit: &RemovalMutationPermit,
                completion: &RemovalCompletion,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                self.reverify(io, proof, lock, permit, deadline)?;
                let record = permit.document()?;
                if record.cursor() != Cursor::TaskDeleteIntent {
                    return Err(NativeError::Foreign);
                }
                completion.reverify(io, proof, lock, record.operation(), deadline)?;
                let tree = completion.tree().clone();
                let context = io.context.clone();
                let lease = lock.0.clone();
                let bytes = permit.bytes.clone();
                let budget = proof.budget(io, deadline)?;
                io.owner.run(Dispatch::Mutation, deadline, move || {
                    let renew = || {
                        check(&context, &lease, &bytes, &budget)?;
                        tree.reverify(&budget)
                    };
                    renew()?;
                    let scheduler = super::super::task::Scheduler::connect(&renew)?;
                    let change = Change::new();
                    // A shorter support budget can expire after COM touched the task. Preserve
                    // may-have-mutated through every following check/absence observation; the
                    // existing CallOwner retires mutations before delivering OutcomeUnknown.
                    change.finish(scheduler.delete_removal(
                        &definition(&context),
                        record.plan().task().expected_xml(),
                        &renew,
                        &|| change.reached(),
                    ))
                })
            }
            pub(crate) fn observe_node(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                lock: &InstallerLock,
                permit: &RemovalMutationPermit,
                index: u16,
                deadline: &Deadline,
            ) -> NativeResult<NodePresence> {
                self.reverify(io, proof, lock, permit, deadline)?;
                let record = permit.document()?;
                let node = record.plan().node(index)?.clone();
                let parents = ancestors(&record, index)?;
                let root = self.programs.clone();
                let context = io.context.clone();
                let lease = lock.0.clone();
                let bytes = permit.bytes.clone();
                let budget = proof.budget(io, deadline)?;
                io.owner.run(Dispatch::Observation, deadline, move || {
                    check(&context, &lease, &bytes, &budget)?;
                    Ok(
                        match root.observe_removal_node(
                            parents[0],
                            node.components(),
                            &parents,
                            &context.security,
                            &budget,
                        )? {
                            None => NodePresence::Absent,
                            Some((id, k))
                                if id == identity(node.identity()) && k == kind(node.kind()) =>
                            {
                                NodePresence::Same
                            }
                            Some(_) => NodePresence::Changed,
                        },
                    )
                })
            }
            // Independent current IO, lock, publication, selected node and retained-tree seals.
            #[allow(clippy::too_many_arguments)]
            pub(crate) fn delete_node(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                lock: &InstallerLock,
                permit: &RemovalMutationPermit,
                index: u16,
                completion: &RemovalCompletion,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                self.reverify(io, proof, lock, permit, deadline)?;
                let record = permit.document()?;
                if record.cursor() != (Cursor::DeleteIntent { index }) {
                    return Err(NativeError::Foreign);
                }
                completion.reverify(io, proof, lock, record.operation(), deadline)?;
                let tree = completion.tree().clone();
                let node = record.plan().node(index)?.clone();
                let parents = ancestors(&record, index)?;
                let root = self.programs.clone();
                let context = io.context.clone();
                let lease = lock.0.clone();
                let bytes = permit.bytes.clone();
                let budget = proof.budget(io, deadline)?;
                io.owner.run(Dispatch::Mutation, deadline, move || {
                    check(&context, &lease, &bytes, &budget)?;
                    tree.reverify(&budget)?;
                    let change = Change::new();
                    change.reached();
                    change.finish(root.delete_removal_node(
                        parents[0],
                        node.components(),
                        &parents,
                        identity(node.parent()),
                        identity(node.identity()),
                        kind(node.kind()),
                        &context.security,
                        &budget,
                    ))
                })
            }
            pub(crate) fn root_absent(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                lock: &InstallerLock,
                permit: &RemovalMutationPermit,
                deadline: &Deadline,
            ) -> NativeResult<bool> {
                self.reverify(io, proof, lock, permit, deadline)?;
                let record = permit.document()?;
                let context = io.context.clone();
                let lease = lock.0.clone();
                let bytes = permit.bytes.clone();
                let root = self.programs.clone();
                let budget = proof.budget(io, deadline)?;
                io.owner.run(Dispatch::Observation, deadline, move || {
                    check(&context, &lease, &bytes, &budget)?;
                    let fresh = root.removal_install_identity(&context.security, &budget)?;
                    if fresh.is_some() && fresh != record.plan().root().map(identity) {
                        return Err(NativeError::Foreign);
                    }
                    Ok(fresh.is_none())
                })
            }
            pub(crate) fn delete_root(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                lock: &InstallerLock,
                permit: &RemovalMutationPermit,
                completion: &RemovalCompletion,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                self.reverify(io, proof, lock, permit, deadline)?;
                let record = permit.document()?;
                if record.cursor() != Cursor::RootDeleteIntent {
                    return Err(NativeError::Foreign);
                }
                completion.reverify(io, proof, lock, record.operation(), deadline)?;
                let tree = completion.tree().clone();
                let root = self.programs.clone();
                let context = io.context.clone();
                let lease = lock.0.clone();
                let bytes = permit.bytes.clone();
                let budget = proof.budget(io, deadline)?;
                io.owner.run(Dispatch::Mutation, deadline, move || {
                    check(&context, &lease, &bytes, &budget)?;
                    tree.reverify(&budget)?;
                    if let Some(id) = record.plan().root() {
                        let change = Change::new();
                        change.reached();
                        change.finish(root.delete_removal_install_root(
                            identity(id),
                            &context.security,
                            &budget,
                        ))?;
                    } else if root
                        .removal_install_identity(&context.security, &budget)?
                        .is_some()
                    {
                        return Err(NativeError::Foreign);
                    }
                    Ok(())
                })
            }
        }
        impl WindowsNativeIo {
            pub(crate) fn read_removal(
                &self,
                proof: &SupportProof,
                deadline: &Deadline,
            ) -> NativeResult<Option<RemovalRecord>> {
                self.read_record(
                    proof,
                    records::RecordName::Removal,
                    files::MAX_RECORD_BYTES,
                    deadline,
                )?
                .map(|record| {
                    let document = RemovalRecord::decode(record.bytes())?;
                    context_matches(&document, self.target().identity())?;
                    Ok(document)
                })
                .transpose()
            }
            pub(crate) fn prepare_removal(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                options: RemovalOptions,
                deadline: &Deadline,
            ) -> NativeResult<(RemovalRoot, RemovalRecord)> {
                self.verify_stop_lock(proof, lock, deadline)?;
                let context = self.context.clone();
                let lease = lock.0.clone();
                let budget = proof.budget(self, deadline)?;
                let (programs,record)=self.owner.run(Dispatch::Observation,deadline,move||{
                    exclusion(&context,&lease,&budget)?;
                    let programs=Arc::new(Anchor::open(context.target.paths.programs(),&context.security,false,&budget)?
                        .ok_or(NativeError::Missing)?);
                    let record=if let Some((_,old))=read(&context,&lease,&budget)? {
                        if old.cursor()!=Cursor::Retired {if old.options()!=options{return Err(NativeError::Foreign)}old}
                        else{RemovalRecord::new(nonce()?,super::super::super::payload::recovery::OuterContextCorrelation::new(&context.target.identity)?,
                            options,plan(&context,&lease,&programs,&budget)?)?}
                    }else{RemovalRecord::new(nonce()?,super::super::super::payload::recovery::OuterContextCorrelation::new(&context.target.identity)?,
                        options,plan(&context,&lease,&programs,&budget)?)?};
                    Ok((programs,record))
                })?;
                Ok((
                    RemovalRoot {
                        io: self.clone(),
                        programs,
                        original: record.clone(),
                    },
                    record,
                ))
            }
            /// Cold terminal FILE-only selection. No new operation, staging, Stop, erase,
            /// image approval or tree authority is created from the recorded plan.
            pub(crate) fn reopen_removal_root(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                permit: &RemovalMutationPermit,
                deadline: &Deadline,
            ) -> NativeResult<RemovalRoot> {
                permit.reverify(self, proof, lock, deadline)?;
                let original = permit.document()?;
                if !matches!(
                    original.cursor(),
                    Cursor::Complete { .. }
                        | Cursor::FinalCopyCleanupIntent { .. }
                        | Cursor::FinalCopyAbsent { .. }
                        | Cursor::Retired
                ) {
                    return Err(NativeError::Foreign);
                }
                let context = self.context.clone();
                let lease = lock.0.clone();
                let bytes = permit.bytes.clone();
                let budget = proof.budget(self, deadline)?;
                let programs = self.owner.run(Dispatch::Observation, deadline, move || {
                    check(&context, &lease, &bytes, &budget)?;
                    let programs = Anchor::open(
                        context.target.paths.programs(),
                        &context.security,
                        false,
                        &budget,
                    )?
                    .ok_or(NativeError::Missing)?;
                    budget.check()?;
                    Ok(Arc::new(programs))
                })?;
                Ok(RemovalRoot {
                    io: self.clone(),
                    programs,
                    original,
                })
            }
            pub(crate) fn admit_removal_permit(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                deadline: &Deadline,
            ) -> NativeResult<RemovalMutationPermit> {
                self.verify_stop_lock(proof, lock, deadline)?;
                let record = self
                    .read_record(
                        proof,
                        records::RecordName::Removal,
                        files::MAX_RECORD_BYTES,
                        deadline,
                    )?
                    .ok_or(NativeError::Missing)?;
                let document = RemovalRecord::decode(record.bytes())?;
                context_matches(&document, self.target().identity())?;
                Ok(RemovalMutationPermit {
                    io: self.clone(),
                    bytes: record.bytes().to_vec(),
                    operation: document.operation(),
                })
            }
            pub(crate) fn publish_removal(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                record: &RemovalRecord,
                deadline: &Deadline,
            ) -> NativeResult<RemovalMutationPermit> {
                self.verify_stop_lock(proof, lock, deadline)?;
                record.validate()?;
                context_matches(record, self.target().identity())?;
                let context = self.context.clone();
                let lease = lock.0.clone();
                let budget = proof.budget(self, deadline)?;
                let requested = record.clone();
                let bytes = record.encode()?;
                let output = bytes.clone();
                let owner = self.owner.clone();
                self.owner.run(Dispatch::Mutation, deadline, move || {
                    exclusion(&context, &lease, &budget)?;
                    match read(&context, &lease, &budget)? {
                        Some((_, old)) if old.cursor() != Cursor::Retired => {
                            // Shared production matcher covers cursor and handoff stage in this record: a stale preparation
                            // cannot regress Ready/commit even while cursor remains Selected.
                            old.publication_successor(&requested)?;
                        }
                        _ => {
                            if requested.cursor() != Cursor::Selected
                                || requested.handoff_stage() != Handoff::None
                            {
                                return Err(NativeError::Foreign);
                            }
                            let programs = Anchor::open(
                                context.target.paths.programs(),
                                &context.security,
                                false,
                                &budget,
                            )?
                            .ok_or(NativeError::Missing)?;
                            plan(&context, &lease, &programs, &budget)?
                                .same_files(requested.plan())?;
                        }
                    }
                    let mut store = NativeStore {
                        context,
                        lease,
                        budget,
                        name: records::RecordName::Removal,
                        change: Change::new(),
                    };
                    let budget = store.budget.clone();
                    let result = records::publish(&mut store, &bytes, &budget);
                    let result = store.change.finish(result)?;
                    if result.native_failure.is_some()
                        || result.state != records::PublicationRecovery::NewPublished
                    {
                        owner.retire_mutations();
                        return Err(NativeError::OutcomeUnknown);
                    }
                    Ok(())
                })?;
                Ok(RemovalMutationPermit {
                    io: self.clone(),
                    bytes: output,
                    operation: record.operation(),
                })
            }
        }

        fn copy_parent(
            context: &Context,
            operation: [u8; 16],
            create: bool,
            budget: &Deadline,
            change: &Change,
        ) -> NativeResult<Option<Arc<Anchor>>> {
            context.validate(budget)?;
            if operation == [0; 16] {
                return Err(NativeError::Invalid);
            }
            if create {
                let state =
                    ensure_payload_child(&context.local, "Crosspane", context, budget, change)?;
                let runtime = ensure_payload_child(&state, "runtime", context, budget, change)?;
                let removal = ensure_payload_child(&runtime, "removal", context, budget, change)?;
                return Ok(Some(Arc::new(ensure_payload_child(
                    &removal,
                    &records::hex(&operation),
                    context,
                    budget,
                    change,
                )?)));
            }
            Anchor::open(
                &format!(
                    "{}\\Crosspane\\runtime\\removal\\{}",
                    context.target.paths.local(),
                    records::hex(&operation)
                ),
                &context.security,
                true,
                budget,
            )
            .map(|root| root.map(Arc::new))
        }
        fn copy(record: &RemovalRecord, index: u8) -> NativeResult<&RemovalCopy> {
            record
                .plan()
                .copies()
                .get(usize::from(index))
                .ok_or(NativeError::Invalid)
        }
        fn peer_phase(record: &RemovalRecord) -> NativeResult<()> {
            if record.cursor() == Cursor::StopIntent {
                return Ok(());
            }
            if matches!(record.cursor(), Cursor::Selected | Cursor::Committed)
                && matches!(
                    record.handoff_stage(),
                    Handoff::Created { .. }
                        | Handoff::ResumeIntent { .. }
                        | Handoff::Ready { .. }
                        | Handoff::RemovalCommitIntent { .. }
                        | Handoff::Committed { .. }
                )
            {
                return Ok(());
            }
            Err(NativeError::Foreign)
        }
        /// A measured fixed copy observation only: it cannot be converted into image approval.
        pub(crate) struct RemovalPeerImage(Arc<RemovalPeerData>);
        struct RemovalPeerData {
            target: [u8; 16],
            original: RemovalRecord,
            index: u8,
            parent: Arc<Anchor>,
            image: native::ImageData,
        }
        impl RemovalPeerImage {
            pub(crate) fn identity(&self) -> FileIdentity {
                self.0.image.identity
            }
            pub(crate) fn facts(&self) -> &PeFacts {
                &self.0.image.facts
            }
            pub(crate) fn canonical_dos_path(&self) -> &str {
                &self.0.image.canonical
            }
            pub(crate) fn reverify(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                if self.0.target != io.context.target.nonce {
                    return Err(NativeError::Foreign);
                }
                let current = io
                    .read_removal(proof, deadline)?
                    .ok_or(NativeError::Missing)?;
                self.0.original.same_selection(&current)?;
                let expected = copy(&current, self.0.index)?;
                if expected.identity() != Some(self.identity().into())
                    || expected.image() != Some(self.facts())
                {
                    return Err(NativeError::Foreign);
                }
                let pin = self.0.clone();
                let context = io.context.clone();
                let budget = proof.budget(io, deadline)?;
                io.owner.run(Dispatch::Observation, deadline, move || {
                    context.validate(&budget)?;
                    pin.parent.revalidate(&context.security, true, &budget)?;
                    let fresh = pin.parent.open_image(
                        copy(&pin.original, pin.index)?.kind().leaf(),
                        true,
                        &pin.image.facts.version,
                        &context.security,
                        &budget,
                    )?;
                    if fresh.identity != pin.image.identity || fresh.facts != pin.image.facts {
                        return Err(NativeError::Foreign);
                    }
                    budget.check()
                })
            }
        }
        pub(crate) struct RemovalKeeperSelection {
            io: Arc<WindowsNativeIo>,
            module: SelfImagePin,
            own: process::own::OwnProcessIdentity,
            image: RemovalPeerImage,
            record: RemovalRecord,
            index: u8,
        }
        impl RemovalKeeperSelection {
            pub(crate) fn operation(&self) -> [u8; 16] {
                self.record.operation()
            }
            pub(crate) fn record(&self) -> &RemovalRecord {
                &self.record
            }
            pub(crate) fn index(&self) -> u8 {
                self.index
            }
            pub(crate) fn module(&self) -> &SelfImagePin {
                &self.module
            }
            pub(crate) fn reverify(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                if !std::ptr::eq(io, self.io.as_ref()) {
                    return Err(NativeError::Foreign);
                }
                self.module.reverify(io, proof, deadline)?;
                self.own.reverify(deadline)?;
                self.image.reverify(io, proof, deadline)?;
                let current = io
                    .read_removal(proof, deadline)?
                    .ok_or(NativeError::Missing)?;
                self.record.same_selection(&current)?;
                let expected = copy(&current, self.index)?;
                if expected.pid() != Some(self.own.pid())
                    || expected.creation() != Some(self.own.creation())
                    || self.module.identity() != self.image.identity()
                    || self.module.facts() != self.image.facts()
                    || process::literal_path(self.module.canonical_dos_path())?
                        != process::literal_path(self.image.canonical_dos_path())?
                {
                    return Err(NativeError::Foreign);
                }
                deadline.check()
            }
        }
        pub(crate) struct RemovalCompletionAdmission {
            selection: RemovalKeeperSelection,
        }
        impl RemovalCompletionAdmission {
            pub(crate) fn operation(&self) -> [u8; 16] {
                self.selection.operation()
            }
            pub(crate) fn reverify(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                self.selection.reverify(io, proof, deadline)?;
                let current = io
                    .read_removal(proof, deadline)?
                    .ok_or(NativeError::Missing)?;
                if current.operation() != self.operation() || current.cursor() != Cursor::StopIntent
                {
                    return Err(NativeError::Foreign);
                }
                current.context().matches(io.target().identity())?;
                deadline.check()
            }
        }
        impl WindowsNativeIo {
            pub(crate) fn prepare_removal_copy(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                permit: &RemovalMutationPermit,
                index: u8,
                own: &SelfImagePin,
                deadline: &Deadline,
            ) -> NativeResult<OpenedPe> {
                permit.reverify(self, proof, lock, deadline)?;
                own.reverify(self, proof, deadline)?;
                let record = permit.document()?;
                if record.cursor() != Cursor::Selected
                    || record.handoff_stage() != (Handoff::CopyPrepareIntent { index })
                    || copy(&record, index)?.identity().is_some()
                {
                    return Err(NativeError::Foreign);
                }
                let expected = ApprovedPe::own_image(own)?;
                let input = self.self_image_reader(own, proof, deadline)?;
                let context = self.context.clone();
                let lease = lock.0.clone();
                let bytes = permit.bytes.clone();
                let budget = proof.budget(self, deadline)?;
                self.owner.run(Dispatch::Mutation, deadline, move || {
                    let change = Change::new();
                    change.finish((|| {
                        let fresh = check(&context, &lease, &bytes, &budget)?;
                        let parent =
                            copy_parent(&context, fresh.operation(), true, &budget, &change)?
                                .ok_or(NativeError::Missing)?;
                        let leaf = copy(&fresh, index)?.kind().leaf();
                        // CREATE_NEW only. An interrupted unknown copy is retained, never adopted.
                        change.reached();
                        let image = parent.stage_image(
                            leaf,
                            input,
                            &expected,
                            &context.security,
                            &budget,
                        )?;
                        Ok(OpenedPe(Arc::new(ApprovedImage {
                            target: context.target.nonce,
                            parent,
                            leaf: leaf.into(),
                            image,
                            expected,
                        })))
                    })())
                })
            }
            fn pin_removal_index(
                &self,
                proof: &SupportProof,
                record: &RemovalRecord,
                index: u8,
                deadline: &Deadline,
            ) -> NativeResult<RemovalPeerImage> {
                record.context().matches(self.target().identity())?;
                let selected = copy(record, index)?;
                let expected = selected.image().ok_or(NativeError::Missing)?.clone();
                let expected_id = selected.identity().ok_or(NativeError::Missing)?;
                let original = record.clone();
                let context = self.context.clone();
                let budget = proof.budget(self, deadline)?;
                self.owner.run(Dispatch::Observation, deadline, move || {
                    context.validate(&budget)?;
                    let parent = copy_parent(
                        &context,
                        original.operation(),
                        false,
                        &budget,
                        &Change::new(),
                    )?
                    .ok_or(NativeError::Missing)?;
                    let image = parent.open_image(
                        copy(&original, index)?.kind().leaf(),
                        true,
                        &expected.version,
                        &context.security,
                        &budget,
                    )?;
                    if image.identity != identity(expected_id) || image.facts != expected {
                        return Err(NativeError::Foreign);
                    }
                    Ok(RemovalPeerImage(Arc::new(RemovalPeerData {
                        target: context.target.nonce,
                        original,
                        index,
                        parent,
                        image,
                    })))
                })
            }
            pub(crate) fn select_removal_keeper(
                self: &Arc<Self>,
                proof: &SupportProof,
                deadline: &Deadline,
            ) -> NativeResult<RemovalKeeperSelection> {
                let record = self
                    .read_removal(proof, deadline)?
                    .ok_or(NativeError::Missing)?;
                peer_phase(&record)?;
                let module = self.self_image(proof, deadline)?;
                let own = self.own_process_identity(proof, deadline)?;
                let index = record
                    .plan()
                    .copies()
                    .iter()
                    .position(|copy| {
                        copy.pid() == Some(own.pid())
                            && copy.creation() == Some(own.creation())
                            && copy.identity() == Some(module.identity().into())
                            && copy.image() == Some(module.facts())
                    })
                    .ok_or(NativeError::Foreign)? as u8;
                let image = self.pin_removal_index(proof, &record, index, deadline)?;
                let result = RemovalKeeperSelection {
                    io: self.clone(),
                    module,
                    own,
                    image,
                    record,
                    index,
                };
                result.reverify(self, proof, deadline)?;
                Ok(result)
            }
            pub(crate) fn prepare_removal_completion(
                self: &Arc<Self>,
                proof: &SupportProof,
                lease: &StopLockLease,
                operation: [u8; 16],
                deadline: &Deadline,
            ) -> NativeResult<RemovalCompletionAdmission> {
                if !std::ptr::eq(self.as_ref(), lease.io.as_ref()) {
                    return Err(NativeError::Foreign);
                }
                lease.reverify(proof, deadline)?;
                let selection = self.select_removal_keeper(proof, deadline)?;
                if selection.operation() != operation {
                    return Err(NativeError::Foreign);
                }
                let result = RemovalCompletionAdmission { selection };
                result.reverify(self, proof, deadline)?;
                Ok(result)
            }
            pub(crate) fn pin_removal_peer(
                &self,
                proof: &SupportProof,
                operation: [u8; 16],
                peer: &super::super::supervisor_owner::KernelOuterPeer,
                deadline: &Deadline,
            ) -> NativeResult<RemovalPeerImage> {
                peer.reverify(self, proof, deadline)?;
                let record = self
                    .read_removal(proof, deadline)?
                    .ok_or(NativeError::Missing)?;
                peer_phase(&record)?;
                if record.operation() != operation {
                    return Err(NativeError::Foreign);
                }
                record.context().matches(peer.token())?;
                let index = record
                    .plan()
                    .copies()
                    .iter()
                    .position(|copy| {
                        copy.pid() == Some(peer.pid()) && copy.creation() == Some(peer.creation())
                    })
                    .ok_or(NativeError::Foreign)? as u8;
                let image = self.pin_removal_index(proof, &record, index, deadline)?;
                if process::literal_path(peer.image())?
                    != process::literal_path(image.canonical_dos_path())?
                {
                    return Err(NativeError::Foreign);
                }
                peer.reverify(self, proof, deadline)?;
                image.reverify(self, proof, deadline)?;
                Ok(image)
            }
        }

        fn copy_presence(
            context: &Context,
            record: &RemovalRecord,
            index: u8,
            budget: &Deadline,
        ) -> NativeResult<bool> {
            let selected = copy(record, index)?;
            let id = selected.identity().ok_or(NativeError::Missing)?;
            let parent = copy_parent(context, record.operation(), false, budget, &Change::new())?;
            let Some(parent) = parent else {
                return Ok(true);
            };
            let observed =
                parent.removal_copy_identity(selected.kind().leaf(), &context.security, budget)?;
            let Some(observed) = observed else {
                return Ok(true);
            };
            if observed != identity(id) {
                return Err(NativeError::Foreign);
            }
            Ok(false)
        }
        fn original_copy_exited(
            process: &std::os::windows::io::OwnedHandle,
            record: &RemovalRecord,
            index: u8,
            budget: &Deadline,
        ) -> NativeResult<()> {
            use windows_sys::Win32::{
                Foundation::{FILETIME, WAIT_OBJECT_0},
                System::Threading::*,
            };
            let selected = copy(record, index)?;
            budget.check()?;
            let mut created = FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            };
            let mut exit = created;
            let mut kernel = created;
            let mut user = created;
            // SAFETY: actual retained original child/kernel-peer handle from a genuine exit seal;
            // query/wait only, no OpenProcess, PID/name selection, termination or inherited claims.
            let ok = unsafe {
                GetProcessTimes(
                    process.as_raw_handle(),
                    &mut created,
                    &mut exit,
                    &mut kernel,
                    &mut user,
                )
            };
            // SAFETY: read-only identity and nonblocking wait on that same retained native object.
            let (pid, wait) = unsafe {
                (
                    GetProcessId(process.as_raw_handle()),
                    WaitForSingleObject(process.as_raw_handle(), 0),
                )
            };
            let creation =
                (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
            if ok == 0
                || wait != WAIT_OBJECT_0
                || selected.pid() != Some(pid)
                || selected.creation() != Some(creation)
            {
                return Err(NativeError::Foreign);
            }
            budget.check()
        }
        fn namespace_held(
            handle: &std::os::windows::io::OwnedHandle,
            budget: &Deadline,
        ) -> NativeResult<()> {
            let mut flags = 0;
            budget.check()?;
            // SAFETY: actual exclusively created FIRST_PIPE_INSTANCE namespace handle, retained
            // from the sealed current lease; this read neither selects nor reconstructs an owner.
            if unsafe {
                windows_sys::Win32::Foundation::GetHandleInformation(
                    handle.as_raw_handle(),
                    &mut flags,
                )
            } == 0
            {
                return Err(NativeError::Foreign);
            }
            budget.check()
        }
        impl WindowsNativeIo {
            pub(crate) fn read_removal_receipt(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                permit: &RemovalMutationPermit,
                completion: &RemovalCompletion,
                deadline: &Deadline,
            ) -> NativeResult<Option<crate::agent_contract::LastExitV1>> {
                permit.reverify(self, proof, lock, deadline)?;
                completion.reverify(self, proof, lock, permit.operation(), deadline)?;
                let generation = completion.generation();
                let started = completion.started_unix_ms();
                let tree = completion.tree().clone();
                let context = self.context.clone();
                let lease = lock.0.clone();
                let bytes = permit.bytes.clone();
                let budget = proof.budget(self, deadline)?;
                self.owner.run(Dispatch::Observation, deadline, move || {
                    check(&context, &lease, &bytes, &budget)?;
                    tree.reverify(&budget)?;
                    let receipt =
                        with_state_exit_receipt(context.target.paths.local(), |state, leaf| {
                            let Some(parent) =
                                Anchor::open(state, &context.security, true, &budget)?
                            else {
                                return Ok(None);
                            };
                            parent.read_private(
                                &PrivateName::new(leaf)?,
                                &context.security,
                                crate::agent_contract::MAX_RESPONSE_BYTES,
                                &budget,
                            )
                        })?;
                    receipt
                        .map(|(_, bytes)| {
                            let parsed = crate::agent_contract::parse_last_exit(&bytes)
                                .map_err(|_| NativeError::Invalid)?;
                            if parsed.instance_id != generation.instance
                                || parsed.stopped_unix_ms < started
                            {
                                return Err(NativeError::Foreign);
                            }
                            tree.reverify(&budget)?;
                            budget.check()?;
                            Ok(parsed)
                        })
                        .transpose()
                })
            }
            pub(crate) fn executing_removal_copy(
                &self,
                proof: &SupportProof,
                permit: &RemovalMutationPermit,
                deadline: &Deadline,
            ) -> NativeResult<Option<u8>> {
                if !std::ptr::eq(self, permit.io.as_ref()) {
                    return Err(NativeError::Foreign);
                }
                let current = self
                    .read_removal(proof, deadline)?
                    .ok_or(NativeError::Missing)?;
                if current.encode()? != permit.bytes {
                    return Err(NativeError::Foreign);
                }
                let module = self.self_image(proof, deadline)?;
                let own = self.own_process_identity(proof, deadline)?;
                let found = current
                    .plan()
                    .copies()
                    .iter()
                    .enumerate()
                    .find(|(_, copy)| {
                        copy.identity() == Some(module.identity().into())
                            && copy.image() == Some(module.facts())
                            && copy.pid() == Some(own.pid())
                            && copy.creation() == Some(own.creation())
                    })
                    .map(|(i, _)| i as u8);
                own.reverify(deadline)?;
                module.reverify(self, proof, deadline)?;
                Ok(found)
            }
            // Original IO, lock, exact journal/index and retained exited process are independent.
            #[allow(clippy::too_many_arguments)]
            pub(crate) fn removal_copy_absent(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                permit: &RemovalMutationPermit,
                index: u8,
                exit: &super::super::super::payload::helper::removal::RemovalCopyExit,
                deadline: &Deadline,
            ) -> NativeResult<bool> {
                permit.reverify(self, proof, lock, deadline)?;
                let record = permit.document()?;
                let id = identity(
                    copy(&record, index)?
                        .identity()
                        .ok_or(NativeError::Missing)?,
                );
                exit.reverify(self, proof, record.operation(), index, id, deadline)?;
                let process = exit.retained_process().clone();
                let context = self.context.clone();
                let lease = lock.0.clone();
                let bytes = permit.bytes.clone();
                let budget = proof.budget(self, deadline)?;
                self.owner.run(Dispatch::Observation, deadline, move || {
                    let record = check(&context, &lease, &bytes, &budget)?;
                    original_copy_exited(&process, &record, index, &budget)?;
                    copy_presence(&context, &record, index, &budget)
                })
            }
            #[allow(clippy::too_many_arguments)]
            pub(crate) fn retire_removal_copy(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                permit: &RemovalMutationPermit,
                index: u8,
                exit: &super::super::super::payload::helper::removal::RemovalCopyExit,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                permit.reverify(self, proof, lock, deadline)?;
                let record = permit.document()?;
                if record.cursor() != (Cursor::CopyDeleteIntent { index }) {
                    return Err(NativeError::Foreign);
                }
                let id = identity(
                    copy(&record, index)?
                        .identity()
                        .ok_or(NativeError::Missing)?,
                );
                exit.reverify(self, proof, record.operation(), index, id, deadline)?;
                let process = exit.retained_process().clone();
                let context = self.context.clone();
                let lease = lock.0.clone();
                let bytes = permit.bytes.clone();
                let budget = proof.budget(self, deadline)?;
                self.owner.run(Dispatch::Mutation, deadline, move || {
                    let record = check(&context, &lease, &bytes, &budget)?;
                    original_copy_exited(&process, &record, index, &budget)?;
                    let Some(parent) =
                        copy_parent(&context, record.operation(), false, &budget, &Change::new())?
                    else {
                        return Ok(());
                    };
                    let change = Change::new();
                    change.reached();
                    change.finish(parent.delete_removal_copy(
                        copy(&record, index)?.kind().leaf(),
                        id,
                        &context.security,
                        &budget,
                    ))?;
                    original_copy_exited(&process, &record, index, &budget)
                })
            }
            #[allow(clippy::too_many_arguments)]
            pub(crate) fn final_removal_copy_absent(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                permit: &RemovalMutationPermit,
                index: u8,
                namespace: &super::super::super::payload::helper::removal::RemovalKeeperLease,
                deadline: &Deadline,
            ) -> NativeResult<bool> {
                permit.reverify(self, proof, lock, deadline)?;
                let record = permit.document()?;
                // FILE-only terminal observation covers every selected index, including a
                // previously retired nonfinal copy. It asserts no original process/tree exit.
                if !matches!(
                    record.cursor(),
                    Cursor::Complete { .. }
                        | Cursor::FinalCopyCleanupIntent { .. }
                        | Cursor::FinalCopyAbsent { .. }
                        | Cursor::Retired
                ) {
                    return Err(NativeError::Foreign);
                }
                copy(&record, index)?;
                namespace.reverify(self, proof, record.operation(), deadline)?;
                let actual = namespace.retained_namespace().clone();
                let context = self.context.clone();
                let lease = lock.0.clone();
                let bytes = permit.bytes.clone();
                let budget = proof.budget(self, deadline)?;
                self.owner.run(Dispatch::Observation, deadline, move || {
                    let record = check(&context, &lease, &bytes, &budget)?;
                    namespace_held(&actual, &budget)?;
                    copy_presence(&context, &record, index, &budget)
                })
            }
            #[allow(clippy::too_many_arguments)]
            pub(crate) fn cleanup_final_removal_copy(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                permit: &RemovalMutationPermit,
                index: u8,
                namespace: &super::super::super::payload::helper::removal::RemovalKeeperLease,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                permit.reverify(self, proof, lock, deadline)?;
                let record = permit.document()?;
                if record.cursor() != (Cursor::FinalCopyCleanupIntent { index }) {
                    return Err(NativeError::Foreign);
                }
                namespace.reverify(self, proof, record.operation(), deadline)?;
                let id = identity(
                    copy(&record, index)?
                        .identity()
                        .ok_or(NativeError::Missing)?,
                );
                let module = self.self_image(proof, deadline)?;
                if module.identity() == id {
                    return Err(NativeError::Foreign);
                }
                drop(module);
                let actual = namespace.retained_namespace().clone();
                let context = self.context.clone();
                let lease = lock.0.clone();
                let bytes = permit.bytes.clone();
                let budget = proof.budget(self, deadline)?;
                self.owner.run(Dispatch::Mutation, deadline, move || {
                    let record = check(&context, &lease, &bytes, &budget)?;
                    namespace_held(&actual, &budget)?;
                    let Some(parent) =
                        copy_parent(&context, record.operation(), false, &budget, &Change::new())?
                    else {
                        return Ok(());
                    };
                    // No mapped image or live copy is force-deleted: exclusive standard DELETE
                    // must succeed naturally, otherwise this FILE-only cleanup remains retained.
                    let change = Change::new();
                    change.reached();
                    change.finish(parent.delete_removal_copy(
                        copy(&record, index)?.kind().leaf(),
                        id,
                        &context.security,
                        &budget,
                    ))?;
                    namespace_held(&actual, &budget)
                })
            }
        }
    }
    #[cfg(not(test))]
    pub(crate) use removal_io::{
        RemovalCompletionAdmission, RemovalKeeperSelection, RemovalMutationPermit,
        RemovalPeerImage, RemovalRoot,
    };

    /// File-only ended-logon authority. None of these private capabilities can be converted to
    /// RetainedTreeCompletion, UpgradeStopProof, image approval, task submission, or Run evidence.
    #[cfg(not(test))]
    mod file_recovery {
        use super::super::super::payload::recovery::{
            self, FileRecordStamp, FileRecoveryCursor as Cursor,
            FileRecoveryDirection as Direction, FileRecoveryJournal as Journal,
            FileRecoveryMutationPermit as Permit, FileRecoveryRoleStep as Step, FixedObservation,
            ImageObservation, OperationRecord, OriginalLeaf, OuterPhase, OuterUpgradeRecord,
            ReopenedRole, StageCatalog, StageObservation,
        };
        use super::*;

        struct Selection {
            outer: OuterUpgradeRecord,
            outer_stamp: FileRecordStamp,
            operation: OperationRecord,
            operation_stamp: FileRecordStamp,
        }
        pub(crate) struct FileRecoverySeal(Arc<SealData>);
        struct SealData {
            io: Arc<WindowsNativeIo>,
            selected: Selection,
        }
        pub(crate) struct FileRecoveryRoot(Arc<RootData>);
        struct RootData {
            io: Arc<WindowsNativeIo>,
            root: PayloadRoot,
            own: SelfImagePin,
            approved: [ApprovedPe; 4],
        }
        pub(crate) struct FileRecoveryPlanning(Selection, [ReopenedRole; 4]);
        impl FileRecoveryPlanning {
            pub(crate) fn into_parts(
                self,
            ) -> (
                OuterUpgradeRecord,
                FileRecordStamp,
                OperationRecord,
                FileRecordStamp,
                [ReopenedRole; 4],
            ) {
                (
                    self.0.outer,
                    self.0.outer_stamp,
                    self.0.operation,
                    self.0.operation_stamp,
                    self.1,
                )
            }
        }
        pub(crate) struct FileTerminalObservation(Arc<TerminalData>);
        pub(super) struct TerminalData {
            io: Arc<WindowsNativeIo>,
            root: Arc<RootData>,
            seal: Arc<SealData>,
            journal: Journal,
        }
        fn stamp(identity: FileIdentity, bytes: &[u8]) -> NativeResult<FileRecordStamp> {
            FileRecordStamp::new(identity.into(), epoch_hash(bytes))
        }
        fn read(
            parent: &Anchor,
            context: &Context,
            name: records::RecordName,
            budget: &Deadline,
        ) -> NativeResult<Option<(FileIdentity, Vec<u8>)>> {
            parent.read_private(
                &name.file_name()?,
                &context.security,
                files::MAX_RECORD_BYTES,
                budget,
            )
        }
        fn selected(
            parent: &Anchor,
            context: &Context,
            budget: &Deadline,
        ) -> NativeResult<Option<Selection>> {
            let pointer = read(parent, context, records::RecordName::OuterUpgrade, budget)?;
            let progress = read(parent, context, records::RecordName::FileRecovery, budget)?
                .map(|(_, bytes)| Journal::decode(&bytes))
                .transpose()?;
            let catalog = read(parent, context, records::RecordName::StageCatalog, budget)?
                .map(|(_, bytes)| {
                    records::record_data::<StageCatalog>(&records::RecordName::StageCatalog, &bytes)
                })
                .transpose()?;
            if let Some(catalog) = &catalog {
                catalog.validate()?
            }
            let (outer, outer_stamp) = match pointer {
                Some((id, bytes)) => (OuterUpgradeRecord::decode(&bytes)?, stamp(id, &bytes)?),
                None => match progress.as_ref() {
                    Some(j)
                        if matches!(j.cursor(), Cursor::OuterRetireIntent | Cursor::Retired) =>
                    {
                        (j.outer_snapshot().clone(), j.outer_record())
                    }
                    Some(_) => return Err(NativeError::Foreign),
                    None => {
                        if catalog.as_ref().is_some_and(|c| c.active.is_some()) {
                            return Err(NativeError::Foreign);
                        }
                        return Ok(None);
                    }
                },
            };
            outer.context().same_user(&context.target.identity)?;
            let (id, bytes) = read(
                parent,
                context,
                records::RecordName::Operation(outer.operation()),
                budget,
            )?
            .ok_or(NativeError::Missing)?;
            let operation: OperationRecord =
                records::record_data(&records::RecordName::Operation(outer.operation()), &bytes)?;
            operation.validate()?;
            if operation.operation() != outer.operation() {
                return Err(NativeError::Foreign);
            }
            // A current-context or healthy-terminal no-op cannot mask a changed same-op
            // source pair. Only unrelated already-Retired history may remain nonapplicable.
            if let Some(j) = &progress {
                if j.operation() == outer.operation() {
                    j.matches_sources(&outer, outer_stamp, &operation, stamp(id, &bytes)?)?;
                } else if j.cursor() != Cursor::Retired {
                    return Err(NativeError::Foreign);
                }
            }
            let catalog = catalog.ok_or(NativeError::Missing)?;
            // Healthy terminal history is nonapplicable only with its exact operation/catalog
            // correlation. It neither settles a copy nor replaces old cleanup/tree authority.
            if matches!(outer.phase(), OuterPhase::Complete | OuterPhase::Cancelled) {
                if progress
                    .as_ref()
                    .is_some_and(|j| j.cursor() != Cursor::Retired)
                {
                    return Err(NativeError::Foreign);
                }
                if outer.phase() == OuterPhase::Complete {
                    if operation.phase() != Phase::Complete || catalog.active.is_some() {
                        return Err(NativeError::Foreign);
                    }
                } else {
                    if !matches!(operation.phase(), Phase::Intent | Phase::RolledBack)
                        || catalog.active.is_some_and(|op| op != outer.operation())
                        || operation.current_role().is_some()
                        || operation.original_instance().is_some()
                        || operation.new_instance().is_some()
                        || operation.handoff().is_some()
                        || operation.retention_incomplete()
                    {
                        return Err(NativeError::Foreign);
                    }
                    for role in PayloadRole::ALL {
                        let row = operation.role(role)?;
                        if row.original != OriginalLeaf::Unobserved
                            || row.staged.is_some()
                            || row.backup.is_some()
                            || row.published.is_some()
                        {
                            return Err(NativeError::Foreign);
                        }
                    }
                }
                budget.check()?;
                return Ok(None);
            }
            // The actual full current context remains on its original warm path. Still require
            // a strict paired operation/catalog; malformed/missing required lineage is retained.
            if recovery::file_recovery_prior_logon(&outer, &context.target.identity)?.is_none() {
                if catalog.active != Some(outer.operation()) {
                    return Err(NativeError::Foreign);
                }
                return Ok(None);
            }
            if outer.phase() != OuterPhase::Committed
                || operation.handoff().is_some()
                || !matches!(
                    operation.phase(),
                    Phase::BackupIntent | Phase::BackedUp | Phase::PublishIntent | Phase::Published
                )
            {
                return Err(NativeError::Unsupported);
            }
            let selection = Selection {
                outer,
                outer_stamp,
                operation,
                operation_stamp: stamp(id, &bytes)?,
            };
            let terminal = progress.as_ref().is_some_and(|j| {
                j.operation() == selection.outer.operation()
                    && matches!(
                        j.cursor(),
                        Cursor::CatalogRetireIntent
                            | Cursor::CatalogInactive
                            | Cursor::OuterRetireIntent
                            | Cursor::Retired
                    )
            });
            if catalog.active != Some(selection.outer.operation())
                && !(terminal && catalog.active.is_none())
            {
                return Err(NativeError::Foreign);
            }
            if !catalog
                .generations
                .iter()
                .any(|g| g.operation == selection.outer.operation() && !g.completed)
            {
                return Err(NativeError::Foreign);
            }
            budget.check()?;
            Ok(Some(selection))
        }
        fn matches_selection(selection: &Selection, journal: &Journal) -> NativeResult<()> {
            journal.matches_sources(
                &selection.outer,
                selection.outer_stamp,
                &selection.operation,
                selection.operation_stamp,
            )
        }
        fn renew(
            data: &SealData,
            context: &Context,
            lease: &LockState,
            expected: Option<&[u8]>,
            owner: &CallOwner,
            budget: &Deadline,
        ) -> NativeResult<()> {
            validate_payload_lock(context, lease, budget)?;
            let fresh = selected(&lease.parent, context, budget)?.ok_or(NativeError::Foreign)?;
            if fresh.outer_stamp != data.selected.outer_stamp
                || fresh.operation_stamp != data.selected.operation_stamp
                || fresh.outer != data.selected.outer
            {
                return Err(NativeError::Foreign);
            }
            if let Some(bytes) = expected {
                let (_, current) = read(
                    &lease.parent,
                    context,
                    records::RecordName::FileRecovery,
                    budget,
                )?
                .ok_or(NativeError::Missing)?;
                if current != bytes {
                    return Err(NativeError::Foreign);
                }
                matches_selection(&data.selected, &Journal::decode(bytes)?)?;
            }
            if let Some((_, bytes)) = read(
                &lease.parent,
                context,
                records::RecordName::FileRecovery,
                budget,
            )? {
                let current = Journal::decode(&bytes)?;
                if current.operation() == fresh.outer.operation()
                    && current.cursor() == Cursor::FilesConverged
                {
                    // Do not create a delete intent to explain a copy that was already missing.
                    // Only a previously durable CopyDeleteIntent may reconcile positive absence.
                    let root = Anchor::open(
                        context.target.paths.install(),
                        &context.security,
                        true,
                        budget,
                    )?
                    .ok_or(NativeError::Missing)?;
                    let parent =
                        generation(&root, "payload-stage", current.operation(), context, budget)?;
                    let actual = parent
                        .as_ref()
                        .map(|p| p.opaque("keeper-copy.exe", false, &context.security, budget))
                        .transpose()?
                        .flatten()
                        .map(|leaf| epoch_stamp(leaf.identity));
                    if actual != fresh.outer.keeper_image() {
                        return Err(NativeError::Foreign);
                    }
                    if let Some(parent) = parent
                        && parent
                            .opaque("helper-copy.exe", false, &context.security, budget)?
                            .is_some()
                    {
                        return Err(NativeError::Unsupported);
                    }
                }
            }
            let luid = recovery::file_recovery_prior_logon(&fresh.outer, &context.target.identity)?
                .ok_or(NativeError::Foreign)?;
            query_ended_logon(context, owner, luid, budget)?;
            validate_payload_lock(context, lease, budget)
        }
        impl FileRecoverySeal {
            pub(crate) fn operation(&self) -> [u8; 16] {
                self.0.selected.outer.operation()
            }
            pub(crate) fn matches_journal(&self, journal: &Journal) -> NativeResult<()> {
                matches_selection(&self.0.selected, journal)
            }
            pub(crate) fn reverify(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                lock: &InstallerLock,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                if !std::ptr::eq(io, self.0.io.as_ref()) {
                    return Err(NativeError::Foreign);
                }
                io.lock_binding(proof, lock, deadline)?;
                let data = self.0.clone();
                let context = io.context.clone();
                let lease = lock.0.clone();
                let owner = io.owner.clone();
                let budget = proof.budget(io, deadline)?;
                io.owner.run(Dispatch::Observation, deadline, move || {
                    renew(&data, &context, &lease, None, &owner, &budget)
                })?;
                proof.budget(io, deadline)?.check()
            }
        }
        impl WindowsNativeIo {
            /// This read-only classifier avoids taking the warm helper parent's actual lock.
            /// Its boolean can only request locked reselection; it cannot construct a seal.
            pub(crate) fn file_recovery_requires_lock(
                &self,
                proof: &SupportProof,
                deadline: &Deadline,
            ) -> NativeResult<bool> {
                let context = self.context.clone();
                let budget = proof.budget(self, deadline)?;
                self.owner.run(Dispatch::Observation, deadline, move || {
                    context.validate(&budget)?;
                    let parent = Anchor::open(
                        context.target.paths.installer(),
                        &context.security,
                        true,
                        &budget,
                    )?;
                    let found = match parent {
                        Some(parent) => selected(&parent, &context, &budget)?.is_some(),
                        None => false,
                    };
                    context.validate(&budget)?;
                    Ok(found)
                })
            }
            pub(crate) fn prepare_file_recovery(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                deadline: &Deadline,
            ) -> NativeResult<Option<FileRecoverySeal>> {
                self.lock_binding(proof, lock, deadline)?;
                let io = self.clone();
                let context = self.context.clone();
                let lease = lock.0.clone();
                let owner = self.owner.clone();
                let budget = proof.budget(self, deadline)?;
                let value = self.owner.run(Dispatch::Observation, deadline, move || {
                    validate_payload_lock(&context, &lease, &budget)?;
                    let Some(selection) = selected(&lease.parent, &context, &budget)? else {
                        return Ok(None);
                    };
                    let data = Arc::new(SealData {
                        io,
                        selected: selection,
                    });
                    renew(&data, &context, &lease, None, &owner, &budget)?;
                    Ok(Some(FileRecoverySeal(data)))
                })?;
                proof.budget(self, deadline)?.check()?;
                Ok(value)
            }
            pub(crate) fn admit_file_recovery_permit(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                seal: &FileRecoverySeal,
                deadline: &Deadline,
            ) -> NativeResult<Permit> {
                recovery::admit_file_recovery_permit(self, proof, lock, seal, deadline)
            }
            pub(crate) fn file_recovery_root(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                seal: &FileRecoverySeal,
                deadline: &Deadline,
            ) -> NativeResult<FileRecoveryRoot> {
                seal.reverify(self, proof, lock, deadline)?;
                let inventory =
                    super::super::super::payload::inventory::ApprovedInventory::embedded()?;
                let own = self.self_image(proof, deadline)?;
                let approved = [
                    ApprovedPe::own_image(&own)?,
                    inventory.role(PayloadRole::Agent)?.clone(),
                    inventory.role(PayloadRole::Ui)?.clone(),
                    inventory.role(PayloadRole::Ctl)?.clone(),
                ];
                let root = self.payload_root(proof, lock, deadline)?;
                let result = FileRecoveryRoot(Arc::new(RootData {
                    io: self.clone(),
                    root,
                    own,
                    approved,
                }));
                seal.reverify(self, proof, lock, deadline)?;
                Ok(result)
            }
        }
        fn root_check(
            data: &RootData,
            context: &Context,
            budget: &Deadline,
        ) -> NativeResult<Arc<Anchor>> {
            let own = &data.own.0;
            if own.target != context.target.nonce {
                return Err(NativeError::Foreign);
            }
            let fresh = own.parent.open_image(
                &own.leaf,
                false,
                env!("CARGO_PKG_VERSION"),
                &context.security,
                budget,
            )?;
            if fresh.identity != own.image.identity || fresh.facts != own.image.facts {
                return Err(NativeError::Foreign);
            }
            drop(fresh);
            data.root
                .check(context, budget)?
                .ok_or(NativeError::Missing)
        }
        fn generation(
            root: &Anchor,
            branch: &str,
            operation: [u8; 16],
            context: &Context,
            budget: &Deadline,
        ) -> NativeResult<Option<Anchor>> {
            root.child(branch, &context.security, budget)?
                .map(|parent| parent.child(&records::hex(&operation), &context.security, budget))
                .transpose()
                .map(|value| value.flatten())
        }
        fn expected(data: &RootData, role: PayloadRole) -> NativeResult<&ApprovedPe> {
            data.approved
                .iter()
                .find(|pin| pin.role() == role)
                .ok_or(NativeError::Invalid)
        }
        fn opaque(
            parent: Option<&Anchor>,
            role: PayloadRole,
            context: &Context,
            budget: &Deadline,
        ) -> NativeResult<Option<native::OpaqueData>> {
            parent
                .map(|p| p.opaque(role.leaf(), false, &context.security, budget))
                .transpose()
                .map(|v| v.flatten())
        }
        /// All three slots are independently opened through retained no-follow parents. Measured
        /// new bytes are correlation for rollback; forward separately checks embedded approval.
        fn observe(
            data: &RootData,
            seal: &SealData,
            journal: Option<&Journal>,
            role: PayloadRole,
            context: &Context,
            budget: &Deadline,
        ) -> NativeResult<ReopenedRole> {
            let root = root_check(data, context, budget)?;
            let op = seal.selected.outer.operation();
            let stage = generation(&root, "payload-stage", op, context, budget)?;
            let backups = generation(&root, "payload-backups", op, context, budget)?;
            let source = seal.selected.operation.role(role)?;
            let (original, new) = if let Some(j) = journal {
                let row = j.role(role)?;
                (row.original(), row.new_image().cloned())
            } else {
                (source.original, source.staged.clone())
            };
            let new = new.ok_or(NativeError::Foreign)?;
            let staged = match opaque(stage.as_ref(), role, context, budget)? {
                None => StageObservation::Missing,
                Some(leaf) => {
                    let id = leaf.identity;
                    drop(leaf);
                    let image = stage
                        .as_ref()
                        .ok_or(NativeError::Missing)?
                        .open_staged_image(
                            role.leaf(),
                            &new.facts.version,
                            &context.security,
                            budget,
                        )?;
                    if image.identity == id
                        && image.identity == epoch_identity(new.identity)
                        && image.facts == new.facts
                    {
                        StageObservation::Ready(new.clone())
                    } else {
                        StageObservation::Unknown
                    }
                }
            };
            let fixed = match opaque(Some(&root), role, context, budget)? {
                None => FixedObservation::Missing,
                Some(leaf) => {
                    let id = leaf.identity;
                    drop(leaf);
                    if id == epoch_identity(new.identity) {
                        let image = root.open_image(
                            role.leaf(),
                            false,
                            &new.facts.version,
                            &context.security,
                            budget,
                        )?;
                        if image.identity == id && image.facts == new.facts {
                            FixedObservation::Published(new.clone())
                        } else {
                            FixedObservation::Unknown
                        }
                    } else if original == OriginalLeaf::Unobserved
                        || original == OriginalLeaf::Present(id.into())
                    {
                        FixedObservation::Original(id.into())
                    } else {
                        FixedObservation::Unknown
                    }
                }
            };
            let backup =
                opaque(backups.as_ref(), role, context, budget)?.map(|leaf| leaf.identity.into());
            let allowed = match original {
                OriginalLeaf::Present(id) => Some(id),
                _ => source.backup,
            };
            let unknown_backup = backup.is_some() && backup != allowed;
            if journal.is_some_and(|j| j.direction() == Direction::Forward)
                && new.facts != *expected(data, role)?.facts()
            {
                return Err(NativeError::Unsupported);
            }
            Ok(ReopenedRole {
                staged,
                fixed,
                backup,
                unknown_backup,
            })
        }
        fn verify_converged(
            data: &RootData,
            seal: &SealData,
            journal: &Journal,
            context: &Context,
            budget: &Deadline,
        ) -> NativeResult<()> {
            for role in PayloadRole::ALL {
                let row = journal.role(role)?;
                let view = observe(data, seal, Some(journal), role, context, budget)?;
                if view.unknown_backup {
                    return Err(NativeError::Foreign);
                }
                let correct = match journal.direction() {
                    Direction::Rollback => {
                        view.staged
                            == StageObservation::Ready(
                                row.new_image().ok_or(NativeError::Foreign)?.clone(),
                            )
                            && match row.original() {
                                OriginalLeaf::Missing => {
                                    matches!(view.fixed, FixedObservation::Missing)
                                        && view.backup.is_none()
                                }
                                OriginalLeaf::Present(id) => {
                                    view.fixed == FixedObservation::Original(id)
                                        && view.backup.is_none()
                                }
                                OriginalLeaf::Unobserved => false,
                            }
                    }
                    Direction::Forward => {
                        matches!(&view.fixed,FixedObservation::Published(image)
                        if Some(image)==row.new_image())
                            && matches!(view.staged, StageObservation::Missing)
                            && view.backup
                                == match row.original() {
                                    OriginalLeaf::Present(id) => Some(id),
                                    _ => None,
                                }
                    }
                };
                if !correct {
                    return Err(NativeError::Foreign);
                }
            }
            budget.check()
        }
        impl FileRecoveryRoot {
            // Each sealed proof/lock/record/deadline boundary remains independently explicit.
            #[allow(clippy::too_many_arguments)]
            fn run<T: Send + 'static>(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                seal: &FileRecoverySeal,
                permit: &Permit,
                dispatch: Dispatch,
                deadline: &Deadline,
                work: impl FnOnce(
                    &RootData,
                    &SealData,
                    &Journal,
                    &Context,
                    &Arc<LockState>,
                    &Deadline,
                    &Change,
                ) -> NativeResult<T>
                + Send
                + 'static,
            ) -> NativeResult<T> {
                let io = &self.0.io;
                if !std::ptr::eq(io.as_ref(), seal.0.io.as_ref())
                    || !std::ptr::eq(io.as_ref(), permit.io().as_ref())
                {
                    return Err(NativeError::Foreign);
                }
                permit.reverify(io, proof, lock, seal, deadline)?;
                let data = self.0.clone();
                let seal = seal.0.clone();
                let context = io.context.clone();
                let lease = lock.0.clone();
                let bytes = permit.bytes().to_vec();
                let owner = io.owner.clone();
                let budget = proof.budget(io, deadline)?;
                io.owner.run(dispatch, deadline, move || {
                    let change = Change::new();
                    let result = change.finish((|| {
                        renew(&seal, &context, &lease, Some(&bytes), &owner, &budget)?;
                        let journal = Journal::decode(&bytes)?;
                        root_check(&data, &context, &budget)?;
                        let value =
                            work(&data, &seal, &journal, &context, &lease, &budget, &change)?;
                        budget.check()?;
                        context.validate(&budget)?;
                        Ok(value)
                    })());
                    if matches!(result, Err(NativeError::OutcomeUnknown)) {
                        owner.retire_mutations()
                    }
                    result
                })
            }
            pub(crate) fn planning(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                seal: &FileRecoverySeal,
                deadline: &Deadline,
            ) -> NativeResult<FileRecoveryPlanning> {
                seal.reverify(&self.0.io, proof, lock, deadline)?;
                let data = self.0.clone();
                let seal = seal.0.clone();
                let context = self.0.io.context.clone();
                let lease = lock.0.clone();
                let budget = proof.budget(&self.0.io, deadline)?;
                let owner = self.0.io.owner.clone();
                self.0
                    .io
                    .owner
                    .run(Dispatch::Observation, deadline, move || {
                        renew(&seal, &context, &lease, None, &owner, &budget)?;
                        let views = PayloadRole::ALL
                            .into_iter()
                            .map(|role| observe(&data, &seal, None, role, &context, &budget))
                            .collect::<NativeResult<Vec<_>>>()?
                            .try_into()
                            .map_err(|_| NativeError::Invalid)?;
                        // Planner itself validates all roles before a new journal can be published.
                        let candidate = Journal::prepare(
                            seal.selected.outer.clone(),
                            seal.selected.outer_stamp,
                            &seal.selected.operation,
                            seal.selected.operation_stamp,
                            views,
                        )?;
                        if candidate.direction() == Direction::Forward {
                            for role in PayloadRole::ALL {
                                if candidate
                                    .role(role)?
                                    .new_image()
                                    .ok_or(NativeError::Foreign)?
                                    .facts
                                    != *expected(&data, role)?.facts()
                                {
                                    return Err(NativeError::Unsupported);
                                }
                            }
                        }
                        let views = PayloadRole::ALL
                            .into_iter()
                            .map(|role| {
                                observe(&data, &seal, Some(&candidate), role, &context, &budget)
                            })
                            .collect::<NativeResult<Vec<_>>>()?
                            .try_into()
                            .map_err(|_| NativeError::Invalid)?;
                        let selection = Selection {
                            outer: seal.selected.outer.clone(),
                            outer_stamp: seal.selected.outer_stamp,
                            operation: seal.selected.operation.clone(),
                            operation_stamp: seal.selected.operation_stamp,
                        };
                        budget.check()?;
                        Ok(FileRecoveryPlanning(selection, views))
                    })
            }
            pub(crate) fn observe_role(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                seal: &FileRecoverySeal,
                permit: &Permit,
                role: PayloadRole,
                deadline: &Deadline,
            ) -> NativeResult<ReopenedRole> {
                self.run(
                    proof,
                    lock,
                    seal,
                    permit,
                    Dispatch::Observation,
                    deadline,
                    move |data, seal, journal, ctx, _, budget, _| {
                        observe(data, seal, Some(journal), role, ctx, budget)
                    },
                )
            }
            // The closed role/cursor is separate from the native capabilities and deadline.
            #[allow(clippy::too_many_arguments)]
            fn effect(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                seal: &FileRecoverySeal,
                permit: &Permit,
                role: PayloadRole,
                step: Step,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                if permit.cursor() != (Cursor::Role { role, step }) {
                    return Err(NativeError::Foreign);
                }
                self.run(
                    proof,
                    lock,
                    seal,
                    permit,
                    Dispatch::Mutation,
                    deadline,
                    move |data, seal, journal, ctx, _, budget, change| {
                        let root = root_check(data, ctx, budget)?;
                        let op = seal.selected.outer.operation();
                        let row = journal.role(role)?;
                        let view = observe(data, seal, Some(journal), role, ctx, budget)?;
                        if view.unknown_backup
                            || matches!(view.fixed, FixedObservation::Unknown)
                            || matches!(view.staged, StageObservation::Unknown)
                        {
                            return Err(NativeError::Foreign);
                        }
                        match step {
                            Step::ReturnPublishedIntent => {
                                if journal.direction() != Direction::Rollback {
                                    return Err(NativeError::Foreign);
                                }
                                match (&view.fixed, &view.staged) {
                                    (FixedObservation::Missing, StageObservation::Ready(image))
                                        if Some(image) == row.new_image() =>
                                    {
                                        return Ok(());
                                    }
                                    (
                                        FixedObservation::Published(image),
                                        StageObservation::Missing,
                                    ) if Some(image) == row.new_image() => {}
                                    _ => return Err(NativeError::Foreign),
                                }
                                let stage = ensure_payload_child(
                                    &root,
                                    "payload-stage",
                                    ctx,
                                    budget,
                                    change,
                                )?;
                                let parent = ensure_payload_child(
                                    &stage,
                                    &records::hex(&op),
                                    ctx,
                                    budget,
                                    change,
                                )?;
                                let source = root
                                    .opaque(role.leaf(), false, &ctx.security, budget)?
                                    .ok_or(NativeError::Missing)?;
                                if source.identity
                                    != epoch_identity(
                                        row.new_image().ok_or(NativeError::Foreign)?.identity,
                                    )
                                {
                                    return Err(NativeError::Foreign);
                                }
                                change.reached();
                                root.move_opaque(
                                    source,
                                    &parent,
                                    role.leaf(),
                                    &ctx.security,
                                    budget,
                                )?;
                            }
                            Step::RestoreOriginalIntent => {
                                if journal.direction() != Direction::Rollback {
                                    return Err(NativeError::Foreign);
                                }
                                match row.original() {
                                    OriginalLeaf::Missing => {
                                        if matches!(view.fixed, FixedObservation::Missing)
                                            && view.backup.is_none()
                                        {
                                            return Ok(());
                                        }
                                        return Err(NativeError::Foreign);
                                    }
                                    OriginalLeaf::Present(id) => {
                                        if view.fixed == FixedObservation::Original(id)
                                            && view.backup.is_none()
                                        {
                                            return Ok(());
                                        }
                                        if !matches!(view.fixed, FixedObservation::Missing)
                                            || view.backup != Some(id)
                                        {
                                            return Err(NativeError::Foreign);
                                        }
                                        let backup =
                                            generation(&root, "payload-backups", op, ctx, budget)?
                                                .ok_or(NativeError::Missing)?;
                                        let source = backup
                                            .opaque(role.leaf(), false, &ctx.security, budget)?
                                            .ok_or(NativeError::Missing)?;
                                        if source.identity != epoch_identity(id) {
                                            return Err(NativeError::Foreign);
                                        }
                                        change.reached();
                                        backup.move_opaque(
                                            source,
                                            &root,
                                            role.leaf(),
                                            &ctx.security,
                                            budget,
                                        )?;
                                    }
                                    OriginalLeaf::Unobserved => return Err(NativeError::Foreign),
                                }
                            }
                            Step::SettleStageIntent => {
                                if journal.direction() != Direction::Forward {
                                    return Err(NativeError::Foreign);
                                }
                                let new = row.new_image().ok_or(NativeError::Foreign)?;
                                if view.staged != StageObservation::Ready(new.clone()) {
                                    return Err(NativeError::Foreign);
                                }
                                let parent = generation(&root, "payload-stage", op, ctx, budget)?
                                    .ok_or(NativeError::Missing)?;
                                flush_stage(&parent, role, new, data, ctx, budget, change)?;
                            }
                            Step::BackupOriginalIntent => {
                                if journal.direction() != Direction::Forward {
                                    return Err(NativeError::Foreign);
                                }
                                match row.original() {
                                    OriginalLeaf::Missing => {
                                        if matches!(view.fixed, FixedObservation::Missing)
                                            && view.backup.is_none()
                                        {
                                            return Ok(());
                                        }
                                        return Err(NativeError::Foreign);
                                    }
                                    OriginalLeaf::Present(id) => {
                                        if matches!(view.fixed, FixedObservation::Missing)
                                            && view.backup == Some(id)
                                        {
                                            return Ok(());
                                        }
                                        if view.fixed != FixedObservation::Original(id)
                                            || view.backup.is_some()
                                        {
                                            return Err(NativeError::Foreign);
                                        }
                                        let branch = ensure_payload_child(
                                            &root,
                                            "payload-backups",
                                            ctx,
                                            budget,
                                            change,
                                        )?;
                                        let backup = ensure_payload_child(
                                            &branch,
                                            &records::hex(&op),
                                            ctx,
                                            budget,
                                            change,
                                        )?;
                                        let source = root
                                            .opaque(role.leaf(), false, &ctx.security, budget)?
                                            .ok_or(NativeError::Missing)?;
                                        if source.identity != epoch_identity(id) {
                                            return Err(NativeError::Foreign);
                                        }
                                        change.reached();
                                        root.move_opaque(
                                            source,
                                            &backup,
                                            role.leaf(),
                                            &ctx.security,
                                            budget,
                                        )?;
                                    }
                                    OriginalLeaf::Unobserved => return Err(NativeError::Foreign),
                                }
                            }
                            Step::PublishStageIntent => {
                                if journal.direction() != Direction::Forward {
                                    return Err(NativeError::Foreign);
                                }
                                let new = row.new_image().ok_or(NativeError::Foreign)?;
                                if view.fixed == FixedObservation::Published(new.clone())
                                    && matches!(view.staged, StageObservation::Missing)
                                {
                                    return Ok(());
                                }
                                if !matches!(view.fixed, FixedObservation::Missing)
                                    || view.staged != StageObservation::Ready(new.clone())
                                {
                                    return Err(NativeError::Foreign);
                                }
                                let stage = generation(&root, "payload-stage", op, ctx, budget)?
                                    .ok_or(NativeError::Missing)?;
                                // An interrupted process may have lost its old settled stage handle.
                                // Re-measure and flush this SAME reopened file before the no-replace move.
                                let image =
                                    flush_stage(&stage, role, new, data, ctx, budget, change)?;
                                change.reached();
                                stage.publish_image(
                                    &image,
                                    &root,
                                    role.leaf(),
                                    &ctx.security,
                                    budget,
                                )?;
                            }
                            _ => return Err(NativeError::Foreign),
                        }
                        let after = observe(data, seal, Some(journal), role, ctx, budget)?;
                        let valid = match step {
                            Step::ReturnPublishedIntent => {
                                matches!(after.fixed, FixedObservation::Missing)
                                    && after.staged
                                        == StageObservation::Ready(
                                            row.new_image().ok_or(NativeError::Foreign)?.clone(),
                                        )
                            }
                            Step::RestoreOriginalIntent => {
                                after.backup.is_none()
                                    && match row.original() {
                                        OriginalLeaf::Missing => {
                                            matches!(after.fixed, FixedObservation::Missing)
                                        }
                                        OriginalLeaf::Present(id) => {
                                            after.fixed == FixedObservation::Original(id)
                                        }
                                        _ => false,
                                    }
                            }
                            Step::SettleStageIntent => {
                                after.staged
                                    == StageObservation::Ready(
                                        row.new_image().ok_or(NativeError::Foreign)?.clone(),
                                    )
                            }
                            Step::BackupOriginalIntent => {
                                matches!(after.fixed, FixedObservation::Missing)
                                    && after.backup
                                        == match row.original() {
                                            OriginalLeaf::Present(id) => Some(id),
                                            _ => None,
                                        }
                            }
                            Step::PublishStageIntent => {
                                after.fixed
                                    == FixedObservation::Published(
                                        row.new_image().ok_or(NativeError::Foreign)?.clone(),
                                    )
                                    && matches!(after.staged, StageObservation::Missing)
                            }
                            _ => false,
                        };
                        if !valid {
                            return Err(NativeError::OutcomeUnknown);
                        }
                        Ok(())
                    },
                )
            }
            pub(crate) fn return_published(
                &self,
                p: &SupportProof,
                l: &InstallerLock,
                s: &FileRecoverySeal,
                m: &Permit,
                r: PayloadRole,
                d: &Deadline,
            ) -> NativeResult<()> {
                self.effect(p, l, s, m, r, Step::ReturnPublishedIntent, d)
            }
            pub(crate) fn restore_original(
                &self,
                p: &SupportProof,
                l: &InstallerLock,
                s: &FileRecoverySeal,
                m: &Permit,
                r: PayloadRole,
                d: &Deadline,
            ) -> NativeResult<()> {
                self.effect(p, l, s, m, r, Step::RestoreOriginalIntent, d)
            }
            pub(crate) fn settle_stage(
                &self,
                p: &SupportProof,
                l: &InstallerLock,
                s: &FileRecoverySeal,
                m: &Permit,
                r: PayloadRole,
                d: &Deadline,
            ) -> NativeResult<()> {
                self.effect(p, l, s, m, r, Step::SettleStageIntent, d)
            }
            pub(crate) fn backup_original(
                &self,
                p: &SupportProof,
                l: &InstallerLock,
                s: &FileRecoverySeal,
                m: &Permit,
                r: PayloadRole,
                d: &Deadline,
            ) -> NativeResult<()> {
                self.effect(p, l, s, m, r, Step::BackupOriginalIntent, d)
            }
            pub(crate) fn publish_stage(
                &self,
                p: &SupportProof,
                l: &InstallerLock,
                s: &FileRecoverySeal,
                m: &Permit,
                r: PayloadRole,
                d: &Deadline,
            ) -> NativeResult<()> {
                self.effect(p, l, s, m, r, Step::PublishStageIntent, d)
            }
            pub(crate) fn observe_convergence(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                seal: &FileRecoverySeal,
                permit: &Permit,
                deadline: &Deadline,
            ) -> NativeResult<FileTerminalObservation> {
                let root = self.0.clone();
                let sealed = seal.0.clone();
                let io = self.0.io.clone();
                self.run(
                    proof,
                    lock,
                    seal,
                    permit,
                    Dispatch::Observation,
                    deadline,
                    move |data, seal, journal, ctx, _, budget, _| {
                        verify_converged(data, seal, journal, ctx, budget)?;
                        Ok(FileTerminalObservation(Arc::new(TerminalData {
                            io,
                            root,
                            seal: sealed,
                            journal: journal.clone(),
                        })))
                    },
                )
            }
            pub(crate) fn retire_catalog(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                seal: &FileRecoverySeal,
                permit: &Permit,
                absence: &super::keeper::FileRecoveryKeeperAbsent,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                if permit.cursor() != Cursor::CatalogRetireIntent {
                    return Err(NativeError::Foreign);
                }
                absence.reverify(&self.0.io, proof, lock, seal, deadline)?;
                let namespace = absence.retain_namespace();
                self.run(
                    proof,
                    lock,
                    seal,
                    permit,
                    Dispatch::Mutation,
                    deadline,
                    move |data, seal, journal, ctx, lease, budget, change| {
                        namespace.reverify(ctx, budget)?;
                        verify_converged(data, seal, journal, ctx, budget)?;
                        verify_copy_absent(ctx, journal, budget)?;
                        let (_, bytes) = read(
                            &lease.parent,
                            ctx,
                            records::RecordName::StageCatalog,
                            budget,
                        )?
                        .ok_or(NativeError::Missing)?;
                        let mut catalog: StageCatalog =
                            records::record_data(&records::RecordName::StageCatalog, &bytes)?;
                        catalog.validate()?;
                        if catalog.active.is_none() {
                            return Ok(());
                        }
                        if catalog.active != Some(journal.operation()) {
                            return Err(NativeError::Foreign);
                        }
                        catalog.active = None; // Retain ALL incomplete generations/backups; no healthy/prune claim.
                        let bytes = records::encode_record(
                            &records::RecordName::StageCatalog,
                            serde_json::to_value(&catalog).map_err(|_| NativeError::Invalid)?,
                        )?;
                        change.reached();
                        let mut store = NativeStore {
                            context: data.io.context.clone(),
                            lease: lease.clone(),
                            budget: budget.clone(),
                            name: records::RecordName::StageCatalog,
                            change: Change::new(),
                        };
                        let result = records::publish(&mut store, &bytes, budget)?;
                        if result.state != records::PublicationRecovery::NewPublished
                            || result.native_failure.is_some()
                        {
                            return Err(NativeError::OutcomeUnknown);
                        }
                        let (_, actual) = read(
                            &lease.parent,
                            ctx,
                            records::RecordName::StageCatalog,
                            budget,
                        )?
                        .ok_or(NativeError::Missing)?;
                        if actual != bytes {
                            return Err(NativeError::OutcomeUnknown);
                        }
                        Ok(())
                    },
                )
            }
            pub(crate) fn retire_outer(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                seal: &FileRecoverySeal,
                permit: &Permit,
                absence: &super::keeper::FileRecoveryKeeperAbsent,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                if permit.cursor() != Cursor::OuterRetireIntent {
                    return Err(NativeError::Foreign);
                }
                absence.reverify(&self.0.io, proof, lock, seal, deadline)?;
                let namespace = absence.retain_namespace();
                self.run(
                    proof,
                    lock,
                    seal,
                    permit,
                    Dispatch::Mutation,
                    deadline,
                    move |data, seal, journal, ctx, lease, budget, change| {
                        namespace.reverify(ctx, budget)?;
                        verify_converged(data, seal, journal, ctx, budget)?;
                        verify_copy_absent(ctx, journal, budget)?;
                        verify_catalog_inactive(lease, ctx, budget)?;
                        match read(
                            &lease.parent,
                            ctx,
                            records::RecordName::OuterUpgrade,
                            budget,
                        )? {
                            None => Ok(()),
                            Some((id, bytes)) => {
                                if stamp(id, &bytes)? != journal.outer_record() {
                                    return Err(NativeError::Foreign);
                                }
                                change.reached();
                                lease.parent.delete_recovered_outer_record(
                                    id,
                                    &ctx.security,
                                    budget,
                                )
                            }
                        }
                    },
                )
            }
            pub(crate) fn observe_terminal(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                seal: &FileRecoverySeal,
                permit: &Permit,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                if !matches!(permit.cursor(), Cursor::OuterRetireIntent | Cursor::Retired) {
                    return Err(NativeError::Foreign);
                }
                self.run(
                    proof,
                    lock,
                    seal,
                    permit,
                    Dispatch::Observation,
                    deadline,
                    move |data, seal, journal, ctx, lease, budget, _| {
                        verify_converged(data, seal, journal, ctx, budget)?;
                        verify_copy_absent(ctx, journal, budget)?;
                        verify_catalog_inactive(lease, ctx, budget)?;
                        if read(
                            &lease.parent,
                            ctx,
                            records::RecordName::OuterUpgrade,
                            budget,
                        )?
                        .is_some()
                        {
                            return Err(NativeError::Foreign);
                        }
                        Ok(())
                    },
                )
            }
        }
        fn flush_stage(
            parent: &Anchor,
            role: PayloadRole,
            new: &ImageObservation,
            data: &RootData,
            ctx: &Context,
            budget: &Deadline,
            change: &Change,
        ) -> NativeResult<native::ImageData> {
            if new.facts != *expected(data, role)?.facts() {
                return Err(NativeError::Unsupported);
            }
            let image =
                parent.open_staged_image(role.leaf(), &new.facts.version, &ctx.security, budget)?;
            if image.identity != epoch_identity(new.identity) || image.facts != new.facts {
                return Err(NativeError::Foreign);
            }
            change.reached();
            // SAFETY: this exact freshly measured writable stage is pinned under its durable
            // file-only intent. No other file or unapproved content is flushed or executed.
            if unsafe { FlushFileBuffers(image.file.as_raw_handle()) } == 0 {
                return Err(native::last_error());
            }
            budget.check()?;
            Ok(image)
        }
        fn verify_catalog_inactive(
            lease: &LockState,
            ctx: &Context,
            budget: &Deadline,
        ) -> NativeResult<()> {
            let (_, bytes) = read(
                &lease.parent,
                ctx,
                records::RecordName::StageCatalog,
                budget,
            )?
            .ok_or(NativeError::Missing)?;
            let catalog: StageCatalog =
                records::record_data(&records::RecordName::StageCatalog, &bytes)?;
            catalog.validate()?;
            if catalog.active.is_some() {
                return Err(NativeError::Foreign);
            }
            Ok(())
        }
        pub(super) fn verify_copy_absent(
            ctx: &Context,
            journal: &Journal,
            budget: &Deadline,
        ) -> NativeResult<()> {
            if let Some(root) =
                Anchor::open(ctx.target.paths.install(), &ctx.security, true, budget)?
                && let Some(parent) =
                    generation(&root, "payload-stage", journal.operation(), ctx, budget)?
                && (parent
                    .opaque("keeper-copy.exe", false, &ctx.security, budget)?
                    .is_some()
                    || parent
                        .opaque("helper-copy.exe", false, &ctx.security, budget)?
                        .is_some())
            {
                return Err(NativeError::Foreign);
            }
            Ok(())
        }
        impl FileTerminalObservation {
            pub(super) fn held(&self) -> Arc<TerminalData> {
                self.0.clone()
            }
            pub(super) fn reverify(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                lock: &InstallerLock,
                seal: &FileRecoverySeal,
                deadline: &Deadline,
            ) -> NativeResult<Journal> {
                if !std::ptr::eq(io, self.0.io.as_ref()) {
                    return Err(NativeError::Foreign);
                }
                self.0.matches_seal(seal)?;
                seal.reverify(io, proof, lock, deadline)?;
                let actual = recovery::read_file_recovery(io, proof, deadline)?
                    .ok_or(NativeError::Missing)?;
                self.0.journal.same_plan(&actual)?;
                let permit =
                    recovery::admit_file_recovery_permit(&self.0.io, proof, lock, seal, deadline)?;
                let root = FileRecoveryRoot(self.0.root.clone());
                root.run(
                    proof,
                    lock,
                    seal,
                    &permit,
                    Dispatch::Observation,
                    deadline,
                    |data, seal, journal, ctx, _, budget, _| {
                        verify_converged(data, seal, journal, ctx, budget)
                    },
                )?;
                Ok(actual)
            }
        }
        impl TerminalData {
            pub(super) fn matches_seal(&self, seal: &FileRecoverySeal) -> NativeResult<()> {
                if !Arc::ptr_eq(&seal.0, &self.seal) {
                    return Err(NativeError::Foreign);
                }
                Ok(())
            }
            pub(super) fn validate_current(
                &self,
                context: &Context,
                lease: &LockState,
                owner: &CallOwner,
                budget: &Deadline,
            ) -> NativeResult<Journal> {
                renew(&self.seal, context, lease, None, owner, budget)?;
                let (_, bytes) = read(
                    &lease.parent,
                    context,
                    records::RecordName::FileRecovery,
                    budget,
                )?
                .ok_or(NativeError::Missing)?;
                let journal = Journal::decode(&bytes)?;
                self.journal.same_plan(&journal)?;
                matches_selection(&self.seal.selected, &journal)?;
                verify_converged(&self.root, &self.seal, &journal, context, budget)?;
                Ok(journal)
            }
        }
    }
    #[cfg(not(test))]
    pub(crate) use file_recovery::{FileRecoveryRoot, FileRecoverySeal, FileTerminalObservation};

    #[cfg(not(test))]
    pub(crate) use keeper::FileRecoveryKeeperAbsent;

    /// Shared bounded exact-LUID worker. Inputs are supplied only by freshly matched native
    /// supervisor/file lineage; this observation never constructs a tree or start capability.
    #[cfg(not(test))]
    fn query_ended_logon(
        context: &Context,
        owner: &CallOwner,
        authentication_id: u64,
        budget: &Deadline,
    ) -> NativeResult<()> {
        use windows_sys::Win32::{
            Foundation::LUID,
            Security::{
                Authentication::Identity::LsaGetLogonSessionData,
                Credentials::STATUS_NO_SUCH_LOGON_SESSION,
            },
        };
        context.validate(budget)?;
        if LOGON_QUERY_QUARANTINE.get().is_some() {
            return Err(NativeError::OutcomeUnknown);
        }
        if LOGON_QUERY_BUSY
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(NativeError::Busy);
        }
        let _in_flight = LogonQueryGuard;
        if LOGON_QUERY_QUARANTINE.get().is_some() {
            return Err(NativeError::OutcomeUnknown);
        }
        let luid = LUID {
            LowPart: authentication_id as u32,
            HighPart: (authentication_id >> 32) as u32 as i32,
        };
        let mut data = std::ptr::null_mut();
        // SAFETY: one exact captured same-user prior AuthenticationId, initialized output.
        // This never enumerates sessions or uses a record/PID to open a process.
        let status = unsafe { LsaGetLogonSessionData(&luid, &mut data) };
        if status == 0 {
            if !data.is_null() {
                let buffer = LsaBuffer(data);
                // Only documented successful ownership is freed. Names/content stay unread.
                drop(buffer);
            }
            budget.check()?;
            context.validate(budget)?;
            return Err(NativeError::Foreign);
        }
        if !data.is_null() {
            // Failed-output ownership is undocumented. Quarantine one opaque value and
            // retire all mutations; neither dereference nor free this ambiguous output.
            let _ = LOGON_QUERY_QUARANTINE.set(data as usize);
            owner.retire_mutations();
            return Err(NativeError::OutcomeUnknown);
        }
        super::epoch_archive::classify_prior_logon_status(status, false)?;
        if status != STATUS_NO_SUCH_LOGON_SESSION {
            return Err(NativeError::Foreign);
        }
        budget.check()?;
        context.validate(budget)
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
        pub(crate) fn payload_root(
            &self,
            proof: &SupportProof,
            lock: &InstallerLock,
            deadline: &Deadline,
        ) -> NativeResult<PayloadRoot> {
            self.lock_binding(proof, lock, deadline)?;
            let budget = proof.budget(self, deadline)?;
            let context = self.context.clone();
            let lease = lock.0.clone();
            self.owner.run(Dispatch::Observation, deadline, move || {
                validate_payload_lock(&context, &lease, &budget)?;
                let install = Anchor::open(
                    context.target.paths.install(),
                    &context.security,
                    true,
                    &budget,
                )?
                .map(Arc::new);
                let canonical = match &install {
                    Some(root) => root
                        .canonical_dos_path(&context.security, &budget)?
                        .to_str()
                        .ok_or(NativeError::Unsupported)?
                        .to_owned(),
                    None => context.target.paths.install().to_owned(),
                };
                Ok(PayloadRoot(Arc::new(PayloadRootData {
                    target: context.target.nonce,
                    install: Mutex::new(install),
                    canonical,
                })))
            })
        }
        /// Retires ONLY an opaque positive post-prune absence result, under the existing intent.
        pub(crate) fn retire_pruned(
            &self,
            proof: &SupportProof,
            lock: &InstallerLock,
            permit: &MutationPermit,
            pruned: PrunedGeneration,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.lock_binding(proof, lock, deadline)?;
            check_permit(self, permit, Phase::PruneIntent, None)?;
            if pruned.target != self.context.target.nonce || pruned.operation != permit.operation()
            {
                return Err(NativeError::Foreign);
            }
            let context = self.context.clone();
            let lease = lock.0.clone();
            let budget = proof.budget(self, deadline)?;
            let expected = permit.bytes().to_vec();
            let owner = self.owner.clone();
            self.owner.run(Dispatch::Mutation, deadline, move || {
                validate_payload_lock(&context, &lease, &budget)?;
                validate_intent(&context, &lease, pruned.operation, &expected, &budget)?;
                let (_, bytes) = lease
                    .parent
                    .read_private(
                        &records::RecordName::StageCatalog.file_name()?,
                        &context.security,
                        files::MAX_RECORD_BYTES,
                        &budget,
                    )?
                    .ok_or(NativeError::Missing)?;
                let mut catalog: super::super::payload::recovery::StageCatalog =
                    records::record_data(&records::RecordName::StageCatalog, &bytes)?;
                if catalog.active != Some(pruned.operation)
                    || !super::super::payload::recovery::prune_candidates(&catalog)?
                        .contains(&pruned.generation)
                {
                    return Err(NativeError::Foreign);
                }
                // Recheck positive absence immediately before retiring correlation. A recreated
                // entry is retained, even though an earlier prune returned a genuine sealed result.
                if let Some(root) = Anchor::open(
                    context.target.paths.install(),
                    &context.security,
                    true,
                    &budget,
                )? && let Some(backups) =
                    root.child("payload-backups", &context.security, &budget)?
                    && backups
                        .opaque(
                            &records::hex(&pruned.generation),
                            false,
                            &context.security,
                            &budget,
                        )?
                        .is_some()
                {
                    return Err(NativeError::Foreign);
                }
                super::super::payload::recovery::retire_completed(&mut catalog, pruned.generation)?;
                let bytes = records::encode_record(
                    &records::RecordName::StageCatalog,
                    serde_json::to_value(&catalog).map_err(|_| NativeError::Invalid)?,
                )?;
                let mut store = NativeStore {
                    context,
                    lease,
                    budget,
                    name: records::RecordName::StageCatalog,
                    change: Change::new(),
                };
                let budget = store.budget.clone();
                let publication = records::publish(&mut store, &bytes, &budget);
                let publication = store.change.finish(publication)?;
                if publication.native_failure.is_some()
                    || publication.state != records::PublicationRecovery::NewPublished
                {
                    owner.retire_mutations();
                    return Err(NativeError::OutcomeUnknown);
                }
                Ok(())
            })
        }
        pub(crate) fn self_image_reader(
            &self,
            image: &SelfImagePin,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<Box<dyn std::io::Read + Send>> {
            image.reverify(self, proof, deadline)?;
            let budget = proof.budget(self, deadline)?;
            let image = image.0.clone();
            let context = self.context.clone();
            let file = self.owner.run(Dispatch::Observation, deadline, move || {
                context.validate(&budget)?;
                image
                    .image
                    .file
                    .try_clone()
                    .map_err(|_| NativeError::Unavailable)
            })?;
            Ok(Box::new(OffsetReader { file, offset: 0 }))
        }
        pub(crate) fn self_image(
            &self,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<SelfImagePin> {
            use windows_sys::Win32::System::Threading::{
                GetCurrentProcess, QueryFullProcessImageNameW,
            };
            let budget = proof.budget(self, deadline)?;
            let context = self.context.clone();
            self.owner.run(Dispatch::Observation, deadline, move || {
                context.validate(&budget)?;
                let mut buffer = vec![0u16; 32768];
                let mut size = buffer.len() as u32;
                // SAFETY: this process's pseudo-handle only; query own executing module path, no PID lookup.
                if unsafe {
                    QueryFullProcessImageNameW(
                        GetCurrentProcess(),
                        0,
                        buffer.as_mut_ptr(),
                        &mut size,
                    )
                } == 0
                {
                    return Err(native::last_error());
                }
                let path = String::from_utf16(&buffer[..size as usize])
                    .map_err(|_| NativeError::Unavailable)?;
                let (parent, leaf) = path.rsplit_once('\\').ok_or(NativeError::Unsupported)?;
                let parent = Arc::new(
                    Anchor::open(parent, &context.security, false, &budget)?
                        .ok_or(NativeError::Missing)?,
                );
                let image = parent.open_image(
                    leaf,
                    false,
                    env!("CARGO_PKG_VERSION"),
                    &context.security,
                    &budget,
                )?;
                Ok(SelfImagePin(Arc::new(OwnImage {
                    target: context.target.nonce,
                    parent,
                    leaf: leaf.into(),
                    image,
                })))
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
        // Additive private bridges preserve every A1/A2 constructor and original capability.
        // Exact new bridge: native production callers are excluded from lib-test roots.
        #[cfg_attr(test, allow(dead_code))]
        fn verify_stop_lock(
            &self,
            proof: &SupportProof,
            lock: &InstallerLock,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.lock_binding(proof, lock, deadline)?;
            let budget = proof.budget(self, deadline)?;
            let context = self.context.clone();
            let lease = lock.0.clone();
            self.owner.run(Dispatch::Observation, deadline, move || {
                validate_payload_lock(&context, &lease, &budget)
            })
        }
        // Exact new bridge: native production callers are excluded from lib-test roots.
        #[cfg_attr(test, allow(dead_code))]
        pub(crate) fn lease_stop_lock(
            self: &Arc<Self>,
            proof: &SupportProof,
            lock: &InstallerLock,
            deadline: &Deadline,
        ) -> NativeResult<Arc<StopLockLease>> {
            self.verify_stop_lock(proof, lock, deadline)?;
            Ok(Arc::new(StopLockLease {
                io: self.clone(),
                lock: InstallerLock(lock.0.clone()),
            }))
        }

        // Exact new bridge: native production callers are excluded from lib-test roots.
        #[cfg_attr(test, allow(dead_code))]
        pub(crate) fn bridge_nonce(
            &self,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<[u8; 16]> {
            proof.check(self, deadline)?;
            Ok(self.context.target.nonce)
        }
        // Exact new bridge: native production callers are excluded from lib-test roots.
        #[cfg_attr(test, allow(dead_code))]
        pub(crate) fn bound_clock(&self) -> Arc<dyn Clock> {
            self.clock.clone()
        }
        // Exact new bridge: native production callers are excluded from lib-test roots.
        #[cfg_attr(test, allow(dead_code))]
        pub(crate) fn own_process_identity(
            &self,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<process::own::OwnProcessIdentity> {
            let budget = proof.budget(self, deadline)?;
            let context = self.context.clone();
            self.owner.run(Dispatch::Observation, deadline, move || {
                context.validate(&budget)?;
                process::own::OwnProcessIdentity::current(&context.target.identity, &budget)
            })
        }
        // Exact new bridge: native production callers are excluded from lib-test roots.
        #[cfg_attr(test, allow(dead_code))]
        pub(crate) fn prepare_supervisor(
            self: &Arc<Self>,
            proof: &SupportProof,
            agent: &OpenedPe,
            root: &PayloadRoot,
            deadline: &Deadline,
        ) -> NativeResult<Arc<jobs::SupervisorOwner>> {
            let admission = JobAdmission {
                io: self.clone(),
                agent: OpenedPe(agent.0.clone()),
                root: PayloadRoot(root.0.clone()),
            };
            admission.reverify(proof, deadline)?;
            jobs::SupervisorOwner::prepare(admission, proof, deadline)
        }
        // Exact new bridge: native production callers are excluded from lib-test roots.
        #[cfg_attr(test, allow(dead_code))]
        pub(crate) fn clone_agent(
            &self,
            agent: &AgentObservation,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<AgentObservation> {
            proof.check(self, deadline)?;
            if agent.0.target != self.context.target.nonce {
                return Err(NativeError::Foreign);
            }
            Ok(AgentObservation(agent.0.clone()))
        }
        // Exact new bridge: native production callers are excluded from lib-test roots.
        #[cfg_attr(test, allow(dead_code))]
        pub(crate) fn agent_identity(
            &self,
            agent: &AgentObservation,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<FileIdentity> {
            if agent.0.target != self.context.target.nonce {
                return Err(NativeError::Foreign);
            }
            let budget = proof.budget(self, deadline)?;
            let context = self.context.clone();
            let observed = agent.0.clone();
            self.owner.run(Dispatch::Observation, deadline, move || {
                context.validate(&budget)?;
                observed.validate_pins(&context, &budget)?;
                Ok(observed.image_identity)
            })
        }
        // Exact new bridge: native production callers are excluded from lib-test roots.
        #[cfg_attr(test, allow(dead_code))]
        pub(crate) fn agent_generation(
            &self,
            agent: &AgentObservation,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<super::super::service::supervisor::Generation> {
            proof.check(self, deadline)?;
            if agent.0.target != self.context.target.nonce {
                return Err(NativeError::Foreign);
            }
            Ok(super::super::service::supervisor::Generation {
                pid: agent.0.bootstrap.pid,
                creation: agent.0.process.creation_time(),
                instance: agent.0.bootstrap.instance_id,
            })
        }
        // Exact new bridge: native production callers are excluded from lib-test roots.
        #[cfg_attr(test, allow(dead_code))]
        /// Read-only precommit support selection, never a terminal or payload-start capability.
        #[cfg(not(test))]
        pub(crate) fn outer_support_selection(
            &self,
            proof: &SupportProof,
            operation: [u8; 16],
            deadline: &Deadline,
        ) -> NativeResult<super::super::payload::recovery::OuterUpgradeRecord> {
            use super::super::payload::recovery::{OuterPhase, OuterUpgradeRecord};
            proof.budget(self, deadline)?.check()?;
            let record =
                OuterUpgradeRecord::read(self, proof, deadline)?.ok_or(NativeError::Missing)?;
            record.context().matches(self.target().identity())?;
            if operation == [0; 16]
                || record.operation() != operation
                || !matches!(record.phase(), OuterPhase::Prepared | OuterPhase::Ready)
            {
                return Err(NativeError::Foreign);
            }
            let selected =
                super::super::payload::recovery::selected_operation(self, proof, deadline)?
                    .ok_or(NativeError::Missing)?;
            if selected.operation() != operation || selected.phase() != Phase::Intent {
                return Err(NativeError::Foreign);
            }
            deadline.check()?;
            Ok(record)
        }
        #[cfg(not(test))]
        pub(crate) fn outer_stop_selection(
            &self,
            proof: &SupportProof,
            operation: [u8; 16],
            deadline: &Deadline,
        ) -> NativeResult<super::super::payload::recovery::OuterUpgradeRecord> {
            use super::super::payload::recovery::{OuterPhase, OuterUpgradeRecord};
            proof.budget(self, deadline)?.check()?;
            let record =
                OuterUpgradeRecord::read(self, proof, deadline)?.ok_or(NativeError::Missing)?;
            record.context().matches(self.target().identity())?;
            if operation == [0; 16]
                || record.operation() != operation
                || record.phase() != OuterPhase::Committed
            {
                return Err(NativeError::Foreign);
            }
            let selected =
                super::super::payload::recovery::selected_operation(self, proof, deadline)?
                    .ok_or(NativeError::Missing)?;
            if selected.operation() != operation || selected.phase() != Phase::StopIntent {
                return Err(NativeError::Foreign);
            }
            Ok(record)
        }
        #[cfg(not(test))]
        pub(crate) fn pin_outer_keeper(
            self: &Arc<Self>,
            proof: &SupportProof,
            operation: [u8; 16],
            deadline: &Deadline,
        ) -> NativeResult<OuterPeerImage> {
            use super::super::payload::recovery::{OuterPhase, OuterUpgradeRecord};
            let record =
                OuterUpgradeRecord::read(self, proof, deadline)?.ok_or(NativeError::Missing)?;
            record.context().matches(self.target().identity())?;
            if record.operation() != operation
                || !matches!(
                    record.phase(),
                    OuterPhase::Prepared
                        | OuterPhase::Ready
                        | OuterPhase::Committed
                        | OuterPhase::Complete
                )
            {
                return Err(NativeError::Foreign);
            }
            let expected = record.keeper_image().ok_or(NativeError::Foreign)?;
            let version = record.outer().image().version.clone();
            let context = self.context.clone();
            let io = self.clone();
            let budget = proof.budget(self, deadline)?;
            self.owner.run(Dispatch::Observation, deadline, move || {
                context.validate(&budget)?;
                let op: String = operation.iter().map(|byte| format!("{byte:02x}")).collect();
                let fixed = format!("{}\\payload-stage\\{op}", context.target.paths.install());
                let parent = Arc::new(
                    Anchor::open(&fixed, &context.security, true, &budget)?
                        .ok_or(NativeError::Missing)?,
                );
                let image = parent.open_image(
                    "keeper-copy.exe",
                    true,
                    &version,
                    &context.security,
                    &budget,
                )?;
                if epoch_stamp(image.identity) != expected || image.facts != *record.outer().image()
                {
                    return Err(NativeError::Foreign);
                }
                budget.check()?;
                Ok(OuterPeerImage {
                    io,
                    parent,
                    leaf: "keeper-copy.exe".into(),
                    image,
                    private: true,
                })
            })
        }
        /// The input can only be produced by the connected pipe's kernel peer factory.
        #[cfg(not(test))]
        pub(crate) fn pin_outer_kernel_image(
            self: &Arc<Self>,
            proof: &SupportProof,
            peer: &super::supervisor_owner::KernelOuterPeer,
            version: &str,
            deadline: &Deadline,
        ) -> NativeResult<OuterPeerImage> {
            peer.reverify(self, proof, deadline)?;
            let actual = peer.image().to_owned();
            let version = version.to_owned();
            let context = self.context.clone();
            let io = self.clone();
            let budget = proof.budget(self, deadline)?;
            let image = self.owner.run(Dispatch::Observation, deadline, move || {
                context.validate(&budget)?;
                let (parent, leaf) = actual.rsplit_once('\\').ok_or(NativeError::Unsupported)?;
                let parent = Arc::new(
                    Anchor::open(parent, &context.security, false, &budget)?
                        .ok_or(NativeError::Missing)?,
                );
                let image = parent.open_image(leaf, false, &version, &context.security, &budget)?;
                budget.check()?;
                Ok(OuterPeerImage {
                    io,
                    parent,
                    leaf: leaf.into(),
                    image,
                    private: false,
                })
            })?;
            peer.reverify(self, proof, deadline)?;
            Ok(image)
        }
        #[cfg(not(test))]
        pub(crate) fn prepare_outer_completion(
            self: &Arc<Self>,
            proof: &SupportProof,
            lock: &InstallerLock,
            operation: [u8; 16],
            deadline: &Deadline,
        ) -> NativeResult<OuterCompletionAdmission> {
            self.verify_stop_lock(proof, lock, deadline)?;
            self.outer_stop_selection(proof, operation, deadline)?;
            let module = self.self_image(proof, deadline)?;
            let own = self.own_process_identity(proof, deadline)?;
            let keeper = self.pin_outer_keeper(proof, operation, deadline)?;
            let admission = OuterCompletionAdmission {
                io: self.clone(),
                module,
                own,
                keeper,
                operation,
            };
            admission.reverify(self, proof, deadline)?;
            self.verify_stop_lock(proof, lock, deadline)?;
            Ok(admission)
        }
        pub(crate) fn broker_admission(
            self: &Arc<Self>,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<Arc<BrokerAdmission>> {
            let budget = proof.budget(self, deadline)?;
            let context = self.context.clone();
            let io = self.clone();
            self.owner.run(Dispatch::Observation, deadline, move || {
                context.validate(&budget)?;
                let parent = Arc::new(
                    Anchor::open(
                        &format!("{}\\Crosspane", context.target.paths.local()),
                        &context.security,
                        true,
                        &budget,
                    )?
                    .ok_or(NativeError::Missing)?,
                );
                let expected_runtime = format!(
                    "{}\\runtime",
                    parent
                        .canonical_dos_path(&context.security, &budget)?
                        .to_str()
                        .ok_or(NativeError::Unsupported)?
                );
                let runtime = Anchor::open(&expected_runtime, &context.security, true, &budget)?
                    .map(Arc::new);
                let install = Arc::new(
                    Anchor::open(
                        context.target.paths.install(),
                        &context.security,
                        false,
                        &budget,
                    )?
                    .ok_or(NativeError::Missing)?,
                );
                let canonical = &expected_runtime;
                let (installer, installer_identity) = install.open_file_metadata(
                    &PrivateName::new("crosspane-installer.exe")?,
                    &context.security,
                    &budget,
                )?;
                let installer_path = format!(
                    "{}\\crosspane-installer.exe",
                    install
                        .canonical_dos_path(&context.security, &budget)?
                        .to_str()
                        .ok_or(NativeError::Unsupported)?
                );
                let mut key = canonical.as_bytes().to_vec();
                key.extend_from_slice(context.target.identity.user.bytes());
                key.extend_from_slice(context.target.identity.logon.bytes());
                key.extend_from_slice(&context.target.identity.session.to_le_bytes());
                let endpoint = format!(
                    r"\\.\pipe\Crosspane.Installer.Supervisor.{:016x}",
                    xxhash_rust::xxh3::xxh3_64(&key)
                );
                Ok(Arc::new(BrokerAdmission {
                    io,
                    runtime: Mutex::new(runtime),
                    parent,
                    expected_runtime,
                    install,
                    installer: Mutex::new(Some(Arc::new(installer))),
                    installer_identity,
                    installer_path,
                    endpoint,
                }))
            })
        }

        #[cfg(not(test))]
        fn query_prior_logon(
            &self,
            proof: &SupportProof,
            prior: &MatchedLogonProvenance,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if !std::ptr::eq(self, prior.io.as_ref())
                || prior.epoch.user() != self.context.target.identity.user.sddl()
                || prior.epoch.authentication_id() == self.context.target.identity.authentication_id
                || prior.epoch.logon_sid() == self.context.target.identity.logon.bytes()
                || LOGON_QUERY_QUARANTINE.get().is_some()
            {
                return Err(NativeError::Foreign);
            }
            prior.epoch.validate()?;
            let budget = proof.budget(self, deadline)?;
            let context = self.context.clone();
            let owner = self.owner.clone();
            let authentication_id = prior.epoch.authentication_id();
            self.owner.run(Dispatch::Observation, deadline, move || {
                query_ended_logon(&context, &owner, authentication_id, &budget)
            })?;
            proof.budget(self, deadline)?.check()
        }
        #[cfg(not(test))]
        pub(crate) fn observe_prior_logon(
            self: &Arc<Self>,
            proof: &SupportProof,
            prior: &MatchedLogonProvenance,
            deadline: &Deadline,
        ) -> NativeResult<PriorLogonDisposition> {
            self.query_prior_logon(proof, prior, deadline)?;
            // Construction still checks the ORIGINAL observation proof; fresh publication
            // budgets may renew only a seal that was actually delivered before this boundary.
            proof.budget(self, deadline)?.check()?;
            Ok(PriorLogonDisposition {
                io: self.clone(),
                kind: PriorLogonKind::SessionGone(MatchedLogonProvenance {
                    io: self.clone(),
                    epoch: prior.epoch.clone(),
                }),
            })
        }
        #[cfg(not(test))]
        pub(crate) fn prepare_logon_reservation(
            self: &Arc<Self>,
            proof: &SupportProof,
            lock: &InstallerLock,
            owner: &Arc<super::jobs::SupervisorOwner>,
            deadline: &Deadline,
        ) -> NativeResult<(LogonReservation, PriorLogonDisposition)> {
            self.verify_stop_lock(proof, lock, deadline)?;
            let namespace = owner.exclusive_lease(proof, deadline)?;
            namespace.reverify(self, proof, deadline)?;
            let own = self.own_process_identity(proof, deadline)?;
            let context = self.context.target.identity.clone();
            let current = self.read_record(
                proof,
                records::RecordName::Supervisor,
                files::MAX_RECORD_BYTES,
                deadline,
            )?;
            let provenance = super::activation::SupervisorLogonRecord::read(self, proof, deadline)?;
            let disposition = match (current, provenance) {
                (None, None) => {
                    let task =
                        super::activation::TaskActivationRecord::read(self, proof, deadline)?;
                    let first = super::epoch_archive::first_logon_absence(
                        task.as_ref(),
                        false,
                        false,
                        read_archive_intent(self, proof, deadline)?.is_some(),
                        archive_history_present(self, proof, deadline)?,
                    );
                    if let Err(error) = first {
                        return Err(if error == NativeError::Unsupported {
                            legacy_provenance_required()
                        } else {
                            error
                        });
                    }
                    PriorLogonDisposition {
                        io: self.clone(),
                        kind: PriorLogonKind::FirstAbsent,
                    }
                }
                (Some(source), Some(record)) => {
                    let prior = super::super::service::journal::Journal::decode(source.bytes())?;
                    let epoch = record.matches_current(&prior)?.clone();
                    let matched = MatchedLogonProvenance {
                        io: self.clone(),
                        epoch,
                    };
                    self.observe_prior_logon(proof, &matched, deadline)?
                }
                _ => return Err(legacy_provenance_required()),
            };
            self.verify_stop_lock(proof, lock, deadline)?;
            namespace.reverify(self, proof, deadline)?;
            own.reverify(deadline)?;
            Ok((
                LogonReservation {
                    io: self.clone(),
                    namespace,
                    own,
                    context,
                    epoch_prepared: std::sync::OnceLock::new(),
                    epoch: std::sync::OnceLock::new(),
                },
                disposition,
            ))
        }

        #[cfg(not(test))]
        pub(crate) fn prepare_supervisor_epoch(
            self: &Arc<Self>,
            proof: &SupportProof,
            lock: &InstallerLock,
            permit: &Arc<super::super::service::task::TaskRunPermit>,
            owner: &Arc<super::jobs::SupervisorOwner>,
            deadline: &Deadline,
        ) -> NativeResult<OwnedArchiveResult> {
            self.verify_stop_lock(proof, lock, deadline)?;
            permit.reverify(self, proof, deadline)?;
            let namespace = owner.exclusive_lease(proof, deadline)?;
            namespace.reverify(self, proof, deadline)?;
            let source = self.read_record(
                proof,
                records::RecordName::Supervisor,
                files::MAX_RECORD_BYTES,
                deadline,
            )?;
            self.check_completed_archive_intent(proof, deadline)?;
            let selected =
                super::super::payload::recovery::selected_operation(self, proof, deadline)?;
            let Some(source) = source else {
                if read_archive_intent(self, proof, deadline)?.is_some()
                    || selected.is_some()
                    || super::activation::SupervisorLogonRecord::read(self, proof, deadline)?
                        .is_some()
                {
                    return Err(NativeError::Foreign);
                }
                publish_epoch_preparing(
                    self,
                    proof,
                    lock,
                    EpochClaim::Task(permit).epoch(self)?,
                    None,
                    deadline,
                )?;
                let result = OwnedArchiveResult::First(FirstSupervisorEpoch {
                    io: self.clone(),
                    namespace,
                    permit: permit.clone(),
                });
                result.reverify(self, proof, lock, permit, owner, deadline)?;
                return Ok(result);
            };
            let prior = super::super::service::journal::Journal::decode(source.bytes())?;
            let lineage = super::activation::correlate_upgrade(
                permit.operation(),
                permit.user(),
                selected.as_ref(),
                Some(&prior),
            )?
            .ok_or(NativeError::Foreign)?;
            self.archive_supervisor_epoch(
                proof, lock, namespace, permit, &lineage, &prior, source, deadline,
            )
        }
        #[cfg(not(test))]
        fn check_completed_archive_intent(
            &self,
            proof: &SupportProof,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            if let Some(intent) = read_archive_intent(self, proof, deadline)? {
                intent.require_complete()?;
                let target = self
                    .read_record(
                        proof,
                        records::RecordName::SupervisorEpoch(intent.slot),
                        files::MAX_RECORD_BYTES,
                        deadline,
                    )?
                    .ok_or(NativeError::Foreign)?;
                if target.identity != epoch_identity(intent.source)
                    || epoch_hash(target.bytes()) != intent.sha256
                {
                    return Err(NativeError::Foreign);
                }
                super::super::service::journal::Journal::decode(target.bytes())?;
            }
            Ok(())
        }
        #[cfg(not(test))]
        // Independent admitted IO/lock/namespace/claim/lineage/source/deadline capabilities
        // remain explicit; the old task/upgrade entry retains its exact signature and authority.
        #[allow(clippy::too_many_arguments)]
        fn archive_supervisor_epoch(
            self: &Arc<Self>,
            proof: &SupportProof,
            lock: &InstallerLock,
            namespace: Arc<super::supervisor_owner::ExclusiveSupervisorLease>,
            permit: &Arc<super::super::service::task::TaskRunPermit>,
            lineage: &super::activation::UpgradeLineage,
            prior: &super::super::service::journal::Journal,
            source: records::ObservedRecord,
            deadline: &Deadline,
        ) -> NativeResult<OwnedArchiveResult> {
            if !lineage.matches_predecessor(prior) || lineage.operation() != permit.operation() {
                return Err(NativeError::Foreign);
            }
            let claim = EpochClaim::Task(permit);
            claim.renew_predecessor(self, proof, prior, deadline)?;
            let files =
                self.archive_epoch_files(proof, lock, &namespace, claim, prior, &source, deadline)?;
            publish_epoch_preparing(
                self,
                proof,
                lock,
                claim.epoch(self)?,
                Some(&files.intent),
                deadline,
            )?;
            Ok(OwnedArchiveResult::Archived(ArchivedSupervisorEpoch {
                io: self.clone(),
                namespace,
                permit: permit.clone(),
                intent: files.intent,
                bytes: files.bytes,
            }))
        }
        #[cfg(not(test))]
        pub(crate) fn prepare_logon_epoch(
            self: &Arc<Self>,
            proof: &SupportProof,
            lock: &InstallerLock,
            permit: &Arc<super::super::service::SupervisorLogonPermit>,
            owner: &Arc<super::jobs::SupervisorOwner>,
            deadline: &Deadline,
        ) -> NativeResult<LogonArchiveResult> {
            self.verify_stop_lock(proof, lock, deadline)?;
            permit.reverify(self, proof, deadline)?;
            let namespace = owner.exclusive_lease(proof, deadline)?;
            if !Arc::ptr_eq(&namespace, &permit.reservation().namespace) {
                return Err(NativeError::Foreign);
            }
            namespace.reverify(self, proof, deadline)?;
            self.check_completed_archive_intent(proof, deadline)?;
            let claim = EpochClaim::Logon(permit);
            let source = self.read_record(
                proof,
                records::RecordName::Supervisor,
                files::MAX_RECORD_BYTES,
                deadline,
            )?;
            let archived = match (source, &permit.disposition().kind) {
                (None, PriorLogonKind::FirstAbsent) => {
                    let task =
                        super::activation::TaskActivationRecord::read(self, proof, deadline)?;
                    let first = super::epoch_archive::first_logon_absence(
                        task.as_ref(),
                        false,
                        super::activation::SupervisorLogonRecord::read(self, proof, deadline)?
                            .is_some(),
                        read_archive_intent(self, proof, deadline)?.is_some(),
                        archive_history_present(self, proof, deadline)?,
                    );
                    if let Err(error) = first {
                        return Err(if error == NativeError::Unsupported {
                            legacy_provenance_required()
                        } else {
                            error
                        });
                    }
                    None
                }
                (Some(source), PriorLogonKind::SessionGone(_)) => {
                    let prior = super::super::service::journal::Journal::decode(source.bytes())?;
                    claim.renew_predecessor(self, proof, &prior, deadline)?;
                    Some(self.archive_epoch_files(
                        proof, lock, &namespace, claim, &prior, &source, deadline,
                    )?)
                }
                _ => return Err(NativeError::Foreign),
            };
            publish_epoch_preparing(
                self,
                proof,
                lock,
                claim.epoch(self)?,
                archived.as_ref().map(|files| &files.intent),
                deadline,
            )?;
            let result = LogonArchiveResult {
                io: self.clone(),
                namespace,
                permit: permit.clone(),
                archived,
            };
            result.reverify(self, proof, lock, permit, owner, deadline)?;
            // Only this actual completed factory can mint the dispatcher seal. It retains exact
            // Preparing/archive observations; a caller flag or a cold record cannot mint it.
            let provenance = verify_logon_preparing_files(
                self,
                proof,
                &claim.epoch(self)?,
                result.archived.as_ref(),
                deadline,
            )?;
            let preparing = self
                .read_record(
                    proof,
                    records::RecordName::SupervisorLogon,
                    files::MAX_RECORD_BYTES,
                    deadline,
                )?
                .ok_or(NativeError::Foreign)?;
            if super::activation::SupervisorLogonRecord::decode(preparing.bytes())? != provenance {
                return Err(NativeError::Foreign);
            }
            permit
                .reservation()
                .epoch_prepared
                .set(PreparedLogonGate {
                    provenance,
                    preparing_identity: preparing.identity,
                    preparing_bytes: preparing.bytes().to_vec(),
                    archived: result.archived.clone(),
                })
                .map_err(|_| NativeError::Foreign)?;
            Ok(result)
        }
        #[cfg(not(test))]
        // Shared exact effects keep each independently revalidated admission explicit.
        #[allow(clippy::too_many_arguments)]
        fn archive_epoch_files(
            self: &Arc<Self>,
            proof: &SupportProof,
            lock: &InstallerLock,
            namespace: &Arc<super::supervisor_owner::ExclusiveSupervisorLease>,
            claim: EpochClaim<'_>,
            prior: &super::super::service::journal::Journal,
            source: &records::ObservedRecord,
            deadline: &Deadline,
        ) -> NativeResult<ArchivedEpochFiles> {
            use super::epoch_archive::{ArchiveIntent, ArchivePhase};
            self.verify_stop_lock(proof, lock, deadline)?;
            namespace.reverify(self, proof, deadline)?;
            claim.reverify(self, proof, deadline)?;
            claim.renew_predecessor(self, proof, prior, deadline)?;
            let (slot, victim) = archive_slots(self, proof, deadline)?;
            let mut intent = ArchiveIntent {
                schema_version: 1,
                operation: claim.operation(),
                owner_creation: claim.owner_creation(),
                slot,
                source: epoch_stamp(source.identity),
                sha256: epoch_hash(source.bytes()),
                victim,
                phase: if victim.is_some() {
                    ArchivePhase::PruneIntent
                } else {
                    ArchivePhase::MoveIntent
                },
            };
            let mut driver = NativeEpochArchive {
                io: self,
                proof,
                lock,
                namespace,
                claim,
                prior,
                source,
                intent: &mut intent,
                deadline,
            };
            super::epoch_archive::ArchiveSequence::default()
                .run_once(&mut driver, victim.is_some())?;
            Ok(ArchivedEpochFiles {
                intent,
                bytes: source.bytes().to_vec(),
            })
        }
        /// Publishes the fixed correlation BEFORE any random operation record is created.
        #[cfg(not(test))]
        pub(crate) fn begin_outer_operation(
            self: &Arc<Self>,
            proof: &SupportProof,
            lock: &InstallerLock,
            operation: [u8; 16],
            sources: &super::super::payload::ApprovedOuterSources,
            deadline: &Deadline,
        ) -> NativeResult<SelectedOuterOperation> {
            use super::super::payload::{
                inventory::{ApprovedInventory, ApprovedPe},
                recovery::{
                    self, OperationRecord, OuterContextCorrelation, OuterPhase,
                    OuterProcessCorrelation, OuterUpgradeRecord,
                },
            };
            self.verify_stop_lock(proof, lock, deadline)?;
            let module = self.self_image(proof, deadline)?;
            let own = self.own_process_identity(proof, deadline)?;
            let inventory = ApprovedInventory::embedded()?;
            let installer = ApprovedPe::own_image(&module)?;
            inventory.check_staging_budget(&installer, true)?;
            for (role, bytes) in sources.roles() {
                super::super::payload::verify_outer_source(
                    bytes,
                    inventory.role(*role)?,
                    deadline,
                )?;
            }
            if recovery::catalog(self, proof, deadline)?.active.is_some() {
                return Err(NativeError::Busy);
            }
            if let Some(prior) = OuterUpgradeRecord::read(self, proof, deadline)? {
                if !matches!(prior.phase(), OuterPhase::Complete | OuterPhase::Cancelled) {
                    return Err(NativeError::Busy);
                }
                // Only the admitted native cleanup can publish positive copy absence. Terminal
                // phase or an uncreated-image observation alone never settles an old fixed leaf.
                if prior.copy_cleanup() != super::super::payload::recovery::OuterCopyCleanup::Absent
                {
                    return Err(NativeError::Busy);
                }
                // Only terminal metadata is replaced; a changed logon is not old-owner proof.
                // Fresh current self/manifest/lock and later kernel reservation remain mandatory.
                prior.context().same_user(&self.context.target.identity)?;
            }
            let record = OuterUpgradeRecord::new(
                operation,
                OuterProcessCorrelation::new(
                    own.pid(),
                    own.creation(),
                    outer_stamp(module.identity()),
                    module.facts().clone(),
                )?,
                OuterContextCorrelation::new(&self.context.target.identity)?,
                sources.facts().clone(),
            )?;
            struct Publisher<'a> {
                io: &'a Arc<WindowsNativeIo>,
                proof: &'a SupportProof,
                lock: &'a InstallerLock,
                deadline: &'a Deadline,
            }
            impl recovery::OuterSelectionPort for Publisher<'_> {
                fn publish_selection(&mut self, record: &OuterUpgradeRecord) -> NativeResult<()> {
                    self.io
                        .publish_outer_operation(self.proof, self.lock, record, self.deadline)
                }
                fn create_operation(&mut self, record: &OperationRecord) -> NativeResult<()> {
                    recovery::save_operation(
                        self.io.clone(),
                        self.proof,
                        self.lock,
                        record,
                        self.deadline,
                    )?;
                    Ok(())
                }
            }
            recovery::publish_outer_selection(
                &mut Publisher {
                    io: self,
                    proof,
                    lock,
                    deadline,
                },
                &record,
            )?;
            let selection = SelectedOuterOperation {
                io: self.clone(),
                module,
                process: own,
                record,
            };
            selection.reverify(self, proof, lock, deadline)?;
            Ok(selection)
        }
        /// A new caller may observe the live selected keeper; this creates only its OWN context
        /// selection and cannot reconstruct an earlier process/job completion or launch permit.
        #[cfg(not(test))]
        pub(crate) fn select_outer_operation(
            self: &Arc<Self>,
            proof: &SupportProof,
            lock: &InstallerLock,
            deadline: &Deadline,
        ) -> NativeResult<SelectedOuterOperation> {
            let record = self.observe_outer_operation(proof, lock, deadline)?;
            let module = self.self_image(proof, deadline)?;
            let process = self.own_process_identity(proof, deadline)?;
            let selected = SelectedOuterOperation {
                io: self.clone(),
                module,
                process,
                record,
            };
            selected.reverify(self, proof, lock, deadline)?;
            Ok(selected)
        }
        /// The fixed record selects metadata; the caller still owns actual original context/lock.
        #[cfg(not(test))]
        pub(crate) fn observe_outer_operation(
            &self,
            proof: &SupportProof,
            lock: &InstallerLock,
            deadline: &Deadline,
        ) -> NativeResult<super::super::payload::recovery::OuterUpgradeRecord> {
            use super::super::payload::recovery::{self, OuterUpgradeRecord, Phase};
            self.verify_stop_lock(proof, lock, deadline)?;
            let record =
                OuterUpgradeRecord::read(self, proof, deadline)?.ok_or(NativeError::Missing)?;
            record.context().matches(&self.context.target.identity)?;
            let catalog = recovery::catalog(self, proof, deadline)?;
            if catalog.active == Some(record.operation()) {
                let selected = recovery::selected_operation(self, proof, deadline)?
                    .ok_or(NativeError::Foreign)?;
                if selected.operation() != record.operation() {
                    return Err(NativeError::Foreign);
                }
            } else if catalog.active.is_none() {
                // Only exact completed/cancelled payload observations permit post-action recording.
                let observed = self
                    .read_record(
                        proof,
                        records::RecordName::Operation(record.operation()),
                        files::MAX_RECORD_BYTES,
                        deadline,
                    )?
                    .ok_or(NativeError::Foreign)?;
                let operation: recovery::OperationRecord = records::record_data(
                    &records::RecordName::Operation(record.operation()),
                    observed.bytes(),
                )?;
                operation.validate()?;
                if operation.operation() != record.operation()
                    || !matches!(operation.phase(), Phase::Complete | Phase::RolledBack)
                {
                    return Err(NativeError::Foreign);
                }
            } else {
                return Err(NativeError::Busy);
            }
            Ok(record)
        }
        #[cfg(not(test))]
        pub(super) fn publish_outer_operation(
            &self,
            proof: &SupportProof,
            lock: &InstallerLock,
            record: &super::super::payload::recovery::OuterUpgradeRecord,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            self.verify_stop_lock(proof, lock, deadline)?;
            record.validate()?;
            record.context().matches(&self.context.target.identity)?;
            let bytes = records::encode_record(
                &records::RecordName::OuterUpgrade,
                serde_json::to_value(record).map_err(|_| NativeError::Invalid)?,
            )?;
            let outcome = self.publish_record(
                proof,
                lock,
                records::RecordName::OuterUpgrade,
                &bytes,
                deadline,
            )?;
            if outcome.state != records::PublicationRecovery::NewPublished
                || outcome.native_failure.is_some()
            {
                return Err(NativeError::OutcomeUnknown);
            }
            Ok(())
        }
        /// Actual copied module and own inherited-process correlation, never a journal PID open.
        #[cfg(not(test))]
        pub(crate) fn keeper_selection(
            self: &Arc<Self>,
            proof: &SupportProof,
            lock: &InstallerLock,
            deadline: &Deadline,
        ) -> NativeResult<SelectedOuterOperation> {
            let record = self.observe_outer_operation(proof, lock, deadline)?;
            let module = self.self_image(proof, deadline)?;
            let process = self.own_process_identity(proof, deadline)?;
            let fixed =
                self.open_keeper_image(proof, lock, record.operation(), &module, deadline)?;
            if fixed.identity() != module.identity()
                || Some(outer_stamp(fixed.identity())) != record.keeper_image()
            {
                return Err(NativeError::Foreign);
            }
            record.peer_matches(
                process.pid(),
                process.creation(),
                outer_stamp(module.identity()),
                module.facts(),
                &self.context.target.identity,
            )?;
            drop(fixed);
            let selected = SelectedOuterOperation {
                io: self.clone(),
                module,
                process,
                record,
            };
            selected.reverify(self, proof, lock, deadline)?;
            Ok(selected)
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

    // A4d fixed keeper source/launch. Every capability retains ORIGINAL IO and actual objects;
    // record values only correlate the returned process/module and cannot select a process/path.
    #[cfg(not(test))]
    pub(crate) mod keeper {
        use super::super::super::payload::{
            ApprovedOuterSources,
            inventory::ApprovedInventory,
            recovery::{OuterPhase, OuterProcessCorrelation, OuterUpgradeRecord},
        };
        use super::super::activation::KeeperStage;
        use super::*;
        use std::os::windows::io::{FromRawHandle, OwnedHandle};
        use std::sync::{
            OnceLock,
            atomic::{AtomicBool, Ordering},
        };
        use windows_sys::Win32::{
            Foundation::*,
            Security::Authorization::*,
            Security::*,
            System::{JobObjects::*, Pipes::*, Threading::*},
        };
        pub(crate) const KEEPER_ARGUMENT: &str = "--windows-upgrade-keeper";
        const CHUNK: usize = 64 * 1024;

        fn locked_record(
            context: &Context,
            lease: &LockState,
            deadline: &Deadline,
        ) -> NativeResult<OuterUpgradeRecord> {
            validate_payload_lock(context, lease, deadline)?;
            let (_, bytes) = lease
                .parent
                .read_private(
                    &records::RecordName::OuterUpgrade.file_name()?,
                    &context.security,
                    files::MAX_RECORD_BYTES,
                    deadline,
                )?
                .ok_or(NativeError::Missing)?;
            let record = OuterUpgradeRecord::decode(&bytes)?;
            record.context().matches(&context.target.identity)?;
            Ok(record)
        }
        impl WindowsNativeIo {
            pub(crate) fn prepare_keeper_image(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                selected: &SelectedOuterOperation,
                deadline: &Deadline,
            ) -> NativeResult<OpenedPe> {
                selected.reverify(self, proof, lock, deadline)?;
                selected.mark_preparing(proof, lock, deadline)?;
                let proof = self.admit_support(deadline)?;
                let root = self.payload_root(&proof, lock, deadline)?;
                let expected = ApprovedPe::own_image(selected.module())?;
                let input = self.self_image_reader(selected.module(), &proof, deadline)?;
                let original = selected.record().clone();
                let context = self.context.clone();
                let lease = lock.0.clone();
                let root = root.0.clone();
                let budget = proof.budget(self, deadline)?;
                let image = self.owner.run(Dispatch::Mutation, deadline, move || {
                    let change = Change::new();
                    change.finish((|| {
                        let actual = locked_record(&context, &lease, &budget)?;
                        if !actual.selection_matches(&original)
                            || actual.phase() != OuterPhase::Preparing
                            || actual.keeper_image().is_some()
                        {
                            return Err(NativeError::Foreign);
                        }
                        let root = PayloadRoot(root).ensure(&context, &budget, &change)?;
                        let stage = ensure_payload_child(
                            &root,
                            "payload-stage",
                            &context,
                            &budget,
                            &change,
                        )?;
                        let parent = Arc::new(ensure_payload_child(
                            &stage,
                            &records::hex(&actual.operation()),
                            &context,
                            &budget,
                            &change,
                        )?);
                        change.reached();
                        let image = parent.stage_image(
                            "keeper-copy.exe",
                            input,
                            &expected,
                            &context.security,
                            &budget,
                        )?;
                        Ok(OpenedPe(Arc::new(ApprovedImage {
                            target: context.target.nonce,
                            parent,
                            leaf: "keeper-copy.exe".into(),
                            image,
                            expected,
                        })))
                    })())
                })?;
                let proof = self.admit_support(deadline)?;
                selected.record_keeper_image(&image, &proof, lock, deadline)?;
                Ok(image)
            }
            pub(crate) fn open_keeper_image(
                &self,
                proof: &SupportProof,
                lock: &InstallerLock,
                operation: [u8; 16],
                own: &SelfImagePin,
                deadline: &Deadline,
            ) -> NativeResult<OpenedPe> {
                self.lock_binding(proof, lock, deadline)?;
                own.reverify(self, proof, deadline)?;
                if operation == [0; 16] {
                    return Err(NativeError::Invalid);
                }
                let root = self.payload_root(proof, lock, deadline)?;
                let context = self.context.clone();
                let lease = lock.0.clone();
                let root = root.0.clone();
                let expected = ApprovedPe::own_image(own)?;
                let budget = proof.budget(self, deadline)?;
                self.owner.run(Dispatch::Observation, deadline, move || {
                    let actual = locked_record(&context, &lease, &budget)?;
                    if actual.operation() != operation {
                        return Err(NativeError::Foreign);
                    }
                    let root = PayloadRoot(root)
                        .check(&context, &budget)?
                        .ok_or(NativeError::Missing)?;
                    let parent = root
                        .child("payload-stage", &context.security, &budget)?
                        .ok_or(NativeError::Missing)?;
                    let parent = Arc::new(
                        parent
                            .child(&records::hex(&operation), &context.security, &budget)?
                            .ok_or(NativeError::Missing)?,
                    );
                    let image = parent.open_image(
                        "keeper-copy.exe",
                        true,
                        expected.version(),
                        &context.security,
                        &budget,
                    )?;
                    if image.facts != *expected.facts()
                        || Some(outer_stamp(image.identity)) != actual.keeper_image()
                    {
                        return Err(NativeError::Foreign);
                    }
                    Ok(OpenedPe(Arc::new(ApprovedImage {
                        target: context.target.nonce,
                        parent,
                        leaf: "keeper-copy.exe".into(),
                        image,
                        expected,
                    })))
                })
            }
        }
        fn process_creation(process: &OwnedHandle) -> NativeResult<u64> {
            let (mut a, mut b, mut c, mut d) = (
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
            );
            // SAFETY: actual retained process query object and complete distinct FILETIME outputs.
            if unsafe { GetProcessTimes(process.as_raw_handle(), &mut a, &mut b, &mut c, &mut d) }
                == 0
            {
                return Err(NativeError::Unavailable);
            }
            let result = (u64::from(a.dwHighDateTime) << 32) | u64::from(a.dwLowDateTime);
            if result == 0 {
                Err(NativeError::Foreign)
            } else {
                Ok(result)
            }
        }
        pub(crate) struct KeeperParent {
            handle: Arc<OwnedHandle>,
            pid: u32,
            created: u64,
            token: identity::TokenFacts,
        }
        impl KeeperParent {
            pub(crate) fn admit(
                io: &WindowsNativeIo,
                selected: &SelectedOuterOperation,
                deadline: &Deadline,
            ) -> NativeResult<Self> {
                deadline.check()?;
                let value = selected
                    .record()
                    .inherited_parent_handle()
                    .ok_or(NativeError::Foreign)?;
                let raw = value as usize as std::os::windows::io::RawHandle;
                let mut flags = 0;
                // SAFETY: this value is ONLY an alleged inherited handle; query it before adoption.
                // No PID lookup/path reopen or use of an unchecked value for a mutation occurs.
                if unsafe { GetHandleInformation(raw, &mut flags) } == 0
                    || flags & HANDLE_FLAG_INHERIT == 0
                {
                    return Err(NativeError::Foreign);
                }
                // SAFETY: the checked live inherited object must be a real process; zero refuses.
                let pid = unsafe { GetProcessId(raw) };
                if pid == 0 || pid != selected.record().outer().pid() {
                    return Err(NativeError::Foreign);
                }
                // SAFETY: only the actual validated inherited real process handle is now owned.
                let handle = Arc::new(unsafe { OwnedHandle::from_raw_handle(raw) });
                let created = process_creation(&handle)?;
                let token = identity::native::observe_process(&handle)?;
                identity::LimitedIdentity::admit(token.clone())?;
                if token != io.context.target.identity
                    || created != selected.record().outer().creation()
                {
                    return Err(NativeError::Foreign);
                }
                let parent = Self {
                    handle,
                    pid,
                    created,
                    token,
                };
                parent.reverify(deadline)?;
                Ok(parent)
            }
            pub(crate) fn handle(&self) -> &Arc<OwnedHandle> {
                &self.handle
            }
            pub(crate) fn reverify(&self, deadline: &Deadline) -> NativeResult<()> {
                deadline.check()?;
                // SAFETY: retained original inherited process object, nonblocking observation.
                if unsafe { WaitForSingleObject(self.handle.as_raw_handle(), 0) } != WAIT_TIMEOUT
                    // SAFETY: query the SAME already retained inherited process object.
                    || unsafe { GetProcessId(self.handle.as_raw_handle()) } != self.pid
                    || process_creation(&self.handle)? != self.created
                    || identity::native::observe_process(&self.handle)? != self.token
                {
                    return Err(NativeError::Foreign);
                }
                deadline.check()
            }
        }
        struct Attributes {
            storage: Vec<usize>,
            handles: Box<[HANDLE; 1]>,
            initialized: bool,
        }
        impl Attributes {
            fn new(parent: HANDLE) -> NativeResult<Self> {
                let mut bytes = 0;
                // SAFETY: documented size query only; no native list exists yet.
                unsafe {
                    InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut bytes)
                };
                if bytes == 0 || bytes > CHUNK {
                    return Err(NativeError::Unavailable);
                }
                let mut a = Self {
                    storage: vec![0; bytes.div_ceil(std::mem::size_of::<usize>())],
                    handles: Box::new([parent]),
                    initialized: false,
                };
                // SAFETY: aligned size-query allocation retained through CreateProcess.
                if unsafe { InitializeProcThreadAttributeList(a.pointer(), 1, 0, &mut bytes) } == 0
                {
                    return Err(NativeError::Unavailable);
                }
                a.initialized = true;
                // SAFETY: exactly one actual inheritable own-parent process handle, boxed so its
                // address remains stable for the full native list lifetime; no other inheritance.
                if unsafe {
                    UpdateProcThreadAttribute(
                        a.pointer(),
                        0,
                        PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                        a.handles.as_ptr().cast(),
                        std::mem::size_of::<[HANDLE; 1]>(),
                        std::ptr::null_mut(),
                        std::ptr::null(),
                    )
                } == 0
                {
                    return Err(NativeError::Unavailable);
                }
                Ok(a)
            }
            fn pointer(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
                self.storage.as_mut_ptr().cast()
            }
        }
        impl Drop for Attributes {
            fn drop(&mut self) {
                if self.initialized {
                    // SAFETY: balances the successful initialization of this exact retained list.
                    unsafe { DeleteProcThreadAttributeList(self.pointer()) };
                }
            }
        }
        struct ChildState {
            process: Option<Arc<OwnedHandle>>,
            thread: Option<Arc<OwnedHandle>>,
            dispatched: bool,
            create_returned: bool,
            known_created: bool,
            resuming: bool,
            resumed: bool,
            cleanup_started: bool,
            retired: bool,
        }
        struct LaunchOwner {
            io: Arc<WindowsNativeIo>,
            selected: Mutex<SelectedOuterOperation>,
            lock: Mutex<Option<InstallerLock>>,
            image: Mutex<Option<OpenedPe>>,
            parent: Arc<OwnedHandle>,
            sources: ApprovedOuterSources,
            calls: Arc<CallOwner>,
            cleanup: Arc<CallOwner>,
            state: Mutex<ChildState>,
            monitor_started: AtomicBool,
            cancel_requested: AtomicBool,
        }
        static LAUNCH: OnceLock<Mutex<Option<Arc<LaunchOwner>>>> = OnceLock::new();
        pub(crate) struct PreparedKeeper(Arc<LaunchOwner>);
        pub(crate) struct KeeperChild(Arc<LaunchOwner>);
        impl PreparedKeeper {
            pub(crate) fn prepare(
                io: Arc<WindowsNativeIo>,
                proof: &SupportProof,
                lock: &InstallerLock,
                selected: &SelectedOuterOperation,
                sources: ApprovedOuterSources,
                deadline: &Deadline,
            ) -> NativeResult<Self> {
                selected.reverify(&io, proof, lock, deadline)?;
                if sources.facts() != selected.record().sources() {
                    return Err(NativeError::Foreign);
                }
                let image = io.prepare_keeper_image(proof, lock, selected, deadline)?;
                let proof = io.admit_support(deadline)?;
                let process = io.own_process_identity(&proof, deadline)?;
                let source = process.handle();
                let mut raw = std::ptr::null_mut();
                // SAFETY: duplicate ONLY own actual process into this process, query/sync and
                // explicit inheritance for HANDLE_LIST; no process is selected by a record PID.
                if unsafe {
                    DuplicateHandle(
                        GetCurrentProcess(),
                        source.as_raw_handle(),
                        GetCurrentProcess(),
                        &mut raw,
                        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                        1,
                        0,
                    )
                } == 0
                    || raw.is_null()
                {
                    return Err(NativeError::Unavailable);
                }
                // SAFETY: successful duplication transferred this one real own process handle.
                let parent = Arc::new(unsafe { OwnedHandle::from_raw_handle(raw) });
                let selection = SelectedOuterOperation {
                    io: io.clone(),
                    module: SelfImagePin(selected.module().0.clone()),
                    process,
                    record: selected.record().clone(),
                };
                let owner = Arc::new(LaunchOwner {
                    io,
                    selected: Mutex::new(selection),
                    lock: Mutex::new(Some(InstallerLock(lock.0.clone()))),
                    image: Mutex::new(Some(image)),
                    parent,
                    sources,
                    calls: Arc::new(CallOwner::default()),
                    cleanup: Arc::new(CallOwner::default()),
                    state: Mutex::new(ChildState {
                        process: None,
                        thread: None,
                        dispatched: false,
                        create_returned: false,
                        known_created: false,
                        resuming: false,
                        resumed: false,
                        cleanup_started: false,
                        retired: false,
                    }),
                    monitor_started: AtomicBool::new(false),
                    cancel_requested: AtomicBool::new(false),
                });
                let mut slot = LAUNCH
                    .get_or_init(|| Mutex::new(None))
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?;
                if slot.is_some() {
                    return Err(NativeError::Busy);
                }
                *slot = Some(owner.clone());
                Ok(Self(owner))
            }
            pub(crate) fn launch(self, deadline: &Deadline) -> NativeResult<KeeperChild> {
                let owner = self.0;
                let held = owner.clone();
                let bound = deadline.clone();
                let result=owner.calls.run(Dispatch::Mutation,deadline,move || {
                    let mut selected=held.selected.lock().map_err(|_|NativeError::OutcomeUnknown)?;
                    let lock=held.lock.lock().map_err(|_|NativeError::OutcomeUnknown)?;
                    let lock=lock.as_ref().ok_or(NativeError::Foreign)?;
                    let proof=held.io.admit_support(&bound)?;
                    selected.mark_launch_intent(&proof,lock,&bound)?;
                    let proof=held.io.admit_support(&bound)?; let image=held.image()?;image.reverify(&held.io,&proof,&bound)?;
                    let path=image.canonical_dos_path();
                    if path.contains(['\0','"']){return Err(NativeError::Foreign);}
                    let app=files::native::wide(path)?;
                    let mut command=files::native::wide(&format!("\"{path}\" {KEEPER_ARGUMENT}"))?;
                    let mut attrs=Attributes::new(held.parent.as_raw_handle())?;
                    let start=STARTUPINFOEXW{StartupInfo:STARTUPINFOW{cb:std::mem::size_of::<STARTUPINFOEXW>() as u32,
                        ..Default::default()},lpAttributeList:attrs.pointer()};
                    let mut output=PROCESS_INFORMATION::default(); bound.check()?;
                    let mut state=held.state.lock().map_err(|_|NativeError::OutcomeUnknown)?;
                    state.dispatched=true;
                    // SAFETY: actual opened fixed copy + sole constant flag, SUSPENDED before any
                    // instruction, explicit sole parent HANDLE_LIST. BREAKAWAY is requested but
                    // never assumed; actual returned child membership is checked before Resume.
                    let ok=unsafe{CreateProcessW(app.as_ptr(),command.as_mut_ptr(),std::ptr::null(),std::ptr::null(),1,
                        CREATE_SUSPENDED|CREATE_NO_WINDOW|CREATE_BREAKAWAY_FROM_JOB|EXTENDED_STARTUPINFO_PRESENT,
                        std::ptr::null(),std::ptr::null(),&start.StartupInfo,&mut output)};
                    state.create_returned=true;state.known_created=ok!=0 && !output.hProcess.is_null() && !output.hThread.is_null()
                        && output.hProcess!=output.hThread;
                    {
                        // SAFETY: each distinct non-null CreateProcess result is reserved in the
                        // owner BEFORE any validation or delivery, including anomalous partial output.
                        state.process=(!output.hProcess.is_null()).then(||Arc::new(unsafe{OwnedHandle::from_raw_handle(output.hProcess)}));
                        // SAFETY: separately transferred returned primary thread, not a claimed id.
                        state.thread=(!output.hThread.is_null() && output.hThread!=output.hProcess)
                            .then(||Arc::new(unsafe{OwnedHandle::from_raw_handle(output.hThread)}));
                    }
                    drop(state);
                    if ok==0 && output.hProcess.is_null() && output.hThread.is_null(){
                        eprintln!("Crosspane upgrade keeper cannot start inside the current containing job; run the installer outside it, for example from Explorer.");
                        return Err(NativeError::Unsupported);
                    }
                    if ok==0 || output.hProcess.is_null() || output.hThread.is_null() || output.hProcess==output.hThread {
                        return Err(NativeError::OutcomeUnknown);
                    }
                    held.monitor_exit()?;
                    let process=held.state.lock().map_err(|_|NativeError::OutcomeUnknown)?.process.clone().ok_or(NativeError::OutcomeUnknown)?;
                    let mut in_job=0;
                    // SAFETY: exact returned suspended child and NULL means any job, not a name or
                    // guessed job handle. Do not alter job policies to make this succeed.
                    if unsafe{IsProcessInJob(process.as_raw_handle(),std::ptr::null_mut(),&mut in_job)}==0 || in_job!=0 {
                        eprintln!("Crosspane upgrade keeper must run outside the current containing job; start the installer from Explorer.");
                        return Err(NativeError::Unsupported);
                    }
                    // SAFETY: retained actual child; this observation does not grant adoption/launch.
                    let pid=unsafe{GetProcessId(process.as_raw_handle())}; let created=process_creation(&process)?;
                    let proof=held.io.admit_support(&bound)?;
                    selected.record_created_keeper(&held.io,&proof,lock,held.parent.as_raw_handle() as usize as u64,
                        OuterProcessCorrelation::new(pid,created,outer_stamp(image.identity()),image.approved().facts().clone())?,&bound)?;
                    let proof=held.io.admit_support(&bound)?; selected.mark_resume_intent(&proof,lock,&bound)?;
                    Ok(())
                });
                if let Err(error) = result {
                    let _ = owner.cancel_suspended();
                    return Err(error);
                }
                // Every caller-supplied lock alias must also be dropped by the outer before this
                // point. We release our private alias; the child takes a fresh lock for admission.
                owner
                    .lock
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?
                    .take();
                let held = owner.clone();
                let bound = deadline.clone();
                let result = owner.calls.run(Dispatch::Mutation, deadline, move || {
                    bound.check()?;
                    let thread = {
                        let mut s = held.state.lock().map_err(|_| NativeError::OutcomeUnknown)?;
                        if s.resuming || s.resumed {
                            return Err(NativeError::OutcomeUnknown);
                        }
                        s.resuming = true;
                        s.thread.clone().ok_or(NativeError::OutcomeUnknown)?
                    };
                    // SAFETY: only our retained actual never-resumed child primary thread. Marked
                    // resuming BEFORE call: failure/late delivery never permits forced cleanup.
                    let previous = unsafe { ResumeThread(thread.as_raw_handle()) };
                    let mut state = held.state.lock().map_err(|_| NativeError::OutcomeUnknown)?;
                    if previous != 1 {
                        return Err(NativeError::OutcomeUnknown);
                    }
                    state.resumed = true;
                    bound.check()
                });
                if let Err(error) = result {
                    let _ = owner.cancel_suspended();
                    return Err(error);
                }
                Ok(KeeperChild(owner))
            }
        }
        impl LaunchOwner {
            fn image(&self) -> NativeResult<OpenedPe> {
                let image = self.image.lock().map_err(|_| NativeError::OutcomeUnknown)?;
                let image = image.as_ref().ok_or(NativeError::OutcomeUnknown)?;
                Ok(OpenedPe(image.0.clone()))
            }
            fn release_if_settled(self: &Arc<Self>) -> NativeResult<bool> {
                if !self.calls.idle() || !self.cleanup.idle() {
                    return Ok(false);
                }
                let mut state = self.state.lock().map_err(|_| NativeError::OutcomeUnknown)?;
                if state.retired {
                    return Ok(true);
                }
                let no_child = (!state.dispatched || state.create_returned)
                    && state.process.is_none()
                    && state.thread.is_none();
                let exited = state.known_created
                    && state.process.as_ref().is_some_and(|process| {
                        // SAFETY: exactly our successful CreateProcess output, observation only.
                        (unsafe { WaitForSingleObject(process.as_raw_handle(), 0) })
                            == WAIT_OBJECT_0
                    });
                if !no_child && !exited {
                    return Ok(false);
                }
                state.retired = true;
                drop(state);
                self.lock
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?
                    .take();
                self.image
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?
                    .take();
                // Only this positively settled original launcher can cancel a pre-commit ledger.
                // A concurrent real Commit remains strict and cannot be overwritten as cancelled.
                let _ = self.cancel_settled_precommit();
                let mut slot = LAUNCH
                    .get_or_init(|| Mutex::new(None))
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?;
                if slot.as_ref().is_some_and(|owner| Arc::ptr_eq(owner, self)) {
                    slot.take();
                }
                Ok(true)
            }
            fn cancel_settled_precommit(&self) -> NativeResult<()> {
                use super::super::super::payload::recovery::{self, Phase};
                let deadline =
                    Deadline::new(30_000, self.io.bound_clock(), Cancellation::default())?;
                let proof = self.io.admit_support(&deadline)?;
                let lock = self.io.acquire_installer_lock(&proof, &deadline)?;
                let proof = self.io.admit_support(&deadline)?;
                let selected = self
                    .selected
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?;
                selected.reverify(&self.io, &proof, &lock, &deadline)?;
                let record = self.io.observe_outer_operation(&proof, &lock, &deadline)?;
                if !matches!(
                    record.phase(),
                    OuterPhase::Selecting
                        | OuterPhase::Preparing
                        | OuterPhase::Prepared
                        | OuterPhase::Ready
                ) {
                    return Ok(());
                }
                let operation = recovery::selected_operation(&self.io, &proof, &deadline)?
                    .ok_or(NativeError::Foreign)?;
                if operation.operation() != record.operation() || operation.phase() != Phase::Intent
                {
                    return Err(NativeError::OutcomeUnknown);
                }
                selected.mark_cancelled(&proof, &lock, &deadline)
            }
            fn monitor_exit(self: &Arc<Self>) -> NativeResult<()> {
                if self.monitor_started.swap(true, Ordering::AcqRel) {
                    return Ok(());
                }
                let held = self.clone();
                let started = std::thread::Builder::new()
                    .name("crosspane-keeper-child-settlement".into())
                    .spawn(move || {
                        // One retained child and no queue. Neither caller death nor a timeout drops
                        // the copy/source pins before the ACTUAL original child has exited.
                        loop {
                            if matches!(held.release_if_settled(), Ok(true)) {
                                return;
                            }
                            if held.cancel_requested.load(Ordering::Acquire) && held.calls.idle() {
                                // An abandoned caller's intent survives late native output. Eligibility
                                // still requires actual successful complete creation and NEVER Resume.
                                let _ = held.cancel_suspended();
                            }
                            std::thread::sleep(Duration::from_millis(25));
                        }
                    });
                if started.is_err() {
                    // Definite failure dispatched no observer. A caller/actual late worker may
                    // renew this reservation; it never permits another child Create or Resume.
                    self.monitor_started.store(false, Ordering::Release);
                    return Err(NativeError::Unavailable);
                }
                Ok(())
            }
            fn cancel_suspended(self: &Arc<Self>) -> NativeResult<()> {
                // Independent intent publication never waits behind the CreateProcess state gate.
                // It is NOT authority: the settled original output gates below decide cleanup.
                self.cancel_requested.store(true, Ordering::Release);
                let observer = self.monitor_exit();
                if !self.calls.idle() {
                    return Err(observer.err().unwrap_or(NativeError::Busy));
                }
                // Observer failure cannot hide positively eligible actual output/no-child state.
                // Busy/partial/Resume-attempted states keep the observer error and retained owner.
                if self.release_if_settled()? {
                    return Ok(());
                }
                let process = {
                    let mut state = self.state.lock().map_err(|_| NativeError::OutcomeUnknown)?;
                    // FALSE/partial native output, a running/late worker or any Resume attempt
                    // is uncertain. No such case is eligible for termination.
                    if !state.known_created
                        || !state.create_returned
                        || state.resuming
                        || state.resumed
                        || state.cleanup_started
                    {
                        return Err(observer.err().unwrap_or(NativeError::OutcomeUnknown));
                    }
                    state.cleanup_started = true;
                    state.process.clone().ok_or(NativeError::OutcomeUnknown)?
                };
                let held = self.clone();
                // The caller's original thirty-second launch window is never renewed. This one
                // separately bounded cleanup remains owned after the caller returns its error.
                std::thread::Builder::new()
                    .name("crosspane-keeper-suspended-cleanup".into())
                    .spawn(move || {
                        let result = (|| {
                            let budget = Deadline::new(
                                30_000,
                                held.io.bound_clock(),
                                Cancellation::default(),
                            )?;
                            let captured = held.clone();
                            let bound = budget.clone();
                            held.cleanup.run(Dispatch::Mutation, &budget, move || {
                                bound.check()?;
                                // SAFETY: actual successful SUSPENDED creation, retained process,
                                // NEVER resumed. No unknown/failed-output/Resume attempt is eligible.
                                if unsafe { TerminateProcess(process.as_raw_handle(), 1) } == 0 {
                                    return Err(NativeError::OutcomeUnknown);
                                }
                                loop {
                                    bound.check()?;
                                    // SAFETY: exact same original object, bounded zero-time exit poll.
                                    if unsafe { WaitForSingleObject(process.as_raw_handle(), 0) }
                                        == WAIT_OBJECT_0
                                    {
                                        break;
                                    }
                                    std::thread::sleep(Duration::from_millis(5));
                                }
                                drop(captured);
                                Ok(())
                            })
                        })();
                        if result.is_ok() {
                            let _ = held.release_if_settled();
                        }
                    })
                    .map_err(|_| NativeError::OutcomeUnknown)?;
                Ok(())
            }
        }

        use super::super::supervisor_owner::{
            OuterPeerPin, admit_keeper_observer_server, admit_keeper_peer,
            admit_outer_observer_peer, admit_outer_source_peer,
        };
        use tokio::{
            io::{AsyncRead, AsyncWrite, ReadBuf},
            net::windows::named_pipe::{
                ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
            },
        };
        #[derive(serde::Serialize, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Message {
            schema: u32,
            operation: [u8; 16],
            method: Method,
            state: Option<WireState>,
        }
        #[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
        enum Method {
            Sources,
            Ready,
            Commit,
            Cancel,
            Observe,
            Ack,
            Refused,
        }
        #[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
        enum WireState {
            Ready,
            Committed,
            Retained,
            Complete,
            Cancelled,
            ReinstallRequired,
        }
        fn wire_stage(stage: KeeperStage) -> WireState {
            match stage {
                KeeperStage::Ready => WireState::Ready,
                KeeperStage::Committed => WireState::Committed,
                KeeperStage::Complete => WireState::Complete,
                KeeperStage::Cancelled => WireState::Cancelled,
                KeeperStage::Preparing | KeeperStage::Retained => WireState::Retained,
            }
        }
        pub(crate) struct KeeperObservation {
            stage: KeeperStage,
        }
        fn endpoint(io: &WindowsNativeIo) -> String {
            format!(
                r"\\.\pipe\Crosspane.{}.{}.upgrade-keeper",
                io.target().identity().user.sddl(),
                io.target().identity().session
            )
        }
        fn runtime() -> NativeResult<tokio::runtime::Runtime> {
            tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
                .map_err(|_| NativeError::Unavailable)
        }
        async fn read_exact<R: AsyncRead + Unpin>(
            pipe: &mut R,
            bytes: &mut [u8],
        ) -> std::io::Result<()> {
            let mut offset = 0;
            while offset < bytes.len() {
                let end = (offset + CHUNK).min(bytes.len());
                let mut target = ReadBuf::new(&mut bytes[offset..end]);
                std::future::poll_fn(|cx| {
                    std::pin::Pin::new(&mut *pipe).poll_read(cx, &mut target)
                })
                .await?;
                if target.filled().is_empty() {
                    return Err(std::io::ErrorKind::UnexpectedEof.into());
                }
                offset += target.filled().len();
            }
            Ok(())
        }
        async fn write_all<W: AsyncWrite + Unpin>(
            pipe: &mut W,
            bytes: &[u8],
        ) -> std::io::Result<()> {
            let mut offset = 0;
            while offset < bytes.len() {
                let end = (offset + CHUNK).min(bytes.len());
                let n = std::future::poll_fn(|cx| {
                    std::pin::Pin::new(&mut *pipe).poll_write(cx, &bytes[offset..end])
                })
                .await?;
                if n == 0 || n > end - offset {
                    return Err(std::io::ErrorKind::WriteZero.into());
                }
                offset += n;
            }
            Ok(())
        }
        async fn write_message<W: AsyncWrite + Unpin>(
            pipe: &mut W,
            op: [u8; 16],
            method: Method,
            state: Option<WireState>,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            let bytes = serde_json::to_vec(&Message {
                schema: 1,
                operation: op,
                method,
                state,
            })
            .map_err(|_| NativeError::Invalid)?;
            if bytes.len() > 2048 {
                return Err(NativeError::Oversize);
            }
            tokio::time::timeout(Duration::from_millis(deadline.remaining_ms()?), async {
                write_all(pipe, &(bytes.len() as u32).to_le_bytes()).await?;
                write_all(pipe, &bytes).await
            })
            .await
            .map_err(|_| NativeError::OutcomeUnknown)?
            .map_err(|_| NativeError::OutcomeUnknown)
        }
        async fn read_message<R: AsyncRead + Unpin>(
            pipe: &mut R,
            op: [u8; 16],
            deadline: &Deadline,
        ) -> NativeResult<Message> {
            let value =
                tokio::time::timeout(Duration::from_millis(deadline.remaining_ms()?), async {
                    let mut length = [0; 4];
                    read_exact(pipe, &mut length)
                        .await
                        .map_err(|_| NativeError::Unavailable)?;
                    let length = u32::from_le_bytes(length) as usize;
                    if length == 0 || length > 2048 {
                        return Err(NativeError::Oversize);
                    }
                    let mut bytes = vec![0; length];
                    read_exact(pipe, &mut bytes)
                        .await
                        .map_err(|_| NativeError::Unavailable)?;
                    serde_json::from_slice::<Message>(&bytes).map_err(|_| NativeError::Invalid)
                })
                .await
                .map_err(|_| NativeError::Timeout)??;
            if value.schema != 1 || value.operation != op {
                return Err(NativeError::Foreign);
            }
            deadline.check()?;
            Ok(value)
        }
        struct Descriptor(PSECURITY_DESCRIPTOR);
        impl Drop for Descriptor {
            fn drop(&mut self) {
                // SAFETY: exactly the LocalAlloc allocation transferred by SDDL conversion.
                unsafe { LocalFree(self.0) };
            }
        }
        fn create_server(io: &WindowsNativeIo) -> NativeResult<NamedPipeServer> {
            let token = io.target().identity();
            let user = token.user.sddl();
            let logon = token.logon.sddl();
            let text = files::native::wide(&format!(
                "O:{user}G:{user}D:P(A;;GA;;;{user})(A;;GRGW;;;{logon})"
            ))?;
            let mut pointer = std::ptr::null_mut();
            // SAFETY: exact validated current user/logon SIDs, protected local descriptor output.
            if unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    text.as_ptr(),
                    SDDL_REVISION_1,
                    &mut pointer,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                return Err(NativeError::Unavailable);
            }
            let descriptor = Descriptor(pointer);
            let attributes = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor.0,
                bInheritHandle: 0,
            };
            // SAFETY: sole first instance, one local connection with no inheritance; validated
            // descriptor lives until the call returns and Tokio then owns the actual server object.
            unsafe {
                ServerOptions::new()
                    .first_pipe_instance(true)
                    .reject_remote_clients(true)
                    .max_instances(1)
                    .in_buffer_size(CHUNK as u32)
                    .out_buffer_size(CHUNK as u32)
                    .create_with_security_attributes_raw(
                        endpoint(io),
                        (&attributes as *const SECURITY_ATTRIBUTES)
                            .cast_mut()
                            .cast(),
                    )
            }
            .map_err(|_| NativeError::Busy)
        }
        pub(crate) struct KeeperServer {
            io: Arc<WindowsNativeIo>,
            selected: SelectedOuterOperation,
            parent: KeeperParent,
            rt: tokio::runtime::Runtime,
            pipe: NamedPipeServer,
            peer: Option<OuterPeerPin>,
            connected: bool,
        }
        impl KeeperServer {
            pub(crate) fn reserve(
                io: Arc<WindowsNativeIo>,
                selected: SelectedOuterOperation,
                deadline: &Deadline,
            ) -> NativeResult<Self> {
                let proof = io.admit_support(deadline)?;
                proof.check(&io, deadline)?;
                let parent = KeeperParent::admit(&io, &selected, deadline)?;
                let rt = runtime()?;
                let pipe = {
                    let _entered = rt.enter();
                    create_server(&io)?
                };
                Ok(Self {
                    io,
                    selected,
                    parent,
                    rt,
                    pipe,
                    peer: None,
                    connected: false,
                })
            }
            pub(crate) fn selected(&self) -> &SelectedOuterOperation {
                &self.selected
            }
            pub(crate) fn receive_sources(
                &mut self,
                deadline: &Deadline,
            ) -> NativeResult<ApprovedOuterSources> {
                self.rt.block_on(async {
                    tokio::time::timeout(
                        Duration::from_millis(deadline.remaining_ms()?),
                        self.pipe.connect(),
                    )
                    .await
                    .map_err(|_| NativeError::Timeout)?
                    .map_err(|_| NativeError::Unavailable)
                })?;
                self.connected = true;
                let proof = self.io.admit_support(deadline)?;
                self.peer = Some(admit_outer_source_peer(
                    self.pipe.as_raw_handle(),
                    self.io.clone(),
                    &proof,
                    &self.parent,
                    self.selected.module(),
                    self.selected.operation(),
                    deadline,
                )?);
                let message = self.rt.block_on(read_message(
                    &mut self.pipe,
                    self.selected.operation(),
                    deadline,
                ))?;
                if message.method != Method::Sources || message.state.is_some() {
                    return Err(NativeError::Foreign);
                }
                let inventory = ApprovedInventory::embedded()?;
                let own = ApprovedPe::own_image(self.selected.module())?;
                inventory.check_staging_budget(&own, true)?;
                let mut received: [Vec<u8>; 3] = std::array::from_fn(|_| Vec::new());
                for (index, role) in [PayloadRole::Agent, PayloadRole::Ui, PayloadRole::Ctl]
                    .into_iter()
                    .enumerate()
                {
                    let expected = inventory.role(role)?.size();
                    self.rt.block_on(async {
                        let mut length = [0; 8];
                        tokio::time::timeout(
                            Duration::from_millis(deadline.remaining_ms()?),
                            read_exact(&mut self.pipe, &mut length),
                        )
                        .await
                        .map_err(|_| NativeError::Timeout)?
                        .map_err(|_| NativeError::Unavailable)?;
                        if u64::from_le_bytes(length) != expected {
                            return Err(NativeError::Foreign);
                        }
                        let length =
                            usize::try_from(expected).map_err(|_| NativeError::Oversize)?;
                        received[index]
                            .try_reserve_exact(length)
                            .map_err(|_| NativeError::Oversize)?;
                        received[index].resize(length, 0);
                        tokio::time::timeout(
                            Duration::from_millis(deadline.remaining_ms()?),
                            read_exact(&mut self.pipe, &mut received[index]),
                        )
                        .await
                        .map_err(|_| NativeError::Timeout)?
                        .map_err(|_| NativeError::Unavailable)
                    })?;
                }
                let proof = self.io.admit_support(deadline)?;
                self.peer
                    .as_ref()
                    .ok_or(NativeError::Foreign)?
                    .reverify(&self.io, &proof, deadline)?;
                let result = ApprovedOuterSources::receive(received, &inventory, &own, deadline)?;
                if result.facts() != self.selected.record().sources() {
                    return Err(NativeError::Foreign);
                }
                Ok(result)
            }
            pub(crate) fn refuse_reinstall_required(
                &mut self,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                let proof = self.io.admit_support(deadline)?;
                self.peer
                    .as_ref()
                    .ok_or(NativeError::Foreign)?
                    .reverify(&self.io, &proof, deadline)?;
                self.rt.block_on(write_message(
                    &mut self.pipe,
                    self.selected.operation(),
                    Method::Refused,
                    Some(WireState::ReinstallRequired),
                    deadline,
                ))
            }
            pub(crate) fn announce_ready(&mut self, deadline: &Deadline) -> NativeResult<()> {
                let proof = self.io.admit_support(deadline)?;
                self.peer
                    .as_ref()
                    .ok_or(NativeError::Foreign)?
                    .reverify(&self.io, &proof, deadline)?;
                self.rt.block_on(write_message(
                    &mut self.pipe,
                    self.selected.operation(),
                    Method::Ready,
                    Some(WireState::Ready),
                    deadline,
                ))
            }
            pub(crate) fn wait_commit(&mut self, deadline: &Deadline) -> NativeResult<bool> {
                let proof = self.io.admit_support(deadline)?;
                self.peer
                    .as_ref()
                    .ok_or(NativeError::Foreign)?
                    .reverify(&self.io, &proof, deadline)?;
                let request = self.rt.block_on(read_message(
                    &mut self.pipe,
                    self.selected.operation(),
                    deadline,
                ))?;
                if request.state.is_some() {
                    return Err(NativeError::Foreign);
                }
                match request.method {
                    Method::Commit => Ok(true),
                    Method::Cancel => Ok(false),
                    _ => Err(NativeError::Foreign),
                }
            }
            pub(crate) fn acknowledge_commit(&mut self, deadline: &Deadline) {
                // Lost ACK never cancels/repeats committed work. Close actual aliases so later
                // same-operation observers can connect; no read/source queue survives this handoff.
                let _ = self.rt.block_on(write_message(
                    &mut self.pipe,
                    self.selected.operation(),
                    Method::Ack,
                    Some(WireState::Committed),
                    deadline,
                ));
                self.disconnect();
            }
            fn disconnect(&mut self) {
                self.peer.take();
                if self.connected {
                    let _ = self.pipe.disconnect();
                    self.connected = false;
                }
            }
            pub(crate) fn poll_observer(
                &mut self,
                stage: KeeperStage,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                if self.connected {
                    return Err(NativeError::Busy);
                }
                let remaining = deadline.remaining_ms()?;
                let connected = self.rt.block_on(async {
                    tokio::time::timeout(Duration::from_millis(remaining), self.pipe.connect())
                        .await
                });
                match connected {
                    Err(_) => return Ok(()),
                    Ok(Err(_)) => return Err(NativeError::Unavailable),
                    Ok(Ok(())) => {}
                }
                self.connected = true;
                let result = (|| {
                    let proof = self.io.admit_support(deadline)?;
                    let peer = admit_outer_observer_peer(
                        self.pipe.as_raw_handle(),
                        self.io.clone(),
                        &proof,
                        self.selected.operation(),
                        deadline,
                    )?;
                    let message = self.rt.block_on(read_message(
                        &mut self.pipe,
                        self.selected.operation(),
                        deadline,
                    ))?;
                    if message.method != Method::Observe || message.state.is_some() {
                        return Err(NativeError::Foreign);
                    }
                    peer.reverify(&self.io, &self.io.admit_support(deadline)?, deadline)?;
                    self.rt.block_on(write_message(
                        &mut self.pipe,
                        self.selected.operation(),
                        Method::Ack,
                        Some(wire_stage(stage)),
                        deadline,
                    ))
                })();
                self.disconnect();
                result
            }
        }

        struct ExclusiveKeeperNamespace {
            io: Arc<WindowsNativeIo>,
            pipe: NamedPipeServer,
            own: process::own::OwnProcessIdentity,
            _rt: tokio::runtime::Runtime,
        }
        impl ExclusiveKeeperNamespace {
            fn reserve(
                io: Arc<WindowsNativeIo>,
                proof: &SupportProof,
                deadline: &Deadline,
            ) -> NativeResult<Arc<Self>> {
                proof.check(&io, deadline)?;
                let own = io.own_process_identity(proof, deadline)?;
                let rt = runtime()?;
                let pipe = {
                    let _entered = rt.enter();
                    create_server(&io)?
                };
                let value = Arc::new(Self {
                    io,
                    pipe,
                    own,
                    _rt: rt,
                });
                value.reverify(proof, deadline)?;
                Ok(value)
            }
            fn reverify(&self, proof: &SupportProof, deadline: &Deadline) -> NativeResult<()> {
                proof.check(&self.io, deadline)?;
                self.own.reverify(deadline)?;
                let mut flags = 0;
                // SAFETY: exact originally created sole FIRST_INSTANCE server object, not a name
                // reconstructed from metadata. Same original process/context retain the lease.
                if unsafe {
                    GetNamedPipeInfo(
                        self.pipe.as_raw_handle(),
                        &mut flags,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                } == 0
                    || flags & PIPE_SERVER_END == 0
                {
                    return Err(NativeError::Foreign);
                }
                deadline.check()
            }
        }
        /// Actual original process EXIT, cleanup only. Never a tree/start/Stop capability.
        pub(crate) struct SettledKeeperCopy {
            io: Arc<WindowsNativeIo>,
            process: Arc<OwnedHandle>,
            pid: u32,
            created: u64,
            operation: [u8; 16],
            identity: FileIdentity,
        }
        impl SettledKeeperCopy {
            fn from_peer(
                io: Arc<WindowsNativeIo>,
                proof: &SupportProof,
                lock: &InstallerLock,
                peer: OuterPeerPin,
                deadline: &Deadline,
            ) -> NativeResult<Self> {
                let record = io.observe_outer_operation(proof, lock, deadline)?;
                peer.reverify_exited_keeper(&io, proof, record.operation(), deadline)?;
                let identity = peer.image_identity();
                let value = Self {
                    io,
                    process: peer.retained_process(),
                    pid: peer.pid(),
                    created: peer.creation(),
                    operation: record.operation(),
                    identity,
                };
                // Settle the peer's actual measured read pins before any exclusive DELETE open.
                drop(peer);
                value.reverify(proof, deadline)?;
                Ok(value)
            }
            fn reverify(&self, proof: &SupportProof, deadline: &Deadline) -> NativeResult<()> {
                proof.check(&self.io, deadline)?;
                // SAFETY: SAME retained kernel-derived original process; never OpenProcess PID.
                if unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) } != WAIT_OBJECT_0
                    // SAFETY: query the SAME original kernel-admitted retained process object.
                    || unsafe { GetProcessId(self.process.as_raw_handle()) } != self.pid
                    || process_creation(&self.process)? != self.created
                {
                    return Err(NativeError::Foreign);
                }
                deadline.check()
            }
        }
        /// Positive fixed-name absence plus ACTUAL exclusive namespace. FS-only retirement;
        /// metadata/this type never reconstructs a process/job or permits Stop/start/Run.
        pub(crate) struct KeeperCopyAbsent {
            io: Arc<WindowsNativeIo>,
            namespace: Arc<ExclusiveKeeperNamespace>,
            operation: [u8; 16],
            identity: Option<FileIdentity>,
        }
        impl KeeperCopyAbsent {
            pub(crate) fn operation(&self) -> [u8; 16] {
                self.operation
            }
            pub(crate) fn reverify(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                lock: &InstallerLock,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                if !std::ptr::eq(io, self.io.as_ref()) {
                    return Err(NativeError::Foreign);
                }
                io.lock_binding(proof, lock, deadline)?;
                self.namespace.reverify(proof, deadline)?;
                let record = cleanup_ledger(io, proof, lock, deadline)?;
                if record.operation() != self.operation
                    || record.keeper_image() != self.identity.map(outer_stamp)
                    || record.copy_cleanup()
                        != super::super::super::payload::recovery::OuterCopyCleanup::Absent
                {
                    return Err(NativeError::Foreign);
                }
                let context = io.context.clone();
                let lease = lock.0.clone();
                let operation = self.operation;
                let budget = proof.budget(io, deadline)?;
                io.owner.run(Dispatch::Observation, deadline, move || {
                    validate_payload_lock(&context, &lease, &budget)?;
                    // Missing admitted ancestors prove absence without creating any path.
                    let Some(root) = Anchor::open(
                        context.target.paths.install(),
                        &context.security,
                        true,
                        &budget,
                    )?
                    else {
                        return Ok(());
                    };
                    let Some(stage) = root.child("payload-stage", &context.security, &budget)?
                    else {
                        return Ok(());
                    };
                    let Some(parent) =
                        stage.child(&records::hex(&operation), &context.security, &budget)?
                    else {
                        return Ok(());
                    };
                    if parent
                        .opaque("keeper-copy.exe", false, &context.security, &budget)?
                        .is_some()
                    {
                        return Err(NativeError::Foreign);
                    }
                    Ok(())
                })
            }
        }
        fn cleanup_ledger(
            io: &WindowsNativeIo,
            proof: &SupportProof,
            lock: &InstallerLock,
            deadline: &Deadline,
        ) -> NativeResult<OuterUpgradeRecord> {
            use super::super::super::payload::recovery::{
                self, OperationRecord, OriginalLeaf, Phase,
            };
            io.lock_binding(proof, lock, deadline)?;
            let record =
                OuterUpgradeRecord::read(io, proof, deadline)?.ok_or(NativeError::Foreign)?;
            record.context().same_user(io.target().identity())?;
            if !matches!(record.phase(), OuterPhase::Complete | OuterPhase::Cancelled) {
                return Err(NativeError::OutcomeUnknown);
            }
            let observed = io
                .read_record(
                    proof,
                    records::RecordName::Operation(record.operation()),
                    files::MAX_RECORD_BYTES,
                    deadline,
                )?
                .ok_or(NativeError::Foreign)?;
            let operation: OperationRecord = records::record_data(
                &records::RecordName::Operation(record.operation()),
                observed.bytes(),
            )?;
            operation.validate()?;
            let catalog = recovery::catalog(io, proof, deadline)?;
            if record.phase() == OuterPhase::Complete {
                if operation.phase() != Phase::Complete || catalog.active.is_some() {
                    return Err(NativeError::Foreign);
                }
            } else {
                if !matches!(operation.phase(), Phase::Intent | Phase::RolledBack)
                    || catalog.active.is_some_and(|op| op != record.operation())
                    || operation.current_role().is_some()
                    || operation.original_instance().is_some()
                    || operation.new_instance().is_some()
                    || operation.handoff().is_some()
                    || operation.retention_incomplete()
                {
                    return Err(NativeError::Foreign);
                }
                for role in PayloadRole::ALL {
                    let role = operation.role(role)?;
                    if role.original != OriginalLeaf::Unobserved
                        || role.staged.is_some()
                        || role.backup.is_some()
                        || role.published.is_some()
                    {
                        return Err(NativeError::Foreign);
                    }
                }
            }
            Ok(record)
        }
        fn publish_cleanup(
            io: &WindowsNativeIo,
            proof: &SupportProof,
            lock: &InstallerLock,
            record: &OuterUpgradeRecord,
            deadline: &Deadline,
        ) -> NativeResult<()> {
            io.lock_binding(proof, lock, deadline)?;
            record.validate()?;
            record.context().same_user(io.target().identity())?;
            if !matches!(record.phase(), OuterPhase::Complete | OuterPhase::Cancelled) {
                return Err(NativeError::Foreign);
            }
            let publication = io.publish_record(
                proof,
                lock,
                records::RecordName::OuterUpgrade,
                &record.encode()?,
                deadline,
            )?;
            if publication.state != records::PublicationRecovery::NewPublished
                || publication.native_failure.is_some()
            {
                return Err(NativeError::OutcomeUnknown);
            }
            Ok(())
        }
        impl WindowsNativeIo {
            /// Explicit lead2bbef658 FS-only cold retirement. It proves current namespace vacancy
            /// by actually reserving first instance, NOT old process exit or tree completion.
            pub(crate) fn cleanup_cold_keeper(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                deadline: &Deadline,
            ) -> NativeResult<KeeperCopyAbsent> {
                self.lock_binding(proof, lock, deadline)?;
                let record = cleanup_ledger(self, proof, lock, deadline)?;
                let namespace = ExclusiveKeeperNamespace::reserve(self.clone(), proof, deadline)?;
                self.cleanup_copy_with_namespace(proof, lock, record, namespace, deadline)
            }
            pub(crate) fn cleanup_exited_keeper(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                peer: OuterPeerPin,
                deadline: &Deadline,
            ) -> NativeResult<KeeperCopyAbsent> {
                let exit = SettledKeeperCopy::from_peer(self.clone(), proof, lock, peer, deadline)?;
                exit.reverify(proof, deadline)?;
                let record = cleanup_ledger(self, proof, lock, deadline)?;
                if record.operation() != exit.operation
                    || record.keeper_image() != Some(outer_stamp(exit.identity))
                {
                    return Err(NativeError::Foreign);
                }
                let namespace = ExclusiveKeeperNamespace::reserve(self.clone(), proof, deadline)?;
                self.cleanup_copy_with_namespace(proof, lock, record, namespace, deadline)
            }
            fn cleanup_copy_with_namespace(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                mut record: OuterUpgradeRecord,
                namespace: Arc<ExclusiveKeeperNamespace>,
                deadline: &Deadline,
            ) -> NativeResult<KeeperCopyAbsent> {
                use super::super::super::payload::recovery::OuterCopyCleanup;
                namespace.reverify(proof, deadline)?;
                let identity = record.keeper_image().map(|stamp| FileIdentity {
                    volume: stamp.volume,
                    file: stamp.file,
                });
                // Fresh fixed-name observation precedes a NEW DeleteIntent. A missing copy with
                // a recorded FileId cannot be explained by an intent we are just about to create.
                let observed = {
                    let context = self.context.clone();
                    let lease = lock.0.clone();
                    let operation = record.operation();
                    let expected = record.encode()?;
                    let budget = proof.budget(self, deadline)?;
                    self.owner.run(Dispatch::Observation, deadline, move || {
                        validate_payload_lock(&context, &lease, &budget)?;
                        let (_, bytes) = lease
                            .parent
                            .read_private(
                                &records::RecordName::OuterUpgrade.file_name()?,
                                &context.security,
                                files::MAX_RECORD_BYTES,
                                &budget,
                            )?
                            .ok_or(NativeError::Foreign)?;
                        if bytes != expected {
                            return Err(NativeError::Foreign);
                        }
                        let Some(root) = Anchor::open(
                            context.target.paths.install(),
                            &context.security,
                            true,
                            &budget,
                        )?
                        else {
                            return Ok(None);
                        };
                        let Some(stage) =
                            root.child("payload-stage", &context.security, &budget)?
                        else {
                            return Ok(None);
                        };
                        let Some(parent) =
                            stage.child(&records::hex(&operation), &context.security, &budget)?
                        else {
                            return Ok(None);
                        };
                        Ok(parent
                            .opaque("keeper-copy.exe", false, &context.security, &budget)?
                            .map(|leaf| leaf.identity))
                    })?
                };
                match (record.copy_cleanup(), identity, observed) {
                    (OuterCopyCleanup::None, None, None) => {}
                    (OuterCopyCleanup::None, Some(expected), Some(actual))
                        if expected == actual => {}
                    (OuterCopyCleanup::DeleteIntent, Some(expected), Some(actual))
                        if expected == actual => {}
                    (OuterCopyCleanup::DeleteIntent, _, None)
                    | (OuterCopyCleanup::Absent, _, None) => {}
                    _ => return Err(NativeError::OutcomeUnknown),
                }
                if record.copy_cleanup() == OuterCopyCleanup::None {
                    record.begin_cleanup()?;
                    publish_cleanup(self, proof, lock, &record, deadline)?;
                }
                let operation = record.operation();
                let expected = record.encode()?;
                let context = self.context.clone();
                let lease = lock.0.clone();
                let namespace_held = namespace.clone();
                let io = self.clone();
                let fresh = self.admit_support(deadline)?;
                let budget = fresh.budget(self, deadline)?;
                self.owner.run(Dispatch::Mutation, deadline, move || {
                    let change = Change::new();
                    let result = change.finish((|| {
                        namespace_held.own.reverify(&budget)?;
                        context.validate(&budget)?;
                        validate_payload_lock(&context, &lease, &budget)?;
                        let (_, bytes) = lease
                            .parent
                            .read_private(
                                &records::RecordName::OuterUpgrade.file_name()?,
                                &context.security,
                                files::MAX_RECORD_BYTES,
                                &budget,
                            )?
                            .ok_or(NativeError::Foreign)?;
                        let current = OuterUpgradeRecord::decode(&bytes)?;
                        current.context().same_user(&context.target.identity)?;
                        if current.encode()? != expected {
                            return Err(NativeError::Foreign);
                        }
                        let Some(root) = Anchor::open(
                            context.target.paths.install(),
                            &context.security,
                            true,
                            &budget,
                        )?
                        else {
                            return Ok(());
                        };
                        let Some(stage) =
                            root.child("payload-stage", &context.security, &budget)?
                        else {
                            return Ok(());
                        };
                        let Some(parent) =
                            stage.child(&records::hex(&operation), &context.security, &budget)?
                        else {
                            return Ok(());
                        };
                        if let Some(actual) =
                            parent.opaque("keeper-copy.exe", false, &context.security, &budget)?
                        {
                            if Some(actual.identity) != identity {
                                return Err(NativeError::Foreign);
                            }
                            drop(actual);
                            change.reached();
                            parent.delete_keeper_copy(
                                identity.ok_or(NativeError::Foreign)?,
                                &context.security,
                                &budget,
                            )?;
                        }
                        if parent
                            .opaque("keeper-copy.exe", false, &context.security, &budget)?
                            .is_some()
                        {
                            return Err(NativeError::OutcomeUnknown);
                        }
                        Ok(())
                    })());
                    if matches!(result, Err(NativeError::OutcomeUnknown)) {
                        io.owner.retire_mutations();
                    }
                    result
                })?;
                if record.copy_cleanup() != OuterCopyCleanup::Absent {
                    record.copy_absent()?;
                    let proof = self.admit_support(deadline)?;
                    publish_cleanup(self, &proof, lock, &record, deadline)?;
                }
                let absence = KeeperCopyAbsent {
                    io: self.clone(),
                    namespace,
                    operation,
                    identity,
                };
                absence.reverify(self, &self.admit_support(deadline)?, lock, deadline)?;
                Ok(absence)
            }
        }

        /// Native positive fixed-copy absence, kept with the actual sole current user/session
        /// namespace. It is file finalization only, never old tree completion or start approval.
        pub(crate) struct FileRecoveryKeeperAbsent {
            io: Arc<WindowsNativeIo>,
            namespace: Arc<ExclusiveKeeperNamespace>,
            terminal: Arc<super::file_recovery::TerminalData>,
        }

        pub(super) struct FileRecoveryNamespaceHold(Arc<ExclusiveKeeperNamespace>);
        impl FileRecoveryNamespaceHold {
            pub(super) fn reverify(
                &self,
                context: &Context,
                budget: &Deadline,
            ) -> NativeResult<()> {
                context.validate(budget)?;
                self.0.own.reverify(budget)?;
                let mut flags = 0;
                // SAFETY: this is the retained original FIRST_INSTANCE server handle. No name or
                // recorded handle value is reopened; the hold survives any abandoned caller.
                if unsafe {
                    GetNamedPipeInfo(
                        self.0.pipe.as_raw_handle(),
                        &mut flags,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                } == 0
                    || flags & PIPE_SERVER_END == 0
                {
                    return Err(NativeError::Foreign);
                }
                budget.check()
            }
        }
        impl FileRecoveryKeeperAbsent {
            pub(super) fn retain_namespace(&self) -> FileRecoveryNamespaceHold {
                FileRecoveryNamespaceHold(self.namespace.clone())
            }
            pub(crate) fn reverify(
                &self,
                io: &WindowsNativeIo,
                proof: &SupportProof,
                lock: &InstallerLock,
                seal: &super::FileRecoverySeal,
                deadline: &Deadline,
            ) -> NativeResult<()> {
                if !std::ptr::eq(io, self.io.as_ref()) {
                    return Err(NativeError::Foreign);
                }
                self.namespace.reverify(proof, deadline)?;
                self.terminal.matches_seal(seal)?;
                seal.reverify(io, proof, lock, deadline)?;
                let terminal = self.terminal.clone();
                let context = io.context.clone();
                let lease = lock.0.clone();
                let owner = io.owner.clone();
                let namespace = self.namespace.clone();
                let budget = proof.budget(io, deadline)?;
                io.owner.run(Dispatch::Observation,deadline,move|| {
                    namespace.own.reverify(&budget)?;
                    let journal=terminal.validate_current(&context,&lease,&owner,&budget)?;
                    if !matches!(journal.cursor(),super::super::super::payload::recovery::FileRecoveryCursor::CopyDeleteIntent
                        |super::super::super::payload::recovery::FileRecoveryCursor::CopyAbsent
                        |super::super::super::payload::recovery::FileRecoveryCursor::CatalogRetireIntent
                        |super::super::super::payload::recovery::FileRecoveryCursor::CatalogInactive
                        |super::super::super::payload::recovery::FileRecoveryCursor::OuterRetireIntent
                        |super::super::super::payload::recovery::FileRecoveryCursor::Retired){return Err(NativeError::Foreign)}
                    super::file_recovery::verify_copy_absent(&context,&journal,&budget)
                })
            }
        }
        impl WindowsNativeIo {
            pub(crate) fn cleanup_file_recovery_keeper(
                self: &Arc<Self>,
                proof: &SupportProof,
                lock: &InstallerLock,
                seal: &super::FileRecoverySeal,
                terminal: &super::FileTerminalObservation,
                deadline: &Deadline,
            ) -> NativeResult<FileRecoveryKeeperAbsent> {
                use super::super::super::payload::recovery::FileRecoveryCursor as Cursor;
                self.lock_binding(proof, lock, deadline)?;
                terminal.reverify(self, proof, lock, seal, deadline)?;
                let namespace = ExclusiveKeeperNamespace::reserve(self.clone(), proof, deadline)?;
                let data = terminal.held();
                let context = self.context.clone();
                let lease = lock.0.clone();
                let owner = self.owner.clone();
                let held_namespace = namespace.clone();
                let budget = proof.budget(self, deadline)?;
                self.owner.run(Dispatch::Mutation, deadline, move || {
                    let change = Change::new();
                    let result = change.finish((|| {
                        held_namespace.own.reverify(&budget)?;
                        let journal = data.validate_current(&context, &lease, &owner, &budget)?;
                        let allow_delete = match journal.cursor() {
                            Cursor::CopyDeleteIntent => true,
                            Cursor::CopyAbsent
                            | Cursor::CatalogRetireIntent
                            | Cursor::CatalogInactive
                            | Cursor::OuterRetireIntent
                            | Cursor::Retired => false,
                            _ => return Err(NativeError::Foreign),
                        };
                        let root = Anchor::open(
                            context.target.paths.install(),
                            &context.security,
                            true,
                            &budget,
                        )?
                        .ok_or(NativeError::Missing)?;
                        let stage = root.child("payload-stage", &context.security, &budget)?;
                        let parent = stage
                            .map(|p| {
                                p.child(
                                    &records::hex(&journal.operation()),
                                    &context.security,
                                    &budget,
                                )
                            })
                            .transpose()?
                            .flatten();
                        if let Some(parent) = parent {
                            // No unrelated helper copy may be interpreted as a settled old owner.
                            if parent
                                .opaque("helper-copy.exe", false, &context.security, &budget)?
                                .is_some()
                            {
                                return Err(NativeError::Unsupported);
                            }
                            let actual = parent.opaque(
                                "keeper-copy.exe",
                                false,
                                &context.security,
                                &budget,
                            )?;
                            if let Some(actual) = actual {
                                let expected = journal
                                    .outer_snapshot()
                                    .keeper_image()
                                    .ok_or(NativeError::Foreign)?;
                                if actual.identity != epoch_identity(expected) || !allow_delete {
                                    return Err(NativeError::Foreign);
                                }
                                let id = actual.identity;
                                drop(actual);
                                change.reached();
                                parent.delete_keeper_copy(id, &context.security, &budget)?;
                            }
                        }
                        super::file_recovery::verify_copy_absent(&context, &journal, &budget)?;
                        validate_payload_lock(&context, &lease, &budget)?;
                        Ok(())
                    })());
                    if matches!(result, Err(NativeError::OutcomeUnknown)) {
                        owner.retire_mutations()
                    }
                    result
                })?;
                let absence = FileRecoveryKeeperAbsent {
                    io: self.clone(),
                    namespace,
                    terminal: terminal.held(),
                };
                absence.reverify(self, proof, lock, seal, deadline)?;
                Ok(absence)
            }
        }

        pub(crate) struct KeeperReady {
            child: KeeperChild,
            rt: tokio::runtime::Runtime,
            pipe: NamedPipeClient,
            peer: OuterPeerPin,
        }
        #[doc(hidden)]
        pub struct KeeperContinuation {
            io: Arc<WindowsNativeIo>,
            operation: [u8; 16],
        }
        impl std::fmt::Debug for KeeperContinuation {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("KeeperContinuation")
            }
        }
        impl KeeperChild {
            pub(crate) fn await_ready(&self, deadline: &Deadline) -> NativeResult<KeeperReady> {
                let rt = runtime()?;
                let mut pipe = loop {
                    deadline.check()?;
                    let result = {
                        let _enter = rt.enter();
                        ClientOptions::new().open(endpoint(&self.0.io))
                    };
                    match result {
                        Ok(pipe) => break pipe,
                        Err(_) => std::thread::sleep(Duration::from_millis(5)),
                    }
                };
                let proof = self.0.io.admit_support(deadline)?;
                let image = self.0.image()?;
                let peer = admit_keeper_peer(
                    pipe.as_raw_handle(),
                    true,
                    self.0.io.clone(),
                    &proof,
                    &self.process()?,
                    &image,
                    deadline,
                )?;
                let op = self
                    .0
                    .selected
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?
                    .operation();
                let transfer = rt.block_on(async {
                    write_message(&mut pipe, op, Method::Sources, None, deadline).await?;
                    for role in [PayloadRole::Agent, PayloadRole::Ui, PayloadRole::Ctl] {
                        let bytes = self.0.sources.bytes(role)?;
                        tokio::time::timeout(
                            Duration::from_millis(deadline.remaining_ms()?),
                            async {
                                write_all(&mut pipe, &(bytes.len() as u64).to_le_bytes()).await?;
                                write_all(&mut pipe, bytes).await
                            },
                        )
                        .await
                        .map_err(|_| NativeError::OutcomeUnknown)?
                        .map_err(|_| NativeError::OutcomeUnknown)?;
                    }
                    let reply = read_message(&mut pipe, op, deadline).await?;
                    if reply.method == Method::Refused
                        && reply.state == Some(WireState::ReinstallRequired)
                    {
                        eprintln!("Windows outer completion unavailable; reinstall required");
                        return Err(NativeError::Unsupported);
                    }
                    if reply.method != Method::Ready || reply.state != Some(WireState::Ready) {
                        return Err(NativeError::Foreign);
                    }
                    Ok(())
                });
                drop(image);
                if let Err(error) = transfer {
                    // Only this previously kernel-admitted ORIGINAL keeper peer can supply exit
                    // cleanup. A still-live/unknown child refuses; no cancellation/kill/Stop occurs.
                    if let Ok(cleanup) =
                        Deadline::new(30_000, self.0.io.bound_clock(), Cancellation::default())
                        && let Ok(proof) = self.0.io.admit_support(&cleanup)
                        && let Ok(lock) = self.0.io.acquire_installer_lock(&proof, &cleanup)
                        && let Ok(proof) = self.0.io.admit_support(&cleanup)
                    {
                        let _ = self
                            .0
                            .io
                            .cleanup_exited_keeper(&proof, &lock, peer, &cleanup);
                    }
                    return Err(error);
                }
                peer.reverify(&self.0.io, &self.0.io.admit_support(deadline)?, deadline)?;
                Ok(KeeperReady {
                    child: KeeperChild(self.0.clone()),
                    rt,
                    pipe,
                    peer,
                })
            }
        }
        impl KeeperReady {
            pub(crate) fn commit(
                mut self,
                deadline: &Deadline,
            ) -> NativeResult<KeeperContinuation> {
                self.peer.reverify(
                    &self.child.0.io,
                    &self.child.0.io.admit_support(deadline)?,
                    deadline,
                )?;
                let op = self
                    .child
                    .0
                    .selected
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?
                    .operation();
                self.rt.block_on(async {
                    write_message(&mut self.pipe, op, Method::Commit, None, deadline).await?;
                    let reply = read_message(&mut self.pipe, op, deadline).await?;
                    if reply.method != Method::Ack || reply.state != Some(WireState::Committed) {
                        return Err(NativeError::Foreign);
                    }
                    Ok(())
                })?;
                Ok(KeeperContinuation {
                    io: self.child.0.io.clone(),
                    operation: op,
                })
            }
        }
        impl KeeperContinuation {
            /// Read-only authenticated observation of the retained keeper. No replay or native
            /// authority is returned; caller loss never cancels a committed operation.
            pub fn status(&self) -> NativeResult<&'static str> {
                let deadline =
                    Deadline::new(30_000, self.io.bound_clock(), Cancellation::default())?;
                Ok(match self.observe(&deadline)?.stage {
                    KeeperStage::Preparing => "preparing",
                    KeeperStage::Ready => "ready",
                    KeeperStage::Committed => "committed",
                    KeeperStage::Retained => "retained",
                    KeeperStage::Complete => "complete",
                    KeeperStage::Cancelled => "cancelled",
                })
            }
            pub(crate) fn admit(
                io: Arc<WindowsNativeIo>,
                proof: &SupportProof,
                selected: &SelectedOuterOperation,
                deadline: &Deadline,
            ) -> NativeResult<Self> {
                if !Arc::ptr_eq(&io, selected.io()) {
                    return Err(NativeError::Foreign);
                }
                proof.check(&io, deadline)?;
                // Connection authority is checked from the actual server kernel peer below;
                // selected record fields only identify this one operation, never its process.
                let value = Self {
                    io,
                    operation: selected.operation(),
                };
                value.observe(deadline)?;
                Ok(value)
            }
            pub(crate) fn observe(&self, deadline: &Deadline) -> NativeResult<KeeperObservation> {
                let rt = runtime()?;
                let mut pipe = {
                    let _entered = rt.enter();
                    ClientOptions::new().open(endpoint(&self.io))
                }
                .map_err(|_| NativeError::Unavailable)?;
                let proof = self.io.admit_support(deadline)?;
                let peer = admit_keeper_observer_server(
                    pipe.as_raw_handle(),
                    self.io.clone(),
                    &proof,
                    self.operation,
                    deadline,
                )?;
                let reply = rt.block_on(async {
                    write_message(&mut pipe, self.operation, Method::Observe, None, deadline)
                        .await?;
                    read_message(&mut pipe, self.operation, deadline).await
                })?;
                if reply.method != Method::Ack {
                    return Err(NativeError::Foreign);
                }
                peer.reverify(&self.io, &self.io.admit_support(deadline)?, deadline)?;
                let stage = match reply.state.ok_or(NativeError::Foreign)? {
                    WireState::Ready => KeeperStage::Ready,
                    WireState::Committed => KeeperStage::Committed,
                    WireState::Retained => KeeperStage::Retained,
                    WireState::Complete => KeeperStage::Complete,
                    WireState::Cancelled => KeeperStage::Cancelled,
                    WireState::ReinstallRequired => return Err(NativeError::Foreign),
                };
                Ok(KeeperObservation { stage })
            }
        }

        impl KeeperChild {
            pub(crate) fn process(&self) -> NativeResult<Arc<OwnedHandle>> {
                self.0
                    .state
                    .lock()
                    .map_err(|_| NativeError::OutcomeUnknown)?
                    .process
                    .clone()
                    .ok_or(NativeError::OutcomeUnknown)
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
